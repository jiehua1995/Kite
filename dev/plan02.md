# kite 性能优化计划 v3 (2026-04-03)

## 目标

**count**: wall time < KMC/2，即 k21/k31 < 35s，k51 不超时  
**dump**: wall time < KMC kmc_dump

---

## 最新基准测试结果概要（2026-04-03 HG002_sample.fastq.gz）

### KMC (v3.2.4, -fq -k{K} -t48 -m64 -ci1 -cs4294967295)

| k  | Count Wall | Peak RSS | 1st Stage | 2nd Stage | Unique k-mers  | Total k-mers   |
|----|-----------|----------|-----------|-----------|----------------|----------------|
| 21 | 1:11.13   | 51.9 GB  | 47.25s    | 23.77s    | 2,711,890,002  | 9,009,987,754  |
| 31 | 1:18.15   | 57.7 GB  | 43.07s    | 34.97s    | 3,101,811,337  | 8,341,582,924  |
| 51 | 10:13.95  | 56.7 GB  | (多趟外排) | (慢)      | ~3,333,065,244 | ~7,015,489,985 |

> ⚠ KMC k=51 极慢原因：3.33B unique × 51B = ~170 GB > 64 GB 内存限制 → Stage 2 需 3 趟外排
> KMC dump 耗时：k=21 ~277s, k=31 ~778s（含 I/O 竞争）, k=51 尚测中

### kite 旧版本 (v12, 未修复 run_queue_depth, LSB sort)

| k  | Wall Time | Peak RSS | Sort CPU | 状态   |
|----|-----------|----------|----------|--------|
| 21 | 74.7s     | 120.8 GB | 369s     | ✓      |
| 31 | 80.9s     | 115.3 GB | (similar)| ✓      |
| 51 | 被 kill   | >172 GB  | —        | OOM/死锁 |

---

## v13 本轮已实施优化（2026-04-03）

### F1 — 修复 run_queue_depth 内存 OOM (src/count/mod.rs)

**问题**: `run_queue_depth = 64 × n_workers = 1920` → 通道内最多堆积 1920 × 7.6MB = ~14.7 GB 未追踪内存  
**修复**: 按内存预算动态计算，限制在总限额的 4%：
```rust
let chan_budget = (budget.limit() * 0.04) as usize;  // 64GB × 4% = 2.56 GB
let avg_run_bytes = (worker_budget / partitions / byte_size) * entry_size;
run_queue_depth = min((chan_budget / avg_run_bytes), 512).max(n_workers * 4);
// k=21@64GB: depth ≈ 341, 通道内存 ≈ 2.6 GB (原来 14.7 GB)
```
**预期收益**: k=51 不再 OOM；整体 RSS 下降 12-15 GB

### F2 — Vec 容量收缩防止 RSS 膨胀 (src/count/worker.rs)

**问题**: Vec::push() 容量翻倍增长 + clear() 后不释放 → 256 桶 × 30 worker × 超额容量 = 大量浪费 RSS  
**修复**: flush_bucket 后调用 `buf.shrink_to(expected_cap)`，将每个桶收缩回预期填充量  
**预期收益**: RSS 每 worker 节省约 1.5× 过额容量（约 20-40 GB 总计）

### O1 — MSD 基数排序替换 LSD (src/count/sorter.rs)

**原理**: LSD 排序需 n_passes × 2n 内存读写（k=21: 6 趟 × 2 × 35M × 8B = 3.36 GB/桶）  
MSD 排序仅需 2 趟到缓存友好小数组（level 0+1 = 2×840MB，然后 comparison sort 在 L1 cache）  
**实现**: 
- `msd_radix_sort_u64(data, byte_hi, 0)` — 从最高字节递归，阈值 1024 项降为 `sort_unstable()`  
- `msd_radix_sort_u128(data, byte_hi, 0)` — 同上用于 k=33..64  
- 大数组（>128K 项）走 MSD；小数组走原 LSD+AVX512 路径  
- 每级复用 thread-local scratch buffer，零额外分配
**理论收益**: k=21 sort: 3.36→1.68 GB/桶内存带宽，约 **2× 提速**  
**依赖**: 保持与 LSD 完全相同的测试覆盖（43 tests pass ✓）

### O2 — 软件预取 (src/count/worker.rs)

当 k-mer 路由到桶时，使用 `_mm_prefetch(..., _MM_HINT_T0)` 提前把该桶的写指针预取到 L1 cache  
**预期收益**: 随机桶访问的 cache miss 延迟降低约 10-15%

