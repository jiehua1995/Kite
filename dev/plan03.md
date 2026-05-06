# kite Plan 03 — 大数据集极限性能优化计划

**目标**: 在 48 线程 / 64GB 内存下，kite 的 count+dump 全面超越 KMC（k=21/31/51）
**奖励条件**: count+dump 总时间 < KMC 的 50%

**数据集**: `test/HG002_sample.fastq.gz` (3.9GB BGZF, ~30M reads @ 150bp)
**基准机器**: Intel Xeon Gold 6248R × 2 socket（48 物理核，96 超线程），256GB RAM，7.3TB NVMe

---

## 已完成的优化（v14 基线）

| 优化 | 描述 | 效果（小数据集 test.fastq.gz） |
|------|------|-------------------------------|
| 无界 channel + budget.alloc/free | 修复 sleeping bug | workers 不再 sleep |
| 真正的败者树（loser tree） | 替换 BinaryHeap | merge 快 ~2× |
| `out_buf` 预分配 to io_buf | 消除 Vec doubling | k21 dump 6.44s→2.72s |

小数据集 v14 vs KMC：
- count: kite 全面快（k21 +93%, k31 +79%, k51 +70%）
- dump: kite 全面快（k21 2.78×, k31 2.41×, k51 2.16×）

---

## 大数据集（HG002, 3.9GB）的特殊考虑

### 规模估算（k=21）
- ~30M reads × 130 bp = 3.9B bases → ~3.85B canonical k-mers
- 唯一 k-mer 数（k=21）: 约 1.5B（人类基因组有大量多拷贝）
- count 产生的 .kite 中间文件: 约 1.5B × 12B = 18GB（未压缩）
- dump 输出 TSV: 1.5B × (21+1+6+1) = 43GB

### 对比小数据集（test.fastq.gz, 85MB）
- 小数据: ~90M 唯一 k-mers 22× smaller
- 大数据 dump 时间估算: 2.72s × 22 ÷ (资源利用率) = 估算 20-60s

---

## 优化路线图（按重要性排序）

### P0: SIMD AVX2/AVX-512 k-mer 解码加速（dump 最大瓶颈）

**现状**: `decode_kmer_any` 每次处理 4 个碱基（BASE4_TABLE 查找），k=21 需要 6 次迭代
**瓶颈**: dump text format 占 78-81% 的 CPU（小数据），大数据更严重

**方案**: AVX2 版本 `decode_kmer_avx2(key: u128, k: u8, buf: &mut [u8])`
- u128 key 左对齐后，取前 16 字节放入 XMM 寄存器
- 用 `vpshufb` 做 16 路并行字节级查找：一次处理 16 个碱基
- k=21: 只需 2 轮（16+5）代替 6 轮
- k=31: 2 轮（16+15 = 对齐至 32，用 YMM）
- k=51: 4 轮（16×3+3 → 用 YMM 做 32+19）
- 预期加速: 3-5× decode，dump 总时间减少 50-60%

**实现文件**: `src/dump/decoder.rs`

```rust
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn decode_kmer_avx2(key: u128, k: u8, buf: &mut [u8]) {
    // Use _mm_shuffle_epi8 / _mm256_shuffle_epi8 for 16/32-way table lookup
    // Map 2-bit values to ASCII in parallel
}
```

### P1: 并行最终汇总（dump 第二大瓶颈）

**现状**: 256 个 /dev/shm 临时文件用串行 `copy_file_range` 依次拼接到输出
**瓶颈**: 大数据 k=21 dump 输出 43GB TSV，串行写入 @ 3.5GB/s = 12s

**方案**: `fallocate` 预分配输出文件，N 线程并行 pwrite
```
1. 统计每个 bucket 文件大小 → 计算前缀和（offset 数组）
2. fallocate(output_fd, offset=0, len=total_bytes)
3. rayon::scope: 48 线程各取若干 bucket，pread(shm) + pwrite(output)
4. 顺序保证: by offset array（bucket 0 → start, bucket 255 → end）
```
预期效果: 12s → 2-3s（并行利用多通道 NVMe 聚合写入）

**实现文件**: `src/dump/mod.rs`（write_output_parallel 函数）

### P2: io_uring 加速大文件读取（count 阶段）

**现状**: BGZF 读取用 File::read() 系统调用 + 线程池解压
**问题**: 每个 BGZF block 约 65KB，每次 read() = 1 syscall。3.9GB ÷ 65KB = 60K 次 syscalls + 内核上下文切换

**方案**: 使用 `tokio-uring` 或直接 `io_uring` crate
- 提交环形队列批量读取 BGZF blocks（128 个 SQE 一批）
- 完成后通知解压线程池
- 零拷贝: 直接将内核 DMA 到用户态 buffer，无需额外 memcpy

**预期效果**: 降低 sys call 开销，提升 NVMe 读取吞吐（当前可能未打满 NVMe 7GB/s 峰值）

**实现文件**: `src/count/bgzf_uring.rs`（新文件）

### P3: Count 阶段 —— zstd 阈值 + 大 chunk

