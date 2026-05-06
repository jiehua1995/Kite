pub mod bgzf;
pub mod decompress;
pub mod kmer;
pub mod memory;
pub mod parser;
pub mod reader;
pub mod router;
pub mod sorter;
pub mod worker;
pub mod writer;

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;

use crate::cli::CountArgs;
use crate::count::decompress::{GzipChunkedReader, PlainChunkedReader};
use crate::count::kmer::{KmerU256, KmerWord};
use crate::count::memory::MemoryBudget;
use crate::count::parser::parse_sequences;
use crate::count::reader::{CompressionType, detect_compression, open_input};
use crate::count::worker::CountWorker;
use crate::count::writer::ConcurrentKiteWriter;
use crate::format::header::{KEY_WIDTH_U64, KEY_WIDTH_U128, KEY_WIDTH_U256};
use crate::format::{CODEC_DVLE, CODEC_RAW, CODEC_ZSTD};
use crate::runtime::{apply_process_memory_limit, resolve_memory_limit_bytes};
use crate::stats::CountStats;

const GIB: usize = 1024 * 1024 * 1024;
const MIB: usize = 1024 * 1024;
const AUTO_MIN_PARTITIONS: usize = 256;
const AUTO_MAX_PARTITIONS: usize = 2048;
const WORKER_SCRATCH_BYTES: usize = 48 * MIB;
const THREAD_STACK_RESERVE_BYTES: usize = 8 * MIB;
const SAMPLE_CHUNK_BYTES: usize = 8 * MIB;
const SAMPLE_MAX_BYTES: usize = 32 * MIB;
const SAMPLE_MAX_READS: u64 = 200_000;

#[derive(Debug, Clone, Copy)]
struct InputSample {
    avg_read_len: f64,
    sampled_reads: u64,
    sampled_bytes: usize,
}

#[derive(Debug, Clone)]
struct HeuristicPlan {
    encoding_factor: usize,
    input_bytes: u64,
    avg_read_len: f64,
    sampled_reads: u64,
    sampled_bytes: usize,
    partitions: usize,
    staging_bytes: usize,
    decomp_threads: usize,
    worker_threads: usize,
    worker_budget_bytes: usize,
    local_flush_bytes: usize,
    tracked_budget_bytes: usize,
    decomp_budget_bytes: usize,
    queue_depth: usize,
    hard_memory_limit_bytes: usize,
    untracked_reserve_bytes: usize,
    reserved_bytes: usize,
    reasons: Vec<String>,
}

/// Return log2(partitions) — the number of bucket index bits for Fibonacci hash routing.
pub fn compute_bucket_bits(partitions: usize) -> u32 {
    partitions.trailing_zeros()
}

fn spawn_count_progress(
    enabled: bool,
    stats: Arc<CountStats>,
    budget: Arc<MemoryBudget>,
    hard_memory_limit_bytes: usize,
    input_bytes: u64,
    estimated_total_reads: u64,
    estimated_total_kmers: u64,
    k: u8,
    total_threads: usize,
    decomp_threads: usize,
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
            let is_tty = std::io::stderr().is_terminal();
            let mut rendered = false;
            let mut last_fallback = Instant::now() - Duration::from_secs(60);
            let total_header = format!(
                "[kite count] total input={} est_reads={} est_kmers={} k={} threads={}/{}/{} mem_limit={:.2}GB",
                fmt_bytes_compact(input_bytes),
                fmt_num_compact(estimated_total_reads),
                fmt_num_compact(estimated_total_kmers),
                k,
                total_threads,
                decomp_threads,
                worker_threads,
                hard_memory_limit_bytes as f64 / GIB as f64,
            );
            while running_flag.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(250));
                if !running_flag.load(Ordering::Relaxed) {
                    break;
                }
                let wall = started_at.elapsed().as_secs_f64();
                let reads = stats.reads_processed.load(Ordering::Relaxed);
                let kmers = stats.valid_kmers_generated.load(Ordering::Relaxed);
                let decomp = stats.decomp_bytes_produced.load(Ordering::Relaxed);
                let rss = budget.current_bytes() as f64 / GIB as f64;
                let progress = if estimated_total_reads > 0 {
                    (reads as f64 / estimated_total_reads as f64).clamp(0.0, 0.999)
                } else {
                    0.0
                };
                let eta = if progress > 0.001 {
                    Some(Duration::from_secs_f64(wall * (1.0 - progress) / progress))
                } else {
                    None
                };
                let detail = format!(
                    "[kite count] done reads={}/{} ({:5.1}%) kmers={} decomp={} elapsed={} eta={} tracked_mem={:.2}GB",
                    fmt_num_compact(reads),
                    fmt_num_compact(estimated_total_reads),
                    progress * 100.0,
                    fmt_num_compact(kmers),
                    fmt_bytes_compact(decomp),
                    fmt_duration_compact(Duration::from_secs_f64(wall)),
                    fmt_eta_compact(eta),
                    rss,
                );

                if is_tty {
                    let mut stderr = std::io::stderr().lock();
                    if rendered {
                        let _ = write!(stderr, "\x1b[2F");
                    }
                    let _ = write!(stderr, "\x1b[2K{}\n\x1b[2K{}\n", total_header, detail);
                    let _ = stderr.flush();
                    rendered = true;
                } else if last_fallback.elapsed() >= Duration::from_secs(15) {
                    eprintln!("{} | {}", total_header, detail);
                    last_fallback = Instant::now();
                }
            }
        }),
    ))
}

