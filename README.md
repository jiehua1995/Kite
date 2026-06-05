# Kite

<p>
	<img src="doc/logo.svg" alt="logo" width="100" align="left">
</p> 
**Kite** is a high-performance Rust toolkit for exact k-mer counting, reusable `.kite` indexes, inspection, and downstream multi-sample analysis.

## Dependencies

- [Rust](https://rust-lang.org/learn/get-started/) toolchain for building Kite
- [Quarto](https://quarto.org/) for rendering `analysis_report.qmd`
- `quarto install tinytex` if you want Quarto to render PDF output


## Command Overview

| Command | Purpose | Typical input | Typical output |
|---|---|---|---|
| `kite count` | Count k-mers from FASTQ input and write a `.kite` index | `FASTQ`, `FASTQ.GZ`, `BGZF FASTQ` | `.kite` |
| `kite dump` | Export exact k-mer counts from a `.kite` index | `.kite` | `TSV` |
| `kite info` | Print or edit structured metadata for one `.kite` index | `.kite` | stdout |
| `kite query` | Look up one exact k-mer count | `.kite` + k-mer | stdout |
| `kite histo` | Write count-frequency histograms for one or more databases | `.kite`... | `.tsv` |
| `kite merge` | Merge compatible `.kite` databases by exact count summation | `.kite`... | `.kite` |
| `kite profile` | Build a union-by-sample raw-count matrix | `.kite`... | `.tsv` |
| `kite analyze` | Build an exact multi-sample count matrix, write summary tables, and generate a Quarto report | `.kite`... | output directory |
| `kite citation` | Print version and citation information | none | stdout |
| `kite fly` | Run count + analyze in one command | FASTQ... | output directory |

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
  --buffer 64 \
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
  --embedding-input pca \
  --heatmap-kmeans 6 \
  --seed 42 \
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
  --correlation all \
  --embedding all \
  --embedding-input pca \
  --heatmap-kmeans 6 \
  --seed 42 \
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
| `--partitions` | `0` | Number of partitions | `0` means auto; the planner accounts for input size, memory, and thread budget; manual values must be powers of two |
| `--chunk-kmers` | `0` | Reader-side target batch size | `0` means auto; the planner chooses a chunk size from memory and worker count |
| `--memory` | auto-detected | Total memory budget | Must be written as `512M`, `64G`, or `1T`; on Unix this is also applied as the process address-space limit |
| `--canonical` | `false` | Whether to canonicalize k-mers | Accepts `true` or `false`; changes result semantics |
| `--min` | none | Header minimum count marker | Raw counts are retained; downstream commands hide counts below this marker |
| `--max` | none | Header maximum count marker | Raw counts are retained; downstream commands hide counts above this marker |
| `--mode` | `fast` | Output compression mode | `fast` writes DVLE/raw only; `balanced` and `compact` additionally try zstd when smaller |
| `--buffer` | auto | Target staging buffer size in MB | Omit for auto; explicit values are treated as the requested upper bound and may be reduced when memory is tight |
| `-p, --progress` | `false` | Show progress | Useful for long jobs |
| `--test-mode` | `false` | Print detailed runtime statistics | Useful for benchmarking and tuning |

When `--partitions`, `--chunk-kmers`, and `--buffer` are left in auto mode, `kite count` derives an internal plan from the input sample, `-t/--threads`, and `--memory`. Under tight fixed-memory benchmarks, the planner may reduce count worker threads, cap partitions, shrink chunks, and lower staging buffers to stay within the process limit. `--test-mode` prints the selected plan, including actual count workers, partitions, chunk size, staging buffer, tracked budget, and reserve.

`--min` and `--max` are lazy filters. They are written into the `.kite` header and do not slow down `count` by rewriting the database. `dump`, `histo`, `profile`, `analyze`, and `merge` read these markers and exclude marked-out k-mers from their outputs. `query` still reports the raw stored count and tells you whether that k-mer is excluded by the current header markers. `info` can edit the markers in place without touching the stored count data.

### `kite dump`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | Input `.kite` file | One database per run |
| `-o, --output` | required | Output TSV path | Use `-` for stdout |
| `-t, --threads` | `0` | Dump worker thread count | `0` means auto |
| `--buffer` | `64` | Output slab / writer buffer size in MB | Larger values increase per-slab buffering and in-flight writeback pressure |
| `-p, --progress` | `false` | Show progress | Useful for large files |
| `--test-mode` | `false` | Print detailed runtime statistics | Useful for validation and benchmarking |

`dump` applies the input file's header `Min Count` and `Max Count` markers while writing TSV. Use `kite info --set-min/--set-max` to change those markers without recounting.
If the parent output directory does not exist, `dump` creates it automatically. For file output, Kite uses a single sequential writer thread backed by large slabs; workers already decode, merge, and format complete TSV slabs before sending them. The writer only performs `write_all` and `flush`.

For file output, `dump` also chooses a fixed in-flight queue budget once at startup from the estimated output size:

- small outputs: `4` slabs
- medium outputs: `3` slabs
- large outputs: `2` slabs

This queue choice is not adjusted during the run. The goal is to keep backpressure simple and avoid overfilling page cache on very large TSV exports.

`--test-mode` reports `writer_io`, `writer_queue_wait`, `peak_inflight_bytes`, `peak_inflight_slabs`, `writer_write_calls`, `avg_write_call_mb`, and `final_budget_bytes`. These are the main indicators for diagnosing I/O-bound dumps.

For benchmarking dump formatting without real TSV disk writes, you can stream to stdout:

```bash
./target/release/kite dump -i sample.kite -o - -t 64 | wc -c
```

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
| `-t, --threads` | `0` | Number of input files to process in parallel | `0` means auto |
| `-p, --progress` | `false` | Show progress | More useful for many inputs |

`histo` applies each input file's header count filters before building count-frequency bins.

### `kite merge`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | Two or more `.kite` files | Use one `-i` followed by all files; inputs must be compatible |
| `-o, --output` | required | Output merged `.kite` file | Exact summation, no sketching |
| `-t, --threads` | `0` | Parallel input-loading workers per partition | `0` means auto |
| `-p, --progress` | `false` | Show progress | Also prints compatibility summaries |

Compatibility requirements:

- same `k`
- same `canonical` setting
- same `partitions`
- same `key_width`

`merge` applies each input file's header filters before summing counts. If every input has the same `Min Count` or `Max Count` marker, that marker is copied to the merged output header; otherwise the merged output leaves the differing marker unset. A Markdown report is written next to the merged output as `<output>.merge_report.md`.

### `kite profile`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | One or more `.kite` files | Use one `-i` followed by all files; uses the union of observed k-mers |
| `--label` | inferred from filenames | Sample labels | Must match the number of inputs |
| `-o, --output` | required | Output TSV path | First column is `kmer` |
| `-t, --threads` | `0` | Parallel input-loading workers per partition | `0` means auto |
| `-p, --progress` | `false` | Show progress | Useful for multi-sample runs |

`profile` applies each input file's header count filters before constructing the union-by-sample matrix.

### `kite analyze`

| Option | Default | Description | Notes |
|---|---|---|---|
| `-i, --input` | required | One or more `.kite` files | Use one `-i` followed by all files; builds an exact multi-sample matrix |
| `--label` | inferred from filenames | Sample labels | Must match the number of inputs |
| `--group` | one per sample | Sample groups for report coloring | Defaults to the sample labels; must match the number of inputs when provided |
| `-o, --output-dir` | required | Output directory | TSV tables, `analysis_report.md`, and `analysis_report.qmd` are written here |
| `--correlation` | `pearson` | Correlation or distance mode | `all`, `pearson`, `spearman`, `kendall`, `cosine`, `euclidean`, `manhattan`, or `mahalanobis` |
| `--embedding` | `pca` | Embedding mode | `all`, `pca`, `tsne`, `umap`, or `pacmap` (`pcamap` is accepted as an alias) |
| `--embedding-input` | `pca` | Input representation for non-linear embeddings | Default speed-oriented mode; use `full` to keep the original full k-mer feature matrix |
| `--embedding-input-dims` | `50` | PCA dimensions kept for `--embedding-input pca` | Effective value is capped by sample count |
| `--heatmap-kmeans` | `6` | Number of k-means row clusters for heatmap generation | Applied to the analyzed heatmap matrix |
| `--seed` | `42` | Random seed for stochastic embeddings | `--heatmap-seed` is kept as a compatibility alias; the current online heatmap path is deterministic |
| `--tsne-perplexity` | auto | t-SNE perplexity | Derived from sample count when omitted |
| `--tsne-learning-rate` | `200.0` | t-SNE learning rate | Must be > `0` |
| `--tsne-iterations` | `750` | t-SNE iterations | Must be >= `1` |
| `--umap-neighbors` | auto | UMAP neighbor count | Derived from sample count when omitted |
| `--umap-min-dist` | `0.1` | UMAP `min_dist` | Must be >= `0` |
| `--umap-spread` | `1.0` | UMAP spread | Must be > `0` |
| `--pacmap-neighbors` | auto | PaCMAP neighbor count | Derived from sample count when omitted |
| `--pacmap-mn-end` | auto | PaCMAP `mn_end` | Derived from sample count when omitted |
| `--cluster-samples` | `true` | Cluster sample columns before writing heatmap outputs | Accepts `true` or `false` |
| `-t, --threads` | `0` | Parallel input-loading workers per partition | `0` means auto |
| `-p, --progress` | `false` | Show progress | Recommended for larger runs |

`analyze` applies each input file's header count filters before constructing the exact matrix. It writes exact tables in Rust, writes `analysis_report.md` as a static run summary, and generates `analysis_report.qmd`. Render the Quarto document when you want R/ggplot2 and ComplexHeatmap figures.

Method notes:

- `pca` is deterministic and has no minimum-sample fallback.
- `tsne`, `umap`, and `pacmap` currently require at least `4` samples and a non-empty feature matrix; otherwise Kite prints a skip message, does not write the corresponding `embedding_*.tsv`, and records the skipped method in `analysis_report.md`.
- Kite prints the effective embedding settings that were actually used, including auto-derived neighbor/perplexity values.
- `--embedding-input pca` is the explicit speed-oriented option for `tsne`, `umap`, and `pacmap`. It leaves exact/count/correlation/heatmap tables unchanged, but it does change the non-linear embedding coordinates because those methods run on PCA-reduced sample features instead of the full k-mer matrix.

Benchmark notes:

- `benchmark/run_kite_test_commands.sh` uses 4 inputs by default: the two `HiChiP` FASTQ files, `test/test.fastq.gz`, and `test/test_half.fastq.gz`.
- The script defaults to `--correlation all --embedding all --seed 42`, so it exercises the full correlation and embedding matrix when the sample count is high enough.

Quarto setup notes:

- Install [Quarto](https://quarto.org/) to render `analysis_report.qmd`.
- Run `quarto install tinytex` if you want Quarto to render PDF output.

### `kite analyze` Output Files

| File | Meaning |
|---|---|
| `exact_count_matrix.tsv` | Exact multi-sample raw-count matrix |
| `sample_summary.tsv` | Per-sample total raw counts and nonzero k-mer counts |
| `correlation_pearson.tsv` | Written when `--correlation all` or `pearson` is selected |
| `correlation_spearman.tsv` | Written when `--correlation all` or `spearman` is selected |
| `correlation_kendall.tsv` | Written when `--correlation all` or `kendall` is selected |
| `correlation_cosine.tsv` | Written when `--correlation all` or `cosine` is selected |
| `correlation_euclidean.tsv` | Written when `--correlation all` or `euclidean` is selected |
| `correlation_manhattan.tsv` | Written when `--correlation all` or `manhattan` is selected |
| `correlation_mahalanobis.tsv` | Written when `--correlation all` or `mahalanobis` is selected; uses a streaming diagonal covariance approximation |
| `embedding_pca.tsv` | Written when `--embedding all` or `pca` is selected |
| `embedding_tsne.tsv` | Written when `--embedding all` or `tsne` is selected and the method is not skipped |
| `embedding_umap.tsv` | Written when `--embedding all` or `umap` is selected and the method is not skipped |
| `embedding_pacmap.tsv` | Written when `--embedding all` or `pacmap` is selected and the method is not skipped |
| `heatmap_matrix.tsv` | Row-scaled heatmap matrix with cluster assignments |
| `heatmap_plot_matrix.tsv` | Smaller heatmap matrix used by the Quarto report |
| `heatmap_clusters.tsv` | Heatmap cluster centroids |
| `heatmap_samples.tsv` | Heatmap sample order |
| `analysis_report.md` | Static Markdown report with inputs, settings, selected methods, and output inventory |
| `analysis_report.qmd` | Quarto/R report for generating figures from the TSV tables |

R-side notes:

- Kite itself does not render figures.
- Install [Quarto](https://quarto.org/) before rendering `analysis_report.qmd`; run `quarto install tinytex` when you need PDF output.
- The generated `analysis_report.qmd` declares and installs the R packages it needs when rendered, then reads and summarizes the TSV inventory and sample summary before plotting.
- The generated `analysis_report.qmd` includes a YAML `color_theme` parameter. Built-in choices are `anime_sky`, `sakura_mint`, `sunset_pop`, and `midnight_tokyo`; switching themes only changes styled tables and figure colors.
- When `embedding_*.tsv` includes a `group` column, the Quarto embedding plots fill points by group.
- When `--group` is omitted, Kite uses one group per sample and names each group after the sample label.
- The default report saves separate PNG and PDF files for sample summaries, correlation/distance heatmaps, embeddings, and the cluster-representative heatmap with fixed figure sizes.
- The default heatmap uses `heatmap_clusters.tsv`, shows one representative row per k-mer cluster, and includes a sample-column dendrogram when `--cluster-samples true`.
- `heatmap_matrix.tsv` still contains one row per exact k-mer. To plot that larger exact heatmap, edit the QMD YAML and set `render_exact_kmer_heatmap: true`; exact k-mer rows are ordered but not hierarchically clustered.
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
| `--group` | one per sample | Sample groups for report coloring | Defaults to the sample labels; must match the number of inputs when provided |
| `-o, --output-dir` | required | Output directory | Creates `databases/` and `analysis/` |
| `-k, --kmer` | `31` | k-mer length used by count | |
| `--canonical` | `false` | Whether to canonicalize k-mers in internal count jobs | Accepts `true` or `false` |
| `--min` | none | Header minimum count marker forwarded to count | Downstream analyze in the same `fly` run excludes lower counts |
| `--max` | none | Header maximum count marker forwarded to count | Downstream analyze in the same `fly` run excludes higher counts |
| `--mode` | `fast` | Count compression mode | Same values and semantics as `kite count --mode` |
| `-t, --threads` | `0` | Thread budget forwarded to count | `0` means auto |
| `--memory` | auto-detected | Memory budget forwarded to count | Must use `M`, `G`, or `T` suffixes |
| `--partitions` | `0` | Count partition count | `0` means auto |
| `--chunk-kmers` | `0` | Count chunk size | Same auto semantics as `kite count` |
| `--buffer` | auto | Count staging buffer in MB | Same auto semantics as `kite count` |
| `--correlation` | `pearson` | Correlation or distance mode passed to analyze | |
| `--embedding` | `pca` | Embedding mode passed to analyze | |
| `--embedding-input` | `pca` | Embedding input mode passed to analyze | `full` keeps the original full k-mer feature matrix |
| `--embedding-input-dims` | `50` | PCA dimensions kept for `--embedding-input pca` | Effective value is capped by sample count |
| `--heatmap-kmeans` | `6` | Heatmap row cluster count passed to analyze | |
| `--seed` | `42` | Random seed passed to analyze | `--heatmap-seed` remains accepted as an alias |
| `--tsne-perplexity` | auto | t-SNE perplexity passed to analyze | |
| `--tsne-learning-rate` | `200.0` | t-SNE learning rate passed to analyze | |
| `--tsne-iterations` | `750` | t-SNE iterations passed to analyze | |
| `--umap-neighbors` | auto | UMAP neighbors passed to analyze | |
| `--umap-min-dist` | `0.1` | UMAP `min_dist` passed to analyze | |
| `--umap-spread` | `1.0` | UMAP spread passed to analyze | |
| `--pacmap-neighbors` | auto | PaCMAP neighbors passed to analyze | |
| `--pacmap-mn-end` | auto | PaCMAP `mn_end` passed to analyze | |
| `--cluster-samples` | `true` | Sample clustering flag passed to analyze | |
| `--keep-intermediate` | `true` | Keep intermediate `.kite` files in `databases/` after analysis | Set `false` to remove them after the report is written |
| `-p, --progress` | `false` | Show progress | Forwarded to internal count and analyze steps |

`fly` runs:

1. `count` on each input sequencing file
2. `analyze` on the resulting `.kite` databases
3. `analysis_report.md`, `analysis_report.qmd`, and `kite_flye_workflow_report.md` generation

`kite_flye_workflow_report.md` records the workflow inputs, per-stage outputs, key settings, and elapsed time. When `--keep-intermediate false` is used, the report is written before intermediate `.kite` files are removed.

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
