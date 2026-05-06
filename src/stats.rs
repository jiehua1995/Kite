use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Statistics collected during `kite count`.
/// Uses atomics for fields updated across threads.
#[derive(Default)]
pub struct CountStats {
    timing_enabled: bool,
    pub reads_processed: AtomicU64,
    pub bases_processed: AtomicU64,
    pub kmers_generated: AtomicU64,
    pub valid_kmers_generated: AtomicU64,
    pub compressed_bytes_read: AtomicU64,
    pub decomp_bytes_produced: AtomicU64,
    pub flush_count: AtomicU64,
    pub total_runs: AtomicU64,
    pub total_run_bytes: AtomicU64,
    pub sort_ns: AtomicU64,
    pub rle_ns: AtomicU64,
    pub encode_ns: AtomicU64,
    pub finalize_ns: AtomicU64,
    pub writer_queue_wait_ns: AtomicU64,
    pub writer_io_ns: AtomicU64,
    pub decomp_ns: AtomicU64,
    pub parse_ns: AtomicU64,
    pub kmer_ns: AtomicU64,
    pub read_ns: AtomicU64,
    // Per-bucket k-mer counts (allocated with partitions capacity)
    pub bucket_kmer_counts: Vec<AtomicU64>,
    pub bucket_run_counts: Vec<AtomicU64>,
    pub hottest_partition_segments: AtomicU64,
    pub least_partition_segments: AtomicU64,
}

#[allow(dead_code)]
impl CountStats {
    pub fn new(partitions: usize, timing_enabled: bool) -> Self {
        let mut s = CountStats::default();
        s.timing_enabled = timing_enabled;
        s.bucket_kmer_counts = (0..partitions).map(|_| AtomicU64::new(0)).collect();
        s.bucket_run_counts = (0..partitions).map(|_| AtomicU64::new(0)).collect();
        s
    }

    #[inline]
    pub fn timing_enabled(&self) -> bool {
        self.timing_enabled
    }

