use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context};
use indicatif::{ProgressBar, ProgressStyle};

use crate::count::kmer::{KmerIter, KmerIter128, KmerIterU256};
use crate::dump::decoder::{decode_kmer_any, decode_kmer_u256};
use crate::dump::reader::KiteReader;
use crate::format::header::{KEY_WIDTH_U128, KEY_WIDTH_U256, KEY_WIDTH_U64};
use crate::format::run_block::{
    encode_dvle_u128, encode_dvle_u64, read_varint_u128, read_varint_u64,
};
use crate::format::{CODEC_DVLE, CODEC_RAW};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct KmerKey {
    pub hi: u128,
    pub lo: u128,
}

#[derive(Clone, Debug)]
pub struct CompatibleMetadata {
    pub k: u8,
    pub canonical: bool,
    pub partitions: usize,
    pub key_width: u16,
}

pub struct SerializedPartitionChunk {
    pub codec: u32,
    pub row_count: u64,
    pub raw_bytes: u64,
    pub data: Vec<u8>,
    pub key_min_lo: u128,
    pub key_min_hi: u128,
    pub key_max_lo: u128,
    pub key_max_hi: u128,
}

pub fn count_passes_filter(count: u64, min_count: Option<u32>, max_count: Option<u32>) -> bool {
    if let Some(min_count) = min_count {
        if count < min_count as u64 {
            return false;
        }
    }
    if let Some(max_count) = max_count {
        if count > max_count as u64 {
            return false;
        }
    }
    true
}

pub fn format_count_filter(value: Option<u32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "none".to_string())
}

pub fn format_reader_filter(reader: &KiteReader) -> String {
    format!(
        "min={}, max={}",
        format_count_filter(reader.header.min_count),
        format_count_filter(reader.header.max_count)
    )
}

pub fn print_command_line(command: &str) {
    let mut args = std::env::args_os();
    let exe = args
        .next()
        .and_then(|value| Path::new(&value).file_name().map(|name| name.to_owned()))
        .and_then(|name| name.into_string().ok())
        .unwrap_or_else(|| "kite".to_string());
    let rendered = std::iter::once(exe)
        .chain(args.map(|arg| shell_quote(&arg.to_string_lossy())))
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("[kite {}] Command: {}", command, rendered);
}

fn shell_quote(value: &str) -> String {
    if value.is_empty()
        || value
            .chars()
            .any(|ch| !matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' | '/' | '=' | ':' | ',' | '@'))
    {
        let escaped = value.replace('\'', "'\\''");
        format!("'{}'", escaped)
    } else {
        value.to_string()
    }
}

pub fn count_passes_reader_filter(reader: &KiteReader, count: u64) -> bool {
    count_passes_filter(count, reader.header.min_count, reader.header.max_count)
}

pub fn apply_reader_filter_to_rows(reader: &KiteReader, rows: &mut Vec<(KmerKey, u64)>) {
    if reader.header.min_count.is_none() && reader.header.max_count.is_none() {
        return;
    }
    rows.retain(|(_, count)| count_passes_reader_filter(reader, *count));
}

pub fn create_output_writer(path: &str) -> anyhow::Result<Box<dyn Write>> {
    if path == "-" {
        Ok(Box::new(BufWriter::new(std::io::stdout())))
    } else {
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        Ok(Box::new(BufWriter::new(File::create(path)?)))
    }
}

pub fn infer_sample_labels(paths: &[String], labels: &[String]) -> anyhow::Result<Vec<String>> {
    if !labels.is_empty() && labels.len() != paths.len() {
        bail!(
            "--label count ({}) must match input count ({}). labels=[{}], inputs=[{}]",
            labels.len(),
            paths.len(),
            labels.join(", "),
            paths.join(", ")
        );
    }
    if !labels.is_empty() {
        return Ok(labels.to_vec());
    }

    let mut derived = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let file_name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path);
        let mut label = file_name.to_string();
        for suffix in [
            ".fastq.gz",
            ".fq.gz",
            ".fastq",
            ".fq",
            ".kite",
            ".tsv",
            ".txt",
            ".gz",
        ] {
            if let Some(stripped) = label.strip_suffix(suffix) {
                label = stripped.to_string();
                break;
            }
        }
        if label.is_empty() {
            label = format!("sample{}", index + 1);
        }
        derived.push(label);
    }
    Ok(derived)
}

