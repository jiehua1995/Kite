use super::fastq::open_seq_reader;
use super::kmer::{INVALID_BASE, KmerKey, KmerMode, RollingKmer, encode_base, words_for_k};
use super::merge::{
    ChunkPacked, ChunkU64, ChunkU128, merge_chunks_packed, merge_chunks_u64, merge_chunks_u128,
};
use super::minimizer::{RollingMinimizerU64, SuperKmerBuilder};
use super::radix_sort::{radix_sort_u64, radix_sort_u128};
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, bounded};
use rayon::prelude::*;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

// 中文：这里是 `count` 的核心执行器，负责并行读取、滚动编码、按 minimizer
// 分桶、分块排序、局部压缩计数和最后的全局归并。
// English: This is the core `count` executor. It handles parallel reading,
// rolling encoding, minimizer-based partitioning, chunk sorting, local
// counting, and final global merge.
#[derive(Debug, Clone)]
pub struct CountConfig {
    pub k: usize,
    pub minimizer: usize,
    pub partitions: usize,
    pub threads: usize,
    pub chunk_kmers: usize,
    pub canonical: bool,
    pub progress: bool,
    pub progress_counters: Option<Arc<ProgressCounters>>,
}

#[derive(Debug, Clone)]
/// 中文：`count` 内部的统一结果表类型，按 `k` 自动切换编码。
/// English: Unified result-table type used by `count`, switching encoding
/// automatically based on `k`.
pub enum CountTable {
    U64(Vec<(u64, u64)>),
    U128(Vec<(u128, u64)>),
    Packed(Vec<(Vec<u64>, u64)>),
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
/// 中文：计数流程各阶段的粗粒度耗时拆分。
/// English: Coarse-grained timing breakdown for the counting pipeline stages.
pub struct TimingBreakdown {
    pub parsing: Duration,
    pub kmer_generation: Duration,
    pub minimizer_partition: Duration,
    pub sorting: Duration,
    pub merging: Duration,
    pub total: Duration,
}

#[derive(Debug, Clone)]
/// 中文：`count` 产出的完整结果，包含编码模式、计数表、统计量和耗时。
/// English: Complete `count` result including encoding mode, count table,
/// summary stats, and timing information.
pub struct CountOutcome {
    pub mode: KmerMode,
    pub table: CountTable,
    pub total_kmers: u64,
    pub total_reads: u64,
    pub canonical: bool,
    pub timings: TimingBreakdown,
}

#[derive(Debug)]
/// 中文：低开销进度计数器，供进度线程异步读取。
/// English: Low-overhead progress counters read asynchronously by the progress
/// reporting thread.
pub struct ProgressCounters {
    pub processed_reads: AtomicU64,
    pub processed_kmers: AtomicU64,
}

impl ProgressCounters {
    /// 中文：创建新的进度计数器，初始值全为 0。
    /// English: Create a new progress counter set initialized to zero.
    pub fn new() -> Self {
        Self {
            processed_reads: AtomicU64::new(0),
            processed_kmers: AtomicU64::new(0),
        }
    }
}

#[derive(Default)]
/// 中文：单个 worker 内部的耗时统计。
/// English: Per-worker timing statistics.
struct WorkerTimings {
    kmer_generation: Duration,
    minimizer_partition: Duration,
    sorting: Duration,
}

#[derive(Default)]
/// 中文：单个 worker 的局部输出，稍后会被主线程统一归并。
/// English: Local output produced by one worker, later merged by the main
/// thread.
struct WorkerOutput {
    chunks_u64: Vec<Vec<ChunkU64>>,
    chunks_u128: Vec<Vec<ChunkU128>>,
    chunks_packed: Vec<Vec<ChunkPacked>>,
    total_kmers: u64,
    total_reads: u64,
    timings: WorkerTimings,
}

/// 中文：生产者发给 worker 的一批序列，使用扁平字节数组加 offset
/// 以减少小对象分配。
/// English: A batch of sequences sent from the producer to a worker, stored as
/// a flat byte array plus offsets to reduce small-object allocation.
struct SeqBatch {
    data: Vec<u8>,
    offsets: Vec<(u32, u32)>,
}

impl SeqBatch {
    /// 中文：按给定 reads/bases 容量创建批次缓冲区。
    /// English: Create a sequence batch with the requested read/base capacity.
    fn with_capacity(reads: usize, bases: usize) -> Self {
        Self {
            data: Vec::with_capacity(bases),
            offsets: Vec::with_capacity(reads),
        }
    }

