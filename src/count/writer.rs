use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;

use crossbeam_channel::{bounded, Sender};

use crate::count::memory::SharedBudget;
use crate::format::{ChunkIndexEntry, KiteHeader, Kx2Footer, PartitionEntry};

#[derive(Debug, Clone)]
pub struct ChunkMeta {
    pub file_offset: u64,
    pub row_count: u64,
    pub codec: u32,
    pub encoded_bytes: u64,
    pub raw_bytes: u64,
    pub key_min_lo: u128,
    pub key_min_hi: u128,
    pub key_max_lo: u128,
    pub key_max_hi: u128,
}

#[derive(Debug, Clone)]
struct PendingChunkMeta {
    relative_offset: u64,
    row_count: u64,
    codec: u32,
    encoded_bytes: u64,
    raw_bytes: u64,
    key_min_lo: u128,
    key_min_hi: u128,
    key_max_lo: u128,
    key_max_hi: u128,
}

#[derive(Default)]
struct PartitionStaging {
    buffer: Vec<u8>,
    pending_chunks: Vec<PendingChunkMeta>,
    committed_chunks: Vec<ChunkMeta>,
    capacity_reserved: bool,
}

// Async writer thread design:
// Workers encode partition data into staging buffers, then enqueue them to a
// bounded crossbeam channel in FIFO order (offset and channel-push are done
// atomically under a short-lived mutex).  A dedicated writer thread drains
// the channel and writes data sequentially to a BufWriter<File>.
// The mutex hold time is O(1) (one atomic increment + one pointer copy into
// the channel), so 23 workers never convoy behind a long OS write.
struct KiteFileState {
    next_offset: u64,
    write_tx: Option<Sender<Vec<u8>>>,  // taken (set to None) during finalize
}

pub struct ConcurrentKiteWriter {
    file_state: Mutex<KiteFileState>,
    /// Clone of write_tx for cheap is_full() pre-checks outside the mutex.
    /// Set to None during finalize to close the channel.
    write_tx_probe: Option<Sender<Vec<u8>>>,
    write_thread: Option<JoinHandle<io::Result<File>>>,
    budget: SharedBudget,
    k: u8,
    canonical: bool,
    partitions: usize,
    key_width: u16,
    codec_mask: u16,
    staging_bytes: usize,
    staged_high_watermark: usize,
    staged_low_watermark: usize,
    staged_bytes: AtomicUsize,
    partition_staging: Vec<Mutex<PartitionStaging>>,
}

impl ConcurrentKiteWriter {
    /// Kept for API compatibility; write-lane count is ignored in the new
    /// sequential-writer design but the caller still passes it.
    pub fn suggest_write_lanes(n_workers: usize) -> usize {
        n_workers.div_ceil(8).clamp(2, 6)
    }

    pub fn create(
        output_path: &str,
        budget: SharedBudget,
        k: u8,
        canonical: bool,
        partitions: usize,
        key_width: u16,
        codec_mask: u16,
        staging_bytes: usize,
        write_lane_count: usize,
    ) -> io::Result<Self> {
        let mut file = File::create(output_path)?;
        let header = KiteHeader::new(k, canonical, partitions as u32, key_width, codec_mask);
        let mut header_buf = vec![0u8; KiteHeader::SIZE];
        {
            let mut cursor = std::io::Cursor::new(&mut header_buf[..]);
            header.write_to(&mut cursor)?;
        }
        file.write_all(&header_buf)?;

        // Channel capacity: enough slots to smooth brief compute bursts.
        // Each slot holds one staging flush (~staging_bytes of data).
        let channel_cap = (write_lane_count * 8).max(32);
        let (write_tx, write_rx) = bounded::<Vec<u8>>(channel_cap);
        let write_tx_probe = write_tx.clone();

        // Spawn dedicated writer thread: drains channel, writes sequentially.
        let buf_capacity = staging_bytes.saturating_mul(4).max(64 * 1024 * 1024);
        let write_thread = std::thread::spawn(move || -> io::Result<File> {
            let mut bw = BufWriter::with_capacity(buf_capacity, file);
            while let Ok(data) = write_rx.recv() {
                bw.write_all(&data)?;
            }
            bw.flush()?;
            bw.into_inner().map_err(|e| e.into_error())
        });

        let high_watermark = staging_bytes.saturating_mul(write_lane_count.saturating_mul(4)).max(staging_bytes);
        let low_watermark = high_watermark / 2;

        Ok(Self {
            file_state: Mutex::new(KiteFileState {
                next_offset: KiteHeader::SIZE as u64,
                write_tx: Some(write_tx),
            }),
            write_tx_probe: Some(write_tx_probe),
            write_thread: Some(write_thread),
            budget,
            k,
            canonical,
            partitions,
            key_width,
            codec_mask,
            staging_bytes,
            staged_high_watermark: high_watermark,
            staged_low_watermark: low_watermark,
            staged_bytes: AtomicUsize::new(0),
            partition_staging: (0..partitions)
                .map(|_| Mutex::new(PartitionStaging::default()))
                .collect(),
        })
    }

