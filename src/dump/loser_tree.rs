#![allow(dead_code)]
/// K-way merge using a Loser Tree.
///
/// A loser tree is an internal-node tournament tree where each internal node
/// stores the *loser* (larger value) of a match, while the winner propagates
/// upward. The overall winner sits at the root as a special "winner" slot.
///
/// Operations:
/// - `new(streams)` — O(n log n) initialization
/// - `next()` — O(log n) advance
///
/// For k-mer dump: the key is u64 (canonical kmer), values are u32 counts
/// that we accumulate when keys match.
#[allow(dead_code)]
pub struct LoserTree {
    /// Current head value (key, count) for each stream, None if exhausted
    heads: Vec<Option<(u64, u32)>>,
    /// Internal loser nodes [0..n-1]; loser_nodes[n-1] is the root
    loser_nodes: Vec<usize>,
    /// Index of the current overall winner stream
    winner: usize,
    n: usize, // number of streams
}

#[allow(dead_code)]
impl LoserTree {
    /// Create a new loser tree from an array of buffered run iterators.
    /// `streams` is a Vec of boxed iterators over (u64, u32) entries.
    pub fn new(mut streams: Vec<Box<dyn Iterator<Item = (u64, u32)>>>) -> Self {
        let n = streams.len();
        if n == 0 {
            return LoserTree {
                heads: vec![],
                loser_nodes: vec![],
                winner: 0,
                n: 0,
            };
        }

        // Load first element from each stream
        let heads: Vec<Option<(u64, u32)>> = streams.iter_mut().map(|s| s.next()).collect();

        // We keep streams alive by leaking them into the heads — actually we
        // need to store them to pull more values. Store in a side vec.
        // Re-design: store streams alongside heads.
        let _ = streams; // consumed into heads loading; we don't need more items now
        // NOTE: This struct doesn't pull more items — it is designed to be used
        // with RunCursor which provides all items upfront via a slice iterator.
        // For streaming, see LoserTreeStreaming below.

        let mut lt = LoserTree {
            heads,
            loser_nodes: vec![0usize; n],
            winner: 0,
            n,
        };
        if n > 0 {
            lt.initialize();
        }
        lt
    }

    fn key_of(h: &Option<(u64, u32)>) -> u64 {
        match h {
            Some((k, _)) => *k,
            None => u64::MAX, // exhausted streams sort to the end
        }
    }

    fn initialize(&mut self) {
        // Set all internal nodes to n (sentinel = "no winner yet, use +inf")
        let n = self.n;
        for v in self.loser_nodes.iter_mut() {
            *v = n; // sentinel
        }
        // Process each leaf
        for i in 0..n {
            self.adjust(i);
        }
    }

    /// After updating heads[winner_stream], propagate upward.
    fn adjust(&mut self, leaf: usize) {
        let n = self.n;
        // Internal node index for leaf i is (i + n - 1) / something...
        // For a loser tree with n leaves, we use n-1 internal nodes.
        // Node indexing: internal nodes are 0..n-1, leaves n..2n-1 (conceptually).
        // We index the tree with: parent of node i = (i-1)/2
        // leaf i maps to internal node position starting at: (n + i) / 2 going up.
        // Standard loser tree indexing:
        // internal nodes: 1..=n (1-indexed), node n is root
        // leaves: n+1..=2n (1-indexed)
        // We'll use 0-indexed with n internal nodes (0..n), root = 0
        // leaf l's starting parent = (l + n) / 2 if using 1-indexed, but let's
        // build a cleaner version:

        // Use 1-indexed array internally for the tournament.
        // Reimplement with helper array approach.

        // This adjust function: we are inserting leaf `leaf`.
        // Key of this leaf:
        let mut key = Self::key_of(&self.heads[leaf]);
        let mut winner = leaf;

        // Walk up the tree. For a 1-indexed complete binary tree with n leaves:
        // leaves are at positions n..2n-1 (0-indexed, with n internal nodes at 0..n-1)
        // parent(i) = (i - 1) / 2
        let mut t2 = (winner + n) >> 1;
        while t2 > 0 {
            let loser_idx = self.loser_nodes[t2 - 1]; // 0-indexed internal node
            let loser_key = if loser_idx == n {
                u64::MAX // sentinel
            } else {
                Self::key_of(&self.heads[loser_idx])
            };
            if loser_key < key {
                // Current winner beats the stored loser — winner becomes new loser
                self.loser_nodes[t2 - 1] = winner;
                winner = loser_idx;
                key = loser_key;
            }
            // else current winner beats stored loser — keep stored loser, winner continues
            t2 >>= 1;
        }
        self.winner = winner;
    }

