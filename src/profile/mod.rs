use crate::cli::ProfileArgs;
use crate::utils::fmt::{format_duration, format_size};
use crate::utils::progress::ProgressSpinner;
use anyhow::Result;
use std::fs;
use std::time::Instant;

// 中文：`profile` 专门负责把多个 `.kite` 文件对齐成一个原始 count 矩阵。
// English: `profile` is dedicated to aligning multiple `.kite` files into one
// raw-count matrix.
pub mod kmer;
pub mod vector;

/// 中文：执行 `kite profile`。输入是多个 `.kite` 文件，输出是一个多样本矩阵文件。
/// English: Execute `kite profile`. It takes multiple `.kite` files and
/// writes a single multi-sample matrix file.
pub fn run(args: &ProfileArgs) -> Result<()> {
    let t0 = Instant::now();
    let progress = ProgressSpinner::start(
        args.progress,
        "kite profile",
        "building multi-sample count matrix",
    );
    vector::run_profile(&args.input, &args.output, &args.format, &args.sample_names)?;
    drop(progress);
    let elapsed = format_duration(t0.elapsed());
    let out_bytes = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);
    let total = args.input.len();
    for (idx, path) in args.input.iter().enumerate() {
        let in_bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        println!(
            "[kite profile] [Input: {}/{}] {} ({})",
            idx + 1,
            total,
            path.display(),
            format_size(in_bytes)
        );
    }
    println!(
        "[kite profile] [Output: 1/1] {} ({})",
        args.output.display(),
        format_size(out_bytes)
    );
    println!("[kite profile] elapsed: {}", elapsed);
    Ok(())
}