    #[inline(always)]
    /// 中文：把一条序列追加到批次中。
    /// English: Append one sequence to the batch.
    fn push(&mut self, seq: &[u8]) {
        let start = self.data.len() as u32;
        self.data.extend_from_slice(seq);
        self.offsets.push((start, seq.len() as u32));
    }

    #[inline(always)]
    /// 中文：清空批次内容但保留容量。
    /// English: Clear the batch while keeping allocated capacity.
    fn clear(&mut self) {
        self.data.clear();
        self.offsets.clear();
    }

    #[inline(always)]
    /// 中文：判断批次是否为空。
    /// English: Test whether the batch is empty.
    fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    #[inline(always)]
    /// 中文：返回批次中的 read 数。
    /// English: Return the number of reads stored in the batch.
    fn len(&self) -> usize {
        self.offsets.len()
    }
}

/// 中文：校验 `count` 配置是否合法。
/// English: Validate that the `count` configuration is internally consistent.
pub fn validate_cfg(cfg: &CountConfig) -> Result<()> {
    if cfg.k == 0 {
        bail!("k must be > 0");
    }
    if cfg.minimizer == 0 || cfg.minimizer > cfg.k {
        bail!("minimizer m must satisfy 1 <= m <= k");
    }
    if cfg.minimizer > 32 {
        bail!("current minimizer implementation requires m <= 32");
    }
    if cfg.partitions == 0 {
        bail!("partitions must be > 0");
    }
    if cfg.threads == 0 {
        bail!("threads must be > 0");
    }
    Ok(())
}

/// 中文：优化版计数主流程。输入是 FASTQ 路径和配置，输出是已归并的计数结果。
/// English: Optimized counting pipeline. It takes a FASTQ path and config and
/// returns the fully merged count result.
pub fn count_optimized(path: &Path, cfg: &CountConfig) -> Result<CountOutcome> {
    validate_cfg(cfg)?;
    let mode = KmerMode::from_k(cfg.k);

    let t_total = Instant::now();
    let t_parse_start = Instant::now();

    // spawn a lightweight progress printer if requested
    let progress_printer_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut progress_printer = None;
    if cfg.progress {
        if let Some(pc) = cfg.progress_counters.clone() {
            let done = progress_printer_done.clone();
            progress_printer = Some(std::thread::spawn(move || {
                use std::io::Write;
                let spinner = ["-", "\\", "|", "/"];
                let start = Instant::now();
                let mut i: usize = 0;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    let reads = pc
                        .processed_reads
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let kmers = pc
                        .processed_kmers
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let elapsed = start.elapsed().as_secs_f64().max(1e-6);
                    let rate = (kmers as f64) / elapsed;
                    let s = format!(
                        "\r[kite count] {} reads={} kmers={} rate={:.1}/s",
                        spinner[i % spinner.len()],
                        reads,
                        kmers,
                        rate
                    );
                    i = i.wrapping_add(1);
                    let _ = std::io::stderr().write_all(s.as_bytes());
                    let _ = std::io::stderr().flush();
                    std::thread::sleep(Duration::from_millis(500));
                }
                // clear the progress line
                let clear = "\r".to_string() + &" ".repeat(80) + "\r";
                let _ = std::io::stderr().write_all(clear.as_bytes());
                let _ = std::io::stderr().flush();
            }));
        }
    }

    let mut senders = Vec::with_capacity(cfg.threads);
    let mut workers = Vec::with_capacity(cfg.threads);
    // increase channel buffering to better decouple producer/consumer
    let queue_cap = (cfg.threads * 8).max(8);

    for _ in 0..cfg.threads {
        let (tx, rx) = bounded::<SeqBatch>(queue_cap);
        senders.push(tx);

        let cfg_cloned = cfg.clone();
        workers.push(thread::spawn(move || worker_loop(rx, &cfg_cloned, mode)));
    }

