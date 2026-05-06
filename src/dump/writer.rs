use crossbeam_channel::{bounded, Receiver, Sender};

pub const DUMP_SLAB_BYTES: usize = 64 * 1024 * 1024;   // 64 MiB per slab
pub const DUMP_QUEUE_BYTES: usize = 4 * 1024 * 1024 * 1024; // 4 GiB write-ahead queue

pub fn queue_slots_for(slab_bytes: usize, queue_bytes: usize, worker_threads: usize) -> usize {
    (queue_bytes / slab_bytes.max(1)).max(worker_threads.max(1)).max(1)
}

pub struct SlabPool {
    sender: Sender<Vec<u8>>,
    receiver: Receiver<Vec<u8>>,
    slab_bytes: usize,
    max_cached_capacity: usize,
}

impl SlabPool {
    pub fn new(slab_bytes: usize, cache_slots: usize) -> Self {
        let channel_capacity = cache_slots.max(1);
        let (sender, receiver) = bounded(channel_capacity);
        let max_cached_capacity = slab_bytes.saturating_mul(2).max(slab_bytes);
        for _ in 0..cache_slots {
            let _ = sender.try_send(Vec::with_capacity(slab_bytes));
        }
        Self {
            sender,
            receiver,
            slab_bytes,
            max_cached_capacity,
        }
    }

    pub fn acquire(&self) -> Vec<u8> {
        let mut bytes = self
            .receiver
            .try_recv()
            .unwrap_or_else(|_| Vec::with_capacity(self.slab_bytes));
        bytes.clear();
        if bytes.capacity() < self.slab_bytes {
            bytes.reserve(self.slab_bytes - bytes.capacity());
        }
        bytes
    }

    pub fn release(&self, mut bytes: Vec<u8>) {
        if bytes.capacity() > self.max_cached_capacity || bytes.capacity() < self.slab_bytes {
            bytes = Vec::with_capacity(self.slab_bytes);
        } else {
            bytes.clear();
        }
        let _ = self.sender.try_send(bytes);
    }
}
