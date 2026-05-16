use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Decompress a gzipped reader into large chunks, respecting FASTQ record
/// boundaries so each chunk starts at the beginning of a FASTQ record.
///
/// Returns chunks via a crossbeam channel. Each chunk is a `Vec<u8>` of
/// decompressed FASTQ bytes aligned to a record boundary.
///
/// The chunk_bytes parameter controls decompressed chunk size (target size).
/// A small carry buffer is maintained across calls to handle boundary alignment.
pub struct GzipChunkedReader<R: Read> {
    inner: flate2::read::MultiGzDecoder<R>,
    buf: Vec<u8>,
    carry: Vec<u8>, // bytes from the tail of the previous chunk that belong to next record
    chunk_size: usize,
    pub bytes_decomp: Arc<AtomicU64>,
}

impl<R: Read> GzipChunkedReader<R> {
    pub fn new(inner: R, chunk_size: usize) -> Self {
        let decoder = flate2::read::MultiGzDecoder::new(inner);
        GzipChunkedReader {
            inner: decoder,
            buf: Vec::with_capacity(chunk_size + 4096),
            carry: Vec::new(),
            chunk_size,
            bytes_decomp: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Read and decompress the next aligned chunk.
    /// Returns Ok(None) when the stream is exhausted.
    /// Each returned `Vec<u8>` starts at the beginning of a FASTQ '@' record.
    pub fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        // Start chunk with leftover carry from previous boundary alignment
        let mut chunk = Vec::with_capacity(self.chunk_size + self.carry.len());
        chunk.extend_from_slice(&self.carry);
        self.carry.clear();

        // Read decompressed data until chunk_size reached or EOF
        self.buf.resize(self.chunk_size, 0);
        let mut total_read = 0;
        while total_read < self.chunk_size {
            match self.inner.read(&mut self.buf[total_read..]) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    total_read += n;
                }
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
        }
        self.bytes_decomp
            .fetch_add(total_read as u64, Ordering::Relaxed);
        chunk.extend_from_slice(&self.buf[..total_read]);

        if chunk.is_empty() {
            return Ok(None);
        }

        // If we hit EOF, return everything as the final chunk
        if total_read < self.chunk_size {
            return Ok(Some(chunk));
        }

        // Find FASTQ record boundary: scan backward from end for '\n@'
        // FASTQ records start with '@', so the boundary is just before '@'
        // after a newline (i.e., find the last '\n' followed by '@').
        let split_pos = find_fastq_split(&chunk);
        if split_pos == 0 || split_pos >= chunk.len() {
            // No boundary found — return whole chunk (last chunk at EOF)
            return Ok(Some(chunk));
        }

        // Everything from split_pos onward goes to carry
        self.carry.extend_from_slice(&chunk[split_pos..]);
        chunk.truncate(split_pos);
        Ok(Some(chunk))
    }
}

/// Plain (uncompressed) chunked reader with FASTQ boundary alignment.
pub struct PlainChunkedReader<R: Read> {
    inner: R,
    buf: Vec<u8>,
    carry: Vec<u8>,
    chunk_size: usize,
    pub bytes_decomp: Arc<AtomicU64>,
}

impl<R: Read> PlainChunkedReader<R> {
    pub fn new(inner: R, chunk_size: usize) -> Self {
        PlainChunkedReader {
            inner,
            buf: Vec::with_capacity(chunk_size + 4096),
            carry: Vec::new(),
            chunk_size,
            bytes_decomp: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut chunk = Vec::with_capacity(self.chunk_size + self.carry.len());
        chunk.extend_from_slice(&self.carry);
        self.carry.clear();

        self.buf.resize(self.chunk_size, 0);
        let mut total_read = 0;
        while total_read < self.chunk_size {
            match self.inner.read(&mut self.buf[total_read..]) {
                Ok(0) => break,
                Ok(n) => total_read += n,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
        }
        self.bytes_decomp
            .fetch_add(total_read as u64, Ordering::Relaxed);
        chunk.extend_from_slice(&self.buf[..total_read]);

        if chunk.is_empty() {
            return Ok(None);
        }
        if total_read < self.chunk_size {
            return Ok(Some(chunk));
        }

        let split_pos = find_fastq_split(&chunk);
        if split_pos == 0 || split_pos >= chunk.len() {
            return Ok(Some(chunk));
        }
        self.carry.extend_from_slice(&chunk[split_pos..]);
        chunk.truncate(split_pos);
        Ok(Some(chunk))
    }
}

/// Find the FASTQ record boundary by counting newlines modulo 4.
///
/// Each FASTQ record has exactly 4 lines (@header, seq, +, qual), so the
/// boundary between complete records always falls after the 4k-th newline.
/// This is correct even when quality lines start with '@', which the old
/// "\n@" backward scan would incorrectly treat as a new record header.
///
/// Returns the byte offset of the first byte of the last incomplete record
/// (i.e. `data[0..retval]` is a complete set of FASTQ records), or 0 if
/// no complete record was found.
fn find_fastq_split(data: &[u8]) -> usize {
    crate::count::bgzf::find_fastq_split_4lines(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_fastq_split() {
        // Two complete records: split should be at end (all data is complete)
        let data = b"@READ1\nACGT\n+\nIIII\n@READ2\nTTTT\n+\nIIII\n";
        let pos = find_fastq_split(data);
        // All 8 newlines — 2 complete records — boundary is at the end
        assert_eq!(pos, data.len(), "both records complete, split at end");

        // One complete + partial: split after quality of READ1
        let partial = b"@READ1\nACGT\n+\nIIII\n@READ2\nTTTT\n";
        let pos2 = find_fastq_split(partial);
        assert_eq!(
            pos2,
            b"@READ1\nACGT\n+\nIIII\n".len(),
            "split after first complete record"
        );
        // carry starts with '@'
        assert_eq!(partial[pos2], b'@');
    }

    #[test]
    fn test_plain_reader_chunks() {
        let input = b"@R1\nACGT\n+\nIIII\n@R2\nTTTT\n+\nIIII\n";
        let reader = std::io::Cursor::new(input.as_ref());
        let mut cr = PlainChunkedReader::new(reader, 1024);
        let chunk = cr.next_chunk().unwrap();
        assert!(chunk.is_some());
        assert!(cr.next_chunk().unwrap().is_none());
    }
}
