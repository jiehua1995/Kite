use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

/// Global memory budget tracker.
/// Workers register allocations/frees to track total in-flight memory.
/// When current usage exceeds the limit, flush signals are triggered.
#[derive(Debug)]
pub struct MemoryBudget {
    current: AtomicUsize,
    limit: usize,           // total byte budget
    flush_threshold: usize, // flush when current > this (90% of limit)
}

impl MemoryBudget {
    pub fn new(limit: usize) -> Self {
        let flush_threshold = limit.saturating_mul(19).checked_div(20).unwrap_or(limit);
        MemoryBudget {
            current: AtomicUsize::new(0),
            limit,
            flush_threshold,
        }
    }

    #[inline]
    pub fn try_acquire(&self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        let mut current = self.current.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return false;
            };
            if next > self.limit {
                return false;
            }
            match self.current.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    #[inline]
    pub fn acquire(&self, bytes: usize) {
        while !self.try_acquire(bytes) {
            thread::yield_now();
        }
    }

    #[inline]
    pub fn free(&self, bytes: usize) {
        // Saturating sub to avoid underflow from racing alloc/free
        let _ = self
            .current
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
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

    #[allow(dead_code)]
    pub fn flush_threshold(&self) -> usize {
        self.flush_threshold
    }

    #[inline]
    pub fn limit_bytes(&self) -> usize {
        self.limit
    }
}

pub type SharedBudget = Arc<MemoryBudget>;
