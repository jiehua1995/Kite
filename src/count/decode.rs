use super::buffer::{
    ChunkedU64Outcome, CountOutcome, CountTable, PartitionedU64Outcome, PartitionedU128Outcome,
};
use super::merge::ChunkU64;
use crate::utils::kite::{
    CURRENT_FORMAT_VERSION, FLAG_BLOCK_FRAMES, FLAG_BLOCK_TSV_BYTES, FLAG_CANONICAL,
    FLAG_DELTA_VARINT, FLAG_ZSTD, KiteInfo, KiteMetadata, patch_entries_and_payload,
    patch_payload_bytes, write_header,
};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const BLOCK_RECORDS: usize = 1_000_000;

#[derive(Debug, Clone, Copy, Default)]
/// 中文：写库压缩模式，决定速度和压缩率的平衡。
/// English: Compression mode used while writing `.kite`, balancing speed and
/// compression ratio.
pub enum CompressionMode {
    #[default]
    Fast,
    Balanced,
    Compact,
}

#[derive(Debug, Clone, Default)]
/// 中文：写 `.kite` 文件时使用的可选参数集合。
/// English: Optional parameter bundle used while writing a `.kite` file.
pub struct WriteBinaryOptions {
    pub mode: CompressionMode,
    pub zstd_level: Option<i32>,
    pub threads: usize,
    pub metadata: KiteMetadata,
}

pub struct StreamWriteStats {
    pub entries: u64,
    pub _payload_bytes: u64,
    pub spill: Option<SpillWriteStats>,
}

#[derive(Debug, Clone, Default)]
pub struct SpillWriteStats {
    pub spill_files: u64,
    pub active_partitions: u64,
    pub files_per_partition_sum: u64,
    pub max_files_per_partition: u64,
    pub hottest_partition: usize,
    pub hottest_partition_file_count: u64,
    pub fanin_buckets: [u64; 6],
    pub merge_passes: u64,
    pub spill_bytes_read: u64,
    pub spill_final_merge: Duration,
    pub spill_output: Duration,
}

#[inline(always)]
fn fanin_bucket(files: usize) -> usize {
    match files {
        0 | 1 => 0,
        2..=4 => 1,
        5..=8 => 2,
        9..=16 => 3,
        17..=32 => 4,
        _ => 5,
    }
}

