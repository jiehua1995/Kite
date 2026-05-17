# Kite

<p>
	<img src="doc/logo.png" alt="logo" width="100" align="left">
</p> 
**Kite** is a high-performance Rust toolkit for exact k-mer counting, storage, inspection, and downstream multi-sample analysis.

Its design goal is simple:

- make `count` fast
- make `dump` fast
- keep the `.kite` format explicit and reusable
- provide built-in commands for inspection, merging, profiling, and Quarto/R-based reporting

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
| `kite analyze` | Build an exact multi-sample count matrix, write summary tables, and generate a Quarto report | `.kite`... | output directory |
| `kite citation` | Print version and citation information | none | stdout |
| `kite fly` | Run count + analyze in one command | FASTQ... | output directory |

## CLI Design Principles

| Principle | Meaning |
|---|---|
| Minimal inspection commands | `info` and `query` print directly to stdout and do not expose thread, memory, output, or progress options; `info` can also edit header-only filter markers. |
| Multi-file input style | Commands that accept multiple inputs use one `-i/--input` followed by all files, for example `-i sample1.kite sample2.kite`. |
| Consistent sample labeling | `histo`, `profile`, `analyze`, and `fly` use `--label sample1 sample2 ...`; label count must match input count. |
| Strict memory syntax | `count` and `fly` use `--memory`, with values like `512M`, `64G`, or `1T`. The suffix must be `M`, `G`, or `T`. |
| Simple thread convention | Commands that expose thread control use `-t, --threads`. |
| Lazy count filters | `count --min/--max` stores filter markers in the `.kite` header without deleting raw counts. Downstream commands apply those markers when exporting or analyzing. |
| Quarto/R reporting | `analyze` writes TSV tables and `analysis_report.qmd`; render the report with Quarto/R to generate figures. |
| One-shot workflow | `fly` forwards the core `count` and `analyze` options so a one-command workflow has the same semantics as running the stages manually. |

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
  --buffer 32 \
  --min 2 \
  --max 1000000 \
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
./target/release/kite info -i sample.kite
```

Edit the lazy count-filter markers stored in a database header:

```bash
./target/release/kite info -i sample.kite --set-min 2 --set-max 1000000
./target/release/kite info -i sample.kite --clear-min --clear-max
```

Query one k-mer:

```bash
./target/release/kite query -i sample.kite -k ACGTACGTACGTACGTACGTA
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
  --cluster-samples true
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
  --min 2 \
  --max 1000000 \
  --buffer 32 \
  --correlation all \
  --embedding all \
  --heatmap-kmeans 6 \
  --heatmap-seed 1 \
  --cluster-samples true
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
| `--canonical` | `false` | Whether to canonicalize k-mers | Accepts `true` or `false`; changes result semantics |
| `--min` | none | Header minimum count marker | Raw counts are retained; downstream commands hide counts below this marker |
| `--max` | none | Header maximum count marker | Raw counts are retained; downstream commands hide counts above this marker |
| `--mode` | `fast` | Output compression mode | `fast` writes DVLE/raw only; `balanced` and `compact` additionally try zstd when smaller |
| `--buffer` | `32` | Target staging buffer size in MB | Independent of `--mode`; larger values may reduce flushes but use more memory; planner falls back to smaller buffers when memory is tight |
| `-p, --progress` | `false` | Show progress | Useful for long jobs |
| `--test-mode` | `false` | Print detailed runtime statistics | Useful for benchmarking and tuning |

`--min` and `--max` are lazy filters. They are written into the `.kite` header and do not slow down `count` by rewriting the database. `dump`, `histo`, `profile`, `analyze`, and `merge` read these markers and exclude marked-out k-mers from their outputs. `query` still reports the raw stored count and tells you whether that k-mer is excluded by the current header markers. `info` can edit the markers in place without touching the stored count data.

