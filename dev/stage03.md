# kite Stage03 说明

本文档面向第一次接触本仓库的开发者，目标是让你看完后可以独立完成以下事情：

1. 在当前仓库上编译出可运行的 kite。
2. 理解 count 和 dump 两个子命令的完整执行流程。
3. 理解 .kite 二进制文件格式，以及 dump 为什么能直接在其上做多路归并。
4. 知道每个模块负责什么、主线采用了什么算法、关键内部参数是多少。
5. 按照同一套命令复现实验，得到和当前主线一致的行为。

当前主线状态不是“所有实验的最新版本”，而是 2026-04-05 回退后的稳定主线：

- count 使用首版启发式规划器。
- dump 使用稳定固定拓扑基线。
- 目标是先保证 k21、k31、k51 的 count + dump 组合表现稳定，再继续做下一轮优化。

## 1. 项目目标与当前边界

仓库当前只维护两个正式子命令：

- kite count
- kite dump

当前主线的设计原则是两阶段外排：

1. count 阶段尽快把 FASTQ 转成分桶后的有序 run，写入 .kite。
2. dump 阶段读取 .kite，对每个 partition 做多路归并，最终输出 TSV。

不在当前主线范围内的内容：

- query、merge、histo、sketch 等附加子命令。
- 严格的进程级硬内存上限控制。
- 跨平台细节完全打磨完毕的 parser/runtime banner 实验分支。

## 2. 仓库结构

当前工作区的关键目录如下：

```text
kite/
├── Cargo.toml
├── benchmark/
│   └── run_kmc_kite_benchmark.sh
├── dev/
│   ├── prompt1.md
│   └── stage03.md
├── src/
│   ├── cli.rs
│   ├── main.rs
│   ├── stats.rs
│   ├── runtime.rs
│   ├── format/
│   ├── count/
│   └── dump/
├── test/
│   ├── test.fastq.gz
│   └── HG002_sample.fastq.gz
├── run_benchmark.sh
└── run_kite_tests.sh
```

需要特别说明：

- benchmark/run_kmc_kite_benchmark.sh 是当前推荐的人工 benchmark 脚本。
- run_benchmark.sh 是较早的记录脚本，可以参考，但不是当前主线的权威入口。
- run_kite_tests.sh 已经过时，脚本里仍引用旧参数，不应作为当前复现入口。
- src/runtime.rs 仍保留在仓库中，但 main.rs 没有再引用它，因此它不参与当前二进制主路径。

## 3. 构建与运行环境

### 3.1 Rust 依赖

Cargo.toml 当前直接依赖以下核心库：

- clap：CLI 解析。
- crossbeam-channel：线程间有界队列。
- flate2：gzip/BGZF 解压。
- byteorder：二进制格式序列化。
- memmap2：dump 侧 mmap 读 .kite。
- rayon：并行测量和分区处理。
- libc：CPU 与 RSS 统计。
- mimalloc：全局分配器。
- io-uring：当前 Cargo 中保留，但主线 dump 实现没有直接走 io_uring。
- itoa：无分配格式化 count。

### 3.2 编译命令

推荐始终使用 release 模式：

```bash
cd /eight_TB/BigMQ/Users/Hua/kmer_counter/kite
cargo build --release
```

当前已经验证过该命令可成功完成。

### 3.3 数据集

仓库自带两个输入：

- test/test.fastq.gz：小样本。
- test/HG002_sample.fastq.gz：较大样本，主要用于性能对比。

## 4. CLI 与参数语义

## 4.1 kite count

当前 CLI：

```bash
./target/release/kite count \
  -k 21 \
  -i test/HG002_sample.fastq.gz \
  -o temp/hg002_k21.kite \
  --threads 48 \
  --max-memory-gb 64 \
  --chunk-kmers 8000000 \
  --canonical true \
  --mode fast \
  --io-buf-mb 16
```

参数含义与当前实际行为：

