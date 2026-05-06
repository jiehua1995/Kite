use std::sync::Arc;
use std::time::Instant;

use crate::dump::decoder::{decode_kmer_any, decode_kmer_u256, format_tsv_line};
use crate::dump::loser_tree::LoserTreeMerger;
use crate::dump::reader::{ChunkLocation, KiteReader};
use crate::stats::DumpStats;

struct MmapRunIter {
    _reader: Arc<KiteReader>,
    base: *const u8,
    entry_count: usize,
    entry_size: usize,
    current: usize,
}

unsafe impl Send for MmapRunIter {}

impl MmapRunIter {
    fn new(reader: Arc<KiteReader>, loc: &ChunkLocation, entry_size: usize) -> Self {
        let (base, entry_count) = {
            let slice = reader.run_data_slice(loc);
            (slice.as_ptr(), loc.row_count as usize)
        };
        Self { _reader: reader, base, entry_count, entry_size, current: 0 }
    }
}

impl Iterator for MmapRunIter {
    type Item = (u128, u32);

    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= self.entry_count {
            return None;
        }
        unsafe {
            let ptr = self.base.add(self.current * self.entry_size);
            self.current += 1;
            if self.entry_size == 12 {
                let key = std::ptr::read_unaligned(ptr as *const u64) as u128;
                let count = std::ptr::read_unaligned(ptr.add(8) as *const u32);
                Some((key, count))
            } else {
                let key = std::ptr::read_unaligned(ptr as *const u128);
                let count = std::ptr::read_unaligned(ptr.add(16) as *const u32);
                Some((key, count))
            }
        }
    }
}

struct DvleRunIter {
    _reader: Arc<KiteReader>,
    data: *const u8,
    len: usize,
    pos: usize,
    remaining: usize,
    prev_key: u128,
    wide: bool,
}

unsafe impl Send for DvleRunIter {}

impl DvleRunIter {
    fn new(reader: Arc<KiteReader>, loc: &ChunkLocation, wide: bool) -> Self {
        let (data, len, remaining) = {
            let slice = reader.run_data_slice(loc);
            (slice.as_ptr(), slice.len(), loc.row_count as usize)
        };
        Self { _reader: reader, data, len, pos: 0, remaining, prev_key: 0, wide }
    }
}

impl Iterator for DvleRunIter {
    type Item = (u128, u32);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let data = unsafe { std::slice::from_raw_parts(self.data, self.len) };
        if self.pos == 0 {
            if self.wide {
                let key = u128::from_le_bytes(data[0..16].try_into().unwrap());
                let count = u32::from_le_bytes(data[16..20].try_into().unwrap());
                self.prev_key = key;
                self.pos = 20;
                return Some((key, count));
            }
            let key = u64::from_le_bytes(data[0..8].try_into().unwrap()) as u128;
            let count = u32::from_le_bytes(data[8..12].try_into().unwrap());
            self.prev_key = key;
            self.pos = 12;
            return Some((key, count));
        }

        if self.wide {
            let (delta, n_delta) = crate::format::run_block::read_varint_u128(&data[self.pos..]);
            self.pos += n_delta;
            let (count, n_count) = crate::format::run_block::read_varint_u64(&data[self.pos..]);
            self.pos += n_count;
            let key = self.prev_key.wrapping_add(delta);
            self.prev_key = key;
            Some((key, count as u32))
        } else {
            let (delta, n_delta) = crate::format::run_block::read_varint_u64(&data[self.pos..]);
            self.pos += n_delta;
            let (count, n_count) = crate::format::run_block::read_varint_u64(&data[self.pos..]);
            self.pos += n_count;
            let key = self.prev_key.wrapping_add(delta as u128);
            self.prev_key = key;
            Some((key, count as u32))
        }
    }
}

fn build_partition_merger(
    partition_chunks: &[ChunkLocation],
    reader: &Arc<KiteReader>,
    entry_size: usize,
) -> std::io::Result<LoserTreeMerger> {
    let mut streams: Vec<Box<dyn Iterator<Item = (u128, u32)> + Send>> =
        Vec::with_capacity(partition_chunks.len());

    for loc in partition_chunks {
        if loc.is_dvle() {
            streams.push(Box::new(DvleRunIter::new(Arc::clone(reader), loc, entry_size == 20)));
        } else {
            streams.push(Box::new(MmapRunIter::new(Arc::clone(reader), loc, entry_size)));
        }
    }
    Ok(LoserTreeMerger::new(streams))
}

