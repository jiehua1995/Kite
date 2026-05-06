use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Read, Write};

pub const KITE_MAGIC: [u8; 4] = *b"KX2!";
pub const KITE_VERSION: u16 = 4;

pub const KEY_WIDTH_U64: u16 = 64;
pub const KEY_WIDTH_U128: u16 = 128;
pub const KEY_WIDTH_U256: u16 = 256;

/// KX2 fixed 128-byte file header.
#[derive(Debug, Clone)]
pub struct KiteHeader {
    pub magic: [u8; 4],
    pub version: u16,
    pub k: u8,
    pub canonical: u8,
    pub partitions: u32,
    pub key_width: u16,
    pub codec_mask: u16,
    pub checksum_mode: u32,
    pub chunk_index_offset: u64,
    pub partition_table_offset: u64,
    pub footer_offset: u64,
    pub total_chunks: u64,
    pub total_rows: u64,
    // 68 bytes reserved — total 128 bytes.
}

impl KiteHeader {
    pub fn new(k: u8, canonical: bool, partitions: u32, key_width: u16, codec_mask: u16) -> Self {
        KiteHeader {
            magic: KITE_MAGIC,
            version: KITE_VERSION,
            k,
            canonical: canonical as u8,
            partitions,
            key_width,
            codec_mask,
            checksum_mode: 0,
            chunk_index_offset: 0,
            partition_table_offset: 0,
            footer_offset: 0,
            total_chunks: 0,
            total_rows: 0,
        }
    }

    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&self.magic)?;
        w.write_u16::<LittleEndian>(self.version)?;
        w.write_u8(self.k)?;
        w.write_u8(self.canonical)?;
        w.write_u32::<LittleEndian>(self.partitions)?;
        w.write_u16::<LittleEndian>(self.key_width)?;
        w.write_u16::<LittleEndian>(self.codec_mask)?;
        w.write_u32::<LittleEndian>(self.checksum_mode)?;
        w.write_u64::<LittleEndian>(self.chunk_index_offset)?;
        w.write_u64::<LittleEndian>(self.partition_table_offset)?;
        w.write_u64::<LittleEndian>(self.footer_offset)?;
        w.write_u64::<LittleEndian>(self.total_chunks)?;
        w.write_u64::<LittleEndian>(self.total_rows)?;
        w.write_all(&[0u8; 68])?;
        Ok(())
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic)?;
        if magic != KITE_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid magic: expected KITE, got {:?}", magic),
            ));
        }
        let version = r.read_u16::<LittleEndian>()?;
        if version != KITE_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported version: {}", version),
            ));
        }
        let k = r.read_u8()?;
        let canonical = r.read_u8()?;
        let partitions = r.read_u32::<LittleEndian>()?;
        let key_width = r.read_u16::<LittleEndian>()?;
        let codec_mask = r.read_u16::<LittleEndian>()?;
        let checksum_mode = r.read_u32::<LittleEndian>()?;
        let chunk_index_offset = r.read_u64::<LittleEndian>()?;
        let partition_table_offset = r.read_u64::<LittleEndian>()?;
        let footer_offset = r.read_u64::<LittleEndian>()?;
        let total_chunks = r.read_u64::<LittleEndian>()?;
        let total_rows = r.read_u64::<LittleEndian>()?;
        let mut _reserved = [0u8; 68];
        r.read_exact(&mut _reserved)?;
        Ok(KiteHeader {
            magic,
            version,
            k,
            canonical,
            partitions,
            key_width,
            codec_mask,
            checksum_mode,
            chunk_index_offset,
            partition_table_offset,
            footer_offset,
            total_chunks,
            total_rows,
        })
    }

    pub const SIZE: usize = 128;

    #[allow(dead_code)]
    #[inline]
    pub fn is_wide_key(&self) -> bool {
        self.key_width == KEY_WIDTH_U128
    }

    #[inline]
    pub fn entry_size(&self) -> usize {
        match self.key_width {
            KEY_WIDTH_U64 => 12,
            KEY_WIDTH_U128 => 20,
            KEY_WIDTH_U256 => 36,
            _ => 12,
        }
    }
}
