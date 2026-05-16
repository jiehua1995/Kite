use crate::count::kmer::KmerWord;
use crate::count::memory::SharedBudget;
use crate::count::parser::parse_sequences;
use crate::count::sorter::rle_compress_typed;
use crate::count::writer::ConcurrentKiteWriter;
use crate::format::{CODEC_DVLE, CODEC_RAW, CODEC_ZSTD};
use crate::stats::{LocalCountStats, SharedStats};
use std::io;
use std::sync::Arc;
use std::time::Instant;

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
/// 5. Workers flush encoded runs into the shared async writer via per-partition staging,
///    keeping the hot path off blocking file I/O.
pub struct CountWorker<K: KmerWord> {
    collect_timing: bool,
    live_progress: bool,
    k: u8,
    canonical: bool,
    partitions: usize,
    bucket_shift: u32,
    local_buckets: Vec<Vec<K>>,
    bucket_reserved_bytes: Vec<usize>,
    local_bytes: usize,
    local_flush_threshold: usize,
    budget: SharedBudget,
    stats: SharedStats,
    local_stats: LocalCountStats,
    kite_writer: Arc<ConcurrentKiteWriter>,
    byte_buf: Vec<u8>,
    byte_buf_reserved_bytes: usize,
    entries_buf: Vec<(K, u32)>,
    entries_reserved_bytes: usize,
    zstd_level: i32,

    /// Write-combining staging buffer.  Flat array: partitions × WCOM_BATCH entries.
    /// Kept in L2 cache; flushed to local_buckets in 64-entry batches.
    wc_staging: Box<[K]>,
    wc_lens: Box<[u8]>, // fill count per staging slot (0..=WCOM_BATCH)
}

impl<K: KmerWord> CountWorker<K> {
    pub fn new(
        collect_timing: bool,
        live_progress: bool,
        k: u8,
        canonical: bool,
        partitions: usize,
        bucket_shift: u32,
        budget: SharedBudget,
        stats: SharedStats,
        kite_writer: Arc<ConcurrentKiteWriter>,
        local_flush_threshold: usize,
        zstd_level: i32,
    ) -> Self {
        // Start with a small initial capacity per bucket.  Larger eager
        // reservations looked attractive on paper, but on the HG002 k=51 path
        // they increase kernel allocator work and system time substantially.
        const INITIAL_BUCKET_CAP: usize = 512;
        let cap_per_bucket = INITIAL_BUCKET_CAP.max(64 / K::byte_size());
        let bucket_bytes = partitions
            .saturating_mul(cap_per_bucket)
            .saturating_mul(K::byte_size());
        let wc_bytes = partitions
            .saturating_mul(WCOM_BATCH)
            .saturating_mul(K::byte_size())
            .saturating_add(partitions);
        let byte_buf = Vec::with_capacity(32 * 1024 * 1024);
        budget.acquire(
            bucket_bytes
                .saturating_add(wc_bytes)
                .saturating_add(byte_buf.capacity()),
        );

        CountWorker {
            collect_timing,
            live_progress,
            k,
            canonical,
            partitions,
            bucket_shift,
            local_buckets: (0..partitions)
                .map(|_| Vec::with_capacity(cap_per_bucket))
                .collect(),
            bucket_reserved_bytes: vec![cap_per_bucket * K::byte_size(); partitions],
            local_bytes: 0,
            local_flush_threshold,
            budget,
            stats,
            local_stats: LocalCountStats::default(),
            kite_writer,
            byte_buf_reserved_bytes: byte_buf.capacity(),
            byte_buf,
            entries_buf: Vec::new(),
            entries_reserved_bytes: 0,
            zstd_level,
            wc_staging: {
                let n = partitions * WCOM_BATCH;
                let mut v: Vec<K> = Vec::with_capacity(n);
                unsafe {
                    std::ptr::write_bytes(
                        v.as_mut_ptr() as *mut u8,
                        0,
                        n * std::mem::size_of::<K>(),
                    );
                    v.set_len(n);
                }
                v.into_boxed_slice()
            },
            wc_lens: vec![0u8; partitions].into_boxed_slice(),
        }
    }

