mod kmer;

use crate::cli::{CorrelationMethod, SketchArgs, SketchProjection};
use crate::dump::decode::read_u64_count_vec;
use crate::utils::fmt::{format_duration, format_size};
use crate::utils::progress::ProgressSpinner;
use anyhow::{Result, anyhow, bail};
use faer::{Mat, Side};
use kmer::decode_u64_fast_into;
use manifolds_rs::{TsneParams, UmapParams, tsne, umap};
use std::fs::{File, create_dir_all, remove_dir_all, remove_file};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

// 中文：`sketch` 的核心任务是把多个 `.kite` 文件变成精确多样本矩阵，
// 并继续派生出相关性、投影坐标、heatmap 聚类表以及对应的 R 脚本。
// English: The core task of `sketch` is to convert multiple `.kite` files into
// an exact multi-sample matrix, then derive correlation tables, projection
// coordinates, clustered heatmap tables, and matching R scripts.
struct SketchSample {
    path: PathBuf,
    name: String,
    entries: Vec<(u64, u64)>,
}

struct SketchSampleMetadata {
    sample_id: String,
    sample_label: String,
    sample_group: String,
    input_path: PathBuf,
    input_order: usize,
    heatmap_order: usize,
}

struct SketchAnalysis {
    correlation: Vec<Vec<f64>>,
    exact_sample_coords: Vec<Vec<f64>>,
    pca_axis_shares: Vec<f64>,
}

/// 中文：执行 `kite sketch`。输入是多个 `.kite` 文件和 sketch 参数，
/// 输出是 profile/correlation/projection/heatmap 表以及 R 绘图脚本。
/// English: Execute `kite sketch`. Inputs are multiple `.kite` files plus
/// sketch parameters; outputs are profile/correlation/projection/heatmap
/// tables and R plotting scripts.
pub fn run(args: &SketchArgs) -> Result<()> {
    let t0 = Instant::now();
    create_dir_all(&args.output)?;
    let progress =
        ProgressSpinner::start(args.progress, "kite sketch", "building exact sketch tables");

    if args.input.len() < 2 {
        bail!("sketch requires at least two input .kite files");
    }

    let sample_names = resolve_sample_names(&args.input, &args.sample_names)?;
    let sample_groups = resolve_sample_groups(&sample_names, &args.sample_groups)?;
    let (k, samples) = load_kite_samples(&args.input, &sample_names)?;

    let profile_table_path = args.output.join("sketch_profile.tsv");
    let correlation_path = args.output.join("sketch_correlation.tsv");
    let pca_scores_path = args.output.join("pca_scores.tsv");
    let tsne_scores_path = args.output.join("tsne_scores.tsv");
    let umap_scores_path = args.output.join("umap_scores.tsv");
    let heatmap_table_path = args.output.join("sketch_heatmap_clustered.tsv");
    let metadata_path = args.output.join("sketch_sample_metadata.tsv");

    let analysis = write_profile_table_and_collect_correlation(
        &profile_table_path,
        &samples,
        k,
        b"\t",
        args.correlation,
    )?;
    write_correlation_matrix(&correlation_path, &sample_names, &analysis.correlation)?;

    let sample_count = sample_names.len();
    let pca_scores = take_or_pad_components(&analysis.exact_sample_coords, 3, sample_count);
    let pca_axis_shares = take_or_pad_values(&analysis.pca_axis_shares, 3);
    let nonlinear_input =
        component_rows_to_sample_rows(&analysis.exact_sample_coords, sample_count);
    let nonlinear_fallback = take_or_pad_components(&analysis.exact_sample_coords, 2, sample_count);
    let write_pca = matches!(
        args.projection,
        SketchProjection::Pca | SketchProjection::All
    );
    let write_tsne = matches!(
        args.projection,
        SketchProjection::Tsne | SketchProjection::All
    );
    let write_umap = matches!(
        args.projection,
        SketchProjection::Umap | SketchProjection::All
    );

    if !write_pca {
        remove_if_exists(&pca_scores_path)?;
        remove_if_exists(&args.output.join("plot_pca.R"))?;
    }
    if !write_tsne {
        remove_if_exists(&tsne_scores_path)?;
        remove_if_exists(&args.output.join("plot_tsne.R"))?;
    }
    if !write_umap {
        remove_if_exists(&umap_scores_path)?;
        remove_if_exists(&args.output.join("plot_umap.R"))?;
    }

    if write_pca {
        write_component_table(&pca_scores_path, "PC", &sample_names, &pca_scores)?;
    }
    if write_tsne {
        let tsne_scores = compute_tsne_scores(&nonlinear_input, &nonlinear_fallback)?;
        let tsne_axis_shares = compute_axis_spread_shares(&tsne_scores, 2);
        write_component_table(&tsne_scores_path, "TSNE", &sample_names, &tsne_scores)?;
        println!(
            "[kite sketch] t-SNE axis spread: TSNE1={} TSNE2={}",
            format_percent(tsne_axis_shares[0]),
            format_percent(tsne_axis_shares[1])
        );
        write_projection_script(
            &args.output.join("plot_tsne.R"),
            &tsne_scores_path,
            &metadata_path,
            "TSNE",
            "t-SNE sample embedding",
            &format!("t-SNE1 (spread {})", format_percent(tsne_axis_shares[0])),
            &format!("t-SNE2 (spread {})", format_percent(tsne_axis_shares[1])),
        )?;
    }
    if write_umap {
        let umap_scores = compute_umap_scores(&nonlinear_input, &nonlinear_fallback)?;
        let umap_axis_shares = compute_axis_spread_shares(&umap_scores, 2);
        write_component_table(&umap_scores_path, "UMAP", &sample_names, &umap_scores)?;
        println!(
            "[kite sketch] UMAP axis spread: UMAP1={} UMAP2={}",
            format_percent(umap_axis_shares[0]),
            format_percent(umap_axis_shares[1])
        );
        write_projection_script(
            &args.output.join("plot_umap.R"),
            &umap_scores_path,
            &metadata_path,
            "UMAP",
            "UMAP sample embedding",
            &format!("UMAP1 (spread {})", format_percent(umap_axis_shares[0])),
            &format!("UMAP2 (spread {})", format_percent(umap_axis_shares[1])),
        )?;
    }

    let sample_order = if args.cluster_samples {
        average_linkage_order(&analysis.correlation)
    } else {
        (0..sample_names.len()).collect()
    };
    let sample_metadata =
        build_sample_metadata(&sample_names, &sample_groups, &args.input, &sample_order);
    write_sample_metadata(&metadata_path, &sample_metadata)?;
    build_heatmap_clustered_table(
        &heatmap_table_path,
        &args.output,
        &samples,
        k,
        &sample_names,
        &sample_order,
        args.heatmap_kmeans.max(1),
    )?;

    write_correlation_script(
        &args.output,
        &correlation_path,
        &metadata_path,
        correlation_method_plot_title(args.correlation),
    )?;
    if write_pca {
        write_projection_script(
            &args.output.join("plot_pca.R"),
            &pca_scores_path,
            &metadata_path,
            "PC",
            "PCA sample embedding",
            &format!("PC1 (explained {})", format_percent(pca_axis_shares[0])),
            &format!("PC2 (explained {})", format_percent(pca_axis_shares[1])),
        )?;
        println!(
            "[kite sketch] PCA explained variance: PC1={} PC2={} PC3={}",
            format_percent(pca_axis_shares[0]),
            format_percent(pca_axis_shares[1]),
            format_percent(pca_axis_shares[2])
        );
    }
    write_heatmap_script(&args.output, &heatmap_table_path, &metadata_path)?;

    drop(progress);

    println!(
        "[kite sketch] profile_table: {}",
        profile_table_path.display()
    );
    println!(
        "[kite sketch] correlation_table: {}",
        correlation_path.display()
    );
    println!("[kite sketch] sample_metadata: {}", metadata_path.display());
    if write_pca {
        println!("[kite sketch] pca_scores: {}", pca_scores_path.display());
    }
    if write_tsne {
        println!("[kite sketch] tsne_scores: {}", tsne_scores_path.display());
    }
    if write_umap {
        println!("[kite sketch] umap_scores: {}", umap_scores_path.display());
    }
    println!(
        "[kite sketch] heatmap_table: {}",
        heatmap_table_path.display()
    );
    match args.projection {
        SketchProjection::Pca => println!("[kite sketch] projection: PCA"),
        SketchProjection::Tsne => println!("[kite sketch] projection: t-SNE"),
        SketchProjection::Umap => println!("[kite sketch] projection: UMAP"),
        SketchProjection::All => println!("[kite sketch] projection: PCA + t-SNE + UMAP"),
    }
    println!(
        "[kite sketch] correlation_method: {}",
        correlation_method_name(args.correlation)
    );
    let total_inputs = samples.len();
    for (idx, sample) in samples.iter().enumerate() {
        let bytes = std::fs::metadata(&sample.path)
            .map(|m| m.len())
            .unwrap_or(0);
        println!(
            "[kite sketch] [Input: {}/{}] {} ({}) -> {}",
            idx + 1,
            total_inputs,
            sample.path.display(),
            format_size(bytes),
            sample.name
        );
    }
    println!("[kite sketch] samples: {}", sample_names.join(", "));
    let out_bytes = dir_bytes(&args.output)?;
    println!(
        "[kite sketch] [Output: 1/1] {} ({})",
        args.output.display(),
        format_size(out_bytes)
    );
    println!("[kite sketch] elapsed: {}", format_duration(t0.elapsed()));
    Ok(())
}

