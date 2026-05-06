use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::stats::DumpStats;

pub const DUMP_SLAB_BYTES: usize = 32 * 1024 * 1024;
pub const DUMP_IO_WRITERS: usize = 8;
pub const DUMP_QUEUE_BYTES: usize = 256 * 1024 * 1024;

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
}

impl AsyncPwriteWriter {
    pub fn create(
        output_path: &str,
        total_len: u64,
        collect_timing: bool,
        stats: Arc<DumpStats>,
    ) -> io::Result<Self> {
        let file = Arc::new(File::create(output_path)?);
        file.set_len(total_len)?;

        let queue_slots = (DUMP_QUEUE_BYTES / DUMP_SLAB_BYTES).max(DUMP_IO_WRITERS);
        let (tx, rx) = bounded::<WriteJob>(queue_slots);

        let mut handles = Vec::with_capacity(DUMP_IO_WRITERS);
        for _ in 0..DUMP_IO_WRITERS {
            let file_cloned = Arc::clone(&file);
            let rx_cloned = rx.clone();
            let stats_cloned = Arc::clone(&stats);
            let handle = thread::spawn(move || worker_loop(file_cloned, rx_cloned, stats_cloned, collect_timing));
            handles.push(handle);
        }

        Ok(Self {
            sender: Some(tx),
            handles,
            stats,
            collect_timing,
            queue_slots,
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
        DUMP_IO_WRITERS
    }

    pub fn queue_bytes_limit(&self) -> usize {
        self.queue_slots * DUMP_SLAB_BYTES
    }
}

fn worker_loop(
    file: Arc<File>,
    rx: Receiver<WriteJob>,
    stats: Arc<DumpStats>,
    collect_timing: bool,
) -> io::Result<()> {
    while let Ok(job) = rx.recv() {
        let write_start = collect_timing.then(Instant::now);
        write_all_at(&file, job.offset, &job.bytes)?;
        if let Some(write_start) = write_start {
            stats.add_writer_io_ns(write_start.elapsed().as_nanos() as u64);
        }
    }
    Ok(())
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