| 参数 | 作用 | 当前是否真实生效 |
| --- | --- | --- |
| -k, --kmer | k 值，支持 1..128 | 是 |
| -i, --input | 输入 FASTQ/FASTQ.GZ | 是 |
| -o, --output | 输出 .kite 路径，必须以 .kite 结尾 | 是 |
| --threads | 总线程数，0 表示自动探测 CPU | 是 |
| --partitions | 指定桶数，必须为 2 的幂，0 表示自动推导 | 是 |
| --chunk-kmers | 用于推导 reader chunk 大小，实际换算为 chunk_bytes=clamp(chunk_kmers*2, 8MiB, 64MiB) | 是 |
| --max-memory-gb | 内存预算，影响 worker 预算、保留空间、flush 阈值 | 是，但不是绝对进程硬上限 |
| --canonical | 是否输出 canonical k-mer | 是 |
| --progress | 预留接口 | 当前主线未接入实际打印逻辑 |
| --test-mode | 打印详细诊断和统计 | 是 |
| --mode | 保留模式提示 | 当前主线只用于打印，不切换算法路径 |
| --io-buf-mb | count 侧每个 partition 的 staging buffer 上限候选 | 是 |

几个重要事实：

- partitions 必须是 2 的幂，因为路由逻辑依赖 log2(partitions) 作为 bucket bits。
- output 不是目录，而是单个 .kite 文件。
- max-memory-gb 当前控制的是主线可追踪 buffer 的预算，不是 RLIMIT 风格的硬杀进程上限。
- count 的 mode 目前没有 fast/balanced/compact 三套完全不同的执行策略，文档中不要把它当成真实算法切换开关。

## 4.2 kite dump

当前 CLI：

```bash
./target/release/kite dump \
  -i temp/hg002_k21.kite \
  -o temp/hg002_k21.tsv \
  --threads 48 \
  --writer-threads 8
```

参数含义与当前实际行为：

| 参数 | 作用 | 当前是否真实生效 |
| --- | --- | --- |
| -i, --input | 输入 .kite 文件 | 是 |
| -o, --output | 输出 TSV；传 - 表示写到 stdout | 是 |
| --threads | dump 侧 worker 数 | 是 |
| --writer-threads | 后台 pwrite 线程数，0 表示自动取 max(1, threads/6) | 是 |
| --progress | 预留接口 | 当前主线未接入实际打印逻辑 |
| --test-mode | 打印详细诊断和统计 | 是 |

dump 当前没有 io-buf-mb 参数。任何仍然引用 dump --io-buf-mb 的脚本都属于旧版本脚本。

## 5. 从输入到输出的主线流程

## 5.1 count 流程

count 的总体流程是：

1. 读取 CLI 参数并校验 k 与 partitions。
2. 根据 k 选择 key 类型。
3. 检测输入压缩类型。
4. 对输入做采样，构造启发式执行计划。
5. reader 线程把输入切成 FASTQ record 对齐的大块。
6. 多个 count worker 对 chunk 做解析、rolling k-mer 生成、分桶、排序、RLE、编码。
7. 共享 writer 把每个 partition 的 chunk 追加到 .kite 数据区。
8. finalize 把 chunk index、partition table、footer 和更新后的 header 一次性写回文件末尾与文件头。

### 5.1.1 key 类型选择

根据 k 走三条静态分派路径：

| k 范围 | key 类型 | 内存字宽 | .kite 单条 entry 大小 |
| --- | --- | --- | --- |
| 1..31 | u64 | 8B | 12B |
| 32..64 | u128 | 16B | 20B |
| 65..128 | KmerU256 | 32B | 36B |

这里的 entry 大小是编码前的逻辑行宽：

- u64: key 8B + count 4B = 12B
- u128: key 16B + count 4B = 20B
- u256: lo 16B + hi 16B + count 4B = 36B

### 5.1.2 输入检测与 chunk 切分

src/count/reader.rs 会识别三种输入：

- Plain：.fastq / .fq
- Gzip：普通 .fastq.gz / .fq.gz
- Bgzf：带 BGZF 额外头字段的 block-gzip

当前主线路径差异：

- Plain：单 reader 线程读取并按 FASTQ 4 行边界切 chunk。
- Gzip：单 reader 线程使用 MultiGzDecoder 流式解压，再按 FASTQ 4 行边界切 chunk。
- BGZF：先扫描 block 边界，再用 rayon 线程池并行解压 block，之后重新拼成 FASTQ 对齐 chunk。

这意味着：

- 普通 gzip 当前并没有真正的多线程解压。
- BGZF 才会吃到 decomp_threads 的并行收益。

### 5.1.3 启发式执行计划

count 的自动调参由 src/count/mod.rs 的 derive_heuristic_plan 负责。当前主线是 2026-04-05 留下的第一版启发式。

