/// 2-bit encoding: A=0, C=1, G=2, T=3, invalid=255
static BASE_ENCODE: [u8; 256] = {
    let mut t = [255u8; 256];
    t[b'A' as usize] = 0; t[b'a' as usize] = 0;
    t[b'C' as usize] = 1; t[b'c' as usize] = 1;
    t[b'G' as usize] = 2; t[b'g' as usize] = 2;
    t[b'T' as usize] = 3; t[b't' as usize] = 3;
    t
};

// ─── KmerU256 ────────────────────────────────────────────────────────────────

/// 256-bit k-mer value for k = 65..=128.
/// Stored as (hi: u128, lo: u128) where lo = bits 0..127, hi = bits 128..255.
/// Ord compares hi first, which correctly orders the 256-bit integer value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KmerU256 {
    pub hi: u128,
    pub lo: u128,
}

impl PartialOrd for KmerU256 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for KmerU256 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.hi.cmp(&other.hi).then(self.lo.cmp(&other.lo))
    }
}

// ─── KmerWord trait ──────────────────────────────────────────────────────────

/// Common interface for k-mer storage types:
///   - `u64`      for k ≤ 32
///   - `u128`     for 33 ≤ k ≤ 64
///   - `KmerU256` for 65 ≤ k ≤ 128
pub trait KmerWord: Copy + Ord + Send + 'static {
    /// Extract the bucket index using Fibonacci multiplicative hashing.
    /// `bucket_bits` = log2(num_buckets); result is in [0, 2^bucket_bits).
    fn to_bucket(self, bucket_bits: u32) -> usize;
    /// Number of bytes per k-mer value in memory (for budget tracking).
    fn byte_size() -> usize;
    /// Number of bytes per (key, count) entry in the .kite on-disk format.
    fn entry_size() -> usize;
    /// Append a (key, count) entry to a serialisation buffer.
    fn write_entry(self, count: u32, buf: &mut Vec<u8>);
    /// In-place sort of a k-mer slice (k-aware radix sort).
    fn sort_slice(buf: &mut Vec<Self>, k: u8);
    /// Iterate over all valid k-mers in `seq` and call `f` for each.
    /// Monomorphised at compile time — zero virtual-dispatch overhead.
    fn scan_sequence(seq: &[u8], k: u8, canonical: bool, f: impl FnMut(Self));
    /// Encode a sorted, non-empty RLE-compressed run into `out` using DVLE.
    /// Returns `true` if DVLE was used; `false` if raw encoding was used (K=KmerU256).
    fn encode_dvle_run(entries: &[(Self, u32)], out: &mut Vec<u8>) -> bool;
    /// Split the key into low/high 128-bit words for chunk range metadata.
    fn split_key(self) -> (u128, u128);
}

impl KmerWord for u64 {
    #[inline(always)]
    fn to_bucket(self, bucket_bits: u32) -> usize {
        // Fibonacci / multiplicative hash: uniform distribution even for biased DNA k-mers.
        // Constant = 2^64 / φ (golden ratio).
        (self.wrapping_mul(0x9E3779B97F4A7C15u64) >> (64 - bucket_bits)) as usize
    }
    #[inline(always)]
    fn byte_size() -> usize { 8 }
    #[inline(always)]
    fn entry_size() -> usize { 12 }
    #[inline(always)]
    fn write_entry(self, count: u32, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
    }
    #[inline(always)]
    fn sort_slice(buf: &mut Vec<u64>, k: u8) {
        crate::count::sorter::radix_sort_u64_kmer(buf, k);
    }
    #[inline]
    fn scan_sequence(seq: &[u8], k: u8, canonical: bool, mut f: impl FnMut(u64)) {
        for kmer in KmerIter::new(seq, k, canonical) { f(kmer); }
    }
    #[inline]
    fn encode_dvle_run(entries: &[(u64, u32)], out: &mut Vec<u8>) -> bool {
        if !entries.is_empty() {
            crate::format::run_block::encode_dvle_u64(entries, out);
        }
        true
    }

    #[inline]
    fn split_key(self) -> (u128, u128) {
        (self as u128, 0)
    }
}

