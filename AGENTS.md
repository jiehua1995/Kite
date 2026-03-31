# AGENTS Guide For Kite

## Purpose

This file is written for AI tools and automated assistants that need to understand, use, inspect, benchmark, or modify Kite.

If you are an AI system, read this file before making changes or giving workflow guidance.

## What Kite Is

Kite is a Rust command-line toolkit for:

- k-mer counting
- `.kite` database generation
- exact TSV dumping
- metadata inspection
- single-k-mer querying
- histogram generation
- exact database merging
- multi-sample profiling
- sketch-style exploratory analysis

Main binary:

```text
target/release/kite.exe
```

## Most Important Architectural Rule

Kite is intentionally organized by subcommand.

That means:

- `src/count/` owns counting logic
- `src/dump/` owns dumping and decode-side logic
- `src/profile/` owns profile logic
- `src/merge/` owns exact database merging logic
- `src/sketch/` owns sketch logic

Only truly shared concepts belong in `src/utils/`, especially:

- `.kite` format definitions in `src/utils/kite.rs`
- generic printing helpers in `src/utils/fmt.rs`

Do not force command-local code back into a shared module unless the sharing is clearly beneficial and does not reduce command-specific performance freedom.

## File Layout

```text
src/
  main.rs
  cli.rs
  count/
  dump/
  info/
  query/
  histo/
  merge/
  profile/
  sketch/
  citation/
  utils/
```

## Command Summary

### `kite count`

Input:

- FASTQ / FASTQ.GZ

Output:

- `.kite`

Notes:

- primary performance-critical command
- supports `u64`, `u128`, and packed modes automatically based on `k`
- optimized `.fastq.gz` ingestion uses block decompression, record-boundary alignment, and direct worker-side FASTQ parsing; BGZF input is detected and routed to a libdeflater-backed parallel BGZF block pipeline

### `kite dump`

Input:

- `.kite`

Output:

- TSV with `kmer<TAB>count`

Notes:

- also performance-critical
- raw output order may differ across tools or workflows
- exact comparisons must sort by k-mer first

### `kite info`

Output:

- metadata and summary statistics on stdout

### `kite query`

Output:

- the count of one exact queried k-mer

### `kite histo`

Output:

- histogram TSV

### `kite profile`

Output:

- raw-count union table across samples

Current limitation:

- currently intended for `u64` databases, therefore effectively `k <= 32`

### `kite merge`

Input:

- two or more `.kite` files

Output:

- one merged `.kite`

Notes:

- counts are summed exactly for identical k-mers
- all inputs must share the same `k`, internal mode, and canonical semantics

### `kite sketch`

Output:

- output directory with exact count-table, correlation-table, projection-score tables, clustered heatmap table, and plotting-only R scripts

Current limitation:

- currently intended for `u64` databases, therefore effectively `k <= 32`

### `kite citation`

Output:

- version and citation text

## CLI Parameter Map

### `kite count`