固定常量：

| 常量 | 数值 | 作用 |
| --- | --- | --- |
| TARGET_PARTITION_BYTES | 500 MiB | 自动分桶目标负载 |
| AUTO_MIN_PARTITIONS | 256 | 自动分桶下限 |
| AUTO_MAX_PARTITIONS | 2048 | 自动分桶上限 |
| WORKER_SCRATCH_BYTES | 128 MiB | 每个 worker 预留 scratch 预算 |
| SAMPLE_CHUNK_BYTES | 8 MiB | 采样读取块大小 |
| SAMPLE_MAX_BYTES | 32 MiB | 采样最多读取的解压后字节数 |
| SAMPLE_MAX_READS | 200000 | 采样最多读取的 reads 数 |

启发式的关键公式：

1. 先采样输入，估计 avg_read_len。
2. 根据 key 宽度得到编码系数 E。
3. 估计物理负载量：estimated_partitionable_bytes = input_bytes * clamp(avg_read_len / 10, 4, 24) * E。
4. 目标 partition 数：round_up_power_of_two(estimated_partitionable_bytes / 500MiB)，再夹在 256..2048 之间。
5. 解压线程候选：threads / max(k/10, 1)。
6. queue_depth：Plain 固定 8，其余 clamp(decomp_threads*2, 8, 64)。
7. staging 候选：16 MiB -> 8 MiB -> 4 MiB，且不超过用户给定 io-buf-mb。
8. 如果预算不够，按顺序缩小 staging，再减半 partitions，再减少 worker_threads。

E 的定义：

- u64 路径 E=1
- u128 与 u256 路径 E=2

当前文档必须特别指出：

- 这是经验启发式，不是理论最优解。
- 该版本是目前仓库里对 k31 和 k51 count 真实验证过最有效的一版。

### 5.1.4 FASTQ parser

src/count/parser.rs 当前是一个轻量状态机，只保留 sequence 行：

- Header -> Seq -> Plus -> Qual -> Header
- 只在读到换行符时切状态
- 回调只拿到 sequence 行切片

当前 parser 特性：

- 零拷贝回调，sequence 直接借用 chunk 内存。
- 主线路径按 LF 处理。
- 当前主线没有保留之前实验分支里的 CRLF/CR 兼容实现。

### 5.1.5 rolling k-mer 生成

src/count/kmer.rs 提供三套 rolling iterator：

- KmerIter：u64，适用于 k<=32
- KmerIter128：u128，适用于 33<=k<=64
- KmerIterU256：KmerU256，适用于 65<=k<=128

编码规则：

- A=0
- C=1
- G=2
- T=3
- 其他字符视为 invalid，并重置窗口

canonical 模式下取 min(forward, reverse-complement)。

partition 路由使用 Fibonacci hash：

- u64 直接乘黄金比例常数后取高位。
- u128 对高低 64 位 xor-fold 后再做 Fibonacci hash。
- u256 对四个 64 位段 xor-fold 后再做 Fibonacci hash。

### 5.1.6 worker hot path

src/count/worker.rs 是 count 热路径核心。

当前算法结构：

1. worker 从有界队列取一个解压 chunk。
2. parse_sequences 逐条回调 sequence。
3. rolling 生成 k-mer。
4. 每个 k-mer 根据 bucket_shift 路由到 partition。
5. 先进入 write-combining staging，而不是直接写入 Vec。
6. staging 满 64 个元素后，一次性批量 append 到对应 local bucket。
7. 如果 local_bytes 超过阈值，或全局预算要求 flush，则选择最大的 bucket 执行 flush。

关键内部参数：

| 参数 | 数值 | 含义 |
| --- | --- | --- |
| WCOM_BATCH | 64 | 每个 partition 的 L2 友好写合并批大小 |
| byte_buf 初始容量 | 32 MiB | 编码输出缓冲 |
| 每桶初始容量 | max(worker_budget / partitions / key_size, 4096) | 本地 bucket Vec 预留 |

flush_bucket 的步骤：

1. 先把该 partition 的 write-combining staging 补刷入本地 Vec。
2. 对 bucket 内 key 做排序。
3. 做 RLE 压缩，把重复 key 聚合成 (key, count)。
4. 编码成字节块。
5. 通过共享 writer 追加为一个 chunk。
6. 回收本地内存并更新预算。

### 5.1.7 排序算法