/// 中文：解析样本名；如果用户没给，就用输入文件名。
/// English: Resolve sample names; if the user provides none, use file stems.
fn resolve_sample_names(inputs: &[PathBuf], provided: &[String]) -> Result<Vec<String>> {
    if provided.is_empty() {
        return Ok(inputs
            .iter()
            .map(|path| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| path.display().to_string())
            })
            .collect());
    }

    if provided.len() != inputs.len() {
        bail!(
            "--sample-name count ({}) must match number of inputs ({})",
            provided.len(),
            inputs.len()
        );
    }

    Ok(provided.to_vec())
}

/// 中文：解析样本分组；如果用户没给，就尝试从 `*_repN` 模式自动推断。
/// English: Resolve sample groups; if the user provides none, try to infer
/// them from names like `*_repN`.
fn resolve_sample_groups(sample_names: &[String], provided: &[String]) -> Result<Vec<String>> {
    if provided.is_empty() {
        return Ok(sample_names
            .iter()
            .map(|name| infer_sample_group(name))
            .collect());
    }

    if provided.len() != sample_names.len() {
        bail!(
            "--sample-group count ({}) must match number of inputs ({})",
            provided.len(),
            sample_names.len()
        );
    }

    Ok(provided.to_vec())
}

/// 中文：从样本名推断组名，例如 `sample1_rep2 -> sample1`。
/// English: Infer a group name from a sample label, for example
/// `sample1_rep2 -> sample1`.
fn infer_sample_group(sample_name: &str) -> String {
    for marker in ["_rep", "-rep", "_Rep", "-Rep"] {
        if let Some(pos) = sample_name.rfind(marker) {
            let suffix = &sample_name[(pos + marker.len())..];
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                return sample_name[..pos].to_string();
            }
        }
    }
    sample_name.to_string()
}

/// 中文：读取全部 `.kite` 样本并检查 `k` 是否一致。
/// English: Load all `.kite` samples and verify that their `k` values match.
fn load_kite_samples(
    inputs: &[PathBuf],
    sample_names: &[String],
) -> Result<(usize, Vec<SketchSample>)> {
    let mut samples = Vec::with_capacity(inputs.len());
    let mut shared_k = None::<usize>;
    for (path, name) in inputs.iter().zip(sample_names.iter()) {
        let (info, entries, _total_count) = read_u64_count_vec(path)?;
        if let Some(expected_k) = shared_k {
            if info.k != expected_k {
                bail!(
                    "all sketch inputs must share the same k: expected {}, got {} for {}",
                    expected_k,
                    info.k,
                    path.display()
                );
            }
        } else {
            shared_k = Some(info.k);
        }
        samples.push(SketchSample {
            path: path.clone(),
            name: name.clone(),
            entries,
        });
    }
    Ok((shared_k.unwrap_or(0), samples))
}

/// 中文：构建样本 metadata 记录，保存显示名、组名和 heatmap 排序。
/// English: Build sample metadata records containing display names, groups,
/// and heatmap order.
fn build_sample_metadata(
    sample_names: &[String],
    sample_groups: &[String],
    input_paths: &[PathBuf],
    sample_order: &[usize],
) -> Vec<SketchSampleMetadata> {
    let mut heatmap_rank = vec![0usize; sample_names.len()];
    for (rank, &sample_idx) in sample_order.iter().enumerate() {
        heatmap_rank[sample_idx] = rank + 1;
    }

    sample_names
        .iter()
        .zip(sample_groups.iter())
        .zip(input_paths.iter())
        .enumerate()
        .map(
            |(idx, ((sample_id, sample_group), input_path))| SketchSampleMetadata {
                sample_id: sample_id.clone(),
                sample_label: sample_id.clone(),
                sample_group: sample_group.clone(),
                input_path: input_path.clone(),
                input_order: idx + 1,
                heatmap_order: heatmap_rank[idx],
            },
        )
        .collect()
}

/// 中文：把样本 metadata 写成 TSV，供后续 R 脚本读取或人工修改。
/// English: Write sample metadata as TSV so later R scripts can read it and
/// users can edit it manually.
fn write_sample_metadata(path: &Path, sample_metadata: &[SketchSampleMetadata]) -> Result<()> {
    let mut w = BufWriter::with_capacity(64 << 10, File::create(path)?);
    writeln!(
        w,
        "sample_id\tsample_label\tsample_group\tinput_path\tinput_order\theatmap_order"
    )?;
    for row in sample_metadata {
        writeln!(
            w,
            "{}\t{}\t{}\t{}\t{}\t{}",
            row.sample_id,
            row.sample_label,
            row.sample_group,
            row.input_path.display(),
            row.input_order,
            row.heatmap_order
        )?;
    }
    w.flush()?;
    Ok(())
}

/// 中文：写出精确 profile 表，同时在线累计相关性/PCA 所需统计量。
/// English: Write the exact profile table while accumulating the statistics
/// needed for correlation and PCA.
fn write_profile_table_and_collect_correlation(
    output: &Path,
    samples: &[SketchSample],
    k: usize,
    sep: &[u8],
    method: CorrelationMethod,
) -> Result<SketchAnalysis> {
    let mut w = BufWriter::with_capacity(4 << 20, File::create(output)?);
    w.write_all(b"kmer")?;
    for sample in samples {
        w.write_all(sep)?;
        w.write_all(sample.name.as_bytes())?;
    }
    w.write_all(b"\n")?;

    let mut stats = CorrelationAccumulator::new(samples.len(), method);
    let mut seq = vec![0u8; k];
    let mut line = Vec::<u8>::with_capacity(k + samples.len() * 16 + 1);

    visit_union_rows(samples, |key, row| {
        decode_u64_fast_into(key, k, &mut seq);
        line.clear();
        line.extend_from_slice(&seq);
        for &count in row {
            line.push(b'\t');
            append_u64_ascii(&mut line, count);
        }
        line.push(b'\n');
        w.write_all(&line)?;
        stats.update(row);
        Ok(())
    })?;

    w.flush()?;
    Ok(stats.finish())
}

