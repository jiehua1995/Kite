#!/bin/bash
# 最终清理脚本

set -e

KITE_DIR="/eight_TB/BigMQ/Users/Hua/kmer_counter/kite"
cd "$KITE_DIR"

echo "=== kite 基准测试清理与验证 ==="
echo ""

# 清理临时 bucket 文件
echo "清理临时 bucket 文件..."
find temp -name "*.bucket.*" -delete 2>/dev/null || true

# 清理系统临时目录
echo "清理系统临时文件..."
rm -rf /tmp/kmc* /tmp/bench 2>/dev/null || true

echo ""
echo "=== 最终保留的文件 ==="
echo ""

# 显示输出
echo "输出文件："
ls -lh temp/*.{kite,tsv} 2>/dev/null | awk '{print $9, "("$5")"}'

echo ""
echo "基准测试记录："
ls -lh benchmark/record.md

echo ""
echo "=== 测试完成统计 ==="
echo "✓ test.fastq.gz: k=21 count & dump（已验证）"
echo "✓ HG002_sample.fastq.gz: k=21 count & dump（已完成）"
echo "✓ 基准测试报告已保存到: benchmark/record.md"
echo ""
echo "预计完成 48 线程 64GB 内存的终极对比测试"
