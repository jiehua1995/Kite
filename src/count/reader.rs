use std::path::Path;

/// Detected compression type for an input FASTQ file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    Plain, // .fastq / .fq
    Gzip,  // .fastq.gz / .fq.gz — standard deflate/gzip stream
    Bgzf,  // .fastq.gz that is BGZF (block gzip): bgzipped BAM-style
}

impl std::fmt::Display for CompressionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompressionType::Plain => write!(f, "plain"),
            CompressionType::Gzip => write!(f, "gzip"),
            CompressionType::Bgzf => write!(f, "bgzf"),
        }
    }
}

/// Detect compression type by reading first few bytes of the file.
pub fn detect_compression(path: &Path) -> std::io::Result<CompressionType> {
    use std::fs::File;
    use std::io::Read;

    let mut f = File::open(path)?;
    let mut header = [0u8; 18];
    let n = f.read(&mut header)?;
    if n < 2 {
        return Ok(CompressionType::Plain);
    }

    // Check gzip magic
    if header[0] != 0x1f || header[1] != 0x8b {
        return Ok(CompressionType::Plain);
    }

    // It's gzip; check BGZF signature.
    // BGZF: FLG byte (header[3]) has FEXTRA bit (0x04) set,
    // and the extra field starts at offset 10 with XLEN >= 6,
    // extra subfield ID SI1=0x42 ('B'), SI2=0x43 ('C'), SLEN=2.
    if n >= 18 && header[3] & 0x04 != 0 {
        // XLEN at bytes 10-11
        let xlen = u16::from_le_bytes([header[10], header[11]]) as usize;
        if xlen >= 6 && header[12] == 0x42 && header[13] == 0x43 {
            return Ok(CompressionType::Bgzf);
        }
    }

    Ok(CompressionType::Gzip)
}

/// Open a file for reading. Returns raw File; the caller wraps it
/// in whatever reader is appropriate based on CompressionType.
pub fn open_input(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}
