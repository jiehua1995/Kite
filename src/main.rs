mod cli;
mod format;
mod count;
mod dump;
mod stats;
mod runtime;

use clap::Parser;
use cli::{Cli, Commands};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Count(args) => count::run_count(args)?,
        Commands::Dump(args)  => dump::run_dump(args)?,
    }
    Ok(())
}
