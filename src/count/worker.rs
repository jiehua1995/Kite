use std::io;
use std::sync::Arc;
use std::time::Instant;
use crate::count::kmer::KmerWord;
use crate::count::parser::parse_sequences;
use crate::count::sorter::rle_compress_typed;
use crate::count::memory::SharedBudget;
use crate::count::writer::ConcurrentKiteWriter;
use crate::stats::{SharedStats, LocalCountStats};
use crate::format::{CODEC_DVLE, CODEC_RAW};

/// Size of each per-bucket staging slot (entries).  With K=u64 (8 B) and
/// 256 buckets: 256 × 64 × 8 = 131 072 B ≈ 128 KB — fits in L2 cache.
/// Cuts per-kmer DRAM latency ~8× by batching scatter writes.
const WCOM_BATCH: usize = 64;

/// A count worker: receives decompressed FASTQ chunks, generates k-mers,
/// accumulates them in per-partition local buffers, flushes directly to the
/// shared final .kite writer as sorted segments.
///
/// # Hot-path optimisations
/// 1. No virtual dispatch for bucket routing — `bucket_shift` is stored as u32.
/// 2. No per-kmer atomic increment — all stats live in `local_stats`.
/// 3. Running `local_bytes` counter avoids O(partitions) scan each chunk.
/// 4. `entries_buf` is reused across flush calls to avoid per-flush mmap.
/// 5. Direct bucket-file write: workers lock bucket_files[b], write RunHeader+payload,
///    unlock.  Lock contention is low (different workers hit different buckets).
pub struct CountWorker<K: KmerWord> {
    collect_timing: bool,
    k: u8,
    canonical: bool,
    partitions: usize,
    bucket_shift: u32,
    local_buckets: Vec<Vec<K>>,
    local_bytes: usize,
    local_flush_threshold: usize,
    budget: SharedBudget,
    stats: SharedStats,
    local_stats: LocalCountStats,
    kite_writer: Arc<ConcurrentKiteWriter>,
    byte_buf: Vec<u8>,
    entries_buf: Vec<(K, u32)>,
    
    /// Write-combining staging buffer.  Flat array: partitions × WCOM_BATCH entries.
    /// Kept in L2 cache; flushed to local_buckets in 64-entry batches.
    wc_staging: Box<[K]>,
    wc_lens: Box<[u8]>,  // fill count per staging slot (0..=WCOM_BATCH)
}

impl<K: KmerWord> CountWorker<K> {
    pub fn new(
        collect_timing: bool,
        k: u8,
        canonical: bool,
        partitions: usize,
        bucket_shift: u32,
        budget: SharedBudget,
        stats: SharedStats,
        kite_writer: Arc<ConcurrentKiteWriter>,
        worker_budget_bytes: usize,
    ) -> Self {
        let cap_per_bucket = (worker_budget_bytes / partitions / K::byte_size()).max(4096);

        CountWorker {
            collect_timing,
            k,
            canonical,
            partitions,
            bucket_shift,
            local_buckets: (0..partitions)
                .map(|_| Vec::with_capacity(cap_per_bucket))
                .collect(),
            local_bytes: 0,
            local_flush_threshold: worker_budget_bytes,
            budget,
            stats,
            local_stats: LocalCountStats::default(),
            kite_writer,
            byte_buf: Vec::with_capacity(32 * 1024 * 1024),
            entries_buf: Vec::new(),
            wc_staging: {
                let n = partitions * WCOM_BATCH;
                let mut v: Vec<K> = Vec::with_capacity(n);
                unsafe {
                    std::ptr::write_bytes(v.as_mut_ptr() as *mut u8, 0, n * std::mem::size_of::<K>());
                    v.set_len(n);
                }
                v.into_boxed_slice()
            },
            wc_lens: vec![0u8; partitions].into_boxed_slice(),
        }
    }

    /// Process one decompressed FASTQ chunk.
    #[inline]
    pub fn process_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        // Back-pressure: if the global budget is over limit, the workers themselves
        // must flush their buckets to free memory since there is no separate writer thread.
        while self.budget.is_over_limit() {
            if self.local_bytes > 0 {
                self.flush_largest_bucket()?;
            } else if !self.kite_writer.drain_one_partition()? {
                std::thread::yield_now();
                break;
            }
        }

