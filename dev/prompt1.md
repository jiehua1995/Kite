你现在要在**/eight_TB/BigMQ/Users/Hua/kmer_counter/kite**里，从零开始写一个 Rust 软件，项目名叫 **kite**。所有的代码都放到这里，临时文件都放在/eight_TB/BigMQ/Users/Hua/kmer_counter/kite/temp，不允许操作这个文件夹以外的文件，除非软链接过去的。

你必须自己创建：

* `Cargo.toml`
* `src/` 目录
* 必要的模块文件
* 可运行的 `cargo build --release`
* 可执行的 `kite count` 和 `kite dump`
* 每个子命令一个文件夹，即src/{command name}，子命令间只共享.kite文件的格式定义，其他代码尽量各自独立，以便单独进行迭代和优化。

我已经在 `test/` 文件夹里放好了两个测试数据集：

* `test/test.fastq.gz`：小样本
* `test/HG002_sample.fastq.gz`：大样本

你的任务不是讨论，不是写设计草稿，不是只写框架，而是**真正把程序写出来，并能在这两个数据集上运行、验证、测性能、继续迭代优化**。

---

# 一、总目标

这个软件的目标只有一个：

**极限追求速度，尽可能充分利用高性能计算机的 CPU、内存和磁盘带宽，构建一个高速的 k-mer counting / dumping 软件。**

优先级严格如下：

1. **速度**
2. **正确性**
3. **资源利用率（CPU / 内存 / I/O）**
4. **代码清晰**
5. **未来扩展性**

不要为了“优雅”牺牲速度。
不要为了“抽象好看”牺牲热路径性能。
不要为了“未来可能支持很多功能”拖慢当前的 `count` 和 `dump`。

本轮只做两个子命令：

* `kite count`
* `kite dump`

其他功能一律不做。
不要实现 `info/query/histo/merge/profile/sketch/citation`。
先把 `count` 和 `dump` 打到极致。

---

# 二、你必须采用的总体架构

这个软件必须采用**两阶段外排型架构**：

## Phase 1: `kite count`

目标不是“直接得到全局最终有序结果”，而是：

* 高速读取输入
* 高速解压与 FASTQ 解析
* rolling 生成 canonical k-mer
* 按 bucket / partition 分桶
* 在内存中形成尽可能大的批次
* 对批次做局部排序与 RLE 压缩
* 直接把每个 bucket 的局部 run 写入 `.kite` 二进制文件

**count 阶段尽量避免做全局最终合并。**

count 的目标是：

* 最大化摄入吞吐
* 最大化 CPU 利用率
* 最大化内存使用效率
* 最小化小 flush、小文件、小 run 爆炸
* 为 dump 提供适合做高效多路归并的 runs

## Phase 2: `kite dump`

dump 阶段负责：

* 读取 `.kite`
* 按 bucket 找到对应 runs
* 对每个 bucket 做多路归并
* 使用 **loser tree** 作为主归并数据结构
* 将相同 key 的 count 累加
* 恢复为字符串 k-mer
* 批量写出 TSV

**dump 可以承担大量整理与合并工作。**

我不在乎 count 和 dump 谁做更多计算。
我只在乎：

**count + dump 总时间尽可能短。**

你已经知道 loser tree 会非常快，而且能把线程跑满，所以 dump 阶段必须把 loser tree 当成主设计，而不是可选玩具。

---

# 三、为什么这样设计

这个设计不是拍脑袋，而是基于近年的高性能方法：

1. 近年的 sorting-based k-mer counting 研究表明，**排序主线**在缓存友好性、内存可控性和并行扩展性上通常优于细粒度哈希表更新，特别是在大规模数据与多核环境下。
2. loser tree 仍然是外部多路归并的高性能核心结构，比普通二叉堆更适合高 fan-in merge。
3. 随着并行 gzipped FASTQ parsing、SIMD minimizer、分布式 sorting-based counting 的进展，现代方案越来越强调：

   * 大块输入
   * 顺序访问
   * bucketed runs
   * 后置多路归并
   * 减少随机内存访问
   * 减少全局锁和热点哈希表

