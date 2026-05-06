# Kite Plan 04 (Extreme) — 无兼容负担的 Kite .kite 设计

## 1. 设计宣言

本计划明确采用“推倒重来”策略：
1. 直接放弃旧格式、旧代码路径、旧参数兼容。
2. 只保留 count 与 dump 两个子命令。
3. 针对 48 线程 + 64GB 内存 + 单机高性能 NVMe 做硬件定制。
4. 目标不是略快，而是在 HG002 的 k21/k31/k51 上对 KMC 拉开明显倍数差距。

核心原则：
1. count 第一优先级，dump 第二优先级，但两者都必须并行饱和。
2. 只做吞吐收益巨大的结构改造，不做温和修补。
3. 每次优化必须可量化，必须通过 test-mode 指标闭环验证。

---

## 2. 总体目标（硬指标）

目标平台：48 物理线程，64GB RAM，NVMe 高带宽。

预期目标区间（.kite 稳定后）：
1. k21：count+dump 总时长 < KMC 的 60%。
2. k31：count+dump 总时长 < KMC 的 35%。
3. k51：count+dump 总时长 < KMC 的 35%。
4. dump 阶段 CPU 平均利用率 > 2200%（约 22 核以上有效忙碌），禁止“单核写出”。
5. 全程 RSS 不超过 64GB 预算，95% 运行时间内保持在预算 90% 以下。

---

## 3. Kite 格式（.kite）

不再沿用旧 `.kite` 结构，定义全新.kite 二进制布局，目标是“为并行 dump 服务”。

## 3.1 文件布局

1. Header (128B, 对齐 64B)
- magic/version
- k/canonical
- partitions
- key_width (64/128/256)
- codec_mask
- chunk_index_offset
- partition_table_offset
- checksum_mode

2. Data Area
- 以 partition 为大区，每个 partition 内由多个 chunk 组成。
- chunk 是最小调度单元，不再以 run 作为核心调度单位。

3. Chunk Index（全局连续）
- 每个 chunk 记录：
  - partition_id
  - file_offset
  - raw_bytes
  - row_count
  - key_min/key_max
  - codec

4. Partition Table
- 每个 partition 记录 first_chunk_id, chunk_count, total_rows。

5. Footer
- total_chunks/total_rows/尾部魔数。

## 3.2 编码策略

1. k<=31：主键固定 u64，count 为 u32。
2. 32<=k<=64：主键 u128。
3. k>64：主键 256-bit packed。
4. 默认 codec：DVLE（delta + varint）或 FOR-delta（二选一，按 chunk 自适应）。
5. 禁止对所有数据无脑 zstd；仅在 chunk 熵高且压缩比收益超过阈值时启用轻压缩。

## 3.3 为什么 .kite 会快很多

1. dump 可以直接按 chunk 并行，而不是被 bucket 粗粒度锁死。
2. key range 元数据使调度器能做负载均衡，避免长尾线程。
3. partition 和 chunk 索引连续存储，mmap 后可零拷贝读取元数据。

---

## 4. Count .kite 架构

count 架构目标：让 CPU 在 parse/sort/rle 上持续满载，同时让 I/O 完全异步化。

## 4.1 五级流水线

1. Reader（io_uring）
- 使用固定 buffer ring，异步批量读文件块。
- gzip/BGZF 统一走块级任务接口。

2. Decompressor
- BGZF block 并行解压，任务分发到解压线程池。
- 解压结果直接进入序列解析队列，禁止中间大拷贝。

3. Parser + Kmer Scan
- 字节状态机，只提取序列行，ID/QUAL 立刻丢弃。
- k-mer 滚动编码（u64/u128/u256 专门热路径）。

4. Partition + Local Sort/RLE
- 线程私有分桶缓冲，达到阈值立即本地 radix sort + RLE。
- 采用 write-combining staging，减少散写 cache miss。

5. Shard Writer（io_uring）
- 不使用“每 bucket 一把 mutex 直接写”的旧模式。
- 改成 N 个 writer shard（建议 8 或 12），每个 shard 维护提交队列。
- 批量 writev/pwrite 提交，统一 completion 回收。

## 4.2 算法与底层优化