fn clear_progress_lines(line_count: usize) {
    if !std::io::stderr().is_terminal() || line_count == 0 {
        return;
    }
    let mut stderr = std::io::stderr().lock();
    let _ = write!(stderr, "\x1b[{}F", line_count);
    for index in 0..line_count {
        let _ = write!(stderr, "\x1b[2K");
        if index + 1 < line_count {
            let _ = write!(stderr, "\n");
        }
    }
    let _ = write!(stderr, "\r");
    let _ = stderr.flush();
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

fn fmt_eta_compact(eta: Option<Duration>) -> String {
    eta.map(fmt_duration_compact)
        .unwrap_or_else(|| "--:--".to_string())
}

pub fn run_count(args: &CountArgs) -> anyhow::Result<()> {
    if args.k < 1 || args.k > 128 {
        anyhow::bail!("k must be between 1 and 128 (got {})", args.k);
    }
    if args.partitions != 0 && !args.partitions.is_power_of_two() {
        anyhow::bail!(
            "--partitions must be a power of 2 (got {})",
            args.partitions
        );
    }

    if args.k <= 31 {
        run_count_typed::<u64>(args)
    } else if args.k <= 64 {
        run_count_typed::<u128>(args)
    } else {
        run_count_typed::<KmerU256>(args)
    }
}

fn run_count_typed<K: KmerWord>(args: &CountArgs) -> anyhow::Result<()> {
    if !args.output.ends_with(".kite") {
        anyhow::bail!("count output must use .kite suffix (got '{}')", args.output);
    }

    let key_width: u16 = match K::entry_size() {
        36 => KEY_WIDTH_U256,
        20 => KEY_WIDTH_U128,
        _ => KEY_WIDTH_U64,
    };
    let codec_mask: u16 = (CODEC_RAW | CODEC_DVLE | CODEC_ZSTD) as u16;
    let n_threads = if args.threads == 0 {
        num_cpus::get()
    } else {
        args.threads
    };

    if let Some(parent) = Path::new(&args.output).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let hard_memory_limit_bytes = resolve_memory_limit_bytes(args.memory.as_deref())?;
    let compression = detect_compression(Path::new(&args.input))?;
    let chunk_size_bytes = (args.chunk_kmers * 2).max(8 * MIB).min(64 * MIB);
    let _applied_memory_limit = apply_process_memory_limit(hard_memory_limit_bytes)?;
    let plan = derive_heuristic_plan(
        args,
        compression,
        key_width,
        n_threads,
        chunk_size_bytes,
        hard_memory_limit_bytes,
    )?;

    let budget = Arc::new(MemoryBudget::new(plan.tracked_budget_bytes));
    let stats = Arc::new(CountStats::new(plan.partitions, args.test_mode));
    let wall_start = Instant::now();
    let estimated_total_reads = if plan.sampled_bytes > 0 && plan.sampled_reads > 0 {
        ((plan.input_bytes as u128)
            .saturating_mul(plan.sampled_reads as u128)
            .checked_div(plan.sampled_bytes as u128)
            .unwrap_or(0)) as u64
    } else {
        0
    };
    let kmers_per_read = plan.avg_read_len.max(args.k as f64) - args.k as f64 + 1.0;
    let estimated_total_kmers = (estimated_total_reads as f64 * kmers_per_read.max(0.0)) as u64;
    let progress_handle = spawn_count_progress(
        args.progress,
        Arc::clone(&stats),
        Arc::clone(&budget),
        plan.hard_memory_limit_bytes,
        plan.input_bytes,
        estimated_total_reads,
        estimated_total_kmers,
        args.k,
        n_threads,
        plan.decomp_threads,
        plan.worker_threads,
        wall_start,
    );

    let (chunk_tx, chunk_rx) = bounded::<Vec<u8>>(plan.queue_depth);
    let writer_lanes = ConcurrentKiteWriter::suggest_write_lanes(plan.worker_threads);
    let bucket_shift = compute_bucket_bits(plan.partitions);

    if args.test_mode {
        eprintln!("[kite count] input:       {}", args.input);
        eprintln!("[kite count] compression: {}", compression);
        eprintln!(
            "[kite count] k={}, canonical={}, partitions={}",
            args.k, args.canonical, plan.partitions
        );
        eprintln!(
            "[kite count] mode={}, codec={}",
            match args.mode {
                crate::cli::CountMode::Fast => "fast",
                crate::cli::CountMode::Balanced => "balanced",
                crate::cli::CountMode::Compact => "compact",
            },
            if K::entry_size() == 36 {
                "raw-u256"
            } else {
                "dvle-only"
            }
        );
        eprintln!(
            "[kite count] threads total={}, decomp={}, count_workers={}",
            n_threads, plan.decomp_threads, plan.worker_threads
        );
        eprintln!(
            "[kite count] memory_limit={:.1}GB (hard limit, tracked {:.1}GB, reserve {:.1}GB)",
            plan.hard_memory_limit_bytes as f64 / GIB as f64,
            budget.limit_bytes() as f64 / GIB as f64,
            plan.untracked_reserve_bytes as f64 / GIB as f64,
        );
        eprintln!(
            "[kite count][auto] input={:.2}GB, read_len={:.1}bp from {} reads, E={}",
            plan.input_bytes as f64 / GIB as f64,
            plan.avg_read_len,
            plan.sampled_reads,
            plan.encoding_factor,
        );
        eprintln!(
            "[kite count][auto] P={}, S={}MB, W={:.2}GB/worker, queue_depth={}, decomp_budget={:.1}MB",
            plan.partitions,
            plan.staging_bytes / MIB,
            plan.worker_budget_bytes as f64 / GIB as f64,
            plan.queue_depth,
            plan.decomp_budget_bytes as f64 / MIB as f64,
        );
        eprintln!(
            "[kite count][auto] sampled_bytes={:.1}MB, reserved={:.1}MB, writer_lanes={}",
            plan.sampled_bytes as f64 / MIB as f64,
            plan.reserved_bytes as f64 / MIB as f64,
            writer_lanes,
        );
        for reason in &plan.reasons {
            eprintln!("[kite count][auto] {}", reason);
        }
    }

    let input_path = args.input.clone();
    let output_path = args.output.clone();
    let collect_timing = args.test_mode;
    let k = args.k;
    let canonical = args.canonical;
    let compression_type = compression;

    let decomp_threads = plan.decomp_threads;
    let worker_threads = plan.worker_threads;
    let partitions = plan.partitions;
    let staging_bytes = plan.staging_bytes;
    let local_flush_bytes = plan.local_flush_bytes;
    let zstd_level = args.mode.zstd_level();
    let show_progress = args.progress;

    let kite_writer = Arc::new(ConcurrentKiteWriter::create(
        &output_path,
        Arc::clone(&budget),
        k,
        canonical,
        partitions,
        key_width,
        codec_mask,
        staging_bytes,
        writer_lanes,
    )?);

    let stats_reader = Arc::clone(&stats);
    let chunk_tx_reader = chunk_tx.clone();
    let reader_thread = thread::spawn(move || match compression_type {
        CompressionType::Bgzf => {
            let t = collect_timing.then(Instant::now);
            match bgzf::decompress_bgzf_parallel(
                &input_path,
                decomp_threads,
                chunk_size_bytes,
                &chunk_tx_reader,
            ) {
                Ok((comp_bytes, decomp_bytes)) => {
                    stats_reader.add_comp_bytes(comp_bytes);
                    if let Some(t) = t {
                        stats_reader.add_decomp_ns(t.elapsed().as_nanos() as u64);
                    }
                    let _ = decomp_bytes;
                }
                Err(e) => eprintln!("[BGZF decomp error] {}", e),
            }
        }
        CompressionType::Gzip => {
            if let Ok(meta) = std::fs::metadata(&input_path) {
                stats_reader.add_comp_bytes(meta.len());
            }
            let file = open_input(Path::new(&input_path)).expect("open input");
            let buffered = std::io::BufReader::with_capacity(2 * MIB, file);
            let mut reader = GzipChunkedReader::new(buffered, chunk_size_bytes);
            loop {
                let t = collect_timing.then(Instant::now);
                match reader.next_chunk() {
                    Ok(Some(chunk)) => {
                        if let Some(t) = t {
                            stats_reader.add_decomp_ns(t.elapsed().as_nanos() as u64);
                        }
                        if chunk_tx_reader.send(chunk).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("decompress error: {}", e);
                        break;
                    }
                }
            }
        }
        CompressionType::Plain => {
            if let Ok(meta) = std::fs::metadata(&input_path) {
                stats_reader.add_comp_bytes(meta.len());
            }
            let file = open_input(Path::new(&input_path)).expect("open input");
            let buffered = std::io::BufReader::with_capacity(2 * MIB, file);
            let mut reader = PlainChunkedReader::new(buffered, chunk_size_bytes);
            loop {
                match reader.next_chunk() {
                    Ok(Some(chunk)) => {
                        if chunk_tx_reader.send(chunk).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("read error: {}", e);
                        break;
                    }
                }
            }
        }
    });
    drop(chunk_tx);

    let mut worker_handles = Vec::new();
    for _ in 0..worker_threads {
        let chunk_rx_w = chunk_rx.clone();
        let budget_w = Arc::clone(&budget);
        let stats_w = Arc::clone(&stats);
        let kite_writer_w = Arc::clone(&kite_writer);

        let handle = thread::spawn(move || -> std::io::Result<()> {
            let mut worker = CountWorker::<K>::new(
                collect_timing,
                show_progress,
                k,
                canonical,
                partitions,
                bucket_shift,
                budget_w,
                stats_w,
                kite_writer_w,
                local_flush_bytes,
                zstd_level,
            );
            for chunk in chunk_rx_w.iter() {
                worker.process_chunk(&chunk)?;
            }
            worker.flush_all()
        });
        worker_handles.push(handle);
    }
    drop(chunk_rx);

    reader_thread.join().expect("reader thread panicked");
    for handle in worker_handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => anyhow::bail!("count worker error: {}", e),
            Err(_) => anyhow::bail!("count worker thread panicked"),
        }
    }

    let t_finalize = collect_timing.then(Instant::now);
    let finalize_summary = Arc::into_inner(kite_writer)
        .ok_or_else(|| anyhow::anyhow!("count writer still has outstanding references"))?
        .finalize()?;
    let finalize_secs = t_finalize.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
    stats.set_partition_segment_distribution(
        finalize_summary.hottest_partition_segments,
        finalize_summary.least_partition_segments,
    );
    if let Some(t_finalize) = t_finalize {
        stats.add_finalize_ns(t_finalize.elapsed().as_nanos() as u64);
    }

    let wall = wall_start.elapsed();

    if let Some((running, handle)) = progress_handle {
        running.store(false, Ordering::Relaxed);
        let _ = handle.join();
        clear_progress_lines(2);
    }

    if args.test_mode {
        let kite_size = std::fs::metadata(&output_path)
            .map(|m| m.len())
            .unwrap_or(0);
        eprintln!(
            "[kite count] finalize:    {:.3}s ({:.1} GB written, {:.1} GB data area, {} chunks, {} rows)",
            finalize_secs,
            kite_size as f64 / 1e9,
            finalize_summary.data_end as f64 / 1e9,
            finalize_summary.total_chunks,
            finalize_summary.total_rows,
        );
        stats.print_report(args, n_threads, partitions, wall);
    } else {
        eprintln!(
            "[kite count] done in {:.3}s  ({} threads, {} decomp, {} workers)",
            wall.as_secs_f64(),
            n_threads,
            decomp_threads,
            worker_threads
        );
    }

    Ok(())
}

