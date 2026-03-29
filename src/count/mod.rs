use crate::cli::{CanonicalMode, CountArgs, CountWriteMode};
use crate::utils::fmt::{format_duration, format_size};
use crate::utils::kite::{KiteMetadata, current_timestamp_rfc3339};
use anyhow::Result;
use std::fs;
use std::sync::Arc;

// 中文：`count` 目录专门处理 FASTQ -> `.kite` 的构建路径，
// 包含读取、滚动编码、分桶、排序、归并和最终写库。
// English: The `count` directory owns the FASTQ -> `.kite` build pipeline,
// including reading, rolling encoding, partitioning, sorting, merging, and
// final database writing.
pub mod buffer;
pub mod decode;
pub mod fastq;
pub mod kmer;
pub mod merge;
pub mod minimizer;
pub mod radix_sort;

/// 中文：执行 `kite count`。输入是 FASTQ 和构建参数，输出是 `.kite` 数据库
/// 以及终端上的 reads/k-mers/unique 等摘要信息。
/// English: Execute `kite count`. Inputs are FASTQ plus build parameters;
/// outputs are the `.kite` database and a stdout summary of reads/k-mers/uniq.
pub fn run(args: &CountArgs) -> Result<()> {
    let input_bytes = fs::metadata(&args.input).map(|m| m.len()).unwrap_or(0);
    let cfg = buffer::CountConfig {
        k: args.k,
        minimizer: args.minimizer,
        partitions: args.partitions,
        threads: args.threads,
        chunk_kmers: args.chunk_kmers,
        canonical: matches!(args.canonical, CanonicalMode::True),
        progress: args.progress,
        progress_counters: if args.progress {
            Some(Arc::new(buffer::ProgressCounters::new()))
        } else {
            None
        },
    };

    let outcome = if args.baseline {
        buffer::count_baseline(&args.input, &cfg)?
    } else {
        buffer::count_optimized(&args.input, &cfg)?
    };

    decode::write_binary(
        &args.output,
        args.k,
        &outcome,
        decode::WriteBinaryOptions {
            mode: match args.mode {
                CountWriteMode::Fast => decode::CompressionMode::Fast,
                CountWriteMode::Balanced => decode::CompressionMode::Balanced,
                CountWriteMode::Compact => decode::CompressionMode::Compact,
            },
            zstd_level: args.zstd_level,
            threads: args.threads,
            metadata: KiteMetadata {
                source_path: Some(args.input.display().to_string()),
                created_at: Some(current_timestamp_rfc3339()),
                total_reads: Some(outcome.total_reads),
                total_kmers: Some(outcome.total_kmers),
                unique_kmers: None,
                minimizer: Some(args.minimizer),
                partitions: Some(args.partitions),
                count_threads: Some(args.threads),
                chunk_kmers: Some(args.chunk_kmers),
                canonical: Some(matches!(args.canonical, CanonicalMode::True)),
                write_mode: Some(
                    match args.mode {
                        CountWriteMode::Fast => "fast",
                        CountWriteMode::Balanced => "balanced",
                        CountWriteMode::Compact => "compact",
                    }
                    .to_string(),
                ),
                zstd_level: None,
            },
        },
    )?;

    let unique = match &outcome.table {
        buffer::CountTable::U64(v) => v.len(),
        buffer::CountTable::U128(v) => v.len(),
        buffer::CountTable::Packed(v) => v.len(),
    };
    let file_bytes = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);

    println!(
        "[kite count] [Input: 1/1] {} ({})",
        args.input.display(),
        format_size(input_bytes)
    );
    println!("[kite count] reads: {}", outcome.total_reads);
    println!("[kite count] kmers: {}", outcome.total_kmers);
    println!("[kite count] unique_kmers: {}", unique);
    println!(
        "[kite count] canonical: {}",
        matches!(args.canonical, CanonicalMode::True)
    );
    println!(
        "[kite count] [Output: 1/1] {} ({})",
        args.output.display(),
        format_size(file_bytes)
    );
    println!(
        "[kite count] elapsed: {}",
        format_duration(outcome.timings.total)
    );
    Ok(())
}
