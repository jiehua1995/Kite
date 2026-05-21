use std::io::Write;
use std::time::Instant;

use rayon::{prelude::*, ThreadPoolBuilder};

use crate::cli::ProfileArgs;
use crate::kiteops::{
    apply_reader_filter_to_rows, create_output_writer, ensure_compatible_with_paths,
    for_each_union_row, format_reader_filter, infer_sample_labels, key_to_string,
    load_partition_counts, open_readers, print_command_line, PartitionProgress,
};
use crate::stats::print_simple_test_report;

pub fn run_profile(args: &ProfileArgs) -> anyhow::Result<()> {
    print_command_line("profile");
    let started = Instant::now();
    let labels = infer_sample_labels(&args.inputs, args.labels.as_deref().unwrap_or(&[]))?;
    eprintln!(
        "[kite profile] inputs={}, output={}",
        args.inputs.len(),
        args.output
    );
    let readers = open_readers(&args.inputs)?;
    eprintln!(
        "[kite profile] labels={} ({})",
        labels.join(","),
        if args.labels.is_some() {
            "user supplied"
        } else {
            "inferred from file names"
        }
    );
    for (index, ((path, label), reader)) in args
        .inputs
        .iter()
        .zip(labels.iter())
        .zip(readers.iter())
        .enumerate()
    {
        eprintln!(
            "[kite profile] input #{:>2} label={} path={} k={} canonical={} partitions={} key_width={} rows={} chunks={} filters: {}",
            index + 1,
            label,
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
    let meta = ensure_compatible_with_paths(&readers, &args.inputs)?;
    let worker_threads = if args.threads == 0 {
        num_cpus::get().max(1)
    } else {
        args.threads.max(1)
    };
    eprintln!(
        "[kite profile] worker_threads={} (parallel partitions in batches)",
        worker_threads
    );
    let pool = ThreadPoolBuilder::new()
        .num_threads(worker_threads)
        .thread_name(|index| format!("kite-profile-{index}"))
        .build()
        .ok();
    if pool.is_none() && worker_threads > 1 {
        eprintln!(
            "[kite profile] failed to create local worker pool; falling back to serial partition processing"
        );
    }
    let mut out = create_output_writer(&args.output)?;
    let mut rows_written = 0u64;

    write!(out, "kmer")?;
    for label in &labels {
        write!(out, "\t{}", label)?;
    }
    writeln!(out)?;

    let progress = PartitionProgress::new("profile", args.progress, meta.partitions, started);
    let batch_size = worker_threads.max(1);
    for batch_start in (0..meta.partitions).step_by(batch_size) {
        let batch_end = (batch_start + batch_size).min(meta.partitions);
        let batch_ids = (batch_start..batch_end).collect::<Vec<_>>();
        let mut batch_chunks = if let Some(pool) = &pool {
            pool.install(|| {
                batch_ids
                    .into_par_iter()
                    .map(|partition_id| render_partition_rows(&readers, &meta, partition_id))
                    .collect::<anyhow::Result<Vec<_>>>()
            })?
        } else {
            batch_ids
                .into_iter()
                .map(|partition_id| render_partition_rows(&readers, &meta, partition_id))
                .collect::<anyhow::Result<Vec<_>>>()?
        };
        batch_chunks.sort_by_key(|chunk| chunk.partition_id);
        for chunk in batch_chunks {
            out.write_all(&chunk.buffer)?;
            rows_written += chunk.rows_written;
            progress.update(chunk.partition_id + 1);
        }
    }
    progress.finish();
    out.flush()?;
    if args.test_mode {
        print_simple_test_report(
            "profile",
            &[
                ("inputs", args.inputs.len().to_string()),
                ("labels", labels.join(",")),
                ("threads", args.threads.to_string()),
                ("worker_threads", worker_threads.to_string()),
                ("partitions", meta.partitions.to_string()),
                ("rows_written", rows_written.to_string()),
                ("output", args.output.clone()),
            ],
            started.elapsed(),
        );
    }
    eprintln!(
        "[kite profile] done in {:.3}s (rows={}, output={})",
        started.elapsed().as_secs_f64(),
        rows_written,
        args.output,
    );
    Ok(())
}

struct PartitionProfileChunk {
    partition_id: usize,
    buffer: Vec<u8>,
    rows_written: u64,
}

fn render_partition_rows(
    readers: &[std::sync::Arc<crate::dump::reader::KiteReader>],
    meta: &crate::kiteops::CompatibleMetadata,
    partition_id: usize,
) -> anyhow::Result<PartitionProfileChunk> {
    let mut vectors = Vec::with_capacity(readers.len());
    for reader in readers {
        let mut rows = load_partition_counts(reader, partition_id)?;
        apply_reader_filter_to_rows(reader, &mut rows);
        vectors.push(rows);
    }

    let mut buffer = Vec::new();
    let mut rows_written = 0u64;
    for_each_union_row(&vectors, |key, row| {
        write!(buffer, "{}", key_to_string(key, meta))?;
        for count in row {
            write!(buffer, "\t{}", count)?;
        }
        writeln!(buffer)?;
        rows_written += 1;
        Ok(())
    })?;

    Ok(PartitionProfileChunk {
        partition_id,
        buffer,
        rows_written,
    })
}
