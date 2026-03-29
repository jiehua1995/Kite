use crate::cli::HistoArgs;
use crate::dump::decode::write_histogram;
use crate::utils::fmt::{format_duration, format_size};
use crate::utils::progress::ProgressSpinner;
use anyhow::Result;
use std::fs;
use std::time::Instant;

/// 中文：执行 `kite histo`。输入是一个 `.kite` 文件和输出路径，
/// 输出是直方图 TSV，同时在终端打印摘要统计。
/// English: Execute `kite histo`. It reads one `.kite` database and writes a
/// histogram TSV, while also printing a summary to stdout.
pub fn run(args: &HistoArgs) -> Result<()> {
    let t0 = Instant::now();
    let progress = ProgressSpinner::start(args.progress, "kite histo", "building count histogram");
    let input_bytes = fs::metadata(&args.input).map(|m| m.len()).unwrap_or(0);
    let summary = write_histogram(&args.input, &args.output)?;
    drop(progress);
    let elapsed = format_duration(t0.elapsed());
    let out_bytes = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);
    println!(
        "[kite histo] [Input: 1/1] {} ({})",
        args.input.display(),
        format_size(input_bytes)
    );
    println!(
        "[kite histo] [Output: 1/1] {} ({})",
        args.output.display(),
        format_size(out_bytes)
    );
    println!("[kite histo] entries: {}", summary.info.entries);
    println!("[kite histo] distinct_bins: {}", summary.distinct_bins);
    println!("[kite histo] total_count: {}", summary.total_count);
    println!("[kite histo] singletons: {}", summary.singleton_kmers);
    println!("[kite histo] max_count: {}", summary.max_count);
    println!("[kite histo] elapsed: {}", elapsed);
    Ok(())
}
