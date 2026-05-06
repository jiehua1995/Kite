use crossbeam_channel::Sender;
use flate2::read::GzDecoder;
use memmap2::Mmap;
/// BGZF (Block GZip Format) parallel decompressor for kite.
///
/// BGZF is a profile of gzip where each member block is independently
/// decompressible, capped at 65536 uncompressed bytes. This lets us:
///   1. Scan all block boundaries in one sequential pass (O(blocks), ~200K for 4GB)
///   2. Decompress all blocks in parallel using a rayon thread pool
///   3. Reassemble in-order and FASTQ-align for downstream workers
///
/// Key functions:
///   - `scan_bgzf_blocks`          : find all (offset, size) pairs
///   - `decompress_bgzf_parallel`  : full pipeline → sends chunks to channel
///   - `find_fastq_split_4lines`   : CORRECT FASTQ boundary by newline-counting
///
/// The correctness fix vs the old `\n@` heuristic:
///   FASTQ quality scores can contain `@`. Scanning backward for `\n@` can
///   mistake a quality line starting with `@` for a new record header.
///   Counting 4-newline groups is unconditionally correct because each FASTQ
///   record has exactly 4 newlines: header\nseq\n+\nqual\n.
use std::io::{self, Read};

// ─── BGZF constants ───────────────────────────────────────────────────────────

/// Minimum BGZF block header size. Standard gzip header (10 bytes) + XLEN (2 bytes)
/// + extra subfield BC (4 bytes) + BSIZE (2 bytes) = 18 bytes.
pub const BGZF_HEADER_MIN: usize = 18;

/// BGZF extra subfield ID bytes: 'B' = 0x42, 'C' = 0x43.
const BGZF_BC: [u8; 2] = [0x42, 0x43];

// ─── Block scanning ───────────────────────────────────────────────────────────

/// Verify that `data[pos..]` starts with a valid BGZF block header.
#[inline]
pub fn is_bgzf_header(data: &[u8], pos: usize) -> bool {
    if pos + BGZF_HEADER_MIN > data.len() {
        return false;
    }
    let h = &data[pos..pos + BGZF_HEADER_MIN];
    h[0] == 0x1f && h[1] == 0x8b        // gzip magic
        && h[3] & 0x04 != 0             // FLG has FEXTRA bit
        && h[12] == BGZF_BC[0]          // SI1 = 'B'
        && h[13] == BGZF_BC[1]          // SI2 = 'C'
        && h[14] == 0x02 && h[15] == 0x00 // SLEN = 2
}

/// Extract the total block size (compressed, including header, data, CRC32, ISIZE).
/// Reads the BSIZE field at offset 16–17 and adds 1 per spec.
#[inline]
pub fn bgzf_block_total_size(data: &[u8], pos: usize) -> usize {
    u16::from_le_bytes([data[pos + 16], data[pos + 17]]) as usize + 1
}

/// Scan the entire compressed buffer for BGZF block boundaries.
/// Returns a Vec of (byte_offset, block_size) pairs in file order.
///
/// This is O(n_blocks): it reads only 18-byte headers and skips entire blocks.
/// For a 4GB BGZF file with 200K blocks this is essentially instant.
pub fn scan_bgzf_blocks(data: &[u8]) -> io::Result<Vec<(usize, usize)>> {
    let mut blocks: Vec<(usize, usize)> = Vec::with_capacity(data.len() / 16_384);
    let mut pos = 0usize;
    while pos + BGZF_HEADER_MIN <= data.len() {
        if !is_bgzf_header(data, pos) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Non-BGZF data at offset {} (expected gzip+BGZF header)",
                    pos
                ),
            ));
        }
        let bsize = bgzf_block_total_size(data, pos);
        if bsize == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "BGZF block size is zero",
            ));
        }
        if pos + bsize > data.len() {
            // Truncated final block — skip it but don't error
            break;
        }
        blocks.push((pos, bsize));
        pos += bsize;
    }
    Ok(blocks)
}

// ─── Block decompression ──────────────────────────────────────────────────────

