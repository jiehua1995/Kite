use std::cmp::Ordering;
use std::collections::BinaryHeap;

// 中文：这里实现分块排序结果的多路归并，把重复 k-mer 的计数重新累加。
// English: This file implements multi-way merge over sorted chunks and
// re-accumulates counts for duplicate k-mers.
#[derive(Debug, Clone)]
pub struct ChunkU64 {
    pub kmers: Vec<u64>,
    pub counts: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct ChunkU128 {
    pub kmers: Vec<u128>,
    pub counts: Vec<u32>,
}

use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct ChunkPacked {
    pub kmers: VecDeque<Vec<u64>>,
    pub counts: VecDeque<u32>,
}

#[derive(Debug, Clone)]
struct HeapItemU64 {
    key: u64,
    count: u32,
    chunk: usize,
    pos: usize,
}

impl PartialEq for HeapItemU64 {
    /// 中文：比较两个堆元素是否代表同一个 `u64` k-mer。
    /// English: Test whether two heap items represent the same `u64` k-mer.
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for HeapItemU64 {}
impl PartialOrd for HeapItemU64 {
    /// 中文：为最小键优先的归并堆提供反向比较。
    /// English: Reverse the comparison so `BinaryHeap` acts like a min-heap
    /// over sorted `u64` keys.
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(other.key.cmp(&self.key))
    }
}
impl Ord for HeapItemU64 {
    /// 中文：为最小键优先的归并堆提供总序比较。
    /// English: Provide total ordering for the min-heap behavior on `u64` keys.
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.cmp(&self.key)
    }
}

/// 中文：归并多个已排序的 `u64` chunk，并把重复键的计数累加。
/// English: Merge several sorted `u64` chunks and accumulate counts for
/// repeated keys.
pub fn merge_chunks_u64(mut chunks: Vec<ChunkU64>) -> Vec<(u64, u64)> {
    let mut heap = BinaryHeap::new();
    for (i, c) in chunks.iter().enumerate() {
        if !c.kmers.is_empty() {
            heap.push(HeapItemU64 {
                key: c.kmers[0],
                count: c.counts[0],
                chunk: i,
                pos: 0,
            });
        }
    }

    let total_len: usize = chunks.iter().map(|c| c.kmers.len()).sum();
    let mut out = Vec::with_capacity(total_len);
    while let Some(item) = heap.pop() {
        let mut total = item.count as u64;
        let key = item.key;

        push_next_u64_owned(&mut heap, &mut chunks, item.chunk, item.pos + 1);

        while let Some(next) = heap.peek() {
            if next.key != key {
                break;
            }
            let next = heap.pop().expect("heap peeked item must exist");
            total += next.count as u64;
            push_next_u64_owned(&mut heap, &mut chunks, next.chunk, next.pos + 1);
        }

        out.push((key, total));
    }

    out
}

#[derive(Debug, Clone)]
struct HeapItemU128 {
    key: u128,
    count: u32,
    chunk: usize,
    pos: usize,
}

impl PartialEq for HeapItemU128 {
    /// 中文：比较两个堆元素是否代表同一个 `u128` k-mer。
    /// English: Test whether two heap items represent the same `u128` k-mer.
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for HeapItemU128 {}
impl PartialOrd for HeapItemU128 {
    /// 中文：为 `u128` 归并路径实现最小键优先的堆比较。
    /// English: Reverse comparison so the `u128` merge path behaves like a
    /// min-heap.
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(other.key.cmp(&self.key))
    }
}
impl Ord for HeapItemU128 {
    /// 中文：为 `u128` 归并路径实现总序比较。
    /// English: Provide total ordering for the `u128` merge min-heap.
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.cmp(&self.key)
    }
}

/// 中文：归并多个已排序的 `u128` chunk，并累加重复键的计数。
/// English: Merge several sorted `u128` chunks and accumulate repeated-key
/// counts.
pub fn merge_chunks_u128(mut chunks: Vec<ChunkU128>) -> Vec<(u128, u64)> {
    let mut heap = BinaryHeap::new();
    for (i, c) in chunks.iter().enumerate() {
        if !c.kmers.is_empty() {
            heap.push(HeapItemU128 {
                key: c.kmers[0],
                count: c.counts[0],
                chunk: i,
                pos: 0,
            });
        }
    }

    let total_len: usize = chunks.iter().map(|c| c.kmers.len()).sum();
    let mut out = Vec::with_capacity(total_len);
    while let Some(item) = heap.pop() {
        let mut total = item.count as u64;
        let key = item.key;

        push_next_u128_owned(&mut heap, &mut chunks, item.chunk, item.pos + 1);

        while let Some(next) = heap.peek() {
            if next.key != key {
                break;
            }
            let next = heap.pop().expect("heap peeked item must exist");
            total += next.count as u64;
            push_next_u128_owned(&mut heap, &mut chunks, next.chunk, next.pos + 1);
        }

        out.push((key, total));
    }

    out
}

