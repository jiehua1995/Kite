use super::fastq::{
    GzipKind, detect_gzip_kind, open_seq_reader, spawn_bgzf_block_reader, spawn_gzip_block_reader,
};
use super::kmer::{INVALID_BASE, KmerKey, KmerMode, RollingKmer, encode_base, words_for_k};
use super::merge::{
    ChunkPacked, ChunkU64, ChunkU128, merge_chunks_packed, merge_chunks_u64, merge_chunks_u128,
};
use super::perf::PerfCounters;
use super::radix_sort::{radix_sort_u64, radix_sort_u128};
use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, bounded};
use memchr::memchr_iter;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, VecDeque};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

// 中文：这里是 `count` 的核心执行器，负责并行读取、滚动编码、直接按 k-mer
// 选择 partition、分块排序、局部压缩计数和最后的全局归并。
// English: This is the core `count` executor. It handles parallel reading,
// rolling encoding, direct k-mer partitioning, chunk sorting, local counting,
// and final global merge.
#[derive(Debug, Clone)]
pub struct CountConfig {
    pub k: usize,
    pub partitions: usize,
    pub threads: usize,
    pub chunk_kmers: usize,
    pub max_memory_bytes: Option<u64>,
    pub canonical: bool,
    pub test_mode: bool,
    pub progress: bool,
    pub progress_counters: Option<Arc<ProgressCounters>>,
    pub perf_counters: Option<Arc<PerfCounters>>,
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
    pub partition_routing: Duration,
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

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct SpillBuildStats {
    pub spill_flushes: u64,
    pub spill_files_created: u64,
    pub spill_files_final: u64,
    pub spill_bytes_written: u64,
    pub spill_bytes_read_local_merge: u64,
    pub spill_local_flush: Duration,
    pub spill_local_merge: Duration,
    pub local_merge_groups: u64,
    pub local_merge_passes_total: u64,
    pub local_merge_passes_max: u64,
    pub local_merge_fanin_sum: u64,
    pub local_merge_fanin_max: u64,
    pub active_partitions: u64,
    pub files_per_partition_sum: u64,
    pub max_files_per_partition: u64,
    pub hottest_partition: usize,
    pub hottest_partition_file_count: u64,
    pub fanin_buckets: [u64; 6],
}

impl SpillBuildStats {
    fn merge_in(&mut self, other: &Self) {
        self.spill_flushes += other.spill_flushes;
        self.spill_files_created += other.spill_files_created;
        self.spill_bytes_written += other.spill_bytes_written;
        self.spill_bytes_read_local_merge += other.spill_bytes_read_local_merge;
        self.spill_local_flush += other.spill_local_flush;
        self.spill_local_merge += other.spill_local_merge;
        self.local_merge_groups += other.local_merge_groups;
        self.local_merge_passes_total += other.local_merge_passes_total;
        self.local_merge_passes_max =
            self.local_merge_passes_max.max(other.local_merge_passes_max);
        self.local_merge_fanin_sum += other.local_merge_fanin_sum;
        self.local_merge_fanin_max = self.local_merge_fanin_max.max(other.local_merge_fanin_max);
    }

    fn finalize_from_partition_chunks(&mut self, partition_chunks: &[Vec<PathBuf>]) {
        self.spill_files_final = 0;
        self.active_partitions = 0;
        self.files_per_partition_sum = 0;
        self.max_files_per_partition = 0;
        self.hottest_partition = 0;
        self.hottest_partition_file_count = 0;
        self.fanin_buckets = [0; 6];

        for (part, files) in partition_chunks.iter().enumerate() {
            let file_count = files.len() as u64;
            if file_count == 0 {
                continue;
            }
            self.spill_files_final += file_count;
            self.active_partitions += 1;
            self.files_per_partition_sum += file_count;
            self.max_files_per_partition = self.max_files_per_partition.max(file_count);
            if file_count > self.hottest_partition_file_count {
                self.hottest_partition = part;
                self.hottest_partition_file_count = file_count;
            }
            self.fanin_buckets[fanin_bucket(files.len())] += 1;
        }
    }
}

#[derive(Debug, Clone)]
/// 中文：`u64` 分片外排计数路径的结果，保存每个 partition 对应的临时 chunk 文件。
/// English: Result of the partitioned/spilled `u64` counting path, storing the
/// temporary sorted chunk files for each partition.
pub struct PartitionedU64Outcome {
    pub partition_chunks: Vec<Vec<PathBuf>>,
    pub temp_dir: PathBuf,
    pub total_kmers: u64,
    pub total_reads: u64,
    pub canonical: bool,
    pub timings: TimingBreakdown,
    pub spill_stats: Option<SpillBuildStats>,
}

#[derive(Debug, Clone)]
/// 中文：`u128` 分片外排计数路径的结果，保存每个 partition 对应的临时 chunk 文件。
/// English: Result of the partitioned/spilled `u128` counting path, storing the
/// temporary sorted chunk files for each partition.
pub struct PartitionedU128Outcome {
    pub partition_chunks: Vec<Vec<PathBuf>>,
    pub temp_dir: PathBuf,
    pub total_kmers: u64,
    pub total_reads: u64,
    pub canonical: bool,
    pub timings: TimingBreakdown,
}

#[derive(Debug, Clone)]
/// 中文：`u64` 内存计数路径的分片结果。每个 partition 保留本地已排序 chunk，
/// 方便写库阶段直接流式归并并编码，避免先拼成全局大表。
/// English: Partitioned in-memory result for the `u64` path. Each partition
/// keeps sorted local chunks so the writer can merge and encode them directly
/// without first materializing one global table.
pub struct ChunkedU64Outcome {
    pub partition_chunks: Vec<Vec<ChunkU64>>,
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
    partition_routing: Duration,
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

#[derive(Default)]
/// 中文：worker 内部的本地进度累加器。它先在线程本地累积 reads/k-mers，
/// 到达阈值或线程结束时再批量写回全局原子变量，避免高频 `fetch_add`。
/// English: Thread-local progress accumulator. It batches read/k-mer progress
/// updates locally and flushes them to the global atomics only occasionally.
struct LocalProgress {
    reads: u64,
    kmers: u64,
}

impl LocalProgress {
    #[inline(always)]
    fn flush(&mut self, progress_ref: &Option<Arc<ProgressCounters>>) {
        if self.reads == 0 && self.kmers == 0 {
            return;
        }
        if let Some(pc) = progress_ref {
            if self.reads != 0 {
                pc.processed_reads.fetch_add(self.reads, Ordering::Relaxed);
            }
            if self.kmers != 0 {
                pc.processed_kmers.fetch_add(self.kmers, Ordering::Relaxed);
            }
        }
        self.reads = 0;
        self.kmers = 0;
    }
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

/// 中文：`count` worker 接收的工作单元。
/// - `SeqBatch`: 非 gzip 路径沿用的序列批次
/// - `FastqChunk`: 已按 FASTQ record 边界对齐的原始字节块，供 worker 直接解析并计数
/// English: Work item consumed by a `count` worker.
/// - `SeqBatch`: sequence batch used by the non-gzip path
/// - `FastqChunk`: raw byte chunk aligned to FASTQ record boundaries so the
///   worker can parse and count directly
enum WorkItem {
    SeqBatch(SeqBatch),
    FastqChunk(Vec<u8>),
}

enum U64ReducerInput {
    Raw(Vec<u64>),
    Counted(Vec<(u64, u64)>),
}

const PRODUCER_BATCH_READS: usize = 16_384;
const PRODUCER_BATCH_BYTES: usize = 16 << 20;
const GZIP_BLOCK_QUEUE_CAP: usize = 64;
const U64_HASH_K_MAX: usize = 21;
const U64_OWNER_BATCH_KMERS: usize = 524_288;
const U64_OWNER_MAX_BATCH_KMERS: usize = 1_048_576;

#[derive(Debug, Clone, Copy)]
/// 中文：按线程数和内存预算推导出的输入侧调优参数。
/// English: Ingestion-side tuning knobs derived from thread count and optional
/// memory budget.
pub struct IngestTuning {
    pub decomp_block_bytes: usize,
    pub work_chunk_bytes: usize,
    pub block_queue_cap: usize,
    pub worker_queue_cap: usize,
    pub prealloc_block_buffers: usize,
}

#[derive(Debug, Clone, Copy)]
/// 中文：按总线程预算拆分出的 `count` 流水线线程布局。
/// English: Thread-budget split for the `count` pipeline.
pub struct PipelineThreads {
    pub gzip_decompressors: usize,
    pub bgzf_decompressors: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct U64OwnerPipeline {
    pub extractors: usize,
    pub reducers: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct InputProfile {
    pub sampled_reads: usize,
    pub mean_read_len: f64,
    pub max_read_len: usize,
}

#[derive(Debug, Clone, Copy)]
struct U64OwnerTuning {
    ingest: IngestTuning,
    extractor_queue_cap: usize,
    reducer_queue_cap: usize,
    pending_kmers_limit: usize,
}

#[inline(always)]
fn gzip_decomp_block_bytes(threads: usize) -> usize {
    let t = threads.max(1) as f64;
    let mb = (40.0 / t.sqrt()).round().clamp(6.0, 20.0) as usize;
    mb << 20
}

#[inline(always)]
fn fastq_work_chunk_bytes(threads: usize) -> usize {
    let t = threads.max(1) as f64;
    let mb = (96.0 / t.sqrt()).round().clamp(6.0, 24.0) as usize;
    mb << 20
}

#[inline(always)]
fn aggressive_auto_memory_budget(threads: usize) -> u64 {
    let gb = (threads.max(1) as u64 * 2).clamp(4, 192);
    gb * 1024 * 1024 * 1024
}

#[inline(always)]
pub fn effective_partitions(
    requested: usize,
    threads: usize,
    max_memory_bytes: Option<u64>,
) -> usize {
    if requested != 256 {
        return requested.max(1);
    }
    let mem = max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(threads)) >> 30;
    let target = ((threads.max(1) * 16) as u64).max(mem * 8).clamp(256, 4096) as usize;
    let next_pow2 = target.next_power_of_two();
    next_pow2.clamp(256, 4096)
}

#[inline(always)]
pub fn effective_owner_partitions(
    requested: usize,
    threads: usize,
    max_memory_bytes: Option<u64>,
    reducers: usize,
) -> usize {
    if requested != 256 {
        return requested.max(reducers.max(1));
    }
    let mem = max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(threads)) >> 30;
    let target = ((reducers.max(1) * 32) as u64)
        .max(mem * 4)
        .clamp(256, 1024) as usize;
    target.next_power_of_two().clamp(256, 1024)
}

#[inline(always)]
pub fn effective_spill_partitions(
    requested: usize,
    threads: usize,
    max_memory_bytes: Option<u64>,
) -> usize {
    if requested != 256 {
        return requested.max(1);
    }
    let mem = max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(threads)) >> 30;
    let target = ((threads.max(1) * 8) as u64).max(mem * 4).clamp(256, 512) as usize;
    target.next_power_of_two().clamp(256, 512)
}

#[inline(always)]
pub fn effective_chunk_kmers(
    requested: usize,
    threads: usize,
    max_memory_bytes: Option<u64>,
) -> usize {
    if requested != 1_000_000 {
        return requested.max(1);
    }
    let mem_gb =
        (max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(threads)) >> 30) as usize;
    let by_threads = threads.max(1) * 125_000;
    let by_memory = mem_gb * 62_500;
    by_threads.max(by_memory).clamp(1_000_000, 12_000_000)
}

#[inline(always)]
pub fn effective_u128_partitions(
    requested: usize,
    threads: usize,
    max_memory_bytes: Option<u64>,
) -> usize {
    if requested != 256 {
        return requested.max(1);
    }
    let mem = max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(threads)) >> 30;
    let target = ((threads.max(1) * 8) as u64).max(mem * 2).clamp(128, 1024) as usize;
    target.next_power_of_two().clamp(128, 1024)
}

#[inline(always)]
pub fn effective_u128_chunk_kmers(
    requested: usize,
    threads: usize,
    max_memory_bytes: Option<u64>,
) -> usize {
    if requested != 1_000_000 {
        return requested.max(1);
    }
    let mem_gb =
        (max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(threads)) >> 30) as usize;
    let by_threads = threads.max(1) * 90_000;
    let by_memory = mem_gb * 45_000;
    by_threads.max(by_memory).clamp(400_000, 4_000_000)
}

/// 中文：根据线程数和可选内存预算生成 `count` 的输入流水线调优参数。
/// 输入：
/// - `threads`: 用户请求的线程数
/// - `max_memory_bytes`: 可选的内存预算
/// 输出：
/// - 解压块大小、FASTQ 工作块大小、块队列深度、worker 队列深度
/// English: Build the adaptive ingestion tuning profile for `count`.
/// Input:
/// - `threads`: requested thread count
/// - `max_memory_bytes`: optional memory budget
/// Output:
/// - decompression block size, FASTQ work chunk size, block queue capacity, and
///   worker queue capacity
pub fn ingest_tuning(threads: usize, max_memory_bytes: Option<u64>) -> IngestTuning {
    let effective_memory =
        max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(threads));
    let decomp_block_bytes = gzip_decomp_block_bytes(threads);
    let work_chunk_bytes = fastq_work_chunk_bytes(threads);
    let mut block_queue_cap = (threads * GZIP_BLOCK_QUEUE_CAP).max(GZIP_BLOCK_QUEUE_CAP);
    let mut worker_queue_cap = (threads * 64).max(64);
    {
        let mem = effective_memory.max((work_chunk_bytes as u64) * (threads.max(1) as u64) * 8);
        let per_worker_slots = ((mem / 4) / (work_chunk_bytes as u64) / (threads.max(1) as u64))
            .clamp(8, 256) as usize;
        let block_slots = ((mem / 16) / (decomp_block_bytes as u64)).clamp(64, 4096) as usize;
        worker_queue_cap = worker_queue_cap.max(per_worker_slots);
        block_queue_cap = block_queue_cap.max(block_slots);
    }
    let prealloc_block_buffers =
        ((effective_memory / 32) / (decomp_block_bytes as u64)).clamp(16, 2048) as usize;

    IngestTuning {
        decomp_block_bytes,
        work_chunk_bytes,
        block_queue_cap,
        worker_queue_cap,
        prealloc_block_buffers,
    }
}

#[inline(always)]
fn local_spill_ingest_tuning(
    cfg: &CountConfig,
    count_workers: usize,
    mut tuning: IngestTuning,
) -> IngestTuning {
    let effective_memory =
        cfg.max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(cfg.threads));
    let worker_queue_cap = ((effective_memory / 32)
        / (tuning.work_chunk_bytes as u64)
        / count_workers.max(1) as u64) as usize;
    let block_queue_cap =
        ((effective_memory / 64) / (tuning.decomp_block_bytes as u64)).clamp(32, 256) as usize;

    tuning.worker_queue_cap = tuning.worker_queue_cap.min(worker_queue_cap.clamp(4, 16));
    tuning.block_queue_cap = tuning.block_queue_cap.min(block_queue_cap);
    tuning.prealloc_block_buffers = tuning
        .prealloc_block_buffers
        .min(tuning.block_queue_cap.saturating_mul(2))
        .clamp(32, 256);
    tuning
}

#[inline(always)]
/// 中文：把用户给的总线程预算拆成 count worker 和解压线程。
/// English: Split the user-provided total thread budget into counting workers
/// and decompression workers.
pub fn pipeline_threads(total_threads: usize) -> PipelineThreads {
    let total = total_threads.max(1);
    let bgzf_decompressors = ((total as f64) * 0.22).round() as usize;
    let bgzf_decompressors = bgzf_decompressors.clamp(1, total.saturating_sub(1).max(1));
    let gzip_decompressors = if total >= 4 { 1 } else { 0 };

    PipelineThreads {
        gzip_decompressors,
        bgzf_decompressors,
    }
}

#[inline(always)]
pub fn u64_owner_pipeline_threads(
    total_threads: usize,
    gzip_kind: Option<GzipKind>,
    k: usize,
) -> U64OwnerPipeline {
    let pipe = pipeline_threads(total_threads);
    let available = match gzip_kind {
        Some(GzipKind::PlainGzip) => total_threads
            .saturating_sub(pipe.gzip_decompressors)
            .max(2),
        Some(GzipKind::Bgzf) => total_threads
            .saturating_sub(pipe.bgzf_decompressors + 1)
            .max(2),
        None => total_threads.max(2),
    };
    let reducers = if available <= 2 {
        1
    } else if k <= 21 {
        ((available * 2) / 5).clamp(2, available - 1).min(32)
    } else if k <= 31 {
        ((available * 2) / 5).clamp(2, available - 1).min(40)
    } else {
        (available / 3).clamp(1, 24).min(available - 1)
    };
    let extractors = available.saturating_sub(reducers).max(1);
    U64OwnerPipeline {
        extractors,
        reducers,
    }
}

