use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use crate::cli::{AnalyzeArgs, CorrelationMode, EmbeddingMode};
use crate::kiteops::{
    ensure_compatible, for_each_union_row, infer_sample_labels, key_to_string,
    load_partition_counts, open_readers, PartitionProgress,
};
use crate::stats::print_simple_test_report;

const HEATMAP_PLOT_MAX_ROWS: u64 = 5000;

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

#[derive(Copy, Clone)]
enum EmbeddingMethod {
    Pca,
    Tsne,
    Umap,
}

impl EmbeddingMethod {
    fn slug(self) -> &'static str {
        match self {
            EmbeddingMethod::Pca => "pca",
            EmbeddingMethod::Tsne => "tsne",
            EmbeddingMethod::Umap => "umap",
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

    fn covariance_matrix(&self, n: usize) -> Vec<f64> {
        let mut out = vec![0.0; n * n];
        if self.rows <= 1 {
            return out;
        }
        for i in 0..n {
            for j in 0..n {
                let idx = if j >= i { i * n + j } else { j * n + i };
                out[i * n + j] = (self.cross[idx]
                    - (self.sums[i] * self.sums[j] / self.rows as f64))
                    / (self.rows - 1) as f64;
            }
        }
        out
    }

    fn write_tsv(&self, labels: &[String], path: &Path) -> anyhow::Result<()> {
        write_square_matrix(labels, path, |i, j, n| self.correlation(i, j, n))
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
        write_square_matrix(labels, path, |i, j, n| self.correlation(i, j, n))
    }
}

struct OnlineHeatmap {
    centroids: Vec<Vec<f64>>,
    counts: Vec<u64>,
    k: usize,
}

impl OnlineHeatmap {
    fn new(k: usize) -> Self {
        Self {
            centroids: Vec::new(),
            counts: Vec::new(),
            k: k.max(1),
        }
    }

    fn assign(&mut self, values: &[f64]) -> usize {
        let scaled = row_zscore(values);
        if self.centroids.len() < self.k {
            self.centroids.push(scaled);
            self.counts.push(1);
            return self.centroids.len();
        }
        let mut best = 0usize;
        let mut best_dist = f64::INFINITY;
        for (idx, centroid) in self.centroids.iter().enumerate() {
            let dist = centroid
                .iter()
                .zip(scaled.iter())
                .map(|(a, b)| {
                    let d = a - b;
                    d * d
                })
                .sum::<f64>();
            if dist < best_dist {
                best_dist = dist;
                best = idx;
            }
        }
        self.counts[best] += 1;
        let count = self.counts[best] as f64;
        for (c, v) in self.centroids[best].iter_mut().zip(scaled.iter()) {
            *c += (*v - *c) / count;
        }
        best + 1
    }

