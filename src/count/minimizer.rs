#[derive(Debug, Clone, Copy)]
/// 中文：双端队列里的一个 minimizer 候选，记录值和它对应窗口的结束位置。
/// English: One minimizer candidate stored in the deque, including the value
/// and the window end position where it was observed.
struct MinEntry {
    value: u64,
    end_pos: usize,
}

/// 中文：`u64` 版本的滚动 minimizer 计算器。
/// 输入是一串已经编码成 2-bit 的碱基；输出是每个完整 k-mer 对应的 minimizer。
/// English: Rolling minimizer calculator for the `u64` path. It consumes
/// 2-bit encoded bases and emits the minimizer for each complete k-mer.
pub struct RollingMinimizerU64 {
    k: usize,
    m: usize,
    seen: usize,
    pos: usize,
    mask: u64,
    forward: u64,
    reverse: u64,
    deque: Vec<MinEntry>,
    head: usize,
}

impl RollingMinimizerU64 {
    /// 中文：创建一个新的滚动 minimizer 状态机。
    /// 输入是 `k` 和 `m`；输出是可重复使用的最小化子计算器。
    /// English: Create a new rolling minimizer state machine from `k` and `m`.
    pub fn new(k: usize, m: usize) -> Self {
        let window = k.saturating_sub(m) + 1;
        Self {
            k,
            m,
            seen: 0,
            pos: 0,
            mask: if m == 32 {
                u64::MAX
            } else {
                (1u64 << (2 * m)) - 1
            },
            forward: 0,
            reverse: 0,
            deque: Vec::with_capacity(window.max(8)),
            head: 0,
        }
    }

    #[inline(always)]
    /// 中文：重置滚动状态，供下一条 read 复用。
    /// English: Reset the rolling state so it can be reused for the next read.
    pub fn reset(&mut self) {
        self.seen = 0;
        self.pos = 0;
        self.forward = 0;
        self.reverse = 0;
        self.deque.clear();
        self.head = 0;
    }

    #[inline(always)]
    /// 中文：推进一个新的 2-bit 碱基，并在形成完整 k-mer 后返回对应 minimizer。
    /// English: Push one new 2-bit base and, once a full k-mer exists, return
    /// the minimizer for that window.
    pub fn push_encoded(&mut self, b: u8) -> Option<u64> {
        self.forward = ((self.forward << 2) | b as u64) & self.mask;
        self.reverse = (self.reverse >> 2) | (((3 - b) as u64) << ((self.m - 1) * 2));

        self.seen += 1;
        let end_pos = self.pos;
        self.pos += 1;

        if self.seen >= self.m {
            let candidate = self.forward.min(self.reverse);
            while self.deque.len() > self.head {
                let back = self.deque[self.deque.len() - 1];
                if back.value <= candidate {
                    break;
                }
                self.deque.pop();
            }
            self.deque.push(MinEntry {
                value: candidate,
                end_pos,
            });
        }

        if self.seen < self.k {
            return None;
        }

        let min_end_pos = end_pos.saturating_sub(self.k - self.m);
        while self.deque.len() > self.head && self.deque[self.head].end_pos < min_end_pos {
            self.head += 1;
        }

        if self.head > 1024 && self.head * 2 > self.deque.len() {
            self.deque.drain(0..self.head);
            self.head = 0;
        }

        self.deque.get(self.head).map(|entry| entry.value)
    }
}

/// 中文：把连续 minimizer 相同的 k-mer 聚成一个 super-k-mer 批次，
/// 减少分桶和写盘时的碎片化。
/// English: Group consecutive k-mers with the same minimizer into one
/// super-k-mer batch, reducing fragmentation during partitioning and flushing.
pub struct SuperKmerBuilder<K> {
    current_minimizer: Option<u64>,
    current: Vec<K>,
}

impl<K> SuperKmerBuilder<K> {
    /// 中文：按给定容量创建 super-k-mer 构造器。
    /// English: Create a super-k-mer builder with the requested capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            current_minimizer: None,
            current: Vec::with_capacity(cap),
        }
    }

    #[inline(always)]
    /// 中文：推入一个 `(minimizer, kmer)`。如果 minimizer 变化，就把上一批交给回调。
    /// English: Push one `(minimizer, kmer)` pair. When the minimizer changes,
    /// emit the previous batch through the callback.
    pub fn push<F>(&mut self, minimizer: u64, kmer: K, mut emit: F)
    where
        F: FnMut(u64, &mut Vec<K>),
    {
        match self.current_minimizer {
            Some(current_minimizer) if current_minimizer == minimizer => self.current.push(kmer),
            Some(current_minimizer) => {
                emit(current_minimizer, &mut self.current);
                self.current_minimizer = Some(minimizer);
                self.current.push(kmer);
            }
            None => {
                self.current_minimizer = Some(minimizer);
                self.current.push(kmer);
            }
        }
    }

    /// 中文：强制输出最后尚未提交的 super-k-mer 批次。
    /// English: Flush the final pending super-k-mer batch.
    pub fn finish<F>(&mut self, mut emit: F)
    where
        F: FnMut(u64, &mut Vec<K>),
    {
        if let Some(minimizer) = self.current_minimizer.take() {
            emit(minimizer, &mut self.current);
        }
    }
}
