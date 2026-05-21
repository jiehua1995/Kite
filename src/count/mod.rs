pub mod bgzf;
pub mod decompress;
pub mod kmer;
pub mod memory;
pub mod parser;
pub mod reader;
pub mod sorter;
pub mod worker;
pub mod writer;

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;
use flate2::read::GzDecoder;
use indicatif::{ProgressBar, ProgressStyle};
use memmap2::Mmap;

use crate::cli::CountArgs;
use crate::count::bgzf::{find_fastq_split_4lines, scan_bgzf_blocks};
use crate::count::decompress::{GzipChunkedReader, PlainChunkedReader};
use crate::count::kmer::{KmerU256, KmerWord};
use crate::count::memory::MemoryBudget;
use crate::count::parser::parse_sequences;
use crate::count::reader::{detect_compression, open_input, CompressionType};
use crate::count::worker::CountWorker;
use crate::count::writer::ConcurrentKiteWriter;
use crate::format::header::{KEY_WIDTH_U128, KEY_WIDTH_U256, KEY_WIDTH_U64};
use crate::format::{CODEC_DVLE, CODEC_RAW, CODEC_ZSTD};
use crate::kiteops::print_command_line;
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
    sampled_input_bytes: u64,
    sampled_decomp_bytes: usize,
}

#[derive(Debug, Clone)]
struct HeuristicPlan {
    encoding_factor: usize,
    input_bytes: u64,
    avg_read_len: f64,
    sampled_reads: u64,
    sampled_input_bytes: u64,
    sampled_decomp_bytes: usize,
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
            let initial_total = estimated_total_reads.max(1);
            let bar = ProgressBar::new(initial_total);
            let style = ProgressStyle::with_template(
                "{spinner:.green} {msg}\n[{bar:40.cyan/blue}] {percent:>3}% | {pos}/{len} reads | eta {eta_precise}",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar());
            bar.set_style(style.progress_chars("=>-"));
            bar.enable_steady_tick(Duration::from_millis(120));
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
                let display_total_reads = estimated_total_reads.max(reads);
                bar.set_length(display_total_reads.max(1));
                bar.set_position(reads.min(display_total_reads));
                bar.set_message(format!(
                    "count input={} k={} threads={}/{}/{} mem={:.2}GB | kmers {}/{} | decomp {} | elapsed {} | tracked_mem {:.2}GB",
                    fmt_bytes_compact(input_bytes),
                    k,
                    total_threads,
                    decomp_threads,
                    worker_threads,
                    hard_memory_limit_bytes as f64 / GIB as f64,
                    fmt_num_compact(kmers),
                    fmt_num_compact(estimated_total_kmers),
                    fmt_bytes_compact(decomp),
                    fmt_duration_compact(Duration::from_secs_f64(wall)),
                    rss,
                ));
            }
            let reads = stats.reads_processed.load(Ordering::Relaxed);
            let kmers = stats.valid_kmers_generated.load(Ordering::Relaxed);
            let decomp = stats.decomp_bytes_produced.load(Ordering::Relaxed);
            let display_total_reads = estimated_total_reads.max(reads).max(1);
            let wall = started_at.elapsed().as_secs_f64();
            bar.set_length(display_total_reads);
            bar.set_position(reads.min(display_total_reads));
            bar.set_message(format!(
                "count done | reads {}/{} | kmers {} | decomp {} | elapsed {}",
                fmt_num_compact(reads),
                fmt_num_compact(display_total_reads),
                fmt_num_compact(kmers),
                fmt_bytes_compact(decomp),
                fmt_duration_compact(Duration::from_secs_f64(wall)),
            ));
            bar.finish_and_clear();
            eprintln!(
                "[kite count] progress reads={}/{} kmers={} decomp={} elapsed={}",
                fmt_num_compact(reads),
                fmt_num_compact(display_total_reads),
                fmt_num_compact(kmers),
                fmt_bytes_compact(decomp),
                fmt_duration_compact(Duration::from_secs_f64(wall)),
            );
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

