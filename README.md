# Kite

<p>
	<img src="doc/logo.png" alt="logo" width="100" align="left">
</p> 
Kite is a high-performance k-mer counting and analysis toolkit written in Rust. It is designed around a simple idea: counting should be fast, dumping should be fast, downstream inspection should be built in, and the internal binary format should remain explicit enough that both humans and AI tools can reason about it.

## Project Description

Kite reads sequencing data, extracts k-mers, stores them in a compact `.kite` database, and exposes a family of subcommands for inspection and lightweight comparative analysis. The project is organized so that each subcommand has its own implementation directory in `src/`, while the shared `.kite` format definition lives in `src/utils/kite.rs`.

This separation is intentional. It allows `count`, `dump`, `info`, `query`, `histo`, `merge`, `profile`, and `sketch` to evolve independently without forcing one command to inherit the performance trade-offs of another.

## Key Features

- Fast k-mer counting from FASTQ and gzip-compressed FASTQ input.
- Size-aware k-mer representations:
  - `u64` for `k <= 32`
  - `u128` for `33 <= k <= 64`
  - packed multi-word representation for `k > 64`
- Unified `.kite` binary format with explicit metadata.
- Built-in `dump`, `info`, `query`, and `histo` commands for direct database inspection.
- Built-in `merge` for exact database-to-database count summation.
- Built-in multi-sample `profile` and `sketch` commands for downstream exploratory analysis.
- Canonical k-mer support.
- Zstandard-compressed storage with multiple write modes.
- Clean subcommand-oriented source tree for performance work and future extension.

## Why Kite Exists

Many k-mer tools are excellent at one narrow stage of the workflow but force the user to switch formats, write custom scripts, or depend on separate analysis utilities afterward. Kite aims to close that gap:

- `count` creates a database.
- `dump` exports an exact tabular view.
- `info` explains what is inside the database.
- `query` answers targeted questions.
- `histo` summarizes abundance structure.
- `merge` combines two or more exact databases into one exact database by summing shared k-mers.
- `profile` produces a union frequency table across samples.
- `sketch` writes exact multi-sample count-derived tables for correlation, projection, and heatmap workflows, plus plotting-only R scripts.

## Command Overview

| Command | Purpose | Typical Output |
|---|---|---|
| `kite count` | Count k-mers from sequencing input and write a `.kite` database | `.kite` |
| `kite dump` | Export a `.kite` database to a plain-text table | `.tsv` |
| `kite info` | Print metadata and summary statistics for one database | stdout |
| `kite query` | Look up one exact k-mer count | stdout |
| `kite histo` | Write a histogram of count frequencies | `.tsv` |
| `kite merge` | Merge two or more compatible `.kite` databases by summing exact counts | `.kite` |
| `kite profile` | Build a union-by-sample raw-count table from multiple databases | `.tsv`, `.csv`, or `.txt` |
| `kite sketch` | Build an exact multi-sample count table from multiple databases, then write correlation, projection-score, and clustered heatmap tables | output directory |
| `kite citation` | Print version and citation information | stdout |

## Build

```bash
cargo build --release
```

Main binary:

```text
target/release/kite.exe
```

## Quick Start

Count 31-mers:

```bash
kite count -k 31 -i input.fastq.gz -o sample.kite --threads 8 --mode fast
```

Inspect the database:

```bash
kite info -i sample.kite
```

Dump exact k-mer counts:

```bash
kite dump -i sample.kite -o sample.tsv -t 8
```

Query one k-mer:

```bash
kite query -i sample.kite --kmer ACGTACGTACGTACGTACGTACGTACGTACG
```

Write a count histogram:

```bash
kite histo -i sample.kite -o sample.histo.tsv
```

Merge multiple compatible databases:

```bash
kite merge -i sample1.kite sample2.kite sample3.kite -o merged.kite --threads 8 --mode fast
```

Prepare multi-sample sketch outputs from multiple databases:

```bash
kite sketch -i sample1.kite sample2.kite --sample-name sample_A sample_B --sample-group group_A group_B -o sketch_out --correlation pearson --projection all --heatmap-kmeans 6
```

## Parameter Reference

### `kite count`

