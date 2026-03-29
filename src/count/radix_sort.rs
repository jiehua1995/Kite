#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;
use rayon::prelude::*;

/// 中文：对 `u64` 数组做基数排序。输入是待排序数据和可复用 scratch，
/// 输出直接回写到 `data`。
/// English: Radix-sort a `u64` slice in place using a reusable scratch buffer.
pub fn radix_sort_u64(data: &mut [u64], scratch: &mut Vec<u64>) {
    if data.len() <= 1 {
        return;
    }
    ensure_scratch_len(scratch, data.len());
    const RADIX16_MIN_LEN: usize = 1 << 14;
    if data.len() >= RADIX16_MIN_LEN {
        radix_sort_u64_16(data, scratch);
        return;
    }
    let mut from_data = true;

    for shift in (0..64).step_by(8) {
        if from_data {
            counting_pass_u64(&data[..], &mut scratch[..data.len()], shift);
        } else {
            counting_pass_u64(&scratch[..data.len()], data, shift);
        }
        from_data = !from_data;
    }
    if !from_data {
        data.copy_from_slice(&scratch[..data.len()]);
    }
}

/// 中文：对 `u128` 数组做基数排序。输入和输出语义与 `radix_sort_u64` 相同。
/// English: Radix-sort a `u128` slice in place, using the same scratch-buffer
/// pattern as `radix_sort_u64`.
pub fn radix_sort_u128(data: &mut [u128], scratch: &mut Vec<u128>) {
    if data.len() <= 1 {
        return;
    }
    ensure_scratch_len(scratch, data.len());
    const RADIX16_MIN_LEN: usize = 1 << 14;
    if data.len() >= RADIX16_MIN_LEN {
        radix_sort_u128_16(data, scratch);
        return;
    }
    let mut from_data = true;

    for shift in (0..128).step_by(8) {
        if from_data {
            counting_pass_u128(&data[..], &mut scratch[..data.len()], shift);
        } else {
            counting_pass_u128(&scratch[..data.len()], data, shift);
        }
        from_data = !from_data;
    }
    if !from_data {
        data.copy_from_slice(&scratch[..data.len()]);
    }
}

#[inline(always)]
/// 中文：执行一次 `u64` 的 8-bit counting pass。
/// English: Run one 8-bit counting pass for `u64` keys.
fn counting_pass_u64(from: &[u64], to: &mut [u64], shift: usize) {
    // optimized: compute counts and write via raw pointers to avoid bounds checks
    let mut counts = [0usize; 256];
    // try AVX2 accelerated counting for large slices when available
    if cfg!(target_arch = "x86_64") && is_x86_feature_detected!("avx2") {
        unsafe {
            counting_pass_u64_avx2_count(from, &mut counts, shift);
        }
    } else {
        for &v in from {
            counts[((v >> shift) & 0xFF) as usize] += 1;
        }
    }
    let mut offsets = [0usize; 256];
    let mut acc = 0usize;
    for i in 0..256 {
        offsets[i] = acc;
        acc += counts[i];
    }
    let to_ptr = to.as_mut_ptr();
    for &v in from {
        let bucket = ((v >> shift) & 0xFF) as usize;
        unsafe {
            let dst = to_ptr.add(offsets[bucket]);
            std::ptr::write(dst, v);
        }
        offsets[bucket] += 1;
    }
}

