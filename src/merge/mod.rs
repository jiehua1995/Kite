use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::cli::MergeArgs;
use crate::count::memory::MemoryBudget;
use crate::count::writer::ConcurrentKiteWriter;
use crate::format::{CODEC_DVLE, CODEC_RAW};
use crate::kiteops::{
    ensure_compatible, load_partition_counts, merge_vectors_total, open_readers,
    serialize_partition_chunk, PartitionProgress,
};
use crate::stats::print_simple_test_report;

const MIB: usize = 1024 * 1024;
const DEFAULT_STAGING_MB: usize = 16;

pub fn run_merge(args: &MergeArgs) -> anyhow::Result<()> {
    let started = Instant::now();
    eprintln!(
        "[kite merge] inputs={}, output={}",
        args.inputs.len(),
        args.output
    );
    let readers = open_readers(&args.inputs)?;
    eprintln!("[kite merge] compatibility check:");
    for (index, (path, reader)) in args.inputs.iter().zip(readers.iter()).enumerate() {
        eprintln!(
            "[kite merge]   input #{:>2}: {} | k={} canonical={} partitions={} key_width={}",
            index + 1,
            path,
            reader.header.k,
            reader.header.canonical != 0,
            reader.header.partitions,
            reader.header.key_width,
        );
    }
    let meta = ensure_compatible(&readers)?;
    eprintln!(
        "[kite merge] compatible inputs confirmed: k={} canonical={} partitions={} key_width={}",
        meta.k, meta.canonical, meta.partitions, meta.key_width
    );

    if let Some(parent) = Path::new(&args.output).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let staging_bytes = DEFAULT_STAGING_MB * MIB;
    let budget = Arc::new(MemoryBudget::new(usize::MAX / 4));
    let mut writer = ConcurrentKiteWriter::create(
        &args.output,
        Arc::clone(&budget),
        meta.k,
        meta.canonical,
        meta.partitions,
        meta.key_width,
        (CODEC_DVLE | CODEC_RAW) as u16,
        staging_bytes,
        ConcurrentKiteWriter::suggest_write_lanes(num_cpus::get()),
    )?;

    let progress = PartitionProgress::new("merge", args.progress, meta.partitions, started);
    for partition_id in 0..meta.partitions {
        let mut vectors = Vec::with_capacity(readers.len());
        for reader in &readers {
            vectors.push(load_partition_counts(reader, partition_id)?);
        }
        let merged = merge_vectors_total(&vectors);
        if let Some(chunk) = serialize_partition_chunk(&merged, meta.key_width)? {
            writer.append_chunk(
                partition_id,
                chunk.codec,
                chunk.row_count,
                chunk.raw_bytes,
                chunk.key_min_lo,
                chunk.key_min_hi,
                chunk.key_max_lo,
                chunk.key_max_hi,
                &chunk.data,
            )?;
        }
        progress.update(partition_id + 1);
    }
    progress.finish();
    let summary = writer.finalize()?;
    eprintln!(
        "[kite merge] wrote {} rows across {} chunks in {:.3}s",
        summary.total_rows,
        summary.total_chunks,
        started.elapsed().as_secs_f64()
    );
    if args.test_mode {
        print_simple_test_report(
            "merge",
            &[
                ("inputs", args.inputs.len().to_string()),
                ("output", args.output.clone()),
                ("partitions", meta.partitions.to_string()),
                ("rows_written", summary.total_rows.to_string()),
                ("chunks_written", summary.total_chunks.to_string()),
            ],
            started.elapsed(),
        );
    }
    Ok(())
}
