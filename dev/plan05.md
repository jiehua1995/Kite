# Plan05: 当前性能回退分析与恢复计划

## 1. 当前问题

当前仓库已经恢复到 stage01 描述的总体架构：

1. `count` 仍是 direct-write `.kite`。
2. `dump` 仍是 partition 内 loser tree，多线程各写 shard 文件，最后 `sendfile` 合并最终 TSV。
3. `cargo build --release` 当前可以通过。

但是当前真实性能已经明显偏离之前对话里记录的快版本，主要表现在：

1. 当前 `kite` 的 `k21` 在主盘写出时明显慢于之前的 stage01 / prompt5 口径。
2. 当前 `kite dump` 在 HG002 上表现出“前期 CPU 尚可，后期 CPU 明显下降”的现象，说明后半程并行度掉得很厉害。
3. 当前机器上做对比时，KMC 和 Kite 都比部分历史记录慢，说明除了代码回退，也存在运行环境或 I/O 状态差异。

## 2. 已确认的事实

### 2.1 stage01 架构事实

从 `dev/stage01.md` 可以确认：

1. `dump` 的正确主路径就是“每个线程顺序写 shard 文件，最后 `sendfile` 合并”。
2. `dump` 的 test-mode 关注项就是 `writer_io_ms`、`concat_ms`、`cpu_util_pct`、`hottest_shard_bytes` 等。
3. `count` 当前仍是全局 `next_data_offset.fetch_add(...)` 的 direct-write，不是物理 shard-native。

### 2.2 近期回退中已经确认的现象

根据这轮排查，已经确认了几个关键现象：

1. 我之前一度把 `fast` 模式改成了轻 zstd，这和 `stage01.md` 里“`fast/balanced/compact => 0/1/5`”的文字不一致，文档与历史日志本身存在冲突。
2. 我后来又把 `chunk-kmers` 恢复成 `8_000_000`，BGZF 线程恢复成 `decomp=16 / workers=31`，这是向 stage01 历史日志靠拢的修正。
3. 用主盘输出 `.kite` 时，`k21 count` 最近一次实测约为：
   1. `total wall time ≈ 127.3s`
   2. `finalize_ms ≈ 70300`
   3. `writer_io_ms ≈ 288525`
   4. `cpu_util_pct ≈ 631%`
4. 同样的代码、同样的输入，如果把 `.kite` 输出到 `/dev/shm`，`k21 count` 立刻变成：
   1. `total wall time ≈ 51.1s`
   2. `finalize_ms ≈ 2`
   3. `writer_io_ms ≈ 60785`
   4. `cpu_util_pct ≈ 1204%`

这个对比说明：

1. 当前 count 的主要问题不是纯计算不够快。
2. 真正的大头是 `.kite` 落盘路径和文件系统交互。
3. 也就是说，当前 count 的主瓶颈已经偏向 I/O，而不是排序或 RLE 算法本身。

### 2.3 dump 当前的已知症状

当前 dump 的核心症状不是“立刻报错”，而是：

1. HG002 `k21 dump` 启动后，最开始 CPU 占用还可以。
2. 跑到中后段 CPU 会明显掉下去。
3. 肉眼可见 wall time 已经超过之前对话里那条快版本。

这说明至少存在下面一种或多种情况：

1. shard worker 后期出现严重长尾。
2. `writer_io_ms` 与 `concat_ms` 在当前磁盘状态下放大了。
3. 当前生成出来的 `.kite` chunk 形态与之前快版本不同，导致 dump 的 merge / format / write 阶段负载分布恶化。

## 3. 当前最可能的问题根源

### 3.1 count 落盘路径回退

这是当前最明确的问题。

从主盘输出与 `/dev/shm` 输出的差距看，当前 direct-write `.kite` 在真实磁盘上的写出路径已经成为主要瓶颈。

可能原因：

1. 数据区大量 `write_at` 随机写与尾部元数据写出在当前文件系统上比 stage01 时更慢。
2. `sync_all()` 或文件系统缓存回写在当前状态下代价更高。
3. 历史快版本可能跑在更干净的盘面或更有利的缓存状态上，而当前实验环境有较重 I/O 干扰。

### 3.2 dump 后半程并行度下降

当前 dump 症状像“开始并行，后面只剩少量 shard 在工作”。

可能原因：

1. shard 近似均衡只看 `partition.data_bytes`，但真实成本其实还受 chunk 数、key 分布、文本化成本影响。
2. 即使 shard 的输入字节量接近，最终 TSV 的文本输出体积也未必接近。
3. 当前 `sendfile` 合并是串行的，最终 concat 仍可能造成明显尾段。

