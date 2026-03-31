use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// 中文：`count` 写库模式枚举。它决定压缩强度与写出速度的平衡。
/// English: Write-mode enum for `count`. It controls the trade-off between
/// compression strength and write throughput.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum CountWriteMode {
    Fast,
    Balanced,
    Compact,
}

/// 中文：规范化 k-mer 的开关枚举，显式保留 `true/false` 方便 CLI 展示。
/// English: Canonical-k-mer switch enum. Using explicit `true/false` keeps
/// the CLI help text readable and stable.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum CanonicalMode {
    True,
    False,
}

/// 中文：`sketch` 可以输出的降维表类型。
/// English: Projection table kinds that `sketch` can emit.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum SketchProjection {
    Pca,
    Tsne,
    Umap,
    All,
}

/// 中文：样本间相似性/相关性计算方法。
/// English: Sample-to-sample association metric used by `sketch`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum CorrelationMethod {
    Pearson,
    Cosine,
}

/// 中文：顶层 CLI 定义，只保存用户选中的子命令。
/// English: Top-level CLI definition. It only stores the selected subcommand.
#[derive(Parser, Debug)]
#[command(
    name = "kite",
    version,
    about = "Kite: K-mer Indexing and Throughput Engine"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// 中文：所有子命令的分发枚举。
/// English: Dispatch enum containing every supported subcommand.
#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(
        alias = "build",
        about = "Count k-mers from FASTQ input and write a .kite database"
    )]
    Count(CountArgs),
    #[command(about = "Dump a .kite file to TSV (kmer\tcount)")]
    Dump(DumpArgs),
    #[command(about = "Show header/info and statistics for a .kite file")]
    Info(InfoArgs),
    #[command(about = "Query a k-mer count from a .kite file")]
    Query(QueryArgs),
    #[command(about = "Write histogram of counts from a .kite file to TSV")]
    Histo(HistoArgs),
    #[command(about = "Write a multi-sample raw-count matrix from one or more .kite files")]
    Profile(ProfileArgs),
    #[command(about = "Merge two or more .kite files by summing exact counts into one .kite file")]
    Merge(MergeArgs),
    #[command(about = "Create multi-sample correlation, projection, and heatmap outputs")]
    Sketch(SketchArgs),
    #[command(about = "Print version and citation information")]
    Citation,
}

/// 中文：`kite count` 的参数集合。输入是 FASTQ，输出是 `.kite` 二进制数据库。
/// English: Argument bundle for `kite count`. Input is FASTQ and output is a
/// `.kite` binary database.
#[derive(Args, Debug, Clone)]
pub struct CountArgs {
    #[arg(
        short = 'k',
        long = "kmer",
        value_name = "K",
        help = "k-mer length (required)",
        required = true
    )]
    pub k: usize,
    #[arg(
        short = 'i',
        long = "input",
        value_name = "INPUT",
        help = "Input FASTQ/FASTQ.GZ file (required)",
        required = true
    )]
    pub input: PathBuf,
    #[arg(
        short = 'o',
        long = "output",
        value_name = "OUTPUT",
        help = "Output kite binary file (required)",
        required = true
    )]
    pub output: PathBuf,
    #[arg(
        long = "partitions",
        value_name = "N",
        default_value_t = 256,
        hide_default_value = true,
        help = "Number of partitions. [Default: 256]"
    )]
    pub partitions: usize,
    #[arg(
        long = "threads",
        value_name = "N",
        default_value_t = 8,
        hide_default_value = true,
        help = "Number of worker threads. [Default: 8]"
    )]
    pub threads: usize,
    #[arg(
        long = "chunk-kmers",
        value_name = "N",
        default_value_t = 1_000_000,
        hide_default_value = true,
        help = "Chunk size in k-mers for sorting/merge. [Default: 1000000]"
    )]
    pub chunk_kmers: usize,
    #[arg(
        long = "max-memory-gb",
        value_name = "GB",
        help = "Optional memory budget in GB for adaptive count-side buffering (for example: 16 on a workstation, 128 or 192 on a large server)"
    )]
    pub max_memory_gb: Option<u64>,
    #[arg(long = "canonical", value_name = "true|false", default_value_t = CanonicalMode::True, hide_default_value = true, help = "Use canonical k-mers (true or false). [Default: true]", value_enum)]
    pub canonical: CanonicalMode,
    #[arg(short = 'p', long = "progress", value_name = "true|false", default_value_t = false, hide_default_value = true, help = "Show low-overhead progress (spinner). [Default: false]", value_parser = clap::value_parser!(bool))]
    pub progress: bool,
    #[arg(long = "baseline", value_name = "true|false", default_value_t = false, hide_default_value = true, help = "Run baseline (non-optimized) pipeline. [Default: false]", value_parser = clap::value_parser!(bool))]
    pub baseline: bool,
    #[arg(long = "mode", value_enum, default_value_t = CountWriteMode::Fast, hide_default_value = true, help = "Write mode: fast|balanced|compact. [Default: fast]")]
    pub mode: CountWriteMode,
    #[arg(
        long = "zstd-level",
        value_name = "LEVEL",
        help = "Zstd compression level (higher = better compression, slower counting). [Default: 1 (fast mode). Optional: 1-9]"
    )]
    pub zstd_level: Option<i32>,
    #[arg(long = "test-mode", default_value_t = false, hide = true, value_parser = clap::value_parser!(bool))]
    pub test_mode: bool,
}

