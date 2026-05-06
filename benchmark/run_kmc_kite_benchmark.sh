#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

INPUT="${INPUT:-$ROOT_DIR/test/HG002_sample.fastq.gz}"
THREADS="${THREADS:-48}"
MEM_GB="${MEM_GB:-64}"
K_LIST="${K_LIST:-21 31 51}"
PARTITIONS="${PARTITIONS:-0}"
CHUNK_KMERS="${CHUNK_KMERS:-8000000}"

KITE_BIN="${KITE_BIN:-$ROOT_DIR/target/release/kite}"
KMC_BIN="${KMC_BIN:-/home/zfp_da03/miniforge3/envs/kmc/bin/kmc}"
KMC_DUMP_BIN="${KMC_DUMP_BIN:-/home/zfp_da03/miniforge3/envs/kmc/bin/kmc_dump}"

RUN_TAG="${RUN_TAG:-$(date +%Y%m%d_%H%M%S)}"
OUT_DIR="${OUT_DIR:-$ROOT_DIR/benchmark/results/$RUN_TAG}"
TEMP_DIR="${TEMP_DIR:-$ROOT_DIR/temp/bench_$RUN_TAG}"

mkdir -p "$OUT_DIR" "$TEMP_DIR"

if [[ ! -f "$INPUT" ]]; then
    echo "input not found: $INPUT" >&2
    exit 1
fi

if [[ ! -x "$KITE_BIN" ]]; then
    echo "kite binary not executable: $KITE_BIN" >&2
    echo "hint: cargo build --release" >&2
    exit 1
fi

if [[ ! -x "$KMC_BIN" ]]; then
    echo "kmc binary not executable: $KMC_BIN" >&2
    exit 1
fi

if [[ ! -x "$KMC_DUMP_BIN" ]]; then
    echo "kmc_dump binary not executable: $KMC_DUMP_BIN" >&2
    exit 1
fi

summary_file="$OUT_DIR/summary.tsv"
printf 'tool\tstage\tk\twall_seconds\tmax_rss_mb\toutput_gb\tthroughput_mb_s\n' > "$summary_file"

parse_elapsed_seconds() {
    local raw="$1"
    awk -v value="$raw" '
        function to_seconds(text, n, parts, sec) {
            n = split(text, parts, ":")
            sec = 0
            if (n == 3) {
                sec = parts[1] * 3600 + parts[2] * 60 + parts[3]
            } else if (n == 2) {
                sec = parts[1] * 60 + parts[2]
            } else {
                sec = parts[1]
            }
            return sec
        }
        BEGIN { printf "%.3f", to_seconds(value) }
    '
}

extract_wall_seconds() {
    local time_file="$1"
    local raw
    raw="$(awk -F': ' '/Elapsed \(wall clock\) time/ {print $2}' "$time_file")"
    parse_elapsed_seconds "$raw"
}

extract_max_rss_mb() {
    local time_file="$1"
    awk -F': ' '/Maximum resident set size/ {printf "%.1f", $2 / 1024.0}' "$time_file"
}

output_gb() {
    local path="$1"
    if [[ ! -f "$path" ]]; then
        printf '0.000'
        return
    fi
    awk -v bytes="$(stat -c '%s' "$path")" 'BEGIN { printf "%.3f", bytes / 1024 / 1024 / 1024 }'
}

throughput_mb_s() {
    local bytes="$1"
    local wall="$2"
    awk -v b="$bytes" -v s="$wall" 'BEGIN { if (s <= 0) printf "0.0"; else printf "%.1f", (b / 1024 / 1024) / s }'
}

run_and_capture() {
    local label="$1"
    local stdout_file="$2"
    local stderr_file="$3"
    local time_file="$4"
    shift 4

    echo
    echo "===== $label ====="
    echo "command: $*"

    /usr/bin/time -v -o "$time_file" "$@" \
        > >(tee "$stdout_file") \
        2> >(tee "$stderr_file" >&2)
}

