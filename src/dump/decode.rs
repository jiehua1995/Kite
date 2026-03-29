use super::kmer::{
    INVALID_BASE, decode_packed_fast_into, decode_u64_fast_into, decode_u128_fast_into, encode_base,
};
use crate::utils::kite::{
    FLAG_BLOCK_FRAMES, FLAG_BLOCK_TSV_BYTES, FLAG_ZSTD, KiteInfo, KiteStats, read_info_from_reader,
};
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, bounded};
use itoa::Buffer as ItoaBuffer;
use memmap2::MmapOptions;
use std::collections::{BTreeMap, HashMap as AHashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread;

use crate::utils::kite::KmerMode;

// 中文：这里聚合 `.kite` 的读取和解码逻辑，供 dump/query/histo/info/profile/sketch
// 这些读路径共享；重点是保持读流程流式化，并尽量利用 block-frame 快路径。
// English: This file centralizes `.kite` read/decode logic for dump/query/
// histo/info/profile/sketch. The main goals are streaming reads and reuse of
// block-frame fast paths whenever possible.
const RECORD_CHUNK_LEN: usize = 512 * 1024;
const BYTE_STREAM_RECORD_CHUNK_LEN: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct BlockFrame {
    records: u32,
    raw_bytes: u32,
    compressed_bytes: u32,
    tsv_bytes: u32,
    data_offset: u64,
}

#[derive(Debug, Clone, Copy)]
struct BlockLayout {
    output_offset: usize,
    output_len: usize,
}

struct CountCache {
    lens: Vec<u8>,
    digits: Vec<[u8; 5]>,
}

/// 中文：把整个 `.kite` 数据库解码导出为 TSV。
/// English: Decode and export a whole `.kite` database as TSV.
pub fn dump_binary_to_tsv(input: &Path, output: &Path, threads: usize) -> Result<()> {
    let f = File::open(input).with_context(|| format!("Failed to open {}", input.display()))?;
    let mut r = BufReader::with_capacity(1 << 20, f);
    let info = read_info_from_reader(&mut r)?;
    match info.mode {
        KmerMode::U64 => {
            if is_block_framed(&info) {
                dump_u64_blocked_parallel(input, output, &info, threads.max(1))
            } else if u64_is_delta_varint(&info) {
                let out = File::create(output)
                    .with_context(|| format!("Failed to create {}", output.display()))?;
                let writer = BufWriter::with_capacity(16 << 20, out);
                dump_u64_stream(&mut r, &info, writer, threads.max(1))
            } else {
                let out = File::create(output)
                    .with_context(|| format!("Failed to create {}", output.display()))?;
                let writer = BufWriter::with_capacity(16 << 20, out);
                dump_u64_fixed_parallel(&mut r, &info, writer, threads.max(1))
            }
        }
        KmerMode::U128 => {
            if is_block_framed(&info) {
                dump_u128_blocked_parallel(input, output, &info, threads.max(1))
            } else {
                let out = File::create(output)
                    .with_context(|| format!("Failed to create {}", output.display()))?;
                let writer = BufWriter::with_capacity(16 << 20, out);
                dump_u128_parallel(&mut r, &info, writer, threads.max(1))
            }
        }
        KmerMode::Packed(_) => {
            let out = File::create(output)
                .with_context(|| format!("Failed to create {}", output.display()))?;
            let writer = BufWriter::with_capacity(16 << 20, out);
            dump_packed_parallel(&mut r, &info, writer, threads.max(1))
        }
    }
}

/// 中文：查询单个 k-mer 的计数。
/// English: Query the count of one exact k-mer.
pub fn query_kmer(path: &Path, kmer: &str) -> Result<u64> {
    let f = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut r = BufReader::with_capacity(1 << 20, f);
    let info = read_info_from_reader(&mut r)?;

    if kmer.len() != info.k {
        bail!(
            "query k-mer length {} does not match file k={}",
            kmer.len(),
            info.k
        );
    }

    let query = encode_query_canonical(kmer.as_bytes(), info.k, info.mode, info.words)?;
    query_reader(&mut r, &info, query)
}

#[derive(Debug, Clone)]
pub struct HistogramSummary {
    pub info: KiteInfo,
    pub distinct_bins: usize,
    pub total_count: u64,
    pub singleton_kmers: u64,
    pub max_count: u64,
}

/// 中文：构建并写出计数直方图，同时返回摘要统计。
/// English: Build and write a count histogram while also returning summary
/// statistics.
pub fn write_histogram(path: &Path, out_path: &Path) -> Result<HistogramSummary> {
    let f = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut r = BufReader::with_capacity(1 << 20, f);
    let info = read_info_from_reader(&mut r)?;
    let mut histogram = AHashMap::<u64, u64>::new();
    let mut total_count = 0u64;
    let mut singleton_kmers = 0u64;
    let mut max_count = 0u64;
    visit_counts_reader(&mut r, &info, |count| {
        *histogram.entry(count).or_insert(0) += 1;
        total_count += count;
        if count == 1 {
            singleton_kmers += 1;
        }
        max_count = max_count.max(count);
        Ok(())
    })?;
    let mut bins: Vec<(u64, u64)> = histogram.into_iter().collect();
    bins.sort_unstable_by_key(|&(count, _)| count);
    let distinct_bins = bins.len();

    let out = File::create(out_path)
        .with_context(|| format!("Failed to create {}", out_path.display()))?;
    let mut w = BufWriter::with_capacity(1 << 20, out);
    writeln!(w, "count\tnum_kmers")?;
    for (count, num_kmers) in bins {
        writeln!(w, "{count}\t{num_kmers}")?;
    }
    w.flush()?;
    Ok(HistogramSummary {
        info,
        distinct_bins,
        total_count,
        singleton_kmers,
        max_count,
    })
}

#[allow(dead_code)]
/// 中文：把 `u64` 数据库读取成频率哈希表。
/// English: Read a `u64` database into a normalized frequency hash map.
pub fn read_u64_frequency_map(path: &Path) -> Result<AHashMap<u64, f64>> {
    let (_info, entries, total) = read_u64_count_vec(path)?;
    let denom = (total as f64).max(1.0);
    let mut map = AHashMap::with_capacity(entries.len());
    for (kmer, count) in entries {
        map.insert(kmer, count as f64 / denom);
    }
    Ok(map)
}

/// 中文：把 `u64` 数据库完整读成有序 `(kmer,count)` 向量。
/// English: Read a `u64` database fully into a sorted `(kmer, count)` vector.
pub fn read_u64_count_vec(path: &Path) -> Result<(KiteInfo, Vec<(u64, u64)>, u64)> {
    let f = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut r = BufReader::with_capacity(1 << 20, f);
    let info = read_info_from_reader(&mut r)?;
    if !matches!(info.mode, KmerMode::U64) {
        bail!("profile currently requires u64 k-mer database (k <= 32)");
    }

    let mut entries = Vec::<(u64, u64)>::with_capacity(info.entries as usize);
    let mut total = 0u64;
    read_u64_entries(&mut r, &info, |kmer, count| {
        total += count;
        entries.push((kmer, count));
        Ok(())
    })?;
    Ok((info, entries, total))
}

/// 中文：扫描数据库并汇总总计数、singleton 和最大计数。
/// English: Scan the database and summarize total count, singleton count, and
/// maximum count.
pub fn collect_stats(path: &Path) -> Result<KiteStats> {
    let f = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut r = BufReader::with_capacity(1 << 20, f);
    let info = read_info_from_reader(&mut r)?;

    let mut total_count = 0u64;
    let mut singleton_kmers = 0u64;
    let mut max_count = 0u64;
    visit_counts_reader(&mut r, &info, |count| {
        total_count += count;
        if count == 1 {
            singleton_kmers += 1;
        }
        max_count = max_count.max(count);
        Ok(())
    })?;

    Ok(KiteStats {
        info,
        total_count,
        singleton_kmers,
        max_count,
    })
}

/// 中文：流式导出 `u64` 记录，适用于普通非 block-framed 路径。
/// English: Stream-dump `u64` records for the regular non-block-framed path.
fn dump_u64_stream<R: Read + Seek, W: Write + Send + 'static>(
    reader: &mut R,
    info: &KiteInfo,
    writer: BufWriter<W>,
    threads: usize,
) -> Result<()> {
    let (tx_in, rx_in) = bounded::<(usize, Vec<(u64, u64)>)>(32);
    let (tx_out, rx_out) = bounded::<(usize, Vec<u8>)>(32);

    let writer_handle = spawn_writer(writer, rx_out);
    let worker_handles = spawn_u64_workers(info.k, threads, rx_in, tx_out.clone());

    let mut chunk = Vec::<(u64, u64)>::with_capacity(RECORD_CHUNK_LEN);
    let mut chunk_id = 0usize;
    read_u64_entries(reader, info, |kmer, count| {
        chunk.push((kmer, count));
        if chunk.len() >= RECORD_CHUNK_LEN {
            tx_in
                .send((chunk_id, std::mem::take(&mut chunk)))
                .map_err(|_| anyhow::anyhow!("dump worker channel closed"))?;
            chunk = Vec::with_capacity(RECORD_CHUNK_LEN);
            chunk_id += 1;
        }
        Ok(())
    })?;

    if !chunk.is_empty() {
        tx_in
            .send((chunk_id, chunk))
            .map_err(|_| anyhow::anyhow!("dump worker channel closed"))?;
    }

    drop(tx_in);
    join_workers(worker_handles)?;
    drop(tx_out);
    writer_handle
        .join()
        .map_err(|_| anyhow::anyhow!("dump writer panicked"))??;
    Ok(())
}

