// 中文：这里是 `dump/query` 专用的解码工具，不和 `count` 共享，
// 这样可以单独优化导出路径而不拖慢计数路径。
// English: These are `dump/query`-specific decode helpers. They are kept
// separate from `count` so the export path can be tuned independently.
#[inline(always)]
/// 中文：把 ASCII 碱基编码为 2-bit 值；非法字符会返回 `INVALID_BASE`。
/// English: Encode an ASCII nucleotide into a 2-bit code; invalid symbols map
/// to `INVALID_BASE`.
pub fn encode_base(b: u8) -> u8 {
    BASE_LUT[b as usize]
}

#[inline(always)]
/// 中文：把 2-bit 编码还原成 `A/C/G/T`。
/// English: Decode a 2-bit nucleotide code back into `A/C/G/T`.
const fn decode_base(v: u8) -> u8 {
    match v & 0b11 {
        0 => b'A',
        1 => b'C',
        2 => b'G',
        _ => b'T',
    }
}

pub const INVALID_BASE: u8 = 0xFF;

/// 中文：构建 256 项查表，把大小写碱基都映射到统一的 2-bit 编码。
/// English: Build a 256-entry LUT that maps upper/lower-case nucleotides into
/// the same 2-bit encoding.
const fn make_base_lut() -> [u8; 256] {
    let mut lut = [INVALID_BASE; 256];
    lut[b'A' as usize] = 0;
    lut[b'a' as usize] = 0;
    lut[b'C' as usize] = 1;
    lut[b'c' as usize] = 1;
    lut[b'G' as usize] = 2;
    lut[b'g' as usize] = 2;
    lut[b'T' as usize] = 3;
    lut[b't' as usize] = 3;
    lut[b'U' as usize] = 3;
    lut[b'u' as usize] = 3;
    lut
}

const BASE_LUT: [u8; 256] = make_base_lut();

/// 中文：预计算 16-bit 到 8 个碱基字符的查表，加速 `dump` 文本写出。
/// English: Precompute a 16-bit -> 8-base lookup table to accelerate text
/// emission during `dump`.
const fn make_decode_u16_lut() -> [[u8; 8]; 65536] {
    let mut lut = [[0u8; 8]; 65536];
    let mut value = 0usize;
    while value < 65536 {
        let word = value as u16;
        lut[value][0] = decode_base(((word >> 14) & 0b11) as u8);
        lut[value][1] = decode_base(((word >> 12) & 0b11) as u8);
        lut[value][2] = decode_base(((word >> 10) & 0b11) as u8);
        lut[value][3] = decode_base(((word >> 8) & 0b11) as u8);
        lut[value][4] = decode_base(((word >> 6) & 0b11) as u8);
        lut[value][5] = decode_base(((word >> 4) & 0b11) as u8);
        lut[value][6] = decode_base(((word >> 2) & 0b11) as u8);
        lut[value][7] = decode_base((word & 0b11) as u8);
        value += 1;
    }
    lut
}

const DECODE_U16_LUT: [[u8; 8]; 65536] = make_decode_u16_lut();

#[inline(always)]
/// 中文：把 `u64` 编码的 k-mer 解码到调用方提供的缓冲区。
/// 输入是编码值、k 长度和输出缓冲区；输出写入 `out`。
/// English: Decode a `u64`-encoded k-mer into the caller-provided buffer.
/// Inputs are the packed value, k length, and output slice; results are
/// written into `out`.
pub fn decode_u64_fast_into(kmer: u64, k: usize, out: &mut [u8]) {
    let rem = k % 8;
    for i in 0..rem {
        let shift = (k - 1 - i) * 2;
        out[i] = decode_base(((kmer >> shift) & 0b11) as u8);
    }

    let groups = (k - rem) / 8;
    for group in 0..groups {
        let out_idx = rem + group * 8;
        let shift = (k - out_idx - 8) * 2;
        let packed = ((kmer >> shift) & 0xFFFF) as usize;
        out[out_idx..out_idx + 8].copy_from_slice(&DECODE_U16_LUT[packed]);
    }
}

#[inline(always)]
/// 中文：把 `u128` 编码的 k-mer 解码到输出缓冲区。
/// English: Decode a `u128`-encoded k-mer into the output buffer.
pub fn decode_u128_fast_into(kmer: u128, k: usize, out: &mut [u8]) {
    let rem = k % 8;
    for i in 0..rem {
        let shift = (k - 1 - i) * 2;
        out[i] = decode_base(((kmer >> shift) & 0b11) as u8);
    }

    let groups = (k - rem) / 8;
    for group in 0..groups {
        let out_idx = rem + group * 8;
        let shift = (k - out_idx - 8) * 2;
        let packed = ((kmer >> shift) & 0xFFFF) as usize;
        out[out_idx..out_idx + 8].copy_from_slice(&DECODE_U16_LUT[packed]);
    }
}

#[inline(always)]
/// 中文：把多 `u64` packed 形式的长 k-mer 解码到输出缓冲区。
/// English: Decode a long packed k-mer stored across multiple `u64` words into
/// the output buffer.
pub fn decode_packed_fast_into(words: &[u64], k: usize, out: &mut [u8]) {
    let rem = k % 8;
    for i in 0..rem {
        let bit_pos = (k - 1 - i) * 2;
        out[i] = decode_base(extract_2bits(words, bit_pos) as u8);
    }

    let groups = (k - rem) / 8;
    for group in 0..groups {
        let out_idx = rem + group * 8;
        let bit_pos = (k - out_idx - 8) * 2;
        let packed = extract_16bits(words, bit_pos) as usize;
        out[out_idx..out_idx + 8].copy_from_slice(&DECODE_U16_LUT[packed]);
    }
}

#[inline(always)]
/// 中文：从 packed word 数组里取出指定 bit 位置的 2-bit 碱基。
/// English: Extract one 2-bit nucleotide from a packed word array.
fn extract_2bits(words: &[u64], bit_pos: usize) -> u64 {
    let word_idx = bit_pos / 64;
    let bit_idx = bit_pos % 64;
    if bit_idx <= 62 {
        (words[word_idx] >> bit_idx) & 0b11
    } else {
        let lo = (words[word_idx] >> bit_idx) & 0b1;
        let hi = (words[word_idx + 1] & 0b1) << 1;
        lo | hi
    }
}

#[inline(always)]
/// 中文：从 packed word 数组里取出一段 16-bit，用于查表批量解码。
/// English: Extract a 16-bit segment from the packed word array for LUT-based
/// batch decoding.
fn extract_16bits(words: &[u64], bit_pos: usize) -> u16 {
    let word_idx = bit_pos / 64;
    let bit_idx = bit_pos % 64;
    if bit_idx <= 48 {
        ((words[word_idx] >> bit_idx) & 0xFFFF) as u16
    } else {
        let lo = words[word_idx] >> bit_idx;
        let hi = words.get(word_idx + 1).copied().unwrap_or(0) << (64 - bit_idx);
        ((lo | hi) & 0xFFFF) as u16
    }
}