        let k = self.k;
        let canonical = self.canonical;
        let bucket_shift = self.bucket_shift;
        let local_buckets = &mut self.local_buckets;
        let wc_staging = &mut self.wc_staging;
        let wc_lens = &mut self.wc_lens;
        // Incremental byte tracking (avoids per-kmer atomic + O(P) sum)
        let mut added_bytes = 0usize;
        let mut local_reads = 0u64;
        let mut local_bases = 0u64;
        let mut local_kmers = 0u64;

        let collect_timing = self.collect_timing;
        let t_start = collect_timing.then(Instant::now);
        let mut kmer_push_ns = 0u64;
        let _ = parse_sequences(chunk, |seq| {
            local_reads += 1;
            local_bases += seq.len() as u64;

            // ── HOT PATH ───────────────────────────────────────────────────
            // No virtual dispatch: bucket = (kmer >> shift) as usize
            // No shared atomic: stats accumulated in local_stats (plain u64)
            let t_kmer = collect_timing.then(Instant::now);
            let mut seq_kmers = 0u64;
            K::scan_sequence(seq, k, canonical, |kmer| {
                seq_kmers += 1;
                let bucket = kmer.to_bucket(bucket_shift);
                // Write-combining: stage to L2-cache-hot buffer (256×64 entries).
                // Amortises DRAM scatter latency: 1 cache-miss per 8 pushes
                // (64 × 8B = 512B = 8 cache lines, vs 1 miss per push without).
                let wlen = unsafe { *wc_lens.get_unchecked(bucket) } as usize;
                unsafe { *wc_staging.get_unchecked_mut(bucket * WCOM_BATCH + wlen) = kmer; }
                let new_wlen = wlen + 1;
                if new_wlen == WCOM_BATCH {
                    let start = bucket * WCOM_BATCH;
                    unsafe {
                        local_buckets.get_unchecked_mut(bucket)
                            .extend_from_slice(wc_staging.get_unchecked(start..start + WCOM_BATCH));
                        *wc_lens.get_unchecked_mut(bucket) = 0;
                    }
                } else {
                    unsafe { *wc_lens.get_unchecked_mut(bucket) = new_wlen as u8; }
                }
            });
            if let Some(t_kmer) = t_kmer {
                kmer_push_ns += t_kmer.elapsed().as_nanos() as u64;
            }
            // ───────────────────────────────────────────────────────────────

            local_kmers += seq_kmers;
            added_bytes += seq_kmers as usize * K::byte_size();
        });

        // All accumulation into thread-local fields — zero shared-atomic traffic
        let parse_ns = t_start.map(|t| t.elapsed().as_nanos() as u64).unwrap_or(0);
        self.local_stats.reads        += local_reads;
        self.local_stats.bases        += local_bases;
        self.local_stats.valid_kmers  += local_kmers;
        self.local_stats.decomp_bytes += chunk.len() as u64;
        if collect_timing {
            self.local_stats.parse_ns += parse_ns;
            self.local_stats.kmer_ns += kmer_push_ns;
            self.local_stats.read_ns += parse_ns.saturating_sub(kmer_push_ns);
        }

        self.local_bytes += added_bytes;
        self.budget.alloc(added_bytes);

