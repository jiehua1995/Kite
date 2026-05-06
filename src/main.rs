mod cli;
mod format;
mod count;
mod dump;
mod stats;

use clap::Parser;
use cli::{Cli, Commands};

// Use mimalloc as the global allocator for reduced malloc overhead,
// especially on many-thread workloads with frequent small allocations.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Count(args) => count::run_count(args)?,
        Commands::Dump(args)  => dump::run_dump(args)?,
    }
    Ok(())
}
