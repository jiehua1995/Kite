use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::cli::MergeArgs;
use crate::count::memory::MemoryBudget;
use crate::count::writer::ConcurrentKiteWriter;
use crate::format::{CODEC_DVLE, CODEC_RAW};
use crate::kiteops::{
    apply_reader_filter_to_rows, ensure_compatible, format_count_filter, load_partition_counts,
    merge_vectors_total, open_readers, serialize_partition_chunk, PartitionProgress,
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
            "[kite merge]   input #{:>2}: {} | k={} canonical={} partitions={} key_width={} min={} max={}",
            index + 1,
            path,
            reader.header.k,
            reader.header.canonical != 0,
            reader.header.partitions,
            reader.header.key_width,
            format_count_filter(reader.header.min_count),
            format_count_filter(reader.header.max_count),
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
    let output_min = readers
        .first()
        .and_then(|reader| {
            readers
                .iter()
                .all(|r| r.header.min_count == reader.header.min_count)
                .then_some(reader.header.min_count)
        })
        .flatten();
    let output_max = readers
        .first()
        .and_then(|reader| {
            readers
                .iter()
                .all(|r| r.header.max_count == reader.header.max_count)
                .then_some(reader.header.max_count)
        })
        .flatten();
    eprintln!(
        "[kite merge] downstream filters applied before merge; output header filters min={}, max={}",
        format_count_filter(output_min),
        format_count_filter(output_max)
    );
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
        output_min,
        output_max,
    )?;

    let progress = PartitionProgress::new("merge", args.progress, meta.partitions, started);
    for partition_id in 0..meta.partitions {
        let mut vectors = Vec::with_capacity(readers.len());
        for reader in &readers {
            let mut rows = load_partition_counts(reader, partition_id)?;
            apply_reader_filter_to_rows(reader, &mut rows);
            vectors.push(rows);
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