### `kite dump`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | Input `.kite` file | One database per run |
| `-o, --output` | required | Output TSV path | Use `-` for stdout |
| `-t, --threads` | `0` | Dump worker thread count | `0` means auto |
| `-p, --progress` | `false` | Show progress | Useful for large files |
| `--test-mode` | `false` | Print detailed runtime statistics | Useful for validation and benchmarking |

`dump` applies the input file's header `Min Count` and `Max Count` markers while writing TSV. Use `kite info --set-min/--set-max` to change those markers without recounting.

### `kite info`

| Argument | Description |
|---|---|
| `-i, --input` | Input `.kite` file |
| `--set-min` | Set the header minimum count marker |
| `--set-max` | Set the header maximum count marker |
| `--clear-min` | Clear the header minimum count marker |
| `--clear-max` | Clear the header maximum count marker |

`info` prints a structured summary directly to stdout. With edit options it updates only the fixed-size header metadata; the count data area is not rewritten. All downstream commands treat these markers as exclusion rules.

### `kite query`

| Argument | Description |
|---|---|
| `-i, --input` | Input `.kite` file |
| `-k, --kmer` | Exact query sequence |

`query` prints the database path, query sequence, target partition, exact raw count, header filter markers, and whether the queried k-mer is included in downstream output/analysis.

### `kite histo`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | One or more `.kite` files | Use one `-i` followed by all files; one histogram column per input |
| `--label` | inferred from filenames | Sample labels | Must match the number of inputs |
| `-o, --output` | required | Histogram TSV output | First column is `count` |
| `-p, --progress` | `false` | Show progress | More useful for many inputs |

`histo` applies each input file's header count filters before building count-frequency bins.

### `kite merge`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | Two or more `.kite` files | Use one `-i` followed by all files; inputs must be compatible |
| `-o, --output` | required | Output merged `.kite` file | Exact summation, no sketching |
| `-p, --progress` | `false` | Show progress | Also prints compatibility summaries |

Compatibility requirements:

- same `k`
- same `canonical` setting
- same `partitions`
- same `key_width`

`merge` applies each input file's header filters before summing counts. If every input has the same `Min Count` or `Max Count` marker, that marker is copied to the merged output header; otherwise the merged output leaves the differing marker unset.

### `kite profile`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | One or more `.kite` files | Use one `-i` followed by all files; uses the union of observed k-mers |
| `--label` | inferred from filenames | Sample labels | Must match the number of inputs |
| `-o, --output` | required | Output TSV path | First column is `kmer` |
| `-p, --progress` | `false` | Show progress | Useful for multi-sample runs |

`profile` applies each input file's header count filters before constructing the union-by-sample matrix.

### `kite analyze`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | One or more `.kite` files | Use one `-i` followed by all files; builds an exact multi-sample matrix |
| `--label` | inferred from filenames | Sample labels | Must match the number of inputs |
| `-o, --output-dir` | required | Output directory | TSV tables and `analysis_report.qmd` are written here |
| `--correlation` | `pearson` | Correlation or distance mode | `all`, `pearson`, `spearman`, `cosine`, `euclidean`, `manhattan`, or `mahalanobis` |
| `--embedding` | `pca` | Embedding mode | `all`, `pca`, `tsne`, `umap`, or `pacmap` (`pcamap` is accepted as an alias) |
| `--heatmap-kmeans` | `6` | Number of k-means row clusters for heatmap generation | Applied to top variable k-mers |
| `--heatmap-seed` | `1` | Random seed for heatmap k-means and stochastic embeddings | Also used for `t-SNE`, `UMAP`, and `PaCMAP` |
| `--cluster-samples` | `true` | Cluster sample columns before writing heatmap outputs | Accepts `true` or `false` |
| `-p, --progress` | `false` | Show progress | Recommended for larger runs |

`analyze` applies each input file's header count filters before constructing the exact matrix. It writes exact tables in Rust and generates `analysis_report.qmd`. Render the Quarto document when you want R/ggplot2 and ComplexHeatmap figures.

