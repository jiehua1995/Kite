# Kite Benchmark Notes

## Overview

This document records the benchmark snapshot that was used to evaluate Kite against KMC for the main performance goals of the project.

The focus of this round was:

- `kite count`
- `kite dump`
- `k = 31`
- `k = 51`
- `8` threads
- input dataset `test/NSC_capture_R1_rep1.1M.fastq.gz`

## Benchmark Environment

| Item | Value |
|---|---|
| Date | 2026-03-28 |
| Input | `test/NSC_capture_R1_rep1.1M.fastq.gz` |
| Threads | `8` |
| Comparison tool | `kmc` and `kmc_dump` |
| KMC count flags | `-ci1 -cs1000000000 -t8 -fq` |

## Performance Table

| k | Kite count | KMC count | Kite dump | KMC dump |
|---|---:|---:|---:|---:|
| 31 | 8.286 s | 34.479 s | 2.590 s | 56.722 s |
| 51 | 27.947 s | 53.514 s | 3.993 s | 76.378 s |

## Storage Table

| k | Kite DB size | KMC DB size | Kite dump size | KMC dump size |
|---|---:|---:|---:|---:|
| 31 | 511,097,578 B | 1,050,840,796 B | 3,244,254,089 B | 3,244,254,089 B |
| 51 | 856,453,717 B | 1,447,625,268 B | 4,881,370,242 B | 4,881,370,242 B |

## Interpretation

### `k = 31`

- Kite count was faster than KMC count.
- Kite dump was faster than KMC dump.
- The Kite database was substantially smaller than the KMC database.

### `k = 51`

- Kite count was faster than KMC count.
- Kite dump was faster than KMC dump.
- The Kite database was substantially smaller than the KMC database.

## Consistency Confirmation

The correctness comparison used the exact TSV outputs from Kite and KMC.

Because `kite dump` and `kmc_dump` are not required to emit rows in the same order, the correct comparison method was:

1. sort both tables by the first column `k-mer`
2. compare row count
3. compare the first column
4. compare the second column `count`

## Consistency Results

| k | Rows compared | Status | Result |
|---|---:|---|---|
| 31 | 95,411,816 | ok | all sorted rows and counts matched |
| 51 | 90,394,653 | ok | all sorted rows and counts matched |

## Important Notes

- The benchmark numbers above are recorded results from the benchmark round and are kept here as a structured summary.
- Temporary benchmark working directories were later cleaned from `target/` to keep the repository tidy.
- The absence of the temporary benchmark artifacts does not change the recorded numbers in this document.
