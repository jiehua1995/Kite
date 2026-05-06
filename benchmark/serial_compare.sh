#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

K="${K:-21}"
THREADS="${THREADS:-48}"
MEM_GB="${MEM_GB:-64}"
PARTITIONS="${PARTITIONS:-256}"
INPUT="${INPUT:-$ROOT_DIR/test/HG002_sample.fastq.gz}"
KITE_BIN="${KITE_BIN:-$ROOT_DIR/target/release/kite}"
KMC_BIN="${KMC_BIN:-/home/zfp_da03/miniforge3/envs/kmc/bin/kmc}"
KMC_DUMP_BIN="${KMC_DUMP_BIN:-/home/zfp_da03/miniforge3/envs/kmc/bin/kmc_dump}"
RUN_NAME="${RUN_NAME:-k${K}_serial_compare}"
LOG_DIR="$ROOT_DIR/benchmark/$RUN_NAME"
LOG_FILE="$LOG_DIR/run.log"

KITE_OUT="$ROOT_DIR/temp/kite_k${K}_${RUN_NAME}.kite"
KITE_TSV="$ROOT_DIR/temp/kite_k${K}_${RUN_NAME}.tsv"
KMC_PREFIX="$ROOT_DIR/temp/kmc_k${K}_${RUN_NAME}"
KMC_TSV="$ROOT_DIR/temp/kmc_k${K}_${RUN_NAME}.tsv"
KMC_TMP_DIR="$ROOT_DIR/temp/kmc_tmp_k${K}_${RUN_NAME}"

mkdir -p "$LOG_DIR" "$ROOT_DIR/temp"
rm -rf "$KITE_OUT" "$KITE_TSV" "$KMC_PREFIX".kmc_pre "$KMC_PREFIX".kmc_suf "$KMC_TSV" "$KMC_TMP_DIR"
mkdir -p "$KMC_TMP_DIR"

if [[ ! -f "$INPUT" ]]; then
    echo "input not found: $INPUT" >&2
    exit 1
fi

if [[ ! -x "$KITE_BIN" ]]; then
    echo "kite binary not executable: $KITE_BIN" >&2
    exit 1
fi

if [[ ! -x "$KMC_BIN" || ! -x "$KMC_DUMP_BIN" ]]; then
    echo "kmc or kmc_dump not executable" >&2
    exit 1
fi

exec > >(tee -a "$LOG_FILE") 2>&1

echo "=== serial compare start $(date) ==="
echo "root=$ROOT_DIR"
echo "input=$INPUT"
echo "k=$K threads=$THREADS mem_gb=$MEM_GB partitions=$PARTITIONS"
echo "kite_bin=$KITE_BIN"
echo "kmc_bin=$KMC_BIN"
echo "kmc_dump_bin=$KMC_DUMP_BIN"
echo

run_step() {
    local label="$1"
    shift
    echo "=== $label START $(date) ==="
    /usr/bin/time -v "$@"
    echo "=== $label END $(date) ==="
    echo
}

run_step "KMC count k=$K" \
    "$KMC_BIN" \
    -fq \
    -k"$K" \
    -t"$THREADS" \
    -m"$MEM_GB" \
    -ci1 \
    -cs4294967295 \
    "$INPUT" \
    "$KMC_PREFIX" \
    "$KMC_TMP_DIR"

run_step "KMC dump k=$K" \
    "$KMC_DUMP_BIN" \
    "$KMC_PREFIX" \
    "$KMC_TSV"

run_step "Kite count k=$K" \
    "$KITE_BIN" count \
    -k "$K" \
    -i "$INPUT" \
    -o "$KITE_OUT" \
    --threads "$THREADS" \
    --max-memory-gb "$MEM_GB" \
    --partitions "$PARTITIONS" \
    --mode fast \
    --canonical \
    --test-mode

run_step "Kite dump k=$K" \
    "$KITE_BIN" dump \
    -i "$KITE_OUT" \
    -o "$KITE_TSV" \
    --threads "$THREADS" \
    --test-mode

echo "=== serial compare end $(date) ==="
echo "log_file=$LOG_FILE"