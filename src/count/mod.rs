pub mod reader;
pub mod decompress;
pub mod parser;
pub mod kmer;
pub mod sorter;
pub mod router;
pub mod writer;
pub mod worker;
pub mod memory;
pub mod bgzf;

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use crossbeam_channel::bounded;

use crate::cli::CountArgs;
use crate::stats::CountStats;
use crate::count::reader::{detect_compression, CompressionType, open_input};
use crate::count::decompress::{GzipChunkedReader, PlainChunkedReader};
use crate::count::memory::MemoryBudget;
use crate::count::writer::ConcurrentKiteWriter;
use crate::count::worker::CountWorker;
use crate::count::kmer::{KmerWord, KmerU256};
use crate::format::header::{KEY_WIDTH_U128, KEY_WIDTH_U256, KEY_WIDTH_U64};
use crate::format::{CODEC_DVLE, CODEC_RAW, CODEC_ZSTD};

/// Return log2(partitions) — the number of bucket index bits for Fibonacci hash routing.
pub fn compute_bucket_bits(partitions: usize) -> u32 {
    partitions.trailing_zeros()
}

pub fn run_count(args: &CountArgs) -> anyhow::Result<()> {
    if args.k < 1 || args.k > 128 {
        anyhow::bail!("k must be between 1 and 128 (got {})", args.k);
    }
    if !args.partitions.is_power_of_two() {
        anyhow::bail!("--partitions must be a power of 2 (got {})", args.partitions);
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
        anyhow::bail!(
            "count output must use .kite suffix (got '{}')",
            args.output
        );
    }

    let key_width: u16 = match K::entry_size() {
        36 => KEY_WIDTH_U256,
        20 => KEY_WIDTH_U128,
        _  => KEY_WIDTH_U64,
    };
    let codec_mask: u16 = (CODEC_RAW | CODEC_DVLE | CODEC_ZSTD) as u16;

    let n_threads = if args.threads == 0 { num_cpus::get() } else { args.threads };

    // Create output dir if needed
    if let Some(parent) = Path::new(&args.output).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let budget = Arc::new(MemoryBudget::new(args.max_memory_gb));
    let stats = Arc::new(CountStats::new(args.partitions));

    let compression = detect_compression(Path::new(&args.input))?;

    // ─── Thread allocation ─────────────────────────────────────────────────────
    // No writer thread any more — workers write directly to per-bucket temp files.
    // Allocate one extra thread for the reader.
    let (n_decomp, n_workers) = match compression {
        CompressionType::Bgzf => {
            // Fewer decomp threads: BGZF decompression is fast (~500 MB/s/thread).
            // Giving more threads to workers improves radix-sort throughput.
            let d = (n_threads / 4).max(1).min(12);
            let w = (n_threads.saturating_sub(d + 1)).max(1); // +1 for reader
            (d, w)
        }
        _ => {
            let w = (n_threads.saturating_sub(1)).max(1);
            (1, w)
        }
    };

    if args.test_mode {
        eprintln!("[kite count] input:       {}", args.input);
        eprintln!("[kite count] compression: {}", compression);
        eprintln!("[kite count] k={}, canonical={}, partitions={}",
            args.k, args.canonical, args.partitions);
        eprintln!("[kite count] mode={}, zstd-level={}",
            match args.mode {
                crate::cli::CountMode::Fast => "fast",
                crate::cli::CountMode::Balanced => "balanced",
                crate::cli::CountMode::Compact => "compact",
            },
            args.mode.zstd_level());
        eprintln!("[kite count] threads total={}, decomp={}, count_workers={}",
            n_threads, n_decomp, n_workers);
        eprintln!("[kite count] max-memory-gb={:.1}, per-worker budget={:.1}MB",
            args.max_memory_gb,
            budget.worker_budget_bytes(n_workers) as f64 / 1024.0 / 1024.0);
    }

    let wall_start = Instant::now();

    let chunk_size_bytes = (args.chunk_kmers * 2)
        .max(8 * 1024 * 1024)
        .min(64 * 1024 * 1024);

    let decomp_budget = (budget.limit() as f64 * 0.20) as usize;
    let queue_depth = (decomp_budget / chunk_size_bytes.max(1)).max(8).min(256);

    let (chunk_tx, chunk_rx) = bounded::<Vec<u8>>(queue_depth);

    let worker_budget = budget.worker_budget_bytes(n_workers);
    let bucket_shift = compute_bucket_bits(args.partitions);

    // Copy args for threads / writer setup
    let input_path       = args.input.clone();
    let k                = args.k;
    let canonical        = args.canonical;
    let partitions       = args.partitions;
    let compression_type = compression;
    let _show_progress   = args.progress;
    let zstd_level       = args.mode.zstd_level();

    let output_path = args.output.clone();
    let kite_writer = Arc::new(ConcurrentKiteWriter::create(
        &output_path,
        k,
        canonical,
        partitions,
        key_width,
        codec_mask,
    )?);

    // ─── Reader / Decompress Thread ───────────────────────────────────────────
    let stats_reader = Arc::clone(&stats);
    let chunk_tx_reader = chunk_tx.clone();
    let reader_thread = thread::spawn(move || {
        match compression_type {
            CompressionType::Bgzf => {
                let t = Instant::now();
                match bgzf::decompress_bgzf_parallel(
                    &input_path, n_decomp, chunk_size_bytes, &chunk_tx_reader,
                ) {
                    Ok((comp_bytes, decomp_bytes)) => {
                        stats_reader.add_comp_bytes(comp_bytes);
                        stats_reader.add_decomp_ns(t.elapsed().as_nanos() as u64);
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
                let buffered = std::io::BufReader::with_capacity(2 * 1024 * 1024, file);
                let mut reader = GzipChunkedReader::new(buffered, chunk_size_bytes);
                loop {
                    let t = Instant::now();
                    match reader.next_chunk() {
                        Ok(Some(chunk)) => {
                            stats_reader.add_decomp_ns(t.elapsed().as_nanos() as u64);
                            if chunk_tx_reader.send(chunk).is_err() { break; }
                        }
                        Ok(None) => break,
                        Err(e) => { eprintln!("decompress error: {}", e); break; }
                    }
                }
            }
            CompressionType::Plain => {
                if let Ok(meta) = std::fs::metadata(&input_path) {
                    stats_reader.add_comp_bytes(meta.len());
                }
                let file = open_input(Path::new(&input_path)).expect("open input");
                let buffered = std::io::BufReader::with_capacity(2 * 1024 * 1024, file);
                let mut reader = PlainChunkedReader::new(buffered, chunk_size_bytes);
                loop {
                    match reader.next_chunk() {
                        Ok(Some(chunk)) => { if chunk_tx_reader.send(chunk).is_err() { break; } }
                        Ok(None) => break,
                        Err(e) => { eprintln!("read error: {}", e); break; }
                    }
                }
            }
        }
    });
    drop(chunk_tx);

    // ─── Count Worker Threads ─────────────────────────────────────────────────
    let mut worker_handles = Vec::new();
    for _ in 0..n_workers {
        let chunk_rx_w    = chunk_rx.clone();
        let budget_w      = Arc::clone(&budget);
        let stats_w       = Arc::clone(&stats);
        let kite_writer_w = Arc::clone(&kite_writer);

        let handle = thread::spawn(move || -> std::io::Result<()> {
            let mut worker = CountWorker::<K>::new(
                k, canonical, partitions, bucket_shift,
                budget_w, stats_w,
                kite_writer_w,
                zstd_level,
                worker_budget,
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
    for h in worker_handles {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => anyhow::bail!("count worker error: {}", e),
            Err(_)     => anyhow::bail!("count worker thread panicked"),
        }
    }

    let t_finalize = Instant::now();
    let finalize_summary = Arc::into_inner(kite_writer)
        .ok_or_else(|| anyhow::anyhow!("count writer still has outstanding references"))?
        .finalize()?;
    let finalize_secs = t_finalize.elapsed().as_secs_f64();
    stats.set_partition_segment_distribution(
        finalize_summary.hottest_partition_segments,
        finalize_summary.least_partition_segments,
    );
    stats.add_finalize_ns(t_finalize.elapsed().as_nanos() as u64);

    let wall = wall_start.elapsed();

    if args.test_mode {
        let kite_size = std::fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0);
        eprintln!("[kite count] finalize:    {:.3}s ({:.1} GB written, {:.1} GB data area, {} chunks, {} rows)",
            finalize_secs,
            kite_size as f64 / 1e9,
            finalize_summary.data_end as f64 / 1e9,
            finalize_summary.total_chunks,
            finalize_summary.total_rows);
        stats.print_report(args, wall);
    } else {
        eprintln!("[kite count] done in {:.3}s  ({} threads, {} decomp, {} workers)",
            wall.as_secs_f64(), n_threads, n_decomp, n_workers);
    }

    Ok(())
}