    #[inline]
    fn ensure_bucket_capacity(&mut self, bucket: usize, additional: usize) {
        let buf = &mut self.local_buckets[bucket];
        let needed = buf.len().saturating_add(additional);
        if needed <= buf.capacity() {
            return;
        }

        let old_cap = buf.capacity();
        let reserve_additional = needed.saturating_sub(old_cap);
        buf.reserve(reserve_additional);

        let new_cap = buf.capacity();
        let old_reserved = self.bucket_reserved_bytes[bucket];
        let new_reserved = new_cap.saturating_mul(K::byte_size());
        if new_reserved > old_reserved {
            let extra = new_reserved - old_reserved;
            self.budget.acquire(extra);
            self.bucket_reserved_bytes[bucket] = new_reserved;
        }
    }

    #[inline]
    fn flush_staging_slot(&mut self, bucket: usize, count: usize) {
        self.ensure_bucket_capacity(bucket, count);
        let start = bucket * WCOM_BATCH;
        unsafe {
            self.local_buckets
                .get_unchecked_mut(bucket)
                .extend_from_slice(self.wc_staging.get_unchecked(start..start + count));
        }
        self.local_bytes = self
            .local_bytes
            .saturating_add(count.saturating_mul(K::byte_size()));
    }

    #[inline]
    fn push_kmer(&mut self, kmer: K) {
        let bucket = kmer.to_bucket(self.bucket_shift);
        let wlen = self.wc_lens[bucket] as usize;
        self.wc_staging[bucket * WCOM_BATCH + wlen] = kmer;
        let new_wlen = wlen + 1;
        if new_wlen == WCOM_BATCH {
            self.flush_staging_slot(bucket, WCOM_BATCH);
            self.wc_lens[bucket] = 0;
        } else {
            self.wc_lens[bucket] = new_wlen as u8;
        }
    }

    #[inline]
    fn track_vec_growth_bytes(
        reserved_bytes: &mut usize,
        budget: &SharedBudget,
        old_cap: usize,
        new_cap: usize,
        unit_bytes: usize,
    ) {
        if new_cap > old_cap {
            let delta = (new_cap - old_cap).saturating_mul(unit_bytes);
            if delta > 0 {
                budget.acquire(delta);
                *reserved_bytes = reserved_bytes.saturating_add(delta);
            }
        }
    }

    /// Process one decompressed FASTQ chunk.
    #[inline]
    pub fn process_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        // Back-pressure: when the global budget hits the limit, workers must flush
        // local buckets so the async writer can keep the data path streaming.
        while self.budget.is_over_limit() {
            if self.local_bytes > 0 {
                self.flush_all_buckets()?;
            } else if !self.kite_writer.drain_one_partition()? {
                std::thread::yield_now();
                break;
            }
        }

