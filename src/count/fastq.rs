use crate::count::perf::PerfCounters;
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, bounded};
use flate2::read::MultiGzDecoder;
use libdeflater::Decompressor;
use memmap2::Mmap;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
/// 中文：输入文件格式枚举，目前支持 FASTQ 和 FASTA。
/// English: Input file format enum. The reader currently supports FASTQ and FASTA.
enum InputFormat {
    Fastq,
    Fasta,
}

/// 中文：统一的序列读取器抽象，屏蔽普通文件、gzip 文件和 mmap 文件的差异。
/// English: Unified sequence reader abstraction that hides the differences
/// between plain files, gzip streams, and mmap-backed files.
pub enum SeqReader {
    Buffered(BufferedSeqReader<BufReader<File>>),
    Gzip(BufferedSeqReader<BufReader<MultiGzDecoder<BufReader<File>>>>),
    Mmap(MmapSeqReader),
}

/// 中文：`.gz` 大块解压线程发出的消息类型，正常时携带一块已解压的字节数据；
/// 出错时携带错误，供上游立即终止流水线。
/// English: Message emitted by the gzip block reader thread. Successful items
/// contain one decompressed byte block; failed items propagate the decode error
/// so the upstream pipeline can abort early.
pub type GzipBlockMessage = Result<Vec<u8>>;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
/// 中文：压缩输入的探测结果。
/// - `PlainGzip`: 普通单流 gzip
/// - `Bgzf`: BGZF（Blocked GZIP），可走块级并行解压
/// English: Probed compressed-input kind.
/// - `PlainGzip`: ordinary single-stream gzip
/// - `Bgzf`: BGZF (Blocked GZIP), which can use block-level parallel inflate
pub enum GzipKind {
    PlainGzip,
    Bgzf,
}

impl SeqReader {
    /// 中文：读取下一条序列到调用者提供的缓冲区。
    /// 输出为 `true/false` 表示是否成功读到一条记录。
    /// English: Read the next sequence into the caller-provided buffer.
    /// Returns `true/false` to indicate whether a record was produced.
    pub fn next_sequence(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        out.clear();
        Ok(self.next_sequence_range(out)?.is_some())
    }

    /// 中文：直接把下一条序列追加到目标缓冲区末尾，并返回追加片段的起始偏移与长度。
    /// English: Append the next sequence directly to the destination buffer and
    /// return the appended `(start, len)` range.
    pub fn next_sequence_range(&mut self, out: &mut Vec<u8>) -> Result<Option<(u32, u32)>> {
        match self {
            SeqReader::Buffered(r) => r.next_sequence_range(out),
            SeqReader::Gzip(r) => r.next_sequence_range(out),
            SeqReader::Mmap(r) => r.next_sequence_range(out),
        }
    }
}

/// 中文：基于 `BufRead` 的通用文本序列读取器，适用于普通文件和 gzip 流。
/// English: Generic text-sequence reader built on `BufRead`, used for plain
/// files and gzip streams.
pub struct BufferedSeqReader<R: BufRead> {
    inner: R,
    format: Option<InputFormat>,
    pending_header: Option<Vec<u8>>,
    line: Vec<u8>,
}

