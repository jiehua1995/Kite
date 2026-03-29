use crate::cli::DumpArgs;
use crate::utils::fmt::{format_duration, format_size};
use crate::utils::kite::read_info;
use crate::utils::progress::ProgressSpinner;
use anyhow::Result;
use std::fs;
use std::time::Instant;

// 中文：`dump` 子命令目录只保留导出 `.kite -> TSV` 所需的逻辑。
// English: The `dump` command directory only keeps logic required to export
// `.kite -> TSV`.
pub mod decode;
pub mod kmer;

/// 中文：执行 `kite dump`。输入是 `.kite` 文件、输出路径和线程数，
/// 输出是 TSV 文本表，同时打印输入输出大小和耗时。
/// English: Execute `kite dump`. It takes a `.kite` file, output path, and
/// thread count, then writes a TSV plus a short runtime summary.
pub fn run(args: &DumpArgs) -> Result<()> {
    let t0 = Instant::now();
    let progress = ProgressSpinner::start(args.progress, "kite dump", "dumping database");
    let info = read_info(&args.input)?;
    decode::dump_binary_to_tsv(&args.input, &args.output, args.threads)?;
    drop(progress);
    let elapsed = format_duration(t0.elapsed());
    let input_bytes = fs::metadata(&args.input).map(|m| m.len()).unwrap_or(0);
    let output_bytes = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);
    println!(
        "[kite dump] [Input: 1/1] {} ({})",
        args.input.display(),
        format_size(input_bytes)
    );
    println!(
        "[kite dump] [Output: 1/1] {} ({})",
        args.output.display(),
        format_size(output_bytes)
    );
    println!("[kite dump] canonical: {}", info.canonical());
    println!("[kite dump] elapsed: {}", elapsed);
    Ok(())
}
