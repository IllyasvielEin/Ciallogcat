# ADB 子进程插桩测试

这些测试通过现有 `CIALLOGCAT_ADB` 接口替换 ADB 可执行文件，真实启动子进程、读取管道并接收后台事件。不需要 Android SDK、USB 设备、ADB server 或图形桌面，也不会转发命令给真实 ADB。它们不是 Android APK instrumentation test，不能代替真机或 GUI 验证。

## 运行

依赖当前 Rust stable。Linux 完整 Python 场景另外需要 Python 3（仅标准库）。原生 Rust fixture 通过 rustc 编译成测试专用可执行文件，Linux 与 Windows 共用，验证 CreateProcess/管道/停止等平台行为。

```bash
# 独立验证 mock 的命令协议和 watchdog
python3 -B tests/fixtures/test_fake_adb.py

# 无 GUI 构建核心库与所有 ADB 插桩场景；忽略项是仅由外层测试调用的内部 worker
cargo check --locked --no-default-features --lib
cargo test --locked --no-default-features --lib adb::integration_tests -- --nocapture

# Linux / Windows 原生子进程、二进制通道和生命周期回归
cargo test --locked --no-default-features --test native_adb_integration

# Agent cursor/read budget 和 native 查询（无需 ADB）
cargo test --locked --no-default-features --test query_integration

# 公共核心 API 回归：解析、过滤一致性、快照及通知顺序
cargo test --locked --no-default-features --test headless_core

# 验证无 GUI 依赖的完整目标集；依赖树不应包含 egui/eframe/epaint/winit/wgpu
cargo test --locked --no-default-features --all-targets
cargo tree --locked --no-default-features -e normal

# 完整项目检查
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --no-default-features --all-targets -- -D warnings
cargo build --locked --release
```

测试不依赖特定的 Rust 安装目录；普通开发环境使用自己的 Rust stable、rustfmt 和 Clippy。默认启用 `gui` 功能，完整项目检查仍会编译并测试桌面应用；`--no-default-features` 构建共享核心及独立的 `ciallogcat-cli`，无需 GUI 依赖；CLI 使用与桌面应用相同的 ADB 核心，详见 [CLI 指南](cli.md)。

## 文件与隔离

- `tests/fixtures/fake_adb.py`：可复用、严格匹配命令的 Python fake ADB。
- `tests/fixtures/test_fake_adb.py`：mock 自测，确认异常命令、退出码和 watchdog 等行为。
- `tests/fixtures/native_fake_adb.rs`：无第三方依赖的原生 executable fixture；严格匹配命令，不执行 shell。独立 12 秒 watchdog；包含原始二进制、非法 UTF-8、静默与 burst 场景。
- `tests/native_adb_integration.rs`：Linux/Windows 实际 CLI 回归。测试临时编译并复制独有 fixture 路径，通过子进程环境隔离场景，Windows 用只读 process wait handle 确认停止，Linux 检查 /proc 回收。生成文件与输出保留在 `target/native-adb-integration/`。
- `tests/query_integration.rs`：真实 CLI 查询、文件代际/游标验证和确定性读取字节计数；工件在 `target/query-integration/`。
- `tests/support/adb_integration.rs`：由核心库的 `src/adb.rs` 在 Linux 测试构建中引入，直接测试生产后端，使用默认无操作 `WakeSignal`，不依赖 egui。应用运行时不包含这些测试。
- `tests/headless_core.rs`：通过公共库 API 验证 threadtime / marker 解析、字面量与正则过滤、大小写、包名及级别规则、同步 / 异步结果一致性、快照隔离，以及过滤 / 高亮 / 后端事件发布后才调用通知。
- `tests/support/config_integration.rs`：仅在默认 GUI 的 Linux 测试中由 `src/config.rs` 引入，独立验证配置目录覆盖与文件保存 / 读回。
- 运行时副本、独立配置、JSONL 命令轨迹和 worker 输出放在 `target/adb-integration/`；Python 自测的临时文件也位于 `target/`；GUI 配置测试诊断文件位于 `target/config-integration/`。源码中的 fixture 是版本管理素材，不是生成的诊断文件。

每个场景单独启动当前 Rust 测试程序，在创建进程时设置 `CIALLOGCAT_ADB`、`CIALLOGCAT_CONFIG_DIR`、`CIALLOGCAT_FAKE_SCENARIO` 和 `CIALLOGCAT_FAKE_TRACE`。不修改共享测试进程的环境变量，因此普通并行 `cargo test` 不会相互污染。每个场景断言发现的 ADB 路径就是该场景的 mock。配置文件的路径覆盖、保存和读回由单独的 GUI 配置子进程测试验证；其环境同样只在创建子进程时设置，不修改共享测试进程环境。

外层 worker 有 20 秒硬截止；后台事件收集使用总截止时间；每个 mock 进程另有独立 8 秒 watchdog，堵塞在 stdout 写入时仍可退出。异常清理只对轨迹记录中、且 `/proc/<pid>/cmdline` 精确包含本场景独有可执行路径的进程发信号。正常场景检查所有 mock 子进程均已回收，包含避免僵尸进程的检查。轨迹和 worker 日志保留在 `target/` 方便诊断。

## 覆盖范围

16 个外层 Rust 场景覆盖：

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
- stdout EOF 后迟到的 stderr 必须早于唯一 `Exited`；终态保证两条输出管道均已排空
- 持有接近整个字节预算时停止，子进程仍先被回收；释放 permit 后全部已读行与终态完整到达

burst 使用大体积 marker 行，专门测试真实管道和队列的背压，而不是把 debug 模式下超长 threadtime 正则解析的耗时当成吞吐测试。超长 threadtime 行仍由独立的 stream_edges 场景验证。

