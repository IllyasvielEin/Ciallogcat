# Ciallogcat

使用 Rust 和 egui 构建的桌面 Android Logcat 查看器。

[English](README.md) | 简体中文

Ciallogcat 是一个独立的 Android 应用日志调试工具，优先面向 Linux 桌面，同时提供 Windows 构建。Cargo 包名和可执行文件名使用小写的 `ciallogcat`。

- 实时显示日志时间、级别、PID/TID、进程名、Tag 和消息
- 按包名、最低级别和关键词组合筛选，支持正则表达式与区分大小写
- 保存筛选组合、复用关键词历史、选择多个 Logcat 缓冲区和手动选择设备
- 多行选择与复制、完整消息详情，以及独立于采集的自动跟随
- 深色/浅色主题、可调行高和内置中英文字体
- 虚拟日志列表和可调内存预算

## 目录

- [背景](#背景)
- [安装](#安装)
- [使用](#使用)
- [配置](#配置)
- [文档](#文档)
- [平台状态](#平台状态)
- [维护者](#维护者)
- [参与贡献](#参与贡献)
- [许可证](#许可证)

## 背景

Ciallogcat 专注于个人 Android 应用开发中的实时 Logcat 调试，通过 ADB 子进程采集日志并在本地筛选。目前不支持离线日志文件、多设备并排、日志导出或其他 ADB 工具功能。

界面目前混用中文和英文标签，下面的操作说明沿用实际界面文字。

## 安装

### 依赖

安装 [Android SDK Platform-Tools](https://developer.android.com/tools/releases/platform-tools)，将 `adb` 加入 `PATH`，或使用已有的 Android SDK。程序不内置 ADB。开启设备的 USB 调试并接受授权提示，然后检查：

```sh
adb devices
```

Linux 需要可用的 X11 或 Wayland 图形桌面、兼容的图形驱动和系统库。Linux 发布包面向 Ubuntu 24.04 x86_64，不保证兼容旧版 Ubuntu 或其他 Debian 系发行版。验证边界见[平台状态](#平台状态)。

### 发布包

在 [GitHub Releases](https://github.com/IllyasvielEin/Ciallogcat/releases) 中选择已发布版本。发布工作流会生成 Windows x86_64 ZIP、Linux x86_64 tar.gz 和 Ubuntu 24.04 x86_64 DEB，每个附件都有对应的 `.sha256` 校验文件。以下示例使用 v0.4 文件名，请以实际下载版本为准。

在 Ubuntu 24.04 上，进入下载目录安装 DEB：

```sh
sudo apt install ./ciallogcat-v0.4-linux-x86_64.deb
ciallogcat
```

也可从应用菜单启动。ADB 是推荐依赖；已有 Android SDK Platform-Tools 的用户可使用 `--no-install-recommends` 安装。卸载使用 `sudo apt remove ciallogcat`，用户配置会保留。

Linux 免安装压缩包：

```sh
tar -xzf ciallogcat-v0.4-linux-x86_64.tar.gz
cd ciallogcat-v0.4-linux-x86_64
./ciallogcat
```

运行库和 ADB 需另行安装。Windows 解压 ZIP 后，运行解压目录中的 `ciallogcat.exe`。

### 从源码构建

安装当前 [Rust stable](https://rustup.rs/)（含 Cargo）和 Git。Windows 使用 MSVC 工具链，需要 Visual Studio Build Tools 的“使用 C++ 的桌面开发”组件与 Windows SDK。

Ubuntu / Debian 安装构建依赖：

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libudev-dev libx11-dev libxi-dev libxrandr-dev libxcursor-dev libxkbcommon-dev libwayland-dev libegl1-mesa-dev
```

其他发行版请安装对应软件包。克隆并运行：

```sh
git clone https://github.com/IllyasvielEin/Ciallogcat.git
cd Ciallogcat
cargo run --locked --release
```

只构建而不启动可使用 `cargo build --locked --release`。Linux 可执行文件为 `target/release/ciallogcat`，Windows 为 `target/release/ciallogcat.exe`。

## 使用

从应用菜单或终端启动已安装的程序：

```sh
ciallogcat
```

1. 连接并授权 Android 设备，在 **Device** 中手动选择；需要时点击“**刷新**”。选择可用设备后开始采集。
2. 聚焦包名输入框选择已安装的第三方应用，也可直接填写包名。设置最低日志级别并输入关键词；`.*` 切换正则表达式，**Aa** 切换大小写匹配。
3. 点击日志行查看完整消息。**Ctrl+C** 复制，**Shift+点击** 连选，**Ctrl+点击** 增减选中行。
4. 使用 **Saved filters** 保存或恢复筛选组合。聚焦关键词输入框可查看历史，最多保留 500 条；历史不包含其他筛选设置。
5. 使用 **View** 调整主题、行高、详情显示和日志内存预算。预算先作为草稿编辑，点击 **Apply budget** 后生效；**Cancel budget**、Esc 或关闭菜单会丢弃草稿。

### 采集与筛选

- “**暂停**”停止采集，保留设备选择与已有日志；“**开启**”从最近日志继续，不回补暂停期间的日志。“**关闭**”停止采集并取消设备选择，已有日志仍可阅读。
- 再次选择同一暂停设备不会恢复采集；同一设备重连后保留已有日志。切换到其他设备前会确认清空。
- 缓冲区默认 `main + system + crash`，也可选择 `events`、`radio` 或与其他项互斥的 `all`。点击“**应用**”后生效：采集中会重启，暂停时保持暂停。已有日志保留，因此可能混合切换前后的缓冲区内容。启动与重启使用 `-T 1`，不回补历史；切换期间可能有短暂缺口。
- 包名筛选包含 `com.example.app:sync` 等子进程，只使用已知进程名，不根据消息内容推测归属。短命进程可能来不及识别；未归属日志仍保留，但会被包名筛选隐藏。清除包名限制可查看它们，关键词和级别条件仍然生效。
- 正则无效时显示错误并清空当前结果，修正后重新筛选。**Clear** 会先确认，只清空本地日志，不会清除设备的 Logcat 缓冲区。

### 键盘与自动跟随

- **Ctrl+F** 或 **/**：聚焦关键词输入框
- **↑ / ↓**、**Home / End**：未处于输入或弹窗操作时选择日志
- **Ctrl+A**：全选筛选结果；**Shift+导航键**：扩展选择
- **Ctrl+C**：优先复制选中文字，否则按显示顺序复制所选完整日志
- **Esc**：收起输入或弹窗，或取消日志选择，不清除筛选
- **Ctrl+End** 或“**回到最新**”：跳到最新日志并恢复跟随

浏览或选择日志会暂停自动跟随，但不会停止采集。输入框保留原生编辑快捷键。更改筛选后只移除已确认不匹配的选择；尚未检查的行保留选择与详情，并显示检查中。整组选中行检查完成前不能复制，不会偷偷复制其中一部分，也不会稍后自动写入剪贴板，需要再次主动复制。内存裁剪只移除已过期的选择，保留仍存留的行和阅读位置；清空日志会取消选择。

### 内存保留

日志预算默认 **300 MiB**，可在 **View** 中调整为 **16–4096 MiB**。超限时删除最旧记录，直到估算值降至预算的 90%。降低预算的草稿不会立即裁剪；Apply 会依据当时保留量执行一次不可撤销的裁剪，菜单在需要时显示警告。预算覆盖保留的日志数据，**不是整个进程的 RSS 硬上限**；图形、字体、临时处理和共享的后台快照另有开销。

单条输入行最多接收 1 MiB，超出内容会被截断并标注。超长消息的预览会缩短，但筛选和复制使用已接收的完整文本。队列、快照与绘制细节见[日志存储与绘制](docs/ui-review.md#日志存储与绘制)。

四个整条日志复制入口共用 **16 MiB UTF-8 输出上限**，与保留预算无关。复制前准确计算格式化字节数，再一次性申请容量；超限、分配失败或选择仍待检查时提示原因，不截断输出，也不覆盖剪贴板。超限后请选择更少的行。这是保守的产品上限，不是操作系统剪贴板或 RSS 安全保证；原生剪贴板的额外副本与选中文字复制另有开销。

后台筛选会协作取消过期扫描；快照结果完成后立即显示，后续新增记录在 UI 中分批检查并标明未检查数量。一次正则匹配、分配或释放无法抢占，因此时间切片不是硬实时保证。

## 配置

设置、筛选组合与关键词历史保存在本地：

- Linux：`$XDG_CONFIG_HOME/ciallogcat/config.json`，未设置时为 `~/.config/ciallogcat/config.json`
- Windows：`%APPDATA%\Ciallogcat\config.json`

使用 `CIALLOGCAT_CONFIG_DIR` 指定其他配置目录，或用 `CIALLOGCAT_ADB` 指定 ADB 可执行文件。否则程序会从 SDK 环境变量、`PATH` 和常见 SDK 目录查找 ADB。图形诊断变量见 [UI 指南](docs/ui-review.md#图形诊断)。

## 文档

详细指南目前使用中文：

- [UI 行为、性能与图形诊断](docs/ui-review.md)
- [ADB 子进程测试与 mock](docs/adb-testing.md)
- [发布工作流与打包](docs/release.md)
- [贡献指南](CONTRIBUTING.md)
- [内置字体来源与许可](assets/fonts/README.md)

## 平台状态

Linux X11 与嵌套 Wayland 桌面已使用模拟日志进行普通交互检查。这些检查不代表真机采集、长期稳定性或所有桌面环境均已通过验证。

已知限制包括：测试中的 X11/XIM 路径存在中文候选框定位偏差；Wayland 输入法尚未完成验证；嵌套 Weston 的窗口缩放检查被合成器崩溃阻塞。Windows 整窗拖动可能卡顿。范围与细节见[验证说明](docs/ui-review.md#验证范围)。

## 维护者

[@IllyasvielEin](https://github.com/IllyasvielEin)

## 参与贡献

欢迎[提交 Issue](https://github.com/IllyasvielEin/Ciallogcat/issues) 或 Pull Request。问题报告要求与目录结构见 [CONTRIBUTING.md](CONTRIBUTING.md)。提交前运行：

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

ADB 子进程测试使用 Python 3 mock，目前在 Linux 上运行，无需 Android 设备。命令与隔离边界见[测试指南](docs/adb-testing.md)。合成[性能测试](docs/ui-review.md#性能测量)测量 CPU 侧开销，不代表 GPU 帧率或真机吞吐。

发布工作流只在推送版本标签时检查和打包，分支推送与 Pull Request 不触发 CI。发布前请阅读[发布指南](docs/release.md)。

## 许可证

[MIT](LICENSE) © 2026 Ciallogcat contributors。

内置字体独立采用 [SIL Open Font License 1.1](assets/fonts/LICENSE-OFL.txt)。