#[inline(always)]
fn u64_owner_tuning(cfg: &CountConfig, owner: U64OwnerPipeline) -> U64OwnerTuning {
    let effective_memory =
        cfg.max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(cfg.threads));
    let mut ingest = ingest_tuning(cfg.threads, cfg.max_memory_bytes);
    ingest.work_chunk_bytes = if cfg.k <= 21 {
        ingest.work_chunk_bytes.clamp(8 << 20, 20 << 20)
    } else if cfg.k <= 31 {
        ingest.work_chunk_bytes.clamp(12 << 20, 24 << 20)
    } else {
        ingest.work_chunk_bytes.clamp(4 << 20, 12 << 20)
    };

    let extractor_budget = (effective_memory / 16)
        .max((ingest.work_chunk_bytes as u64) * owner.extractors.max(1) as u64 * 2);
    let extractor_slots = ((extractor_budget
        / (ingest.work_chunk_bytes as u64)
        / owner.extractors.max(1) as u64) as usize)
        .clamp(16, 256);

    let reducer_batch_kmers = u64_owner_batch_kmers(cfg, owner.reducers);
    let reducer_batch_bytes = (reducer_batch_kmers as u64) * (std::mem::size_of::<u64>() as u64);
    let reducer_budget =
        (effective_memory / 16).max(reducer_batch_bytes * owner.reducers.max(1) as u64 * 2);
    let reducer_slots = ((reducer_budget / reducer_batch_bytes / owner.reducers.max(1) as u64)
        as usize)
        .clamp(16, 512);

    let block_budget =
        (effective_memory / 32).max((ingest.decomp_block_bytes as u64) * 8);
    ingest.block_queue_cap = ((block_budget / (ingest.decomp_block_bytes as u64)) as usize)
        .clamp(64, 1024);
    ingest.worker_queue_cap = extractor_slots.max(128);
    ingest.prealloc_block_buffers = ingest.block_queue_cap.saturating_mul(2).clamp(128, 2048);

    let per_reducer_bytes = (effective_memory / owner.reducers.max(1) as u64) / 8;
    let pending_kmers_limit = ((per_reducer_bytes / (std::mem::size_of::<u64>() as u64)) as usize)
        .clamp(2_000_000, 32_000_000);

    U64OwnerTuning {
        ingest,
        extractor_queue_cap: extractor_slots,
        reducer_queue_cap: reducer_slots,
        pending_kmers_limit,
    }
}

#[inline(always)]
fn should_combine_u64_owner_batches(cfg: &CountConfig, reducers: usize) -> bool {
    cfg.k <= 31 && reducers <= 40
}

#[inline(always)]
fn u64_owner_batch_kmers(cfg: &CountConfig, reducers: usize) -> usize {
    let mut base = if cfg.k <= 21 {
        U64_OWNER_BATCH_KMERS
    } else if cfg.k <= 31 {
        U64_OWNER_BATCH_KMERS
    } else {
        U64_OWNER_BATCH_KMERS / 2
    };
    if reducers <= 8 {
        base = (base * 3) / 2;
    } else if reducers >= 48 {
        base /= 4;
    } else if reducers >= 32 {
        base /= 2;
    } else if reducers >= 24 {
        base = (base * 3) / 4;
    }
    if let Some(max_mem) = cfg.max_memory_bytes {
        if max_mem <= (16u64 << 30) {
            base /= 2;
        }
    }
    base.clamp(32_768, U64_OWNER_MAX_BATCH_KMERS)
}

#[inline(always)]
fn temp_count_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = std::env::var_os("KITE_TMPDIR")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!("kite-count-{}-{nanos}", std::process::id()))
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
/// 中文：FASTQ 四行解析状态。优化的 gzip 热路径只关心四行结构，
/// 并且只把第 2 行 sequence 拷贝进 `SeqBatch`。
/// English: FASTQ four-line parser state. The optimized gzip hot path only
/// cares about the four-line structure and copies only the sequence line into
/// the outgoing `SeqBatch`.
enum FastqLineState {
    Header,
    Sequence,
    Plus,
    Quality,
}

/// 中文：FASTQ 记录边界对齐器。它接收解压后的大块字节流，
/// 只负责找到完整 record 的边界，并把对齐后的原始字节块交给 worker。
/// English: FASTQ record-boundary aligner. It consumes large decompressed byte
/// blocks, finds complete record boundaries, and hands aligned raw byte chunks
/// directly to workers.
struct FastqChunkAligner {
    carry: Vec<u8>,
    target_bytes: usize,
}

impl FastqChunkAligner {
    /// 中文：创建一个新的记录边界对齐器。
    /// 输入：
    /// - `target_bytes`: 每个原始 FASTQ 工作块的目标大小
    /// 输出：
    /// - 初始化后的对齐器
    /// English: Create a new record-boundary aligner.
    /// Input:
    /// - `target_bytes`: target size of each raw FASTQ work chunk
    /// Output:
    /// - an initialized aligner
    fn new(target_bytes: usize) -> Self {
        Self {
            carry: Vec::with_capacity(target_bytes + 256),
            target_bytes,
        }
    }

    /// 中文：把一个新解压块喂给对齐器，并在达到 record 边界后发出原始工作块。
    /// English: Feed one new decompressed block into the aligner and emit raw
    /// work chunks once a record-aligned boundary is available.
    fn push_block<F>(&mut self, block: &[u8], emit_chunk: &mut F) -> Result<()>
    where
        F: FnMut(Vec<u8>) -> Result<()>,
    {
        self.carry.extend_from_slice(block);
        self.drain_ready_chunks(false, emit_chunk)
    }

    /// 中文：结束输入，发出最后一个完整工作块，并检查 FASTQ 是否被完整截断。
    /// English: Finish input, emit the final complete work chunk, and verify
    /// that the FASTQ stream was not truncated.
    fn finish<F>(&mut self, emit_chunk: &mut F) -> Result<()>
    where
        F: FnMut(Vec<u8>) -> Result<()>,
    {
        self.drain_ready_chunks(true, emit_chunk)
    }

    fn drain_ready_chunks<F>(&mut self, final_block: bool, emit_chunk: &mut F) -> Result<()>
    where
        F: FnMut(Vec<u8>) -> Result<()>,
    {
        loop {
            if self.carry.is_empty() {
                return Ok(());
            }

            let (state, parsed_end, cut_end) = self.scan_carry(final_block)?;

            if final_block {
                if state != FastqLineState::Header || parsed_end != self.carry.len() {
                    bail!("Malformed FASTQ: truncated record at end of gzip stream");
                }
                if cut_end != 0 {
                    let chunk = std::mem::take(&mut self.carry);
                    emit_chunk(chunk)?;
                }
                return Ok(());
            }

            if cut_end >= self.target_bytes {
                let tail = self.carry.split_off(cut_end);
                let chunk = std::mem::replace(&mut self.carry, tail);
                emit_chunk(chunk)?;
                continue;
            }

            return Ok(());
        }
    }

    fn scan_carry(&self, final_block: bool) -> Result<(FastqLineState, usize, usize)> {
        let mut state = FastqLineState::Header;
        let mut line_start = 0usize;
        let mut cut_end = 0usize;

        for end_raw in memchr_iter(b'\n', &self.carry) {
            let next_start = end_raw + 1;
            let mut end = end_raw;
            if end > line_start && self.carry[end - 1] == b'\r' {
                end -= 1;
            }
            let line = &self.carry[line_start..end];
            match state {
                FastqLineState::Header => {
                    if line.iter().all(|b| b.is_ascii_whitespace()) {
                        line_start = next_start;
                        continue;
                    }
                    if line.first().copied() != Some(b'@') {
                        bail!("Malformed FASTQ: missing @ header");
                    }
                    state = FastqLineState::Sequence;
                }
                FastqLineState::Sequence => {
                    if line.is_empty() {
                        bail!("Malformed FASTQ: empty sequence line");
                    }
                    state = FastqLineState::Plus;
                }
                FastqLineState::Plus => {
                    if line.first().copied() != Some(b'+') {
                        bail!("Malformed FASTQ: missing + line");
                    }
                    state = FastqLineState::Quality;
                }
                FastqLineState::Quality => {
                    state = FastqLineState::Header;
                    cut_end = next_start;
                    if !final_block && cut_end >= self.target_bytes {
                        return Ok((state, next_start, cut_end));
                    }
                }
            }
            line_start = next_start;
        }

        if final_block && line_start < self.carry.len() {
            let line = &self.carry[line_start..];
            match state {
                FastqLineState::Header => {
                    if line.iter().all(|b| b.is_ascii_whitespace()) {
                        line_start = self.carry.len();
                    } else if line.first().copied() != Some(b'@') {
                        bail!("Malformed FASTQ: missing @ header");
                    } else {
                        state = FastqLineState::Sequence;
                        line_start = self.carry.len();
                    }
                }
                FastqLineState::Sequence => {
                    if line.is_empty() {
                        bail!("Malformed FASTQ: empty sequence line");
                    }
                    state = FastqLineState::Plus;
                    line_start = self.carry.len();
                }
                FastqLineState::Plus => {
                    if line.first().copied() != Some(b'+') {
                        bail!("Malformed FASTQ: missing + line");
                    }
                    state = FastqLineState::Quality;
                    line_start = self.carry.len();
                }
                FastqLineState::Quality => {
                    state = FastqLineState::Header;
                    line_start = self.carry.len();
                    cut_end = self.carry.len();
                }
            }
        }

        Ok((state, line_start, cut_end))
    }
}

/// 中文：解析一个已经按 FASTQ record 边界对齐的原始字节块，并把其中的序列直接送入
/// 现有的 rolling k-mer 统计逻辑。
/// English: Parse a raw byte chunk that is already aligned to FASTQ record
/// boundaries, and feed the extracted sequence lines directly into the rolling
/// k-mer counting logic.
fn process_fastq_chunk_direct<F>(chunk: &[u8], mut on_sequence: F) -> Result<()>
where
    F: FnMut(&[u8]) -> Result<()>,
{
    let mut line_start = 0usize;
    let mut line_no = 0usize;

    for end_raw in memchr_iter(b'\n', chunk) {
        let next_start = end_raw + 1;
        let mut end = end_raw;
        if end > line_start && chunk[end - 1] == b'\r' {
            end -= 1;
        }
        let line = &chunk[line_start..end];

        match line_no & 3 {
            0 => {
                if line.first().copied() != Some(b'@') {
                    bail!("Malformed FASTQ: missing @ header");
                }
            }
            1 => {
                if line.is_empty() {
                    bail!("Malformed FASTQ: empty sequence line");
                }
                on_sequence(line)?;
            }
            2 => {
                if line.first().copied() != Some(b'+') {
                    bail!("Malformed FASTQ: missing + line");
                }
            }
            3 => {}
            _ => unreachable!(),
        }
        line_no += 1;
        line_start = next_start;
    }

    if line_start != chunk.len() || (line_no & 3) != 0 {
        bail!("Malformed FASTQ chunk: expected complete records aligned to newline boundaries");
    }

    Ok(())
}

#[inline(always)]
fn process_sequence_u64(
    seq: &[u8],
    cfg: &CountConfig,
    partitions_pow2: bool,
    partition_shift: usize,
    mask64: u64,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    buffers_u64: &mut [Vec<u64>],
    scratch_u64: &mut Vec<u64>,
    out: &mut WorkerOutput,
) {
    if progress_enabled {
        local_progress.reads += 1;
    }
    out.total_reads += 1;

    let mut seen = 0usize;
    let mut fwd = 0u64;
    let mut rev = 0u64;

    for &ch in seq {
        let b = encode_base(ch);
        if b == INVALID_BASE {
            seen = 0;
            fwd = 0;
            rev = 0;
            continue;
        }

        let b64 = b as u64;
        fwd = ((fwd << 2) | b64) & mask64;
        rev = (rev >> 2) | (((3 - b) as u64) << rev_shift);

        if seen < cfg.k {
            seen += 1;
            if seen < cfg.k {
                continue;
            }
        }

        let v = if cfg.canonical { fwd.min(rev) } else { fwd };
        out.total_kmers += 1;
        if progress_enabled {
            local_progress.kmers += 1;
        }

        let part = partition_u64(v, cfg.partitions, partitions_pow2, partition_shift);
        let src = unsafe { buffers_u64.get_unchecked_mut(part) };
        src.push(v);
        if src.len() >= cfg.chunk_kmers {
            flush_partition_u64(part, buffers_u64, scratch_u64, out);
        }
    }
}

fn process_fastq_chunk_u64_direct(
    chunk: &[u8],
    cfg: &CountConfig,
    partitions_pow2: bool,
    partition_shift: usize,
    mask64: u64,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    buffers_u64: &mut [Vec<u64>],
    scratch_u64: &mut Vec<u64>,
    out: &mut WorkerOutput,
) -> Result<()> {
    process_fastq_chunk_direct(chunk, |line| {
        process_sequence_u64(
            line,
            cfg,
            partitions_pow2,
            partition_shift,
            mask64,
            rev_shift,
            progress_enabled,
            local_progress,
            buffers_u64,
            scratch_u64,
            out,
        );
        Ok(())
    })
}

#[inline(always)]
fn process_sequence_u128(
    seq: &[u8],
    cfg: &CountConfig,
    partitions_pow2: bool,
    partition_shift: usize,
    mask128: u128,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    buffers_u128: &mut [Vec<u128>],
    scratch_u128: &mut Vec<u128>,
    out: &mut WorkerOutput,
) {
    if progress_enabled {
        local_progress.reads += 1;
    }
    out.total_reads += 1;

    let mut seen = 0usize;
    let mut fwd = 0u128;
    let mut rev = 0u128;

    for &ch in seq {
        let b = encode_base(ch);
        if b == INVALID_BASE {
            seen = 0;
            fwd = 0;
            rev = 0;
            continue;
        }

        let b128 = b as u128;
        fwd = ((fwd << 2) | b128) & mask128;
        rev = (rev >> 2) | (((3 - b) as u128) << rev_shift);

        if seen < cfg.k {
            seen += 1;
            if seen < cfg.k {
                continue;
            }
        }

        let v = if cfg.canonical { fwd.min(rev) } else { fwd };
        out.total_kmers += 1;
        if progress_enabled {
            local_progress.kmers += 1;
        }

        let part = partition_u128(v, cfg.partitions, partitions_pow2, partition_shift);
        let src = unsafe { buffers_u128.get_unchecked_mut(part) };
        src.push(v);
        if src.len() >= cfg.chunk_kmers {
            flush_partition_u128(part, buffers_u128, scratch_u128, out);
        }
    }
}

fn process_fastq_chunk_u128_direct(
    chunk: &[u8],
    cfg: &CountConfig,
    partitions_pow2: bool,
    partition_shift: usize,
    mask128: u128,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    buffers_u128: &mut [Vec<u128>],
    scratch_u128: &mut Vec<u128>,
    out: &mut WorkerOutput,
) -> Result<()> {
    process_fastq_chunk_direct(chunk, |line| {
        process_sequence_u128(
            line,
            cfg,
            partitions_pow2,
            partition_shift,
            mask128,
            rev_shift,
            progress_enabled,
            local_progress,
            buffers_u128,
            scratch_u128,
            out,
        );
        Ok(())
    })
}

fn process_sequence_u128_hash(
    seq: &[u8],
    cfg: &CountConfig,
    mask128: u128,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    counts: &mut FxHashMap<u128, u32>,
    out: &mut WorkerOutput,
) {
    if progress_enabled {
        local_progress.reads += 1;
    }
    out.total_reads += 1;

    let mut seen = 0usize;
    let mut fwd = 0u128;
    let mut rev = 0u128;

    for &ch in seq {
        let b = encode_base(ch);
        if b == INVALID_BASE {
            seen = 0;
            fwd = 0;
            rev = 0;
            continue;
        }

        let b128 = b as u128;
        fwd = ((fwd << 2) | b128) & mask128;
        rev = (rev >> 2) | (((3 - b) as u128) << rev_shift);

        if seen < cfg.k {
            seen += 1;
            if seen < cfg.k {
                continue;
            }
        }

        let v = if cfg.canonical { fwd.min(rev) } else { fwd };
        out.total_kmers += 1;
        if progress_enabled {
            local_progress.kmers += 1;
        }
        let counter = counts.entry(v).or_insert(0);
        *counter = counter.wrapping_add(1);
    }
}

