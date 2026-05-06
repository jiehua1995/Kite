# Record 04 — kite v14 vs KMC 小数据集基准测试与优化分析

## 测试环境

- CPU: Intel Xeon Gold 6248R × 2 socket（48 物理核，96 超线程）
- RAM: 256 GB
- NVMe: 7.3 TB（挂载 `/eight_TB`）
- OS: Linux 5.4
- kite: `target/release/kite`（v14，含 sleeping-fix: 无界 channel + budget.alloc/free）
- KMC: `/home/zfp_da03/miniforge3/envs/kmc/bin/kmc`（v3）
- 线程数: 48
- 测试数据: `test/test.fastq.gz`（1M reads，88 MB 压缩，346 MB 解压）
- temp 目录: `/eight_TB/BigMQ/Users/Hua/kmer_counter/kite/temp/`

---

## 基准测试结果

### Count 对比（kite warm cache / KMC count）

| k  | KMC wall | KMC CPU% | KMC RSS  | kite wall | kite CPU% | kite RSS | kite/KMC |
|----|----------|----------|----------|-----------|-----------|----------|----------|
| 21 | 2.97 s   | 700%     | 1.03 GB  | **1.54 s** | 750%    | 2.22 GB  | **+93%** |
| 31 | 3.04 s   | 742%     | 1.72 GB  | **1.70 s** | 784%    | 1.16 GB  | **+79%** |
| 51 | 3.29 s   | 677%     | 1.47 GB  | **1.94 s** | 950%    | 1.33 GB  | **+70%** |

> kite count warm cache 全面领先，k=21 快近 2×。

### Dump 对比 — v14 基线 vs 优化后（O2+O3）vs KMC

| k  | KMC wall | kite v14 (基线) | kite opt | 优化倍速 | vs KMC   |
|----|----------|-----------------|-----------|---------:|---------:|
| 21 | 7.57 s   | 6.44 s          | **2.72 s** | +2.37× | **+178%** |
| 31 | 9.49 s   | 4.43 s          | **3.94 s** | +1.12× | **+141%** |
| 51 | 12.99 s  | 5.99 s          | **6.01 s** | ~1×    | **+116%** |

> O2=预分配 out_buf 至 io_buf(16MB)；O3=真正的败者树替代 BinaryHeap。
>
> k51 改善不明显是因为每桶输出 22MB > io_buf 16MB，out_buf 仍需第二段分配。  
> k21 改善最大：minor page faults 从 886K 降至 271K（3.3×减少），sys_time 从 ~8s 降至 4.82s。

### test-mode 详细对比（dump）

#### v14 基线

| k  | merge (ms) | format (ms) | wall  | RSS    | minor PF | sys time |
|----|------------|-------------|-------|--------|----------|----------|
| 21 | 3612       | 21579       | 6.44s | 1.88GB | 886K     | ~8s      |
| 31 | 3703       | 26163       | 4.43s | 2.52GB | 892K     | 12.06s   |
| 51 | 3416       | 31883       | 5.99s | 3.58GB | 1086K    | 15.70s   |

#### 优化后（O2: pre-alloc out_buf + O3: loser tree）

| k  | merge (ms) | format (ms) | wall  | RSS    | minor PF | sys time |
|----|------------|-------------|-------|--------|----------|----------|
| 21 | 3730       | **16895**   | **2.72s** | 1.67GB | **271K** | **4.82s** |
| 31 | 4047       | **20926**   | **3.94s** | 2.21GB | **349K** | **7.41s** |
| 51 | 3519       | **26238**   | 6.01s | 3.96GB | **224K** | **11.77s** |

> text format 时间均下降 17-22%（pre-alloc 减少了 Vec 内数据复制 + 相关 cache miss）。  
> k21/k31 wall 显著下降是因为每桶输出 ≤ 16MB，pre-alloc 完全消除了 Vec doubling。  
> k51 每桶输出 ~22MB > 16MB 仍有一次二级增长，收益较小。

---

## Kite test-mode 深度分析

### count k=21 各阶段 CPU 时间（wall=1.50s）

| 阶段          | CPU 时间  | 占比  | 说明                     |
|---------------|-----------|-------|--------------------------|
| rle+ser       | 5766 ms   | 49%   | RLE 压缩 + zstd 序列化   |
| sort          | 2283 ms   | 19%   | 桶内 MSD 排序             |
| kmer gen+push | 1744 ms   | 15%   | k-mer 提取 + 分桶         |
| decomp        | 933 ms    | 8%    | BGZF/gzip 解压            |
| write         | 505 ms    | 4%    | 写 .kite 文件              |
| 其他          | ~525 ms   | 5%    | 调度、同步等              |

- **最大瓶颈**: rle+ser（49%），即 5888 次 zstd 压缩调用（avg 22K entries × 12B = 264KB/run）

### dump 各阶段 CPU 时间（各 k 值）

| k  | merge (ms) | format (ms) | write (ms) | 总 CPU (ms) | wall   | 有效并行核 |
|----|------------|-------------|------------|-------------|--------|------------|
| 21 | 3612       | 21579       | 0*         | ~27500      | 6.44 s | 4.3 核     |
| 31 | 3703       | 26163       | 0*         | ~33250      | 4.43 s | 7.5 核     |
| 51 | 3416       | 31883       | 0*         | ~39300      | 5.99 s | 6.6 核     |

> \* write time 未在 test-mode 中统计（仪器代码未插桩），实际有 /dev/shm 写入和 NVMe 最终汇总。
> 
> format = decode k-mer + 写 ASCII 行（`format_tsv_line`）
> merge = BinaryHeap 败者树归并（当前用 BinaryHeap，非真正败者树）

