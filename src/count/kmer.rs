pub use crate::utils::kite::KmerMode;
use smallvec::SmallVec;

#[inline(always)]
/// 中文：把 ASCII 碱基编码成 2-bit 值；非法字符会返回 `INVALID_BASE`。
/// English: Encode an ASCII nucleotide into a 2-bit value; invalid symbols
/// return `INVALID_BASE`.
pub fn encode_base(b: u8) -> u8 {
    BASE_LUT[b as usize]
}

pub const INVALID_BASE: u8 = 0xFF;

/// 中文：构建 FASTQ 读取阶段使用的碱基查表。
/// English: Build the nucleotide lookup table used during FASTQ parsing.
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
/// 中文：长 k-mer 的 packed 存储形式，适用于 `k > 64`。
/// English: Packed representation for long k-mers used when `k > 64`.
pub struct PackedKmer {
    pub words: SmallVec<[u64; 4]>,
}

/// 中文：计算给定 `k` 需要多少个 `u64` word 来容纳 2-bit 编码。
/// English: Compute how many `u64` words are needed to hold a 2-bit encoded
/// k-mer of length `k`.
pub fn words_for_k(k: usize) -> usize {
    (k * 2).div_ceil(64)
}

impl KmerMode {
    /// 中文：根据 `k` 自动选择内部编码模式：`u64`、`u128` 或 `packed`。
    /// English: Choose the internal encoding mode from `k`: `u64`, `u128`,
    /// or `packed`.
    pub fn from_k(k: usize) -> Self {
        if k <= 32 {
            Self::U64
        } else if k <= 64 {
            Self::U128
        } else {
            Self::Packed(words_for_k(k))
        }
    }
}

#[derive(Debug, Clone)]
/// 中文：统一的滚动 k-mer 输出键类型，封装三种编码分支。
/// English: Unified rolling-k-mer key type that wraps all three encoding
/// branches.
pub enum KmerKey {
    U64(u64),
    U128(u128),
    Packed(PackedKmer),
}

#[inline(always)]
/// 中文：对 `u64` k-mer 取 canonical 形式。
/// English: Return the canonical orientation for a `u64` k-mer.
fn canonical_u64(fwd: u64, rev: u64) -> u64 {
    fwd.min(rev)
}

#[inline(always)]
/// 中文：对 `u128` k-mer 取 canonical 形式。
/// English: Return the canonical orientation for a `u128` k-mer.
fn canonical_u128(fwd: u128, rev: u128) -> u128 {
    fwd.min(rev)
}

#[inline(always)]
/// 中文：按高位优先比较 packed words，用于决定两个长 k-mer 的字典序。
/// English: Compare packed words from the most significant side to decide the
/// lexical order of two long k-mers.
fn cmp_words_desc(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    for i in (0..a.len()).rev() {
        match a[i].cmp(&b[i]) {
            std::cmp::Ordering::Equal => continue,
            non_eq => return non_eq,
        }
    }
    std::cmp::Ordering::Equal
}

/// 中文：滚动 k-mer 编码器。输入是连续 2-bit 碱基流，
/// 输出是每个完整窗口对应的内部键类型。
/// English: Rolling k-mer encoder. It consumes a stream of 2-bit bases and
/// emits the internal key for each complete k-mer window.
pub struct RollingKmer {
    k: usize,
    mode: KmerMode,
    seen: usize,
    ring: Vec<u8>,
    ring_idx: usize,
    fwd64: u64,
    rev64: u64,
    mask64: u64,
    fwd128: u128,
    rev128: u128,
    mask128: u128,
    fwd_words: SmallVec<[u64; 4]>,
    rev_words: SmallVec<[u64; 4]>,
    top_bit_pos: usize,
    top_mask: u64,
    canonical: bool,
}

impl RollingKmer {
    /// 中文：创建新的滚动 k-mer 状态机。
    /// English: Create a new rolling k-mer state machine.
    pub fn new(k: usize, canonical: bool) -> Self {
        let mode = KmerMode::from_k(k);
        let words = words_for_k(k);
        let total_bits = k * 2;
        let high_word_bits = total_bits.saturating_sub((words.saturating_sub(1)) * 64);
        let top_mask = if high_word_bits >= 64 {
            u64::MAX
        } else {
            (1u64 << high_word_bits) - 1
        };

        Self {
            k,
            mode,
            seen: 0,
            ring: vec![0; k.max(1)],
            ring_idx: 0,
            fwd64: 0,
            rev64: 0,
            mask64: if k == 32 {
                u64::MAX
            } else {
                (1u64 << (2 * k)) - 1
            },
            fwd128: 0,
            rev128: 0,
            mask128: if k == 64 {
                u128::MAX
            } else {
                (1u128 << (2 * k)) - 1
            },
            fwd_words: SmallVec::from_vec(vec![0; words]),
            rev_words: SmallVec::from_vec(vec![0; words]),
            top_bit_pos: total_bits.saturating_sub(2),
            top_mask,
            canonical,
        }
    }

