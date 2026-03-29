use super::buffer::{CountOutcome, CountTable};
use crate::utils::kite::{
    CURRENT_FORMAT_VERSION, FLAG_BLOCK_FRAMES, FLAG_BLOCK_TSV_BYTES, FLAG_CANONICAL,
    FLAG_DELTA_VARINT, FLAG_ZSTD, KiteInfo, KiteMetadata, patch_payload_bytes, write_header,
};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use rayon::prelude::*;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread;

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