    /// Returns the next minimum (key, accumulated_count), accumulating across
    /// streams with identical keys. Returns None when all streams exhausted.
    pub fn next_accumulated(&mut self, heads: &mut Vec<Option<(u64, u32)>>) -> Option<(u64, u32)> {
        if self.n == 0 {
            return None;
        }
        loop {
            let w = self.winner;
            match heads[w] {
                None => return None, // all exhausted
                Some((key, _)) => {
                    // Accumulate while same key
                    let _total_count = 0u64;
                    // We need to pump all streams with this key — but LoserTree
                    // doesn't store stream iterators. So we use a simpler approach
                    // where the caller manages the iteration.
                    let _ = key;
                    let _ = _total_count;
                    return None; // placeholder — see LoserTreeMerger below
                }
            }
        }
    }
}

/// K-way merge using a proper loser tree (tournament tree).
///
/// A loser tree for n streams uses n_tree = next_power_of_two(n) internal nodes.
/// Each internal node stores the INDEX of the LOSER (stream with the larger key)
/// of the tournament at that level.  The overall WINNER (minimum key) is stored
/// in `self.winner`.
///
/// Initialization: build a winner tree bottom-up in O(n), deriving loser nodes
/// from the winner-tree sibling comparisons.  This avoids the broken
/// "insert-one-at-a-time" approach where earlier streams go missing.
///
/// Advance: `adjust(leaf)` walks the path from `leaf` to root — exactly one
/// comparison per level — so each element costs O(log n) compares with no
/// sibling lookup.  BinaryHeap needs O(2 log n).  For n≈23, this is ~2×
/// fewer comparisons per element.
///
/// Keys are u128 so both k≤32 (u64 promoted) and k≤64 (u128) are uniform.
/// The sentinel index is `n` with virtual key `u128::MAX`; exhausted streams
/// also report `u128::MAX`.
pub struct LoserTreeMerger {
    /// Internal loser nodes, 1-indexed: loser[1..=n_tree].
    /// loser[0] is unused padding (we use self.winner instead).
    loser: Vec<usize>,
    /// Current head (key, count) for each real stream.  None = exhausted.
    heads: Vec<Option<(u128, u32)>>,
    streams: Vec<Box<dyn Iterator<Item = (u128, u32)> + Send>>,
    /// Index of the current overall winner (minimum key stream).
    winner: usize,
    n: usize,
    n_tree: usize,
    pub comparisons: u64,
}

impl LoserTreeMerger {
    pub fn new(mut streams: Vec<Box<dyn Iterator<Item = (u128, u32)> + Send>>) -> Self {
        let n = streams.len();
        if n == 0 {
            return LoserTreeMerger {
                loser: vec![],
                heads: vec![],
                streams,
                winner: 0,
                n: 0,
                n_tree: 1,
                comparisons: 0,
            };
        }

        let n_tree = n.next_power_of_two().max(2);
        let sentinel = n; // virtual index with key = u128::MAX

        // Load first elements.
        let heads: Vec<Option<(u128, u32)>> = streams.iter_mut().map(|s| s.next()).collect();

        // key_of: returns u128::MAX for sentinel or exhausted streams.
        let key_of = |i: usize| -> u128 {
            if i >= n {
                u128::MAX
            } else {
                heads[i].map(|(k, _)| k).unwrap_or(u128::MAX)
            }
        };

        // Build winner tree bottom-up.
        // Standard 1-indexed complete binary tree with n_tree leaves:
        //   internal nodes: 1..n_tree-1  (n_tree-1 nodes)
        //   leaves:         n_tree..2*n_tree-1 (n_tree leaves)
        //   root = 1, children of t = 2t and 2t+1
        //
        // winner_at[t] = index of the winner (min key) of the subtree at node t.
        // We need indices up to 2*n_tree-1, so size = 2*n_tree.
        let mut winner_at = vec![sentinel; 2 * n_tree]; // 1-indexed, [0] unused
        for j in 0..n {
            winner_at[n_tree + j] = j; // leaves at n_tree..n_tree+n-1
        }
        // Extra leaves n_tree+n..2*n_tree-1 remain = sentinel.

        // loser[t] for t in 1..n_tree (n_tree-1 internal nodes, 1-indexed).
        let mut loser = vec![sentinel; n_tree]; // loser[0] unused, loser[1..n_tree-1] used

        // Process internal nodes from bottom up (leaves are already set above).
        for t in (1..n_tree).rev() {
            let l = winner_at[2 * t];
            let r = winner_at[2 * t + 1];
            if key_of(l) <= key_of(r) {
                loser[t] = r; // r is the loser (larger key stays at node t)
                winner_at[t] = l; // l is the winner (propagates up)
            } else {
                loser[t] = l;
                winner_at[t] = r;
            }
        }

        let winner = winner_at[1];

        LoserTreeMerger {
            loser,
            heads,
            streams,
            winner,
            n,
            n_tree,
            comparisons: 0,
        }
    }