/// 中文：把 `count` 结果写成统一的 `.kite` 二进制格式。
/// 输入是输出路径、k 值、计数结果和写出选项；输出是写好的数据库文件。
/// English: Serialize `count` output into the unified `.kite` binary format.
/// Inputs are the output path, k value, counting outcome, and write options;
/// output is the finished database file.
pub fn write_binary(
    path: &Path,
    k: usize,
    outcome: &CountOutcome,
    options: WriteBinaryOptions,
) -> Result<()> {
    let entries = match &outcome.table {
        CountTable::U64(v) => v.len() as u64,
        CountTable::U128(v) => v.len() as u64,
        CountTable::Packed(v) => v.len() as u64,
    };

    let block_framed_fast = matches!(options.mode, CompressionMode::Fast)
        && matches!(&outcome.table, CountTable::U64(_) | CountTable::U128(_));
    let (mut flags, words) = match &outcome.table {
        CountTable::U64(_) => (
            if block_framed_fast {
                0u8
            } else {
                FLAG_DELTA_VARINT
            },
            0usize,
        ),
        CountTable::U128(_) => (0u8, 0usize),
        CountTable::Packed(_) => (0u8, ((k * 2).div_ceil(64)) as usize),
    };
    flags |= FLAG_ZSTD;
    if block_framed_fast {
        flags |= FLAG_BLOCK_FRAMES;
        flags |= FLAG_BLOCK_TSV_BYTES;
    }
    if outcome.canonical {
        flags |= FLAG_CANONICAL;
    }

    let level = std::env::var("KITE_ZSTD_LEVEL")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .or(options.zstd_level)
        .unwrap_or(match options.mode {
            CompressionMode::Fast => 1,
            CompressionMode::Balanced => 3,
            CompressionMode::Compact => 9,
        });
    let mut metadata = options.metadata;
    metadata.unique_kmers = Some(entries);
    metadata.canonical = Some(outcome.canonical);
    metadata.zstd_level = Some(level);
    let zstd_workers = match &outcome.table {
        CountTable::U64(_) if block_framed_fast => 0u32,
        CountTable::U64(_) => 1u32,
        CountTable::U128(_) | CountTable::Packed(_) => options.threads.clamp(1, 16) as u32,
    };

    let mut file =
        File::create(path).with_context(|| format!("Failed to create {}", path.display()))?;
    let patch = write_header(
        &mut file,
        &KiteInfo {
            format_version: CURRENT_FORMAT_VERSION,
            k,
            mode: outcome.mode,
            entries,
            words,
            header_bytes: 0,
            metadata_bytes: 0,
            flags,
            payload_bytes: 0,
            metadata,
        },
    )?;
    let payload_start = file.stream_position()?;
    file.flush()?;

    if block_framed_fast {
        let payload_bytes = match &outcome.table {
            CountTable::U64(v) => {
                write_block_framed_u64(path, payload_start, v, k, level, options.threads)?
            }
            CountTable::U128(v) => {
                write_block_framed_u128(path, payload_start, v, k, level, options.threads)?
            }
            CountTable::Packed(_) => unreachable!("packed is not block-framed in fast mode"),
        };
        return patch_payload_bytes(path, patch, payload_bytes);
    }

    let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = bounded(16);
    let path_clone = path.to_path_buf();
    let writer_thread = thread::spawn(move || -> Result<u64> {
        let mut out_file = OpenOptions::new().write(true).open(&path_clone)?;
        out_file.seek(SeekFrom::Start(payload_start))?;
        let bw = BufWriter::with_capacity(1 << 20, out_file);
        let mut encoder = zstd::stream::Encoder::new(bw, level)?;
        if zstd_workers > 1 {
            encoder.multithread(zstd_workers)?;
        }
        while let Ok(chunk) = rx.recv() {
            encoder.write_all(&chunk)?;
        }
        let mut bw = encoder.finish()?;
        bw.flush()?;
        let final_file = bw.into_inner()?;
        Ok(final_file.metadata()?.len().saturating_sub(payload_start))
    });

    match &outcome.table {
        CountTable::U64(v) => encode_u64_delta_varint_to_sender(v, &tx)?,
        CountTable::U128(v) => encode_u128_to_sender(v, &tx)?,
        CountTable::Packed(v) => encode_packed_to_sender(v, &tx)?,
    }

    drop(tx);
    let compressed_len = writer_thread
        .join()
        .map_err(|_| anyhow::anyhow!("writer thread panicked"))??;

    patch_payload_bytes(path, patch, compressed_len)
}

