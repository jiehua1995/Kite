use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

// CountMode
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
            CountMode::Fast => 0,
            CountMode::Balanced => 2,
            CountMode::Compact => 5,
        }
    }
}

// Citation
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum CitationFormat {
    Text,
    Bibtex,
}

// Correlation
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum CorrelationMode {
    All,
    Pearson,
    Spearman,
    Kendall,
    Cosine,
    Euclidean,
    Manhattan,
    Mahalanobis,
}

// Embedding
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum EmbeddingMode {
    All,
    Pca,
    Tsne,
    Umap,
    #[value(alias = "pcamap")]
    Pacmap,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum EmbeddingInputMode {
    Full,
    Pca,
}

#[derive(Parser, Debug)]
#[command(
    name = "kite",
    author,
    about = "k-mer indexing and throughput engine",
    version,
    disable_version_flag = true,
    color = clap::ColorChoice::Never,
    help_template = "\
{name}:  {about}

Version: {version}
Author:  {author}

Usage:   {usage}

{all-args}
"
)]
pub struct Cli {
    #[arg(
        short = 'v',
        long = "version",
        action = ArgAction::Version,
        help = "Print version information"
    )]
    _version: Option<bool>,

    #[command(subcommand)]
    pub command: Commands,
}

// Commands
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Count k-mers from FASTA/FASTQ input and write a .kite index.
    Count(CountArgs),

    /// Dump k-mer counts from a .kite file into a table.
    Dump(DumpArgs),

    /// Show metadata and summary information for a .kite file.
    Info(InfoArgs),

    /// Query specific k-mer from a .kite file.
    Query(QueryArgs),

    /// Generate a k-mer count histogram table.
    Histo(HistoArgs),

    /// Merge multiple .kite files into one output file.
    Merge(MergeArgs),

    /// Profile k-mer content across samples into a table.
    Profile(ProfileArgs),

    /// Analyze multiple .kite files and write TSV tables plus a Quarto report.
    Analyze(AnalyzeArgs),

    /// Print citation information for Kite.
    Citation(CitationArgs),

    /// Run the whole workflow in a single command.
    Fly(FlyArgs),

    /// Hidden easter egg.
    #[command(hide = true)]
    Wind(WindArgs),
}

// All subcommands and their arguments

#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  kite count -i reads.fq.gz -o reads.kite\n  kite count -i reads.fq.gz -o reads.kite -k 31 -t 32 --mode fast\n  kite count -i reads.fq.gz -o reads.kite --canonical true\n  kite count -i reads.fq.gz -o reads.kite --memory 32G --mode compact",
    help_template = "{about}\n\nUsage: {usage}\n\n{all-args}\n{after-help}",
    disable_help_flag = true
)]
pub struct CountArgs {
    /// Input FASTA/FASTQ file.
    #[arg(
        short = 'i',
        long = "input",
        value_name = "FILE",
        help_heading = "Required"
    )]
    pub input: String,

    /// Output .kite file.
    #[arg(
        short = 'o',
        long = "output",
        value_name = "FILE",
        help_heading = "Required"
    )]
    pub output: String,

    /// Length of k-mers to count.
    #[arg(
        short = 'k',
        long = "kmer",
        default_value_t = 21,
        value_name = "k",
        help_heading = "k-mer"
    )]
    pub k: u8,

    /// Whether to canonicalize k-mers. Set to true or false.
    #[arg(long = "canonical", action = ArgAction::Set, default_value_t = false, value_name = "BOOL", help_heading = "k-mer")]
    pub canonical: bool,

    /// Store a header marker that downstream commands use to hide counts below this value.
    #[arg(long = "min", value_name = "N", help_heading = "k-mer")]
    pub min: Option<u32>,

    /// Store a header marker that downstream commands use to hide counts above this value.
    #[arg(long = "max", value_name = "N", help_heading = "k-mer")]
    pub max: Option<u32>,

    /// Output compression mode.
    #[arg(long = "mode", value_enum, default_value_t = CountMode::Fast, value_name = "MODE", help_heading = "Performance")]
    pub mode: CountMode,

    /// Number of threads to use. Use 0 to auto-detect.
    #[arg(
        short = 't',
        long = "threads",
        default_value_t = 0,
        value_name = "N",
        help_heading = "Performance"
    )]
    pub threads: usize,

    /// Number of partitions. Use 0 to auto-select.
    #[arg(
        long = "partitions",
        default_value_t = 0,
        value_name = "N",
        help_heading = "Performance"
    )]
    pub partitions: usize,

    /// Number of k-mers per chunk.
    #[arg(
        long = "chunk-kmers",
        default_value_t = 8_000_000,
        value_name = "N",
        help_heading = "Performance"
    )]
    pub chunk_kmers: usize,

    /// Memory limit, for example 8G, 16000M, or 500000000. If omitted, count auto-uses about 2/3 of currently available memory.
    #[arg(long = "memory", value_name = "SIZE", help_heading = "Performance")]
    pub memory: Option<String>,

    /// Staging buffer target in MB. Independent of --mode.
    #[arg(
        long = "buffer",
        default_value_t = 32,
        value_name = "MB",
        help_heading = "Performance"
    )]
    pub buffer: usize,

    /// Show progress.
    #[arg(short = 'p', long = "progress", help_heading = "Behavior")]
    pub progress: bool,

    /// Run in test mode.
    #[arg(long = "test-mode", default_value_t = false, help_heading = "Behavior")]
    pub test_mode: bool,

    /// Print help information for `kite count`.
    #[arg(short = 'h', long = "help", action = ArgAction::Help, help_heading = "Help")]
    pub _help: Option<bool>,
}