impl KmerWord for u128 {
    #[inline(always)]
    fn to_bucket(self, bucket_bits: u32) -> usize {
        // XOR-fold high and low 64-bit halves, then Fibonacci hash.
        let lo = self as u64;
        let hi = (self >> 64) as u64;
        ((lo ^ hi).wrapping_mul(0x9E3779B97F4A7C15u64) >> (64 - bucket_bits)) as usize
    }
    #[inline(always)]
    fn byte_size() -> usize { 16 }
    #[inline(always)]
    fn entry_size() -> usize { 20 }
    #[inline(always)]
    fn write_entry(self, count: u32, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
    }
    #[inline(always)]
    fn sort_slice(buf: &mut Vec<u128>, k: u8) {
        crate::count::sorter::radix_sort_u128_kmer(buf, k);
    }
    #[inline]
    fn scan_sequence(seq: &[u8], k: u8, canonical: bool, mut f: impl FnMut(u128)) {
        for kmer in KmerIter128::new(seq, k, canonical) { f(kmer); }
    }
    #[inline]
    fn encode_dvle_run(entries: &[(u128, u32)], out: &mut Vec<u8>) -> bool {
        if !entries.is_empty() {
            crate::format::run_block::encode_dvle_u128(entries, out);
        }
        true
    }

    #[inline]
    fn split_key(self) -> (u128, u128) {
        (self, 0)
    }
}

impl KmerWord for KmerU256 {
    #[inline(always)]
    fn to_bucket(self, bucket_bits: u32) -> usize {
        // XOR-fold all four 64-bit words, then Fibonacci hash.
        let a = self.lo as u64 ^ (self.lo >> 64) as u64;
        let b = self.hi as u64 ^ (self.hi >> 64) as u64;
        ((a ^ b).wrapping_mul(0x9E3779B97F4A7C15u64) >> (64 - bucket_bits)) as usize
    }
    #[inline(always)]
    fn byte_size() -> usize { 32 }
    #[inline(always)]
    fn entry_size() -> usize { 36 } // 32B key + 4B count
    #[inline(always)]
    fn write_entry(self, count: u32, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.lo.to_le_bytes());
        buf.extend_from_slice(&self.hi.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
    }
    #[inline(always)]
    fn sort_slice(buf: &mut Vec<KmerU256>, k: u8) {
        crate::count::sorter::radix_sort_u256_kmer(buf, k);
    }
    #[inline]
    fn scan_sequence(seq: &[u8], k: u8, canonical: bool, mut f: impl FnMut(KmerU256)) {
        for kmer in KmerIterU256::new(seq, k, canonical) { f(kmer); }
    }
    #[inline]
    fn encode_dvle_run(entries: &[(KmerU256, u32)], out: &mut Vec<u8>) -> bool {
        for &(key, count) in entries { key.write_entry(count, out); }
        false
    }

    #[inline]
    fn split_key(self) -> (u128, u128) {
        (self.lo, self.hi)
    }
}

// ─── KmerIter (u64, k ≤ 32) ──────────────────────────────────────────────────

/// Rolling k-mer generator for k ≤ 32 (uses u64 internally).
pub struct KmerIter<'a> {
    seq: &'a [u8],
    pos: usize,
    fwd: u64,
    rev: u64,
    valid: u32,
    k: u8,
    mask: u64,
    shift: u32,
    canonical: bool,
}

impl<'a> KmerIter<'a> {
    #[inline]
    pub fn new(seq: &'a [u8], k: u8, canonical: bool) -> Self {
        assert!(k >= 1 && k <= 32, "k must be 1..=32 for u64 path");
        let mask = if k == 32 { u64::MAX } else { (1u64 << (2 * k as u32)) - 1 };
        KmerIter { seq, pos: 0, fwd: 0, rev: 0, valid: 0, k, mask,
                   shift: 2 * (k as u32 - 1), canonical }
    }
}

impl<'a> Iterator for KmerIter<'a> {
    type Item = u64;
    #[inline]
    fn next(&mut self) -> Option<u64> {
        let k = self.k as u32;
        loop {
            if self.pos >= self.seq.len() { return None; }
            let b = self.seq[self.pos]; self.pos += 1;
            let enc = BASE_ENCODE[b as usize];
            if enc == 255 { self.fwd = 0; self.rev = 0; self.valid = 0; continue; }
            self.fwd = ((self.fwd << 2) | enc as u64) & self.mask;
            self.rev = (self.rev >> 2) | (((enc ^ 3) as u64) << self.shift);
            self.valid += 1;
            if self.valid >= k {
                return Some(if self.canonical { self.fwd.min(self.rev) } else { self.fwd });
            }
        }
    }
}