        let k = self.k;
        let canonical = self.canonical;
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
                self.push_kmer(kmer);
            });
            if let Some(t_kmer) = t_kmer {
                kmer_push_ns += t_kmer.elapsed().as_nanos() as u64;
            }
            // ───────────────────────────────────────────────────────────────

            local_kmers += seq_kmers;
        });

        // All accumulation into thread-local fields — zero shared-atomic traffic
        let parse_ns = t_start.map(|t| t.elapsed().as_nanos() as u64).unwrap_or(0);
        if self.live_progress {
            self.stats.add_reads(local_reads);
            self.stats.add_bases(local_bases);
            self.stats.add_kmers(local_kmers);
            self.stats.add_valid_kmers(local_kmers);
            self.stats.add_decomp_bytes(chunk.len() as u64);
        } else {
            self.local_stats.reads += local_reads;
            self.local_stats.bases += local_bases;
            self.local_stats.valid_kmers += local_kmers;
            self.local_stats.decomp_bytes += chunk.len() as u64;
        }
        if collect_timing {
            self.local_stats.parse_ns += parse_ns;
            self.local_stats.kmer_ns += kmer_push_ns;
            self.local_stats.read_ns += parse_ns.saturating_sub(kmer_push_ns);
        }

        if self.local_bytes >= self.local_flush_threshold || self.budget.should_flush() {
            // Flush ALL non-empty buckets in one go so local_bytes resets to
            // ~0.  Flushing only the largest bucket (the previous strategy)
            // leaves 255/256 of the data still above the threshold, causing
            // workers to re-enter this branch after every single chunk — a
            // pathological O(P) write-overhead loop when the total data per
            // worker is close to the limit (as it is for k ≥ 51 with u128).
            self.flush_all_buckets()?;
            if self.kite_writer.needs_runtime_drain() {
                let _ = self.kite_writer.drain_excess_staging()?;
            }
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
            self.flush_staging_slot(b, wlen);
            self.wc_lens[b] = 0;
        }
        if self.local_buckets[b].is_empty() {
            return Ok(());
        }
        let freed_from_local = self.local_buckets[b].len() * K::byte_size();
        self.local_bytes = self.local_bytes.saturating_sub(freed_from_local);

        let t_sort = self.collect_timing.then(Instant::now);
        K::sort_slice(&mut self.local_buckets[b], self.k);
        if let Some(t_sort) = t_sort {
            self.local_stats.sort_ns += t_sort.elapsed().as_nanos() as u64;
        }

        let t_rle = self.collect_timing.then(Instant::now);
        self.entries_buf.clear();
        let entries_old_cap = self.entries_buf.capacity();
        rle_compress_typed(&self.local_buckets[b], &mut self.entries_buf);
        Self::track_vec_growth_bytes(
            &mut self.entries_reserved_bytes,
            &self.budget,
            entries_old_cap,
            self.entries_buf.capacity(),
            std::mem::size_of::<(K, u32)>(),
        );

        let (key_min_lo, key_min_hi) = self.entries_buf[0].0.split_key();
        let (key_max_lo, key_max_hi) = self.entries_buf[self.entries_buf.len() - 1].0.split_key();

        self.byte_buf.clear();
        let byte_old_cap = self.byte_buf.capacity();
        let used_dvle = K::encode_dvle_run(&self.entries_buf, &mut self.byte_buf);
        Self::track_vec_growth_bytes(
            &mut self.byte_buf_reserved_bytes,
            &self.budget,
            byte_old_cap,
            self.byte_buf.capacity(),
            1,
        );
        let mut codec = if used_dvle { CODEC_DVLE } else { CODEC_RAW };
        let payload_raw_bytes = self.byte_buf.len() as u64;
        let logical_entry_bytes = self.entries_buf.len().saturating_mul(K::entry_size());
        let k = self.k;
        let base_zstd_level = self.zstd_level;
        let zstd_level = Self::compression_level_for(
            base_zstd_level,
            k,
            payload_raw_bytes as usize,
            logical_entry_bytes,
            used_dvle,
            K::entry_size(),
        );
        if zstd_level > 0
            && payload_raw_bytes as usize
                >= Self::min_compression_input_bytes(k, used_dvle, K::entry_size())
        {
            if let Ok(compressed) = zstd::bulk::compress(&self.byte_buf, zstd_level) {
                let min_savings = Self::min_compression_savings_bytes(
                    payload_raw_bytes as usize,
                    logical_entry_bytes,
                    used_dvle,
                    K::entry_size(),
                );
                if compressed.len().saturating_add(min_savings) < self.byte_buf.len() {
                    self.byte_buf = compressed;
                    if self.byte_buf.capacity() > self.byte_buf_reserved_bytes {
                        let delta = self.byte_buf.capacity() - self.byte_buf_reserved_bytes;
                        self.budget.acquire(delta);
                        self.byte_buf_reserved_bytes = self.byte_buf.capacity();
                    }
                    codec |= CODEC_ZSTD;
                }
            }
        }
        if let Some(t_rle) = t_rle {
            let encode_ns = t_rle.elapsed().as_nanos() as u64;
            self.local_stats.rle_ns += encode_ns;
            self.local_stats.encode_ns += encode_ns;
        }

        let n_entries = self.entries_buf.len();
        self.stats
            .add_bucket_kmers(b, self.local_buckets[b].len() as u64);
        self.stats.add_bucket_runs(b);
        self.local_stats.flush_count += 1;
        self.local_stats.run_count += 1;
        self.local_stats.run_bytes += self.byte_buf.len() as u64;

        let expected_cap = (self
            .local_flush_threshold
            .checked_div(self.partitions * K::byte_size())
            .unwrap_or(4096))
        .max(4096);

        self.entries_buf.clear();
        if self.entries_buf.capacity() > expected_cap * 4 {
            let old_cap = self.entries_buf.capacity();
            self.entries_buf.shrink_to(expected_cap * 2);
            let new_cap = self.entries_buf.capacity();
            let old_bytes = old_cap.saturating_mul(std::mem::size_of::<(K, u32)>());
            let new_bytes = new_cap.saturating_mul(std::mem::size_of::<(K, u32)>());
            if old_bytes > new_bytes {
                self.budget.free(old_bytes - new_bytes);
                self.entries_reserved_bytes = self
                    .entries_reserved_bytes
                    .saturating_sub(old_bytes - new_bytes);
            }
        }

        self.local_buckets[b].clear();
        if self.local_buckets[b].capacity() > expected_cap * 4 {
            let old_cap = self.local_buckets[b].capacity();
            self.local_buckets[b].shrink_to(expected_cap * 2);
            let new_cap = self.local_buckets[b].capacity();
            let old_bytes = old_cap.saturating_mul(K::byte_size());
            let new_bytes = new_cap.saturating_mul(K::byte_size());
            if old_bytes > new_bytes {
                self.budget.free(old_bytes - new_bytes);
                self.bucket_reserved_bytes[b] = new_bytes;
            }
        }
        self.local_bytes = self.local_bytes.saturating_sub(freed_from_local);

        {
            let t_write = self.collect_timing.then(Instant::now);
            self.kite_writer
                .append_chunk(
                    b,
                    codec,
                    n_entries as u64,
                    payload_raw_bytes,
                    key_min_lo,
                    key_min_hi,
                    key_max_lo,
                    key_max_hi,
                    &self.byte_buf,
                )
                .map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("partition {} append_chunk failed: {}", b, e),
                    )
                })?;
            if let Some(t_write) = t_write {
                self.local_stats.writer_io_ns += t_write.elapsed().as_nanos() as u64;
            }
        }
        Ok(())
    }

    #[inline]
    fn min_compression_input_bytes(k: u8, used_dvle: bool, entry_size: usize) -> usize {
        match (entry_size, used_dvle, k) {
            (12, true, 0..=31) => 16 * 1024,
            (20, true, _) => 32 * 1024,
            (36, _, _) => 64 * 1024,
            _ => 32 * 1024,
        }
    }

    #[inline]
    fn min_compression_savings_bytes(
        payload_bytes: usize,
        logical_entry_bytes: usize,
        used_dvle: bool,
        entry_size: usize,
    ) -> usize {
        let ratio_bias = logical_entry_bytes.saturating_sub(payload_bytes) / 32;
        let absolute_floor = match (entry_size, used_dvle) {
            (12, true) => 8 * 1024,
            (20, true) => 16 * 1024,
            _ => 32 * 1024,
        };
        absolute_floor
            .max(payload_bytes / 100)
            .saturating_sub(ratio_bias.min(24 * 1024))
    }

    #[inline]
    fn compression_level_for(
        base_level: i32,
        k: u8,
        payload_bytes: usize,
        logical_entry_bytes: usize,
        used_dvle: bool,
        entry_size: usize,
    ) -> i32 {
        if base_level <= 0 {
            return 0;
        }
        let mut level = base_level;
        if used_dvle && entry_size == 12 && k <= 31 && payload_bytes >= 1024 * 1024 {
            level += 1;
        }
        if logical_entry_bytes >= payload_bytes.saturating_mul(2)
            && payload_bytes >= 4 * 1024 * 1024
        {
            level += 1;
        }
        level.min(6)
    }
}