        if self.local_bytes >= self.local_flush_threshold || self.budget.should_flush() {
            self.flush_largest_bucket()?;
            if self.kite_writer.needs_runtime_drain() {
                let _ = self.kite_writer.drain_excess_staging()?;
            }
        }
        Ok(())
    }

    fn flush_largest_bucket(&mut self) -> io::Result<()> {
        let mut best_bucket = None;
        let mut best_len = 0usize;
        for bucket in 0..self.partitions {
            let staged = self.wc_lens[bucket] as usize;
            let len = self.local_buckets[bucket].len() + staged;
            if len > best_len {
                best_len = len;
                best_bucket = Some(bucket);
            }
        }
        if let Some(bucket) = best_bucket {
            self.flush_bucket(bucket)?;
        }
        Ok(())
    }

    fn flush_all_buckets(&mut self) -> io::Result<()> {
        for b in 0..self.partitions {
            if !self.local_buckets[b].is_empty() || self.wc_lens[b] > 0 {
                self.flush_bucket(b)?;
            }
        }
        Ok(())
    }

    /// Flush all non-empty buckets + commit thread-local stats.
    /// Called once at end of worker.
    pub fn flush_all(&mut self) -> io::Result<()> {
        self.flush_all_buckets()?;
        while self.kite_writer.needs_runtime_drain() {
            if !self.kite_writer.drain_one_partition()? {
                break;
            }
        }
        // Single bulk flush of all thread-local stats → shared atomics
        self.stats.flush_local(&self.local_stats);
        Ok(())
    }

    fn flush_bucket(&mut self, b: usize) -> io::Result<()> {
        // Drain write-combiner staging slot first (may hold up to WCOM_BATCH-1 entries).
        let wlen = self.wc_lens[b] as usize;
        if wlen > 0 {
            let start = b * WCOM_BATCH;
            let staged_ptr = self.wc_staging.as_ptr();
            unsafe {
                self.local_buckets[b].extend_from_slice(
                    std::slice::from_raw_parts(staged_ptr.add(start), wlen)
                );
            }
            self.wc_lens[b] = 0;
        }
        let buf = &mut self.local_buckets[b];
        if buf.is_empty() { return Ok(()); }
        let freed_from_local = buf.len() * K::byte_size();
        self.local_bytes = self.local_bytes.saturating_sub(freed_from_local);

        let t_sort = self.collect_timing.then(Instant::now);
        K::sort_slice(buf, self.k);
        if let Some(t_sort) = t_sort {
            self.local_stats.sort_ns += t_sort.elapsed().as_nanos() as u64;
        }

        let t_rle = self.collect_timing.then(Instant::now);
        self.entries_buf.clear();
        rle_compress_typed(buf, &mut self.entries_buf);

        let raw_bytes = (self.entries_buf.len() * K::entry_size()) as u64;
        let (key_min_lo, key_min_hi) = self.entries_buf[0].0.split_key();
        let (key_max_lo, key_max_hi) = self.entries_buf[self.entries_buf.len() - 1].0.split_key();

        self.byte_buf.clear();
        let used_dvle = K::encode_dvle_run(&self.entries_buf, &mut self.byte_buf);
        let codec = if used_dvle { CODEC_DVLE } else { CODEC_RAW };
        if let Some(t_rle) = t_rle {
            let encode_ns = t_rle.elapsed().as_nanos() as u64;
            self.local_stats.rle_ns += encode_ns;
            self.local_stats.encode_ns += encode_ns;
        }

        let n_entries = self.entries_buf.len();
        self.stats.add_bucket_kmers(b, buf.len() as u64);
        self.stats.add_bucket_runs(b);
        self.local_stats.flush_count += 1;
        self.local_stats.run_count   += 1;
        self.local_stats.run_bytes   += self.byte_buf.len() as u64;

        let expected_cap = (self.local_flush_threshold
            .checked_div(self.partitions * K::byte_size())
            .unwrap_or(4096))
            .max(4096);

        self.entries_buf.clear();
        if self.entries_buf.capacity() > expected_cap * 4 {
            self.entries_buf.shrink_to(expected_cap * 2);
        }

        buf.clear();
        if buf.capacity() > expected_cap * 4 {
            buf.shrink_to(expected_cap * 2);
        }

        self.budget.free(freed_from_local);

        {
            let t_write = self.collect_timing.then(Instant::now);
            self.kite_writer.append_chunk(
                b,
                codec,
                n_entries as u64,
                raw_bytes,
                key_min_lo,
                key_min_hi,
                key_max_lo,
                key_max_hi,
                &self.byte_buf,
            )
                .map_err(|e| io::Error::new(e.kind(),
                    format!("partition {} append_chunk failed: {}", b, e)))?;
            if let Some(t_write) = t_write {
                self.local_stats.writer_io_ns += t_write.elapsed().as_nanos() as u64;
            }
        }
        Ok(())
    }
}
