pub mod decoder;
pub mod loser_tree;
pub mod merge;
pub mod reader;
pub mod writer;

use std::fs;
use std::fs::File;
use std::io::{BufWriter, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
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
use crate::kiteops::print_command_line;
use crate::stats::DumpStats;

const MIB: usize = 1024 * 1024;
const DUMP_MIN_QUEUE_SLOTS: usize = 2;
const DUMP_MAX_QUEUE_SLOTS: usize = 4;
const DUMP_PREALLOC_INITIAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const DUMP_PREALLOC_GROW_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const DUMP_PREALLOC_HEADROOM_BYTES: u64 = 1 * 1024 * 1024 * 1024;
const DUMP_SMALL_DATASET_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const DUMP_LARGE_DATASET_BYTES: u64 = 64 * 1024 * 1024 * 1024;

fn estimate_dump_output_bytes(total_rows: u64, k: u8) -> u64 {
    let per_row = u64::from(k) + 2 + 4;
    total_rows.saturating_mul(per_row).saturating_mul(110) / 100
}

fn auto_dump_channel_depth(estimated_output_bytes: u64, worker_count: usize) -> usize {
    let slots = if estimated_output_bytes >= DUMP_LARGE_DATASET_BYTES {
        2
    } else if estimated_output_bytes >= DUMP_SMALL_DATASET_BYTES {
        3
    } else {
        4
    };
    slots
        .clamp(DUMP_MIN_QUEUE_SLOTS, DUMP_MAX_QUEUE_SLOTS)
        .min(worker_count.max(1))
        .max(1)
}

fn should_manage_file_space(file: &File) -> bool {
    file.metadata()
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

#[cfg(unix)]
fn try_preallocate_output(file: &File, bytes: u64) -> std::io::Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let fd = file.as_raw_fd();
    let rc = unsafe { libc::posix_fallocate(fd, 0, bytes as libc::off_t) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(rc))
    }
}

#[cfg(not(unix))]
fn try_preallocate_output(_file: &File, _bytes: u64) -> std::io::Result<()> {
    Ok(())
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

fn spawn_dump_progress(
    enabled: bool,
    stats: Arc<DumpStats>,
    total_partitions: usize,
    total_rows: u64,
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
            bar.set_position(0);
            bar.set_message(format!(
                "dump output 0/{} | written 0B | elapsed 00:00",
                fmt_num_compact(total_rows),
            ));
            bar.enable_steady_tick(Duration::from_millis(120));
            while running_flag.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(250));
                if !running_flag.load(Ordering::Relaxed) {
                    break;
                }
                let wall = started_at.elapsed();
                let done = stats.partitions_processed.load(Ordering::Relaxed);
                let rows = stats.total_rows.load(Ordering::Relaxed);
                let written = stats.bytes_written.load(Ordering::Relaxed);
                let total_units = (total_partitions as u64).max(done).max(1);
                bar.set_length(total_units);
                bar.set_position(done.min(total_units));
                bar.set_message(format!(
                    "dump output {}/{} | written {} | elapsed {}",
                    fmt_num_compact(rows),
                    fmt_num_compact(total_rows),
                    fmt_bytes_compact(written),
                    fmt_duration_compact(wall),
                ));
            }
            let done = stats.partitions_processed.load(Ordering::Relaxed);
            let rows = stats.total_rows.load(Ordering::Relaxed);
            let written = stats.bytes_written.load(Ordering::Relaxed);
            let total_units = (total_partitions as u64).max(done).max(1);
            bar.set_length(total_units);
            bar.set_position(done.min(total_units));
            bar.set_message(format!(
                "dump done | output {}/{} | written {} | elapsed {}",
                fmt_num_compact(rows),
                fmt_num_compact(total_rows),
                fmt_bytes_compact(written),
                fmt_duration_compact(started_at.elapsed()),
            ));
            bar.finish_and_clear();
        }),
    ))
}

