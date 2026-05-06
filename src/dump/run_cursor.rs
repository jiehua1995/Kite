/// Iterator over (key: u64, count: u32) entries in a single run's data slice.
/// The slice must contain exactly `entry_count * 12` bytes (u64 + u32 per entry).
#[allow(dead_code)]
pub struct RunCursor<'a> {
    data: &'a [u8],
    pos: usize,
    pub entry_count: usize,
    current: usize,
}

#[allow(dead_code)]
impl<'a> RunCursor<'a> {
    pub fn new(data: &'a [u8], entry_count: u32) -> Self {
        RunCursor { data, pos: 0, entry_count: entry_count as usize, current: 0 }
    }
}

impl<'a> Iterator for RunCursor<'a> {
    type Item = (u64, u32);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= self.entry_count {
            return None;
        }
        if self.pos + 12 > self.data.len() {
            return None;
        }
        // Read u64 key (little-endian) + u32 count (little-endian)
        let key_bytes = &self.data[self.pos..self.pos + 8];
        let cnt_bytes = &self.data[self.pos + 8..self.pos + 12];
        let key = u64::from_le_bytes(key_bytes.try_into().unwrap());
        let cnt = u32::from_le_bytes(cnt_bytes.try_into().unwrap());
        self.pos += 12;
        self.current += 1;
        Some((key, cnt))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.entry_count - self.current;
        (remaining, Some(remaining))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use byteorder::WriteBytesExt;
    use byteorder::LittleEndian;

    #[test]
    fn test_run_cursor() {
        let mut data = Vec::new();
        let entries: Vec<(u64, u32)> = vec![(10, 3), (20, 1), (30, 5)];
        for &(k, c) in &entries {
            data.write_u64::<LittleEndian>(k).unwrap();
            data.write_u32::<LittleEndian>(c).unwrap();
        }
        let cursor = RunCursor::new(&data, 3);
        let result: Vec<_> = cursor.collect();
        assert_eq!(result, entries);
    }

    #[test]
    fn test_run_cursor_empty() {
        let cursor = RunCursor::new(&[], 0);
        let result: Vec<_> = cursor.collect();
        assert!(result.is_empty());
    }
}
