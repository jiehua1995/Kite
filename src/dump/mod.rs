pub mod reader;
pub mod run_cursor;
pub mod loser_tree;
pub mod decoder;
pub mod writer;
pub mod worker;
pub mod merge;

use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;

use crate::cli::DumpArgs;
use crate::dump::merge::{measure_partition_output, stream_partition_slabs, PartitionOutputMeasure};
use crate::dump::reader::KiteReader;
use crate::dump::writer::{
    default_writer_threads,
    queue_slots_for,
    AsyncPwriteWriter,
    DumpWriterConfig,
    SlabPool,
    DUMP_QUEUE_BYTES,
    DUMP_SLAB_BYTES,
};
use crate::stats::DumpStats;

struct WorkerPlan {
    start_partition: usize,
    end_partition: usize,
    total_output_bytes: u64,
}

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

    let measure_results = pool.install(|| {
        (0..n_partitions)
            .into_par_iter()
            .map(|partition_id| measure_partition_output(partition_id, &reader, k, entry_size))
            .collect::<Vec<_>>()
    });
    let partition_measures: Vec<PartitionOutputMeasure> = measure_results
        .into_iter()
        .collect::<std::io::Result<Vec<_>>>()?;

    let mut partition_offsets = vec![0u64; n_partitions];
    let mut total_tsv_bytes = 0u64;
    for (partition_id, measure) in partition_measures.iter().enumerate() {
        partition_offsets[partition_id] = total_tsv_bytes;
        total_tsv_bytes = total_tsv_bytes.saturating_add(measure.bytes);
    }

    let worker_plans = build_worker_plans(&partition_measures, worker_count);
    let hottest_shard_partitions = worker_plans
        .iter()
        .map(|s| (s.end_partition - s.start_partition) as u64)
        .max()
        .unwrap_or(0);
    let coldest_shard_partitions = worker_plans
        .iter()
        .map(|s| (s.end_partition - s.start_partition) as u64)
        .min()
        .unwrap_or(0);
    let hottest_shard_bytes = worker_plans.iter().map(|s| s.total_output_bytes).max().unwrap_or(0);
    let coldest_shard_bytes = worker_plans.iter().map(|s| s.total_output_bytes).min().unwrap_or(0);
    stats.set_shard_distribution(
        worker_plans.len() as u64,
        hottest_shard_partitions,
        coldest_shard_partitions,
        hottest_shard_bytes,
        coldest_shard_bytes,
    );

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
    let async_writer = Arc::new(AsyncPwriteWriter::create(
        &output_path,
        total_tsv_bytes,
        slab_bytes,
        DumpWriterConfig {
            writer_threads,
            queue_bytes: DUMP_QUEUE_BYTES,
        },
        collect_timing,
        Arc::clone(&stats),
        Arc::clone(&slab_pool),
    )?);

    if args.test_mode {
        eprintln!("[kite dump] opening: {}", args.input);
        eprintln!("[kite dump] k={}, partitions={}, total_chunks={}", k, n_partitions, total_chunks);
        eprintln!(
            "[kite dump] slab_bytes={}, workers={}, io_writers={}, queue_mb={}, mode=partition-offsets+async-pwrite",
            slab_bytes,
            worker_plans.len(),
            async_writer.io_writers(),
            async_writer.queue_bytes_limit() / 1024 / 1024,
        );
        eprintln!(
            "[kite dump] final_tsv_bytes={:.2} GB",
            total_tsv_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
        );
    }

    let stats_work = Arc::clone(&stats);
    let reader_work = Arc::clone(&reader);
    let writer_work = Arc::clone(&async_writer);
    let partition_offsets = Arc::new(partition_offsets);
    let partition_measures = Arc::new(partition_measures);
    let worker_results = pool.install(|| {
        worker_plans
            .into_par_iter()
            .map(|worker| -> anyhow::Result<()> {
                for partition_id in worker.start_partition..worker.end_partition {
                    let read_bytes: u64 = reader_work.partition_chunks[partition_id]
                        .iter()
                        .map(|c| c.payload_len as u64)
                        .sum();
                    stats_work.add_read(read_bytes);

                    let partition_base = partition_offsets[partition_id];
                    let partition_end = partition_base + partition_measures[partition_id].bytes;
                    let mut next_offset = partition_base;

                    stream_partition_slabs(partition_id, &reader_work, k, entry_size, slab_bytes, &slab_pool, collect_timing, &stats_work, |bytes, rows| {
                        let offset = next_offset;
                        let bytes_len = bytes.len() as u64;
                        next_offset = next_offset.saturating_add(bytes_len);
                        writer_work.submit(offset, bytes)?;
                        stats_work.add_rows(rows);
                        stats_work.add_written(bytes_len);
                        Ok(())
                    }).map_err(anyhow::Error::from)?;

                    if next_offset != partition_end {
                        anyhow::bail!(
                            "partition {} output bytes mismatch: expected {}, wrote {}",
                            partition_id,
                            partition_measures[partition_id].bytes,
                            next_offset - partition_base,
                        );
                    }
                    stats_work.inc_partitions();
                }
                Ok(())
            })
            .collect::<Vec<_>>()
    });

    for result in worker_results {
        result?;
    }

    drop(writer_work);

    let async_writer = Arc::try_unwrap(async_writer)
        .map_err(|_| anyhow::anyhow!("dump async writer still has outstanding references"))?;
    async_writer.finish()?;

    let wall = wall_start.elapsed();
    if args.test_mode {
        stats.print_report(&args.input, total_chunks, n_partitions, wall);
    } else {
        eprintln!("[kite dump] done in {:.3}s", wall.as_secs_f64());
    }
    Ok(())
}

fn build_worker_plans(measures: &[PartitionOutputMeasure], requested_workers: usize) -> Vec<WorkerPlan> {
    let partition_count = measures.len();
    if partition_count == 0 {
        return Vec::new();
    }

    let worker_count = requested_workers.clamp(1, partition_count);

    let mut prefix_bytes = vec![0u64; partition_count + 1];
    for (idx, measure) in measures.iter().enumerate() {
        prefix_bytes[idx + 1] = prefix_bytes[idx].saturating_add(measure.bytes);
    }

    let inf = u64::MAX;
    let mut best_max = vec![vec![inf; partition_count + 1]; worker_count + 1];
    let mut cut_at = vec![vec![0usize; partition_count + 1]; worker_count + 1];
    best_max[0][0] = 0;

    for groups in 1..=worker_count {
        for end in groups..=partition_count {
            let mut best_cost = inf;
            let mut best_start = groups - 1;

            for start in (groups - 1)..end {
                let previous = best_max[groups - 1][start];
                if previous == inf {
                    continue;
                }

                let segment_bytes = prefix_bytes[end].saturating_sub(prefix_bytes[start]);
                let cost = previous.max(segment_bytes);

                if cost < best_cost {
                    best_cost = cost;
                    best_start = start;
                }
            }

            best_max[groups][end] = best_cost;
            cut_at[groups][end] = best_start;
        }
    }

    let mut ranges_rev = Vec::with_capacity(worker_count);
    let mut end = partition_count;
    let mut groups = worker_count;
    while groups > 0 {
        let start = cut_at[groups][end];
        let total_output_bytes = prefix_bytes[end].saturating_sub(prefix_bytes[start]);
        ranges_rev.push(WorkerPlan {
            start_partition: start,
            end_partition: end,
            total_output_bytes,
        });
        end = start;
        groups -= 1;
    }

    ranges_rev.reverse();
    ranges_rev
}
