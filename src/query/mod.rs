use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;

use crate::cli::QueryArgs;
use crate::dump::reader::KiteReader;
use crate::kiteops::{
    bucket_for_key, count_passes_reader_filter, encode_query_kmer, format_count_filter,
    key_to_string, load_partition_counts, metadata_from_reader,
};
use crate::stats::print_simple_test_report;

pub fn run_query(args: &QueryArgs) -> anyhow::Result<()> {
    let started = Instant::now();
    eprintln!("[kite query] opening: {}", args.input);
    eprintln!("[kite query] query:   {}", args.kmer);
    let reader = Arc::new(
        KiteReader::open(&args.input).with_context(|| format!("failed to open {}", args.input))?,
    );
    let meta = metadata_from_reader(&reader);
    let query_key = encode_query_kmer(&args.kmer, &meta)?;
    let partition_id = bucket_for_key(query_key, meta.partitions, meta.key_width);

    let rows = load_partition_counts(&reader, partition_id)?;
    let count = rows
        .binary_search_by_key(&query_key, |entry| entry.0)
        .ok()
        .map(|index| rows[index].1)
        .unwrap_or(0);

    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    writeln!(out, "Kite Query")?;
    writeln!(out, "  Database:           {}", args.input)?;
    writeln!(
        out,
        "  Query:              {}",
        key_to_string(query_key, &meta)
    )?;
    writeln!(out, "  Partition:          {}", partition_id)?;
    writeln!(out, "  Count:              {}", count)?;
    writeln!(
        out,
        "  Header Min Count:   {}",
        format_count_filter(reader.header.min_count)
    )?;
    writeln!(
        out,
        "  Header Max Count:   {}",
        format_count_filter(reader.header.max_count)
    )?;
    let excluded_by_header = count > 0 && !count_passes_reader_filter(&reader, count);
    writeln!(
        out,
        "  Header Filter:      {}",
        if count == 0 {
            "not present"
        } else if excluded_by_header {
            "excluded from downstream analysis/output"
        } else {
            "included"
        }
    )?;
    out.flush()?;
    if args.test_mode {
        print_simple_test_report(
            "query",
            &[
                ("input", args.input.clone()),
                ("query", args.kmer.clone()),
                ("partition", partition_id.to_string()),
                ("count", count.to_string()),
                ("header_min", format_count_filter(reader.header.min_count)),
                ("header_max", format_count_filter(reader.header.max_count)),
                (
                    "header_filter",
                    if count == 0 {
                        "not_present".to_string()
                    } else if excluded_by_header {
                        "excluded".to_string()
                    } else {
                        "included".to_string()
                    },
                ),
            ],
            started.elapsed(),
        );
    }
    eprintln!(
        "[kite query] done in {:.3}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
