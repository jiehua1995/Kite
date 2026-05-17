use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use faer::Mat;
use manifolds_rs::{pacmap, tsne, umap, PacmapParams, TsneParams, UmapParams};
use nalgebra::{DMatrix, SymmetricEigen};

use crate::cli::{AnalyzeArgs, CorrelationMode, EmbeddingMode};
use crate::kiteops::{
    apply_reader_filter_to_rows, ensure_compatible, for_each_union_row, format_reader_filter,
    infer_sample_labels, key_to_string, load_partition_counts, open_readers, PartitionProgress,
};
use crate::stats::print_simple_test_report;

const HEATMAP_PLOT_MAX_ROWS: u64 = 5000;

#[derive(Copy, Clone, Eq, PartialEq)]
enum CorrelationMethod {
    Pearson,
    Spearman,
    Cosine,
    Euclidean,
    Manhattan,
    Mahalanobis,
}

impl CorrelationMethod {
    fn slug(self) -> &'static str {
        match self {
            CorrelationMethod::Pearson => "pearson",
            CorrelationMethod::Spearman => "spearman",
            CorrelationMethod::Cosine => "cosine",
            CorrelationMethod::Euclidean => "euclidean",
            CorrelationMethod::Manhattan => "manhattan",
            CorrelationMethod::Mahalanobis => "mahalanobis",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            CorrelationMethod::Pearson => "Pearson",
            CorrelationMethod::Spearman => "Spearman",
            CorrelationMethod::Cosine => "Cosine",
            CorrelationMethod::Euclidean => "Euclidean distance",
            CorrelationMethod::Manhattan => "Manhattan distance",
            CorrelationMethod::Mahalanobis => "Mahalanobis distance",
        }
    }

    fn is_distance(self) -> bool {
        matches!(
            self,
            CorrelationMethod::Euclidean
                | CorrelationMethod::Manhattan
                | CorrelationMethod::Mahalanobis
        )
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum EmbeddingMethod {
    Pca,
    Tsne,
    Umap,
    Pacmap,
}

impl EmbeddingMethod {
    fn slug(self) -> &'static str {
        match self {
            EmbeddingMethod::Pca => "pca",
            EmbeddingMethod::Tsne => "tsne",
            EmbeddingMethod::Umap => "umap",
            EmbeddingMethod::Pacmap => "pacmap",
        }
    }

    fn needs_dense_features(self) -> bool {
        !matches!(self, EmbeddingMethod::Pca)
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

struct DistanceAccumulator {
    euclidean_sumsq: Vec<f64>,
    manhattan_sum: Vec<f64>,
    mahalanobis_diag_sumsq: Vec<f64>,
}

impl DistanceAccumulator {
    fn new(samples: usize) -> Self {
        Self {
            euclidean_sumsq: vec![0.0; samples * samples],
            manhattan_sum: vec![0.0; samples * samples],
            mahalanobis_diag_sumsq: vec![0.0; samples * samples],
        }
    }

    fn update(&mut self, values: &[f64]) {
        let n = values.len();
        let mean = values.iter().sum::<f64>() / n.max(1) as f64;
        let row_var = values
            .iter()
            .map(|value| {
                let delta = *value - mean;
                delta * delta
            })
            .sum::<f64>()
            / n.max(1) as f64;
        let weight = if row_var > 1e-12 { 1.0 / row_var } else { 0.0 };
        for i in 0..n {
            for j in i..n {
                let idx = i * n + j;
                let delta = values[i] - values[j];
                self.euclidean_sumsq[idx] += delta * delta;
                self.manhattan_sum[idx] += delta.abs();
                self.mahalanobis_diag_sumsq[idx] += delta * delta * weight;
            }
        }
    }

    fn value(&self, method: CorrelationMethod, i: usize, j: usize, n: usize) -> f64 {
        let idx = if j >= i { i * n + j } else { j * n + i };
        match method {
            CorrelationMethod::Euclidean => self.euclidean_sumsq[idx].sqrt(),
            CorrelationMethod::Manhattan => self.manhattan_sum[idx],
            CorrelationMethod::Mahalanobis => self.mahalanobis_diag_sumsq[idx].sqrt(),
            _ => 0.0,
        }
    }

    fn write_tsv(
        &self,
        labels: &[String],
        method: CorrelationMethod,
        path: &Path,
    ) -> anyhow::Result<()> {
        write_square_matrix(labels, path, |i, j, n| self.value(method, i, j, n))
    }
}

fn push_columns(columns: &mut [Vec<f64>], values: &[f64]) {
    for (column, value) in columns.iter_mut().zip(values.iter()) {
        column.push(*value);
    }
}

fn average_ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| {
        values[*a]
            .partial_cmp(&values[*b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut ranks = vec![0.0; values.len()];
    let mut start = 0usize;
    while start < order.len() {
        let mut end = start + 1;
        while end < order.len() && values[order[start]] == values[order[end]] {
            end += 1;
        }
        let rank = (start + 1 + end) as f64 / 2.0;
        for idx in start..end {
            ranks[order[idx]] = rank;
        }
        start = end;
    }
    ranks
}

fn write_spearman_tsv(columns: &[Vec<f64>], labels: &[String], path: &Path) -> anyhow::Result<()> {
    if columns.is_empty() {
        return write_square_matrix(labels, path, |_, _, _| 0.0);
    }
    let ranks = columns
        .iter()
        .map(|column| average_ranks(column))
        .collect::<Vec<_>>();
    write_square_matrix(labels, path, |i, j, _| pearson_slice(&ranks[i], &ranks[j]))
}

fn pearson_slice(x: &[f64], y: &[f64]) -> f64 {
    if x.len() != y.len() || x.is_empty() {
        return 0.0;
    }
    let n = x.len() as f64;
    let sum_x = x.iter().sum::<f64>();
    let sum_y = y.iter().sum::<f64>();
    let sumsq_x = x.iter().map(|value| value * value).sum::<f64>();
    let sumsq_y = y.iter().map(|value| value * value).sum::<f64>();
    let cross = x.iter().zip(y.iter()).map(|(a, b)| a * b).sum::<f64>();
    let numerator = cross - (sum_x * sum_y / n);
    let denom_x = sumsq_x - (sum_x * sum_x / n);
    let denom_y = sumsq_y - (sum_y * sum_y / n);
    let denom = (denom_x.max(0.0) * denom_y.max(0.0)).sqrt();
    if denom > 0.0 {
        numerator / denom
    } else {
        0.0
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
        CorrelationMode::All => vec![
            CorrelationMethod::Pearson,
            CorrelationMethod::Spearman,
            CorrelationMethod::Cosine,
            CorrelationMethod::Euclidean,
            CorrelationMethod::Manhattan,
            CorrelationMethod::Mahalanobis,
        ],
        CorrelationMode::Pearson => vec![CorrelationMethod::Pearson],
        CorrelationMode::Spearman => vec![CorrelationMethod::Spearman],
        CorrelationMode::Cosine => vec![CorrelationMethod::Cosine],
        CorrelationMode::Euclidean => vec![CorrelationMethod::Euclidean],
        CorrelationMode::Manhattan => vec![CorrelationMethod::Manhattan],
        CorrelationMode::Mahalanobis => vec![CorrelationMethod::Mahalanobis],
    }
}

fn selected_embedding_methods(mode: EmbeddingMode) -> Vec<EmbeddingMethod> {
    match mode {
        EmbeddingMode::All => vec![
            EmbeddingMethod::Pca,
            EmbeddingMethod::Tsne,
            EmbeddingMethod::Umap,
            EmbeddingMethod::Pacmap,
        ],
        EmbeddingMode::Pca => vec![EmbeddingMethod::Pca],
        EmbeddingMode::Tsne => vec![EmbeddingMethod::Tsne],
        EmbeddingMode::Umap => vec![EmbeddingMethod::Umap],
        EmbeddingMode::Pacmap => vec![EmbeddingMethod::Pacmap],
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
    for (label, reader) in labels.iter().zip(readers.iter()) {
        eprintln!(
            "[kite analyze] input {} filters: {}",
            label,
            format_reader_filter(reader)
        );
    }
    let meta = ensure_compatible(&readers)?;
    let out_dir = Path::new(&args.output_dir);

    let matrix_path = out_dir.join("exact_count_matrix.tsv");
    let summary_path = out_dir.join("sample_summary.tsv");
    let heatmap_matrix_path = out_dir.join("heatmap_matrix.tsv");
    let heatmap_plot_matrix_path = out_dir.join("heatmap_plot_matrix.tsv");
    let heatmap_clusters_path = out_dir.join("heatmap_clusters.tsv");
    let heatmap_samples_path = out_dir.join("heatmap_samples.tsv");
    let quarto_path = out_dir.join("analysis_report.qmd");
    let correlation_methods = selected_correlation_methods(args.correlation);
    let embedding_methods = selected_embedding_methods(args.embedding);
    let needs_spearman = correlation_methods.contains(&CorrelationMethod::Spearman);
    let needs_distances = correlation_methods
        .iter()
        .any(|method| method.is_distance());
    let needs_dense_embedding = embedding_methods
        .iter()
        .any(|method| method.needs_dense_features());

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
    let mut distances = needs_distances.then(|| DistanceAccumulator::new(labels.len()));
    let mut spearman_columns =
        needs_spearman.then(|| (0..labels.len()).map(|_| Vec::new()).collect::<Vec<_>>());
    let mut embedding_rows: Vec<Vec<f64>> = Vec::new();
    let mut heatmap = OnlineHeatmap::new(args.heatmap_kmeans);
    let mut heatmap_plot_rows: Vec<(String, usize, Vec<f64>)> =
        Vec::with_capacity(HEATMAP_PLOT_MAX_ROWS as usize);
    let mut rows_written = 0u64;

    let progress = PartitionProgress::new("analyze", args.progress, meta.partitions, started);
    for partition_id in 0..meta.partitions {
        let mut vectors = Vec::with_capacity(readers.len());
        for reader in &readers {
            let mut rows = load_partition_counts(reader, partition_id)?;
            apply_reader_filter_to_rows(reader, &mut rows);
            vectors.push(rows);
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
            if let Some(distances) = distances.as_mut() {
                distances.update(&values);
            }
            if let Some(columns) = spearman_columns.as_mut() {
                push_columns(columns, &values);
            }
            if needs_dense_embedding {
                embedding_rows.push(values.clone());
            }
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
    if args.progress {
        eprintln!("[kite analyze] stage 2/4: writing heatmap summaries");
    }
    heatmap.write_centroids(&labels, &heatmap_clusters_path)?;
    write_sample_order(
        &labels,
        args.cluster_samples,
        &pearson,
        &heatmap_samples_path,
    )?;

    if args.progress {
        eprintln!(
            "[kite analyze] stage 3/4: writing {} correlation/distance table(s)",
            correlation_methods.len()
        );
    }
    for method in &correlation_methods {
        let path = out_dir.join(format!("correlation_{}.tsv", method.slug()));
        match method {
            CorrelationMethod::Pearson => pearson.write_tsv(&labels, &path)?,
            CorrelationMethod::Spearman => {
                let columns = spearman_columns
                    .as_ref()
                    .context("internal error: Spearman columns were not collected")?;
                write_spearman_tsv(columns, &labels, &path)?;
            }
            CorrelationMethod::Cosine => cosine.write_tsv(&labels, &path)?,
            CorrelationMethod::Euclidean
            | CorrelationMethod::Manhattan
            | CorrelationMethod::Mahalanobis => {
                let distances = distances
                    .as_ref()
                    .context("internal error: distance accumulators were not collected")?;
                distances.write_tsv(&labels, *method, &path)?;
            }
        }
    }

    let pca_scores = pca_scores(&pearson.covariance_matrix(labels.len()), labels.len(), 2);
    if args.progress {
        eprintln!(
            "[kite analyze] stage 4/4: writing {} embedding table(s)",
            embedding_methods.len()
        );
    }
    for method in &embedding_methods {
        let path = out_dir.join(format!("embedding_{}.tsv", method.slug()));
        let scores = match method {
            EmbeddingMethod::Pca => pca_scores.clone(),
            EmbeddingMethod::Tsne | EmbeddingMethod::Umap | EmbeddingMethod::Pacmap => {
                manifold_embedding(&embedding_rows, labels.len(), *method, args.heatmap_seed)
                    .with_context(|| format!("failed to compute {} embedding", method.slug()))?
            }
        };
        write_embedding_table(&labels, &scores, *method, &path)?;
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
    let order = if cluster_samples && labels.len() > 2 {
        average_linkage_order(labels.len(), |i, j| {
            1.0 - pearson.correlation(i, j, labels.len()).clamp(-1.0, 1.0)
        })
    } else {
        (0..labels.len()).collect()
    };
    let mut out = BufWriter::new(File::create(path)?);
    writeln!(out, "sample\torder")?;
    for (rank, idx) in order.iter().enumerate() {
        writeln!(out, "{}\t{}", labels[*idx], rank + 1)?;
    }
    out.flush()?;
    Ok(())
}

fn average_linkage_order<F>(n: usize, mut distance: F) -> Vec<usize>
where
    F: FnMut(usize, usize) -> f64,
{
    let mut clusters: Vec<Vec<usize>> = (0..n).map(|idx| vec![idx]).collect();
    while clusters.len() > 1 {
        let mut best = (0usize, 1usize);
        let mut best_dist = f64::INFINITY;
        for i in 0..clusters.len() {
            for j in (i + 1)..clusters.len() {
                let mut total = 0.0;
                let mut pairs = 0usize;
                for a in &clusters[i] {
                    for b in &clusters[j] {
                        total += distance(*a, *b);
                        pairs += 1;
                    }
                }
                let avg = total / pairs.max(1) as f64;
                if avg < best_dist {
                    best_dist = avg;
                    best = (i, j);
                }
            }
        }
        let right = clusters.remove(best.1);
        clusters[best.0].extend(right);
    }
    clusters.pop().unwrap_or_default()
}

fn pca_scores(cov: &[f64], n: usize, dims: usize) -> Vec<Vec<f64>> {
    let mut scores = vec![vec![0.0; dims]; n];
    if n == 0 || dims == 0 {
        return scores;
    }
    let matrix = DMatrix::from_row_slice(n, n, cov);
    let eigen = SymmetricEigen::new(matrix);
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| {
        eigen.eigenvalues[*b]
            .partial_cmp(&eigen.eigenvalues[*a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (dim, eig_idx) in order.into_iter().take(dims).enumerate() {
        let scale = eigen.eigenvalues[eig_idx].max(0.0).sqrt();
        for i in 0..n {
            scores[i][dim] = eigen.eigenvectors[(i, eig_idx)] * scale;
        }
    }
    scores
}

fn manifold_embedding(
    feature_rows: &[Vec<f64>],
    samples: usize,
    method: EmbeddingMethod,
    seed: u64,
) -> anyhow::Result<Vec<Vec<f64>>> {
    if samples == 0 {
        return Ok(Vec::new());
    }
    if samples < 4 || feature_rows.is_empty() {
        eprintln!(
            "[kite analyze] {} needs at least 4 samples and non-empty features; writing zero coordinates",
            method.slug()
        );
        return Ok(vec![vec![0.0; 2]; samples]);
    }
    let features = feature_rows.len();
    let matrix = Mat::from_fn(samples, features, |sample, feature| {
        feature_rows[feature][sample]
    });
    let seed = seed as usize;
    let embedding = match method {
        EmbeddingMethod::Tsne => {
            let max_perplexity = ((samples - 1) / 3).max(1) as f64;
            let perplexity = max_perplexity.min(30.0).max(1.0);
            let params = TsneParams::new(
                Some(2),
                Some(perplexity),
                None,
                Some(200.0),
                Some(750),
                Some("kmknn".to_string()),
                Some(0.5),
                Some(3),
            );
            tsne(matrix.as_ref(), None, &params, "bh", seed, false)?
        }
        EmbeddingMethod::Umap => {
            let k = (samples - 1).clamp(2, 15);
            let params = UmapParams::default_2d(Some(2), Some(k), Some(0.1), Some(1.0));
            umap(matrix.as_ref(), None, &params, seed, false)?
        }
        EmbeddingMethod::Pacmap => {
            let k = (samples - 1).clamp(2, 10);
            let mn_end = (samples - 1).clamp(k, 50);
            let params = PacmapParams::new(
                Some(2),
                Some(k),
                Some("kmknn".to_string()),
                Some("adam_parallel".to_string()),
                None,
                None,
                None,
                Some(mn_end),
                Some("pca".to_string()),
                None,
                None,
            );
            pacmap(matrix.as_ref(), None, &params, seed, false)?
        }
        EmbeddingMethod::Pca => unreachable!("PCA does not use manifold_embedding"),
    };
    embedding_to_scores(embedding, samples)
}

fn embedding_to_scores(embedding: Vec<Vec<f64>>, samples: usize) -> anyhow::Result<Vec<Vec<f64>>> {
    let mut scores = vec![vec![0.0; 2]; samples];
    for dim in 0..2 {
        let Some(values) = embedding.get(dim) else {
            continue;
        };
        if values.len() != samples {
            anyhow::bail!(
                "embedding dimension {} has {} rows, expected {}",
                dim + 1,
                values.len(),
                samples
            );
        }
        for sample in 0..samples {
            scores[sample][dim] = values[sample];
        }
    }
    Ok(scores)
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

The generated correlation and distance tables are: `{correlation_list}`. The default command writes only `correlation_pearson.tsv`; use `--correlation all` or a specific method to write additional tables.

```{{r}}
plot_correlation <- function(name, palette_low = "#2166AC", palette_high = "#B2182B") {{
  df <- read_kite_tsv(name)
  labels <- colnames(df)[-1]
  is_distance <- grepl("euclidean|manhattan|mahalanobis", name)
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
  p <- ggplot(long, aes(sample_x, sample_y, fill = value)) +
    geom_tile(color = "white", linewidth = 0.25) +
    coord_equal() +
    labs(x = NULL, y = NULL, fill = ifelse(is_distance, "distance", "correlation"), title = sub("\\\\.tsv$", "", name)) +
    kite_theme(base_size = 12) +
    theme(
      axis.text.x = element_text(angle = 45, hjust = 1),
      panel.grid = element_blank()
    )
  if (is_distance) {{
    p + scale_fill_viridis_c(option = "magma", direction = -1)
  }} else {{
    p + scale_fill_gradient2(low = palette_low, mid = "white", high = palette_high,
                             midpoint = 0, limits = c(-1, 1), oob = squish)
  }}
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(left: f64, right: f64) {
        assert!((left - right).abs() < 1e-8, "left={left}, right={right}");
    }

    #[test]
    fn average_ranks_handles_ties() {
        assert_eq!(
            average_ranks(&[10.0, 20.0, 20.0, 30.0]),
            vec![1.0, 2.5, 2.5, 4.0]
        );
        assert_eq!(
            average_ranks(&[3.0, 1.0, 3.0, 2.0]),
            vec![3.5, 1.0, 3.5, 2.0]
        );
    }

    #[test]
    fn pearson_slice_matches_expected_correlation() {
        assert_close(pearson_slice(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]), 1.0);
        assert_close(pearson_slice(&[1.0, 2.0, 3.0], &[6.0, 4.0, 2.0]), -1.0);
        assert_close(pearson_slice(&[1.0, 1.0, 1.0], &[2.0, 3.0, 4.0]), 0.0);
    }

    #[test]
    fn accumulators_compute_pearson_cosine_and_distances() {
        let rows = [vec![1.0, 2.0], vec![2.0, 4.0], vec![3.0, 6.0]];
        let mut pearson = PearsonAccumulator::new(2);
        let mut cosine = CosineAccumulator::new(2);
        let mut distances = DistanceAccumulator::new(2);
        for row in &rows {
            pearson.update(row);
            cosine.update(row);
            distances.update(row);
        }
        assert_close(pearson.correlation(0, 1, 2), 1.0);
        assert_close(cosine.correlation(0, 1, 2), 1.0);
        assert_close(
            distances.value(CorrelationMethod::Euclidean, 0, 1, 2),
            14.0_f64.sqrt(),
        );
        assert_close(distances.value(CorrelationMethod::Manhattan, 0, 1, 2), 6.0);
        assert!(distances.value(CorrelationMethod::Mahalanobis, 0, 1, 2) > 0.0);
    }

    #[test]
    fn selected_methods_expand_all_and_defaults() {
        let correlation = selected_correlation_methods(CorrelationMode::All)
            .into_iter()
            .map(|method| method.slug())
            .collect::<Vec<_>>();
        assert_eq!(
            correlation,
            vec![
                "pearson",
                "spearman",
                "cosine",
                "euclidean",
                "manhattan",
                "mahalanobis"
            ]
        );

        let embedding = selected_embedding_methods(EmbeddingMode::All)
            .into_iter()
            .map(|method| method.slug())
            .collect::<Vec<_>>();
        assert_eq!(embedding, vec!["pca", "tsne", "umap", "pacmap"]);
        assert_eq!(
            selected_embedding_methods(EmbeddingMode::Pca)[0].needs_dense_features(),
            false
        );
        assert!(selected_embedding_methods(EmbeddingMode::Umap)[0].needs_dense_features());
    }

    #[test]
    fn row_zscore_centers_and_scales_rows() {
        let scaled = row_zscore(&[1.0, 2.0, 3.0]);
        assert_close(scaled.iter().sum::<f64>(), 0.0);
        let variance = scaled.iter().map(|value| value * value).sum::<f64>() / scaled.len() as f64;
        assert_close(variance, 1.0);
        assert_eq!(row_zscore(&[5.0, 5.0]), vec![0.0, 0.0]);
    }

    #[test]
    fn average_linkage_order_keeps_closest_samples_adjacent() {
        let order = average_linkage_order(3, |i, j| match (i.min(j), i.max(j)) {
            (0, 1) => 0.1,
            (0, 2) => 5.0,
            (1, 2) => 4.0,
            _ => 0.0,
        });
        let pos0 = order.iter().position(|idx| *idx == 0).unwrap();
        let pos1 = order.iter().position(|idx| *idx == 1).unwrap();
        assert_eq!(pos0.abs_diff(pos1), 1);
    }

    #[test]
    fn pca_scores_returns_requested_shape() {
        let scores = pca_scores(&[2.0, 0.0, 0.0, 1.0], 2, 2);
        assert_eq!(scores.len(), 2);
        assert_eq!(scores[0].len(), 2);
        assert!(scores.iter().flatten().any(|value| value.abs() > 0.0));
    }

    #[test]
    fn embedding_to_scores_validates_dimensions() {
        let scores = embedding_to_scores(vec![vec![1.0, 2.0], vec![3.0, 4.0]], 2).unwrap();
        assert_eq!(scores, vec![vec![1.0, 3.0], vec![2.0, 4.0]]);
        assert!(embedding_to_scores(vec![vec![1.0]], 2).is_err());
    }

    #[test]
    fn online_heatmap_assigns_and_updates_centroids() {
        let mut heatmap = OnlineHeatmap::new(2);
        assert_eq!(heatmap.assign(&[1.0, 2.0, 3.0]), 1);
        assert_eq!(heatmap.assign(&[3.0, 2.0, 1.0]), 2);
        let cluster = heatmap.assign(&[1.1, 2.0, 2.9]);
        assert_eq!(cluster, 1);
        assert_eq!(heatmap.counts, vec![2, 1]);
    }
}