fn process_fastq_chunk_u128_hash_direct(
    chunk: &[u8],
    cfg: &CountConfig,
    mask128: u128,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    counts: &mut FxHashMap<u128, u32>,
    out: &mut WorkerOutput,
) -> Result<()> {
    process_fastq_chunk_direct(chunk, |line| {
        process_sequence_u128_hash(
            line,
            cfg,
            mask128,
            rev_shift,
            progress_enabled,
            local_progress,
            counts,
            out,
        );
        Ok(())
    })
}

#[inline(always)]
fn process_sequence_u64_hash(
    seq: &[u8],
    cfg: &CountConfig,
    mask64: u64,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    counts: &mut FxHashMap<u64, u32>,
    out: &mut WorkerOutput,
) {
    if progress_enabled {
        local_progress.reads += 1;
    }
    out.total_reads += 1;

    let mut seen = 0usize;
    let mut fwd = 0u64;
    let mut rev = 0u64;

    for &ch in seq {
        let b = encode_base(ch);
        if b == INVALID_BASE {
            seen = 0;
            fwd = 0;
            rev = 0;
            continue;
        }

        let b64 = b as u64;
        fwd = ((fwd << 2) | b64) & mask64;
        rev = (rev >> 2) | (((3 - b) as u64) << rev_shift);

        if seen < cfg.k {
            seen += 1;
            if seen < cfg.k {
                continue;
            }
        }

        let v = if cfg.canonical { fwd.min(rev) } else { fwd };
        out.total_kmers += 1;
        if progress_enabled {
            local_progress.kmers += 1;
        }
        let counter = counts.entry(v).or_insert(0);
        *counter = counter.wrapping_add(1);
    }
}

fn process_fastq_chunk_u64_hash_direct(
    chunk: &[u8],
    cfg: &CountConfig,
    mask64: u64,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    counts: &mut FxHashMap<u64, u32>,
    out: &mut WorkerOutput,
) -> Result<()> {
    if cfg.test_mode {
        let mut seq_ranges = Vec::<(usize, usize)>::new();
        let t_parse = Instant::now();
        let mut line_start = 0usize;
        let mut line_no = 0usize;
        for end_raw in memchr_iter(b'\n', chunk) {
            let next_start = end_raw + 1;
            let mut end = end_raw;
            if end > line_start && chunk[end - 1] == b'\r' {
                end -= 1;
            }
            if (line_no & 3) == 1 {
                if end == line_start {
                    bail!("Malformed FASTQ: empty sequence line");
                }
                seq_ranges.push((line_start, end));
            }
            line_no += 1;
            line_start = next_start;
        }
        if line_start != chunk.len() || (line_no & 3) != 0 {
            bail!("Malformed FASTQ chunk: expected complete records aligned to newline boundaries");
        }
        if let Some(perf) = &cfg.perf_counters {
            perf.add_worker_parse(t_parse.elapsed());
        }

        let t_count = Instant::now();
        let canonical = cfg.canonical;
        let k = cfg.k;
        for (start, end) in seq_ranges {
            let line = &chunk[start..end];
            if progress_enabled {
                local_progress.reads += 1;
            }
            out.total_reads += 1;

            let mut seen = 0usize;
            let mut fwd = 0u64;
            let mut rev = 0u64;

            for &ch in line {
                let b = encode_base(ch);
                if b == INVALID_BASE {
                    seen = 0;
                    fwd = 0;
                    rev = 0;
                    continue;
                }

                let b64 = b as u64;
                fwd = ((fwd << 2) | b64) & mask64;
                rev = (rev >> 2) | (((3 - b) as u64) << rev_shift);

                if seen < k {
                    seen += 1;
                    if seen < k {
                        continue;
                    }
                }

                let v = if canonical { fwd.min(rev) } else { fwd };
                out.total_kmers += 1;
                if progress_enabled {
                    local_progress.kmers += 1;
                }
                let counter = counts.entry(v).or_insert(0);
                *counter = counter.wrapping_add(1);
            }
        }
        if let Some(perf) = &cfg.perf_counters {
            perf.add_worker_count(t_count.elapsed());
        }
        return Ok(());
    }

    let mut line_start = 0usize;
    let mut line_no = 0usize;
    let canonical = cfg.canonical;
    let k = cfg.k;

    for end_raw in memchr_iter(b'\n', chunk) {
        let next_start = end_raw + 1;
        let mut end = end_raw;
        if end > line_start && chunk[end - 1] == b'\r' {
            end -= 1;
        }

        if (line_no & 3) == 1 {
            let line = &chunk[line_start..end];
            if line.is_empty() {
                bail!("Malformed FASTQ: empty sequence line");
            }
            if progress_enabled {
                local_progress.reads += 1;
            }
            out.total_reads += 1;

            let mut seen = 0usize;
            let mut fwd = 0u64;
            let mut rev = 0u64;

            for &ch in line {
                let b = encode_base(ch);
                if b == INVALID_BASE {
                    seen = 0;
                    fwd = 0;
                    rev = 0;
                    continue;
                }

                let b64 = b as u64;
                fwd = ((fwd << 2) | b64) & mask64;
                rev = (rev >> 2) | (((3 - b) as u64) << rev_shift);

                if seen < k {
                    seen += 1;
                    if seen < k {
                        continue;
                    }
                }

                let v = if canonical { fwd.min(rev) } else { fwd };
                out.total_kmers += 1;
                if progress_enabled {
                    local_progress.kmers += 1;
                }
                let counter = counts.entry(v).or_insert(0);
                *counter = counter.wrapping_add(1);
            }
        }

        line_no += 1;
        line_start = next_start;
    }

    if line_start != chunk.len() || (line_no & 3) != 0 {
        bail!("Malformed FASTQ chunk: expected complete records aligned to newline boundaries");
    }

    Ok(())
}

#[inline(always)]
fn owner_for_partition(part: usize, reducers: usize, partitions: usize) -> usize {
    ((part as u128 * reducers as u128) / partitions as u128) as usize
}

#[inline(always)]
fn process_sequence_u64_owner(
    seq: &[u8],
    cfg: &CountConfig,
    partitions_pow2: bool,
    partition_shift: usize,
    mask64: u64,
    rev_shift: usize,
    reducers: usize,
    owner_batch_kmers: usize,
    owner_buffers: &mut [Vec<u64>],
    owner_scratch: &mut Vec<u64>,
    reducer_txs: &[Sender<U64ReducerInput>],
    combine_counts: bool,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    out: &mut WorkerOutput,
) -> Result<()> {
    if progress_enabled {
        local_progress.reads += 1;
    }
    out.total_reads += 1;

    let mut seen = 0usize;
    let mut fwd = 0u64;
    let mut rev = 0u64;

    for &ch in seq {
        let b = encode_base(ch);
        if b == INVALID_BASE {
            seen = 0;
            fwd = 0;
            rev = 0;
            continue;
        }

        let b64 = b as u64;
        fwd = ((fwd << 2) | b64) & mask64;
        rev = (rev >> 2) | (((3 - b) as u64) << rev_shift);

        if seen < cfg.k {
            seen += 1;
            if seen < cfg.k {
                continue;
            }
        }

        let v = if cfg.canonical { fwd.min(rev) } else { fwd };
        out.total_kmers += 1;
        if progress_enabled {
            local_progress.kmers += 1;
        }
        let part = partition_u64(v, cfg.partitions, partitions_pow2, partition_shift);
        let owner = owner_for_partition(part, reducers, cfg.partitions);
        let buf = unsafe { owner_buffers.get_unchecked_mut(owner) };
        buf.push(v);
        if buf.len() >= owner_batch_kmers {
            flush_u64_owner_batch(buf, owner_scratch, &reducer_txs[owner], combine_counts)?;
        }
    }

    Ok(())
}

fn flush_u64_owner_batch(
    buf: &mut Vec<u64>,
    scratch: &mut Vec<u64>,
    reducer_tx: &Sender<U64ReducerInput>,
    combine_counts: bool,
) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    if !combine_counts {
        let mut send_buf = Vec::with_capacity(buf.len());
        std::mem::swap(&mut send_buf, buf);
        reducer_tx
            .send(U64ReducerInput::Raw(send_buf))
            .context("failed to send raw u64 owner batch")?;
        return Ok(());
    }

    radix_sort_u64(buf, scratch);
    let mut counted = Vec::with_capacity(buf.len().min(U64_OWNER_BATCH_KMERS));
    let mut i = 0usize;
    while i < buf.len() {
        let key = buf[i];
        let mut count = 1u64;
        i += 1;
        while i < buf.len() && buf[i] == key {
            count += 1;
            i += 1;
        }
        counted.push((key, count));
    }
    buf.clear();
    reducer_tx
        .send(U64ReducerInput::Counted(counted))
        .context("failed to send counted u64 owner batch")?;
    Ok(())
}

fn process_fastq_chunk_u64_owner_direct(
    chunk: &[u8],
    cfg: &CountConfig,
    partitions_pow2: bool,
    partition_shift: usize,
    mask64: u64,
    rev_shift: usize,
    reducers: usize,
    owner_batch_kmers: usize,
    owner_buffers: &mut [Vec<u64>],
    owner_scratch: &mut Vec<u64>,
    reducer_txs: &[Sender<U64ReducerInput>],
    combine_counts: bool,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    out: &mut WorkerOutput,
) -> Result<()> {
    process_fastq_chunk_direct(chunk, |line| {
        process_sequence_u64_owner(
            line,
            cfg,
            partitions_pow2,
            partition_shift,
            mask64,
            rev_shift,
            reducers,
            owner_batch_kmers,
            owner_buffers,
            owner_scratch,
            reducer_txs,
            combine_counts,
            progress_enabled,
            local_progress,
            out,
        )
    })
}

/// 中文：处理一条序列并把其中生成的 k-mer 直接路由到对应 partition 的缓冲区。
/// 输入：
/// - `seq`: 一条 DNA 序列
/// - 其余参数：当前 worker 的滚动状态、分桶缓冲和局部统计
/// 输出：
/// - 无显式返回值；结果直接累加到 `out` 和各 partition 缓冲区
/// English: Process one sequence and route all generated k-mers into the
/// appropriate partition-local buffers.
/// Input:
/// - `seq`: one DNA sequence
/// - remaining arguments: worker-local rolling state, partition buffers, and
///   local accounting
/// Output:
/// - no explicit return value; results are accumulated into `out` and the
///   partition buffers in place
#[allow(clippy::too_many_arguments)]
fn process_sequence(
    seq: &[u8],
    cfg: &CountConfig,
    mode: KmerMode,
    partitions_pow2: bool,
    partition_shift: usize,
    roll: &mut RollingKmer,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
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
    if progress_enabled {
        local_progress.reads += 1;
    }
    out.total_reads += 1;
    roll.reset();

    match mode {
        KmerMode::U64 => {
            for &ch in seq {
                let b = encode_base(ch);
                if b == INVALID_BASE {
                    roll.reset();
                    continue;
                }
                if let Some(KmerKey::U64(v)) = roll.push_encoded(b) {
                    out.total_kmers += 1;
                    if progress_enabled {
                        local_progress.kmers += 1;
                    }
                    let part = partition_u64(v, cfg.partitions, partitions_pow2, partition_shift);
                    buffers_u64[part].push(v);
                    buffers_u64_total[part] += 1;
                    if buffers_u64_total[part] >= cfg.chunk_kmers {
                        flush_partition(
                            KmerMode::U64,
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
                            scratch_u64,
                            scratch_u128,
                            out,
                        );
                    }
                }
            }
        }
        KmerMode::U128 => {
            for &ch in seq {
                let b = encode_base(ch);
                if b == INVALID_BASE {
                    roll.reset();
                    continue;
                }
                if let Some(KmerKey::U128(v)) = roll.push_encoded(b) {
                    out.total_kmers += 1;
                    if progress_enabled {
                        local_progress.kmers += 1;
                    }
                    let part = partition_u128(v, cfg.partitions, partitions_pow2, partition_shift);
                    buffers_u128[part].push(v);
                    buffers_u128_total[part] += 1;
                    if buffers_u128_total[part] >= cfg.chunk_kmers {
                        flush_partition(
                            KmerMode::U128,
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
                            scratch_u64,
                            scratch_u128,
                            out,
                        );
                    }
                }
            }
        }
        KmerMode::Packed(_) => {
            for &ch in seq {
                let b = encode_base(ch);
                if b == INVALID_BASE {
                    roll.reset();
                    continue;
                }
                if let Some(KmerKey::Packed(v)) = roll.push_encoded(b) {
                    out.total_kmers += 1;
                    if progress_enabled {
                        local_progress.kmers += 1;
                    }
                    let part = partition_packed(
                        &v.words,
                        cfg.partitions,
                        partitions_pow2,
                        partition_shift,
                    );
                    buffers_packed[part].push_back(v.words.into_vec());
                    buffers_packed_total[part] += 1;
                    if buffers_packed_total[part] >= cfg.chunk_kmers {
                        flush_partition(
                            KmerMode::Packed(words_for_k(cfg.k)),
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
                            scratch_u64,
                            scratch_u128,
                            out,
                        );
                    }
                }
            }
        }
    }
}

/// 中文：校验 `count` 配置是否合法。
/// English: Validate that the `count` configuration is internally consistent.
pub fn validate_cfg(cfg: &CountConfig) -> Result<()> {
    if cfg.k == 0 {
        bail!("k must be > 0");
    }
    if cfg.partitions == 0 {
        bail!("partitions must be > 0");
    }
    if cfg.threads == 0 {
        bail!("threads must be > 0");
    }
    Ok(())
}

pub fn sample_input_profile(path: &Path, sample_reads: usize) -> Result<InputProfile> {
    let mut reader = open_seq_reader(path)?;
    let mut seq = Vec::with_capacity(512);
    let mut reads = 0usize;
    let mut total_bases = 0usize;
    let mut max_read_len = 0usize;
    while reads < sample_reads && reader.next_sequence(&mut seq)? {
        reads += 1;
        total_bases += seq.len();
        max_read_len = max_read_len.max(seq.len());
    }
    if reads == 0 {
        return Ok(InputProfile::default());
    }
    Ok(InputProfile {
        sampled_reads: reads,
        mean_read_len: total_bases as f64 / reads as f64,
        max_read_len,
    })
}

/// 中文：优化版计数主流程。输入是 FASTQ 路径和配置，输出是已归并的计数结果。
/// English: Optimized counting pipeline. It takes a FASTQ path and config and
/// returns the fully merged count result.
pub fn count_optimized(path: &Path, cfg: &CountConfig) -> Result<CountOutcome> {
    validate_cfg(cfg)?;
    let mode = KmerMode::from_k(cfg.k);
    let tuning = ingest_tuning(cfg.threads, cfg.max_memory_bytes);
    let gzip_kind = detect_gzip_kind(path)?;
    let pipeline = pipeline_threads(cfg.threads);
    let count_workers = match gzip_kind {
        Some(GzipKind::PlainGzip) => cfg
            .threads
            .saturating_sub(pipeline.gzip_decompressors)
            .max(1),
        Some(GzipKind::Bgzf) => cfg
            .threads
            .saturating_sub(pipeline.bgzf_decompressors + 1)
            .max(1),
        None => cfg.threads.max(1),
    };

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

    let mut senders = Vec::with_capacity(count_workers);
    let mut workers = Vec::with_capacity(count_workers);
    // increase channel buffering to better decouple producer/consumer
    let queue_cap = tuning.worker_queue_cap;

    for _ in 0..count_workers {
        let (tx, rx) = bounded::<WorkItem>(queue_cap);
        senders.push(tx);
        let cfg_cloned = cfg.clone();
        workers.push(thread::spawn(move || worker_loop(rx, &cfg_cloned, mode)));
    }

    dispatch_batches(path, cfg, &senders, gzip_kind, pipeline, tuning)?;

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
    let mut partition_routing = Duration::ZERO;
    let mut sorting = Duration::ZERO;
    let mut total_kmers = 0u64;
    let mut total_reads = 0u64;

    for o in &outputs {
        kmer_generation += o.timings.kmer_generation;
        partition_routing += o.timings.partition_routing;
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
            if let Some(perf) = &cfg.perf_counters {
                let active_partitions = by_partition.iter().filter(|part| !part.is_empty()).count();
                let total_chunks = by_partition.iter().map(Vec::len).sum::<usize>();
                perf.add_merge_inputs(by_partition.len(), active_partitions, total_chunks);
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
            partition_routing,
            sorting,
            merging,
            total,
        },
    })
}