/// Dump k-mer counts from a .kite file into a table.
#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  kite dump -i reads.kite -o reads.tsv",
    help_template = "{about}\n\nUsage: {usage}\n\n{all-args}\n{after-help}",
    disable_help_flag = true
)]
pub struct DumpArgs {
    /// Input .kite file.
    #[arg(
        short = 'i',
        long = "input",
        value_name = "FILE",
        help_heading = "Required"
    )]
    pub input: String,

    /// Output table.
    #[arg(
        short = 'o',
        long = "output",
        value_name = "FILE",
        help_heading = "Required"
    )]
    pub output: String,

    /// Threads to be used for dumping. Use 0 to auto-detect.
    #[arg(
        short = 't',
        long = "threads",
        default_value_t = 0,
        value_name = "N",
        help_heading = "Performance"
    )]
    pub threads: usize,

    /// Show progress.
    #[arg(short = 'p', long = "progress", help_heading = "Behavior")]
    pub progress: bool,

    /// Run in test mode.
    #[arg(long = "test-mode", default_value_t = false, help_heading = "Behavior")]
    pub test_mode: bool,

    /// Print help information for `kite dump`.
    #[arg(short = 'h', long = "help", action = ArgAction::Help, help_heading = "Help")]
    pub _help: Option<bool>,
}

/// Show metadata and summary information for a .kite file.
#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  kite info -i sample.kite\n  kite info -i sample.kite --set-min 2 --set-max 100\n  kite info -i sample.kite --clear-min --clear-max",
    help_template = "{about}\n\nUsage: {usage}\n\n{all-args}\n{after-help}",
    disable_help_flag = true
)]
pub struct InfoArgs {
    /// Input .kite file.
    #[arg(
        short = 'i',
        long = "input",
        value_name = "FILE",
        help_heading = "Required"
    )]
    pub input: String,

    /// Set the header minimum count marker used by downstream commands.
    #[arg(long = "set-min", value_name = "N", help_heading = "Edit")]
    pub set_min: Option<u32>,

    /// Set the header maximum count marker used by downstream commands.
    #[arg(long = "set-max", value_name = "N", help_heading = "Edit")]
    pub set_max: Option<u32>,

    /// Clear the header minimum count marker.
    #[arg(long = "clear-min", default_value_t = false, help_heading = "Edit")]
    pub clear_min: bool,

    /// Clear the header maximum count marker.
    #[arg(long = "clear-max", default_value_t = false, help_heading = "Edit")]
    pub clear_max: bool,

    /// Run in test mode.
    #[arg(long = "test-mode", default_value_t = false, help_heading = "Behavior")]
    pub test_mode: bool,

    /// Print help information for `kite info`.
    #[arg(short = 'h', long = "help", action = ArgAction::Help, help_heading = "Help")]
    pub _help: Option<bool>,
}