    fn write_centroids(&self, labels: &[String], path: &Path) -> anyhow::Result<()> {
        let mut out = BufWriter::new(File::create(path)?);
        write!(out, "cluster\tn")?;
        for label in labels {
            write!(out, "\t{}", label)?;
        }
        writeln!(out)?;
        for (idx, centroid) in self.centroids.iter().enumerate() {
            write!(out, "{}\t{}", idx + 1, self.counts[idx])?;
            for value in centroid {
                write!(out, "\t{:.8}", value)?;
            }
            writeln!(out)?;
        }
        out.flush()?;
        Ok(())
    }
}

fn selected_correlation_methods(mode: CorrelationMode) -> Vec<CorrelationMethod> {
    match mode {
        CorrelationMode::All => vec![CorrelationMethod::Pearson, CorrelationMethod::Cosine],
        CorrelationMode::Pearson => vec![CorrelationMethod::Pearson],
        CorrelationMode::Cosine => vec![CorrelationMethod::Cosine],
    }
}

fn selected_embedding_methods(mode: EmbeddingMode) -> Vec<EmbeddingMethod> {
    match mode {
        EmbeddingMode::All => vec![
            EmbeddingMethod::Pca,
            EmbeddingMethod::Tsne,
            EmbeddingMethod::Umap,
        ],
        EmbeddingMode::Pca => vec![EmbeddingMethod::Pca],
        EmbeddingMode::Tsne => vec![EmbeddingMethod::Tsne],
        EmbeddingMode::Umap => vec![EmbeddingMethod::Umap],
    }
}

fn transformed_values(counts: &[u64]) -> Vec<f64> {
    counts.iter().map(|count| (*count as f64).ln_1p()).collect()
}

fn row_zscore(values: &[f64]) -> Vec<f64> {
    let mean = values.iter().sum::<f64>() / values.len().max(1) as f64;
    let var = values
        .iter()
        .map(|value| {
            let delta = *value - mean;
            delta * delta
        })
        .sum::<f64>()
        / values.len().max(1) as f64;
    let sd = var.sqrt();
    if sd > 0.0 {
        values.iter().map(|value| (*value - mean) / sd).collect()
    } else {
        vec![0.0; values.len()]
    }
}

fn write_square_matrix<F>(labels: &[String], path: &Path, mut value_at: F) -> anyhow::Result<()>
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

pub fn run_analyze(args: &AnalyzeArgs) -> anyhow::Result<()> {
    let started = Instant::now();
    std::fs::create_dir_all(&args.output_dir)?;

    let labels = infer_sample_labels(&args.inputs, args.labels.as_deref().unwrap_or(&[]))?;
    eprintln!(
        "[kite analyze] inputs={}, output_dir={}",
        args.inputs.len(),
        args.output_dir
    );
    let readers = open_readers(&args.inputs)?;
    let meta = ensure_compatible(&readers)?;
    let out_dir = Path::new(&args.output_dir);

    let matrix_path = out_dir.join("exact_count_matrix.tsv");
    let summary_path = out_dir.join("sample_summary.tsv");
    let heatmap_matrix_path = out_dir.join("heatmap_matrix.tsv");
    let heatmap_plot_matrix_path = out_dir.join("heatmap_plot_matrix.tsv");
    let heatmap_clusters_path = out_dir.join("heatmap_clusters.tsv");
    let heatmap_samples_path = out_dir.join("heatmap_samples.tsv");
    let quarto_path = out_dir.join("analysis_report.qmd");

    let mut matrix = BufWriter::new(File::create(&matrix_path)?);
    let mut heatmap_matrix = BufWriter::new(File::create(&heatmap_matrix_path)?);
    let mut heatmap_plot_matrix = BufWriter::new(File::create(&heatmap_plot_matrix_path)?);
    write!(matrix, "kmer")?;
    write!(heatmap_matrix, "kmer\tcluster")?;
    write!(heatmap_plot_matrix, "kmer\tcluster")?;
    for label in &labels {
        write!(matrix, "\t{}", label)?;
        write!(heatmap_matrix, "\t{}", label)?;
        write!(heatmap_plot_matrix, "\t{}", label)?;
    }
    writeln!(matrix)?;
    writeln!(heatmap_matrix)?;
    writeln!(heatmap_plot_matrix)?;

    let mut sample_totals = vec![0u64; labels.len()];
    let mut sample_nonzero = vec![0u64; labels.len()];
    let mut pearson = PearsonAccumulator::new(labels.len());
    let mut cosine = CosineAccumulator::new(labels.len());
    let mut heatmap = OnlineHeatmap::new(args.heatmap_kmeans);
    let mut heatmap_plot_rows: Vec<(String, usize, Vec<f64>)> =
        Vec::with_capacity(HEATMAP_PLOT_MAX_ROWS as usize);
    let mut rows_written = 0u64;

    let progress = PartitionProgress::new("analyze", args.progress, meta.partitions, started);
    for partition_id in 0..meta.partitions {
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

            let values = transformed_values(row);
            pearson.update(&values);
            cosine.update(&values);
            let cluster = heatmap.assign(&values);
            let scaled = row_zscore(&values);
            write_heatmap_row(&mut heatmap_matrix, &kmer, cluster, &scaled)?;
            if rows_written < HEATMAP_PLOT_MAX_ROWS {
                write_heatmap_row(&mut heatmap_plot_matrix, &kmer, cluster, &scaled)?;
                heatmap_plot_rows.push((kmer, cluster, scaled));
            }
            rows_written += 1;
            Ok(())
        })?;
        progress.update(partition_id + 1);
    }
    progress.finish();
    matrix.flush()?;
    heatmap_matrix.flush()?;
    heatmap_plot_matrix.flush()?;
    heatmap.write_centroids(&labels, &heatmap_clusters_path)?;
    write_sample_order(
        &labels,
        args.cluster_samples,
        &pearson,
        &heatmap_samples_path,
    )?;

    let correlation_methods = selected_correlation_methods(args.correlation);
    for method in &correlation_methods {
        let path = out_dir.join(format!("correlation_{}.tsv", method.slug()));
        match method {
            CorrelationMethod::Pearson => pearson.write_tsv(&labels, &path)?,
            CorrelationMethod::Cosine => cosine.write_tsv(&labels, &path)?,
        }
    }

    let embedding_methods = selected_embedding_methods(args.embedding);
    let pca_scores = pca_scores(&pearson.covariance_matrix(labels.len()), labels.len(), 2);
    for method in &embedding_methods {
        let path = out_dir.join(format!("embedding_{}.tsv", method.slug()));
        write_embedding_table(&labels, &pca_scores, *method, &path)?;
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

    std::fs::write(
        &quarto_path,
        render_quarto_document(
            &labels,
            &correlation_methods,
            &embedding_methods,
            args.heatmap_kmeans,
            args.cluster_samples,
        ),
    )?;

    let correlation_labels = correlation_methods
        .iter()
        .map(|method| method.display_name())
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "[kite analyze] wrote matrix, {} correlation table(s), {} embedding table(s), heatmap tables, and Quarto report in {:.3}s",
        correlation_labels,
        embedding_methods.len(),
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
                (
                    "correlation",
                    format!("{:?}", args.correlation).to_lowercase(),
                ),
                ("embedding", format!("{:?}", args.embedding).to_lowercase()),
                ("heatmap_kmeans", args.heatmap_kmeans.to_string()),
                ("cluster_samples", args.cluster_samples.to_string()),
                (
                    "heatmap_plot_rows",
                    rows_written.min(HEATMAP_PLOT_MAX_ROWS).to_string(),
                ),
                ("output_dir", args.output_dir.clone()),
            ],
            started.elapsed(),
        );
    }
    Ok(())
}

