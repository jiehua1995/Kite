use clap::{Parser, Subcommand, Args, ValueEnum};

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum CountMode {
    Fast,
    Balanced,
    Compact,
}

#[derive(Parser, Debug)]
#[command(name = "kite", about = "High-performance k-mer counter", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Count k-mers from FASTQ input
    Count(CountArgs),
    /// Dump k-mer counts from .kite file to TSV
    Dump(DumpArgs),
}

#[derive(Args, Debug)]
pub struct CountArgs {
    /// K-mer length (1–31 uses u64, 32–64 uses u128)
    #[arg(short = 'k', long = "kmer", default_value_t = 21)]
    pub k: u8,

    /// Input FASTQ file (.fastq, .fq, .fastq.gz, .fq.gz)
    #[arg(short = 'i', long = "input")]
    pub input: String,

    /// Output .kite file path
    #[arg(short = 'o', long = "output")]
    pub output: String,

    /// Number of threads (0 = auto-detect)
    #[arg(long = "threads", default_value_t = 0)]
    pub threads: usize,

    /// Number of partitions/buckets (0 = auto; otherwise must be power of 2)
    #[arg(long = "partitions", default_value_t = 0)]
    pub partitions: usize,

    /// K-mers per chunk (controls batch size, flush frequency, and run granularity)
    #[arg(long = "chunk-kmers", default_value_t = 8_000_000)]
    pub chunk_kmers: usize,

    /// Maximum memory budget in GB (must truly affect buffer sizes and flush strategy)
    #[arg(long = "max-memory-gb", default_value_t = 16.0)]
    pub max_memory_gb: f64,

    /// Count canonical k-mers (min of forward and reverse complement)
    #[arg(long = "canonical", default_value_t = true)]
    pub canonical: bool,

    /// Show progress during counting
    #[arg(long = "progress")]
    pub progress: bool,

    /// Enable detailed test-mode diagnostics output
    #[arg(long = "test-mode")]
    pub test_mode: bool,

    /// Output mode hint. Current Plan04 baseline uses DVLE-only for u64/u128 hot paths.
    #[arg(long = "mode", value_enum, default_value_t = CountMode::Fast)]
    pub mode: CountMode,

    /// Per-partition staging buffer in MB for .kite pwrite batching (clamped to 4-32 MB)
    #[arg(long = "io-buf-mb", default_value_t = 16)]
    pub io_buf_mb: usize,
}

#[derive(Args, Debug)]
pub struct DumpArgs {
    /// Input .kite file
    #[arg(short = 'i', long = "input")]
    pub input: String,

    /// Output TSV file path (- for stdout)
    #[arg(short = 'o', long = "output")]
    pub output: String,

    /// Number of threads (0 = auto-detect)
    #[arg(long = "threads", default_value_t = 0)]
    pub threads: usize,

    /// Number of dump writer threads (0 = auto => max(1, threads / 6)).
    #[arg(long = "writer-threads", default_value_t = 0)]
    pub writer_threads: usize,

    /// Show progress during dumping
    #[arg(long = "progress")]
    pub progress: bool,

    /// Enable detailed test-mode diagnostics output
    #[arg(long = "test-mode")]
    pub test_mode: bool,
}