因此，你必须围绕：

* **sorting-based**
* **bucketed binary runs**
* **dump-stage loser tree merge**
* **high-memory, high-throughput pipeline**
  来实现整个系统。

---

# 四、命令行参数要求

不要模仿 KMC 参数风格。
只做少量但真正有效的参数。

## `kite count`

必须支持：

* `-k, --kmer <INT>`
* `-i, --input <PATH>`
* `-o, --output <PATH>`
* `--threads <INT>`
* `--partitions <INT>`
* `--chunk-kmers <INT>`
* `--max-memory-gb <FLOAT>`
* `--canonical <true|false>`
* `--progress`
* `--test-mode`

## `kite dump`

必须支持：

* `-i, --input <PATH>`
* `-o, --output <PATH>`
* `--threads <INT>`
* `--progress`
* `--test-mode`

参数语义必须严格生效：

### `--threads`

必须真正影响：

* 解压线程数
* count worker 数
* dump merge worker 数
* CPU 利用率

不能是名义参数。

### `--partitions`

用于：

* bucket 数
* 负载分散
* run 组织
* dump 并行度分配

不要写死固定 partition 数。

### `--chunk-kmers`

这是性能核心旋钮。必须真实影响：

* 每批处理大小
* flush 频率
* run 大小
* 排序粒度
* RLE 粒度
* 总体吞吐

### `--max-memory-gb`

必须真实影响：

* block pool 深度
* buffer 大小
* shard 容量
* flush 策略
* spill 时机
* run 数量
* 中间 I/O 压力

绝不能是装饰参数。

---

# 五、输入与解压架构

支持：

* `.fastq`
* `.fq`
* `.fastq.gz`
* `.fq.gz`
* BGZF / block-gzip 风格的 FASTQ.GZ

## 设计要求

### 1. 不允许先整文件解压到磁盘

必须流式处理。

### 2. 输入必须大块化

不要按行慢吞吞处理。
要按大块读入，解压到可复用的大 buffer 中。

### 3. FASTQ 记录边界对齐

解压后的大块在分发给 worker 前，必须按 FASTQ 记录边界对齐，避免 worker 解析时跨 record 出错。

### 4. 普通 gzip 与 BGZF 尽量分流优化

* 普通 gzip：高吞吐流式解压
* BGZF：如果能识别 block 边界，则做 block 级并行解压
* 如果短期来不及把 BGZF 路线做到极致，也至少要保证不明显拖后腿

### 5. buffer pool

必须实现可复用 block pool / buffer pool，避免频繁分配释放。

### 6. bounded queue

输入层与计算层之间用 bounded batch queue，但不能太窄。
队列深度必须受 `--max-memory-gb` 影响。

---

# 六、FASTQ 解析要求

FASTQ 按 4 行记录处理，但只保留 sequence 行。

不要做这些无用事情：

* 不保留 read name
* 不保留 quality
* 不创建无意义 String
* 不做额外元数据封装

## 要求

* 使用字节切片解析
* 尽量零拷贝或少拷贝
* 使用轻量状态机
* 对非法 FASTQ 给出明确错误
* 不允许用高层慢速文本 API 做热路径解析

---

# 七、k-mer 生成算法要求

第一版重点优化 `k <= 32`。

## 必须实现

1. 2-bit 编码
2. rolling forward 编码
3. rolling reverse-complement 编码
4. canonical 模式下取较小者
5. 遇到 N / 非 ACTG 时重置窗口
6. 仅当窗口满 `k` 时产出合法 k-mer

## 热路径要求

* 绝对不能 substring
* 绝对不能每个 k-mer 重新扫描整个片段
* 尽量使用整数位运算
* 尽量减少分支
* 尽量让路径适合 SIMD / 编译器自动向量化

## 扩展要求