impl<R: BufRead> BufferedSeqReader<R> {
    /// 中文：创建新的缓冲读取器。
    /// English: Create a new buffered sequence reader.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            format: None,
            pending_header: None,
            line: Vec::with_capacity(512),
        }
    }

    /// 中文：读取下一条序列，自动根据文件头分派到 FASTQ 或 FASTA 分支。
    /// English: Read the next sequence, dispatching automatically to FASTQ or
    /// FASTA logic after format detection.
    pub fn next_sequence_range(&mut self, out: &mut Vec<u8>) -> Result<Option<(u32, u32)>> {
        match self.detect_format()? {
            None => Ok(None),
            Some(InputFormat::Fastq) => self.next_fastq_range(out),
            Some(InputFormat::Fasta) => self.next_fasta_range(out),
        }
    }

    /// 中文：探测输入格式并缓存结果，避免后续重复判断。
    /// English: Detect the input format once and cache the result for later
    /// reads.
    fn detect_format(&mut self) -> Result<Option<InputFormat>> {
        if let Some(fmt) = self.format {
            return Ok(Some(fmt));
        }

        loop {
            self.line.clear();
            let n = self.inner.read_until(b'\n', &mut self.line)?;
            if n == 0 {
                return Ok(None);
            }
            if self.line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            match self.line[0] {
                b'@' => {
                    self.format = Some(InputFormat::Fastq);
                    self.pending_header = Some(self.line.clone());
                    return Ok(self.format);
                }
                b'>' => {
                    self.format = Some(InputFormat::Fasta);
                    self.pending_header = Some(self.line.clone());
                    return Ok(self.format);
                }
                _ => bail!("Unsupported input format: expected FASTQ (@) or FASTA (>) header"),
            }
        }
    }

    /// 中文：读取一条 FASTQ 记录中的 sequence 行。
    /// English: Read the sequence portion of one FASTQ record.
    fn next_fastq_range(&mut self, out: &mut Vec<u8>) -> Result<Option<(u32, u32)>> {
        if self.pending_header.is_none() {
            self.line.clear();
            if self.inner.read_until(b'\n', &mut self.line)? == 0 {
                return Ok(None);
            }
            if self.line.first().copied() != Some(b'@') {
                bail!("Malformed FASTQ: missing @ header");
            }
        } else {
            self.pending_header.take();
        }

        let start = out.len();
        self.line.clear();
        self.inner
            .read_until(b'\n', &mut self.line)
            .context("Malformed FASTQ sequence line")?;
        if self.line.is_empty() {
            bail!("Malformed FASTQ: empty sequence line");
        }
        trim_newline(&mut self.line);
        out.extend_from_slice(&self.line);

        self.line.clear();
        self.inner
            .read_until(b'\n', &mut self.line)
            .context("Malformed FASTQ plus line")?;

        self.line.clear();
        self.inner
            .read_until(b'\n', &mut self.line)
            .context("Malformed FASTQ quality line")?;

        let len = out.len() - start;
        Ok(Some((start as u32, len as u32)))
    }

    /// 中文：读取一条 FASTA 记录，支持多行 sequence 拼接。
    /// English: Read one FASTA record, concatenating multi-line sequence
    /// content.
    fn next_fasta_range(&mut self, out: &mut Vec<u8>) -> Result<Option<(u32, u32)>> {
        let start = out.len();
        if self.pending_header.is_none() {
            loop {
                self.line.clear();
                let n = self.inner.read_until(b'\n', &mut self.line)?;
                if n == 0 {
                    return Ok(None);
                }
                if self.line.first().copied() == Some(b'>') {
                    break;
                }
            }
        } else {
            self.pending_header.take();
        }

        loop {
            self.line.clear();
            let n = self.inner.read_until(b'\n', &mut self.line)?;
            if n == 0 {
                let len = out.len() - start;
                return Ok((len != 0).then_some((start as u32, len as u32)));
            }
            if self.line.first().copied() == Some(b'>') {
                self.pending_header = Some(self.line.clone());
                let len = out.len() - start;
                return Ok((len != 0).then_some((start as u32, len as u32)));
            }
            trim_newline(&mut self.line);
            out.extend_from_slice(&self.line);
        }
    }
}

/// 中文：基于内存映射的高速读取器，优先用于未压缩输入。
/// English: Memory-mapped high-throughput reader, preferred for uncompressed
/// inputs.
pub struct MmapSeqReader {
    mmap: Mmap,
    pos: usize,
    format: Option<InputFormat>,
    pending_header: Option<(usize, usize)>,
}

impl MmapSeqReader {
    /// 中文：把文件映射进内存并创建读取器。
    /// English: Memory-map a file and construct the mmap-based reader.
    fn new(file: &File) -> Result<Self> {
        let mmap = unsafe { Mmap::map(file) }.context("failed to mmap input file")?;
        Ok(Self {
            mmap,
            pos: 0,
            format: None,
            pending_header: None,
        })
    }

