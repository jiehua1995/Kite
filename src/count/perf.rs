use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Default)]
pub struct PerfCounters {
    pub read_ns: AtomicU64,
    pub decompress_ns: AtomicU64,
    pub dispatch_ns: AtomicU64,
    pub worker_parse_ns: AtomicU64,
    pub worker_count_ns: AtomicU64,
    pub dispatched_chunks: AtomicU64,
    pub dispatched_bytes: AtomicU64,
    pub max_worker_queue_len: AtomicU64,
    pub active_merge_partitions: AtomicU64,
    pub merge_partitions: AtomicU64,
    pub merge_chunks: AtomicU64,
    pub u64_hash_flushes: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PerfSnapshot {
    pub read_ns: u64,
    pub decompress_ns: u64,
    pub dispatch_ns: u64,
    pub worker_parse_ns: u64,
    pub worker_count_ns: u64,
    pub dispatched_chunks: u64,
    pub dispatched_bytes: u64,
    pub max_worker_queue_len: u64,
    pub active_merge_partitions: u64,
    pub merge_partitions: u64,
    pub merge_chunks: u64,
    pub u64_hash_flushes: u64,
}

impl PerfCounters {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline(always)]
    pub fn add_read(&self, d: Duration) {
        self.read_ns
            .fetch_add(d.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn add_decompress(&self, d: Duration) {
        self.decompress_ns
            .fetch_add(d.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn add_dispatch(&self, d: Duration) {
        self.dispatch_ns
            .fetch_add(d.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn add_worker_parse(&self, d: Duration) {
        self.worker_parse_ns
            .fetch_add(d.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn add_worker_count(&self, d: Duration) {
        self.worker_count_ns
            .fetch_add(d.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn observe_dispatch(&self, bytes: usize, queue_len: usize) {
        self.dispatched_chunks.fetch_add(1, Ordering::Relaxed);
        self.dispatched_bytes
            .fetch_add(bytes.min(u64::MAX as usize) as u64, Ordering::Relaxed);
        self.observe_max(&self.max_worker_queue_len, queue_len as u64);
    }

    #[inline(always)]
    pub fn add_merge_inputs(&self, partitions: usize, active_partitions: usize, chunks: usize) {
        self.active_merge_partitions
            .store(active_partitions.min(u64::MAX as usize) as u64, Ordering::Relaxed);
        self.merge_partitions
            .store(partitions.min(u64::MAX as usize) as u64, Ordering::Relaxed);
        self.merge_chunks
            .store(chunks.min(u64::MAX as usize) as u64, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn inc_u64_hash_flush(&self) {
        self.u64_hash_flushes.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    fn observe_max(&self, slot: &AtomicU64, value: u64) {
        let mut current = slot.load(Ordering::Relaxed);
        while value > current {
            match slot.compare_exchange_weak(
                current,
                value,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }

    pub fn snapshot(&self) -> PerfSnapshot {
        PerfSnapshot {
            read_ns: self.read_ns.load(Ordering::Relaxed),
            decompress_ns: self.decompress_ns.load(Ordering::Relaxed),
            dispatch_ns: self.dispatch_ns.load(Ordering::Relaxed),
            worker_parse_ns: self.worker_parse_ns.load(Ordering::Relaxed),
            worker_count_ns: self.worker_count_ns.load(Ordering::Relaxed),
            dispatched_chunks: self.dispatched_chunks.load(Ordering::Relaxed),
            dispatched_bytes: self.dispatched_bytes.load(Ordering::Relaxed),
            max_worker_queue_len: self.max_worker_queue_len.load(Ordering::Relaxed),
            active_merge_partitions: self.active_merge_partitions.load(Ordering::Relaxed),
            merge_partitions: self.merge_partitions.load(Ordering::Relaxed),
            merge_chunks: self.merge_chunks.load(Ordering::Relaxed),
            u64_hash_flushes: self.u64_hash_flushes.load(Ordering::Relaxed),
        }
    }
}
