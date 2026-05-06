use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

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
            CountMode::Fast => 1,
            CountMode::Balanced => 2,
            CountMode::Compact => 5,
        }
    }

    #[inline]
    pub fn target_staging_cap_mb(self) -> usize {
        match self {
            CountMode::Fast => 24,
            CountMode::Balanced => 8,
            CountMode::Compact => 4,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum CitationFormat {
    Text,
    Bibtex,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum CorrelationMode {
    All,
    Pearson,
    Cosine,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum EmbeddingMode {
    All,
    Pca,
    Tsne,
    Umap,
}

#[derive(Parser, Debug)]
#[command(
    name = "kite",
    about = "High-performance k-mer database toolkit",
    version,
    disable_version_flag = true
)]
pub struct Cli {
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    _version: Option<bool>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Count(CountArgs),
    Dump(DumpArgs),
    Info(InfoArgs),
    Query(QueryArgs),
    Histo(HistoArgs),
    Merge(MergeArgs),
    Profile(ProfileArgs),
    Analyze(AnalyzeArgs),
    Citation(CitationArgs),
    Fly(FlyArgs),
}

#[derive(Args, Debug)]
pub struct CountArgs {
    #[arg(short = 'k', long = "kmer", default_value_t = 21)]
    pub k: u8,
    #[arg(short = 'i', long = "input")]
    pub input: String,
    #[arg(short = 'o', long = "output")]
    pub output: String,
    #[arg(short = 't', long = "threads", default_value_t = 0)]
    pub threads: usize,
    #[arg(long = "partitions", default_value_t = 0)]
    pub partitions: usize,
    #[arg(long = "chunk-kmers", default_value_t = 8_000_000)]
    pub chunk_kmers: usize,
    #[arg(long = "memory", value_name = "SIZE")]
    pub memory: Option<String>,
    #[arg(long = "canonical", default_value_t = true)]
    pub canonical: bool,
    #[arg(short = 'p', long = "progress")]
    pub progress: bool,
    #[arg(long = "test-mode")]
    pub test_mode: bool,
    #[arg(long = "mode", value_enum, default_value_t = CountMode::Fast)]
    pub mode: CountMode,
    #[arg(long = "buffer", default_value_t = 16, value_name = "MB")]
    pub buffer: usize,
}

#[derive(Args, Debug)]
pub struct DumpArgs {
    #[arg(short = 'i', long = "input")]
    pub input: String,
    #[arg(short = 'o', long = "output")]
    pub output: String,
    #[arg(short = 't', long = "threads", default_value_t = 0)]
    pub threads: usize,
    #[arg(short = 'p', long = "progress")]
    pub progress: bool,
    #[arg(long = "test-mode")]
    pub test_mode: bool,
}

#[derive(Args, Debug)]
pub struct InfoArgs {
    #[arg(value_name = "INPUT")]
    pub input: String,
    #[arg(long = "test-mode")]
    pub test_mode: bool,
}

#[derive(Args, Debug)]
pub struct QueryArgs {
    #[arg(value_name = "INPUT")]
    pub input: String,
    #[arg(value_name = "KMER")]
    pub kmer: String,
    #[arg(long = "test-mode")]
    pub test_mode: bool,
}

#[derive(Args, Debug)]
pub struct HistoArgs {
    #[arg(short = 'i', long = "input", required = true, num_args = 1..)]
    pub inputs: Vec<String>,
    #[arg(short = 'o', long = "output")]
    pub output: String,
    #[arg(long = "label", num_args = 1.., action = ArgAction::Set)]
    pub labels: Option<Vec<String>>,
    #[arg(short = 'p', long = "progress")]
    pub progress: bool,
    #[arg(long = "test-mode")]
    pub test_mode: bool,
}

#[derive(Args, Debug)]
pub struct MergeArgs {
    #[arg(short = 'i', long = "input", required = true, num_args = 2..)]
    pub inputs: Vec<String>,
    #[arg(short = 'o', long = "output")]
    pub output: String,
    #[arg(short = 'p', long = "progress")]
    pub progress: bool,
    #[arg(long = "test-mode")]
    pub test_mode: bool,
}

#[derive(Args, Debug)]
pub struct ProfileArgs {
    #[arg(short = 'i', long = "input", required = true, num_args = 1..)]
    pub inputs: Vec<String>,
    #[arg(short = 'o', long = "output")]
    pub output: String,
    #[arg(long = "label", num_args = 1.., action = ArgAction::Set)]
    pub labels: Option<Vec<String>>,
    #[arg(short = 'p', long = "progress")]
    pub progress: bool,
    #[arg(long = "test-mode")]
    pub test_mode: bool,
}

#[derive(Args, Debug, Clone)]
pub struct AnalyzeArgs {
    #[arg(short = 'i', long = "input", required = true, num_args = 1..)]
    pub inputs: Vec<String>,
    #[arg(short = 'o', long = "output-dir")]
    pub output_dir: String,
    #[arg(long = "label", num_args = 1.., action = ArgAction::Set)]
    pub labels: Option<Vec<String>>,
    #[arg(long = "top-variable-kmers", default_value_t = 5000)]
    pub top_variable_kmers: usize,
    #[arg(long = "correlation", value_enum, default_value_t = CorrelationMode::All)]
    pub correlation: CorrelationMode,
    #[arg(long = "embedding", value_enum, default_value_t = EmbeddingMode::All)]
    pub embedding: EmbeddingMode,
    #[arg(long = "heatmap-kmeans", default_value_t = 6)]
    pub heatmap_kmeans: usize,
    #[arg(long = "heatmap-seed", default_value_t = 1)]
    pub heatmap_seed: u64,
    #[arg(
        long = "cluster-samples",
        default_value_t = true,
        action = ArgAction::Set,
        value_name = "BOOL"
    )]
    pub cluster_samples: bool,
    #[arg(short = 'p', long = "progress")]
    pub progress: bool,
    #[arg(long = "test-mode")]
    pub test_mode: bool,
}

