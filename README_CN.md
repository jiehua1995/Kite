# Kite

<p>
	<img src="doc/logo.png" alt="logo" width="100" align="left">
</p> 
**Kite** 是一个用 Rust 编写的高性能精确 k-mer 工具箱，覆盖了从计数、存储、导出，到多样本分析和 Quarto/R 报告生成的一整条流程。

它的核心目标很明确：

- `count` 要快
- `dump` 要快
- `.kite` 格式要清楚、可复用
- 查看、合并、矩阵构建、分析和 Quarto/R 报告生成要内建

Kite 的代码结构按子命令组织。每个子命令基本都放在自己的 `src/<command>/` 目录中，而 `.kite` 格式和公共逻辑作为共享层被复用。

## 子命令总览

| 子命令 | 作用 | 典型输入 | 典型输出 |
|---|---|---|---|
| `kite count` | 从 FASTQ 输入构建 `.kite` 数据库 | `FASTQ`、`FASTQ.GZ`、`BGZF FASTQ` | `.kite` |
| `kite dump` | 从 `.kite` 导出精确 k-mer 计数表 | `.kite` | `TSV` |
| `kite info` | 打印一个数据库的结构化元信息和摘要 | `.kite` | stdout |
| `kite query` | 查询一个精确 k-mer 的计数 | `.kite` + k-mer | stdout |
| `kite histo` | 统计一个或多个数据库的 count 频率分布 | `.kite`... | `.tsv` |
| `kite merge` | 精确合并多个兼容数据库 | `.kite`... | `.kite` |
| `kite profile` | 构建 union-by-sample 原始计数矩阵 | `.kite`... | `.tsv` |
| `kite analyze` | 构建精确多样本矩阵，输出统计表和 Quarto 报告 | `.kite`... | 输出目录 |
| `kite citation` | 打印版本和引用信息 | 无 | stdout |
| `kite fly` | 一键完成 count + analyze | FASTQ... | 输出目录 |

## CLI 设计原则

| 原则 | 说明 |
|---|---|
| 查看命令尽量轻量 | `info` 和 `query` 直接输出到 stdout，不暴露线程、内存、输出路径和进度参数。 |
| 多文件输入风格统一 | 接受多个输入的命令统一使用一个 `-i/--input` 后接所有文件，例如 `-i sample1.kite sample2.kite`。 |
| 样本标签统一 | `histo`、`profile`、`analyze`、`fly` 统一使用 `--label sample1 sample2 ...`，标签数必须和输入数一致。 |
| 内存参数格式严格 | `count` 和 `fly` 使用 `--memory`，必须写成 `512M`、`64G`、`1T` 这种格式。 |
| 线程参数统一 | 暴露线程控制的命令统一使用 `-t, --threads`。 |
| Quarto/R 报告 | `analyze` 输出 TSV 表和 `analysis_report.qmd`；需要图片时用 Quarto/R 渲染报告。 |
| `fly` 就是一键流程 | `fly` 只保留少量最有价值的参数，不把内部实现细节全部暴露给用户。 |

## 构建

```bash
cargo build --release
```

生成的程序：

```text
target/release/kite
```

## 快速开始

统计 31-mers：

```bash
./target/release/kite count \
  -k 31 \
  -i reads.fastq.gz \
  -o sample.kite \
  -t 48 \
  --memory 64G \
  --buffer 16 \
  -p
```

导出精确计数：

```bash
./target/release/kite dump \
  -i sample.kite \
  -o sample.tsv \
  -t 48 \
  -p
```

查看数据库信息：

```bash
./target/release/kite info -i sample.kite
```

查询一个 k-mer：

```bash
./target/release/kite query -i sample.kite -k ACGTACGTACGTACGTACGTA
```

对多个数据库做 histogram：

```bash
./target/release/kite histo \
  -i tumor.kite normal.kite \
  --label tumor normal \
  -o histo.tsv
```

合并兼容数据库：

```bash
./target/release/kite merge \
  -i sample1.kite sample2.kite sample3.kite \
  -o merged.kite
```

构建多样本原始计数矩阵：

```bash
./target/release/kite profile \
  -i tumor.kite normal.kite control.kite \
  --label tumor normal control \
  -o profile.tsv
```

执行多样本分析：

```bash
./target/release/kite analyze \
  -i tumor.kite normal.kite control.kite \
  --label tumor normal control \
  -o analysis_out \
  --correlation all \
  --embedding all \
  --heatmap-kmeans 6 \
  --heatmap-seed 1 \
  --cluster-samples true
```

执行一键流程：

