# Plan: kite — 高性能 k-mer Counter (Rust 从零构建)

## TL;DR
从零构建 kite，采用两阶段外排架构：`count` 阶段将输入流式解压 → rolling k-mer → 按 bucket 排序 + RLE → 写入 `.kite` 二进制；`dump` 阶段用 loser tree 对每个 bucket 的多路 run 做归并 → TSV 输出。全程以最大化吞吐为目标，参数真正生效，test-mode 输出详细诊断信息。

---

## 文件结构

```
kite/
  Cargo.toml
  src/
    main.rs
    cli.rs
    format/          ← 共享 .kite 格式定义
      mod.rs
      header.rs      KiteHeader struct + serde
      bucket_dir.rs  BucketDirEntry struct + serde
      run_block.rs   RunHeader struct + serde
    count/
      mod.rs         run_count() 入口
      reader.rs      文件读取 + 压缩检测
      decompress.rs  flate2/bgzf 流式解压 + buffer pool
      parser.rs      FASTQ 字节状态机，只产出 seq 切片
      kmer.rs        2-bit rolling kmer, canonical, u64/u128 路径
      router.rs      Partitioner trait + hash-based impl
      sorter.rs      radix sort (u64) + RLE 压缩
      worker.rs      count worker 线程逻辑
      memory.rs      MemoryBudget 全局原子计数
      writer.rs      KiteWriter — 追加 run，最后写 footer index
      stats.rs       CountStats 收集 test-mode 数据
    dump/
      mod.rs         run_dump() 入口
      reader.rs      KiteReader — mmap + bucket 定位
      run_cursor.rs  RunCursor — 逐条读取一个 run
      loser_tree.rs  LoserTree<K,V> 泛型 k-way merge
      decoder.rs     u64 → DNA 字符串，批量格式化
      writer.rs      排序输出 BufWriter<File>
      worker.rs      dump bucket worker 线程逻辑
      stats.rs       DumpStats 收集 test-mode 数据
  test/              (用户已放好数据集)
  temp/              (运行时临时文件，count 写 .kite 到这里)
```

---

## Phase 1: 骨架 + 最小正确版 (Steps 1-2)

### Step 1 — Cargo.toml + CLI 骨架

**Cargo.toml 关键依赖：**
- `clap = { version = "4", features = ["derive"] }`
- `crossbeam-channel = "0.5"`
- `flate2 = { version = "1", features = ["zlib-ng"] }` (更快的 zlib)
- `byteorder = "1"` (大/小端二进制 I/O)
- `memmap2 = "0.9"` (dump 侧 mmap 读 .kite)
- `rayon = "1"` (dump 侧 bucket 并行)
- `libc = "0.2"` (getrusage 用于 peak RSS)
- `mimalloc = { version = "0.1", default-features = false, optional = true }` (可选快速分配器)

**profile.release：** `opt-level=3`, `lto="thin"`, `codegen-units=1`

**src/cli.rs：** 用 clap derive 定义 `Cli`, `Commands::Count(CountArgs)`, `Commands::Dump(DumpArgs)`，所有参数按规格定义好，含默认值（k=21, threads=num_cpus, partitions=256, chunk-kmers=4_000_000, max-memory-gb=4.0, canonical=true）

### Step 2 — 最小正确版（单线程）

**format/header.rs：** KiteHeader，固定大小 64 字节
```
magic: [u8;4] = b"KITE"
version: u16 = 1
k: u8
canonical: u8
partitions: u32
flags: u32
footer_offset: u64    ← 写完后 seekback 填入
reserved: [u8;44]
```

**format/bucket_dir.rs：** BucketDirEntry，16 字节
```
run_count: u32
_pad: u32
data_offset: u64      ← 该 bucket 第一个 run 的文件偏移
```
bucket directory 是固定大小数组（partitions 个 entry）写在 footer 前。

**format/run_block.rs：** RunHeader，32 字节
```
bucket_id: u32
entry_count: u32
flags: u32            bit0=sorted, bit1=rle
_reserved: u32
byte_size: u64        整个 run (header+data) 的字节数
checksum: u64         可选 xxhash，flags=0 时跳过
```
后跟 entry_count 个 `(key: u64, count: u32)` = 12 bytes/entry（或合并为 u64 count 视溢出风险）

