# Kite Usage Guide

## Overview

Kite is a command-line toolkit for k-mer counting, storage, inspection, and lightweight downstream analysis. The normal workflow is:

1. read sequencing data
2. count k-mers
3. store them in a `.kite` database
4. inspect or export that database
5. combine multiple databases for comparative analysis

Main executable:

```bash
kite <subcommand> [options]
```

## Commands

| Command | Purpose | Input | Output |
|---|---|---|---|
| `count` | count k-mers and build a database | FASTQ / FASTQ.GZ | `.kite` |
| `dump` | export exact counts | `.kite` | TSV |
| `info` | inspect metadata and summary statistics | `.kite` | stdout |
| `query` | retrieve one exact k-mer count | `.kite` + k-mer | stdout |
| `histo` | summarize count frequencies | `.kite` | TSV |
| `profile` | build a raw-count union table across samples | multiple `.kite` | TSV / CSV / TXT |
| `sketch` | generate exact correlation, projection-score, and clustered heatmap tables from `.kite` inputs | multiple `.kite` | directory |
| `citation` | print version and citation text | none | stdout |

## Core Concepts

### K-mer

A k-mer is a substring of length `k`. A read of length `L` contributes:

```text
max(L - k + 1, 0)
```

k-mers.

### Canonical K-mer

If `x` is a k-mer and `rc(x)` is its reverse complement, then canonical mode stores:

```text
canonical(x) = min(x, rc(x))
```

This merges reverse-complement pairs into one count.

### Minimizer

A minimizer is the smallest encoded `m`-mer observed inside a k-mer window. Kite uses minimizers to group neighboring k-mers into super-kmers, improving locality and partitioning efficiency.

### `.kite` Database

A `.kite` file contains:

- a header
- format flags
- `k`
- representation mode
- entry count
- metadata
- compressed payload

The format contract is implemented in `src/utils/kite.rs`.

### Automatic Representation Modes

Kite chooses storage mode from `k`:

| Range | Mode |
|---|---|
| `k <= 32` | `u64` |
| `33 <= k <= 64` | `u128` |
| `k > 64` | packed multi-word |

## `kite count`

### Purpose

Read sequence input and create a `.kite` binary database.

### Syntax

```bash
kite count --kmer <K> --input <INPUT> --output <OUTPUT> [options]
```

### Parameters

| Parameter | Default | Meaning |
|---|---:|---|
| `-k, --kmer <K>` | required | k-mer length |
| `-i, --input <INPUT>` | required | input FASTQ or FASTQ.GZ |
| `-o, --output <OUTPUT>` | required | output `.kite` file |
| `-m, --minimizer <M>` | `15` | minimizer length |
| `--partitions <N>` | `256` | number of partitions |
| `--threads <N>` | `8` | worker thread count |
| `--chunk-kmers <N>` | `1000000` | chunk size used during sort/merge |
| `--canonical <true|false>` | `true` | canonicalize reverse complements |
| `-p, --progress` | `false` | show lightweight progress |
| `--baseline` | `false` | use the slower reference pipeline |
| `--mode <fast|balanced|compact>` | `fast` | write/compression profile |
| `--zstd-level <LEVEL>` | auto | explicit Zstandard level override |

### Notes

- `minimizer` must satisfy `1 <= m <= k`
- current implementation requires `m <= 32`
- `fast` mode is the main performance-oriented mode
- all current modes still use Zstandard compression

### Example

```bash
kite count -k 31 -i reads.fastq.gz -o sample.kite --threads 8 --mode fast
```

### Printed Summary

`count` prints:

- input file and size
- total reads
- total observed k-mers
- number of unique k-mers
- canonical setting
- output file and size
- elapsed time

## `kite dump`

### Purpose

Export one `.kite` database to a two-column TSV:

```text
kmer<TAB>count
```

### Syntax

```bash
kite dump --input <INPUT> --output <OUTPUT> [--threads <N>]
```

### Parameters

| Parameter | Default | Meaning |
|---|---:|---|
| `-i, --input <INPUT>` | required | input `.kite` file |
| `-o, --output <OUTPUT>` | required | output TSV |
| `-t, --threads <N>` | `8` | number of dump workers |
| `-p, --progress` | `false` | show lightweight progress spinner |

### Important Comparison Note

The row order of `kite dump` is not required to match the row order of `kmc_dump`. To compare exact results:

1. sort both tables by the first column
2. compare row counts
3. compare k-mer strings
4. compare counts

### Example

```bash
kite dump -i sample.kite -o sample.tsv -t 8
```

## `kite info`

### Purpose

Inspect one `.kite` database and print detailed metadata and summary statistics.

### Syntax

```bash
kite info --input <INPUT>
```

### Parameters

| Parameter | Meaning |
|---|---|
| `-i, --input <INPUT>` | input `.kite` file |
| `-p, --progress` | show lightweight progress spinner |

### Fields Commonly Printed

- format version
- `k`
- mode
- canonical flag
- encoding style
- payload compression
- source path
- creation timestamp
- write mode
- zstd level
- counting thread count
- minimizer
- partition count
- chunk size
- total reads
- total k-mers
- unique k-mers
- singleton count
- maximum count
- mean count per unique k-mer
- singleton fraction
- header bytes
- metadata bytes
- payload bytes
- file size

## `kite query`

