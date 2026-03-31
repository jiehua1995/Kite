use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

// 中文：这里定义 `.kite` 的共享文件格式，是所有子命令之间唯一必须共用的层。
// English: This file defines the shared `.kite` on-disk format, which is the
// only layer that must remain common across subcommands.
pub const MAGIC: &[u8; 4] = b"KITE";
pub const CURRENT_FORMAT_VERSION: u8 = 1;
pub const FLAG_ZSTD: u8 = 0b0000_0001;
pub const FLAG_DELTA_VARINT: u8 = 0b0000_0010;
pub const FLAG_CANONICAL: u8 = 0b0000_0100;
pub const FLAG_BLOCK_FRAMES: u8 = 0b0000_1000;
pub const FLAG_BLOCK_TSV_BYTES: u8 = 0b0001_0000;

const FIXED_HEADER_BYTES: u64 = 32;
const LEGACY_HEADER_BYTES: u64 = 26;
const LEGACY_KIT2_MAGIC: &[u8; 4] = b"KIT2";
const LEGACY_KIT2_VERSION: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 中文：`.kite` 内部 k-mer 键的编码方式。
/// English: Internal k-mer key encoding used by `.kite`.
pub enum KmerMode {
    U64,
    U128,
    Packed(usize),
}

#[derive(Debug, Clone, Default)]
/// 中文：附加在 `.kite` 头部里的可选 metadata。
/// English: Optional metadata stored inside the `.kite` header.
pub struct KiteMetadata {
    pub source_path: Option<String>,
    pub created_at: Option<String>,
    pub total_reads: Option<u64>,
    pub total_kmers: Option<u64>,
    pub unique_kmers: Option<u64>,
    pub minimizer: Option<usize>,
    pub partitions: Option<usize>,
    pub count_threads: Option<usize>,
    pub chunk_kmers: Option<usize>,
    pub canonical: Option<bool>,
    pub write_mode: Option<String>,
    pub zstd_level: Option<i32>,
}

#[derive(Debug, Clone)]
/// 中文：读取头部后得到的 `.kite` 文件描述信息。
/// English: Parsed `.kite` file description returned after reading the header.
pub struct KiteInfo {
    pub format_version: u8,
    pub k: usize,
    pub mode: KmerMode,
    pub entries: u64,
    pub words: usize,
    pub header_bytes: u64,
    pub metadata_bytes: u32,
    pub flags: u8,
    pub payload_bytes: u64,
    pub metadata: KiteMetadata,
}

#[derive(Debug, Clone)]
/// 中文：`info/histo` 等命令汇总出的简单统计结果。
/// English: Lightweight aggregate statistics used by commands such as
/// `info` and `histo`.
pub struct KiteStats {
    pub info: KiteInfo,
    pub total_count: u64,
    pub singleton_kmers: u64,
    pub max_count: u64,
}

#[derive(Debug, Clone, Copy)]
/// 中文：头部补丁信息，用于在 payload 写完后回填长度。
/// English: Header patch record used to back-fill payload length after the
/// payload has been written.
pub struct HeaderPatch {
    pub entries_pos: u64,
    pub payload_len_pos: u64,
}

impl KiteInfo {
    /// 中文：返回数据库是否以 canonical k-mer 模式生成。
    /// English: Report whether the database was produced in canonical-k-mer
    /// mode.
    pub fn canonical(&self) -> bool {
        self.metadata
            .canonical
            .unwrap_or((self.flags & FLAG_CANONICAL) != 0)
    }
}

/// 中文：生成当前 UTC 时间的 RFC3339 字符串，用于写入 metadata。
/// English: Generate the current UTC timestamp in RFC3339 format for metadata.
pub fn current_timestamp_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

/// 中文：把 `.kite` 头部和 metadata 写到输出文件开头。
/// 输出是 `HeaderPatch`，供后续回填 payload 长度。
/// English: Write the `.kite` header and metadata at the start of the output
/// file. Returns a `HeaderPatch` so payload length can be patched later.
pub fn write_header<W: Write + Seek>(writer: &mut W, info: &KiteInfo) -> Result<HeaderPatch> {
    writer.seek(SeekFrom::Start(0))?;
    let metadata = encode_metadata(&info.metadata);
    let metadata_bytes = metadata.as_bytes();

    writer.write_all(MAGIC)?;
    writer.write_all(&[info.format_version])?;
    writer.write_all(&[mode_to_u8(info.mode)])?;
    writer.write_all(&[info.flags])?;
    writer.write_all(&[0u8])?;
    writer.write_all(&(info.k as u16).to_le_bytes())?;
    writer.write_all(&(info.words as u16).to_le_bytes())?;
    let entries_pos = writer.stream_position()?;
    writer.write_all(&info.entries.to_le_bytes())?;
    let payload_len_pos = writer.stream_position()?;
    writer.write_all(&0u64.to_le_bytes())?;
    writer.write_all(&(metadata_bytes.len() as u32).to_le_bytes())?;
    writer.write_all(metadata_bytes)?;
    writer.flush()?;

    Ok(HeaderPatch {
        entries_pos,
        payload_len_pos,
    })
}

