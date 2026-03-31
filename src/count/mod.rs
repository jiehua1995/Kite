use crate::cli::{CanonicalMode, CountArgs, CountWriteMode};
use crate::count::fastq::{GzipKind, detect_gzip_kind};
use crate::count::kmer::KmerMode;
use crate::count::perf::{PerfCounters, PerfSnapshot};
use crate::utils::fmt::{format_duration, format_size};
use crate::utils::kite::{KiteMetadata, current_timestamp_rfc3339};
use anyhow::Result;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum U64CountStrategy {
    InMemory,
    Chunked,
    LocalSpill,
    #[allow(dead_code)]
    OwnerReducer,
    #[allow(dead_code)]
    OwnerInMemReducer,
}

// 中文：`count` 目录专门处理 FASTQ -> `.kite` 的构建路径，
// 包含读取、滚动编码、分桶、排序、归并和最终写库。
// English: The `count` directory owns the FASTQ -> `.kite` build pipeline,
// including reading, rolling encoding, partitioning, sorting, merging, and
// final database writing.
pub mod buffer;
pub mod decode;
pub mod fastq;
pub mod kmer;
pub mod merge;
pub mod perf;
pub mod radix_sort;

/// 中文：执行 `kite count`。输入是 FASTQ 和构建参数，输出是 `.kite` 数据库
/// 以及终端上的 reads/k-mers/unique 等摘要信息。
/// English: Execute `kite count`. Inputs are FASTQ plus build parameters;
/// outputs are the `.kite` database and a stdout summary of reads/k-mers/uniq.
pub fn run(args: &CountArgs) -> Result<()> {
    let wall_start = Instant::now();
    let kmer_mode = KmerMode::from_k(args.k);
    let input_bytes = fs::metadata(&args.input).map(|m| m.len()).unwrap_or(0);
    let max_memory_bytes = args
        .max_memory_gb
        .map(|gb| gb.saturating_mul(1024 * 1024 * 1024));
    let use_partitioned_u64 =
        !args.baseline && matches!(args.mode, CountWriteMode::Fast) && kmer_mode == KmerMode::U64;
    let use_partitioned_u128 =
        !args.baseline
            && matches!(args.mode, CountWriteMode::Fast)
            && kmer_mode == KmerMode::U128
            && input_bytes >= (1u64 << 30);
    let gzip_kind = detect_gzip_kind(&args.input)?;
    let input_profile = if use_partitioned_u64 {
        Some(buffer::sample_input_profile(&args.input, 2048)?)
    } else {
        None
    };
    let mut runtime_threads = args.threads.max(1);
    if input_bytes > 0 {
        let heavy_u128 = kmer_mode == KmerMode::U128 && args.k >= 41;
        let short_u64 = kmer_mode == KmerMode::U64 && args.k <= 31;
        let small_input_thread_cap = match gzip_kind {
            Some(GzipKind::Bgzf) => {
                if input_bytes <= (128u64 << 20) {
                    if heavy_u128 { 8 } else if short_u64 { 24 } else { 16 }
                } else if input_bytes <= (1u64 << 30) {
                    if heavy_u128 { 16 } else if short_u64 { 36 } else { 24 }
                } else if input_bytes <= (4u64 << 30) {
                    if heavy_u128 {
                        28
                    } else if kmer_mode == KmerMode::U64 && args.k <= 21 {
                        36
                    } else if short_u64 {
                        48
                    } else {
                        36
                    }
                } else {
                    args.threads.max(1)
                }
            }
            _ => {
                if input_bytes <= (128u64 << 20) {
                    if heavy_u128 { 6 } else { 12 }
                } else if input_bytes <= (1u64 << 30) {
                    if heavy_u128 { 12 } else { 24 }
                } else if input_bytes <= (4u64 << 30) {
                    if heavy_u128 { 24 } else { 36 }
                } else {
                    args.threads.max(1)
                }
            }
        };
        runtime_threads = runtime_threads.min(small_input_thread_cap);
    }
    if matches!(kmer_mode, KmerMode::U128) {
        runtime_threads = runtime_threads.min(48);
    }
    if kmer_mode == KmerMode::U64 && args.k <= 21 {
        if matches!(gzip_kind, Some(GzipKind::Bgzf)) {
            runtime_threads = runtime_threads.min(48);
        }
    } else if kmer_mode == KmerMode::U64
        && args.k <= 31
        && matches!(gzip_kind, Some(GzipKind::PlainGzip))
    {
        runtime_threads = runtime_threads.min(48);
    }
    let u64_strategy = if use_partitioned_u64 {
        let profile = input_profile.unwrap_or_default();
        let max_mem = max_memory_bytes.unwrap_or(0);
        if args.k <= 21 {
            if input_bytes <= (2u64 << 30) && max_mem >= (24u64 << 30) {
                U64CountStrategy::InMemory
            } else {
                U64CountStrategy::LocalSpill
            }
        } else if args.k <= 31 {
            if input_bytes <= (2u64 << 30) {
                U64CountStrategy::InMemory
            } else if input_bytes <= (6u64 << 30)
                && max_mem >= (48u64 << 30)
                && profile.mean_read_len >= 80.0
            {
                U64CountStrategy::Chunked
            } else {
                U64CountStrategy::LocalSpill
            }
        } else if input_bytes <= (2u64 << 30) && profile.mean_read_len <= 64.0 {
            U64CountStrategy::InMemory
        } else {
            U64CountStrategy::LocalSpill
        }
    } else {
        U64CountStrategy::InMemory
    };
    let pipeline = buffer::pipeline_threads(runtime_threads);
    let owner_pipeline = if matches!(
        u64_strategy,
        U64CountStrategy::OwnerReducer | U64CountStrategy::OwnerInMemReducer
    ) {
        Some(buffer::u64_owner_pipeline_threads(
            runtime_threads,
            gzip_kind,
            args.k,
        ))
    } else {
        None
    };
    let effective_partitions = if kmer_mode == KmerMode::U128 {
        buffer::effective_u128_partitions(args.partitions, runtime_threads, max_memory_bytes)
    } else {
        match owner_pipeline {
            Some(owner) => buffer::effective_owner_partitions(
                args.partitions,
                runtime_threads,
                max_memory_bytes,
                owner.reducers,
            ),
            None => {
                if matches!(u64_strategy, U64CountStrategy::LocalSpill) {
                    buffer::effective_spill_partitions(
                        args.partitions,
                        runtime_threads,
                        max_memory_bytes,
                    )
                } else {
                    buffer::effective_partitions(args.partitions, runtime_threads, max_memory_bytes)
                }
            }
        }
    };
    let effective_chunk_kmers = if kmer_mode == KmerMode::U128 {
        buffer::effective_u128_chunk_kmers(args.chunk_kmers, runtime_threads, max_memory_bytes)
    } else {
        buffer::effective_chunk_kmers(args.chunk_kmers, runtime_threads, max_memory_bytes)
    };
    let effective_partitions = if args.partitions == 256 && input_bytes <= (128u64 << 20) {
        if kmer_mode == KmerMode::U128 { effective_partitions.min(16) } else { effective_partitions.min(256) }
    } else if args.partitions == 256 && input_bytes <= (1u64 << 30) {
        if kmer_mode == KmerMode::U128 { effective_partitions.min(64) } else { effective_partitions.min(512) }
    } else {
        effective_partitions
    };
    let effective_chunk_kmers = if args.chunk_kmers == 1_000_000 && input_bytes <= (128u64 << 20) {
        effective_chunk_kmers.min(1_000_000)
    } else if args.chunk_kmers == 1_000_000 && input_bytes <= (1u64 << 30) {
        effective_chunk_kmers.min(2_000_000)
    } else {
        effective_chunk_kmers
    };
    let effective_partitions =
        if use_partitioned_u64 && args.k <= 21 && args.partitions == 256 && input_bytes <= (2u64 << 30) {
            effective_partitions.min(128)
        } else {
            effective_partitions
        };
    let effective_chunk_kmers =
        if use_partitioned_u64
            && args.k <= 21
            && args.chunk_kmers == 1_000_000
            && input_bytes <= (2u64 << 30)
            && max_memory_bytes.unwrap_or(0) >= (24u64 << 30)
        {
            effective_chunk_kmers.max(8_000_000)
        } else {
            effective_chunk_kmers
        };
    let count_workers = match gzip_kind {
        Some(GzipKind::PlainGzip) => runtime_threads
            .saturating_sub(pipeline.gzip_decompressors)
            .max(1),
        Some(GzipKind::Bgzf) => runtime_threads
            .saturating_sub(pipeline.bgzf_decompressors + 1)
            .max(1),
        None => runtime_threads.max(1),
    };
    let input_path_kind = if args.baseline {
        "baseline generic reader".to_string()
    } else {
        match gzip_kind {
            Some(GzipKind::PlainGzip) => {
                "plain gzip block reader + aligned worker parsing".to_string()
            }
            Some(GzipKind::Bgzf) => format!(
                "BGZF libdeflater block pipeline + aligned worker parsing (bgzf_threads={}, count_workers={})",
                pipeline.bgzf_decompressors, count_workers
            ),
            None => "plain FASTQ reader".to_string(),
        }
    };
    let cfg = buffer::CountConfig {
        k: args.k,
        partitions: effective_partitions,
        threads: runtime_threads,
        chunk_kmers: effective_chunk_kmers,
        max_memory_bytes,
        canonical: matches!(args.canonical, CanonicalMode::True),
        test_mode: args.test_mode,
        progress: args.progress,
        progress_counters: if args.progress {
            Some(Arc::new(buffer::ProgressCounters::new()))
        } else {
            None
        },
        perf_counters: if args.test_mode {
            Some(Arc::new(PerfCounters::new()))
        } else {
            None
        },
    };
    let tuning = buffer::ingest_tuning(runtime_threads, max_memory_bytes);
    let strategy_label = if kmer_mode == KmerMode::U128 && use_partitioned_u128 {
        "u128_partitioned"
    } else if matches!(u64_strategy, U64CountStrategy::Chunked) {
        "u64_chunked"
    } else if matches!(u64_strategy, U64CountStrategy::LocalSpill) {
        "u64_local_spill"
    } else if matches!(u64_strategy, U64CountStrategy::OwnerReducer) {
        "u64_owner_reducer"
    } else if matches!(u64_strategy, U64CountStrategy::OwnerInMemReducer) {
        "u64_owner_inmem_reducer"
    } else {
        "in_memory"
    };
    eprintln!(
        "[kite count] launch: k={} strategy={} threads={} partitions={} chunk_kmers={} max_memory={}",
        args.k,
        strategy_label,
        runtime_threads,
        effective_partitions,
        effective_chunk_kmers,
        max_memory_bytes
            .map(format_size)
            .unwrap_or_else(|| "auto".to_string())
    );
    let write_options = decode::WriteBinaryOptions {
        mode: match args.mode {
            CountWriteMode::Fast => decode::CompressionMode::Fast,
            CountWriteMode::Balanced => decode::CompressionMode::Balanced,
            CountWriteMode::Compact => decode::CompressionMode::Compact,
        },
        zstd_level: args.zstd_level,
        threads: runtime_threads,
        metadata: KiteMetadata {
            source_path: Some(args.input.display().to_string()),
            created_at: Some(current_timestamp_rfc3339()),
            total_reads: None,
            total_kmers: None,
            unique_kmers: None,
            minimizer: None,
            partitions: Some(effective_partitions),
            count_threads: Some(runtime_threads),
            chunk_kmers: Some(effective_chunk_kmers),
            canonical: Some(matches!(args.canonical, CanonicalMode::True)),
            write_mode: Some(
                match args.mode {
                    CountWriteMode::Fast => "fast",
                    CountWriteMode::Balanced => "balanced",
                    CountWriteMode::Compact => "compact",
                }
                .to_string(),
            ),
            zstd_level: None,
        },
    };
    let (
        timings,
        total_reads,
        total_kmers,
        unique,
        file_bytes,
        write_elapsed,
        spill_build_stats,
        spill_write_stats,
    ) = if use_partitioned_u128 {
        let outcome = buffer::count_optimized_u128_partitioned(&args.input, &cfg)?;
        let t_write = Instant::now();
        let write_stats =
            decode::write_partitioned_u128_binary(&args.output, args.k, &outcome, write_options)?;
        let write_elapsed = t_write.elapsed();
        let file_bytes = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);
        let _ = fs::remove_dir_all(&outcome.temp_dir);
        (
            outcome.timings,
            outcome.total_reads,
            outcome.total_kmers,
            write_stats.entries as usize,
            file_bytes,
            write_elapsed,
            None,
            write_stats.spill,
        )
    } else if matches!(u64_strategy, U64CountStrategy::Chunked) {
        let outcome = buffer::count_optimized_u64_chunked(&args.input, &cfg)?;
        let t_write = Instant::now();
        let write_stats =
            decode::write_chunked_u64_binary(&args.output, args.k, &outcome, write_options)?;
        let write_elapsed = t_write.elapsed();
        let file_bytes = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);
        (
            outcome.timings,
            outcome.total_reads,
            outcome.total_kmers,
            write_stats.entries as usize,
            file_bytes,
            write_elapsed,
            None,
            write_stats.spill,
        )
    } else if matches!(u64_strategy, U64CountStrategy::LocalSpill) {
        let outcome = buffer::count_optimized_u64_local_spill(&args.input, &cfg)?;
        let spill_build_stats = outcome.spill_stats.clone();
        let t_write = Instant::now();
        let write_stats =
            decode::write_partitioned_u64_binary(&args.output, args.k, &outcome, write_options)?;
        let write_elapsed = t_write.elapsed();
        let file_bytes = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);
        let _ = fs::remove_dir_all(&outcome.temp_dir);
        (
            outcome.timings,
            outcome.total_reads,
            outcome.total_kmers,
            write_stats.entries as usize,
            file_bytes,
            write_elapsed,
            spill_build_stats,
            write_stats.spill,
        )
    } else if matches!(u64_strategy, U64CountStrategy::OwnerReducer) {
        let outcome = match u64_strategy {
            U64CountStrategy::OwnerReducer => {
                buffer::count_optimized_u64_partitioned(&args.input, &cfg)?
            }
            U64CountStrategy::InMemory
            | U64CountStrategy::Chunked
            | U64CountStrategy::LocalSpill
            | U64CountStrategy::OwnerInMemReducer => unreachable!(),
        };
        let t_write = Instant::now();
        let write_stats =
            decode::write_partitioned_u64_binary(&args.output, args.k, &outcome, write_options)?;
        let write_elapsed = t_write.elapsed();
        let file_bytes = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);
        let _ = fs::remove_dir_all(&outcome.temp_dir);
        (
            outcome.timings,
            outcome.total_reads,
            outcome.total_kmers,
            write_stats.entries as usize,
            file_bytes,
            write_elapsed,
            outcome.spill_stats.clone(),
            write_stats.spill,
        )
    } else if matches!(u64_strategy, U64CountStrategy::OwnerInMemReducer) {
        let outcome = buffer::count_optimized_u64_inmem_reducer(&args.input, &cfg)?;
        let t_write = Instant::now();
        decode::write_binary(&args.output, args.k, &outcome, write_options)?;
        let write_elapsed = t_write.elapsed();
        let unique = match &outcome.table {
            buffer::CountTable::U64(v) => v.len(),
            buffer::CountTable::U128(v) => v.len(),
            buffer::CountTable::Packed(v) => v.len(),
        };
        let file_bytes = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);
        (
            outcome.timings,
            outcome.total_reads,
            outcome.total_kmers,
            unique,
            file_bytes,
            write_elapsed,
            None,
            None,
        )
    } else {
        let outcome = if args.baseline {
            buffer::count_baseline(&args.input, &cfg)?
        } else {
            buffer::count_optimized(&args.input, &cfg)?
        };
        let t_write = Instant::now();
        decode::write_binary(&args.output, args.k, &outcome, write_options)?;
        let write_elapsed = t_write.elapsed();
        let unique = match &outcome.table {
            buffer::CountTable::U64(v) => v.len(),
            buffer::CountTable::U128(v) => v.len(),
            buffer::CountTable::Packed(v) => v.len(),
        };
        let file_bytes = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);
        (
            outcome.timings,
            outcome.total_reads,
            outcome.total_kmers,
            unique,
            file_bytes,
            write_elapsed,
            None,
            None,
        )
    };
    let wall_elapsed = wall_start.elapsed();

    println!(
        "[kite count] [Input: 1/1] {} ({})",
        args.input.display(),
        format_size(input_bytes)
    );
    println!("[kite count] ingestion: {}", input_path_kind);
    if runtime_threads != args.threads {
        println!(
            "[kite count] thread_cap: requested={} effective={}",
            args.threads, runtime_threads
        );
    }
    if use_partitioned_u64 {
        let strategy = match u64_strategy {
            U64CountStrategy::Chunked => "chunked",
            U64CountStrategy::LocalSpill => "local_spill",
            U64CountStrategy::OwnerReducer => "owner_reducer",
            U64CountStrategy::OwnerInMemReducer => "owner_inmem_reducer",
            U64CountStrategy::InMemory => "in_memory",
        };
        println!("[kite count] strategy: {}", strategy);
    }
    if let Some(profile) = input_profile {
        println!(
            "[kite count] input_profile: sampled_reads={} mean_read_len={:.1} max_read_len={}",
            profile.sampled_reads,
            profile.mean_read_len,
            profile.max_read_len
        );
    }
    println!(
        "[kite count] tuning: decomp_block={} work_chunk={} block_queue_cap={} worker_queue_cap={} max_memory={}",
        format_size(tuning.decomp_block_bytes as u64),
        format_size(tuning.work_chunk_bytes as u64),
        tuning.block_queue_cap,
        tuning.worker_queue_cap,
        max_memory_bytes
            .map(format_size)
            .unwrap_or_else(|| "auto".to_string())
    );
    match owner_pipeline {
        Some(owner) => println!(
            "[kite count] runtime_params: partitions={} chunk_kmers={} prealloc_block_buffers={} extractors={} reducers={} gzip_decompressors={} bgzf_decompressors={}",
            effective_partitions,
            effective_chunk_kmers,
            tuning.prealloc_block_buffers,
            owner.extractors,
            owner.reducers,
            pipeline.gzip_decompressors,
            pipeline.bgzf_decompressors
        ),
        None => println!(
            "[kite count] runtime_params: partitions={} chunk_kmers={} prealloc_block_buffers={} count_workers={} gzip_decompressors={} bgzf_decompressors={}",
            effective_partitions,
            effective_chunk_kmers,
            tuning.prealloc_block_buffers,
            count_workers,
            pipeline.gzip_decompressors,
            pipeline.bgzf_decompressors
        ),
    };
    println!(
        "[kite count] stage_times: parse={} generate={} sort={} merge={}",
        format_duration(timings.parsing),
        format_duration(timings.kmer_generation),
        format_duration(timings.sorting),
        format_duration(timings.merging),
    );
    println!("[kite count] write_time: {}", format_duration(write_elapsed));
    println!("[kite count] reads: {}", total_reads);
    println!("[kite count] kmers: {}", total_kmers);
    println!("[kite count] unique_kmers: {}", unique);
    println!(
        "[kite count] canonical: {}",
        matches!(args.canonical, CanonicalMode::True)
    );
    println!(
        "[kite count] [Output: 1/1] {} ({})",
        args.output.display(),
        format_size(file_bytes)
    );
    println!(
        "[kite count] elapsed: {}",
        format_duration(wall_elapsed)
    );
    if args.test_mode {
        let perf = cfg
            .perf_counters
            .as_ref()
            .map(|c| c.snapshot())
            .unwrap_or_default();
        print_test_mode_summary(
            &perf,
            &timings,
            write_elapsed,
            wall_elapsed,
            total_reads,
            total_kmers,
            read_peak_rss_kb(),
            spill_build_stats.as_ref(),
            spill_write_stats.as_ref(),
        );
    }
    Ok(())
}

