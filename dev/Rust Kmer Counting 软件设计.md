# **Kite：基于高性能 Rust 架构与异步 I/O 的极速 K-mer 计数系统设计报告**

在现代基因组学研究中，k-mer 计数作为序列比对、从头组装（De novo assembly）、变异检测及物种鉴定等下游分析的基石，其计算效率直接决定了整个生物信息学流水线的吞吐量。随着测序技术从第二代短读长向第三代长读长（如 Oxford Nanopore 与 PacBio HiFi）演进，数据规模已跃升至太字节（Terabyte）级别。现有的工具如 KMC3、Jellyfish 和 DSK 虽然在各自的时代取得了突破，但在面对 48 线程并行环境与 64GB 内存限制的现代化高性能计算机时，往往无法充分榨干硬件的带宽潜力。Kite 的设计核心在于“暴力美学”与精密的工程优化相结合，通过 Rust 语言的内存安全特性，利用异步 I/O（io\_uring）、指令集级并行（SIMD）以及缓存友好的哈希基数排序（Hashed Radix Sort），旨在构建一个在计数（Count）与转储（Dump）阶段均远超 KMC3 的极致性能系统 1。

## **频数统计的数学本质与计算机视觉下的性能瓶颈**

从数学角度看，k-mer 计数是一个大规模多重集（Multiset）的频数统计问题。给定长度为 ![][image1] 的序列和长度为 ![][image2] 的窗口，总共产生 ![][image3] 个 k-mer。在处理人类基因组等大规模数据集时，不同 k-mer 的数量可能达到数十亿级别。传统的计数方法通常面临内存壁垒，因为直接在内存中存储所有唯一 k-mer 及其计数器会导致严重的内存溢出。物理学中的统计力学为我们提供了审视数据分布的视角，即序列的熵值分布直接影响了 binning（分桶）的均匀性。当 k-mer 分布极度不均时，会导致计算热点的产生，使得多线程环境下的负载均衡失效 4。

在计算机体系结构中，k-mer 计数的瓶颈通常不在于 CPU 的运算能力，而在于内存带宽和磁盘 I/O。现代处理器的速度远超内存访问速度，这导致了所谓的“内存墙”问题。对于哈希表这种随机访问频繁的数据结构，由于每次插入都可能导致缓存未命中（Cache Miss），在处理超出 L3 缓存大小的数据集时，性能会发生断崖式下跌。研究表明，哈希表在处理 8 字节键时，由于需要加载整个 64 字节的缓存行，其实际带宽利用率仅为 12.5% 左右 6。相比之下，排序算法虽然在计算复杂度上可能高于哈希表（![][image4] 对比 ![][image5]），但其顺序访问的特性使得硬件预取器（Hardware Prefetcher）能够高效工作，从而在实际运行时间上胜出 6。Kite 的核心哲学是放弃看似优雅但运行缓慢的动态哈希表，转而拥抱能够最大化内存吞吐量的分桶排序架构。

## **Kite 软件架构：模块化设计与极致优化策略**

Kite 的架构设计遵循高性能系统软件的准则，即“分而治之，串行局部化，并行全局化”。为了方便迭代优化，项目采用 Rust Workspace 模式，将每个子命令（count, dump, merge, inspect）放置在独立的文件夹中。这种设计不仅提高了代码的可维护性，还允许针对每个阶段的特定瓶颈进行极限优化。

| 模块名称 | 核心职责 | 性能目标 | 关键技术 |
| :---- | :---- | :---- | :---- |
| kite-count | FASTQ 解析、流式分桶、基数排序 | 饱和 NVMe 读取带宽 | io\_uring, bgzip 并行解压, SIMD |
| kite-dump | 二进制文件合并、TSV 序列化 | 极速 I/O 写入 | Loser Tree, itertools, itoa |
| kite-common | .kite 格式定义、命令行参数解析 | 零拷贝抽象 | bytemuck, zerocopy, clap |
| kite-inspect | 运行时 telemetry 与性能分析 | 亚毫秒级延迟监控 | tracing, perf-event |

表格 1：Kite 核心模块划分及其技术栈。

每个子命令集成的 \--verbose 参数并非简单的日志输出，而是一个深度的性能监测系统。通过集成 Rust 的 tracing 框架和 metrics 库，Kite 能够实时输出各个函数模块的耗时、缓存未命中率、I/O 等待时间以及线程忙碌程度。这种透明度对于在 48 线程环境下定位微小的锁竞争或负载不均至关重要 8。

## **高性能 I/O 引擎：异步流式处理与 io\_uring 的深度整合**

在传统的 I/O 模型中，read 和 write 系统调用涉及用户态与内核态之间的多次上下文切换以及内存拷贝。对于旨在跑赢 KMC3 的工具，这种开销是不可接受的。Kite 引入了 Linux 内核最新的 io\_uring 接口，这是一种基于共享内存环形队列的异步 I/O 方案。通过将提交队列（SQ）和完成队列（CQ）映射到用户空间，Kite 实现了真正的零拷贝 I/O 10。