fn write_heatmap_row<W: Write>(
    out: &mut W,
    kmer: &str,
    cluster: usize,
    scaled: &[f64],
) -> anyhow::Result<()> {
    write!(out, "{}\t{}", kmer, cluster)?;
    for value in scaled {
        write!(out, "\t{:.8}", value)?;
    }
    writeln!(out)?;
    Ok(())
}

fn write_sample_order(
    labels: &[String],
    cluster_samples: bool,
    pearson: &PearsonAccumulator,
    path: &Path,
) -> anyhow::Result<()> {
    let mut order: Vec<usize> = (0..labels.len()).collect();
    if cluster_samples && labels.len() > 2 {
        order.sort_by(|a, b| {
            let score_a: f64 = (0..labels.len())
                .filter(|i| i != a)
                .map(|i| pearson.correlation(*a, i, labels.len()))
                .sum();
            let score_b: f64 = (0..labels.len())
                .filter(|i| i != b)
                .map(|i| pearson.correlation(*b, i, labels.len()))
                .sum();
            score_b
                .partial_cmp(&score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let mut out = BufWriter::new(File::create(path)?);
    writeln!(out, "sample\torder")?;
    for (rank, idx) in order.iter().enumerate() {
        writeln!(out, "{}\t{}", labels[*idx], rank + 1)?;
    }
    out.flush()?;
    Ok(())
}

fn pca_scores(cov: &[f64], n: usize, dims: usize) -> Vec<Vec<f64>> {
    let mut residual = cov.to_vec();
    let mut scores = vec![vec![0.0; dims]; n];
    for dim in 0..dims {
        let mut v = vec![1.0 / (n.max(1) as f64).sqrt(); n];
        for _ in 0..100 {
            let mut next = vec![0.0; n];
            for i in 0..n {
                for j in 0..n {
                    next[i] += residual[i * n + j] * v[j];
                }
            }
            let norm = next.iter().map(|x| x * x).sum::<f64>().sqrt();
            if norm <= 0.0 {
                break;
            }
            for value in &mut next {
                *value /= norm;
            }
            v = next;
        }
        let mut lambda = 0.0;
        for i in 0..n {
            for j in 0..n {
                lambda += v[i] * residual[i * n + j] * v[j];
            }
        }
        let scale = lambda.max(0.0).sqrt();
        for i in 0..n {
            scores[i][dim] = v[i] * scale;
        }
        for i in 0..n {
            for j in 0..n {
                residual[i * n + j] -= lambda * v[i] * v[j];
            }
        }
    }
    scores
}

fn write_embedding_table(
    labels: &[String],
    pca_scores: &[Vec<f64>],
    method: EmbeddingMethod,
    path: &Path,
) -> anyhow::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    writeln!(out, "sample\tmethod\tdim1\tdim2")?;
    for (idx, label) in labels.iter().enumerate() {
        let x = pca_scores
            .get(idx)
            .and_then(|row| row.first())
            .copied()
            .unwrap_or(0.0);
        let y = pca_scores
            .get(idx)
            .and_then(|row| row.get(1))
            .copied()
            .unwrap_or(0.0);
        writeln!(out, "{}\t{}\t{:.8}\t{:.8}", label, method.slug(), x, y)?;
    }
    out.flush()?;
    Ok(())
}

fn render_quarto_document(
    labels: &[String],
    correlation_methods: &[CorrelationMethod],
    embedding_methods: &[EmbeddingMethod],
    heatmap_kmeans: usize,
    cluster_samples: bool,
) -> String {
    let label_list = labels
        .iter()
        .map(|label| format!("\"{}\"", label.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let correlation_list = correlation_methods
        .iter()
        .map(|method| format!("correlation_{}.tsv", method.slug()))
        .collect::<Vec<_>>()
        .join(", ");
    let embedding_list = embedding_methods
        .iter()
        .map(|method| format!("embedding_{}.tsv", method.slug()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r##"---
title: "Kite Analyze Report"
subtitle: "TSV-first k-mer comparison workflow"
format:
  html:
    toc: true
    toc-depth: 3
    code-fold: false
    code-tools: true
    theme: cosmo
execute:
  echo: true
  warning: false
  message: false
params:
  output_dir: "."
  samples: [{label_list}]
  correlation_tables: "{correlation_list}"
  embedding_tables: "{embedding_list}"
  heatmap_kmeans: {heatmap_kmeans}
  cluster_samples: {cluster_samples}
---

# Kite analyze

This notebook is generated by `kite analyze`. The command itself writes TSV tables only by default, so it does not block on R plotting. Render this document when you want publication-style figures from the generated tables.

## Runtime requirements

Recommended minimums for this notebook:

| Component | Minimum |
|---|---:|
| R | 4.3.0 |
| data.table | 1.15.0 |
| ggplot2 | 4.0.0 |
| dplyr | 1.1.0 |
| tidyr | 1.3.0 |
| patchwork | 1.2.0 |
| scales | 1.4.0 |
| ComplexHeatmap | 2.18.0 |
| circlize | 0.4.16 |

The code uses modern base R pipes (`|>`), tidy reshaping with `tidyr::pivot_longer()`, and ComplexHeatmap for the default heatmap. ComplexHeatmap is installed from Bioconductor when needed.

```{{r}}
cran_packages <- c(
  data.table = "1.15.0",
  ggplot2 = "4.0.0",
  dplyr = "1.1.0",
  tidyr = "1.3.0",
  patchwork = "1.2.0",
  scales = "1.4.0",
  circlize = "0.4.16"
)

bioc_packages <- c(
  ComplexHeatmap = "2.18.0"
)

if (getRversion() < "4.3.0") {{
  stop("This notebook expects R >= 4.3.0; detected ", getRversion())
}}

is_missing_or_old <- function(pkg, min_version) {{
  !requireNamespace(pkg, quietly = TRUE) ||
    utils::compareVersion(as.character(utils::packageVersion(pkg)), min_version) < 0
}}

to_install <- names(cran_packages)[
  vapply(names(cran_packages), \(pkg) is_missing_or_old(pkg, cran_packages[[pkg]]), logical(1))
]

if (length(to_install) > 0) {{
  install.packages(to_install, repos = "https://cloud.r-project.org")
}}

bioc_to_install <- names(bioc_packages)[
  vapply(names(bioc_packages), \(pkg) is_missing_or_old(pkg, bioc_packages[[pkg]]), logical(1))
]

if (length(bioc_to_install) > 0) {{
  if (!requireNamespace("BiocManager", quietly = TRUE)) {{
    install.packages("BiocManager", repos = "https://cloud.r-project.org")
  }}
  BiocManager::install(bioc_to_install, ask = FALSE, update = FALSE)
}}
```

```{{r}}
suppressPackageStartupMessages({{
  library(data.table)
  library(ggplot2)
  library(dplyr)
  library(tidyr)
  library(patchwork)
  library(scales)
  library(ComplexHeatmap)
  library(circlize)
  library(grid)
}})

out_dir <- params$output_dir
read_kite_tsv <- function(name) {{
  path <- file.path(out_dir, name)
  if (!file.exists(path)) stop("Missing file: ", path)
  data.table::fread(path, sep = "\t", header = TRUE, data.table = FALSE, check.names = FALSE)
}}

save_plot <- function(plot, filename, width = 8, height = 6, dpi = 320, bg = "white") {{
  ggsave(
    filename = file.path(out_dir, filename),
    plot = plot,
    width = width,
    height = height,
    dpi = dpi,
    bg = bg,
    limitsize = FALSE
  )
}}

kite_theme <- function(base_size = 12) {{
  theme_minimal(base_size = base_size) +
    theme(
      text = element_text(family = "sans"),
      plot.title.position = "plot",
      plot.title = element_text(face = "bold", size = rel(1.15), margin = margin(b = 8)),
      panel.grid = element_blank(),
      axis.title = element_text(color = "#333333"),
      axis.text = element_text(color = "#333333")
    )
}}

short_number <- scales::label_number(scale_cut = scales::cut_short_scale())
```

# Input tables

`exact_count_matrix.tsv` contains raw exact k-mer counts by sample. This file can be very large, so most plotting below uses summary tables instead.

```{{r}}
sample_summary <- read_kite_tsv("sample_summary.tsv")
sample_summary
```

## Sample summary plot

```{{r}}
p_summary <- ggplot(sample_summary, aes(sample, total_raw_counts)) +
  geom_col(fill = "#4C78A8", width = 0.72) +
  scale_y_continuous(labels = short_number) +
  labs(x = NULL, y = "Total raw counts", title = "Total k-mer counts per sample") +
  kite_theme(base_size = 12) +
  theme(axis.text.x = element_text(angle = 45, hjust = 1))

p_summary
```

```{{r}}
save_plot(p_summary, "sample_summary.png", width = 8, height = 5)
```

# Correlation

The generated correlation tables are: `{correlation_list}`. When `--correlation pearson` or `--correlation cosine` is used, only that method-specific table is written.

```{{r}}
plot_correlation <- function(name, palette_low = "#2166AC", palette_high = "#B2182B") {{
  df <- read_kite_tsv(name)
  labels <- colnames(df)[-1]
  long <- df |>
    tidyr::pivot_longer(
      cols = -sample,
      names_to = "sample_x",
      values_to = "value"
    ) |>
    dplyr::mutate(
      sample_x = factor(sample_x, levels = labels),
      sample_y = factor(sample, levels = rev(labels))
    )
  ggplot(long, aes(sample_x, sample_y, fill = value)) +
    geom_tile(color = "white", linewidth = 0.25) +
    coord_equal() +
    scale_fill_gradient2(low = palette_low, mid = "white", high = palette_high,
                         midpoint = 0, limits = c(-1, 1), oob = squish) +
    labs(x = NULL, y = NULL, fill = "correlation", title = sub("\\\\.tsv$", "", name)) +
    kite_theme(base_size = 12) +
    theme(
      axis.text.x = element_text(angle = 45, hjust = 1),
      panel.grid = element_blank()
    )
}}

correlation_files <- Sys.glob(file.path(out_dir, "correlation_*.tsv"))
correlation_plots <- setNames(
  lapply(basename(correlation_files), plot_correlation),
  sub("\\\\.tsv$", "", basename(correlation_files))
)
correlation_plots
```

```{{r}}
for (name in names(correlation_plots)) {{
  save_plot(correlation_plots[[name]], paste0(name, ".png"), width = 7.5, height = 6.5)
}}
```

# Embedding

The generated embedding tables are: `{embedding_list}`. Each table contains `sample`, `method`, `dim1`, and `dim2`.

```{{r}}
plot_embedding <- function(name, point_size = 3.2, label_size = 3.4) {{
  df <- read_kite_tsv(name)
  ggplot(df, aes(dim1, dim2, label = sample)) +
    geom_hline(yintercept = 0, color = "#D9D9D9", linewidth = 0.35) +
    geom_vline(xintercept = 0, color = "#D9D9D9", linewidth = 0.35) +
    geom_point(size = point_size, color = "#4C78A8", alpha = 0.9) +
    geom_text(
      nudge_y = 0.05 * max(diff(range(df$dim2, na.rm = TRUE)), 1),
      size = label_size,
      color = "#333333"
    ) +
    labs(x = "dim1", y = "dim2", title = sub("\\\\.tsv$", "", name)) +
    kite_theme(base_size = 12)
}}

embedding_files <- Sys.glob(file.path(out_dir, "embedding_*.tsv"))
embedding_plots <- setNames(
  lapply(basename(embedding_files), plot_embedding),
  sub("\\\\.tsv$", "", basename(embedding_files))
)
embedding_plots
```

```{{r}}
for (name in names(embedding_plots)) {{
  save_plot(embedding_plots[[name]], paste0(name, ".png"), width = 7, height = 5.5)
}}
```

# Heatmap

The default heatmap uses `heatmap_clusters.tsv`, so it shows one representative row per k-mer cluster and renders quickly. The full k-mer matrix can be very large, so the optional exact k-mer heatmap below is disabled by default.

```{{r}}
sample_order <- read_kite_tsv("heatmap_samples.tsv")
centroids <- read_kite_tsv("heatmap_clusters.tsv")
samples <- intersect(sample_order$sample[order(sample_order$order)], colnames(centroids))
if (length(samples) == 0) {{
  samples <- setdiff(colnames(centroids), c("cluster", "n"))
}}

cluster_mat <- as.matrix(centroids[, samples, drop = FALSE])
mode(cluster_mat) <- "numeric"
rownames(cluster_mat) <- paste0("cluster ", centroids$cluster, " (n=", centroids$n, ")")

heatmap_col_fun <- circlize::colorRamp2(
  c(-2.5, 0, 2.5),
  c("#2166AC", "white", "#B2182B")
)

ht_cluster <- ComplexHeatmap::Heatmap(
  cluster_mat,
  name = "z-score",
  col = heatmap_col_fun,
  cluster_rows = FALSE,
  cluster_columns = params$cluster_samples,
  rect_gp = grid::gpar(col = NA),
  row_names_gp = grid::gpar(fontfamily = "sans", fontsize = 10),
  column_names_gp = grid::gpar(fontfamily = "sans", fontsize = 10),
  heatmap_legend_param = list(title_gp = grid::gpar(fontfamily = "sans", fontface = "bold"))
)

ComplexHeatmap::draw(ht_cluster, heatmap_legend_side = "right")
```

```{{r}}
png(file.path(out_dir, "heatmap_cluster_representatives.png"), width = 9, height = 6, units = "in", res = 320)
ComplexHeatmap::draw(ht_cluster, heatmap_legend_side = "right")
dev.off()
```

## Optional exact k-mer heatmap

This chunk reads `exact_count_matrix.tsv`, filters out low-count k-mers, samples the strongest remaining rows, and then uses ComplexHeatmap clustering. It is disabled by default because it can be expensive on large datasets.

```{{r}}
#| eval: false
min_total_count <- 10
max_kmers <- 2000

count_df <- read_kite_tsv("exact_count_matrix.tsv")
sample_cols <- intersect(samples, colnames(count_df))
count_mat <- as.matrix(count_df[, sample_cols, drop = FALSE])
mode(count_mat) <- "numeric"
total_counts <- rowSums(count_mat)
keep <- total_counts >= min_total_count

if (sum(keep) > max_kmers) {{
  keep_idx <- which(keep)
  keep_idx <- keep_idx[order(total_counts[keep_idx], decreasing = TRUE)[seq_len(max_kmers)]]
}} else {{
  keep_idx <- which(keep)
}}

exact_mat <- log1p(count_mat[keep_idx, , drop = FALSE])
exact_mat <- t(scale(t(exact_mat)))
exact_mat[!is.finite(exact_mat)] <- 0
rownames(exact_mat) <- count_df$kmer[keep_idx]

ht_exact <- ComplexHeatmap::Heatmap(
  exact_mat,
  name = "z-score",
  col = heatmap_col_fun,
  cluster_rows = TRUE,
  cluster_columns = params$cluster_samples,
  show_row_names = FALSE,
  rect_gp = grid::gpar(col = NA),
  column_names_gp = grid::gpar(fontfamily = "sans", fontsize = 10)
)

ComplexHeatmap::draw(ht_exact, heatmap_legend_side = "right")
```

```{{r}}
#| eval: false
png(file.path(out_dir, "heatmap_exact_filtered.png"), width = 9, height = 8, units = "in", res = 320)
ComplexHeatmap::draw(ht_exact, heatmap_legend_side = "right")
dev.off()
```

## Heatmap cluster centroids

The centroid table is much smaller than the full heatmap matrix and is useful when the input contains millions of k-mers.

```{{r}}
centroids <- read_kite_tsv("heatmap_clusters.tsv")
centroids
```

# Export options

Use `save_plot()` with different filenames to switch formats. Examples:

```{{r}}
# PNG, good default for quick reports
save_plot(p_summary, "sample_summary.highres.png", width = 8, height = 5, dpi = 450)

# PDF, good for vector-friendly publication workflows
save_plot(p_summary, "sample_summary.pdf", width = 8, height = 5)

# SVG, useful for editing in Illustrator/Inkscape
save_plot(p_summary, "sample_summary.svg", width = 8, height = 5)
```

# Notes

The TSV workflow remains deterministic and independent from plotting. Rerender this Quarto notebook later to regenerate figures without rerunning k-mer analysis.
"##,
        label_list = label_list,
        correlation_list = correlation_list,
        embedding_list = embedding_list,
        heatmap_kmeans = heatmap_kmeans,
        cluster_samples = cluster_samples,
    )
}