    #[inline]
    pub fn current_staged_bytes(&self) -> usize {
        self.staged_bytes.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn needs_runtime_drain(&self) -> bool {
        self.current_staged_bytes() >= self.staged_high_watermark
    }

    fn reserve_partition_capacity(&self, staging: &mut PartitionStaging) {
        if !staging.capacity_reserved {
            staging.buffer.reserve_exact(self.staging_bytes);
            staging.capacity_reserved = true;
            self.budget.alloc(self.staging_bytes);
        }
    }

    fn flush_partition_locked(
        &self,
        _partition_id: usize,
        staging: &mut PartitionStaging,
    ) -> io::Result<()> {
        if staging.buffer.is_empty() {
            return Ok(());
        }

        let flush_len = staging.buffer.len();
        // Move data out of staging buffer; restore on channel-full (see below).
        let mut data = std::mem::take(&mut staging.buffer);

        // Claim sequential file offset and enqueue to the async writer thread.
        // The mutex protects the atomic (offset-claim + channel-send) pair so
        // FIFO send order matches the claimed offsets.
        // If the channel is full we spin-yield OUTSIDE the mutex to avoid a
        // mutex convoy while the writer thread drains.
        let offset = loop {
            // Pre-check without the mutex: avoids a futex round-trip when the
            // channel is saturated and the writer thread is still catching up.
            while self.write_tx_probe.as_ref().map_or(false, |p| p.is_full()) {
                std::thread::yield_now();
            }

            let mut fs = self.file_state.lock().unwrap();
            // Evaluate try_send in a sub-block so the borrow of `fs` through
            // `tx` ends before we mutate `fs.next_offset` below.
            let send_result = {
                let tx = fs.write_tx.as_ref()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "writer finalized"))?;
                tx.try_send(data)
            };
            match send_result {
                Ok(()) => {
                    let off = fs.next_offset;
                    fs.next_offset += flush_len as u64;
                    break off;
                }
                Err(crossbeam_channel::TrySendError::Full(returned)) => {
                    // Lost the is_full() vs lock race; retry outside the mutex.
                    data = returned;
                    drop(fs);
                    std::thread::yield_now();
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "writer thread exited unexpectedly",
                    ));
                }
            }
        };

        self.staged_bytes.fetch_sub(flush_len, Ordering::Relaxed);

        for pending in staging.pending_chunks.drain(..) {
            staging.committed_chunks.push(ChunkMeta {
                file_offset: offset + pending.relative_offset,
                row_count: pending.row_count,
                codec: pending.codec,
                encoded_bytes: pending.encoded_bytes,
                raw_bytes: pending.raw_bytes,
                key_min_lo: pending.key_min_lo,
                key_min_hi: pending.key_min_hi,
                key_max_lo: pending.key_max_lo,
                key_max_hi: pending.key_max_hi,
            });
        }
        // staging.buffer was already taken via mem::take above.
        Ok(())
    }

    pub fn drain_one_partition(&self) -> io::Result<bool> {
        let mut best_partition = None;
        let mut best_bytes = 0usize;

        for partition_id in 0..self.partitions {
            let Ok(staging) = self.partition_staging[partition_id].try_lock() else {
                continue;
            };
            let staged = staging.buffer.len();
            if staged > best_bytes {
                best_bytes = staged;
                best_partition = Some(partition_id);
            }
        }

        let Some(partition_id) = best_partition else {
            return Ok(false);
        };

        let mut staging = self.partition_staging[partition_id].lock().unwrap();
        if staging.buffer.is_empty() {
            return Ok(false);
        }
        self.flush_partition_locked(partition_id, &mut staging)?;
        Ok(true)
    }

    pub fn drain_excess_staging(&self) -> io::Result<bool> {
        let mut drained_any = false;
        // Drain up to a fixed number of partitions per call.
        // With the sequential writer one drain at a time is fine.
        let max_attempts = 4usize;

        while self.current_staged_bytes() > self.staged_low_watermark && {
            let attempts_left = max_attempts.saturating_sub(if drained_any { 1 } else { 0 });
            attempts_left > 0
        } {
            if !self.drain_one_partition()? {
                break;
            }
            drained_any = true;
        }

        Ok(drained_any)
    }

    pub fn append_chunk(
        &self,
        partition_id: usize,
        codec: u32,
        row_count: u64,
        raw_bytes: u64,
        key_min_lo: u128,
        key_min_hi: u128,
        key_max_lo: u128,
        key_max_hi: u128,
        data: &[u8],
    ) -> io::Result<()> {
        {
            let mut staging = self.partition_staging[partition_id].lock().unwrap();
            self.reserve_partition_capacity(&mut staging);

            if !staging.buffer.is_empty() && staging.buffer.len() + data.len() > self.staging_bytes {
                self.flush_partition_locked(partition_id, &mut staging)?;
            }

            if data.len() >= self.staging_bytes {
                // Large chunk: enqueue directly to the async writer thread,
                // bypassing the staging buffer.
                let chunk_len = data.len() as u64;
                let mut owned = data.to_vec();
                let offset = loop {
                    while self.write_tx_probe.as_ref().map_or(false, |p| p.is_full()) {
                        std::thread::yield_now();
                    }
                    let mut fs = self.file_state.lock().unwrap();
                    let send_result = {
                        let tx = fs.write_tx.as_ref()
                            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "writer finalized"))?;
                        tx.try_send(owned)
                    };
                    match send_result {
                        Ok(()) => {
                            let off = fs.next_offset;
                            fs.next_offset += chunk_len;
                            break off;
                        }
                        Err(crossbeam_channel::TrySendError::Full(returned)) => {
                            owned = returned;
                            drop(fs);
                            std::thread::yield_now();
                        }
                        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                            return Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "writer thread exited unexpectedly",
                            ));
                        }
                    }
                };
                staging.committed_chunks.push(ChunkMeta {
                    file_offset: offset,
                    row_count,
                    codec,
                    encoded_bytes: chunk_len,
                    raw_bytes,
                    key_min_lo,
                    key_min_hi,
                    key_max_lo,
                    key_max_hi,
                });
                return Ok(());
            }

            let relative_offset = staging.buffer.len() as u64;
            staging.buffer.extend_from_slice(data);
            staging.pending_chunks.push(PendingChunkMeta {
                relative_offset,
                row_count,
                codec,
                encoded_bytes: data.len() as u64,
                raw_bytes,
                key_min_lo,
                key_min_hi,
                key_max_lo,
                key_max_hi,
            });
            self.staged_bytes.fetch_add(data.len(), Ordering::Relaxed);

            if staging.buffer.len() >= self.staging_bytes {
                self.flush_partition_locked(partition_id, &mut staging)?;
            }
        }

        if self.needs_runtime_drain() {
            let _ = self.drain_excess_staging()?;
        }

        Ok(())
    }

    pub fn finalize(&mut self) -> io::Result<FinalizeSummary> {
        let mut all_chunks: Vec<Vec<ChunkMeta>> = Vec::with_capacity(self.partitions);
        for (partition_id, partition) in self.partition_staging.iter().enumerate() {
            let mut staging = partition.lock().unwrap();
            self.flush_partition_locked(partition_id, &mut staging)?;
            let mut chunks = std::mem::take(&mut staging.committed_chunks);
            chunks.sort_unstable_by_key(|chunk| chunk.file_offset);
            all_chunks.push(chunks);
        }

        // Drop BOTH sender clones so the writer channel is fully closed and
        // the writer thread's recv() loop exits cleanly.
        let _ = self.write_tx_probe.take();  // drop probe clone
        let data_end = {
            let mut fs = self.file_state.lock().unwrap();
            let data_end = fs.next_offset;
            let _ = fs.write_tx.take(); // drop in-mutex clone → channel fully closed
            data_end
        };

        // Wait for writer thread to flush and return the underlying File.
        let raw_file_result = self
            .write_thread
            .take()
            .expect("write thread missing")
            .join()
            .expect("writer thread panicked");
        let mut raw_file = raw_file_result?;

        let total_chunks: u64 = all_chunks.iter().map(|chunks| chunks.len() as u64).sum();
        let _total_rows: u64 = all_chunks
            .iter()
            .map(|chunks| chunks.iter().map(|chunk| chunk.row_count).sum::<u64>())
            .sum();

        let chunk_index_offset = data_end;
        let partition_table_offset = chunk_index_offset + total_chunks * ChunkIndexEntry::SIZE as u64;
        let footer_offset = partition_table_offset + self.partitions as u64 * PartitionEntry::SIZE as u64;
        let _total_size = footer_offset + Kx2Footer::SIZE as u64;
        let total_rows: u64 = all_chunks
            .iter()
            .map(|chunks| chunks.iter().map(|chunk| chunk.row_count).sum::<u64>())
            .sum();

        let chunk_index_offset = data_end;
        let partition_table_offset = chunk_index_offset + total_chunks * ChunkIndexEntry::SIZE as u64;
        let footer_offset = partition_table_offset + self.partitions as u64 * PartitionEntry::SIZE as u64;
        let total_size = footer_offset + Kx2Footer::SIZE as u64;

        let mut trailer = Vec::with_capacity((total_size - data_end) as usize + 4096);
        let mut partition_entries = Vec::with_capacity(self.partitions);
        let mut hottest_partition_segments = 0u64;
        let mut least_partition_segments = u64::MAX;
        let mut first_chunk_idx = 0u64;

        for chunks in &all_chunks {
            let chunk_count = chunks.len() as u64;
            let total_rows_partition = chunks.iter().map(|chunk| chunk.row_count).sum::<u64>();
            let data_offset = chunks.iter().map(|chunk| chunk.file_offset).min().unwrap_or(0);
            let data_bytes = chunks.iter().map(|chunk| chunk.encoded_bytes).sum::<u64>();
            hottest_partition_segments = hottest_partition_segments.max(chunk_count);
            least_partition_segments = least_partition_segments.min(chunk_count);

            partition_entries.push(PartitionEntry {
                first_chunk_idx,
                chunk_count,
                total_rows: total_rows_partition,
                data_offset,
                data_bytes,
            });
            first_chunk_idx += chunk_count;
        }

        for (partition_id, chunks) in all_chunks.iter().enumerate() {
            for chunk in chunks {
                let mut buf = vec![0u8; ChunkIndexEntry::SIZE];
                let mut cursor = std::io::Cursor::new(&mut buf[..]);
                ChunkIndexEntry {
                    partition_id: partition_id as u32,
                    codec: chunk.codec,
                    file_offset: chunk.file_offset,
                    encoded_bytes: chunk.encoded_bytes,
                    raw_bytes: chunk.raw_bytes,
                    row_count: chunk.row_count,
                    key_min_lo: chunk.key_min_lo,
                    key_min_hi: chunk.key_min_hi,
                    key_max_lo: chunk.key_max_lo,
                    key_max_hi: chunk.key_max_hi,
                }
                .write_to(&mut cursor)?;
                trailer.extend_from_slice(&buf);
            }
        }

        for partition in &partition_entries {
            let mut buf = vec![0u8; PartitionEntry::SIZE];
            let mut cursor = std::io::Cursor::new(&mut buf[..]);
            partition.write_to(&mut cursor)?;
            trailer.extend_from_slice(&buf);
        }

        let mut footer_buf = vec![0u8; Kx2Footer::SIZE];
        {
            let mut cursor = std::io::Cursor::new(&mut footer_buf[..]);
            Kx2Footer::new(total_chunks, total_rows).write_to(&mut cursor)?;
        }
        trailer.extend_from_slice(&footer_buf);

        // raw_file is already obtained from the writer thread above.

        raw_file.seek(SeekFrom::Start(data_end))?;
        raw_file.write_all(&trailer)?;

        let mut header_buf = vec![0u8; KiteHeader::SIZE];
        {
            let mut cursor = std::io::Cursor::new(&mut header_buf[..]);
            KiteHeader {
                magic: crate::format::header::KITE_MAGIC,
                version: crate::format::header::KITE_VERSION,
                k: self.k,
                canonical: self.canonical as u8,
                partitions: self.partitions as u32,
                key_width: self.key_width,
                codec_mask: self.codec_mask,
                checksum_mode: 0,
                chunk_index_offset,
                partition_table_offset,
                footer_offset,
                total_chunks,
                total_rows,
            }
            .write_to(&mut cursor)?;
        }
        raw_file.seek(SeekFrom::Start(0))?;
        raw_file.write_all(&header_buf)?;
        raw_file.set_len(total_size)?;
        raw_file.sync_all()?;

        Ok(FinalizeSummary {
            total_chunks,
            total_rows,
            data_end,
            hottest_partition_segments,
            least_partition_segments: if self.partitions == 0 { 0 } else { least_partition_segments },
        })
    }
}

pub struct FinalizeSummary {
    pub total_chunks: u64,
    pub total_rows: u64,
    pub data_end: u64,
    pub hottest_partition_segments: u64,
    pub least_partition_segments: u64,
}