/// 中文：在 payload 写完后回填实际的 payload 字节数。
/// English: Patch the true payload byte length after the payload has been
/// written.
pub fn patch_payload_bytes(path: &Path, patch: HeaderPatch, payload_bytes: u64) -> Result<()> {
    let mut file = File::options()
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open {} for header patch", path.display()))?;
    file.seek(SeekFrom::Start(patch.payload_len_pos))?;
    file.write_all(&payload_bytes.to_le_bytes())?;
    file.flush()?;
    Ok(())
}

/// 中文：在流式写出结束后同时回填 entry 数和 payload 长度。
/// English: Patch both the entry count and payload byte length after a
/// streaming write finishes.
pub fn patch_entries_and_payload(
    path: &Path,
    patch: HeaderPatch,
    entries: u64,
    payload_bytes: u64,
) -> Result<()> {
    let mut file = File::options()
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open {} for header patch", path.display()))?;
    file.seek(SeekFrom::Start(patch.entries_pos))?;
    file.write_all(&entries.to_le_bytes())?;
    file.seek(SeekFrom::Start(patch.payload_len_pos))?;
    file.write_all(&payload_bytes.to_le_bytes())?;
    file.flush()?;
    Ok(())
}

/// 中文：从磁盘路径读取 `.kite` 头信息。
/// English: Read `.kite` header information from a file path.
pub fn read_info(path: &Path) -> Result<KiteInfo> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    read_info_from_reader(&mut reader)
}

/// 中文：从任意支持 `Read + Seek` 的 reader 读取 `.kite` 头信息。
/// English: Read `.kite` header information from any `Read + Seek` reader.
pub fn read_info_from_reader<R: Read + Seek>(reader: &mut R) -> Result<KiteInfo> {
    reader.seek(SeekFrom::Start(0))?;
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic == LEGACY_KIT2_MAGIC {
        return read_kit2_header(reader);
    }
    if &magic != MAGIC {
        bail!("Invalid kite binary: bad magic");
    }

    let mut discriminator = [0u8; 1];
    reader.read_exact(&mut discriminator)?;
    if discriminator[0] <= CURRENT_FORMAT_VERSION {
        read_current_header(reader, discriminator[0])
    } else {
        read_legacy_header(reader, discriminator[0])
    }
}

/// 中文：把内部 `KmerMode` 编码成头部里的单字节标志。
/// English: Encode `KmerMode` into the single-byte header representation.
pub fn mode_to_u8(mode: KmerMode) -> u8 {
    match mode {
        KmerMode::U64 => 0,
        KmerMode::U128 => 1,
        KmerMode::Packed(_) => 2,
    }
}

/// 中文：把头部单字节模式值还原成 `KmerMode`。
/// English: Decode the one-byte header mode field back into `KmerMode`.
pub fn u8_to_mode(v: u8) -> Result<KmerMode> {
    match v {
        0 => Ok(KmerMode::U64),
        1 => Ok(KmerMode::U128),
        2 => Ok(KmerMode::Packed(0)),
        _ => bail!("Invalid mode in binary file: {v}"),
    }
}