pub fn write_partitioned_u64_binary(
    path: &Path,
    k: usize,
    outcome: &PartitionedU64Outcome,
    options: WriteBinaryOptions,
) -> Result<StreamWriteStats> {
    let level = std::env::var("KITE_ZSTD_LEVEL")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .or(options.zstd_level)
        .unwrap_or(1);
    let mut metadata = options.metadata;
    metadata.total_reads = Some(outcome.total_reads);
    metadata.total_kmers = Some(outcome.total_kmers);
    metadata.canonical = Some(outcome.canonical);
    metadata.zstd_level = Some(level);
    let flags =
        FLAG_ZSTD | FLAG_BLOCK_FRAMES | FLAG_BLOCK_TSV_BYTES | if outcome.canonical { FLAG_CANONICAL } else { 0 };

    let mut file =
        File::create(path).with_context(|| format!("Failed to create {}", path.display()))?;
    let patch = write_header(
        &mut file,
        &KiteInfo {
            format_version: CURRENT_FORMAT_VERSION,
            k,
            mode: crate::utils::kite::KmerMode::U64,
            entries: 0,
            words: 0,
            header_bytes: 0,
            metadata_bytes: 0,
            flags,
            payload_bytes: 0,
            metadata,
        },
    )?;
    let payload_start = file.stream_position()?;
    let mut bw = BufWriter::with_capacity(4 << 20, file);
    bw.write_all(&0u32.to_le_bytes())?;

    let mut block_count = 0u32;
    let mut entries_total = 0u64;
    let mut block_buf = Vec::<(u64, u64)>::with_capacity(BLOCK_RECORDS);
    let mut spill = SpillWriteStats::default();
    for (part, files) in outcome.partition_chunks.iter().enumerate() {
        if files.is_empty() {
            continue;
        }
        spill.active_partitions += 1;
        spill.spill_files += files.len() as u64;
        spill.files_per_partition_sum += files.len() as u64;
        spill.max_files_per_partition = spill.max_files_per_partition.max(files.len() as u64);
        if (files.len() as u64) > spill.hottest_partition_file_count {
            spill.hottest_partition = part;
            spill.hottest_partition_file_count = files.len() as u64;
        }
        spill.fanin_buckets[fanin_bucket(files.len())] += 1;
        let merge_stats = merge_chunk_files_u64_into_blocks(
            files,
            &mut bw,
            level,
            k,
            &mut block_buf,
            &mut entries_total,
            &mut block_count,
        )?;
        spill.spill_bytes_read += merge_stats.bytes_read;
        spill.spill_final_merge += merge_stats.merge_elapsed;
        spill.spill_output += merge_stats.output_elapsed;
        for path in files {
            let _ = std::fs::remove_file(path);
        }
    }
    let t_tail_flush = Instant::now();
    flush_u64_block(&mut bw, &mut block_buf, level, k, &mut block_count)?;
    spill.spill_output += t_tail_flush.elapsed();
    let t_flush = Instant::now();
    bw.flush()?;
    spill.spill_output += t_flush.elapsed();
    let t_into = Instant::now();
    let mut file = bw.into_inner()?;
    spill.spill_output += t_into.elapsed();
    let end_pos = file.stream_position()?;
    let t_patch = Instant::now();
    file.seek(SeekFrom::Start(payload_start))?;
    file.write_all(&block_count.to_le_bytes())?;
    file.flush()?;
    spill.spill_output += t_patch.elapsed();
    let payload_bytes = end_pos.saturating_sub(payload_start);
    drop(file);
    patch_entries_and_payload(path, patch, entries_total, payload_bytes)?;
    spill.merge_passes = if spill.active_partitions == 0 { 0 } else { 1 };
    Ok(StreamWriteStats {
        entries: entries_total,
        _payload_bytes: payload_bytes,
        spill: Some(spill),
    })
}

pub fn write_partitioned_u128_binary(
    path: &Path,
    k: usize,
    outcome: &PartitionedU128Outcome,
    options: WriteBinaryOptions,
) -> Result<StreamWriteStats> {
    let level = std::env::var("KITE_ZSTD_LEVEL")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .or(options.zstd_level)
        .unwrap_or(1);
    let mut metadata = options.metadata;
    metadata.total_reads = Some(outcome.total_reads);
    metadata.total_kmers = Some(outcome.total_kmers);
    metadata.canonical = Some(outcome.canonical);
    metadata.zstd_level = Some(level);
    let flags = FLAG_ZSTD
        | FLAG_BLOCK_FRAMES
        | FLAG_BLOCK_TSV_BYTES
        | if outcome.canonical { FLAG_CANONICAL } else { 0 };

    let mut file =
        File::create(path).with_context(|| format!("Failed to create {}", path.display()))?;
    let patch = write_header(
        &mut file,
        &KiteInfo {
            format_version: CURRENT_FORMAT_VERSION,
            k,
            mode: crate::utils::kite::KmerMode::U128,
            entries: 0,
            words: 0,
            header_bytes: 0,
            metadata_bytes: 0,
            flags,
            payload_bytes: 0,
            metadata,
        },
    )?;
    let payload_start = file.stream_position()?;
    let mut bw = BufWriter::with_capacity(4 << 20, file);
    bw.write_all(&0u32.to_le_bytes())?;

    let mut block_count = 0u32;
    let mut entries_total = 0u64;
    let mut block_buf = Vec::<(u128, u64)>::with_capacity(BLOCK_RECORDS);
    for files in &outcome.partition_chunks {
        if files.is_empty() {
            continue;
        }
        merge_chunk_files_u128_into_blocks(
            files,
            &mut bw,
            level,
            k,
            &mut block_buf,
            &mut entries_total,
            &mut block_count,
        )?;
        for path in files {
            let _ = std::fs::remove_file(path);
        }
    }
    flush_u128_block(&mut bw, &mut block_buf, level, k, &mut block_count)?;
    bw.flush()?;
    let mut file = bw.into_inner()?;
    let end_pos = file.stream_position()?;
    file.seek(SeekFrom::Start(payload_start))?;
    file.write_all(&block_count.to_le_bytes())?;
    file.flush()?;
    let payload_bytes = end_pos.saturating_sub(payload_start);
    drop(file);
    patch_entries_and_payload(path, patch, entries_total, payload_bytes)?;
    Ok(StreamWriteStats {
        entries: entries_total,
        _payload_bytes: payload_bytes,
        spill: None,
    })
}