/// 中文：并行导出定长 `u64` 记录。
/// English: Dump fixed-width `u64` records in parallel.
fn dump_u64_fixed_parallel<R: Read + Seek, W: Write + Send + 'static>(
    reader: &mut R,
    info: &KiteInfo,
    writer: BufWriter<W>,
    threads: usize,
) -> Result<()> {
    let k = info.k;
    dump_byte_stream_parallel(
        reader,
        info,
        16,
        writer,
        threads,
        k + 24,
        move |chunk, out| {
            let mut seq = vec![0u8; k];
            let mut offset = 0usize;
            let mut itoa_buf = ItoaBuffer::new();
            while offset + 16 <= chunk.len() {
                let kmer = read_u64_le_unaligned(&chunk[offset..]);
                let count = read_u64_le_unaligned(&chunk[offset + 8..]);
                decode_u64_fast_into(kmer, k, &mut seq);
                append_tsv_record(out, &seq, count, &mut itoa_buf);
                offset += 16;
            }
        },
    )
}

/// 中文：并行导出 block-framed `u64` 数据。
/// English: Dump block-framed `u64` data in parallel.
fn dump_u64_blocked_parallel(
    path: &Path,
    output: &Path,
    info: &KiteInfo,
    threads: usize,
) -> Result<()> {
    let blocks = read_block_frames(path, info)?;
    let k = info.k;
    if has_block_tsv_bytes(info) {
        let layout = compute_block_layouts(&blocks);
        write_blocks_u64_mmap(path, output, &blocks, &layout, k, threads)
    } else {
        let out = File::create(output)
            .with_context(|| format!("Failed to create {}", output.display()))?;
        let writer = BufWriter::with_capacity(16 << 20, out);
        dump_u64_blocked_stream(path, &blocks, writer, k, threads)
    }
}

/// 中文：并行导出 `u128` 数据。
/// English: Dump `u128` data in parallel.
fn dump_u128_parallel<R: Read + Seek, W: Write + Send + 'static>(
    reader: &mut R,
    info: &KiteInfo,
    writer: BufWriter<W>,
    threads: usize,
) -> Result<()> {
    let record_size = 24usize;
    let k = info.k;
    dump_byte_stream_parallel(
        reader,
        info,
        record_size,
        writer,
        threads,
        k + 24,
        move |chunk, out| {
            let mut seq = vec![0u8; k];
            let mut offset = 0usize;
            let mut itoa_buf = ItoaBuffer::new();
            while offset + record_size <= chunk.len() {
                let kmer = read_u128_le_unaligned(&chunk[offset..]);
                let count = read_u64_le_unaligned(&chunk[offset + 16..]);
                decode_u128_fast_into(kmer, k, &mut seq);
                append_tsv_record(out, &seq, count, &mut itoa_buf);
                offset += record_size;
            }
        },
    )?;
    Ok(())
}

/// 中文：并行导出 block-framed `u128` 数据。
/// English: Dump block-framed `u128` data in parallel.
fn dump_u128_blocked_parallel(
    path: &Path,
    output: &Path,
    info: &KiteInfo,
    threads: usize,
) -> Result<()> {
    let blocks = read_block_frames(path, info)?;
    let k = info.k;
    if has_block_tsv_bytes(info) {
        let layout = compute_block_layouts(&blocks);
        write_blocks_u128_mmap(path, output, &blocks, &layout, k, threads)
    } else {
        let out = File::create(output)
            .with_context(|| format!("Failed to create {}", output.display()))?;
        let writer = BufWriter::with_capacity(16 << 20, out);
        dump_u128_blocked_stream(path, &blocks, writer, k, threads)
    }
}