### 3.3 文档与代码/日志语义不一致

目前至少有一处已暴露：

1. `stage01.md` 写的是 `fast/balanced/compact => zstd_level = 0/1/5`。
2. 历史日志里却出现了 `mode=fast, zstd-level=1`。

这意味着我们现在不能只靠文档文字恢复性能基线，必须同时核对：

1. 实际源码。
2. 当时的运行命令。
3. 当时的 test-mode 输出。

## 4. 目前想到的解决方案

### 方案 A：先做“可重复、单日志”的基准框架

先不要继续凭记忆手工敲命令，而是固定一份脚本：

1. 严格串行执行 `kmc count -> kmc dump -> kite count -> kite dump`。
2. 每一步都用 `/usr/bin/time -v`。
3. 所有 stdout/stderr 都追加到一个总日志文件。
4. 每次跑前自动清理输出与临时文件。

这是后续所有排查的基础，否则每次对比条件都会漂移。

### 方案 B：把 count 的 I/O 瓶颈与计算瓶颈彻底分离

需要用最小实验明确 count 到底慢在哪：

1. 主盘输出 `.kite`。
2. `/dev/shm` 输出 `.kite`。
3. 同时比较 `writer_io_ms / finalize_ms / cpu_util_pct`。

如果 `/dev/shm` 仍然快很多，就说明：

1. 先不要再碰 k-mer 算法本身。
2. 优先处理 `.kite` 写出路径，包括随机写、flush、sync、缓冲策略。

### 方案 C：把 dump 低 CPU 问题拆成三段测

dump 的总时间需要拆开看：

1. merge
2. format
3. shard write + concat

需要重点盯：

1. `writer_io_ms`
2. `concat_ms`
3. `longest_worker_ms / shortest_worker_ms`
4. `hottest_shard_bytes / coldest_shard_bytes`

如果最长 worker 明显远大于最短 worker，就说明当前 shard 切分仍有长尾。

### 方案 D：增加更细的 dump test-mode 指标

如果现有指标还不足以定位，就补这些指标：

1. 每个 shard 的实际 wall time。
2. 每个 shard 的输出 TSV bytes。
3. 每个 shard 的 partitions 数和 chunks 数。
4. concat 前等待最慢 shard 的尾巴时间。

这样才能知道问题到底在：

1. merge 偏斜。
2. format 偏斜。
3. 写盘偏斜。
4. 最终串行 concat。

## 5. 一步一步的执行计划

### 第 1 步：冻结当前可重复基准流程

1. 使用 `benchmark/serial_compare.sh` 串行跑 `k21`。
2. 只保留一份总日志，避免多终端、多命令混在一起。
3. 每次实验前都先清理 `temp` 的目标输出和 shard 残留。

### 第 2 步：先把 `k21 count` 的回退定性

1. 跑主盘输出版。
2. 跑 `/dev/shm` 输出版。
3. 比较 `writer_io_ms` 与 `finalize_ms`。
4. 如果差距仍然巨大，先判定 count 的主瓶颈是写盘。

### 第 3 步：再定位 `k21 dump` 的限速点

1. 统一使用同一个 `.kite` 输入。
2. 跑一次完整 dump。
3. 记录 `writer_io_ms`、`concat_ms`、`longest_worker_ms`、`cpu_util_pct`。
4. 如果 CPU 中后期持续掉下去，补 per-shard timing 指标后再跑一次。

### 第 4 步：根据结果决定优化方向

1. 如果主要慢在 count 写盘：优先优化 `.kite` writer 路径。
2. 如果主要慢在 dump shard 长尾：优先优化 shard 规划与调度。
3. 如果主要慢在 concat：优先评估减少 shard 数、预分配、或更低开销拼接方式。

### 第 5 步：恢复到真正的 stage01 速度线

目标不是只让代码“结构上像 stage01”，而是要让数字重新靠近对话里那条已知快版本：

1. `k21 count` 回到 70s 量级或更快。
2. `k21 dump` 回到 100s 左右量级。
3. `count + dump` 回到约 172s 附近或更低。

## 6. 当前结论

当前最值得先做的不是继续盲改算法，而是：

1. 先用固定脚本拿到干净、单日志的 `k21` 基准。
2. 先把 count 的磁盘 I/O 回退定死。
3. 再用 test-mode 把 dump 的低 CPU 症状拆开。

只有这样，后面的优化才不会继续偏离 stage01 的正确方向。