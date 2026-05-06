pub mod reader;
pub mod run_cursor;
pub mod loser_tree;
pub mod decoder;
pub mod writer;
pub mod worker;
pub mod merge;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use crossbeam_channel::bounded;
use rayon::prelude::*;

use crate::cli::DumpArgs;
use crate::dump::merge::stream_partition_slabs;
use crate::dump::reader::KiteReader;
use crate::dump::writer::{
    default_writer_threads,
    queue_slots_for,
    SlabPool,
    DUMP_QUEUE_BYTES,
    DUMP_SLAB_BYTES,
};
use crate::stats::DumpStats;

pub fn run_dump(args: &DumpArgs) -> anyhow::Result<()> {
    let n_threads = if args.threads == 0 { num_cpus::get() } else { args.threads };
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

    let chunk_counts: Vec<u64> = reader.partition_entries.iter().map(|p| p.chunk_count).collect();
    let hottest = chunk_counts.iter().copied().max().unwrap_or(0);
    let coldest = chunk_counts.iter().copied().min().unwrap_or(0);
    let mut chunk_sizes: Vec<u64> = reader.chunk_entries.iter().map(|c| c.encoded_bytes).collect();
    chunk_sizes.sort_unstable();
    let p99_chunk_bytes = if chunk_sizes.is_empty() {
        0
    } else {
        let idx = ((chunk_sizes.len() as f64) * 0.99).floor() as usize;
        chunk_sizes[idx.min(chunk_sizes.len() - 1)]
    };
    stats.set_partition_distribution(hottest, coldest, p99_chunk_bytes);

    let worker_count = n_threads.clamp(1, n_partitions.max(1));
    let writer_threads = if args.writer_threads == 0 {
        default_writer_threads(n_threads)
    } else {
        args.writer_threads.max(1)
    };
    let slab_pool = Arc::new(SlabPool::new(
        slab_bytes,
        queue_slots_for(slab_bytes, DUMP_QUEUE_BYTES, writer_threads).saturating_add(writer_threads),
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
            stream_partition_slabs(partition_id, &reader, k, entry_size, slab_bytes, &slab_pool, collect_timing, &stats, |bytes, rows| {
                out.write_all(&bytes)?;
                stats.add_rows(rows);
                stats.add_written(bytes.len() as u64);
                Ok(())
            })?;
            stats.inc_partitions();
        }
        out.flush()?;
        let wall = wall_start.elapsed();
        if args.test_mode {
            stats.print_report(&args.input, total_chunks, n_partitions, wall);
        } else {
            eprintln!("[kite dump] done in {:.3}s", wall.as_secs_f64());
        }
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
    let channel_depth = worker_count.saturating_mul(2).max(4);
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

    if args.test_mode {
        eprintln!("[kite dump] opening: {}", args.input);
        eprintln!("[kite dump] k={}, partitions={}, total_chunks={}", k, n_partitions, total_chunks);
        eprintln!(
            "[kite dump] slab_bytes={}, workers={}, channel_depth={}, mode=async-writer",
            slab_bytes,
            worker_count,
            channel_depth,
        );
    }

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
    if args.test_mode {
        stats.print_report(&args.input, total_chunks, n_partitions, wall);
    } else {
        eprintln!("[kite dump] done in {:.3}s", wall.as_secs_f64());
    }
    Ok(())
}


