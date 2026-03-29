# Learning Kite

## Audience

This document is for readers who want to understand how Kite is written, not just how to run it. It is especially aimed at:

- beginners in Rust
- beginners in k-mer methods
- contributors who want to optimize one command without breaking another
- readers who want a conceptual bridge between biology, algorithms, and implementation

## The Central Design Idea

Kite is organized by subcommand.

That means:

- `src/count/` owns counting
- `src/dump/` owns dumping and decode-side logic
- `src/info/` owns inspection output
- `src/query/` owns exact lookup
- `src/histo/` owns histogram reporting
- `src/profile/` owns multi-sample exact profiling
- `src/sketch/` owns exact multi-sample correlation, projection, clustered-heatmap table generation, and plotting script export

Only truly shared concepts are placed in `src/utils/`.

This design is important because performance trade-offs should remain local. A trick that helps `dump` should not automatically be forced onto `count`, and vice versa.

## The Source Tree

```text
src/
  main.rs
  cli.rs
  count/
  dump/
  info/
  query/
  histo/
  profile/
  sketch/
  citation/
  utils/
```

## High-Level Data Flow

### `count`

```text
FASTQ -> sequences -> rolling k-mers -> minimizers -> super-kmers
      -> partitions -> local sort -> global merge -> .kite
```

### `dump`

```text
.kite -> header parse -> choose decode path -> parallel text formatting -> TSV
```

### `profile`

```text
multiple .kite -> exact sorted vectors -> union scan -> raw-count matrix
```

### `sketch`

```text
multiple .kite -> exact raw-count matrix -> correlation table + projection tables + clustered heatmap table
```

## Mathematical Foundations

## 1. K-mer Counting

If a read has length `L`, the number of k-mer windows is:

```text
max(L - k + 1, 0)
```

This is the combinatorial starting point of the whole project.

## 2. Two-Bit Encoding

Kite uses:

| Base | Bits |
|---|---:|
| `A` | `00` |
| `C` | `01` |
| `G` | `10` |
| `T` or `U` | `11` |

Why it matters:

- it makes k-mers compact
- it enables direct integer comparison
- it enables radix sorting

## 3. Canonicalization

Let `rc(x)` be the reverse complement of `x`. Canonical mode stores:

```text
canonical(x) = min(x, rc(x))
```

This merges both orientations into one representative.

## 4. Minimizers

A minimizer is the smallest encoded `m`-mer inside a k-mer window.

Why Kite uses minimizers:

- nearby k-mers often share a minimizer
- this creates super-kmers
- super-kmers improve locality and reduce overhead

## 5. Similarity Metrics

### Cosine Similarity

For vectors `a` and `b`:

```text
cos(a, b) = (a ¡¤ b) / (||a|| ||b||)
```

Use:

- compare normalized abundance patterns


## 6. Exact Multi-Sample Matrices

For `profile` and `sketch`, Kite now keeps the exact union of observed k-mers:

```text
row = one exact k-mer
column = one sample
cell = raw count in that sample, or 0 if absent
```

This makes the exported table directly interpretable and easy to reuse in downstream statistics.

## Entry Layer

## `src/main.rs`

Role:

- parse CLI arguments with Clap
- dispatch to the selected subcommand

Main idea:

- keep the executable thin
- keep algorithmic work out of the top-level file

## `src/cli.rs`

Role:

- define every subcommand
- define every argument and default value

Why it matters:

- the CLI is explicit
- help text stays synchronized with the implementation
- tools and contributors can inspect one file to understand the public interface

## Shared Format Layer

## `src/utils/kite.rs`

This file is the format contract of the whole project.

Key types:

- `KmerMode`
- `KiteMetadata`
- `KiteInfo`
- `KiteStats`
- `HeaderPatch`

Key responsibilities:

- define magic bytes and format flags
- write the `.kite` header
- patch payload length after streaming writes
- read current and legacy headers
- encode metadata
- decode metadata

Why this file is central:

- all commands that read or write `.kite` depend on it
- the format must remain synchronized across subcommands

## `src/utils/fmt.rs`

Small helper file with two practical functions:

- `format_duration`
- `format_size`

It exists so all commands print sizes and timing in a consistent way.

## Counting Subsystem

Files:

