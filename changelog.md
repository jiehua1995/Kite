# Changelog

## 2026-05-29

### `kite dump`

- Fixed `kite dump -p` progress rendering so the initial progress bar frame is not printed as a blank status line before later updates.
- Kept the count-like live dump status line and made the labels clearer:
  - `dump output ...`
  - `written ...`
  - `elapsed ...`
- Started dump progress only after setup diagnostics are printed, preventing later setup logs from disrupting the live progress bar refresh.

### `kite count`

- Allowed `kite count -o /dev/null` for benchmark and smoke-test runs.
- For non-regular count outputs such as `/dev/null`, count now keeps the counting and writer data path active but skips final `.kite` layout rewrites that require seek/truncate support.
- Improved several user-facing count error messages with clearer causes, actual values, and example fixes.

### Error messages

- Improved `.kite` compatibility errors for multi-input commands so mismatched `k`, `canonical`, `partitions`, and `key_width` are easier to understand.

## 2026-05-28

### `kite dump`

- Kept file output on the single-writer sequential path.
  - Worker threads decode, merge, and format TSV slabs.
  - One writer thread performs `write_all` and `flush` to the output file.
- Added automatic parent-directory creation for dump outputs.
- Kept large-slab streaming writes and segmented file preallocation for file output.
- Replaced the earlier runtime-adaptive dump queue experiment with a fixed startup choice based on estimated TSV output size:
  - small output: `4` slabs
  - medium output: `3` slabs
  - large output: `2` slabs
- Kept dump progress output in a compact `count`-like format so it does not wrap as easily on narrow terminals.

### Dump diagnostics

- Expanded `dump --test-mode` reporting to include more write-path diagnostics:
  - `writer_io`
  - `writer_queue_wait`
  - `peak_inflight_bytes`
  - `peak_inflight_slabs`
  - `writer_write_calls`
  - `avg_write_call_mb`
  - `final_budget_bytes`
- Removed leftover unused code from the abandoned adaptive queue governor path.

### Benchmarking

- Added [benchmark/compare_counters_stdout.sh](/eight_TB/BigMQ/Users/Hua/kmer_counter/kite/benchmark/compare_counters_stdout.sh).
- This benchmark script compares:
  - `kite`
  - `kmc`
  - `jellyfish`
  - `dsk`
- Default benchmark settings:
  - `k = 21, 31, 51`
  - `threads = 64`
  - `count memory = 64G`
  - count databases written under `/ten_TB/Hua/temp`
  - dump measured through `stdout | wc -c` to avoid TSV disk-write bottlenecks
  - `3` runs per command
  - `sync` and `sleep 120` between timed commands
- The script writes:
  - per-command `/usr/bin/time -v` logs
  - `summary.tsv`
  - `summary_mean.tsv`
  - `summary.md`

### Documentation

- Updated `README.md`
- Updated `README_CN.md`

Both README files now document:

- current `kite dump` behavior
- automatic output-directory creation
- fixed startup queue selection for dump
- the current `--test-mode` dump metrics
- stdout-based dump benchmarking via `kite dump -o - | wc -c`
