
  <img src="doc/logo.png" alt="Kite Logo" width="120" align="left">
  
  # Kite
  
  **A high-performance K-mer counter**

  [![Rust](https://img.shields.io/badge/rust-1.78+-orange.svg)](https://www.rust-lang.org)


**Kite** (K-mer indexing and throughput engine) is a high-performance Rust toolkit designed for exact k-mer counting, storage, inspection, and downstream multi-sample analysis. It leverages Rust's memory safety and performance to provide a robust engine for bioinformatics workflows.

Its design goal is simple:

- make `count` fast
- make `dump` fast
- keep the `.kite` format explicit and reusable
- provide built-in commands for inspection, merging, profiling, and plotting

Kite is organized as a command-oriented codebase. Each subcommand lives in its own `src/<command>/` directory, while the `.kite` format and shared helpers are reused across commands.

## Command Overview

| Command | Purpose | Typical input | Typical output |
|---|---|---|---|
| `kite count` | Count k-mers from FASTQ input and write a `.kite` database | `FASTQ`, `FASTQ.GZ`, `BGZF FASTQ` | `.kite` |
| `kite dump` | Export exact k-mer counts from a `.kite` database | `.kite` | `TSV` |
| `kite info` | Print structured metadata and summary statistics for one database | `.kite` | stdout |
| `kite query` | Look up one exact k-mer count | `.kite` + k-mer | stdout |
| `kite histo` | Write count-frequency histograms for one or more databases | `.kite`... | `.tsv` |
| `kite merge` | Merge compatible `.kite` databases by exact count summation | `.kite`... | `.kite` |
| `kite profile` | Build a union-by-sample raw-count matrix | `.kite`... | `.tsv` |
| `kite analyze` | Build an exact multi-sample count matrix, write summary tables, and generate plotting assets | `.kite`... | output directory |
| `kite citation` | Print version and citation information | none | stdout |
| `kite fly` | Run count + analyze + plotting in one command | FASTQ... | output directory |

## CLI Design Principles

| Principle | Meaning |
|---|---|
| Minimal inspection commands | `info` and `query` print directly to stdout and do not expose thread, memory, output, or progress options. |
| Consistent sample labeling | `histo`, `profile`, `analyze`, and `fly` use `--label sample1 sample2 ...`; label count must match input count. |
| Strict memory syntax | `count` and `fly` use `--memory`, with values like `512M`, `64G`, or `1T`. The suffix must be `M`, `G`, or `T`. |
| Simple thread convention | Commands that expose thread control use `-t, --threads`. |
| Plotting-first analysis | `analyze` writes `ggplot2`-based R scripts and uses `ggsave()` to save PDF figures when the R environment is available. |
| One-shot workflow | `fly` is intentionally a small, practical wrapper around `count` and `analyze`, not a kitchen-sink interface. |

## Build

```bash
cargo build --release
```

Binary:

```text
target/release/kite
```

## Quick Start

Count 31-mers:

```bash
./target/release/kite count \
  -k 31 \
  -i reads.fastq.gz \
  -o sample.kite \
  -t 48 \
  --memory 64G \
  --buffer 16 \
  -p
```

Dump exact counts:

```bash
./target/release/kite dump \
  -i sample.kite \
  -o sample.tsv \
  -t 48 \
  -p
```

Inspect a database:

```bash
./target/release/kite info sample.kite
```

Query one k-mer:

```bash
./target/release/kite query sample.kite ACGTACGTACGTACGTACGTA
```

Build a histogram across multiple databases:

```bash
./target/release/kite histo \
  -i tumor.kite normal.kite \
  --label tumor normal \
  -o histo.tsv
```

Merge compatible databases:

```bash
./target/release/kite merge \
  -i sample1.kite sample2.kite sample3.kite \
  -o merged.kite
```

Build a union-by-sample matrix:

```bash
./target/release/kite profile \
  -i tumor.kite normal.kite control.kite \
  --label tumor normal control \
  -o profile.tsv
```

Run multi-sample analysis:

```bash
./target/release/kite analyze \
  -i tumor.kite normal.kite control.kite \
  --label tumor normal control \
  -o analysis_out \
  --correlation all \
  --embedding all \
  --heatmap-kmeans 6 \
  --heatmap-seed 1 \
  --cluster-samples true \
  --top-variable-kmers 5000
```

Run the full one-shot workflow:

```bash
./target/release/kite fly \
  -i tumor.fastq.gz normal.fastq.gz control.fastq.gz \
  --label tumor normal control \
  -o fly_out \
  -k 31 \
  -t 48 \
  --memory 64G \
  --correlation all \
  --embedding all \
  --heatmap-kmeans 6 \
  --heatmap-seed 1 \
  --cluster-samples true \
  --top-variable-kmers 5000
```

## Parameter Reference

### `kite count`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-k, --kmer` | `21` | k-mer length | Supported range: `1..128` |
| `-i, --input` | required | Input `FASTQ`, `FASTQ.GZ`, or `BGZF FASTQ` | One input file per run |
| `-o, --output` | required | Output `.kite` path | Should end with `.kite` |
| `-t, --threads` | `0` | Total thread budget | `0` means auto |
| `--partitions` | `0` | Number of partitions | `0` means auto; manual values must be powers of two |
| `--chunk-kmers` | `8000000` | Reader-side target batch size | Affects flushing and memory behavior |
| `--memory` | auto-detected | Total memory budget | Must be written as `512M`, `64G`, or `1T` |
| `--canonical` | `true` | Use canonical k-mers | Changes result semantics |
| `--mode` | `fast` | Output mode | `fast`, `balanced`, or `compact` |
| `--buffer` | `16` | Target staging buffer size in MB | Larger values may reduce flushes but use more memory |
| `-p, --progress` | `false` | Show progress | Useful for long jobs |
| `--test-mode` | `false` | Print detailed runtime statistics | Useful for benchmarking and tuning |

### `kite dump`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | Input `.kite` file | One database per run |
| `-o, --output` | required | Output TSV path | Use `-` for stdout |
| `-t, --threads` | `0` | Dump worker thread count | `0` means auto |
| `-p, --progress` | `false` | Show progress | Useful for large files |
| `--test-mode` | `false` | Print detailed runtime statistics | Useful for validation and benchmarking |

### `kite info`

| Argument | Description |
|---|---|
| `INPUT` | Input `.kite` file |

`info` prints a structured summary directly to stdout.

### `kite query`

| Argument | Description |
|---|---|
| `INPUT` | Input `.kite` file |
| `KMER` | Exact query sequence |

`query` prints the database path, query sequence, target partition, and exact count.

### `kite histo`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | One or more `.kite` files | One histogram column per input |
| `--label` | inferred from filenames | Sample labels | Must match the number of inputs |
| `-o, --output` | required | Histogram TSV output | First column is `count` |
| `-p, --progress` | `false` | Show progress | More useful for many inputs |

### `kite merge`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | Two or more `.kite` files | Inputs must be compatible |
| `-o, --output` | required | Output merged `.kite` file | Exact summation, no sketching |
| `-p, --progress` | `false` | Show progress | Also prints compatibility summaries |

Compatibility requirements:

- same `k`
- same `canonical` setting
- same `partitions`
- same `key_width`

### `kite profile`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | One or more `.kite` files | Uses the union of observed k-mers |
| `--label` | inferred from filenames | Sample labels | Must match the number of inputs |
| `-o, --output` | required | Output TSV path | First column is `kmer` |
| `-p, --progress` | `false` | Show progress | Useful for multi-sample runs |

### `kite analyze`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | One or more `.kite` files | Builds an exact multi-sample matrix |
| `--label` | inferred from filenames | Sample labels | Must match the number of inputs |
| `-o, --output-dir` | required | Output directory | Tables, scripts, and figures are written here |
| `--top-variable-kmers` | `5000` | Number of high-variance k-mers kept for visualization | Affects heatmap and embedding input |
| `--correlation` | `all` | Correlation mode | `all`, `pearson`, or `cosine` |
| `--embedding` | `all` | Embedding mode | `all`, `pca`, `tsne`, or `umap` |
| `--heatmap-kmeans` | `6` | Number of k-means row clusters for heatmap generation | Applied to top variable k-mers |
| `--heatmap-seed` | `1` | Random seed for heatmap k-means and stochastic embeddings | Also used for `t-SNE` and `UMAP` |
| `--cluster-samples` | `true` | Cluster sample columns before writing heatmap outputs | Accepts `true` or `false` |
| `-p, --progress` | `false` | Show progress | Recommended for larger runs |

`analyze` writes exact tables in Rust and then generates a `ggplot2`-based R script that uses `ggsave()` to write PDF figures when the required R packages are available.

### `kite analyze` Output Files

| File | Meaning |
|---|---|
| `exact_count_matrix.tsv` | Exact multi-sample raw-count matrix |
| `sample_summary.tsv` | Per-sample total raw counts and nonzero k-mer counts |
| `top_variable_kmers.tsv` | High-variance k-mer subset |
| `correlation.tsv` | Primary correlation matrix |
| `correlation_pearson.tsv` | Written when `--correlation all` or `pearson` is selected |
| `correlation_cosine.tsv` | Written when `--correlation all` or `cosine` is selected |
| `heatmap_rows.tsv` | Heatmap row order and cluster assignments |
| `heatmap_samples.tsv` | Heatmap sample order |
| `plot_analysis.R` | `ggplot2` plotting script |
| `correlation.pdf` | Correlation heatmap PDF |
| `heatmap.pdf` | Clustered heatmap PDF |
| `pca_scores.tsv` | PCA coordinates |
| `pca.pdf` | PCA PDF |
| `tsne_scores.tsv` | Written if `t-SNE` is requested and `Rtsne` is available |
| `tsne.pdf` | Written if `t-SNE` is requested and `Rtsne` is available |
| `umap_scores.tsv` | Written if `UMAP` is requested and `uwot` is available |
| `umap.pdf` | Written if `UMAP` is requested and `uwot` is available |

R-side notes:

- `ggplot2` is required for figure generation.
- `Rtsne` is required for `t-SNE`.
- `uwot` is required for `UMAP`.
- If these packages are missing, Kite still writes the exact tables and the plotting script.

### `kite citation`

| Option | Default | Description |
|---|---|---|
| `-o, --output` | `-` | Output path; `-` means stdout |
| `--format` | `text` | `text` or `bibtex` |

### `kite fly`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | One or more FASTQ inputs | One `.kite` database is produced per input |
| `--label` | inferred from filenames | Sample labels | Must match the number of inputs |
| `-o, --output-dir` | required | Output directory | Creates `databases/` and `analysis/` |
| `-k, --kmer` | `31` | k-mer length used by count | `fly` uses canonical counting and `fast` mode internally |
| `-t, --threads` | `0` | Thread budget forwarded to count | `0` means auto |
| `--memory` | auto-detected | Memory budget forwarded to count | Must use `M`, `G`, or `T` suffixes |
| `--top-variable-kmers` | `5000` | Number of high-variance k-mers passed to analyze | |
| `--correlation` | `all` | Correlation mode passed to analyze | |
| `--embedding` | `all` | Embedding mode passed to analyze | |
| `--heatmap-kmeans` | `6` | Heatmap row cluster count passed to analyze | |
| `--heatmap-seed` | `1` | Random seed passed to analyze | |
| `--cluster-samples` | `true` | Sample clustering flag passed to analyze | |
| `-p, --progress` | `false` | Show progress | Forwarded to internal count and analyze steps |

`fly` runs:

1. `count` on each input sequencing file
2. `analyze` on the resulting `.kite` databases
3. plotting asset generation for downstream figures

## Installation

Kite is currently intended to be built directly from source with Cargo:

```bash
cargo build --release
```

The resulting binary is:

```text
target/release/kite
```

This keeps installation simple and makes it easy for users to build the exact version they want from source.
