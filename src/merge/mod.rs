use crate::cli::{CountWriteMode, MergeArgs};
use crate::count::buffer::{CountOutcome, CountTable, TimingBreakdown};
use crate::count::decode::{CompressionMode, WriteBinaryOptions, write_binary};
use crate::utils::fmt::{format_duration, format_size};
use crate::utils::kite::{
    FLAG_BLOCK_FRAMES, FLAG_DELTA_VARINT, FLAG_ZSTD, KiteInfo, KiteMetadata, KmerMode,
    current_timestamp_rfc3339, read_info_from_reader,
};
use crate::utils::progress::ProgressSpinner;
use anyhow::{Context, Result, bail};
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

// 中文：`merge` 子命令负责把两个或更多 `.kite` 文件中的相同 k-mer 计数直接相加，
// 并写出新的 `.kite` 数据库。所有输入必须具有相同的 k、内部编码模式和 canonical 语义。
// English: The `merge` subcommand sums matching k-mers across two or more
// `.kite` files and writes the combined result as a new `.kite` database.
// All inputs must share the same k, internal storage mode, and canonical semantics.

struct LoadedTable {
    info: KiteInfo,
    table: CountTable,
    total_count: u64,
    total_reads: Option<u64>,
}

/// 中文：执行 `kite merge`。输入是两个或更多 `.kite` 文件和一个输出路径；
/// 输出是一个新的 `.kite` 文件，其每个 k-mer 计数等于所有输入中的精确求和。
/// English: Execute `kite merge`. Inputs are two or more `.kite` files plus
/// an output path; output is a new `.kite` file whose counts are the exact sums
/// across all compatible inputs.
pub fn run(args: &MergeArgs) -> Result<()> {
    let t0 = Instant::now();
    let progress = ProgressSpinner::start(args.progress, "kite merge", "loading and summing");

    let mut loaded = Vec::with_capacity(args.input.len());
    let mut total_input_bytes = 0u64;
    for path in &args.input {
        total_input_bytes += fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        loaded.push(load_table(path)?);
    }

    for idx in 1..loaded.len() {
        ensure_compatible(
            &loaded[0].info,
            &loaded[idx].info,
            &args.input[0],
            &args.input[idx],
        )?;
    }

    let reference_info = loaded[0].info.clone();
    let reference_minimizer = loaded[0].info.metadata.minimizer;
    let all_same_minimizer = loaded
        .iter()
        .all(|item| item.info.metadata.minimizer == reference_minimizer);
    let total_count = loaded.iter().map(|item| item.total_count).sum::<u64>();
    let total_reads = if loaded.iter().all(|item| item.total_reads.is_some()) {
        Some(
            loaded
                .iter()
                .map(|item| item.total_reads.unwrap_or(0))
                .sum::<u64>(),
        )
    } else {
        None
    };

    let mut tables = loaded.into_iter();
    let first = tables
        .next()
        .context("merge requires at least two input .kite files")?;
    let mut merged_table = first.table;
    for item in tables {
        merged_table = merge_tables(merged_table, item.table)?;
    }

    let merged_unique = match &merged_table {
        CountTable::U64(v) => v.len() as u64,
        CountTable::U128(v) => v.len() as u64,
        CountTable::Packed(v) => v.len() as u64,
    };

    let outcome = CountOutcome {
        mode: reference_info.mode,
        table: merged_table,
        total_kmers: total_count,
        total_reads: total_reads.unwrap_or(0),
        canonical: reference_info.canonical(),
        timings: TimingBreakdown {
            parsing: Duration::ZERO,
            kmer_generation: Duration::ZERO,
            partition_routing: Duration::ZERO,
            sorting: Duration::ZERO,
            merging: t0.elapsed(),
            total: t0.elapsed(),
        },
    };

    write_binary(
        &args.output,
        reference_info.k,
        &outcome,
        WriteBinaryOptions {
            mode: map_write_mode(args.mode),
            zstd_level: args.zstd_level,
            threads: args.threads,
            metadata: KiteMetadata {
                source_path: Some(format!(
                    "merge:{}",
                    args.input
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("+")
                )),
                created_at: Some(current_timestamp_rfc3339()),
                total_reads,
                total_kmers: Some(total_count),
                unique_kmers: Some(merged_unique),
                minimizer: if all_same_minimizer {
                    reference_minimizer
                } else {
                    None
                },
                partitions: None,
                count_threads: Some(args.threads),
                chunk_kmers: None,
                canonical: Some(reference_info.canonical()),
                write_mode: Some(
                    match args.mode {
                        CountWriteMode::Fast => "fast",
                        CountWriteMode::Balanced => "balanced",
                        CountWriteMode::Compact => "compact",
                    }
                    .to_string(),
                ),
                zstd_level: args.zstd_level,
            },
        },
    )?;

    drop(progress);

    let output_bytes = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);
    for (idx, path) in args.input.iter().enumerate() {
        let input_bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        println!(
            "[kite merge] [Input: {}/{}] {} ({})",
            idx + 1,
            args.input.len(),
            path.display(),
            format_size(input_bytes)
        );
    }
    println!("[kite merge] inputs: {}", args.input.len());
    println!(
        "[kite merge] input_total_bytes: {}",
        format_size(total_input_bytes)
    );
    println!("[kite merge] k: {}", reference_info.k);
    println!("[kite merge] canonical: {}", reference_info.canonical());
    println!("[kite merge] mode: {:?}", reference_info.mode);
    println!("[kite merge] merged_total_kmers: {}", total_count);
    println!("[kite merge] merged_unique_kmers: {}", merged_unique);
    println!(
        "[kite merge] [Output: 1/1] {} ({})",
        args.output.display(),
        format_size(output_bytes)
    );
    println!("[kite merge] elapsed: {}", format_duration(t0.elapsed()));
    Ok(())
}

