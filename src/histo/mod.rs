use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::time::Instant;

use crate::cli::HistoArgs;
use crate::kiteops::{
    create_output_writer, infer_sample_labels, load_partition_counts, metadata_from_reader,
    open_readers, print_partition_progress,
};
use crate::stats::print_simple_test_report;

pub fn run_histo(args: &HistoArgs) -> anyhow::Result<()> {
    let started = Instant::now();
    let labels = infer_sample_labels(&args.inputs, args.labels.as_deref().unwrap_or(&[]))?;
    let readers = open_readers(&args.inputs)?;

    let mut histograms: Vec<BTreeMap<u64, u64>> = Vec::with_capacity(readers.len());
    let mut total_nonzero_bins = 0usize;
    for (file_index, reader) in readers.iter().enumerate() {
        let meta = metadata_from_reader(reader);
        let mut histogram = BTreeMap::new();
        for partition_id in 0..meta.partitions {
            print_partition_progress(
                &format!("histo {}", labels[file_index]),
                args.progress,
                partition_id + 1,
                meta.partitions,
                started,
            );
            for (_, count) in load_partition_counts(reader, partition_id)? {
                *histogram.entry(count).or_insert(0) += 1;
            }
        }
        total_nonzero_bins += histogram.len();
        histograms.push(histogram);
    }

    let mut all_bins = BTreeSet::new();
    for histogram in &histograms {
        all_bins.extend(histogram.keys().copied());
    }

    let mut out = create_output_writer(&args.output)?;
    write!(out, "count")?;
    for label in &labels {
        write!(out, "\t{}", label)?;
    }
    writeln!(out)?;
    let all_bins_len = all_bins.len();
    for count in all_bins {
        write!(out, "{}", count)?;
        for histogram in &histograms {
            write!(out, "\t{}", histogram.get(&count).copied().unwrap_or(0))?;
        }
        writeln!(out)?;
    }
    out.flush()?;
    if args.test_mode {
        print_simple_test_report(
            "histo",
            &[
                ("inputs", args.inputs.len().to_string()),
                ("labels", labels.join(",")),
                ("bins", all_bins_len.to_string()),
                ("nonzero_bins_total", total_nonzero_bins.to_string()),
                ("output", args.output.clone()),
            ],
            started.elapsed(),
        );
    }
    Ok(())
}