    let mut reader = open_seq_reader(path)?;
    let mut seq = Vec::with_capacity(256);
    let mut batch = SeqBatch::with_capacity(4096, 1 << 20);
    let mut rr = 0usize;
    // local progress counters reference (may be None)
    let _progress_ref = cfg.progress_counters.clone();

    while reader.next_sequence(&mut seq)? {
        batch.push(&seq);
        if batch.len() >= 4096 || batch.data.len() >= (4 << 20) {
            let tx: &Sender<SeqBatch> = &senders[rr % senders.len()];
            let mut out = SeqBatch::with_capacity(batch.offsets.capacity(), batch.data.capacity());
            std::mem::swap(&mut out, &mut batch);
            tx.send(out).context("failed to send batch to worker")?;
            batch.clear();
            rr += 1;
        }
    }
    if !batch.is_empty() {
        let tx: &Sender<SeqBatch> = &senders[rr % senders.len()];
        tx.send(batch).context("failed to send final batch")?;
    }

    drop(senders);
    let parsing = t_parse_start.elapsed();

    let mut outputs = Vec::with_capacity(workers.len());
    for w in workers {
        outputs.push(
            w.join()
                .map_err(|_| anyhow::anyhow!("worker thread panicked"))??,
        );
    }

    // stop progress printer if running
    if cfg.progress {
        progress_printer_done.store(true, Ordering::Relaxed);
        if let Some(h) = progress_printer {
            let _ = h.join();
        }
    }

    let mut kmer_generation = Duration::ZERO;
    let mut minimizer_partition = Duration::ZERO;
    let mut sorting = Duration::ZERO;
    let mut total_kmers = 0u64;
    let mut total_reads = 0u64;

    for o in &outputs {
        kmer_generation += o.timings.kmer_generation;
        minimizer_partition += o.timings.minimizer_partition;
        sorting += o.timings.sorting;
        total_kmers += o.total_kmers;
        total_reads += o.total_reads;
    }

    let t_merge = Instant::now();
    let table = match mode {
        KmerMode::U64 => {
            let mut by_partition = vec![Vec::<ChunkU64>::new(); cfg.partitions];
            for out in outputs {
                for (p, chunks) in out.chunks_u64.into_iter().enumerate() {
                    by_partition[p].extend(chunks);
                }
            }
            let mut merged_parts: Vec<Vec<(u64, u64)>> = by_partition
                .into_par_iter()
                .map(|part| merge_chunks_u64(part))
                .collect();
            let total_len: usize = merged_parts.iter().map(Vec::len).sum();
            let mut all = Vec::with_capacity(total_len);
            for part in merged_parts.drain(..) {
                all.extend(part);
            }
            CountTable::U64(all)
        }
        KmerMode::U128 => {
            let mut by_partition = vec![Vec::<ChunkU128>::new(); cfg.partitions];
            for out in outputs {
                for (p, chunks) in out.chunks_u128.into_iter().enumerate() {
                    by_partition[p].extend(chunks);
                }
            }
            let mut merged_parts: Vec<Vec<(u128, u64)>> = by_partition
                .into_par_iter()
                .map(|part| merge_chunks_u128(part))
                .collect();
            let total_len: usize = merged_parts.iter().map(Vec::len).sum();
            let mut all = Vec::with_capacity(total_len);
            for part in merged_parts.drain(..) {
                all.extend(part);
            }
            CountTable::U128(all)
        }
        KmerMode::Packed(_) => {
            let mut by_partition = vec![Vec::<ChunkPacked>::new(); cfg.partitions];
            for out in outputs {
                for (p, chunks) in out.chunks_packed.into_iter().enumerate() {
                    by_partition[p].extend(chunks);
                }
            }
            let mut merged_parts: Vec<Vec<(Vec<u64>, u64)>> = by_partition
                .into_par_iter()
                .map(merge_chunks_packed)
                .collect();
            let total_len: usize = merged_parts.iter().map(Vec::len).sum();
            let mut all = Vec::with_capacity(total_len);
            for part in merged_parts.drain(..) {
                all.extend(part);
            }
            CountTable::Packed(all)
        }
    };