CLI 集成测试还使用 `once_then_idle`：输出一条记录后保持静默，直到 watchdog 或被停止。Linux `/dev/full` 回归让该条输出立即失败，确认 CLI 不等待下一条日志或完整采集期限，保留真实写入错误并回收 ADB。GUI 的真实子进程回归另外覆盖发现失败/offline/missing 不杀活流、用户仍能暂停，以及设备缺失时切换缓冲区后能在设备恢复时重新采集。

## 已知限制与发现

- 生产查询并发读取 stdout/stderr（各最多 16 MiB），5 秒超时后终止并回收直接 ADB 子进程。挂起测试不再由 harness 主动杀进程，而是验证生产超时先于 fixture 的 8 秒 watchdog 返回。普通发现查询无前端取消；实验 proto 的启动选项支持取消回调与更早 deadline；`ps` 兼容回退最多执行两次命令。若异常可执行文件派生进程并继承管道，读取线程可能继续等待该派生进程关闭管道；本测试不承诺通用进程树清理。
- stdout EOF 后提供 250 ms 自然退出宽限，每 25 ms 检查并释放子进程锁；主动 stop 立即发送 kill。非零退出场景严格断言返回 17。超过宽限仍不退出的子进程会终止并回收；这不是所有调度环境下都能保留自然退出码的保证。
- 这些测试不验证窗口绘制、GPU 性能、实际 Android 授权、ADB 版本差异或长期真机吞吐。

采集 stderr 仅保留每行最多 4 KiB 的有效 UTF-8 诊断前缀，超长内容会标记截断，并继续读取下一行。4096 个事件的有界通道将排队 stderr 文本载荷限制在约 16.15 MiB（含截断标记）；这不是总进程 RSS 上限，事件结构、分配器和其他应用内存另计。stdout 日志仍保持原有 1 MiB 行限制和独立的 16 MiB 排队字节预算。

## 原生跨平台 fixture

`tests/fixtures/native_fake_adb.rs` 是独立、无外部 crate 的 Rust 可执行程序，测试时用 rustc 构建到 target 隔离目录，不依赖 Windows 执行 Python shebang。`tests/native_adb_integration.rs` 在 Linux/Windows CI 共用，覆盖 Unicode/坏 UTF-8/部分 EOF、有限输出、静默停止与回收、stderr、shell_v2 探测以及精确 protobuf payload 字节。所有场景有 watchdog、命令期限及独立文件；Windows 只读进程 handle 用于确认退出，不把保存 PID 作为控制权限。

本地 Linux 实际运行、MSVC 交叉编译已通过；这不是 Windows 实际执行或 Android 真机证据。完整协议畸形帧和旧版设备场景仍由产品 decoder 测试及 Python fixture 补充。

## Linux 原生窗口启动烟测

Linux GUI CI 另外运行 [native-gui-smoke.py](../tools/native-gui-smoke.py)，使用
Xvfb 的独立 DISPLAY 和软件渲染启动实际 GUI 可执行文件。该脚本仅依赖 Python
标准库与 ctypes X11；不执行键鼠操作，也不触碰已有用户窗口。

```bash
cargo build --locked --bin ciallogcat
xvfb-run -a env WINIT_UNIX_BACKEND=x11 LIBGL_ALWAYS_SOFTWARE=1 \
  python3 -B tools/native-gui-smoke.py --binary target/debug/ciallogcat
```

CI 安装 xvfb、xauth、libgl1-mesa-dri、mesa-vulkan-drivers 和已有 GUI 开发库，
包括 libxkbcommon-x11-dev。烟测给自己的 GUI 子进程配置独立临时目录和
`no_devices` fake ADB，在 15 秒启动期限内检查 `_NET_WM_PID` 精确匹配该
子进程、窗口处于 mapped/viewable 状态且尺寸非零，并确认短暂持续存活。
结束时只 terminate/kill 并 wait 自己启动的 GUI。成功或失败的 stderr、stdout、
fake ADB 轨迹和 result.json 保留在 `target/native-gui-smoke/` 的独立运行目录；
CI 失败时上传这些工件。

这个测试能够捕获 `ldd` 看不到的运行时 `dlopen` 依赖缺失，例如
libxkbcommon-x11。deb 安装验证还断言包显式依赖 libxkbcommon-x11-0 且依赖已
安装。本地已分别验证有该库时窗口成功映射、缺库时烟测失败并保留 panic 诊断；
本地验证使用既有 X11 显示，CI 配置使用独立 Xvfb。

**窗口成功 mapped 只证明原生启动，不代表 GPU 呈现、输入法、剪贴板、Wayland、
Windows 或 Android 实机验收。** 控件行为仍由 GUI Harness 测试；系统交互与
设备测试继续使用各自的验收矩阵。

## 长流观测

`python3 tools/capture-soak.py --cli PATH --output-dir NEW_DIR --seconds 1800` 使用独立的有限合成源，采样 capture worker RSS、增量查询时延/读量，并在停止后逐条核对序号、文件尾和可见水位。`--adb PATH --serial SERIAL` 改用真实设备。报告明确区分两种来源，默认每秒 200 条的合成结果不能替代授权、拔插、重启或设备损失验收。

## 完整任务体验

[场景体验协议](experience-testing.md)将真实 GUI/CLI 与独立 mock ADB 组合，让不同参与者完成在线排障、离线交接和 Agent 查询。它记录键鼠操作、截图、完整工件及首次观察之后的交叉复验，不以模型评分代替真人可用性研究。

[体验工具](../tools/experience-lab/README.md)提供本地依赖准备、私有 X11 桌面和场景控制；[验证摘要](validation.md)记录已执行范围与未覆盖环境。完整体验适合发布候选或交互变更后的验证；正常 CI 继续使用确定性回归和少量原生启动烟测。