### O3 — 64 MB 写缓冲 (src/cli.rs)

写缓冲从默认 16 MB 提升到 64 MB，减少 syscall 频率约 4×  
**预期收益**: 微小，约 2-3%

---

## 预期新 kite 基准（v13, 待实测）

基于以上优化的理论收益估算：

| k  | 旧 Wall  | 预期新 Wall | 变化  | 与 KMC 比较   |
|----|---------|-----------|-------|-------------|
| 21 | 74.7s   | ~45-55s   | -30%  | 低于 71s → 胜 |
| 31 | 80.9s   | ~50-60s   | -30%  | 低于 78s → 胜 |
| 51 | OOM     | ~60-90s   | 修复  | 低于 614s → 大胜 |

> MSD sort 是最大变量：若 k=21 sort wall 从 ~12s 降到 ~6s → wall 从 74.7s → ~62s (15%)  
> 主要剩余瓶颈：写时间 (~47s @ 1 GB/s NVMe)，实际排序加速约可节省 12-15s

---

## 下一步优化（未实施，优先级排序）

### N1 — 动态去重 Hash Table (最高 ROI)

**问题**: 当前 sort+RLE 对每次 flush 内的重复有效，但 cross-flush 重复（同 bucket 多次刷出的相同 k-mer）会在 .kite 文件中存重复条目  
**实测**: kite_k21.kite = 51.8 GB vs 理论最小 2.71B × 12B = 32.5 GB → 1.6× 冗余  
**方案**: per-worker per-bucket 的 FxHashMap<K, u32>，固定容量限额
- 容量选择：每 bucket 256K 条目（u64: 4MB, u128: 6.4MB）× 256 × 30 = 30-49 GB!!
  → 过大，需更小: 16K 条目 × 256 × 30 = 1.9-3 GB
  → 缺点：频繁 drain → 更多 runs → sort 减少但 runs/flushes 增加
- 更好方案：使用 Cuckoo hash with 4K 小槽 (64KB per map × 256 × 30 = 5 GB 可接受)  
  或固定 open-addressing hash table 用整除寻址（无 Vec overhead）

**关注点**: 随机 k-mer hash miss 带来的 DRAM 延迟 vs 减少 sort 的收益  
**当前预期 ROI**: 如果 sort 占 12s wall，减少 1.6× → 7.5s；写入量减少 1.6× → 降低 ~12s。总收益约 15-20s。

### N2 — RADULS 风格并行 MSD (中优先级)

在 rayon worker pool 中对超大桶(>8M)并行 MSD 分区  
**当前 MSD**: 单线程递归  
**改进**: 并行化第一级分区（256 个子数组并行从 level 1 开始排序）  
适合最热端桶（bucket 0: 2.7亿条目 = 7.6× 平均值）

### N3 — io_uring 写 (中优先级)

使用 `io-uring = "0.5"` (Linux 5.4 基本支持) 取代 BufWriter  
**注意**: kernel 5.4 不支持 IOSQE_WRITE_FIXED (registered buffers，需 5.6+)  
基本 async writes 可用但 vs 64MB BufWriter 提升有限  
**ROI**: 低，写速度受 NVMe 1GB/s 物理限制

### N4 — NUMA-aware 工作线程绑定 (低优先级)

96 核 = 2 NUMA 节点各 48 核  
当前 rayon 线程池可能跨 NUMA 访问 → 额外 ~20ns per access  
`libc::sched_setaffinity` 绑定 worker 到同 NUMA 的核  
**ROI**: 低（随机桶访问主导，NUMA 本地化收益约 5-10%）

### N5 — Minimizer/Super-K-mer 路由 (高 ROI 但复杂)

KMC3 核心技术：按 canonical minimizer 路由 k-mer，聚合相同 minimizer 的 k-mer  
- 减少 sort 输入约 10-15× (vs 当前的 bucket 内部 RLE 仅约 2×)  
- 实现复杂：需维护 minimizer 窗口、处理边界情况  
**ROI**: 解决 write 瓶颈（输出下降约 10×，write 从 47s → 4.7s）

---

## 已实施功能总览（累计到 v13）