fn derive_heuristic_plan(
    args: &CountArgs,
    compression: CompressionType,
    key_width: u16,
    total_threads: usize,
    chunk_size_bytes: usize,
    total_memory_bytes: usize,
) -> anyhow::Result<HeuristicPlan> {
    let input_bytes = std::fs::metadata(&args.input).map(|m| m.len()).unwrap_or(0);
    let sample = sample_input(Path::new(&args.input), compression)?;
    let encoding_factor = encoding_factor_for(key_width);
    let target_partition_bytes = target_partition_bytes_for(args.mode, args.k, key_width);
    let min_worker_budget = GIB / 2;
    let mut reasons = Vec::new();

    reasons.push(format!(
        "Encoding coefficient E={} because k={} uses {} path",
        encoding_factor,
        args.k,
        if key_width == KEY_WIDTH_U64 {
            "u64"
        } else {
            "u128-or-wider"
        }
    ));

    let decomp_threads = match compression {
        CompressionType::Plain => {
            reasons.push("Input is plain FASTQ, so decomp_threads fixed at 1".to_string());
            1
        }
        CompressionType::Gzip => {
            reasons.push("Input is standard gzip, so decomp runs on the reader thread and worker threads get the remaining cores".to_string());
            1
        }
        CompressionType::Bgzf => {
            let max_decomp = (total_threads / 2).max(1);
            let divisor = match args.k {
                0..=21 => 4,
                22..=31 => 4,
                32..=51 => 5,
                52..=75 => 6,
                _ => 8,
            };
            let derived = total_threads
                .checked_div(divisor)
                .unwrap_or(1)
                .max(1)
                .min(max_decomp);
            reasons.push(format!(
                "decomp_threads={} from BGZF stage ratio {}:{} (threads/divisor = {}/{}) capped at {}",
                derived,
                1,
                divisor.saturating_sub(1),
                total_threads,
                divisor,
                max_decomp,
            ));
            derived
        }
    };

    let queue_depth = match compression {
        CompressionType::Plain => 8,
        _ => decomp_threads.saturating_mul(2).clamp(8, 64),
    };
    let decomp_budget_bytes = chunk_size_bytes.saturating_mul(queue_depth);
    let untracked_reserve_bytes =
        estimate_untracked_reserve_bytes(total_memory_bytes, total_threads, decomp_threads);
    let mut worker_threads = total_threads.saturating_sub(decomp_threads + 1).max(1);
    reasons.push(format!(
        "worker_threads={} after reserving one reader slot",
        worker_threads,
    ));
    reasons.push(format!(
        "Reserved {:.1}GB for allocator/thread-stack/decompress overhead outside the tracked hot-path budget",
        untracked_reserve_bytes as f64 / GIB as f64,
    ));
    let mut partitions = if args.partitions != 0 {
        reasons.push(format!("Using user-defined partitions={}", args.partitions));
        args.partitions
    } else {
        let derived = derive_partition_count(
            input_bytes,
            sample.avg_read_len,
            encoding_factor,
            target_partition_bytes,
        );
        let estimated_physical_gb =
            estimated_partitionable_bytes(input_bytes, sample.avg_read_len, encoding_factor) as f64
                / GIB as f64;
        reasons.push(format!(
            "Auto partitions={} from estimated physical payload {:.1}GB targeting ~{}MB per partition",
            derived,
            estimated_physical_gb,
            target_partition_bytes / MIB,
        ));
        derived
    };

    let requested_staging_mb = args.buffer.clamp(4, args.mode.target_staging_cap_mb());
    let mut staging_candidates_mb = vec![16usize, 8, 4];
    staging_candidates_mb.retain(|value| *value <= requested_staging_mb);
    if staging_candidates_mb.is_empty() {
        staging_candidates_mb.push(requested_staging_mb);
    }
    staging_candidates_mb.sort_unstable_by(|left, right| right.cmp(left));
    staging_candidates_mb.dedup();

    let mut staging_index = 0usize;
    loop {
        let staging_bytes = staging_candidates_mb[staging_index] * MIB;
        let reserved_bytes = decomp_budget_bytes
            .saturating_add(partitions.saturating_mul(staging_bytes))
            .saturating_add(worker_threads.saturating_mul(WORKER_SCRATCH_BYTES))
            .saturating_add(untracked_reserve_bytes);
        let tracked_budget_bytes = total_memory_bytes
            .saturating_sub(untracked_reserve_bytes)
            .saturating_sub(decomp_budget_bytes);
        let worker_budget_bytes = total_memory_bytes
            .saturating_sub(reserved_bytes)
            .checked_div(worker_threads.max(1))
            .unwrap_or(0);
        let local_flush_bytes = streaming_flush_target_bytes(
            args.mode,
            args.k,
            key_width,
            chunk_size_bytes,
            staging_bytes,
            worker_budget_bytes,
        );

        if reserved_bytes < total_memory_bytes
            && tracked_budget_bytes > 0
            && worker_budget_bytes >= min_worker_budget
        {
            if staging_index > 0 {
                reasons.push(format!(
                    "Reduced staging to {}MB to save worker budget",
                    staging_candidates_mb[staging_index],
                ));
            } else {
                reasons.push(format!(
                    "Kept staging at {}MB because worker budget reached {:.2}GB/worker",
                    staging_candidates_mb[staging_index],
                    worker_budget_bytes as f64 / GIB as f64,
                ));
            }
            return Ok(HeuristicPlan {
                encoding_factor,
                input_bytes,
                avg_read_len: sample.avg_read_len,
                sampled_reads: sample.sampled_reads,
                sampled_bytes: sample.sampled_bytes,
                partitions,
                staging_bytes,
                decomp_threads,
                worker_threads,
                worker_budget_bytes,
                local_flush_bytes,
                tracked_budget_bytes,
                decomp_budget_bytes,
                queue_depth,
                hard_memory_limit_bytes: total_memory_bytes,
                untracked_reserve_bytes,
                reserved_bytes,
                reasons,
            });
        }

        if staging_index + 1 < staging_candidates_mb.len() {
            staging_index += 1;
            continue;
        }

        if args.partitions == 0 && partitions > AUTO_MIN_PARTITIONS {
            partitions /= 2;
            staging_index = 0;
            reasons.push(format!(
                "Reduced partitions to {} because worker budget dropped below {:.1}GB/worker",
                partitions,
                min_worker_budget as f64 / GIB as f64,
            ));
            continue;
        }

        if worker_threads > 1 {
            worker_threads -= 1;
            staging_index = 0;
            reasons.push(format!(
                "Reduced worker_threads to {} to protect per-worker budget",
                worker_threads,
            ));
            continue;
        }

        anyhow::bail!(
            "unable to derive count heuristic plan within memory budget {} bytes",
            total_memory_bytes
        );
    }
}