架构上为 `33 <= k <= 64` 预留 `u128` 路径，但第一版主要把 `u64` 打磨到极致。

---

# 八、count 的核心算法设计

这是整个项目的核心。

## 总原则

* worker-local
* 大批量
* 排序主线
* 少同步
* 少小 flush
* 少小 run
* 少随机 I/O
* 大块顺序写

## 工作流

1. 输入块进入 count worker
2. worker 解析 sequence
3. rolling 生成 canonical k-mer
4. 将每个 k-mer 路由到某个 partition / bucket
5. 每个 worker 持有本地 bucket buffers
6. 当某 bucket buffer 或全局内存压力达到阈值时，才触发批处理
7. 批处理过程：

   * 把该批次转为连续整数数组
   * 对整数数组做排序（优先 radix / integer sort 思路）
   * 排序后做 RLE 压缩
   * 形成局部 `(key,count)` run
   * 追加写入 `.kite` 中该 bucket 的 run 区域或 run 日志
8. count 阶段不强求全局最终合并
9. count 的职责是把输入高速转成适于 dump merge 的 bucket runs

---

# 九、排序策略要求

默认主线必须是：

**整数排序 + RLE**

而不是直接在热路径中维护超大 HashMap。

## 为什么

* 排序更 cache-friendly
* 顺序访问更适合现代 CPU 和内存带宽
* 与 bucketed runs 更匹配
* 更适合后续 dump merge
* 更适合高吞吐的外排式系统

## 具体要求

* 优先为 `u64` 实现高性能 radix / integer sort 路线
* 可允许在小批次下切换到 std sort 或更轻量路径
* 但不要在热路径上无脑用通用 HashMap 计数

## RLE

排序后必须做线性扫描 RLE：

* 相邻相同 key 合并 count
* 形成紧凑 run
* 减少 run 大小
* 减少 dump merge 压力

---

# 十、partition / bucket 设计

`--partitions` 控制 bucket 数。

## 要求

1. 不允许固定神数
2. 每个 worker 有自己的本地 bucket buffers
3. 尽量避免多个线程争抢一个全局结构
4. bucket 是 count 与 dump 两阶段共同的组织单元
5. `.kite` 文件索引必须围绕 bucket 组织
6. dump 阶段按 bucket 并行

## 你必须记录并输出：

* 每个 bucket 的 k-mer 数
* hottest buckets
* 每个 bucket 的 run 数
* avg / max run size
* flush 次数分布


## 目标

* 真正把几十 GB 内存吃起来
* 显著减少小批次、小排序、小写盘
* 不允许机器有大量空闲内存而程序只用 8GB 左右

---

# 十三、`.kite` 二进制格式设计

这是一个从零设计的新格式，只为高性能 `count` / `dump` 服务。

## 设计目标

* 显式
* 易 debug
* 适合 bucketed runs
* 适合 dump 并行
* 适合 loser tree merge
* 不追求“一步到位支持所有未来功能”

## 建议文件布局

### 1. Header

包含：

* magic
* version
* k
* canonical flag
* key representation (`u64` first)
* partition count
* mode flags
* total bucket count
* 可选压缩标记

### 2. Bucket directory

记录每个 bucket 的：

* run 数
* bucket 索引偏移
* bucket 数据偏移
* bucket 总 entry 数
* bucket 总字节数

### 3. Run blocks

每个 run 是一段局部数据块，保存：

* run header
* entry count
* 是否排序
* 是否 RLE
* `(key,count)` 数组
* 可选 block compression

### 4. Footer / index tail

可包含：

* 总 bucket 数
* 总 run 数
* index offset
* checksum（可选）

## 原则

* dump 能直接按 bucket 定位 runs
* bucket 内多个 run 能直接进入 loser tree merge
* 不要设计成只能全局顺序扫描
* 也不要设计成为了 query 而严重牺牲写入吞吐

---

# 十四、count 写出策略

count 阶段写出必须是：

