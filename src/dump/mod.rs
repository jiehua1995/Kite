pub mod reader;
pub mod run_cursor;
pub mod loser_tree;
pub mod decoder;
pub mod writer;
pub mod worker;
pub mod merge;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;

use crate::cli::DumpArgs;
use crate::dump::merge::stream_partition_slabs;
use crate::dump::reader::KiteReader;
use crate::dump::writer::append_file_sendfile;
use crate::stats::DumpStats;

struct ShardPlan {
    shard_id: usize,
    start_partition: usize,
    end_partition: usize,
    total_data_bytes: u64,
}

struct ShardOutput {
    shard_id: usize,
    path: String,
}

pub fn run_dump(args: &DumpArgs) -> anyhow::Result<()> {
    let n_threads = if args.threads == 0 { num_cpus::get() } else { args.threads };
    let slab_bytes = (args.io_buf_mb.max(1) * 1024 * 1024).max(128 * 1024);
    let collect_timing = args.test_mode;

    let wall_start = Instant::now();
    let stats = Arc::new(DumpStats::new(collect_timing));
    let collect_timing = stats.timing_enabled();
    let reader = Arc::new(KiteReader::open(&args.input)?);

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

    let shard_count = n_threads.clamp(1, n_partitions.max(1));
    let shard_plans = build_shard_plans(&reader, shard_count);
    let hottest_shard_partitions = shard_plans
        .iter()
        .map(|s| (s.end_partition - s.start_partition) as u64)
        .max()
        .unwrap_or(0);
    let coldest_shard_partitions = shard_plans
        .iter()
        .map(|s| (s.end_partition - s.start_partition) as u64)
        .min()
        .unwrap_or(0);
    let hottest_shard_bytes = shard_plans.iter().map(|s| s.total_data_bytes).max().unwrap_or(0);
    let coldest_shard_bytes = shard_plans.iter().map(|s| s.total_data_bytes).min().unwrap_or(0);
    stats.set_shard_distribution(
        shard_plans.len() as u64,
        hottest_shard_partitions,
        coldest_shard_partitions,
        hottest_shard_bytes,
        coldest_shard_bytes,
    );

    if args.test_mode {
        eprintln!("[kite dump] opening: {}", args.input);
        eprintln!("[kite dump] k={}, partitions={}, total_chunks={}", k, n_partitions, total_chunks);
        eprintln!(
            "[kite dump] slab_bytes={}, shards={}, mode=partition-range-shards+sendfile",
            slab_bytes,
            shard_plans.len(),
        );
    }

    if args.output == "-" {
        let stdout = std::io::stdout();
        let mut out = BufWriter::with_capacity(slab_bytes, stdout.lock());
        for partition_id in 0..n_partitions {
            let read_bytes: u64 = reader.partition_chunks[partition_id]
                .iter()
                .map(|c| c.payload_len as u64)
                .sum();
            stats.add_read(read_bytes);
            stream_partition_slabs(partition_id, &reader, k, entry_size, slab_bytes, collect_timing, &stats, |bytes, rows| {
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
    let local_tmp_dir = Path::new("/eight_TB/BigMQ/Users/Hua/kmer_counter/kite/temp");
    std::fs::create_dir_all(local_tmp_dir)?;
    let stem = Path::new(&output_path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dump".to_string());
    let pid = std::process::id();
    let prefix = local_tmp_dir.join(format!("kite_dump_{}_{}", pid, stem));
    let prefix = prefix.to_string_lossy().into_owned();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(shard_plans.len().max(1))
        .build()?;

    let stats_work = Arc::clone(&stats);
    let reader_work = Arc::clone(&reader);
    let outputs = pool.install(|| {
        shard_plans
            .into_par_iter()
            .map(|shard| -> anyhow::Result<ShardOutput> {
                let shard_path = format!("{}.shard{:03}", prefix, shard.shard_id);
                let file = File::create(&shard_path)
                    .map_err(|e| anyhow::anyhow!("create shard '{}': {}", shard_path, e))?;
                let mut out = BufWriter::with_capacity(slab_bytes.max(8 * 1024 * 1024), file);

                for partition_id in shard.start_partition..shard.end_partition {
                    let read_bytes: u64 = reader_work.partition_chunks[partition_id]
                        .iter()
                        .map(|c| c.payload_len as u64)
                        .sum();
                    stats_work.add_read(read_bytes);

                    stream_partition_slabs(partition_id, &reader_work, k, entry_size, slab_bytes, collect_timing, &stats_work, |bytes, rows| {
                        let t_write = collect_timing.then(Instant::now);
                        out.write_all(&bytes)?;
                        if let Some(t_write) = t_write {
                            stats_work.add_writer_io_ns(t_write.elapsed().as_nanos() as u64);
                        }
                        stats_work.add_rows(rows);
                        stats_work.add_written(bytes.len() as u64);
                        Ok(())
                    }).map_err(anyhow::Error::from)?;
                    stats_work.inc_partitions();
                }

                out.flush().map_err(anyhow::Error::from)?;
                Ok(ShardOutput {
                    shard_id: shard.shard_id,
                    path: shard_path,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })?;

    if args.progress {
        eprintln!("  [dump] shard workers done, starting sendfile concat");
    }

    let mut outputs = outputs;
    outputs.sort_unstable_by_key(|shard| shard.shard_id);

    let t_concat = collect_timing.then(Instant::now);
    let final_out = File::create(&output_path)?;
    for shard in &outputs {
        let shard_in = File::open(&shard.path)
            .map_err(|e| anyhow::anyhow!("open shard '{}': {}", shard.path, e))?;
        append_file_sendfile(&final_out, &shard_in)
            .map_err(|e| anyhow::anyhow!("sendfile shard '{}': {}", shard.path, e))?;
        let _ = std::fs::remove_file(&shard.path);
    }
    if let Some(t_concat) = t_concat {
        stats.add_concat_ns(t_concat.elapsed().as_nanos() as u64);
    }

    if args.progress {
        eprintln!("  [dump] sendfile concat done");
    }

    let wall = wall_start.elapsed();
    if args.test_mode {
        stats.print_report(&args.input, total_chunks, n_partitions, wall);
    } else {
        eprintln!("[kite dump] done in {:.3}s", wall.as_secs_f64());
    }
    Ok(())
}

fn build_shard_plans(reader: &KiteReader, requested_shards: usize) -> Vec<ShardPlan> {
    let partition_count = reader.partition_entries.len();
    if partition_count == 0 {
        return Vec::new();
    }

    let shard_count = requested_shards.clamp(1, partition_count);

    let total_bytes: u64 = reader.partition_entries.iter().map(|p| p.data_bytes).sum();
    let target_bytes = ((total_bytes + shard_count as u64 - 1) / shard_count as u64).max(1);
    let mut plans = Vec::with_capacity(shard_count);
    let mut start = 0usize;

    for shard_id in 0..shard_count {
        if start >= partition_count {
            break;
        }

        let remaining_shards = shard_count - shard_id;
        let max_end = partition_count - (remaining_shards - 1);
        let mut end = start;
        let mut bytes = 0u64;

        while end < max_end {
            let next_bytes = reader.partition_entries[end].data_bytes;
            if end > start && bytes >= target_bytes {
                break;
            }
            bytes += next_bytes;
            end += 1;
        }

        if end == start {
            bytes = reader.partition_entries[start].data_bytes;
            end = start + 1;
        }

        plans.push(ShardPlan {
            shard_id,
            start_partition: start,
            end_partition: end,
            total_data_bytes: bytes,
        });
        start = end;
    }

    plans
}