fn estimate_untracked_reserve_bytes(
    hard_limit_bytes: usize,
    total_threads: usize,
    decomp_threads: usize,
) -> usize {
    let base_fraction = hard_limit_bytes / 6;
    let thread_stack_bytes = total_threads
        .saturating_add(3)
        .saturating_mul(THREAD_STACK_RESERVE_BYTES);
    let codec_bytes = decomp_threads.saturating_mul(32 * MIB);
    base_fraction
        .saturating_add(thread_stack_bytes)
        .saturating_add(codec_bytes)
        .clamp(2 * GIB, 12 * GIB)
        .min(hard_limit_bytes / 3)
}

fn target_partition_bytes_for(mode: crate::cli::CountMode, k: u8, key_width: u16) -> usize {
    let base_mb: usize = match (key_width, mode) {
        (KEY_WIDTH_U64, crate::cli::CountMode::Fast) => 640,
        (KEY_WIDTH_U64, crate::cli::CountMode::Balanced) => 512,
        (KEY_WIDTH_U64, crate::cli::CountMode::Compact) => 448,
        (KEY_WIDTH_U128, crate::cli::CountMode::Fast) => 448,
        (KEY_WIDTH_U128, crate::cli::CountMode::Balanced) => 384,
        (KEY_WIDTH_U128, crate::cli::CountMode::Compact) => 320,
        (_, crate::cli::CountMode::Fast) => 320,
        (_, crate::cli::CountMode::Balanced) => 256,
        (_, crate::cli::CountMode::Compact) => 224,
    };
    let adjusted_mb = match (key_width, mode, k) {
        (KEY_WIDTH_U64, crate::cli::CountMode::Fast, 22..=31) => 192,
        (KEY_WIDTH_U64, crate::cli::CountMode::Balanced, 22..=31) => 224,
        (KEY_WIDTH_U64, crate::cli::CountMode::Compact, 22..=31) => 256,
        (_, _, 0..=21) => base_mb + 64,
        (_, _, 52..=u8::MAX) => base_mb.saturating_sub(64),
        _ => base_mb,
    };
    adjusted_mb * MIB
}