### `kite analyze` Output Files

| File | Meaning |
|---|---|
| `exact_count_matrix.tsv` | Exact multi-sample raw-count matrix |
| `sample_summary.tsv` | Per-sample total raw counts and nonzero k-mer counts |
| `correlation_pearson.tsv` | Written when `--correlation all` or `pearson` is selected |
| `correlation_spearman.tsv` | Written when `--correlation all` or `spearman` is selected |
| `correlation_cosine.tsv` | Written when `--correlation all` or `cosine` is selected |
| `correlation_euclidean.tsv` | Written when `--correlation all` or `euclidean` is selected |
| `correlation_manhattan.tsv` | Written when `--correlation all` or `manhattan` is selected |
| `correlation_mahalanobis.tsv` | Written when `--correlation all` or `mahalanobis` is selected; uses a streaming diagonal covariance approximation |
| `embedding_pca.tsv` | Written when `--embedding all` or `pca` is selected |
| `embedding_tsne.tsv` | Written when `--embedding all` or `tsne` is selected |
| `embedding_umap.tsv` | Written when `--embedding all` or `umap` is selected |
| `embedding_pacmap.tsv` | Written when `--embedding all` or `pacmap` is selected |
| `heatmap_matrix.tsv` | Row-scaled heatmap matrix with cluster assignments |
| `heatmap_plot_matrix.tsv` | Smaller heatmap matrix used by the Quarto report |
| `heatmap_clusters.tsv` | Heatmap cluster centroids |
| `heatmap_samples.tsv` | Heatmap sample order |
| `analysis_report.qmd` | Quarto/R report for generating figures from the TSV tables |

R-side notes:

- Kite itself does not render figures.
- The generated `analysis_report.qmd` declares and installs the R packages it needs when rendered.
- Kite still writes all TSV tables even if R or Quarto is not installed.

### `kite citation`

| Option | Default | Description |
|---|---|---|
| `-o, --output` | `-` | Output path; `-` means stdout |
| `--format` | `text` | `text` or `bibtex` |

### `kite fly`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | One or more FASTQ inputs | Use one `-i` followed by all files; one `.kite` database is produced per input |
| `--label` | inferred from filenames | Sample labels | Must match the number of inputs |
| `-o, --output-dir` | required | Output directory | Creates `databases/` and `analysis/` |
| `-k, --kmer` | `31` | k-mer length used by count | |
| `--canonical` | `false` | Whether to canonicalize k-mers in internal count jobs | Accepts `true` or `false` |
| `--min` | none | Header minimum count marker forwarded to count | Downstream analyze in the same `fly` run excludes lower counts |
| `--max` | none | Header maximum count marker forwarded to count | Downstream analyze in the same `fly` run excludes higher counts |
| `--mode` | `fast` | Count compression mode | Same values and semantics as `kite count --mode` |
| `-t, --threads` | `0` | Thread budget forwarded to count | `0` means auto |
| `--memory` | auto-detected | Memory budget forwarded to count | Must use `M`, `G`, or `T` suffixes |
| `--partitions` | `0` | Count partition count | `0` means auto |
| `--chunk-kmers` | `8000000` | Count chunk size | Same semantics as `kite count` |
| `--buffer` | `32` | Count staging buffer in MB | Same semantics as `kite count` |
| `--correlation` | `pearson` | Correlation or distance mode passed to analyze | |
| `--embedding` | `pca` | Embedding mode passed to analyze | |
| `--heatmap-kmeans` | `6` | Heatmap row cluster count passed to analyze | |
| `--heatmap-seed` | `1` | Random seed passed to analyze | |
| `--cluster-samples` | `true` | Sample clustering flag passed to analyze | |
| `-p, --progress` | `false` | Show progress | Forwarded to internal count and analyze steps |

`fly` runs:

1. `count` on each input sequencing file
2. `analyze` on the resulting `.kite` databases
3. `analysis_report.qmd` generation for downstream figures

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
