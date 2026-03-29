use crate::cli::InfoArgs;
use crate::dump::decode::collect_stats;
use crate::utils::fmt::{format_duration, format_size};
use crate::utils::kite::{FLAG_DELTA_VARINT, FLAG_ZSTD, read_info};
use crate::utils::progress::ProgressSpinner;
use anyhow::Result;
use std::fs;
use std::time::Instant;

/// 中文：执行 `kite info`。输入是一个 `.kite` 数据库路径，
/// 输出是详细 metadata 和聚合统计的终端报告。
/// English: Execute `kite info`. It takes one `.kite` database path and
/// prints a detailed metadata/statistics report to stdout.
pub fn run(args: &InfoArgs) -> Result<()> {
    let t0 = Instant::now();
    let progress =
        ProgressSpinner::start(args.progress, "kite info", "scanning metadata and counts");
    let file_size = fs::metadata(&args.input).map(|m| m.len()).unwrap_or(0);
    let info = read_info(&args.input)?;
    let stats = collect_stats(&args.input)?;
    drop(progress);
    let elapsed = format_duration(t0.elapsed());

    println!(
        "[kite info] [Input: 1/1] {} ({})",
        args.input.display(),
        format_size(file_size)
    );
    println!("[kite info] format_version: {}", info.format_version);
    println!("[kite info] k: {}", info.k);
    println!("[kite info] mode: {}", mode_name(info.mode));
    println!("[kite info] canonical: {}", info.canonical());
    println!(
        "[kite info] encoding: {}",
        if (info.flags & FLAG_DELTA_VARINT) != 0 {
            "delta-varint"
        } else {
            "fixed-width"
        }
    );
    println!(
        "[kite info] payload_compression: {}",
        if (info.flags & FLAG_ZSTD) != 0 {
            "zstd"
        } else {
            "none"
        }
    );
    println!(
        "[kite info] source_path: {}",
        display_opt_str(info.metadata.source_path.as_deref())
    );
    println!(
        "[kite info] created_at: {}",
        display_opt_str(info.metadata.created_at.as_deref())
    );
    println!(
        "[kite info] write_mode: {}",
        display_opt_str(info.metadata.write_mode.as_deref())
    );
    println!(
        "[kite info] zstd_level: {}",
        display_opt_i32(info.metadata.zstd_level)
    );
    println!(
        "[kite info] count_threads: {}",
        display_opt_usize(info.metadata.count_threads)
    );
    println!(
        "[kite info] minimizer: {}",
        display_opt_usize(info.metadata.minimizer)
    );
    println!(
        "[kite info] partitions: {}",
        display_opt_usize(info.metadata.partitions)
    );
    println!(
        "[kite info] chunk_kmers: {}",
        display_opt_usize(info.metadata.chunk_kmers)
    );
    println!(
        "[kite info] total_reads: {}",
        display_opt_u64(info.metadata.total_reads)
    );
    println!(
        "[kite info] total_kmers: {}",
        info.metadata.total_kmers.unwrap_or(stats.total_count)
    );
    println!(
        "[kite info] unique_kmers: {}",
        info.metadata.unique_kmers.unwrap_or(info.entries)
    );
    println!("[kite info] singletons: {}", stats.singleton_kmers);
    println!("[kite info] max_count: {}", stats.max_count);
    println!(
        "[kite info] mean_count_per_kmer: {:.4}",
        stats.total_count as f64 / (stats.info.entries as f64).max(1.0)
    );
    println!(
        "[kite info] singleton_fraction: {:.4}",
        stats.singleton_kmers as f64 / (stats.info.entries as f64).max(1.0)
    );
    println!("[kite info] header_bytes: {}", info.header_bytes);
    println!("[kite info] metadata_bytes: {}", info.metadata_bytes);
    println!("[kite info] payload_bytes: {}", info.payload_bytes);
    println!("[kite info] file_size: {}", format_size(file_size));
    println!(
        "[kite info] compressed_bytes_per_entry: {:.4}",
        info.payload_bytes as f64 / (stats.info.entries as f64).max(1.0)
    );
    println!("[kite info] elapsed: {}", elapsed);
    Ok(())
}

/// 中文：把内部的 k-mer 编码模式转换成更适合终端显示的字符串。
/// 输入是 `KmerMode`，输出是 `u64/u128/packed` 之一。
/// English: Convert the internal k-mer encoding mode into a short display
/// label. Input is `KmerMode`; output is one of `u64/u128/packed`.
fn mode_name(mode: crate::utils::kite::KmerMode) -> &'static str {
    match mode {
        crate::utils::kite::KmerMode::U64 => "u64",
        crate::utils::kite::KmerMode::U128 => "u128",
        crate::utils::kite::KmerMode::Packed(_) => "packed",
    }
}

/// 中文：把可选字符串转成可打印文本，空值时显示 `n/a`。
/// English: Convert an optional string into printable text, using `n/a`
/// when the field is absent.
fn display_opt_str(value: Option<&str>) -> &str {
    value.unwrap_or("n/a")
}

/// 中文：把可选 `u64` 统计值转成可打印文本。
/// English: Convert an optional `u64` statistic into printable text.
fn display_opt_u64(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}

/// 中文：把可选 `i32` 统计值转成可打印文本。
/// English: Convert an optional `i32` statistic into printable text.
fn display_opt_i32(value: Option<i32>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}

/// 中文：把可选 `usize` 统计值转成可打印文本。
/// English: Convert an optional `usize` statistic into printable text.
fn display_opt_usize(value: Option<usize>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}