### Purpose

Return the count for exactly one k-mer.

### Syntax

```bash
kite query --input <INPUT> --kmer <KMER>
```

### Parameters

| Parameter | Meaning |
|---|---|
| `-i, --input <INPUT>` | input `.kite` file |
| `--kmer <KMER>` | query sequence |
| `-p, --progress` | show lightweight progress spinner |

### Behavior

- the query length must equal the database `k`
- invalid nucleotide characters are rejected
- if the database is canonical, the query is canonicalized before lookup

## `kite histo`

### Purpose

Summarize count frequencies across one database.

### Syntax

```bash
kite histo --input <INPUT> --output <OUTPUT>
```

### Parameters

| Parameter | Meaning |
|---|---|
| `-i, --input <INPUT>` | input `.kite` file |
| `-o, --output <OUTPUT>` | output histogram TSV |
| `-p, --progress` | show lightweight progress spinner |

### Output Format

```text
count<TAB>num_kmers
```

The first column is an observed count value. The second column is how many distinct k-mers had that count.

## `kite profile`

### Purpose

Create a raw-count union table across multiple samples.

### Syntax

```bash
kite profile --input <INPUT>... --output <OUTPUT> [--sample-name <NAME>...] [--format <FORMAT>]
```

### Parameters

| Parameter | Default | Meaning |
|---|---:|---|
| `-i, --input <INPUT>...` | required | one or more `.kite` files |
| `-o, --output <OUTPUT>` | required | output path |
| `--sample-name <NAME>...` | none | optional sample labels |
| `--format <FORMAT>` | `tsv` | `tsv`, `csv`, or `txt` |
| `-p, --progress` | `false` | show lightweight progress spinner |

### Current Limitation

`profile` currently reads exact `u64` count vectors, so it is intended for:

```text
k <= 32
```

### Output Meaning

- first column: k-mer string
- later columns: exact raw counts for each sample
- missing k-mers in a sample are written as `0`

## `kite sketch`

### Purpose

Generate multi-sample correlation, projection, and heatmap outputs.

### Syntax

```bash
kite sketch --input <INPUT>... [options]
```

### Parameters

| Parameter | Default | Meaning |
|---|---:|---|
| `-i, --input <INPUT>...` | required | two or more `.kite` files |
| `--sample-name <NAME>...` | none | optional sample labels |
| `--sample-group <GROUP>...` | none | optional sample groups for generated sketch metadata and plot grouping |
| `-o, --output-dir <DIR>` | `sketch_out` | output directory |
| `--correlation <pearson|cosine>` | `pearson` | sample-to-sample association metric |
| `--projection <pca|tsne|umap|all>` | `all` | which projection score tables and plotting scripts to write |
| `--heatmap-kmeans <N>` | `6` | row cluster count for the clustered heatmap table |
| `--cluster-samples <true|false>` | `true` | whether Kite clusters sample columns before writing the heatmap table |
| `-p, --progress` | `false` | show lightweight progress spinner |

### Current Limitation

`sketch` currently works on `u64` databases, so it is intended for:

```text
k <= 32
```

### Typical Outputs

- `sketch_profile.tsv` when sketch starts from multiple `.kite` files
- `sketch_correlation.tsv`
- `sketch_sample_metadata.tsv`
- `pca_scores.tsv`
- `tsne_scores.tsv`
- `umap_scores.tsv`
- `sketch_heatmap_clustered.tsv`
- `plot_correlation.R`
- `plot_pca.R`
- `plot_tsne.R`
- `plot_umap.R`
- `plot_heatmap.R`

### Conceptual Meaning

- profile-style table: exact k-mer count matrix with one sample per column
- correlation table: sample-by-sample Pearson or cosine matrix
- PCA / t-SNE / UMAP score tables: sample coordinates computed directly by Kite from the exact multi-sample count matrix
- clustered heatmap table: exact k-mer-by-sample count table reordered by optional sample clustering and annotated with Kite-computed k-means row clusters
- sample metadata table: editable mapping of `sample_id -> sample_label / sample_group`, used by the generated R plotting scripts
- plotting scripts: read those precomputed tables and draw figures; they do not compute the embeddings or clustering themselves

## `kite citation`

### Purpose

Print version and citation information.

### Syntax

```bash
kite citation
```

## Metadata Stored In Modern `.kite` Files

Current files may record:

- `source_path`
- `created_at`
- `total_reads`
- `total_kmers`
- `unique_kmers`
- `minimizer`
- `partitions`
- `count_threads`
- `chunk_kmers`
- `canonical`
- `write_mode`
- `zstd_level`

## Practical Recommendations

### Recommended Everyday Count Command

```bash
kite count -k 31 -i reads.fastq.gz -o sample.kite --threads 8 --mode fast
```

### When To Use Canonical Mode

Use canonical mode when you want strand-invariant counts.

Disable it only when you explicitly need orientation-specific behavior.

### When To Use `balanced` Or `compact`

Use them when file size matters more than maximum counting speed.

### When Comparing With Another Tool

Always compare sorted tables, not raw dump order.

## Common Error Conditions

- `k = 0`
- `minimizer > k`
- `minimizer > 32`
- zero threads
- query k-mer length mismatch
- `profile` inputs with different `k`
- `sketch` inputs with different `k`
- `profile` or `sketch` on non-`u64` databases
