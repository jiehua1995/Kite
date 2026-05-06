use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use crate::cli::{AnalyzeArgs, CorrelationMode, EmbeddingMode};
use crate::kiteops::{
    ensure_compatible, for_each_union_row, infer_sample_labels, key_to_string,
    load_partition_counts, open_readers, print_partition_progress,
};
use crate::stats::print_simple_test_report;

#[derive(Clone)]
struct TopFeature {
    variance: f64,
    kmer: String,
    counts: Vec<u64>,
}

impl Eq for TopFeature {}

impl PartialEq for TopFeature {
    fn eq(&self, other: &Self) -> bool {
        self.variance == other.variance
    }
}

impl Ord for TopFeature {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .variance
            .partial_cmp(&self.variance)
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for TopFeature {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Copy, Clone)]
enum CorrelationMethod {
    Pearson,
    Cosine,
}

impl CorrelationMethod {
    fn slug(self) -> &'static str {
        match self {
            CorrelationMethod::Pearson => "pearson",
            CorrelationMethod::Cosine => "cosine",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            CorrelationMethod::Pearson => "Pearson",
            CorrelationMethod::Cosine => "Cosine",
        }
    }
}

struct PearsonAccumulator {
    rows: u64,
    sums: Vec<f64>,
    sumsq: Vec<f64>,
    cross: Vec<f64>,
}

impl PearsonAccumulator {
    fn new(samples: usize) -> Self {
        Self {
            rows: 0,
            sums: vec![0.0; samples],
            sumsq: vec![0.0; samples],
            cross: vec![0.0; samples * samples],
        }
    }

    fn update(&mut self, values: &[f64]) {
        self.rows += 1;
        for i in 0..values.len() {
            self.sums[i] += values[i];
            self.sumsq[i] += values[i] * values[i];
            for j in i..values.len() {
                self.cross[i * values.len() + j] += values[i] * values[j];
            }
        }
    }

    fn correlation(&self, i: usize, j: usize, n: usize) -> f64 {
        if self.rows == 0 {
            return 0.0;
        }
        let idx = if j >= i { i * n + j } else { j * n + i };
        let numerator = self.cross[idx] - (self.sums[i] * self.sums[j] / self.rows as f64);
        let denom_i = self.sumsq[i] - (self.sums[i] * self.sums[i] / self.rows as f64);
        let denom_j = self.sumsq[j] - (self.sums[j] * self.sums[j] / self.rows as f64);
        let denom = (denom_i.max(0.0) * denom_j.max(0.0)).sqrt();
        if denom > 0.0 {
            numerator / denom
        } else {
            0.0
        }
    }

    fn write_tsv(&self, labels: &[String], path: &Path) -> anyhow::Result<()> {
        write_correlation_matrix(labels, path, |i, j, n| self.correlation(i, j, n))
    }
}

struct CosineAccumulator {
    sumsq: Vec<f64>,
    cross: Vec<f64>,
}

impl CosineAccumulator {
    fn new(samples: usize) -> Self {
        Self {
            sumsq: vec![0.0; samples],
            cross: vec![0.0; samples * samples],
        }
    }

    fn update(&mut self, values: &[f64]) {
        for i in 0..values.len() {
            self.sumsq[i] += values[i] * values[i];
            for j in i..values.len() {
                self.cross[i * values.len() + j] += values[i] * values[j];
            }
        }
    }

    fn correlation(&self, i: usize, j: usize, n: usize) -> f64 {
        let idx = if j >= i { i * n + j } else { j * n + i };
        let denom = (self.sumsq[i].max(0.0) * self.sumsq[j].max(0.0)).sqrt();
        if denom > 0.0 {
            self.cross[idx] / denom
        } else {
            0.0
        }
    }

    fn write_tsv(&self, labels: &[String], path: &Path) -> anyhow::Result<()> {
        write_correlation_matrix(labels, path, |i, j, n| self.correlation(i, j, n))
    }
}

fn selected_correlation_methods(mode: CorrelationMode) -> Vec<CorrelationMethod> {
    match mode {
        CorrelationMode::All => vec![CorrelationMethod::Pearson, CorrelationMethod::Cosine],
        CorrelationMode::Pearson => vec![CorrelationMethod::Pearson],
        CorrelationMode::Cosine => vec![CorrelationMethod::Cosine],
    }
}