/// 中文：并行导出 packed 长 k-mer 数据。
/// English: Dump packed long-k-mer data in parallel.
fn dump_packed_parallel<R: Read + Seek, W: Write + Send + 'static>(
    reader: &mut R,
    info: &KiteInfo,
    writer: BufWriter<W>,
    threads: usize,
) -> Result<()> {
    let record_size = info.words * 8 + 8;
    let k = info.k;
    let words = info.words;
    dump_byte_stream_parallel(
        reader,
        info,
        record_size,
        writer,
        threads,
        k + 24,
        move |chunk, out| {
            let mut seq = vec![0u8; k];
            let mut arr = vec![0u64; words.max(1)];
            let mut offset = 0usize;
            let mut itoa_buf = ItoaBuffer::new();
            while offset + record_size <= chunk.len() {
                for (word_idx, word) in arr.iter_mut().enumerate().take(words) {
                    let start = offset + word_idx * 8;
                    *word = read_u64_le_unaligned(&chunk[start..]);
                }
                let count_start = offset + words * 8;
                let count = read_u64_le_unaligned(&chunk[count_start..]);
                decode_packed_fast_into(&arr, k, &mut seq);
                append_tsv_record(out, &seq, count, &mut itoa_buf);
                offset += record_size;
            }
        },
    )
}

/// 中文：通用的字节流并行导出框架。
/// English: Generic parallel byte-stream dump framework.
fn dump_byte_stream_parallel<R, W, F>(
    reader: &mut R,
    info: &KiteInfo,
    record_size: usize,
    writer: BufWriter<W>,
    threads: usize,
    output_record_bytes: usize,
    worker: F,
) -> Result<()>
where
    R: Read + Seek,
    W: Write + Send + 'static,
    F: Fn(&[u8], &mut Vec<u8>) + Copy + Send + 'static,
{
    let chunk_records = if record_size <= 16 {
        RECORD_CHUNK_LEN
    } else {
        BYTE_STREAM_RECORD_CHUNK_LEN
    };
    let chunk_bytes = chunk_records * record_size;
    let (tx_in, rx_in) = bounded::<(usize, Vec<u8>)>(32);
    let (tx_out, rx_out) = bounded::<(usize, Vec<u8>)>(32);
    let writer_handle = spawn_writer(writer, rx_out);

    let mut worker_handles = Vec::with_capacity(threads);
    for _ in 0..threads {
        let rx = rx_in.clone();
        let tx = tx_out.clone();
        let output_record_bytes = output_record_bytes;
        worker_handles.push(thread::spawn(move || -> Result<()> {
            while let Ok((chunk_id, chunk)) = rx.recv() {
                let records = (chunk.len() / record_size).max(1);
                let mut out_buf = Vec::with_capacity(records * output_record_bytes);
                worker(&chunk, &mut out_buf);
                tx.send((chunk_id, out_buf))
                    .map_err(|_| anyhow::anyhow!("dump writer channel closed"))?;
            }
            Ok(())
        }));
    }

    let mut input = open_payload_reader(reader, info)?;
    let mut chunk_id = 0usize;
    loop {
        let mut chunk = vec![0u8; chunk_bytes];
        let mut filled = 0usize;
        while filled < chunk_bytes {
            let n = input.read(&mut chunk[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        let aligned = filled - (filled % record_size);
        if aligned == 0 {
            bail!("corrupted payload: record chunk not aligned to record size");
        }
        chunk.truncate(aligned);
        tx_in
            .send((chunk_id, chunk))
            .map_err(|_| anyhow::anyhow!("dump worker channel closed"))?;
        chunk_id += 1;
    }

    drop(tx_in);
    join_workers(worker_handles)?;
    drop(tx_out);
    writer_handle
        .join()
        .map_err(|_| anyhow::anyhow!("dump writer panicked"))??;
    Ok(())
}

/// 中文：通用的 block-frame 并行导出框架。
/// English: Generic parallel dump framework for block-framed payloads.
fn dump_block_frames_parallel<W, F>(
    path: &Path,
    blocks: &[BlockFrame],
    writer: BufWriter<W>,
    threads: usize,
    worker: F,
) -> Result<()>
where
    W: Write + Send + 'static,
    F: Fn(&[u8], &mut Vec<u8>) + Copy + Send + 'static,
{
    let (tx_in, rx_in) = bounded::<(usize, BlockFrame)>(threads.max(2));
    let (tx_out, rx_out) = bounded::<(usize, Vec<u8>)>(threads.max(2));
    let writer_handle = spawn_writer(writer, rx_out);

    let mut worker_handles = Vec::with_capacity(threads);
    for _ in 0..threads {
        let rx = rx_in.clone();
        let tx = tx_out.clone();
        let path = path.to_path_buf();
        worker_handles.push(thread::spawn(move || -> Result<()> {
            let mut file = File::open(&path)?;
            while let Ok((chunk_id, block)) = rx.recv() {
                file.seek(SeekFrom::Start(block.data_offset))?;
                let mut compressed = vec![0u8; block.compressed_bytes as usize];
                file.read_exact(&mut compressed)?;
                let raw = zstd::bulk::decompress(&compressed, block.raw_bytes as usize)?;
                let mut out = Vec::with_capacity(block.records as usize * 64);
                worker(&raw, &mut out);
                tx.send((chunk_id, out))
                    .map_err(|_| anyhow::anyhow!("dump writer channel closed"))?;
            }
            Ok(())
        }));
    }

    for (idx, &block) in blocks.iter().enumerate() {
        tx_in
            .send((idx, block))
            .map_err(|_| anyhow::anyhow!("dump worker channel closed"))?;
    }

    drop(tx_in);
    join_workers(worker_handles)?;
    drop(tx_out);
    writer_handle
        .join()
        .map_err(|_| anyhow::anyhow!("dump writer panicked"))??;
    Ok(())
}

/// 中文：根据 block 头信息计算每个 block 在输出文件中的偏移和长度。
/// English: Compute output offsets and lengths for each block from block-frame
/// metadata.
fn compute_block_layouts(blocks: &[BlockFrame]) -> Vec<BlockLayout> {
    let mut layouts = Vec::with_capacity(blocks.len());
    let mut offset = 0usize;
    for block in blocks {
        let output_len = block.tsv_bytes as usize;
        layouts.push(BlockLayout {
            output_offset: offset,
            output_len,
        });
        offset += output_len;
    }
    layouts
}

/// 中文：用 mmap 把 `u64` block 直接写入目标文件的正确位置。
/// English: Use mmap to place each `u64` block directly at the correct output
/// position.
fn write_blocks_u64_mmap(
    path: &Path,
    output: &Path,
    blocks: &[BlockFrame],
    layouts: &[BlockLayout],
    k: usize,
    threads: usize,
) -> Result<()> {
    let total_size = layouts
        .last()
        .map(|l| l.output_offset + l.output_len)
        .unwrap_or(0);
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(output)?;
    file.set_len(total_size as u64)?;
    let mut mmap = unsafe { MmapOptions::new().len(total_size).map_mut(&file)? };
    let base_addr = mmap.as_mut_ptr() as usize;

    let worker_count = threads.max(1).min(blocks.len().max(1));
    std::thread::scope(|scope| -> Result<()> {
        let mut start = 0usize;
        let mut handles = Vec::with_capacity(worker_count);
        for worker_idx in 0..worker_count {
            let path = path.to_path_buf();
            let cache = CountCache::new();
            let chunk_len = (blocks.len() + worker_count - 1 - worker_idx) / worker_count;
            let end = (start + chunk_len).min(blocks.len());
            let block_slice = &blocks[start..end];
            let layout_slice = &layouts[start..end];
            let handle = scope.spawn(move || -> Result<()> {
                let mut file = File::open(&path)?;
                let mut seq = vec![0u8; k];
                let mut itoa_buf = ItoaBuffer::new();
                for (&block, &layout) in block_slice.iter().zip(layout_slice.iter()) {
                    let raw = read_block_raw_from_file(&mut file, block)?;
                    let mut raw_offset = 0usize;
                    let mut written = 0usize;
                    let out_ptr = unsafe { (base_addr as *mut u8).add(layout.output_offset) };
                    for _ in 0..block.records {
                        let kmer = read_u64_le_unaligned(&raw[raw_offset..]);
                        let count = read_u64_le_unaligned(&raw[raw_offset + 8..]);
                        decode_u64_fast_into(kmer, k, &mut seq);
                        unsafe {
                            std::ptr::copy_nonoverlapping(seq.as_ptr(), out_ptr.add(written), k);
                        }
                        written += k;
                        unsafe {
                            *out_ptr.add(written) = b'\t';
                        }
                        written += 1;
                        let count_bytes = cache.bytes(count, &mut itoa_buf);
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                count_bytes.as_ptr(),
                                out_ptr.add(written),
                                count_bytes.len(),
                            );
                        }
                        written += count_bytes.len();
                        unsafe {
                            *out_ptr.add(written) = b'\n';
                        }
                        written += 1;
                        raw_offset += 16;
                    }
                    debug_assert_eq!(written, layout.output_len);
                }
                Ok(())
            });
            handles.push(handle);
            start = end;
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("dump worker panicked"))??;
        }
        Ok(())
    })?;
    drop(mmap);
    Ok(())
}