fn read_peak_rss_kb() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())?;
            return Some(kb);
        }
    }
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())?;
            return Some(kb);
        }
    }
    None
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn ns_to_duration(ns: u64) -> Duration {
    Duration::from_nanos(ns)
}

fn print_test_line(
    phase: &str,
    d: Duration,
    total: Duration,
    throughput: String,
    notes: &str,
) {
    let pct = if total.is_zero() {
        0.0
    } else {
        (secs(d) / secs(total)) * 100.0
    };
    println!(
        "[kite count][test] {:<22} {:>8.3} {:>8.1}% {:<20} {}",
        phase,
        secs(d),
        pct,
        throughput,
        notes
    );
}

fn print_test_mode_summary(
    perf: &PerfSnapshot,
    timings: &buffer::TimingBreakdown,
    write_elapsed: Duration,
    wall_elapsed: Duration,
    total_reads: u64,
    total_kmers: u64,
    peak_rss_kb: Option<u64>,
    spill_build: Option<&buffer::SpillBuildStats>,
    spill_write: Option<&decode::SpillWriteStats>,
) {
    let read_d = ns_to_duration(perf.read_ns);
    let decompress_d = ns_to_duration(perf.decompress_ns);
    let dispatch_d = ns_to_duration(perf.dispatch_ns);
    let worker_parse_d = ns_to_duration(perf.worker_parse_ns);
    let worker_count_d = ns_to_duration(perf.worker_count_ns);
    let total = wall_elapsed;

    println!("[kite count][test] phase                  time(s)    %total    throughput           notes");
    print_test_line(
        "read_io",
        read_d,
        total,
        format!("{:.1} MB/s", (perf.dispatched_bytes as f64 / (1 << 20) as f64) / secs(read_d).max(1e-9)),
        "sum-thread",
    );
    print_test_line(
        "decompress",
        decompress_d,
        total,
        format!("{:.1} MB/s", (perf.dispatched_bytes as f64 / (1 << 20) as f64) / secs(decompress_d).max(1e-9)),
        "sum-thread",
    );
    print_test_line(
        "dispatch_stage_wall",
        timings.parsing,
        total,
        format!("{:.2} chunk/s", perf.dispatched_chunks as f64 / secs(timings.parsing).max(1e-9)),
        "wall",
    );
    print_test_line(
        "dispatch_handoff",
        dispatch_d,
        total,
        format!("{:.2} chunk/s", perf.dispatched_chunks as f64 / secs(dispatch_d).max(1e-9)),
        "sum-thread",
    );
    print_test_line(
        "fastq_parse",
        worker_parse_d,
        total,
        format!("{:.2} Mread/s", (total_reads as f64 / 1e6) / secs(worker_parse_d).max(1e-9)),
        "sum-worker",
    );
    print_test_line(
        "count_update",
        worker_count_d,
        total,
        format!("{:.2} Mkmers/s", (total_kmers as f64 / 1e6) / secs(worker_count_d).max(1e-9)),
        "sum-worker",
    );
    print_test_line(
        "local_sort_flush",
        timings.sorting,
        total,
        format!("{:.2} Mkmers/s", (total_kmers as f64 / 1e6) / secs(timings.sorting).max(1e-9)),
        "sum-worker",
    );
    if let Some(stats) = spill_build {
        print_test_line(
            "spill_local_flush",
            stats.spill_local_flush,
            total,
            format!("{:.1} MB/s", mb_per_s(stats.spill_bytes_written, stats.spill_local_flush)),
            "sum-worker",
        );
        print_test_line(
            "spill_local_merge",
            stats.spill_local_merge,
            total,
            format!(
                "{:.1} MB/s",
                mb_per_s(stats.spill_bytes_read_local_merge, stats.spill_local_merge)
            ),
            "sum-worker",
        );
    }
    print_test_line(
        "merge_reduce",
        timings.merging,
        total,
        format!("{:.2} Mkmers/s", (total_kmers as f64 / 1e6) / secs(timings.merging).max(1e-9)),
        "wall",
    );
    if let Some(stats) = spill_write {
        print_test_line(
            "spill_final_merge",
            stats.spill_final_merge,
            total,
            format!("{:.1} MB/s", mb_per_s(stats.spill_bytes_read, stats.spill_final_merge)),
            "wall",
        );
        print_test_line(
            "spill_output",
            stats.spill_output,
            total,
            format!("{:.2} Mkmers/s", (total_kmers as f64 / 1e6) / secs(stats.spill_output).max(1e-9)),
            "wall",
        );
    }
    print_test_line(
        "write_output_total",
        write_elapsed,
        total,
        format!("{:.2} Mkmers/s", (total_kmers as f64 / 1e6) / secs(write_elapsed).max(1e-9)),
        "wall",
    );
    println!(
        "[kite count][test] total                  {:>8.3} {:>8.1}% {:<20} wall",
        secs(total),
        100.0,
        format!("{:.2} Mkmers/s", (total_kmers as f64 / 1e6) / secs(total).max(1e-9))
    );
    println!(
        "[kite count][test] peak_rss               {}",
        peak_rss_kb
            .map(|kb| format_size(kb * 1024))
            .unwrap_or_else(|| "unknown".to_string())
    );
    println!("[kite count][test] reads_total             {}", total_reads);
    println!("[kite count][test] kmers_total             {}", total_kmers);
    println!("[kite count][test] dispatched_chunks       {}", perf.dispatched_chunks);
    println!(
        "[kite count][test] avg_chunk_bytes         {:.1} KB",
        if perf.dispatched_chunks == 0 {
            0.0
        } else {
            (perf.dispatched_bytes as f64 / perf.dispatched_chunks as f64) / 1024.0
        }
    );
    println!(
        "[kite count][test] max_worker_queue_len    {}",
        perf.max_worker_queue_len
    );
    println!(
        "[kite count][test] merge_inputs            partitions={} active={} chunks={}",
        perf.merge_partitions, perf.active_merge_partitions, perf.merge_chunks
    );
    println!(
        "[kite count][test] u64_hash_flushes        {}",
        perf.u64_hash_flushes
    );
    if let Some(stats) = spill_build {
        let avg_files_per_partition = if stats.active_partitions == 0 {
            0.0
        } else {
            stats.files_per_partition_sum as f64 / stats.active_partitions as f64
        };
        let avg_local_merge_fanin = if stats.local_merge_groups == 0 {
            0.0
        } else {
            stats.local_merge_fanin_sum as f64 / stats.local_merge_groups as f64
        };
        println!("[kite count][test] spill_flushes           {}", stats.spill_flushes);
        println!(
            "[kite count][test] spill_files_created     {}",
            stats.spill_files_created
        );
        println!(
            "[kite count][test] spill_files             {}",
            stats.spill_files_final
        );
        println!(
            "[kite count][test] spill_bytes_written     {}",
            format_size(stats.spill_bytes_written)
        );
        println!(
            "[kite count][test] spill_bytes_read_local  {}",
            format_size(stats.spill_bytes_read_local_merge)
        );
        println!(
            "[kite count][test] active_partitions       {}",
            stats.active_partitions
        );
        println!(
            "[kite count][test] hottest_partition       part={} files={}",
            stats.hottest_partition, stats.hottest_partition_file_count
        );
        println!(
            "[kite count][test] avg_files_per_partition {:.2}",
            avg_files_per_partition
        );
        println!(
            "[kite count][test] max_files_per_partition {}",
            stats.max_files_per_partition
        );
        println!(
            "[kite count][test] local_merge_passes      total={} max={} groups={}",
            stats.local_merge_passes_total,
            stats.local_merge_passes_max,
            stats.local_merge_groups
        );
        println!(
            "[kite count][test] local_merge_fanin       avg={:.2} max={}",
            avg_local_merge_fanin,
            stats.local_merge_fanin_max
        );
        println!(
            "[kite count][test] merge_fanin_dist        {}",
            format_fanin_buckets(&stats.fanin_buckets)
        );
        println!(
            "[kite count][test] temp_write_throughput   {:.1} MB/s",
            mb_per_s(
                stats.spill_bytes_written,
                stats.spill_local_flush + stats.spill_local_merge,
            )
        );
    }
    if let Some(stats) = spill_write {
        println!(
            "[kite count][test] spill_bytes_read_final  {}",
            format_size(stats.spill_bytes_read)
        );
        println!(
            "[kite count][test] final_merge_passes      {}",
            stats.merge_passes
        );
        println!(
            "[kite count][test] final_hottest_partition part={} files={}",
            stats.hottest_partition, stats.hottest_partition_file_count
        );
        println!(
            "[kite count][test] temp_read_throughput    {:.1} MB/s",
            mb_per_s(
                stats.spill_bytes_read
                    + spill_build
                        .map(|s| s.spill_bytes_read_local_merge)
                        .unwrap_or(0),
                stats.spill_final_merge
                    + spill_build
                        .map(|s| s.spill_local_merge)
                        .unwrap_or(Duration::ZERO),
            )
        );
    }
}

fn mb_per_s(bytes: u64, d: Duration) -> f64 {
    (bytes as f64 / (1 << 20) as f64) / secs(d).max(1e-9)
}

fn format_fanin_buckets(buckets: &[u64; 6]) -> String {
    format!(
        "1:{} 2-4:{} 5-8:{} 9-16:{} 17-32:{} 33+:{}",
        buckets[0], buckets[1], buckets[2], buckets[3], buckets[4], buckets[5]
    )
}