    #[inline(always)]
    /// 中文：重置内部状态，供下一条 read 复用。
    /// English: Reset the internal rolling state so it can be reused for the
    /// next read.
    pub fn reset(&mut self) {
        self.seen = 0;
        self.ring_idx = 0;
        self.fwd64 = 0;
        self.rev64 = 0;
        self.fwd128 = 0;
        self.rev128 = 0;
        self.fwd_words.fill(0);
        self.rev_words.fill(0);
    }

    #[inline(always)]
    /// 中文：推入一个新的 2-bit 碱基；当窗口长度达到 `k` 后返回编码 k-mer。
    /// English: Push a new 2-bit base; once the window reaches length `k`,
    /// return the encoded k-mer.
    pub fn push_encoded(&mut self, b: u8) -> Option<KmerKey> {
        self.ring[self.ring_idx] = b;
        self.ring_idx += 1;
        if self.ring_idx == self.k {
            self.ring_idx = 0;
        }

        if self.seen < self.k {
            self.seen += 1;
        }

        match self.mode {
            KmerMode::U64 => {
                self.fwd64 = ((self.fwd64 << 2) | b as u64) & self.mask64;
                self.rev64 = (self.rev64 >> 2) | (((3 - b) as u64) << ((self.k - 1) * 2));
                if self.seen >= self.k {
                    Some(KmerKey::U64(if self.canonical {
                        canonical_u64(self.fwd64, self.rev64)
                    } else {
                        self.fwd64
                    }))
                } else {
                    None
                }
            }
            KmerMode::U128 => {
                self.fwd128 = ((self.fwd128 << 2) | b as u128) & self.mask128;
                self.rev128 = (self.rev128 >> 2) | (((3 - b) as u128) << ((self.k - 1) * 2));
                if self.seen >= self.k {
                    Some(KmerKey::U128(if self.canonical {
                        canonical_u128(self.fwd128, self.rev128)
                    } else {
                        self.fwd128
                    }))
                } else {
                    None
                }
            }
            KmerMode::Packed(words) => {
                shift_left_insert(&mut self.fwd_words, b as u64);
                shift_right_insert_top(&mut self.rev_words, (3 - b) as u64, self.top_bit_pos);
                self.fwd_words[words - 1] &= self.top_mask;
                self.rev_words[words - 1] &= self.top_mask;
                if self.seen < self.k {
                    None
                } else if self.canonical {
                    let canonical = if cmp_words_desc(&self.fwd_words, &self.rev_words).is_gt() {
                        &self.rev_words
                    } else {
                        &self.fwd_words
                    };
                    Some(KmerKey::Packed(PackedKmer {
                        words: canonical.clone(),
                    }))
                } else {
                    Some(KmerKey::Packed(PackedKmer {
                        words: self.fwd_words.clone(),
                    }))
                }
            }
        }
    }
}

#[inline(always)]
/// 中文：把 packed 长 k-mer 左移 2 bit 并插入新碱基。
/// English: Left-shift a packed long k-mer by 2 bits and insert a new base.
fn shift_left_insert(words: &mut [u64], val2: u64) {
    let mut carry = val2 & 0b11;
    for word in words.iter_mut() {
        let next_carry = (*word >> 62) & 0b11;
        *word = (*word << 2) | carry;
        carry = next_carry;
    }
}

#[inline(always)]
/// 中文：把反向互补的 packed 长 k-mer 右移 2 bit 并在高位写入新碱基。
/// English: Right-shift the packed reverse-complement state by 2 bits and
/// insert the new base at the high end.
fn shift_right_insert_top(words: &mut [u64], val2: u64, bit_pos: usize) {
    let mut carry = 0u64;
    for word in words.iter_mut().rev() {
        let next_carry = (*word & 0b11) << 62;
        *word = (*word >> 2) | carry;
        carry = next_carry;
    }
    set_2bits(words, bit_pos, val2 & 0b11);
}

#[inline(always)]
/// 中文：在 packed 长 k-mer 的指定 bit 位置写入 2-bit 值。
/// English: Write a 2-bit value into the specified bit position of a packed
/// long k-mer.
fn set_2bits(words: &mut [u64], bit_pos: usize, val2: u64) {
    let word_idx = bit_pos / 64;
    let bit_idx = bit_pos % 64;
    if bit_idx <= 62 {
        let mask = !(0b11u64 << bit_idx);
        words[word_idx] = (words[word_idx] & mask) | (val2 << bit_idx);
    } else {
        words[word_idx] &= !(1u64 << 63);
        words[word_idx] |= (val2 & 0b1) << 63;
        words[word_idx + 1] &= !1u64;
        words[word_idx + 1] |= (val2 >> 1) & 0b1;
    }
}