/// 中文：用 mmap 把 `u128` block 直接写入目标文件的正确位置。
/// English: Use mmap to place each `u128` block directly at the correct output
/// position.
fn write_blocks_u128_mmap(
    path: &Path,
    output: &Path,
    blocks: &[BlockFrame],
    layouts: &[BlockLayout],
    k: usize,
    threads: usize,
) -> Result<()> {
    let total_size = layouts
        .last()
        .map(|l| l.output_offset + l.output_len)
        .unwrap_or(0);
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(output)?;
    file.set_len(total_size as u64)?;
    let mut mmap = unsafe { MmapOptions::new().len(total_size).map_mut(&file)? };
    let base_addr = mmap.as_mut_ptr() as usize;

    let worker_count = threads.max(1).min(blocks.len().max(1));
    std::thread::scope(|scope| -> Result<()> {
        let mut start = 0usize;
        let mut handles = Vec::with_capacity(worker_count);
        for worker_idx in 0..worker_count {
            let path = path.to_path_buf();
            let cache = CountCache::new();
            let chunk_len = (blocks.len() + worker_count - 1 - worker_idx) / worker_count;
            let end = (start + chunk_len).min(blocks.len());
            let block_slice = &blocks[start..end];
            let layout_slice = &layouts[start..end];
            let handle = scope.spawn(move || -> Result<()> {
                let mut file = File::open(&path)?;
                let mut seq = vec![0u8; k];
                let mut itoa_buf = ItoaBuffer::new();
                for (&block, &layout) in block_slice.iter().zip(layout_slice.iter()) {
                    let raw = read_block_raw_from_file(&mut file, block)?;
                    let mut raw_offset = 0usize;
                    let mut written = 0usize;
                    let out_ptr = unsafe { (base_addr as *mut u8).add(layout.output_offset) };
                    for _ in 0..block.records {
                        let kmer = read_u128_le_unaligned(&raw[raw_offset..]);
                        let count = read_u64_le_unaligned(&raw[raw_offset + 16..]);
                        decode_u128_fast_into(kmer, k, &mut seq);
                        unsafe {
                            std::ptr::copy_nonoverlapping(seq.as_ptr(), out_ptr.add(written), k);
                        }
                        written += k;
                        unsafe {
                            *out_ptr.add(written) = b'\t';
                        }
                        written += 1;
                        let count_bytes = cache.bytes(count, &mut itoa_buf);
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                count_bytes.as_ptr(),
                                out_ptr.add(written),
                                count_bytes.len(),
                            );
                        }
                        written += count_bytes.len();
                        unsafe {
                            *out_ptr.add(written) = b'\n';
                        }
                        written += 1;
                        raw_offset += 24;
                    }
                    debug_assert_eq!(written, layout.output_len);
                }
                Ok(())
            });
            handles.push(handle);
            start = end;
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("dump worker panicked"))??;
        }
        Ok(())
    })?;
    drop(mmap);
    Ok(())
}

/// 中文：从磁盘读取并解压一个 block frame。
/// English: Read and decompress one block frame from disk.
fn read_block_raw_from_file(file: &mut File, block: BlockFrame) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(block.data_offset))?;
    let mut compressed = vec![0u8; block.compressed_bytes as usize];
    file.read_exact(&mut compressed)?;
    Ok(zstd::bulk::decompress(
        &compressed,
        block.raw_bytes as usize,
    )?)
}

impl CountCache {
    /// 中文：创建一个小整数计数文本缓存，减少高频 `itoa` 开销。
    /// English: Create a small-integer text cache to reduce repeated `itoa`
    /// work for common count values.
    fn new() -> Self {
        let mut lens = vec![0u8; 65_536];
        let mut digits = vec![[0u8; 5]; 65_536];
        for value in 0u32..=65_535 {
            let s = value.to_string();
            lens[value as usize] = s.len() as u8;
            digits[value as usize][..s.len()].copy_from_slice(s.as_bytes());
        }
        Self { lens, digits }
    }

    #[inline(always)]
    /// 中文：返回计数的 ASCII 字节切片；小值直接命中缓存。
    /// English: Return an ASCII byte slice for a count; small values are served
    /// directly from cache.
    fn bytes<'a>(&'a self, value: u64, itoa_buf: &'a mut ItoaBuffer) -> &'a [u8] {
        if value <= 65_535 {
            let idx = value as usize;
            &self.digits[idx][..self.lens[idx] as usize]
        } else {
            itoa_buf.format(value).as_bytes()
        }
    }
}

