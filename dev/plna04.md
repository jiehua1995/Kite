# Kite Plan 04+（plna04）: 面向 48C/64GB 的结构性重构方案

## 0. 目标与约束

目标不是小修，而是通过 **格式+管线协同重构**，在 HG002 的 k21/k31/k51 上显著缩短 count 与 dump，总体稳定压过历史版本与 KMC。

硬约束：
- 48 线程
- 内存上限 64GB
- 必须保留 test-mode，且指标可做 A/B

---

## 1. 现状诊断（基于当前 src）

### 1.1 dump 慢、CPU sleeping/waiting 的直接原因

当前 dump 的写路径已经退化为 **单写线程顺序 write(2)**：
- 见 src/dump/mod.rs 中的 sequential_writer：一次只由一个线程写出，天然形成串行瓶颈。
- 格式线程虽然并行，但每个 bucket 先完整格式化成一个大 Vec<u8> 后再送给单写线程，导致“生产并行 + 消费串行”的背压。

结果：
- 常见现象就是只有 1 个核持续工作，其他线程在 channel 或 I/O 上等待。
- wall time 被写出路径主导，CPU 利用率偏低。

### 1.2 count 仍存在结构性损耗

- 当前 worker 通过 `Mutex<BucketFile>` 直接写每个 bucket 临时文件，属于“高并发小锁+大量小 run”的折中方案，吞吐会受到 flush 颗粒度和锁争用影响。
- assemble 阶段目前是顺序遍历 bucket 复制，不是并行拼接，尾段耗时仍明显。

### 1.3 格式层面问题

当前 .kite v1（header + run + trailer index）可用，但对高吞吐 dump 不够友好：
- run 的组织更偏“写入方便”，不是“读取/并行 merge/并行序列化方便”。
- 缺少按 chunk 的二级索引与 key-range 元数据，导致 dump 难以精细调度。

---

## 2. 核心改造：定义 .kite v2（破局点）

> 这次大改的核心是：把 .kite 从“run 容器”升级为“可并行调度的数据平面”。

## 2.1 v2 文件布局

建议引入新 magic/version（与 v1 并存可读）：

1. HeaderV2（固定 128B）
- magic: K2TE
- version: 2
- k, canonical, partitions
- key_width: 8/16/32
- chunk_target_bytes
- flags: 编码能力位（DVLE/FrameOfReference/ZSTD/CRC）
- offsets: bucket_dir_off, chunk_index_off, footer_off

2. Data Region（按 bucket 的 chunk 流）
- 每个 bucket 由多个 chunk 组成（建议 4MB~16MB）
- chunk 内 key 有序、去重
- chunk header：
  - bucket_id
  - row_count
  - payload_bytes
  - key_min/key_max
  - sum_count
  - codec（raw/dvle/for-delta）
  - crc32

3. ChunkIndex（全局连续）
- 每个 chunk 一条索引：
  - bucket_id
  - file_off
  - payload_bytes
  - row_count
  - key_min/key_max

4. BucketDirV2
- 每个 bucket：
  - first_chunk_idx
  - chunk_count
  - total_rows
  - total_payload_bytes

5. FooterV2
- 总 chunk 数、总 run/row、尾部 magic

## 2.2 v2 设计收益

- count 写出端可直接按 chunk 追加，不需要大量零碎 run 元信息。
- dump 可按 chunk 做细粒度任务切分，而不是“一个 bucket 一个大任务”。
- key_range 支持后续 query/merge/增量 compaction（借鉴 LSM/ClickHouse MergeTree 思路）。

---

## 3. count v2 重构计划

## 3.1 从“每 bucket 锁写”改成“分区写线程 + 无锁队列”

现状：worker 直接持锁写 bucket 文件。

改造：
- 按 NUMA/CPU 分组设置 N 个 writer shard（建议 8~16）
- worker 只做 parse + kmer + sort + rle + encode chunk
- worker 将 chunk 投递到 shard 队列（MPSC/无锁 ring）
- writer shard 顺序追加到自己的 segment 文件，减少锁竞争与 fd 抖动

收益：
- 锁竞争显著下降
- 写系统调用更大块、更顺序
- 更容易接 io_uring 批量提交

## 3.2 排序与编码路径

- 保留 radix + RLE
- DVLE 保留作为默认编码
- 新增 FOR-delta（Frame of Reference）可选编码：
  - 对高局部性 key 区间通常优于原始 12/20/36B