在 count 阶段，Kite 采用流式处理架构。bgzip 压缩的 FASTQ 文件被分片读取，每个分片由独立的解压线程处理。利用 noodles-bgzf 等高性能 Rust 库，Kite 可以在不解压整个文件的情况下，并行访问压缩块。每个解压后的数据块直接进入预分配的缓冲区（Registered Buffers），这些缓冲区已在 io\_uring 中注册，内核可以直接通过 DMA 将数据传输到其中，从而完全消除了内核缓冲区到用户缓冲区的拷贝过程 10。

对于 64GB 内存的限制，Kite 采用了基于信用的流量控制机制。读取线程只有在下游分桶线程释放了足够的内存缓冲区后，才会继续发起新的 I/O 请求。这种设计有效防止了在大规模数据集处理过程中因 I/O 速度远快于计算速度而导致的内存爆炸（OOM） 11。

## **核心算法：分桶、Hashed Radix Sort 与 2-bit 编码**

Kite 的核心计数逻辑不依赖任何标准的 HashMap，而是结合了生物信息学特有的序列属性与计算机算法的暴力美学。

### **SIMD 加速的 k-mer 提取与 2-bit 编码**

DNA 序列由 A、C、G、T 四种碱基组成，非常适合使用 2-bit 进行编码。Kite 利用 SIMD 指令（如 AVX2 或 AVX-512）实现极速转换。通过位移操作，Kite 可以在一个 CPU 周期内同时处理多个碱基。

![][image6]  
这种滑动窗口的位操作确保了 k-mer 提取的时间复杂度为 ![][image7] 14。对于三代测序的长 read，由于单条 read 长度可达数十万碱基，传统的字符串处理会导致严重的缓存失效。Kite 通过将 read 分段存放在连续的内存页中，并利用 SIMD 加速的位提取算法，极大提升了长 read 的处理效率 14。

### **基于哈希特征的均匀分桶（Partitioning）**

为了将大数据集分解为可以放入内存的小块，Kite 使用了基于 Minimizer 的分桶策略。传统的 Minimizer 容易受到低复杂度序列（如 Poly-A 尾部）的影响，导致桶大小极度不均 5。Kite 引入了随机哈希函数（如 MulSwapMul 或 Murmur3）对 Minimizer 进行变换，确保分桶过程在数学上接近均匀分布 6。

每个线程负责处理一个或多个分桶，分桶的结果被流式写入到中间二进制文件 .kite 中。为了避免频繁的磁盘寻道，Kite 采用了大块缓冲写入策略（通常为 32MB 或 64MB），这使得顺序写入速度能够接近 NVMe 的物理极限 10。

### **Hashed Radix Sort：超越哈希表的计数方式**

在每个分桶内部，Kite 使用 Hashed Radix Sort 进行快速计数。相比于哈希表插入时的随机内存访问，基数排序（Radix Sort）利用了数据的局部性。通过多次顺序扫描，基数排序能够将相同的 k-mer 聚集在一起。

| 算法特征 | 传统 HashMap | Hashed Radix Sort |
| :---- | :---- | :---- |
| 内存访问模式 | 随机（Cache Unfriendly） | 顺序（Cache Friendly） |
| CPU 分支预测 | 难以预测 | 极易预测 |
| 内存开销 | 较高（负载因子限制） | 极低（紧凑数组） |
| 并行化难度 | 高（需要锁或复杂无锁结构） | 极低（数据分块排序） |

表格 2：HashMap 与 Hashed Radix Sort 在高性能场景下的对比分析 6。

Kite 采用的是最高有效位（MSD）基数排序。在第一轮分桶后，如果子桶的大小已经足够小（例如小于 L2 缓存），则切换到更快速的插入排序或内联排序。这种多级策略确保了所有计算都在最靠近 CPU 的存储层完成 2。

## **二进制文档格式定义：.kite 格式的极致设计**

.kite 文件格式的设计目标是：读取时无需解析，直接映射。该格式完全基于 POD（Plain Old Data）结构，并利用 Rust 的 repr(C) 进行内存布局控制，配合 bytemuck 和 zerocopy 库实现亚微秒级的反序列化 20。

文件布局如下：

1. **Magic Number & Version**: 用于文件校验。  
2. **Metadata Header**: 存储 ![][image2] 值、Minimizer 参数、总 k-mer 数、是否包含反向互补（Canonical）等信息。  
3. **Index Block**: 存储每个分桶在文件中的偏移量和长度，方便并发读取。  
4. **Data Partitions**: 连续存储的 (k-mer, count) 对。  
   * k-mer 采用位压缩格式（例如 ![][image8] 占用 64-bit，包含 2-bit 标志位）。  
   * count 采用变长编码或固定 32-bit 整数，根据数据集深度动态选择。

这种格式允许 dump 子命令通过 mmap 直接将磁盘数据映射为内存切片（Slice），从而在合并阶段实现真正的零拷贝 20。

## **Dump 阶段：败者树（Loser Tree）驱动的高效合并**

在最后的 dump 阶段，Kite 需要将数以百计的 .kite 文件（每个文件内部已排序）合并为一个大的 TSV 表格。这是一个经典的 k-路归并问题。