fn map_write_mode(mode: CountWriteMode) -> CompressionMode {
    match mode {
        CountWriteMode::Fast => CompressionMode::Fast,
        CountWriteMode::Balanced => CompressionMode::Balanced,
        CountWriteMode::Compact => CompressionMode::Compact,
    }
}

fn ensure_compatible(
    left: &KiteInfo,
    right: &KiteInfo,
    left_path: &Path,
    right_path: &Path,
) -> Result<()> {
    if left.k != right.k {
        bail!(
            "merge requires the same k in all inputs: {} has k={}, {} has k={}",
            left_path.display(),
            left.k,
            right_path.display(),
            right.k
        );
    }
    if left.mode != right.mode {
        bail!(
            "merge requires the same internal mode in all inputs: {} uses {:?}, {} uses {:?}",
            left_path.display(),
            left.mode,
            right_path.display(),
            right.mode
        );
    }
    if left.canonical() != right.canonical() {
        bail!(
            "merge requires matching canonical settings: {} uses {}, {} uses {}",
            left_path.display(),
            left.canonical(),
            right_path.display(),
            right.canonical()
        );
    }
    Ok(())
}

fn load_table(path: &Path) -> Result<LoadedTable> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let info = read_info_from_reader(&mut reader)?;
    let payload = read_payload_bytes(&mut reader, &info)?;
    let total_reads = info.metadata.total_reads;
    match info.mode {
        KmerMode::U64 => {
            let (entries, total_count) = decode_u64_entries(&payload, &info)?;
            Ok(LoadedTable {
                info,
                table: CountTable::U64(entries),
                total_count,
                total_reads,
            })
        }
        KmerMode::U128 => {
            let (entries, total_count) = decode_u128_entries(&payload, &info)?;
            Ok(LoadedTable {
                info,
                table: CountTable::U128(entries),
                total_count,
                total_reads,
            })
        }
        KmerMode::Packed(_) => {
            let (entries, total_count) = decode_packed_entries(&payload, &info)?;
            Ok(LoadedTable {
                info,
                table: CountTable::Packed(entries),
                total_count,
                total_reads,
            })
        }
    }
}

fn read_payload_bytes<R: Read + Seek>(reader: &mut R, info: &KiteInfo) -> Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(info.header_bytes))?;
    if (info.flags & FLAG_BLOCK_FRAMES) != 0 {
        read_block_payload(reader)
    } else {
        let mut payload = vec![0u8; info.payload_bytes as usize];
        reader.read_exact(&mut payload)?;
        if (info.flags & FLAG_ZSTD) != 0 {
            zstd::stream::decode_all(Cursor::new(payload)).context("failed to decode zstd payload")
        } else {
            Ok(payload)
        }
    }
}

fn read_block_payload<R: Read>(reader: &mut R) -> Result<Vec<u8>> {
    let mut blocks_buf = [0u8; 4];
    reader.read_exact(&mut blocks_buf)?;
    let blocks = u32::from_le_bytes(blocks_buf) as usize;
    let mut raw_all = Vec::<u8>::new();
    for _ in 0..blocks {
        let mut header = [0u8; 16];
        reader.read_exact(&mut header)?;
        let raw_bytes = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let compressed_bytes = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        let mut compressed = vec![0u8; compressed_bytes];
        reader.read_exact(&mut compressed)?;
        let raw = zstd::bulk::decompress(&compressed, raw_bytes)?;
        raw_all.extend_from_slice(&raw);
    }
    Ok(raw_all)
}

fn decode_u64_entries(payload: &[u8], info: &KiteInfo) -> Result<(Vec<(u64, u64)>, u64)> {
    if (info.flags & FLAG_DELTA_VARINT) != 0 {
        decode_u64_delta_varint(payload, info.entries)
    } else {
        let mut entries = Vec::with_capacity(info.entries as usize);
        let mut total = 0u64;
        let mut offset = 0usize;
        while offset + 16 <= payload.len() {
            let key = u64::from_le_bytes(payload[offset..offset + 8].try_into().unwrap());
            let count = u64::from_le_bytes(payload[offset + 8..offset + 16].try_into().unwrap());
            entries.push((key, count));
            total += count;
            offset += 16;
        }
        Ok((entries, total))
    }
}