    pub fn add_reads(&self, n: u64) {
        self.reads_processed.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_bases(&self, n: u64) {
        self.bases_processed.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_kmers(&self, n: u64) {
        self.kmers_generated.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_valid_kmers(&self, n: u64) {
        self.valid_kmers_generated.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_comp_bytes(&self, n: u64) {
        self.compressed_bytes_read.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_decomp_bytes(&self, n: u64) {
        self.decomp_bytes_produced.fetch_add(n, Ordering::Relaxed);
    }
    pub fn inc_flush(&self) {
        self.flush_count.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_runs(&self) {
        self.total_runs.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add_run_bytes(&self, n: u64) {
        self.total_run_bytes.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_sort_ns(&self, n: u64) {
        self.sort_ns.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_rle_ns(&self, n: u64) {
        self.rle_ns.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_encode_ns(&self, n: u64) {
        self.encode_ns.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_finalize_ns(&self, n: u64) {
        self.finalize_ns.fetch_add(n, Ordering::Relaxed);
    }
    #[allow(dead_code)]
    pub fn add_writer_queue_wait_ns(&self, n: u64) {
        self.writer_queue_wait_ns.fetch_add(n, Ordering::Relaxed);
    }
    #[allow(dead_code)]
    pub fn add_writer_io_ns(&self, n: u64) {
        self.writer_io_ns.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_decomp_ns(&self, n: u64) {
        self.decomp_ns.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_kmer_ns(&self, n: u64) {
        self.kmer_ns.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_parse_ns(&self, n: u64) {
        self.parse_ns.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_read_ns(&self, n: u64) {
        self.read_ns.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_bucket_kmers(&self, bucket: usize, n: u64) {
        if bucket < self.bucket_kmer_counts.len() {
            self.bucket_kmer_counts[bucket].fetch_add(n, Ordering::Relaxed);
        }
    }

    pub fn add_bucket_runs(&self, bucket: usize) {
        if bucket < self.bucket_run_counts.len() {
            self.bucket_run_counts[bucket].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn set_partition_segment_distribution(&self, hottest: u64, least: u64) {
        self.hottest_partition_segments
            .store(hottest, Ordering::Relaxed);
        self.least_partition_segments
            .store(least, Ordering::Relaxed);
    }

    pub fn print_report(
        &self,
        args: &crate::cli::CountArgs,
        actual_threads: usize,
        actual_partitions: usize,
        wall: Duration,
    ) {
        let reads = self.reads_processed.load(Ordering::Relaxed);
        let bases = self.bases_processed.load(Ordering::Relaxed);
        let kmers = self.kmers_generated.load(Ordering::Relaxed);
        let valid = self.valid_kmers_generated.load(Ordering::Relaxed);
        let comp = self.compressed_bytes_read.load(Ordering::Relaxed);
        let decomp = self.decomp_bytes_produced.load(Ordering::Relaxed);
        let flushes = self.flush_count.load(Ordering::Relaxed);
        let runs = self.total_runs.load(Ordering::Relaxed);
        let run_bytes = self.total_run_bytes.load(Ordering::Relaxed);
        let sort_ms = self.sort_ns.load(Ordering::Relaxed) / 1_000_000;
        let rle_ms = self.rle_ns.load(Ordering::Relaxed) / 1_000_000;
        let encode_ms = self.encode_ns.load(Ordering::Relaxed) / 1_000_000;
        let finalize_ms = self.finalize_ns.load(Ordering::Relaxed) / 1_000_000;
        let writer_queue_wait_ms = self.writer_queue_wait_ns.load(Ordering::Relaxed) / 1_000_000;
        let writer_io_ms = self.writer_io_ns.load(Ordering::Relaxed) / 1_000_000;
        let decomp_ms = self.decomp_ns.load(Ordering::Relaxed) / 1_000_000;
        let kmer_ms = self.kmer_ns.load(Ordering::Relaxed) / 1_000_000;
        let parse_ms = self.parse_ns.load(Ordering::Relaxed) / 1_000_000;
        let read_ms = self.read_ns.load(Ordering::Relaxed) / 1_000_000;

        let peak_rss_mb = get_peak_rss_mb();
        let cpu_util_pct = get_process_cpu_util_pct(wall);

        eprintln!("=== kite count test-mode report ===");
        eprintln!("  input file:           {}", args.input);
        eprintln!("  k:                    {}", args.k);
        eprintln!("  canonical:            {}", args.canonical);
        eprintln!("  threads:              {}", actual_threads);
        eprintln!("  partitions:           {}", actual_partitions);
        eprintln!("  chunk-kmers:          {}", args.chunk_kmers);
        eprintln!(
            "  memory:               {}",
            args.memory.as_deref().unwrap_or("auto")
        );
        eprintln!("  buffer-mb:            {}", args.buffer);
        eprintln!("  ---");
        eprintln!("  compressed bytes read:    {}", fmt_bytes(comp));
        eprintln!("  decompressed bytes:       {}", fmt_bytes(decomp));
        eprintln!("  reads processed:          {}", fmt_num(reads));
        eprintln!("  bases processed:          {}", fmt_num(bases));
        eprintln!("  kmers generated:          {}", fmt_num(kmers));
        eprintln!("  valid kmers generated:    {}", fmt_num(valid));
        eprintln!("  ---");
        eprintln!("  total flushes:        {}", flushes);
        eprintln!("  total runs:           {}", runs);
        eprintln!(
            "  avg run entries:      {}",
            if runs > 0 { valid / runs } else { 0 }
        );
        eprintln!("  run bytes written:    {}", fmt_bytes(run_bytes));
        eprintln!("  ---");
        eprintln!("  read_ms:              {}", read_ms);
        eprintln!("  decomp_ms:            {}", decomp_ms);
        eprintln!("  parse_ms:             {}", parse_ms);
        eprintln!("  kmer_scan_ms:         {}", kmer_ms);
        eprintln!("  sort_ms:              {}", sort_ms);
        eprintln!("  rle_ms:               {}", rle_ms);
        eprintln!("  encode_ms:            {}", encode_ms);
        eprintln!("  finalize_ms:          {}", finalize_ms);
        eprintln!("  writer_queue_wait_ms: {}", writer_queue_wait_ms);
        eprintln!("  writer_io_ms:         {}", writer_io_ms);
        eprintln!("  total wall time:      {:.3}s", wall.as_secs_f64());
        eprintln!("  cpu_util_pct:         {:.1}", cpu_util_pct);
        eprintln!("  peak_rss_mb:          {}", peak_rss_mb);
        // Bucket distribution
        let n_buckets = self.bucket_kmer_counts.len();
        if n_buckets > 0 {
            let counts: Vec<u64> = self
                .bucket_kmer_counts
                .iter()
                .map(|a| a.load(Ordering::Relaxed))
                .collect();
            let total_b: u64 = counts.iter().sum();
            let max_b = counts.iter().copied().max().unwrap_or(0);
            let avg_b = if n_buckets > 0 {
                total_b / n_buckets as u64
            } else {
                0
            };
            // Find top 5 hottest buckets
            let mut sorted_b: Vec<(u64, usize)> = counts
                .iter()
                .copied()
                .enumerate()
                .map(|(i, c)| (c, i))
                .collect();
            sorted_b.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            eprintln!("  ---");
            eprintln!("  bucket count:         {}", n_buckets);
            eprintln!("  avg kmers/bucket:     {}", avg_b);
            eprintln!("  max kmers/bucket:     {}", max_b);
            eprintln!(
                "  hottest_partition_segments: {}",
                self.hottest_partition_segments.load(Ordering::Relaxed)
            );
            eprintln!(
                "  least_partition_segments:   {}",
                self.least_partition_segments.load(Ordering::Relaxed)
            );
            eprintln!("  hottest buckets:");
            for (cnt, idx) in sorted_b.iter().take(5) {
                eprintln!(
                    "    bucket {:4}:  {} kmers  ({:.2}%)",
                    idx,
                    cnt,
                    if total_b > 0 {
                        *cnt as f64 / total_b as f64 * 100.0
                    } else {
                        0.0
                    }
                );
            }
        }
        eprintln!("====================================");
    }
}

/// Thread-local statistics accumulator for CountWorker.
/// Holds all per-chunk / per-flush counters as plain integers so workers
/// never touch shared atomics in the hot path.  Flushed once per worker at
/// the very end of `flush_all()`.
#[derive(Default)]
pub struct LocalCountStats {
    pub reads: u64,
    pub bases: u64,
    pub valid_kmers: u64,
    pub decomp_bytes: u64,
    pub flush_count: u64,
    pub run_count: u64,
    pub run_bytes: u64,
    pub sort_ns: u64,
    pub rle_ns: u64,
    pub encode_ns: u64,
    pub parse_ns: u64,
    /// Time spent inside K::scan_sequence + Vec push (subset of parse_ns).
    /// parse_ns - kmer_ns ≈ FASTQ record-finding overhead.
    pub kmer_ns: u64,
    pub read_ns: u64,
    pub writer_io_ns: u64,
}

impl CountStats {
    /// Flush a worker's thread-local counters into the shared atomic stats.
    /// Called exactly once per worker at the end of its run.
    pub fn flush_local(&self, local: &LocalCountStats) {
        self.reads_processed
            .fetch_add(local.reads, Ordering::Relaxed);
        self.bases_processed
            .fetch_add(local.bases, Ordering::Relaxed);
        self.kmers_generated
            .fetch_add(local.valid_kmers, Ordering::Relaxed);
        self.valid_kmers_generated
            .fetch_add(local.valid_kmers, Ordering::Relaxed);
        self.decomp_bytes_produced
            .fetch_add(local.decomp_bytes, Ordering::Relaxed);
        self.flush_count
            .fetch_add(local.flush_count, Ordering::Relaxed);
        self.total_runs
            .fetch_add(local.run_count, Ordering::Relaxed);
        self.total_run_bytes
            .fetch_add(local.run_bytes, Ordering::Relaxed);
        self.sort_ns.fetch_add(local.sort_ns, Ordering::Relaxed);
        self.rle_ns.fetch_add(local.rle_ns, Ordering::Relaxed);
        self.encode_ns.fetch_add(local.encode_ns, Ordering::Relaxed);
        self.parse_ns.fetch_add(local.parse_ns, Ordering::Relaxed);
        self.kmer_ns.fetch_add(local.kmer_ns, Ordering::Relaxed);
        self.read_ns.fetch_add(local.read_ns, Ordering::Relaxed);
        self.writer_io_ns
            .fetch_add(local.writer_io_ns, Ordering::Relaxed);
    }
}

pub type SharedStats = Arc<CountStats>;

/// Statistics collected during `kite dump`.
#[derive(Default)]
pub struct DumpStats {
    timing_enabled: bool,
    pub total_rows: AtomicU64,
    pub bytes_read: AtomicU64,
    pub bytes_written: AtomicU64,
    pub decode_ns: AtomicU64,
    pub merge_ns: AtomicU64,
    pub format_ns: AtomicU64,
    pub writer_queue_wait_ns: AtomicU64,
    pub writer_io_ns: AtomicU64,
    pub concat_ns: AtomicU64,
    pub partitions_processed: AtomicU64,
    pub longest_worker_ns: AtomicU64,
    pub shortest_worker_ns: AtomicU64,
    pub hottest_partition_chunks: AtomicU64,
    pub coldest_partition_chunks: AtomicU64,
    pub p99_chunk_bytes: AtomicU64,
    pub shard_count: AtomicU64,
    pub hottest_shard_partitions: AtomicU64,
    pub coldest_shard_partitions: AtomicU64,
    pub hottest_shard_bytes: AtomicU64,
    pub coldest_shard_bytes: AtomicU64,
}

impl DumpStats {
    pub fn new(timing_enabled: bool) -> Self {
        let mut stats = DumpStats::default();
        stats.timing_enabled = timing_enabled;
        stats
    }

    #[inline]
    pub fn timing_enabled(&self) -> bool {
        self.timing_enabled
    }

    pub fn add_rows(&self, n: u64) {
        self.total_rows.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_read(&self, n: u64) {
        self.bytes_read.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_written(&self, n: u64) {
        self.bytes_written.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_decode_ns(&self, n: u64) {
        self.decode_ns.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_merge_ns(&self, n: u64) {
        self.merge_ns.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_format_ns(&self, n: u64) {
        self.format_ns.fetch_add(n, Ordering::Relaxed);
    }
    #[allow(dead_code)]
    pub fn add_writer_queue_wait_ns(&self, n: u64) {
        self.writer_queue_wait_ns.fetch_add(n, Ordering::Relaxed);
    }
    #[allow(dead_code)]
    pub fn add_writer_io_ns(&self, n: u64) {
        self.writer_io_ns.fetch_add(n, Ordering::Relaxed);
    }
    #[allow(dead_code)]
    pub fn add_concat_ns(&self, n: u64) {
        self.concat_ns.fetch_add(n, Ordering::Relaxed);
    }
    pub fn inc_partitions(&self) {
        self.partitions_processed.fetch_add(1, Ordering::Relaxed);
    }
    pub fn observe_worker_ns(&self, n: u64) {
        let _ = self
            .longest_worker_ns
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| Some(cur.max(n)));
        let _ = self
            .shortest_worker_ns
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(if cur == 0 { n } else { cur.min(n) })
            });
    }
    pub fn set_partition_distribution(&self, hottest: u64, coldest: u64, p99_chunk_bytes: u64) {
        self.hottest_partition_chunks
            .store(hottest, Ordering::Relaxed);
        self.coldest_partition_chunks
            .store(coldest, Ordering::Relaxed);
        self.p99_chunk_bytes
            .store(p99_chunk_bytes, Ordering::Relaxed);
    }

    pub fn print_report(
        &self,
        args: &crate::cli::DumpArgs,
        actual_threads: usize,
        total_runs: u64,
        n_buckets: usize,
        wall: Duration,
    ) {
        let rows = self.total_rows.load(Ordering::Relaxed);
        let bread = self.bytes_read.load(Ordering::Relaxed);
        let bwrite = self.bytes_written.load(Ordering::Relaxed);
        let decode_ms = self.decode_ns.load(Ordering::Relaxed) / 1_000_000;
        let merge_ms = self.merge_ns.load(Ordering::Relaxed) / 1_000_000;
        let format_ms = self.format_ns.load(Ordering::Relaxed) / 1_000_000;
        let writer_queue_wait_ms = self.writer_queue_wait_ns.load(Ordering::Relaxed) / 1_000_000;
        let writer_io_ms = self.writer_io_ns.load(Ordering::Relaxed) / 1_000_000;
        let concat_ms = self.concat_ns.load(Ordering::Relaxed) / 1_000_000;
        let peak_rss_mb = get_peak_rss_mb();
        let longest_worker_ms = self.longest_worker_ns.load(Ordering::Relaxed) / 1_000_000;
        let shortest_worker_ms = self.shortest_worker_ns.load(Ordering::Relaxed) / 1_000_000;
        let cpu_util_pct = get_process_cpu_util_pct(wall);
        let throughput_mb_s = if wall.as_secs_f64() > 0.0 {
            bwrite as f64 / 1024.0 / 1024.0 / wall.as_secs_f64()
        } else {
            0.0
        };

        eprintln!("=== kite dump test-mode report ===");
        eprintln!("  input .kite:          {}", args.input);
        eprintln!("  output:               {}", args.output);
        eprintln!("  threads:              {}", actual_threads);
        eprintln!("  partition count:      {}", n_buckets);
        eprintln!("  total chunks:         {}", total_runs);
        eprintln!("  ---");
        eprintln!("  total rows output:    {}", fmt_num(rows));
        eprintln!("  bytes read:           {}", fmt_bytes(bread));
        eprintln!("  bytes written:        {}", fmt_bytes(bwrite));
        eprintln!("  ---");
        eprintln!("  decode_ms:            {}", decode_ms);
        eprintln!("  merge_ms:             {}", merge_ms);
        eprintln!("  format_ms:            {}", format_ms);
        eprintln!("  writer_queue_wait_ms: {}", writer_queue_wait_ms);
        eprintln!("  writer_io_ms:         {}", writer_io_ms);
        eprintln!("  tsv_write_ms:         {}", writer_io_ms);
        eprintln!("  concat_ms:            {}", concat_ms);
        eprintln!("  longest_worker_ms:    {}", longest_worker_ms);
        eprintln!("  shortest_worker_ms:   {}", shortest_worker_ms);
        eprintln!("  total wall time:      {:.3}s", wall.as_secs_f64());
        eprintln!("  dump_throughput_mb_s: {:.1}", throughput_mb_s);
        eprintln!("  cpu_util_pct:         {:.1}", cpu_util_pct);
        eprintln!("  peak_rss_mb:          {}", peak_rss_mb);
        eprintln!(
            "  hottest_partition_chunks: {}",
            self.hottest_partition_chunks.load(Ordering::Relaxed)
        );
        eprintln!(
            "  coldest_partition_chunks: {}",
            self.coldest_partition_chunks.load(Ordering::Relaxed)
        );
        eprintln!(
            "  hottest_partition_segments: {}",
            self.hottest_partition_chunks.load(Ordering::Relaxed)
        );
        eprintln!(
            "  least_partition_segments:   {}",
            self.coldest_partition_chunks.load(Ordering::Relaxed)
        );
        eprintln!(
            "  p99_chunk_bytes:      {}",
            self.p99_chunk_bytes.load(Ordering::Relaxed)
        );
        eprintln!(
            "  shard_count:          {}",
            self.shard_count.load(Ordering::Relaxed)
        );
        eprintln!(
            "  hottest_shard_partitions: {}",
            self.hottest_shard_partitions.load(Ordering::Relaxed)
        );
        eprintln!(
            "  coldest_shard_partitions: {}",
            self.coldest_shard_partitions.load(Ordering::Relaxed)
        );
        eprintln!(
            "  hottest_shard_bytes:  {}",
            self.hottest_shard_bytes.load(Ordering::Relaxed)
        );
        eprintln!(
            "  coldest_shard_bytes:  {}",
            self.coldest_shard_bytes.load(Ordering::Relaxed)
        );
        eprintln!("===================================");
    }
}

#[allow(dead_code)]
pub type SharedDumpStats = Arc<DumpStats>;

fn fmt_bytes(n: u64) -> String {
    if n >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", n as f64 / 1024.0 / 1024.0 / 1024.0)
    } else if n >= 1024 * 1024 {
        format!("{:.2} MB", n as f64 / 1024.0 / 1024.0)
    } else if n >= 1024 {
        format!("{:.2} KB", n as f64 / 1024.0)
    } else {
        format!("{} B", n)
    }
}

fn fmt_num(n: u64) -> String {
    // Insert commas every 3 digits
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Get peak RSS in MB via /proc/self/status on Linux.
pub fn get_peak_rss_mb() -> u64 {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if rc == 0 {
            let usage = unsafe { usage.assume_init() };
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            {
                return (usage.ru_maxrss as u64) / 1024 / 1024;
            }
            #[cfg(not(any(target_os = "macos", target_os = "ios")))]
            {
                return (usage.ru_maxrss as u64) / 1024;
            }
        }
    }
    0
}

pub fn get_process_cpu_util_pct(wall: Duration) -> f64 {
    if wall.is_zero() {
        return 0.0;
    }
    #[cfg(target_os = "linux")]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if rc == 0 {
            let usage = unsafe { usage.assume_init() };
            let user_s = usage.ru_utime.tv_sec as f64 + usage.ru_utime.tv_usec as f64 / 1_000_000.0;
            let sys_s = usage.ru_stime.tv_sec as f64 + usage.ru_stime.tv_usec as f64 / 1_000_000.0;
            return (user_s + sys_s) / wall.as_secs_f64() * 100.0;
        }
    }
    0.0
}

pub fn print_simple_test_report(command: &str, entries: &[(&str, String)], wall: Duration) {
    let cpu_util_pct = get_process_cpu_util_pct(wall);
    let peak_rss_mb = get_peak_rss_mb();
    let width = entries
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or(0)
        .max("total wall time".len())
        .max("cpu_util_pct".len())
        .max("peak_rss_mb".len());

    eprintln!("=== kite {} test-mode report ===", command);
    for (label, value) in entries {
        eprintln!("  {:width$}: {}", label, value, width = width);
    }
    eprintln!(
        "  {:width$}: {:.3}s",
        "total wall time",
        wall.as_secs_f64(),
        width = width
    );
    eprintln!(
        "  {:width$}: {:.1}",
        "cpu_util_pct",
        cpu_util_pct,
        width = width
    );
    eprintln!("  {:width$}: {}", "peak_rss_mb", peak_rss_mb, width = width);
    eprintln!("====================================");
}