/// Query specific k-mer from a .kite file.
#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  kite query -i sample.kite -k ACGT",
    help_template = "{about}\n\nUsage: {usage}\n\n{all-args}\n{after-help}",
    disable_help_flag = true
)]
pub struct QueryArgs {
    /// Input .kite file.
    #[arg(
        short = 'i',
        long = "input",
        value_name = "FILE",
        help_heading = "Required"
    )]
    pub input: String,

    /// K-mer to query.
    #[arg(
        short = 'k',
        long = "kmer",
        value_name = "k-mer",
        help_heading = "Required"
    )]
    pub kmer: String,

    /// Run in test mode.
    #[arg(long = "test-mode", default_value_t = false, help_heading = "Behavior")]
    pub test_mode: bool,

    /// Print help information for `kite query`.
    #[arg(short = 'h', long = "help", action = ArgAction::Help, help_heading = "Help")]
    pub _help: Option<bool>,
}

/// Generate a k-mer count histogram table.
#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  kite histo -i a.kite b.kite -o histo.tsv",
    help_template = "{about}\n\nUsage: {usage}\n\n{all-args}\n{after-help}",
    disable_help_flag = true
)]
pub struct HistoArgs {
    /// Input .kite files. Pass all files after one -i/--input.
    #[arg(short = 'i', long = "input", required = true, num_args = 1.., value_name = "FILE", help_heading = "Required")]
    pub inputs: Vec<String>,

    /// Output histogram table.
    #[arg(
        short = 'o',
        long = "output",
        value_name = "FILE",
        help_heading = "Required"
    )]
    pub output: String,

    /// Labels for the input files. Defaults to file names without common suffixes.
    #[arg(long = "label", num_args = 1.., action = ArgAction::Set, value_name = "LABEL", help_heading = "Behavior")]
    pub labels: Option<Vec<String>>,

    /// Number of threads to use. Use 0 to auto-detect.
    #[arg(
        short = 't',
        long = "threads",
        default_value_t = 0,
        value_name = "N",
        help_heading = "Performance"
    )]
    pub threads: usize,

    /// Show progress.
    #[arg(short = 'p', long = "progress", help_heading = "Behavior")]
    pub progress: bool,

    /// Run in test mode.
    #[arg(long = "test-mode", default_value_t = false, help_heading = "Behavior")]
    pub test_mode: bool,

    /// Print help information for `kite histo`.
    #[arg(short = 'h', long = "help", action = ArgAction::Help, help_heading = "Help")]
    pub _help: Option<bool>,
}

/// Merge multiple .kite files into one output file.
#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  kite merge -i a.kite b.kite -o merged.kite",
    help_template = "{about}\n\nUsage: {usage}\n\n{all-args}\n{after-help}",
    disable_help_flag = true
)]
pub struct MergeArgs {
    /// Input .kite files. Pass all files after one -i/--input.
    #[arg(short = 'i', long = "input", required = true, num_args = 2.., value_name = "FILE", help_heading = "Required")]
    pub inputs: Vec<String>,

    /// Output .kite file.
    #[arg(
        short = 'o',
        long = "output",
        value_name = "FILE",
        help_heading = "Required"
    )]
    pub output: String,

    /// Number of threads to use. Use 0 to auto-detect.
    #[arg(
        short = 't',
        long = "threads",
        default_value_t = 0,
        value_name = "N",
        help_heading = "Performance"
    )]
    pub threads: usize,

    /// Show progress.
    #[arg(short = 'p', long = "progress", help_heading = "Behavior")]
    pub progress: bool,

    /// Run in test mode.
    #[arg(long = "test-mode", default_value_t = false, help_heading = "Behavior")]
    pub test_mode: bool,

    /// Print help information for `kite merge`.
    #[arg(short = 'h', long = "help", action = ArgAction::Help, help_heading = "Help")]
    pub _help: Option<bool>,
}

/// Profile k-mer content across samples into a table.
#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  kite profile -i a.kite b.kite -o profile.tsv",
    help_template = "{about}\n\nUsage: {usage}\n\n{all-args}\n{after-help}",
    disable_help_flag = true
)]
pub struct ProfileArgs {
    /// Input .kite files. Pass all files after one -i/--input.
    #[arg(short = 'i', long = "input", required = true, num_args = 1.., value_name = "FILE", help_heading = "Required")]
    pub inputs: Vec<String>,