pub fn count_optimized_u64_inmem_reducer(
    path: &Path,
    cfg: &CountConfig,
) -> Result<CountOutcome> {
    validate_cfg(cfg)?;
    let gzip_kind = detect_gzip_kind(path)?;
    let pipeline = pipeline_threads(cfg.threads);
    let owner = u64_owner_pipeline_threads(cfg.threads, gzip_kind, cfg.k);
    let owner_tuning = u64_owner_tuning(cfg, owner);
    let combine_counts = should_combine_u64_owner_batches(cfg, owner.reducers);
    let owner_batch_kmers = u64_owner_batch_kmers(cfg, owner.reducers);

    let t_total = Instant::now();
    let t_parse_start = Instant::now();

    let mut reducer_senders = Vec::with_capacity(owner.reducers);
    let mut reducer_handles = Vec::with_capacity(owner.reducers);
    for _reducer_id in 0..owner.reducers {
        let (tx, rx) = bounded::<U64ReducerInput>(owner_tuning.reducer_queue_cap);
        reducer_senders.push(tx);
        reducer_handles.push(thread::spawn(move || u64_inmem_reducer_loop(rx)));
    }

    let mut extractor_senders = Vec::with_capacity(owner.extractors);
    let mut extractor_handles = Vec::with_capacity(owner.extractors);
    for _ in 0..owner.extractors {
        let (tx, rx) = bounded::<WorkItem>(owner_tuning.extractor_queue_cap);
        extractor_senders.push(tx);
        let cfg_cloned = cfg.clone();
        let reducer_txs = reducer_senders.clone();
        extractor_handles.push(thread::spawn(move || {
            worker_loop_u64_owner(
                rx,
                &cfg_cloned,
                &reducer_txs,
                combine_counts,
                owner_batch_kmers,
            )
        }));
    }

    dispatch_batches(
        path,
        cfg,
        &extractor_senders,
        gzip_kind,
        pipeline,
        owner_tuning.ingest,
    )?;
    drop(extractor_senders);
    let parsing = t_parse_start.elapsed();

    let mut total_kmers = 0u64;
    let mut total_reads = 0u64;
    let mut kmer_generation = Duration::ZERO;
    for handle in extractor_handles {
        let out = handle
            .join()
            .map_err(|_| anyhow::anyhow!("u64 inmem extractor panicked"))??;
        total_kmers += out.total_kmers;
        total_reads += out.total_reads;
        kmer_generation += out.timings.kmer_generation;
    }

    drop(reducer_senders);
    let mut reducer_tables = Vec::with_capacity(owner.reducers);
    let mut sorting = Duration::ZERO;
    for handle in reducer_handles {
        let out = handle
            .join()
            .map_err(|_| anyhow::anyhow!("u64 inmem reducer panicked"))??;
        sorting += out.sorting;
        reducer_tables.push(out.table);
    }

    let t_merge = Instant::now();
    // Every key is routed by partition(owner), so reducers own disjoint key ranges.
    // That means we can concatenate reducer-local sorted tables directly and avoid
    // an expensive global k-way merge.
    let total_len: usize = reducer_tables.iter().map(Vec::len).sum();
    let mut all = Vec::with_capacity(total_len);
    for table in reducer_tables {
        all.extend(table);
    }
    let table = CountTable::U64(all);
    let merging = t_merge.elapsed();

    Ok(CountOutcome {
        mode: KmerMode::U64,
        table,
        total_kmers,
        total_reads,
        canonical: cfg.canonical,
        timings: TimingBreakdown {
            parsing,
            kmer_generation,
            partition_routing: Duration::ZERO,
            sorting,
            merging,
            total: t_total.elapsed(),
        },
    })
}

pub fn count_optimized_u64_partitioned(
    path: &Path,
    cfg: &CountConfig,
) -> Result<PartitionedU64Outcome> {
    validate_cfg(cfg)?;
    let gzip_kind = detect_gzip_kind(path)?;
    let pipeline = pipeline_threads(cfg.threads);
    let owner = u64_owner_pipeline_threads(cfg.threads, gzip_kind, cfg.k);
    let owner_tuning = u64_owner_tuning(cfg, owner);
    let combine_counts = should_combine_u64_owner_batches(cfg, owner.reducers);
    let owner_batch_kmers = u64_owner_batch_kmers(cfg, owner.reducers);
    let temp_dir = temp_count_dir();
    fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create temp dir {}", temp_dir.display()))?;

    let t_total = Instant::now();
    let t_parse_start = Instant::now();

    let mut reducer_senders = Vec::with_capacity(owner.reducers);
    let mut reducer_handles = Vec::with_capacity(owner.reducers);
    for reducer_id in 0..owner.reducers {
        let (tx, rx) = bounded::<U64ReducerInput>(owner_tuning.reducer_queue_cap);
        reducer_senders.push(tx);
        let cfg_cloned = cfg.clone();
        let temp_dir_cloned = temp_dir.clone();
        reducer_handles.push(thread::spawn(move || {
            u64_reducer_loop(
                rx,
                &cfg_cloned,
                reducer_id,
                owner.reducers,
                &temp_dir_cloned,
                owner_tuning.pending_kmers_limit,
            )
        }));
    }

    let mut extractor_senders = Vec::with_capacity(owner.extractors);
    let mut extractor_handles = Vec::with_capacity(owner.extractors);
    for _ in 0..owner.extractors {
        let (tx, rx) = bounded::<WorkItem>(owner_tuning.extractor_queue_cap);
        extractor_senders.push(tx);
        let cfg_cloned = cfg.clone();
        let reducer_txs = reducer_senders.clone();
        extractor_handles.push(thread::spawn(move || {
            worker_loop_u64_owner(
                rx,
                &cfg_cloned,
                &reducer_txs,
                combine_counts,
                owner_batch_kmers,
            )
        }));
    }

    let dispatch_result = dispatch_batches(
        path,
        cfg,
        &extractor_senders,
        gzip_kind,
        pipeline,
        owner_tuning.ingest,
    );
    drop(extractor_senders);
    let parsing = t_parse_start.elapsed();
    dispatch_result?;

    let mut total_kmers = 0u64;
    let mut total_reads = 0u64;
    let mut kmer_generation = Duration::ZERO;
    for handle in extractor_handles {
        let out = handle
            .join()
            .map_err(|_| anyhow::anyhow!("u64 owner extractor panicked"))??;
        total_kmers += out.total_kmers;
        total_reads += out.total_reads;
        kmer_generation += out.timings.kmer_generation;
    }

    drop(reducer_senders);
    let mut sorting = Duration::ZERO;
    let mut partition_chunks = vec![Vec::<PathBuf>::new(); cfg.partitions];
    for handle in reducer_handles {
        let out = handle
            .join()
            .map_err(|_| anyhow::anyhow!("u64 reducer thread panicked"))??;
        sorting += out.sorting;
        for (part, files) in out.partition_chunks.into_iter().enumerate() {
            if !files.is_empty() {
                partition_chunks[part] = files;
            }
        }
    }

    Ok(PartitionedU64Outcome {
        partition_chunks,
        temp_dir,
        total_kmers,
        total_reads,
        canonical: cfg.canonical,
        timings: TimingBreakdown {
            parsing,
            kmer_generation,
            partition_routing: Duration::ZERO,
            sorting,
            merging: Duration::ZERO,
            total: t_total.elapsed(),
        },
        spill_stats: None,
    })
}

pub fn count_optimized_u128_partitioned(
    path: &Path,
    cfg: &CountConfig,
) -> Result<PartitionedU128Outcome> {
    validate_cfg(cfg)?;
    let gzip_kind = detect_gzip_kind(path)?;
    let pipeline = pipeline_threads(cfg.threads);
    let count_workers = match gzip_kind {
        Some(GzipKind::PlainGzip) => cfg
            .threads
            .saturating_sub(pipeline.gzip_decompressors)
            .max(1),
        Some(GzipKind::Bgzf) => cfg
            .threads
            .saturating_sub(pipeline.bgzf_decompressors + 1)
            .max(1),
        None => cfg.threads.max(1),
    };
    let tuning = local_spill_ingest_tuning(
        cfg,
        count_workers,
        ingest_tuning(cfg.threads, cfg.max_memory_bytes),
    );
    let temp_dir = temp_count_dir();
    fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create temp dir {}", temp_dir.display()))?;

    let t_total = Instant::now();
    let t_parse_start = Instant::now();

    let mut senders = Vec::with_capacity(count_workers);
    let mut workers = Vec::with_capacity(count_workers);
    let queue_cap = tuning.worker_queue_cap;

    for worker_id in 0..count_workers {
        let (tx, rx) = bounded::<WorkItem>(queue_cap);
        senders.push(tx);
        let cfg_cloned = cfg.clone();
        let temp_dir_cloned = temp_dir.clone();
        workers.push(thread::spawn(move || {
            worker_loop_u128_local_spill(rx, &cfg_cloned, &temp_dir_cloned, worker_id, count_workers)
        }));
    }

    dispatch_batches(path, cfg, &senders, gzip_kind, pipeline, tuning)?;
    drop(senders);
    let parsing = t_parse_start.elapsed();

    let mut kmer_generation = Duration::ZERO;
    let mut sorting = Duration::ZERO;
    let mut total_kmers = 0u64;
    let mut total_reads = 0u64;
    let mut partition_chunks = vec![Vec::<PathBuf>::new(); cfg.partitions];
    for worker in workers {
        let out = worker
            .join()
            .map_err(|_| anyhow::anyhow!("u128 partition worker panicked"))??;
        kmer_generation += out.timings.kmer_generation;
        sorting += out.timings.sorting;
        total_kmers += out.total_kmers;
        total_reads += out.total_reads;
        for (part, files) in out.partition_chunks.into_iter().enumerate() {
            if !files.is_empty() {
                partition_chunks[part].extend(files);
            }
        }
    }

    Ok(PartitionedU128Outcome {
        partition_chunks,
        temp_dir,
        total_kmers,
        total_reads,
        canonical: cfg.canonical,
        timings: TimingBreakdown {
            parsing,
            kmer_generation,
            partition_routing: Duration::ZERO,
            sorting,
            merging: Duration::ZERO,
            total: t_total.elapsed(),
        },
    })
}

pub fn count_optimized_u64_chunked(path: &Path, cfg: &CountConfig) -> Result<ChunkedU64Outcome> {
    validate_cfg(cfg)?;
    let mode = KmerMode::U64;
    let tuning = ingest_tuning(cfg.threads, cfg.max_memory_bytes);
    let gzip_kind = detect_gzip_kind(path)?;
    let pipeline = pipeline_threads(cfg.threads);
    let count_workers = match gzip_kind {
        Some(GzipKind::PlainGzip) => cfg
            .threads
            .saturating_sub(pipeline.gzip_decompressors)
            .max(1),
        Some(GzipKind::Bgzf) => cfg
            .threads
            .saturating_sub(pipeline.bgzf_decompressors + 1)
            .max(1),
        None => cfg.threads.max(1),
    };

    let t_total = Instant::now();
    let t_parse_start = Instant::now();

    let mut senders = Vec::with_capacity(count_workers);
    let mut workers = Vec::with_capacity(count_workers);
    let queue_cap = tuning.worker_queue_cap;
    for _ in 0..count_workers {
        let (tx, rx) = bounded::<WorkItem>(queue_cap);
        senders.push(tx);
        let cfg_cloned = cfg.clone();
        workers.push(thread::spawn(move || worker_loop(rx, &cfg_cloned, mode)));
    }

    dispatch_batches(path, cfg, &senders, gzip_kind, pipeline, tuning)?;
    drop(senders);
    let parsing = t_parse_start.elapsed();

    let mut outputs = Vec::with_capacity(workers.len());
    for worker in workers {
        outputs.push(
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("u64 chunked worker panicked"))??,
        );
    }

    let mut kmer_generation = Duration::ZERO;
    let mut sorting = Duration::ZERO;
    let mut total_kmers = 0u64;
    let mut total_reads = 0u64;
    let mut partition_chunks = vec![Vec::<ChunkU64>::new(); cfg.partitions];
    for out in outputs {
        kmer_generation += out.timings.kmer_generation;
        sorting += out.timings.sorting;
        total_kmers += out.total_kmers;
        total_reads += out.total_reads;
        for (part, chunks) in out.chunks_u64.into_iter().enumerate() {
            if !chunks.is_empty() {
                partition_chunks[part].extend(chunks);
            }
        }
    }

    Ok(ChunkedU64Outcome {
        partition_chunks,
        total_kmers,
        total_reads,
        canonical: cfg.canonical,
        timings: TimingBreakdown {
            parsing,
            kmer_generation,
            partition_routing: Duration::ZERO,
            sorting,
            merging: Duration::ZERO,
            total: t_total.elapsed(),
        },
    })
}

pub fn count_optimized_u64_local_spill(
    path: &Path,
    cfg: &CountConfig,
) -> Result<PartitionedU64Outcome> {
    validate_cfg(cfg)?;
    let gzip_kind = detect_gzip_kind(path)?;
    let pipeline = pipeline_threads(cfg.threads);
    let count_workers = match gzip_kind {
        Some(GzipKind::PlainGzip) => cfg
            .threads
            .saturating_sub(pipeline.gzip_decompressors)
            .max(1),
        Some(GzipKind::Bgzf) => cfg
            .threads
            .saturating_sub(pipeline.bgzf_decompressors + 1)
            .max(1),
        None => cfg.threads.max(1),
    };
    let tuning = local_spill_ingest_tuning(
        cfg,
        count_workers,
        ingest_tuning(cfg.threads, cfg.max_memory_bytes),
    );
    let temp_dir = temp_count_dir();
    fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create temp dir {}", temp_dir.display()))?;

    let t_total = Instant::now();
    let t_parse_start = Instant::now();

    let mut senders = Vec::with_capacity(count_workers);
    let mut workers = Vec::with_capacity(count_workers);
    for worker_id in 0..count_workers {
        let (tx, rx) = bounded::<WorkItem>(tuning.worker_queue_cap);
        senders.push(tx);
        let cfg_cloned = cfg.clone();
        let temp_dir_cloned = temp_dir.clone();
        workers.push(thread::spawn(move || {
            worker_loop_u64_local_spill(rx, &cfg_cloned, &temp_dir_cloned, worker_id, count_workers)
        }));
    }

    dispatch_batches(path, cfg, &senders, gzip_kind, pipeline, tuning)?;
    drop(senders);
    let parsing = t_parse_start.elapsed();

    let mut partition_chunks = vec![Vec::<PathBuf>::new(); cfg.partitions];
    let mut total_kmers = 0u64;
    let mut total_reads = 0u64;
    let mut kmer_generation = Duration::ZERO;
    let mut sorting = Duration::ZERO;
    let mut spill_stats = SpillBuildStats::default();
    for handle in workers {
        let out = handle
            .join()
            .map_err(|_| anyhow::anyhow!("u64 local-spill worker panicked"))??;
        total_kmers += out.total_kmers;
        total_reads += out.total_reads;
        kmer_generation += out.timings.kmer_generation;
        sorting += out.timings.sorting;
        spill_stats.merge_in(&out.spill_stats);
        for (part, files) in out.partition_chunks.into_iter().enumerate() {
            if !files.is_empty() {
                partition_chunks[part].extend(files);
            }
        }
    }
    spill_stats.finalize_from_partition_chunks(&partition_chunks);

    Ok(PartitionedU64Outcome {
        partition_chunks,
        temp_dir,
        total_kmers,
        total_reads,
        canonical: cfg.canonical,
        timings: TimingBreakdown {
            parsing,
            kmer_generation,
            partition_routing: Duration::ZERO,
            sorting,
            merging: spill_stats.spill_local_merge,
            total: t_total.elapsed(),
        },
        spill_stats: Some(spill_stats),
    })
}

struct ReducerThreadOutput {
    partition_chunks: Vec<Vec<PathBuf>>,
    sorting: Duration,
}

struct SpillWorkerOutput {
    partition_chunks: Vec<Vec<PathBuf>>,
    total_kmers: u64,
    total_reads: u64,
    timings: WorkerTimings,
    spill_stats: SpillBuildStats,
}

struct InMemoryReducerOutput {
    table: Vec<(u64, u64)>,
    sorting: Duration,
}