- `src/count/mod.rs`
- `src/count/fastq.rs`
- `src/count/kmer.rs`
- `src/count/minimizer.rs`
- `src/count/buffer.rs`
- `src/count/radix_sort.rs`
- `src/count/merge.rs`
- `src/count/decode.rs`

## `src/count/mod.rs`

Main function:

- `run`

What it does:

- build a `CountConfig`
- choose optimized or baseline counting
- forward the result to the binary writer
- print the command summary

Input:

- parsed `CountArgs`

Output:

- `.kite` database

## `src/count/fastq.rs`

Main public function:

- `open_seq_reader`

Conceptual job:

- hide the details of reading FASTQ and FASTQ.GZ
- return a sequence iterator-like abstraction to the counting pipeline

Why this separation is good:

- parsing logic stays isolated from algorithmic logic

## `src/count/kmer.rs`

Important items:

- `encode_base`
- `words_for_k`
- `KmerMode::from_k`
- `KmerKey`
- `RollingKmer`
- `shift_left_insert`
- `shift_right_insert_top`
- `set_2bits`

Core idea:

`RollingKmer` updates the current k-mer incrementally as each new base arrives. That is much cheaper than rebuilding every k-mer from scratch.

It also maintains forward and reverse-complement state so canonicalization can be done in constant time per step.

## `src/count/minimizer.rs`

Important items:

- `RollingMinimizerU64`
- `SuperKmerBuilder`

Core idea:

- the minimizer is updated in a rolling manner
- a deque stores the current candidate minima efficiently
- consecutive k-mers with the same minimizer are grouped into one super-kmer run

This is a strong locality optimization.

## `src/count/buffer.rs`

This is the main engine of `count`.

Important types:

- `CountConfig`
- `CountTable`
- `TimingBreakdown`
- `CountOutcome`
- `ProgressCounters`
- `WorkerOutput`
- `SeqBatch`

Important functions:

- `validate_cfg`
- `count_optimized`
- `worker_loop`
- `flush_super`
- `flush_partition`
- `count_baseline`
- `partition_u64`
- `partition_u128`
- `partition_packed`

### What `count_optimized` does

1. validate the configuration
2. decide the representation mode from `k`
3. spawn worker threads
4. read the input in batches
5. send batches to workers through channels
6. let each worker build local chunks
7. merge partition outputs into one final count table

### Why channels appear

Producer-consumer separation helps:

- parsing stay independent from counting
- workers stay busy while the reader continues
- batches amortize coordination cost

### Why partitioning appears

Instead of sorting one giant unsplit stream, Kite partitions the work. That reduces the effective size of local sorts and improves merge behavior.

## `src/count/radix_sort.rs`

Important functions:

- `radix_sort_u64`
- `radix_sort_u128`
- `counting_pass_u64`
- `counting_pass_u128`
- `radix_sort_u64_16`
- `radix_sort_u128_16`
- `ensure_scratch_len`

Why radix sort is used:

- encoded k-mers are fixed-width integers
- radix sort exploits that structure
- it is often better than comparison sorting for these keys

Why AVX2 appears:

- large histogram-like counting passes can benefit from vectorized x86-64 instructions

## `src/count/merge.rs`

Important functions:

- `merge_chunks_u64`
- `merge_chunks_u128`
- `merge_chunks_packed`
- `push_next_u64_owned`
- `push_next_u128_owned`
- `push_next_packed_owned`

Core idea:

- each chunk is already sorted
- a binary heap performs a multiway merge
- equal keys are collapsed into a single final count

## `src/count/decode.rs`

This is the count-side binary writer.

Important functions:

- `write_binary`
- `write_block_framed_u64`
- `write_block_framed_u128`
- `write_block_payload`
- `encode_block_u64`
- `encode_block_u128`
- `encode_u64_delta_varint_to_sender`
- `encode_u128_to_sender`
- `encode_packed_to_sender`
- `put_varint_u64`

Why this file lives in `count/`:

- writing strategy is a count-side performance decision
- it should not be forced into a generic shared module if that harms specialization

## Dump And Inspection Subsystem

Files:

- `src/dump/mod.rs`
- `src/dump/decode.rs`
- `src/dump/kmer.rs`
- `src/info/mod.rs`
- `src/query/mod.rs`
- `src/histo/mod.rs`

## `src/dump/mod.rs`

Main function:

