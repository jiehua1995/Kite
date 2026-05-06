mod analyze;
mod citation;
mod cli;
mod count;
mod dump;
mod fly;
mod format;
mod histo;
mod info;
mod kiteops;
mod merge;
mod profile;
mod query;
mod runtime;
mod stats;

use clap::Parser;
use cli::{Cli, Commands};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Count(args) => count::run_count(args)?,
        Commands::Dump(args) => dump::run_dump(args)?,
        Commands::Info(args) => info::run_info(args)?,
        Commands::Query(args) => query::run_query(args)?,
        Commands::Histo(args) => histo::run_histo(args)?,
        Commands::Merge(args) => merge::run_merge(args)?,
        Commands::Profile(args) => profile::run_profile(args)?,
        Commands::Analyze(args) => analyze::run_analyze(args)?,
        Commands::Citation(args) => citation::run_citation(args)?,
        Commands::Fly(args) => fly::run_fly(args)?,
    }
    Ok(())
}
