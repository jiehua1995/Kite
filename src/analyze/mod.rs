use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use faer::Mat;
use manifolds_rs::prelude::{run_ann_search, NearestNeighbourParams};
use manifolds_rs::{pacmap, tsne, umap, PacmapParams, TsneParams, UmapParams};
use nalgebra::{DMatrix, SymmetricEigen};
use rayon::{prelude::*, ThreadPoolBuilder};

use crate::cli::{AnalyzeArgs, CorrelationMode, EmbeddingInputMode, EmbeddingMode};
use crate::kiteops::{
    apply_reader_filter_to_rows, ensure_compatible_with_paths, fill_key_buffer, for_each_union_row,
    format_reader_filter, infer_sample_labels_and_groups, load_partition_counts, open_readers,
    print_command_line, PartitionProgress,
};
use crate::stats::print_simple_test_report;

const HEATMAP_PLOT_MAX_ROWS: u64 = 5000;
const EMBEDDING_EXHAUSTIVE_MAX_SAMPLES: usize = 64;

type EmbeddingKnnGraph = (Vec<Vec<usize>>, Vec<Vec<f64>>);

#[derive(Copy, Clone, Eq, PartialEq)]
enum CorrelationMethod {
    Pearson,
    Spearman,
    Kendall,
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
            CorrelationMethod::Kendall => "kendall",
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
            CorrelationMethod::Kendall => "Kendall tau-b",
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

enum EmbeddingExecutionPlan {
    Pca,
    Tsne {
        min_samples: usize,
        perplexity: f64,
        learning_rate: f64,
        iterations: usize,
    },
    Umap {
        min_samples: usize,
        neighbors: usize,
        min_dist: f64,
        spread: f64,
    },
    Pacmap {
        min_samples: usize,
        neighbors: usize,
        mn_end: usize,
    },
}

impl EmbeddingExecutionPlan {
    fn method_name(&self) -> &'static str {
        match self {
            EmbeddingExecutionPlan::Pca => "pca",
            EmbeddingExecutionPlan::Tsne { .. } => "tsne",
            EmbeddingExecutionPlan::Umap { .. } => "umap",
            EmbeddingExecutionPlan::Pacmap { .. } => "pacmap",
        }
    }

    fn min_samples(&self) -> usize {
        match self {
            EmbeddingExecutionPlan::Pca => 1,
            EmbeddingExecutionPlan::Tsne { min_samples, .. }
            | EmbeddingExecutionPlan::Umap { min_samples, .. }
            | EmbeddingExecutionPlan::Pacmap { min_samples, .. } => *min_samples,
        }
    }

    fn knn_k(&self, samples: usize) -> Option<usize> {
        match self {
            EmbeddingExecutionPlan::Pca => None,
            EmbeddingExecutionPlan::Tsne { perplexity, .. } => Some(
                ((*perplexity * 3.0) as usize)
                    .max(5)
                    .min(samples.saturating_sub(1)),
            ),
            EmbeddingExecutionPlan::Umap { neighbors, .. } => Some(*neighbors),
            EmbeddingExecutionPlan::Pacmap { mn_end, .. } => Some(*mn_end),
        }
    }

    fn describe(&self, seed: u64, samples: usize) -> String {
        match self {
            EmbeddingExecutionPlan::Pca => {
                "deterministic; min_samples=1; covariance_eigendecomposition=true".to_string()
            }
            EmbeddingExecutionPlan::Tsne {
                min_samples,
                perplexity,
                learning_rate,
                iterations,
            } => format!(
                "seed={seed}; min_samples={min_samples}; perplexity={perplexity:.4}; learning_rate={learning_rate:.4}; iterations={iterations}; init=pca; knn={}; knn_k={}",
                embedding_ann_type(samples),
                self.knn_k(samples).unwrap_or(0)
            ),
            EmbeddingExecutionPlan::Umap {
                min_samples,
                neighbors,
                min_dist,
                spread,
            } => format!(
                "seed={seed}; min_samples={min_samples}; neighbors={neighbors}; min_dist={min_dist:.4}; spread={spread:.4}; knn={}; knn_k={}",
                embedding_ann_type(samples),
                self.knn_k(samples).unwrap_or(0)
            ),
            EmbeddingExecutionPlan::Pacmap {
                min_samples,
                neighbors,
                mn_end,
            } => format!(
                "seed={seed}; min_samples={min_samples}; neighbors={neighbors}; mn_end={mn_end}; init=pca; knn={}; knn_k={}; optimizer=adam_parallel",
                embedding_ann_type(samples),
                self.knn_k(samples).unwrap_or(0)
            ),
        }
    }
}