pub fn infer_sample_groups(names: &[String], groups: &[String]) -> anyhow::Result<Vec<String>> {
    if !groups.is_empty() && groups.len() != names.len() {
        bail!(
            "--group count ({}) must match input count ({}). groups=[{}], inputs=[{}]",
            groups.len(),
            names.len(),
            groups.join(", "),
            names.join(", ")
        );
    }
    if !groups.is_empty() {
        return Ok(groups.to_vec());
    }
    Ok(names.to_vec())
}

pub fn infer_sample_labels_and_groups(
    paths: &[String],
    labels: &[String],
    groups: &[String],
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let labels = infer_sample_labels(paths, labels)?;
    let groups = infer_sample_groups(&labels, groups)?;
    Ok((labels, groups))
}

pub fn open_readers(paths: &[String]) -> anyhow::Result<Vec<Arc<KiteReader>>> {
    let mut readers = Vec::with_capacity(paths.len());
    for path in paths {
        readers.push(Arc::new(
            KiteReader::open(path).with_context(|| format!("failed to open {}", path))?,
        ));
    }
    Ok(readers)
}

pub fn ensure_compatible_with_paths(
    readers: &[Arc<KiteReader>],
    paths: &[String],
) -> anyhow::Result<CompatibleMetadata> {
    ensure_compatible_with_names(readers, Some(paths))
}

fn input_name(index: usize, paths: Option<&[String]>) -> String {
    paths
        .and_then(|paths| paths.get(index))
        .map(|path| format!("input #{} ({})", index + 1, path))
        .unwrap_or_else(|| format!("input #{}", index + 1))
}

fn ensure_compatible_with_names(
    readers: &[Arc<KiteReader>],
    paths: Option<&[String]>,
) -> anyhow::Result<CompatibleMetadata> {
    let Some(first) = readers.first() else {
        bail!("no input .kite files provided");
    };
    let meta = CompatibleMetadata {
        k: first.header.k,
        canonical: first.header.canonical != 0,
        partitions: first.header.partitions as usize,
        key_width: first.header.key_width,
    };
    for (index, reader) in readers.iter().enumerate().skip(1) {
        let name = input_name(index, paths);
        let first_name = input_name(0, paths);
        if reader.header.k != meta.k {
            bail!(
                "{} has k={}, expected k={} from {}",
                name,
                reader.header.k,
                meta.k,
                first_name
            );
        }
        if (reader.header.canonical != 0) != meta.canonical {
            bail!(
                "{} has canonical={}, expected canonical={} from {}",
                name,
                reader.header.canonical != 0,
                meta.canonical,
                first_name
            );
        }
        if reader.header.partitions as usize != meta.partitions {
            bail!(
                "{} has partitions={}, expected partitions={} from {}",
                name,
                reader.header.partitions,
                meta.partitions,
                first_name
            );
        }
        if reader.header.key_width != meta.key_width {
            bail!(
                "{} has key_width={}, expected key_width={} from {}",
                name,
                reader.header.key_width,
                meta.key_width,
                first_name
            );
        }
    }
    Ok(meta)
}

pub fn metadata_from_reader(reader: &Arc<KiteReader>) -> CompatibleMetadata {
    CompatibleMetadata {
        k: reader.header.k,
        canonical: reader.header.canonical != 0,
        partitions: reader.header.partitions as usize,
        key_width: reader.header.key_width,
    }
}

pub fn bucket_for_key(key: KmerKey, partitions: usize, key_width: u16) -> usize {
    let bucket_bits = partitions.trailing_zeros();
    match key_width {
        KEY_WIDTH_U64 => {
            let value = key.lo as u64;
            (value.wrapping_mul(0x9E3779B97F4A7C15u64) >> (64 - bucket_bits)) as usize
        }
        KEY_WIDTH_U128 => {
            let lo = key.lo as u64;
            let hi = (key.lo >> 64) as u64;
            ((lo ^ hi).wrapping_mul(0x9E3779B97F4A7C15u64) >> (64 - bucket_bits)) as usize
        }
        _ => {
            let a = key.lo as u64 ^ (key.lo >> 64) as u64;
            let b = key.hi as u64 ^ (key.hi >> 64) as u64;
            ((a ^ b).wrapping_mul(0x9E3779B97F4A7C15u64) >> (64 - bucket_bits)) as usize
        }
    }
}

