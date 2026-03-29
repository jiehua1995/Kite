use anyhow::{Context, Result, bail};
use flate2::read::MultiGzDecoder;
use memmap2::Mmap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

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
    Gzip(BufferedSeqReader<BufReader<MultiGzDecoder<File>>>),
    Mmap(MmapSeqReader),
}

impl SeqReader {
    /// 中文：读取下一条序列到调用者提供的缓冲区。
    /// 输出为 `true/false` 表示是否成功读到一条记录。
    /// English: Read the next sequence into the caller-provided buffer.
    /// Returns `true/false` to indicate whether a record was produced.
    pub fn next_sequence(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        match self {
            SeqReader::Buffered(r) => r.next_sequence(out),
            SeqReader::Gzip(r) => r.next_sequence(out),
            SeqReader::Mmap(r) => r.next_sequence(out),
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
    pub fn next_sequence(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        match self.detect_format()? {
            None => Ok(false),
            Some(InputFormat::Fastq) => self.next_fastq(out),
            Some(InputFormat::Fasta) => self.next_fasta(out),
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
    fn next_fastq(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        out.clear();
        if self.pending_header.is_none() {
            self.line.clear();
            if self.inner.read_until(b'\n', &mut self.line)? == 0 {
                return Ok(false);
            }
            if self.line.first().copied() != Some(b'@') {
                bail!("Malformed FASTQ: missing @ header");
            }
        } else {
            self.pending_header.take();
        }

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

        Ok(true)
    }

    /// 中文：读取一条 FASTA 记录，支持多行 sequence 拼接。
    /// English: Read one FASTA record, concatenating multi-line sequence
    /// content.
    fn next_fasta(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        out.clear();

        if self.pending_header.is_none() {
            loop {
                self.line.clear();
                let n = self.inner.read_until(b'\n', &mut self.line)?;
                if n == 0 {
                    return Ok(false);
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
                return Ok(!out.is_empty());
            }
            if self.line.first().copied() == Some(b'>') {
                self.pending_header = Some(self.line.clone());
                return Ok(!out.is_empty());
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
    fn next_sequence(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        match self.detect_format()? {
            None => Ok(false),
            Some(InputFormat::Fastq) => self.next_fastq(out),
            Some(InputFormat::Fasta) => self.next_fasta(out),
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
    fn next_fastq(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        out.clear();

        if self.pending_header.is_none() {
            loop {
                let Some((s, e)) = self.next_line_range() else {
                    return Ok(false);
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

        Ok(true)
    }

    /// 中文：在 mmap 输入上读取一条 FASTA sequence。
    /// English: Read one FASTA sequence from mmap-backed input.
    fn next_fasta(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        out.clear();

        if self.pending_header.is_none() {
            loop {
                let Some((s, e)) = self.next_line_range() else {
                    return Ok(false);
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

        Ok(!out.is_empty())
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
    let is_gz = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("gz"))
        .unwrap_or(false);

    if is_gz {
        let decoder = MultiGzDecoder::new(file);
        return Ok(SeqReader::Gzip(BufferedSeqReader::new(
            BufReader::with_capacity(1 << 20, decoder),
        )));
    }

    match MmapSeqReader::new(&file) {
        Ok(reader) => Ok(SeqReader::Mmap(reader)),
        Err(_) => Ok(SeqReader::Buffered(BufferedSeqReader::new(
            BufReader::with_capacity(1 << 20, file),
        ))),
    }
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