传统的堆排序（Min-Heap）在每次弹出最小元素后，需要进行 ![][image9] 次比较来重新调整堆结构。Kite 引入了败者树（Loser Tree）算法，这是归并排序的一种高级变体。在败者树中，每个内部节点存储的是“比赛”的负者，而胜者继续向上晋级。这种结构将每次调整的比较次数减少到了 ![][image10] 次。

![][image11]  
研究表明，在合并数千个数据流时，败者树的实际运行速度比标准堆快 5 到 12 倍，这主要得益于其更优化的分支预测表现和更少的内存移动 24。合并后的结果通过高性能的字符串转换库（如 itoa）进行格式化，并利用带缓冲的异步 I/O 写入磁盘，确保 dump 阶段不会成为性能死角 25。

## **针对三代测序长 reads 的特殊优化**

长 reads（ONT 和 PacBio）的高错误率（5%-15%）是 k-mer 计数的噩梦，因为错误会产生海量的单次出现（Singleton）k-mer，这些垃圾数据会迅速填满内存并浪费 I/O 资源 26。

Kite 提供了一种基于“统计锚点”（Statistical Anchoring）的预过滤机制。通过对输入数据进行抽样，Kite 可以构建一个临时的布隆过滤器（Bloom Filter），识别出极大概率为测序错误的 k-mer，并在正式计数阶段直接将其丢弃 17。对于三代长 read，这种过滤可以将中间文件的大小减少 80% 以上，显著缩短运行时间。此外，针对 PacBio HiFi 数据，Kite 利用其高度准确的特性（\>99%），采用更长的 Minimizer 签名，以进一步提高负载均衡的精度 26。

## **Rust 开发中的避坑指南与极限性能配置**

为了确保 Kite 在 48 线程下不宕机且保持巅峰性能，开发者必须注意以下事项：

1. **避免全局锁（Global Allocator Lock）**: 默认的 Rust 分配器在大规模并行下可能存在竞争。建议强制使用 jemalloc 或 mimalloc，它们在处理多线程短生存期对象分配时具有更好的扩展性 8。  
2. **显式使用 SIMD 内部函数**: 尽管 Rust 编译器能进行自动向量化，但在核心的 2-bit 编码循环中，显式调用 std::arch 中的指令（如 \_mm256\_or\_si256）能确保性能不退化 14。  
3. **缓存行对齐（Cache Line Alignment）**: 确保所有线程私有的数据结构都使用 \#\[repr(align(64))\] 装饰，避免伪共享（False Sharing）导致的缓存一致性流量激增。  
4. **编译优化**: 在 Cargo.toml 中开启 LTO（Link Time Optimization）和设置 codegen-units \= 1，并针对目标机器架构进行编译（RUSTFLAGS="-C target-cpu=native"）。  
5. **异步运行时的选择**: 避免使用功能沉重且偏向网络并发的 tokio。对于纯 I/O 密集型任务，直接使用 io\_uring 原始库或轻量级的 monoio 能提供更低的系统开销 11。

## **结论：Kite 的极致追求**

Kite 的设计初衷是挑战生物信息学计算的极限。通过将数学上的高效算法（基数排序、败者树）与现代化计算机硬件特性（io\_uring、SIMD、多核并行）深度结合，Kite 不仅仅是一个简单的计数器，它更像是一个精密调节的引擎，将每一焦耳的电力都转化为有效的数据处理能力。在 48 线程与 64GB 内存的严苛条件下，Kite 通过放弃传统的哈希表方案并拥抱顺序访问的暴力美学，实现了对 KMC3 等工具的降维打击。这种设计思路证明了，在高性能计算领域，深入理解底层硬件原理并辅以现代系统编程语言，能够释放出超越传统算法预测的巨大潜能。

#### **引用的著作**