    /// 中文：读取下一条序列，自动分派到 FASTQ/FASTA 分支。
    /// English: Read the next sequence, dispatching to FASTQ/FASTA logic.
    fn next_sequence_range(&mut self, out: &mut Vec<u8>) -> Result<Option<(u32, u32)>> {
        match self.detect_format()? {
            None => Ok(None),
            Some(InputFormat::Fastq) => self.next_fastq_range(out),
            Some(InputFormat::Fasta) => self.next_fasta_range(out),
        }
    }

    /// 中文：探测 mmap 输入的格式。
    /// English: Detect the format of the mmap-backed input.
    fn detect_format(&mut self) -> Result<Option<InputFormat>> {
        if let Some(fmt) = self.format {
            return Ok(Some(fmt));
        }

        while let Some((s, e)) = self.next_line_range() {
            if is_whitespace_line(&self.mmap[s..e]) {
                continue;
            }
            match self.mmap[s] {
                b'@' => {
                    self.format = Some(InputFormat::Fastq);
                    self.pending_header = Some((s, e));
                    return Ok(self.format);
                }
                b'>' => {
                    self.format = Some(InputFormat::Fasta);
                    self.pending_header = Some((s, e));
                    return Ok(self.format);
                }
                _ => bail!("Unsupported input format: expected FASTQ (@) or FASTA (>) header"),
            }
        }

        Ok(None)
    }

    /// 中文：在 mmap 输入上读取一条 FASTQ sequence。
    /// English: Read one FASTQ sequence from mmap-backed input.
    fn next_fastq_range(&mut self, out: &mut Vec<u8>) -> Result<Option<(u32, u32)>> {
        if self.pending_header.is_none() {
            loop {
                let Some((s, e)) = self.next_line_range() else {
                    return Ok(None);
                };
                if e > s && self.mmap[s] == b'@' {
                    break;
                }
                if !is_whitespace_line(&self.mmap[s..e]) {
                    bail!("Malformed FASTQ: missing @ header");
                }
            }
        } else {
            self.pending_header.take();
        }

        let start = out.len();
        let Some((ss, se)) = self.next_line_range() else {
            bail!("Malformed FASTQ sequence line");
        };
        if se <= ss {
            bail!("Malformed FASTQ: empty sequence line");
        }
        out.extend_from_slice(&self.mmap[ss..se]);

        let Some((ps, _)) = self.next_line_range() else {
            bail!("Malformed FASTQ plus line");
        };
        if self.mmap[ps] != b'+' {
            bail!("Malformed FASTQ plus line");
        }

        if self.next_line_range().is_none() {
            bail!("Malformed FASTQ quality line");
        }

        let len = out.len() - start;
        Ok(Some((start as u32, len as u32)))
    }

    /// 中文：在 mmap 输入上读取一条 FASTA sequence。
    /// English: Read one FASTA sequence from mmap-backed input.
    fn next_fasta_range(&mut self, out: &mut Vec<u8>) -> Result<Option<(u32, u32)>> {
        let start = out.len();
        if self.pending_header.is_none() {
            loop {
                let Some((s, e)) = self.next_line_range() else {
                    return Ok(None);
                };
                if e > s && self.mmap[s] == b'>' {
                    break;
                }
            }
        } else {
            self.pending_header.take();
        }

        while let Some((s, e)) = self.next_line_range() {
            if e <= s {
                continue;
            }
            if self.mmap[s] == b'>' {
                self.pending_header = Some((s, e));
                break;
            }
            out.extend_from_slice(&self.mmap[s..e]);
        }

        let len = out.len() - start;
        Ok((len != 0).then_some((start as u32, len as u32)))
    }

    #[inline(always)]
    /// 中文：返回下一行的 `[start, end)` 范围，并自动处理 `\r\n`。
    /// English: Return the `[start, end)` range of the next line while
    /// normalizing `\r\n`.
    fn next_line_range(&mut self) -> Option<(usize, usize)> {
        let data = &self.mmap;
        if self.pos >= data.len() {
            return None;
        }

        let start = self.pos;
        let mut end = self.pos;
        while end < data.len() && data[end] != b'\n' {
            end += 1;
        }
        self.pos = if end < data.len() {
            end + 1
        } else {
            data.len()
        };

        let mut trimmed_end = end;
        if trimmed_end > start && data[trimmed_end - 1] == b'\r' {
            trimmed_end -= 1;
        }
        Some((start, trimmed_end))
    }
}

