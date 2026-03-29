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

/// 中文：构建 `u16 -> 8bp` 解码查表，减少 profile 导出中的逐字符分支。
/// English: Build the `u16 -> 8bp` decode LUT so profile export avoids
/// per-base branching.
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
/// 中文：把 `u64` 编码的 k-mer 解码到调用者给定的输出缓冲区。
/// English: Decode a `u64`-encoded k-mer into the caller-provided output
/// buffer.
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