1. 分桶哈希使用 MulSwapMul/Fibonacci 组合，抗低复杂度偏斜。
2. radix sort 优先，必要时切换小数组插排。
3. 关键结构 64B 对齐，避免 false sharing。
4. 内存分配器强制 mimalloc 或 jemalloc。
5. 可选 AVX2/AVX-512 path：
- base 编码批处理
- key decode/format 批处理

## 4.3 64GB 内存治理

1. 全局信用额度（credit-based budget）。
2. 各流水级独立水位线，达到高水位即背压上游。
3. chunk 大小自适应（4MB, 8MB, 16MB 三档）。
4. 任何单阶段都不得持有超过预算 35% 的内存。

---

## 5. Dump .kite 架构

dump 架构目标：彻底消灭“一个线程写，全体等待”的现象。

## 5.1 三段式并行

1. Chunk Scheduler
- 输入 .kite 索引，按 chunk 粒度切任务。
- 调度策略：先大后小 + work stealing，压平长尾。

2. Decode + Merge + Format Worker
- 每个 worker 处理 chunk 组。
- loser tree 仍保留，但树节点做 cache-friendly 布局。
- 输出写入 page-aligned slab（1MB~4MB），满即投递。

3. Async Writer Group
- 多 writer 线程 + io_uring，批量顺序落盘。
- 对最终 TSV 使用分段预分配 + 并行拼接策略。
- 写后按窗口执行 fadvise，控制 page cache 膨胀。

## 5.2 避免旧版 dump 退化

1. 禁止“整 bucket 全量格式化到一个巨大 Vec 后再写”。
2. 禁止单 writer 串行写全量数据。
3. 禁止无上限 channel 堆积导致内存暴涨。

## 5.3 文本序列化优化

1. count 使用 itoa，禁止 println。
2. k-mer 解码采用 LUT + 位操作，减少分支。
3. slab 内一次写入多行，减少 syscall 密度。

---

## 6. 线程与 NUMA 策略

1. 线程默认绑核（core affinity）。
2. count/dump 都按 socket 分组，尽量本地内存分配。
3. I/O 线程和计算线程分离，避免抢占。
4. 高负载阶段控制活跃线程数，避免调度抖动。

---

## 7. test-mode 指标体系（强制）

每次提交必须输出以下指标，并可与基线自动对比。

count 必测：
1. read_ms
2. decomp_ms
3. parse_ms
4. kmer_scan_ms
5. sort_ms
6. rle_ms
7. encode_ms
8. writer_queue_wait_ms
9. writer_io_ms
10. peak_rss_mb

dump 必测：
1. decode_ms
2. merge_ms
3. format_ms
4. writer_queue_wait_ms
5. writer_io_ms
6. concat_ms
7. cpu_util_pct
8. peak_rss_mb
9. longest_worker_ms / shortest_worker_ms

负载分布必测：
1. hottest_partition_chunks
2. coldest_partition_chunks
3. p99 chunk bytes

---

## 8. 工程实施路线（四个里程碑）

## M1: .kite 格式与最小闭环

1. 定义 .kite header/chunk index/partition table/footer。
2. count 写 .kite，dump 读 .kite。
3. 删除旧格式读写代码，避免双路径维护。

## M2: Count 全并行流水线

1. 上线五级流水线与 shard writer。
2. 完成 64GB 信用预算控制。
3. 打通 test-mode 新指标。

## M3: Dump 全并行写出

1. 上线 chunk scheduler + worker + async writer group。
2. 去掉单线程写路径。
3. 验证 48 线程下无长时间 sleeping/waiting。

## M4: 极限优化

1. AVX2/AVX-512 热路径。
2. NUMA 策略与参数自调优。
3. 在 HG002 k21/k31/k51 形成稳定优势报告。

---

## 9. 失败判据与回滚策略

失败判据：
1. 任一 k 值总时长回退超过 8%。
2. dump CPU 利用率长期低于 800%。
3. RSS 超过 64GB 或出现频繁 OOM 保护触发。

处理策略：
1. 保留上一里程碑可运行分支。
2. 所有优化都以 feature flag 逐步开启。
3. 不达标的优化直接撤销，不保留“理论更优、实测更慢”的代码。

---

## 10. 最终交付标准

1. 仅保留 count/dump 两个命令，命令行接口简洁。
2. .kite 成为唯一格式，不做向后兼容。
3. 文档、指标、基准脚本三者一致。
4. 在目标服务器上，稳定实现“明显快于 KMC 的倍数级优势”。

