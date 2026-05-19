use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use rayon::{prelude::*, ThreadPoolBuilder};

use crate::cli::MergeArgs;
use crate::count::memory::MemoryBudget;
use crate::count::writer::ConcurrentKiteWriter;
use crate::format::{CODEC_DVLE, CODEC_RAW};
use crate::kiteops::{
    apply_reader_filter_to_rows, ensure_compatible_with_paths, format_count_filter,
    load_partition_counts, merge_vectors_total, open_readers, serialize_partition_chunk,
    PartitionProgress,
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
    let meta = ensure_compatible_with_paths(&readers, &args.inputs)?;
    eprintln!(
        "[kite merge] compatible inputs confirmed: k={} canonical={} partitions={} key_width={}",
        meta.k, meta.canonical, meta.partitions, meta.key_width
    );
    let worker_threads = if args.threads == 0 {
        num_cpus::get().max(1)
    } else {
        args.threads.max(1)
    };
    eprintln!(
        "[kite merge] worker_threads={} (parallel partitions in batches)",
        worker_threads
    );
    let pool = ThreadPoolBuilder::new()
        .num_threads(worker_threads)
        .thread_name(|index| format!("kite-merge-{index}"))
        .build()
        .ok();
    if pool.is_none() && worker_threads > 1 {
        eprintln!(
            "[kite merge] failed to create local worker pool; falling back to serial partition processing"
        );
    }

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
    let batch_size = worker_threads.max(1);
    for batch_start in (0..meta.partitions).step_by(batch_size) {
        let batch_end = (batch_start + batch_size).min(meta.partitions);
        let batch_ids = (batch_start..batch_end).collect::<Vec<_>>();
        let mut batch_chunks = if let Some(pool) = &pool {
            pool.install(|| {
                batch_ids
                    .into_par_iter()
                    .map(|partition_id| build_merge_partition_chunk(&readers, &meta, partition_id))
                    .collect::<anyhow::Result<Vec<_>>>()
            })?
        } else {
            batch_ids
                .into_iter()
                .map(|partition_id| build_merge_partition_chunk(&readers, &meta, partition_id))
                .collect::<anyhow::Result<Vec<_>>>()?
        };
        batch_chunks.sort_by_key(|chunk| chunk.partition_id);
        for chunk in batch_chunks {
            let partition_id = chunk.partition_id;
            if let Some(chunk) = chunk.serialized {
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
    }
    progress.finish();
    let summary = writer.finalize()?;
    let report_path = merge_report_path(&args.output);
    write_merge_report(
        &report_path,
        args,
        &readers,
        &meta,
        output_min,
        output_max,
        worker_threads,
        summary.total_rows,
        summary.total_chunks,
        started.elapsed().as_secs_f64(),
    )?;
    eprintln!(
        "[kite merge] wrote {} rows across {} chunks in {:.3}s (report={})",
        summary.total_rows,
        summary.total_chunks,
        started.elapsed().as_secs_f64(),
        report_path.display()
    );
    if args.test_mode {
        print_simple_test_report(
            "merge",
            &[
                ("inputs", args.inputs.len().to_string()),
                ("output", args.output.clone()),
                ("report", report_path.display().to_string()),
                ("threads", args.threads.to_string()),
                ("worker_threads", worker_threads.to_string()),
                ("partitions", meta.partitions.to_string()),
                ("rows_written", summary.total_rows.to_string()),
                ("chunks_written", summary.total_chunks.to_string()),
            ],
            started.elapsed(),
        );
    }
    Ok(())
}

struct MergePartitionChunk {
    partition_id: usize,
    serialized: Option<crate::kiteops::SerializedPartitionChunk>,
}

fn build_merge_partition_chunk(
    readers: &[std::sync::Arc<crate::dump::reader::KiteReader>],
    meta: &crate::kiteops::CompatibleMetadata,
    partition_id: usize,
) -> anyhow::Result<MergePartitionChunk> {
    let mut vectors = Vec::with_capacity(readers.len());
    for reader in readers {
        let mut rows = load_partition_counts(reader, partition_id)?;
        apply_reader_filter_to_rows(reader, &mut rows);
        vectors.push(rows);
    }
    let merged = merge_vectors_total(&vectors);
    Ok(MergePartitionChunk {
        partition_id,
        serialized: serialize_partition_chunk(&merged, meta.key_width)?,
    })
}

fn merge_report_path(output: &str) -> PathBuf {
    let output = Path::new(output);
    let parent = output.parent().unwrap_or_else(|| Path::new(""));
    let stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("merge");
    parent.join(format!("{stem}.merge_report.md"))
}

fn write_merge_report(
    path: &Path,
    args: &MergeArgs,
    readers: &[Arc<crate::dump::reader::KiteReader>],
    meta: &crate::kiteops::CompatibleMetadata,
    output_min: Option<u32>,
    output_max: Option<u32>,
    worker_threads: usize,
    total_rows: u64,
    total_chunks: u64,
    elapsed_secs: f64,
) -> anyhow::Result<()> {
    let mut out = BufWriter::new(std::fs::File::create(path)?);
    writeln!(out, "# Kite Merge Report")?;
    writeln!(out)?;
    writeln!(out, "## Summary")?;
    writeln!(out)?;
    writeln!(out, "| Field | Value |")?;
    writeln!(out, "|---|---:|")?;
    writeln!(out, "| Inputs | {} |", args.inputs.len())?;
    writeln!(out, "| Output rows | {} |", total_rows)?;
    writeln!(out, "| Output chunks | {} |", total_chunks)?;
    writeln!(out, "| Partitions | {} |", meta.partitions)?;
    writeln!(out, "| Threads requested | {} |", args.threads)?;
    writeln!(out, "| Worker threads used | {} |", worker_threads)?;
    writeln!(out, "| Elapsed seconds | {:.3} |", elapsed_secs)?;
    writeln!(out)?;
    writeln!(out, "## Compatibility")?;
    writeln!(out)?;
    writeln!(out, "| Field | Value |")?;
    writeln!(out, "|---|---:|")?;
    writeln!(out, "| k | {} |", meta.k)?;
    writeln!(out, "| Canonical | {} |", meta.canonical)?;
    writeln!(out, "| Key width | {} |", meta.key_width)?;
    writeln!(out)?;
    writeln!(out, "## Downstream Filter Semantics")?;
    writeln!(out)?;
    writeln!(
        out,
        "Input header min/max filters are applied before merge. The output header retains a min or max marker only when all inputs share the same marker."
    )?;
    writeln!(out)?;
    writeln!(out, "- Output min: {}", format_count_filter(output_min))?;
    writeln!(out, "- Output max: {}", format_count_filter(output_max))?;
    writeln!(out)?;
    writeln!(out, "## Inputs")?;
    writeln!(out)?;
    writeln!(
        out,
        "| # | Path | k | Canonical | Partitions | Key width | Rows | Chunks | Min | Max |"
    )?;
    writeln!(out, "|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|")?;
    for (index, (input, reader)) in args.inputs.iter().zip(readers.iter()).enumerate() {
        writeln!(
            out,
            "| {} | `{}` | {} | {} | {} | {} | {} | {} | {} | {} |",
            index + 1,
            input,
            reader.header.k,
            reader.header.canonical != 0,
            reader.header.partitions,
            reader.header.key_width,
            reader.header.total_rows,
            reader.total_chunks,
            format_count_filter(reader.header.min_count),
            format_count_filter(reader.header.max_count),
        )?;
    }
    writeln!(out)?;
    writeln!(out, "## Output")?;
    writeln!(out)?;
    writeln!(out, "- File: `{}`", args.output)?;
    writeln!(out, "- Report: `{}`", path.display())?;
    out.flush()?;
    Ok(())
}
