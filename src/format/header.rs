use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Read, Seek, SeekFrom, Write};

pub const KITE_MAGIC: [u8; 4] = *b"KX2!";
pub const KITE_VERSION: u16 = 1;

pub const KEY_WIDTH_U64: u16 = 64;
pub const KEY_WIDTH_U128: u16 = 128;
pub const KEY_WIDTH_U256: u16 = 256;
const METADATA_MAGIC: [u8; 4] = *b"KMT1";
const METADATA_HAS_MIN: u32 = 1;
const METADATA_HAS_MAX: u32 = 2;

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
    pub min_count: Option<u32>,
    pub max_count: Option<u32>,
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
            min_count: None,
            max_count: None,
        }
    }

    pub fn with_count_filters(mut self, min_count: Option<u32>, max_count: Option<u32>) -> Self {
        self.min_count = min_count;
        self.max_count = max_count;
        self
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
        let mut reserved = [0u8; 68];
        if self.min_count.is_some() || self.max_count.is_some() {
            reserved[0..4].copy_from_slice(&METADATA_MAGIC);
            let mut flags = 0u32;
            if self.min_count.is_some() {
                flags |= METADATA_HAS_MIN;
            }
            if self.max_count.is_some() {
                flags |= METADATA_HAS_MAX;
            }
            reserved[4..8].copy_from_slice(&flags.to_le_bytes());
            reserved[8..12].copy_from_slice(&self.min_count.unwrap_or(0).to_le_bytes());
            reserved[12..16].copy_from_slice(&self.max_count.unwrap_or(0).to_le_bytes());
        }
        w.write_all(&reserved)?;
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
        let mut reserved = [0u8; 68];
        r.read_exact(&mut reserved)?;
        let (min_count, max_count) = if reserved[0..4] == METADATA_MAGIC {
            let flags = u32::from_le_bytes(reserved[4..8].try_into().unwrap());
            let min = u32::from_le_bytes(reserved[8..12].try_into().unwrap());
            let max = u32::from_le_bytes(reserved[12..16].try_into().unwrap());
            (
                (flags & METADATA_HAS_MIN != 0).then_some(min),
                (flags & METADATA_HAS_MAX != 0).then_some(max),
            )
        } else {
            (None, None)
        };
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
            min_count,
            max_count,
        })
    }

    pub const SIZE: usize = 128;

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

pub fn update_count_filters_in_file<W: Read + Write + Seek>(
    file: &mut W,
    min_count: Option<u32>,
    max_count: Option<u32>,
) -> io::Result<KiteHeader> {
    file.seek(SeekFrom::Start(0))?;
    let mut header = KiteHeader::read_from(file)?;
    header.min_count = min_count;
    header.max_count = max_count;
    let mut buf = vec![0u8; KiteHeader::SIZE];
    {
        let mut cursor = io::Cursor::new(&mut buf[..]);
        header.write_to(&mut cursor)?;
    }
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&buf)?;
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn header_roundtrips_optional_count_filters() {
        let header =
            KiteHeader::new(21, false, 256, KEY_WIDTH_U64, 0).with_count_filters(Some(2), None);
        let mut bytes = Vec::new();
        header.write_to(&mut bytes).unwrap();
        assert_eq!(bytes.len(), KiteHeader::SIZE);

        let parsed = KiteHeader::read_from(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(parsed.k, 21);
        assert_eq!(parsed.min_count, Some(2));
        assert_eq!(parsed.max_count, None);
    }

    #[test]
    fn update_count_filters_rewrites_only_header_metadata() {
        let mut bytes = Vec::new();
        KiteHeader::new(31, true, 512, KEY_WIDTH_U64, 3)
            .with_count_filters(Some(1), Some(10))
            .write_to(&mut bytes)
            .unwrap();
        bytes.extend_from_slice(b"payload");

        let mut cursor = Cursor::new(bytes);
        let updated = update_count_filters_in_file(&mut cursor, None, Some(5)).unwrap();
        assert_eq!(updated.min_count, None);
        assert_eq!(updated.max_count, Some(5));

        cursor.set_position(0);
        let parsed = KiteHeader::read_from(&mut cursor).unwrap();
        assert_eq!(parsed.min_count, None);
        assert_eq!(parsed.max_count, Some(5));
        assert_eq!(&cursor.get_ref()[KiteHeader::SIZE..], b"payload");
    }
}
