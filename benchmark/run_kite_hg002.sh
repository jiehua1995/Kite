#!/bin/bash
set -e
cd /eight_TB/BigMQ/Users/Hua/kmer_counter/kite
KITE=./target/release/kite
LOG=benchmark/kite_hg002.log
INPUT=test/HG002_sample.fastq.gz
TMPDIR=temp/kite_tmp

mkdir -p "$TMPDIR" benchmark

for K in 21 31 51; do
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
        -o "temp/HG002_k${K}.kite" \
        2>&1 | tee -a "$LOG"
    echo "=== kite k=${K} count END $(date) ===" | tee -a "$LOG"

    echo "=== kite k=${K} dump START $(date) ===" | tee -a "$LOG"
    /usr/bin/time -v $KITE dump \
        --threads 48 \
        --io-buf-mb 64 \
        --test-mode \
        -i "temp/HG002_k${K}.kite" \
        -o "temp/HG002_k${K}.tsv" \
        2>&1 | tee -a "$LOG"
    echo "=== kite k=${K} dump END $(date) ===" | tee -a "$LOG"
done

echo "=== ALL kite benchmarks done $(date) ===" | tee -a "$LOG"
