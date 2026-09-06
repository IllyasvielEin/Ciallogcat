# Ciallogcat

## 人话

一个纯 Vibe 的 LogCat GUI，主要用于个人开发中安卓设备 APP 开发日志调试查看。

## 简介

Ciallogcat 是一个独立的桌面 Logcat 查看器，用于调试个人开发的 Android 应用。它使用 Rust 和 egui，优先支持 Linux，同时保留 Windows 兼容性。

## 功能

- 自动查找 ADB，并列出已连接设备；设备只能由用户手动选择。
- 列出设备上的第三方应用，也允许直接输入包名。
- 实时读取 Logcat，显示时间、级别、PID/TID、进程、Tag 和消息。
- 开启/暂停采集，关闭当前设备会话；支持选择 main、system、crash、events、radio 或 all 缓冲区。
- 按包名、最低日志级别和关键词组合筛选；关键词支持普通文字、正则表达式和区分大小写。
- 包名筛选会包含同一应用的子进程，例如 `com.example.app:sync`。
- 保存、应用和删除常用筛选组合。
- 选择日志后查看完整消息并复制；也可右键复制或按进程筛选。
- 深色/浅色主题、行高调整、虚拟列表和自动跟随最新日志。
- 内置 Cascadia Next SC NF，Linux 和 Windows 使用一致的中英文字体。
- 自动保存设置。日志按内存预算滚动保留，默认 300 MiB，可在 View 中调整。

## 构建与运行

安装当前 [Rust stable](https://rustup.rs/)（包含 Cargo）以及 Android SDK Platform-Tools，并将 `adb` 加入 `PATH`。Windows 使用 MSVC 工具链，需要 Visual Studio Build Tools 的“使用 C++ 的桌面开发”组件与 Windows SDK。

Ubuntu / Debian 构建依赖可安装为：

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libudev-dev libx11-dev libxi-dev libxrandr-dev libxcursor-dev libxkbcommon-dev libwayland-dev libegl1-mesa-dev
```

Linux 运行需要可用的 X11 或 Wayland 桌面及图形驱动。其他发行版请安装对应软件包。

先确认 ADB 可用，且设备已授权：

```bash
adb devices
```

然后运行发布构建：

```bash
cargo run --locked --release
```

应用不会要求填写 ADB 路径。没有可用设备时，设备列表显示“无设备”；连接或授权设备后点击“刷新”。

## 基本使用

1. 连接并授权 Android 设备。
2. 从设备列表手动选择设备；聚焦包名输入框会列出已安装应用，输入文字会实时筛选，也可直接填写包名。
3. 输入关键词。右侧 `.*` 切换正则表达式，`Aa` 切换大小写匹配；级别栏设置最低日志级别。`View` 菜单调整日志内存预算、主题、行高和详情显示。
4. 点击日志行查看完整内容；按 `Ctrl+C` 复制所选日志。
5. 从 `Saved filters` 中保存或恢复常用筛选组合。

采集栏提供两个按钮：运行时“开启”变为“暂停”，暂停会结束当前 adb logcat 进程，但保留设备选择与已有日志；再次开启从最新日志继续，不回补暂停期间的日志。“关闭”结束采集并取消设备选择，已有日志仍可查看、筛选和复制。选择设备后自动开始采集。

缓冲区菜单支持多选，默认 `main + system + crash`。`all` 与其他选项互斥；勾选后点击“应用”才生效。如果正在启动或采集，会重启 logcat 使用新的 `-b` 参数；暂停时修改不会自动恢复采集，未选设备时用于下一次启动。已有日志保留，因此可能混合切换前后的缓冲区内容。启动与重启使用 `-T 1`，不回补历史，切换期间可能有短暂缺口；可用缓冲区和权限取决于设备。

## 内存保留

日志超过预算时删除最旧内容，裁剪到预算的 90%。预算默认 300 MiB，可调范围 16–4096 MiB；界面显示当前估算值。估算包括 UTF-8 文本、日志块容量、字符串分配及每条日志的索引/辅助数据预留。筛选结果只保存索引，不复制一份完整日志。

该预算用于保留的日志数据，**不是整个进程的 RSS 硬上限**。图形后端、字体、设备列表、临时解析和后台筛选快照另有开销；快照共享日志块，筛选队列只保留最新的一个待处理请求。裁剪时正在使用的旧快照可能暂时保留已淘汰的数据，筛选完成后释放。接收日志队列另有 16 MiB 字节预算，队列满时让读取线程等待。

为避免异常输出无限占用内存，采集端单行最多接收 1 MiB 原始字节；超过部分丢弃并在该日志末尾标注 `truncated`，下一行继续读取。

按 `Ctrl+F` 或 `/` 聚焦关键词输入框；退出输入后用方向键及 `Home` / `End` 选择日志，`Ctrl+End` 跳到最新日志。`Esc` 退出输入或取消日志选择，不清除筛选。`Ctrl+C` 优先复制选中的文字，没有文字选择时复制所选完整日志。

详情区只在选中日志时展开。超长消息在表格中只显示前缀，在详情中分段滚动查看；复制与筛选使用已接收的完整日志。`Clear` 只清空本地视图，不会清除设备中的日志缓冲区。

## 配置文件

- Linux：`$XDG_CONFIG_HOME/ciallogcat/config.json`，未设置该变量时为 `~/.config/ciallogcat/config.json`
- Windows：`%APPDATA%\Ciallogcat\config.json`

也可通过 `CIALLOGCAT_CONFIG_DIR` 指定配置目录。ADB 会从系统 `PATH`、Android SDK 环境变量和常见 SDK 目录中自动查找。

环境变量使用 `CIALLOGCAT_` 前缀：`CIALLOGCAT_ADB` 指定 ADB 路径，`CIALLOGCAT_CONFIG_DIR` 指定配置目录；图形诊断变量见 [UI 设计与性能](docs/ui-review.md)。

## 支持范围

应用用于连接设备后的实时日志调试，不支持离线日志文件、多设备并排、导出或其他 ADB 工具功能。

Windows 整窗拖动可能出现卡顿，默认使用同步呈现。Linux 桌面交互、真机采集与持续运行的验证尚未完成。CI 编译和合成性能测试不代表这些场景已通过。

`prototypes/layout-lab.html` 为布局原型，不参与应用运行。

## 开发检查

```bash
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

UI 性能测量：`cargo test --release performance::measure_ui -- --ignored --nocapture`。测试使用合成日志，测量 CPU 侧布局、文字处理、三角化和采集事件处理，不代表 GPU 帧率或真机吞吐。方法与验证范围见 [UI 设计与性能](docs/ui-review.md)。

开发流程见 [贡献指南](CONTRIBUTING.md)，首次上传与版本打包见 [发布说明](docs/release.md)。推送版本标签后，GitHub Actions 在 Linux 和 Windows 上执行格式、测试、Clippy 和 release 构建，全部通过后自动发布带双平台压缩包与 SHA-256 校验文件的 GitHub Release。分支推送和 PR 不触发 CI。

## 许可证

源码采用 [MIT License](LICENSE)。内置字体独立采用 [SIL Open Font License 1.1](assets/fonts/LICENSE-OFL.txt)，来源与校验值见 [字体说明](assets/fonts/README.md)。