#[cfg(all(target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
/// 中文：AVX2 加速版 `u64` 计数阶段，只负责统计 bucket 频数。
/// English: AVX2-accelerated counting stage for `u64`, responsible only for
/// bucket frequency accumulation.
unsafe fn counting_pass_u64_avx2_count(from: &[u64], counts: &mut [usize; 256], shift: usize) {
    let len = from.len();
    let mut i = 0usize;
    let mask = _mm256_set1_epi64x(0xFFu64 as i64);
    while i + 4 <= len {
        let ptr = unsafe { from.as_ptr().add(i) } as *const __m256i;
        let v = unsafe { _mm256_loadu_si256(ptr) };
        // logical shift right by shift bits - use constant immediates via match
        let shifted = match shift {
            0 => _mm256_srli_epi64(v, 0),
            8 => _mm256_srli_epi64(v, 8),
            16 => _mm256_srli_epi64(v, 16),
            24 => _mm256_srli_epi64(v, 24),
            32 => _mm256_srli_epi64(v, 32),
            40 => _mm256_srli_epi64(v, 40),
            48 => _mm256_srli_epi64(v, 48),
            56 => _mm256_srli_epi64(v, 56),
            _ => _mm256_srli_epi64(v, 0),
        };
        let masked = _mm256_and_si256(shifted, mask);
        // extract lanes
        let a = _mm256_extract_epi64(masked, 0) as u64;
        let b = _mm256_extract_epi64(masked, 1) as u64;
        let c = _mm256_extract_epi64(masked, 2) as u64;
        let d = _mm256_extract_epi64(masked, 3) as u64;
        counts[(a & 0xFF) as usize] += 1;
        counts[(b & 0xFF) as usize] += 1;
        counts[(c & 0xFF) as usize] += 1;
        counts[(d & 0xFF) as usize] += 1;
        i += 4;
    }
    while i < len {
        let v = unsafe { *from.get_unchecked(i) };
        counts[((v >> shift) & 0xFF) as usize] += 1;
        i += 1;
    }
}

#[inline(always)]
/// 中文：执行一次 `u128` 的 8-bit counting pass。
/// English: Run one 8-bit counting pass for `u128` keys.
fn counting_pass_u128(from: &[u128], to: &mut [u128], shift: usize) {
    let mut counts = [0usize; 256];
    for &v in from {
        counts[((v >> shift) & 0xFF) as usize] += 1;
    }
    let mut offsets = [0usize; 256];
    let mut acc = 0usize;
    for i in 0..256 {
        offsets[i] = acc;
        acc += counts[i];
    }
    let to_ptr = to.as_mut_ptr();
    for &v in from {
        let bucket = ((v >> shift) & 0xFF) as usize;
        unsafe {
            let dst = to_ptr.add(offsets[bucket]);
            std::ptr::write(dst, v);
        }
        offsets[bucket] += 1;
    }
}

// 中文：`u64` 的 16-bit 路径 AVX2 计数辅助函数，只做 bucket 统计。
// English: AVX2 helper for the 16-bit `u64` path; it only counts buckets.
#[cfg(all(target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn counting_pass_u64_16_avx2_count(from: &[u64], counts: &mut [usize], shift: usize) {
    let len = from.len();
    let mut i = 0usize;
    let mask = _mm256_set1_epi64x(0xFFFFu64 as i64);
    while i + 4 <= len {
        let ptr = unsafe { from.as_ptr().add(i) } as *const __m256i;
        let v = unsafe { _mm256_loadu_si256(ptr) };
        // shift by appropriate immediate then mask
        let shifted = match shift {
            0 => v,
            16 => _mm256_srli_epi64(v, 16),
            32 => _mm256_srli_epi64(v, 32),
            48 => _mm256_srli_epi64(v, 48),
            _ => _mm256_srli_epi64(v, 0),
        };
        let masked = _mm256_and_si256(shifted, mask);
        let a = _mm256_extract_epi64(masked, 0) as u64;
        let b = _mm256_extract_epi64(masked, 1) as u64;
        let c = _mm256_extract_epi64(masked, 2) as u64;
        let d = _mm256_extract_epi64(masked, 3) as u64;
        counts[(a & 0xFFFF) as usize] += 1;
        counts[(b & 0xFFFF) as usize] += 1;
        counts[(c & 0xFFFF) as usize] += 1;
        counts[(d & 0xFFFF) as usize] += 1;
        i += 4;
    }
    while i < len {
        let v = unsafe { *from.get_unchecked(i) };
        counts[((v >> shift) & 0xFFFF) as usize] += 1;
        i += 1;
    }
}

