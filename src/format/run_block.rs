pub const CODEC_RAW: u32 = 0x01;
pub const CODEC_DVLE: u32 = 0x02;
#[allow(dead_code)]
pub const CODEC_FOR_DELTA: u32 = 0x08;

// ── DVLE encode / decode ──────────────────────────────────────────────────────

/// Write an unsigned 64-bit integer as LEB128 varint (1–10 bytes).
#[inline]
pub fn write_varint_u64(v: u64, out: &mut Vec<u8>) {
    let mut v = v;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 { out.push(byte); return; }
        out.push(byte | 0x80);
    }
}

/// Write an unsigned 128-bit integer as LEB128 varint (1–19 bytes).
#[inline]
pub fn write_varint_u128(v: u128, out: &mut Vec<u8>) {
    let mut v = v;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 { out.push(byte); return; }
        out.push(byte | 0x80);
    }
}

/// Read an unsigned 64-bit LEB128 varint. Returns `(value, bytes_consumed)`.
/// Returns `(0, 0)` on empty input.
#[inline]
pub fn read_varint_u64(data: &[u8]) -> (u64, usize) {
    let mut result = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in data.iter().enumerate().take(10) {
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 { return (result, i + 1); }
        shift += 7;
    }
    (result, data.len().min(10))
}

/// Read an unsigned 128-bit LEB128 varint. Returns `(value, bytes_consumed)`.
#[inline]
pub fn read_varint_u128(data: &[u8]) -> (u128, usize) {
    let mut result = 0u128;
    let mut shift = 0u32;
    for (i, &byte) in data.iter().enumerate().take(19) {
        result |= ((byte & 0x7f) as u128) << shift;
        if byte & 0x80 == 0 { return (result, i + 1); }
        shift += 7;
    }
    (result, data.len().min(19))
}

/// Encode a sorted, non-empty slice of `(u64_key, count)` entries as DVLE into `out`.
/// `entries` must be sorted ascending with unique keys (post-RLE).
pub fn encode_dvle_u64(entries: &[(u64, u32)], out: &mut Vec<u8>) {
    debug_assert!(!entries.is_empty());
    let (first_key, first_count) = entries[0];
    out.extend_from_slice(&first_key.to_le_bytes());
    out.extend_from_slice(&first_count.to_le_bytes());
    let mut prev = first_key;
    for &(key, count) in &entries[1..] {
        write_varint_u64(key - prev, out);
        write_varint_u64(count as u64, out);
        prev = key;
    }
}

/// Encode a sorted, non-empty slice of `(u128_key, count)` entries as DVLE into `out`.
pub fn encode_dvle_u128(entries: &[(u128, u32)], out: &mut Vec<u8>) {
    debug_assert!(!entries.is_empty());
    let (first_key, first_count) = entries[0];
    out.extend_from_slice(&first_key.to_le_bytes());
    out.extend_from_slice(&first_count.to_le_bytes());
    let mut prev = first_key;
    for &(key, count) in &entries[1..] {
        write_varint_u128(key - prev, out);
        write_varint_u64(count as u64, out);
        prev = key;
    }
}