pub fn write_chunked_u64_binary(
    path: &Path,
    k: usize,
    outcome: &ChunkedU64Outcome,
    options: WriteBinaryOptions,
) -> Result<StreamWriteStats> {
    let level = std::env::var("KITE_ZSTD_LEVEL")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .or(options.zstd_level)
        .unwrap_or(1);
    let mut metadata = options.metadata;
    metadata.total_reads = Some(outcome.total_reads);
    metadata.total_kmers = Some(outcome.total_kmers);
    metadata.canonical = Some(outcome.canonical);
    metadata.zstd_level = Some(level);
    let flags =
        FLAG_ZSTD | FLAG_BLOCK_FRAMES | FLAG_BLOCK_TSV_BYTES | if outcome.canonical { FLAG_CANONICAL } else { 0 };

    let mut file =
        File::create(path).with_context(|| format!("Failed to create {}", path.display()))?;
    let patch = write_header(
        &mut file,
        &KiteInfo {
            format_version: CURRENT_FORMAT_VERSION,
            k,
            mode: crate::utils::kite::KmerMode::U64,
            entries: 0,
            words: 0,
            header_bytes: 0,
            metadata_bytes: 0,
            flags,
            payload_bytes: 0,
            metadata,
        },
    )?;
    let payload_start = file.stream_position()?;
    let mut bw = BufWriter::with_capacity(4 << 20, file);
    bw.write_all(&0u32.to_le_bytes())?;

    let mut block_count = 0u32;
    let mut entries_total = 0u64;
    let mut block_buf = Vec::<(u64, u64)>::with_capacity(BLOCK_RECORDS);
    for chunks in &outcome.partition_chunks {
        if chunks.is_empty() {
            continue;
        }
        merge_chunk_slices_u64_into_blocks(
            chunks,
            &mut bw,
            level,
            k,
            &mut block_buf,
            &mut entries_total,
            &mut block_count,
        )?;
    }
    flush_u64_block(&mut bw, &mut block_buf, level, k, &mut block_count)?;
    bw.flush()?;
    let mut file = bw.into_inner()?;
    let end_pos = file.stream_position()?;
    file.seek(SeekFrom::Start(payload_start))?;
    file.write_all(&block_count.to_le_bytes())?;
    file.flush()?;
    let payload_bytes = end_pos.saturating_sub(payload_start);
    drop(file);
    patch_entries_and_payload(path, patch, entries_total, payload_bytes)?;
    Ok(StreamWriteStats {
        entries: entries_total,
        _payload_bytes: payload_bytes,
        spill: None,
    })
}

fn flush_u64_block<W: Write>(
    out: &mut W,
    block_buf: &mut Vec<(u64, u64)>,
    level: i32,
    k: usize,
    block_count: &mut u32,
) -> Result<()> {
    if block_buf.is_empty() {
        return Ok(());
    }
    let encoded = encode_block_u64(block_buf, level, k)?;
    out.write_all(&encoded)?;
    *block_count = block_count.wrapping_add(1);
    block_buf.clear();
    Ok(())
}

fn flush_u128_block<W: Write>(
    out: &mut W,
    block_buf: &mut Vec<(u128, u64)>,
    level: i32,
    k: usize,
    block_count: &mut u32,
) -> Result<()> {
    if block_buf.is_empty() {
        return Ok(());
    }
    let encoded = encode_block_u128(block_buf, level, k)?;
    out.write_all(&encoded)?;
    *block_count = block_count.wrapping_add(1);
    block_buf.clear();
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct HeapItemU64Ref {
    key: u64,
    count: u32,
    chunk: usize,
    pos: usize,
}

impl PartialEq for HeapItemU64Ref {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for HeapItemU64Ref {}

impl PartialOrd for HeapItemU64Ref {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(other.key.cmp(&self.key))
    }
}

impl Ord for HeapItemU64Ref {
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.cmp(&self.key)
    }
}