fn decode_u64_delta_varint(payload: &[u8], entries_n: u64) -> Result<(Vec<(u64, u64)>, u64)> {
    let mut cur = Cursor::new(payload);
    let mut out = Vec::with_capacity(entries_n as usize);
    let mut prev = 0u64;
    let mut total = 0u64;
    for idx in 0..entries_n {
        let delta = get_varint_u64(&mut cur)?;
        let count = get_varint_u64(&mut cur)?;
        let key = if idx == 0 {
            delta
        } else {
            prev.wrapping_add(delta)
        };
        out.push((key, count));
        total += count;
        prev = key;
    }
    Ok((out, total))
}

fn decode_u128_entries(payload: &[u8], info: &KiteInfo) -> Result<(Vec<(u128, u64)>, u64)> {
    let mut entries = Vec::with_capacity(info.entries as usize);
    let mut total = 0u64;
    let mut offset = 0usize;
    while offset + 24 <= payload.len() {
        let key = u128::from_le_bytes(payload[offset..offset + 16].try_into().unwrap());
        let count = u64::from_le_bytes(payload[offset + 16..offset + 24].try_into().unwrap());
        entries.push((key, count));
        total += count;
        offset += 24;
    }
    Ok((entries, total))
}

fn decode_packed_entries(payload: &[u8], info: &KiteInfo) -> Result<(Vec<(Vec<u64>, u64)>, u64)> {
    let record_size = info.words * 8 + 8;
    let mut entries = Vec::with_capacity(info.entries as usize);
    let mut total = 0u64;
    let mut offset = 0usize;
    while offset + record_size <= payload.len() {
        let mut words = Vec::with_capacity(info.words);
        for word_idx in 0..info.words {
            let start = offset + word_idx * 8;
            words.push(u64::from_le_bytes(
                payload[start..start + 8].try_into().unwrap(),
            ));
        }
        let count_start = offset + info.words * 8;
        let count = u64::from_le_bytes(payload[count_start..count_start + 8].try_into().unwrap());
        entries.push((words, count));
        total += count;
        offset += record_size;
    }
    Ok((entries, total))
}

fn merge_tables(left: CountTable, right: CountTable) -> Result<CountTable> {
    match (left, right) {
        (CountTable::U64(a), CountTable::U64(b)) => Ok(CountTable::U64(merge_u64(a, b))),
        (CountTable::U128(a), CountTable::U128(b)) => Ok(CountTable::U128(merge_u128(a, b))),
        (CountTable::Packed(a), CountTable::Packed(b)) => {
            Ok(CountTable::Packed(merge_packed(a, b)))
        }
        _ => bail!("merge inputs use incompatible table variants"),
    }
}

fn merge_u64(left: Vec<(u64, u64)>, right: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < left.len() && j < right.len() {
        match left[i].0.cmp(&right[j].0) {
            Ordering::Less => {
                out.push(left[i]);
                i += 1;
            }
            Ordering::Greater => {
                out.push(right[j]);
                j += 1;
            }
            Ordering::Equal => {
                out.push((left[i].0, left[i].1 + right[j].1));
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&left[i..]);
    out.extend_from_slice(&right[j..]);
    out
}

fn merge_u128(left: Vec<(u128, u64)>, right: Vec<(u128, u64)>) -> Vec<(u128, u64)> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < left.len() && j < right.len() {
        match left[i].0.cmp(&right[j].0) {
            Ordering::Less => {
                out.push(left[i]);
                i += 1;
            }
            Ordering::Greater => {
                out.push(right[j]);
                j += 1;
            }
            Ordering::Equal => {
                out.push((left[i].0, left[i].1 + right[j].1));
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&left[i..]);
    out.extend_from_slice(&right[j..]);
    out
}

fn merge_packed(left: Vec<(Vec<u64>, u64)>, right: Vec<(Vec<u64>, u64)>) -> Vec<(Vec<u64>, u64)> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < left.len() && j < right.len() {
        match left[i].0.cmp(&right[j].0) {
            Ordering::Less => {
                out.push(left[i].clone());
                i += 1;
            }
            Ordering::Greater => {
                out.push(right[j].clone());
                j += 1;
            }
            Ordering::Equal => {
                out.push((left[i].0.clone(), left[i].1 + right[j].1));
                i += 1;
                j += 1;
            }
        }
    }
    out.extend(left[i..].iter().cloned());
    out.extend(right[j..].iter().cloned());
    out
}

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
    bail!("invalid varint sequence in merged input")
}