    /// Output profile table.
    #[arg(
        short = 'o',
        long = "output",
        value_name = "FILE",
        help_heading = "Required"
    )]
    pub output: String,

    /// Labels for the input files. Defaults to file names without common suffixes.
    #[arg(long = "label", num_args = 1.., action = ArgAction::Set, value_name = "LABEL", help_heading = "Behavior")]
    pub labels: Option<Vec<String>>,

    /// Number of threads to use. Use 0 to auto-detect.
    #[arg(
        short = 't',
        long = "threads",
        default_value_t = 0,
        value_name = "N",
        help_heading = "Performance"
    )]
    pub threads: usize,

    /// Show progress.
    #[arg(short = 'p', long = "progress", help_heading = "Behavior")]
    pub progress: bool,

    /// Run in test mode.
    #[arg(long = "test-mode", default_value_t = false, help_heading = "Behavior")]
    pub test_mode: bool,

    /// Print help information for `kite profile`.
    #[arg(short = 'h', long = "help", action = ArgAction::Help, help_heading = "Help")]
    pub _help: Option<bool>,
}

/// Analyze multiple .kite files and write TSV tables plus a Quarto report.
#[derive(Args, Debug, Clone)]
#[command(
    after_help = "Examples:\n  kite analyze -i sample1.kite sample2.kite -o analysis_dir\n  kite analyze -i sample1.kite sample2.kite -o analysis_dir --embedding all --embedding-input pca\n  kite analyze -i sample1.kite sample2.kite -o analysis_dir --embedding all --embedding-input full\n\nNotes:\n  Non-linear embeddings default to `--embedding-input pca` for speed.\n  Use `--embedding-input full` when you want the original full k-mer feature matrix instead.\n  Background: scikit-learn TSNE recommends PCA/SVD before t-SNE for very high-dimensional features:\n    https://scikit-learn.org/stable/modules/generated/sklearn.manifold.TSNE.html\n  PaCMAP docs also note that PCA preprocessing can substantially accelerate DR:\n    https://github.com/YingfanWang/PaCMAP",
    help_template = "{about}\n\nUsage: {usage}\n\n{all-args}\n{after-help}",
    disable_help_flag = true
)]
pub struct AnalyzeArgs {
    /// Input .kite files. Pass all files after one -i/--input.
    #[arg(short = 'i', long = "input", required = true, num_args = 1.., value_name = "FILE", help_heading = "Required")]
    pub inputs: Vec<String>,

