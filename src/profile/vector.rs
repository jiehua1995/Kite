use super::kmer::decode_u64_fast_into;
use crate::dump::decode::read_u64_count_vec;
use anyhow::{Result, bail};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

// 中文：`profile` 的核心实现是“多路有序归并”，把多个样本的 k-mer
// 流对齐成同一张矩阵；缺失值补 0。
// English: The core `profile` implementation performs a multi-way ordered
// merge so several sample k-mer streams become one aligned matrix with zeros
// for missing entries.
struct CountSample {
    name: String,
    entries: Vec<(u64, u64)>,
}

/// 中文：执行多样本 profile 构建。输入是多个 `.kite` 路径、输出文件路径、
/// 文本格式和可选样本名；输出是一个按 k-mer 对齐的 count 矩阵。
/// English: Build a multi-sample profile. Inputs are `.kite` paths, output
/// path, text format, and optional sample names; output is an aligned count
/// matrix keyed by k-mer.
pub fn run_profile(
    inputs: &[PathBuf],
    output: &Path,
    format: &str,
    sample_names: &[String],
) -> Result<()> {
    if inputs.is_empty() {
        bail!("profile requires at least one input");
    }

    let names = resolve_sample_names(inputs, sample_names)?;
    let mut samples = Vec::with_capacity(inputs.len());
    let mut shared_k = None::<usize>;
    for (path, name) in inputs.iter().zip(names.into_iter()) {
        let (info, entries, _total_count) = read_u64_count_vec(path)?;
        if let Some(expected_k) = shared_k {
            if info.k != expected_k {
                bail!(
                    "profile requires all inputs to have the same k: expected {}, got {} for {}",
                    expected_k,
                    info.k,
                    path.display()
                );
            }
        } else {
            shared_k = Some(info.k);
        }
        samples.push(CountSample { name, entries });
    }

    let sep = match format.to_lowercase().as_str() {
        "csv" => ",",
        _ => "\t",
    };

    let mut w = BufWriter::with_capacity(1 << 20, File::create(output)?);
    write!(w, "kmer")?;
    for sample in &samples {
        write!(w, "{}{}", sep, sample.name)?;
    }
    writeln!(w)?;

    write_union_table(&mut w, &samples, shared_k.unwrap_or(0), sep.as_bytes())?;
    w.flush()?;
    Ok(())
}

/// 中文：解析 profile 的列名。如果用户没提供，就从输入文件名自动推断。
/// English: Resolve profile column names. If the user provides none, derive
/// them from the input file stems.
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

/// 中文：把多个已排序的样本条目写成统一的 union 表。
/// 输入是输出 writer、样本数组、k 长度和字段分隔符；输出是逐行写出的矩阵文本。
/// English: Write multiple sorted sample streams into one union table.
/// Inputs are the output writer, sample array, k length, and field separator;
/// output is the streamed matrix text.
fn write_union_table<W: Write>(
    writer: &mut W,
    samples: &[CountSample],
    k: usize,
    sep: &[u8],
) -> Result<()> {
    let mut positions = vec![0usize; samples.len()];
    let mut seq_buf = vec![0u8; k];
    let mut line = Vec::<u8>::with_capacity(k + samples.len() * 16 + 1);

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

        decode_u64_fast_into(key, k, &mut seq_buf);
        line.clear();
        line.extend_from_slice(&seq_buf);
        for (idx, sample) in samples.iter().enumerate() {
            let count = if let Some(&(cur_key, count)) = sample.entries.get(positions[idx]) {
                if cur_key == key {
                    positions[idx] += 1;
                    count
                } else {
                    0
                }
            } else {
                0
            };
            line.extend_from_slice(sep);
            append_u64_ascii(&mut line, count);
        }
        line.push(b'\n');
        writer.write_all(&line)?;
    }

    Ok(())
}

#[inline(always)]
/// 中文：把 `u64` 计数快速追加为 ASCII 文本，避免 `format!` 带来的分配。
/// English: Append a `u64` count as ASCII text without using `format!`, which
/// avoids extra allocation during large matrix writes.
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
