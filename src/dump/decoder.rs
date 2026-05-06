use itoa;

/// Lookup table: each byte encodes 4 DNA bases (2 bits each, MSB-first).
/// BASE4_TABLE[byte] = [base0, base1, base2, base3]
/// base_i = DNA[(byte >> (6 - 2*i)) & 3]
/// Used by the fast 4-at-a-time k-mer decode routines.
const fn make_4base_table() -> [[u8; 4]; 256] {
    let dna = [b'A', b'C', b'G', b'T'];
    let mut table = [[0u8; 4]; 256];
    let mut b = 0usize;
    while b < 256 {
        table[b] = [
            dna[(b >> 6) & 3],
            dna[(b >> 4) & 3],
            dna[(b >> 2) & 3],
            dna[b & 3],
        ];
        b += 1;
    }
    table
}
static BASE4_TABLE: [[u8; 4]; 256] = make_4base_table();

/// Decode a k-mer key of any width (u128 superset; for k≤32, key fits in u64).
/// 4-base-at-a-time using BASE4_TABLE: ~4× faster than the 1-base loop.
#[inline]
pub fn decode_kmer_any(key: u128, k: u8, buf: &mut [u8]) {
    debug_assert!(buf.len() >= k as usize);
    let k = k as usize;
    // Left-align significant bits: base[0] occupies bits 127..126 of `shifted`.
    // For k=64: shift==0 (no-op). For k=1: shift==126.
    let shifted = key << (128u32.saturating_sub(2 * k as u32));
    let full = k / 4;   // number of complete 4-base groups
    let rem  = k % 4;   // leftover bases (0..3)
    for i in 0..full {
        // Group i occupies byte at bit position 127..120 shifted down by 8*i.
        let byte = (shifted >> (120usize.saturating_sub(8 * i))) as u8;
        unsafe {
            let src = BASE4_TABLE.get_unchecked(byte as usize);
            buf.get_unchecked_mut(4 * i..4 * i + 4).copy_from_slice(src);
        }
    }
    if rem > 0 {
        let byte = (shifted >> (120usize.saturating_sub(8 * full))) as u8;
        let src = unsafe { BASE4_TABLE.get_unchecked(byte as usize) };
        for j in 0..rem {
            buf[full * 4 + j] = src[j];
        }
    }
}

/// Decode a 256-bit k-mer (KmerU256, k=65..128) back to DNA string.
/// `lo` = bits 0..127, `hi` = bits 128..255.
/// Uses the same 4-base-at-a-time lookup strategy: concatenate hi||lo into a
/// virtual 256-bit value, decoded from the MSB.
#[inline]
pub fn decode_kmer_u256(lo: u128, hi: u128, k: u8, buf: &mut [u8]) {
    debug_assert!(buf.len() >= k as usize);
    // The k-mer is stored as (hi_bits, lo_bits) where base[0] is at the
    // most-significant 2 bits of hi.  Left-align to a virtual 256-bit field
    // by treating the first `min(k,64)` bases from hi and remainder from lo.
    let k = k as usize;
    let hi_bases = k.min(64);     // bases encoded in `hi` (up to 64)
    let lo_bases = k - hi_bases;  // bases encoded in `lo` (the rest)

    // Decode hi portion (bases 0..hi_bases) using hi left-aligned.
    let hi_shifted = hi << (128u32.saturating_sub(2 * hi_bases as u32));
    let full_hi = hi_bases / 4;
    let rem_hi  = hi_bases % 4;
    for i in 0..full_hi {
        let byte = (hi_shifted >> (120usize.saturating_sub(8 * i))) as u8;
        unsafe {
            let src = BASE4_TABLE.get_unchecked(byte as usize);
            buf.get_unchecked_mut(4 * i..4 * i + 4).copy_from_slice(src);
        }
    }
    if rem_hi > 0 {
        let byte = (hi_shifted >> (120usize.saturating_sub(8 * full_hi))) as u8;
        let src = unsafe { BASE4_TABLE.get_unchecked(byte as usize) };
        for j in 0..rem_hi {
            buf[full_hi * 4 + j] = src[j];
        }
    }

    if lo_bases > 0 {
        // Decode lo portion (bases hi_bases..k) using lo left-aligned.
        let lo_shifted = lo << (128u32.saturating_sub(2 * lo_bases as u32));
        let off = hi_bases;
        let full_lo = lo_bases / 4;
        let rem_lo  = lo_bases % 4;
        for i in 0..full_lo {
            let byte = (lo_shifted >> (120usize.saturating_sub(8 * i))) as u8;
            unsafe {
                let src = BASE4_TABLE.get_unchecked(byte as usize);
                buf.get_unchecked_mut(off + 4 * i..off + 4 * i + 4).copy_from_slice(src);
            }
        }
        if rem_lo > 0 {
            let byte = (lo_shifted >> (120usize.saturating_sub(8 * full_lo))) as u8;
            let src = unsafe { BASE4_TABLE.get_unchecked(byte as usize) };
            for j in 0..rem_lo {
                buf[off + full_lo * 4 + j] = src[j];
            }
        }
    }
}

/// Format a single TSV line into buf: "<kmer>\t<count>\n"
/// Uses a reused itoa buffer to avoid per-line formatter construction.
#[inline]
pub fn format_tsv_line(kmer: &[u8], count: u32, buf: &mut Vec<u8>, count_buf: &mut itoa::Buffer) {
    buf.extend_from_slice(kmer);
    buf.push(b'\t');
    buf.extend_from_slice(count_buf.format(count).as_bytes());
    buf.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_kmer_k3() {
        // ACG = 0b00_01_10 = 6
        let mut buf = [0u8; 3];
        decode_kmer(6, 3, &mut buf);
        assert_eq!(&buf, b"ACG");
    }

    #[test]
    fn test_decode_kmer_k1() {
        let mut buf = [0u8; 1];
        decode_kmer(0, 1, &mut buf); assert_eq!(&buf, b"A");
        decode_kmer(1, 1, &mut buf); assert_eq!(&buf, b"C");
        decode_kmer(2, 1, &mut buf); assert_eq!(&buf, b"G");
        decode_kmer(3, 1, &mut buf); assert_eq!(&buf, b"T");
    }

    #[test]
    fn test_roundtrip_k21() {
        use crate::count::kmer::{KmerIter};
        let seq = b"ACGTACGTACGTACGTACGTA";
        let kmers: Vec<u64> = KmerIter::new(seq, 21, false).collect();
        assert_eq!(kmers.len(), 1);
        let mut buf = vec![0u8; 21];
        decode_kmer(kmers[0], 21, &mut buf);
        assert_eq!(&buf, &seq[..21]);
    }

    #[test]
    fn test_write_u32() {
        let cases = [0u32, 1, 9, 10, 99, 100, 999, 65535, u32::MAX];
        for &n in &cases {
            let mut buf = Vec::new();
            write_u32_decimal(n, &mut buf);
            assert_eq!(std::str::from_utf8(&buf).unwrap(), n.to_string(),
                "mismatch for n={n}");
        }
    }

    #[test]
    fn test_format_tsv_line() {
        let mut buf = Vec::new();
        let mut count_buf = itoa::Buffer::new();
        format_tsv_line(b"ACGT", 42, &mut buf, &mut count_buf);
        assert_eq!(&buf, b"ACGT\t42\n");
    }
}