fn merge_chunk_slices_u64_into_blocks<W: Write>(
    chunks: &[ChunkU64],
    out: &mut W,
    level: i32,
    k: usize,
    block_buf: &mut Vec<(u64, u64)>,
    entries_total: &mut u64,
    block_count: &mut u32,
) -> Result<()> {
    let mut heap = BinaryHeap::new();
    for (chunk_id, chunk) in chunks.iter().enumerate() {
        if !chunk.kmers.is_empty() {
            heap.push(HeapItemU64Ref {
                key: chunk.kmers[0],
                count: chunk.counts[0],
                chunk: chunk_id,
                pos: 0,
            });
        }
    }

    while let Some(item) = heap.pop() {
        let key = item.key;
        let mut total = item.count as u64;
        push_next_u64_ref(&mut heap, chunks, item.chunk, item.pos + 1);

        while let Some(next) = heap.peek() {
            if next.key != key {
                break;
            }
            let next = heap.pop().expect("heap peeked item must exist");
            total += next.count as u64;
            push_next_u64_ref(&mut heap, chunks, next.chunk, next.pos + 1);
        }

        block_buf.push((key, total));
        *entries_total += 1;
        if block_buf.len() >= BLOCK_RECORDS {
            flush_u64_block(out, block_buf, level, k, block_count)?;
        }
    }

    Ok(())
}

fn push_next_u64_ref(
    heap: &mut BinaryHeap<HeapItemU64Ref>,
    chunks: &[ChunkU64],
    chunk: usize,
    pos: usize,
) {
    let c = &chunks[chunk];
    if pos < c.kmers.len() {
        heap.push(HeapItemU64Ref {
            key: c.kmers[pos],
            count: c.counts[pos],
            chunk,
            pos,
        });
    }
}

/// 中文：把 `u64` 计数结果按 block-frame 形式写出，供 fast dump 使用。
/// English: Write `u64` count results in block-framed form so fast dump can
/// decode them efficiently later.
fn write_block_framed_u64(
    path: &Path,
    payload_start: u64,
    entries: &[(u64, u64)],
    k: usize,
    level: i32,
    threads: usize,
) -> Result<u64> {
    let blocks: Result<Vec<Vec<u8>>> = entries
        .par_chunks(BLOCK_RECORDS)
        .with_min_len(1)
        .map(|chunk| encode_block_u64(chunk, level, k))
        .collect();
    write_block_payload(path, payload_start, &blocks?, threads)
}

/// 中文：把 `u128` 计数结果按 block-frame 形式写出。
/// English: Write `u128` count results in block-framed form.
fn write_block_framed_u128(
    path: &Path,
    payload_start: u64,
    entries: &[(u128, u64)],
    k: usize,
    level: i32,
    threads: usize,
) -> Result<u64> {
    let blocks: Result<Vec<Vec<u8>>> = entries
        .par_chunks(BLOCK_RECORDS)
        .with_min_len(1)
        .map(|chunk| encode_block_u128(chunk, level, k))
        .collect();
    write_block_payload(path, payload_start, &blocks?, threads)
}

/// 中文：把所有 block 串接写到 payload 区，并返回 payload 字节数。
/// English: Append all block frames to the payload section and return the
/// resulting payload byte length.
fn write_block_payload(
    path: &Path,
    payload_start: u64,
    blocks: &[Vec<u8>],
    _threads: usize,
) -> Result<u64> {
    let mut out_file = OpenOptions::new().write(true).open(path)?;
    out_file.seek(SeekFrom::Start(payload_start))?;
    let mut bw = BufWriter::with_capacity(4 << 20, out_file);
    bw.write_all(&(blocks.len() as u32).to_le_bytes())?;
    for block in blocks {
        bw.write_all(block)?;
    }
    bw.flush()?;
    let out = bw.into_inner()?;
    Ok(out.metadata()?.len().saturating_sub(payload_start))
}

