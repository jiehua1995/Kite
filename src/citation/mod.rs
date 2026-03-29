use anyhow::Result;

/// 中文：打印当前版本和引用信息。输入为空，输出是终端上的引用提示文本。
/// English: Print the current version and citation text. It takes no input
/// and writes a citation reminder to stdout.
pub fn run() -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    println!("[kite citation] version: {}", version);
    println!(
        "[kite citation] Please cite: Jie Hua et al. (2026). Kite: K-mer Indexing and Throughput Engine. DOI: <placeholder>>"
    );
    Ok(())
}