record_summary() {
    local tool="$1"
    local stage="$2"
    local k="$3"
    local time_file="$4"
    local output_path="$5"

    local wall rss bytes gb mbps
    wall="$(extract_wall_seconds "$time_file")"
    rss="$(extract_max_rss_mb "$time_file")"
    bytes="0"
    if [[ -f "$output_path" ]]; then
        bytes="$(stat -c '%s' "$output_path")"
    fi
    gb="$(output_gb "$output_path")"
    mbps="$(throughput_mb_s "$bytes" "$wall")"

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$tool" "$stage" "$k" "$wall" "$rss" "$gb" "$mbps" >> "$summary_file"

    echo "summary: tool=$tool stage=$stage k=$k wall=${wall}s rss=${rss}MB output=${gb}GB throughput=${mbps}MB/s"
}

echo "root_dir=$ROOT_DIR"
echo "input=$INPUT"
echo "threads=$THREADS"
echo "mem_gb=$MEM_GB"
echo "k_list=$K_LIST"
echo "partitions=$PARTITIONS"
echo "chunk_kmers=$CHUNK_KMERS"
echo "out_dir=$OUT_DIR"
echo "temp_dir=$TEMP_DIR"

for k in $K_LIST; do
    kmc_prefix="$TEMP_DIR/kmc_k${k}"
    kmc_tsv="$TEMP_DIR/kmc_k${k}.tsv"
    kmc_tmp_dir="$TEMP_DIR/kmc_tmp_k${k}"
    kite_db="$TEMP_DIR/kite_k${k}.kite"
    kite_tsv="$TEMP_DIR/kite_k${k}.tsv"

    rm -rf "$kmc_prefix".kmc_pre "$kmc_prefix".kmc_suf "$kmc_tsv" "$kmc_tmp_dir" "$kite_db" "$kite_tsv"
    mkdir -p "$kmc_tmp_dir"

    run_and_capture \
        "KMC count k=$k" \
        "$OUT_DIR/kmc_k${k}.count.stdout" \
        "$OUT_DIR/kmc_k${k}.count.stderr" \
        "$OUT_DIR/kmc_k${k}.count.time" \
        "$KMC_BIN" \
        -fq \
        -k"$k" \
        -t"$THREADS" \
        -m"$MEM_GB" \
        -ci1 \
        -cs4294967295 \
        "$INPUT" \
        "$kmc_prefix" \
        "$kmc_tmp_dir"
    record_summary "kmc" "count" "$k" "$OUT_DIR/kmc_k${k}.count.time" "$kmc_prefix.kmc_pre"

    run_and_capture \
        "KMC dump k=$k" \
        "$OUT_DIR/kmc_k${k}.dump.stdout" \
        "$OUT_DIR/kmc_k${k}.dump.stderr" \
        "$OUT_DIR/kmc_k${k}.dump.time" \
        "$KMC_DUMP_BIN" \
        "$kmc_prefix" \
        "$kmc_tsv"
    record_summary "kmc" "dump" "$k" "$OUT_DIR/kmc_k${k}.dump.time" "$kmc_tsv"

    kite_count_cmd=(
        "$KITE_BIN" count
        -k "$k"
        -i "$INPUT"
        -o "$kite_db"
        --threads "$THREADS"
        --max-memory-gb "$MEM_GB"
        --chunk-kmers "$CHUNK_KMERS"
        --mode fast
        --canonical
    )
    if [[ "$PARTITIONS" != "0" ]]; then
        kite_count_cmd+=(--partitions "$PARTITIONS")
    fi

    run_and_capture \
        "Kite count k=$k" \
        "$OUT_DIR/kite_k${k}.count.stdout" \
        "$OUT_DIR/kite_k${k}.count.stderr" \
        "$OUT_DIR/kite_k${k}.count.time" \
        "${kite_count_cmd[@]}"
    record_summary "kite" "count" "$k" "$OUT_DIR/kite_k${k}.count.time" "$kite_db"

    run_and_capture \
        "Kite dump k=$k" \
        "$OUT_DIR/kite_k${k}.dump.stdout" \
        "$OUT_DIR/kite_k${k}.dump.stderr" \
        "$OUT_DIR/kite_k${k}.dump.time" \
        "$KITE_BIN" dump \
        -i "$kite_db" \
        -o "$kite_tsv" \
        --threads "$THREADS"
    record_summary "kite" "dump" "$k" "$OUT_DIR/kite_k${k}.dump.time" "$kite_tsv"
done

echo
echo "===== benchmark summary ====="
column -t -s $'\t' "$summary_file"
echo
echo "all logs saved under: $OUT_DIR"