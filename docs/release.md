# GitHub 发布指南

## 独立组件版本

当前 GUI 为 **1.4.0**（根 Cargo.toml 的 `ciallogcat`），CLI 为 **1.4.0**（`crates/cli/Cargo.toml` 的 `ciallogcat-cli`），内部共享库为 **1.4.0**（`crates/core/Cargo.toml`）。三者组成一个 workspace，共用 Cargo.lock。Rust 库名仍为 `ciallogcat`，核心不单独发布二进制或上传 crates.io。

GUI 与 CLI 独立递增版本；发布 GUI 不修改 CLI 版本，也不重发 CLI 附件。共享核心变更需验证两端，但不强制同时发布。每个标签指向完整源码提交，能够重建该组件所用的核心代码；应用版本、核心包版本与存档/协议版本分别管理。

新版本使用完整的组件标签：`gui-v1.3.1`、`cli-v1.3.3`，预发布例如 `gui-v1.3.2-rc.1`。标签必须匹配目标 Cargo package 及锁文件中的版本。旧 `v1.3.1`、`v1.3.2` 已用于 CLI，保持原提交和资产；不移动、覆盖或补入本次 GUI。旧标签重跑使用旧代码，新 workspace 发布拒绝无组件前缀标签。

## 发布清单与说明

标签对应提交必须包含 `docs/releases/<TAG>.json` 与非空同名 `.md`。清单恰好三个字段，例如：

```json
{"tag":"gui-v1.3.1","version":"1.3.1","scope":"gui"}
```

新工作流的 `scope` 为 `gui` 或 `cli`，必须与标签前缀相同。发布策略保留旧单 package 的 `cli/full` 校验以验证历史契约，但不允许在新 workspace 中发布旧式全量标签。缺少清单、说明、版本不匹配或附件异常均停止，不隐式扩大范围。

## v1.4.0 更新（GUI 与 CLI）

两个组件同时发布，各用独立标签和六件附件：[CLI v1.4.0](releases/cli-v1.4.0.md) 面向 Agent 循环（标记、`--wait-ms`、`crashes`、`--seq`、精简响应 schema 3、官方 skill）并为 text 采集标注进程名；[GUI v1.4.0](releases/gui-v1.4.0.md) 通过共享核心使用同一生命周期进程名推断。CLI 响应 schema、token 和 `get` 默认视图有不兼容变化，见 CLI 说明。先推送 `cli-v1.4.0`，再推送 `gui-v1.4.0`，使 GUI 正式版成为 Latest。

## GUI v1.3.1 更新

详见 [GUI v1.3.1 发布说明](releases/gui-v1.3.1.md)。本版加入英文/简体中文、集中设置、跟随系统主题与快捷键帮助，整理窄窗口布局和文案。新安装默认英文，旧配置保留中文及深浅色选择。仅发布 GUI 六件附件；CLI 最新版本仍为 v1.3.2。

## 发布流程

1. 只更新目标组件的 Cargo 版本，通过 `cargo check --workspace` 同步锁文件，不升级无关依赖。新增对应的清单和发布说明。
2. 按 CONTRIBUTING 执行格式、workspace 测试、无 GUI 测试、Clippy 与 release 构建。共享核心改动必须验证两端。
3. 提交源码、资源和文档，推送 main（自动 Checks）或通过 PR 验证。Checks 只验证、不打包发布。
4. 创建并推送组件标签。例如本次：

```sh
git tag -a gui-v1.3.1 -m "Ciallogcat GUI v1.3.1"
git push origin gui-v1.3.1
```

Release 只由组件版本标签触发，不提供手动入口。GUI 发布要求 Linux/Windows GUI 与核心测试、Clippy、GUI release 构建和版本冒烟检查成功，CLI 发布 job 必须跳过。CLI 发布要求两平台 CLI 与核心检查、无 GUI release 构建及版本/帮助冒烟检查成功，GUI job 必须跳过。任一必需 job 失败、取消或意外跳过均不发布。日常 Checks 继续验证整个 workspace。

Linux 使用固定版本 cargo-deb 3.8.0 打包 GUI；CLI 由独立脚本生成 DEB。两种 DEB 均在干净 Ubuntu 24.04 容器验证安装、依赖、文档和卸载。GUI 安装验证不启动图形窗口；GUI `--version` 可无显示器运行，必须与清单版本相同。

发布脚本只下载当前运行、所选组件的 artifacts，要求恰好六件附件。每个校验文件只允许一行对应文件名及正确 SHA-256。先创建或恢复草稿，拒绝额外或重复远端附件；上传后重新下载核对集合与字节内容。公开前再次确认远端标签仍指向构建提交且 Release 仍为草稿。网络与认证失败不会当作 Release 不存在。

正式 GUI 发布设为仓库 Latest；CLI 与预发布不改变 Latest。仓库只有一个 Latest，CLI 用户通过 CLI Release 的直接链接下载。预发布状态根据纯版本中的预发布后缀判断，组件标签中的连字符不算预发布。公开后复核标题、正文、附件、Latest 与预发布状态。不覆盖公开附件、不移动公开标签；失败草稿可修复后重试，公开后核对失败则先查实际状态。

## 发布附件

标签前缀不重复写进附件名：`gui-v1.3.1` 的附件是 `ciallogcat-v1.3.1-…`；`cli-v1.3.3` 的附件是 `ciallogcat-cli-v1.3.3-…`。