#### dump 各 k 值瓶颈分布

| k  | merge 占比 | format 占比 | 说明                        |
|----|------------|-------------|-----------------------------|
| 21 | 13%        | 78%         | format 绝对主导              |
| 31 | 11%        | 79%         | format 主导（lines 更长）    |
| 51 | 9%         | 81%         | format 主导（u128 + 更长行） |

> **dump 核心瓶颈：text format（decode k-mer + 生成 TSV 行）占 78-81% CPU**

#### 内存与 page fault 分析（dump）

| k  | RSS    | minor PF   | PF 总数据量 | sys time |
|----|--------|------------|-------------|----------|
| 21 | 1.88GB | 886K       | 3.44 GB     | ~8s      |
| 31 | 2.52GB | 892K       | 3.56 GB     | 12.06 s  |
| 51 | 3.58GB | 1086K      | 4.34 GB     | 15.70 s  |

> sys time（内核态时间）占总 CPU 的 37-40%，主要来自：
> 1. **Vec realloc 的数据复制引发 page fault**：`out_buf` 从 1MB 增长到 16MB（4次 doubling = 1+2+4+8=15MB 复制/桶 × 256桶 = 3.84GB 额外复制）
> 2. **写入 /dev/shm（tmpfs）引发 page fault**：写入新 tmpfs 页

---

## 瓶颈分级与优化计划

### O1（已实现）: sleeping-fix — 无界 channel + 正确 budget.alloc/free

已完成，workers 不再 sleep，count 性能正常。

### O2（优先）: dump — `out_buf` 预分配至 `io_buf`（16MB）

**问题**: `Vec::with_capacity(io_buf.min(1024 * 1024))` = 1MB，然后增长到 16MB 需要 4 次 doubling，复制数据 15MB × 256 = 3.84GB。

**修复**:
```rust
// 修改前
let mut out_buf: Vec<u8> = Vec::with_capacity(io_buf.min(1024 * 1024));

// 修改后  
let mut out_buf: Vec<u8> = Vec::with_capacity(io_buf);
```

**预期收益**: 消除 3.84GB 的 Vec 复制引发的 extra page faults，估计减少 sys time 40%，wall time 节省约 0.5-1.0s（dump k31/k51）。

### O3（中优先）: dump — 真正的败者树（loser tree）替代 BinaryHeap

**问题**: `LoserTreeMerger` 实名用 `std::collections::BinaryHeap`（不是败者树）。
- BinaryHeap pop() = 2×log(n) 比较，push() = log(n) 比较，每个 k-mer ~13-15 次
- 真正败者树 adjust() = log(n) 比较（仅单路径无 sibling 查找），每个 k-mer ~5 次

**修复**: 用 n_tree = next_power_of_2(n) 的 1-indexed 败者树替换 BinaryHeap。

**预期收益**: merge time 减少约 50-60%（3700ms → 1500ms），整体 dump wall time 减少约 5-10%（merge 只占总 CPU 10%）。

### O4（中优先）: count — zstd 小 run 跳过阈值

**问题**: 5888 次 zstd 压缩调用，其中部分 run 较小（flush 时提前压缩），zstd overhead 不划算。

**修复**: 在 `worker.rs::flush_bucket()` 中添加阈值：
```rust
if self.zstd_level > 0 && data.len() < 32 * 1024 {
    // 跳过 zstd，直接存原始数据
}
```

**预期收益**: 减少冗余 zstd 调用，rle+ser 时间减少 10-20%。

### O5（低优先）: dump — parallel final assembly

**问题**: 最终汇总（256 个 /dev/shm 文件 → 单个输出）是串行 `copy_file_range` 循环。

**估算开销**: k31=3.1GB/~4GB/s = 0.78s，k51=4.6GB/4GB/s = 1.15s（约占 wall time 18-20%）。

**修复**: 用 `fallocate` 预分配输出文件，然后 48 线程并行 `pread(/dev/shm) + pwrite(output)`。

**预期收益**: final assembly 时间减少约 70%（0.78s → 0.23s for k31）。

---

## 性能预测（优化后）

| 场景        | 当前 wall | 优化后估计 | KMC wall | 预期优势 |
|-------------|-----------|------------|----------|----------|
| count k=21  | 1.54 s    | 1.35-1.45s | 2.97 s   | 2.0-2.2× |
| count k=31  | 1.70 s    | 1.50-1.60s | 3.04 s   | 1.9-2.0× |
| count k=51  | 1.94 s    | 1.70-1.80s | 3.29 s   | 1.8-1.9× |
| dump k=21   | 6.44 s    | 5.5-6.0s   | 7.57 s   | 1.3×     |
| dump k=31   | 4.43 s    | 3.5-4.0s   | 9.49 s   | 2.4-2.7× |
| dump k=51   | 5.99 s    | 4.5-5.5s   | 12.99 s  | 2.4-2.9× |

---

## 附：KMC 完整基准数据

### KMC count（`kmc -k{k} -t48 test.fastq.gz output temp`）

```
k=21: wall=2.97s, CPU=700%, RSS=1.03GB
k=31: wall=3.04s, CPU=742%, RSS=1.72GB
k=51: wall=3.29s, CPU=677%, RSS=1.47GB
```

### KMC dump（`kmc_dump output.kmc output.tsv`，单线程）

```
k=21: wall=7.57s, CPU=99%,  RSS=0.06GB
k=31: wall=9.49s, CPU=99%,  RSS=0.06GB
k=51: wall=12.99s, CPU=99%, RSS=0.06GB
```