* 大块顺序写
* 尽量减少小 append
* 尽量减少碎片
* 尽量减少小文件

## 强制要求

* 不允许每次小 flush 都单独创建一个外部临时小文件风暴
* 优先写进统一 `.kite` 容器格式，或者受控的少量中间 run 区
* 如果中间阶段确实需要临时文件，也必须受控且大块化

---

# 十五、dump 的核心算法

dump 必须使用 **loser tree** 作为主归并结构。

## 工作流

1. 读取 `.kite` header 和 bucket directory
2. 选择 bucket 并行处理
3. 对于某个 bucket：

   * 打开该 bucket 的多个 runs
   * 为每个 run 创建顺序读取游标
   * 若 run 已按 key 排序，则直接参与 loser tree merge
   * 若 run 仅局部压缩但未全局有序，则先桶内排序再 merge（尽量避免这种情况过多）
4. 使用 loser tree 做 k-way merge
5. 相同 key 连续出现时累加 count
6. 将合并后的结果转换为字符串 k-mer
7. 批量写出 TSV

## 为什么必须用 loser tree

因为我已经确认 loser tree 在这里非常快，且能把线程跑满。
你不需要再犹豫 heap vs loser tree。
主设计就是 loser tree。

## 要求

* 按 bucket 并行
* bucket 内 loser tree merge
* 流式输出
* 不允许把整库全量载入内存再 dump
* 批量字符串格式化
* 批量写磁盘

---

# 十六、k-mer 字符串恢复与 TSV 输出

dump 阶段的另一个热点是：

* key -> DNA string
* 数字 -> ASCII
* 文本写出

## 要求

1. 高效恢复 k-mer 字符串
2. 不要低效 push 字符串
3. 尽量复用输出 buffer
4. count 数字转字符串尽量用快速整数格式化
5. 批量写出 TSV

每行格式固定：

`<kmer>\t<count>\n`

---

# 十七、线程模型要求

必须显式设计线程，不允许“名义多线程、实际单线程瓶颈”。

## count 推荐线程分工

* reader / decompressor 线程组
* record align / dispatcher
* count worker 线程组
* run writer 线程组（必要时）
* 统计 / telemetry 可轻量附带

## dump 推荐线程分工

* bucket scheduler
* bucket merge worker 线程组
* loser tree merge 内部逻辑
* writer 线程或批量 writer

## 原则

* 避免全局锁热点
* 避免一个线程串行做所有写入
* 避免过窄队列
* 避免小任务粒度导致调度浪费
* 批处理优先于细粒度消息传递

---

# 十八、`--test-mode` 必须实现，而且必须很详细

这是硬要求，不是可选项。

## `kite count --test-mode` 必须输出

* input file
* detected compression type
* k
* canonical
* threads
* partitions
* chunk-kmers
* max-memory-gb
* compressed bytes read
* decompressed bytes produced
* reads processed
* bases processed
* kmers generated
* valid kmers generated
* per-bucket load distribution
* hottest buckets
* flush 次数
* avg flush size
* max flush size
* run 数
* avg run size
* spill / run bytes written
* parse time
* decompress time
* kmer generation time
* sort time
* rle time
* write time
* total count wall time
* peak RSS
* 近似 CPU busy %

## `kite dump --test-mode` 必须输出

* input .kite path
* bucket 数
* total runs
* per-bucket run count
* merge fan-in 分布
* loser tree comparisons / 或近似统计
* decode time
* merge time
* text formatting time
* write time
* total rows output
* total dump wall time
* bytes read
* bytes written
* peak RSS
* 近似 CPU busy %

---

# 十九、正确性要求

必须确保：

* 小样本结果正确
* count -> dump 闭环正确
* canonical 正确
* N / 非 ACTG 处理正确
* 单线程与多线程结果一致
* 不同 chunk 大小结果一致
* 不同 partition 数结果一致
* 不同内存预算结果一致

## 你必须写测试