src/count/sorter.rs 当前主线采用 k-mer 感知的 radix sort。

u64 与 u128 的关键点：

- 小数组直接 sort_unstable。
- 普通情况下用 LSD radix sort。
- 大数组在阈值以上转为 MSD radix sort，减少 DRAM 往返。
- 线程本地 scratch buffer 会长期复用，避免每次 flush 都重新 mmap/分配。

关键阈值：

| 参数 | 数值 |
| --- | --- |
| MSD_U64_THRESHOLD | 131072 |
| MSD_U128_THRESHOLD | 131072 |
| MSD_LEAF | 1024 |

u256 路径也有独立的宽 key 排序实现，目标相同：保证每个 bucket flush 前是局部有序的。

### 5.1.8 count 编码与 writer

src/count/writer.rs 负责把局部有序 run 写入 .kite。

编码规则：

- u64/u128 路径优先用 DVLE。
- u256 路径当前直接写 RAW。

DVLE 的具体格式：

1. 第一条记录写定长 key + 定长 count。
2. 后续记录写 delta(key) 的 varint。
3. count 也写 varint。

writer 当前不是“一个 chunk 立即一个 pwrite”，而是 partition 内分段 staging：

1. 每个 partition 有一个 PartitionStaging。
2. 小于 staging_bytes 的 chunk 先进入 partition buffer。
3. buffer 满了再一次性 pwrite 到数据区。
4. 大于 staging_bytes 的 chunk 直接写盘，不走 staging。

关键参数：

| 参数 | 数值 | 含义 |
| --- | --- | --- |
| MIN_STAGING_BYTES | 4 MiB | io-buf-mb 下限 |
| MAX_STAGING_BYTES | 32 MiB | io-buf-mb 理论上限，当前启发式只会用到 4/8/16 MiB |
| write_lanes | ceil(workers/8) 再夹到 2..6 | 并发写 lane 数 |
| staged_high_watermark | staging_bytes * lane_count * 4 | 超过后主动 drain |
| staged_low_watermark | high_watermark / 2 | drain 到该值以下停止 |

finalize 时 writer 会：

1. 刷掉所有 partition staging。
2. 收集所有 chunk 元信息。
3. 写 chunk index。
4. 写 partition table。
5. 写 footer。
6. 回头重写 header 中的 offset 和总计数。

## 5.2 dump 流程

dump 的总体流程是：

1. mmap 打开 .kite。
2. 读取 header、chunk index、partition table、footer。
3. 先并行测量每个 partition 最终 TSV 会输出多少字节。
4. 根据测量结果切分连续 partition 区间，构造 worker plan。
5. 每个 worker 对自己的 partition 范围逐个做多路归并。
6. 把结果格式化成 TSV slab，提交给异步 pwrite writer。
7. 所有 slab 写完后关闭 writer。

### 5.2.1 读取 .kite

src/dump/reader.rs 直接 mmap 整个 .kite 文件，并做以下工作：

- 解析 128B header。
- 解析 partition table。
- 解析 chunk index。
- 校验 footer magic。
- 建立 partition_id -> Vec<ChunkLocation> 映射。

ChunkLocation 记录：

- file_offset
- payload_len
- codec
- row_count
- key_min / key_max

### 5.2.2 预估输出字节数

src/dump/merge.rs 里的 measure_partition_output 会在正式 dump 前先测量每个 partition 的最终 TSV 字节数。

原因：

- 这样可以提前知道最终输出文件长度，直接 set_len 预分配。
- 可以用“最终输出字节数”而不是“chunk 数量”做 worker 负载均衡。
- 可以为每个 partition 分配精确写偏移，避免最终串接步骤。

算法分两类：

- u64/u128：建立 loser tree merger，逐个 next_min 聚合并估算 TSV 行长。
- u256：把该 partition 的所有 entry 全读出到内存，排序后归并估算。

这里需要特别注意：

- 当前 dump 的 loser tree 主路径主要覆盖 u64/u128。
- u256 路径当前是“全量装入 + 排序”的特殊处理，不是 loser tree streaming 版本。

### 5.2.3 worker plan 切分

src/dump/mod.rs 的 build_worker_plans 使用动态规划，把连续 partition 区间切给不同 worker，目标是最小化最大 shard 输出字节数。

也就是说它不是简单均分 partition 数，而是均衡“预计输出字节”。