/// 中文：根据文件后缀和 mmap 能力选择最合适的序列读取器。
/// English: Open the best available sequence reader based on file suffix and
/// mmap availability.
pub fn open_seq_reader(path: &Path) -> Result<SeqReader> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let is_gz = detect_gzip_kind(path)?.is_some();

    if is_gz {
        let file_reader = BufReader::with_capacity(4 << 20, file);
        let decoder = MultiGzDecoder::new(file_reader);
        return Ok(SeqReader::Gzip(BufferedSeqReader::new(
            BufReader::with_capacity(4 << 20, decoder),
        )));
    }

    match MmapSeqReader::new(&file) {
        Ok(reader) => Ok(SeqReader::Mmap(reader)),
        Err(_) => Ok(SeqReader::Buffered(BufferedSeqReader::new(
            BufReader::with_capacity(4 << 20, file),
        ))),
    }
}

fn has_gzip_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| {
            s.eq_ignore_ascii_case("gz")
                || s.eq_ignore_ascii_case("bgz")
                || s.eq_ignore_ascii_case("bgzf")
        })
        .unwrap_or(false)
}

/// 中文：探测输入文件究竟是普通 gzip 还是 BGZF。
/// 输入：
/// - `path`: 输入文件路径（支持扩展名和魔数识别）
/// 输出：
/// - `None`: 不是 gzip
/// - `Some(PlainGzip)`: 普通 gzip
/// - `Some(Bgzf)`: BGZF
/// English: Probe whether an input is ordinary gzip or BGZF.
/// Input:
/// - `path`: input path (extension + magic-byte detection)
/// Output:
/// - `None`: not gzip
/// - `Some(PlainGzip)`: ordinary gzip
/// - `Some(Bgzf)`: BGZF
pub fn detect_gzip_kind(path: &Path) -> Result<Option<GzipKind>> {
    const GZIP_ID1: u8 = 0x1f;
    const GZIP_ID2: u8 = 0x8b;
    const GZIP_CM_DEFLATE: u8 = 0x08;
    const GZIP_FEXTRA: u8 = 0x04;

    let ext_claims_gzip = has_gzip_extension(path);

    let mut file =
        File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut header = [0u8; 18];
    let n = file
        .read(&mut header)
        .with_context(|| format!("Failed to read gzip header from {}", path.display()))?;

    if n < 3 {
        if ext_claims_gzip {
            bail!("Invalid gzip header in {}", path.display());
        }
        return Ok(None);
    }

    if header[0] != GZIP_ID1 || header[1] != GZIP_ID2 {
        if ext_claims_gzip {
            bail!("Invalid gzip header in {}", path.display());
        }
        return Ok(None);
    }

    if header[2] != GZIP_CM_DEFLATE {
        bail!("Unsupported gzip compression method in {}", path.display());
    }

    if n < 18 {
        return Ok(Some(GzipKind::PlainGzip));
    }

    if (header[3] & GZIP_FEXTRA) == 0 {
        return Ok(Some(GzipKind::PlainGzip));
    }

    let xlen = u16::from_le_bytes([header[10], header[11]]) as usize;
    if xlen < 6 {
        return Ok(Some(GzipKind::PlainGzip));
    }

    if header[12] == b'B' && header[13] == b'C' && header[14] == 2 && header[15] == 0 {
        return Ok(Some(GzipKind::Bgzf));
    }

    Ok(Some(GzipKind::PlainGzip))
}

