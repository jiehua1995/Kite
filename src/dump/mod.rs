pub mod decoder;
pub mod loser_tree;
pub mod merge;
pub mod reader;
pub mod writer;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;

use crate::cli::DumpArgs;
use crate::dump::merge::stream_partition_slabs;
use crate::dump::reader::KiteReader;
use crate::dump::writer::{queue_slots_for, SlabPool, DUMP_QUEUE_BYTES, DUMP_SLAB_BYTES};
use crate::stats::DumpStats;

fn spawn_dump_progress(
    enabled: bool,
    stats: Arc<DumpStats>,
    total_partitions: usize,
    total_rows: u64,
    total_chunks: u64,
    worker_threads: usize,
    started_at: Instant,
) -> Option<(Arc<AtomicBool>, thread::JoinHandle<()>)> {
    if !enabled {
        return None;
    }
    let running = Arc::new(AtomicBool::new(true));
    let running_flag = Arc::clone(&running);
    Some((
        running,
        thread::spawn(move || {
            let initial_total = (total_partitions as u64).max(1);
            let bar = ProgressBar::new(initial_total);
            let style = ProgressStyle::with_template(
                "{spinner:.green} {msg}\n[{bar:40.cyan/blue}] {percent:>3}% | {pos}/{len} partitions | eta {eta_precise}",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar());
            bar.set_style(style.progress_chars("=>-"));
            bar.enable_steady_tick(Duration::from_millis(120));
            bar.set_message(format!(
                "dump workers={} writers=1 | chunks {} | output_rows 0 | input_rows {} | written 0B | elapsed 00:00",
                worker_threads,
                total_chunks,
                fmt_num_compact(total_rows),
            ));
            while running_flag.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(250));
                if !running_flag.load(Ordering::Relaxed) {
                    break;
                }
                let wall = started_at.elapsed().as_secs_f64();
                let done = stats.partitions_processed.load(Ordering::Relaxed);
                let rows = stats.total_rows.load(Ordering::Relaxed);
                let written = stats.bytes_written.load(Ordering::Relaxed);
                let total_units = (total_partitions as u64).max(done).max(1);
                bar.set_length(total_units);
                bar.set_position(done.min(total_units));
                bar.set_message(format!(
                    "dump workers={} writers=1 | chunks {} | output_rows {} | input_rows {} | written {} | elapsed {}",
                    worker_threads,
                    total_chunks,
                    fmt_num_compact(rows),
                    fmt_num_compact(total_rows),
                    fmt_bytes_compact(written),
                    fmt_duration_compact(Duration::from_secs_f64(wall)),
                ));
            }
            bar.finish_and_clear();
        }),
    ))
}

fn fmt_num_compact(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn fmt_bytes_compact(value: u64) -> String {
    if value >= 1024 * 1024 * 1024 {
        format!("{:.2}GB", value as f64 / 1024.0 / 1024.0 / 1024.0)
    } else if value >= 1024 * 1024 {
        format!("{:.2}MB", value as f64 / 1024.0 / 1024.0)
    } else if value >= 1024 {
        format!("{:.2}KB", value as f64 / 1024.0)
    } else {
        format!("{}B", value)
    }
}

fn fmt_duration_compact(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}", minutes, seconds)
    }
}