fn streaming_flush_target_bytes(
    mode: crate::cli::CountMode,
    k: u8,
    key_width: u16,
    chunk_size_bytes: usize,
    staging_bytes: usize,
    worker_budget_bytes: usize,
) -> usize {
    let mode_cap_mb = match (key_width, mode) {
        (KEY_WIDTH_U64, crate::cli::CountMode::Fast) => 192,
        (KEY_WIDTH_U64, crate::cli::CountMode::Balanced) => 144,
        (KEY_WIDTH_U64, crate::cli::CountMode::Compact) => 112,
        (KEY_WIDTH_U128, crate::cli::CountMode::Fast) => 256,
        (KEY_WIDTH_U128, crate::cli::CountMode::Balanced) => 192,
        (KEY_WIDTH_U128, crate::cli::CountMode::Compact) => 144,
        (_, crate::cli::CountMode::Fast) => 320,
        (_, crate::cli::CountMode::Balanced) => 256,
        (_, crate::cli::CountMode::Compact) => 192,
    };
    let path_bias_bytes = if k <= 21 {
        32 * MIB
    } else if k <= 31 {
        16 * MIB
    } else {
        0
    };
    let baseline = chunk_size_bytes
        .saturating_mul(2)
        .max(staging_bytes.saturating_mul(4))
        .max(mode_cap_mb * MIB)
        .saturating_add(path_bias_bytes);
    baseline.min(worker_budget_bytes)
}