/// 中文：`u64` 的 16-bit 基数排序版本，减少 pass 次数，适合更大数组。
/// English: 16-bit radix-sort variant for `u64`, reducing the number of passes
/// for larger arrays.
fn radix_sort_u64_16(data: &mut [u64], scratch: &mut Vec<u64>) {
    let len = data.len();
    ensure_scratch_len(scratch, len);
    let mut from_data = true;
    // Reuse large 16-bit counts/offsets buffers across passes to avoid repeated allocations.
    let mut counts = vec![0usize; 1 << 16];
    let mut offsets = vec![0usize; 1 << 16];
    for shift in (0..64).step_by(16) {
        if from_data {
            counting_pass_u64_16_buf(
                &data[..],
                &mut scratch[..len],
                shift,
                &mut counts,
                &mut offsets,
            );
        } else {
            counting_pass_u64_16_buf(&scratch[..len], data, shift, &mut counts, &mut offsets);
        }
        from_data = !from_data;
    }
    if !from_data {
        data.copy_from_slice(&scratch[..len]);
    }
}

/// 中文：`u64` 16-bit counting pass，支持大数据并行计数与写回。
/// English: 16-bit counting pass for `u64` with optional parallel counting and
/// scatter for large inputs.
fn counting_pass_u64_16_buf(
    from: &[u64],
    to: &mut [u64],
    shift: usize,
    counts: &mut Vec<usize>,
    offsets: &mut Vec<usize>,
) {
    const PARALLEL_THRESHOLD: usize = 1 << 20; // tuneable
    let n = from.len();
    if n >= PARALLEL_THRESHOLD {
        // parallel counting: per-chunk histograms then reduce
        let nthreads = rayon::current_num_threads();
        let chunk_size = (n + nthreads - 1) / nthreads;
        let local_counts: Vec<Vec<usize>> = from
            .par_chunks(chunk_size)
            .map(|slice| {
                let mut lc = vec![0usize; 1 << 16];
                for &v in slice {
                    lc[((v >> shift) & 0xFFFF) as usize] += 1;
                }
                lc
            })
            .collect();

        // reduce into counts
        for i in 0..(1 << 16) {
            let mut s = 0usize;
            for t in 0..local_counts.len() {
                s += local_counts[t][i];
            }
            counts[i] = s;
        }

        // compute offsets
        let mut acc = 0usize;
        for i in 0..(1 << 16) {
            offsets[i] = acc;
            acc += counts[i];
        }

        // per-thread starting offsets for each bucket
        let mut thread_offsets: Vec<Vec<usize>> = vec![vec![0usize; 1 << 16]; local_counts.len()];
        for b in 0..(1 << 16) {
            let mut pos = offsets[b];
            for t in 0..local_counts.len() {
                thread_offsets[t][b] = pos;
                pos += local_counts[t][b];
            }
        }

        // parallel write: pass pointer as usize to make it Sync
        let to_ptr_addr = to.as_mut_ptr() as usize;
        from.par_chunks(chunk_size)
            .enumerate()
            .for_each(|(ti, slice)| {
                let mut local_off = thread_offsets[ti].clone();
                let to_ptr = to_ptr_addr as *mut u64;
                for &v in slice {
                    let bucket = ((v >> shift) & 0xFFFF) as usize;
                    let dst_idx = local_off[bucket];
                    unsafe {
                        let dst = to_ptr.add(dst_idx);
                        std::ptr::write(dst, v);
                    }
                    local_off[bucket] += 1;
                }
            });

        return;
    }

    // fallback serial path with light unrolling (or AVX2 accelerated if available)
    #[cfg(all(target_arch = "x86_64"))]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                counting_pass_u64_16_avx2_count(from, counts, shift);
            }
        } else {
            for c in counts.iter_mut() {
                *c = 0;
            }
            let mut i = 0;
            let len = from.len();
            while i + 4 <= len {
                counts[((from[i] >> shift) & 0xFFFF) as usize] += 1;
                counts[((from[i + 1] >> shift) & 0xFFFF) as usize] += 1;
                counts[((from[i + 2] >> shift) & 0xFFFF) as usize] += 1;
                counts[((from[i + 3] >> shift) & 0xFFFF) as usize] += 1;
                i += 4;
            }
            while i < len {
                counts[((from[i] >> shift) & 0xFFFF) as usize] += 1;
                i += 1;
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        for c in counts.iter_mut() {
            *c = 0;
        }
        let mut i = 0;
        let len = from.len();
        while i + 4 <= len {
            counts[((from[i] >> shift) & 0xFFFF) as usize] += 1;
            counts[((from[i + 1] >> shift) & 0xFFFF) as usize] += 1;
            counts[((from[i + 2] >> shift) & 0xFFFF) as usize] += 1;
            counts[((from[i + 3] >> shift) & 0xFFFF) as usize] += 1;
            i += 4;
        }
        while i < len {
            counts[((from[i] >> shift) & 0xFFFF) as usize] += 1;
            i += 1;
        }
    }

    let mut acc = 0usize;
    for i in 0..(1 << 16) {
        offsets[i] = acc;
        acc += counts[i];
    }
    let to_ptr = to.as_mut_ptr();
    for &v in from {
        let bucket = ((v >> shift) & 0xFFFF) as usize;
        unsafe {
            let dst = to_ptr.add(offsets[bucket]);
            std::ptr::write(dst, v);
        }
        offsets[bucket] += 1;
    }
}