- `run`

Role:

- load database metadata
- call the dump engine
- print a concise command summary

## `src/dump/kmer.rs`

Important functions:

- `encode_base`
- `decode_u64_fast_into`
- `decode_u128_fast_into`
- `decode_packed_fast_into`
- `extract_2bits`
- `extract_16bits`

Why it is separate from `count/kmer.rs`:

- count needs fast rolling updates
- dump needs fast decoding to ASCII

Those are different hot paths, so the implementation is intentionally separated.

## `src/dump/decode.rs`

This is the largest single engine file in the project.

Important public functions:

- `dump_binary_to_tsv`
- `query_kmer`
- `write_histogram`
- `read_u64_frequency_map`
- `read_u64_count_vec`
- `collect_stats`

Important internal groups:

- dump path selection functions
- block-framed readers
- byte-stream readers
- query helpers
- histogram/statistics visitors
- worker and writer pipelines
- low-level decode helpers

Why so many paths exist:

- `u64`, `u128`, and packed modes differ
- delta-varint and fixed-width payloads differ
- block-framed and stream-style layouts differ
- one specialized fast path can matter a lot in practice

## `src/info/mod.rs`

Main function:

- `run`

Role:

- read metadata
- collect summary statistics
- print a detailed report for users

## `src/query/mod.rs`

Main function:

- `run`

Role:

- load one database
- look up one k-mer
- print the result

## `src/histo/mod.rs`

Main function:

- `run`

Role:

- scan counts
- build a histogram
- write a compact TSV summary

## Multi-Sample Analysis Subsystem

Files:

- `src/profile/mod.rs`
- `src/profile/vector.rs`
- `src/profile/kmer.rs`
- `src/sketch/mod.rs`
- `src/sketch/kmer.rs`

## `src/profile/mod.rs`

Main function:

- `run`

Role:

- coordinate profile generation
- print input/output summaries

## `src/profile/vector.rs`

Important functions:

- `run_profile`
- `write_union_table`
- `append_u64_ascii`

Core idea:

- each sample already has a sorted vector of exact counts
- the union table is generated by a multi-pointer scan
- missing keys in a sample become zero

This is much lighter than building a giant hash map for every row.

## `src/sketch/mod.rs`

Important functions:

- `run`
- `resolve_sample_names`
- `load_kite_samples`
- `write_profile_table_and_collect_correlation`
- `write_correlation_matrix`
- `write_correlation_script`
- `write_projection_script`
- `write_heatmap_script`
- `write_script`
- `dir_bytes`
- `append_u64_ascii`
- `CorrelationAccumulator`

What `sketch` does conceptually:

- build an exact raw-count matrix from multiple `.kite` files
- compute pairwise sample correlations
- compute exact PCA sample scores from the centered Gram matrix of the raw-count table
- compute t-SNE and UMAP coordinates from an exact low-rank coordinate system that preserves sample distances from the raw-count table
- write a clustered heatmap table with optional sample clustering and k-mer k-means assignment
- emit R scripts that only read those precomputed tables and plot them

## `src/citation/mod.rs`

Main function:

- `run`

Role:

- print version and citation text

## Why This Project Is Fast

### 1. Integer-Native Encodings

Small and medium k-mers live in `u64` or `u128`, which makes sorting and comparison cheap.

### 2. Rolling Windows

K-mers and minimizers are updated incrementally instead of recomputed from scratch.

### 3. Partitioned Work

Large streams are split into smaller, more manageable groups before final merge.

### 4. Representation-Aware Algorithms

Different `k` ranges use different internal strategies.

### 5. Subcommand Independence

`count` and `dump` do not need to share identical internal helpers if different helpers are faster for each command.

## Recommended Reading Order

1. `README.md`
2. `doc/usage.md`
3. `src/main.rs`
4. `src/cli.rs`
5. `src/utils/kite.rs`
6. `src/count/mod.rs`
7. `src/count/buffer.rs`
8. `src/dump/mod.rs`
9. `src/dump/decode.rs`
10. `src/profile/vector.rs`
11. `src/sketch/mod.rs`

## Final Advice

If you are new to the project, begin with visible behavior and then move inward:

- run a command
- inspect its stdout
- find the `run` function for that command
- follow the call chain into the hot path

That learning order matches the architecture of the codebase itself.