/// Decompress a single BGZF block using flate2 GzDecoder.
#[allow(dead_code)]
#[inline]
fn decompress_block(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(raw);
    let mut out = Vec::with_capacity(65536);
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

// ─── FASTQ boundary alignment ─────────────────────────────────────────────────

/// Find the last FASTQ-record boundary in `data` by counting newlines modulo 4.
///
/// Each FASTQ record is exactly 4 lines:
///   1. `@header\n`
///   2. `SEQUENCE\n`
///   3. `+comment\n`
///   4. `QUALITY\n`
///
/// After every 4th newline, we are at the start of a new record (or EOF).
/// Returns the byte offset of the first byte of the last incomplete record,
/// i.e. the byte immediately after the last 4k-th newline.
/// Returns 0 if no complete record was found.
///
/// # Correctness
/// This approach is immune to `@` appearing inside quality lines, making it
/// strictly more correct than scanning backward for `\n@`.
pub fn find_fastq_split_4lines(data: &[u8]) -> usize {
    let mut newlines = 0usize;
    let mut last_boundary = 0usize;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            newlines += 1;
            if newlines % 4 == 0 {
                last_boundary = i + 1; // first byte of next record (or EOF)
            }
        }
    }
    last_boundary
}

/// Variant: find the FASTQ split closest to but not exceeding `target_bytes`.
/// Used by the assembler to produce chunks of approximately `target_bytes`.
pub fn find_fastq_split_near(data: &[u8], target_bytes: usize) -> usize {
    // Count up to target_bytes + one max-record overhead (~2KB) to get the
    // last complete record boundary in that window.
    let scan_end = (target_bytes + 8192).min(data.len());
    let boundary = find_fastq_split_4lines(&data[..scan_end]);
    if boundary > 0 {
        return boundary;
    }
    // Very long read or tiny chunk — scan the entire buffer
    find_fastq_split_4lines(data)
}

// ─── Main parallel decompress entry point ────────────────────────────────────