/// 中文：多路归并多个已排序样本，把相同 k-mer 的各样本计数对齐到一行。
/// English: Multi-way merge sorted sample streams so the same k-mer is aligned
/// across all sample columns on one row.
fn visit_union_rows<F>(samples: &[SketchSample], mut visit: F) -> Result<()>
where
    F: FnMut(u64, &[u64]) -> Result<()>,
{
    if samples.is_empty() {
        return Ok(());
    }

    let mut positions = vec![0usize; samples.len()];
    let mut row = vec![0u64; samples.len()];

    loop {
        let mut next_key = None::<u64>;
        for (idx, sample) in samples.iter().enumerate() {
            if let Some(&(key, _)) = sample.entries.get(positions[idx]) {
                next_key = Some(match next_key {
                    Some(current) => current.min(key),
                    None => key,
                });
            }
        }

        let Some(key) = next_key else {
            break;
        };

        for (idx, sample) in samples.iter().enumerate() {
            row[idx] = if let Some(&(cur_key, count)) = sample.entries.get(positions[idx]) {
                if cur_key == key {
                    positions[idx] += 1;
                    count
                } else {
                    0
                }
            } else {
                0
            };
        }

        visit(key, &row)?;
    }

    Ok(())
}

/// 中文：把坐标行扩展或截断到指定维数。
/// English: Pad or truncate coordinate rows to the requested dimensionality.
fn take_or_pad_components(
    rows: &[Vec<f64>],
    wanted_dims: usize,
    sample_count: usize,
) -> Vec<Vec<f64>> {
    let mut out = Vec::with_capacity(wanted_dims);
    for row in rows.iter().take(wanted_dims) {
        out.push(row.clone());
    }
    while out.len() < wanted_dims {
        out.push(vec![0.0; sample_count]);
    }
    out
}

/// 中文：把数值列表扩展或截断到指定长度。
/// English: Pad or truncate a numeric list to the requested length.
fn take_or_pad_values(values: &[f64], wanted_dims: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity(wanted_dims);
    for &value in values.iter().take(wanted_dims) {
        out.push(value);
    }
    while out.len() < wanted_dims {
        out.push(0.0);
    }
    out
}

/// 中文：根据各轴方差估计它们的相对解释份额或 spread。
/// English: Estimate each axis's relative explained share / spread from its
/// variance.
fn compute_axis_spread_shares(rows: &[Vec<f64>], wanted_dims: usize) -> Vec<f64> {
    let mut spreads = Vec::with_capacity(rows.len());
    for row in rows {
        if row.is_empty() {
            spreads.push(0.0);
            continue;
        }
        let mean = row.iter().sum::<f64>() / row.len() as f64;
        let var = row
            .iter()
            .map(|value| {
                let d = *value - mean;
                d * d
            })
            .sum::<f64>()
            / row.len() as f64;
        spreads.push(var.max(0.0));
    }
    let total = spreads.iter().sum::<f64>();
    let shares: Vec<f64> = if total > 0.0 {
        spreads.iter().map(|value| *value / total).collect()
    } else {
        vec![0.0; spreads.len()]
    };
    take_or_pad_values(&shares, wanted_dims)
}

/// 中文：把小数比例格式化成百分比字符串。
/// English: Format a fractional ratio as a percentage string.
fn format_percent(value: f64) -> String {
    format!("{:.2}%", value * 100.0)
}

/// 中文：把“分量为行”的矩阵转成“样本为行”的矩阵。
/// English: Convert a component-row matrix into a sample-row matrix.
fn component_rows_to_sample_rows(
    component_rows: &[Vec<f64>],
    sample_count: usize,
) -> Vec<Vec<f64>> {
    if sample_count == 0 {
        return Vec::new();
    }
    if component_rows.is_empty() {
        return vec![vec![0.0; 1]; sample_count];
    }

    let dims = component_rows.len();
    let mut samples = vec![vec![0.0; dims]; sample_count];
    for (dim_idx, row) in component_rows.iter().enumerate() {
        for (sample_idx, &value) in row.iter().enumerate() {
            samples[sample_idx][dim_idx] = value;
        }
    }
    samples
}

/// 中文：基于精确特征矩阵计算 t-SNE；样本过少时退回 fallback。
/// English: Compute t-SNE from the exact feature matrix, falling back when the
/// sample count is too small.
fn compute_tsne_scores(
    feature_matrix: &[Vec<f64>],
    fallback_2d: &[Vec<f64>],
) -> Result<Vec<Vec<f64>>> {
    let n_samples = feature_matrix.len();
    if n_samples < 3 {
        return Ok(fallback_2d.to_vec());
    }

    let n_features = feature_matrix[0].len();
    let data = Mat::from_fn(n_samples, n_features, |i, j| feature_matrix[i][j]);
    let perplexity = ((n_samples.saturating_sub(1) as f64) / 3.0).clamp(1.0, 30.0);
    let params = TsneParams::<f64>::new(
        Some(2),
        Some(perplexity),
        Some(1e-4),
        Some(200.0),
        Some(1000),
        None,
        Some(0.5),
        Some(3),
    );
    let embedding = tsne(data.as_ref(), None, &params, "bh", 42, false);
    Ok(embedding)
}

/// 中文：基于精确特征矩阵计算 UMAP；样本过少时退回 fallback。
/// English: Compute UMAP from the exact feature matrix, falling back when the
/// sample count is too small.
fn compute_umap_scores(
    feature_matrix: &[Vec<f64>],
    fallback_2d: &[Vec<f64>],
) -> Result<Vec<Vec<f64>>> {
    let n_samples = feature_matrix.len();
    if n_samples < 3 {
        return Ok(fallback_2d.to_vec());
    }

    let n_features = feature_matrix[0].len();
    let data = Mat::from_fn(n_samples, n_features, |i, j| feature_matrix[i][j]);
    let neighbors = n_samples.saturating_sub(1).clamp(2, 15);
    let params = UmapParams::<f64>::default_2d(Some(2), Some(neighbors), Some(0.1), Some(1.0));
    let embedding = umap(data.as_ref(), None, &params, 42, false);
    Ok(embedding)
}

/// 中文：从样本点积矩阵构造居中后的 Gram 矩阵，供 PCA 使用。
/// English: Construct the centered Gram matrix from sample dot products for PCA.
fn centered_gram_from_dots(dots: &[f64], n: usize) -> Vec<Vec<f64>> {
    if n == 0 {
        return Vec::new();
    }

    let mut row_means = vec![0.0; n];
    let mut total = 0.0;
    for i in 0..n {
        let mut row_sum = 0.0;
        for j in 0..n {
            row_sum += dots[i * n + j];
        }
        row_means[i] = row_sum / n as f64;
        total += row_sum;
    }
    let total_mean = total / (n * n) as f64;

    let mut centered = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in 0..n {
            centered[i][j] = dots[i * n + j] - row_means[i] - row_means[j] + total_mean;
        }
    }
    centered
}

