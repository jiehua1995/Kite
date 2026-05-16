use std::io::Write;
use std::time::Instant;

use crate::cli::ProfileArgs;
use crate::kiteops::{
    create_output_writer, ensure_compatible, for_each_union_row, infer_sample_labels,
    key_to_string, load_partition_counts, open_readers, PartitionProgress,
};
use crate::stats::print_simple_test_report;

pub fn run_profile(args: &ProfileArgs) -> anyhow::Result<()> {
    let started = Instant::now();
    let labels = infer_sample_labels(&args.inputs, args.labels.as_deref().unwrap_or(&[]))?;
    eprintln!(
        "[kite profile] inputs={}, output={}",
        args.inputs.len(),
        args.output
    );
    let readers = open_readers(&args.inputs)?;
    let meta = ensure_compatible(&readers)?;
    let mut out = create_output_writer(&args.output)?;
    let mut rows_written = 0u64;

    write!(out, "kmer")?;
    for label in &labels {
        write!(out, "\t{}", label)?;
    }
    writeln!(out)?;

    let progress = PartitionProgress::new("profile", args.progress, meta.partitions, started);
    for partition_id in 0..meta.partitions {
        let mut vectors = Vec::with_capacity(readers.len());
        for reader in &readers {
            vectors.push(load_partition_counts(reader, partition_id)?);
        }
        for_each_union_row(&vectors, |key, row| {
            write!(out, "{}", key_to_string(key, &meta))?;
            for count in row {
                write!(out, "\t{}", count)?;
            }
            writeln!(out)?;
            rows_written += 1;
            Ok(())
        })?;
        progress.update(partition_id + 1);
    }
    progress.finish();
    out.flush()?;
    if args.test_mode {
        print_simple_test_report(
            "profile",
            &[
                ("inputs", args.inputs.len().to_string()),
                ("labels", labels.join(",")),
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