    let merging = t_merge.elapsed();
    let total = t_total.elapsed();

    // total_reads was accumulated from per-worker outputs above; if zero, fall back to progress counter
    let total_reads = if total_reads == 0 {
        cfg.progress_counters
            .as_ref()
            .map(|c| c.processed_reads.load(Ordering::Relaxed))
            .unwrap_or(0)
    } else {
        total_reads
    };

    Ok(CountOutcome {
        mode,
        table,
        total_kmers,
        total_reads,
        canonical: cfg.canonical,
        timings: TimingBreakdown {
            parsing,
            kmer_generation,
            minimizer_partition,
            sorting,
            merging,
            total,
        },
    })
}

const PAR_SORT_THRESHOLD: usize = 1 << 14; // use parallel sort for large slices (lowered)

/// 中文：单个 worker 的执行循环。输入是一串序列批次，输出是该 worker 的局部 chunk。
/// English: Worker execution loop. It consumes sequence batches and produces
/// local sorted/count-compressed chunks for one worker.
fn worker_loop(rx: Receiver<SeqBatch>, cfg: &CountConfig, mode: KmerMode) -> Result<WorkerOutput> {
    let mut out = WorkerOutput {
        chunks_u64: vec![Vec::new(); cfg.partitions],
        chunks_u128: vec![Vec::new(); cfg.partitions],
        chunks_packed: vec![Vec::new(); cfg.partitions],
        total_kmers: 0,
        total_reads: 0,
        timings: WorkerTimings::default(),
    };

    let mut roll = RollingKmer::new(cfg.k, cfg.canonical);
    let mut rolling_min = RollingMinimizerU64::new(cfg.k, cfg.minimizer);

    // atomic counters reference for this worker (clone of Arc)
    let progress_ref = cfg.progress_counters.clone();
    let mut buffers_u64 =
        vec![Vec::<u64>::with_capacity(cfg.chunk_kmers / cfg.partitions + 8); cfg.partitions];
    let mut buffers_u128 =
        vec![Vec::<u128>::with_capacity(cfg.chunk_kmers / cfg.partitions + 8); cfg.partitions];
    let mut buffers_packed = vec![
        std::collections::VecDeque::<Vec<u64>>::with_capacity(
            cfg.chunk_kmers / cfg.partitions + 8,
        );
        cfg.partitions
    ];

    // per-partition totals used to decide when to flush
    let mut buffers_u64_total = vec![0usize; cfg.partitions];
    let mut buffers_u128_total = vec![0usize; cfg.partitions];
    let mut buffers_packed_total = vec![0usize; cfg.partitions];
    let mut scratch_u64 = Vec::<u64>::with_capacity(cfg.chunk_kmers);
    let mut scratch_u128 = Vec::<u128>::with_capacity(cfg.chunk_kmers);

    let mut super_builder = SuperKmerBuilder::with_capacity(512);

    let partitions_pow2 = cfg.partitions.is_power_of_two();
    let partition_shift = if partitions_pow2 {
        64usize - (cfg.partitions.trailing_zeros() as usize)
    } else {
        0usize
    };

    while let Ok(batch) = rx.recv() {
        let t_batch_k = Instant::now();
        for &(start, len) in &batch.offsets {
            if let Some(pc) = &progress_ref {
                pc.processed_reads.fetch_add(1, Ordering::Relaxed);
            }
            // always increment worker-local read counter
            out.total_reads += 1;
            let s = start as usize;
            let e = s + len as usize;
            let seq = &batch.data[s..e];
            roll.reset();
            rolling_min.reset();
            for &ch in seq {
                let b = encode_base(ch);
                if b == INVALID_BASE {
                    super_builder.finish(|m, current| {
                        flush_super(
                            mode,
                            m,
                            current,
                            cfg,
                            partitions_pow2,
                            partition_shift,
                            &mut buffers_u64,
                            &mut buffers_u128,
                            &mut buffers_packed,
                            &mut buffers_u64_total,
                            &mut buffers_u128_total,
                            &mut buffers_packed_total,
                            &mut scratch_u64,
                            &mut scratch_u128,
                            &mut out,
                        );
                    });
                    roll.reset();
                    rolling_min.reset();
                    continue;
                }

                let key = roll.push_encoded(b);
                let minz = rolling_min.push_encoded(b);

                if let Some(kmer) = key {
                    out.total_kmers += 1;
                    if let Some(pc) = &progress_ref {
                        pc.processed_kmers.fetch_add(1, Ordering::Relaxed);
                    }
                    let minz = minz.expect("rolling minimizer and k-mer windows must align");

                    super_builder.push(minz, kmer, |m, current| {
                        flush_super(
                            mode,
                            m,
                            current,
                            cfg,
                            partitions_pow2,
                            partition_shift,
                            &mut buffers_u64,
                            &mut buffers_u128,
                            &mut buffers_packed,
                            &mut buffers_u64_total,
                            &mut buffers_u128_total,
                            &mut buffers_packed_total,
                            &mut scratch_u64,
                            &mut scratch_u128,
                            &mut out,
                        );
                    });
                }
            }
            super_builder.finish(|m, current| {
                flush_super(
                    mode,
                    m,
                    current,
                    cfg,
                    partitions_pow2,
                    partition_shift,
                    &mut buffers_u64,
                    &mut buffers_u128,
                    &mut buffers_packed,
                    &mut buffers_u64_total,
                    &mut buffers_u128_total,
                    &mut buffers_packed_total,
                    &mut scratch_u64,
                    &mut scratch_u128,
                    &mut out,
                );
            });
        }
        out.timings.kmer_generation += t_batch_k.elapsed();
    }

    for p in 0..cfg.partitions {
        flush_partition(
            mode,
            p,
            cfg.partitions,
            partitions_pow2,
            partition_shift,
            &mut buffers_u64,
            &mut buffers_u128,
            &mut buffers_packed,
            &mut buffers_u64_total,
            &mut buffers_u128_total,
            &mut buffers_packed_total,
            &mut scratch_u64,
            &mut scratch_u128,
            &mut out,
        );
    }

    Ok(out)
}