pub fn run_count(args: &CountArgs) -> anyhow::Result<()> {
    print_command_line("count");
    if args.k < 1 || args.k > 128 {
        anyhow::bail!("k must be between 1 and 128 (got {})", args.k);
    }
    if args.partitions != 0 && !args.partitions.is_power_of_two() {
        anyhow::bail!(
            "--partitions must be a power of 2 (got {})",
            args.partitions
        );
    }
    if let (Some(min), Some(max)) = (args.min, args.max) {
        if min > max {
            anyhow::bail!("--min ({}) must be <= --max ({})", min, max);
        }
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
    let estimated_total_reads = if plan.sampled_input_bytes > 0 && plan.sampled_reads > 0 {
        ((plan.input_bytes as u128)
            .saturating_mul(plan.sampled_reads as u128)
            .checked_div(plan.sampled_input_bytes as u128)
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

    eprintln!("[kite count] input:       {}", args.input);
    eprintln!("[kite count] output:      {}", args.output);
    eprintln!("[kite count] compression: {}", compression);
    eprintln!(
        "[kite count] k={}, canonical={}, partitions={}, min={}, max={}",
        args.k,
        args.canonical,
        plan.partitions,
        args.min
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        args.max
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
    );
    eprintln!(
        "[kite count] mode={}, codec={}",
        match args.mode {
            crate::cli::CountMode::Fast => "fast",
            crate::cli::CountMode::Balanced => "balanced",
            crate::cli::CountMode::Compact => "compact",
        },
        codec_policy_label(K::entry_size(), args.mode.zstd_level())
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
    if args.test_mode {
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
            "[kite count][auto] sampled_input={:.1}MB, sampled_decomp={:.1}MB, reserved={:.1}MB, writer_lanes={}",
            plan.sampled_input_bytes as f64 / MIB as f64,
            plan.sampled_decomp_bytes as f64 / MIB as f64,
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
        args.min,
        args.max,
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
                Err(e) => eprintln!("[kite count] bgzf_decompress_error: {}", e),
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
                        eprintln!("[kite count] decompress_error: {}", e);
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
                        eprintln!("[kite count] read_error: {}", e);
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
    if let Some((running, handle)) = progress_handle {
        running.store(false, Ordering::Relaxed);
        let _ = handle.join();
    }
    if args.min.is_some() || args.max.is_some() {
        eprintln!(
            "[kite count] header filter markers saved: min={}, max={} (raw k-mers are retained in the .kite file)",
            args.min
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".to_string()),
            args.max
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".to_string()),
        );
    }
    stats.set_partition_segment_distribution(
        finalize_summary.hottest_partition_segments,
        finalize_summary.least_partition_segments,
    );
    if let Some(t_finalize) = t_finalize {
        stats.add_finalize_ns(t_finalize.elapsed().as_nanos() as u64);
    }

    let wall = wall_start.elapsed();

    if args.test_mode {
        let kite_size = std::fs::metadata(&output_path)
            .map(|m| m.len())
            .unwrap_or(0);
        stats.print_report(
            args,
            n_threads,
            partitions,
            kite_size,
            finalize_summary.data_end,
            finalize_summary.total_chunks,
            finalize_summary.total_rows,
            wall,
        );
    }
    let reads = stats.reads_processed.load(Ordering::Relaxed);
    let bases = stats.bases_processed.load(Ordering::Relaxed);
    let kmers = stats.kmers_generated.load(Ordering::Relaxed);
    eprintln!(
        "[kite count] summary reads={} bases={} kmers={} raw_rows_written={} chunks={} header_min={} header_max={} elapsed={:.3}s",
        reads,
        bases,
        kmers,
        finalize_summary.total_rows,
        finalize_summary.total_chunks,
        args.min
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        args.max
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        wall.as_secs_f64(),
    );
    eprintln!(
        "[kite count] done in {:.3}s ({}, {} chunks, {} rows)",
        wall.as_secs_f64(),
        output_path,
        finalize_summary.total_chunks,
        finalize_summary.total_rows,
    );

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
    let target_partition_bytes = target_partition_bytes_for(args.k, key_width);
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

    let requested_staging_mb = args.buffer.max(4);
    let mut staging_candidates_mb = vec![requested_staging_mb, 16usize, 8, 4];
    staging_candidates_mb.retain(|value| *value <= requested_staging_mb);
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
                sampled_input_bytes: sample.sampled_input_bytes,
                sampled_decomp_bytes: sample.sampled_decomp_bytes,
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

fn codec_policy_label(entry_size: usize, zstd_level: i32) -> &'static str {
    match (entry_size, zstd_level > 0) {
        (36, false) => "raw-u256",
        (36, true) => "raw-u256+zstd-if-smaller",
        (_, false) => "dvle-only",
        (_, true) => "dvle+zstd-if-smaller",
    }
}

fn target_partition_bytes_for(k: u8, key_width: u16) -> usize {
    let base_mb: usize = match key_width {
        KEY_WIDTH_U64 => 640,
        KEY_WIDTH_U128 => 448,
        _ => 320,
    };
    let adjusted_mb = match (key_width, k) {
        (KEY_WIDTH_U64, 22..=31) => 192,
        (_, 0..=21) => base_mb + 64,
        (_, 52..=u8::MAX) => base_mb.saturating_sub(64),
        _ => base_mb,
    };
    adjusted_mb * MIB
}

fn streaming_flush_target_bytes(
    _k: u8,
    _key_width: u16,
    chunk_size_bytes: usize,
    staging_bytes: usize,
    worker_budget_bytes: usize,
) -> usize {
    let minimum = chunk_size_bytes
        .saturating_mul(2)
        .max(staging_bytes.saturating_mul(4));
    let target = worker_budget_bytes.saturating_mul(45) / 100;
    target.max(minimum)
}

fn encoding_factor_for(key_width: u16) -> usize {
    if key_width == KEY_WIDTH_U64 {
        1
    } else {
        2
    }
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

    let mut sampled_reads = 0u64;
    let mut sampled_bases = 0u64;
    let mut sampled_decomp_bytes = 0usize;
    let mut sampled_input_bytes = 0u64;

    match compression {
        CompressionType::Plain => {
            let buffered = std::io::BufReader::with_capacity(2 * MIB, file);
            let mut reader = PlainChunkedReader::new(buffered, SAMPLE_CHUNK_BYTES);
            while sampled_reads < SAMPLE_MAX_READS && sampled_decomp_bytes < SAMPLE_MAX_BYTES {
                let Some(chunk) = reader.next_chunk()? else {
                    break;
                };
                sampled_decomp_bytes = sampled_decomp_bytes.saturating_add(chunk.len());
                sampled_input_bytes = sampled_decomp_bytes as u64;
                let _ = parse_sequences(&chunk, |seq| {
                    sampled_reads += 1;
                    sampled_bases += seq.len() as u64;
                });
            }
        }
        CompressionType::Gzip => {
            let bytes_read = Arc::new(AtomicU64::new(0));
            let counting = CountingReader::new(file, Arc::clone(&bytes_read));
            let mut reader = GzipChunkedReader::new(counting, SAMPLE_CHUNK_BYTES);
            while sampled_reads < SAMPLE_MAX_READS && sampled_decomp_bytes < SAMPLE_MAX_BYTES {
                let Some(chunk) = reader.next_chunk()? else {
                    break;
                };
                sampled_decomp_bytes = sampled_decomp_bytes.saturating_add(chunk.len());
                sampled_input_bytes = bytes_read.load(Ordering::Relaxed);
                let _ = parse_sequences(&chunk, |seq| {
                    sampled_reads += 1;
                    sampled_bases += seq.len() as u64;
                });
            }
        }
        CompressionType::Bgzf => {
            let (reads, bases, input_bytes, decomp_bytes) = sample_bgzf_input(path)?;
            sampled_reads = reads;
            sampled_bases = bases;
            sampled_input_bytes = input_bytes;
            sampled_decomp_bytes = decomp_bytes;
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
        sampled_input_bytes,
        sampled_decomp_bytes,
    })
}

fn sample_bgzf_input(path: &Path) -> anyhow::Result<(u64, u64, u64, usize)> {
    let file = std::fs::File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };
    let blocks = scan_bgzf_blocks(&mmap)?;

    let mut sampled_reads = 0u64;
    let mut sampled_bases = 0u64;
    let mut sampled_input_bytes = 0u64;
    let mut sampled_decomp_bytes = 0usize;
    let mut carry = Vec::with_capacity(SAMPLE_CHUNK_BYTES + 8192);

    for (offset, size) in blocks {
        let raw = &mmap[offset..offset + size];
        let mut out = Vec::with_capacity(65536);
        GzDecoder::new(raw).read_to_end(&mut out)?;

        sampled_input_bytes = sampled_input_bytes.saturating_add(size as u64);
        sampled_decomp_bytes = sampled_decomp_bytes.saturating_add(out.len());
        carry.extend_from_slice(&out);

        let split = find_fastq_split_4lines(&carry);
        if split > 0 {
            let complete = &carry[..split];
            let _ = parse_sequences(complete, |seq| {
                sampled_reads += 1;
                sampled_bases += seq.len() as u64;
            });
            let leftover = carry.len() - split;
            if leftover > 0 {
                carry.copy_within(split.., 0);
            }
            carry.truncate(leftover);
        }

        if sampled_reads >= SAMPLE_MAX_READS || sampled_decomp_bytes >= SAMPLE_MAX_BYTES {
            break;
        }
    }

    Ok((
        sampled_reads,
        sampled_bases,
        sampled_input_bytes,
        sampled_decomp_bytes,
    ))
}

struct CountingReader<R> {
    inner: R,
    bytes_read: Arc<AtomicU64>,
}

impl<R> CountingReader<R> {
    fn new(inner: R, bytes_read: Arc<AtomicU64>) -> Self {
        Self { inner, bytes_read }
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

fn round_up_power_of_two(value: usize) -> usize {
    value.max(1).next_power_of_two()
}