**count/reader.rs：** 检测扩展名/.gz/BGZF magic，返回 CompressionType 枚举

**count/decompress.rs：** 用 flate2::read::GzDecoder 包装 BufReader，Read::read 读取大块(4MB)，写入可复用 Vec<u8>

**count/parser.rs：** 字节切片状态机，4 状态（Header/Seq/Plus/Qual），只在 Seq 状态 emit 序列字节范围

**count/kmer.rs：** KmerIter 结构体，持有 fwd/rev: u64, valid: u32, k: u8, mask: u64
- `push(base: u8) -> Option<u64>` 实现 rolling + canonical
- base 编码: A=0, C=1, G=2, T=3; N → reset valid=0
- rev complement 位运算: complement = base ^ 3; rev = (rev >> 2) | (complement << (2*(k-1)))

**count/sorter.rs：** 
- `radix_sort_u64(buf: &mut Vec<u64>)`: 8-pass 256桶 counting sort，小于 2048 elements 时用 std::sort_unstable
- `rle_compress(sorted: &[u64], out: &mut Vec<(u64,u32)>)`: 线性扫描，相邻相同 key 累加

**count/writer.rs：** KiteWriter
- 持有 File + BufWriter（8MB write buffer）
- 写 header（先占位 footer_offset=0）
- `append_run(bucket_id, entries: &[(u64,u32)])` — 写 RunHeader + data，记录 (bucket_id, offset, entry_count)
- `finalize()` — 写 BucketDirectory + footer，seekback 填写 header.footer_offset

**count/mod.rs：** run_count() — 单线程版本：读 → 解压 → 解析 → kmer → 每个 worker-local Vec<u64> bucket buffer（初始实现用 Vec<Vec<u64>>，一个 partition 一个）→ 当 total 内存接近 max-memory-gb 时按内存最大的 bucket flush

**dump/reader.rs：** KiteReader — open + mmap，解析 header，从 footer 读取 BucketDirectory

**dump/run_cursor.rs：** RunCursor — 持有 (start_ptr, len, pos)，实现 next() -> Option<(u64,u32)>

**dump/loser_tree.rs：** LoserTree<K:Ord,V>
- `new(streams: Vec<impl Iterator<Item=(K,V)>>) -> Self`
- `next() -> Option<(K,V)>` — 返回全局最小 key 的元素，累加相同 key 的 count
- 内部: losers: Vec<usize>, values: Vec<Option<(K,V)>>
- 初始化: 每个叶子打 PK 往上传递，loser 留在 internal node，winner 跑到 root

**dump/decoder.rs：** `decode_kmer(key: u64, k: u8, buf: &mut [u8])` — 位运算提取每 2 bits → ACGT

**dump/writer.rs：** 持有 BufWriter，`write_entry(kmer: &[u8], count: u32)`，批量格式化

**dump/mod.rs：** run_dump() — 打开 .kite，按 bucket 串行: load all runs → build LoserTree → iterate + accumulate → write TSV

---

## Phase 2: Bucketed Runs 架构 (Step 3)

**count/worker.rs 改造：**
- 每个 worker 持有 `local_buckets: Vec<Vec<u64>>` (partitions 个，每个是 raw kmer vec)
- 当 local total bytes > per_worker_budget 时：找最大 bucket → sort → RLE → send (bucket_id, entries) to writer channel
- 全局 `MemoryBudget` 原子计数，超过阈值时 broadcast flush 信号

**count/memory.rs：**
```rust
struct MemoryBudget {
    current: AtomicUsize,
    limit: usize,  // from --max-memory-gb
}
```
- `alloc(n)` / `free(n)` — AtomicUsize fetch_add/fetch_sub
- `should_flush() -> bool` — current > limit * 0.9

---

## Phase 3: 多线程 count (Step 4)

**线程拓扑：**
```
[Reader Thread] --raw_chunks--> [Decompress Thread(s)] --decompressed_chunks--> [Count Workers x N] --run_sends--> [Writer Thread]
```