/// 中文：把一个 super-k-mer 批次分发到对应 partition 的缓冲区。
/// 如果某个 partition 达到阈值，就立即 flush。
/// English: Route one super-k-mer batch into partition buffers. If one
/// partition reaches its threshold, flush it immediately.
fn flush_super(
    mode: KmerMode,
    _minimizer: u64,
    current: &mut Vec<KmerKey>,
    cfg: &CountConfig,
    partitions_pow2: bool,
    partition_shift: usize,
    buffers_u64: &mut [Vec<u64>],
    buffers_u128: &mut [Vec<u128>],
    buffers_packed: &mut [VecDeque<Vec<u64>>],
    buffers_u64_total: &mut [usize],
    buffers_u128_total: &mut [usize],
    buffers_packed_total: &mut [usize],
    _scratch_u64: &mut Vec<u64>,
    _scratch_u128: &mut Vec<u128>,
    out: &mut WorkerOutput,
) {
    if current.is_empty() {
        return;
    }
    match mode {
        KmerMode::U64 => {
            for k in current.drain(..) {
                if let KmerKey::U64(v) = k {
                    let part = partition_u64(v, cfg.partitions, partitions_pow2, partition_shift);
                    buffers_u64[part].push(v);
                    buffers_u64_total[part] += 1;
                    if buffers_u64_total[part] >= cfg.chunk_kmers {
                        flush_partition(
                            mode,
                            part,
                            cfg.partitions,
                            partitions_pow2,
                            partition_shift,
                            buffers_u64,
                            buffers_u128,
                            buffers_packed,
                            buffers_u64_total,
                            buffers_u128_total,
                            buffers_packed_total,
                            _scratch_u64,
                            _scratch_u128,
                            out,
                        );
                    }
                }
            }
        }
        KmerMode::U128 => {
            for k in current.drain(..) {
                if let KmerKey::U128(v) = k {
                    let part = partition_u128(v, cfg.partitions, partitions_pow2, partition_shift);
                    buffers_u128[part].push(v);
                    buffers_u128_total[part] += 1;
                    if buffers_u128_total[part] >= cfg.chunk_kmers {
                        flush_partition(
                            mode,
                            part,
                            cfg.partitions,
                            partitions_pow2,
                            partition_shift,
                            buffers_u64,
                            buffers_u128,
                            buffers_packed,
                            buffers_u64_total,
                            buffers_u128_total,
                            buffers_packed_total,
                            _scratch_u64,
                            _scratch_u128,
                            out,
                        );
                    }
                }
            }
        }
        KmerMode::Packed(_) => {
            for k in current.drain(..) {
                if let KmerKey::Packed(v) = k {
                    let part = partition_packed(
                        &v.words,
                        cfg.partitions,
                        partitions_pow2,
                        partition_shift,
                    );
                    // move SmallVec into a Vec to avoid an extra allocation/copy
                    let vec = v.words.into_vec();
                    buffers_packed[part].push_back(vec);
                    buffers_packed_total[part] += 1;
                    if buffers_packed_total[part] >= cfg.chunk_kmers {
                        flush_partition(
                            mode,
                            part,
                            cfg.partitions,
                            partitions_pow2,
                            partition_shift,
                            buffers_u64,
                            buffers_u128,
                            buffers_packed,
                            buffers_u64_total,
                            buffers_u128_total,
                            buffers_packed_total,
                            _scratch_u64,
                            _scratch_u128,
                            out,
                        );
                    }
                }
            }
        }
    }
}