    /// Output analysis directory.
    #[arg(
        short = 'o',
        long = "output-dir",
        value_name = "DIR",
        help_heading = "Required"
    )]
    pub output_dir: String,

    /// Labels for the input files. Defaults to file names without common suffixes.
    #[arg(long = "label", num_args = 1.., action = ArgAction::Set, value_name = "LABEL", help_heading = "Behavior")]
    pub labels: Option<Vec<String>>,

    /// Correlation mode for analysis.
    #[arg(long = "correlation", value_enum, default_value_t = CorrelationMode::Pearson, value_name = "MODE", help_heading = "Analysis")]
    pub correlation: CorrelationMode,

    /// Embedding mode for analysis.
    #[arg(long = "embedding", value_enum, default_value_t = EmbeddingMode::Pca, value_name = "MODE", help_heading = "Analysis")]
    pub embedding: EmbeddingMode,

    /// Input representation for non-linear embeddings. `pca` is the default speed-oriented path; `full` keeps the original full k-mer feature matrix.
    #[arg(long = "embedding-input", value_enum, default_value_t = EmbeddingInputMode::Pca, value_name = "MODE", help_heading = "Analysis")]
    pub embedding_input: EmbeddingInputMode,

    /// Number of PCA dimensions to keep when `--embedding-input pca` is used.
    #[arg(
        long = "embedding-input-dims",
        default_value_t = 50,
        value_name = "N",
        help_heading = "Analysis"
    )]
    pub embedding_input_dims: usize,

    /// Number of clusters for heatmap k-means.
    #[arg(
        long = "heatmap-kmeans",
        default_value_t = 6,
        value_name = "N",
        help_heading = "Analysis"
    )]
    pub heatmap_kmeans: usize,

    /// Seed for stochastic analysis methods. `--heatmap-seed` is kept as a compatibility alias.
    #[arg(
        long = "seed",
        visible_alias = "heatmap-seed",
        default_value_t = 42,
        value_name = "N",
        help_heading = "Analysis"
    )]
    pub seed: u64,

    /// Whether to cluster samples in heatmaps.
    #[arg(
        long = "cluster-samples",
        default_value_t = true,
        action = ArgAction::Set,
        value_name = "BOOL",
        help_heading = "Analysis"
    )]
    pub cluster_samples: bool,

    /// t-SNE perplexity. When omitted, Kite derives it from the sample count.
    #[arg(
        long = "tsne-perplexity",
        value_name = "FLOAT",
        help_heading = "Analysis"
    )]
    pub tsne_perplexity: Option<f64>,

    /// t-SNE learning rate.
    #[arg(
        long = "tsne-learning-rate",
        default_value_t = 200.0,
        value_name = "FLOAT",
        help_heading = "Analysis"
    )]
    pub tsne_learning_rate: f64,

    /// t-SNE iterations.
    #[arg(
        long = "tsne-iterations",
        default_value_t = 750,
        value_name = "N",
        help_heading = "Analysis"
    )]
    pub tsne_iterations: usize,

    /// UMAP neighbors. When omitted, Kite derives it from the sample count.
    #[arg(long = "umap-neighbors", value_name = "N", help_heading = "Analysis")]
    pub umap_neighbors: Option<usize>,

    /// UMAP min_dist.
    #[arg(
        long = "umap-min-dist",
        default_value_t = 0.1,
        value_name = "FLOAT",
        help_heading = "Analysis"
    )]
    pub umap_min_dist: f64,

    /// UMAP spread.
    #[arg(
        long = "umap-spread",
        default_value_t = 1.0,
        value_name = "FLOAT",
        help_heading = "Analysis"
    )]
    pub umap_spread: f64,

    /// PaCMAP neighbors. When omitted, Kite derives it from the sample count.
    #[arg(long = "pacmap-neighbors", value_name = "N", help_heading = "Analysis")]
    pub pacmap_neighbors: Option<usize>,

    /// PaCMAP mid-near end. When omitted, Kite derives it from the sample count.
    #[arg(long = "pacmap-mn-end", value_name = "N", help_heading = "Analysis")]
    pub pacmap_mn_end: Option<usize>,

    /// Number of threads to use. Use 0 to auto-detect.
    #[arg(
        short = 't',
        long = "threads",
        default_value_t = 0,
        value_name = "N",
        help_heading = "Performance"
    )]
    pub threads: usize,

    /// Show progress.
    #[arg(short = 'p', long = "progress", help_heading = "Behavior")]
    pub progress: bool,

    /// Run in test mode.
    #[arg(long = "test-mode", default_value_t = false, help_heading = "Behavior")]
    pub test_mode: bool,

    /// Print help information for `kite analyze`.
    #[arg(short = 'h', long = "help", action = ArgAction::Help, help_heading = "Help")]
    pub _help: Option<bool>,
}

/// Print citation information for Kite.
#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  kite citation -o citation.txt",
    help_template = "{about}\n\nUsage: {usage}\n\n{all-args}\n{after-help}",
    disable_help_flag = true
)]
pub struct CitationArgs {
    /// Output file. Use - for stdout.
    #[arg(
        short = 'o',
        long = "output",
        default_value = "-",
        value_name = "FILE",
        help_heading = "Output"
    )]
    pub output: String,

    /// Citation format.
    #[arg(long = "format", value_enum, default_value_t = CitationFormat::Text, value_name = "FORMAT", help_heading = "Output")]
    pub format: CitationFormat,

    /// Run in test mode.
    #[arg(long = "test-mode", default_value_t = false, help_heading = "Behavior")]
    pub test_mode: bool,

    /// Print help information for `kite citation`.
    #[arg(short = 'h', long = "help", action = ArgAction::Help, help_heading = "Help")]
    pub _help: Option<bool>,
}