/// 中文：对居中 Gram 矩阵做特征分解，得到样本坐标与解释份额。
/// English: Eigendecompose the centered Gram matrix to obtain sample
/// coordinates and explained shares.
fn exact_sample_coordinates_from_centered_gram(
    centered_gram: &[Vec<f64>],
) -> Result<(Vec<Vec<f64>>, Vec<f64>)> {
    let n = centered_gram.len();
    if n == 0 {
        return Ok((Vec::new(), Vec::new()));
    }

    let gram = Mat::from_fn(n, n, |i, j| centered_gram[i][j]);
    let evd = gram
        .as_ref()
        .self_adjoint_eigen(Side::Lower)
        .map_err(|err| anyhow!("failed to eigendecompose centered sketch Gram matrix: {err:?}"))?;
    let eigenvectors = evd.U();
    let eigenvalues = evd.S();
    let mut coords = Vec::with_capacity(n);
    let mut shares = Vec::with_capacity(n);
    let total_positive = (0..n).map(|idx| eigenvalues[idx].max(0.0)).sum::<f64>();

    for idx in (0..n).rev() {
        let lambda = eigenvalues[idx];
        if lambda <= 1e-12 {
            continue;
        }
        let scale = lambda.sqrt();
        let mut component = vec![0.0; n];
        for sample_idx in 0..n {
            component[sample_idx] = eigenvectors[(sample_idx, idx)] * scale;
        }
        coords.push(component);
        shares.push(if total_positive > 0.0 {
            lambda / total_positive
        } else {
            0.0
        });
    }

    Ok((coords, shares))
}

/// 中文：把投影坐标写成“分量为行、样本为列”的 TSV。
/// English: Write projection coordinates as a TSV with components in rows and
/// samples in columns.
fn write_component_table(
    path: &Path,
    prefix: &str,
    sample_names: &[String],
    rows: &[Vec<f64>],
) -> Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(path)?);
    write!(w, "component")?;
    for sample_name in sample_names {
        write!(w, "\t{sample_name}")?;
    }
    writeln!(w)?;
    for (idx, row) in rows.iter().enumerate() {
        write!(w, "{prefix}{}", idx + 1)?;
        for value in row {
            write!(w, "\t{value:.8}")?;
        }
        writeln!(w)?;
    }
    w.flush()?;
    Ok(())
}

/// 中文：把样本间相关性矩阵写成方阵 TSV。
/// English: Write the sample-to-sample correlation matrix as a square TSV.
fn write_correlation_matrix(
    path: &Path,
    sample_names: &[String],
    matrix: &[Vec<f64>],
) -> Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(path)?);
    write!(w, "sample")?;
    for name in sample_names {
        write!(w, "\t{name}")?;
    }
    writeln!(w)?;

    for (row_name, row) in sample_names.iter().zip(matrix.iter()) {
        write!(w, "{row_name}")?;
        for value in row {
            write!(w, "\t{value:.8}")?;
        }
        writeln!(w)?;
    }
    w.flush()?;
    Ok(())
}

/// 中文：构建 heatmap 聚类表，按 k-mer 行输出样本计数和 cluster 标签。
/// English: Build the clustered heatmap table, emitting sample counts and a
/// cluster label for each k-mer row.
fn build_heatmap_clustered_table(
    output: &Path,
    out_dir: &Path,
    samples: &[SketchSample],
    k: usize,
    sample_names: &[String],
    sample_order: &[usize],
    kmeans_k: usize,
) -> Result<()> {
    let centers = fit_row_kmeans(samples, kmeans_k)?;
    let tmp_dir = out_dir.join("heatmap_tmp");
    create_dir_all(&tmp_dir)?;

    let mut writers = Vec::with_capacity(centers.len());
    let mut bucket_paths = Vec::with_capacity(centers.len());
    for cluster_idx in 0..centers.len() {
        let path = tmp_dir.join(format!("cluster_{cluster_idx}.tsv"));
        bucket_paths.push(path.clone());
        writers.push(BufWriter::with_capacity(1 << 20, File::create(path)?));
    }

    let mut seq = vec![0u8; k];
    let mut line = Vec::<u8>::with_capacity(k + sample_names.len() * 16 + 16);
    visit_union_rows(samples, |key, row| {
        let cluster_idx = nearest_center(row, &centers);
        decode_u64_fast_into(key, k, &mut seq);
        line.clear();
        line.extend_from_slice(&seq);
        for &sample_idx in sample_order {
            line.push(b'\t');
            append_u64_ascii(&mut line, row[sample_idx]);
        }
        line.push(b'\t');
        append_u64_ascii(&mut line, (cluster_idx + 1) as u64);
        line.push(b'\n');
        writers[cluster_idx].write_all(&line)?;
        Ok(())
    })?;

    for writer in &mut writers {
        writer.flush()?;
    }

    let mut final_writer = BufWriter::with_capacity(4 << 20, File::create(output)?);
    final_writer.write_all(b"kmer")?;
    for &sample_idx in sample_order {
        final_writer.write_all(b"\t")?;
        final_writer.write_all(sample_names[sample_idx].as_bytes())?;
    }
    final_writer.write_all(b"\tkmeans_cluster\n")?;

    for path in &bucket_paths {
        let mut reader = BufReader::with_capacity(1 << 20, File::open(path)?);
        std::io::copy(&mut reader, &mut final_writer)?;
    }
    final_writer.flush()?;

    for path in bucket_paths {
        remove_file(path)?;
    }
    remove_dir_all(tmp_dir)?;
    Ok(())
}

/// 中文：对行向量做轻量级 k-means，供 heatmap 行聚类使用。
/// English: Fit a lightweight k-means model over row vectors for heatmap row
/// clustering.
fn fit_row_kmeans(samples: &[SketchSample], kmeans_k: usize) -> Result<Vec<Vec<f64>>> {
    if kmeans_k <= 1 {
        let dims = samples.len();
        return Ok(vec![vec![0.0; dims]]);
    }

    let reservoir = collect_reservoir_rows(samples, 4096)?;
    if reservoir.is_empty() {
        return Ok(vec![vec![0.0; samples.len()]]);
    }

    let k = kmeans_k.min(reservoir.len()).max(1);
    let mut centers = farthest_point_init(&reservoir, k);
    for _ in 0..6 {
        let mut sums = vec![vec![0.0; samples.len()]; k];
        let mut counts = vec![0usize; k];
        visit_union_rows(samples, |_, row| {
            let cluster_idx = nearest_center(row, &centers);
            counts[cluster_idx] += 1;
            for (dim, &value) in row.iter().enumerate() {
                sums[cluster_idx][dim] += value as f64;
            }
            Ok(())
        })?;

        for idx in 0..k {
            if counts[idx] > 0 {
                let denom = counts[idx] as f64;
                for value in &mut sums[idx] {
                    *value /= denom;
                }
                centers[idx] = sums[idx].clone();
            }
        }
    }

    Ok(centers)
}

/// 中文：从完整矩阵流中抽样若干代表性行，作为 k-means 初始化候选。
/// English: Sample representative rows from the full matrix stream for
/// k-means initialization.
fn collect_reservoir_rows(samples: &[SketchSample], limit: usize) -> Result<Vec<Vec<f64>>> {
    let mut rows = Vec::with_capacity(limit);
    visit_union_rows(samples, |_, row| {
        if rows.len() < limit {
            rows.push(row.iter().map(|&x| x as f64).collect());
        }
        Ok(())
    })?;
    Ok(rows)
}

/// 中文：用 farthest-point heuristic 初始化 k-means 中心。
/// English: Initialize k-means centers using a farthest-point heuristic.
fn farthest_point_init(rows: &[Vec<f64>], k: usize) -> Vec<Vec<f64>> {
    if rows.is_empty() {
        return Vec::new();
    }

    let mut centers = vec![rows[0].clone()];
    while centers.len() < k {
        let mut best_row = &rows[0];
        let mut best_dist = -1.0f64;
        for row in rows {
            let min_dist = centers
                .iter()
                .map(|center| squared_distance_f64(row, center))
                .fold(f64::INFINITY, f64::min);
            if min_dist > best_dist {
                best_dist = min_dist;
                best_row = row;
            }
        }
        centers.push(best_row.clone());
    }
    centers
}