/// 中文：`u128` 的 16-bit 基数排序版本。
/// English: 16-bit radix-sort variant for `u128`.
fn radix_sort_u128_16(data: &mut [u128], scratch: &mut Vec<u128>) {
    let len = data.len();
    ensure_scratch_len(scratch, len);
    let mut from_data = true;
    // reuse large buffers for 16-bit passes
    let mut counts = vec![0usize; 1 << 16];
    let mut offsets = vec![0usize; 1 << 16];
    for shift in (0..128).step_by(16) {
        if from_data {
            counting_pass_u128_16_buf(
                &data[..],
                &mut scratch[..len],
                shift,
                &mut counts,
                &mut offsets,
            );
        } else {
            counting_pass_u128_16_buf(&scratch[..len], data, shift, &mut counts, &mut offsets);
        }
        from_data = !from_data;
    }
    if !from_data {
        data.copy_from_slice(&scratch[..len]);
    }
}

/// 中文：`u128` 的 16-bit counting pass。
/// English: 16-bit counting pass for `u128`.
fn counting_pass_u128_16_buf(
    from: &[u128],
    to: &mut [u128],
    shift: usize,
    counts: &mut Vec<usize>,
    offsets: &mut Vec<usize>,
) {
    // reuse the u64 implementation pattern but for u128; use serial path and small unrolling
    for c in counts.iter_mut() {
        *c = 0;
    }
    let mut i = 0usize;
    let len = from.len();
    while i + 4 <= len {
        counts[((from[i] >> shift) & 0xFFFF) as usize] += 1;
        counts[((from[i + 1] >> shift) & 0xFFFF) as usize] += 1;
        counts[((from[i + 2] >> shift) & 0xFFFF) as usize] += 1;
        counts[((from[i + 3] >> shift) & 0xFFFF) as usize] += 1;
        i += 4;
    }
    while i < len {
        counts[((from[i] >> shift) & 0xFFFF) as usize] += 1;
        i += 1;
    }

    let mut acc = 0usize;
    for i in 0..(1 << 16) {
        offsets[i] = acc;
        acc += counts[i];
    }
    let to_ptr = to.as_mut_ptr();
    for &v in from {
        let bucket = ((v >> shift) & 0xFFFF) as usize;
        unsafe {
            let dst = to_ptr.add(offsets[bucket]);
            std::ptr::write(dst, v);
        }
        offsets[bucket] += 1;
    }
}

// 中文：保证 scratch 向量长度足够，同时避免没必要的零填充开销。
// English: Ensure the scratch vector has the requested length while avoiding
// unnecessary zero-filling before counting passes overwrite every element.
fn ensure_scratch_len<T: Copy>(scratch: &mut Vec<T>, len: usize) {
    if scratch.len() != len {
        if scratch.capacity() < len {
            scratch.reserve(len - scratch.capacity());
        }
        unsafe {
            scratch.set_len(len);
        }
    }
}