/// 中文：为 `.fastq.gz` 启动一个专用的大块解压线程。它会反复把 gzip 流解压到
/// 固定大小的字节块中，并通过有界通道把所有权交给上游解析器。
/// 输入：
/// - `path`: gzip FASTQ 文件路径
/// - `block_bytes`: 每个解压块的目标大小
/// - `queue_cap`: 解压块通道容量
/// 输出：
/// - 一个接收端，用于消费解压后的字节块
/// - 一个线程句柄，用于在消费结束后 `join`
/// English: Spawn a dedicated block-based decompression thread for
/// `.fastq.gz`. It repeatedly inflates the gzip stream into fixed-size byte
/// blocks and transfers ownership of those blocks through a bounded channel.
/// Input:
/// - `path`: path to the gzip FASTQ
/// - `block_bytes`: target decompressed bytes per block
/// - `queue_cap`: channel capacity for decompressed blocks
/// Output:
/// - a receiver that yields decompressed byte blocks
/// - a thread handle that should be joined after consumption
pub fn spawn_gzip_block_reader(
    path: &Path,
    block_bytes: usize,
    queue_cap: usize,
    prealloc_blocks: usize,
    perf: Option<Arc<PerfCounters>>,
) -> Result<(
    Receiver<GzipBlockMessage>,
    crossbeam_channel::Sender<Vec<u8>>,
    thread::JoinHandle<()>,
)> {
    let path = path.to_path_buf();
    let (tx, rx) = bounded::<GzipBlockMessage>(queue_cap.max(1));
    let (recycle_tx, recycle_rx) = bounded::<Vec<u8>>(queue_cap.max(1) * 2);
    for _ in 0..prealloc_blocks {
        if recycle_tx.send(Vec::with_capacity(block_bytes)).is_err() {
            break;
        }
    }
    let handle = thread::spawn(move || {
        let run = || -> Result<()> {
            let file =
                File::open(&path).with_context(|| format!("Failed to open {}", path.display()))?;
            let file_reader = BufReader::with_capacity(4 << 20, file);
            let mut decoder = MultiGzDecoder::new(file_reader);

            loop {
                let mut block = recycle_rx
                    .try_recv()
                    .unwrap_or_else(|_| Vec::with_capacity(block_bytes));
                if block.capacity() < block_bytes {
                    block.reserve(block_bytes - block.capacity());
                }
                block.resize(block_bytes, 0);
                let mut filled = 0usize;
                while filled < block_bytes {
                    let t_read = Instant::now();
                    let n = decoder
                        .read(&mut block[filled..])
                        .with_context(|| format!("Failed to decompress {}", path.display()))?;
                    if let Some(perf) = &perf {
                        perf.add_decompress(t_read.elapsed());
                    }
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }

                if filled == 0 {
                    break;
                }

                block.truncate(filled);
                if tx.send(Ok(block)).is_err() {
                    return Ok(());
                }
            }
            Ok(())
        };

        if let Err(err) = run() {
            let _ = tx.send(Err(err));
        }
    });
    Ok((rx, recycle_tx, handle))
}