- raw_chunks channel: bounded(16) × 4MB chunks
- decompressed_chunks channel: bounded(32) × 8MB decoded bufs
- run_sends channel: bounded(256) × (bucket_id, Vec<(u64,u32)>)

**对齐逻辑 (decompress.rs)：** 解压出来的 buffer 需要找到最后一个完整 FASTQ record（找最后一个 `\n@` 或类似边界），将尾部未完成部分 carry-over 到下一块

**count/worker.rs：** 接收 decompressed chunk → 解析 → kmer → 路由到 local_buckets → 监控内存 → flush

**Writer Thread：** 串行接收 (bucket_id, entries) runs，追加到 .kite 文件，用大 BufWriter

---

## Phase 4: 多线程 dump (Step 5)

**dump/mod.rs 改造：**
- 用 crossbeam channel 建 bucket 任务队列
- 启动 N dump worker 线程
- 每个 worker: pop bucket_id → load runs from mmap → LoserTree merge → 写到临时 per-bucket buffer → 写到输出

**输出有序性问题：**
- 如果输出必须全局有序，需要 bucket 间排序（bucket 用 top bits 路由，因此 bucket 内有序后，按 bucket 顺序写出即可有序）
- 否则允许 per-bucket 并行无序输出（TSV 行乱序但每个 kmer 正确）

**per-bucket writer：** 每个 worker 拥有独立 output buf，最后顺序 flush 到输出文件（或直接 stdout 允许乱序）

---

## Phase 5: test-mode (Step 6)

**count/stats.rs CountStats：**
所有统计量：AtomicU64 fields (reads, bases, kmers, valid_kmers, flush_count, total_runs, compressed_bytes, decomp_bytes) + per-bucket Vec<AtomicU64> + timing fields (Instant-based)

**dump/stats.rs DumpStats：**
类似，收集 merge 统计、loser tree comparisons、timing

test-mode 在命令结束时打印格式化报告到 stderr (不污染 stdout 的 TSV)

---

## .kite 文件布局（最终确定）

```
[  0.. 64]  KiteHeader (fixed 64 bytes)
[ 64..64+P*16]  BucketDirectory placeholder (P=partitions, 16B/entry)
  -- 注意: 在 count 结束前这里是 0，count 结束时 seekback 填写 --
  -- 或者：runs 写完后，BucketDir 追加到末尾，header 指向 footer_offset --
[...]  Run blocks (顺序追加)
[...]  BucketDirectory (实际写在末尾)
[last 16B]  KiteFooter: {total_runs: u32, bucket_dir_offset: u64, magic_tail: [u8;4]}
```

优先选用**末尾追加 + 尾部 index** 方案（无需 seekback，更适合流式写）:
- Header 中 `footer_offset` 字段初始为 0xFFFFFFFFFFFFFFFF
- 写完所有 runs 后，写 BucketDirectory，写 Footer，最后 seekback 到 header.footer_offset 位置写入实际偏移

---

## 关键算法细节

### Radix Sort (8-pass, 256 buckets)
```
for pass in 0..8:
    shift = pass * 8
    count buckets: hist[256]
    prefix sum
    scatter to temp buf
    swap src/dst
最后确保结果在 src
```
小于 1024 元素时退化为 std::sort_unstable (避免 radix 开销)

### Loser Tree 实现
n 条输入流 → n-1 个 internal nodes + n 个 leaf slots
- internal[i] 存该节点的 "loser" 流 index（较大者留下）
- winner 是最小 key 的流
- 初始化：每个叶子打 PK 向上传播
- 调整：当 winner 流推进后，从该叶子位置向上重新打 PK

### K-mer canonical 位运算
```rust
let fwd = ((fwd << 2) | enc) & mask;    // mask = (1<<2k)-1
let rc_enc = enc ^ 3;
let rev = (rev >> 2) | (rc_enc << (2*(k-1)));
let canonical = fwd.min(rev);
let bucket = (canonical >> (64 - log2_partitions)) as usize;
```

### FASTQ 解析状态机
```
State: {Header, Seq, Plus, Qual}
遇到 '\n':
  Header → Seq (记录 seq_start = pos+1)
  Seq → Plus (emit seq[seq_start..pos])
  Plus → Qual
  Qual → Header
遇到 EOF: done
```