| 版本 | 优化项 | 收益 |
|------|------|------|
| v1-v4 | 基础框架、FASTQ 解析、Fibonacci hash 路由 | — |
| v5    | 并行 BGZF 解压（rayon pool） | -35s decomp |
| v6    | 并行序列化（预串行化 RunMsg 在 worker 侧） | -10s writer CPU |
| v7    | Thread-local 热缓冲（消除每帧 alloc） | 细微 |
| v8    | BGZF block 缓冲复用（消除 mmap/munmap） | -3-5s |
| v9    | Thread-local scratch（消去页错误） | -5-10s |
| v10   | Buffer pool（失败，+30 GB RSS，回滚） | — |
| v11   | BGZF carry buffer 复用 | -2-3s |
| v12   | AVX-512 向量化直方图（排序加速 20-30%） | -3-5s sort |
| **v13** | **run_queue_depth 修复（防止 OOM）** | **k51 可运行** |
| **v13** | **Vec shrink_to（RSS 下降）** | **-30-40 GB RSS** |
| **v13** | **MSD radix sort（约 2× sort 加速）** | **-12-15s wall** |
| **v13** | **软件预取（cache miss 减少 10-15%）** | **-3-5s** |
| **v13** | **64 MB 写缓冲** | **-1-2s** |


### Count 时间分解（k21，48线程，Fibonacci hash 后）

```
decomp:  40s wall   (单线程 gzip 解压，固定开销)
sort:   440s CPU    = 9.2s 平均每线程，wall ~15-20s
rle:     68s CPU    = 1.4s 平均每线程，wall ~5s
write:   56s CPU    = 1.2s 平均每线程，wall ~10s
-----------------------------------------
total:   91s wall
```

**关键发现**：
- `decomp` 40s 是串行上限，必须并行化
- `sort` 总 CPU 最大，但因为 48 线程并行，wall time 约 15-20s
- `write` 56s CPU 但只有 1 个文件（串行 append 会竞争锁）
- k=51 的 sort 是 k=21 的 **2.75×**（u128 内存带宽加倍）

### Dump 时间分解

- merge（loser tree）：64ms 几乎可忽略  
- text format：512s CPU / 48线程 = 10.7s 平均，wall ~48s
- dest write：0ms（全部在 format 里 buffered）
- **架构问题**：先写 256 个临时桶文件再 concat，需要 2× 磁盘 IO 空间

---

## 优化路线图

### P0 — 并行 BGZF 解压（预期收益 ~30s）

**当前**：单线程 `MultiGzDecoder` 流式解压，40s 固定开销  
**方案**：HG002_sample 是 BGZF 格式（每块 ≤ 65536B，独立可解压）
1. 扫描 BGZF block 边界（已有 `scan_bgzf_blocks()`）
2. 将所有 block 分片分配给 N 个解压线程
3. 解压线程向 channel 投送 `Arc<Vec<u8>>` 块
4. worker 线程从 channel 消费，并行 kmer 生成

预期：decomp 40s → 3-5s（N=8-16 解压线程足够）  
实现位置：`src/count/bgzf.rs` + `src/count/mod.rs`

**注意**：当前代码已扫描 BGZF block 边界，只缺并行解压 pipeline。

---

### P1 — 动态去重 Hash Table（预期收益 k21 ~2×，k51 ~1.5×）

**当前**：所有 k-mer 直接写入 bucket buffer → sort → RLE 去重  
**方案**：每个 bucket 维护一个 64KB~2MB 的 counting hash table
- thread-local，每线程拥有 `256 个小 hashmap`（每 bucket 一个）
- k-mer 路由后先在 hashmap 里 `count += 1`
- hashmap 满（阈值 ~75%）时 drain、sort、RLE 压缩、写文件
- 最终 flush 同现有逻辑

**预期数据量减少**：
| k | 原始 k-mer | 唯一 k-mer | 重复率 | sort 减少 |
|---|----------|----------|------|---------|
| 21 | 9.0B | 2.7B | 3.3× | ~3× |
| 31 | 8.3B | 3.1B | 2.7× | ~2.7× |
| 51 | 7.0B | 3.3B | 2.1× | ~2.1× |

预期 sort time：k21 440s → 140s，k51 1207s → 580s  
预期 wall time 变化（结合 P0）：k21 91s → ~25s，k51 141s → ~60s

**实现位置**：`src/count/worker.rs` 或 `src/count/mod.rs`  
精确实现：使用 `rustc-hash::FxHashMap<u64, u32>` 或 `u128, u32`，或 robin-hood hashmap

---

### P2 — 无临时文件流式 Dump（收益：空间减半，速度接近不变）