/// 中文：按 block-stream 方式导出 `u64` 数据，减少内存峰值。
/// English: Dump `u64` data through a block-stream path to reduce peak memory.
fn dump_u64_blocked_stream<W: Write + Send + 'static>(
    path: &Path,
    blocks: &[BlockFrame],
    writer: BufWriter<W>,
    k: usize,
    threads: usize,
) -> Result<()> {
    dump_block_frames_parallel(path, blocks, writer, threads, move |raw, out| {
        let cache = CountCache::new();
        let mut seq = vec![0u8; k];
        let mut itoa_buf = ItoaBuffer::new();
        let records = raw.len() / 16;
        out.reserve(records * (k + 8));
        let mut offset = 0usize;
        while offset + 16 <= raw.len() {
            let kmer = read_u64_le_unaligned(&raw[offset..]);
            let count = read_u64_le_unaligned(&raw[offset + 8..]);
            decode_u64_fast_into(kmer, k, &mut seq);
            out.extend_from_slice(&seq);
            out.push(b'\t');
            out.extend_from_slice(cache.bytes(count, &mut itoa_buf));
            out.push(b'\n');
            offset += 16;
        }
    })
}

/// 中文：按 block-stream 方式导出 `u128` 数据。
/// English: Dump `u128` data through a block-stream path.
fn dump_u128_blocked_stream<W: Write + Send + 'static>(
    path: &Path,
    blocks: &[BlockFrame],
    writer: BufWriter<W>,
    k: usize,
    threads: usize,
) -> Result<()> {
    dump_block_frames_parallel(path, blocks, writer, threads, move |raw, out| {
        let cache = CountCache::new();
        let mut seq = vec![0u8; k];
        let mut itoa_buf = ItoaBuffer::new();
        let records = raw.len() / 24;
        out.reserve(records * (k + 8));
        let mut offset = 0usize;
        while offset + 24 <= raw.len() {
            let kmer = read_u128_le_unaligned(&raw[offset..]);
            let count = read_u64_le_unaligned(&raw[offset + 16..]);
            decode_u128_fast_into(kmer, k, &mut seq);
            out.extend_from_slice(&seq);
            out.push(b'\t');
            out.extend_from_slice(cache.bytes(count, &mut itoa_buf));
            out.push(b'\n');
            offset += 24;
        }
    })
}

enum QueryKey {
    U64(u64),
    U128(u128),
    Packed(Vec<u64>),
}

/// 中文：把查询字符串编码成内部可比较键，并自动取 canonical。
/// English: Encode a query string into the internal comparable key while also
/// applying canonicalization.
fn encode_query_canonical(seq: &[u8], k: usize, mode: KmerMode, words: usize) -> Result<QueryKey> {
    match mode {
        KmerMode::U64 => {
            let mut forward = 0u64;
            let mut reverse = 0u64;
            for &ch in seq {
                let b = encode_base(ch);
                if b == INVALID_BASE {
                    bail!(
                        "query contains invalid base '{}'; only A/C/G/T/U are supported",
                        ch as char
                    );
                }
                forward = (forward << 2) | b as u64;
                reverse = (reverse >> 2) | (((3 - b) as u64) << ((k - 1) * 2));
            }
            Ok(QueryKey::U64(forward.min(reverse)))
        }
        KmerMode::U128 => {
            let mut forward = 0u128;
            let mut reverse = 0u128;
            for &ch in seq {
                let b = encode_base(ch);
                if b == INVALID_BASE {
                    bail!(
                        "query contains invalid base '{}'; only A/C/G/T/U are supported",
                        ch as char
                    );
                }
                forward = (forward << 2) | b as u128;
                reverse = (reverse >> 2) | (((3 - b) as u128) << ((k - 1) * 2));
            }
            Ok(QueryKey::U128(forward.min(reverse)))
        }
        KmerMode::Packed(_) => {
            let mut forward = vec![0u64; words];
            let mut reverse = vec![0u64; words];
            let total_bits = k * 2;
            let high_word_bits = total_bits.saturating_sub((words.saturating_sub(1)) * 64);
            let top_mask = if high_word_bits >= 64 {
                u64::MAX
            } else {
                (1u64 << high_word_bits) - 1
            };
            let top_bit_pos = total_bits.saturating_sub(2);
            for &ch in seq {
                let b = encode_base(ch);
                if b == INVALID_BASE {
                    bail!(
                        "query contains invalid base '{}'; only A/C/G/T/U are supported",
                        ch as char
                    );
                }
                shift_left_insert(&mut forward, b as u64);
                shift_right_insert_top(&mut reverse, (3 - b) as u64, top_bit_pos);
                forward[words - 1] &= top_mask;
                reverse[words - 1] &= top_mask;
            }
            if forward <= reverse {
                Ok(QueryKey::Packed(forward))
            } else {
                Ok(QueryKey::Packed(reverse))
            }
        }
    }
}

/// 中文：打开 payload reader；如果 payload 使用 zstd，则自动包上解压器。
/// English: Open a payload reader and wrap it in a zstd decoder when needed.
fn open_payload_reader<'a, R: Read + Seek + 'a>(
    reader: &'a mut R,
    info: &KiteInfo,
) -> Result<Box<dyn Read + 'a>> {
    reader.seek(SeekFrom::Start(info.header_bytes))?;
    let limited = reader.take(info.payload_bytes);
    if (info.flags & FLAG_ZSTD) != 0 {
        Ok(Box::new(zstd::stream::read::Decoder::new(limited)?))
    } else {
        Ok(Box::new(limited))
    }
}

/// 中文：只读取 block-frame 的目录信息，不解码实际 block 内容。
/// English: Read only the block-frame directory metadata without decoding
/// the actual block bodies.
fn read_block_frames(path: &Path, info: &KiteInfo) -> Result<Vec<BlockFrame>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(info.header_bytes))?;

    let mut num_blocks_buf = [0u8; 4];
    file.read_exact(&mut num_blocks_buf)?;
    let num_blocks = u32::from_le_bytes(num_blocks_buf) as usize;
    let mut blocks = Vec::with_capacity(num_blocks);

    for _ in 0..num_blocks {
        let header_len = if has_block_tsv_bytes(info) { 16 } else { 12 };
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..header_len])?;
        let records = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let raw_bytes = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let compressed_bytes = u32::from_le_bytes(header[8..12].try_into().unwrap());
        let tsv_bytes = if has_block_tsv_bytes(info) {
            u32::from_le_bytes(header[12..16].try_into().unwrap())
        } else {
            0
        };
        let data_offset = file.stream_position()?;
        blocks.push(BlockFrame {
            records,
            raw_bytes,
            compressed_bytes,
            tsv_bytes,
            data_offset,
        });
        file.seek(SeekFrom::Current(compressed_bytes as i64))?;
    }

    Ok(blocks)
}

