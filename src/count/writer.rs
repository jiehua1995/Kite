use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::format::{ChunkIndexEntry, KiteHeader, Kx2Footer, PartitionEntry};

#[derive(Debug, Clone)]
pub struct ChunkMeta {
    pub file_offset: u64,
    pub row_count: u64,
    pub codec: u32,
    pub encoded_bytes: u64,
    pub raw_bytes: u64,
    pub key_min_lo: u128,
    pub key_min_hi: u128,
    pub key_max_lo: u128,
    pub key_max_hi: u128,
}

pub struct ConcurrentKiteWriter {
    file: File,
    k: u8,
    canonical: bool,
    partitions: usize,
    key_width: u16,
    codec_mask: u16,
    next_data_offset: AtomicU64,
    partition_chunks: Vec<Mutex<Vec<ChunkMeta>>>,
}

impl ConcurrentKiteWriter {
    pub fn create(
        output_path: &str,
        k: u8,
        canonical: bool,
        partitions: usize,
        key_width: u16,
        codec_mask: u16,
    ) -> io::Result<Self> {
        let mut file = File::create(output_path)?;
        let header = KiteHeader::new(k, canonical, partitions as u32, key_width, codec_mask);
        let mut header_buf = vec![0u8; KiteHeader::SIZE];
        {
            let mut cursor = std::io::Cursor::new(&mut header_buf[..]);
            header.write_to(&mut cursor)?;
        }
        file.write_all(&header_buf)?;

        Ok(Self {
            file,
            k,
            canonical,
            partitions,
            key_width,
            codec_mask,
            next_data_offset: AtomicU64::new(KiteHeader::SIZE as u64),
            partition_chunks: (0..partitions).map(|_| Mutex::new(Vec::new())).collect(),
        })
    }

    pub fn append_chunk(
        &self,
        partition_id: usize,
        codec: u32,
        row_count: u64,
        raw_bytes: u64,
        key_min_lo: u128,
        key_min_hi: u128,
        key_max_lo: u128,
        key_max_hi: u128,
        data: &[u8],
    ) -> io::Result<()> {
        let offset = self.next_data_offset.fetch_add(data.len() as u64, Ordering::Relaxed);
        let mut written = 0usize;
        while written < data.len() {
            let n = self.file.write_at(&data[written..], offset + written as u64)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "write_at returned 0"));
            }
            written += n;
        }

        let mut partition = self.partition_chunks[partition_id].lock().unwrap();
        partition.push(ChunkMeta {
            file_offset: offset,
            row_count,
            codec,
            encoded_bytes: data.len() as u64,
            raw_bytes,
            key_min_lo,
            key_min_hi,
            key_max_lo,
            key_max_hi,
        });
        Ok(())
    }

    pub fn finalize(&mut self) -> io::Result<FinalizeSummary> {
        let data_end = self.next_data_offset.load(Ordering::Relaxed);

        let mut all_chunks: Vec<Vec<ChunkMeta>> = Vec::with_capacity(self.partitions);
        for partition in &self.partition_chunks {
            let mut chunks = partition.lock().unwrap().clone();
            chunks.sort_unstable_by_key(|chunk| chunk.file_offset);
            all_chunks.push(chunks);
        }

        let total_chunks: u64 = all_chunks.iter().map(|chunks| chunks.len() as u64).sum();
        let total_rows: u64 = all_chunks
            .iter()
            .map(|chunks| chunks.iter().map(|chunk| chunk.row_count).sum::<u64>())
            .sum();

        let chunk_index_offset = data_end;
        let partition_table_offset = chunk_index_offset + total_chunks * ChunkIndexEntry::SIZE as u64;
        let footer_offset = partition_table_offset + self.partitions as u64 * PartitionEntry::SIZE as u64;
        let total_size = footer_offset + Kx2Footer::SIZE as u64;

        let mut trailer = Vec::with_capacity((total_size - data_end) as usize + 4096);
        let mut partition_entries = Vec::with_capacity(self.partitions);
        let mut hottest_partition_segments = 0u64;
        let mut least_partition_segments = u64::MAX;
        let mut first_chunk_idx = 0u64;

        for chunks in &all_chunks {
            let chunk_count = chunks.len() as u64;
            let total_rows_partition = chunks.iter().map(|chunk| chunk.row_count).sum::<u64>();
            let data_offset = chunks.iter().map(|chunk| chunk.file_offset).min().unwrap_or(0);
            let data_bytes = chunks.iter().map(|chunk| chunk.encoded_bytes).sum::<u64>();
            hottest_partition_segments = hottest_partition_segments.max(chunk_count);
            least_partition_segments = least_partition_segments.min(chunk_count);

            partition_entries.push(PartitionEntry {
                first_chunk_idx,
                chunk_count,
                total_rows: total_rows_partition,
                data_offset,
                data_bytes,
            });
            first_chunk_idx += chunk_count;
        }

        for (partition_id, chunks) in all_chunks.iter().enumerate() {
            for chunk in chunks {
                let mut buf = vec![0u8; ChunkIndexEntry::SIZE];
                let mut cursor = std::io::Cursor::new(&mut buf[..]);
                ChunkIndexEntry {
                    partition_id: partition_id as u32,
                    codec: chunk.codec,
                    file_offset: chunk.file_offset,
                    encoded_bytes: chunk.encoded_bytes,
                    raw_bytes: chunk.raw_bytes,
                    row_count: chunk.row_count,
                    key_min_lo: chunk.key_min_lo,
                    key_min_hi: chunk.key_min_hi,
                    key_max_lo: chunk.key_max_lo,
                    key_max_hi: chunk.key_max_hi,
                }
                .write_to(&mut cursor)?;
                trailer.extend_from_slice(&buf);
            }
        }

        for partition in &partition_entries {
            let mut buf = vec![0u8; PartitionEntry::SIZE];
            let mut cursor = std::io::Cursor::new(&mut buf[..]);
            partition.write_to(&mut cursor)?;
            trailer.extend_from_slice(&buf);
        }

        let mut footer_buf = vec![0u8; Kx2Footer::SIZE];
        {
            let mut cursor = std::io::Cursor::new(&mut footer_buf[..]);
            Kx2Footer::new(total_chunks, total_rows).write_to(&mut cursor)?;
        }
        trailer.extend_from_slice(&footer_buf);

        self.file.seek(SeekFrom::Start(data_end))?;
        self.file.write_all(&trailer)?;

        let mut header_buf = vec![0u8; KiteHeader::SIZE];
        {
            let mut cursor = std::io::Cursor::new(&mut header_buf[..]);
            KiteHeader {
                magic: crate::format::header::KITE_MAGIC,
                version: crate::format::header::KITE_VERSION,
                k: self.k,
                canonical: self.canonical as u8,
                partitions: self.partitions as u32,
                key_width: self.key_width,
                codec_mask: self.codec_mask,
                checksum_mode: 0,
                chunk_index_offset,
                partition_table_offset,
                footer_offset,
                total_chunks,
                total_rows,
            }
            .write_to(&mut cursor)?;
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header_buf)?;
        self.file.set_len(total_size)?;
        self.file.sync_all()?;

        Ok(FinalizeSummary {
            total_chunks,
            total_rows,
            data_end,
            hottest_partition_segments,
            least_partition_segments: if self.partitions == 0 { 0 } else { least_partition_segments },
        })
    }
}

pub struct FinalizeSummary {
    pub total_chunks: u64,
    pub total_rows: u64,
    pub data_end: u64,
    pub hottest_partition_segments: u64,
    pub least_partition_segments: u64,
}