fn worker_loop_u64_owner(
    rx: Receiver<WorkItem>,
    cfg: &CountConfig,
    reducer_txs: &[Sender<U64ReducerInput>],
    combine_counts: bool,
    owner_batch_kmers: usize,
) -> Result<WorkerOutput> {
    let mut out = WorkerOutput {
        chunks_u64: vec![Vec::new(); cfg.partitions],
        chunks_u128: Vec::new(),
        chunks_packed: Vec::new(),
        total_kmers: 0,
        total_reads: 0,
        timings: WorkerTimings::default(),
    };
    let progress_ref = cfg.progress_counters.clone();
    let progress_enabled = progress_ref.is_some();
    let mut local_progress = LocalProgress::default();
    let mut owner_buffers = vec![
        Vec::<u64>::with_capacity(owner_batch_kmers.min(16_384));
        reducer_txs.len()
    ];
    let mut owner_scratch = Vec::<u64>::with_capacity(owner_batch_kmers.min(U64_OWNER_BATCH_KMERS));
    let partitions_pow2 = cfg.partitions.is_power_of_two();
    let partition_shift = if partitions_pow2 {
        64usize - (cfg.partitions.trailing_zeros() as usize)
    } else {
        0usize
    };
    let mask64 = if cfg.k == 32 {
        u64::MAX
    } else {
        (1u64 << (2 * cfg.k)) - 1
    };
    let rev_shift = (cfg.k - 1) * 2;

    while let Ok(item) = rx.recv() {
        let t_batch = Instant::now();
        match item {
            WorkItem::SeqBatch(batch) => {
                for &(start, len) in &batch.offsets {
                    let s = start as usize;
                    let e = s + len as usize;
                    process_sequence_u64_owner(
                        &batch.data[s..e],
                        cfg,
                        partitions_pow2,
                        partition_shift,
                        mask64,
                        rev_shift,
                        reducer_txs.len(),
                        owner_batch_kmers,
                        &mut owner_buffers,
                        &mut owner_scratch,
                        reducer_txs,
                        combine_counts,
                        progress_enabled,
                        &mut local_progress,
                        &mut out,
                    )?;
                }
            }
            WorkItem::FastqChunk(chunk) => {
                process_fastq_chunk_u64_owner_direct(
                    &chunk,
                    cfg,
                    partitions_pow2,
                    partition_shift,
                    mask64,
                    rev_shift,
                    reducer_txs.len(),
                    owner_batch_kmers,
                    &mut owner_buffers,
                    &mut owner_scratch,
                    reducer_txs,
                    combine_counts,
                    progress_enabled,
                    &mut local_progress,
                    &mut out,
                )?;
            }
        }
        if progress_enabled {
            local_progress.flush(&progress_ref);
        }
        out.timings.kmer_generation += t_batch.elapsed();
    }

    if progress_enabled {
        local_progress.flush(&progress_ref);
    }
    for (owner, mut buf) in owner_buffers.into_iter().enumerate() {
        if !buf.is_empty() {
            flush_u64_owner_batch(
                &mut buf,
                &mut owner_scratch,
                &reducer_txs[owner],
                combine_counts,
            )?;
        }
    }
    Ok(out)
}

#[inline(always)]
fn local_spill_pending_limit(cfg: &CountConfig, count_workers: usize) -> usize {
    let effective_memory =
        cfg.max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(cfg.threads));
    let per_worker_bytes = (effective_memory / count_workers.max(1) as u64) / 16;
    ((per_worker_bytes / (std::mem::size_of::<u64>() as u64)) as usize).clamp(2_000_000, 16_000_000)
}

#[inline(always)]
fn local_spill_part_limit(worker_pending_limit: usize) -> usize {
    (worker_pending_limit / 4).clamp(256_000, 4_000_000)
}