#[inline(always)]
/// 中文：为 `u64` 键选择 partition，下游用它把工作分散到多个桶。
/// English: Choose the target partition for a `u64` key so downstream work can
/// be spread across buckets.
fn partition_u64(
    v: u64,
    partitions: usize,
    partitions_pow2: bool,
    partition_shift: usize,
) -> usize {
    if partitions_pow2 {
        // partitions = 2^p -> take high p bits: v >> (64 - p)
        return (v >> partition_shift) as usize;
    }
    (((v as u128) * (partitions as u128)) >> 64) as usize
}

#[inline(always)]
/// 中文：为 `u128` 键选择 partition，当前使用高 64 位近似分桶。
/// English: Choose the target partition for a `u128` key, currently using the
/// high 64 bits as the partition proxy.
fn partition_u128(
    v: u128,
    partitions: usize,
    partitions_pow2: bool,
    partition_shift: usize,
) -> usize {
    partition_u64(
        (v >> 64) as u64,
        partitions,
        partitions_pow2,
        partition_shift,
    )
}

#[inline(always)]
/// 中文：为 packed 长 k-mer 选择 partition，当前使用最高 word 近似分桶。
/// English: Choose the target partition for a packed long k-mer, currently
/// using the highest word as the partition proxy.
fn partition_packed(
    words: &[u64],
    partitions: usize,
    partitions_pow2: bool,
    partition_shift: usize,
) -> usize {
    partition_u64(
        words.last().copied().unwrap_or(0),
        partitions,
        partitions_pow2,
        partition_shift,
    )
}