/// 中文：`kite dump` 的参数集合。它把 `.kite` 解码成文本表格。
/// English: Argument bundle for `kite dump`. It decodes a `.kite` database
/// into a plain-text k-mer count table.
#[derive(Args, Debug, Clone)]
pub struct DumpArgs {
    #[arg(
        short = 'i',
        long = "input",
        value_name = "INPUT",
        help = "Input kite binary file (required)",
        required = true
    )]
    pub input: PathBuf,
    #[arg(
        short = 'o',
        long = "output",
        value_name = "OUTPUT",
        help = "Output TSV file (required)",
        required = true
    )]
    pub output: PathBuf,
    #[arg(
        short = 't',
        long = "threads",
        value_name = "N",
        default_value_t = 8,
        hide_default_value = true,
        help = "Number of worker threads for parallel dump. [Default: 8]"
    )]
    pub threads: usize,
    #[arg(short = 'p', long = "progress", value_name = "true|false", default_value_t = false, hide_default_value = true, help = "Show a lightweight progress spinner. [Default: false]", value_parser = clap::value_parser!(bool))]
    pub progress: bool,
}

/// 中文：`kite info` 的参数集合。它只读取并展示数据库信息。
/// English: Argument bundle for `kite info`. It only inspects and reports
/// database metadata/statistics.
#[derive(Args, Debug, Clone)]
pub struct InfoArgs {
    #[arg(
        short = 'i',
        long = "input",
        value_name = "INPUT",
        help = "Input kite binary file (required)",
        required = true
    )]
    pub input: PathBuf,
    #[arg(short = 'p', long = "progress", value_name = "true|false", default_value_t = false, hide_default_value = true, help = "Show a lightweight progress spinner. [Default: false]", value_parser = clap::value_parser!(bool))]
    pub progress: bool,
}

/// 中文：`kite query` 的参数集合。它查询单个 k-mer 的计数。
/// English: Argument bundle for `kite query`. It looks up the count of one
/// specific k-mer.
#[derive(Args, Debug, Clone)]
pub struct QueryArgs {
    #[arg(
        short = 'i',
        long = "input",
        value_name = "INPUT",
        help = "Input kite binary file (required)",
        required = true
    )]
    pub input: PathBuf,
    #[arg(
        long = "kmer",
        value_name = "KMER",
        help = "Query k-mer string (A/C/G/T) (required)",
        required = true
    )]
    pub kmer: String,
    #[arg(short = 'p', long = "progress", value_name = "true|false", default_value_t = false, hide_default_value = true, help = "Show a lightweight progress spinner. [Default: false]", value_parser = clap::value_parser!(bool))]
    pub progress: bool,
}

/// 中文：`kite histo` 的参数集合。它输出计数频率直方图。
/// English: Argument bundle for `kite histo`. It writes a count-frequency
/// histogram table.
#[derive(Args, Debug, Clone)]
pub struct HistoArgs {
    #[arg(
        short = 'i',
        long = "input",
        value_name = "INPUT",
        help = "Input kite binary file (required)",
        required = true
    )]
    pub input: PathBuf,
    #[arg(
        short = 'o',
        long = "output",
        value_name = "OUTPUT",
        help = "Output histogram TSV (required)",
        required = true
    )]
    pub output: PathBuf,
    #[arg(short = 'p', long = "progress", value_name = "true|false", default_value_t = false, hide_default_value = true, help = "Show a lightweight progress spinner. [Default: false]", value_parser = clap::value_parser!(bool))]
    pub progress: bool,
}

/// 中文：`kite profile` 的参数集合。它把多个 `.kite` 合成统一的 count 矩阵。
/// English: Argument bundle for `kite profile`. It combines multiple `.kite`
/// files into one aligned count matrix.
#[derive(Args, Debug, Clone)]
pub struct ProfileArgs {
    #[arg(short = 'i', long = "input", value_name = "INPUT", required = true, num_args = 1.., help = "Input kite binary files (specify one or more files after -i) (required)")]
    pub input: Vec<PathBuf>,
    #[arg(
        short = 'o',
        long = "output",
        value_name = "OUTPUT",
        help = "Output profile file (tsv/csv/txt)"
    )]
    pub output: PathBuf,
    #[arg(long = "sample-name", value_name = "NAME", num_args = 1.., help = "Optional sample names, in the same order as --input")]
    pub sample_names: Vec<String>,
    #[arg(
        long = "format",
        value_name = "FORMAT",
        default_value = "tsv",
        hide_default_value = true,
        help = "Output format: tsv|csv|txt. [Default: tsv]"
    )]
    pub format: String,
    #[arg(short = 'p', long = "progress", value_name = "true|false", default_value_t = false, hide_default_value = true, help = "Show a lightweight progress spinner. [Default: false]", value_parser = clap::value_parser!(bool))]
    pub progress: bool,
}