pub fn encode_query_kmer(seq: &str, meta: &CompatibleMetadata) -> anyhow::Result<KmerKey> {
    let bytes = seq.as_bytes();
    if bytes.len() != meta.k as usize {
        bail!(
            "query k-mer length {} does not match database k={}",
            bytes.len(),
            meta.k
        );
    }
    if bytes
        .iter()
        .any(|base| !matches!(base, b'A' | b'C' | b'G' | b'T' | b'a' | b'c' | b'g' | b't'))
    {
        bail!("query k-mer contains non-ACGT characters");
    }

    match meta.key_width {
        KEY_WIDTH_U64 => {
            let mut it = KmerIter::new(bytes, meta.k.min(32), meta.canonical);
            let Some(key) = it.next() else {
                bail!("failed to encode query k-mer");
            };
            Ok(KmerKey {
                hi: 0,
                lo: key as u128,
            })
        }
        KEY_WIDTH_U128 => {
            let mut it = KmerIter128::new(bytes, meta.k, meta.canonical);
            let Some(key) = it.next() else {
                bail!("failed to encode query k-mer");
            };
            Ok(KmerKey { hi: 0, lo: key })
        }
        KEY_WIDTH_U256 => {
            let mut it = KmerIterU256::new(bytes, meta.k, meta.canonical);
            let Some(key) = it.next() else {
                bail!("failed to encode query k-mer");
            };
            Ok(KmerKey {
                hi: key.hi,
                lo: key.lo,
            })
        }
        other => bail!("unsupported key_width={other}"),
    }
}

pub fn key_to_string(key: KmerKey, meta: &CompatibleMetadata) -> String {
    let mut buf = vec![0u8; meta.k as usize];
    fill_key_buffer(key, meta, &mut buf);
    String::from_utf8(buf).unwrap_or_else(|_| "N".repeat(meta.k as usize))
}

pub fn fill_key_buffer(key: KmerKey, meta: &CompatibleMetadata, buf: &mut [u8]) {
    if meta.key_width == KEY_WIDTH_U256 {
        decode_kmer_u256(key.lo, key.hi, meta.k, buf);
    } else {
        decode_kmer_any(key.lo, meta.k, buf);
    }
}

pub struct PartitionProgress {
    bar: Option<ProgressBar>,
    command: String,
    total: usize,
    started: Instant,
}

impl PartitionProgress {
    pub fn new(command: impl Into<String>, enabled: bool, total: usize, started: Instant) -> Self {
        let command = command.into();
        let bar = enabled.then(|| {
            let bar = ProgressBar::new(total.max(1) as u64);
            let style = ProgressStyle::with_template(
                "{spinner:.green} {msg}\n[{bar:40.cyan/blue}] {percent:>3}% | {pos}/{len} partitions | eta {eta_precise}",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar());
            bar.set_style(style.progress_chars("=>-"));
            bar.enable_steady_tick(std::time::Duration::from_millis(120));
            bar
        });
        Self {
            bar,
            command,
            total,
            started,
        }
    }

    pub fn update(&self, done: usize) {
        let Some(bar) = &self.bar else {
            return;
        };
        let total = self.total.max(done).max(1);
        bar.set_length(total as u64);
        bar.set_position(done.min(total) as u64);
        bar.set_message(format!(
            "{} | elapsed {}",
            self.command,
            format_duration_compact(self.started.elapsed()),
        ));
    }