**现状**: 5888 次 zstd 调用（每个 bucket 每次 flush）
**问题**: 对于大数据集，flush 次数 × k & 数据量 = 更多次 zstd 调用

**方案**:
1. 在 `flush_bucket()` 中增加阈值：`if data.len() < 32 * 1024 { skip_zstd }`
2. 增大 `--max-memory-gb` 以减少 flush 次数（64GB 时每 worker 可持有更大缓冲）
3. 使用 `lz4-flex` 替换 zstd（5× 更快，压缩率近似）

**实现文件**: `src/count/worker.rs`

### P4: .kite 文件格式瘦身

**现状**: k≤32 用 u64(8B) + u32(4B) = 12B/entry，zstd 压缩后约 8-10B/entry
**目标**: 比 KMC 数据库（.kmc_suf + .kmc_pre）更小

**分析**:
- KMC 数据库格式: 复杂的 radix 分区，经过 LUT 压缩，约 4-6B/kmer
- kite 当前用 zstd 压缩，compression ratio 约 1.4-2×，有效 6-8B/kmer
- 差距主要来自前缀共享（有序 k-mer 相邻差值很小）

**方案 A**: Delta encoding（增量编码）
- 在 RLE 压缩之前，对排序后的 kmer 做 delta 编码
- 相邻 kmer 差值往往只有 1-4，用 varint 编码 1-2 字节
- 搭配 zstd 效果更好（delta 后 entropy 更低）

**方案 B**: FLAG_PACKED_KEY（奇数字节存储）
- k=21: 42 bits → packed 6 bytes（而非 u64 的 8 bytes）
- k=31: 62 bits → packed 8 bytes（与 u64 相同，无效）
- k=51: 102 bits → packed 13 bytes（而非 u128 的 16 bytes）
- 需要修改 format/header.rs 增加 FLAG_PACKED_KEY 标志

**推荐**: 先实现 Delta encoding（纯内容优化，无需修改 header），预期尺寸减少 30-50%

### P5: Murmur3/MulSwap 均匀分桶

**现状**: Fibonacci 乘法哈希（`x.wrapping_mul(0x9E3779B97F4A7C15)`）
**问题**: 对于 DNA k-mer 的某些偏态分布（如 poly-A stretch），某些桶可能偏热

**分析**: 实际测试中 loser tree compares per bucket 分布是否均匀？

**方案**: 使用 MurmurHash3 finalize（avalanche step）
```rust
fn murmur3_mix64(mut h: u64) -> u64 {
    h ^= h >> 33; h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33; h = h.wrapping_mul(0xc4ceb9fe1a85ec53);
    h ^= h >> 33; h
}
```
对于高 entropy 的 k-mer 数据，Fibonacci hash 和 Murmur3 效果接近，优先级较低。

---

## 实施顺序与时间线

```
Day 1:
  [x] 清理目录
  [~] KMC baseline (后台运行)
  [ ] 写 plan03.md (本文件)
  [ ] 实现 P0: SIMD AVX2 kmer decode
  [ ] 实现 P1: 并行 final assembly
  [ ] kite baseline (用当前代码)
  [ ] benchmark record05.md

Day 2:
  [ ] 分析 large-file bottlenecks
  [ ] 实现 P2: io_uring (if needed)
  [ ] 实现 P3: count 优化 (zstd 阈值 + lz4)
  [ ] 实现 P4: delta encoding 尺寸压缩
  [ ] 迭代基准测试

Day 3+:
  [ ] 细粒度 profiling
  [ ] NUMA binding  
  [ ] hugepages (madvise HUGEPAGE)
  [ ] summary.md (if kite < 50% KMC time)
```

---

## 关键代码路径与文件对应

| 阶段 | 主要文件 | 优化点 |
|------|---------|--------|
| 读取/解压 | src/count/bgzf.rs, decompress.rs | io_uring, 并行 BGZF |
| K-mer 提取 | src/count/kmer.rs, worker.rs | SIMD 2-bit 编码, 更大 batch |
| 排序+RLE | src/count/sorter.rs, worker.rs | zstd 阈值, lz4 |
| 写 .kite | src/count/writer.rs | delta encoding, 更大 io_buf |
| 读 .kite | src/dump/reader.rs | mmap 优化 |
| Merge | src/dump/loser_tree.rs | ✅ 真败者树 (已实现) |
| 解码+格式化 | src/dump/decoder.rs | SIMD AVX2 decode |
| 汇总写出 | src/dump/mod.rs | 并行 pwrite |

---

## 成功指标

| 条件 | 状态 |
|------|------|
| kite count k21 < KMC count k21 | TBD |
| kite count k31 < KMC count k31 | TBD |
| kite count k51 < KMC count k51 | TBD |
| kite dump k21 < KMC dump k21 | TBD |
| kite dump k31 < KMC dump k31 | TBD |
| kite dump k51 < KMC dump k51 | TBD |
| kite (count+dump) k21 < 50% KMC | TBD → **10K 美元** |