/// 中文：为 BGZF 输入启动一个块级并行解压流水线。内部按 BGZF block 读取压缩数据，
/// 用 `libdeflater` 并行解压，再按块顺序重组回连续的 FASTQ 文本块。
/// 输入：
/// - `path`: BGZF FASTQ 文件路径
/// - `worker_count`: BGZF 内部并行解压线程数
/// - `block_bytes`: 输出文本块大小
/// - `queue_cap`: 输出通道容量
/// 输出：
/// - 解压文本块接收端
/// - 后台线程句柄
/// English: Spawn a block-parallel decompression pipeline for BGZF input. It
/// reads raw BGZF blocks, inflates them in parallel with `libdeflater`, then
/// restores block order and exposes the result as a stream of FASTQ text
/// blocks.
pub fn spawn_bgzf_block_reader(
    path: &Path,
    worker_count: usize,
    block_bytes: usize,
    queue_cap: usize,
    prealloc_blocks: usize,
    perf: Option<Arc<PerfCounters>>,
) -> Result<(
    Receiver<GzipBlockMessage>,
    crossbeam_channel::Sender<Vec<u8>>,
    thread::JoinHandle<()>,
)> {
    #[derive(Debug)]
    struct CompressedBgzfBlock {
        index: u64,
        data: Vec<u8>,
    }

    #[derive(Debug)]
    struct DecompressedBgzfBlock {
        index: u64,
        data: Vec<u8>,
    }

    fn read_bgzf_block<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>> {
        let mut header = [0u8; 18];
        let n = reader.read(&mut header[..1])?;
        if n == 0 {
            return Ok(None);
        }
        reader
            .read_exact(&mut header[1..])
            .context("Failed to read complete BGZF header")?;

        if header[0] != 0x1f || header[1] != 0x8b || header[2] != 0x08 {
            bail!("Invalid BGZF header");
        }
        if (header[3] & 0x04) == 0 {
            bail!("Malformed BGZF: missing FEXTRA flag");
        }
        if header[12] != b'B' || header[13] != b'C' || header[14] != 2 || header[15] != 0 {
            bail!("Malformed BGZF: missing BC extra field");
        }

        let block_size = (u16::from_le_bytes([header[16], header[17]]) as usize) + 1;
        if block_size < 26 {
            bail!("Malformed BGZF: invalid block size");
        }

        let mut block = vec![0u8; block_size];
        block[..18].copy_from_slice(&header);
        reader
            .read_exact(&mut block[18..])
            .context("Failed to read full BGZF block")?;
        Ok(Some(block))
    }

    let path = path.to_path_buf();
    let (tx, rx) = bounded::<GzipBlockMessage>(queue_cap.max(1));
    let (recycle_tx, recycle_rx) = bounded::<Vec<u8>>(queue_cap.max(1) * 2);
    for _ in 0..prealloc_blocks {
        if recycle_tx.send(Vec::with_capacity(block_bytes)).is_err() {
            break;
        }
    }
    let handle = thread::spawn(move || {
        let run = || -> Result<()> {
            let file =
                File::open(&path).with_context(|| format!("Failed to open {}", path.display()))?;
            let mut reader = BufReader::with_capacity(8 << 20, file);
            let decompress_threads = worker_count.max(1);
            let (comp_tx, comp_rx) =
                bounded::<Result<CompressedBgzfBlock>>((queue_cap.max(1) * 4).max(64));
            let (out_tx, out_rx) =
                bounded::<Result<DecompressedBgzfBlock>>((queue_cap.max(1) * 4).max(64));

            let reader_path = path.clone();
            let perf_read = perf.clone();
            let reader_handle = thread::spawn(move || {
                let mut index = 0u64;
                let mut read_loop = || -> Result<()> {
                    loop {
                        let t_read = Instant::now();
                        let maybe_block = read_bgzf_block(&mut reader).with_context(|| {
                            format!("Failed to read BGZF block from {}", reader_path.display())
                        })?;
                        if let Some(perf) = &perf_read {
                            perf.add_read(t_read.elapsed());
                        }
                        let Some(block) = maybe_block else {
                            break;
                        };
                        if comp_tx
                            .send(Ok(CompressedBgzfBlock { index, data: block }))
                            .is_err()
                        {
                            return Ok(());
                        }
                        index += 1;
                    }
                    Ok(())
                };
                if let Err(err) = read_loop() {
                    let _ = comp_tx.send(Err(err));
                }
            });

            let mut workers = Vec::with_capacity(decompress_threads);
            for _ in 0..decompress_threads {
                let comp_rx = comp_rx.clone();
                let out_tx = out_tx.clone();
                let perf_decompress = perf.clone();
                workers.push(thread::spawn(move || {
                    let mut decompressor = Decompressor::new();
                    while let Ok(msg) = comp_rx.recv() {
                        let block = match msg {
                            Ok(block) => block,
                            Err(err) => {
                                let _ = out_tx.send(Err(err));
                                return;
                            }
                        };

                        let expected_size = u32::from_le_bytes([
                            block.data[block.data.len() - 4],
                            block.data[block.data.len() - 3],
                            block.data[block.data.len() - 2],
                            block.data[block.data.len() - 1],
                        ]) as usize;
                        let mut out = vec![0u8; expected_size];
                        let t_decompress = Instant::now();
                        match decompressor.gzip_decompress(&block.data, &mut out) {
                            Ok(written) => {
                                if let Some(perf) = &perf_decompress {
                                    perf.add_decompress(t_decompress.elapsed());
                                }
                                out.truncate(written);
                                if out_tx
                                    .send(Ok(DecompressedBgzfBlock {
                                        index: block.index,
                                        data: out,
                                    }))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(err) => {
                                let _ = out_tx.send(Err(anyhow::anyhow!(
                                    "Failed to decompress BGZF block {}: {}",
                                    block.index,
                                    err
                                )));
                                return;
                            }
                        }
                    }
                }));
            }
            drop(out_tx);

            let mut next_index = 0u64;
            let mut pending = BTreeMap::<u64, Vec<u8>>::new();
            let mut text_block = recycle_rx
                .try_recv()
                .unwrap_or_else(|_| Vec::with_capacity(block_bytes));

            while let Ok(msg) = out_rx.recv() {
                let block = msg?;
                pending.insert(block.index, block.data);
                while let Some(data) = pending.remove(&next_index) {
                    text_block.extend_from_slice(&data);
                    if text_block.len() >= block_bytes {
                        if tx.send(Ok(text_block)).is_err() {
                            return Ok(());
                        }
                        text_block = recycle_rx
                            .try_recv()
                            .unwrap_or_else(|_| Vec::with_capacity(block_bytes));
                    }
                    next_index += 1;
                }
            }

            if !text_block.is_empty() {
                if tx.send(Ok(text_block)).is_err() {
                    return Ok(());
                }
            }

            reader_handle
                .join()
                .map_err(|_| anyhow::anyhow!("BGZF compressed-block reader thread panicked"))?;
            for worker in workers {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("BGZF decompressor worker panicked"))?;
            }
            Ok(())
        };

        if let Err(err) = run() {
            let _ = tx.send(Err(err));
        }
    });
    Ok((rx, recycle_tx, handle))
}