每个组件提供 Linux x86_64 tar.gz、Windows x86_64 ZIP 和 Ubuntu 24.04 x86_64 DEB，另有三个同名 `.sha256`，共六件。

GUI 压缩包包含程序、文档、MIT 和字体 OFL 许可、BUILD-INFO.txt。GUI DEB 包名仍为 `ciallogcat`，安装到 `/usr/bin/ciallogcat`，保持旧 GUI 包的升级路径；DEB 版本使用 GUI package 版本。CLI DEB 包名为 `ciallogcat-cli`，版本使用 CLI package 版本，两者独立安装与卸载。CLI 包不含 GUI、字体或 `.rlib`。ADB 单独安装，现有 SDK 用户可自行提供。

Linux GUI 需要兼容 Ubuntu 24.04 的系统库和图形桌面，不保证兼容所有 Debian/Ubuntu。CI 不等同于原生桌面、IME、真机连接和长期 GPU/采集性能验收；验证范围见 [UI 说明](ui-review.md)。临时构建与诊断留在被忽略的 target，不提交设备日志、个人配置或凭据。

## 旧版发布历史

以下版本使用当时的共用 Cargo package 版本，范围说明适用于各自标签。

## v1.3.2 更新

完整说明见 [v1.3.2 发布说明](releases/v1.3.2.md)。本版增加独立于时间筛选的 received/device/both 文本展示、集中解释查询/存档/采集完整性，以及有界保存主机尾部记录的停止流程和独立确认反馈。产品代码仅涉及 CLI 模块，GUI 与共享核心功能没有改动；仅发布 CLI 的六件附件，Latest 保留既有全量版本。共用 Cargo package 的源码版本同步升至 1.3.2。

## v1.3.1 更新

完整说明见 [v1.3.1 发布说明](releases/v1.3.1.md)。本版修复 CLI 查询未收尾文件时误报缺少 artifact_id 的诊断，提供实际末行字节偏移和活动会话查询指引。仅发布 CLI 的 Linux/Windows 压缩包、Ubuntu DEB 及三个 SHA-256 文件；GUI 用户继续使用 v1.3.0 二进制。同一 Cargo package 的源码版本升至 1.3.1，自动源码归档包含已有 GUI 修改。

## v1.3.0 更新

完整说明见 [v1.3.0 发布说明](releases/v1.3.0.md)。本版包含 CLI、共享存档核心和 GUI 的不向后兼容整理：仅保留 query/get/context 与一种原生存档布局，移除旧命令、格式选项、旧 reader 和旧游标兼容。升级前保留旧工具以读取旧存档，不要将活动会话交给新版本。统一快照边界、结构记录展示和最终编码验证，并改进 GUI 文件失败恢复。

## v1.2.2 更新

完整说明见 [v1.2.2 发布说明](releases/v1.2.2.md)。查询汇总明确解析后的绝对时间窗口，包括零结果；text 进程名默认以 32 显示列中间省略，可用 --process-width 0 恢复全名；默认 received 不再逐行重复标注，异常与 device 来源仍明确显示。

## v1.2.1 更新

完整说明见 [v1.2.1 发布说明](releases/v1.2.1.md)。CLI text 默认改为人类可读时间并支持格式/时区选择，避免重复追加末尾换行；stale stop 仍请求合作停止，显式 --force 可管理性关闭并冻结查询前缀，不代表确认 worker 退出。

## v1.2.0 更新

完整说明见 [v1.2.0 发布说明](releases/v1.2.0.md)。本版增加 CLI 共享过滤条件、固定快照全量流式查询、接收时间范围和文本输出，默认采集上限提高到 1 GiB，并修复 Windows 预览预算测试。保留旧分页协议和旧标签；X27–X29 等已知问题未在本版修复。

## v1.1.0 更新

完整发布说明及已知问题见 [v1.1.0 发布说明](releases/v1.1.0.md)。本版包含采集控制、Agent 查询、原生 v2/v3 文档、protobuf 传输、设备诊断和桌面文件/阅读流程。X27–X29 仍为已知 P2，真机和原生平台验收范围见发布说明。

## v1.0 更新

- 将 ADB 采集、解析、筛选和日志存储抽为共享核心；`--no-default-features` 构建无需 GUI 依赖。
- 新增独立 `ciallogcat-cli`，支持设备查询、有界前台采集，以及本地后台会话的启动、状态、查询和停止。
- CLI 使用 JSON / JSONL 输出、独立会话目录和停止标记，不依赖持久化 PID 结束进程，也不清空设备日志。
- 保留默认桌面应用，新增 Linux / Windows CLI 测试和独立压缩包；共享核心 `.rlib` 不作为发布附件。


## 为既有 v1.0 追加 CLI DEB

独立的 `Supplement v1.0 CLI Debian package` 工作流只能从 main 手动触发，
使用固定的原始 v1.0 提交 `fdb6956886e9ac144b43b4b6e80bb807b535fad7`
构建 CLI，检查现有标签与当前 Cargo/源码一致，并使用新的打包脚本和安装指南。
在干净 Ubuntu 24.04 容器验证与已发布 GUI DEB 的共存及独立卸载后，
仅追加缺少的 CLI DEB 和 `.sha256`。已有同名资产必须与本次文件逐字节相同；
否则失败并等待人工检查。不会移动 v1.0 标签、覆盖其他资产或重新发布 release。