/// 中文：顺序遍历 block-framed payload，并把每个解压后的 block 交给回调。
/// English: Sequentially traverse a block-framed payload and pass each
/// decompressed block to the callback.
fn visit_block_framed_payload<R, F>(
    reader: &mut R,
    info: &KiteInfo,
    mut visit_block: F,
) -> Result<()>
where
    R: Read + Seek,
    F: FnMut(&[u8], u32) -> Result<()>,
{
    let mut num_blocks_buf = [0u8; 4];
    reader.read_exact(&mut num_blocks_buf)?;
    let num_blocks = u32::from_le_bytes(num_blocks_buf);
    for _ in 0..num_blocks {
        let header_len = if has_block_tsv_bytes(info) { 16 } else { 12 };
        let mut header = [0u8; 16];
        reader.read_exact(&mut header[..header_len])?;
        let records = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let raw_bytes = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let compressed_bytes = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        let mut compressed = vec![0u8; compressed_bytes];
        reader.read_exact(&mut compressed)?;
        let raw = zstd::bulk::decompress(&compressed, raw_bytes)?;
        visit_block(&raw, records)?;
    }
    Ok(())
}

/// 中文：按 `(kmer,count)` 遍历 `u64` 条目。
/// English: Visit `u64` entries as `(kmer, count)` pairs.
fn read_u64_entries<R, F>(reader: &mut R, info: &KiteInfo, mut visit: F) -> Result<()>
where
    R: Read + Seek,
    F: FnMut(u64, u64) -> Result<()>,
{
    if is_block_framed(info) {
        reader.seek(SeekFrom::Start(info.header_bytes))?;
        visit_block_framed_payload(reader, info, |raw, records| {
            let mut offset = 0usize;
            for _ in 0..records {
                let key = read_u64_le_unaligned(&raw[offset..]);
                let count = read_u64_le_unaligned(&raw[offset + 8..]);
                visit(key, count)?;
                offset += 16;
            }
            Ok(())
        })
    } else if u64_is_delta_varint(info) {
        reader.seek(SeekFrom::Start(info.header_bytes))?;
        if (info.flags & FLAG_ZSTD) != 0 {
            let limited = reader.take(info.payload_bytes);
            let mut decoder = zstd::stream::read::Decoder::new(limited)?;
            decode_u64_delta_varint_entries_from_reader(&mut decoder, info.entries, &mut visit)
        } else {
            let mut limited = reader.take(info.payload_bytes);
            decode_u64_delta_varint_entries_from_reader(&mut limited, info.entries, &mut visit)
        }
    } else {
        let mut input = open_payload_reader(reader, info)?;
        let mut key_buf = [0u8; 8];
        let mut count_buf = [0u8; 8];
        for _ in 0..info.entries {
            input.read_exact(&mut key_buf)?;
            input.read_exact(&mut count_buf)?;
            visit(u64::from_le_bytes(key_buf), u64::from_le_bytes(count_buf))?;
        }
        Ok(())
    }
}

#[allow(dead_code)]
/// 中文：在内存 payload 上查询一个键值。
/// English: Query one key from an in-memory payload.
fn query_payload(payload: &[u8], info: &KiteInfo, query: QueryKey) -> Result<u64> {
    match query {
        QueryKey::U64(target) => {
            if u64_is_delta_varint(info) {
                let mut answer = 0u64;
                decode_u64_delta_varint_entries(payload, info.entries, |kmer, count| {
                    if kmer == target {
                        answer = count;
                    }
                    Ok(())
                })?;
                Ok(answer)
            } else {
                let mut cur = Cursor::new(payload);
                let mut key_buf = [0u8; 8];
                let mut count_buf = [0u8; 8];
                for _ in 0..info.entries {
                    cur.read_exact(&mut key_buf)?;
                    cur.read_exact(&mut count_buf)?;
                    let key = u64::from_le_bytes(key_buf);
                    if key == target {
                        return Ok(u64::from_le_bytes(count_buf));
                    }
                }
                Ok(0)
            }
        }
        QueryKey::U128(target) => {
            let mut cur = Cursor::new(payload);
            let mut key_buf = [0u8; 16];
            let mut count_buf = [0u8; 8];
            for _ in 0..info.entries {
                cur.read_exact(&mut key_buf)?;
                cur.read_exact(&mut count_buf)?;
                if u128::from_le_bytes(key_buf) == target {
                    return Ok(u64::from_le_bytes(count_buf));
                }
            }
            Ok(0)
        }
        QueryKey::Packed(target) => {
            let mut cur = Cursor::new(payload);
            let mut arr = vec![0u64; info.words];
            let mut count_buf = [0u8; 8];
            for _ in 0..info.entries {
                for word in arr.iter_mut().take(info.words) {
                    let mut word_buf = [0u8; 8];
                    cur.read_exact(&mut word_buf)?;
                    *word = u64::from_le_bytes(word_buf);
                }
                cur.read_exact(&mut count_buf)?;
                if arr == target {
                    return Ok(u64::from_le_bytes(count_buf));
                }
            }
            Ok(0)
        }
    }
}

/// 中文：在 reader 上查询一个键值，根据编码模式走不同快路径。
/// English: Query one key from a reader, using mode-specific fast paths.
fn query_reader<R: Read + Seek>(reader: &mut R, info: &KiteInfo, query: QueryKey) -> Result<u64> {
    match query {
        QueryKey::U64(target) => {
            if is_block_framed(info) {
                reader.seek(SeekFrom::Start(info.header_bytes))?;
                let mut answer = 0u64;
                visit_block_framed_payload(reader, info, |raw, records| {
                    let mut offset = 0usize;
                    for _ in 0..records {
                        let key = read_u64_le_unaligned(&raw[offset..]);
                        let count = read_u64_le_unaligned(&raw[offset + 8..]);
                        if key == target {
                            answer = count;
                            break;
                        }
                        if key > target {
                            break;
                        }
                        offset += 16;
                    }
                    Ok(())
                })?;
                Ok(answer)
            } else {
                let mut input = open_payload_reader(reader, info)?;
                if u64_is_delta_varint(info) {
                    let mut prev = 0u64;
                    for index in 0..info.entries {
                        let delta = get_varint_u64(&mut input)?;
                        let count = get_varint_u64(&mut input)?;
                        let kmer = if index == 0 {
                            delta
                        } else {
                            prev.wrapping_add(delta)
                        };
                        if kmer == target {
                            return Ok(count);
                        }
                        if kmer > target {
                            return Ok(0);
                        }
                        prev = kmer;
                    }
                    Ok(0)
                } else {
                    let mut key_buf = [0u8; 8];
                    let mut count_buf = [0u8; 8];
                    for _ in 0..info.entries {
                        input.read_exact(&mut key_buf)?;
                        input.read_exact(&mut count_buf)?;
                        let key = u64::from_le_bytes(key_buf);
                        if key == target {
                            return Ok(u64::from_le_bytes(count_buf));
                        }
                        if key > target {
                            return Ok(0);
                        }
                    }
                    Ok(0)
                }
            }
        }
        QueryKey::U128(target) => {
            if is_block_framed(info) {
                reader.seek(SeekFrom::Start(info.header_bytes))?;
                let mut answer = 0u64;
                visit_block_framed_payload(reader, info, |raw, records| {
                    let mut offset = 0usize;
                    for _ in 0..records {
                        let key = read_u128_le_unaligned(&raw[offset..]);
                        let count = read_u64_le_unaligned(&raw[offset + 16..]);
                        if key == target {
                            answer = count;
                            break;
                        }
                        if key > target {
                            break;
                        }
                        offset += 24;
                    }
                    Ok(())
                })?;
                Ok(answer)
            } else {
                let mut input = open_payload_reader(reader, info)?;
                let mut key_buf = [0u8; 16];
                let mut count_buf = [0u8; 8];
                for _ in 0..info.entries {
                    input.read_exact(&mut key_buf)?;
                    input.read_exact(&mut count_buf)?;
                    let key = u128::from_le_bytes(key_buf);
                    if key == target {
                        return Ok(u64::from_le_bytes(count_buf));
                    }
                    if key > target {
                        return Ok(0);
                    }
                }
                Ok(0)
            }
        }
        QueryKey::Packed(target) => {
            let mut input = open_payload_reader(reader, info)?;
            let mut words = vec![0u64; info.words];
            let mut word_buf = [0u8; 8];
            let mut count_buf = [0u8; 8];
            for _ in 0..info.entries {
                for word in words.iter_mut().take(info.words) {
                    input.read_exact(&mut word_buf)?;
                    *word = u64::from_le_bytes(word_buf);
                }
                input.read_exact(&mut count_buf)?;
                if words == target {
                    return Ok(u64::from_le_bytes(count_buf));
                }
                if words > target {
                    return Ok(0);
                }
            }
            Ok(0)
        }
    }
}

