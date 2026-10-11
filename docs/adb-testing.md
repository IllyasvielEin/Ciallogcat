# ADB 子进程插桩测试

这些测试通过现有 `CIALLOGCAT_ADB` 接口替换 ADB 可执行文件，真实启动子进程、读取管道并接收后台事件。不需要 Android SDK、USB 设备、ADB server 或图形桌面，也不会转发命令给真实 ADB。它们不是 Android APK instrumentation test，不能代替真机或 GUI 验证。

## 运行

依赖当前 Rust stable、Python 3（仅标准库）。Rust 子进程测试当前仅在 Linux 编译和运行，Python fixture 自测也以 Linux 为验证平台。

```bash
# 独立验证 mock 的命令协议和 watchdog
python3 -B tests/fixtures/test_fake_adb.py

# 运行所有 ADB 插桩场景；忽略项是仅由外层测试调用的内部 worker
cargo test --locked adb::integration_tests -- --nocapture

# 完整项目检查
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

本次云端配置把 Rust stable、rustfmt 和 Clippy 安装到被忽略的 `target/toolchains/`，可以用 `. target/toolchains/env.sh` 激活。该文件仅存在于配置过的工作目录，普通开发环境继续使用自己的 Rust stable。清理 `target/` 会同时移除此本地工具链；可按 README 的官方 rustup 指引重新安装。测试不依赖这个安装位置。构建时使用已有 GCC、pkg-config 与系统运行库，未更改系统软件包。

## 文件与隔离

- `tests/fixtures/fake_adb.py`：可复用、严格匹配命令的 Python fake ADB。
- `tests/fixtures/test_fake_adb.py`：mock 自测，确认异常命令、退出码和 watchdog 等行为。
- `tests/support/adb_integration.rs`：由 `src/adb.rs` 在 Linux 测试构建中引入，直接测试生产后端。应用运行时不包含这些测试。
- 运行时副本、独立配置、JSONL 命令轨迹和 worker 输出放在 `target/adb-integration/`；Python 自测的临时文件也位于 `target/`。源码中的 fixture 是版本管理素材，不是生成的诊断文件。

每个场景单独启动当前 Rust 测试程序，在创建进程时设置 `CIALLOGCAT_ADB`、`CIALLOGCAT_CONFIG_DIR`、`CIALLOGCAT_FAKE_SCENARIO` 和 `CIALLOGCAT_FAKE_TRACE`。不修改共享测试进程的环境变量，因此普通并行 `cargo test` 不会相互污染。每个场景会实际写入并读回自己的配置，并断言发现的 ADB 路径就是该场景的 mock。

外层 worker 有 20 秒硬截止；后台事件收集使用总截止时间；每个 mock 进程另有独立 8 秒 watchdog，堵塞在 stdout 写入时仍可退出。异常清理只对轨迹记录中、且 `/proc/<pid>/cmdline` 精确包含本场景独有可执行路径的进程发信号。正常场景检查所有 mock 子进程均已回收，包含避免僵尸进程的检查。轨迹和 worker 日志保留在 `target/` 方便诊断。

## 覆盖范围

14 个外层 Rust 场景覆盖：

- 正常设备发现、包名排序去重、进程映射
- 空设备列表、unauthorized/offline 状态
- USB / Wi-Fi / emulator 多设备，以及显式选择非首个序列号
- `ps -A -o PID,NAME` 出错或空结果时回退 `ps -A`
- 查询失败传播；生产查询 5 秒超时、错误返回和子进程回收
- logcat 标记、Unicode、非法 UTF-8、超过 1 MiB 单行、后续行及无换行末尾
- stderr、非零退出、stdout 已关闭但子进程仍存活；同一 stderr 场景覆盖超长行、非法 UTF-8、后续错误及队列饱和时停止
- 六轮启动/暂停/重启/Drop，验证 session ID、重复 stop 和进程回收
- 显式 `-T 1`、默认缓冲区、排序后的缓冲区和 `all` 互斥参数
- 32 MiB 有界 burst，在持有队列 permit 时验证 16 MiB 字节预算阻塞，释放后验证全部 128 行到达
- 事件通道已满时快速停止，再丢弃接收端并验证清理

burst 使用大体积 marker 行，专门测试真实管道和队列的背压，而不是把 debug 模式下超长 threadtime 正则解析的耗时当成吞吐测试。超长 threadtime 行仍由独立的 stream_edges 场景验证。

## 已知限制与发现

- 生产查询并发读取 stdout/stderr（各最多 16 MiB），5 秒超时后终止并回收直接 ADB 子进程。挂起测试不再由 harness 主动杀进程，而是验证生产超时先于 fixture 的 8 秒 watchdog 返回。尚无立即取消接口；`ps` 兼容回退最多执行两次命令。若异常可执行文件派生进程并继承管道，读取线程可能继续等待该派生进程关闭管道；本测试不承诺通用进程树清理。
- stdout EOF 后提供 250 ms 自然退出宽限，每 5 ms 检查并释放子进程锁；非零退出场景严格断言返回 17。超过宽限仍不退出的子进程会终止并回收；这不是所有调度环境下都能保留自然退出码的保证。
- 这些测试不验证窗口绘制、GPU 性能、实际 Android 授权、ADB 版本差异或长期真机吞吐。

采集 stderr 仅保留每行最多 4 KiB 的有效 UTF-8 诊断前缀，超长内容会标记截断，并继续读取下一行。4096 个事件的有界通道将排队 stderr 文本载荷限制在约 16.15 MiB（含截断标记）；这不是总进程 RSS 上限，事件结构、分配器和其他应用内存另计。stdout 日志仍保持原有 1 MiB 行限制和独立的 16 MiB 排队字节预算。