- k<=31 路径持续维持最紧凑 key 宽度

## 3.3 自适应分桶升级

- 在 worker 内维护 bucket 热度采样
- 对热点 bucket 启用子分片（virtual bucket）写入，最终在索引层回并
- 防止少数热点 bucket 拖慢全局 flush 节奏

---

## 4. dump v2 重构计划（重点解决 1 核写瓶颈）

## 4.1 两阶段并行：并行生成 + 并行拼接

阶段 A：并行 decode+merge+format 到 stripe 临时文件
- 把 bucket 按连续区间分给 S 个 stripe worker（例如 8 或 12）
- 每个 stripe worker 内部并行处理自己的 chunk/bucket 任务
- 每个 stripe 输出一个大临时 TSV（顺序写）

阶段 B：并行拼接到最终输出
- 先收集所有 stripe 文件长度，前缀和得到目标 offset
- 对最终文件 fallocate
- 多线程 pwrite/copy_file_range 并行拼接

这样可以保持全局顺序（按 bucket 区间拼接）并消除单写线程。

## 4.2 去掉“整 bucket 大缓冲”的模式

- 不再为 bucket 生成完整大 Vec<u8>
- 改为 page-aligned slab（例如 1~4MB）流式序列化
- slab 满即投递写队列

收益：
- 降低峰值内存
- 减少长尾 bucket 导致的背压
- 写线程可持续饱和磁盘

## 4.3 Loser tree 调度升级

- 从“bucket 粒度”升级为“chunk 粒度”任务
- 结合 chunk key-range，优先调度更短、更均衡的任务
- 加入 work-stealing，避免线程不均

---

## 5. 跨领域可借鉴方法（本轮将落地）

1. LSM-Tree 思路
- count 产出的多段有序 chunk，dump/merge 进行层级 compaction。

2. 列式数据库思路
- index 与 data 解耦，避免 dump 扫描时无效元数据访问。

3. 流处理背压模型
- 对 formatter/writer 用水位线控制，避免“上游爆发、下游阻塞”。

4. HPC NUMA 调度
- 按 socket 绑定线程与内存池，降低跨 NUMA 访存。

---

## 6. 兼容策略与落地顺序

## 6.1 兼容

- reader 支持 v1/v2 双读
- count 默认写 v2，提供 `--format v1|v2` 便于回归
- dump 自动识别版本

## 6.2 实施里程碑

M1（格式底座）
- 引入 HeaderV2/ChunkIndex/BucketDirV2
- 完成 v2 读写最小闭环

M2（count v2 写入管线）
- worker -> shard writer 队列化
- chunk 化写入与元信息落盘

M3（dump v2 并行化）
- stripe 并行生成
- 并行拼接输出
- 去除单线程顺序写

M4（性能打磨）
- NUMA 绑定
- chunk 大小/队列深度自适应
- 热桶子分片策略

---

## 7. test-mode 指标升级（必须）

在现有指标基础上补充：
- count
  - chunk_encode_ms
  - shard_queue_wait_ms
  - shard_write_ms
  - compaction_ms（若启用）
- dump
  - merge_ms
  - format_ms
  - stripe_write_ms
  - final_concat_ms
  - avg_queue_depth
  - writer_busy_pct

并继续保留：
- parse_ms / sort_ms / rle_ms / decode_ms / tsv_write_ms
- cpu_util_pct
- hottest/least partition segments

---

## 8. 预期收益（相对当前重构版）

- dump：从“单写线程瓶颈”恢复到多核写出，预计 2x~5x 提升（取决于 NVMe 与文件系统）
- count：锁争用与尾部 assemble 开销下降，预计 15%~35% 提升
- k21 场景改善最明显（写放大和序列化最重）

---

## 9. 风险与规避

1. 风险：v2 上线初期出现兼容性 bug
- 规避：双格式读写并行维护 + golden 文件回归

2. 风险：并行拼接造成顺序错误
- 规避：按 bucket 区间切 stripe + offset 前缀和校验

3. 风险：高并发下内存超限
- 规避：全链路内存预算器（队列水位 + slab 池 + chunk 上限）

---

## 10. 本计划的执行原则

- 先做结构性收益最大的改造（格式 v2 + dump 写路径）
- 每一步都要可回滚、可 A/B、可量化
- 不接受“代码更复杂但指标不涨”的改动
