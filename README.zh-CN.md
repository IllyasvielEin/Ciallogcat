# Ciallogcat

使用 Rust 构建的 Android Logcat 查看器与无界面采集工具。

[English](README.md) | 简体中文

用桌面应用浏览实时日志或离线文件，或通过 CLI 在脚本和本地 Agent 中采集、查询日志。两者共用 Rust 核心，实时采集需要外部 ADB。桌面应用优先面向 Linux；发布打包支持 Linux 与 Windows x86_64。

- 实时显示时间、级别、PID/TID、进程名、Tag 和完整消息
- 桌面端按包名、最低级别与关键词筛选，支持正则表达式和区分大小写
- 保存筛选组合、复用关键词历史、选择多个 Logcat 缓冲区和手动选择设备
- 多行复制、键盘导航，以及浏览时暂停的自动跟随
- 深色/浅色主题、内置中英文字体和可调日志内存预算
- 独立的无界面 CLI，支持 JSONL 采集与有截止条件的本地后台会话
- 打开 CLI v1 / 原生 v2 文件，另存保留日志或导出筛选快照，保留来源与稳定记录引用

## 目录

- [背景](#背景)
- [安装](#安装)
- [桌面端使用](#桌面端使用)
- [无界面 CLI](#无界面-cli)
- [配置](#配置)
- [文档](#文档)
- [平台状态](#平台状态)
- [维护者](#维护者)
- [参与贡献](#参与贡献)
- [许可证](#许可证)

## 背景

Ciallogcat 专注于 Android 应用调试。桌面应用 `ciallogcat` 通过 ADB 读取实时 Logcat，并在本地筛选。独立的 `ciallogcat-cli` 为脚本和 Agent 输出规范化 JSONL，无需打开窗口。

桌面应用可通过 **File** 菜单或拖放打开 JSONL 文件，无需 ADB。文件视图只读；另存和筛选导出使用原生 v2 格式。当前不支持多设备并排显示，界面混用中文和英文标签。文件功能见[使用说明](docs/file-workflow.md)。

## 安装

### ADB 依赖

安装 [Android SDK Platform-Tools](https://developer.android.com/tools/releases/platform-tools)，将 `adb` 加入 `PATH`，或使用已有的 Android SDK。**程序不内置 ADB。** 开启 USB 调试、连接 Android 设备并接受授权提示，然后检查连接：

```sh
adb devices
```

### 发布包

在 [GitHub Releases](https://github.com/IllyasvielEin/Ciallogcat/releases) 查看已发布的下载。当前源码定义了以下发布包；较早版本可能不含 CLI。下面的 `<TAG>` 代表实际版本标签，例如 `v1.0`，不是文件名中的字面内容。

- **桌面 GUI：** `ciallogcat-<TAG>-windows-x86_64.zip`、`ciallogcat-<TAG>-linux-x86_64.tar.gz` 或 `ciallogcat-<TAG>-linux-x86_64.deb`
- **无界面 CLI：** `ciallogcat-cli-<TAG>-windows-x86_64.zip` 或 `ciallogcat-cli-<TAG>-linux-x86_64.tar.gz`；Ubuntu 24.04 DEB：`ciallogcat-cli-<TAG>-linux-x86_64.deb`

每个压缩包或安装包都有 `.sha256` 校验文件。GUI 与 CLI 分开下载；CLI DEB 与 GUI DEB 独立，可以同时安装；没有独立的核心库下载包。

解压免安装包，运行 `ciallogcat` / `ciallogcat.exe` 启动桌面应用，或运行 `ciallogcat-cli` / `ciallogcat-cli.exe` 使用 CLI。在解压目录中，Linux 用 `./ciallogcat` 启动 GUI，Windows PowerShell 用 `.\ciallogcat.exe`。查看 CLI 帮助则分别使用 `./ciallogcat-cli --help` 或 `.\ciallogcat-cli.exe --help`。将该目录加入 `PATH` 后，即可使用下文不带路径的命令。

Ubuntu 24.04 用户可用 `sudo apt install ./<下载的文件名>.deb` 安装 GUI DEB，再从应用菜单启动 **Ciallogcat** 或运行 `ciallogcat`。ADB 是推荐依赖；已有 Android SDK 的用户可加上 `--no-install-recommends`。卸载使用 `sudo apt remove ciallogcat`，用户设置会保留。

CLI DEB 使用 `sudo apt install ./ciallogcat-cli-<TAG>-linux-x86_64.deb` 安装，运行 `ciallogcat-cli --help`，使用 `sudo apt remove ciallogcat-cli` 卸载。ADB 为推荐依赖，也可由 Android SDK Platform-Tools 提供。

Linux 发布包在 Ubuntu 24.04 x86_64 上构建，不保证兼容较旧发行版。**GUI** 需要 X11 或 Wayland 桌面、兼容的图形驱动和运行库；**CLI** 无需显示服务器、GPU 或 GUI 库。

### 从源码构建

安装当前 [Rust stable](https://rustup.rs/)（含 Cargo）和 Git。Windows 构建使用 MSVC 工具链、Visual Studio Build Tools 的“使用 C++ 的桌面开发”组件与 Windows SDK。Linux CLI 构建需要 C 链接器；GUI 还需要窗口系统开发库。Ubuntu / Debian 可安装以下 GUI 构建依赖：

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libudev-dev libx11-dev libxi-dev libxrandr-dev libxcursor-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libegl1-mesa-dev
```

克隆仓库，然后选择需要的前端：

```sh
git clone https://github.com/IllyasvielEin/Ciallogcat.git
cd Ciallogcat

# 运行桌面应用
cargo run --locked --release --bin ciallogcat

# 仅构建 CLI，不引入 GUI 依赖
cargo build --locked --release --no-default-features --bin ciallogcat-cli
./target/release/ciallogcat-cli --help
```

只构建 GUI 而不启动可使用 `cargo build --locked --release --bin ciallogcat`。可执行文件位于 `target/release/`，Windows 下带 `.exe` 后缀。Rust 项目也可以关闭默认 feature，使用[共享核心](docs/headless-core.md)。

## 桌面端使用

1. 启动 `ciallogcat`，连接并授权设备，在 **Device** 中手动选择；需要时点击“**刷新**”。选择可用设备后开始采集。
2. 选择或输入包名，设置最低级别并输入关键词。`.*` 切换正则表达式，**Aa** 切换大小写匹配。**Saved filters** 保存组合，关键词历史保留最近搜索。
3. 点击日志行查看完整消息。**Shift+点击** 连选，**Ctrl+点击** 增减选中行，**Ctrl+C** 复制。**Ctrl+F** 或 **/** 聚焦搜索。
4. 浏览或选择日志会暂停自动跟随，但不会停止采集。按 **Ctrl+End** 或点击“**回到最新**”恢复跟随。
5. 在 **View** 调整主题、行高、详情和日志内存预算。预算草稿仅在点击 **Apply budget** 后生效。
6. **File → 打开日志**（Ctrl+O）读取已有 JSONL；**另存保留日志**（Ctrl+Shift+S）或**导出当前筛选结果**保存固定快照。当前使用路径输入与拖放，保存目标须为新文件。

一些实用细节：

- “**暂停**”停止采集并保留日志；“**开启**”恢复采集，不回补暂停期间的内容。“**关闭**”还会取消设备选择。同一设备重连会保留日志；切换设备前会确认清空。
- 设备发现失败时保留上次成功列表与当前选择。快照中的离线或设备缺失不会单独停止仍在运行的日志流，“暂停”仍可使用。
- 实际日志流退出后按 1、2、4 秒最多重连三次；暂停或关闭会取消重连。重新采集须等待旧进程与输出排空确认，超过 5 秒未确认则报错并阻止新启动。
- 缓冲区默认 `main + system + crash`，修改后点击“**应用**”生效。启动与重启使用 `-T 1`，不会回补历史，期间可能有缺口。
- 包名筛选包含 `com.example.app:sync` 等子进程。GUI 进程名来自近期 `ps` 推定，并非已核实的身份；查询失败或过期后不再使用，也不会用后来的快照回填历史未知行。未识别或短命进程可能被该筛选隐藏，清除包名限制即可检查它们的日志。Logcat 结构分隔行仍受级别与消息条件筛选，但不受包名条件限制，也不计入未知进程数量。
- 正则无效时显示错误，修正后恢复结果。**Clear** 确认后仅清除本地记录，不清除设备上的 Logcat 缓冲区。

### 内存保留

- 日志保留预算默认 **300 MiB**，可调为 **16–4096 MiB**，超限时删除最旧记录。这不是整个进程的内存上限。
- 单条输入行最多 **1 MiB**，截断会标注。完整日志复制有 **16 MiB UTF-8** 上限；选中内容过大时会提示错误，不改动剪贴板。

更多键盘导航、选择行为、内存计算方式和图形诊断说明见 [UI 指南](docs/ui-review.md)。

## 无界面 CLI

`ciallogcat-cli` 适用于终端、脚本和本地 Agent。安装后或将 `target/release` 加入 `PATH` 后，先查找设备，再将下文的 `DEVICE_SERIAL` 替换为实际序列号：

```sh
ciallogcat-cli devices

# 在前台采集到新文件
ciallogcat-cli capture --serial DEVICE_SERIAL --output capture.jsonl --duration-secs 30

# 使用新目录启动有截止条件的本地后台会话
ciallogcat-cli start --serial DEVICE_SERIAL --session-dir ./capture-session --duration-secs 300
ciallogcat-cli status --session-dir ./capture-session
ciallogcat-cli query --session-dir ./capture-session --after-seq 0 --limit 100 --query "timeout"
ciallogcat-cli stop --session-dir ./capture-session
```

- 控制命令返回 JSON，日志记录采用**规范化 JSONL**，并非原始 Logcat 字节。`capture --output -` 将日志写入 stdout，采集摘要写入 stderr。
- 时长与输出大小限制始终同时生效，默认分别为 **600 秒**与 **64 MiB**。使用 `--duration-secs` 和 `--max-bytes` 调整。达到限制、ADB 退出、收到停止请求或发生错误时会结束，不会无限驻留。
- `start` 返回 `adb_process_started` 仅表示 worker 与 ADB 进程已启动，**不代表设备已就绪或一定会产生日志**。请检查 `status` 并查询输出。
- `query --query` 对消息进行区分大小写的字面子串匹配。翻页时将返回的 `next_after_seq` 传给 `--after-seq`。CLI 尚未提供全部桌面筛选功能。
- 输出文件与会话目录必须是新路径。设备日志可能含敏感信息，请存放在私人目录，并在分享前检查内容。

选项、分页限制、退出码、停止行为和打包说明见 [CLI 指南](docs/cli.md)。快速参考可运行 `ciallogcat-cli --help`。

## 配置

桌面设置、筛选组合与关键词历史保存在本地：

- Linux：`$XDG_CONFIG_HOME/ciallogcat/config.json`，未设置时为 `~/.config/ciallogcat/config.json`
- Windows：`%APPDATA%\Ciallogcat\config.json`

使用 `CIALLOGCAT_CONFIG_DIR` 更改桌面配置目录。两个前端均支持用 `CIALLOGCAT_ADB` 指定 ADB 可执行文件，CLI 还支持 `--adb PATH`。否则会从 SDK 环境变量、`PATH` 和常见 SDK 目录查找 ADB。CLI 会话使用 `--session-dir` 指定的目录。

Agent 新集成可使用 `capture/start --record-format v2` 和 `query-v2/get-v2/context-v2`，沿用 GUI 可打开的记录身份。实验性 `--transport proto` 显式选择；详见 [CLI](docs/cli.md)。

## 文档

详细指南使用英文或中文：

- [文档索引与迁移](docs/README.md)
- [桌面交互与诊断](docs/ui-review.md)
- [CLI 命令与会话](docs/cli.md)
- [共享 Rust 核心](docs/headless-core.md)
- [Agent v2 查询、全文与上下文](docs/agent-query.md)
- [开发方向与实施进度](docs/development-plan.md)
- [后续任务与 Agent 交接](TODO.md)
- [验证状态与边界](docs/validation.md)
- [Text/protobuf/compact 性能对比](docs/transport-performance-2026-10-24.md)
- [只读设备诊断](docs/device-diagnostics.md)
- [外部测试前后有界证据流程](docs/agent-test-workflow.md)
- [Logcat protobuf 调研与兼容方案](docs/logcat-protobuf.md)
- [Logcat 选项、软件实践与功能取舍](docs/logcat-options-and-ecosystem.md)
- [ADB 子进程测试与 mock](docs/adb-testing.md)
- [发布工作流与打包](docs/release.md)
- [贡献指南](CONTRIBUTING.md)
- [内置字体来源与许可](assets/fonts/README.md)

## 平台状态

发布任务面向 **Linux 和 Windows x86_64**。Linux fake-ADB 测试覆盖采集与会话生命周期；共用原生 Rust fixture 已接入 Windows CI。本地证据包含 Linux 实际运行和 Windows 交叉编译，托管 Windows 执行及真机行为仍需确认。这些测试不代表真机可靠性或长期稳定性已通过验证。

桌面端已在 Linux X11 与嵌套 Wayland 中使用模拟日志检查普通交互。已知限制包括测试中的 X11/XIM 中文候选框定位偏差、尚未完成的 Wayland 输入法验证，以及被合成器崩溃阻塞的嵌套 Weston 窗口缩放检查。Windows 整窗拖动可能卡顿。详见[验证说明](docs/ui-review.md#验证范围)。

## 维护者

[@IllyasvielEin](https://github.com/IllyasvielEin)

## 参与贡献

欢迎[提交 Issue](https://github.com/IllyasvielEin/Ciallogcat/issues) 或 Pull Request。问题报告要求与目录结构见 [CONTRIBUTING.md](CONTRIBUTING.md)。提交前运行：

```sh
cargo fmt --all --check
cargo test --locked
cargo test --locked --no-default-features
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --no-default-features --all-targets -- -D warnings
cargo build --locked --release
cargo build --locked --release --no-default-features --bin ciallogcat-cli
```

Mock ADB 测试在 Linux 上需要 Python 3，无需 Android 设备。详见[测试指南](docs/adb-testing.md)与 [CLI 验证指南](docs/cli.md#validation-and-packaging)。**Checks** 工作流在 Pull Request、推送 `main` 和手动触发时运行；Windows 采集与会话生命周期已配置原生 Rust fake-ADB fixture；本机交叉编译不等于 Windows 运行验证。原发布工作流仍仅在推送版本标签时运行。发布前请阅读[发布指南](docs/release.md)。

## 许可证

[MIT](LICENSE) © 2026 Ciallogcat contributors。

内置字体独立采用 [SIL Open Font License 1.1](assets/fonts/LICENSE-OFL.txt)。