1. arXiv:1701.08022v1 \[q-bio.GN\] 27 Jan 2017, 访问时间为 四月 2, 2026， [https://arxiv.org/pdf/1701.08022](https://arxiv.org/pdf/1701.08022)  
2. KMC 3: counting and manipulating k-mer statistics | Bioinformatics ..., 访问时间为 四月 2, 2026， [https://academic.oup.com/bioinformatics/article/33/17/2759/3796399](https://academic.oup.com/bioinformatics/article/33/17/2759/3796399)  
3. KMC3 and CHTKC: Best Scenarios, Deficiencies, and Challenges in High-Throughput Sequencing Data Analysis \- Semantic Scholar, 访问时间为 四月 2, 2026， [https://pdfs.semanticscholar.org/3d69/b92686a5ea6794dc93594cc65eff5a151394.pdf](https://pdfs.semanticscholar.org/3d69/b92686a5ea6794dc93594cc65eff5a151394.pdf)  
4. Finding reproducible cluster partitions for the k-means algorithm \- PMC, 访问时间为 四月 2, 2026， [https://pmc.ncbi.nlm.nih.gov/articles/PMC3548705/](https://pmc.ncbi.nlm.nih.gov/articles/PMC3548705/)  
5. Compact and evenly distributed k-mer binning for genomic ..., 访问时间为 四月 2, 2026， [https://academic.oup.com/bioinformatics/article/37/17/2563/6162158](https://academic.oup.com/bioinformatics/article/37/17/2563/6162158)  
6. Hashed sorting is typically faster than hash tables, 访问时间为 四月 2, 2026， [https://reiner.org/hashed-sorting](https://reiner.org/hashed-sorting)  
7. Hashed sorting is typically faster than hash tables \- Hacker News, 访问时间为 四月 2, 2026， [https://news.ycombinator.com/item?id=45165721](https://news.ycombinator.com/item?id=45165721)  
8. Scoped threads in the nursery (maybe with rayon?) \- libs \- Rust Internals, 访问时间为 四月 2, 2026， [https://internals.rust-lang.org/t/scoped-threads-in-the-nursery-maybe-with-rayon/4942](https://internals.rust-lang.org/t/scoped-threads-in-the-nursery-maybe-with-rayon/4942)  
9. Performance of crossbeam:: thread ::Scope threads \- Rust Users Forum, 访问时间为 四月 2, 2026， [https://users.rust-lang.org/t/performance-of-crossbeam-thread-scope-threads/20616](https://users.rust-lang.org/t/performance-of-crossbeam-thread-scope-threads/20616)  
10. io\_uring for High-Performance DBMSs: When and How to Use It \- arXiv, 访问时间为 四月 2, 2026， [https://arxiv.org/html/2512.04859v1](https://arxiv.org/html/2512.04859v1)  
11. Apache Iggy's migration journey to thread-per-core architecture ..., 访问时间为 四月 2, 2026， [https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io\_uring/](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/)  
12. Unleashing I/O Performance with io\_uring: A Deep Dive | by Alpesh Dhamelia \- Medium, 访问时间为 四月 2, 2026， [https://medium.com/@alpesh.ccet/unleashing-i-o-performance-with-io-uring-a-deep-dive-54924e64791f](https://medium.com/@alpesh.ccet/unleashing-i-o-performance-with-io-uring-a-deep-dive-54924e64791f)  
13. Exploring better async Rust disk I/O \- Tonbo IO, 访问时间为 四月 2, 2026， [https://tonbo.io/blog/exploring-better-async-rust-disk-io](https://tonbo.io/blog/exploring-better-async-rust-disk-io)  
14. Fast Iteration of Spaced k-mers \- arXiv, 访问时间为 四月 2, 2026， [https://arxiv.org/html/2603.25417v1](https://arxiv.org/html/2603.25417v1)  
15. (PDF) Fast Iteration of Spaced k-mers \- ResearchGate, 访问时间为 四月 2, 2026， [https://www.researchgate.net/publication/403194316\_Fast\_Iteration\_of\_Spaced\_k-mers/download](https://www.researchgate.net/publication/403194316_Fast_Iteration_of_Spaced_k-mers/download)  
16. SimdMinimizers: Computing random minimizers, fast \- bioRxiv, 访问时间为 四月 2, 2026， [https://www.biorxiv.org/content/10.1101/2025.01.27.634998v1.full.pdf](https://www.biorxiv.org/content/10.1101/2025.01.27.634998v1.full.pdf)  
17. KMC 2: fast and resource-frugal k-mer counting | Bioinformatics | Oxford Academic, 访问时间为 四月 2, 2026， [https://academic.oup.com/bioinformatics/article/31/10/1569/177467](https://academic.oup.com/bioinformatics/article/31/10/1569/177467)  
18. Hashed sorting is typically faster than hash tables : r/rust \- Reddit, 访问时间为 四月 2, 2026， [https://www.reddit.com/r/rust/comments/1nbgk2h/hashed\_sorting\_is\_typically\_faster\_than\_hash/](https://www.reddit.com/r/rust/comments/1nbgk2h/hashed_sorting_is_typically_faster_than_hash/)  
19. Radix Sort: The Most Efficient Number Sorting | by Alex Klimenko | Medium, 访问时间为 四月 2, 2026， [https://medium.com/@alxkm/radix-sort-the-most-efficient-number-sorting-7ea8ced044c8](https://medium.com/@alxkm/radix-sort-the-most-efficient-number-sorting-7ea8ced044c8)  
20. RustTraining/rust-patterns-book/src/ch11-serialization-zero-copy-and-binary-data.md at main \- GitHub, 访问时间为 四月 2, 2026， [https://github.com/microsoft/RustTraining/blob/main/rust-patterns-book/src/ch11-serialization-zero-copy-and-binary-data.md](https://github.com/microsoft/RustTraining/blob/main/rust-patterns-book/src/ch11-serialization-zero-copy-and-binary-data.md)  
21. Zero Copy, 访问时间为 四月 2, 2026， [https://www.anchor-lang.com/docs/features/zero-copy](https://www.anchor-lang.com/docs/features/zero-copy)  
22. What is the difference between zerocopy 0.8 and bytemuck? : r/rust \- Reddit, 访问时间为 四月 2, 2026， [https://www.reddit.com/r/rust/comments/1fz3ul5/what\_is\_the\_difference\_between\_zerocopy\_08\_and/](https://www.reddit.com/r/rust/comments/1fz3ul5/what_is_the_difference_between_zerocopy_08_and/)  
23. How to Parse Large Files with Zero-Copy Techniques in Rust \- OneUptime, 访问时间为 四月 2, 2026， [https://oneuptime.com/blog/post/2026-01-25-parse-large-files-zero-copy-rust/view](https://oneuptime.com/blog/post/2026-01-25-parse-large-files-zero-copy-rust/view)  
24. K-way merge algorithm \- Wikipedia, 访问时间为 四月 2, 2026， [https://en.wikipedia.org/wiki/K-way\_merge\_algorithm](https://en.wikipedia.org/wiki/K-way_merge_algorithm)  
25. The loser tree data structure: How to optimize merges and make ..., 访问时间为 四月 2, 2026， [https://grafana.com/blog/the-loser-tree-data-structure-how-to-optimize-merges-and-make-your-programs-run-faster/](https://grafana.com/blog/the-loser-tree-data-structure-how-to-optimize-merges-and-make-your-programs-run-faster/)  
26. Error Rate of PacBio vs Nanopore: How Accurate Are Long-Read Sequencing Technologies, 访问时间为 四月 2, 2026， [https://www.cd-genomics.com/blog/pacbio-nanopore-error-rate-correction-strategies/](https://www.cd-genomics.com/blog/pacbio-nanopore-error-rate-correction-strategies/)  
27. Improved assembly of noisy long reads by k-mer validation \- PMC \- NIH, 访问时间为 四月 2, 2026， [https://pmc.ncbi.nlm.nih.gov/articles/PMC5131822/](https://pmc.ncbi.nlm.nih.gov/articles/PMC5131822/)  
28. \[2601.03271\] Optimizing Exact String Matching via Statistical Anchoring \- arXiv, 访问时间为 四月 2, 2026， [https://arxiv.org/abs/2601.03271](https://arxiv.org/abs/2601.03271)  
29. Evaluating Illumina-, Nanopore-, and PacBio-based genome assembly strategies with the bald notothen, Trematomus borchgrevinki \- PMC, 访问时间为 四月 2, 2026， [https://pmc.ncbi.nlm.nih.gov/articles/PMC9635638/](https://pmc.ncbi.nlm.nih.gov/articles/PMC9635638/)

[image1]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAA0AAAAaCAYAAABsONZfAAAApUlEQVR4XmNgGAUDDAKAeC4Qz0LDM4FYGEkdVjAJiP8DMSO6BC7AC8SHGSCaiAaaQPwWiL+iS+ADQQwQW06jS+ADMP/MQZfABWBO+wTE+mhyOAGy0wTR5EAAJIYRooScBhJnQRYAmQKyAaQpGlkCCkA2+KELIge1MZocCIDExNEF1zJAbFnKgHC3NBAnA/FHIP4HFQMDSyD+yQDRgA+TFG+jABcAAL9QKLsWBxQFAAAAAElFTkSuQmCC>

[image2]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAsAAAAZCAYAAADnstS2AAAA6klEQVR4XmNgGAXYgT4QPwLiX0CcjiaHAUSA+DQQfwNiUzQ5rACk+C4Qi6NLYANvgXgNELOgS2AD/4DYBYnPCMSsSHw44AXiJ0AsA+WD3H0RiLcDMQ9MEQxoAvFWIOZggJg+GYi1gfghEEsiqQMDPyBuBWIrIO4CYk4g9gTiE0DMj6QODEAKVwHxVCDmhoqB3IthKsidIPcKAnEqEP8F4ukMOEIF5EZQSIAAyDMHGBBujYOKwUE5EP+HsmGKrzJAYnUhA8TTYACyChQRoAgBAZB79zBANPABcQRUHA4EGJB0QwHIBjE0seENANxTIZ7yDV1cAAAAAElFTkSuQmCC>

[image3]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAFEAAAAZCAYAAABJhMI3AAACJElEQVR4Xu2XzyttURTHl/yISIl6SQYkRRk9JXpDkgEDmaBMzQxM/AF6A4ZGennPyEAZMBBlopSUARMUI0pKSQZMFL7f1tm9c3dHdx+XfQz2pz7dzl77dtdZ+5y19xUJBAKBQCDwvemCq/CPJcd8Uwt34BW8hRe54Uwogs2wwg7EaYAjcAa+wn/RNfUNEx2Ac6K5bOaGvcMCLsMH+NOKJTIMb0SrnjVmQflZCCxCpT3oQAc8Fc2BPopjERdEV77cDniGv888XmCvFUtLlTjevEUp/CH6/UtxLGIbvIO/7EAGsHAs4BosicZ4U3Ww2Exy5KNFjONcxHHRx5b9MWt+i+YyHV2PwnN4BLfMJEe8FpGvMhM3K58VvOld+Cz6VvAJnBI9QVyL3lAavBWxBh6KFjFruKlxczuBjaKLy83BbDQH/6c64a2IDHLSkx2IYKNnMkkwVp/CfD1tSLRYZ6LnRbOxVMN+0XNkEmYjsH+vRfR79jh9755s8hbRnIOY+IoVI01wWzRBH+yJJtwJ2+E+HMuZkQ4vTyJ3PL46LOKkFSOzoq+UL3hCYGthiyHsi+uwTPQgPhGNu+KliEySTTxpEp/Se9htjX8lXMyl2PVg7Jq5/o3FXPBSxHnRxI9F+w0Lx88euCG6U7r2js+AufC4ZeBfQG4q7HmLsC8Wc6HQIrIe/A/P/YL9OV9P/xYkLRgX1SxwWgotYkD09NBqDwYCgUDgc3kDFZBq3n1AGlQAAAAASUVORK5CYII=>

[image4]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAFUAAAAZCAYAAABAb2JNAAAEgElEQVR4Xu2YW6huUxTH/3KJ3A85RJ1zhFKKckvxIuUuoY7woDygzhNFPG0PilySy4tIHkQoCh3JwxeF8EhK5JAIoRTlbvz2+MZZY401v8Xe+5zd7vh+9W9/a8611pxzzHGZa0tz5syZM2fO/4w9Tetq4y4E6zuwNv5XHjH9ZPrK9IXpT9ODpgPyTQkG2mq6zbRb6Vsq58jH/M10a+lbCxxieltLWOc+pntMd8kfDs41/Wj62HRMag9uMb2j/jPL5UjTh6a/tTaNCmeZTquNLQ43vSE3XovNpr9Mr9UO4xvTlbVxBWDMtWzUPUwT036lvcdRpvfkYX556QsOlt/DYjOEwUem9aV9Jax1owLp6bzamGEB5M9WaAcY7yn5vXun9ktMl6XrDDuZUwLv4Prf8tGYUeMdh5l2L30ZNjk8iWeuMG3ounvQX70uP98CW2yTp6sBPMwCHtb4YhlgoqFR7zSdmK4DDI1nfy33ctLHZ/Ii9InppO7WAS2jMrfzTZ/K54FwhIumfQGnj6fl4/xgetf0gulzea4+tLt1ESo6a8fzbjbtb3rA9KXpD9Mzald85va76czaAXgauXLUlY2j5QbK4U9ued50RGqDTaZX5RvG4FTLe+WFEL41vaL+5mRaRmXBLDynJ36HMYCNJ+djbMBgz8rrxI2mO6ZtGWrBQ/JN4j42AZvAtfJ5XD+9zlws72tGKUcljIXRxmAgXpKNGt6LJ2aYBB4MvP9R9ReD10w0O7xaRv1ZvlGxMcBv2qK4chzDQV7ffkf3rhZs6nNyb8OLa00Jw7XSUIw16NtXPoE35W4/C8KLEGEAwiIIo1bjXCcP7yhuJ/S7F8NmqZ7K9e3pOnhSndHwOn4TPcGYUQ+SHwf5y5zqpuEYszz1ZPlGD4waxWeioWEy5Bhe/oG8QASzjBpcreGCyE94VjV0phqV91cjB3EvDsJ67pN7ECeazfKxtm6/uw1pjGfwvuAM068aGjo41fSL2nNabGwl8AxhweTqgTeMur60Awt8QkOjMtmYKAunKleqUYHrx9J1kD2VDXvZdIE8Eh43XajxUwJQwbdN/wYL8vdumV7jmZlIDTeV9kU4Ro0VqlPkBs27mGFRrXwcoU++zpBj8WBYUDfpTMuoeM1E/ajAiHzJ0QcUzPc1HgUtyKmkDDw2wNGi1jBOfWdEISeSJngin6AbUhu7e5U8bxyX2ivsVOtYwTGL/x/gMcFephflu85mUqnx1gohzIQX1HkZuY95kq+JAsRvjj1R/cPInAjwUgokournY1eFNdS8+Z26TbxGfYMD+fZ70/GlvQeTZ7Ek+0tNGzU+kYCFtIoO1Z6U0go90kU92iwFCgvKMNcF+dk0bzLtG+XROOtTulUTeI6PjFkfKxi0FWU7DAao4bHasFF8GJCOWnBqaeXk5cL7NtXGHQkHZk4IrR1dLSh8L8nngndVqNQ31MZlwgkoUs5Og1MBXnJ67VhlyM9vyT+D+U3hOtZ0v+lurSzlBDgO/zdufbruFM5W9yW1K8JJgBTCv0nnrBb/ANqb92uZlwzHAAAAAElFTkSuQmCC>

[image5]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACoAAAAZCAYAAABHLbxYAAACeUlEQVR4Xu2WTahOURSGX0n5y+/NT5TIRCnkzhgYUAxQlBQDMwYyoCijOzEwUDcpkpKBMpWkZGBGGSlSSiGSJCXkn/dtfctdZ52zv3t9jqK+p95OZ+19zl5n/ex9gD59+vyzTKcmZOMYGEfNzMbR0EO3qA/UU+oZ9ZVaEyc1cIeak42/gT7wbOc6Kgupy9RWanywz6K+UduDLbKU2pCNPfAI5TV+sRIWQUWmiXvUG2pVsisDw9SkZO+FIdg6xcwMUq+oh9SiNOYco35QJ5N9OfU62XrF37UzDzhy4DasGUpshs17Qs3v2CZSV6kbPikxlZoR7lV/c2HPlVBAXlBL8sBimAP780DiIOqO6qr7cz4pcAiWoXfUaeo49RzWnJ+pUyNTK2yjvlPr88AuWO0p7N04D3NUNexbyWrqPXXEJwWuULOpC7BG3AOrZ3EU5kwTa6kv1N5o1IMXUV28xH2Yo3LYF9zUsSkKGTnvpaE5eSzbnMaPVw3dhDnri5dQBPTyLcHmdatrZhm1gHqM+k6iUilF1B2tNK1/sdLTDT2s9OW9spujwiOn8nI8tcpME+6oyqOCukzOljpRUb9OHUY96r6oGi3jQXhLrQh2RUrR3EjtwEhjOo2p94FSM2k7UcfqxU1Hm9Krfa/29aim3et/AFbrD2Db1LWOLeJ1X8uSoqSBS9SUYJ8GK4mP1IFgj/jCqvGMRztuXf5hmq8jel8Yc9TtOQsVlKp1sHrSVSkfC2quT9kIy4D+EXK56F7bVjwIHP/woWRvBZ3LOp/bQHX7Es1l2Aq7YSfcn6JDQj84OQutoTS3scBd2K/mX2UedQa9LTSZOpGN/x0/Ac/IgPGwf4TIAAAAAElFTkSuQmCC>

[image6]: <data:image/png;base64,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fuCgTR+xpor1WL2I7lGwTcNvqOFDEREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREREVke/g+NN0Vje3l2EQAAAABJRU5ErkJggg==>

[image7]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACgAAAAZCAYAAABD2GxlAAACSklEQVR4Xu2Wz4uOURTHv5MZ0Rg/J0ONRpKVsiA2s5JiQxoLk7K2nwX5A5SSktjZWNjZimRxM6WJhQ0pNQuSyUJK2cw0+H6d9477nO5z7zt6KfV+6tvbe849557n/jjPA/Tp0+efM0Rt9cYCA9QWb+yG29RX6iP1nlqmblIb00EJm6iH1GXYpDl2IR+v8YpVjirrqWvUVWpbYj9OfaHeUnsTe+QiNYdmTIpyLlInvaODYpWj7eF+sYN6Ciskx1nqO/XYO8gnatrZtlPPYKsv/UB7gYrVjh30jsg49QKW6IzzRXRWNEYTpeip31Bjzr4GVuRmKqBcoGKV4xZaVlHBOm+57Yso8B5s7LrEfoqaSv7nCCgXKJRDY5SvgaqXo7X6Dhvwe6K0wCvUgeR/joB6gdreb7B8DVSxztYJ73DsoRbQ3OJB6j61M7HlCKgXqBzvYLvUQO1DE6uAEnoQTZIWGFe11ssC6gWOUq9hY1cYpp5Qs9RI6nBo63UENMmHxB4L1G+JgHqBMZe0Qjz4MpYmuQGb4BXsZkZ6WaAWSAsVnB2XYEurJW5D7Uf98bCzxwJ9i/EE1AuMZ1A72kCtpXRJDsGKO+YdHe6ifn4D6gXGS3jHO4RWSK+xicSmRnsOdvX3JXbPDDXpjY6XsALPwz4ocijHEnXBOyIqSL1Ir53T1G6U+2JEL/kHaPbG1aJY5XgE+x7oOZ+p/d64ChSrHP593jOew256NyueQ7H6CKl1gz9Gt3ueOuIdXaJY3yH+CkeReZcWWEtd98b/hp/r+Xdi7SXxKQAAAABJRU5ErkJggg==>

[image8]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAADgAAAAZCAYAAABkdu2NAAACDUlEQVR4Xu2WQUgWQRiGv9DExIw0jMAOiRe7morh0YiIMKxLCnrtIB28dA2igzf1oojRPQU9JAVd/lMKgkYQQp0SQ1DKk4KE5vv6zeDsuP+ugu56mAcedOab5Z93d2dmRQKBQCBwOnyGK3Ad/vRqeXEB1sAb8KJX87kE60WvieUBHIT/4axXy4Nm+A0uwHG4Kzo/BvFh+CVYgJXRUpSXogH5N29+wcdO+73o3EZhqel7Dv+aflqQhIDlok9uD3Z4tTzghHdgm2nbm/8HNpo+hrku+gRTAzIUw03J4R3ihddgiR2UIU2wwWm/Ew3xSeJf09SAb0QHDZj2M/hD9N3+aAflyAbchC1+wZAasAD/wXbRJ/cCtsLfouuhGNy17sKnJ/DhwZXpVMMncBjOwdvRcoTUgGvwO7wJR0Qnbt/7eWecz1kH7IQ9okdYvxQ/LlIDcsCy6HloN5kqeF/0LMqb16JznIRXvBpJDHgZbsE7oq/BF9gdGZE9j0RvsIWHPZcKg8Sd04kBue3yQL1q2lyHM7BMdMfqNf1xVMAPoj9wXLlhJHFLdNy06PFF6uCq6T9xwC444bR592ybYd86tSzgDbdBbEB+2Wyb/lemzyUxIDcVLmQLP9u4wXBBj8F7Ti0L+LtfRTc8y5BoiEVY6/QTe9Bzp+WTPkJcam4stOjHawYwID/X+sz/gUAgcL7YB1Dse/g1MSM/AAAAAElFTkSuQmCC>

[image9]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAADwAAAAZCAYAAABtnU33AAADPUlEQVR4Xu2XS6hOURTH//KIvB95FJl4TyQir4lIwsArr5IyYUJRFKlrYGCgJAN5DEwMpChEGNwkCYWBFBmQRwwYUd7Wr7X3/fbZ95yPm3Cvvl/9+87Ze51z9tp7rbX3JzVo0KBBg45BJ9NA0wzTyKwvZ6PpuemraXHW96+4YnpmemN6nPWVclNuHB05aOpZsKgx3vTS9F3tx+EFpn3yMV3I+lrBis6XrzIMlz941dQ3GmWcUPtyGHbIx8RvXVjNJao5DO/kD29I2lLam8Pd5Sv7zTQ36yuAk/tNl029kvZm1Va5LLTrOcw7h5kGmzpnfZGupiGqfXOMabl+Xj+qwEmcPW3qEtr4xiCVjIEB5o0P5A6x+mWUOcx7yKUnpiPySSPXFyU2MMD0QV5k3ppuma6Znsq/yyDbyl75eLaG+9WmR6a7povRqB7MFmE9Ke8I5A7j7DbTJ9OyaBSuaYvpwooSTaPCPatwyjTZtMm0J7S1lWbTZ9Ms+fObTdNML+QTWRe2Jyp1OvCc3GEG/N50ydQjGoVr2uLExdBL04QiUxVJv8oreXSMkL+LCY5FjB2oEmaHcFyhYhHLyR3mI9zvbLGogS0hB6vkdjHPgIFh8zvwzofy/TgWrT7y3YcFbAXObTfdVy3cyLWZKg4ukjvcHO7LtgTaYvHjOxTJXfLtb6U8dfLtj6K3Wz75W7K+nN7y6JpimmC6YVpTsChhvTy3hiZtOFM187nDcYWPtVjUwDaGLI6dlzvDNnLctDD0pRyVhyfVm4LG5FTBQei2qX+4J4/PmrrJU2pdaC9Ano2Tz2wUHy1bMcgdnm36KF/pdHvDQXJoerjnvXdq3ZVcl9sCz1aNA5aqONGMKd7jPJNagAcYfJnK9lk4J+8nJwnTmBIUu3hYoY3rL+Ea4gTckw+EkEUTExt4bRobrnG8KtKA6Fmb3LM1MkHUo8OmeUnfH4NB9svacKhJvvemznF9Ul69mcAU+g6o/oEkjagIhQql3/nrcLLiUFK2WuQof1zyGsDWSCHtkFBASAVOVjlz5Kcv/npGpprOqFhIOxysJNsGocs1oT9a7ix/7+IpixA+JN8aoSn8/rfkxZNC1KCt/AB5eLGCD7EyLwAAAABJRU5ErkJggg==>

[image10]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAC8AAAAZCAYAAAChBHccAAACoUlEQVR4Xu2WS6iNURTHl1CEvF9RRJER5THCQJ4DBhh4FANJpgZuzCVKSaSUZCQlkkeRwY2Jx8gApQyYCDEQCnn8f3ft7exvnXMv5ybnlPOvX+fsx7e//7f22uv7zDrqqKOO/pWWixfim1gbxlqlueKRua8vYayiKWKb+GHtYx5PG8Vtc1+/VTuZRyPEHfEyDjRSu5mfI96K7tDfUL2ZHygmiMlxIGhIAg0WS8WK2nDTOmbuaV3RN8pq96gomsf0ZvFKXBanxQ0xtZiDJomL4r34JG6Ke6nvazGvGY0WD8wjzw5wz+vioXhczPulaH6veCcWFX2nxFMxLbXHivvmNxovpotn4oqYmeb3R2XKDBfnxSxx0no5wNH8Z3FcDCj65ouP4pwYZL6lXHc2jdN3wTzii1Nff7TVfF1SZ54YY27+jbmvOkXztHcVbUTeP7fadu6xqnnE/7hWs8r5fsk86oggUv95iDqVN2SraK+vDfcom89zWfCIeTS2i33mO7M7X1BotjgstoihYaxUmTJEfIe4JUYWc+pUmscU7a7acI9i5MeJa2KluTHAZBTrHjI/F5wJDh8BaiQCxr0PpDZeCAgpi9ak34riVvO5cMaqOU8ek88cSKLHgpz+GcWcKEob8zknGN4kvlu1BJaKJZLrc7C4nnUqoi5zAQtns1Qbyh9Rzbpq1WpD5PkG+SDumlcXYKupRIibk7uUOfryoY+7mkWK8FbNAaEIsNvsOu+NE6n/j9TXS+qo+cOUqUIgVpl/VPFgUexCXznMA8YXEe2J5mv/NZG/+2NnEuWOCJdiVzHOQWy5us0PbDx8RIjz8iT0bxDDQl/LxCHCIG/Z1eaptcT8E+G1WFabagvEwfR/odhZjLWtiDSpQkHIUAh44fy/+gnCnpVcU/S6wwAAAABJRU5ErkJggg==>

[image11]: <data:image/png;base64,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>