### 内存预算分配（max-memory-gb = M GB）
```
input_pool:        M * 0.10  (raw compressed chunks)
decomp_pool:       M * 0.20  (decompressed FASTQ chunks)
worker_local:      M * 0.60  (所有 worker 的 bucket buffers 合计)
writer_buffer:     M * 0.05  (BufWriter + run encode buffer)
dump_merge_buffer: M * 0.05  (dump 侧每 bucket 的 merge workspace)
```

---

## Cargo.toml 完整依赖

```toml
[package]
name = "kite"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "kite"
path = "src/main.rs"

[dependencies]
clap = { version = "4", features = ["derive"] }
crossbeam-channel = "0.5"
flate2 = { version = "1", features = ["zlib-ng"] }
byteorder = "1"
memmap2 = "0.9"
rayon = "1"
libc = "0.2"
num_cpus = "1"

[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
panic = "abort"
```

---

## 验证步骤

1. `cargo build --release` — 编译通过，无 warnings
2. `./target/release/kite count -k 21 -i test/test.fastq.gz -o temp/test.kite --threads 4 --partitions 256 --test-mode`
3. `./target/release/kite dump -i temp/test.kite -o temp/test.tsv --test-mode`
4. 对比小样本结果与 KMC: `kmc -k21 -fm -ci1 test/test.fastq.gz kmc_out temp/kmc_tmp && kmc_dump kmc_out kmc_out.txt && sort kmc_out.txt > kmc_sorted.txt && sort temp/test.tsv > kite_sorted.txt && diff kmc_sorted.txt kite_sorted.txt`
5. 大样本: `./target/release/kite count -k 21 -i test/HG002_sample.fastq.gz -o temp/hg002.kite --threads 32 --max-memory-gb 16 --test-mode`
6. 对比 KMC 速度，目标 count+dump < KMC 总时间

---

## 实施顺序（按依赖关系）

**Phase 1 (可独立并行的文件):**
- Cargo.toml 和目录结构
- src/cli.rs (无依赖)
- src/format/header.rs, bucket_dir.rs, run_block.rs (互相独立)
- src/count/kmer.rs (无依赖)
- src/count/sorter.rs (无依赖)
- src/dump/loser_tree.rs (无依赖)
- src/dump/decoder.rs (无依赖)

**Phase 2 (依赖 Phase 1):**
- src/count/parser.rs
- src/count/reader.rs + decompress.rs
- src/count/router.rs (依赖 kmer.rs 的 hash 结果)
- src/dump/run_cursor.rs (依赖 format)
- src/dump/reader.rs (依赖 format)

**Phase 3 (依赖 Phase 2):**
- src/count/writer.rs (依赖 format + sorter)
- src/count/worker.rs (depends on parser, kmer, router, sorter, writer)
- src/count/memory.rs
- src/dump/writer.rs
- src/dump/worker.rs (依赖 run_cursor, loser_tree, decoder)

**Phase 4 (最后集成):**
- src/count/mod.rs (穿起所有 count 部分)
- src/dump/mod.rs (穿起所有 dump 部分)
- src/count/stats.rs, src/dump/stats.rs
- src/main.rs

---

## 决策记录

- **BucketDir 写在尾部**：避免 seekback，便于流式写入，dump 读时先 mmap 尾部找 footer，再定位 BucketDir
- **u64 key only (k≤32)**：第一版专注 u64，u128 路径预留接口但不实现
- **hash routing（非 minimizer）**：第一版用 top bits routing，Partitioner trait 预留替换接口
- **radix sort 主线**：8-pass 256桶，< 1024 entries 退化 std::sort_unstable
- **loser tree 必须实现（非 BinaryHeap）**：按用户要求
- **TSV 输出允许 per-bucket 无序**（行间无序但 kmer 正确），除非用户要全局排序
- **临时文件**：全部写在 `temp/` 下，正式输出写 `-o` 指定路径
- **count 写文件策略**：Writer Thread 持有唯一 File handle，所有 run 追加到一个 .kite 文件，使用大 BufWriter（8MB）减少 syscall