#[derive(Args, Debug)]
pub struct CitationArgs {
    #[arg(short = 'o', long = "output", default_value = "-")]
    pub output: String,
    #[arg(long = "format", value_enum, default_value_t = CitationFormat::Text)]
    pub format: CitationFormat,
    #[arg(long = "test-mode")]
    pub test_mode: bool,
}

#[derive(Args, Debug)]
pub struct FlyArgs {
    #[arg(short = 'i', long = "input", required = true, num_args = 1..)]
    pub inputs: Vec<String>,
    #[arg(short = 'o', long = "output-dir")]
    pub output_dir: String,
    #[arg(long = "label", num_args = 1.., action = ArgAction::Set)]
    pub labels: Option<Vec<String>>,
    #[arg(short = 'k', long = "kmer", default_value_t = 31)]
    pub k: u8,
    #[arg(short = 't', long = "threads", default_value_t = 0)]
    pub threads: usize,
    #[arg(long = "memory", value_name = "SIZE")]
    pub memory: Option<String>,
    #[arg(long = "top-variable-kmers", default_value_t = 5000)]
    pub top_variable_kmers: usize,
    #[arg(long = "correlation", value_enum, default_value_t = CorrelationMode::All)]
    pub correlation: CorrelationMode,
    #[arg(long = "embedding", value_enum, default_value_t = EmbeddingMode::All)]
    pub embedding: EmbeddingMode,
    #[arg(long = "heatmap-kmeans", default_value_t = 6)]
    pub heatmap_kmeans: usize,
    #[arg(long = "heatmap-seed", default_value_t = 1)]
    pub heatmap_seed: u64,
    #[arg(
        long = "cluster-samples",
        default_value_t = true,
        action = ArgAction::Set,
        value_name = "BOOL"
    )]
    pub cluster_samples: bool,
    #[arg(short = 'p', long = "progress")]
    pub progress: bool,
    #[arg(long = "test-mode")]
    pub test_mode: bool,
}
