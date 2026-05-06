#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

THREADS="${THREADS:-48}"
MEM_GB="${MEM_GB:-64}"
K="${K:-21}"
PARTITIONS="${PARTITIONS:-256}"
INPUT="${INPUT:-}"
KITE_BIN="${KITE_BIN:-$ROOT_DIR/target/release/kite}"
KMC_BIN="${KMC_BIN:-/home/zfp_da03/miniforge3/envs/kmc/bin/kmc}"
KMC_DUMP_BIN="${KMC_DUMP_BIN:-/home/zfp_da03/miniforge3/envs/kmc/bin/kmc_dump}"
RUN_TAG="${RUN_TAG:-record05}"
OUT_DIR="$ROOT_DIR/benchmark/$RUN_TAG"
KMC_TMP_DIR="$OUT_DIR/kmc_tmp"

if [[ -z "$INPUT" ]]; then
    if [[ -f "$ROOT_DIR/test/HG0023_sample.fastq.gz" ]]; then
        INPUT="$ROOT_DIR/test/HG0023_sample.fastq.gz"
    else
        INPUT="$ROOT_DIR/test/HG002_sample.fastq.gz"
    fi
fi

if [[ ! -f "$INPUT" ]]; then
    echo "input not found: $INPUT" >&2
    exit 1
fi

if [[ ! -x "$KITE_BIN" ]]; then
    echo "kite binary not found or not executable: $KITE_BIN" >&2
    exit 1
fi

if [[ ! -x "$KMC_BIN" ]]; then
    echo "kmc binary not found or not executable: $KMC_BIN" >&2
    exit 1
fi

if [[ ! -x "$KMC_DUMP_BIN" ]]; then
    echo "kmc_dump binary not found or not executable: $KMC_DUMP_BIN" >&2
    exit 1
fi

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR" "$KMC_TMP_DIR"

cat > "$OUT_DIR/run.env" <<EOF
ROOT_DIR=$ROOT_DIR
INPUT=$INPUT
THREADS=$THREADS
MEM_GB=$MEM_GB
K=$K
PARTITIONS=$PARTITIONS
KITE_BIN=$KITE_BIN
KMC_BIN=$KMC_BIN
KMC_DUMP_BIN=$KMC_DUMP_BIN
EOF

run_cmd() {
    local label="$1"
    shift
    local stdout_log="$OUT_DIR/${label}.stdout.log"
    local time_log="$OUT_DIR/${label}.time.log"

    echo "==> $label"
    /usr/bin/time -f 'elapsed_seconds=%e\nuser_seconds=%U\nsys_seconds=%S\nmax_rss_kb=%M\nexit_status=%x' \
        -o "$time_log" \
        "$@" \
        2>&1 | tee "$stdout_log"
}

run_cmd kite_count \
    "$KITE_BIN" count \
    -k "$K" \
    -i "$INPUT" \
    -o "$OUT_DIR/kite_k${K}.kite" \
    --threads "$THREADS" \
    --partitions "$PARTITIONS" \
    --max-memory-gb "$MEM_GB" \
    --test-mode

run_cmd kite_dump \
    "$KITE_BIN" dump \
    -i "$OUT_DIR/kite_k${K}.kite" \
    -o "$OUT_DIR/kite_k${K}.tsv" \
    --threads "$THREADS" \
    --test-mode

run_cmd kmc_count \
    "$KMC_BIN" \
    -fq \
    -k"$K" \
    -t"$THREADS" \
    -m"$MEM_GB" \
    -ci1 \
    -cs4294967295 \
    "$INPUT" \
    "$OUT_DIR/kmc_k${K}" \
    "$KMC_TMP_DIR"

run_cmd kmc_dump \
    "$KMC_DUMP_BIN" \
    "$OUT_DIR/kmc_k${K}" \
    "$OUT_DIR/kmc_k${K}.tsv"

echo "benchmark complete: $OUT_DIR"