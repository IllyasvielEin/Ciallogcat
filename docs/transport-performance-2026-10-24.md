# 三种采集与存档组合性能复测（2026-10-24）

本轮 compact v3 明显缩小 protobuf 工件；CPU 用时较 v2/proto 略降，但端到端吞吐没有普遍提升。保持显式选择，不据此切换默认传输。全部 6 次预热和 30 次测量通过原始 payload、记录身份、footer 和 CLI 状态校验。

## 方法与证据

- 冻结 CLI SHA-256：`ad9fc2abb214ffad82e0662b64d52b2efb578ee1614b4108cf19d0e720091351`。
- 环境：`Linux-6.18.33.2-microsoft-standard-WSL2-x86_64-with-glibc2.43`。每组合每负载 1 次预热、5 次测量；按轮次轮换三种组合，表中为中位数。基准完成后才启动本轮 GUI 体验进程。
- 相同预编码输入，proto 两组逐字节相同；text 组消息/Tag/PID/TID/级别相同，proto 另外保存 UID、精确时间、进程名和 raw payload，因此 text 是功能不同的参照。
- wall 包含启动、采集、写入及子进程回收；CPU 包含 CLI 与它回收的 mock 子进程。RSS 为每 5 ms 采样的 CLI 单进程峰值，不能当作进程树总和。
- 本地缓冲文件写入，无 fsync 保证；不测 Android 编码、ADB/USB、GPU 或 GUI 留存。5 次有限测量不提供统计显著性或跨设备保证。
- [复现工具](../tools/transport-benchmark.md)。旧主机原始报告与逐次记录位于 `target/queue-benchmark-2026-10-24/report/{report.json,runs.jsonl}`，包含输入、工具、replay hash 和运行顺序；这些可选原始证据不随源码迁移。

## 中位数

MB 使用十进制；KiB 为 1024 字节。吞吐是完整进程 wall 口径，尤其短任务受固定等待/回收开销影响。

| 消息负载 | 组合 | 工件 MB | wall 秒 | CPU 秒 | 条/秒 | CLI 峰值 RSS KiB |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 50,000 × 128 B | v2/text | 26.879 | 1.283 | 0.914 | 38,984 | 5,376 |
| 50,000 × 128 B | v2/proto | 60.129 | 1.318 | 0.930 | 37,941 | 4,648 |
| 50,000 × 128 B | v3/proto | 27.879 | 1.292 | 0.857 | 38,695 | 4,708 |
| 20,000 × 2048 B | v2/text | 49.139 | 0.631 | 0.709 | 31,707 | 5,740 |
| 20,000 × 2048 B | v2/proto | 164.839 | 0.490 | 0.521 | 40,818 | 5,456 |
| 20,000 × 2048 B | v3/proto | 62.339 | 0.593 | 0.480 | 33,744 | 5,504 |

## 对选择策略的影响

- 128 B 消息：v3 相比 v2/proto 工件减少 **53.63%**，CPU 中位数减少 **7.8%**，吞吐变化 **+2.0%**。v3 工件仍为 v2/text 的 **1.04 倍**，吞吐变化 **-0.7%**。
- 2048 B 消息：v3 相比 v2/proto 工件减少 **62.18%**，CPU 中位数减少 **7.8%**，吞吐变化 **-17.3%**。v3 工件仍为 v2/text 的 **1.27 倍**，吞吐变化 **+6.4%**。

短消息三组约 3.8–3.9 万条/秒，差异小；2 KiB 消息下 v2/proto 的 wall 最短，v3/proto 虽然写得更少、CPU 更低，端到端吞吐却低约 17%。目前没有分段计时证据能把差异归因于 encoder、队列或回收等待；不把“存档更小”写成“采集一定更快”。本轮 v3 的成立理由是保留原始 payload 的存档效率和可读派生视图。

| 负载 | 组合 | 5 次 wall 范围（秒） |
| --- | --- | ---: |
| 128 B | v2/text | 1.256–1.533 |
| 128 B | v2/proto | 1.292–1.319 |
| 128 B | v3/proto | 1.245–1.293 |
| 2048 B | v2/text | 0.582–0.655 |
| 2048 B | v2/proto | 0.490–0.491 |
| 2048 B | v3/proto | 0.566–0.593 |

RSS 在本负载中相近；v3 当前只去重磁盘格式，内存仍保留 raw/decoded/display 的完整证据和展示。流式 CLI 的低 RSS 不能外推 GUI 打开整个文件的内存。此前 codec 的 70,000 条转换验证见[存档方案](protobuf-storage-plan.md#已测取舍)，其 header 与本轮直接采集有少量元数据差异，文件字节数不应混用。

## 复现

```sh
cargo build --locked --release --no-default-features --bin ciallogcat-cli
mkdir -p target/my-transport-benchmark
install -m 755 target/release/ciallogcat-cli target/my-transport-benchmark/ciallogcat-cli
python3 -B tools/transport-benchmark.py \
  --cli target/my-transport-benchmark/ciallogcat-cli \
  --output-dir target/my-transport-benchmark/report \
  --include-compact --trials 5
```

输出 report 目录必须尚不存在。失败运行保留证据但不参与成功聚合；先检查报告 `state=completed`、`success=true` 与各组合 `complete=true`。