pub fn run_dump(args: &DumpArgs) -> anyhow::Result<()> {
    print_command_line("dump");
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
    let estimated_output_bytes = estimate_dump_output_bytes(total_rows, k);

    let slab_mb = args.buffer.unwrap_or(DUMP_SLAB_BYTES / MIB).clamp(1, 1024);
    let slab_bytes = slab_mb * MIB;
    let requested_threads = n_threads.clamp(1, n_partitions.max(1));
    let worker_count = requested_threads;
    let max_slots_by_budget = (DUMP_QUEUE_BYTES / slab_bytes.max(1)).max(1);
    let requested_channel_depth = auto_dump_channel_depth(estimated_output_bytes, worker_count);
    let channel_depth = requested_channel_depth.min(max_slots_by_budget).max(1);
    let queue_bytes = (channel_depth as u64).saturating_mul(slab_bytes as u64);
    let initial_budget_bytes = (channel_depth as u64).saturating_mul(slab_bytes as u64);
    stats.set_final_budget_bytes(initial_budget_bytes);
    stats.observe_inflight(0, 0);
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
        "[kite dump] slab_bytes={}, workers={}, channel_depth={}, queue_budget={}, mode={}",
        slab_bytes,
        worker_count,
        channel_depth,
        fmt_bytes_compact(queue_bytes),
        if args.output == "-" {
            "stdout"
        } else {
            "async-writer"
        },
    );
    if args.buffer.is_some() {
        eprintln!(
            "[kite dump][auto] Using user-defined dump buffer={}MB",
            slab_mb
        );
    } else {
        eprintln!("[kite dump][auto] Auto dump buffer={}MB", slab_mb);
    }
    eprintln!("[kite dump][auto] Dump workers={}", worker_count);
    eprintln!(
        "[kite dump][auto] Fixed in-flight budget={} slabs (~{}) from estimated output {}",
        channel_depth,
        fmt_bytes_compact(initial_budget_bytes),
        fmt_bytes_compact(estimated_output_bytes),
    );
    let slab_pool = Arc::new(SlabPool::new(
        slab_bytes,
        queue_slots_for(slab_bytes, DUMP_QUEUE_BYTES, worker_count)
            .min(channel_depth.saturating_add(1))
            .max(1),
    ));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count.max(1))
        .build()?;

    if args.output == "-" {
        let progress_handle = spawn_dump_progress(
            args.progress,
            Arc::clone(&stats),
            n_partitions,
            total_rows,
            wall_start,
        );
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
                    let t_write = collect_timing.then(Instant::now);
                    out.write_all(&bytes)?;
                    if let Some(t_write) = t_write {
                        stats.add_writer_io_ns(t_write.elapsed().as_nanos() as u64);
                    }
                    stats.add_rows(rows);
                    stats.add_written(bytes.len() as u64);
                    Ok(())
                },
            )?;
            stats.inc_partitions();
        }
        let t_flush = collect_timing.then(Instant::now);
        out.flush()?;
        if let Some(t_flush) = t_flush {
            stats.add_writer_io_ns(t_flush.elapsed().as_nanos() as u64);
        }
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
    if let Some(parent) = std::path::Path::new(&output_path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    // Parallel dump to file:
    //   - A dedicated writer thread owns the BufWriter and drains a bounded
    //     crossbeam channel.
    //   - Rayon workers process partitions in parallel, format TSV into slab
    //     buffers, and send each slab to the writer.
    //   - Channel depth is intentionally kept small to smooth kernel writeback.
    let out_file = File::create(&output_path)?;
    let manage_file_space = should_manage_file_space(&out_file);
    let initial_prealloc_bytes = if manage_file_space {
        estimated_output_bytes.min(DUMP_PREALLOC_INITIAL_BYTES)
    } else {
        0
    };
    if manage_file_space {
        match try_preallocate_output(&out_file, initial_prealloc_bytes) {
            Ok(()) => eprintln!(
                "[kite dump][auto] Preallocated output file space={} initially (estimate {})",
                fmt_bytes_compact(initial_prealloc_bytes),
                fmt_bytes_compact(estimated_output_bytes),
            ),
            Err(err) => eprintln!("[kite dump][warn] Output preallocation skipped: {}", err),
        }
    } else {
        eprintln!(
            "[kite dump][auto] Output is not a regular file; preallocation/truncation disabled"
        );
    }
    let progress_handle = spawn_dump_progress(
        args.progress,
        Arc::clone(&stats),
        n_partitions,
        total_rows,
        wall_start,
    );
    let (slab_tx, slab_rx) = bounded::<Vec<u8>>(channel_depth);
    let slab_pool_writer = Arc::clone(&slab_pool);
    let stats_writer = Arc::clone(&stats);
    let writer_handle = thread::spawn(move || -> std::io::Result<()> {
        let mut writer = BufWriter::with_capacity(slab_bytes, out_file);
        let mut total_written = 0u64;
        let mut reserved_bytes = initial_prealloc_bytes;
        for slab in slab_rx {
            let written = slab.len() as u64;
            let current_bytes = stats_writer
                .current_inflight_bytes
                .fetch_sub(written, Ordering::Relaxed)
                .saturating_sub(written);
            let current_slabs = stats_writer
                .current_inflight_slabs
                .fetch_sub(1, Ordering::Relaxed)
                .saturating_sub(1);
            stats_writer.observe_inflight(current_bytes, current_slabs);
            if manage_file_space
                && reserved_bytes < estimated_output_bytes
                && total_written
                    .saturating_add(written)
                    .saturating_add(DUMP_PREALLOC_HEADROOM_BYTES)
                    >= reserved_bytes
            {
                let grow_by =
                    (estimated_output_bytes - reserved_bytes).min(DUMP_PREALLOC_GROW_BYTES);
                if grow_by > 0 {
                    let next_reserved = reserved_bytes.saturating_add(grow_by);
                    match try_preallocate_output(writer.get_ref(), next_reserved) {
                        Ok(()) => {
                            reserved_bytes = next_reserved;
                        }
                        Err(err) => eprintln!(
                            "[kite dump][warn] Output preallocation growth skipped at {}: {}",
                            fmt_bytes_compact(next_reserved),
                            err
                        ),
                    }
                }
            }
            let t_write = collect_timing.then(Instant::now);
            writer.write_all(&slab)?;
            let write_ns = t_write
                .map(|t_write| t_write.elapsed().as_nanos() as u64)
                .unwrap_or(0);
            if write_ns > 0 {
                stats_writer.add_writer_io_ns(write_ns);
            }
            stats_writer.add_written(written);
            stats_writer.inc_writer_write_calls();
            total_written = total_written.saturating_add(written);
            slab_pool_writer.release(slab);
        }
        writer.flush()?;
        if manage_file_space {
            writer.get_ref().set_len(total_written)
        } else {
            Ok(())
        }
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
                        let t_wait = collect_timing.then(Instant::now);
                        let bytes_len = bytes.len() as u64;
                        tx.send(bytes).map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "dump writer thread died",
                            )
                        })?;
                        if let Some(t_wait) = t_wait {
                            stats_work.add_writer_queue_wait_ns(t_wait.elapsed().as_nanos() as u64);
                        }
                        let current_bytes = stats_work
                            .current_inflight_bytes
                            .fetch_add(bytes_len, Ordering::Relaxed)
                            .saturating_add(bytes_len);
                        let current_slabs = stats_work
                            .current_inflight_slabs
                            .fetch_add(1, Ordering::Relaxed)
                            .saturating_add(1);
                        stats_work.observe_inflight(current_bytes, current_slabs);
                        stats_work.add_rows(rows);
                        Ok(())
                    },
                )
                .map_err(anyhow::Error::from)?;

                stats_work.inc_partitions();
                Ok(())
            })
            .collect::<Vec<_>>()
    });

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

#[cfg(test)]
mod tests {
    use super::{auto_dump_channel_depth, estimate_dump_output_bytes};

    #[test]
    fn auto_dump_queue_stays_small() {
        assert_eq!(auto_dump_channel_depth(4_u64 * 1024 * 1024 * 1024, 64), 4);
        assert_eq!(auto_dump_channel_depth(16_u64 * 1024 * 1024 * 1024, 64), 3);
        assert_eq!(auto_dump_channel_depth(128_u64 * 1024 * 1024 * 1024, 64), 2);
        assert_eq!(auto_dump_channel_depth(128_u64 * 1024 * 1024 * 1024, 2), 2);
    }

    #[test]
    fn dump_output_estimate_has_slack() {
        let estimate = estimate_dump_output_bytes(1_000, 31);
        assert_eq!(estimate, 40_700);
    }
}