/// 中文：为一行 count 向量找到最近的聚类中心。
/// English: Find the nearest cluster center for one count row.
fn nearest_center(row: &[u64], centers: &[Vec<f64>]) -> usize {
    let mut best_idx = 0usize;
    let mut best_dist = f64::INFINITY;
    for (idx, center) in centers.iter().enumerate() {
        let dist = squared_distance_u64_f64(row, center);
        if dist < best_dist {
            best_dist = dist;
            best_idx = idx;
        }
    }
    best_idx
}

/// 中文：根据相关性矩阵计算样本的平均连接排序。
/// English: Compute sample ordering with average-linkage clustering from the
/// correlation matrix.
fn average_linkage_order(correlation: &[Vec<f64>]) -> Vec<usize> {
    let n = correlation.len();
    let mut clusters: Vec<Vec<usize>> = (0..n).map(|idx| vec![idx]).collect();
    while clusters.len() > 1 {
        let mut best_pair = (0usize, 1usize);
        let mut best_dist = f64::INFINITY;
        for i in 0..clusters.len() {
            for j in (i + 1)..clusters.len() {
                let dist = average_cluster_distance(&clusters[i], &clusters[j], correlation);
                if dist < best_dist {
                    best_dist = dist;
                    best_pair = (i, j);
                }
            }
        }

        let right = clusters.remove(best_pair.1);
        let mut left = clusters.remove(best_pair.0);
        left.extend(right);
        clusters.push(left);
    }

    clusters.pop().unwrap_or_default()
}

/// 中文：计算两个聚类之间的平均距离。
/// English: Compute the average distance between two clusters.
fn average_cluster_distance(a: &[usize], b: &[usize], correlation: &[Vec<f64>]) -> f64 {
    let mut total = 0.0f64;
    let mut count = 0usize;
    for &i in a {
        for &j in b {
            total += 1.0 - correlation[i][j];
            count += 1;
        }
    }
    total / (count as f64).max(1.0)
}

/// 中文：计算 `u64` 行向量与 `f64` 中心之间的平方距离。
/// English: Compute squared distance between a `u64` row vector and an `f64`
/// center vector.
fn squared_distance_u64_f64(a: &[u64], b: &[f64]) -> f64 {
    let mut total = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = *x as f64 - *y;
        total += d * d;
    }
    total
}

/// 中文：计算两个 `f64` 向量之间的平方距离。
/// English: Compute squared distance between two `f64` vectors.
fn squared_distance_f64(a: &[f64], b: &[f64]) -> f64 {
    let mut total = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = *x - *y;
        total += d * d;
    }
    total
}

/// 中文：生成 correlation 热图对应的 R 脚本。
/// English: Generate the R script used to draw the correlation heatmap.
fn write_correlation_script(
    out_dir: &Path,
    correlation_path: &Path,
    metadata_path: &Path,
    plot_title: &str,
) -> Result<()> {
    let script = CORRELATION_SCRIPT
        .replace("__CORR_PATH__", &r_path(correlation_path))
        .replace("__META_PATH__", &r_path(metadata_path))
        .replace("__PLOT_TITLE__", plot_title);
    write_script(out_dir.join("plot_correlation.R"), &script)
}

/// 中文：生成 PCA / t-SNE / UMAP 的 R 绘图脚本。
/// English: Generate the R plotting script for PCA / t-SNE / UMAP.
fn write_projection_script(
    path: &Path,
    scores_path: &Path,
    metadata_path: &Path,
    prefix: &str,
    plot_title: &str,
    x_label: &str,
    y_label: &str,
) -> Result<()> {
    let script = PROJECTION_SCRIPT
        .replace("__SCORES_PATH__", &r_path(scores_path))
        .replace("__META_PATH__", &r_path(metadata_path))
        .replace("__PREFIX__", prefix)
        .replace("__PLOT_TITLE__", plot_title)
        .replace("__X_LABEL__", x_label)
        .replace("__Y_LABEL__", y_label);
    write_script(path.to_path_buf(), &script)
}

/// 中文：生成 heatmap 的 R 绘图脚本。
/// English: Generate the R plotting script for the heatmap.
fn write_heatmap_script(
    out_dir: &Path,
    heatmap_table_path: &Path,
    metadata_path: &Path,
) -> Result<()> {
    let script = HEATMAP_SCRIPT
        .replace("__HEATMAP_PATH__", &r_path(heatmap_table_path))
        .replace("__META_PATH__", &r_path(metadata_path));
    write_script(out_dir.join("plot_heatmap.R"), &script)
}

/// 中文：把任意脚本文本写到磁盘。
/// English: Write an arbitrary script string to disk.
fn write_script(path: PathBuf, script: &str) -> Result<()> {
    let mut w = BufWriter::with_capacity(64 << 10, File::create(path)?);
    w.write_all(script.as_bytes())?;
    w.flush()?;
    Ok(())
}

/// 中文：如果文件存在就删除。
/// English: Remove a file if it exists.
fn remove_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        remove_file(path)?;
    }
    Ok(())
}

/// 中文：统计输出目录里所有文件的总大小。
/// English: Sum the total size of all files in an output directory.
fn dir_bytes(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_file() {
            total += meta.len();
        }
    }
    Ok(total)
}

/// 中文：把相关性方法转换成短名字。
/// English: Convert a correlation method into a short display name.
fn correlation_method_name(method: CorrelationMethod) -> &'static str {
    match method {
        CorrelationMethod::Pearson => "pearson",
        CorrelationMethod::Cosine => "cosine",
    }
}

/// 中文：把相关性方法转换成适合图标题的名字。
/// English: Convert a correlation method into a plot-title-friendly name.
fn correlation_method_plot_title(method: CorrelationMethod) -> &'static str {
    match method {
        CorrelationMethod::Pearson => "Sample-to-sample correlation heatmap (Pearson)",
        CorrelationMethod::Cosine => "Sample-to-sample correlation heatmap (Cosine)",
    }
}