**当前**：写 256 个 `.bucket.NNN` 临时文件 → concat 到最终输出  
**问题**：
1. 需要 2× 磁盘空间（源 .kite + 临时桶 + 输出）
2. concat 步骤到 NVMe 比 /dev/shm 慢 ~10×
3. 256 个并发写文件 IO 压力大

**方案**：改为 **两阶段 streaming**：
1. Phase 1（并行）：per-bucket merge → 写到 `Arc<TempBuf>` 内存缓冲（256 × 内存桶）
2. Phase 2（串行）：按 bucket 顺序依次 flush 到最终输出文件

或者更彻底：**pipeline 方案**（生产者-消费者）：
- 生产者线程池：每桶 merge + text-format，生产 `(bucket_id, Vec<u8>)` chunks
- 消费者（单 writer）：按 bucket_id 顺序 in-order 写出（需一个小的 reorder buffer）
- 不创建临时文件，整个 dump 只有一个输出文件

实现位置：`src/dump/mod.rs`，重写 bucket 处理循环

---

### P3 — u128 Radix Sort SoA 优化（k=33..64 性能提升）

**当前**：每条目 `(u128 key, u32 count)` = 20B，radix sort 移动整条目  
**方案**：`Structure of Arrays`
- 分离 `keys: Vec<u128>` 和 `counts: Vec<u32>`
- radix sort 只移动 keys（16B/entry），最后按排列结果重排 counts
- 每 cache line (64B) 装 4 个 u128 key（提升 cache utilization）
- 预期: 20B/entry → 16B/entry 有效排序宽 → 1.25× cache 效率提升

实现位置：`src/count/sorter.rs`，新增 `radix_sort_u128_soa`

---

### P4 — Writer 并发优化

**当前**：KiteWriter 有全局 Mutex，每次 flush 需要锁  
**优化**：
- 每个 bucket 独立文件（已是当前实现），但 writer lock 竞争高
- 改为 per-bucket lock（256 个独立锁）
- 并发写 256 个文件，OS page cache 自动聚合 IO
- 或者：用 io_uring 批量提交 writes（高级）

---

### P5 — Kmer 生成 SIMD 优化

**当前**：每个碱基逐个处理  
**方案**：
- 一次生成 4~8 个 k-mer（unrolled loop）
- 使用 SIMD shuffle 查表同时编码 16 个碱基
- 对 canonical k-mer 的 min(fwd, rev) 计算可以 SIMD 并行比较
- 适用于所有 k 值（u64/u128/KmerU256 路径）

---

## 实施优先级与预期收益

| 优先级 | 优化项 | 实现难度 | k21 收益 | k51 收益 | 应在哪改 |
|--------|--------|----------|----------|----------|---------|
| **P0** | 并行 BGZF 解压 | 中 | -35s | -35s | bgzf.rs + mod.rs |
| **P1** | 动态去重 hashmap | 中高 | -20s | -40s | worker/mod.rs |
| **P2** | 无 tempfile dump | 中 | dump 可靠 | dump 可靠 | dump/mod.rs |
| **P3** | u128 SoA sort | 低 | — | -10s | sorter.rs |
| **P4** | 并发 bucket writer | 低 | -5s | -15s | mod.rs/writer.rs |
| **P5** | Kmer gen SIMD | 高 | -5s | -5s | kmer.rs |

---

## 本轮实施计划（按顺序）

### Step 1: 并行 BGZF 解压（P0）

修改 `src/count/bgzf.rs`：
```
// 新增：block_count, scan_all_blocks() → Vec<BgzfBlockLoc>
// 每个 BgzfBlockLoc { file_offset: u64, compressed_size: u32 }
```

修改 `src/count/mod.rs`：
```
// 新增 parallel_bgzf_pipeline(path, n_decomp, n_workers, ...)
// 1. mmap 整个输入文件
// 2. scan_bgzf_blocks → Vec<BgzfBlockLoc>
// 3. rayon 并行解压每个 block → Vec<Arc<Vec<u8>>>
// 4. parse_sequences → 生成 k-mer → 路由到 bucket buffer
```

**关键细节**：
- BGZF block 解压后是明文；需要将相邻 blocks 合并时注意跨 block 的 FASTQ record 边界
- 可以先 collect 所有解压后的块，然后在块边界处切割 FASTQ records，再分发给 worker 线程
- 或者：按 FASTQ record 行边界对齐切割（更复杂但更高效）