/// 中文：把某个 partition 的缓冲区排序并压缩成 `(key,count)` chunk。
/// English: Sort one partition buffer and compress it into a `(key, count)`
/// chunk.
fn flush_partition(
    mode: KmerMode,
    part: usize,
    _partitions: usize,
    _partitions_pow2: bool,
    _partition_shift: usize,
    buffers_u64: &mut [Vec<u64>],
    buffers_u128: &mut [Vec<u128>],
    buffers_packed: &mut [VecDeque<Vec<u64>>],
    buffers_u64_total: &mut [usize],
    buffers_u128_total: &mut [usize],
    buffers_packed_total: &mut [usize],
    scratch_u64: &mut Vec<u64>,
    scratch_u128: &mut Vec<u128>,
    out: &mut WorkerOutput,
) {
    match mode {
        KmerMode::U64 => {
            let src = &mut buffers_u64[part];
            if src.is_empty() {
                return;
            }
            let t = Instant::now();
            if src.len() >= PAR_SORT_THRESHOLD {
                src.par_sort_unstable();
            } else {
                radix_sort_u64(src, scratch_u64);
            }
            let mut kmers = Vec::new();
            let mut counts = Vec::new();
            let mut i = 0usize;
            while i < src.len() {
                let key = src[i];
                let mut c = 1u32;
                i += 1;
                while i < src.len() && src[i] == key {
                    c = c.wrapping_add(1);
                    i += 1;
                }
                kmers.push(key);
                counts.push(c);
            }
            src.clear();
            buffers_u64_total[part] = 0;
            out.chunks_u64[part].push(ChunkU64 { kmers, counts });
            out.timings.sorting += t.elapsed();
        }
        KmerMode::U128 => {
            let src = &mut buffers_u128[part];
            if src.is_empty() {
                return;
            }
            let t = Instant::now();
            if src.len() >= PAR_SORT_THRESHOLD {
                src.par_sort_unstable();
            } else {
                radix_sort_u128(src, scratch_u128);
            }
            let mut kmers = Vec::new();
            let mut counts = Vec::new();
            let mut i = 0usize;
            while i < src.len() {
                let key = src[i];
                let mut c = 1u32;
                i += 1;
                while i < src.len() && src[i] == key {
                    c = c.wrapping_add(1);
                    i += 1;
                }
                kmers.push(key);
                counts.push(c);
            }
            src.clear();
            buffers_u128_total[part] = 0;
            out.chunks_u128[part].push(ChunkU128 { kmers, counts });
            out.timings.sorting += t.elapsed();
        }
        KmerMode::Packed(_) => {
            let src_deque = &mut buffers_packed[part];
            if src_deque.is_empty() {
                return;
            }
            let t = Instant::now();
            // move into a vec to sort
            let mut vecs: Vec<Vec<u64>> = src_deque.drain(..).collect();
            if vecs.len() >= PAR_SORT_THRESHOLD {
                vecs.par_sort_unstable();
            } else {
                vecs.sort_unstable();
            }
            let mut kmers = Vec::new();
            let mut counts = Vec::new();
            let mut i = 0usize;
            while i < vecs.len() {
                let key = vecs[i].clone();
                let mut c = 1u32;
                i += 1;
                while i < vecs.len() && vecs[i] == key {
                    c = c.wrapping_add(1);
                    i += 1;
                }
                kmers.push(key);
                counts.push(c);
            }
            buffers_packed_total[part] = 0;
            out.chunks_packed[part].push(ChunkPacked {
                kmers: VecDeque::from(kmers),
                counts: VecDeque::from(counts),
            });
            out.timings.sorting += t.elapsed();
        }
    }
}