    /// Key of stream i, or u128::MAX if sentinel / exhausted.
    #[inline(always)]
    fn key_of(&self, i: usize) -> u128 {
        if i >= self.n {
            u128::MAX
        } else {
            self.heads[i].map(|(k, _)| k).unwrap_or(u128::MAX)
        }
    }

    /// After updating `self.heads[leaf]`, walk the path from leaf to root.
    /// Exactly one comparison per level: O(log n_tree) total.
    #[inline]
    fn adjust(&mut self, leaf: usize) {
        let mut cand = leaf;
        let mut cand_key = self.key_of(leaf);
        let mut t = (self.n_tree + leaf) >> 1;
        while t >= 1 {
            let s = self.loser[t];
            let s_key = self.key_of(s);
            if s_key < cand_key {
                // Stored loser (smaller key) wins this match; current candidate loses.
                self.loser[t] = cand;
                cand = s;
                cand_key = s_key;
            }
            t >>= 1;
        }
        self.winner = cand;
    }

    /// Pull the overall minimum key with its accumulated count.
    /// Accumulates duplicate keys across all streams.
    /// Returns None when all streams are exhausted.
    pub fn next_min(&mut self) -> Option<(u128, u32)> {
        if self.n == 0 {
            return None;
        }
        // Check if winner is sentinel (all exhausted) or its head is None.
        if self.winner >= self.n {
            return None;
        }
        if self.heads[self.winner].is_none() {
            return None;
        }

        let min_key = self.heads[self.winner].unwrap().0;
        let mut total: u32 = 0;

        // Consume all streams currently at min_key (including duplicates across streams).
        loop {
            let w = self.winner;
            if w >= self.n {
                break;
            }
            match self.heads[w] {
                None => break,
                Some((k, _)) if k != min_key => break,
                Some((_, cnt)) => {
                    total = total.saturating_add(cnt);
                    self.comparisons += 1;
                    // Advance stream w and restore loser tree invariant.
                    self.heads[w] = self.streams[w].next();
                    self.adjust(w);
                }
            }
        }
        Some((min_key, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_stream(v: Vec<(u128, u32)>) -> Box<dyn Iterator<Item = (u128, u32)> + Send> {
        Box::new(v.into_iter())
    }

    #[test]
    fn test_loser_tree_basic() {
        let streams = vec![
            mk_stream(vec![(1u128, 1), (4u128, 1)]),
            mk_stream(vec![(2u128, 1), (3u128, 2)]),
            mk_stream(vec![(1u128, 2), (5u128, 1)]),
        ];
        let mut lt = LoserTreeMerger::new(streams);
        let mut result = Vec::new();
        while let Some(entry) = lt.next_min() {
            result.push(entry);
        }
        // Expected: key 1 accumulated count=3, key 2=1, key 3=2, key 4=1, key 5=1
        assert_eq!(result, vec![(1, 3), (2, 1), (3, 2), (4, 1), (5, 1)]);
    }

    #[test]
    fn test_loser_tree_single_stream() {
        let streams = vec![mk_stream(vec![(10u128, 5), (20u128, 3)])];
        let mut lt = LoserTreeMerger::new(streams);
        assert_eq!(lt.next_min(), Some((10, 5)));
        assert_eq!(lt.next_min(), Some((20, 3)));
        assert_eq!(lt.next_min(), None);
    }

    #[test]
    fn test_loser_tree_empty() {
        let streams: Vec<Box<dyn Iterator<Item = (u128, u32)> + Send>> = vec![];
        let mut lt = LoserTreeMerger::new(streams);
        assert_eq!(lt.next_min(), None);
    }

    #[test]
    fn test_loser_tree_many_streams() {
        // 100 streams each with 1 element; verify all merged in order
        let mut streams: Vec<Box<dyn Iterator<Item = (u128, u32)> + Send>> = (0u128..100)
            .rev()
            .map(|i| mk_stream(vec![(i, 1)]))
            .collect();
        // Also make some have duplicate keys
        streams.push(mk_stream(vec![(50u128, 3)]));
        let mut lt = LoserTreeMerger::new(streams);
        let mut result = Vec::new();
        while let Some(entry) = lt.next_min() {
            result.push(entry);
        }
        // 0..100 with key 50 having extra count
        assert_eq!(result.len(), 100);
        for i in 0usize..100 {
            assert_eq!(result[i].0, i as u128);
        }
        assert_eq!(result[50].1, 4); // original 1 + extra 3
    }
}