/// 中文：把一个 `u64` block 编码为“块头 + zstd 压缩原始记录”。
/// English: Encode one `u64` block as `block-header + zstd-compressed raw records`.
fn encode_block_u64(chunk: &[(u64, u64)], level: i32, k: usize) -> Result<Vec<u8>> {
    let raw_bytes = chunk.len() * 16;
    let mut raw = Vec::with_capacity(raw_bytes);
    let mut tsv_bytes = 0usize;
    for &(kmer, count) in chunk {
        raw.extend_from_slice(&kmer.to_le_bytes());
        raw.extend_from_slice(&count.to_le_bytes());
        tsv_bytes += k + 2 + decimal_len_u64(count);
    }
    let compressed = zstd::bulk::compress(&raw, level)?;
    let mut out = Vec::with_capacity(20 + compressed.len());
    out.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
    out.extend_from_slice(&(raw_bytes as u32).to_le_bytes());
    out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
    out.extend_from_slice(&(tsv_bytes as u32).to_le_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

/// 中文：把一个 `u128` block 编码为“块头 + zstd 压缩原始记录”。
/// English: Encode one `u128` block as `block-header + zstd-compressed raw records`.
fn encode_block_u128(chunk: &[(u128, u64)], level: i32, k: usize) -> Result<Vec<u8>> {
    let raw_bytes = chunk.len() * 24;
    let mut raw = Vec::with_capacity(raw_bytes);
    let mut tsv_bytes = 0usize;
    for &(kmer, count) in chunk {
        raw.extend_from_slice(&kmer.to_le_bytes());
        raw.extend_from_slice(&count.to_le_bytes());
        tsv_bytes += k + 2 + decimal_len_u64(count);
    }
    let compressed = zstd::bulk::compress(&raw, level)?;
    let mut out = Vec::with_capacity(20 + compressed.len());
    out.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
    out.extend_from_slice(&(raw_bytes as u32).to_le_bytes());
    out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
    out.extend_from_slice(&(tsv_bytes as u32).to_le_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

#[inline(always)]
/// 中文：计算一个无符号整数写成十进制文本所需的字符数。
/// English: Compute how many decimal digits are needed to print an unsigned
/// integer.
fn decimal_len_u64(value: u64) -> usize {
    if value < 10 {
        1
    } else if value < 100 {
        2
    } else if value < 1_000 {
        3
    } else if value < 10_000 {
        4
    } else if value < 100_000 {
        5
    } else if value < 1_000_000 {
        6
    } else if value < 10_000_000 {
        7
    } else if value < 100_000_000 {
        8
    } else if value < 1_000_000_000 {
        9
    } else if value < 10_000_000_000 {
        10
    } else if value < 100_000_000_000 {
        11
    } else if value < 1_000_000_000_000 {
        12
    } else if value < 10_000_000_000_000 {
        13
    } else if value < 100_000_000_000_000 {
        14
    } else if value < 1_000_000_000_000_000 {
        15
    } else if value < 10_000_000_000_000_000 {
        16
    } else if value < 100_000_000_000_000_000 {
        17
    } else if value < 1_000_000_000_000_000_000 {
        18
    } else if value < 10_000_000_000_000_000_000u64 {
        19
    } else {
        20
    }
}

/// 中文：把排序后的 `u64` 结果编码成 delta-varint 字节流并分块发送给写线程。
/// English: Encode sorted `u64` entries into a delta-varint byte stream and
/// send chunks to the writer thread.
fn encode_u64_delta_varint_to_sender(entries: &[(u64, u64)], tx: &Sender<Vec<u8>>) -> Result<()> {
    let mut buf = Vec::with_capacity(1 << 16);
    let mut prev = 0u64;
    for (idx, &(key, count)) in entries.iter().enumerate() {
        if idx == 0 {
            put_varint_u64(key, &mut buf);
        } else {
            put_varint_u64(key.wrapping_sub(prev), &mut buf);
        }
        put_varint_u64(count, &mut buf);
        prev = key;
        if buf.len() >= (1 << 15) {
            tx.send(std::mem::take(&mut buf))
                .map_err(|_| anyhow::anyhow!("failed to send chunk"))?;
            buf = Vec::with_capacity(1 << 16);
        }
    }
    if !buf.is_empty() {
        tx.send(buf)
            .map_err(|_| anyhow::anyhow!("failed to send final chunk"))?;
    }
    Ok(())
}

/// 中文：把 `u128` 结果编码成定长二进制块并发送给写线程。
/// English: Encode `u128` entries as fixed-width binary chunks and send them
/// to the writer thread.
fn encode_u128_to_sender(entries: &[(u128, u64)], tx: &Sender<Vec<u8>>) -> Result<()> {
    let mut buf = Vec::with_capacity(1 << 16);
    for &(kmer, count) in entries {
        buf.extend_from_slice(&kmer.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        if buf.len() >= (1 << 15) {
            tx.send(std::mem::take(&mut buf))
                .map_err(|_| anyhow::anyhow!("failed to send chunk"))?;
            buf = Vec::with_capacity(1 << 16);
        }
    }
    if !buf.is_empty() {
        tx.send(buf)
            .map_err(|_| anyhow::anyhow!("failed to send final chunk"))?;
    }
    Ok(())
}

/// 中文：把 packed 长 k-mer 结果编码成定长二进制块并发送给写线程。
/// English: Encode packed long-k-mer entries as fixed-width binary chunks and
/// send them to the writer thread.
fn encode_packed_to_sender(entries: &[(Vec<u64>, u64)], tx: &Sender<Vec<u8>>) -> Result<()> {
    let mut buf = Vec::with_capacity(1 << 16);
    for (kmer, count) in entries {
        for &word in kmer {
            buf.extend_from_slice(&word.to_le_bytes());
        }
        buf.extend_from_slice(&count.to_le_bytes());
        if buf.len() >= (1 << 15) {
            tx.send(std::mem::take(&mut buf))
                .map_err(|_| anyhow::anyhow!("failed to send chunk"))?;
            buf = Vec::with_capacity(1 << 16);
        }
    }
    if !buf.is_empty() {
        tx.send(buf)
            .map_err(|_| anyhow::anyhow!("failed to send final chunk"))?;
    }
    Ok(())
}

struct ChunkFileReaderU64 {
    reader: BufReader<File>,
    done: bool,
    bytes_total: u64,
}

impl ChunkFileReaderU64 {
    fn open(path: &Path, buf_cap: usize) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("failed to open chunk {}", path.display()))?;
        let bytes_total = file.metadata()?.len();
        Ok(Self {
            reader: BufReader::with_capacity(buf_cap, file),
            done: false,
            bytes_total,
        })
    }

    fn next(&mut self) -> Result<Option<(u64, u64)>> {
        if self.done {
            return Ok(None);
        }
        let mut key = [0u8; 8];
        match self.reader.read_exact(&mut key) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.done = true;
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        }
        let mut count = [0u8; 8];
        self.reader.read_exact(&mut count)?;
        Ok(Some((u64::from_le_bytes(key), u64::from_le_bytes(count))))
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct HeapItemFileU64 {
    key: u64,
    count: u64,
    reader_idx: usize,
}

impl Ord for HeapItemFileU64 {
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.cmp(&self.key)
    }
}

impl PartialOrd for HeapItemFileU64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
struct MergeChunkFilesU64Stats {
    bytes_read: u64,
    merge_elapsed: Duration,
    output_elapsed: Duration,
}

fn merge_chunk_files_u64_into_blocks<W: Write>(
    paths: &[PathBuf],
    out: &mut W,
    level: i32,
    k: usize,
    block_buf: &mut Vec<(u64, u64)>,
    entries_total: &mut u64,
    block_count: &mut u32,
) -> Result<MergeChunkFilesU64Stats> {
    let t_total = Instant::now();
    let mut readers = Vec::with_capacity(paths.len());
    let mut heap = BinaryHeap::new();
    let reader_buf_cap = chunk_reader_buf_cap(paths.len());
    let mut bytes_read = 0u64;
    let mut output_elapsed = Duration::ZERO;
    for (idx, path) in paths.iter().enumerate() {
        let mut reader = ChunkFileReaderU64::open(path, reader_buf_cap)?;
        bytes_read += reader.bytes_total;
        if let Some((key, count)) = reader.next()? {
            heap.push(HeapItemFileU64 {
                key,
                count,
                reader_idx: idx,
            });
        }
        readers.push(reader);
    }

    while let Some(item) = heap.pop() {
        let key = item.key;
        let mut total = item.count;
        if let Some((next_key, next_count)) = readers[item.reader_idx].next()? {
            heap.push(HeapItemFileU64 {
                key: next_key,
                count: next_count,
                reader_idx: item.reader_idx,
            });
        }
        while let Some(peek) = heap.peek() {
            if peek.key != key {
                break;
            }
            let next = heap.pop().expect("peeked heap item must exist");
            total += next.count;
            if let Some((next_key, next_count)) = readers[next.reader_idx].next()? {
                heap.push(HeapItemFileU64 {
                    key: next_key,
                    count: next_count,
                    reader_idx: next.reader_idx,
                });
            }
        }
        block_buf.push((key, total));
        *entries_total += 1;
        if block_buf.len() >= BLOCK_RECORDS {
            let t_flush = Instant::now();
            flush_u64_block(out, block_buf, level, k, block_count)?;
            output_elapsed += t_flush.elapsed();
        }
    }
    Ok(MergeChunkFilesU64Stats {
        bytes_read,
        merge_elapsed: t_total.elapsed().saturating_sub(output_elapsed),
        output_elapsed,
    })
}

struct ChunkFileReaderU128 {
    reader: BufReader<File>,
    done: bool,
}

impl ChunkFileReaderU128 {
    fn open(path: &Path, buf_cap: usize) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("failed to open chunk {}", path.display()))?;
        Ok(Self {
            reader: BufReader::with_capacity(buf_cap, file),
            done: false,
        })
    }

    fn next(&mut self) -> Result<Option<(u128, u64)>> {
        if self.done {
            return Ok(None);
        }
        let mut key = [0u8; 16];
        match self.reader.read_exact(&mut key) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.done = true;
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        }
        let mut count = [0u8; 8];
        self.reader.read_exact(&mut count)?;
        Ok(Some((u128::from_le_bytes(key), u64::from_le_bytes(count))))
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct HeapItemFileU128 {
    key: u128,
    count: u64,
    reader_idx: usize,
}

impl Ord for HeapItemFileU128 {
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.cmp(&self.key)
    }
}

impl PartialOrd for HeapItemFileU128 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn merge_chunk_files_u128_into_blocks<W: Write>(
    paths: &[PathBuf],
    out: &mut W,
    level: i32,
    k: usize,
    block_buf: &mut Vec<(u128, u64)>,
    entries_total: &mut u64,
    block_count: &mut u32,
) -> Result<()> {
    let mut readers = Vec::with_capacity(paths.len());
    let mut heap = BinaryHeap::new();
    let reader_buf_cap = chunk_reader_buf_cap(paths.len());
    for (idx, path) in paths.iter().enumerate() {
        let mut reader = ChunkFileReaderU128::open(path, reader_buf_cap)?;
        if let Some((key, count)) = reader.next()? {
            heap.push(HeapItemFileU128 {
                key,
                count,
                reader_idx: idx,
            });
        }
        readers.push(reader);
    }

    while let Some(item) = heap.pop() {
        let key = item.key;
        let mut total = item.count;
        if let Some((next_key, next_count)) = readers[item.reader_idx].next()? {
            heap.push(HeapItemFileU128 {
                key: next_key,
                count: next_count,
                reader_idx: item.reader_idx,
            });
        }
        while let Some(peek) = heap.peek() {
            if peek.key != key {
                break;
            }
            let next = heap.pop().expect("peeked heap item must exist");
            total += next.count;
            if let Some((next_key, next_count)) = readers[next.reader_idx].next()? {
                heap.push(HeapItemFileU128 {
                    key: next_key,
                    count: next_count,
                    reader_idx: next.reader_idx,
                });
            }
        }
        block_buf.push((key, total));
        *entries_total += 1;
        if block_buf.len() >= BLOCK_RECORDS {
            flush_u128_block(out, block_buf, level, k, block_count)?;
        }
    }
    Ok(())
}

#[inline(always)]
fn chunk_reader_buf_cap(open_files: usize) -> usize {
    const MERGE_READER_BUDGET: usize = 64 << 20;
    const MIN_BUF: usize = 16 << 10;
    const MAX_BUF: usize = 256 << 10;
    (MERGE_READER_BUDGET / open_files.max(1)).clamp(MIN_BUF, MAX_BUF)
}

#[inline(always)]
/// 中文：把一个 `u64` 以 varint 形式写入缓冲区。
/// English: Append one `u64` to the buffer using varint encoding.
fn put_varint_u64(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push(((v as u8) & 0x7F) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}