pub fn stream_partition_slabs<F>(
    partition_id: usize,
    reader: &Arc<KiteReader>,
    k: u8,
    entry_size: usize,
    slab_bytes: usize,
    collect_timing: bool,
    stats: &Arc<DumpStats>,
    mut emit: F,
) -> std::io::Result<()>
where
    F: FnMut(Vec<u8>, u64) -> std::io::Result<()>,
{
    let partition_chunks = &reader.partition_chunks[partition_id];
    if partition_chunks.is_empty() {
        if collect_timing {
            stats.observe_worker_ns(0);
        }
        return Ok(());
    }

    let worker_start = collect_timing.then(Instant::now);
    let mut kmer_buf = vec![0u8; k as usize];

    if entry_size == 36 {
        let t_decode = collect_timing.then(Instant::now);
        let total_entries: usize = partition_chunks.iter().map(|c| c.row_count as usize).sum();
        let mut all: Vec<(u128, u128, u32)> = Vec::with_capacity(total_entries);
        for loc in partition_chunks {
            let slice = reader.run_data_slice(loc);
            for i in 0..loc.row_count as usize {
                let off = i * 36;
                let lo = u128::from_le_bytes(slice[off..off + 16].try_into().unwrap());
                let hi = u128::from_le_bytes(slice[off + 16..off + 32].try_into().unwrap());
                let cnt = u32::from_le_bytes(slice[off + 32..off + 36].try_into().unwrap());
                all.push((hi, lo, cnt));
            }
        }
        if let Some(t_decode) = t_decode {
            stats.add_decode_ns(t_decode.elapsed().as_nanos() as u64);
        }

        let t_merge = collect_timing.then(Instant::now);
        all.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        if let Some(t_merge) = t_merge {
            stats.add_merge_ns(t_merge.elapsed().as_nanos() as u64);
        }

        let t_format = collect_timing.then(Instant::now);
        let mut slab = Vec::with_capacity(slab_bytes);
        let mut slab_rows = 0u64;
        let mut i = 0usize;
        while i < all.len() {
            let (hi, lo, mut acc) = all[i];
            let mut j = i + 1;
            while j < all.len() && all[j].0 == hi && all[j].1 == lo {
                acc = acc.saturating_add(all[j].2);
                j += 1;
            }
            decode_kmer_u256(lo, hi, k, &mut kmer_buf);
            format_tsv_line(&kmer_buf, acc, &mut slab);
            slab_rows += 1;
            if slab.len() >= slab_bytes {
                emit(std::mem::replace(&mut slab, Vec::with_capacity(slab_bytes)), slab_rows)?;
                slab_rows = 0;
            }
            i = j;
        }
        if !slab.is_empty() {
            emit(slab, slab_rows)?;
        }
        if let Some(t_format) = t_format {
            stats.add_format_ns(t_format.elapsed().as_nanos() as u64);
        }
    } else {
        let t_decode = collect_timing.then(Instant::now);
        let mut merger = build_partition_merger(partition_chunks, reader, entry_size)?;
        if let Some(t_decode) = t_decode {
            stats.add_decode_ns(t_decode.elapsed().as_nanos() as u64);
        }

        let mut merge_ns = 0u64;
        let mut format_ns = 0u64;
        let mut slab = Vec::with_capacity(slab_bytes);
        let mut slab_rows = 0u64;
        loop {
            let t_merge = collect_timing.then(Instant::now);
            let next = merger.next_min();
            if let Some(t_merge) = t_merge {
                merge_ns += t_merge.elapsed().as_nanos() as u64;
            }
            let Some((key, count)) = next else { break; };

            let t_format = collect_timing.then(Instant::now);
            decode_kmer_any(key, k, &mut kmer_buf);
            format_tsv_line(&kmer_buf, count, &mut slab);
            slab_rows += 1;
            if let Some(t_format) = t_format {
                format_ns += t_format.elapsed().as_nanos() as u64;
            }

            if slab.len() >= slab_bytes {
                emit(std::mem::replace(&mut slab, Vec::with_capacity(slab_bytes)), slab_rows)?;
                slab_rows = 0;
            }
        }
        if !slab.is_empty() {
            emit(slab, slab_rows)?;
        }
        if collect_timing {
            stats.add_merge_ns(merge_ns);
            stats.add_format_ns(format_ns);
        }
    }

    if let Some(worker_start) = worker_start {
        stats.observe_worker_ns(worker_start.elapsed().as_nanos() as u64);
    }
    Ok(())
}