fn embedding_ann_type(samples: usize) -> &'static str {
    if samples <= EMBEDDING_EXHAUSTIVE_MAX_SAMPLES {
        "exhaustive"
    } else {
        "kmknn"
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

    fn merge_from(&mut self, other: &Self) {
        self.rows += other.rows;
        for (left, right) in self.sums.iter_mut().zip(other.sums.iter()) {
            *left += right;
        }
        for (left, right) in self.sumsq.iter_mut().zip(other.sumsq.iter()) {
            *left += right;
        }
        for (left, right) in self.cross.iter_mut().zip(other.cross.iter()) {
            *left += right;
        }
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

    fn merge_from(&mut self, other: &Self) {
        for (left, right) in self.sumsq.iter_mut().zip(other.sumsq.iter()) {
            *left += right;
        }
        for (left, right) in self.cross.iter_mut().zip(other.cross.iter()) {
            *left += right;
        }
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

    fn merge_from(&mut self, other: &Self) {
        for (left, right) in self
            .euclidean_sumsq
            .iter_mut()
            .zip(other.euclidean_sumsq.iter())
        {
            *left += right;
        }
        for (left, right) in self
            .manhattan_sum
            .iter_mut()
            .zip(other.manhattan_sum.iter())
        {
            *left += right;
        }
        for (left, right) in self
            .mahalanobis_diag_sumsq
            .iter_mut()
            .zip(other.mahalanobis_diag_sumsq.iter())
        {
            *left += right;
        }
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

fn count_inversions(values: &mut [f64], scratch: &mut [f64]) -> u128 {
    let len = values.len();
    if len <= 1 {
        return 0;
    }
    let mid = len / 2;
    let (left, right) = values.split_at_mut(mid);
    let (scratch_left, scratch_right) = scratch.split_at_mut(mid);
    let mut inversions =
        count_inversions(left, scratch_left) + count_inversions(right, scratch_right);
    let mut i = 0usize;
    let mut j = 0usize;
    let mut k = 0usize;
    while i < left.len() && j < right.len() {
        if left[i] <= right[j] {
            scratch[k] = left[i];
            i += 1;
        } else {
            scratch[k] = right[j];
            inversions += (left.len() - i) as u128;
            j += 1;
        }
        k += 1;
    }
    while i < left.len() {
        scratch[k] = left[i];
        i += 1;
        k += 1;
    }
    while j < right.len() {
        scratch[k] = right[j];
        j += 1;
        k += 1;
    }
    values.copy_from_slice(&scratch[..len]);
    inversions
}

fn tie_pairs_sorted(values: &[f64]) -> u128 {
    if values.len() < 2 {
        return 0;
    }
    let mut ties = 0u128;
    let mut start = 0usize;
    while start < values.len() {
        let mut end = start + 1;
        while end < values.len() && values[start] == values[end] {
            end += 1;
        }
        let count = (end - start) as u128;
        ties += count.saturating_mul(count.saturating_sub(1)) / 2;
        start = end;
    }
    ties
}

fn kendall_tau_b(x: &[f64], y: &[f64]) -> f64 {
    if x.len() != y.len() || x.len() < 2 {
        return 0.0;
    }
    let mut pairs = x.iter().copied().zip(y.iter().copied()).collect::<Vec<_>>();
    pairs.sort_by(|(x1, y1), (x2, y2)| x1.total_cmp(x2).then(y1.total_cmp(y2)));

    let n = pairs.len() as u128;
    let total_pairs = n.saturating_mul(n.saturating_sub(1)) / 2;

    let mut ties_x = 0u128;
    let mut ties_xy = 0u128;
    let mut start = 0usize;
    while start < pairs.len() {
        let mut end = start + 1;
        while end < pairs.len() && pairs[start].0 == pairs[end].0 {
            end += 1;
        }
        let count_x = (end - start) as u128;
        ties_x += count_x.saturating_mul(count_x.saturating_sub(1)) / 2;

        let mut joint_start = start;
        while joint_start < end {
            let mut joint_end = joint_start + 1;
            while joint_end < end && pairs[joint_start] == pairs[joint_end] {
                joint_end += 1;
            }
            let count_xy = (joint_end - joint_start) as u128;
            ties_xy += count_xy.saturating_mul(count_xy.saturating_sub(1)) / 2;
            joint_start = joint_end;
        }
        start = end;
    }

    let mut y_sorted = pairs.iter().map(|(_, yv)| *yv).collect::<Vec<_>>();
    y_sorted.sort_by(|a, b| a.total_cmp(b));
    let ties_y = tie_pairs_sorted(&y_sorted);

    let mut y_values = pairs.iter().map(|(_, yv)| *yv).collect::<Vec<_>>();
    let mut scratch = vec![0.0; y_values.len()];
    let discordant = count_inversions(&mut y_values, &mut scratch);

    let numerator = total_pairs as i128 - ties_x as i128 - ties_y as i128 + ties_xy as i128
        - 2 * discordant as i128;
    let denom_x = total_pairs.saturating_sub(ties_x) as f64;
    let denom_y = total_pairs.saturating_sub(ties_y) as f64;
    let denominator = (denom_x * denom_y).sqrt();
    if denominator > 0.0 {
        numerator as f64 / denominator
    } else {
        0.0
    }
}

fn write_kendall_tsv(columns: &[Vec<f64>], labels: &[String], path: &Path) -> anyhow::Result<()> {
    if columns.is_empty() {
        return write_square_matrix(labels, path, |_, _, _| 0.0);
    }
    write_square_matrix(labels, path, |i, j, _| {
        kendall_tau_b(&columns[i], &columns[j])
    })
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
    centroids: Vec<f64>,
    counts: Vec<u64>,
    k: usize,
    dims: usize,
}

impl OnlineHeatmap {
    fn new(k: usize) -> Self {
        Self {
            centroids: Vec::new(),
            counts: Vec::new(),
            k: k.max(1),
            dims: 0,
        }
    }

    fn assign_scaled(&mut self, scaled: &[f64]) -> usize {
        if self.dims == 0 {
            self.dims = scaled.len();
        }
        if self.counts.len() < self.k {
            self.centroids.extend_from_slice(scaled);
            self.counts.push(1);
            return self.counts.len();
        }
        let mut best = 0usize;
        let mut best_dist = f64::INFINITY;
        for idx in 0..self.counts.len() {
            let centroid = &self.centroids[idx * self.dims..(idx + 1) * self.dims];
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
        let centroid = &mut self.centroids[best * self.dims..(best + 1) * self.dims];
        for (c, v) in centroid.iter_mut().zip(scaled.iter()) {
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
        for (idx, _count) in self.counts.iter().enumerate() {
            let centroid = &self.centroids[idx * self.dims..(idx + 1) * self.dims];
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

fn validate_analyze_args(args: &AnalyzeArgs) -> anyhow::Result<()> {
    if args.embedding_input_dims == 0 {
        anyhow::bail!("--embedding-input-dims must be >= 1");
    }
    if let Some(perplexity) = args.tsne_perplexity {
        if !perplexity.is_finite() || perplexity <= 0.0 {
            anyhow::bail!("--tsne-perplexity must be > 0");
        }
    }
    if !args.tsne_learning_rate.is_finite() || args.tsne_learning_rate <= 0.0 {
        anyhow::bail!("--tsne-learning-rate must be > 0");
    }
    if args.tsne_iterations == 0 {
        anyhow::bail!("--tsne-iterations must be >= 1");
    }
    if let Some(neighbors) = args.umap_neighbors {
        if neighbors < 2 {
            anyhow::bail!("--umap-neighbors must be >= 2");
        }
    }
    if !args.umap_min_dist.is_finite() || args.umap_min_dist < 0.0 {
        anyhow::bail!("--umap-min-dist must be >= 0");
    }
    if !args.umap_spread.is_finite() || args.umap_spread <= 0.0 {
        anyhow::bail!("--umap-spread must be > 0");
    }
    if let Some(neighbors) = args.pacmap_neighbors {
        if neighbors < 2 {
            anyhow::bail!("--pacmap-neighbors must be >= 2");
        }
    }
    if let Some(mn_end) = args.pacmap_mn_end {
        if mn_end < 1 {
            anyhow::bail!("--pacmap-mn-end must be >= 1");
        }
    }
    Ok(())
}

fn build_embedding_plan(
    args: &AnalyzeArgs,
    method: EmbeddingMethod,
    samples: usize,
) -> EmbeddingExecutionPlan {
    match method {
        EmbeddingMethod::Pca => EmbeddingExecutionPlan::Pca,
        EmbeddingMethod::Tsne => {
            let max_perplexity = ((samples.saturating_sub(1)) / 3).max(1) as f64;
            let perplexity = args
                .tsne_perplexity
                .unwrap_or(max_perplexity.min(30.0).max(1.0))
                .clamp(1.0, max_perplexity.max(1.0));
            EmbeddingExecutionPlan::Tsne {
                min_samples: 4,
                perplexity,
                learning_rate: args.tsne_learning_rate,
                iterations: args.tsne_iterations.max(1),
            }
        }
        EmbeddingMethod::Umap => {
            let max_neighbors = samples.saturating_sub(1).max(2);
            let neighbors = args
                .umap_neighbors
                .unwrap_or(samples.saturating_sub(1).clamp(2, 15))
                .clamp(2, max_neighbors);
            EmbeddingExecutionPlan::Umap {
                min_samples: 4,
                neighbors,
                min_dist: args.umap_min_dist,
                spread: args.umap_spread,
            }
        }
        EmbeddingMethod::Pacmap => {
            let max_neighbors = samples.saturating_sub(1).max(2);
            let neighbors = args
                .pacmap_neighbors
                .unwrap_or(samples.saturating_sub(1).clamp(2, 10))
                .clamp(2, max_neighbors);
            let max_mn_end = samples.saturating_sub(1).max(neighbors);
            let mn_end = args
                .pacmap_mn_end
                .unwrap_or(samples.saturating_sub(1).clamp(neighbors, 50))
                .clamp(neighbors, max_mn_end);
            EmbeddingExecutionPlan::Pacmap {
                min_samples: 4,
                neighbors,
                mn_end,
            }
        }
    }
}

fn selected_correlation_methods(mode: CorrelationMode) -> Vec<CorrelationMethod> {
    match mode {
        CorrelationMode::All => vec![
            CorrelationMethod::Pearson,
            CorrelationMethod::Spearman,
            CorrelationMethod::Kendall,
            CorrelationMethod::Cosine,
            CorrelationMethod::Euclidean,
            CorrelationMethod::Manhattan,
            CorrelationMethod::Mahalanobis,
        ],
        CorrelationMode::Pearson => vec![CorrelationMethod::Pearson],
        CorrelationMode::Spearman => vec![CorrelationMethod::Spearman],
        CorrelationMode::Kendall => vec![CorrelationMethod::Kendall],
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

fn fill_transformed_values(counts: &[u64], out: &mut [f64]) {
    for (dst, count) in out.iter_mut().zip(counts.iter()) {
        *dst = (*count as f64).ln_1p();
    }
}

#[cfg(test)]
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

fn fill_row_zscore(values: &[f64], out: &mut [f64]) {
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
        for (dst, value) in out.iter_mut().zip(values.iter()) {
            *dst = (*value - mean) / sd;
        }
    } else {
        out.fill(0.0);
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
    print_command_line("analyze");
    let started = Instant::now();
    validate_analyze_args(args)?;
    std::fs::create_dir_all(&args.output_dir)?;

    let (labels, groups) = infer_sample_labels_and_groups(
        &args.inputs,
        args.labels.as_deref().unwrap_or(&[]),
        args.groups.as_deref().unwrap_or(&[]),
    )?;
    eprintln!(
        "[kite analyze] inputs={}, output_dir={}",
        args.inputs.len(),
        args.output_dir
    );
    let readers = open_readers(&args.inputs)?;
    eprintln!(
        "[kite analyze] labels={} ({})",
        labels.join(","),
        if args.labels.is_some() {
            "user supplied"
        } else {
            "inferred from file names"
        }
    );
    eprintln!(
        "[kite analyze] groups={} ({})",
        groups.join(","),
        if args.groups.is_some() {
            "user supplied"
        } else {
            "inferred from sample labels"
        }
    );
    for (index, ((path, label), reader)) in args
        .inputs
        .iter()
        .zip(labels.iter())
        .zip(readers.iter())
        .enumerate()
    {
        eprintln!(
            "[kite analyze] input #{:>2} label={} path={} k={} canonical={} partitions={} key_width={} rows={} chunks={} filters: {}",
            index + 1,
            label,
            path,
            reader.header.k,
            reader.header.canonical != 0,
            reader.header.partitions,
            reader.header.key_width,
            reader.header.total_rows,
            reader.total_chunks,
            format_reader_filter(reader)
        );
    }
    let meta = ensure_compatible_with_paths(&readers, &args.inputs)?;
    let worker_threads = if args.threads == 0 {
        num_cpus::get().max(1)
    } else {
        args.threads.max(1)
    };
    eprintln!(
        "[kite analyze] worker_threads={} (parallel partition prefetch in batches)",
        worker_threads
    );
    let pool = ThreadPoolBuilder::new()
        .num_threads(worker_threads)
        .thread_name(|index| format!("kite-analyze-{index}"))
        .build()
        .ok();
    if pool.is_none() && worker_threads > 1 {
        eprintln!(
            "[kite analyze] failed to create local worker pool; falling back to serial partition processing"
        );
    }
    let out_dir = Path::new(&args.output_dir);

    let matrix_path = out_dir.join("exact_count_matrix.tsv");
    let summary_path = out_dir.join("sample_summary.tsv");
    let heatmap_matrix_path = out_dir.join("heatmap_matrix.tsv");
    let heatmap_plot_matrix_path = out_dir.join("heatmap_plot_matrix.tsv");
    let heatmap_clusters_path = out_dir.join("heatmap_clusters.tsv");
    let heatmap_samples_path = out_dir.join("heatmap_samples.tsv");
    let quarto_path = out_dir.join("analysis_report.qmd");
    let markdown_report_path = out_dir.join("analysis_report.md");
    let correlation_methods = selected_correlation_methods(args.correlation);
    let embedding_methods = selected_embedding_methods(args.embedding);
    let needs_rank_columns = correlation_methods.contains(&CorrelationMethod::Spearman)
        || correlation_methods.contains(&CorrelationMethod::Kendall);
    let needs_distances = correlation_methods
        .iter()
        .any(|method| method.is_distance());
    let needs_dense_embedding = embedding_methods
        .iter()
        .any(|method| method.needs_dense_features());
    let embedding_plans = embedding_methods
        .iter()
        .map(|method| build_embedding_plan(args, *method, labels.len()))
        .collect::<Vec<_>>();
    eprintln!(
        "[kite analyze] analysis: correlation={:?}, embedding={:?}, embedding_input={:?}, embedding_input_dims={}, heatmap_kmeans={}, seed={}, cluster_samples={}",
        args.correlation,
        args.embedding,
        args.embedding_input,
        args.embedding_input_dims,
        args.heatmap_kmeans,
        args.seed,
        args.cluster_samples,
    );
    eprintln!(
        "[kite analyze] heatmap: online clusters={} (deterministic assignment; seed retained for stochastic embedding methods)",
        args.heatmap_kmeans.max(1)
    );
    for (method, plan) in embedding_methods.iter().zip(embedding_plans.iter()) {
        eprintln!(
            "[kite analyze] embedding {}: {}",
            method.slug(),
            plan.describe(args.seed, labels.len())
        );
    }

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
    let mut rank_columns =
        needs_rank_columns.then(|| (0..labels.len()).map(|_| Vec::new()).collect::<Vec<_>>());
    let mut embedding_rows: Vec<f64> = Vec::new();
    let mut heatmap = OnlineHeatmap::new(args.heatmap_kmeans);
    let mut rows_written = 0u64;
    let sample_count = labels.len();

    let progress = PartitionProgress::new("analyze", args.progress, meta.partitions, started);
    let batch_size = worker_threads.max(1);
    for batch_start in (0..meta.partitions).step_by(batch_size) {
        let batch_end = (batch_start + batch_size).min(meta.partitions);
        let batch_ids = (batch_start..batch_end).collect::<Vec<_>>();
        let mut batch_chunks = if let Some(pool) = &pool {
            pool.install(|| {
                batch_ids
                    .into_par_iter()
                    .map(|partition_id| {
                        build_analyze_partition_chunk(
                            &readers,
                            &meta,
                            partition_id,
                            sample_count,
                            needs_rank_columns,
                            needs_dense_embedding,
                            needs_distances,
                        )
                    })
                    .collect::<anyhow::Result<Vec<_>>>()
            })?
        } else {
            batch_ids
                .into_iter()
                .map(|partition_id| {
                    build_analyze_partition_chunk(
                        &readers,
                        &meta,
                        partition_id,
                        sample_count,
                        needs_rank_columns,
                        needs_dense_embedding,
                        needs_distances,
                    )
                })
                .collect::<anyhow::Result<Vec<_>>>()?
        };
        batch_chunks.sort_by_key(|chunk| chunk.partition_id);
        for chunk in batch_chunks {
            matrix.write_all(&chunk.matrix_buf)?;
            pearson.merge_from(&chunk.pearson);
            cosine.merge_from(&chunk.cosine);
            if let (Some(global), Some(local)) = (distances.as_mut(), chunk.distances.as_ref()) {
                global.merge_from(local);
            }
            for (left, right) in sample_totals.iter_mut().zip(chunk.sample_totals.iter()) {
                *left = left.saturating_add(*right);
            }
            for (left, right) in sample_nonzero.iter_mut().zip(chunk.sample_nonzero.iter()) {
                *left = left.saturating_add(*right);
            }
            if let (Some(global), Some(local)) = (rank_columns.as_mut(), chunk.rank_columns) {
                for (dst, src) in global.iter_mut().zip(local.into_iter()) {
                    dst.extend(src);
                }
            }
            if needs_dense_embedding {
                embedding_rows.extend_from_slice(&chunk.embedding_values);
            }
            let mut heatmap_buf = Vec::with_capacity(chunk.row_count * (sample_count * 12 + 32));
            let mut plot_buf = Vec::new();
            for row_index in 0..chunk.row_count {
                let offset = row_index * sample_count;
                let scaled = &chunk.scaled_values[offset..offset + sample_count];
                let cluster = heatmap.assign_scaled(scaled);
                let kmer_start = row_index * chunk.kmer_width;
                let kmer = &chunk.kmers[kmer_start..kmer_start + chunk.kmer_width];
                write_heatmap_row(&mut heatmap_buf, kmer, cluster, scaled)?;
                if rows_written < HEATMAP_PLOT_MAX_ROWS {
                    write_heatmap_row(&mut plot_buf, kmer, cluster, scaled)?;
                }
                rows_written += 1;
            }
            heatmap_matrix.write_all(&heatmap_buf)?;
            if !plot_buf.is_empty() {
                heatmap_plot_matrix.write_all(&plot_buf)?;
            }
            progress.update(chunk.partition_id + 1);
        }
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
                let columns = rank_columns
                    .as_ref()
                    .context("internal error: Spearman columns were not collected")?;
                write_spearman_tsv(columns, &labels, &path)?;
            }
            CorrelationMethod::Kendall => {
                let columns = rank_columns
                    .as_ref()
                    .context("internal error: Kendall columns were not collected")?;
                write_kendall_tsv(columns, &labels, &path)?;
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

    let manifold_samples = labels.len();
    let requested_embedding_pca_dims = args.embedding_input_dims.max(1);
    let effective_embedding_pca_dims =
        requested_embedding_pca_dims.min(manifold_samples.saturating_sub(1).max(1));
    let pca_basis_dims = match args.embedding_input {
        EmbeddingInputMode::Full => 2,
        EmbeddingInputMode::Pca => requested_embedding_pca_dims.max(2),
    };
    let all_pca_scores = pca_scores(
        &pearson.covariance_matrix(labels.len()),
        labels.len(),
        pca_basis_dims,
    );
    let pca_scores = all_pca_scores
        .iter()
        .map(|row| {
            let mut coords = vec![0.0; 2];
            for dim in 0..2.min(row.len()) {
                coords[dim] = row[dim];
            }
            coords
        })
        .collect::<Vec<_>>();
    let manifold_features = rows_written as usize;
    let empty_manifold_matrix = Mat::<f64>::zeros(manifold_samples, 0);
    let manifold_matrix = if needs_dense_embedding && manifold_samples > 0 {
        match args.embedding_input {
            EmbeddingInputMode::Full if manifold_features > 0 => Some(Mat::from_fn(
                manifold_samples,
                manifold_features,
                |sample, feature| embedding_rows[feature * manifold_samples + sample],
            )),
            EmbeddingInputMode::Pca => Some(Mat::from_fn(
                manifold_samples,
                effective_embedding_pca_dims,
                |sample, feature| all_pca_scores[sample][feature],
            )),
            EmbeddingInputMode::Full => None,
        }
    } else {
        None
    };
    let shared_manifold_knn = manifold_matrix.as_ref().and_then(|matrix| {
        let shared_k = embedding_plans
            .iter()
            .filter(|plan| {
                !matches!(plan, EmbeddingExecutionPlan::Pca) && manifold_samples >= plan.min_samples()
            })
            .filter_map(|plan| plan.knn_k(manifold_samples))
            .max();
        shared_k.map(|knn_k| {
            let ann_type = embedding_ann_type(manifold_samples);
            let input_note = match args.embedding_input {
                EmbeddingInputMode::Full => "full".to_string(),
                EmbeddingInputMode::Pca => format!(
                    "pca(requested={}, effective={})",
                    requested_embedding_pca_dims,
                    matrix.ncols()
                ),
            };
            eprintln!(
                "[kite analyze] embedding manifold input={}; shared_kNN=true; ann={}; k={}; samples={}; features={}",
                input_note,
                ann_type,
                knn_k,
                manifold_samples,
                matrix.ncols()
            );
            run_ann_search(
                matrix.as_ref(),
                knn_k,
                ann_type.to_string(),
                &NearestNeighbourParams::default(),
                args.seed as usize,
                false,
            )
        })
    });
    if args.progress {
        eprintln!(
            "[kite analyze] stage 4/4: writing {} embedding table(s)",
            embedding_methods.len()
        );
    }
    let mut written_embedding_methods = Vec::new();
    let mut skipped_embedding_methods = Vec::new();
    for (method, plan) in embedding_methods.iter().zip(embedding_plans.iter()) {
        let path = out_dir.join(format!("embedding_{}.tsv", method.slug()));
        let maybe_scores = match method {
            EmbeddingMethod::Pca => Some(pca_scores.clone()),
            EmbeddingMethod::Tsne | EmbeddingMethod::Umap | EmbeddingMethod::Pacmap => {
                let matrix = manifold_matrix.as_ref().unwrap_or(&empty_manifold_matrix);
                manifold_embedding(
                    matrix.as_ref(),
                    manifold_samples,
                    plan,
                    args.seed,
                    shared_manifold_knn.as_ref(),
                )
                .with_context(|| format!("failed to compute {} embedding", method.slug()))?
            }
        };
        if let Some(scores) = maybe_scores {
            write_embedding_table(&labels, &groups, &scores, *method, &path)?;
            written_embedding_methods.push(*method);
        } else {
            skipped_embedding_methods.push(*method);
        }
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
            &groups,
            &correlation_methods,
            &embedding_methods,
            args.heatmap_kmeans,
            args.cluster_samples,
        ),
    )?;
    write_analysis_markdown_report(
        &markdown_report_path,
        args,
        &labels,
        &groups,
        &readers,
        rows_written,
        worker_threads,
        &correlation_methods,
        &embedding_methods,
        &written_embedding_methods,
        &embedding_plans,
        &skipped_embedding_methods,
        started.elapsed().as_secs_f64(),
    )?;

    let correlation_labels = correlation_methods
        .iter()
        .map(|method| method.display_name())
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "[kite analyze] wrote matrix, {} correlation table(s), {} embedding table(s), heatmap tables, Quarto report, and Markdown report in {:.3}s",
        correlation_labels,
        written_embedding_methods.len(),
        started.elapsed().as_secs_f64()
    );
    if args.test_mode {
        print_simple_test_report(
            "analyze",
            &[
                ("inputs", args.inputs.len().to_string()),
                ("labels", labels.join(",")),
                ("groups", groups.join(",")),
                ("threads", args.threads.to_string()),
                ("worker_threads", worker_threads.to_string()),
                ("partitions", meta.partitions.to_string()),
                ("matrix_rows", rows_written.to_string()),
                (
                    "correlation",
                    format!("{:?}", args.correlation).to_lowercase(),
                ),
                ("embedding", format!("{:?}", args.embedding).to_lowercase()),
                (
                    "embedding_input",
                    format!("{:?}", args.embedding_input).to_lowercase(),
                ),
                (
                    "embedding_input_dims",
                    args.embedding_input_dims.to_string(),
                ),
                ("heatmap_kmeans", args.heatmap_kmeans.to_string()),
                ("seed", args.seed.to_string()),
                ("cluster_samples", args.cluster_samples.to_string()),
                (
                    "heatmap_plot_rows",
                    rows_written.min(HEATMAP_PLOT_MAX_ROWS).to_string(),
                ),
                ("output_dir", args.output_dir.clone()),
                (
                    "markdown_report",
                    markdown_report_path.display().to_string(),
                ),
            ],
            started.elapsed(),
        );
    }
    Ok(())
}

// Writes a human-readable Markdown report summarizing the analysis parameters, inputs, and outputs. It may be better to use .html format in the future, but .md is more universally viewable and can be easily converted to .html if needed.
// [FUTURE PLAN]

fn write_analysis_markdown_report(
    path: &Path,
    args: &AnalyzeArgs,
    labels: &[String],
    groups: &[String],
    readers: &[std::sync::Arc<crate::dump::reader::KiteReader>],
    rows_written: u64,
    worker_threads: usize,
    correlation_methods: &[CorrelationMethod],
    embedding_methods: &[EmbeddingMethod],
    written_embedding_methods: &[EmbeddingMethod],
    embedding_plans: &[EmbeddingExecutionPlan],
    skipped_embedding_methods: &[EmbeddingMethod],
    elapsed_secs: f64,
) -> anyhow::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    writeln!(out, "# Kite Analyze Report")?;
    writeln!(out)?;
    writeln!(out, "## Summary")?;
    writeln!(out)?;
    writeln!(out, "| Field | Value |")?;
    writeln!(out, "|---|---:|")?;
    writeln!(out, "| Inputs | {} |", args.inputs.len())?;
    writeln!(
        out,
        "| Groups | {} |",
        if args.groups.is_some() {
            groups.join(", ")
        } else {
            "default single group".to_string()
        }
    )?;
    writeln!(out, "| Matrix rows | {} |", rows_written)?;
    writeln!(out, "| Threads requested | {} |", args.threads)?;
    writeln!(out, "| Worker threads used | {} |", worker_threads)?;
    writeln!(out, "| Heatmap clusters | {} |", args.heatmap_kmeans)?;
    writeln!(out, "| Embedding input | {:?} |", args.embedding_input)?;
    writeln!(
        out,
        "| Embedding input dims | {} |",
        args.embedding_input_dims
    )?;
    writeln!(out, "| Seed | {} |", args.seed)?;
    writeln!(out, "| Cluster samples | {} |", args.cluster_samples)?;
    writeln!(out, "| Elapsed seconds | {:.3} |", elapsed_secs)?;
    writeln!(out)?;
    writeln!(out, "## Methods")?;
    writeln!(out)?;
    writeln!(
        out,
        "- Correlation/distance: {}",
        correlation_methods
            .iter()
            .map(|method| method.display_name())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(
        out,
        "- Embedding: {}",
        embedding_methods
            .iter()
            .map(|method| method.slug())
            .collect::<Vec<_>>()
            .join(", ")
    )?;
    writeln!(
        out,
        "- Embedding input: {:?} (requested dims: {}).",
        args.embedding_input, args.embedding_input_dims
    )?;
    if !written_embedding_methods.is_empty() {
        writeln!(
            out,
            "- Written embeddings: {}",
            written_embedding_methods
                .iter()
                .map(|method| method.slug())
                .collect::<Vec<_>>()
                .join(", ")
        )?;
    }
    for (method, plan) in written_embedding_methods.iter().filter_map(|method| {
        embedding_methods
            .iter()
            .position(|candidate| candidate == method)
            .map(|index| (method, &embedding_plans[index]))
    }) {
        writeln!(
            out,
            "- Embedding `{}` settings: {}",
            method.slug(),
            plan.describe(args.seed, labels.len())
        )?;
    }
    if !skipped_embedding_methods.is_empty() {
        writeln!(
            out,
            "- Skipped embeddings: {}",
            skipped_embedding_methods
                .iter()
                .map(|method| method.slug())
                .collect::<Vec<_>>()
                .join(", ")
        )?;
    }
    writeln!(out, "- Count transform: `log1p(raw_count)`.")?;
    writeln!(
        out,
        "- Mahalanobis mode, when selected, uses a streaming diagonal covariance approximation."
    )?;
    writeln!(
        out,
        "- Manifold embeddings are skipped when sample count is below the method minimum or when the feature matrix is empty."
    )?;
    writeln!(out)?;
    writeln!(out, "## Inputs")?;
    writeln!(out)?;
    writeln!(
        out,
        "| # | Label | Group | Path | k | Canonical | Partitions | Key width | Rows | Chunks | Min | Max |"
    )?;
    writeln!(
        out,
        "|---:|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|"
    )?;
    for (index, (((input, label), group), reader)) in args
        .inputs
        .iter()
        .zip(labels.iter())
        .zip(groups.iter())
        .zip(readers.iter())
        .enumerate()
    {
        writeln!(
            out,
            "| {} | {} | {} | `{}` | {} | {} | {} | {} | {} | {} | {} | {} |",
            index + 1,
            label,
            group,
            input,
            reader.header.k,
            reader.header.canonical != 0,
            reader.header.partitions,
            reader.header.key_width,
            reader.header.total_rows,
            reader.total_chunks,
            crate::kiteops::format_count_filter(reader.header.min_count),
            crate::kiteops::format_count_filter(reader.header.max_count),
        )?;
    }
    writeln!(out)?;
    writeln!(out, "## Output Files")?;
    writeln!(out)?;
    writeln!(out, "| File | Size bytes |")?;
    writeln!(out, "|---|---:|")?;
    for name in [
        "exact_count_matrix.tsv",
        "sample_summary.tsv",
        "heatmap_matrix.tsv",
        "heatmap_plot_matrix.tsv",
        "heatmap_clusters.tsv",
        "heatmap_samples.tsv",
        "analysis_report.qmd",
    ] {
        let file_path = Path::new(&args.output_dir).join(name);
        let size = std::fs::metadata(&file_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        writeln!(out, "| `{}` | {} |", name, size)?;
    }
    for method in correlation_methods {
        let name = format!("correlation_{}.tsv", method.slug());
        let file_path = Path::new(&args.output_dir).join(&name);
        let size = std::fs::metadata(&file_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        writeln!(out, "| `{}` | {} |", name, size)?;
    }
    for method in written_embedding_methods {
        let name = format!("embedding_{}.tsv", method.slug());
        let file_path = Path::new(&args.output_dir).join(&name);
        let size = std::fs::metadata(&file_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        writeln!(out, "| `{}` | {} |", name, size)?;
    }
    writeln!(out)?;
    writeln!(out, "This report: `{}`", path.display())?;
    out.flush()?;
    Ok(())
}

fn write_heatmap_row<W: Write>(
    out: &mut W,
    kmer: &[u8],
    cluster: usize,
    scaled: &[f64],
) -> anyhow::Result<()> {
    out.write_all(kmer)?;
    write!(out, "\t{}", cluster)?;
    for value in scaled {
        write!(out, "\t{:.8}", value)?;
    }
    writeln!(out)?;
    Ok(())
}

fn append_exact_matrix_row<W: Write>(
    out: &mut W,
    kmer: &[u8],
    counts: &[u64],
) -> anyhow::Result<()> {
    out.write_all(kmer)?;
    for count in counts {
        write!(out, "\t{}", count)?;
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
    matrix: faer::MatRef<'_, f64>,
    samples: usize,
    plan: &EmbeddingExecutionPlan,
    seed: u64,
    precomputed_knn: Option<&EmbeddingKnnGraph>,
) -> anyhow::Result<Option<Vec<Vec<f64>>>> {
    if samples == 0 {
        return Ok(None);
    }
    if samples < plan.min_samples() || matrix.ncols() == 0 {
        let method = plan.method_name();
        eprintln!(
            "[kite analyze] skipping {}: needs at least {} samples and non-empty features",
            method,
            plan.min_samples()
        );
        return Ok(None);
    }
    let seed = seed as usize;
    let embedding = match plan {
        EmbeddingExecutionPlan::Tsne {
            perplexity,
            learning_rate,
            iterations,
            ..
        } => {
            let params = TsneParams::new(
                Some(2),
                Some(*perplexity),
                None,
                Some(*learning_rate),
                Some(*iterations),
                Some(embedding_ann_type(samples).to_string()),
                Some(0.5),
                Some(3),
            );
            tsne(matrix, precomputed_knn.cloned(), &params, "bh", seed, false)?
        }
        EmbeddingExecutionPlan::Umap {
            neighbors,
            min_dist,
            spread,
            ..
        } => {
            let params =
                UmapParams::default_2d(Some(2), Some(*neighbors), Some(*min_dist), Some(*spread));
            umap(matrix, precomputed_knn.cloned(), &params, seed, false)?
        }
        EmbeddingExecutionPlan::Pacmap {
            neighbors, mn_end, ..
        } => {
            let params = PacmapParams::new(
                Some(2),
                Some(*neighbors),
                Some(embedding_ann_type(samples).to_string()),
                Some("adam_parallel".to_string()),
                None,
                None,
                None,
                Some(*mn_end),
                Some("pca".to_string()),
                None,
                None,
            );
            pacmap(matrix, precomputed_knn.cloned(), &params, seed, false)?
        }
        EmbeddingExecutionPlan::Pca => unreachable!("PCA does not use manifold_embedding"),
    };
    embedding_to_scores(embedding, samples).map(Some)
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
    groups: &[String],
    pca_scores: &[Vec<f64>],
    method: EmbeddingMethod,
    path: &Path,
) -> anyhow::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    writeln!(out, "sample\tgroup\tmethod\tdim1\tdim2")?;
    for (idx, label) in labels.iter().enumerate() {
        let group = groups.get(idx).map(String::as_str).unwrap_or("all");
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
        writeln!(
            out,
            "{}\t{}\t{}\t{:.8}\t{:.8}",
            label,
            group,
            method.slug(),
            x,
            y
        )?;
    }
    out.flush()?;
    Ok(())
}

struct PartitionAnalyzeChunk {
    partition_id: usize,
    kmers: Vec<u8>,
    kmer_width: usize,
    matrix_buf: Vec<u8>,
    sample_totals: Vec<u64>,
    sample_nonzero: Vec<u64>,
    pearson: PearsonAccumulator,
    cosine: CosineAccumulator,
    distances: Option<DistanceAccumulator>,
    rank_columns: Option<Vec<Vec<f64>>>,
    embedding_values: Vec<f64>,
    scaled_values: Vec<f64>,
    row_count: usize,
}

fn build_analyze_partition_chunk(
    readers: &[std::sync::Arc<crate::dump::reader::KiteReader>],
    meta: &crate::kiteops::CompatibleMetadata,
    partition_id: usize,
    sample_count: usize,
    needs_rank_columns: bool,
    needs_dense_embedding: bool,
    needs_distances: bool,
) -> anyhow::Result<PartitionAnalyzeChunk> {
    let mut vectors = Vec::with_capacity(readers.len());
    for reader in readers {
        let mut rows = load_partition_counts(reader, partition_id)?;
        apply_reader_filter_to_rows(reader, &mut rows);
        vectors.push(rows);
    }
    let row_count_estimate: usize = vectors.iter().map(Vec::len).sum();
    let kmer_width = meta.k as usize;
    let mut kmers = Vec::with_capacity(row_count_estimate * kmer_width);
    let mut matrix_buf = Vec::with_capacity(row_count_estimate * (sample_count * 8 + 32));
    let mut sample_totals = vec![0u64; sample_count];
    let mut sample_nonzero = vec![0u64; sample_count];
    let mut pearson = PearsonAccumulator::new(sample_count);
    let mut cosine = CosineAccumulator::new(sample_count);
    let mut distances = needs_distances.then(|| DistanceAccumulator::new(sample_count));
    let mut rank_columns =
        needs_rank_columns.then(|| (0..sample_count).map(|_| Vec::new()).collect::<Vec<_>>());
    let mut embedding_values = Vec::new();
    let mut scaled_values = Vec::with_capacity(row_count_estimate * sample_count);
    let mut values = vec![0.0; sample_count];
    let mut scaled = vec![0.0; sample_count];
    let mut kmer_buf = vec![0u8; kmer_width];
    for_each_union_row(&vectors, |key, row| {
        fill_key_buffer(key, meta, &mut kmer_buf);
        append_exact_matrix_row(&mut matrix_buf, &kmer_buf, row)?;
        kmers.extend_from_slice(&kmer_buf);
        for (index, count) in row.iter().enumerate() {
            sample_totals[index] = sample_totals[index].saturating_add(*count);
            if *count > 0 {
                sample_nonzero[index] += 1;
            }
        }
        fill_transformed_values(row, &mut values);
        pearson.update(&values);
        cosine.update(&values);
        if let Some(distances) = distances.as_mut() {
            distances.update(&values);
        }
        if let Some(columns) = rank_columns.as_mut() {
            push_columns(columns, &values);
        }
        if needs_dense_embedding {
            embedding_values.extend(values.iter().copied());
        }
        fill_row_zscore(&values, &mut scaled);
        scaled_values.extend(scaled.iter().copied());
        Ok(())
    })?;
    let row_count = kmers.len() / kmer_width.max(1);
    Ok(PartitionAnalyzeChunk {
        partition_id,
        kmers,
        kmer_width,
        matrix_buf,
        sample_totals,
        sample_nonzero,
        pearson,
        cosine,
        distances,
        rank_columns,
        embedding_values,
        scaled_values,
        row_count,
    })
}

fn render_quarto_document(
    labels: &[String],
    groups: &[String],
    correlation_methods: &[CorrelationMethod],
    embedding_methods: &[EmbeddingMethod],
    heatmap_kmeans: usize,
    cluster_samples: bool,
) -> String {
    let subtitle = yaml_double_quoted(&generated_by_subtitle());
    let label_list = labels
        .iter()
        .map(|label| format!("\"{}\"", label.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let group_list = groups
        .iter()
        .map(|group| format!("\"{}\"", group.replace('"', "\\\"")))
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
subtitle: "{subtitle}"

format:
  html:
    toc: true
    toc-depth: 3
    toc-location: left
    section-divs: true
    code-fold: true
    code-tools: true
    code-line-numbers: true
    code-copy: hover
    number-sections: true
    highlight-style: tango
    page-layout: full
    theme:
      light: default
      dark: darkly

  pdf:
    toc: true
    toc-depth: 3
    number-sections: true
    colorlinks: true
    geometry:
      - margin=0.8in

execute:
  echo: true
  warning: false
  message: false

params:
  output_dir: "."
  samples: [{label_list}]
  groups: [{group_list}]
  correlation_tables: "{correlation_list}"
  embedding_tables: "{embedding_list}"
  color_theme: "anime_sky"
  heatmap_kmeans: {heatmap_kmeans}
  cluster_samples: {cluster_samples}
  render_exact_kmer_heatmap: false
  exact_heatmap_min_total_signal: 10
  exact_heatmap_max_kmers: 2000
---

# Kite analyze

This notebook is generated by `kite analyze`. The command itself writes TSV tables only by default. Render this document with [quarto](https://quarto.org/) when you want publication-style figures.

## Report theme

The YAML header exposes `color_theme`, so readers can switch visual style without regenerating TSV tables. Built-in choices are:

- `anime_sky`: airy blue-pink default
- `sakura_mint`: pastel pink and mint
- `sunset_pop`: peach, gold, and coral
- `midnight_tokyo`: navy, cyan, and neon rose

Only the rendered figures and styled tables change; the underlying analysis tables stay identical.

## Runtime requirements

Recommended minimums for this notebook:

| Component | Minimum |
|---|---:|
| R | 4.3.0 |
| data.table | 1.15.0 |
| ggplot2 | 4.0.0 |
| dplyr | 1.1.0 |
| tidyr | 1.3.0 |
| gt | 0.11.1 |
| ggrepel | 0.9.6 |
| patchwork | 1.2.0 |
| scales | 1.4.0 |
| ComplexHeatmap | 2.18.0 |
| circlize | 0.4.16 |

```{{r package_setup}}
cran_packages <- c(
  data.table = "1.15.0",
  ggplot2 = "4.0.0",
  dplyr = "1.1.0",
  tidyr = "1.3.0",
  gt = "0.11.1",
  ggrepel = "0.9.6",
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

if (!requireNamespace("pak", quietly = TRUE)) {{
  install.packages("pak", repos = "https://cloud.r-project.org")
}}

is_missing_or_old <- function(pkg, min_version) {{
  !requireNamespace(pkg, quietly = TRUE) ||
    utils::compareVersion(
      as.character(utils::packageVersion(pkg)),
      min_version
    ) < 0
}}

cran_to_install <- names(cran_packages)[
  vapply(
    names(cran_packages),
    function(pkg) is_missing_or_old(pkg, cran_packages[[pkg]]),
    logical(1)
  )
]

bioc_to_install <- names(bioc_packages)[
  vapply(
    names(bioc_packages),
    function(pkg) is_missing_or_old(pkg, bioc_packages[[pkg]]),
    logical(1)
  )
]

if (length(cran_to_install) > 0) {{
  message("Installing CRAN packages: ", paste(cran_to_install, collapse = ", "))
  pak::pkg_install(cran_to_install)
}} else {{
  message("All CRAN packages are available.")
}}

if (length(bioc_to_install) > 0) {{
  message("Installing Bioconductor packages: ", paste(bioc_to_install, collapse = ", "))
  pak::pkg_install(paste0("bioc::", bioc_to_install))
}} else {{
  message("All Bioconductor packages are available.")
}}
```

```{{r report_helpers}}
suppressPackageStartupMessages({{
  library(data.table)
  library(ggplot2)
  library(dplyr)
  library(tidyr)
  library(gt)
  library(ggrepel)
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

save_heatmap <- function(ht, filename, width = 9, height = 6, dpi = 320) {{
  png(file.path(out_dir, filename), width = width, height = height, units = "in", res = dpi)
  ComplexHeatmap::draw(ht, heatmap_legend_side = "right")
  dev.off()

  pdf(file.path(out_dir, sub("\\.png$", ".pdf", filename)), width = width, height = height)
  ComplexHeatmap::draw(ht, heatmap_legend_side = "right")
  dev.off()
}}

kite_theme <- function(base_size = 12) {{
  theme_bw(base_size = base_size, base_family = "sans") +
    theme(
      text = element_text(family = "sans", color = pal$ink),

      plot.title.position = "plot",
      plot.title = element_text(
        family = "sans",
        face = "bold",
        size = rel(1.15),
        hjust = 0.5,
        color = pal$ink,
        margin = margin(b = 8)
      ),
      plot.subtitle = element_text(
        family = "sans",
        color = pal$ink,
        hjust = 0.5
      ),
      plot.caption = element_text(
        family = "sans",
        color = pal$ink
      ),

      panel.grid.major = element_blank(),
      panel.grid.minor = element_blank(),

      panel.border = element_rect(
        color = pal$border,
        fill = NA,
        linewidth = 0.35
      ),

      axis.line = element_blank(),
      axis.title = element_text(
        family = "sans",
        color = pal$ink
      ),
      axis.text = element_text(
        family = "sans",
        color = pal$ink
      ),
      axis.ticks = element_line(
        color = pal$border,
        linewidth = 0.3
      ),
      axis.ticks.length = unit(2, "pt"),

      legend.title = element_text(
        family = "sans",
        color = pal$ink
      ),
      legend.text = element_text(
        family = "sans",
        color = pal$ink
      ),
      legend.key = element_rect(fill = "transparent", color = NA),
      legend.background = element_rect(fill = "transparent", color = NA),
      legend.box.background = element_rect(fill = "transparent", color = NA),

      strip.text = element_text(
        family = "sans",
        color = pal$ink,
        face = "bold"
      ),
      strip.background = element_rect(
        fill = "transparent",
        color = pal$border,
        linewidth = 0.35
      ),

      plot.background = element_rect(fill = pal$page_bg, color = NA),
      panel.background = element_rect(fill = pal$panel_bg, color = NA)
    )
}}

short_number <- scales::label_number(scale_cut = scales::cut_short_scale())
human_size <- function(bytes) {{
  ifelse(
    is.na(bytes),
    NA_character_,
    ifelse(
      bytes < 1024,
      paste0(bytes, " B"),
      ifelse(
        bytes < 1024^2,
        paste0(round(bytes / 1024, 1), " KB"),
        ifelse(
          bytes < 1024^3,
          paste0(round(bytes / 1024^2, 2), " MB"),
          paste0(round(bytes / 1024^3, 2), " GB")
        )
      )
    )
  )
}}

theme_palette <- function(name) {{
  palettes <- list(
    anime_sky = list(
      page_bg = "#FCFBFF",
      panel_bg = "#FDFBFF",
      ink = "#000000",
      border = "#000000",
      accent = "#69B7FF",
      accent_soft = "#AFC8FF",
      accent2 = "#FF9BC2",
      accent2_soft = "#FFD4E7",
      bar_primary = "#79B8FF",
      bar_secondary = "#80D7C4",
      matrix_low = "#D8ECFF",
      matrix_mid = "#FFFDFE",
      matrix_high = "#FFE7A8",
      heatmap_low = "#8FC7FF",
      heatmap_mid = "#FFFDFE",
      heatmap_high = "#FF9DB8",
      segment = "#AFC8FF"
    ),
    sakura_mint = list(
      page_bg = "#FFFDFD",
      panel_bg = "#FFF9FB",
      ink = "#000000",
      border = "#000000",
      accent = "#6BC9B8",
      accent_soft = "#C5EFE7",
      accent2 = "#F58FB5",
      accent2_soft = "#FFD7E7",
      bar_primary = "#F6A5C0",
      bar_secondary = "#7FD6C2",
      matrix_low = "#D9F5F0",
      matrix_mid = "#FFFDFD",
      matrix_high = "#FFE3B3",
      heatmap_low = "#8ED9C8",
      heatmap_mid = "#FFFDFD",
      heatmap_high = "#F49BBC",
      segment = "#A9DED3"
    ),
    sunset_pop = list(
      page_bg = "#FFFCF8",
      panel_bg = "#FFF8F0",
      ink = "#000000",
      border = "#000000",
      accent = "#FF9F6E",
      accent_soft = "#FFD1A8",
      accent2 = "#FF6FA5",
      accent2_soft = "#FFD4E3",
      bar_primary = "#FFB26B",
      bar_secondary = "#FF8FA3",
      matrix_low = "#D7ECFF",
      matrix_mid = "#FFFDF9",
      matrix_high = "#FFE59A",
      heatmap_low = "#91C9FF",
      heatmap_mid = "#FFFDF9",
      heatmap_high = "#FF8FA3",
      segment = "#FFC8A2"
    ),
    midnight_tokyo = list(
      page_bg = "#F7FAFF",
      panel_bg = "#F2F7FF",
      ink = "#000000",
      border = "#000000",
      accent = "#5EC8FF",
      accent_soft = "#A9DFFF",
      accent2 = "#FF6FB5",
      accent2_soft = "#FFC2DE",
      bar_primary = "#5EC8FF",
      bar_secondary = "#9A8CFF",
      matrix_low = "#D5E7FF",
      matrix_mid = "#F8FBFF",
      matrix_high = "#FFE48A",
      heatmap_low = "#6AB5FF",
      heatmap_mid = "#F8FBFF",
      heatmap_high = "#FF7AB8",
      segment = "#8DB8FF"
    )
  )

  if (!name %in% names(palettes)) {{
    warning("Unknown color_theme: ", name, "; falling back to anime_sky")
    name <- "anime_sky"
  }}

  palettes[[name]]
}}

pal <- theme_palette(params$color_theme)

pretty_table <- function(df, title = NULL, digits = 3) {{
  numeric_cols <- names(df)[vapply(df, is.numeric, logical(1))]
  tab <- gt::gt(df)
  if (!is.null(title)) {{
    tab <- gt::tab_header(tab, title = gt::md(title))
  }}
  if (length(numeric_cols) > 0) {{
    tab <- gt::fmt_number(
      tab,
      columns = dplyr::all_of(numeric_cols),
      decimals = digits,
      drop_trailing_zeros = TRUE
    )
  }}
  tab |>
    gt::tab_options(
      table.width = gt::pct(100),
      heading.align = "left",
      table.font.names = c("Source Sans Pro", "Helvetica", "Arial", "sans-serif"),
      table.background.color = "white",
      table.border.top.color = "#000000",
      table.border.bottom.color = "#000000",
      table.border.top.style = "solid",
      table.border.bottom.style = "solid",
      heading.background.color = "white",
      heading.border.bottom.color = "#000000",
      heading.title.font.weight = "bold",
      column_labels.background.color = "white",
      column_labels.font.weight = "bold",
      column_labels.border.bottom.color = "#000000",
      column_labels.border.bottom.style = "solid",
      data_row.padding = gt::px(5),
      table.font.color = "#000000",
      table_body.hlines.color = "#000000",
      table_body.hlines.style = "solid"
    )
}}
```

## Output figure sizes

The report saves each figure separately with fixed dimensions so the rendered
report can be used without manual resizing.

| Figure | File | Size |
|---|---|---:|
| Sample summary | `sample_summary.png/pdf` | 8 x 5 in |
| Correlation/distance heatmaps | `correlation_*.png/pdf` | 7.5 x 6.5 in |
| Embeddings | `embedding_*.png/pdf` | 7 x 5.5 in |
| Cluster representative heatmap | `heatmap_cluster_representatives.png/pdf` | 9 x 6 in |
| Optional exact k-mer heatmap | `heatmap_exact_filtered.png/pdf` | 9 x 8 in |

# Input tables

`exact_count_matrix.tsv` contains raw exact k-mer counts by sample. This file can be very large, so most plotting below uses summary tables instead.
This section is meant to answer two simple questions before any interpretation:

1. Which tables did `kite analyze` actually produce for this run?
2. Which of those tables are small summaries versus very large matrices?

```{{r table_inventory}}
expected_tables <- c(
  "exact_count_matrix.tsv",
  "sample_summary.tsv",
  "heatmap_matrix.tsv",
  "heatmap_plot_matrix.tsv",
  "heatmap_clusters.tsv",
  "heatmap_samples.tsv"
)

table_inventory <- data.frame(
  file = basename(Sys.glob(file.path(out_dir, "*.tsv"))),
  stringsAsFactors = FALSE
) |>
  dplyr::mutate(
    path = file.path(out_dir, file),
    size_bytes = file.info(path)$size,
    size = human_size(size_bytes),
    role = dplyr::case_when(
      file == "exact_count_matrix.tsv" ~ "raw exact k-mer matrix",
      file == "sample_summary.tsv" ~ "sample totals and nonzero rows",
      file == "heatmap_matrix.tsv" ~ "all row-scaled k-mers for optional exact heatmap",
      file == "heatmap_plot_matrix.tsv" ~ "small row-scaled matrix used for quick plotting",
      file == "heatmap_clusters.tsv" ~ "cluster representative heatmap matrix",
      file == "heatmap_samples.tsv" ~ "sample order for heatmaps",
      grepl("^correlation_", file) ~ "correlation or distance matrix",
      grepl("^embedding_", file) ~ "sample embedding coordinates",
      TRUE ~ "other"
    )
  ) |>
  dplyr::select(file, role, size)

missing_tables <- setdiff(expected_tables, table_inventory$file)
if (length(missing_tables) > 0) {{
  warning("Missing expected Kite analyze table(s): ", paste(missing_tables, collapse = ", "))
}}

pretty_table(table_inventory, "Table inventory", digits = 3)
```

`sample_summary.tsv` is the smallest per-sample overview table. It is usually the fastest place to check whether one sample is much deeper, much sparser, or obviously inconsistent with the others.

```{{r sample_overview}}
sample_summary <- read_kite_tsv("sample_summary.tsv")
heatmap_sample_order <- read_kite_tsv("heatmap_samples.tsv")
sample_overview <- sample_summary |>
  dplyr::left_join(heatmap_sample_order, by = "sample") |>
  dplyr::arrange(order) |>
  dplyr::rename(
    heatmap_order = order
  )
sample_groups <- data.frame(
  sample = params$samples,
  group = params$groups,
  stringsAsFactors = FALSE
)
sample_overview <- sample_overview |>
  dplyr::left_join(sample_groups, by = "sample")

pretty_table(sample_overview, "Sample overview", digits = 3)
```

## Sample information

This summary compresses the sample table into run-level quantities. The names below are intentionally explicit:

- `samples`: how many input samples were analyzed
- `total_raw_counts_across_samples`: the combined count mass across all samples
- `median_raw_counts_per_sample`: the median library size across samples
- `median_nonzero_kmers_per_sample`: the median number of nonzero k-mers across samples

```{{r sample_summary_stats}}
pretty_table(
  sample_overview |>
    dplyr::summarise(
      samples = dplyr::n(),
      total_raw_counts_across_samples = sum(total_raw_counts),
      median_raw_counts_per_sample = median(total_raw_counts),
      median_nonzero_kmers_per_sample = median(nonzero_kmers)
    ),
  "Sample summary statistics",
  digits = 3
)
```

## Sample summary plots

These two bar plots answer the most basic QC question: do the samples have broadly similar total signal and broadly similar support size, or is one sample clearly deeper or sparser than the rest?

```{{r sample_summary_plots}}
group_levels <- unique(sample_overview$group)
group_colors <- setNames(scales::hue_pal(l = 68, c = 100)(length(group_levels)), group_levels)

p_summary <- ggplot(sample_overview, aes(sample, total_raw_counts, fill = group)) +
  geom_col(color = pal$ink, linewidth = 0.35, width = 0.72) +
  scale_y_continuous(labels = short_number) +
  scale_fill_manual(values = group_colors, drop = FALSE) +
  labs(x = NULL, y = "Total raw counts", title = "Total k-mer counts per sample", fill = "group") +
  kite_theme(base_size = 12) +
  theme(
    axis.text.x = element_text(angle = 45, hjust = 1),
    legend.position = if (length(group_levels) > 1) "right" else "none"
  )

p_nonzero <- ggplot(sample_overview, aes(sample, nonzero_kmers, fill = group)) +
  geom_col(color = pal$ink, linewidth = 0.35, width = 0.72) +
  scale_y_continuous(labels = short_number) +
  scale_fill_manual(values = group_colors, drop = FALSE) +
  labs(x = NULL, y = "Nonzero k-mers", title = "Nonzero k-mers per sample", fill = "group") +
  kite_theme(base_size = 12) +
  theme(
    axis.text.x = element_text(angle = 45, hjust = 1),
    legend.position = if (length(group_levels) > 1) "right" else "none"
  )

p_summary / p_nonzero
```

```{{r save_sample_summary_plots}}
save_plot(p_summary, "sample_summary.png", width = 8, height = 5)
save_plot(p_summary, "sample_summary.pdf", width = 8, height = 5)
save_plot(p_nonzero, "sample_nonzero_kmers.png", width = 8, height = 5)
save_plot(p_nonzero, "sample_nonzero_kmers.pdf", width = 8, height = 5)
```

# Correlation

The generated correlation and distance tables are: `{correlation_list}`. The default command writes only `correlation_pearson.tsv`; use `--correlation all` or a specific method to write additional tables.
Read this section as a sample-to-sample relationship overview:

- `pearson`, `spearman`, `kendall`, and `cosine` are similarity-style summaries
- `euclidean`, `manhattan`, and `mahalanobis` are distance/score-style summaries

For the similarity-style matrices, values closer to `1` indicate more similar samples, values near `0` indicate weak agreement, and negative values indicate opposing trends. For the distance-style matrices, smaller values indicate more similar samples.
The plots below reorder samples by average-linkage clustering on the matrix values so neighboring rows and columns usually reflect stronger similarity.

```{{r correlation_plots}}
base_matrix_plot <- function(
  long,
  name,
  fill_label,
  fill_scale,
  label_digits = 2,
  label_size = 3
) {{
  ggplot(long, aes(sample_x, sample_y, fill = value)) +
    geom_tile(
      color = "white",
      linewidth = 0.4
    ) +
    geom_text(
      aes(label = label),
      family = "sans",
      size = label_size,
      color = pal$ink
    ) +
    coord_equal() +
    labs(
      x = NULL,
      y = NULL,
      fill = fill_label,
      title = sub("\\.tsv$", "", name)
    ) +
    fill_scale +
    kite_theme(base_size = 12) +
    theme(
      axis.text.x = element_text(
        angle = 45,
        hjust = 1,
        vjust = 1,
        color = pal$ink,
        family = "sans"
      ),
      axis.text.y = element_text(
        color = pal$ink,
        family = "sans"
      ),
      panel.grid.major = element_blank(),
      panel.grid.minor = element_blank(),
      legend.key = element_rect(fill = "transparent", color = NA),
      legend.background = element_rect(fill = "transparent", color = NA),
      legend.box.background = element_rect(fill = "transparent", color = NA)
    )
}}

matrix_sample_order <- function(values, distance_mode = FALSE) {{
  if (nrow(values) < 2) {{
    return(colnames(values))
  }}
  values <- (values + t(values)) / 2
  diag(values) <- if (distance_mode) 0 else 1
  dist_values <- if (distance_mode) {{
    stats::as.dist(values)
  }} else {{
    stats::as.dist(pmax(0, 1 - values))
  }}
  ordering <- tryCatch(
    stats::hclust(dist_values, method = "average")$order,
    error = function(...) NULL
  )
  if (is.null(ordering)) {{
    colnames(values)
  }} else {{
    colnames(values)[ordering]
  }}
}}

plot_similarity_matrix <- function(
  name,
  label_digits = 2,
  label_size = 3
) {{
  df <- read_kite_tsv(name)
  labels <- colnames(df)[-1]
  matrix_values <- as.matrix(df[, labels, drop = FALSE])
  mode(matrix_values) <- "numeric"
  rownames(matrix_values) <- df$sample
  colnames(matrix_values) <- labels

  ordered_labels <- matrix_sample_order(matrix_values, distance_mode = FALSE)

  long <- df |>
    tidyr::pivot_longer(
      cols = -sample,
      names_to = "sample_x",
      values_to = "value"
    ) |>
    dplyr::mutate(
      sample_x = factor(sample_x, levels = ordered_labels),
      sample_y = factor(sample, levels = rev(ordered_labels)),
      label = sprintf(paste0("%.", label_digits, "f"), value)
    )

  fill_scale <- if (min(long$value, na.rm = TRUE) < 0 && max(long$value, na.rm = TRUE) > 0) {{
    scale_fill_gradient2(
      low = pal$matrix_low,
      mid = pal$matrix_mid,
      high = pal$matrix_high,
      midpoint = 0,
      oob = scales::squish
    )
  }} else {{
    scale_fill_gradient(
      low = pal$matrix_low,
      high = pal$matrix_high,
      oob = scales::squish
    )
  }}

  base_matrix_plot(long, name, "correlation", fill_scale, label_digits, label_size)
}}

plot_score_matrix <- function(
  name,
  label_digits = 2,
  label_size = 3
) {{
  df <- read_kite_tsv(name)
  labels <- colnames(df)[-1]
  matrix_values <- as.matrix(df[, labels, drop = FALSE])
  mode(matrix_values) <- "numeric"
  rownames(matrix_values) <- df$sample
  colnames(matrix_values) <- labels
  ordered_labels <- matrix_sample_order(matrix_values, distance_mode = TRUE)

  long <- df |>
    tidyr::pivot_longer(
      cols = -sample,
      names_to = "sample_x",
      values_to = "value"
    ) |>
    dplyr::mutate(
      sample_x = factor(sample_x, levels = ordered_labels),
      sample_y = factor(sample, levels = rev(ordered_labels)),
      label = sprintf(paste0("%.", label_digits, "f"), value)
    )

  fill_scale <- scale_fill_gradient(
    low = pal$matrix_low,
    high = pal$matrix_high,
    oob = scales::squish
  )

  base_matrix_plot(long, name, "score", fill_scale, label_digits, label_size)
}}

correlation_files <- Sys.glob(file.path(out_dir, "correlation_*.tsv"))

correlation_plots <- setNames(
  lapply(basename(correlation_files), function(name) {{
    if (grepl("euclidean|manhattan|mahalanobis", name, ignore.case = TRUE)) {{
      plot_score_matrix(name)
    }} else {{
      plot_similarity_matrix(name)
    }}
  }}),
  sub("\\.tsv$", "", basename(correlation_files))
)

correlation_plots
```

```{{r save_correlation_plots}}
for (name in names(correlation_plots)) {{
  save_plot(correlation_plots[[name]], paste0(name, ".png"), width = 7.5, height = 6.5)
  save_plot(correlation_plots[[name]], paste0(name, ".pdf"), width = 7.5, height = 6.5)
}}
```

# Embedding

Requested embedding tables: `{embedding_list}`. Kite reads and plots whichever `embedding_*.tsv` files are available; methods skipped because of sample-count or feature-matrix limits simply do not produce a table.
This section turns the sample-to-sample relationships into a 2D view. Points that fall close together are more similar under that embedding; points that separate strongly are more distinct. Different embedding methods emphasize different structure, so this section is best read comparatively rather than as a single absolute answer.
If the embedding table includes a `group` column, points are filled by group so related samples are easier to scan at a glance.
The label layer uses repel-based placement so sample names remain readable even when points are close together.

```{{r embedding_plots}}
plot_embedding <- function(name, point_size = 3.2, label_size = 3.4) {{
  df <- read_kite_tsv(name)
  df$group <- if ("group" %in% names(df)) {{
    factor(df$group, levels = unique(df$group))
  }} else {{
    factor(rep("all", nrow(df)))
  }}
  group_colors <- setNames(scales::hue_pal(l = 68, c = 100)(nlevels(df$group)), levels(df$group))

  ggplot(df, aes(dim1, dim2, label = sample)) +
    geom_hline(yintercept = 0, color = pal$accent_soft, linewidth = 0.4) +
    geom_vline(xintercept = 0, color = pal$accent_soft, linewidth = 0.4) +
    geom_point(
      size = point_size + 1.1,
      color = pal$matrix_mid,
      alpha = 0.95
    ) +
    geom_point(
      size = point_size,
      aes(fill = group),
      shape = 21,
      color = pal$ink,
      stroke = 0.7,
      alpha = 0.95
    ) +
    scale_fill_manual(values = group_colors, drop = FALSE) +
    ggrepel::geom_label_repel(
      size = label_size,
      color = pal$ink,
      fill = scales::alpha(pal$matrix_mid, 0.92),
      label.size = 0.18,
      label.padding = grid::unit(0.12, "lines"),
      box.padding = grid::unit(0.28, "lines"),
      point.padding = grid::unit(0.18, "lines"),
      segment.color = pal$segment,
      segment.size = 0.3,
      seed = 42,
      max.overlaps = Inf
    ) +
    labs(
      x = "dim1",
      y = "dim2",
      title = sub("\\.tsv$", "", name),
      subtitle = "Nearby points indicate more similar samples in this embedding",
      fill = "group"
    ) +
    kite_theme(base_size = 12) +
    theme(
      panel.background = element_rect(fill = pal$panel_bg, color = NA),
      plot.background = element_rect(fill = pal$page_bg, color = NA),
      legend.position = if (nlevels(df$group) > 1) "right" else "none"
    )
}}

embedding_files <- Sys.glob(file.path(out_dir, "embedding_*.tsv"))
embedding_plots <- setNames(
  lapply(basename(embedding_files), plot_embedding),
  sub("\\.tsv$", "", basename(embedding_files))
)
embedding_plots
```

```{{r save_embedding_plots}}
for (name in names(embedding_plots)) {{
  save_plot(embedding_plots[[name]], paste0(name, ".png"), width = 7, height = 5.5)
  save_plot(embedding_plots[[name]], paste0(name, ".pdf"), width = 7, height = 5.5)
}}
```

# Heatmap

The default heatmap uses `heatmap_clusters.tsv`, so it shows one representative row per k-mer cluster and renders quickly. Sample columns are clustered by default when `cluster_samples` is true, so the heatmap includes a sample dendrogram. K-mer rows are not hierarchically clustered; each row is a k-means cluster representative.
This is the fastest high-level view of structured k-mer differences across samples. Each row is a representative standardized pattern, not a single raw count row. Red means relatively higher-than-row-average signal for that sample, and blue means relatively lower-than-row-average signal for that sample.

```{{r cluster_heatmap}}
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
  c(pal$heatmap_low, pal$heatmap_mid, pal$heatmap_high)
)

ht_cluster <- ComplexHeatmap::Heatmap(
  cluster_mat,
  name = "z-score",
  col = heatmap_col_fun,
  cluster_rows = FALSE,
  cluster_columns = params$cluster_samples,
  show_column_dend = params$cluster_samples,
  column_dend_height = grid::unit(24, "mm"),
  rect_gp = grid::gpar(col = NA),
  row_names_gp = grid::gpar(fontfamily = "sans", fontsize = 10),
  column_names_gp = grid::gpar(fontfamily = "sans", fontsize = 10),
  heatmap_legend_param = list(title_gp = grid::gpar(fontfamily = "sans", fontface = "bold"))
)

ComplexHeatmap::draw(ht_cluster, heatmap_legend_side = "right")
```

```{{r save_cluster_heatmap}}
save_heatmap(ht_cluster, "heatmap_cluster_representatives.png", width = 9, height = 6)
```

## Optional exact k-mer heatmap

`kite analyze` writes `heatmap_matrix.tsv`, where each row is one exact k-mer and each value is the row-scaled signal used for heatmaps. This exact k-mer heatmap is disabled by default because it can be very large. To enable it, edit the YAML header at the top of this file and set:

```{{text exact_heatmap_toggle}}
render_exact_kmer_heatmap: true
```

The exact heatmap uses the same sample column dendrogram as the default heatmap when `cluster_samples` is true. K-mer rows are not clustered because millions of row dendrogram leaves are not interpretable; rows are ordered by cluster and then by total absolute signal. Users can manually change `exact_heatmap_min_total_signal` and `exact_heatmap_max_kmers` in the YAML header to control how many rows are plotted and saved.

```{{r exact_heatmap}}
#| eval: !expr params$render_exact_kmer_heatmap
min_total_signal <- params$exact_heatmap_min_total_signal
max_kmers <- params$exact_heatmap_max_kmers

heatmap_df <- read_kite_tsv("heatmap_matrix.tsv")
sample_cols <- intersect(samples, colnames(heatmap_df))
heatmap_mat <- as.matrix(heatmap_df[, sample_cols, drop = FALSE])
mode(heatmap_mat) <- "numeric"
total_signal <- rowSums(abs(heatmap_mat))
keep <- total_signal >= min_total_signal

if (sum(keep) > max_kmers) {{
  keep_idx <- which(keep)
  keep_idx <- keep_idx[order(total_signal[keep_idx], decreasing = TRUE)[seq_len(max_kmers)]]
}} else {{
  keep_idx <- which(keep)
}}

keep_idx <- keep_idx[order(heatmap_df$cluster[keep_idx], -total_signal[keep_idx])]
exact_mat <- heatmap_mat[keep_idx, , drop = FALSE]
exact_mat[!is.finite(exact_mat)] <- 0
rownames(exact_mat) <- heatmap_df$kmer[keep_idx]

ht_exact <- ComplexHeatmap::Heatmap(
  exact_mat,
  name = "z-score",
  col = heatmap_col_fun,
  cluster_rows = FALSE,
  cluster_columns = params$cluster_samples,
  show_column_dend = params$cluster_samples,
  column_dend_height = grid::unit(24, "mm"),
  show_row_names = FALSE,
  rect_gp = grid::gpar(col = NA),
  column_names_gp = grid::gpar(fontfamily = "sans", fontsize = 10)
)

ComplexHeatmap::draw(ht_exact, heatmap_legend_side = "right")
```

```{{r save_exact_heatmap}}
#| eval: !expr params$render_exact_kmer_heatmap
save_heatmap(ht_exact, "heatmap_exact_filtered.png", width = 9, height = 8)
```

## Heatmap cluster centroids

The centroid table is much smaller than the full heatmap matrix and is useful when the input contains millions of k-mers.
Each row below is the numeric centroid behind one representative heatmap row. `n` is the number of exact k-mer rows assigned to that cluster.

```{{r centroid_table}}
centroids <- read_kite_tsv("heatmap_clusters.tsv")
pretty_table(centroids, "Heatmap cluster centroids", digits = 3)
```

# Export options

Use `save_plot()` with different filenames to switch formats. Examples:

```{{r export_examples}}
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
        subtitle = subtitle,
    )
}

fn generated_by_subtitle() -> String {
    let url = env!("CARGO_PKG_REPOSITORY");
    let version = env!("CARGO_PKG_VERSION");
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown-user".to_string());
    let host = hostname_string().unwrap_or_else(|| "unknown-host".to_string());
    format!("Generated by kite ({url}) v{version} on {user}@{host}")
}

fn hostname_string() -> Option<String> {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|byte| *byte == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..len]).ok().map(str::to_string)
}

fn yaml_double_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
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
    fn kendall_tau_b_matches_expected_values() {
        assert_close(kendall_tau_b(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]), 1.0);
        assert_close(kendall_tau_b(&[1.0, 2.0, 3.0], &[6.0, 4.0, 2.0]), -1.0);
        assert_close(kendall_tau_b(&[1.0, 1.0, 2.0], &[1.0, 2.0, 2.0]), 0.5);
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
                "kendall",
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
        assert_eq!(heatmap.assign_scaled(&[1.0, 2.0, 3.0]), 1);
        assert_eq!(heatmap.assign_scaled(&[3.0, 2.0, 1.0]), 2);
        let cluster = heatmap.assign_scaled(&[1.1, 2.0, 2.9]);
        assert_eq!(cluster, 1);
        assert_eq!(heatmap.counts, vec![2, 1]);
    }
}