fn write_correlation_matrix<F>(
    labels: &[String],
    path: &Path,
    mut value_at: F,
) -> anyhow::Result<()>
where
    F: FnMut(usize, usize, usize) -> f64,
{
    let mut out = BufWriter::new(File::create(path)?);
    write!(out, "sample")?;
    for label in labels {
        write!(out, "\t{}", label)?;
    }
    writeln!(out)?;
    let n = labels.len();
    for i in 0..n {
        write!(out, "{}", labels[i])?;
        for j in 0..n {
            write!(out, "\t{:.8}", value_at(i, j, n))?;
        }
        writeln!(out)?;
    }
    out.flush()?;
    Ok(())
}

fn transformed_values(counts: &[u64]) -> Vec<f64> {
    counts.iter().map(|count| (*count as f64).ln_1p()).collect()
}

pub fn run_analyze(args: &AnalyzeArgs) -> anyhow::Result<()> {
    let started = Instant::now();
    std::fs::create_dir_all(&args.output_dir)?;

    let labels = infer_sample_labels(&args.inputs, args.labels.as_deref().unwrap_or(&[]))?;
    let readers = open_readers(&args.inputs)?;
    let meta = ensure_compatible(&readers)?;

    let matrix_path = Path::new(&args.output_dir).join("exact_count_matrix.tsv");
    let summary_path = Path::new(&args.output_dir).join("sample_summary.tsv");
    let variable_path = Path::new(&args.output_dir).join("top_variable_kmers.tsv");
    let script_path = Path::new(&args.output_dir).join("plot_analysis.R");
    let primary_correlation_path = Path::new(&args.output_dir).join("correlation.tsv");

    let mut matrix = BufWriter::new(File::create(&matrix_path)?);
    write!(matrix, "kmer")?;
    for label in &labels {
        write!(matrix, "\t{}", label)?;
    }
    writeln!(matrix)?;

    let mut sample_totals = vec![0u64; labels.len()];
    let mut sample_nonzero = vec![0u64; labels.len()];
    let mut top_features: BinaryHeap<TopFeature> = BinaryHeap::new();
    let top_n = args.top_variable_kmers.max(1);
    let mut pearson = PearsonAccumulator::new(labels.len());
    let mut cosine = CosineAccumulator::new(labels.len());
    let methods = selected_correlation_methods(args.correlation);
    let mut rows_written = 0u64;

    for partition_id in 0..meta.partitions {
        print_partition_progress(
            "analyze",
            args.progress,
            partition_id + 1,
            meta.partitions,
            started,
        );
        let mut vectors = Vec::with_capacity(readers.len());
        for reader in &readers {
            vectors.push(load_partition_counts(reader, partition_id)?);
        }
        for_each_union_row(&vectors, |key, row| {
            let kmer = key_to_string(key, &meta);
            write!(matrix, "{}", kmer)?;
            for (index, count) in row.iter().enumerate() {
                write!(matrix, "\t{}", count)?;
                sample_totals[index] = sample_totals[index].saturating_add(*count);
                if *count > 0 {
                    sample_nonzero[index] += 1;
                }
            }
            writeln!(matrix)?;
            rows_written += 1;

            let values = transformed_values(row);
            pearson.update(&values);
            cosine.update(&values);

            let mean = values.iter().sum::<f64>() / values.len().max(1) as f64;
            let variance = values
                .iter()
                .map(|value| {
                    let delta = *value - mean;
                    delta * delta
                })
                .sum::<f64>()
                / values.len().max(1) as f64;
            top_features.push(TopFeature {
                variance,
                kmer,
                counts: row.to_vec(),
            });
            if top_features.len() > top_n {
                let _ = top_features.pop();
            }
            Ok(())
        })?;
    }
    matrix.flush()?;

    for method in &methods {
        let path = Path::new(&args.output_dir).join(format!("correlation_{}.tsv", method.slug()));
        match method {
            CorrelationMethod::Pearson => pearson.write_tsv(&labels, &path)?,
            CorrelationMethod::Cosine => cosine.write_tsv(&labels, &path)?,
        }
    }
    match methods
        .first()
        .copied()
        .unwrap_or(CorrelationMethod::Pearson)
    {
        CorrelationMethod::Pearson => pearson.write_tsv(&labels, &primary_correlation_path)?,
        CorrelationMethod::Cosine => cosine.write_tsv(&labels, &primary_correlation_path)?,
    }

    let mut summary = BufWriter::new(File::create(&summary_path)?);
    writeln!(summary, "sample\ttotal_raw_counts\tnonzero_kmers")?;
    for index in 0..labels.len() {
        writeln!(
            summary,
            "{}\t{}\t{}",
            labels[index], sample_totals[index], sample_nonzero[index]
        )?;
    }
    summary.flush()?;

    let mut features: Vec<TopFeature> = top_features.into_sorted_vec();
    features.sort_by(|left, right| {
        right
            .variance
            .partial_cmp(&left.variance)
            .unwrap_or(Ordering::Equal)
    });
    let mut top_out = BufWriter::new(File::create(&variable_path)?);
    write!(top_out, "kmer")?;
    for label in &labels {
        write!(top_out, "\t{}", label)?;
    }
    writeln!(top_out)?;
    for feature in &features {
        write!(top_out, "{}", feature.kmer)?;
        for count in &feature.counts {
            write!(top_out, "\t{}", count)?;
        }
        writeln!(top_out)?;
    }
    top_out.flush()?;

    std::fs::write(&script_path, render_r_script(args))?;
    try_run_rscript(&script_path, &args.output_dir);

    let correlation_labels = methods
        .iter()
        .map(|method| method.display_name())
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "[kite analyze] wrote matrix, {} correlation table(s), summaries, and plotting assets in {:.3}s",
        correlation_labels,
        started.elapsed().as_secs_f64()
    );
    if args.test_mode {
        print_simple_test_report(
            "analyze",
            &[
                ("inputs", args.inputs.len().to_string()),
                ("labels", labels.join(",")),
                ("partitions", meta.partitions.to_string()),
                ("matrix_rows", rows_written.to_string()),
                ("top_variable_kmers", features.len().to_string()),
                ("output_dir", args.output_dir.clone()),
            ],
            started.elapsed(),
        );
    }
    Ok(())
}

