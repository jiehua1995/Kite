use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::stats::DumpStats;

pub const DUMP_SLAB_BYTES: usize = 32 * 1024 * 1024;
pub const DUMP_QUEUE_BYTES: usize = 256 * 1024 * 1024;

pub fn default_writer_threads(worker_threads: usize) -> usize {
    (worker_threads / 6).max(1)
}

pub fn queue_slots_for(slab_bytes: usize, queue_bytes: usize, writer_threads: usize) -> usize {
    (queue_bytes / slab_bytes.max(1)).max(writer_threads.max(1)).max(1)
}

pub struct DumpWriterConfig {
    pub writer_threads: usize,
    pub queue_bytes: usize,
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

pub struct WriteJob {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

pub struct AsyncPwriteWriter {
    sender: Option<Sender<WriteJob>>,
    handles: Vec<thread::JoinHandle<io::Result<()>>>,
    stats: Arc<DumpStats>,
    collect_timing: bool,
    queue_slots: usize,
    slab_bytes: usize,
    writer_threads: usize,
}

impl AsyncPwriteWriter {
    pub fn create(
        output_path: &str,
        total_len: u64,
        slab_bytes: usize,
        config: DumpWriterConfig,
        collect_timing: bool,
        stats: Arc<DumpStats>,
        slab_pool: Arc<SlabPool>,
    ) -> io::Result<Self> {
        let requested_writer_threads = config.writer_threads.max(1);
        let queue_slots = queue_slots_for(slab_bytes, config.queue_bytes, requested_writer_threads);
        let (tx, rx) = bounded::<WriteJob>(queue_slots);
        let file = Arc::new(open_output_file(output_path, total_len)?);
        let mut handles = Vec::with_capacity(requested_writer_threads);
        for _ in 0..requested_writer_threads {
            let file_cloned = Arc::clone(&file);
            let rx_cloned = rx.clone();
            let stats_cloned = Arc::clone(&stats);
            let slab_pool_cloned = Arc::clone(&slab_pool);
            let handle = thread::spawn(move || worker_loop(file_cloned, rx_cloned, stats_cloned, collect_timing, slab_pool_cloned));
            handles.push(handle);
        }

        Ok(Self {
            sender: Some(tx),
            handles,
            stats,
            collect_timing,
            queue_slots,
            slab_bytes,
            writer_threads: requested_writer_threads,
        })
    }

    pub fn submit(&self, offset: u64, bytes: Vec<u8>) -> io::Result<()> {
        let wait_start = self.collect_timing.then(Instant::now);
        self.sender
            .as_ref()
            .expect("dump async writer sender missing")
            .send(WriteJob { offset, bytes })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "dump async writer queue closed"))?;
        if let Some(wait_start) = wait_start {
            self.stats.add_writer_queue_wait_ns(wait_start.elapsed().as_nanos() as u64);
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        drop(self.sender.take());
        for handle in self.handles.drain(..) {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "dump async writer thread panicked",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn io_writers(&self) -> usize {
        self.writer_threads
    }

    pub fn queue_bytes_limit(&self) -> usize {
        self.queue_slots * self.slab_bytes
    }
}

fn worker_loop(
    file: Arc<File>,
    rx: Receiver<WriteJob>,
    stats: Arc<DumpStats>,
    collect_timing: bool,
    slab_pool: Arc<SlabPool>,
) -> io::Result<()> {
    while let Ok(job) = rx.recv() {
        let write_start = collect_timing.then(Instant::now);
        write_all_at(&file, job.offset, &job.bytes)?;
        if let Some(write_start) = write_start {
            stats.add_writer_io_ns(write_start.elapsed().as_nanos() as u64);
        }
        slab_pool.release(job.bytes);
    }
    Ok(())
}

fn open_output_file(output_path: &str, total_len: u64) -> io::Result<File> {
    let file = File::create(output_path)?;
    file.set_len(total_len)?;
    Ok(file)
}

fn write_all_at(file: &File, offset: u64, data: &[u8]) -> io::Result<()> {
    let mut written = 0usize;
    while written < data.len() {
        let n = file.write_at(&data[written..], offset + written as u64)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "dump write_at returned 0"));
        }
        written += n;
    }
    Ok(())
}
