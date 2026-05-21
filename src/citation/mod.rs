use std::io::Write;
use std::time::Instant;

use crate::cli::{CitationArgs, CitationFormat};
use crate::kiteops::{create_output_writer, print_command_line};
use crate::stats::print_simple_test_report;

pub fn run_citation(args: &CitationArgs) -> anyhow::Result<()> {
    print_command_line("citation");
    let started = Instant::now();
    eprintln!(
        "[kite citation] format={:?}, output={}",
        args.format, args.output
    );
    let mut out = create_output_writer(&args.output)?;
    match args.format {
        CitationFormat::Text => {
            writeln!(out, "kite {}", env!("CARGO_PKG_VERSION"))?;
            writeln!(out, "Title: Kite")?;
            writeln!(out, "Description: k-mer index and throughput engine")?;
            writeln!(
                out,
                "Recommended citation: Kite software, version {}",
                env!("CARGO_PKG_VERSION")
            )?;
            writeln!(
                out,
                "Note: no formal DOI or manuscript metadata is embedded in this build yet. We recommend citing the software version and title, and including a link to the GitHub repository: https://github.com/jiehua1995/kite. Once a formal DOI and manuscript are available, we will update this citation information accordingly."
            )?;
        }
        CitationFormat::Bibtex => {
            writeln!(
                out,
                "@misc{{kite_{},",
                env!("CARGO_PKG_VERSION").replace('.', "_")
            )?;
            writeln!(out, "  title = {{Kite}},")?;
            writeln!(out, "  note = {{k-mer index and throughput engine}},")?;
            writeln!(out, "  year = {{2026}},")?;
            writeln!(
                out,
                "  howpublished = {{Software, version {}}}",
                env!("CARGO_PKG_VERSION")
            )?;
            writeln!(out, "}}")?;
        }
    }
    out.flush()?;
    if args.test_mode {
        print_simple_test_report(
            "citation",
            &[
                ("format", format!("{:?}", args.format).to_lowercase()),
                ("output", args.output.clone()),
                ("version", env!("CARGO_PKG_VERSION").to_string()),
            ],
            started.elapsed(),
        );
    }
    eprintln!(
        "[kite citation] done in {:.3}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