    pub fn finish(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

impl Drop for PartitionProgress {
    fn drop(&mut self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

fn format_duration_compact(duration: std::time::Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}", minutes, seconds)
    }
}

pub fn load_partition_counts(
    reader: &Arc<KiteReader>,
    partition_id: usize,
) -> anyhow::Result<Vec<(KmerKey, u64)>> {
    let mut rows: Vec<(KmerKey, u64)> = Vec::new();
    let entry_size = reader.header.entry_size();

    for loc in &reader.partition_chunks[partition_id] {
        let payload = reader.run_data_for_decode(loc)?;
        let bytes = payload.as_ref();
        if entry_size == 36 {
            for index in 0..loc.row_count as usize {
                let off = index * 36;
                let lo = u128::from_le_bytes(bytes[off..off + 16].try_into().unwrap());
                let hi = u128::from_le_bytes(bytes[off + 16..off + 32].try_into().unwrap());
                let count = u32::from_le_bytes(bytes[off + 32..off + 36].try_into().unwrap());
                rows.push((KmerKey { hi, lo }, count as u64));
            }
            continue;
        }

        if loc.is_dvle() {
            decode_dvle_rows(bytes, loc.row_count as usize, entry_size == 20, &mut rows)?;
        } else {
            decode_raw_rows(bytes, loc.row_count as usize, entry_size, &mut rows)?;
        }
    }

    rows.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut merged: Vec<(KmerKey, u64)> = Vec::with_capacity(rows.len());
    for (key, count) in rows {
        if let Some((last_key, last_count)) = merged.last_mut() {
            if *last_key == key {
                *last_count = last_count.saturating_add(count);
                continue;
            }
        }
        merged.push((key, count));
    }
    Ok(merged)
}

fn decode_raw_rows(
    bytes: &[u8],
    row_count: usize,
    entry_size: usize,
    out: &mut Vec<(KmerKey, u64)>,
) -> anyhow::Result<()> {
    for index in 0..row_count {
        let off = index * entry_size;
        if off + entry_size > bytes.len() {
            bail!("raw row payload truncated at row {}", index);
        }
        if entry_size == 12 {
            let key = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
            let count = u32::from_le_bytes(bytes[off + 8..off + 12].try_into().unwrap());
            out.push((
                KmerKey {
                    hi: 0,
                    lo: key as u128,
                },
                count as u64,
            ));
        } else {
            let key = u128::from_le_bytes(bytes[off..off + 16].try_into().unwrap());
            let count = u32::from_le_bytes(bytes[off + 16..off + 20].try_into().unwrap());
            out.push((KmerKey { hi: 0, lo: key }, count as u64));
        }
    }
    Ok(())
}

fn decode_dvle_rows(
    bytes: &[u8],
    row_count: usize,
    wide: bool,
    out: &mut Vec<(KmerKey, u64)>,
) -> anyhow::Result<()> {
    if row_count == 0 {
        return Ok(());
    }
    let mut prev_key: u128;
    if wide {
        let mut pos = 20usize;
        if bytes.len() < 20 {
            bail!("u128 DVLE payload too small");
        }
        prev_key = u128::from_le_bytes(bytes[0..16].try_into().unwrap());
        let count = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        out.push((
            KmerKey {
                hi: 0,
                lo: prev_key,
            },
            count as u64,
        ));
        for _ in 1..row_count {
            let (delta, used_delta) = read_varint_u128(&bytes[pos..]);
            if used_delta == 0 {
                bail!("invalid u128 DVLE delta");
            }
            pos += used_delta;
            let (count, used_count) = read_varint_u64(&bytes[pos..]);
            if used_count == 0 {
                bail!("invalid u128 DVLE count");
            }
            pos += used_count;
            prev_key = prev_key.wrapping_add(delta);
            out.push((
                KmerKey {
                    hi: 0,
                    lo: prev_key,
                },
                count,
            ));
        }
    } else {
        let mut pos = 12usize;
        if bytes.len() < 12 {
            bail!("u64 DVLE payload too small");
        }
        prev_key = u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as u128;
        let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        out.push((
            KmerKey {
                hi: 0,
                lo: prev_key,
            },
            count as u64,
        ));
        for _ in 1..row_count {
            let (delta, used_delta) = read_varint_u64(&bytes[pos..]);
            if used_delta == 0 {
                bail!("invalid u64 DVLE delta");
            }
            pos += used_delta;
            let (count, used_count) = read_varint_u64(&bytes[pos..]);
            if used_count == 0 {
                bail!("invalid u64 DVLE count");
            }
            pos += used_count;
            prev_key = prev_key.wrapping_add(delta as u128);
            out.push((
                KmerKey {
                    hi: 0,
                    lo: prev_key,
                },
                count,
            ));
        }
    }
    Ok(())
}

pub fn merge_vectors_total(vectors: &[Vec<(KmerKey, u64)>]) -> Vec<(KmerKey, u64)> {
    match vectors.len() {
        0 => return Vec::new(),
        1 => return vectors[0].clone(),
        2 => return merge_vectors_total_two(vectors),
        3 => return merge_vectors_total_three(vectors),
        _ => {}
    }
    let mut indexes = vec![0usize; vectors.len()];
    let capacity = vectors.iter().map(Vec::len).sum();
    let mut merged = Vec::with_capacity(capacity);
    loop {
        let mut min_key = None;
        for (file_index, vector) in vectors.iter().enumerate() {
            if let Some((key, _)) = vector.get(indexes[file_index]) {
                min_key = Some(match min_key {
                    Some(current) if current <= *key => current,
                    _ => *key,
                });
            }
        }
        let Some(target_key) = min_key else {
            break;
        };
        let mut total = 0u64;
        for (file_index, vector) in vectors.iter().enumerate() {
            if let Some((key, count)) = vector.get(indexes[file_index]) {
                if *key == target_key {
                    total = total.saturating_add(*count);
                    indexes[file_index] += 1;
                }
            }
        }
        merged.push((target_key, total));
    }
    merged
}

pub fn for_each_union_row<F>(vectors: &[Vec<(KmerKey, u64)>], mut f: F) -> anyhow::Result<()>
where
    F: FnMut(KmerKey, &[u64]) -> anyhow::Result<()>,
{
    match vectors.len() {
        0 => return Ok(()),
        1 => return for_each_union_row_one(vectors, f),
        2 => return for_each_union_row_two(vectors, f),
        3 => return for_each_union_row_three(vectors, f),
        _ => {}
    }
    let mut indexes = vec![0usize; vectors.len()];
    let mut row = vec![0u64; vectors.len()];
    loop {
        let mut min_key = None;
        for (file_index, vector) in vectors.iter().enumerate() {
            if let Some((key, _)) = vector.get(indexes[file_index]) {
                min_key = Some(match min_key {
                    Some(current) if current <= *key => current,
                    _ => *key,
                });
            }
        }
        let Some(target_key) = min_key else {
            break;
        };
        row.fill(0);
        for (file_index, vector) in vectors.iter().enumerate() {
            if let Some((key, count)) = vector.get(indexes[file_index]) {
                if *key == target_key {
                    row[file_index] = *count;
                    indexes[file_index] += 1;
                }
            }
        }
        f(target_key, &row)?;
    }
    Ok(())
}

fn merge_vectors_total_two(vectors: &[Vec<(KmerKey, u64)>]) -> Vec<(KmerKey, u64)> {
    let left = &vectors[0];
    let right = &vectors[1];
    let mut i = 0usize;
    let mut j = 0usize;
    let mut merged = Vec::with_capacity(left.len() + right.len());
    while i < left.len() && j < right.len() {
        match left[i].0.cmp(&right[j].0) {
            std::cmp::Ordering::Less => {
                merged.push(left[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                merged.push(right[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                merged.push((left[i].0, left[i].1.saturating_add(right[j].1)));
                i += 1;
                j += 1;
            }
        }
    }
    merged.extend_from_slice(&left[i..]);
    merged.extend_from_slice(&right[j..]);
    merged
}

fn merge_vectors_total_three(vectors: &[Vec<(KmerKey, u64)>]) -> Vec<(KmerKey, u64)> {
    let a = &vectors[0];
    let b = &vectors[1];
    let c = &vectors[2];
    let mut ia = 0usize;
    let mut ib = 0usize;
    let mut ic = 0usize;
    let mut merged = Vec::with_capacity(a.len() + b.len() + c.len());
    loop {
        let ka = a.get(ia).map(|entry| entry.0);
        let kb = b.get(ib).map(|entry| entry.0);
        let kc = c.get(ic).map(|entry| entry.0);
        let Some(target) = [ka, kb, kc].into_iter().flatten().min() else {
            break;
        };
        let mut total = 0u64;
        if ka == Some(target) {
            total = total.saturating_add(a[ia].1);
            ia += 1;
        }
        if kb == Some(target) {
            total = total.saturating_add(b[ib].1);
            ib += 1;
        }
        if kc == Some(target) {
            total = total.saturating_add(c[ic].1);
            ic += 1;
        }
        merged.push((target, total));
    }
    merged
}

fn for_each_union_row_one<F>(vectors: &[Vec<(KmerKey, u64)>], mut f: F) -> anyhow::Result<()>
where
    F: FnMut(KmerKey, &[u64]) -> anyhow::Result<()>,
{
    let mut row = [0u64; 1];
    for (key, count) in &vectors[0] {
        row[0] = *count;
        f(*key, &row)?;
    }
    Ok(())
}

fn for_each_union_row_two<F>(vectors: &[Vec<(KmerKey, u64)>], mut f: F) -> anyhow::Result<()>
where
    F: FnMut(KmerKey, &[u64]) -> anyhow::Result<()>,
{
    let left = &vectors[0];
    let right = &vectors[1];
    let mut i = 0usize;
    let mut j = 0usize;
    let mut row = [0u64; 2];
    while i < left.len() || j < right.len() {
        let target = match (left.get(i), right.get(j)) {
            (Some((key, _)), Some((other, _))) => (*key).min(*other),
            (Some((key, _)), None) => *key,
            (None, Some((key, _))) => *key,
            (None, None) => break,
        };
        row.fill(0);
        if left.get(i).map(|entry| entry.0) == Some(target) {
            row[0] = left[i].1;
            i += 1;
        }
        if right.get(j).map(|entry| entry.0) == Some(target) {
            row[1] = right[j].1;
            j += 1;
        }
        f(target, &row)?;
    }
    Ok(())
}

fn for_each_union_row_three<F>(vectors: &[Vec<(KmerKey, u64)>], mut f: F) -> anyhow::Result<()>
where
    F: FnMut(KmerKey, &[u64]) -> anyhow::Result<()>,
{
    let a = &vectors[0];
    let b = &vectors[1];
    let c = &vectors[2];
    let mut ia = 0usize;
    let mut ib = 0usize;
    let mut ic = 0usize;
    let mut row = [0u64; 3];
    loop {
        let ka = a.get(ia).map(|entry| entry.0);
        let kb = b.get(ib).map(|entry| entry.0);
        let kc = c.get(ic).map(|entry| entry.0);
        let Some(target) = [ka, kb, kc].into_iter().flatten().min() else {
            break;
        };
        row.fill(0);
        if ka == Some(target) {
            row[0] = a[ia].1;
            ia += 1;
        }
        if kb == Some(target) {
            row[1] = b[ib].1;
            ib += 1;
        }
        if kc == Some(target) {
            row[2] = c[ic].1;
            ic += 1;
        }
        f(target, &row)?;
    }
    Ok(())
}

pub fn serialize_partition_chunk(
    rows: &[(KmerKey, u64)],
    key_width: u16,
) -> anyhow::Result<Option<SerializedPartitionChunk>> {
    if rows.is_empty() {
        return Ok(None);
    }

    let mut data = Vec::new();
    let codec = match key_width {
        KEY_WIDTH_U64 => {
            let mut entries: Vec<(u64, u32)> = Vec::with_capacity(rows.len());
            for (key, count) in rows {
                let count = u32::try_from(*count)
                    .map_err(|_| anyhow!("merged count exceeds u32 for key {}", key.lo))?;
                entries.push((key.lo as u64, count));
            }
            encode_dvle_u64(&entries, &mut data);
            CODEC_DVLE
        }
        KEY_WIDTH_U128 => {
            let mut entries: Vec<(u128, u32)> = Vec::with_capacity(rows.len());
            for (key, count) in rows {
                let count = u32::try_from(*count)
                    .map_err(|_| anyhow!("merged count exceeds u32 for key {}", key.lo))?;
                entries.push((key.lo, count));
            }
            encode_dvle_u128(&entries, &mut data);
            CODEC_DVLE
        }
        KEY_WIDTH_U256 => {
            for (key, count) in rows {
                let count =
                    u32::try_from(*count).map_err(|_| anyhow!("merged count exceeds u32"))?;
                data.extend_from_slice(&key.lo.to_le_bytes());
                data.extend_from_slice(&key.hi.to_le_bytes());
                data.extend_from_slice(&count.to_le_bytes());
            }
            CODEC_RAW
        }
        other => bail!("unsupported key_width={other}"),
    };

    let first = rows.first().unwrap().0;
    let last = rows.last().unwrap().0;
    Ok(Some(SerializedPartitionChunk {
        codec,
        row_count: rows.len() as u64,
        raw_bytes: data.len() as u64,
        data,
        key_min_lo: first.lo,
        key_min_hi: first.hi,
        key_max_lo: last.lo,
        key_max_hi: last.hi,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{CODEC_DVLE, CODEC_RAW};

    fn key(lo: u128) -> KmerKey {
        KmerKey { hi: 0, lo }
    }

    #[test]
    fn count_filter_handles_open_and_closed_bounds() {
        assert!(count_passes_filter(5, None, None));
        assert!(!count_passes_filter(1, Some(2), None));
        assert!(count_passes_filter(2, Some(2), None));
        assert!(count_passes_filter(10, None, Some(10)));
        assert!(!count_passes_filter(11, None, Some(10)));
        assert!(count_passes_filter(5, Some(2), Some(10)));
        assert!(!count_passes_filter(1, Some(2), Some(10)));
        assert!(!count_passes_filter(11, Some(2), Some(10)));
    }

    #[test]
    fn merge_vectors_total_sums_sorted_union() {
        let vectors = vec![
            vec![(key(1), 2), (key(3), 4)],
            vec![(key(2), 5), (key(3), 6)],
            vec![(key(1), 7), (key(4), 8)],
        ];
        let merged = merge_vectors_total(&vectors);
        assert_eq!(
            merged,
            vec![(key(1), 9), (key(2), 5), (key(3), 10), (key(4), 8)]
        );
    }

    #[test]
    fn infer_sample_labels_and_groups_defaults_to_labels() {
        let paths = vec!["sample_a.kite".to_string(), "sample_b.kite".to_string()];
        let (labels, groups) = infer_sample_labels_and_groups(&paths, &[], &[]).unwrap();
        assert_eq!(labels, vec!["sample_a", "sample_b"]);
        assert_eq!(groups, vec!["sample_a", "sample_b"]);
    }

    #[test]
    fn infer_sample_labels_and_groups_uses_labels_when_groups_omitted() {
        let paths = vec!["x.fastq.gz".to_string(), "y.fastq.gz".to_string()];
        let labels = vec!["tumor".to_string(), "normal".to_string()];
        let (resolved_labels, groups) =
            infer_sample_labels_and_groups(&paths, &labels, &[]).unwrap();
        assert_eq!(resolved_labels, labels);
        assert_eq!(groups, vec!["tumor", "normal"]);
    }

    #[test]
    fn infer_sample_labels_and_groups_uses_groups_when_provided() {
        let paths = vec!["x.fastq.gz".to_string(), "y.fastq.gz".to_string()];
        let labels = vec!["tumor".to_string(), "normal".to_string()];
        let groups = vec!["case".to_string(), "control".to_string()];
        let (resolved_labels, resolved_groups) =
            infer_sample_labels_and_groups(&paths, &labels, &groups).unwrap();
        assert_eq!(resolved_labels, labels);
        assert_eq!(resolved_groups, groups);
    }

    #[test]
    fn infer_sample_labels_and_groups_rejects_mismatched_groups() {
        let paths = vec!["x.fastq.gz".to_string(), "y.fastq.gz".to_string()];
        let groups = vec!["case".to_string()];
        assert!(infer_sample_labels_and_groups(&paths, &[], &groups).is_err());
    }

    #[test]
    fn for_each_union_row_emits_zero_filled_rows() {
        let vectors = vec![vec![(key(1), 2), (key(3), 4)], vec![(key(2), 5)]];
        let mut rows = Vec::new();
        for_each_union_row(&vectors, |key, row| {
            rows.push((key.lo, row.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            rows,
            vec![(1, vec![2, 0]), (2, vec![0, 5]), (3, vec![4, 0])]
        );
    }

    #[test]
    fn serialize_partition_chunk_uses_expected_codec_and_bounds() {
        let rows = vec![(key(2), 3), (key(5), 7)];
        let chunk = serialize_partition_chunk(&rows, KEY_WIDTH_U64)
            .unwrap()
            .unwrap();
        assert_eq!(chunk.codec, CODEC_DVLE);
        assert_eq!(chunk.row_count, 2);
        assert_eq!(chunk.key_min_lo, 2);
        assert_eq!(chunk.key_max_lo, 5);

        let wide_rows = vec![
            (
                KmerKey {
                    hi: 1,
                    lo: u128::MAX - 1,
                },
                9,
            ),
            (KmerKey { hi: 2, lo: 1 }, 10),
        ];
        let wide = serialize_partition_chunk(&wide_rows, KEY_WIDTH_U256)
            .unwrap()
            .unwrap();
        assert_eq!(wide.codec, CODEC_RAW);
        assert_eq!(wide.raw_bytes, 72);
        assert_eq!(wide.key_min_hi, 1);
        assert_eq!(wide.key_max_hi, 2);
    }
}