/// 中文：基线版计数流程。它不做 minimizer 分桶，主要用来对照和回归测试。
/// English: Baseline counting pipeline. It skips minimizer partitioning and is
/// mainly used for comparison and regression testing.
pub fn count_baseline(path: &Path, cfg: &CountConfig) -> Result<CountOutcome> {
    validate_cfg(cfg)?;
    let mode = KmerMode::from_k(cfg.k);

    let t_total = Instant::now();
    let t_parse = Instant::now();

    let mut reader = open_seq_reader(path)?;
    let mut seq = Vec::with_capacity(256);
    let parsing = t_parse.elapsed();

    let mut total_reads = 0u64;

    match mode {
        KmerMode::U64 => {
            let mut all = Vec::<u64>::with_capacity(cfg.chunk_kmers.max(1 << 20));
            let mut scratch = Vec::<u64>::new();
            let mut roll = RollingKmer::new(cfg.k, cfg.canonical);
            let mut total_kmers = 0u64;
            let t_k = Instant::now();
            while reader.next_sequence(&mut seq)? {
                total_reads += 1;
                roll.reset();
                for &ch in &seq {
                    let b = encode_base(ch);
                    if b == INVALID_BASE {
                        roll.reset();
                        continue;
                    }
                    if let Some(KmerKey::U64(v)) = roll.push_encoded(b) {
                        all.push(v);
                        total_kmers += 1;
                    }
                }
            }
            let kmer_generation = t_k.elapsed();
            let t_sort = Instant::now();
            if all.len() >= PAR_SORT_THRESHOLD {
                all.par_sort_unstable();
            } else {
                radix_sort_u64(&mut all, &mut scratch);
            }
            let mut table = Vec::<(u64, u64)>::new();
            let mut i = 0usize;
            while i < all.len() {
                let key = all[i];
                let mut c = 1u64;
                i += 1;
                while i < all.len() && all[i] == key {
                    c += 1;
                    i += 1;
                }
                table.push((key, c));
            }
            let total = t_total.elapsed();
            Ok(CountOutcome {
                mode,
                table: CountTable::U64(table),
                total_kmers,
                total_reads,
                canonical: cfg.canonical,
                timings: TimingBreakdown {
                    parsing,
                    kmer_generation,
                    minimizer_partition: Duration::ZERO,
                    sorting: t_sort.elapsed(),
                    merging: Duration::ZERO,
                    total,
                },
            })
        }
        KmerMode::U128 => {
            let mut all = Vec::<u128>::with_capacity(cfg.chunk_kmers.max(1 << 20));
            let mut scratch = Vec::<u128>::new();
            let mut roll = RollingKmer::new(cfg.k, cfg.canonical);
            let mut total_kmers = 0u64;
            let t_k = Instant::now();
            while reader.next_sequence(&mut seq)? {
                roll.reset();
                for &ch in &seq {
                    let b = encode_base(ch);
                    if b == INVALID_BASE {
                        roll.reset();
                        continue;
                    }
                    if let Some(KmerKey::U128(v)) = roll.push_encoded(b) {
                        all.push(v);
                        total_kmers += 1;
                    }
                }
            }
            let kmer_generation = t_k.elapsed();
            let t_sort = Instant::now();
            if all.len() >= PAR_SORT_THRESHOLD {
                all.par_sort_unstable();
            } else {
                radix_sort_u128(&mut all, &mut scratch);
            }
            let mut table = Vec::<(u128, u64)>::new();
            let mut i = 0usize;
            while i < all.len() {
                let key = all[i];
                let mut c = 1u64;
                i += 1;
                while i < all.len() && all[i] == key {
                    c += 1;
                    i += 1;
                }
                table.push((key, c));
            }
            let total = t_total.elapsed();
            Ok(CountOutcome {
                mode,
                table: CountTable::U128(table),
                total_kmers,
                total_reads,
                canonical: cfg.canonical,
                timings: TimingBreakdown {
                    parsing,
                    kmer_generation,
                    minimizer_partition: Duration::ZERO,
                    sorting: t_sort.elapsed(),
                    merging: Duration::ZERO,
                    total,
                },
            })
        }
        KmerMode::Packed(_) => {
            let words = words_for_k(cfg.k);
            let mut all = Vec::<Vec<u64>>::with_capacity(cfg.chunk_kmers.max(1 << 18));
            let mut roll = RollingKmer::new(cfg.k, cfg.canonical);
            let mut total_kmers = 0u64;
            let t_k = Instant::now();
            while reader.next_sequence(&mut seq)? {
                roll.reset();
                for &ch in &seq {
                    let b = encode_base(ch);
                    if b == INVALID_BASE {
                        roll.reset();
                        continue;
                    }
                    if let Some(KmerKey::Packed(v)) = roll.push_encoded(b) {
                        if v.words.len() != words {
                            bail!("packed word width mismatch");
                        }
                        all.push(v.words.into_vec());
                        total_kmers += 1;
                    }
                }
            }
            let kmer_generation = t_k.elapsed();
            let t_sort = Instant::now();
            if all.len() >= PAR_SORT_THRESHOLD {
                all.par_sort_unstable();
            } else {
                all.sort_unstable();
            }
            let mut table = Vec::<(Vec<u64>, u64)>::new();
            let mut i = 0usize;
            while i < all.len() {
                let key = all[i].clone();
                let mut c = 1u64;
                i += 1;
                while i < all.len() && all[i] == key {
                    c += 1;
                    i += 1;
                }
                table.push((key, c));
            }
            let total = t_total.elapsed();
            Ok(CountOutcome {
                mode,
                table: CountTable::Packed(table),
                total_kmers,
                total_reads,
                canonical: cfg.canonical,
                timings: TimingBreakdown {
                    parsing,
                    kmer_generation,
                    minimizer_partition: Duration::ZERO,
                    sorting: t_sort.elapsed(),
                    merging: Duration::ZERO,
                    total,
                },
            })
        }
    }
}
