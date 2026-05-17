use std::fs::OpenOptions;
use std::io::Write;
use std::time::Instant;

use anyhow::Context;

use crate::cli::InfoArgs;
use crate::dump::reader::KiteReader;
use crate::format::header::update_count_filters_in_file;
use crate::format::{CODEC_DVLE, CODEC_RAW, CODEC_ZSTD};
use crate::kiteops::format_count_filter;
use crate::stats::print_simple_test_report;

pub fn run_info(args: &InfoArgs) -> anyhow::Result<()> {
    let started = Instant::now();
    if args.set_min.is_some() && args.clear_min {
        anyhow::bail!("--set-min and --clear-min cannot be used together");
    }
    if args.set_max.is_some() && args.clear_max {
        anyhow::bail!("--set-max and --clear-max cannot be used together");
    }
    eprintln!("[kite info] opening: {}", args.input);
    if args.set_min.is_some() || args.set_max.is_some() || args.clear_min || args.clear_max {
        let current = KiteReader::open(&args.input)
            .with_context(|| format!("failed to open {}", args.input))?
            .header;
        let new_min = if args.clear_min {
            None
        } else {
            args.set_min.or(current.min_count)
        };
        let new_max = if args.clear_max {
            None
        } else {
            args.set_max.or(current.max_count)
        };
        if let (Some(min), Some(max)) = (new_min, new_max) {
            if min > max {
                anyhow::bail!("header min ({}) must be <= header max ({})", min, max);
            }
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&args.input)?;
        update_count_filters_in_file(&mut file, new_min, new_max)?;
        eprintln!(
            "[kite info] updated header count filters: min={}, max={}",
            format_count_filter(new_min),
            format_count_filter(new_max),
        );
    }
    let reader =
        KiteReader::open(&args.input).with_context(|| format!("failed to open {}", args.input))?;
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let file_size = std::fs::metadata(&args.input)?.len();

    let total_partition_bytes: u64 = reader
        .partition_entries
        .iter()
        .map(|entry| entry.data_bytes)
        .sum();
    let total_partition_rows: u64 = reader
        .partition_entries
        .iter()
        .map(|entry| entry.total_rows)
        .sum();
    let hottest_partition_rows = reader
        .partition_entries
        .iter()
        .map(|entry| entry.total_rows)
        .max()
        .unwrap_or(0);
    let coldest_partition_rows = reader
        .partition_entries
        .iter()
        .map(|entry| entry.total_rows)
        .min()
        .unwrap_or(0);
    let max_chunk_bytes = reader
        .chunk_entries
        .iter()
        .map(|entry| entry.encoded_bytes)
        .max()
        .unwrap_or(0);
    let min_chunk_bytes = reader
        .chunk_entries
        .iter()
        .map(|entry| entry.encoded_bytes)
        .min()
        .unwrap_or(0);

    let mut raw_chunks = 0u64;
    let mut dvle_chunks = 0u64;
    let mut zstd_chunks = 0u64;
    for chunk in &reader.chunk_entries {
        if chunk.codec & CODEC_RAW != 0 {
            raw_chunks += 1;
        }
        if chunk.codec & CODEC_DVLE != 0 {
            dvle_chunks += 1;
        }
        if chunk.codec & CODEC_ZSTD != 0 {
            zstd_chunks += 1;
        }
    }

    writeln!(out, "Kite Database")?;
    writeln!(out, "  Path:               {}", args.input)?;
    writeln!(out, "  File Size:          {} bytes", file_size)?;
    writeln!(out)?;
    writeln!(out, "Format")?;
    writeln!(out, "  Version:            {}", reader.header.version)?;
    writeln!(out, "  K:                  {}", reader.header.k)?;
    writeln!(
        out,
        "  Canonical:          {}",
        reader.header.canonical != 0
    )?;
    writeln!(
        out,
        "  Min Count:          {}",
        format_count_filter(reader.header.min_count)
    )?;
    writeln!(
        out,
        "  Max Count:          {}",
        format_count_filter(reader.header.max_count)
    )?;
    writeln!(
        out,
        "  Filter Semantics:   downstream commands exclude rows outside Min/Max"
    )?;
    writeln!(out, "  Partitions:         {}", reader.header.partitions)?;
    writeln!(out, "  Key Width:          {}", reader.header.key_width)?;
    writeln!(out)?;
    writeln!(out, "Content")?;
    writeln!(out, "  Total Chunks:       {}", reader.total_chunks)?;
    writeln!(out, "  Total Rows:         {}", reader.header.total_rows)?;
    writeln!(out, "  Partition Rows:     {}", total_partition_rows)?;
    writeln!(out, "  Partition Bytes:    {}", total_partition_bytes)?;
    writeln!(out)?;
    writeln!(out, "Codec Mix")?;
    writeln!(out, "  RAW Chunks:         {}", raw_chunks)?;
    writeln!(out, "  DVLE Chunks:        {}", dvle_chunks)?;
    writeln!(out, "  ZSTD Chunks:        {}", zstd_chunks)?;
    writeln!(out)?;
    writeln!(out, "Distribution")?;
    writeln!(out, "  Largest Chunk:      {} bytes", max_chunk_bytes)?;
    writeln!(out, "  Smallest Chunk:     {} bytes", min_chunk_bytes)?;
    writeln!(out, "  Hottest Partition:  {} rows", hottest_partition_rows)?;
    writeln!(out, "  Coldest Partition:  {} rows", coldest_partition_rows)?;
    out.flush()?;
    if args.test_mode {
        let wall = started.elapsed();
        print_simple_test_report(
            "info",
            &[
                ("input", args.input.clone()),
                ("file_size_bytes", file_size.to_string()),
                ("partitions", reader.header.partitions.to_string()),
                ("total_chunks", reader.total_chunks.to_string()),
                ("total_rows", reader.header.total_rows.to_string()),
                ("min", format_count_filter(reader.header.min_count)),
                ("max", format_count_filter(reader.header.max_count)),
            ],
            wall,
        );
    }
    eprintln!(
        "[kite info] done in {:.3}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