fn encoding_factor_for(key_width: u16) -> usize {
    if key_width == KEY_WIDTH_U64 { 1 } else { 2 }
}

fn derive_partition_count(
    input_bytes: u64,
    avg_read_len: f64,
    encoding_factor: usize,
    target_partition_bytes: usize,
) -> usize {
    let estimated = estimated_partitionable_bytes(input_bytes, avg_read_len, encoding_factor);
    let raw = estimated.div_ceil(target_partition_bytes).max(1);
    round_up_power_of_two(raw).clamp(AUTO_MIN_PARTITIONS, AUTO_MAX_PARTITIONS)
}

fn estimated_partitionable_bytes(
    input_bytes: u64,
    avg_read_len: f64,
    encoding_factor: usize,
) -> usize {
    let expansion = (avg_read_len / 10.0).clamp(4.0, 24.0);
    ((input_bytes as f64) * expansion * encoding_factor as f64) as usize
}

fn sample_input(path: &Path, compression: CompressionType) -> anyhow::Result<InputSample> {
    let file = open_input(path)?;
    let buffered = std::io::BufReader::with_capacity(2 * MIB, file);

    let mut sampled_reads = 0u64;
    let mut sampled_bases = 0u64;
    let mut sampled_bytes = 0usize;

    match compression {
        CompressionType::Plain => {
            let mut reader = PlainChunkedReader::new(buffered, SAMPLE_CHUNK_BYTES);
            while sampled_reads < SAMPLE_MAX_READS && sampled_bytes < SAMPLE_MAX_BYTES {
                let Some(chunk) = reader.next_chunk()? else {
                    break;
                };
                sampled_bytes = sampled_bytes.saturating_add(chunk.len());
                let _ = parse_sequences(&chunk, |seq| {
                    sampled_reads += 1;
                    sampled_bases += seq.len() as u64;
                });
            }
        }
        CompressionType::Gzip | CompressionType::Bgzf => {
            let mut reader = GzipChunkedReader::new(buffered, SAMPLE_CHUNK_BYTES);
            while sampled_reads < SAMPLE_MAX_READS && sampled_bytes < SAMPLE_MAX_BYTES {
                let Some(chunk) = reader.next_chunk()? else {
                    break;
                };
                sampled_bytes = sampled_bytes.saturating_add(chunk.len());
                let _ = parse_sequences(&chunk, |seq| {
                    sampled_reads += 1;
                    sampled_bases += seq.len() as u64;
                });
            }
        }
    }

    let avg_read_len = if sampled_reads == 0 {
        150.0
    } else {
        sampled_bases as f64 / sampled_reads as f64
    };

    Ok(InputSample {
        avg_read_len,
        sampled_reads,
        sampled_bytes,
    })
}

fn round_up_power_of_two(value: usize) -> usize {
    value.max(1).next_power_of_two()
}