---

### Step 2: 动态去重 Hash Map（P1）

在 `src/count/mod.rs` 的 worker 循环中：
```rust
// 现有：
for kmer in kmers { bucket_bufs[bucket].push((kmer, 1u32)); }

// 改为：
for kmer in kmers {
    let entry = bucket_hmaps[bucket].entry(kmer).or_insert(0u32);
    *entry = entry.saturating_add(1);
    if bucket_hmaps[bucket].len() >= DEDUP_CAPACITY {
        // drain → extend bucket_bufs[bucket]
        flush_hmap_to_buf(&mut bucket_hmaps[bucket], &mut bucket_bufs[bucket]);
    }
}
```

Hash map 容量 `DEDUP_CAPACITY` = 2MB / sizeof(Entry) = ~50K (u64) 或 ~30K (u128)  
此阶段去重取消了 RLE 的必要性（因为 write 时已有 sorted + rle），但 RLE 仍可处理 inter-flush 重复。

---

### Step 3: 无 tempfile 流式 Dump（P2）

重构 `src/dump/mod.rs`：
1. 将 bucket 处理结果改为写入 `Vec<u8>` 内存缓冲而非临时文件
2. 使用 `crossbeam_channel` 按 bucket_id 顺序向单个 writer 线程投送完成块
3. writer 线程按 bucket 顺序拼接到最终输出（Channel 中用优先队列保序）

或者更简单的方案：使用 `arc-swap` 受控的 ordered output：
- 用 `Vec<Mutex<Vec<u8>>>` 预分配 256 个桶缓冲 
- 处理完成后不写文件，直接保存在内存桶里
- 最后按序遍历输出到 sink

内存估算：3.1B k-mers × 25B/line / bucket = 44GB/256 = 172MB/bucket  
同时处理 48 个桶 = 48 × 172MB = 8.3GB 内存峰值 → 可接受（在 64GB 预算内）

---

## 实施后预期性能

| 场景 | 优化项 | 预期 wall time | vs KMC | 目标 |
|------|--------|----------------|--------|------|
| k=21 count | P0 + P1 | ~30-35s | ~0.47× | ✅ <33s |
| k=31 count | P0 + P1 | ~30-35s | ~0.47× | ✅ <33s |
| k=51 count | P0 + P1 + P3 | ~50-60s | ~0.70× | ❌ 仍需 P3/P4 |
| k=21 dump | P2 | ~50s | 0.30× | ✅ 已达标 |
| k=31 dump | P2 (无 tempfile) | ~60s | 0.50× | ✅ <121s |
| k=51 dump | P2 (无 tempfile) | ~80s | 0.42× | ✅ <189s |

**k=51 count 差距根因**：u128 sort 内存带宽是 u64 的 2×，需要 P3（SoA sort）才能弥补。  
若 P0+P1+P3 都实施，k=51 有望达到 40-45s（<37s 目标仍有难度，需要进一步分析）。

---

## 验收标准

- [x] `cargo build --release` 零警告
- [x] `cargo test` 43/43 通过（含 KmerU256 测试）
- [ ] k21 count wall time < 33s
- [ ] k31 count wall time < 33s  
- [ ] k51 count wall time < 37s
- [ ] k21 dump wall time < 85s（已满足：48s）
- [ ] k31 dump wall time < 121s（需 P2 实现后测试）
- [ ] dump 不创建临时文件（P2）
- [ ] dump 输出行数与 KMC dump 一致（正确性）

---

## 实施注意事项

1. **BGZF 并行解压的 record 边界**：BGZF block 内容不一定从 FASTQ record 开头开始。需要先解压块，再按 `\n@` 找 record 边界。可以跨 block 边界 prefetch 下一个 block 来处理跨块 record。

2. **Hashmap 线程安全**：每个 worker 线程有自己的 hashmap 数组（256 个 bucket 对应 256 个 hashmap），完全无锁。只在 flush 时才访问共享的 bucket 写文件 Mutex。

3. **Dump 内存估算**：如果 k31 dump 的每桶结果都存内存，需要 ~8GB。需要确保不超过 --max-memory-gb 限制。可以添加 --dump-memory-gb 参数控制。

4. **Fibonacci hash 已生效**：max bucket 1.4-1.5× 均值，不需要进一步优化路由。

5. **KmerU256 (k=65..128)**：已完整实现，但性能未测试。plan02 后可专项测试 k=65/128。