/// 中文：`kite sketch` 的参数集合。它基于多个 `.kite` 生成相关性、降维和热图输入。
/// English: Argument bundle for `kite sketch`. It builds correlation,
/// projection, and clustered heatmap outputs from multiple `.kite` files.
/// 中文：`kite merge` 的参数集合。它读取两个或更多 `.kite` 文件，
/// 逐个验证兼容性后按 k-mer 精确相加，写出一个新的 `.kite` 文件。
/// English: Argument bundle for `kite merge`. It reads two or more `.kite`
/// files, validates compatibility, and writes one merged `.kite` file with
/// exact summed counts.
#[derive(Args, Debug, Clone)]
#[command(about = "Merge two or more .kite files by summing exact counts into one .kite file")]
pub struct MergeArgs {
    #[arg(short = 'i', long = "input", value_name = "INPUT", required = true, num_args = 2.., help = "Input .kite files (specify two or more files after -i)")]
    pub input: Vec<PathBuf>,
    #[arg(
        short = 'o',
        long = "output",
        value_name = "OUTPUT",
        help = "Output merged .kite file (required)",
        required = true
    )]
    pub output: PathBuf,
    #[arg(
        long = "threads",
        value_name = "N",
        default_value_t = 8,
        hide_default_value = true,
        help = "Number of worker threads for output compression. [Default: 8]"
    )]
    pub threads: usize,
    #[arg(long = "mode", value_enum, default_value_t = CountWriteMode::Fast, hide_default_value = true, help = "Write mode for the merged file: fast|balanced|compact. [Default: fast]")]
    pub mode: CountWriteMode,
    #[arg(
        long = "zstd-level",
        value_name = "LEVEL",
        help = "Optional zstd compression level for the merged file"
    )]
    pub zstd_level: Option<i32>,
    #[arg(short = 'p', long = "progress", value_name = "true|false", default_value_t = false, hide_default_value = true, help = "Show a lightweight progress spinner. [Default: false]", value_parser = clap::value_parser!(bool))]
    pub progress: bool,
}

#[derive(Args, Debug, Clone)]
pub struct SketchArgs {
    #[arg(short = 'i', long = "input", value_name = "INPUT", required = true, num_args = 2.., help = "Input .kite files (specify two or more files after -i)")]
    pub input: Vec<PathBuf>,
    #[arg(long = "sample-name", value_name = "NAME", num_args = 1.., help = "Optional sample names, in the same order as --input")]
    pub sample_names: Vec<String>,
    #[arg(long = "sample-group", value_name = "GROUP", num_args = 1.., help = "Optional sample groups, in the same order as --input")]
    pub sample_groups: Vec<String>,
    #[arg(
        short = 'o',
        long = "output-dir",
        value_name = "DIR",
        default_value = "sketch_out",
        hide_default_value = true,
        help = "Output directory for sketch results. [Default: sketch_out]"
    )]
    pub output: PathBuf,
    #[arg(long = "correlation", value_enum, default_value_t = CorrelationMethod::Pearson, hide_default_value = true, help = "Sample-to-sample association metric: pearson|cosine. [Default: pearson]")]
    pub correlation: CorrelationMethod,
    #[arg(long = "projection", value_enum, default_value_t = SketchProjection::All, hide_default_value = true, help = "Projection tables to compute: pca|tsne|umap|all. [Default: all]")]
    pub projection: SketchProjection,
    #[arg(
        long = "heatmap-kmeans",
        value_name = "N",
        default_value_t = 6,
        hide_default_value = true,
        help = "Number of row k-means clusters for the clustered heatmap table. [Default: 6]"
    )]
    pub heatmap_kmeans: usize,
    #[arg(long = "cluster-samples", value_name = "true|false", default_value_t = true, hide_default_value = true, help = "Cluster sample columns before writing the heatmap table. [Default: true]", value_parser = clap::value_parser!(bool))]
    pub cluster_samples: bool,
    #[arg(short = 'p', long = "progress", value_name = "true|false", default_value_t = false, hide_default_value = true, help = "Show a lightweight progress spinner. [Default: false]", value_parser = clap::value_parser!(bool))]
    pub progress: bool,
}

// 中文：旧 bench 命令已经移除，多样本比较统一由 `sketch` 负责。
// English: The old bench command is gone; multi-sample comparison now lives
// entirely inside `sketch`.
