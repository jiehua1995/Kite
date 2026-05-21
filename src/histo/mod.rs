use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use rayon::{prelude::*, ThreadPoolBuilder};

use crate::cli::HistoArgs;
use crate::kiteops::{
    apply_reader_filter_to_rows, create_output_writer, format_reader_filter, infer_sample_labels,
    load_partition_counts, metadata_from_reader, open_readers, print_command_line,
    PartitionProgress,
};
use crate::stats::print_simple_test_report;

pub fn run_histo(args: &HistoArgs) -> anyhow::Result<()> {
    print_command_line("histo");
    let started = Instant::now();
    let labels = infer_sample_labels(&args.inputs, args.labels.as_deref().unwrap_or(&[]))?;
    eprintln!(
        "[kite histo] inputs={}, output={}",
        args.inputs.len(),
        args.output
    );
    let readers = open_readers(&args.inputs)?;
    eprintln!(
        "[kite histo] labels={} ({})",
        labels.join(","),
        if args.labels.is_some() {
            "user supplied"
        } else {
            "inferred from file names"
        }
    );
    for (index, (path, reader)) in args.inputs.iter().zip(readers.iter()).enumerate() {
        eprintln!(
            "[kite histo] input #{:>2} label={} path={} k={} canonical={} partitions={} key_width={} rows={} chunks={} filters: {}",
            index + 1,
            labels[index],
            path,
            reader.header.k,
            reader.header.canonical != 0,
            reader.header.partitions,
            reader.header.key_width,
            reader.header.total_rows,
            reader.total_chunks,
            format_reader_filter(reader)
        );
    }

    let total_partitions: usize = readers
        .iter()
        .map(|reader| reader.header.partitions as usize)
        .sum();
    let progress = PartitionProgress::new("histo", args.progress, total_partitions, started);
    let done = AtomicUsize::new(0);
    let worker_threads = if args.threads == 0 {
        num_cpus::get().max(1)
    } else {
        args.threads.max(1)
    };
    eprintln!(
        "[kite histo] worker_threads={} (parallel within each input file)",
        worker_threads
    );
    let pool = ThreadPoolBuilder::new()
        .num_threads(worker_threads)
        .thread_name(|index| format!("kite-histo-{index}"))
        .build()
        .ok();
    if pool.is_none() && worker_threads > 1 {
        eprintln!(
            "[kite histo] failed to create local worker pool; falling back to serial partition processing"
        );
    }
    let compute_one = |reader: &std::sync::Arc<crate::dump::reader::KiteReader>|
     -> anyhow::Result<BTreeMap<u64, u64>> {
        let meta = metadata_from_reader(reader);
        if let Some(pool) = &pool {
            pool.install(|| {
                (0..meta.partitions)
                    .into_par_iter()
                    .map(|partition_id| {
                        let mut rows = load_partition_counts(reader, partition_id)?;
                        apply_reader_filter_to_rows(reader, &mut rows);
                        let mut histogram = BTreeMap::new();
                        for (_, count) in rows {
                            *histogram.entry(count).or_insert(0) += 1;
                        }
                        let completed = done.fetch_add(1, Ordering::Relaxed) + 1;
                        progress.update(completed);
                        Ok(histogram)
                    })
                    .try_reduce(BTreeMap::new, |left, right| {
                        Ok(merge_histograms(left, right))
                    })
            })
        } else {
            let mut histogram = BTreeMap::new();
            for partition_id in 0..meta.partitions {
                let mut rows = load_partition_counts(reader, partition_id)?;
                apply_reader_filter_to_rows(reader, &mut rows);
                for (_, count) in rows {
                    *histogram.entry(count).or_insert(0) += 1;
                }
                let completed = done.fetch_add(1, Ordering::Relaxed) + 1;
                progress.update(completed);
            }
            Ok(histogram)
        }
    };
    let mut indexed_histograms = Vec::with_capacity(readers.len());
    for (file_index, reader) in readers.iter().enumerate() {
        indexed_histograms.push((file_index, compute_one(reader)?));
    }
    progress.finish();
    indexed_histograms.sort_by_key(|(index, _)| *index);
    let total_nonzero_bins: usize = indexed_histograms
        .iter()
        .map(|(_, histogram)| histogram.len())
        .sum();
    let histograms = indexed_histograms
        .into_iter()
        .map(|(_, histogram)| histogram)
        .collect::<Vec<_>>();

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
                ("threads", args.threads.to_string()),
                ("worker_threads", worker_threads.to_string()),
                ("bins", all_bins_len.to_string()),
                ("nonzero_bins_total", total_nonzero_bins.to_string()),
                ("output", args.output.clone()),
            ],
            started.elapsed(),
        );
    }
    eprintln!(
        "[kite histo] done in {:.3}s (bins={}, output={})",
        started.elapsed().as_secs_f64(),
        all_bins_len,
        args.output,
    );
    Ok(())
}

fn merge_histograms(mut left: BTreeMap<u64, u64>, right: BTreeMap<u64, u64>) -> BTreeMap<u64, u64> {
    for (count, freq) in right {
        *left.entry(count).or_insert(0) += freq;
    }
    left
}
