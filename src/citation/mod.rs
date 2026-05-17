use std::io::Write;
use std::time::Instant;

use crate::cli::{CitationArgs, CitationFormat};
use crate::kiteops::create_output_writer;
use crate::stats::print_simple_test_report;

pub fn run_citation(args: &CitationArgs) -> anyhow::Result<()> {
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
            writeln!(
                out,
                "Description: High-performance two-stage k-mer database toolkit"
            )?;
            writeln!(
                out,
                "Recommended citation: Kite software, version {}",
                env!("CARGO_PKG_VERSION")
            )?;
            writeln!(
                out,
                "Note: no formal DOI or manuscript metadata is embedded in this build yet."
            )?;
        }
        CitationFormat::Bibtex => {
            writeln!(
                out,
                "@misc{{kite_{},",
                env!("CARGO_PKG_VERSION").replace('.', "_")
            )?;
            writeln!(out, "  title = {{Kite}},")?;
            writeln!(
                out,
                "  note = {{High-performance two-stage k-mer database toolkit}},"
            )?;
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