/// Decompress a BGZF file in parallel, sending FASTQ-aligned chunks to `output_tx`.
///
/// # Thread model
/// Internally creates a rayon thread pool of `n_decomp_threads` threads. Each
/// thread decompresses a batch of BGZF blocks independently. The calling thread
/// assembles results in-order and FASTQ-aligns before sending to `output_tx`.
///
/// # Memory model
/// Processes `BATCH_BLOCKS` blocks at a time to cap peak memory use.
/// With BATCH_BLOCKS=4096 and avg block size 50KB decompressed, peak ≈ 200MB.
///
/// # Returns
/// `(compressed_bytes, decompressed_bytes)` for stats reporting.
pub fn decompress_bgzf_parallel(
    file_path: &str,
    n_decomp_threads: usize,
    chunk_target_bytes: usize,
    output_tx: &Sender<Vec<u8>>,
) -> io::Result<(u64, u64)> {
    use std::fs::File;

    let file = File::open(file_path)?;
    let compressed_size = file.metadata()?.len();

    // Memory-map the compressed file.
    // Mmap is O(1) — the OS lazily loads pages as we access them.
    // SAFETY: the file is not modified while we hold the map.
    let mmap = unsafe { Mmap::map(&file)? };

    // Scan all block boundaries (O(n_blocks) sequential pass).
    let blocks = scan_bgzf_blocks(&mmap)?;
    let total_blocks = blocks.len();

    if total_blocks == 0 {
        return Ok((compressed_size, 0));
    }

    // Build a rayon thread pool dedicated to BGZF decompression.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(n_decomp_threads.max(1))
        .build()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    // Process in batches to bound peak memory.
    // Each batch: decompress in parallel, then assemble in-order.
    // Larger batch = fewer sequential carry-assembly phases,
    // which is the only serial section in the BGZF pipeline.
    // 8192 blocks × 65 KB ≈ 512 MB per batch; still well within RAM.
    const BATCH_BLOCKS: usize = 8192;

    let mut decompressed_total = 0u64;
    let mut carry: Vec<u8> = Vec::with_capacity(chunk_target_bytes + 131072);
    let mut batch_start = 0usize;

    // Pre-allocate decompression output buffers — one per slot in a batch.
    // Reusing these avoids 365 K mmap/munmap cycles from per-block Vec allocs.
    let mut block_bufs: Vec<Vec<u8>> = (0..BATCH_BLOCKS)
        .map(|_| Vec::with_capacity(66000)) // BGZF block ≤ 65536 bytes
        .collect();

    while batch_start < total_blocks {
        let batch_end = (batch_start + BATCH_BLOCKS).min(total_blocks);
        let batch = &blocks[batch_start..batch_end];
        let n_batch = batch.len();

        // Parallel decompress this batch into pre-owned buffers.
        // SAFETY: `mmap` is Sync; each thread reads a disjoint slice.
        use rayon::prelude::*;
        pool.install(|| {
            batch
                .par_iter()
                .zip(block_bufs[..n_batch].par_iter_mut())
                .for_each(|(&(offset, size), buf)| {
                    buf.clear();
                    let raw = &mmap[offset..offset + size];
                    let _ = GzDecoder::new(raw).read_to_end(buf);
                });
        });

        // Append decompressed data to carry, then emit FASTQ-aligned chunks.
        for buf in &block_bufs[..n_batch] {
            if buf.is_empty() {
                continue;
            }
            decompressed_total += buf.len() as u64;
            carry.extend_from_slice(buf);

            while carry.len() >= chunk_target_bytes {
                let split = find_fastq_split_near(&carry, chunk_target_bytes);
                if split == 0 {
                    let full_split = find_fastq_split_4lines(&carry);
                    if full_split == 0 {
                        break;
                    }
                    let chunk = carry[..full_split].to_vec();
                    let leftover = carry.len() - full_split;
                    if leftover > 0 {
                        carry.copy_within(full_split.., 0);
                    }
                    carry.truncate(leftover);
                    if output_tx.send(chunk).is_err() {
                        return Ok((compressed_size, decompressed_total));
                    }
                    break;
                }
                let chunk = carry[..split].to_vec();
                let leftover = carry.len() - split;
                if leftover > 0 {
                    carry.copy_within(split.., 0);
                }
                carry.truncate(leftover);
                if output_tx.send(chunk).is_err() {
                    return Ok((compressed_size, decompressed_total));
                }
            }
        }

        batch_start = batch_end;
    }

    if !carry.is_empty() {
        let _ = output_tx.send(carry);
    }

    Ok((compressed_size, decompressed_total))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_fastq_split_4lines_basic() {
        // 2 complete FASTQ records
        let data = b"@r1\nACGT\n+\nIIII\n@r2\nACGT\n+\nIIII\n";
        let split = find_fastq_split_4lines(data);
        // All 8 newlines → 2 complete records → boundary at end
        assert_eq!(split, data.len(), "all data is complete records");
    }

    #[test]
    fn test_find_fastq_split_4lines_partial() {
        // 1 complete + 1 partial (just the header)
        let data = b"@r1\nACGT\n+\nIIII\n@r2\n";
        let split = find_fastq_split_4lines(data);
        // 4 newlines = 1 complete record → boundary after quality of r1
        assert_eq!(split, b"@r1\nACGT\n+\nIIII\n".len());
        // carry starts with "@r2\n"
        assert_eq!(&data[split..], b"@r2\n");
    }

    #[test]
    fn test_find_fastq_split_4lines_quality_at_sign() {
        // Quality line contains '@' — should NOT confuse the splitter
        let data = b"@r1\nACGT\n+\n@III\n@r2\nACGT\n+\nIIII\n";
        let split = find_fastq_split_4lines(data);
        assert_eq!(
            split,
            data.len(),
            "all data is complete records despite @ in quality"
        );
    }

    #[test]
    fn test_find_fastq_split_4lines_partial_with_quality_at() {
        // 1 complete record where quality has '@', then partial header
        let data = b"@r1\nACGT\n+\n@III\n@r2\n";
        let split = find_fastq_split_4lines(data);
        assert_eq!(split, b"@r1\nACGT\n+\n@III\n".len());
        assert_eq!(&data[split..], b"@r2\n");
    }

    #[test]
    fn test_find_fastq_split_4lines_empty() {
        assert_eq!(find_fastq_split_4lines(b""), 0);
    }

    #[test]
    fn test_find_fastq_split_4lines_no_complete() {
        let data = b"@r1\nACGT\n";
        assert_eq!(find_fastq_split_4lines(data), 0);
    }

    #[test]
    fn test_scan_bgzf_blocks_eof_detection() {
        // Can't test without a real BGZF file, but verify function exists
        // and returns Ok on empty data.
        let result = scan_bgzf_blocks(b"");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }
}
