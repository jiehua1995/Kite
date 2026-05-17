use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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

impl CountStats {
    pub fn new(partitions: usize, timing_enabled: bool) -> Self {
        let mut s = CountStats::default();
        s.timing_enabled = timing_enabled;
        s.bucket_kmer_counts = (0..partitions).map(|_| AtomicU64::new(0)).collect();
        s.bucket_run_counts = (0..partitions).map(|_| AtomicU64::new(0)).collect();
        s
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
    pub fn add_finalize_ns(&self, n: u64) {
        self.finalize_ns.fetch_add(n, Ordering::Relaxed);
    }
    pub fn add_decomp_ns(&self, n: u64) {
        self.decomp_ns.fetch_add(n, Ordering::Relaxed);
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
        output_bytes: u64,
        data_area_bytes: u64,
        total_chunks: u64,
        total_rows: u64,
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

        let mut sections = vec![
            ReportSection::new(
                "configuration",
                vec![
                    ("input", args.input.clone()),
                    ("k", args.k.to_string()),
                    ("canonical", args.canonical.to_string()),
                    (
                        "min",
                        args.min
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "none".to_string()),
                    ),
                    (
                        "max",
                        args.max
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "none".to_string()),
                    ),
                    ("threads", actual_threads.to_string()),
                    ("partitions", actual_partitions.to_string()),
                    ("chunk_kmers", args.chunk_kmers.to_string()),
                    (
                        "memory",
                        args.memory.as_deref().unwrap_or("auto").to_string(),
                    ),
                    ("buffer_mb", args.buffer.to_string()),
                ],
            ),
            ReportSection::new(
                "throughput",
                vec![
                    ("compressed_bytes_read", fmt_bytes(comp)),
                    ("decompressed_bytes", fmt_bytes(decomp)),
                    ("reads_processed", fmt_num(reads)),
                    ("bases_processed", fmt_num(bases)),
                    ("kmers_generated", fmt_num(kmers)),
                    ("valid_kmers_generated", fmt_num(valid)),
                ],
            ),
            ReportSection::new(
                "storage",
                vec![
                    ("output_bytes", fmt_bytes(output_bytes)),
                    ("data_area_bytes", fmt_bytes(data_area_bytes)),
                    ("total_chunks", fmt_num(total_chunks)),
                    ("total_rows", fmt_num(total_rows)),
                    ("total_flushes", flushes.to_string()),
                    ("total_runs", runs.to_string()),
                    (
                        "avg_run_entries",
                        if runs > 0 { valid / runs } else { 0 }.to_string(),
                    ),
                    ("run_bytes_written", fmt_bytes(run_bytes)),
                ],
            ),
            ReportSection::new(
                "timing",
                vec![
                    ("total_wall_time", fmt_seconds(wall)),
                    ("read", fmt_millis(read_ms)),
                    ("decompress", fmt_millis(decomp_ms)),
                    ("parse", fmt_millis(parse_ms)),
                    ("kmer_scan", fmt_millis(kmer_ms)),
                    ("sort", fmt_millis(sort_ms)),
                    ("rle", fmt_millis(rle_ms)),
                    ("encode", fmt_millis(encode_ms)),
                    ("finalize", fmt_millis(finalize_ms)),
                    ("writer_queue_wait", fmt_millis(writer_queue_wait_ms)),
                    ("writer_io", fmt_millis(writer_io_ms)),
                ],
            ),
            ReportSection::new(
                "resources",
                vec![
                    ("cpu_util_pct", format!("{:.1}", cpu_util_pct)),
                    ("peak_rss_mb", peak_rss_mb.to_string()),
                ],
            ),
        ];
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
            let mut rows = vec![
                ("bucket_count", n_buckets.to_string()),
                ("avg_kmers_per_bucket", fmt_num(avg_b)),
                ("max_kmers_per_bucket", fmt_num(max_b)),
                (
                    "hottest_partition_segments",
                    self.hottest_partition_segments
                        .load(Ordering::Relaxed)
                        .to_string(),
                ),
                (
                    "least_partition_segments",
                    self.least_partition_segments
                        .load(Ordering::Relaxed)
                        .to_string(),
                ),
            ];
            for (cnt, idx) in sorted_b.iter().take(5) {
                let pct = if total_b > 0 {
                    *cnt as f64 / total_b as f64 * 100.0
                } else {
                    0.0
                };
                rows.push((
                    "hot_bucket",
                    format!("#{}: {} kmers ({:.2}%)", idx, fmt_num(*cnt), pct),
                ));
            }
            sections.push(ReportSection::new("distribution", rows));
        }
        print_test_report("count", sections);
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

        print_test_report(
            "dump",
            vec![
                ReportSection::new(
                    "configuration",
                    vec![
                        ("input", args.input.clone()),
                        ("output", args.output.clone()),
                        ("threads", actual_threads.to_string()),
                        ("partitions", n_buckets.to_string()),
                        ("total_chunks", total_runs.to_string()),
                    ],
                ),
                ReportSection::new(
                    "throughput",
                    vec![
                        ("total_rows_output", fmt_num(rows)),
                        ("bytes_read", fmt_bytes(bread)),
                        ("bytes_written", fmt_bytes(bwrite)),
                        ("dump_throughput_mb_s", format!("{:.1}", throughput_mb_s)),
                    ],
                ),
                ReportSection::new(
                    "timing",
                    vec![
                        ("total_wall_time", fmt_seconds(wall)),
                        ("decode", fmt_millis(decode_ms)),
                        ("merge", fmt_millis(merge_ms)),
                        ("format", fmt_millis(format_ms)),
                        ("writer_queue_wait", fmt_millis(writer_queue_wait_ms)),
                        ("writer_io", fmt_millis(writer_io_ms)),
                        ("concat", fmt_millis(concat_ms)),
                        ("longest_worker", fmt_millis(longest_worker_ms)),
                        ("shortest_worker", fmt_millis(shortest_worker_ms)),
                    ],
                ),
                ReportSection::new(
                    "resources",
                    vec![
                        ("cpu_util_pct", format!("{:.1}", cpu_util_pct)),
                        ("peak_rss_mb", peak_rss_mb.to_string()),
                    ],
                ),
                ReportSection::new(
                    "distribution",
                    vec![
                        (
                            "hottest_partition_chunks",
                            self.hottest_partition_chunks
                                .load(Ordering::Relaxed)
                                .to_string(),
                        ),
                        (
                            "coldest_partition_chunks",
                            self.coldest_partition_chunks
                                .load(Ordering::Relaxed)
                                .to_string(),
                        ),
                        (
                            "p99_chunk_bytes",
                            self.p99_chunk_bytes.load(Ordering::Relaxed).to_string(),
                        ),
                        (
                            "shard_count",
                            self.shard_count.load(Ordering::Relaxed).to_string(),
                        ),
                        (
                            "hottest_shard_partitions",
                            self.hottest_shard_partitions
                                .load(Ordering::Relaxed)
                                .to_string(),
                        ),
                        (
                            "coldest_shard_partitions",
                            self.coldest_shard_partitions
                                .load(Ordering::Relaxed)
                                .to_string(),
                        ),
                        (
                            "hottest_shard_bytes",
                            fmt_bytes(self.hottest_shard_bytes.load(Ordering::Relaxed)),
                        ),
                        (
                            "coldest_shard_bytes",
                            fmt_bytes(self.coldest_shard_bytes.load(Ordering::Relaxed)),
                        ),
                    ],
                ),
            ],
        );
    }
}

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