pub fn run_dump(args: &DumpArgs) -> anyhow::Result<()> {
    let n_threads = if args.threads == 0 {
        num_cpus::get()
    } else {
        args.threads
    };
    let collect_timing = args.test_mode;

    let wall_start = Instant::now();
    let stats = Arc::new(DumpStats::new(collect_timing));
    let collect_timing = stats.timing_enabled();
    let reader = Arc::new(KiteReader::open(&args.input)?);
    let slab_bytes = DUMP_SLAB_BYTES;

    let k = reader.header.k;
    let n_partitions = reader.header.partitions as usize;
    let total_chunks = reader.total_chunks;
    let entry_size = reader.header.entry_size();
    let total_rows: u64 = reader
        .chunk_entries
        .iter()
        .map(|chunk| chunk.row_count)
        .sum();
    let min_count = reader.header.min_count;
    let max_count = reader.header.max_count;

    let chunk_counts: Vec<u64> = reader
        .partition_entries
        .iter()
        .map(|p| p.chunk_count)
        .collect();
    let hottest = chunk_counts.iter().copied().max().unwrap_or(0);
    let coldest = chunk_counts.iter().copied().min().unwrap_or(0);
    let mut chunk_sizes: Vec<u64> = reader
        .chunk_entries
        .iter()
        .map(|c| c.encoded_bytes)
        .collect();
    chunk_sizes.sort_unstable();
    let p99_chunk_bytes = if chunk_sizes.is_empty() {
        0
    } else {
        let idx = ((chunk_sizes.len() as f64) * 0.99).floor() as usize;
        chunk_sizes[idx.min(chunk_sizes.len() - 1)]
    };
    stats.set_partition_distribution(hottest, coldest, p99_chunk_bytes);

    let worker_count = n_threads.clamp(1, n_partitions.max(1));
    let channel_depth = worker_count.saturating_mul(2).max(4);
    eprintln!("[kite dump] opening: {}", args.input);
    eprintln!("[kite dump] output:  {}", args.output);
    eprintln!(
        "[kite dump] k={}, partitions={}, total_chunks={}",
        k, n_partitions, total_chunks
    );
    eprintln!(
        "[kite dump] header filters min={}, max={}",
        min_count
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        max_count
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
    );
    eprintln!(
        "[kite dump] slab_bytes={}, workers={}, channel_depth={}, mode={}",
        slab_bytes,
        worker_count,
        channel_depth,
        if args.output == "-" {
            "stdout"
        } else {
            "async-writer"
        },
    );
    let progress_handle = spawn_dump_progress(
        args.progress,
        Arc::clone(&stats),
        n_partitions,
        total_rows,
        total_chunks,
        worker_count,
        wall_start,
    );
    let slab_pool = Arc::new(SlabPool::new(
        slab_bytes,
        queue_slots_for(slab_bytes, DUMP_QUEUE_BYTES, worker_count).saturating_add(1),
    ));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count.max(1))
        .build()?;

    if args.output == "-" {
        let stdout = std::io::stdout();
        let mut out = BufWriter::with_capacity(slab_bytes, stdout.lock());
        for partition_id in 0..n_partitions {
            let read_bytes: u64 = reader.partition_chunks[partition_id]
                .iter()
                .map(|c| c.payload_len as u64)
                .sum();
            stats.add_read(read_bytes);
            stream_partition_slabs(
                partition_id,
                &reader,
                k,
                entry_size,
                slab_bytes,
                &slab_pool,
                collect_timing,
                &stats,
                min_count,
                max_count,
                |bytes, rows| {
                    out.write_all(&bytes)?;
                    stats.add_rows(rows);
                    stats.add_written(bytes.len() as u64);
                    Ok(())
                },
            )?;
            stats.inc_partitions();
        }
        out.flush()?;
        let wall = wall_start.elapsed();
        if let Some((running, handle)) = progress_handle {
            running.store(false, Ordering::Relaxed);
            let _ = handle.join();
        }
        if args.test_mode {
            stats.print_report(args, n_threads, total_chunks, n_partitions, wall);
        }
        eprintln!("[kite dump] done in {:.3}s", wall.as_secs_f64());
        return Ok(());
    }

    let output_path = args.output.clone();

    // Parallel dump to file:
    //   - A dedicated writer thread owns the BufWriter and drains a bounded
    //     crossbeam channel.  It runs at full NVMe sequential-write speed.
    //   - Rayon workers process partitions in parallel, format TSV into slab
    //     buffers, and send each slab to the channel.  Workers block only when
    //     the channel is full (back-pressure), freeing them from waiting on I/O.
    //   - After the writer thread writes a slab it returns it to the slab pool.
    // Channel depth: worker_count × 2 (≤ worker_count × 2 × 64 MiB in flight).
    let out_file = File::create(&output_path)?;
    let (slab_tx, slab_rx) = bounded::<Vec<u8>>(channel_depth);
    let slab_pool_writer = Arc::clone(&slab_pool);
    let writer_handle = thread::spawn(move || -> std::io::Result<()> {
        let mut writer = BufWriter::with_capacity(slab_bytes, out_file);
        for slab in slab_rx {
            writer.write_all(&slab)?;
            slab_pool_writer.release(slab);
        }
        writer.flush()
    });

    let stats_work = Arc::clone(&stats);
    let reader_work = Arc::clone(&reader);
    let worker_results = pool.install(|| {
        (0..n_partitions)
            .into_par_iter()
            .map(|partition_id| -> anyhow::Result<()> {
                let read_bytes: u64 = reader_work.partition_chunks[partition_id]
                    .iter()
                    .map(|c| c.payload_len as u64)
                    .sum();
                stats_work.add_read(read_bytes);

                let tx = slab_tx.clone();
                stream_partition_slabs(
                    partition_id,
                    &reader_work,
                    k,
                    entry_size,
                    slab_bytes,
                    &slab_pool,
                    collect_timing,
                    &stats_work,
                    min_count,
                    max_count,
                    |bytes, rows| {
                        let written = bytes.len() as u64;
                        tx.send(bytes).map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "dump writer thread died",
                            )
                        })?;
                        // slab will be returned to pool by the writer thread
                        stats_work.add_rows(rows);
                        stats_work.add_written(written);
                        Ok(())
                    },
                )
                .map_err(anyhow::Error::from)?;

                stats_work.inc_partitions();
                Ok(())
            })
            .collect::<Vec<_>>()
    });

    // Close the send side so the writer thread exits after draining.
    drop(slab_tx);

    for result in worker_results {
        result?;
    }

    writer_handle
        .join()
        .map_err(|_| anyhow::anyhow!("dump writer thread panicked"))?
        .map_err(anyhow::Error::from)?;

    let wall = wall_start.elapsed();
    if let Some((running, handle)) = progress_handle {
        running.store(false, Ordering::Relaxed);
        let _ = handle.join();
    }
    if args.test_mode {
        stats.print_report(args, n_threads, total_chunks, n_partitions, wall);
    }
    eprintln!("[kite dump] done in {:.3}s", wall.as_secs_f64());
    Ok(())
}
