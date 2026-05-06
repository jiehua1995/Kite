use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context};

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
            "--label count ({}) must match input count ({})",
            labels.len(),
            paths.len()
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

pub fn open_readers(paths: &[String]) -> anyhow::Result<Vec<Arc<KiteReader>>> {
    let mut readers = Vec::with_capacity(paths.len());
    for path in paths {
        readers.push(Arc::new(
            KiteReader::open(path).with_context(|| format!("failed to open {}", path))?,
        ));
    }
    Ok(readers)
}

pub fn ensure_compatible(readers: &[Arc<KiteReader>]) -> anyhow::Result<CompatibleMetadata> {
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
        if reader.header.k != meta.k {
            bail!(
                "input #{} has k={}, expected {}",
                index + 1,
                reader.header.k,
                meta.k
            );
        }
        if (reader.header.canonical != 0) != meta.canonical {
            bail!(
                "input #{} has canonical={}, expected {}",
                index + 1,
                reader.header.canonical != 0,
                meta.canonical
            );
        }
        if reader.header.partitions as usize != meta.partitions {
            bail!(
                "input #{} has partitions={}, expected {}",
                index + 1,
                reader.header.partitions,
                meta.partitions
            );
        }
        if reader.header.key_width != meta.key_width {
            bail!(
                "input #{} has key_width={}, expected {}",
                index + 1,
                reader.header.key_width,
                meta.key_width
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
    if meta.key_width == KEY_WIDTH_U256 {
        decode_kmer_u256(key.lo, key.hi, meta.k, &mut buf);
    } else {
        decode_kmer_any(key.lo, meta.k, &mut buf);
    }
    String::from_utf8(buf).unwrap_or_else(|_| "N".repeat(meta.k as usize))
}

pub fn print_partition_progress(
    tag: &str,
    enabled: bool,
    done: usize,
    total: usize,
    started: Instant,
) {
    if !enabled {
        return;
    }
    let pct = if total > 0 {
        done as f64 * 100.0 / total as f64
    } else {
        100.0
    };
    eprintln!(
        "[kite {}] partitions {}/{} ({:.1}%) elapsed {:.1}s",
        tag,
        done,
        total,
        pct,
        started.elapsed().as_secs_f64()
    );
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
    let mut indexes = vec![0usize; vectors.len()];
    let mut merged = Vec::new();
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