/// 中文：把本地路径转成可安全嵌入 R 脚本的字符串。
/// English: Convert a local path into a string that is safe to embed in R.
fn r_path(path: &Path) -> String {
    let raw = std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let normalized = raw.replace('\\', "/");
    normalized
        .trim_start_matches("//?/")
        .trim_start_matches(r"\\?\")
        .replace('"', "\\\"")
}

#[inline(always)]
/// 中文：把 `u64` 计数快速追加为 ASCII 文本，避免 `format!`。
/// English: Append a `u64` as ASCII text without using `format!`.
fn append_u64_ascii(buf: &mut Vec<u8>, value: u64) {
    if value == 0 {
        buf.push(b'0');
        return;
    }

    let start = buf.len();
    let mut n = value;
    while n > 0 {
        buf.push(b'0' + (n % 10) as u8);
        n /= 10;
    }
    buf[start..].reverse();
}

/// 中文：在线累计相关性和 PCA 所需统计量的状态机。
/// English: Stateful accumulator that collects the statistics needed for
/// correlation and PCA in one pass.
struct CorrelationAccumulator {
    method: CorrelationMethod,
    rows: u64,
    sums: Vec<f64>,
    sums_sq: Vec<f64>,
    dots: Vec<f64>,
}

impl CorrelationAccumulator {
    /// 中文：创建新的相关性累计器。
    /// English: Create a new correlation accumulator.
    fn new(samples: usize, method: CorrelationMethod) -> Self {
        Self {
            method,
            rows: 0,
            sums: vec![0.0; samples],
            sums_sq: vec![0.0; samples],
            dots: vec![0.0; samples * samples],
        }
    }

    /// 中文：吸收一行多样本 count，更新点积、和、平方和。
    /// English: Consume one multi-sample count row and update dot products,
    /// sums, and squared sums.
    fn update(&mut self, counts: &[u64]) {
        self.rows += 1;
        let n = counts.len();
        for (i, &count) in counts.iter().enumerate() {
            let xi = count as f64;
            self.sums[i] += xi;
            self.sums_sq[i] += xi * xi;
        }
        for i in 0..n {
            let xi = counts[i] as f64;
            for j in i..n {
                let xj = counts[j] as f64;
                let value = xi * xj;
                self.dots[i * n + j] += value;
                if i != j {
                    self.dots[j * n + i] += value;
                }
            }
        }
    }

    /// 中文：结束累计并输出完整分析结果。
    /// English: Finalize accumulation and produce the complete analysis result.
    fn finish(&self) -> SketchAnalysis {
        let correlation = self.correlation_matrix();
        let centered_gram = centered_gram_from_dots(&self.dots, self.sums.len());
        let (exact_sample_coords, pca_axis_shares) =
            exact_sample_coordinates_from_centered_gram(&centered_gram).unwrap_or_default();
        SketchAnalysis {
            correlation,
            exact_sample_coords,
            pca_axis_shares,
        }
    }

    /// 中文：根据累计统计量计算完整相关性矩阵。
    /// English: Compute the full correlation matrix from accumulated stats.
    fn correlation_matrix(&self) -> Vec<Vec<f64>> {
        let n = self.sums.len();
        let mut matrix = vec![vec![0.0; n]; n];
        for i in 0..n {
            matrix[i][i] = 1.0;
            for j in (i + 1)..n {
                let value = match self.method {
                    CorrelationMethod::Pearson => self.pearson(i, j),
                    CorrelationMethod::Cosine => self.cosine(i, j),
                };
                matrix[i][j] = value;
                matrix[j][i] = value;
            }
        }
        matrix
    }

    /// 中文：计算两个样本列之间的 Pearson 相关系数。
    /// English: Compute the Pearson correlation between two sample columns.
    fn pearson(&self, i: usize, j: usize) -> f64 {
        let n = self.rows as f64;
        if n <= 1.0 {
            return 0.0;
        }
        let sum_x = self.sums[i];
        let sum_y = self.sums[j];
        let sum_x2 = self.sums_sq[i];
        let sum_y2 = self.sums_sq[j];
        let sum_xy = self.dots[i * self.sums.len() + j];
        let numerator = n * sum_xy - sum_x * sum_y;
        let left = n * sum_x2 - sum_x * sum_x;
        let right = n * sum_y2 - sum_y * sum_y;
        let denom = (left * right).sqrt();
        if denom == 0.0 { 0.0 } else { numerator / denom }
    }

    /// 中文：计算两个样本列之间的 cosine 相似度。
    /// English: Compute cosine similarity between two sample columns.
    fn cosine(&self, i: usize, j: usize) -> f64 {
        let dot = self.dots[i * self.sums.len() + j];
        let denom = (self.sums_sq[i] * self.sums_sq[j]).sqrt();
        if denom == 0.0 { 0.0 } else { dot / denom }
    }
}

// 中文：下面三个字符串常量是 sketch 自动生成的 R 脚本模板。
// English: The three string constants below are the R-script templates
// generated by sketch.
const CORRELATION_SCRIPT: &str = r##"#!/usr/bin/env Rscript
# ============================================================
# User-editable settings
# ------------------------------------------------------------
# Edit this section first if you want to change the title,
# output filename, palette, text size, or general plot style.
# The rest of the script is intentionally structured so users
# can read it top-to-bottom without hunting for hidden options.
# ============================================================

plot_title <- "__PLOT_TITLE__"
output_png <- "sketch_correlation_heatmap.png"
device_width <- 7
device_height <- 6
base_size <- 12
tile_low <- "#2166AC"
tile_mid <- "white"
tile_high <- "#B2182B"
text_size <- 3.2
x_text_angle <- 45
plot_margin_lines <- c(0.8, 0.8, 0.8, 0.8)

# ============================================================
# Dependency bootstrap
# ------------------------------------------------------------
# 1. Make sure `pak` exists.
# 2. If a required package is missing, install it with `pak`.
# 3. Print a short status line so users know what happened.
# ============================================================

ensure_pak <- function() {
  if (requireNamespace("pak", quietly = TRUE)) {
    message("[kite sketch] Package 'pak' is installed.")
    return(invisible(TRUE))
  }

  message("[kite sketch] Package 'pak' is missing. Installing from CRAN...")
  install.packages("pak", repos = "https://cloud.r-project.org")

  if (!requireNamespace("pak", quietly = TRUE)) {
    stop("Failed to install the 'pak' package.", call. = FALSE)
  }

  message("[kite sketch] Package 'pak' installed successfully.")
}

ensure_packages <- function(package_refs) {
  ensure_pak()

  missing_refs <- character()
  for (pkg in names(package_refs)) {
    if (requireNamespace(pkg, quietly = TRUE)) {
      message(sprintf("[kite sketch] Package '%s' is installed.", pkg))
    } else {
      message(sprintf("[kite sketch] Package '%s' is missing.", pkg))
      missing_refs <- c(missing_refs, unname(package_refs[[pkg]]))
    }
  }

  if (length(missing_refs) > 0) {
    message("[kite sketch] Installing missing packages with pak...")
    pak::pak(missing_refs)
  }

  for (pkg in names(package_refs)) {
    if (!requireNamespace(pkg, quietly = TRUE)) {
      stop(sprintf("Package '%s' is still unavailable after installation.", pkg), call. = FALSE)
    }
  }
}

ensure_packages(c(
  data.table = "data.table",
  ggplot2 = "ggplot2"
))

suppressPackageStartupMessages({
  library(data.table)
  library(ggplot2)
})

# ============================================================
# Helper functions
# ============================================================

load_sample_metadata <- function(path, sample_ids) {
  meta <- fread(path)
  meta <- meta[match(sample_ids, sample_id)]
  meta[is.na(sample_label) | sample_label == "", sample_label := sample_id]
  meta[is.na(sample_group) | sample_group == "", sample_group := sample_id]
  meta
}

# ============================================================
# Step 1: Load the Kite-generated correlation table and map the
# raw sample ids to the user-facing sample labels.
# ============================================================

tbl <- fread("__CORR_PATH__")
sample_ids <- colnames(tbl)[-1]
meta <- load_sample_metadata("__META_PATH__", sample_ids)
label_map <- setNames(meta$sample_label, meta$sample_id)
sample_order <- tbl[[1]]
sample_labels <- label_map[sample_order]
column_labels <- label_map[sample_ids]

# ============================================================
# Step 2: Convert the square matrix into long format so ggplot2
# can draw one tile per sample-vs-sample comparison.
# ============================================================

long_tbl <- melt(tbl, id.vars = "sample", variable.name = "sample_j", value.name = "correlation")
long_tbl[, sample := factor(label_map[sample], levels = sample_labels)]
long_tbl[, sample_j := factor(label_map[sample_j], levels = column_labels)]
long_tbl[, score_label := sprintf("%.2f", correlation)]

# ============================================================
# Step 3: Draw the heatmap. Each tile color encodes the
# correlation score, and each tile also prints the exact score.
# ============================================================

p <- ggplot(long_tbl, aes(sample_j, sample, fill = correlation)) +
  geom_tile() +
  geom_text(aes(label = score_label), size = text_size, color = "black") +
  scale_fill_gradient2(
    low = tile_low,
    mid = tile_mid,
    high = tile_high,
    midpoint = 0,
    name = "Score"
  ) +
  ggtitle(plot_title) +
  theme_bw(base_size = base_size) +
  xlab(NULL) +
  ylab(NULL) +
  theme(
    plot.title = element_text(hjust = 0.5, face = "bold"),
    axis.text.x = element_text(angle = x_text_angle, hjust = 1),
    panel.grid = element_blank(),
    legend.title = element_text(face = "bold"),
    plot.margin = margin(
      t = plot_margin_lines[1],
      r = plot_margin_lines[2],
      b = plot_margin_lines[3],
      l = plot_margin_lines[4],
      unit = "lines"
    )
  )

ggsave(output_png, p, width = device_width, height = device_height)
"##;

const PROJECTION_SCRIPT: &str = r##"#!/usr/bin/env Rscript
# ============================================================
# User-editable settings
# ------------------------------------------------------------
# Edit this section first to customize the title, output file,
# point size, label size, colors, or plot theme.
# ============================================================

plot_title <- "__PLOT_TITLE__"
x_label <- "__X_LABEL__"
y_label <- "__Y_LABEL__"
output_png <- paste0(tolower("__PREFIX__"), ".png")
device_width <- 8
device_height <- 6
base_size <- 12
point_size <- 3
label_size <- 3.2
use_ggrepel <- TRUE
group_palette_name <- "Dark 3"
plot_margin_lines <- c(0.8, 0.8, 0.8, 0.8)

# ============================================================
# Dependency bootstrap
# ------------------------------------------------------------
# 1. Make sure `pak` exists.
# 2. Install missing plotting packages with `pak`.
# 3. Load the packages after they are confirmed to exist.
# ============================================================

ensure_pak <- function() {
  if (requireNamespace("pak", quietly = TRUE)) {
    message("[kite sketch] Package 'pak' is installed.")
    return(invisible(TRUE))
  }

  message("[kite sketch] Package 'pak' is missing. Installing from CRAN...")
  install.packages("pak", repos = "https://cloud.r-project.org")

  if (!requireNamespace("pak", quietly = TRUE)) {
    stop("Failed to install the 'pak' package.", call. = FALSE)
  }

  message("[kite sketch] Package 'pak' installed successfully.")
}

ensure_packages <- function(package_refs) {
  ensure_pak()

  missing_refs <- character()
  for (pkg in names(package_refs)) {
    if (requireNamespace(pkg, quietly = TRUE)) {
      message(sprintf("[kite sketch] Package '%s' is installed.", pkg))
    } else {
      message(sprintf("[kite sketch] Package '%s' is missing.", pkg))
      missing_refs <- c(missing_refs, unname(package_refs[[pkg]]))
    }
  }

  if (length(missing_refs) > 0) {
    message("[kite sketch] Installing missing packages with pak...")
    pak::pak(missing_refs)
  }

  for (pkg in names(package_refs)) {
    if (!requireNamespace(pkg, quietly = TRUE)) {
      stop(sprintf("Package '%s' is still unavailable after installation.", pkg), call. = FALSE)
    }
  }
}

ensure_packages(c(
  data.table = "data.table",
  ggplot2 = "ggplot2",
  ggrepel = "ggrepel"
))

suppressPackageStartupMessages({
  library(data.table)
  library(ggplot2)
  library(ggrepel)
})

# ============================================================
# Helper functions
# ============================================================

load_sample_metadata <- function(path, sample_ids) {
  meta <- fread(path)
  meta <- meta[match(sample_ids, sample_id)]
  meta[is.na(sample_label) | sample_label == "", sample_label := sample_id]
  meta[is.na(sample_group) | sample_group == "", sample_group := sample_id]
  meta
}

build_group_palette <- function(groups, palette_name) {
  group_levels <- unique(groups)
  palette_values <- grDevices::hcl.colors(length(group_levels), palette = palette_name)
  setNames(palette_values, group_levels)
}

extract_component_values <- function(tbl, component_id, sample_count) {
  component_row <- tbl[component == component_id, -1, with = FALSE]
  if (nrow(component_row) == 0) {
    return(rep(0, sample_count))
  }
  as.numeric(component_row[1])
}

# ============================================================
# Step 1: Load the precomputed projection scores generated by
# Kite, then merge them with the editable sample metadata table.
# ============================================================

tbl <- fread("__SCORES_PATH__")
samples <- colnames(tbl)[-1]
meta <- load_sample_metadata("__META_PATH__", samples)
component_id_x <- paste0("__PREFIX__", "1")
component_id_y <- paste0("__PREFIX__", "2")
x_vals <- extract_component_values(tbl, component_id_x, length(samples))
y_vals <- extract_component_values(tbl, component_id_y, length(samples))

plot_df <- data.table(sample_id = samples, x = x_vals, y = y_vals)
plot_df <- merge(plot_df, meta, by = "sample_id", sort = FALSE, all.x = TRUE)
plot_df[is.na(sample_label) | sample_label == "", sample_label := sample_id]
plot_df[is.na(sample_group) | sample_group == "", sample_group := sample_id]
palette_map <- build_group_palette(plot_df$sample_group, group_palette_name)

# ============================================================
# Step 2: Draw the scatter plot. Points are colored by group,
# and labels come from `sample_label` in the metadata table.
# ============================================================

p <- ggplot(plot_df, aes(x, y, color = sample_group, label = sample_label)) +
  geom_point(size = point_size) +
  scale_color_manual(values = palette_map, name = "Group") +
  ggtitle(plot_title) +
  theme_bw(base_size = base_size) +
  xlab(x_label) +
  ylab(y_label) +
  theme(
    plot.title = element_text(hjust = 0.5, face = "bold"),
    panel.grid.minor = element_blank(),
    legend.title = element_text(face = "bold"),
    plot.margin = margin(
      t = plot_margin_lines[1],
      r = plot_margin_lines[2],
      b = plot_margin_lines[3],
      l = plot_margin_lines[4],
      unit = "lines"
    )
  )

if (use_ggrepel) {
  p <- p + ggrepel::geom_text_repel(show.legend = FALSE, size = label_size, max.overlaps = 100)
} else {
  p <- p + geom_text(show.legend = FALSE, vjust = -0.6, size = label_size)
}

ggsave(output_png, p, width = device_width, height = device_height)
"##;

const HEATMAP_SCRIPT: &str = r##"#!/usr/bin/env Rscript
# ============================================================
# User-editable settings
# ------------------------------------------------------------
# Edit this section first to control the heatmap title, output
# filename, z-score behavior, dimensions, clustering settings,
# palette, and label rotation.
# ============================================================

plot_title <- "K-mer heatmap grouped by Rust-side k-means clusters"
output_png <- "sketch_heatmap.png"
use_log1p_before_zscore <- TRUE
use_row_zscore <- TRUE
max_rows <- 2000L
device_width <- 13
device_height <- 14
fontsize_col <- 10
sample_label_angle <- 45
sample_tree_height_mm <- 60
sample_clustering_method <- "average"
z_score_limits <- c(-2.5, 2.5)
count_limits <- c(0, 8)
heatmap_palette_z <- circlize::colorRamp2(
  c(z_score_limits[1], 0, z_score_limits[2]),
  c("#2166AC", "white", "#B2182B")
)
heatmap_palette_count <- circlize::colorRamp2(
  c(count_limits[1], mean(count_limits), count_limits[2]),
  c("white", "#9ECAE1", "#08519C")
)
left_padding_mm <- 14
bottom_padding_mm <- 16

# ============================================================
# Dependency bootstrap
# ------------------------------------------------------------
# 1. Make sure `pak` exists.
# 2. Install any missing packages with `pak`.
# 3. Load the plotting libraries once they are available.
# ComplexHeatmap is installed from Bioconductor via `pak`.
# ============================================================

ensure_pak <- function() {
  if (requireNamespace("pak", quietly = TRUE)) {
    message("[kite sketch] Package 'pak' is installed.")
    return(invisible(TRUE))
  }

  message("[kite sketch] Package 'pak' is missing. Installing from CRAN...")
  install.packages("pak", repos = "https://cloud.r-project.org")

  if (!requireNamespace("pak", quietly = TRUE)) {
    stop("Failed to install the 'pak' package.", call. = FALSE)
  }

  message("[kite sketch] Package 'pak' installed successfully.")
}

ensure_packages <- function(package_refs) {
  ensure_pak()

  missing_refs <- character()
  for (pkg in names(package_refs)) {
    if (requireNamespace(pkg, quietly = TRUE)) {
      message(sprintf("[kite sketch] Package '%s' is installed.", pkg))
    } else {
      message(sprintf("[kite sketch] Package '%s' is missing.", pkg))
      missing_refs <- c(missing_refs, unname(package_refs[[pkg]]))
    }
  }

  if (length(missing_refs) > 0) {
    message("[kite sketch] Installing missing packages with pak...")
    pak::pak(missing_refs)
  }

  for (pkg in names(package_refs)) {
    if (!requireNamespace(pkg, quietly = TRUE)) {
      stop(sprintf("Package '%s' is still unavailable after installation.", pkg), call. = FALSE)
    }
  }
}

ensure_packages(c(
  data.table = "data.table",
  circlize = "circlize",
  ComplexHeatmap = "bioc::ComplexHeatmap"
))

suppressPackageStartupMessages({
  library(data.table)
  library(ComplexHeatmap)
  library(circlize)
  library(grid)
})

# ============================================================
# Helper functions
# ============================================================

load_sample_metadata <- function(path, sample_ids) {
  meta <- fread(path)
  meta <- meta[match(sample_ids, sample_id)]
  meta[is.na(sample_label) | sample_label == "", sample_label := sample_id]
  meta[is.na(sample_group) | sample_group == "", sample_group := sample_id]
  meta
}

build_group_palette <- function(groups) {
  group_levels <- unique(groups)
  palette_values <- grDevices::hcl.colors(length(group_levels), palette = "Dark 3")
  setNames(palette_values, group_levels)
}

build_cluster_palette <- function(cluster_ids) {
  cluster_levels <- unique(as.character(cluster_ids))
  palette_values <- grDevices::hcl.colors(length(cluster_levels), palette = "Set 2")
  setNames(palette_values, cluster_levels)
}

row_zscore <- function(mat) {
  z <- t(scale(t(mat)))
  z[!is.finite(z)] <- 0
  z
}

# ============================================================
# Step 1: Load the clustered heatmap table and reorder rows by
# the Rust-generated k-means cluster labels and then by k-mer.
# ============================================================

tbl <- fread("__HEATMAP_PATH__")
setorder(tbl, kmeans_cluster, kmer)
sample_cols <- colnames(tbl)[2:(ncol(tbl) - 1)]
meta <- load_sample_metadata("__META_PATH__", sample_cols)
label_map <- setNames(meta$sample_label, meta$sample_id)
sample_labels <- label_map[sample_cols]

# ============================================================
# Step 2: Build the numeric matrix. If requested, apply `log1p`
# before row-wise z-scoring so the heatmap emphasizes relative
# sample shifts instead of raw count magnitude.
# ============================================================

matrix_tbl <- copy(tbl)
setnames(matrix_tbl, sample_cols, sample_labels)
ordered_labels <- sample_labels
mat <- as.matrix(matrix_tbl[, ..ordered_labels])
storage.mode(mat) <- "numeric"
rownames(mat) <- matrix_tbl$kmer
annotation_row <- data.frame(kmeans_cluster = factor(matrix_tbl$kmeans_cluster))
rownames(annotation_row) <- matrix_tbl$kmer

value_mat <- if (use_log1p_before_zscore) log1p(mat) else mat
plot_mat <- if (use_row_zscore) row_zscore(value_mat) else value_mat

if (nrow(plot_mat) > max_rows) {
  row_score <- apply(plot_mat, 1, var)
  keep_unsorted <- order(row_score, decreasing = TRUE)[seq_len(max_rows)]
  keep <- sort(keep_unsorted)
  plot_mat <- plot_mat[keep, , drop = FALSE]
  annotation_row <- annotation_row[keep, , drop = FALSE]
}

# ============================================================
# Step 3: Prepare color maps, top sample-group annotation, left
# k-mer-cluster annotation, and the legends shown on the right.
# ============================================================

cluster_factor <- factor(annotation_row$kmeans_cluster, levels = sort(unique(annotation_row$kmeans_cluster)))
group_palette <- build_group_palette(meta$sample_group)
cluster_palette <- build_cluster_palette(cluster_factor)
color_fun <- if (use_row_zscore) heatmap_palette_z else heatmap_palette_count
legend_title <- if (use_row_zscore) "Z-score" else "Count"

top_anno <- HeatmapAnnotation(
  Group = meta$sample_group,
  col = list(Group = group_palette),
  show_annotation_name = FALSE,
  show_legend = FALSE,
  simple_anno_size = unit(4, "mm")
)

left_anno <- rowAnnotation(
  Cluster = cluster_factor,
  col = list(Cluster = cluster_palette),
  show_annotation_name = FALSE,
  show_legend = FALSE,
  width = unit(4, "mm")
)

value_legend <- if (use_row_zscore) {
  Legend(
    title = "Z-score",
    col_fun = heatmap_palette_z,
    at = c(z_score_limits[1], 0, z_score_limits[2]),
    direction = "vertical"
  )
} else {
  Legend(
    title = "Count",
    col_fun = heatmap_palette_count,
    at = pretty(count_limits, n = 5),
    direction = "vertical"
  )
}

group_legend <- Legend(
  title = "Group",
  at = names(group_palette),
  legend_gp = gpar(fill = unname(group_palette)),
  direction = "vertical"
)

cluster_legend <- Legend(
  title = "K-mer cluster",
  at = names(cluster_palette),
  legend_gp = gpar(fill = unname(cluster_palette)),
  direction = "vertical"
)

# ============================================================
# Step 4: Build and render the heatmap. Rows remain in the
# Rust-side cluster order, while sample columns are clustered.
# ============================================================

ht <- left_anno + Heatmap(
  plot_mat,
  name = legend_title,
  col = color_fun,
  top_annotation = top_anno,
  cluster_rows = FALSE,
  cluster_columns = TRUE,
  clustering_method_columns = sample_clustering_method,
  column_dend_height = unit(sample_tree_height_mm, "mm"),
  show_row_names = FALSE,
  show_column_names = TRUE,
  column_names_rot = sample_label_angle,
  column_names_gp = gpar(fontsize = fontsize_col),
  column_title = plot_title,
  column_title_gp = gpar(fontsize = 13, fontface = "bold"),
  show_heatmap_legend = FALSE
)

png(output_png, width = device_width, height = device_height, units = "in", res = 300)
draw(
  ht,
  heatmap_legend_side = "right",
  annotation_legend_side = "right",
  merge_legend = FALSE,
  heatmap_legend_list = list(value_legend),
  annotation_legend_list = list(group_legend, cluster_legend),
  padding = unit(c(6, 6, bottom_padding_mm, left_padding_mm), "mm")
)
dev.off()
"##;
