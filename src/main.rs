// 中文：主入口只负责解析命令行并把请求分发到对应子命令模块。
// English: The binary entry point only parses CLI arguments and dispatches
// each request to the matching subcommand module.
mod citation;
mod cli;
mod count;
mod dump;
mod histo;
mod info;
mod merge;
mod profile;
mod query;
mod sketch;
mod utils;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// 中文：程序总入口。输入是命令行参数，输出是子命令执行结果；
/// 如果子命令失败，就把错误继续向上传递给 `anyhow`。
/// English: Program entry point. It consumes CLI arguments and returns the
/// selected subcommand result, bubbling any failure up through `anyhow`.
fn main() -> Result<()> {
    let cli = Cli::parse();

    // 中文：这里不做业务逻辑，只做命令分发，保证每个子命令模块独立。
    // English: No business logic lives here; this keeps each subcommand
    // self-contained and allows command-specific optimization later.
    match cli.command {
        Commands::Count(args) => count::run(&args)?,
        Commands::Dump(args) => dump::run(&args)?,
        Commands::Info(args) => info::run(&args)?,
        Commands::Query(args) => query::run(&args)?,
        Commands::Histo(args) => histo::run(&args)?,
        Commands::Profile(args) => profile::run(&args)?,
        Commands::Merge(args) => merge::run(&args)?,
        Commands::Sketch(args) => sketch::run(&args)?,
        Commands::Citation => citation::run()?,
    }

    Ok(())
}
