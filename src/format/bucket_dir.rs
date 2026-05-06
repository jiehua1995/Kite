use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Read, Write};

/// KX2 per-partition table entry. 40 bytes.
#[derive(Debug, Clone, Default)]
pub struct PartitionEntry {
    pub first_chunk_idx: u64,
    pub chunk_count: u64,
    pub total_rows: u64,
    pub data_offset: u64,
    pub data_bytes: u64,
}

impl PartitionEntry {
    pub const SIZE: usize = 40;

    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_u64::<LittleEndian>(self.first_chunk_idx)?;
        w.write_u64::<LittleEndian>(self.chunk_count)?;
        w.write_u64::<LittleEndian>(self.total_rows)?;
        w.write_u64::<LittleEndian>(self.data_offset)?;
        w.write_u64::<LittleEndian>(self.data_bytes)?;
        Ok(())
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let first_chunk_idx = r.read_u64::<LittleEndian>()?;
        let chunk_count = r.read_u64::<LittleEndian>()?;
        let total_rows = r.read_u64::<LittleEndian>()?;
        let data_offset = r.read_u64::<LittleEndian>()?;
        let data_bytes = r.read_u64::<LittleEndian>()?;
        Ok(PartitionEntry {
            first_chunk_idx,
            chunk_count,
            total_rows,
            data_offset,
            data_bytes,
        })
    }
}

/// KX2 per-chunk index entry. 128 bytes.
#[derive(Debug, Clone, Default)]
pub struct ChunkIndexEntry {
    pub partition_id: u32,
    pub codec: u32,
    pub file_offset: u64,
    pub encoded_bytes: u64,
    pub raw_bytes: u64,
    pub row_count: u64,
    pub key_min_lo: u128,
    pub key_min_hi: u128,
    pub key_max_lo: u128,
    pub key_max_hi: u128,
}

impl ChunkIndexEntry {
    pub const SIZE: usize = 128;

    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_u32::<LittleEndian>(self.partition_id)?;
        w.write_u32::<LittleEndian>(self.codec)?;
        w.write_u64::<LittleEndian>(self.file_offset)?;
        w.write_u64::<LittleEndian>(self.encoded_bytes)?;
        w.write_u64::<LittleEndian>(self.raw_bytes)?;
        w.write_u64::<LittleEndian>(self.row_count)?;
        w.write_u128::<LittleEndian>(self.key_min_lo)?;
        w.write_u128::<LittleEndian>(self.key_min_hi)?;
        w.write_u128::<LittleEndian>(self.key_max_lo)?;
        w.write_u128::<LittleEndian>(self.key_max_hi)?;
        w.write_all(&[0u8; 24])?;
        Ok(())
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let partition_id = r.read_u32::<LittleEndian>()?;
        let codec = r.read_u32::<LittleEndian>()?;
        let file_offset = r.read_u64::<LittleEndian>()?;
        let encoded_bytes = r.read_u64::<LittleEndian>()?;
        let raw_bytes = r.read_u64::<LittleEndian>()?;
        let row_count = r.read_u64::<LittleEndian>()?;
        let key_min_lo = r.read_u128::<LittleEndian>()?;
        let key_min_hi = r.read_u128::<LittleEndian>()?;
        let key_max_lo = r.read_u128::<LittleEndian>()?;
        let key_max_hi = r.read_u128::<LittleEndian>()?;
        let mut _reserved = [0u8; 24];
        r.read_exact(&mut _reserved)?;
        Ok(ChunkIndexEntry {
            partition_id,
            codec,
            file_offset,
            encoded_bytes,
            raw_bytes,
            row_count,
            key_min_lo,
            key_min_hi,
            key_max_lo,
            key_max_hi,
        })
    }
}

/// Footer written at the very end of the KX2 file. 24 bytes.
#[derive(Debug, Clone)]
pub struct Kx2Footer {
    pub total_chunks: u64,
    pub total_rows: u64,
    pub magic_tail: [u8; 8],
}

impl Kx2Footer {
    pub const MAGIC_TAIL: [u8; 8] = *b"KX2FOOT!";
    pub const SIZE: usize = 24;

    pub fn new(total_chunks: u64, total_rows: u64) -> Self {
        Kx2Footer {
            total_chunks,
            total_rows,
            magic_tail: Self::MAGIC_TAIL,
        }
    }

    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_u64::<LittleEndian>(self.total_chunks)?;
        w.write_u64::<LittleEndian>(self.total_rows)?;
        w.write_all(&self.magic_tail)?;
        Ok(())
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let total_chunks = r.read_u64::<LittleEndian>()?;
        let total_rows = r.read_u64::<LittleEndian>()?;
        let mut magic_tail = [0u8; 8];
        r.read_exact(&mut magic_tail)?;
        if magic_tail != Self::MAGIC_TAIL {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid footer magic",
            ));
        }
        Ok(Kx2Footer {
            total_chunks,
            total_rows,
            magic_tail,
        })
    }
}