pub struct ReportSection {
    title: String,
    rows: Vec<(String, String)>,
}

impl ReportSection {
    pub fn new(title: impl Into<String>, rows: Vec<(&str, String)>) -> Self {
        Self {
            title: title.into(),
            rows: rows
                .into_iter()
                .map(|(label, value)| (label.to_string(), value))
                .collect(),
        }
    }
}

pub fn print_test_report(command: &str, sections: Vec<ReportSection>) {
    let prefix = format!("[kite {}]", command);
    let width = sections
        .iter()
        .flat_map(|section| section.rows.iter().map(|(label, _)| label.len()))
        .max()
        .unwrap_or(0);

    eprintln!("{} === test-mode report ===", prefix);
    for section in sections {
        eprintln!("{} -- {} --", prefix, section.title);
        for (label, value) in section.rows {
            eprintln!("{}   {:width$}: {}", prefix, label, value, width = width);
        }
    }
    eprintln!("{} === end test-mode report ===", prefix);
}

pub fn print_simple_test_report(command: &str, entries: &[(&str, String)], wall: Duration) {
    let cpu_util_pct = get_process_cpu_util_pct(wall);
    let peak_rss_mb = get_peak_rss_mb();
    print_test_report(
        command,
        vec![
            ReportSection::new(
                "summary",
                entries
                    .iter()
                    .map(|(label, value)| (*label, value.clone()))
                    .collect(),
            ),
            ReportSection::new("timing", vec![("total_wall_time", fmt_seconds(wall))]),
            ReportSection::new(
                "resources",
                vec![
                    ("cpu_util_pct", format!("{:.1}", cpu_util_pct)),
                    ("peak_rss_mb", peak_rss_mb.to_string()),
                ],
            ),
        ],
    );
}

fn fmt_seconds(duration: Duration) -> String {
    format!("{:.3}s", duration.as_secs_f64())
}

fn fmt_millis(ms: u64) -> String {
    format!("{}ms", ms)
}
