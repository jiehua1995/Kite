use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Global memory budget tracker.
/// Workers register allocations/frees to track total in-flight memory.
/// When current usage exceeds the limit, flush signals are triggered.
#[derive(Debug)]
pub struct MemoryBudget {
    current: AtomicUsize,
    limit: usize,       // total byte budget
    flush_threshold: usize, // flush when current > this (90% of limit)
}

impl MemoryBudget {
    pub fn new(limit_gb: f64) -> Self {
        let limit = (limit_gb * 1024.0 * 1024.0 * 1024.0) as usize;
        let flush_threshold = (limit as f64 * 0.85) as usize;
        MemoryBudget {
            current: AtomicUsize::new(0),
            limit,
            flush_threshold,
        }
    }

    #[inline]
    pub fn alloc(&self, bytes: usize) {
        self.current.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn free(&self, bytes: usize) {
        // Saturating sub to avoid underflow from racing alloc/free
        let _ = self.current.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            Some(cur.saturating_sub(bytes))
        });
    }

    #[allow(dead_code)]
    #[inline]
    pub fn current_bytes(&self) -> usize {
        self.current.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn should_flush(&self) -> bool {
        self.current.load(Ordering::Relaxed) >= self.flush_threshold
    }

    #[allow(dead_code)]
    #[inline]
    pub fn is_over_limit(&self) -> bool {
        self.current.load(Ordering::Relaxed) >= self.limit
    }

    /// Budget per worker for their local bucket buffers (flush threshold).
    ///
    /// With the direct-write architecture (no channel), workers write sorted+RLE
    /// runs directly to per-bucket temp files.  There is no in-channel memory to
    /// account for, so we can safely use a large fraction of the total budget.
    ///
    /// 65% of total limit / n_workers: at 64 GB / 30 workers this is ~1.4 GB/worker.
    /// The remaining 35% covers BGZF decompression queue, encoding scratch buffers,
    /// allocator fragmentation, and OS cache overhead.
    pub fn worker_budget_bytes(&self, num_workers: usize) -> usize {
        let worker_total = (self.limit as f64 * 0.65) as usize;
        worker_total / num_workers.max(1)
    }

    pub fn limit(&self) -> usize { self.limit }
    #[allow(dead_code)]
    pub fn flush_threshold(&self) -> usize { self.flush_threshold }
}

pub type SharedBudget = Arc<MemoryBudget>;