```bash
./target/release/kite fly \
  -i tumor.fastq.gz normal.fastq.gz control.fastq.gz \
  --label tumor normal control \
  -o fly_out \
  -k 31 \
  -t 48 \
  --memory 64G \
  --correlation all \
  --embedding all \
  --heatmap-kmeans 6 \
  --heatmap-seed 1 \
  --cluster-samples true
```

## 参数说明

### `kite count`

| 参数 | 默认值 | 说明 | 备注 |
|---|---|---|---|
| `-k, --kmer` | `21` | k-mer 长度 | 支持范围 `1..128` |
| `-i, --input` | 必填 | 输入 `FASTQ`、`FASTQ.GZ` 或 `BGZF FASTQ` | 当前一次处理一个输入文件 |
| `-o, --output` | 必填 | 输出 `.kite` 路径 | 建议以 `.kite` 结尾 |
| `-t, --threads` | `0` | 总线程预算 | `0` 表示自动 |
| `--partitions` | `0` | 分区数量 | `0` 表示自动规划；手动指定时必须是 2 的幂 |
| `--chunk-kmers` | `8000000` | reader 侧目标批大小 | 会影响 flush 频率和内存行为 |
| `--memory` | 自动探测 | 总内存预算 | 必须写成 `512M`、`64G`、`1T` |
| `--canonical` | `false` | 是否 canonicalize k-mer | 接受 `true` 或 `false`；会改变结果语义 |
| `--mode` | `fast` | 输出压缩模式 | `fast` 只写 DVLE/raw；`balanced` 和 `compact` 会在更小时额外尝试 zstd |
| `--buffer` | `16` | staging 缓冲目标大小，单位 MB | 独立于 `--mode`；更大通常 flush 更少，但内存占用更高 |
| `-p, --progress` | `false` | 显示进度 | 长任务建议开启 |
| `--test-mode` | `false` | 打印详细运行统计 | 适合 benchmark 和调参 |

### `kite dump`

| 参数 | 默认值 | 说明 | 备注 |
|---|---|---|---|
| `-i, --input` | 必填 | 输入 `.kite` 文件 | 每次只处理一个数据库 |
| `-o, --output` | 必填 | 输出 TSV 路径 | 传 `-` 可输出到 stdout |
| `-t, --threads` | `0` | dump worker 线程数 | `0` 表示自动 |
| `-p, --progress` | `false` | 显示进度 | 大文件建议开启 |
| `--test-mode` | `false` | 打印详细运行统计 | 用于验证和 benchmark |

### `kite info`

| 参数 | 说明 |
|---|---|
| `-i, --input` | 输入 `.kite` 文件 |

`info` 直接向 stdout 打印结构化摘要。

### `kite query`

| 参数 | 说明 |
|---|---|
| `-i, --input` | 输入 `.kite` 文件 |
| `-k, --kmer` | 要查询的精确序列 |

`query` 会打印数据库路径、查询序列、目标分区和精确计数。

### `kite histo`

| 参数 | 默认值 | 说明 | 备注 |
|---|---|---|---|
| `-i, --input` | 必填 | 一个或多个 `.kite` 文件 | 使用一个 `-i` 后接所有文件；每个输入对应一列 histogram |
| `--label` | 由文件名推断 | 样本标签 | 数量必须和输入数一致 |
| `-o, --output` | 必填 | histogram TSV 输出 | 第一列是 `count` |
| `-p, --progress` | `false` | 显示进度 | 输入较多时更有用 |

### `kite merge`

| 参数 | 默认值 | 说明 | 备注 |
|---|---|---|---|
| `-i, --input` | 必填 | 两个或更多 `.kite` 文件 | 使用一个 `-i` 后接所有文件；输入必须兼容 |
| `-o, --output` | 必填 | 合并后的 `.kite` 输出路径 | 精确求和，不做 sketch |
| `-p, --progress` | `false` | 显示进度 | 同时会打印兼容性摘要 |

兼容性要求：

- `k` 相同
- `canonical` 设置相同
- `partitions` 相同
- `key_width` 相同

### `kite profile`

| 参数 | 默认值 | 说明 | 备注 |
|---|---|---|---|
| `-i, --input` | 必填 | 一个或多个 `.kite` 文件 | 使用一个 `-i` 后接所有文件；会对观察到的 k-mer 并集建表 |
| `--label` | 由文件名推断 | 样本标签 | 数量必须和输入数一致 |
| `-o, --output` | 必填 | 输出 TSV 路径 | 第一列为 `kmer` |
| `-p, --progress` | `false` | 显示进度 | 多样本时建议开启 |

### `kite analyze`