/// 中文：解析旧版 `KIT2` 头部，保证历史文件仍可读。
/// English: Parse the legacy `KIT2` header so older files remain readable.
fn read_kit2_header<R: Read + Seek>(reader: &mut R) -> Result<KiteInfo> {
    let mut version_buf = [0u8; 1];
    reader.read_exact(&mut version_buf)?;
    if version_buf[0] != LEGACY_KIT2_VERSION {
        bail!("Unsupported legacy kite binary version {}", version_buf[0]);
    }

    let mut k_buf = [0u8; 2];
    reader.read_exact(&mut k_buf)?;
    let k = u16::from_le_bytes(k_buf) as usize;

    let mut mode_buf = [0u8; 1];
    reader.read_exact(&mut mode_buf)?;
    let mode = u8_to_mode(mode_buf[0])?;

    let mut flags_buf = [0u8; 1];
    reader.read_exact(&mut flags_buf)?;
    let flags = flags_buf[0];

    let mut entries_buf = [0u8; 8];
    reader.read_exact(&mut entries_buf)?;
    let entries = u64::from_le_bytes(entries_buf);

    let mut words_buf = [0u8; 2];
    reader.read_exact(&mut words_buf)?;
    let words = u16::from_le_bytes(words_buf) as usize;

    let mut payload_buf = [0u8; 8];
    reader.read_exact(&mut payload_buf)?;
    let payload_bytes = u64::from_le_bytes(payload_buf);

    Ok(KiteInfo {
        format_version: LEGACY_KIT2_VERSION,
        k,
        mode: match mode {
            KmerMode::Packed(_) => KmerMode::Packed(words),
            _ => mode,
        },
        entries,
        words,
        header_bytes: 27,
        metadata_bytes: 0,
        flags,
        payload_bytes,
        metadata: KiteMetadata {
            canonical: Some((flags & FLAG_CANONICAL) != 0),
            unique_kmers: Some(entries),
            ..KiteMetadata::default()
        },
    })
}

/// 中文：解析当前版本 `.kite` 头部。
/// English: Parse the current `.kite` header layout.
fn read_current_header<R: Read + Seek>(reader: &mut R, version: u8) -> Result<KiteInfo> {
    let mut mode_buf = [0u8; 1];
    reader.read_exact(&mut mode_buf)?;
    let mode = u8_to_mode(mode_buf[0])?;

    let mut flags_buf = [0u8; 1];
    reader.read_exact(&mut flags_buf)?;
    let flags = flags_buf[0];

    let mut reserved = [0u8; 1];
    reader.read_exact(&mut reserved)?;

    let mut k_buf = [0u8; 2];
    reader.read_exact(&mut k_buf)?;
    let k = u16::from_le_bytes(k_buf) as usize;

    let mut words_buf = [0u8; 2];
    reader.read_exact(&mut words_buf)?;
    let words = u16::from_le_bytes(words_buf) as usize;

    let mut entries_buf = [0u8; 8];
    reader.read_exact(&mut entries_buf)?;
    let entries = u64::from_le_bytes(entries_buf);

    let mut payload_buf = [0u8; 8];
    reader.read_exact(&mut payload_buf)?;
    let payload_bytes = u64::from_le_bytes(payload_buf);

    let mut metadata_len_buf = [0u8; 4];
    reader.read_exact(&mut metadata_len_buf)?;
    let metadata_bytes = u32::from_le_bytes(metadata_len_buf);

    let mut metadata_buf = vec![0u8; metadata_bytes as usize];
    reader.read_exact(&mut metadata_buf)?;
    let metadata = decode_metadata(std::str::from_utf8(&metadata_buf).unwrap_or_default());

    Ok(KiteInfo {
        format_version: version,
        k,
        mode: match mode {
            KmerMode::Packed(_) => KmerMode::Packed(words),
            _ => mode,
        },
        entries,
        words,
        header_bytes: FIXED_HEADER_BYTES + metadata_bytes as u64,
        metadata_bytes,
        flags,
        payload_bytes,
        metadata,
    })
}

/// 中文：解析更早的旧版头部格式。
/// English: Parse the older legacy header layout.
fn read_legacy_header<R: Read + Seek>(reader: &mut R, k_lo: u8) -> Result<KiteInfo> {
    let mut k_hi = [0u8; 1];
    reader.read_exact(&mut k_hi)?;
    let k = u16::from_le_bytes([k_lo, k_hi[0]]) as usize;

    let mut mode_buf = [0u8; 1];
    reader.read_exact(&mut mode_buf)?;
    let mode = u8_to_mode(mode_buf[0])?;

    let mut flags_buf = [0u8; 1];
    reader.read_exact(&mut flags_buf)?;
    let flags = flags_buf[0];

    let mut entries_buf = [0u8; 8];
    reader.read_exact(&mut entries_buf)?;
    let entries = u64::from_le_bytes(entries_buf);

    let mut words_buf = [0u8; 2];
    reader.read_exact(&mut words_buf)?;
    let words = u16::from_le_bytes(words_buf) as usize;

    let mut payload_buf = [0u8; 8];
    reader.read_exact(&mut payload_buf)?;
    let payload_bytes = u64::from_le_bytes(payload_buf);

    Ok(KiteInfo {
        format_version: 0,
        k,
        mode: match mode {
            KmerMode::Packed(_) => KmerMode::Packed(words),
            _ => mode,
        },
        entries,
        words,
        header_bytes: LEGACY_HEADER_BYTES,
        metadata_bytes: 0,
        flags,
        payload_bytes,
        metadata: KiteMetadata {
            canonical: Some((flags & FLAG_CANONICAL) != 0),
            unique_kmers: Some(entries),
            ..KiteMetadata::default()
        },
    })
}