/// Run the whole workflow in a single command.
#[derive(Args, Debug)]
#[command(
    after_help = "Examples:\n  kite fly -i reads1.fq reads2.fq -o out_dir\n  kite fly -i reads1.fq reads2.fq -o out_dir --canonical true\n  kite fly -i reads1.fq reads2.fq -o out_dir --embedding all --embedding-input pca\n  kite fly -i reads1.fq reads2.fq -o out_dir --embedding all --embedding-input full\n\nNotes:\n  Non-linear embeddings default to `--embedding-input pca` for speed.\n  Use `--embedding-input full` when you want the original full k-mer feature matrix instead.\n  Background: scikit-learn TSNE recommends PCA/SVD before t-SNE for very high-dimensional features:\n    https://scikit-learn.org/stable/modules/generated/sklearn.manifold.TSNE.html\n  PaCMAP docs also note that PCA preprocessing can substantially accelerate DR:\n    https://github.com/YingfanWang/PaCMAP",
    help_template = "{about}\n\nUsage: {usage}\n\n{all-args}\n{after-help}",
    disable_help_flag = true
)]
pub struct FlyArgs {
    /// Input FASTA/FASTQ files. Pass all files after one -i/--input.
    #[arg(short = 'i', long = "input", required = true, num_args = 1.., value_name = "FILE", help_heading = "Required")]
    pub inputs: Vec<String>,