| Short | Long | Meaning | Default | Usage Guidance | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-k` | `--kmer` | k-mer length | required | Must be provided. Drives both biology and internal representation mode. | Changes the result domain and the selected storage path. |
| `-i` | `--input` | input FASTQ / FASTQ.GZ | required | Point to the sequencing file. | Input size and compression affect runtime only through I/O and parsing cost. |
| `-o` | `--output` | output `.kite` path | required | Choose the destination database path. | Output device speed affects runtime, not count values. |
| none | `--partitions` | partition count | `256` | Tune only when doing performance work. | Changes partition balance and merge overhead. Should not change final counts. |
| none | `--threads` | total count-side thread budget | `8` | Match available CPU cores. Kite may split this budget across decompression and counting workers for compressed input. | Directly affects runtime and internal pipeline layout. Should not change final counts. |
| none | `--chunk-kmers` | chunk flush threshold | `1000000` | Tune for memory-versus-flush trade-offs. | Changes memory behavior and speed, not final counts. |
| none | `--max-memory-gb` | optional memory budget for adaptive buffering | auto | Increase on large-memory servers when you want deeper count-side queues and more aggressive ingestion buffers. | Changes buffering strategy and throughput, not final counts. |
| none | `--canonical` | canonical mode | `true` | Use `false` only when strand-sensitive or non-canonical comparisons are required. | Changes the meaning of the count table by merging or not merging reverse complements. |
| `-p` | `--progress` | progress display | `false` | Enable for visibility during long runs. | Slight output overhead, no result change. |
| none | `--baseline` | reference pipeline | `false` | Use for debugging or algorithm comparison, not routine benchmarking. | Usually slower; intended to preserve result semantics. |
| none | `--mode` | write mode | `fast` | Choose `fast`, `balanced`, or `compact`. | Changes database writing behavior, runtime, and file size, not count values. |
| none | `--zstd-level` | compression level override | mode-dependent | Use only when explicit compression control is needed. | Higher levels usually increase CPU time and reduce file size. |

### `kite dump`

| Short | Long | Meaning | Default | Usage Guidance | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | input `.kite` database | required | Choose the database to export. | Input size and representation affect runtime. |
| `-o` | `--output` | output TSV | required | Choose the exported table path. | Output storage speed affects runtime, not values. |
| `-t` | `--threads` | dump worker threads | `8` | Tune when benchmarking dump performance. | Changes dump speed only. Exact row order may differ from external tools, so comparisons must sort first. |
| `-p` | `--progress` | progress spinner | `false` | Enable when you want visible progress during dump. | Slight output overhead, no result change. |

### `kite info`

| Short | Long | Meaning | Default | Usage Guidance | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | input `.kite` database | required | Choose the file to inspect. | May require a full scan for summary statistics. Does not modify the file. |
| `-p` | `--progress` | progress spinner | `false` | Enable when you want visible progress during metadata/statistics scanning. | Slight output overhead, no result change. |

### `kite query`

| Short | Long | Meaning | Default | Usage Guidance | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | input `.kite` database | required | Choose the database to query. | Affects lookup source only. |
| none | `--kmer` | query sequence | required | Length must equal the database `k`. | Changes which count is returned. Canonical databases canonicalize the query internally. |
| `-p` | `--progress` | progress spinner | `false` | Enable when you want visible progress during lookup. | Slight output overhead, no result change. |

### `kite histo`

| Short | Long | Meaning | Default | Usage Guidance | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | input `.kite` database | required | Choose the file to summarize. | Requires a scan, but does not alter stored counts. |
| `-o` | `--output` | output histogram TSV | required | Choose the histogram file path. | Output location affects runtime, not histogram values. |
| `-p` | `--progress` | progress spinner | `false` | Enable when you want visible progress during histogram generation. | Slight output overhead, no result change. |

### `kite profile`

| Short | Long | Meaning | Default | Usage Guidance | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | one or more input `.kite` files | required | Provide all sample databases after `-i`; they must share the same `k` and currently be `u64` databases. | More inputs increase runtime and expand the union table. |
| `-o` | `--output` | output path | required | Choose the profile matrix destination. | Output location affects runtime, not numeric values. |
| none | `--sample-name` | optional sample labels | none | Supply names in the same order as inputs when you want stable column names. | Changes labels only. |
| none | `--format` | output format | `tsv` | Choose `tsv`, `csv`, or `txt` based on downstream tooling. | Changes separator style, not the underlying values. |
| `-p` | `--progress` | progress spinner | `false` | Enable when you want visible progress during matrix generation. | Slight output overhead, no result change. |

### `kite merge`

| Short | Long | Meaning | Default | Usage Guidance | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | two or more input `.kite` files | required | Provide compatible inputs after `-i`; they must share the same `k`, the same internal mode, and the same canonical setting. | Changes which databases are added together. |
| `-o` | `--output` | output merged `.kite` path | required | Choose the destination merged database. | Output location affects runtime, not merged values. |
| none | `--threads` | worker threads for merged-file compression | `8` | Tune only if you are benchmarking large merged outputs. | Affects write speed only. |
| none | `--mode` | write mode for the merged file | `fast` | Choose `fast`, `balanced`, or `compact` depending on whether you prefer write speed or output size. | Changes output compression behavior, not merged counts. |
| none | `--zstd-level` | compression level override | mode-dependent | Use only when explicit compression control is needed. | Higher levels usually increase CPU time and reduce file size. |
| `-p` | `--progress` | progress spinner | `false` | Enable when you want visible progress during merge. | Slight output overhead, no result change. |

### `kite sketch`

| Short | Long | Meaning | Default | Usage Guidance | Effect On Speed And Result |
|---|---|---|---|---|---|
| `-i` | `--input` | two or more input `.kite` files | required | Use when sketch should build the internal profile-like raw-count matrix directly from `.kite` inputs. | More inputs increase runtime and output size. |
| none | `--sample-name` | optional labels | none | Supply names in the same order as inputs when you want stable column labels. | Changes labels only. |
| none | `--sample-group` | optional sample groups | none | Supply groups in the same order as inputs when the generated sketch metadata and plots should carry sample groups. | Changes generated metadata and plot grouping, not the raw numeric tables. |
| `-o` | `--output-dir` | output directory | `sketch_out` | Choose where all sketch artifacts should be written. | Output location affects runtime, not values. |
| none | `--correlation` | sample-to-sample association metric | `pearson` | Choose `pearson` or `cosine` based on how you want to compare sample columns. | Changes the numeric values in the correlation matrix, not the raw count matrix. |
| none | `--projection` | projection table selection | `all` | Choose `pca`, `tsne`, `umap`, or `all`. | Changes which projection score tables and plotting scripts are written. |
| none | `--heatmap-kmeans` | row cluster count for the clustered heatmap table | `6` | Tune downstream k-mer grouping behavior. | Changes the clustered heatmap table and the resulting plot. |
| none | `--cluster-samples` | cluster sample columns before writing the heatmap table | `true` | Set `false` to preserve input sample order in the heatmap outputs. | Changes the heatmap table column order and plot layout. |
| `-p` | `--progress` | progress spinner | `false` | Enable when you want visible progress during sketch preparation. | Slight output overhead, no result change. |

### `kite citation`

| Short | Long | Meaning | Default | Usage Guidance | Effect On Speed And Result |
|---|---|---|---|---|---|
| none | none | no parameters | not applicable | Run directly. | Only prints version and citation text. |

## Format Contract

Shared `.kite` format logic lives in:

```text
src/utils/kite.rs
```

If you modify the format, you must keep these in agreement:

- count-side writer
- dump-side reader
- info
- query
- histo
- merge
- any downstream helper that reads `.kite`

Treat `src/utils/kite.rs` as the source of truth for format semantics.

## License Summary

The repository uses a custom license.

- Academic use is free.
- Non-profit use is free.
- Commercial use requires prior written authorization from the copyright holders.
- Attribution to Kite and its contributors is mandatory for all use.

If an AI tool prepares release notes, packaging guidance, or usage instructions, it should not describe Kite as MIT-licensed or generally open for unrestricted commercial use.

## Exact Comparison Rule

Raw dump order may differ even when the mathematical result is identical.

Correct method:

1. sort both TSV files by the first column
2. compare row count
3. compare the first column
4. compare the second column

Do not assume line-by-line equality before sorting.

## Typical Commands

### Build

```bash
cargo build --release
```

### Help

```bash
target/release/kite.exe --help
```

### Count

```bash
target/release/kite.exe count -k 31 -i test/NSC_capture_R1_rep1.1M.fastq.gz -o sample.kite --threads 8 --mode fast
```

### Dump

```bash
target/release/kite.exe dump -i sample.kite -o sample.tsv -t 8
```

### Info

```bash
target/release/kite.exe info -i sample.kite
```

### Query

```bash
target/release/kite.exe query -i sample.kite --kmer ACGTACGTACGTACGTACGTACGTACGTACG
```

### Histogram

```bash
target/release/kite.exe histo -i sample.kite -o sample.histo.tsv
```

### Profile

```bash
target/release/kite.exe profile -i sample1.kite sample2.kite -o profile.tsv --format tsv
```

### Merge

```bash
target/release/kite.exe merge -i sample1.kite sample2.kite sample3.kite -o merged.kite --threads 8 --mode fast
```

### Sketch

```bash
target/release/kite.exe sketch -i sample1.kite sample2.kite --sample-name s1 s2 --sample-group g1 g2 -o sketch_out --correlation pearson --projection pca --heatmap-kmeans 6
```

## Practical AI Guidance

When working in this repository:

- inspect the command-local directory first
- verify CLI syntax in `src/cli.rs`
- verify format assumptions in `src/utils/kite.rs`
- distinguish exact outputs from hashed or projected outputs
- document limitations clearly

Do not imply support that the current code does not provide.

## Quick Mental Model

```text
count builds a .kite database
dump turns .kite into exact TSV
info/query/histo inspect one .kite
merge combines multiple .kite files into one exact summed database
profile/sketch combine multiple .kite files
utils/kite.rs defines the shared file-format contract
```
