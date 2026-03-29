use crate::cli::QueryArgs;
use crate::dump::decode::query_kmer;
use crate::utils::fmt::{format_duration, format_size};
use crate::utils::progress::ProgressSpinner;
use anyhow::Result;
use std::fs;
use std::time::Instant;

/// 中文：执行 `kite query`。输入是 `.kite` 文件和一个 k-mer 字符串，
/// 输出是该 k-mer 的精确计数与基础执行信息。
/// English: Execute `kite query`. It takes a `.kite` file and one k-mer
/// string, then prints the exact count plus basic runtime information.
pub fn run(args: &QueryArgs) -> Result<()> {
    let t0 = Instant::now();
    let progress = ProgressSpinner::start(args.progress, "kite query", "searching for exact k-mer");
    let input_bytes = fs::metadata(&args.input).map(|m| m.len()).unwrap_or(0);
    let count = query_kmer(&args.input, &args.kmer)?;
    drop(progress);
    let elapsed = format_duration(t0.elapsed());
    println!(
        "[kite query] [Input: 1/1] {} ({})",
        args.input.display(),
        format_size(input_bytes)
    );
    println!("[kite query] kmer: {}", args.kmer);
    println!("[kite query] count: {}", count);
    println!("[kite query] elapsed: {}", elapsed);
    Ok(())
}