// ─── KmerIter128 (u128, 33 ≤ k ≤ 64) ────────────────────────────────────────

/// Rolling k-mer generator for 33 ≤ k ≤ 64 (uses u128 internally).
pub struct KmerIter128<'a> {
    seq: &'a [u8],
    pos: usize,
    fwd: u128,
    rev: u128,
    valid: u32,
    k: u8,
    mask: u128,
    shift: u32,
    canonical: bool,
}

impl<'a> KmerIter128<'a> {
    #[inline]
    pub fn new(seq: &'a [u8], k: u8, canonical: bool) -> Self {
        assert!(k >= 1 && k <= 64, "k must be 1..=64 for u128 path");
        let mask = if k == 64 { u128::MAX } else { (1u128 << (2 * k as u32)) - 1 };
        KmerIter128 { seq, pos: 0, fwd: 0, rev: 0, valid: 0, k, mask,
                      shift: 2 * (k as u32 - 1), canonical }
    }
}

impl<'a> Iterator for KmerIter128<'a> {
    type Item = u128;
    #[inline]
    fn next(&mut self) -> Option<u128> {
        let k = self.k as u32;
        loop {
            if self.pos >= self.seq.len() { return None; }
            let b = self.seq[self.pos]; self.pos += 1;
            let enc = BASE_ENCODE[b as usize];
            if enc == 255 { self.fwd = 0; self.rev = 0; self.valid = 0; continue; }
            self.fwd = ((self.fwd << 2) | enc as u128) & self.mask;
            self.rev = (self.rev >> 2) | (((enc ^ 3) as u128) << self.shift);
            self.valid += 1;
            if self.valid >= k {
                return Some(if self.canonical { self.fwd.min(self.rev) } else { self.fwd });
            }
        }
    }
}

// ─── KmerIterU256 (KmerU256, 65 ≤ k ≤ 128) ───────────────────────────────────

/// Rolling k-mer generator for 65 ≤ k ≤ 128 using packed 256-bit integers.
///
/// Layout: the k-mer occupies bits 0..2k-1, with bit 0 at the LSB of `lo`,
/// and bit 2k-1 at the MSB of the valid range.  For k in [65..128]:
///   - `lo` holds bits 0..127 (always fully used)
///   - `hi` holds bits 128..2k-1 (uses 2*(k-64) bits out of 128)
pub struct KmerIterU256<'a> {
    seq: &'a [u8],
    pos: usize,
    fwd: KmerU256,
    rev: KmerU256,
    valid: u32,
    k: u8,
    /// Number of valid bits in `hi`: `2k - 128`.
    _hi_bits: u32,
    /// Mask for the valid portion of `hi`.
    mask_hi: u128,
    /// Bit position within `hi` where the reverse-complement MSB is placed: `2*(k-1) - 128`.
    rev_top: u32,
    canonical: bool,
}

impl<'a> KmerIterU256<'a> {
    #[inline]
    pub fn new(seq: &'a [u8], k: u8, canonical: bool) -> Self {
        assert!(k >= 65 && k <= 128, "k must be 65..=128 for KmerU256 path");
        let hi_bits = 2 * k as u32 - 128;
        let mask_hi = if hi_bits == 128 { u128::MAX } else { (1u128 << hi_bits) - 1 };
        let rev_top = 2 * (k as u32 - 1) - 128; // = 2k-2-128, always in [0..126]
        KmerIterU256 {
            seq, pos: 0,
            fwd: KmerU256 { hi: 0, lo: 0 },
            rev: KmerU256 { hi: 0, lo: 0 },
            valid: 0, k, _hi_bits: hi_bits, mask_hi, rev_top, canonical,
        }
    }
}

