use std::io;
use std::fs::File;
use std::borrow::Cow;
use memmap2::Mmap;

use crate::format::{KiteHeader, ChunkIndexEntry, PartitionEntry, Kx2Footer};
use crate::format::{CODEC_DVLE, CODEC_RAW, CODEC_ZSTD};

/// Per-chunk location in the .kite data area.
#[derive(Debug, Clone)]
pub struct ChunkLocation {
    pub file_offset: u64,
    pub payload_len: usize,
    pub raw_bytes: u64,
    pub codec: u32,
    pub row_count: u64,
    #[allow(dead_code)]
    pub key_min_lo: u128,
    #[allow(dead_code)]
    pub key_min_hi: u128,
    #[allow(dead_code)]
    pub key_max_lo: u128,
    #[allow(dead_code)]
    pub key_max_hi: u128,
}

impl ChunkLocation {
    #[inline]
    pub fn is_zstd(&self) -> bool {
        self.codec & CODEC_ZSTD != 0
    }

    #[allow(dead_code)]
    #[inline]
    pub fn is_dvle(&self) -> bool {
        self.codec & CODEC_DVLE != 0
    }
}

/// Reads and indexes a .kite file.
pub struct KiteReader {
    pub header: KiteHeader,
    pub mmap: Mmap,
    pub partition_entries: Vec<PartitionEntry>,
    pub chunk_entries: Vec<ChunkIndexEntry>,
    /// Per-partition chunk locations, indexed by partition_id.
    pub partition_chunks: Vec<Vec<ChunkLocation>>,
    pub total_chunks: u64,
    #[allow(dead_code)]
    file_size: u64,
}

impl KiteReader {
    pub fn open(path: &str) -> io::Result<Self> {
        let file = File::open(path)?;
        let file_size = file.metadata()?.len();
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < KiteHeader::SIZE + Kx2Footer::SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file too small"));
        }

        // Read header
        let mut cursor = io::Cursor::new(&mmap[..]);
        let header = KiteHeader::read_from(&mut cursor)?;

        let partitions = header.partitions as usize;

        let footer_offset = header.footer_offset as usize;
        if footer_offset + Kx2Footer::SIZE > mmap.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "footer out of bounds"));
        }
        let mut footer_cursor = io::Cursor::new(&mmap[footer_offset..]);
        let footer = Kx2Footer::read_from(&mut footer_cursor)?;
        let total_chunks = footer.total_chunks;

        let partition_table_offset = header.partition_table_offset as usize;
        let partition_entry_size = PartitionEntry::SIZE;
        let partition_table_end = partition_table_offset + partitions * partition_entry_size;
        if partition_table_end > mmap.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "partition table out of bounds"));
        }
        let mut p_cursor = io::Cursor::new(&mmap[partition_table_offset..]);
        let mut partition_entries = Vec::with_capacity(partitions);
        for _ in 0..partitions {
            partition_entries.push(PartitionEntry::read_from(&mut p_cursor)?);
        }

        let chunk_index_offset = header.chunk_index_offset as usize;
        let chunk_index_end = header.partition_table_offset as usize;
        if chunk_index_end < chunk_index_offset {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid chunk index offsets"));
        }
        let chunk_bytes = chunk_index_end - chunk_index_offset;
        if chunk_bytes % ChunkIndexEntry::SIZE != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk index size misaligned"));
        }
        let chunk_count = chunk_bytes / ChunkIndexEntry::SIZE;
        let mut c_cursor = io::Cursor::new(&mmap[chunk_index_offset..chunk_index_end]);
        let mut chunk_entries = Vec::with_capacity(chunk_count);
        for _ in 0..chunk_count {
            chunk_entries.push(ChunkIndexEntry::read_from(&mut c_cursor)?);
        }

        let mut partition_chunks: Vec<Vec<ChunkLocation>> = vec![vec![]; partitions];
        for (b, p) in partition_entries.iter().enumerate() {
            let start = p.first_chunk_idx as usize;
            let end = start + p.chunk_count as usize;
            if end > chunk_entries.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData,
                    format!("partition {} chunk range out of bounds", b)));
            }

            let mut chunks = Vec::with_capacity(p.chunk_count as usize);
            for e in &chunk_entries[start..end] {
                let file_offset = e.file_offset as usize;
                let encoded_bytes = e.encoded_bytes as usize;
                if encoded_bytes == 0 || file_offset + encoded_bytes > mmap.len() {
                    return Err(io::Error::new(io::ErrorKind::InvalidData,
                        format!("partition {} chunk out of bounds", b)));
                }
                if e.codec & !(CODEC_DVLE | CODEC_RAW | CODEC_ZSTD) != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("partition {} chunk uses unsupported codec mask {:#x}", b, e.codec),
                    ));
                }
                chunks.push(ChunkLocation {
                    file_offset: e.file_offset,
                    payload_len: encoded_bytes,
                    raw_bytes: e.raw_bytes,
                    codec: e.codec,
                    row_count: e.row_count,
                    key_min_lo: e.key_min_lo,
                    key_min_hi: e.key_min_hi,
                    key_max_lo: e.key_max_lo,
                    key_max_hi: e.key_max_hi,
                });
            }
            partition_chunks[b] = chunks;
        }

        Ok(KiteReader {
            header,
            mmap,
            partition_entries,
            chunk_entries,
            partition_chunks,
            total_chunks,
            file_size,
        })
    }

    pub fn run_data_slice(&self, loc: &ChunkLocation) -> &[u8] {
        let start = loc.file_offset as usize;
        let len = loc.payload_len;
        &self.mmap[start..start + len]
    }

    pub fn run_data_for_decode<'a>(&'a self, loc: &ChunkLocation) -> io::Result<Cow<'a, [u8]>> {
        let payload = self.run_data_slice(loc);
        if !loc.is_zstd() {
            return Ok(Cow::Borrowed(payload));
        }

        let decoded = zstd::bulk::decompress(payload, loc.raw_bytes as usize).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("zstd decompress failed: {e}"))
        })?;
        if decoded.len() != loc.raw_bytes as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "decoded payload size mismatch: got {}, expected {}",
                    decoded.len(),
                    loc.raw_bytes,
                ),
            ));
        }
        Ok(Cow::Owned(decoded))
    }
}