| 参数 | 默认值 | 说明 | 备注 |
|---|---|---|---|
| `-i, --input` | 必填 | 一个或多个 `.kite` 文件 | 使用一个 `-i` 后接所有文件；用于构建精确多样本矩阵 |
| `--label` | 由文件名推断 | 样本标签 | 数量必须和输入数一致 |
| `-o, --output-dir` | 必填 | 输出目录 | 输出 TSV 表和 `analysis_report.qmd` |
| `--correlation` | `all` | 相关性模式 | `all`、`pearson`、`cosine` |
| `--embedding` | `all` | 降维模式 | `all`、`pca`、`tsne`、`umap` |
| `--heatmap-kmeans` | `6` | 热图行的 k-means 聚类数 | 应用于高方差 k-mer |
| `--heatmap-seed` | `1` | 热图和随机降维的随机种子 | 同时影响 `t-SNE`、`UMAP` |
| `--cluster-samples` | `true` | 是否对样本列聚类排序 | 接受 `true` 或 `false` |
| `-p, --progress` | `false` | 显示进度 | 多样本分析建议开启 |

`analyze` 会先在 Rust 侧生成精确表，再生成 `analysis_report.qmd`。需要图片时，用 Quarto 渲染这个文档，由 R/ggplot2 和 ComplexHeatmap 负责出图。

### `kite analyze` 输出文件

| 文件 | 含义 |
|---|---|
| `exact_count_matrix.tsv` | 精确多样本 raw-count 矩阵 |
| `sample_summary.tsv` | 每个样本的总 raw count 和非零 k-mer 数 |
| `correlation_pearson.tsv` | 当选择 `--correlation all` 或 `pearson` 时写出 |
| `correlation_cosine.tsv` | 当选择 `--correlation all` 或 `cosine` 时写出 |
| `embedding_pca.tsv` | 当选择 `--embedding all` 或 `pca` 时写出 |
| `embedding_tsne.tsv` | 当选择 `--embedding all` 或 `tsne` 时写出 |
| `embedding_umap.tsv` | 当选择 `--embedding all` 或 `umap` 时写出 |
| `heatmap_matrix.tsv` | 带聚类编号的行标准化热图矩阵 |
| `heatmap_plot_matrix.tsv` | Quarto 报告使用的较小热图矩阵 |
| `heatmap_clusters.tsv` | 热图聚类中心表 |
| `heatmap_samples.tsv` | 热图样本列顺序 |
| `analysis_report.qmd` | 用于从 TSV 表生成图片的 Quarto/R 报告 |

R 端说明：

- Kite 本身不渲染图片。
- 生成的 `analysis_report.qmd` 在渲染时会声明并安装所需 R 包。
- 即使没有安装 R 或 Quarto，Kite 仍然会正常输出所有 TSV 表。

### `kite citation`

| 参数 | 默认值 | 说明 |
|---|---|---|
| `-o, --output` | `-` | 输出路径；`-` 表示 stdout |
| `--format` | `text` | `text` 或 `bibtex` |

### `kite fly`

| 参数 | 默认值 | 说明 | 备注 |
|---|---|---|---|
| `-i, --input` | 必填 | 一个或多个 FASTQ 输入 | 使用一个 `-i` 后接所有文件；每个输入会生成一个 `.kite` |
| `--label` | 由文件名推断 | 样本标签 | 数量必须和输入数一致 |
| `-o, --output-dir` | 必填 | 输出目录 | 会创建 `databases/` 和 `analysis/` |
| `-k, --kmer` | `31` | `count` 使用的 k-mer 长度 | |
| `--canonical` | `false` | 内部 count 是否 canonicalize k-mer | 接受 `true` 或 `false`；内部使用 `fast` 模式 |
| `-t, --threads` | `0` | 传给 `count` 的线程预算 | `0` 表示自动 |
| `--memory` | 自动探测 | 传给 `count` 的内存预算 | 必须使用 `M`、`G` 或 `T` 后缀 |
| `--correlation` | `all` | 传给 `analyze` 的相关性模式 | |
| `--embedding` | `all` | 传给 `analyze` 的降维模式 | |
| `--heatmap-kmeans` | `6` | 传给 `analyze` 的热图聚类数 | |
| `--heatmap-seed` | `1` | 传给 `analyze` 的随机种子 | |
| `--cluster-samples` | `true` | 传给 `analyze` 的样本聚类开关 | |
| `-p, --progress` | `false` | 显示进度 | 会传给内部的 `count` 和 `analyze` |

`fly` 的执行流程是：

1. 对每个输入测序文件执行 `count`
2. 对生成的 `.kite` 数据库执行 `analyze`
3. 生成用于下游出图的 `analysis_report.qmd` 和分析结果

## 安装方式

当前推荐的安装方式就是直接通过 Cargo 从源码构建：

```bash
cargo build --release
```

生成的程序位于：

```text
target/release/kite
```

这种方式最直接，也方便用户自行构建自己需要的版本。