impl<'a> Iterator for KmerIterU256<'a> {
    type Item = KmerU256;
    #[inline]
    fn next(&mut self) -> Option<KmerU256> {
        let k = self.k as u32;
        loop {
            if self.pos >= self.seq.len() { return None; }
            let b = self.seq[self.pos]; self.pos += 1;
            let enc = BASE_ENCODE[b as usize];
            if enc == 255 {
                self.fwd = KmerU256 { hi: 0, lo: 0 };
                self.rev = KmerU256 { hi: 0, lo: 0 };
                self.valid = 0;
                continue;
            }
            let rc_enc = enc ^ 3;

            // Forward: shift left by 2, insert enc at LSB of lo.
            // Carry = top 2 bits of lo that overflow into hi.
            let carry_fwd = self.fwd.lo >> 126;
            self.fwd.lo  = (self.fwd.lo << 2) | (enc as u128);
            self.fwd.hi  = ((self.fwd.hi << 2) | carry_fwd) & self.mask_hi;

            // Reverse complement: shift right by 2, insert rc_enc at bit rev_top of hi.
            let carry_rev = self.rev.hi << 126;  // bottom 2 bits of hi → top 2 bits of lo
            self.rev.hi  = (self.rev.hi >> 2) | ((rc_enc as u128) << self.rev_top);
            self.rev.lo  = (self.rev.lo >> 2) | carry_rev;

            self.valid += 1;
            if self.valid >= k {
                let kmer = if self.canonical {
                    self.fwd.min(self.rev)
                } else {
                    self.fwd
                };
                return Some(kmer);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kmer_simple() {
        // AACGT k=3: AACGT → kmers: AAC, ACG, CGT
        let seq = b"AACGT";
        let kmers: Vec<u64> = KmerIter::new(seq, 3, false).collect();
        // A=0,A=0,C=1,G=2,T=3
        // AAC = 0b00_00_01 = 1
        // ACG = 0b00_01_10 = 6
        // CGT = 0b01_10_11 = 27
        assert_eq!(kmers, vec![1, 6, 27]);
    }

    #[test]
    fn test_kmer_n_reset() {
        let seq = b"ACGNACT";
        // After N at pos 3, window resets, next valid kmer starts at ACT
        let kmers: Vec<u64> = KmerIter::new(seq, 3, false).collect();
        // ACG: A=0,C=1,G=2 → 0b00_01_10 = 6
        // After N: reset, A, C, T → ACT = 0b00_01_11 = 7
        assert_eq!(kmers, vec![6, 7]);
    }

    #[test]
    fn test_canonical() {
        // For k=1: A vs T → canonical=A(0) vs T(3) → min=0 (A)
        //           C vs G → canonical=C(1) vs G(2) → min=1 (C)
        let seq = b"ACT";
        let fwd: Vec<u64> = KmerIter::new(seq, 1, false).collect();
        let can: Vec<u64> = KmerIter::new(seq, 1, true).collect();
        // fwd: A=0, C=1, T=3
        // rev complement of A=T=3, C=G=2, T=A=0
        // canonical: min(0,3)=0, min(1,2)=1, min(3,0)=0
        assert_eq!(fwd, vec![0, 1, 3]);
        assert_eq!(can, vec![0, 1, 0]);
    }

    #[test]
    fn test_canonical_k21() {
        // Verify that canonical(seq) == canonical(rev_complement(seq))
        let seq = b"ACGTACGTACGTACGTACGT"; // 20 bases, need k=21 → no kmers
        let kmers: Vec<u64> = KmerIter::new(seq, 21, true).collect();
        assert!(kmers.is_empty());

        let seq = b"ACGTACGTACGTACGTACGTA"; // 21 bases → 1 kmer
        let k1: Vec<u64> = KmerIter::new(seq, 21, true).collect();
        // Reverse complement of ACGTACGTACGTACGTACGTA
        let rc: Vec<u8> = b"TACGTACGTACGTACGTACGT".iter().rev()
            .map(|&b| match b { b'A' => b'T', b'T' => b'A', b'C' => b'G', b'G' => b'C', x => x }).collect();
        let k2: Vec<u64> = KmerIter::new(&rc, 21, true).collect();
        assert_eq!(k1, k2, "canonical kmer must equal that of reverse complement sequence");
    }

    #[test]
    fn test_kmer128_simple() {
        // Same as test_kmer_simple but via KmerIter128 — should produce identical values
        let seq = b"AACGT";
        let kmers128: Vec<u128> = KmerIter128::new(seq, 3, false).collect();
        let kmers64: Vec<u64>  = KmerIter::new(seq, 3, false).collect();
        let as_u128: Vec<u128> = kmers64.iter().map(|&x| x as u128).collect();
        assert_eq!(kmers128, as_u128);
    }

    #[test]
    fn test_kmer128_k51() {
        // 51-base sequence produces one k=51 kmer
        let seq: Vec<u8> = b"ACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACG".to_vec();
        assert_eq!(seq.len(), 51);
        let kmers: Vec<u128> = KmerIter128::new(&seq, 51, false).collect();
        assert_eq!(kmers.len(), 1);
        // The kmer must fit in 102 bits (2*51)
        assert!(kmers[0] < (1u128 << 102));
    }

    #[test]
    fn test_kmer128_canonical() {
        // canonical(seq) == canonical(rev_complement(seq)) for k=51
        let seq = b"ACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACG";
        let fwd: Vec<u128> = KmerIter128::new(seq, 51, true).collect();
        // Build its reverse complement manually
        let rc: Vec<u8> = seq.iter().rev()
            .map(|&b| match b { b'A' => b'T', b'T' => b'A', b'C' => b'G', b'G' => b'C', x => x })
            .collect();
        let rev: Vec<u128> = KmerIter128::new(&rc, 51, true).collect();
        assert_eq!(fwd, rev, "canonical k=51 kmer must equal that of reverse complement sequence");
    }

    #[test]
    fn test_kmer128_n_reset() {
        let seq = b"ACGACGACGACGACGACGACGACGACGACGACGACGACGACGACGNACGACGACGACGACGACGACGACGACGACGACGACGACGACGACGACGACGACG";
        // k=50, seq has N at position 46 → window resets → last few bases form no complete 50-mer from reset point
        let kmers: Vec<u128> = KmerIter128::new(seq, 50, false).collect();
        // N at position 45: pre-N = 45 bases → 0 kmers (45 < 50)
        // post-N = 54 bases → 54-50+1 = 5 kmers
        assert_eq!(kmers.len(), 5);
    }

    #[test]
    fn test_kmer128_agrees_with_u64_at_k32() {
        // For k=32, both iterators should produce the same value (modulo u64 -> u128 cast)
        let seq = b"ACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGT"; // 52 bases
        let k = 32u8;
        let v64: Vec<u64>  = KmerIter::new(seq, k, false).collect();
        let v128: Vec<u128> = KmerIter128::new(seq, k, false).collect();
        let as128: Vec<u128> = v64.iter().map(|&x| x as u128).collect();
        assert_eq!(v128, as128, "KmerIter128 at k=32 must match KmerIter");
    }

    // ─── KmerIterU256 tests ───────────────────────────────────────────────────

    #[test]
    fn test_kmeru256_k65_basic() {
        // Generate one k=65 kmer from 65 bases, check it fits in 130 bits (2*65)
        let seq: Vec<u8> = b"ACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTAG".to_vec();
        assert_eq!(seq.len(), 66);
        let kmers: Vec<KmerU256> = KmerIterU256::new(&seq, 65, false).collect();
        assert_eq!(kmers.len(), 2);
        // hi must only have 2 bits (hi_bits = 2*65-128 = 2)
        assert!(kmers[0].hi < 4, "hi should fit in 2 bits for k=65");
    }

    #[test]
    fn test_kmeru256_canonical() {
        // canonical(seq) == canonical(rev_complement(seq)) for k=65
        let seq = b"ACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTACGTA";
        assert_eq!(seq.len(), 65);
        let fwd: Vec<KmerU256> = KmerIterU256::new(seq, 65, true).collect();
        let rc: Vec<u8> = seq.iter().rev()
            .map(|&b| match b { b'A' => b'T', b'T' => b'A', b'C' => b'G', b'G' => b'C', x => x })
            .collect();
        let rev: Vec<KmerU256> = KmerIterU256::new(&rc, 65, true).collect();
        assert_eq!(fwd, rev, "canonical k=65 kmer must equal that of rev-complement sequence");
    }

    #[test]
    fn test_kmeru256_n_reset() {
        // k=65, N in the middle resets the window
        let mut seq = vec![b'A'; 70];
        seq[40] = b'N';
        let kmers: Vec<KmerU256> = KmerIterU256::new(&seq, 65, false).collect();
        // pre-N: 40 bases → 0 kmers (40 < 65)
        // post-N: 29 bases → 0 kmers (29 < 65)
        assert_eq!(kmers.len(), 0);

        let mut seq2 = vec![b'A'; 150];
        seq2[60] = b'N';
        let kmers2: Vec<KmerU256> = KmerIterU256::new(&seq2, 65, false).collect();
        // pre-N: 60 bases → 0 kmers; post-N: 89 bases → 89-65+1 = 25 kmers
        assert_eq!(kmers2.len(), 25);
    }
}