* 2-bit 编码测试
* reverse-complement 测试
* canonical 测试
* rolling k-mer 测试
* FASTQ record 解析测试
* `.kite` header/index 读写测试
* count -> dump 端到端测试
* 小样本 golden test

---

# 二十、性能开发顺序

不要一上来写一堆没法跑的“大而全”代码。
按下面顺序推进，并且每一步都要能运行：

## Step 1

创建工程骨架：

* `Cargo.toml`
* `src/main.rs`
* `src/cli.rs`
* 模块目录
* 最小 CLI

## Step 2

实现最小正确版：

* 单线程
* `.fastq` / `.fastq.gz`
* `k <= 32`
* rolling k-mer
* canonical
* 基础 `.kite` 写入
* 基础 dump 输出 TSV

## Step 3

加入两阶段 bucketed run 架构：

* partition
* bucket buffers
* 排序 + RLE
* `.kite` bucket index

## Step 4

加入多线程 count：

* block pipeline
* worker-local bucket buffers
* bounded queue
* 真正的多核利用

## Step 5

加入多线程 dump：

* bucket parallelism
* loser tree merge
* 批量 TSV 输出

## Step 6

加入完整 `--test-mode`

## Step 7

用我给的数据集做 benchmark：

* `test/test.fastq.gz`
* `test/HG002_sample.fastq.gz`

并记录：

* count 时间
* dump 时间
* count + dump 总时间
* CPU 利用率
* peak RSS
* run 数
* hottest bucket
* flush 次数

## Step 8

基于数据继续迭代
只允许根据 test-mode 输出决定优化方向，不允许盲改。

---

# 二十一、你可以用的优化方向

以下优化允许采用，但必须在 profile / test-mode 数据证明有价值时再深入：

* mimalloc / jemalloc 作为全局分配器
* 对齐到 cache line，避免 false sharing
* SIMD 加速碱基映射
* 预取（prefetch）
* NUMA 亲和性
* io_uring 或更激进 I/O 路线
* 更高性能 radix sort
* 更均匀的 minimizer / random minimizer / super-k-mer 分桶
* 可选 block compression

但请记住：
**第一版先跑通高性能主架构，再逐步把这些刀磨利。**

---

# 二十二、明确禁止的事情

1. 不要先整文件解压到磁盘
2. 不要热路径里频繁创建 String
3. 不要 substring 生成 k-mer
4. 不要全局大 HashMap 直接硬顶所有计数
5. 不要让 `--threads` 成为假参数
6. 不要让 `--max-memory-gb` 成为假参数
7. 不要让 count 阶段大量小 flush、小 run 爆炸
8. 不要创建海量小临时文件
9. 不要让 dump 把整库一次性读进内存
10. 不要没有 test-mode 数据就乱改
11. 不要为了未来功能拖慢当前 count/dump
12. 不要只写框架不写真正热路径
13. 不要只给伪代码
14. 不要停留在设计讨论不落地实现

---

# 二十三、交付方式

你必须像真正的工程师一样工作。每一轮输出都要包含：

1. 当前代码改了哪些文件
2. 现在的架构是什么
3. 为什么这样设计
4. 运行了什么命令
5. 小样本结果是否正确
6. 大样本目前跑到哪里
7. `--test-mode` 给出了什么关键数据
8. 下一轮要改什么，为什么

你的工作不是“说你会做”，而是**在当前文件夹里真正把程序写出来**。

---

# 二十四、现在立刻执行

请按以下顺序立即开始，不要先问我问题：

1. 创建 `Cargo.toml`
2. 创建 `src/`
3. 设计模块结构
4. 写最小可运行 CLI
5. 实现最小正确版 `count`
6. 实现最小正确版 `dump`
7. 跑小样本验证
8. 然后再逐步引入 bucketed runs、多线程、loser tree、test-mode 和性能优化

记住：
这个软件的核心不是“优雅”，而是：

**暴力利用硬件，把 `count` 和 `dump` 都做到快。**