输出的 WorkerPlan 结构包含：

- start_partition
- end_partition
- total_output_bytes

### 5.2.4 loser tree 多路归并

src/dump/loser_tree.rs 的 LoserTreeMerger 是 dump 的主算法核心。

输入：

- 多个 run iterator，每个 iterator 提供该 partition 下一个有序 (key, count)

输出：

- 反复取全局最小 key
- 对所有同 key stream 做累加
- 返回 (key, accumulated_count)

复杂度：

- 初始化约 O(n)
- 每弹出一个元素约 O(log n)

它比 BinaryHeap 更适合 fan-in 较大的外部归并。

### 5.2.5 解码与格式化

src/dump/decoder.rs 负责把 key 还原成 DNA 字符串，再格式化成 TSV。

当前做法：

- 用 256 项 lookup table，每次解 4 个碱基。
- count 使用 itoa::Buffer 复用，避免每行重新 format。
- TSV 行格式固定为：kmer TAB count NEWLINE。

即最终输出没有表头，每行都是：

```text
ACGT...	123
```

### 5.2.6 dump writer

src/dump/writer.rs 当前主线是固定 slab + 有界队列 + 后台 pwrite 线程。

关键常量：

| 参数 | 数值 |
| --- | --- |
| DUMP_SLAB_BYTES | 32 MiB |
| DUMP_QUEUE_BYTES | 256 MiB |
| 默认 writer_threads | max(1, threads/6) |

行为：

1. 创建输出文件并 set_len(total_tsv_bytes)。
2. 创建有界队列，容量约为 queue_bytes / slab_bytes。
3. worker 产生一个 slab 后，直接 submit(offset, bytes)。
4. 后台线程执行 write_at(offset, bytes)。
5. slab 用 SlabPool 复用，减少 Vec 分配。

当 output 为 - 时，不创建异步 writer，而是直接串行写 stdout。

## 6. .kite 文件格式

当前 .kite 是小端序二进制格式，版本号为 4，magic 为 KX2!。

总体布局如下：

```text
[128B Header]
[Data Area: partition chunks payload]
[Chunk Index Entries]
[Partition Table]
[24B Footer]
```

### 6.1 Header

header 固定 128 字节，字段如下：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| magic | [u8;4] | 固定为 KX2! |
| version | u16 | 当前为 4 |
| k | u8 | k 值 |
| canonical | u8 | 0 或 1 |
| partitions | u32 | partition 数 |
| key_width | u16 | 64 / 128 / 256 |
| codec_mask | u16 | 文件允许的 codec 掩码 |
| checksum_mode | u32 | 当前固定 0 |
| chunk_index_offset | u64 | chunk index 起始偏移 |
| partition_table_offset | u64 | partition table 起始偏移 |
| footer_offset | u64 | footer 起始偏移 |
| total_chunks | u64 | chunk 总数 |
| total_rows | u64 | 全文件总行数 |
| reserved | 68B | 保留 |

### 6.2 Data area

data area 是实际的 run payload 区。每个 partition 下可以有多个 chunk，每个 chunk 在 chunk index 里描述它的位置和编码信息。

当前主线实际编码：

- u64 / u128 run：DVLE
- u256 run：RAW

### 6.3 Chunk index entry

每个 ChunkIndexEntry 固定 128 字节：

| 字段 | 类型 |
| --- | --- |
| partition_id | u32 |
| codec | u32 |
| file_offset | u64 |
| encoded_bytes | u64 |
| raw_bytes | u64 |
| row_count | u64 |
| key_min_lo | u128 |
| key_min_hi | u128 |
| key_max_lo | u128 |
| key_max_hi | u128 |
| reserved | 24B |

用途：

- dump 能快速定位每个 run 的 payload。
- 以后如果做更激进的范围跳过、分层 merge，可继续利用 key range。

### 6.4 Partition entry

每个 PartitionEntry 固定 40 字节：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| first_chunk_idx | u64 | 该 partition 第一个 chunk 在 chunk index 中的下标 |
| chunk_count | u64 | chunk 数 |
| total_rows | u64 | 该 partition 总逻辑行数 |
| data_offset | u64 | 该 partition 最早数据偏移 |
| data_bytes | u64 | 该 partition 总编码字节数 |

### 6.5 Footer

footer 固定 24 字节：