#[inline(always)]
fn fanin_bucket(files: usize) -> usize {
    match files {
        0 | 1 => 0,
        2..=4 => 1,
        5..=8 => 2,
        9..=16 => 3,
        17..=32 => 4,
        _ => 5,
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_partition_u64_to_file(
    part: usize,
    buffers_u64: &mut [Vec<u64>],
    scratch_u64: &mut Vec<u64>,
    temp_dir: &Path,
    worker_id: usize,
    chunk_seq: &mut usize,
    partition_chunks: &mut [Vec<PathBuf>],
    sorting: &mut Duration,
    spill_stats: &mut SpillBuildStats,
) -> Result<usize> {
    const LOCAL_SPILL_RETAIN: usize = 16_384;
    let src = &mut buffers_u64[part];
    if src.is_empty() {
        return Ok(0);
    }
    let flushed = src.len();
    let t = Instant::now();
    radix_sort_u64(src, scratch_u64);
    let path = temp_dir.join(format!(
        "part-{part:05}-worker-{worker_id:03}-chunk-{chunk:08}.u64c",
        chunk = *chunk_seq
    ));
    let file = File::create(&path)
        .with_context(|| format!("failed to create spilled chunk {}", path.display()))?;
    let mut bw = BufWriter::with_capacity(1 << 20, file);
    let mut i = 0usize;
    let mut records = 0u64;
    while i < src.len() {
        let key = src[i];
        let mut c = 1u64;
        i += 1;
        while i < src.len() && src[i] == key {
            c += 1;
            i += 1;
        }
        records += 1;
        bw.write_all(&key.to_le_bytes())?;
        bw.write_all(&c.to_le_bytes())?;
    }
    bw.flush()?;
    src.clear();
    if src.capacity() > (LOCAL_SPILL_RETAIN << 2) {
        src.shrink_to(LOCAL_SPILL_RETAIN);
    }
    partition_chunks[part].push(path);
    *chunk_seq += 1;
    let elapsed = t.elapsed();
    *sorting += elapsed;
    spill_stats.spill_flushes += 1;
    spill_stats.spill_files_created += 1;
    spill_stats.spill_bytes_written += records.saturating_mul(16);
    spill_stats.spill_local_flush += elapsed;
    Ok(flushed)
}

struct SpillChunkReaderU64 {
    reader: BufReader<File>,
    done: bool,
    bytes_total: u64,
}

impl SpillChunkReaderU64 {
    fn open(path: &Path, buf_cap: usize) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open spilled chunk {}", path.display()))?;
        let bytes_total = file.metadata()?.len();
        Ok(Self {
            reader: BufReader::with_capacity(buf_cap, file),
            done: false,
            bytes_total,
        })
    }

    fn next(&mut self) -> Result<Option<(u64, u64)>> {
        if self.done {
            return Ok(None);
        }
        let mut key = [0u8; 8];
        match self.reader.read_exact(&mut key) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.done = true;
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        }
        let mut count = [0u8; 8];
        self.reader.read_exact(&mut count)?;
        Ok(Some((u64::from_le_bytes(key), u64::from_le_bytes(count))))
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct SpillHeapItemU64 {
    key: u64,
    count: u64,
    reader_idx: usize,
}

impl Ord for SpillHeapItemU64 {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other.key.cmp(&self.key)
    }
}

impl PartialOrd for SpillHeapItemU64 {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

#[inline(always)]
fn local_spill_merge_reader_buf_cap(open_files: usize) -> usize {
    const LOCAL_MERGE_READER_BUDGET: usize = 16 << 20;
    const MIN_BUF: usize = 16 << 10;
    const MAX_BUF: usize = 128 << 10;
    (LOCAL_MERGE_READER_BUDGET / open_files.max(1)).clamp(MIN_BUF, MAX_BUF)
}

fn merge_u64_count_files_to_path(inputs: &[PathBuf], out_path: &Path) -> Result<(u64, u64)> {
    let mut readers = Vec::with_capacity(inputs.len());
    let mut heap = BinaryHeap::with_capacity(inputs.len());
    let reader_buf_cap = local_spill_merge_reader_buf_cap(inputs.len());
    let mut bytes_read = 0u64;

    for (idx, path) in inputs.iter().enumerate() {
        let mut reader = SpillChunkReaderU64::open(path, reader_buf_cap)?;
        bytes_read += reader.bytes_total;
        if let Some((key, count)) = reader.next()? {
            heap.push(SpillHeapItemU64 {
                key,
                count,
                reader_idx: idx,
            });
        }
        readers.push(reader);
    }

    let file = File::create(out_path)
        .with_context(|| format!("failed to create local-merged chunk {}", out_path.display()))?;
    let mut bw = BufWriter::with_capacity(1 << 20, file);
    let mut records = 0u64;
    while let Some(item) = heap.pop() {
        let key = item.key;
        let mut total = item.count;
        if let Some((next_key, next_count)) = readers[item.reader_idx].next()? {
            heap.push(SpillHeapItemU64 {
                key: next_key,
                count: next_count,
                reader_idx: item.reader_idx,
            });
        }
        while let Some(peek) = heap.peek() {
            if peek.key != key {
                break;
            }
            let next = heap.pop().expect("peeked heap item must exist");
            total += next.count;
            if let Some((next_key, next_count)) = readers[next.reader_idx].next()? {
                heap.push(SpillHeapItemU64 {
                    key: next_key,
                    count: next_count,
                    reader_idx: next.reader_idx,
                });
            }
        }
        records += 1;
        bw.write_all(&key.to_le_bytes())?;
        bw.write_all(&total.to_le_bytes())?;
    }
    bw.flush()?;
    Ok((bytes_read, records.saturating_mul(16)))
}

fn maybe_compact_worker_partition_u64_files(
    part: usize,
    files: &mut Vec<PathBuf>,
    temp_dir: &Path,
    worker_id: usize,
    chunk_seq: &mut usize,
    spill_stats: &mut SpillBuildStats,
) -> Result<()> {
    const LOCAL_SPILL_COMPACT_TRIGGER: usize = 16;

    while files.len() >= LOCAL_SPILL_COMPACT_TRIGGER {
        let group = std::mem::take(files);
        let path = temp_dir.join(format!(
            "part-{part:05}-worker-{worker_id:03}-compact-chunk-{chunk:08}.u64c",
            chunk = *chunk_seq
        ));
        let t_merge = Instant::now();
        let (bytes_read, bytes_written) = merge_u64_count_files_to_path(&group, &path)?;
        spill_stats.spill_local_merge += t_merge.elapsed();
        spill_stats.spill_bytes_read_local_merge += bytes_read;
        spill_stats.spill_bytes_written += bytes_written;
        spill_stats.spill_files_created += 1;
        spill_stats.local_merge_groups += 1;
        spill_stats.local_merge_passes_total += 1;
        spill_stats.local_merge_passes_max = spill_stats.local_merge_passes_max.max(1);
        spill_stats.local_merge_fanin_sum += group.len() as u64;
        spill_stats.local_merge_fanin_max =
            spill_stats.local_merge_fanin_max.max(group.len() as u64);
        *chunk_seq += 1;
        for path in &group {
            let _ = fs::remove_file(path);
        }
        files.push(path);
    }
    Ok(())
}

fn consolidate_worker_partition_u64_files(
    part: usize,
    files: &mut Vec<PathBuf>,
    temp_dir: &Path,
    worker_id: usize,
    chunk_seq: &mut usize,
    spill_stats: &mut SpillBuildStats,
) -> Result<()> {
    const LOCAL_SPILL_MERGE_FANIN: usize = 32;

    if files.len() <= 1 {
        return Ok(());
    }

    let mut current = std::mem::take(files);
    let mut pass = 0u64;
    while current.len() > 1 {
        pass += 1;
        let mut merged_any = false;
        let mut next = Vec::with_capacity(current.len().div_ceil(LOCAL_SPILL_MERGE_FANIN));
        for group in current.chunks(LOCAL_SPILL_MERGE_FANIN) {
            if group.len() == 1 {
                next.push(group[0].clone());
                continue;
            }
            merged_any = true;
            let path = temp_dir.join(format!(
                "part-{part:05}-worker-{worker_id:03}-merge-{pass:03}-chunk-{chunk:08}.u64c",
                chunk = *chunk_seq
            ));
            let t_merge = Instant::now();
            let (bytes_read, bytes_written) = merge_u64_count_files_to_path(group, &path)?;
            spill_stats.spill_local_merge += t_merge.elapsed();
            spill_stats.spill_bytes_read_local_merge += bytes_read;
            spill_stats.spill_bytes_written += bytes_written;
            spill_stats.spill_files_created += 1;
            spill_stats.local_merge_groups += 1;
            spill_stats.local_merge_fanin_sum += group.len() as u64;
            spill_stats.local_merge_fanin_max =
                spill_stats.local_merge_fanin_max.max(group.len() as u64);
            *chunk_seq += 1;
            for path in group {
                let _ = fs::remove_file(path);
            }
            next.push(path);
        }
        if merged_any {
            spill_stats.local_merge_passes_total += 1;
            spill_stats.local_merge_passes_max =
                spill_stats.local_merge_passes_max.max(pass);
        }
        current = next;
    }
    *files = current;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_sequence_u64_spill(
    seq: &[u8],
    cfg: &CountConfig,
    partitions_pow2: bool,
    partition_shift: usize,
    mask64: u64,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    buffers_u64: &mut [Vec<u64>],
    total_pending: &mut usize,
    part_limit: usize,
    scratch_u64: &mut Vec<u64>,
    temp_dir: &Path,
    worker_id: usize,
    chunk_seq: &mut usize,
    partition_chunks: &mut [Vec<PathBuf>],
    sorting: &mut Duration,
    spill_stats: &mut SpillBuildStats,
    total_reads: &mut u64,
    total_kmers: &mut u64,
) -> Result<()> {
    if progress_enabled {
        local_progress.reads += 1;
    }
    *total_reads += 1;

    let mut seen = 0usize;
    let mut fwd = 0u64;
    let mut rev = 0u64;

    for &ch in seq {
        let b = encode_base(ch);
        if b == INVALID_BASE {
            seen = 0;
            fwd = 0;
            rev = 0;
            continue;
        }

        let b64 = b as u64;
        fwd = ((fwd << 2) | b64) & mask64;
        rev = (rev >> 2) | (((3 - b) as u64) << rev_shift);

        if seen < cfg.k {
            seen += 1;
            if seen < cfg.k {
                continue;
            }
        }

        let v = if cfg.canonical { fwd.min(rev) } else { fwd };
        *total_kmers += 1;
        if progress_enabled {
            local_progress.kmers += 1;
        }
        let part = partition_u64(v, cfg.partitions, partitions_pow2, partition_shift);
        buffers_u64[part].push(v);
        *total_pending += 1;
        if buffers_u64[part].len() >= part_limit {
            *total_pending = total_pending.saturating_sub(flush_partition_u64_to_file(
                part,
                buffers_u64,
                scratch_u64,
                temp_dir,
                worker_id,
                chunk_seq,
                partition_chunks,
                sorting,
                spill_stats,
            )?);
            maybe_compact_worker_partition_u64_files(
                part,
                &mut partition_chunks[part],
                temp_dir,
                worker_id,
                chunk_seq,
                spill_stats,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_fastq_chunk_u64_spill_direct(
    chunk: &[u8],
    cfg: &CountConfig,
    partitions_pow2: bool,
    partition_shift: usize,
    mask64: u64,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    buffers_u64: &mut [Vec<u64>],
    total_pending: &mut usize,
    part_limit: usize,
    scratch_u64: &mut Vec<u64>,
    temp_dir: &Path,
    worker_id: usize,
    chunk_seq: &mut usize,
    partition_chunks: &mut [Vec<PathBuf>],
    sorting: &mut Duration,
    spill_stats: &mut SpillBuildStats,
    total_reads: &mut u64,
    total_kmers: &mut u64,
) -> Result<()> {
    process_fastq_chunk_direct(chunk, |line| {
        process_sequence_u64_spill(
            line,
            cfg,
            partitions_pow2,
            partition_shift,
            mask64,
            rev_shift,
            progress_enabled,
            local_progress,
            buffers_u64,
            total_pending,
            part_limit,
            scratch_u64,
            temp_dir,
            worker_id,
            chunk_seq,
            partition_chunks,
            sorting,
            spill_stats,
            total_reads,
            total_kmers,
        )
    })
}

fn worker_loop_u64_local_spill(
    rx: Receiver<WorkItem>,
    cfg: &CountConfig,
    temp_dir: &Path,
    worker_id: usize,
    count_workers: usize,
) -> Result<SpillWorkerOutput> {
    let mut out = SpillWorkerOutput {
        partition_chunks: vec![Vec::new(); cfg.partitions],
        total_kmers: 0,
        total_reads: 0,
        timings: WorkerTimings::default(),
        spill_stats: SpillBuildStats::default(),
    };
    let progress_ref = cfg.progress_counters.clone();
    let progress_enabled = progress_ref.is_some();
    let mut local_progress = LocalProgress::default();
    let mut buffers_u64 =
        vec![Vec::<u64>::with_capacity((cfg.chunk_kmers / cfg.partitions.max(1)).max(1024)); cfg.partitions];
    let mut scratch_u64 = Vec::<u64>::new();
    let mut total_pending = 0usize;
    let worker_pending_limit = local_spill_pending_limit(cfg, count_workers);
    let part_limit = local_spill_part_limit(worker_pending_limit);
    let mut chunk_seq = 0usize;
    let partitions_pow2 = cfg.partitions.is_power_of_two();
    let partition_shift = if partitions_pow2 {
        64usize - (cfg.partitions.trailing_zeros() as usize)
    } else {
        0usize
    };
    let mask64 = if cfg.k == 32 {
        u64::MAX
    } else {
        (1u64 << (2 * cfg.k)) - 1
    };
    let rev_shift = (cfg.k - 1) * 2;

    while let Ok(item) = rx.recv() {
        let t_batch = Instant::now();
        match item {
            WorkItem::SeqBatch(batch) => {
                for &(start, len) in &batch.offsets {
                    let s = start as usize;
                    let e = s + len as usize;
                    process_sequence_u64_spill(
                        &batch.data[s..e],
                        cfg,
                        partitions_pow2,
                        partition_shift,
                        mask64,
                        rev_shift,
                        progress_enabled,
                        &mut local_progress,
                        &mut buffers_u64,
                        &mut total_pending,
                        part_limit,
                        &mut scratch_u64,
                        temp_dir,
                        worker_id,
                        &mut chunk_seq,
                        &mut out.partition_chunks,
                        &mut out.timings.sorting,
                        &mut out.spill_stats,
                        &mut out.total_reads,
                        &mut out.total_kmers,
                    )?;
                }
            }
            WorkItem::FastqChunk(chunk) => {
                process_fastq_chunk_u64_spill_direct(
                    &chunk,
                    cfg,
                    partitions_pow2,
                    partition_shift,
                    mask64,
                    rev_shift,
                    progress_enabled,
                    &mut local_progress,
                    &mut buffers_u64,
                    &mut total_pending,
                    part_limit,
                    &mut scratch_u64,
                    temp_dir,
                    worker_id,
                    &mut chunk_seq,
                    &mut out.partition_chunks,
                    &mut out.timings.sorting,
                    &mut out.spill_stats,
                    &mut out.total_reads,
                    &mut out.total_kmers,
                )?;
            }
        }

        while total_pending > worker_pending_limit {
            let Some((part, len)) = buffers_u64
                .iter()
                .enumerate()
                .map(|(idx, buf)| (idx, buf.len()))
                .max_by_key(|(_, len)| *len)
            else {
                break;
            };
            if len == 0 {
                break;
            }
            total_pending = total_pending.saturating_sub(flush_partition_u64_to_file(
                part,
                &mut buffers_u64,
                &mut scratch_u64,
                temp_dir,
                worker_id,
                &mut chunk_seq,
                &mut out.partition_chunks,
                &mut out.timings.sorting,
                &mut out.spill_stats,
            )?);
            maybe_compact_worker_partition_u64_files(
                part,
                &mut out.partition_chunks[part],
                temp_dir,
                worker_id,
                &mut chunk_seq,
                &mut out.spill_stats,
            )?;
        }

        if progress_enabled {
            local_progress.flush(&progress_ref);
        }
        out.timings.kmer_generation += t_batch.elapsed();
    }

    if progress_enabled {
        local_progress.flush(&progress_ref);
    }
    for part in 0..cfg.partitions {
        total_pending = total_pending.saturating_sub(flush_partition_u64_to_file(
            part,
            &mut buffers_u64,
            &mut scratch_u64,
            temp_dir,
            worker_id,
            &mut chunk_seq,
            &mut out.partition_chunks,
            &mut out.timings.sorting,
            &mut out.spill_stats,
        )?);
    }
    for part in 0..cfg.partitions {
        consolidate_worker_partition_u64_files(
            part,
            &mut out.partition_chunks[part],
            temp_dir,
            worker_id,
            &mut chunk_seq,
            &mut out.spill_stats,
        )?;
    }
    Ok(out)
}

#[inline(always)]
fn local_spill_pending_limit_u128(cfg: &CountConfig, count_workers: usize) -> usize {
    let effective_memory =
        cfg.max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(cfg.threads));
    let per_worker_bytes = (effective_memory / count_workers.max(1) as u64) / 24;
    ((per_worker_bytes / (std::mem::size_of::<u128>() as u64)) as usize).clamp(500_000, 8_000_000)
}

#[inline(always)]
fn local_spill_part_limit_u128(worker_pending_limit: usize) -> usize {
    (worker_pending_limit / 8).clamp(64_000, 2_000_000)
}

#[allow(clippy::too_many_arguments)]
fn flush_partition_u128_to_file(
    part: usize,
    buffers_u128: &mut [Vec<u128>],
    scratch_u128: &mut Vec<u128>,
    temp_dir: &Path,
    worker_id: usize,
    chunk_seq: &mut usize,
    partition_chunks: &mut [Vec<PathBuf>],
    sorting: &mut Duration,
) -> Result<usize> {
    const LOCAL_SPILL_RETAIN: usize = 8_192;
    let src = &mut buffers_u128[part];
    if src.is_empty() {
        return Ok(0);
    }
    let flushed = src.len();
    let t = Instant::now();
    if src.len() >= (1 << 16) {
        radix_sort_u128(src, scratch_u128);
    } else {
        src.sort_unstable();
    }
    let path = temp_dir.join(format!(
        "part-{part:05}-worker-{worker_id:03}-chunk-{chunk:08}.u128c",
        chunk = *chunk_seq
    ));
    let file = File::create(&path)
        .with_context(|| format!("failed to create spilled chunk {}", path.display()))?;
    let mut bw = BufWriter::with_capacity(1 << 20, file);
    let mut i = 0usize;
    while i < src.len() {
        let key = src[i];
        let mut c = 1u64;
        i += 1;
        while i < src.len() && src[i] == key {
            c += 1;
            i += 1;
        }
        bw.write_all(&key.to_le_bytes())?;
        bw.write_all(&c.to_le_bytes())?;
    }
    bw.flush()?;
    src.clear();
    if src.capacity() > (LOCAL_SPILL_RETAIN << 2) {
        src.shrink_to(LOCAL_SPILL_RETAIN);
    }
    partition_chunks[part].push(path);
    *chunk_seq += 1;
    *sorting += t.elapsed();
    Ok(flushed)
}

#[allow(clippy::too_many_arguments)]
fn process_sequence_u128_spill(
    seq: &[u8],
    cfg: &CountConfig,
    partitions_pow2: bool,
    partition_shift: usize,
    mask128: u128,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    buffers_u128: &mut [Vec<u128>],
    total_pending: &mut usize,
    part_limit: usize,
    scratch_u128: &mut Vec<u128>,
    temp_dir: &Path,
    worker_id: usize,
    chunk_seq: &mut usize,
    partition_chunks: &mut [Vec<PathBuf>],
    sorting: &mut Duration,
    total_reads: &mut u64,
    total_kmers: &mut u64,
) -> Result<()> {
    if progress_enabled {
        local_progress.reads += 1;
    }
    *total_reads += 1;

    let mut seen = 0usize;
    let mut fwd = 0u128;
    let mut rev = 0u128;

    for &ch in seq {
        let b = encode_base(ch);
        if b == INVALID_BASE {
            seen = 0;
            fwd = 0;
            rev = 0;
            continue;
        }
        let b128 = b as u128;
        fwd = ((fwd << 2) | b128) & mask128;
        rev = (rev >> 2) | (((3 - b) as u128) << rev_shift);
        if seen < cfg.k {
            seen += 1;
            if seen < cfg.k {
                continue;
            }
        }
        let v = if cfg.canonical { fwd.min(rev) } else { fwd };
        *total_kmers += 1;
        if progress_enabled {
            local_progress.kmers += 1;
        }
        let part = partition_u128(v, cfg.partitions, partitions_pow2, partition_shift);
        buffers_u128[part].push(v);
        *total_pending += 1;
        if buffers_u128[part].len() >= part_limit {
            *total_pending = total_pending.saturating_sub(flush_partition_u128_to_file(
                part,
                buffers_u128,
                scratch_u128,
                temp_dir,
                worker_id,
                chunk_seq,
                partition_chunks,
                sorting,
            )?);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_fastq_chunk_u128_spill_direct(
    chunk: &[u8],
    cfg: &CountConfig,
    partitions_pow2: bool,
    partition_shift: usize,
    mask128: u128,
    rev_shift: usize,
    progress_enabled: bool,
    local_progress: &mut LocalProgress,
    buffers_u128: &mut [Vec<u128>],
    total_pending: &mut usize,
    part_limit: usize,
    scratch_u128: &mut Vec<u128>,
    temp_dir: &Path,
    worker_id: usize,
    chunk_seq: &mut usize,
    partition_chunks: &mut [Vec<PathBuf>],
    sorting: &mut Duration,
    total_reads: &mut u64,
    total_kmers: &mut u64,
) -> Result<()> {
    process_fastq_chunk_direct(chunk, |line| {
        process_sequence_u128_spill(
            line,
            cfg,
            partitions_pow2,
            partition_shift,
            mask128,
            rev_shift,
            progress_enabled,
            local_progress,
            buffers_u128,
            total_pending,
            part_limit,
            scratch_u128,
            temp_dir,
            worker_id,
            chunk_seq,
            partition_chunks,
            sorting,
            total_reads,
            total_kmers,
        )
    })
}

fn worker_loop_u128_local_spill(
    rx: Receiver<WorkItem>,
    cfg: &CountConfig,
    temp_dir: &Path,
    worker_id: usize,
    count_workers: usize,
) -> Result<SpillWorkerOutput> {
    let mut out = SpillWorkerOutput {
        partition_chunks: vec![Vec::new(); cfg.partitions],
        total_kmers: 0,
        total_reads: 0,
        timings: WorkerTimings::default(),
        spill_stats: SpillBuildStats::default(),
    };
    let progress_ref = cfg.progress_counters.clone();
    let progress_enabled = progress_ref.is_some();
    let mut local_progress = LocalProgress::default();
    let worker_pending_limit = local_spill_pending_limit_u128(cfg, count_workers);
    let part_limit = local_spill_part_limit_u128(worker_pending_limit);
    let per_part_cap = (part_limit / 64).clamp(64, 512);
    let mut buffers_u128 = vec![Vec::<u128>::with_capacity(per_part_cap); cfg.partitions];
    let mut scratch_u128 = Vec::<u128>::new();
    let mut total_pending = 0usize;
    let mut chunk_seq = 0usize;
    let partitions_pow2 = cfg.partitions.is_power_of_two();
    let partition_shift = if partitions_pow2 {
        64usize - (cfg.partitions.trailing_zeros() as usize)
    } else {
        0usize
    };
    let mask128 = if cfg.k == 64 {
        u128::MAX
    } else {
        (1u128 << (2 * cfg.k)) - 1
    };
    let rev_shift = (cfg.k - 1) * 2;

    while let Ok(item) = rx.recv() {
        let t_batch = Instant::now();
        match item {
            WorkItem::SeqBatch(batch) => {
                for &(start, len) in &batch.offsets {
                    let s = start as usize;
                    let e = s + len as usize;
                    process_sequence_u128_spill(
                        &batch.data[s..e],
                        cfg,
                        partitions_pow2,
                        partition_shift,
                        mask128,
                        rev_shift,
                        progress_enabled,
                        &mut local_progress,
                        &mut buffers_u128,
                        &mut total_pending,
                        part_limit,
                        &mut scratch_u128,
                        temp_dir,
                        worker_id,
                        &mut chunk_seq,
                        &mut out.partition_chunks,
                        &mut out.timings.sorting,
                        &mut out.total_reads,
                        &mut out.total_kmers,
                    )?;
                }
            }
            WorkItem::FastqChunk(chunk) => {
                process_fastq_chunk_u128_spill_direct(
                    &chunk,
                    cfg,
                    partitions_pow2,
                    partition_shift,
                    mask128,
                    rev_shift,
                    progress_enabled,
                    &mut local_progress,
                    &mut buffers_u128,
                    &mut total_pending,
                    part_limit,
                    &mut scratch_u128,
                    temp_dir,
                    worker_id,
                    &mut chunk_seq,
                    &mut out.partition_chunks,
                    &mut out.timings.sorting,
                    &mut out.total_reads,
                    &mut out.total_kmers,
                )?;
            }
        }

        while total_pending > worker_pending_limit {
            let Some((part, len)) = buffers_u128
                .iter()
                .enumerate()
                .map(|(idx, buf)| (idx, buf.len()))
                .max_by_key(|(_, len)| *len)
            else {
                break;
            };
            if len == 0 {
                break;
            }
            total_pending = total_pending.saturating_sub(flush_partition_u128_to_file(
                part,
                &mut buffers_u128,
                &mut scratch_u128,
                temp_dir,
                worker_id,
                &mut chunk_seq,
                &mut out.partition_chunks,
                &mut out.timings.sorting,
            )?);
        }

        if progress_enabled {
            local_progress.flush(&progress_ref);
        }
        out.timings.kmer_generation += t_batch.elapsed();
    }

    if progress_enabled {
        local_progress.flush(&progress_ref);
    }
    for part in 0..cfg.partitions {
        total_pending = total_pending.saturating_sub(flush_partition_u128_to_file(
            part,
            &mut buffers_u128,
            &mut scratch_u128,
            temp_dir,
            worker_id,
            &mut chunk_seq,
            &mut out.partition_chunks,
            &mut out.timings.sorting,
        )?);
    }
    Ok(out)
}

fn u64_inmem_reducer_loop(rx: Receiver<U64ReducerInput>) -> Result<InMemoryReducerOutput> {
    let mut counts = FxHashMap::<u64, u64>::with_capacity_and_hasher(1 << 20, Default::default());
    while let Ok(input) = rx.recv() {
        match input {
            U64ReducerInput::Raw(batch) => {
                for key in batch {
                    *counts.entry(key).or_insert(0) += 1;
                }
            }
            U64ReducerInput::Counted(batch) => {
                for (key, count) in batch {
                    *counts.entry(key).or_insert(0) += count;
                }
            }
        }
    }

    let t = Instant::now();
    let mut table: Vec<(u64, u64)> = counts.into_iter().collect();
    table.par_sort_unstable_by_key(|&(key, _)| key);
    Ok(InMemoryReducerOutput {
        table,
        sorting: t.elapsed(),
    })
}

fn u64_reducer_loop(
    rx: Receiver<U64ReducerInput>,
    cfg: &CountConfig,
    reducer_id: usize,
    reducers: usize,
    temp_dir: &Path,
    pending_limit: usize,
) -> Result<ReducerThreadOutput> {
    let start = cfg.partitions * reducer_id / reducers;
    let end = cfg.partitions * (reducer_id + 1) / reducers;
    let local_parts = end.saturating_sub(start);
    let partitions_pow2 = cfg.partitions.is_power_of_two();
    let partition_shift = if partitions_pow2 {
        64usize - (cfg.partitions.trailing_zeros() as usize)
    } else {
        0usize
    };
    let mut raw_buffers = vec![Vec::<u64>::new(); local_parts];
    let mut counted_pending =
        FxHashMap::<u64, u64>::with_capacity_and_hasher(pending_limit.min(1 << 20), Default::default());
    let mut counted_buffers = vec![Vec::<(u64, u64)>::new(); local_parts];
    let mut chunk_paths = vec![Vec::<PathBuf>::new(); cfg.partitions];
    let mut scratch = Vec::<u64>::new();
    let mut pending_total = 0usize;
    let mut sorting = Duration::ZERO;
    let mut chunk_seq = 0usize;

    while let Ok(input) = rx.recv() {
        match input {
            U64ReducerInput::Raw(batch) => {
                for v in batch {
                    let part = partition_u64(v, cfg.partitions, partitions_pow2, partition_shift);
                    if part < start || part >= end {
                        bail!("partition ownership violation");
                    }
                    raw_buffers[part - start].push(v);
                    pending_total += 1;
                }
                while pending_total > pending_limit {
                    let Some((local_idx, _)) =
                        raw_buffers.iter().enumerate().max_by_key(|(_, v)| v.len())
                    else {
                        break;
                    };
                    if raw_buffers[local_idx].is_empty() {
                        break;
                    }
                    let len = raw_buffers[local_idx].len();
                    let t = Instant::now();
                    spill_u64_raw_partition(
                        &mut raw_buffers[local_idx],
                        &mut scratch,
                        temp_dir,
                        start + local_idx,
                        chunk_seq,
                        &mut chunk_paths[start + local_idx],
                    )?;
                    sorting += t.elapsed();
                    pending_total = pending_total.saturating_sub(len);
                    chunk_seq += 1;
                }
            }
            U64ReducerInput::Counted(batch) => {
                for (key, count) in batch {
                    match counted_pending.entry(key) {
                        std::collections::hash_map::Entry::Occupied(mut entry) => {
                            *entry.get_mut() += count;
                        }
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(count);
                            pending_total += 1;
                        }
                    }
                }
                if pending_total > pending_limit {
                    let t = Instant::now();
                    let flushed = spill_u64_counted_pending(
                        &mut counted_pending,
                        &mut counted_buffers,
                        cfg.partitions,
                        partitions_pow2,
                        partition_shift,
                        start,
                        end,
                        temp_dir,
                        &mut chunk_seq,
                        &mut chunk_paths,
                    )?;
                    sorting += t.elapsed();
                    pending_total = pending_total.saturating_sub(flushed);
                }
            }
        }
    }

    if !counted_pending.is_empty() {
        let t = Instant::now();
        spill_u64_counted_pending(
            &mut counted_pending,
            &mut counted_buffers,
            cfg.partitions,
            partitions_pow2,
            partition_shift,
            start,
            end,
            temp_dir,
            &mut chunk_seq,
            &mut chunk_paths,
        )?;
        sorting += t.elapsed();
    }

    for local_idx in 0..local_parts {
        if raw_buffers[local_idx].is_empty() {
            continue;
        }
        let t = Instant::now();
        spill_u64_raw_partition(
            &mut raw_buffers[local_idx],
            &mut scratch,
            temp_dir,
            start + local_idx,
            chunk_seq,
            &mut chunk_paths[start + local_idx],
        )?;
        sorting += t.elapsed();
        chunk_seq += 1;
    }

    Ok(ReducerThreadOutput {
        partition_chunks: chunk_paths,
        sorting,
    })
}

#[allow(clippy::too_many_arguments)]
fn spill_u64_counted_pending(
    pending: &mut FxHashMap<u64, u64>,
    counted_buffers: &mut [Vec<(u64, u64)>],
    partitions: usize,
    partitions_pow2: bool,
    partition_shift: usize,
    start: usize,
    end: usize,
    temp_dir: &Path,
    chunk_seq: &mut usize,
    chunk_paths: &mut [Vec<PathBuf>],
) -> Result<usize> {
    let flushed = pending.len();
    for (key, count) in pending.drain() {
        let part = partition_u64(key, partitions, partitions_pow2, partition_shift);
        if part < start || part >= end {
            bail!("partition ownership violation");
        }
        counted_buffers[part - start].push((key, count));
    }

    let retain = U64_OWNER_BATCH_KMERS / 2;
    for (local_idx, buf) in counted_buffers.iter_mut().enumerate() {
        if buf.is_empty() {
            continue;
        }
        buf.sort_unstable_by_key(|entry| entry.0);
        let part = start + local_idx;
        let path = temp_dir.join(format!("part-{part:05}-chunk-{:08}.u64c", *chunk_seq));
        let file = File::create(&path)
            .with_context(|| format!("failed to create spilled chunk {}", path.display()))?;
        let mut bw = BufWriter::with_capacity(1 << 20, file);
        for &(key, count) in buf.iter() {
            bw.write_all(&key.to_le_bytes())?;
            bw.write_all(&count.to_le_bytes())?;
        }
        bw.flush()?;
        chunk_paths[part].push(path);
        buf.clear();
        if buf.capacity() > retain {
            buf.shrink_to(retain);
        }
        *chunk_seq += 1;
    }

    Ok(flushed)
}

fn spill_u64_raw_partition(
    raw: &mut Vec<u64>,
    scratch: &mut Vec<u64>,
    temp_dir: &Path,
    part: usize,
    chunk_seq: usize,
    out_paths: &mut Vec<PathBuf>,
) -> Result<()> {
    if raw.is_empty() {
        return Ok(());
    }
    radix_sort_u64(raw, scratch);
    let path = temp_dir.join(format!("part-{part:05}-chunk-{chunk_seq:08}.u64c"));
    let file = File::create(&path)
        .with_context(|| format!("failed to create spilled chunk {}", path.display()))?;
    let mut bw = BufWriter::with_capacity(1 << 20, file);
    let mut entry_count = 0u64;
    let mut i = 0usize;
    while i < raw.len() {
        entry_count += 1;
        let key = raw[i];
        let mut count = 1u64;
        i += 1;
        while i < raw.len() && raw[i] == key {
            count += 1;
            i += 1;
        }
        bw.write_all(&key.to_le_bytes())?;
        bw.write_all(&count.to_le_bytes())?;
    }
    bw.flush()?;
    raw.clear();
    let retain = U64_OWNER_BATCH_KMERS.saturating_mul(2);
    if raw.capacity() > retain {
        raw.shrink_to(retain);
    }
    out_paths.push(path);
    let _ = entry_count;
    Ok(())
}

/// 中文：把输入序列按当前最合适的读取策略切成 `SeqBatch` 并分发给 workers。
/// `.gz` 输入走“块解压 + 四状态机解析”的高吞吐路径，其他输入继续走通用 reader。
/// English: Split input sequences into `SeqBatch` objects and dispatch them to
/// workers using the best available read strategy. `.gz` inputs take the
/// high-throughput path with block decompression plus a four-state FASTQ
/// parser; other inputs keep using the generic reader.
fn dispatch_batches(
    path: &Path,
    cfg: &CountConfig,
    senders: &[Sender<WorkItem>],
    gzip_kind: Option<GzipKind>,
    pipeline: PipelineThreads,
    tuning: IngestTuning,
) -> Result<()> {
    match gzip_kind {
        Some(GzipKind::Bgzf) => dispatch_bgzf_fastq_batches(path, cfg, senders, pipeline, tuning),
        Some(GzipKind::PlainGzip) => {
            dispatch_gzip_fastq_batches(path, cfg, senders, pipeline, tuning)
        }
        None => dispatch_generic_batches(path, senders),
    }
}

fn dispatch_generic_batches(path: &Path, senders: &[Sender<WorkItem>]) -> Result<()> {
    let mut reader = open_seq_reader(path)?;
    let mut batch = SeqBatch::with_capacity(PRODUCER_BATCH_READS, PRODUCER_BATCH_BYTES);
    let mut rr = 0usize;

    while let Some((start, len)) = reader.next_sequence_range(&mut batch.data)? {
        batch.offsets.push((start, len));
        if batch.len() >= PRODUCER_BATCH_READS || batch.data.len() >= PRODUCER_BATCH_BYTES {
            let tx: &Sender<WorkItem> = &senders[rr % senders.len()];
            let mut out = SeqBatch::with_capacity(batch.offsets.capacity(), batch.data.capacity());
            std::mem::swap(&mut out, &mut batch);
            tx.send(WorkItem::SeqBatch(out))
                .context("failed to send batch to worker")?;
            batch.clear();
            rr += 1;
        }
    }
    if !batch.is_empty() {
        let tx: &Sender<WorkItem> = &senders[rr % senders.len()];
        tx.send(WorkItem::SeqBatch(batch))
            .context("failed to send final batch")?;
    }
    Ok(())
}

fn dispatch_gzip_fastq_batches(
    path: &Path,
    cfg: &CountConfig,
    senders: &[Sender<WorkItem>],
    _pipeline: PipelineThreads,
    tuning: IngestTuning,
) -> Result<()> {
    let block_queue_cap = tuning.block_queue_cap;
    let block_bytes = tuning.decomp_block_bytes;
    let work_chunk_bytes = tuning.work_chunk_bytes;
    let (block_rx, recycle_tx, decompress_handle) = spawn_gzip_block_reader(
        path,
        block_bytes,
        block_queue_cap,
        tuning.prealloc_block_buffers,
        cfg.perf_counters.clone(),
    )?;
    let mut rr = 0usize;
    let mut aligner = FastqChunkAligner::new(work_chunk_bytes);
    let dispatch_result = (|| -> Result<()> {
        let mut emit_chunk = |chunk: Vec<u8>| -> Result<()> {
            let tx: &Sender<WorkItem> = &senders[rr % senders.len()];
            let bytes = chunk.len();
            let t_dispatch = Instant::now();
            tx.send(WorkItem::FastqChunk(chunk))
                .context("failed to send aligned FASTQ chunk to worker")?;
            if let Some(perf) = &cfg.perf_counters {
                perf.add_dispatch(t_dispatch.elapsed());
                perf.observe_dispatch(bytes, tx.len());
            }
            rr += 1;
            Ok(())
        };

        while let Ok(msg) = block_rx.recv() {
            let mut block = msg?;
            let t_dispatch = Instant::now();
            aligner.push_block(&block, &mut emit_chunk)?;
            if let Some(perf) = &cfg.perf_counters {
                perf.add_dispatch(t_dispatch.elapsed());
            }
            block.clear();
            let _ = recycle_tx.send(block);
        }
        aligner.finish(&mut emit_chunk)
    })();
    let decompress_join = decompress_handle
        .join()
        .map_err(|_| anyhow::anyhow!("gzip reader thread panicked"));

    dispatch_result?;
    decompress_join?;
    Ok(())
}

fn dispatch_bgzf_fastq_batches(
    path: &Path,
    cfg: &CountConfig,
    senders: &[Sender<WorkItem>],
    pipeline: PipelineThreads,
    tuning: IngestTuning,
) -> Result<()> {
    let block_queue_cap = tuning.block_queue_cap;
    let bgzf_threads = pipeline.bgzf_decompressors.max(1);
    let block_bytes = tuning.decomp_block_bytes;
    let work_chunk_bytes = tuning.work_chunk_bytes;
    let (block_rx, recycle_tx, decompress_handle) = spawn_bgzf_block_reader(
        path,
        bgzf_threads,
        block_bytes,
        block_queue_cap,
        tuning.prealloc_block_buffers,
        cfg.perf_counters.clone(),
    )?;
    let dispatch_result = (|| -> Result<()> {
        let mut rr = 0usize;
        let mut aligner = FastqChunkAligner::new(work_chunk_bytes);
        let mut emit_chunk = |chunk: Vec<u8>| -> Result<()> {
            let tx: &Sender<WorkItem> = &senders[rr % senders.len()];
            let bytes = chunk.len();
            let t_dispatch = Instant::now();
            tx.send(WorkItem::FastqChunk(chunk))
                .context("failed to send aligned BGZF FASTQ chunk to worker")?;
            if let Some(perf) = &cfg.perf_counters {
                perf.add_dispatch(t_dispatch.elapsed());
                perf.observe_dispatch(bytes, tx.len());
            }
            rr += 1;
            Ok(())
        };

        while let Ok(msg) = block_rx.recv() {
            let mut block = msg?;
            let t_dispatch = Instant::now();
            aligner.push_block(&block, &mut emit_chunk)?;
            if let Some(perf) = &cfg.perf_counters {
                perf.add_dispatch(t_dispatch.elapsed());
            }
            block.clear();
            let _ = recycle_tx.send(block);
        }
        aligner.finish(&mut emit_chunk)
    })();

    let decompress_join = decompress_handle
        .join()
        .map_err(|_| anyhow::anyhow!("bgzf reader thread panicked"));

    dispatch_result?;
    decompress_join?;
    Ok(())
}

const PAR_SORT_THRESHOLD: usize = 1 << 14; // only used by packed long-k paths

#[inline(always)]
fn use_u128_hash_path(cfg: &CountConfig) -> bool {
    let effective_memory =
        cfg.max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(cfg.threads));
    let per_thread_budget = effective_memory / cfg.threads.max(1) as u64;
    cfg.partitions <= 256 && cfg.chunk_kmers <= 2_000_000 && per_thread_budget >= (256u64 << 20)
}

/// 中文：单个 worker 的执行循环。输入是一串序列批次，输出是该 worker 的局部 chunk。
/// English: Worker execution loop. It consumes sequence batches and produces
/// local sorted/count-compressed chunks for one worker.
fn worker_loop(rx: Receiver<WorkItem>, cfg: &CountConfig, mode: KmerMode) -> Result<WorkerOutput> {
    match mode {
        KmerMode::U64 if cfg.k <= U64_HASH_K_MAX => worker_loop_u64_hash(rx, cfg),
        KmerMode::U64 => worker_loop_u64(rx, cfg),
        KmerMode::U128 if use_u128_hash_path(cfg) => worker_loop_u128_hash(rx, cfg),
        KmerMode::U128 => worker_loop_u128(rx, cfg),
        _ => worker_loop_generic(rx, cfg, mode),
    }
}

fn worker_loop_u64_hash(rx: Receiver<WorkItem>, cfg: &CountConfig) -> Result<WorkerOutput> {
    let mut out = WorkerOutput {
        chunks_u64: vec![Vec::new(); cfg.partitions],
        chunks_u128: Vec::new(),
        chunks_packed: Vec::new(),
        total_kmers: 0,
        total_reads: 0,
        timings: WorkerTimings::default(),
    };

    let progress_ref = cfg.progress_counters.clone();
    let progress_enabled = progress_ref.is_some();
    let mut local_progress = LocalProgress::default();
    let mut counts = FxHashMap::<u64, u32>::with_capacity_and_hasher(
        u64_hash_map_capacity(cfg),
        Default::default(),
    );
    let mask64 = if cfg.k == 32 {
        u64::MAX
    } else {
        (1u64 << (2 * cfg.k)) - 1
    };
    let rev_shift = (cfg.k - 1) * 2;

    while let Ok(item) = rx.recv() {
        let t_batch_k = Instant::now();
        match item {
            WorkItem::SeqBatch(batch) => {
                for &(start, len) in &batch.offsets {
                    let s = start as usize;
                    let e = s + len as usize;
                    process_sequence_u64_hash(
                        &batch.data[s..e],
                        cfg,
                        mask64,
                        rev_shift,
                        progress_enabled,
                        &mut local_progress,
                        &mut counts,
                        &mut out,
                    );
                }
            }
            WorkItem::FastqChunk(chunk) => {
                process_fastq_chunk_u64_hash_direct(
                    &chunk,
                    cfg,
                    mask64,
                    rev_shift,
                    progress_enabled,
                    &mut local_progress,
                    &mut counts,
                    &mut out,
                )?;
            }
        }

        if progress_enabled {
            local_progress.flush(&progress_ref);
        }
        out.timings.kmer_generation += t_batch_k.elapsed();
    }

    if progress_enabled {
        local_progress.flush(&progress_ref);
    }

    flush_u64_hash_counts(&mut counts, cfg.partitions, &mut out);
    if let Some(perf) = &cfg.perf_counters {
        perf.inc_u64_hash_flush();
    }

    Ok(out)
}

fn worker_loop_u128_hash(rx: Receiver<WorkItem>, cfg: &CountConfig) -> Result<WorkerOutput> {
    let mut out = WorkerOutput {
        chunks_u64: Vec::new(),
        chunks_u128: vec![Vec::new(); cfg.partitions],
        chunks_packed: Vec::new(),
        total_kmers: 0,
        total_reads: 0,
        timings: WorkerTimings::default(),
    };

    let progress_ref = cfg.progress_counters.clone();
    let progress_enabled = progress_ref.is_some();
    let mut local_progress = LocalProgress::default();
    let mut counts = FxHashMap::<u128, u32>::with_capacity_and_hasher(
        u128_hash_map_capacity(cfg),
        Default::default(),
    );
    let mask128 = if cfg.k == 64 {
        u128::MAX
    } else {
        (1u128 << (2 * cfg.k)) - 1
    };
    let rev_shift = (cfg.k - 1) * 2;

    while let Ok(item) = rx.recv() {
        let t_batch_k = Instant::now();
        match item {
            WorkItem::SeqBatch(batch) => {
                for &(start, len) in &batch.offsets {
                    let s = start as usize;
                    let e = s + len as usize;
                    process_sequence_u128_hash(
                        &batch.data[s..e],
                        cfg,
                        mask128,
                        rev_shift,
                        progress_enabled,
                        &mut local_progress,
                        &mut counts,
                        &mut out,
                    );
                }
            }
            WorkItem::FastqChunk(chunk) => {
                process_fastq_chunk_u128_hash_direct(
                    &chunk,
                    cfg,
                    mask128,
                    rev_shift,
                    progress_enabled,
                    &mut local_progress,
                    &mut counts,
                    &mut out,
                )?;
            }
        }

        if progress_enabled {
            local_progress.flush(&progress_ref);
        }
        out.timings.kmer_generation += t_batch_k.elapsed();
    }

    if progress_enabled {
        local_progress.flush(&progress_ref);
    }

    flush_u128_hash_counts(&mut counts, cfg.partitions, &mut out);

    Ok(out)
}

fn worker_loop_u64(rx: Receiver<WorkItem>, cfg: &CountConfig) -> Result<WorkerOutput> {
    let mut out = WorkerOutput {
        chunks_u64: vec![Vec::new(); cfg.partitions],
        chunks_u128: Vec::new(),
        chunks_packed: Vec::new(),
        total_kmers: 0,
        total_reads: 0,
        timings: WorkerTimings::default(),
    };

    let progress_ref = cfg.progress_counters.clone();
    let progress_enabled = progress_ref.is_some();
    let mut local_progress = LocalProgress::default();
    let mut buffers_u64 =
        vec![Vec::<u64>::with_capacity(cfg.chunk_kmers / cfg.partitions + 8); cfg.partitions];
    let mut scratch_u64 = Vec::<u64>::with_capacity(cfg.chunk_kmers);

    let partitions_pow2 = cfg.partitions.is_power_of_two();
    let partition_shift = if partitions_pow2 {
        64usize - (cfg.partitions.trailing_zeros() as usize)
    } else {
        0usize
    };
    let mask64 = if cfg.k == 32 {
        u64::MAX
    } else {
        (1u64 << (2 * cfg.k)) - 1
    };
    let rev_shift = (cfg.k - 1) * 2;

    while let Ok(item) = rx.recv() {
        let t_batch_k = Instant::now();
        match item {
            WorkItem::SeqBatch(batch) => {
                for &(start, len) in &batch.offsets {
                    let s = start as usize;
                    let e = s + len as usize;
                    process_sequence_u64(
                        &batch.data[s..e],
                        cfg,
                        partitions_pow2,
                        partition_shift,
                        mask64,
                        rev_shift,
                        progress_enabled,
                        &mut local_progress,
                        &mut buffers_u64,
                        &mut scratch_u64,
                        &mut out,
                    );
                }
            }
            WorkItem::FastqChunk(chunk) => {
                process_fastq_chunk_u64_direct(
                    &chunk,
                    cfg,
                    partitions_pow2,
                    partition_shift,
                    mask64,
                    rev_shift,
                    progress_enabled,
                    &mut local_progress,
                    &mut buffers_u64,
                    &mut scratch_u64,
                    &mut out,
                )?;
            }
        }

        if progress_enabled {
            local_progress.flush(&progress_ref);
        }
        out.timings.kmer_generation += t_batch_k.elapsed();
    }

    if progress_enabled {
        local_progress.flush(&progress_ref);
    }

    for p in 0..cfg.partitions {
        flush_partition_u64(p, &mut buffers_u64, &mut scratch_u64, &mut out);
    }

    Ok(out)
}

fn worker_loop_u128(rx: Receiver<WorkItem>, cfg: &CountConfig) -> Result<WorkerOutput> {
    let mut out = WorkerOutput {
        chunks_u64: Vec::new(),
        chunks_u128: vec![Vec::new(); cfg.partitions],
        chunks_packed: Vec::new(),
        total_kmers: 0,
        total_reads: 0,
        timings: WorkerTimings::default(),
    };

    let progress_ref = cfg.progress_counters.clone();
    let progress_enabled = progress_ref.is_some();
    let mut local_progress = LocalProgress::default();
    let mut buffers_u128 =
        vec![Vec::<u128>::with_capacity(cfg.chunk_kmers / cfg.partitions + 8); cfg.partitions];
    let mut scratch_u128 = Vec::<u128>::with_capacity(cfg.chunk_kmers);

    let partitions_pow2 = cfg.partitions.is_power_of_two();
    let partition_shift = if partitions_pow2 {
        64usize - (cfg.partitions.trailing_zeros() as usize)
    } else {
        0usize
    };
    let mask128 = if cfg.k == 64 {
        u128::MAX
    } else {
        (1u128 << (2 * cfg.k)) - 1
    };
    let rev_shift = (cfg.k - 1) * 2;

    while let Ok(item) = rx.recv() {
        let t_batch_k = Instant::now();
        match item {
            WorkItem::SeqBatch(batch) => {
                for &(start, len) in &batch.offsets {
                    let s = start as usize;
                    let e = s + len as usize;
                    process_sequence_u128(
                        &batch.data[s..e],
                        cfg,
                        partitions_pow2,
                        partition_shift,
                        mask128,
                        rev_shift,
                        progress_enabled,
                        &mut local_progress,
                        &mut buffers_u128,
                        &mut scratch_u128,
                        &mut out,
                    );
                }
            }
            WorkItem::FastqChunk(chunk) => {
                process_fastq_chunk_u128_direct(
                    &chunk,
                    cfg,
                    partitions_pow2,
                    partition_shift,
                    mask128,
                    rev_shift,
                    progress_enabled,
                    &mut local_progress,
                    &mut buffers_u128,
                    &mut scratch_u128,
                    &mut out,
                )?;
            }
        }

        if progress_enabled {
            local_progress.flush(&progress_ref);
        }
        out.timings.kmer_generation += t_batch_k.elapsed();
    }

    if progress_enabled {
        local_progress.flush(&progress_ref);
    }

    for p in 0..cfg.partitions {
        flush_partition_u128(p, &mut buffers_u128, &mut scratch_u128, &mut out);
    }

    Ok(out)
}

fn worker_loop_generic(
    rx: Receiver<WorkItem>,
    cfg: &CountConfig,
    mode: KmerMode,
) -> Result<WorkerOutput> {
    let mut out = WorkerOutput {
        chunks_u64: vec![Vec::new(); cfg.partitions],
        chunks_u128: vec![Vec::new(); cfg.partitions],
        chunks_packed: vec![Vec::new(); cfg.partitions],
        total_kmers: 0,
        total_reads: 0,
        timings: WorkerTimings::default(),
    };

    let mut roll = RollingKmer::new(cfg.k, cfg.canonical);

    // atomic counters reference for this worker (clone of Arc)
    let progress_ref = cfg.progress_counters.clone();
    let progress_enabled = progress_ref.is_some();
    let mut local_progress = LocalProgress::default();
    let per_partition_cap = cfg.chunk_kmers / cfg.partitions + 8;
    let mut buffers_u64 = if matches!(mode, KmerMode::U64) {
        vec![Vec::<u64>::with_capacity(per_partition_cap); cfg.partitions]
    } else {
        vec![Vec::<u64>::new(); cfg.partitions]
    };
    let mut buffers_u128 = if matches!(mode, KmerMode::U128) {
        vec![Vec::<u128>::with_capacity(per_partition_cap); cfg.partitions]
    } else {
        vec![Vec::<u128>::new(); cfg.partitions]
    };
    let mut buffers_packed = if matches!(mode, KmerMode::Packed(_)) {
        vec![
            std::collections::VecDeque::<Vec<u64>>::with_capacity(per_partition_cap);
            cfg.partitions
        ]
    } else {
        vec![std::collections::VecDeque::<Vec<u64>>::new(); cfg.partitions]
    };

    // per-partition totals used to decide when to flush
    let mut buffers_u64_total = if matches!(mode, KmerMode::U64) {
        vec![0usize; cfg.partitions]
    } else {
        Vec::new()
    };
    let mut buffers_u128_total = if matches!(mode, KmerMode::U128) {
        vec![0usize; cfg.partitions]
    } else {
        Vec::new()
    };
    let mut buffers_packed_total = if matches!(mode, KmerMode::Packed(_)) {
        vec![0usize; cfg.partitions]
    } else {
        Vec::new()
    };
    let mut scratch_u64 = if matches!(mode, KmerMode::U64) {
        Vec::<u64>::with_capacity(cfg.chunk_kmers)
    } else {
        Vec::<u64>::new()
    };
    let mut scratch_u128 = if matches!(mode, KmerMode::U128) {
        Vec::<u128>::with_capacity(cfg.chunk_kmers)
    } else {
        Vec::<u128>::new()
    };

    let partitions_pow2 = cfg.partitions.is_power_of_two();
    let partition_shift = if partitions_pow2 {
        64usize - (cfg.partitions.trailing_zeros() as usize)
    } else {
        0usize
    };

    while let Ok(item) = rx.recv() {
        let t_batch_k = Instant::now();
        match item {
            WorkItem::SeqBatch(batch) => {
                for &(start, len) in &batch.offsets {
                    let s = start as usize;
                    let e = s + len as usize;
                    process_sequence(
                        &batch.data[s..e],
                        cfg,
                        mode,
                        partitions_pow2,
                        partition_shift,
                        &mut roll,
                        progress_enabled,
                        &mut local_progress,
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
            }
            WorkItem::FastqChunk(chunk) => {
                process_fastq_chunk_direct(&chunk, |seq| {
                    process_sequence(
                        seq,
                        cfg,
                        mode,
                        partitions_pow2,
                        partition_shift,
                        &mut roll,
                        progress_enabled,
                        &mut local_progress,
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
                    Ok(())
                })?;
            }
        }
        // Flush once per received batch rather than once per read/k-mer.
        if progress_enabled {
            local_progress.flush(&progress_ref);
        }
        out.timings.kmer_generation += t_batch_k.elapsed();
    }

    if progress_enabled {
        local_progress.flush(&progress_ref);
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
#[inline(always)]
fn partition_mix_u64(v: u64) -> u64 {
    v.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[inline(always)]
/// 中文：为 `u64` 键选择 partition。这里先做一次乘法混合，避免小 k 时高位全零
/// 导致所有数据挤进同一个 partition。
/// English: Choose the target partition for a `u64` key. We first apply a fast
/// multiplicative mix so short k-mers do not collapse into a single bucket just
/// because their high bits are zero.
fn partition_u64(
    v: u64,
    partitions: usize,
    partitions_pow2: bool,
    partition_shift: usize,
) -> usize {
    if partitions <= 1 {
        return 0;
    }
    let mixed = partition_mix_u64(v);
    if partitions_pow2 {
        // partitions = 2^p -> take high p bits after mixing.
        return (mixed >> partition_shift) as usize;
    }
    (((mixed as u128) * (partitions as u128)) >> 64) as usize
}

#[inline(always)]
/// 中文：为 `u128` 键选择 partition。这里把高低 64 位折叠后再混合，避免高位稀疏时
/// 分桶严重倾斜。
/// English: Choose the target partition for a `u128` key. We fold high/low
/// halves before partitioning so sparse upper bits do not skew the buckets.
fn partition_u128(
    v: u128,
    partitions: usize,
    partitions_pow2: bool,
    partition_shift: usize,
) -> usize {
    partition_u64((v as u64) ^ ((v >> 64) as u64), partitions, partitions_pow2, partition_shift)
}

#[inline(always)]
/// 中文：为 packed 长 k-mer 选择 partition。这里折叠所有 word，尽量利用整条键的熵。
/// English: Choose the target partition for a packed long k-mer by folding all
/// words, which gives the partitioner access to entropy from the full key.
fn partition_packed(
    words: &[u64],
    partitions: usize,
    partitions_pow2: bool,
    partition_shift: usize,
) -> usize {
    let folded = words.iter().copied().fold(0u64, |acc, word| {
        partition_mix_u64(acc ^ word.rotate_left(13))
    });
    partition_u64(folded, partitions, partitions_pow2, partition_shift)
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
            flush_partition_u64(part, buffers_u64, scratch_u64, out);
            buffers_u64_total[part] = 0;
        }
        KmerMode::U128 => {
            flush_partition_u128(part, buffers_u128, scratch_u128, out);
            buffers_u128_total[part] = 0;
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
                let key = std::mem::take(&mut vecs[i]);
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

fn flush_partition_u64(
    part: usize,
    buffers_u64: &mut [Vec<u64>],
    scratch_u64: &mut Vec<u64>,
    out: &mut WorkerOutput,
) {
    let src = &mut buffers_u64[part];
    if src.is_empty() {
        return;
    }
    let t = Instant::now();
    radix_sort_u64(src, scratch_u64);
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
    out.chunks_u64[part].push(ChunkU64 { kmers, counts });
    out.timings.sorting += t.elapsed();
}

fn flush_partition_u128(
    part: usize,
    buffers_u128: &mut [Vec<u128>],
    scratch_u128: &mut Vec<u128>,
    out: &mut WorkerOutput,
) {
    let src = &mut buffers_u128[part];
    if src.is_empty() {
        return;
    }
    let t = Instant::now();
    if src.len() >= (1 << 16) {
        radix_sort_u128(src, scratch_u128);
    } else {
        src.sort_unstable();
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
    out.chunks_u128[part].push(ChunkU128 { kmers, counts });
    out.timings.sorting += t.elapsed();
}

fn flush_u64_hash_counts(
    counts: &mut FxHashMap<u64, u32>,
    partitions: usize,
    out: &mut WorkerOutput,
) {
    if counts.is_empty() {
        return;
    }
    let t = Instant::now();
    let final_parts = partitions.max(1);
    let final_parts_pow2 = final_parts.is_power_of_two();
    let final_part_shift = if final_parts_pow2 {
        64usize - (final_parts.trailing_zeros() as usize)
    } else {
        0usize
    };
    let mut entries: Vec<(u64, u32)> = counts.drain().collect();
    if entries.len() >= (1 << 15) {
        entries.par_sort_unstable_by_key(|&(k, _)| k);
    } else {
        entries.sort_unstable_by_key(|&(k, _)| k);
    }

    let mut part_sizes = vec![0usize; final_parts];
    for &(kmer, _) in &entries {
        let part = partition_u64(kmer, final_parts, final_parts_pow2, final_part_shift);
        part_sizes[part] += 1;
    }

    let mut part_kmers: Vec<Vec<u64>> = part_sizes.iter().map(|&n| Vec::with_capacity(n)).collect();
    let mut part_counts: Vec<Vec<u32>> = part_sizes.iter().map(|&n| Vec::with_capacity(n)).collect();
    for (kmer, count) in entries {
        let part = partition_u64(kmer, final_parts, final_parts_pow2, final_part_shift);
        part_kmers[part].push(kmer);
        part_counts[part].push(count);
    }

    for part in 0..final_parts {
        if part_kmers[part].is_empty() {
            continue;
        }
        out.chunks_u64[part].push(ChunkU64 {
            kmers: std::mem::take(&mut part_kmers[part]),
            counts: std::mem::take(&mut part_counts[part]),
        });
    }
    out.timings.sorting += t.elapsed();
}

fn flush_u128_hash_counts(
    counts: &mut FxHashMap<u128, u32>,
    partitions: usize,
    out: &mut WorkerOutput,
) {
    if counts.is_empty() {
        return;
    }
    let t = Instant::now();
    let final_parts = partitions.max(1);
    let final_parts_pow2 = final_parts.is_power_of_two();
    let final_part_shift = if final_parts_pow2 {
        64usize - (final_parts.trailing_zeros() as usize)
    } else {
        0usize
    };
    let mut entries: Vec<(u128, u32)> = counts.drain().collect();
    if entries.len() >= (1 << 15) {
        entries.par_sort_unstable_by_key(|&(k, _)| k);
    } else {
        entries.sort_unstable_by_key(|&(k, _)| k);
    }

    let mut part_sizes = vec![0usize; final_parts];
    for &(kmer, _) in &entries {
        let part = partition_u128(kmer, final_parts, final_parts_pow2, final_part_shift);
        part_sizes[part] += 1;
    }

    let mut part_kmers: Vec<Vec<u128>> =
        part_sizes.iter().map(|&n| Vec::with_capacity(n)).collect();
    let mut part_counts: Vec<Vec<u32>> =
        part_sizes.iter().map(|&n| Vec::with_capacity(n)).collect();
    for (kmer, count) in entries {
        let part = partition_u128(kmer, final_parts, final_parts_pow2, final_part_shift);
        part_kmers[part].push(kmer);
        part_counts[part].push(count);
    }

    for part in 0..final_parts {
        if part_kmers[part].is_empty() {
            continue;
        }
        out.chunks_u128[part].push(ChunkU128 {
            kmers: std::mem::take(&mut part_kmers[part]),
            counts: std::mem::take(&mut part_counts[part]),
        });
    }
    out.timings.sorting += t.elapsed();
}

#[inline(always)]
fn u64_hash_map_capacity(cfg: &CountConfig) -> usize {
    let effective_memory =
        cfg.max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(cfg.threads));
    let per_thread_budget = effective_memory / cfg.threads.max(1) as u64;
    let by_budget = (per_thread_budget / 56) as usize;
    let by_chunk = cfg.chunk_kmers.saturating_mul(2);
    by_budget.min(by_chunk).clamp(1 << 18, 8 << 20)
}

#[inline(always)]
fn u128_hash_map_capacity(cfg: &CountConfig) -> usize {
    let effective_memory =
        cfg.max_memory_bytes.unwrap_or_else(|| aggressive_auto_memory_budget(cfg.threads));
    let per_thread_budget = effective_memory / cfg.threads.max(1) as u64;
    let by_budget = (per_thread_budget / 64) as usize;
    let by_chunk = cfg.chunk_kmers.saturating_mul(2);
    by_budget.min(by_chunk).clamp(1 << 17, 2 << 20)
}

/// 中文：基线版计数流程。它不做 optimized path 的工程优化，主要用来对照和回归测试。
/// English: Baseline counting pipeline. It skips the optimized path engineering fast path and is
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
                    partition_routing: Duration::ZERO,
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
                    partition_routing: Duration::ZERO,
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
                let key = std::mem::take(&mut all[i]);
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
                    partition_routing: Duration::ZERO,
                    sorting: t_sort.elapsed(),
                    merging: Duration::ZERO,
                    total,
                },
            })
        }
    }
}