fn render_r_script(args: &AnalyzeArgs) -> String {
    let embedding_mode = match args.embedding {
        EmbeddingMode::All => "all",
        EmbeddingMode::Pca => "pca",
        EmbeddingMode::Tsne => "tsne",
        EmbeddingMode::Umap => "umap",
    };
    let correlation_mode = match args.correlation {
        CorrelationMode::All => "all",
        CorrelationMode::Pearson => "pearson",
        CorrelationMode::Cosine => "cosine",
    };
    let cluster_samples = if args.cluster_samples {
        "TRUE"
    } else {
        "FALSE"
    };

    format!(
        r##"
args <- commandArgs(trailingOnly = TRUE)
out_dir <- args[[1]]
feature_path <- file.path(out_dir, "top_variable_kmers.tsv")
cor_path <- file.path(out_dir, "correlation.tsv")
embedding_mode <- "{embedding_mode}"
correlation_mode <- "{correlation_mode}"
heatmap_kmeans <- {heatmap_kmeans}
heatmap_seed <- {heatmap_seed}
cluster_samples <- {cluster_samples}

if (!requireNamespace("ggplot2", quietly = TRUE)) {{
  message("ggplot2 is not installed; tables and plot script were generated, but PDF figures were skipped.")
  quit(save = "no", status = 0)
}}

suppressPackageStartupMessages(library(ggplot2))

read_matrix <- function(path) {{
  if (!file.exists(path)) {{
    return(NULL)
  }}
  raw_lines <- readLines(path, warn = FALSE)
  raw_lines <- raw_lines[!startsWith(raw_lines, "#")]
  if (length(raw_lines) == 0) {{
    return(NULL)
  }}
  read.delim(text = raw_lines, check.names = FALSE)
}}

feature_df <- read_matrix(feature_path)
if (is.null(feature_df) || ncol(feature_df) < 2 || nrow(feature_df) < 1) {{
  quit(save = "no", status = 0)
}}

rownames(feature_df) <- feature_df[[1]]
feature_mat <- as.matrix(feature_df[, -1, drop = FALSE])
mode(feature_mat) <- "numeric"
sample_mat <- t(log1p(feature_mat))

if (file.exists(cor_path)) {{
  cor_df <- read_matrix(cor_path)
  if (!is.null(cor_df) && ncol(cor_df) >= 2) {{
    rownames(cor_df) <- cor_df[[1]]
    cor_mat <- as.matrix(cor_df[, -1, drop = FALSE])
    mode(cor_mat) <- "numeric"
    cor_labels <- colnames(cor_mat)
    cor_plot_df <- as.data.frame(as.table(cor_mat))
    colnames(cor_plot_df) <- c("sample_x", "sample_y", "correlation")
    cor_plot_df$sample_x <- factor(cor_plot_df$sample_x, levels = cor_labels)
    cor_plot_df$sample_y <- factor(cor_plot_df$sample_y, levels = rev(cor_labels))
    cor_title <- if (correlation_mode == "all") "Sample Correlation (primary: pearson)" else paste("Sample Correlation (", correlation_mode, ")", sep = "")
    cor_plot <- ggplot(cor_plot_df, aes(sample_x, sample_y, fill = correlation)) +
      geom_tile() +
      scale_fill_gradient2(low = "#2166ac", mid = "white", high = "#b2182b", midpoint = 0, limits = c(-1, 1)) +
      labs(title = cor_title, x = NULL, y = NULL, fill = "corr") +
      theme_minimal(base_size = 12) +
      theme(axis.text.x = element_text(angle = 45, hjust = 1))
    ggsave(file.path(out_dir, "correlation.pdf"), cor_plot, width = 8.5, height = 7)
  }}
}}

do_pca <- embedding_mode %in% c("all", "pca")
do_tsne <- embedding_mode %in% c("all", "tsne")
do_umap <- embedding_mode %in% c("all", "umap")

if (do_pca && nrow(sample_mat) >= 2 && ncol(sample_mat) >= 2) {{
  pca <- prcomp(sample_mat, center = TRUE, scale. = TRUE)
  pca_scores <- data.frame(sample = rownames(sample_mat), pca$x[, 1:min(4, ncol(pca$x)), drop = FALSE])
  write.table(pca_scores, file.path(out_dir, "pca_scores.tsv"), sep = "\t", quote = FALSE, row.names = FALSE)
  if (ncol(pca_scores) >= 3) {{
    pca_plot <- ggplot(pca_scores, aes(x = PC1, y = PC2, label = sample)) +
      geom_point(size = 3.2, color = "#1f77b4") +
      geom_text(nudge_y = 0.05 * max(diff(range(pca_scores$PC2, na.rm = TRUE)), 1), size = 3.5) +
      labs(title = "PCA of log1p exact k-mer counts", x = "PC1", y = "PC2") +
      theme_minimal(base_size = 13)
    ggsave(file.path(out_dir, "pca.pdf"), pca_plot, width = 8.5, height = 6.5)
  }}
}}

heatmap_mat <- log1p(feature_mat)
row_scaled <- t(scale(t(heatmap_mat)))
row_scaled[is.na(row_scaled)] <- 0
sample_order <- colnames(row_scaled)
if (cluster_samples && ncol(row_scaled) >= 2) {{
  sample_hc <- hclust(dist(t(row_scaled)))
  sample_order <- sample_hc$labels[sample_hc$order]
}}

row_clusters <- rep(1, nrow(row_scaled))
if (heatmap_kmeans > 1 && nrow(row_scaled) >= heatmap_kmeans) {{
  set.seed(heatmap_seed)
  km <- kmeans(row_scaled, centers = heatmap_kmeans, nstart = 10)
  row_clusters <- km$cluster
}}
row_order <- order(row_clusters, rowMeans(row_scaled[, sample_order, drop = FALSE], na.rm = TRUE))
ordered_mat <- row_scaled[row_order, sample_order, drop = FALSE]
row_info <- data.frame(
  kmer = rownames(row_scaled)[row_order],
  cluster = row_clusters[row_order],
  order = seq_along(row_order)
)
write.table(row_info, file.path(out_dir, "heatmap_rows.tsv"), sep = "\t", quote = FALSE, row.names = FALSE)
sample_info <- data.frame(sample = sample_order, order = seq_along(sample_order))
write.table(sample_info, file.path(out_dir, "heatmap_samples.tsv"), sep = "\t", quote = FALSE, row.names = FALSE)

heat_df <- as.data.frame(as.table(ordered_mat))
colnames(heat_df) <- c("kmer", "sample", "value")
heat_df$sample <- factor(heat_df$sample, levels = sample_order)
heat_df$kmer <- factor(heat_df$kmer, levels = rev(rownames(ordered_mat)))
heat_plot <- ggplot(heat_df, aes(sample, kmer, fill = value)) +
  geom_tile() +
  scale_fill_gradient2(low = "#2166ac", mid = "white", high = "#b2182b", midpoint = 0) +
  labs(title = "Clustered Heatmap of top variable exact k-mers", x = NULL, y = NULL, fill = "z") +
  theme_minimal(base_size = 11) +
  theme(
    axis.text.x = element_text(angle = 45, hjust = 1),
    axis.text.y = element_blank(),
    axis.ticks.y = element_blank()
  )
ggsave(file.path(out_dir, "heatmap.pdf"), heat_plot, width = 9, height = 8.5)

if (do_tsne && nrow(sample_mat) >= 3 && requireNamespace("Rtsne", quietly = TRUE)) {{
  set.seed(heatmap_seed)
  perplexity <- max(1, min(30, floor((nrow(sample_mat) - 1) / 3)))
  tsne <- Rtsne::Rtsne(sample_mat, perplexity = perplexity, check_duplicates = FALSE)
  tsne_scores <- data.frame(sample = rownames(sample_mat), tsne1 = tsne$Y[, 1], tsne2 = tsne$Y[, 2])
  write.table(tsne_scores, file.path(out_dir, "tsne_scores.tsv"), sep = "\t", quote = FALSE, row.names = FALSE)
  tsne_plot <- ggplot(tsne_scores, aes(tsne1, tsne2, label = sample)) +
    geom_point(size = 3.2, color = "#2ca02c") +
    geom_text(nudge_y = 0.05 * max(diff(range(tsne_scores$tsne2, na.rm = TRUE)), 1), size = 3.5) +
    labs(title = "t-SNE of log1p exact k-mer counts", x = "tSNE1", y = "tSNE2") +
    theme_minimal(base_size = 13)
  ggsave(file.path(out_dir, "tsne.pdf"), tsne_plot, width = 8.5, height = 6.5)
}}

if (do_umap && nrow(sample_mat) >= 2 && requireNamespace("uwot", quietly = TRUE)) {{
  set.seed(heatmap_seed)
  umap <- uwot::umap(sample_mat, n_neighbors = max(2, min(15, nrow(sample_mat) - 1)))
  umap_scores <- data.frame(sample = rownames(sample_mat), umap1 = umap[, 1], umap2 = umap[, 2])
  write.table(umap_scores, file.path(out_dir, "umap_scores.tsv"), sep = "\t", quote = FALSE, row.names = FALSE)
  umap_plot <- ggplot(umap_scores, aes(umap1, umap2, label = sample)) +
    geom_point(size = 3.2, color = "#9467bd") +
    geom_text(nudge_y = 0.05 * max(diff(range(umap_scores$umap2, na.rm = TRUE)), 1), size = 3.5) +
    labs(title = "UMAP of log1p exact k-mer counts", x = "UMAP1", y = "UMAP2") +
    theme_minimal(base_size = 13)
  ggsave(file.path(out_dir, "umap.pdf"), umap_plot, width = 8.5, height = 6.5)
}}
"##,
        embedding_mode = embedding_mode,
        correlation_mode = correlation_mode,
        heatmap_kmeans = args.heatmap_kmeans.max(1),
        heatmap_seed = args.heatmap_seed,
        cluster_samples = cluster_samples,
    )
}

fn try_run_rscript(script_path: &Path, output_dir: &str) {
    let result = Command::new("Rscript")
        .arg(script_path)
        .arg(output_dir)
        .output();
    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            eprintln!(
                "[kite analyze] Rscript exited with status {}. Generated script kept at {}.",
                output.status,
                script_path.display()
            );
            if !output.stderr.is_empty() {
                eprintln!("{}", String::from_utf8_lossy(&output.stderr));
            }
        }
        Err(error) => {
            eprintln!(
                "[kite analyze] could not run Rscript ({}). Generated script kept at {}.",
                error,
                script_path.display()
            );
        }
    }
}