#[inline(always)]
/// 中文：判断一行是否只包含空白字符。
/// English: Test whether a line contains only ASCII whitespace.
fn is_whitespace_line(line: &[u8]) -> bool {
    line.iter().all(|b| b.is_ascii_whitespace())
}

/// 中文：去掉序列行结尾的换行符。
/// English: Strip trailing newline characters from a sequence line buffer.
fn trim_newline(v: &mut Vec<u8>) {
    while let Some(last) = v.last().copied() {
        if last == b'\n' || last == b'\r' {
            v.pop();
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GzipKind, detect_gzip_kind};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        temp_path_with_ext(name, "gz")
    }

    fn temp_path_with_ext(name: &str, ext: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("kite_{name}_{nanos}.{ext}"))
    }

    #[test]
    fn detect_plain_gzip_header() {
        let path = temp_path("plain");
        let header = [
            0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        fs::write(&path, header).unwrap();
        let kind = detect_gzip_kind(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(kind, Some(GzipKind::PlainGzip));
    }

    #[test]
    fn detect_bgzf_header() {
        let path = temp_path("bgzf");
        let header = [
            0x1f, 0x8b, 0x08, 0x04, 0, 0, 0, 0, 0, 255, 0x06, 0x00, b'B', b'C', 0x02, 0x00, 0x1b,
            0x00,
        ];
        fs::write(&path, header).unwrap();
        let kind = detect_gzip_kind(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(kind, Some(GzipKind::Bgzf));
    }

    #[test]
    fn detect_bgzf_header_with_bgz_extension() {
        let path = temp_path_with_ext("bgzf_ext", "bgz");
        let header = [
            0x1f, 0x8b, 0x08, 0x04, 0, 0, 0, 0, 0, 255, 0x06, 0x00, b'B', b'C', 0x02, 0x00, 0x1b,
            0x00,
        ];
        fs::write(&path, header).unwrap();
        let kind = detect_gzip_kind(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(kind, Some(GzipKind::Bgzf));
    }

    #[test]
    fn detect_plain_gzip_without_extension() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("kite_noext_{nanos}"));
        let header = [
            0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        fs::write(&path, header).unwrap();
        let kind = detect_gzip_kind(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(kind, Some(GzipKind::PlainGzip));
    }
}