#[allow(dead_code)]
/// 中文：在内存 payload 中遍历全部 count 值。
/// English: Visit every count value in an in-memory payload.
fn visit_payload_counts<F>(payload: &[u8], info: &KiteInfo, mut visit: F) -> Result<()>
where
    F: FnMut(u64) -> Result<()>,
{
    match info.mode {
        KmerMode::U64 => {
            if u64_is_delta_varint(info) {
                decode_u64_delta_varint_entries(payload, info.entries, |_, count| visit(count))
            } else {
                let mut cur = Cursor::new(payload);
                let mut key_buf = [0u8; 8];
                let mut count_buf = [0u8; 8];
                for _ in 0..info.entries {
                    cur.read_exact(&mut key_buf)?;
                    cur.read_exact(&mut count_buf)?;
                    visit(u64::from_le_bytes(count_buf))?;
                }
                Ok(())
            }
        }
        KmerMode::U128 => {
            let mut cur = Cursor::new(payload);
            let mut key_buf = [0u8; 16];
            let mut count_buf = [0u8; 8];
            for _ in 0..info.entries {
                cur.read_exact(&mut key_buf)?;
                cur.read_exact(&mut count_buf)?;
                visit(u64::from_le_bytes(count_buf))?;
            }
            Ok(())
        }
        KmerMode::Packed(_) => {
            let mut cur = Cursor::new(payload);
            let mut scratch = vec![0u8; info.words * 8];
            let mut count_buf = [0u8; 8];
            for _ in 0..info.entries {
                cur.read_exact(&mut scratch)?;
                cur.read_exact(&mut count_buf)?;
                visit(u64::from_le_bytes(count_buf))?;
            }
            Ok(())
        }
    }
}

/// 中文：在 reader 中遍历全部 count 值。
/// English: Visit every count value from a reader.
fn visit_counts_reader<R, F>(reader: &mut R, info: &KiteInfo, mut visit: F) -> Result<()>
where
    R: Read + Seek,
    F: FnMut(u64) -> Result<()>,
{
    match info.mode {
        KmerMode::U64 => read_u64_entries(reader, info, |_, count| visit(count)),
        KmerMode::U128 => {
            if is_block_framed(info) {
                reader.seek(SeekFrom::Start(info.header_bytes))?;
                visit_block_framed_payload(reader, info, |raw, records| {
                    let mut offset = 0usize;
                    for _ in 0..records {
                        let count = read_u64_le_unaligned(&raw[offset + 16..]);
                        visit(count)?;
                        offset += 24;
                    }
                    Ok(())
                })
            } else {
                let mut input = open_payload_reader(reader, info)?;
                let mut skip = [0u8; 16];
                let mut count_buf = [0u8; 8];
                for _ in 0..info.entries {
                    input.read_exact(&mut skip)?;
                    input.read_exact(&mut count_buf)?;
                    visit(u64::from_le_bytes(count_buf))?;
                }
                Ok(())
            }
        }
        KmerMode::Packed(_) => {
            let mut input = open_payload_reader(reader, info)?;
            let mut skip = vec![0u8; info.words * 8];
            let mut count_buf = [0u8; 8];
            for _ in 0..info.entries {
                input.read_exact(&mut skip)?;
                input.read_exact(&mut count_buf)?;
                visit(u64::from_le_bytes(count_buf))?;
            }
            Ok(())
        }
    }
}

#[allow(dead_code)]
/// 中文：从内存 payload 中解码 delta-varint `u64` 条目。
/// English: Decode delta-varint `u64` entries from an in-memory payload.
fn decode_u64_delta_varint_entries<F>(payload: &[u8], n: u64, mut visit: F) -> Result<()>
where
    F: FnMut(u64, u64) -> Result<()>,
{
    let mut cur = Cursor::new(payload);
    decode_u64_delta_varint_entries_from_reader(&mut cur, n, &mut visit)
}

/// 中文：从任意 reader 中解码 delta-varint `u64` 条目。
/// English: Decode delta-varint `u64` entries from any reader.
fn decode_u64_delta_varint_entries_from_reader<R, F>(
    reader: &mut R,
    n: u64,
    visit: &mut F,
) -> Result<()>
where
    R: Read,
    F: FnMut(u64, u64) -> Result<()>,
{
    let mut prev = 0u64;
    for index in 0..n {
        let delta = get_varint_u64(reader)?;
        let count = get_varint_u64(reader)?;
        let kmer = if index == 0 {
            delta
        } else {
            prev.wrapping_add(delta)
        };
        visit(kmer, count)?;
        prev = kmer;
    }
    Ok(())
}

/// 中文：从 reader 读取一个 varint 编码的 `u64`。
/// English: Read one varint-encoded `u64` from a reader.
fn get_varint_u64<R: Read>(r: &mut R) -> Result<u64> {
    let mut shift = 0u32;
    let mut out = 0u64;
    for _ in 0..10 {
        let mut buf = [0u8; 1];
        r.read_exact(&mut buf)?;
        let byte = buf[0];
        out |= ((byte & 0x7F) as u64) << shift;
        if (byte & 0x80) == 0 {
            return Ok(out);
        }
        shift += 7;
    }
    bail!("invalid varint sequence")
}