/// 中文：把 `KiteMetadata` 序列化成简单的 `key=value` 文本块。
/// English: Serialize `KiteMetadata` into a simple `key=value` text block.
fn encode_metadata(metadata: &KiteMetadata) -> String {
    let mut out = String::new();
    push_opt_string(&mut out, "source_path", metadata.source_path.as_deref());
    push_opt_string(&mut out, "created_at", metadata.created_at.as_deref());
    push_opt_u64(&mut out, "total_reads", metadata.total_reads);
    push_opt_u64(&mut out, "total_kmers", metadata.total_kmers);
    push_opt_u64(&mut out, "unique_kmers", metadata.unique_kmers);
    push_opt_usize(&mut out, "minimizer", metadata.minimizer);
    push_opt_usize(&mut out, "partitions", metadata.partitions);
    push_opt_usize(&mut out, "count_threads", metadata.count_threads);
    push_opt_usize(&mut out, "chunk_kmers", metadata.chunk_kmers);
    push_opt_bool(&mut out, "canonical", metadata.canonical);
    push_opt_string(&mut out, "write_mode", metadata.write_mode.as_deref());
    if let Some(value) = metadata.zstd_level {
        out.push_str("zstd_level=");
        out.push_str(&value.to_string());
        out.push('\n');
    }
    out
}

/// 中文：把头部里的 `key=value` metadata 文本解析回结构体。
/// English: Parse the `key=value` metadata text block back into a struct.
fn decode_metadata(text: &str) -> KiteMetadata {
    let mut metadata = KiteMetadata::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "source_path" => metadata.source_path = Some(unescape_value(value)),
            "created_at" => metadata.created_at = Some(unescape_value(value)),
            "total_reads" => metadata.total_reads = value.parse().ok(),
            "total_kmers" => metadata.total_kmers = value.parse().ok(),
            "unique_kmers" => metadata.unique_kmers = value.parse().ok(),
            "minimizer" => metadata.minimizer = value.parse().ok(),
            "partitions" => metadata.partitions = value.parse().ok(),
            "count_threads" => metadata.count_threads = value.parse().ok(),
            "chunk_kmers" => metadata.chunk_kmers = value.parse().ok(),
            "canonical" => metadata.canonical = value.parse().ok(),
            "write_mode" => metadata.write_mode = Some(unescape_value(value)),
            "zstd_level" => metadata.zstd_level = value.parse().ok(),
            _ => {}
        }
    }
    metadata
}

/// 中文：向 metadata 文本里追加一个可选字符串字段。
/// English: Append one optional string field to the metadata text.
fn push_opt_string(out: &mut String, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        out.push_str(key);
        out.push('=');
        out.push_str(&escape_value(value));
        out.push('\n');
    }
}

/// 中文：向 metadata 文本里追加一个可选 `u64` 字段。
/// English: Append one optional `u64` field to the metadata text.
fn push_opt_u64(out: &mut String, key: &str, value: Option<u64>) {
    if let Some(value) = value {
        out.push_str(key);
        out.push('=');
        out.push_str(&value.to_string());
        out.push('\n');
    }
}

/// 中文：向 metadata 文本里追加一个可选 `usize` 字段。
/// English: Append one optional `usize` field to the metadata text.
fn push_opt_usize(out: &mut String, key: &str, value: Option<usize>) {
    if let Some(value) = value {
        out.push_str(key);
        out.push('=');
        out.push_str(&value.to_string());
        out.push('\n');
    }
}

/// 中文：向 metadata 文本里追加一个可选布尔字段。
/// English: Append one optional boolean field to the metadata text.
fn push_opt_bool(out: &mut String, key: &str, value: Option<bool>) {
    if let Some(value) = value {
        out.push_str(key);
        out.push('=');
        out.push_str(if value { "true" } else { "false" });
        out.push('\n');
    }
}

/// 中文：转义 metadata 文本值中的控制字符。
/// English: Escape control characters inside metadata text values.
fn escape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

/// 中文：把 metadata 文本值从转义形式还原回原始字符串。
/// English: Unescape a metadata text value back into its original string.
fn unescape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}
