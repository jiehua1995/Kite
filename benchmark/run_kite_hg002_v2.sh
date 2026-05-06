#!/bin/bash
# kite HG002 benchmark v2
# Key improvements over v1:
#   1. Write .kite files to /dev/shm (RAM, 60 GB/s) — eliminates single-writer NVMe bottleneck
#   2. Read .kite from /dev/shm for dump — eliminates NVMe read I/O contention
#   3. Use new binary with parallel final assembly in dump
#   4. Run cleanly without competing I/O from KMC
set -e
cd /eight_TB/BigMQ/Users/Hua/kmer_counter/kite
KITE=./target/release/kite
LOG=benchmark/kite_hg002_v2.log
INPUT=test/HG002_sample.fastq.gz
SHM=/dev/shm/kite_hg002_$$   # process-unique prefix on /dev/shm

mkdir -p benchmark
mkdir -p "$SHM"
echo "=== kite HG002 benchmark v2 START $(date) ===" | tee -a "$LOG"
echo "  Binary: $(./target/release/kite --version 2>/dev/null || echo unknown)" | tee -a "$LOG"
echo "  /dev/shm free: $(df -h /dev/shm | tail -1 | awk '{print $4}')" | tee -a "$LOG"
echo "" | tee -a "$LOG"

for K in 21 31 51; do
    # ── COUNT: write .kite to /dev/shm (RAM) to avoid NVMe write bottleneck ──
    echo "=== kite k=${K} count START $(date) ===" | tee -a "$LOG"
    /usr/bin/time -v $KITE count \
        -k $K \
        --threads 48 \
        --max-memory-gb 64 \
        --partitions 256 \
        --mode fast \
        --canonical \
        --test-mode \
        -i "$INPUT" \
        -o "$SHM/HG002_k${K}.kite" \
        2>&1 | tee -a "$LOG"
    echo "=== kite k=${K} count END $(date) ===" | tee -a "$LOG"

    echo "  .kite size: $(ls -lh $SHM/HG002_k${K}.kite | awk '{print $5}')" | tee -a "$LOG"

    # ── DUMP: read from /dev/shm (RAM), write TSV to NVMe ──────────────────
    echo "=== kite k=${K} dump START $(date) ===" | tee -a "$LOG"
    /usr/bin/time -v $KITE dump \
        --threads 48 \
        --io-buf-mb 64 \
        --test-mode \
        -i "$SHM/HG002_k${K}.kite" \
        -o "temp/HG002_k${K}_v2.tsv" \
        2>&1 | tee -a "$LOG"
    echo "=== kite k=${K} dump END $(date) ===" | tee -a "$LOG"

    # ── Cleanup /dev/shm  (free RAM before next k) ─────────────────────────
    rm -f "$SHM/HG002_k${K}.kite"
    echo "" | tee -a "$LOG"
done

rmdir "$SHM" 2>/dev/null || true
echo "=== ALL kite v2 benchmarks done $(date) ===" | tee -a "$LOG"