| 字段 | 类型 |
| --- | --- |
| total_chunks | u64 |
| total_rows | u64 |
| magic_tail | [u8;8] = KX2FOOT! |

## 7. 模块职责总表

## 7.1 顶层模块

| 模块 | 职责 | 是否在当前主线使用 |
| --- | --- | --- |
| src/main.rs | 程序入口，解析 CLI 并分发到 count/dump | 是 |
| src/cli.rs | 定义 count/dump 参数 | 是 |
| src/stats.rs | count/dump 统计与 test-mode 报告 | 是 |
| src/runtime.rs | 早期实验的 runtime 信息和限制逻辑 | 否 |

## 7.2 format 模块

| 模块 | 职责 |
| --- | --- |
| src/format/header.rs | 128B header 的读写 |
| src/format/bucket_dir.rs | partition table、chunk index、footer 的读写 |
| src/format/run_block.rs | DVLE/RAW 编码与 varint 读写 |
| src/format/mod.rs | 汇总导出格式结构 |

## 7.3 count 模块

| 模块 | 职责 | 备注 |
| --- | --- | --- |
| src/count/mod.rs | count 总流程、类型分派、启发式计划 | 主入口 |
| src/count/reader.rs | 检测 plain/gzip/BGZF | 主线路径 |
| src/count/decompress.rs | plain/gzip 流式 chunk reader | 主线路径 |
| src/count/bgzf.rs | BGZF block 扫描与并行解压 | BGZF 输入时使用 |
| src/count/parser.rs | FASTQ 状态机，仅提取 sequence 行 | 主线路径 |
| src/count/kmer.rs | rolling k-mer、canonical、bucket hash | 主线路径 |
| src/count/sorter.rs | radix sort 与 RLE 辅助 | 主线路径 |
| src/count/worker.rs | count 热路径与 flush 逻辑 | 主线路径 |
| src/count/writer.rs | 并发 staging writer 与 finalize | 主线路径 |
| src/count/memory.rs | 内存预算跟踪 | 主线路径 |
| src/count/router.rs | 说明 bucket 路由已内联到 KmerWord::to_bucket | 仅占位 |

## 7.4 dump 模块

| 模块 | 职责 | 备注 |
| --- | --- | --- |
| src/dump/mod.rs | dump 总流程、预估输出、worker plan、异步 writer 装配 | 主入口 |
| src/dump/reader.rs | mmap 读取 .kite 并索引各 partition chunk | 主线路径 |
| src/dump/merge.rs | 预估输出、构建 merger、流式产出 slab | 主线路径 |
| src/dump/loser_tree.rs | loser tree 多路归并 | 主线路径 |
| src/dump/decoder.rs | k-mer 解码与 TSV 行格式化 | 主线路径 |
| src/dump/writer.rs | slab pool、有界队列、异步 pwrite | 主线路径 |
| src/dump/run_cursor.rs | 早期单 run 迭代器 | 当前主线未直接使用 |
| src/dump/worker.rs | 未来扩展占位 | 当前主线未直接使用 |

## 8. 复现步骤

## 8.1 最小可运行复现

```bash
cd /eight_TB/BigMQ/Users/Hua/kmer_counter/kite
mkdir -p temp
cargo build --release

./target/release/kite count \
  -k 21 \
  -i test/test.fastq.gz \
  -o temp/test_k21.kite \
  --threads 48 \
  --max-memory-gb 64 \
  --chunk-kmers 8000000 \
  --mode fast \
  --canonical

./target/release/kite dump \
  -i temp/test_k21.kite \
  -o temp/test_k21.tsv \
  --threads 48
```

检查输出：

```bash
ls -lh temp/test_k21.kite temp/test_k21.tsv
head temp/test_k21.tsv
```

## 8.2 带 test-mode 的诊断复现

如果你要看内部统计和启发式计划：

```bash
./target/release/kite count \
  -k 31 \
  -i test/HG002_sample.fastq.gz \
  -o temp/hg002_k31.kite \
  --threads 48 \
  --max-memory-gb 64 \
  --chunk-kmers 8000000 \
  --mode fast \
  --canonical \
  --test-mode

./target/release/kite dump \
  -i temp/hg002_k31.kite \
  -o temp/hg002_k31.tsv \
  --threads 48 \
  --test-mode
```

## 8.3 KMC 对比 benchmark

当前推荐脚本：

