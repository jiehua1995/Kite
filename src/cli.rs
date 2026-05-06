use clap::{Parser, Subcommand, Args, ValueEnum};

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum CountMode {
    Fast,
    Balanced,
    Compact,
}

impl CountMode {
    #[inline]
    pub fn zstd_level(self) -> i32 {
        match self {
            CountMode::Fast => 0, // Disabled in Fast mode for max speed
            CountMode::Balanced => 1,
            CountMode::Compact => 5,
        }
    }
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

    /// Number of partitions/buckets (must be power of 2)
    #[arg(long = "partitions", default_value_t = 256)]
    pub partitions: usize,

    /// K-mers per chunk (controls batch size, flush frequency, and run granularity)
    #[arg(long = "chunk-kmers", default_value_t = 16_000_000)]
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

    /// Output mode: fast(1) / balanced(5) / compact(9) zstd compression level
    #[arg(long = "mode", value_enum, default_value_t = CountMode::Fast)]
    pub mode: CountMode,

    /// I/O buffer size in MB for the .kite write buffer (larger = fewer syscalls)
    #[arg(long = "io-buf-mb", default_value_t = 64)]
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

    /// Show progress during dumping
    #[arg(long = "progress")]
    pub progress: bool,

    /// Enable detailed test-mode diagnostics output
    #[arg(long = "test-mode")]
    pub test_mode: bool,

    /// I/O buffer size in MB for output writing (larger = fewer syscalls)
    #[arg(long = "io-buf-mb", default_value_t = 16)]
    pub io_buf_mb: usize,
}