/// 中文：启动一组 `u64` 解码 worker，把记录块转换成 TSV 文本块。
/// English: Spawn a set of `u64` decode workers that convert record chunks
/// into TSV text chunks.
fn spawn_u64_workers(
    k: usize,
    threads: usize,
    rx_in: Receiver<(usize, Vec<(u64, u64)>)>,
    tx_out: Sender<(usize, Vec<u8>)>,
) -> Vec<thread::JoinHandle<Result<()>>> {
    let mut handles = Vec::with_capacity(threads);
    for _ in 0..threads {
        let rx = rx_in.clone();
        let tx = tx_out.clone();
        handles.push(thread::spawn(move || -> Result<()> {
            let mut seq = vec![0u8; k];
            let mut itoa_buf = ItoaBuffer::new();
            while let Ok((chunk_id, chunk)) = rx.recv() {
                let mut out_buf = Vec::with_capacity(chunk.len() * 48);
                for (kmer, count) in chunk {
                    decode_u64_fast_into(kmer, k, &mut seq);
                    append_tsv_record(&mut out_buf, &seq, count, &mut itoa_buf);
                }
                tx.send((chunk_id, out_buf))
                    .map_err(|_| anyhow::anyhow!("dump writer channel closed"))?;
            }
            Ok(())
        }));
    }
    handles
}

/// 中文：启动有序 writer 线程，按 chunk id 顺序输出结果。
/// English: Spawn an ordered writer thread that emits chunks in chunk-id order.
fn spawn_writer<W: Write + Send + 'static>(
    writer: BufWriter<W>,
    rx_out: Receiver<(usize, Vec<u8>)>,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        let mut writer = writer;
        let mut next_chunk = 0usize;
        let mut pending = BTreeMap::<usize, Vec<u8>>::new();
        while let Ok((chunk_id, chunk)) = rx_out.recv() {
            if chunk_id == next_chunk {
                writer.write_all(&chunk)?;
                next_chunk += 1;
                while let Some(chunk) = pending.remove(&next_chunk) {
                    writer.write_all(&chunk)?;
                    next_chunk += 1;
                }
            } else {
                pending.insert(chunk_id, chunk);
            }
        }
        while let Some(chunk) = pending.remove(&next_chunk) {
            writer.write_all(&chunk)?;
            next_chunk += 1;
        }
        writer.flush()?;
        Ok(())
    })
}

/// 中文：等待全部 worker 线程结束并汇总错误。
/// English: Join all worker threads and propagate any error.
fn join_workers(handles: Vec<thread::JoinHandle<Result<()>>>) -> Result<()> {
    for handle in handles {
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("dump worker panicked"))??;
    }
    Ok(())
}

#[inline(always)]
/// 中文：把一条 `(sequence,count)` 追加成 TSV 行。
/// English: Append one `(sequence, count)` pair as a TSV line.
fn append_tsv_record(buf: &mut Vec<u8>, seq: &[u8], count: u64, itoa_buf: &mut ItoaBuffer) {
    buf.extend_from_slice(seq);
    buf.push(b'\t');
    buf.extend_from_slice(itoa_buf.format(count).as_bytes());
    buf.push(b'\n');
}

#[inline(always)]
/// 中文：从未对齐字节切片读取小端 `u64`。
/// English: Read a little-endian `u64` from an unaligned byte slice.
fn read_u64_le_unaligned(bytes: &[u8]) -> u64 {
    debug_assert!(bytes.len() >= 8);
    let ptr = bytes.as_ptr() as *const u64;
    u64::from_le(unsafe { std::ptr::read_unaligned(ptr) })
}

#[inline(always)]
/// 中文：从未对齐字节切片读取小端 `u128`。
/// English: Read a little-endian `u128` from an unaligned byte slice.
fn read_u128_le_unaligned(bytes: &[u8]) -> u128 {
    debug_assert!(bytes.len() >= 16);
    let ptr = bytes.as_ptr() as *const u128;
    u128::from_le(unsafe { std::ptr::read_unaligned(ptr) })
}

#[inline(always)]
/// 中文：packed 长 k-mer 左移 2 bit 并插入新碱基。
/// English: Left-shift a packed long k-mer by 2 bits and insert a new base.
fn shift_left_insert(words: &mut [u64], val2: u64) {
    let mut carry = val2 & 0b11;
    for word in words.iter_mut() {
        let next_carry = (*word >> 62) & 0b11;
        *word = (*word << 2) | carry;
        carry = next_carry;
    }
}

#[inline(always)]
/// 中文：packed 反向互补状态右移 2 bit 并在高位插入新碱基。
/// English: Right-shift the packed reverse-complement state by 2 bits and
/// insert a new base at the high end.
fn shift_right_insert_top(words: &mut [u64], val2: u64, bit_pos: usize) {
    let mut carry = 0u64;
    for word in words.iter_mut().rev() {
        let next_carry = (*word & 0b11) << 62;
        *word = (*word >> 2) | carry;
        carry = next_carry;
    }
    set_2bits(words, bit_pos, val2 & 0b11);
}

#[inline(always)]
/// 中文：在 packed 数组里写入一个 2-bit 碱基值。
/// English: Write one 2-bit base value into a packed array.
fn set_2bits(words: &mut [u64], bit_pos: usize, val2: u64) {
    let word_idx = bit_pos / 64;
    let bit_idx = bit_pos % 64;
    if bit_idx <= 62 {
        let mask = !(0b11u64 << bit_idx);
        words[word_idx] = (words[word_idx] & mask) | (val2 << bit_idx);
    } else {
        words[word_idx] &= !(1u64 << 63);
        words[word_idx] |= (val2 & 0b1) << 63;
        words[word_idx + 1] &= !1u64;
        words[word_idx + 1] |= (val2 >> 1) & 0b1;
    }
}

#[inline(always)]
/// 中文：判断当前 `u64` 数据是否使用 delta-varint 编码。
/// English: Check whether the current `u64` data uses delta-varint encoding.
fn u64_is_delta_varint(info: &KiteInfo) -> bool {
    matches!(info.mode, KmerMode::U64) && (info.flags & crate::utils::kite::FLAG_DELTA_VARINT) != 0
}

#[inline(always)]
/// 中文：判断 payload 是否使用 block-frame 布局。
/// English: Check whether the payload uses block-frame layout.
fn is_block_framed(info: &KiteInfo) -> bool {
    (info.flags & FLAG_BLOCK_FRAMES) != 0
}

#[inline(always)]
/// 中文：判断 block-frame 头里是否带有 TSV 字节数信息。
/// English: Check whether block-frame headers include TSV byte-length fields.
fn has_block_tsv_bytes(info: &KiteInfo) -> bool {
    (info.flags & FLAG_BLOCK_TSV_BYTES) != 0
}