#[derive(Debug, Clone)]
struct HeapItemPacked {
    key: Vec<u64>,
    count: u32,
    chunk: usize,
}

impl PartialEq for HeapItemPacked {
    /// 中文：比较两个 packed 堆元素是否为同一个长 k-mer。
    /// English: Test whether two packed heap items refer to the same long
    /// k-mer.
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for HeapItemPacked {}
impl PartialOrd for HeapItemPacked {
    /// 中文：为 packed 归并路径实现最小键优先堆。
    /// English: Reverse comparison so the packed merge path behaves like a
    /// min-heap.
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(other.key.cmp(&self.key))
    }
}
impl Ord for HeapItemPacked {
    /// 中文：为 packed 归并路径实现总序比较。
    /// English: Provide total ordering for the packed merge min-heap.
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.cmp(&self.key)
    }
}

/// 中文：归并多个长 k-mer chunk。这里把键 move 出 `VecDeque`，
/// 避免反复 clone 长向量。
/// English: Merge long-k-mer chunks. Keys are moved out of `VecDeque` to avoid
/// repeatedly cloning long vectors.
pub fn merge_chunks_packed(mut chunks: Vec<ChunkPacked>) -> Vec<(Vec<u64>, u64)> {
    let mut heap = BinaryHeap::new();
    for (i, c) in chunks.iter_mut().enumerate() {
        if !c.kmers.is_empty() {
            // move first kmer and count out of the chunk to avoid cloning
            let key = c.kmers.pop_front().expect("kmer exists");
            let count = c.counts.pop_front().expect("count exists");
            heap.push(HeapItemPacked {
                key,
                count,
                chunk: i,
            });
        }
    }

    let total_len: usize = chunks.iter().map(|c| c.kmers.len()).sum();
    let mut out = Vec::with_capacity(total_len);
    while let Some(item) = heap.pop() {
        let mut total = item.count as u64;
        let key = item.key;

        push_next_packed_owned(&mut heap, &mut chunks, item.chunk);

        while let Some(next) = heap.peek() {
            if next.key != key {
                break;
            }
            let next = heap.pop().expect("heap peeked item must exist");
            total += next.count as u64;
            push_next_packed_owned(&mut heap, &mut chunks, next.chunk);
        }

        out.push((key, total));
    }

    out
}

/// 中文：把某个 `u64` chunk 的下一个元素压入归并堆。
/// English: Push the next element from one `u64` chunk into the merge heap.
fn push_next_u64_owned(
    heap: &mut BinaryHeap<HeapItemU64>,
    chunks: &mut [ChunkU64],
    chunk: usize,
    pos: usize,
) {
    let c = &chunks[chunk];
    if pos < c.kmers.len() {
        heap.push(HeapItemU64 {
            key: c.kmers[pos],
            count: c.counts[pos],
            chunk,
            pos,
        });
    }
}

/// 中文：把某个 `u128` chunk 的下一个元素压入归并堆。
/// English: Push the next element from one `u128` chunk into the merge heap.
fn push_next_u128_owned(
    heap: &mut BinaryHeap<HeapItemU128>,
    chunks: &mut [ChunkU128],
    chunk: usize,
    pos: usize,
) {
    let c = &chunks[chunk];
    if pos < c.kmers.len() {
        heap.push(HeapItemU128 {
            key: c.kmers[pos],
            count: c.counts[pos],
            chunk,
            pos,
        });
    }
}

/// 中文：把某个 packed chunk 的下一个长 k-mer 压入归并堆。
/// English: Push the next long-k-mer entry from one packed chunk into the
/// merge heap.
fn push_next_packed_owned(
    heap: &mut BinaryHeap<HeapItemPacked>,
    chunks: &mut [ChunkPacked],
    chunk: usize,
) {
    let c = &mut chunks[chunk];
    if !c.kmers.is_empty() {
        let key = c.kmers.pop_front().expect("kmer exists");
        let count = c.counts.pop_front().expect("count exists");
        heap.push(HeapItemPacked { key, count, chunk });
    }
}