    /// Output workflow directory.
    #[arg(
        short = 'o',
        long = "output-dir",
        value_name = "DIR",
        help_heading = "Required"
    )]
    pub output_dir: String,

    /// Labels for the input files. Defaults to file names without common suffixes.
    #[arg(long = "label", num_args = 1.., action = ArgAction::Set, value_name = "LABEL", help_heading = "Behavior")]
    pub labels: Option<Vec<String>>,

    /// Length of k-mers to count.
    #[arg(
        short = 'k',
        long = "kmer",
        default_value_t = 31,
        value_name = "k",
        help_heading = "k-mer"
    )]
    pub k: u8,

    /// Whether to canonicalize k-mers. Set to true or false.
    #[arg(long = "canonical", action = ArgAction::Set, default_value_t = false, value_name = "BOOL", help_heading = "k-mer")]
    pub canonical: bool,

    /// Store a header marker that downstream commands use to hide counts below this value.
    #[arg(long = "min", value_name = "N", help_heading = "k-mer")]
    pub min: Option<u32>,

    /// Store a header marker that downstream commands use to hide counts above this value.
    #[arg(long = "max", value_name = "N", help_heading = "k-mer")]
    pub max: Option<u32>,

    /// Output compression mode used by the count stage.
    #[arg(long = "mode", value_enum, default_value_t = CountMode::Fast, value_name = "MODE", help_heading = "Performance")]
    pub mode: CountMode,

    /// Number of threads to use. Use 0 to auto-detect.
    #[arg(
        short = 't',
        long = "threads",
        default_value_t = 0,
        value_name = "N",
        help_heading = "Performance"
    )]
    pub threads: usize,

    /// Memory limit, for example 8G, 16000M, or 500000000.
    #[arg(long = "memory", value_name = "SIZE", help_heading = "Performance")]
    pub memory: Option<String>,

    /// Number of partitions for the count stage. Use 0 to auto-select.
    #[arg(
        long = "partitions",
        default_value_t = 0,
        value_name = "N",
        help_heading = "Performance"
    )]
    pub partitions: usize,

    /// Number of k-mers per count chunk.
    #[arg(
        long = "chunk-kmers",
        default_value_t = 8_000_000,
        value_name = "N",
        help_heading = "Performance"
    )]
    pub chunk_kmers: usize,

    /// Staging buffer target in MB for the count stage.
    #[arg(
        long = "buffer",
        default_value_t = 32,
        value_name = "MB",
        help_heading = "Performance"
    )]
    pub buffer: usize,

    /// Correlation mode for analysis.
    #[arg(long = "correlation", value_enum, default_value_t = CorrelationMode::Pearson, value_name = "MODE", help_heading = "Analysis")]
    pub correlation: CorrelationMode,

    /// Embedding mode for analysis.
    #[arg(long = "embedding", value_enum, default_value_t = EmbeddingMode::Pca, value_name = "MODE", help_heading = "Analysis")]
    pub embedding: EmbeddingMode,

    /// Input representation for non-linear embeddings. `pca` is the default speed-oriented path; `full` keeps the original full k-mer feature matrix.
    #[arg(long = "embedding-input", value_enum, default_value_t = EmbeddingInputMode::Pca, value_name = "MODE", help_heading = "Analysis")]
    pub embedding_input: EmbeddingInputMode,

    /// Number of PCA dimensions to keep when `--embedding-input pca` is used.
    #[arg(
        long = "embedding-input-dims",
        default_value_t = 50,
        value_name = "N",
        help_heading = "Analysis"
    )]
    pub embedding_input_dims: usize,

    /// Number of clusters for heatmap k-means.
    #[arg(
        long = "heatmap-kmeans",
        default_value_t = 6,
        value_name = "N",
        help_heading = "Analysis"
    )]
    pub heatmap_kmeans: usize,

    /// Seed for stochastic analysis methods. `--heatmap-seed` is kept as a compatibility alias.
    #[arg(
        long = "seed",
        visible_alias = "heatmap-seed",
        default_value_t = 42,
        value_name = "N",
        help_heading = "Analysis"
    )]
    pub seed: u64,

    /// Whether to cluster samples in heatmaps.
    #[arg(
        long = "cluster-samples",
        default_value_t = true,
        action = ArgAction::Set,
        value_name = "BOOL",
        help_heading = "Analysis"
    )]
    pub cluster_samples: bool,

    /// t-SNE perplexity. When omitted, Kite derives it from the sample count.
    #[arg(
        long = "tsne-perplexity",
        value_name = "FLOAT",
        help_heading = "Analysis"
    )]
    pub tsne_perplexity: Option<f64>,

    /// t-SNE learning rate.
    #[arg(
        long = "tsne-learning-rate",
        default_value_t = 200.0,
        value_name = "FLOAT",
        help_heading = "Analysis"
    )]
    pub tsne_learning_rate: f64,

    /// t-SNE iterations.
    #[arg(
        long = "tsne-iterations",
        default_value_t = 750,
        value_name = "N",
        help_heading = "Analysis"
    )]
    pub tsne_iterations: usize,

    /// UMAP neighbors. When omitted, Kite derives it from the sample count.
    #[arg(long = "umap-neighbors", value_name = "N", help_heading = "Analysis")]
    pub umap_neighbors: Option<usize>,

    /// UMAP min_dist.
    #[arg(
        long = "umap-min-dist",
        default_value_t = 0.1,
        value_name = "FLOAT",
        help_heading = "Analysis"
    )]
    pub umap_min_dist: f64,

    /// UMAP spread.
    #[arg(
        long = "umap-spread",
        default_value_t = 1.0,
        value_name = "FLOAT",
        help_heading = "Analysis"
    )]
    pub umap_spread: f64,

    /// PaCMAP neighbors. When omitted, Kite derives it from the sample count.
    #[arg(long = "pacmap-neighbors", value_name = "N", help_heading = "Analysis")]
    pub pacmap_neighbors: Option<usize>,

    /// PaCMAP mid-near end. When omitted, Kite derives it from the sample count.
    #[arg(long = "pacmap-mn-end", value_name = "N", help_heading = "Analysis")]
    pub pacmap_mn_end: Option<usize>,

    /// Keep intermediate .kite databases after analysis.
    #[arg(
        long = "keep-intermediate",
        default_value_t = true,
        action = ArgAction::Set,
        value_name = "BOOL",
        help_heading = "Behavior"
    )]
    pub keep_intermediate: bool,

    /// Show progress.
    #[arg(short = 'p', long = "progress", help_heading = "Behavior")]
    pub progress: bool,

    /// Run in test mode.
    #[arg(long = "test-mode", default_value_t = false, help_heading = "Behavior")]
    pub test_mode: bool,

    /// Print help information for `kite fly`.
    #[arg(short = 'h', long = "help", action = ArgAction::Help, help_heading = "Help")]
    pub _help: Option<bool>,
}

#[derive(Args, Debug)]
#[command(
    help_template = "{about}\n\nUsage: {usage}\n",
    disable_help_flag = true
)]
pub struct WindArgs {
    /// Print help information for `kite wind`.
    #[arg(short = 'h', long = "help", action = ArgAction::Help, hide = true)]
    pub _help: Option<bool>,
}