```bash
cd /eight_TB/BigMQ/Users/Hua/kmer_counter/kite
bash benchmark/run_kmc_kite_benchmark.sh
```

默认行为：

- 输入：test/HG002_sample.fastq.gz
- 线程：48
- 内存：64GB
- K 列表：21 31 51
- 不带 --test-mode
- 同时运行 KMC count、KMC dump、kite count、kite dump
- 输出 summary.tsv 和每个步骤的 stdout/stderr/time 日志

支持环境变量覆盖，例如：

```bash
THREADS=32 MEM_GB=96 K_LIST="21 31" RUN_TAG=my_run bash benchmark/run_kmc_kite_benchmark.sh
```

结果目录默认在：

```text
benchmark/results/<RUN_TAG>/
```

## 9. 已验证的历史运行时间

下面这些数字来自仓库内已保存的阶段记录，主要用于说明“当前主线为什么长这样”。没有写明的项，表示仓库里没有留下足够可靠的单独日志，不应该臆造。

统一上下文：

- 数据集：HG002_sample.fastq.gz
- 线程：48
- 内存预算：64GB
- 记录时间：2026-04-05 附近

### 9.1 当前主线相关的已验证数字

| 数据集 | k | 阶段 | 时间 | 说明 |
| --- | --- | --- | --- | --- |
| HG002 | 21 | count | 97.47s | test-mode，回归到稳定主线后验证 |
| HG002 | 21 | dump | 52.31s | test-mode，稳定 dump 基线 |
| HG002 | 21 | count+dump | 149.78s | test-mode，总和 |
| HG002 | 31 | count | 104.8s | 首版启发式 count 记录 |
| HG002 | 51 | count | 370.7s | 首版启发式 count 记录 |

### 9.2 对照数字

| 数据集 | k | 阶段 | 时间 | 说明 |
| --- | --- | --- | --- | --- |
| HG002 | 31 | count | 177.7s | 启发式引擎前的旧值 |
| HG002 | 51 | count | 438.7s | 启发式引擎前的旧值 |

### 9.3 无法在本文硬写死的数字

以下数字目前没有在仓库里留下足够可靠的最终日志，因此不在本文伪造：

- HG002 k31 的稳定 dump 单独时间。
- HG002 k51 的稳定 dump 单独时间。
- 当前主线在“不带 test-mode”下的最新完整 21/31/51 汇总。

这些应通过 benchmark/run_kmc_kite_benchmark.sh 自行重新跑出。

## 10. 当前主线的已知限制

这些限制必须写清楚，否则别人很容易误判项目状态：

1. --progress 目前是保留接口，当前主线没有真正接入进度条或周期性打印。
2. count 的 --mode 目前不是算法分支开关，只是保留的模式标签。
3. 普通 gzip 目前仍是单 reader 线程解压；只有 BGZF 才有并行 block 解压。
4. max-memory-gb 目前是预算控制，不是严格进程级硬上限。
5. parser 当前主线按 LF 处理，之前的 CRLF/CR 兼容实验没有保留在主分支。
6. dump 的 loser tree 主路径主要覆盖 u64/u128；u256 当前是单 partition 全量装入排序路径。
7. run_kite_tests.sh 是旧脚本，不要按它的参数表判断当前 CLI。

## 11. 新开发者的最短上手路径

如果你第一次接手这个仓库，建议按下面顺序走：

1. 先 cargo build --release，确认当前主线能编过。
2. 用 test/test.fastq.gz 跑一遍 k21 的 count 和 dump，确认功能闭环。
3. 用 --test-mode 跑一遍 HG002 k21，观察自动分桶、staging、worker 预算和 finalize 统计。
4. 用 benchmark/run_kmc_kite_benchmark.sh 跑完整 21/31/51，对比 KMC 与 kite。
5. 真正开始改性能前，先判断你动的是 count 热路径、dump 归并路径，还是 .kite 格式本身。

## 12. 一句话总结当前主线

当前 kite 主线是一个面向高吞吐 count + dump 总时间优化的两阶段 sorting-based k-mer counter：count 负责把 FASTQ 流式切块、rolling 生成 canonical k-mer、按 partition 分桶并写成 .kite runs，dump 负责 mmap 读取这些 runs，并以 loser tree 为核心对 partition 做多路归并输出 TSV；当前最值得信任的版本是“首版启发式 count + 稳定 dump 基线”。