| Short | Long | Meaning | Default | How To Use It | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-k` | `--kmer` | k-mer length | required | Choose the biological resolution of counting. | Changes both the biological meaning and the internal representation. Larger `k` usually increases specificity and can increase runtime or memory use. |
| `-i` | `--input` | input FASTQ / FASTQ.GZ file | required | Point to the sequencing file you want to count. | Larger or compressed inputs increase I/O cost. Does not change the mathematical result beyond changing the dataset itself. |
| `-o` | `--output` | output `.kite` path | required | Choose where the database should be written. | Output path and storage speed affect runtime, not the count values. |
| none | `--partitions` | number of partitions | `256` | Increase when exploring higher parallel partitioning; decrease if overhead dominates. | Mainly affects speed, memory balance, and merge behavior. It should not change final counts. |
| none | `--threads` | total count-side thread budget | `8` | Match available CPU cores. Kite splits this budget across decompression and counting stages for compressed input. | Directly affects runtime and the internal pipeline split. It should not change final counts. |
| none | `--chunk-kmers` | chunk size before sort/merge flushing | `1000000` | Increase for fewer flushes and larger working batches; decrease to reduce peak memory. | Mostly affects speed and memory footprint. It should not change final counts. |
| none | `--max-memory-gb` | optional memory budget for adaptive buffering | auto | Set a larger value on big-memory servers to let `count` allocate deeper queues, more preallocated block buffers, and more aggressive ingestion settings. | Changes buffering strategy and throughput, not final counts. |
| none | `--canonical` | whether to canonicalize reverse complements | `true` | Use `true` for strand-invariant counting, `false` for orientation-sensitive counting. | Changes the meaning of the result. When `true`, reverse-complement pairs are merged. |
| `-p` | `--progress` | show lightweight progress output | `false` | Enable when you want live feedback during long runs. | Slightly increases output overhead, but is designed to stay lightweight. Does not change results. |
| none | `--baseline` | use the slower reference pipeline | `false` | Use only for debugging, validation, or algorithm comparison. | Usually slower. Intended to preserve the same result semantics as the optimized path. |
| none | `--mode` | write mode | `fast` | Choose `fast`, `balanced`, or `compact` depending on whether you prefer speed or smaller database size. | Strongly affects write-time speed and file size. It should not change counts. |
| none | `--zstd-level` | explicit Zstandard compression level | mode-dependent | Override the automatic compression level when you want more control. | Higher levels usually reduce file size and increase CPU time. It should not change counts. |

### `kite dump`

| Short | Long | Meaning | Default | How To Use It | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | input `.kite` database | required | Choose the database to export. | Input size and mode affect runtime. |
| `-o` | `--output` | output TSV path | required | Choose where the plain-text table should be written. | Output disk speed affects runtime, not the values. |
| `-t` | `--threads` | dump worker thread count | `8` | Increase when enough CPU and I/O bandwidth exist. | Affects dump speed. Should not change the table contents. Row order is deterministic for a given file, but may differ from other tools. |
| `-p` | `--progress` | show lightweight progress spinner | `false` | Enable when you want visual feedback during dump. | Slight output overhead, no result change. |

### `kite info`

| Short | Long | Meaning | Default | How To Use It | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | input `.kite` database | required | Choose the file you want to inspect. | Runtime scales with file size because summary statistics may require a full scan. Does not change any stored result. |
| `-p` | `--progress` | show lightweight progress spinner | `false` | Enable when you want visual feedback during metadata/statistics scanning. | Slight output overhead, no result change. |

### `kite query`

| Short | Long | Meaning | Default | How To Use It | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | input `.kite` database | required | Choose the database to query. | Affects lookup path only. |
| none | `--kmer` | query sequence | required | Provide one k-mer string whose length must equal the database `k`. | Changes which value is looked up. If the database is canonical, the query is canonicalized internally before lookup. |
| `-p` | `--progress` | show lightweight progress spinner | `false` | Enable when you want visual feedback during lookup. | Slight output overhead, no result change. |

### `kite histo`

| Short | Long | Meaning | Default | How To Use It | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | input `.kite` database | required | Choose the file to summarize. | Requires a database scan. Does not change stored counts. |
| `-o` | `--output` | output histogram TSV | required | Choose where the histogram table should be written. | Output path affects runtime, not the histogram values. |
| `-p` | `--progress` | show lightweight progress spinner | `false` | Enable when you want visual feedback during histogram generation. | Slight output overhead, no result change. |

### `kite profile`

| Short | Long | Meaning | Default | How To Use It | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | one or more input `.kite` files | required | Pass all sample databases after `-i`. They must currently be `u64` databases and share the same `k`. | More inputs increase runtime and output size. Changes the resulting matrix because each added sample adds a column and may expand the union of k-mers. |
| `-o` | `--output` | output profile path | required | Choose the path of the profile matrix. | Output location affects runtime, not the matrix values. |
| none | `--sample-name` | optional sample labels | none | Provide labels in the same order as inputs when you want custom column names. | Changes header labels only, not the numeric values. |
| none | `--format` | output format | `tsv` | Choose `tsv`, `csv`, or `txt` depending on your downstream parser. | Changes separator style, not the underlying numeric values. |
| `-p` | `--progress` | show lightweight progress spinner | `false` | Enable when you want visual feedback during multi-sample table generation. | Slight output overhead, no result change. |

### `kite merge`

| Short | Long | Meaning | Default | How To Use It | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | two or more input `.kite` files | required | Pass compatible databases after `-i`. They must share the same `k`, the same internal storage mode, and the same canonical setting. | Changes which databases are merged. Inputs determine the final counts. |
| `-o` | `--output` | output merged `.kite` path | required | Choose where the merged database should be written. | Output location affects runtime, not merged count semantics. |
| none | `--threads` | worker thread count for output compression | `8` | Tune when benchmarking merged-file writing or compressing large outputs. | Affects write-time speed only. |
| none | `--mode` | write mode | `fast` | Choose `fast`, `balanced`, or `compact` for the merged file. | Changes write-time speed and output size, not merged counts. |
| none | `--zstd-level` | explicit Zstandard compression level | mode-dependent | Override the automatic compression level when you need tighter control over output size. | Higher levels usually reduce file size and increase CPU time. |
| `-p` | `--progress` | show lightweight progress spinner | `false` | Enable when you want live feedback during merge. | Slight output overhead, no result change. |

### `kite sketch`

| Short | Long | Meaning | Default | How To Use It | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | two or more input `.kite` files | required | Pass all sample databases after `-i`. Inputs currently need to be `u64` databases and share the same `k`. | More inputs increase runtime and output size. |
| none | `--sample-name` | optional sample labels | none | Provide labels in the same order as inputs. | Changes output labeling only, not the numeric analysis. |
| none | `--sample-group` | optional sample groups | none | Provide group names in the same order as inputs when you want the generated R scripts to color samples by group. | Changes generated metadata and plot grouping, not the raw numeric tables. |
| `-o` | `--output-dir` | output directory | `sketch_out` | Choose where the tables and R scripts will be written. | Output location affects runtime, not the computed values. |
| none | `--correlation` | sample-to-sample association metric | `pearson` | Choose `pearson` or `cosine` depending on how you want to compare the sample columns. | Changes the numeric values in the correlation matrix, but not the raw count matrix. |
| none | `--projection` | projection table set | `all` | Choose `pca`, `tsne`, `umap`, or `all`. | Changes which projection score tables and plotting scripts are written. |
| none | `--heatmap-kmeans` | number of row clusters in the clustered heatmap table | `6` | Tune how many k-mer clusters Kite assigns before writing the heatmap table. | Changes the final clustered heatmap table and its plot. |
| none | `--cluster-samples` | cluster sample columns before writing the heatmap table | `true` | Set to `false` when you want to preserve the input sample order in the heatmap outputs. | Changes the heatmap table column order and the resulting plot. |
| `-p` | `--progress` | show lightweight progress spinner | `false` | Enable when you want visual feedback during multi-sample sketch preparation. | Slight output overhead, no result change. |

### `kite citation`

| Short | Long | Meaning | Default | How To Use It | Effect On Speed And Result |
|---|---|---|---|---|---|
| none | none | no parameters | not applicable | Run the command directly. | Only prints version and citation text. |

## Documentation

- Detailed command guide: [doc/usage.md](doc/usage.md)
- Architecture and learning guide: [doc/learning.md](doc/learning.md)
- Structured benchmark and validation notes: [doc/benchmark.md](doc/benchmark.md)
- AI-oriented operational guide: [AGENTS.md](AGENTS.md)

If you are new to the project and want an AI system to help you use or extend Kite, a practical shortcut is to feed [AGENTS.md](AGENTS.md) to the AI first. That file is written specifically to help automated tools understand the project structure, constraints, workflows, and expected command usage.

## Core Concepts

### What is a k-mer?

A k-mer is a substring of length `k` extracted from a biological sequence. If a read has length `L`, then it contributes:

```text
max(L - k + 1, 0)
```

k-mers.

### What is a canonical k-mer?

For a sequence `x`, let `rc(x)` be its reverse complement. When canonical mode is enabled, Kite stores:

```text
canonical(x) = min(x, rc(x))
```

under lexicographic or numeric encoding order. This merges reverse-complement pairs into one count.

### What is direct partitioning?

In the current optimized `count` path, each encoded k-mer is sent directly to one worker-local partition based on its encoded key. This removes extra hot-path bookkeeping and keeps the main counting loop focused on:

- rolling exact k-mer generation
- direct partition routing
- integer sorting
- local count compaction
- final partition-wise merge

### What is a `.kite` file?

A `.kite` file is Kite's binary database format. It stores:

- a header
- format flags
- k-mer representation mode
- the number of entries
- metadata
- the compressed payload

The exact format definition is implemented in `src/utils/kite.rs`.

## Current Scope And Limitations

- `count`, `dump`, `info`, `query`, `histo`, and `merge` support all current representation modes.
- `merge` currently requires all inputs to have the same `k`, the same internal representation mode, and the same canonical semantics.
- `profile` currently operates on `u64` databases, which means it is intended for `k <= 32`.
- `sketch` currently operates on `u64` databases, which means it is also intended for `k <= 32`.
- `profile` writes exact integer counts, not normalized frequencies.
- Input support is currently focused on FASTQ and FASTQ.GZ in the actively optimized path.
- For optimized `count`, `.fastq.gz` input now uses a block-driven ingestion path. Ordinary gzip is decompressed into large byte blocks, aligned to FASTQ record boundaries, and then parsed directly inside count workers. BGZF is detected automatically and routed to a libdeflater-backed parallel BGZF block pipeline before the same record-aligned worker path.
- `sketch` writes the exact profile table, the correlation matrix, exact PCA/t-SNE/UMAP score tables, and a clustered heatmap table directly in Rust. The generated R scripts only read those tables and plot them.
- `sketch` also writes `sketch_sample_metadata.tsv`, which can be edited to change display labels and sample groups without rewriting the main sketch tables. Group colors are defined inside the generated R scripts.

## Project Layout

```text
src/
  main.rs          CLI dispatcher
  cli.rs           Clap argument definitions
  count/           Counting pipeline
  dump/            Dump and decode pipeline
  info/            Metadata and statistics reporting
  query/           Exact k-mer lookup
  histo/           Count histogram generation
  merge/           Exact database-by-database count summation
  profile/         Multi-sample raw-count union table
  sketch/          Exact multi-sample correlation/projection/heatmap table generation
  citation/        Citation text
  utils/           Shared file-format and formatting utilities
```

## Design Philosophy

- Keep subcommands independent.
- Share only what is truly cross-command.
- Keep the file format explicit.
- Favor predictable outputs over hidden automation.
- Optimize the common path, but preserve future room for specialization.

## Example Workflow

```bash
kite count -k 31 -i sample.fastq.gz -o sample.kite --threads 8 --mode fast
kite info -i sample.kite
kite dump -i sample.kite -o sample.tsv -t 8
kite histo -i sample.kite -o sample.histo.tsv
kite merge -i sample_a.kite sample_b.kite sample_c.kite -o merged.kite --threads 8 --mode fast
```

For multi-sample work:

```bash
kite profile -i sample1.kite sample2.kite --sample-name s1 s2 -o profile.tsv --format tsv
kite sketch -i sample1.kite sample2.kite --sample-name s1 s2 --sample-group g1 g2 -o sketch_out --correlation pearson --projection pca --heatmap-kmeans 6
```

## Citation

```bash
kite citation
```

## License

Kite is distributed under a custom academic and non-commercial license.

- Academic and non-profit use is free of charge.
- Commercial use requires prior written authorization.
- Attribution to Kite and its contributors is mandatory for all use.

See [LICENSE](LICENSE) for the full terms.
