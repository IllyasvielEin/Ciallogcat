# GitHub 发布指南

## 版本与触发

当前源码目标版本为 v1.3.1，Cargo 版本为 1.3.1；本版仅发布 CLI 二进制附件。最新公开版本需以 GitHub Releases 为准；CLI-only 版本不更新仓库级 Latest 标记，CLI 用户应访问对应标签的 Release 页面。完整标签必须等于 Cargo 版本加 v 前缀；补丁号为零的正式版本也接受短标签，例如 v1.0 对应 1.0.0。预发布标签使用完整版本，例如 v1.0.0-rc.1。

Release 工作流仅在推送版本标签时触发构建、打包和发布，不提供手动触发入口。

每次发布必须在标签对应提交中提供 `docs/releases/<TAG>.json` 和非空的同名 `.md` 发布说明，例如：

```json
{"tag":"v1.3.1","version":"1.3.1","scope":"cli"}
```

`scope` 仅允许 `cli`（仅 CLI 附件）或 `full`（GUI 和 CLI 附件）。配置必须恰好包含这三个字段，版本必须匹配 Cargo.toml 和 Cargo.lock；缺失或错误时停止，不默认发布全量。使用经过审核的说明正文，不自动生成混合组件说明。源码标签覆盖完整仓库，发布范围只控制二进制附件。

独立的 [Checks 工作流](../.github/workflows/checks.yml) 在 Pull request、推送 main 和手动触发时执行开发期验证，不打包或发布。其他分支推送本身不触发检查，可通过 PR 或手动运行验证。重跑旧标签任务仍使用该标签的旧代码。公开标签不移动或覆盖，修复通过新版本发布。

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

## 发布流程

1. 更新 Cargo.toml 版本并通过 cargo check 更新 Cargo.lock，不升级无关依赖。添加与标签一致的发布 JSON 清单和 Markdown 说明。
2. 执行贡献指南中的格式、测试、Clippy 和 release 构建检查，检查待提交差异。
3. 提交源码、资源和文档并推送待发布分支，通过 PR 或手动运行 Checks；合并到 main 后也会自动检查。
4. 创建并推送对应版本标签，例如：

```bash
git tag -a v1.3.1 -m "Ciallogcat CLI v1.3.1"
git push origin v1.3.1
```

CI 首先校验标签、版本、发布范围和说明，然后在 Windows 和 Ubuntu 24.04 执行所选组件的格式检查、测试、Clippy、release 构建和打包。所有 job checkout 同一触发提交。`full` 模式构建 GUI，Linux 使用固定版本 cargo-deb 3.8.0 额外生成 GUI DEB，在干净 Ubuntu 24.04 容器中验证安装、桌面入口、图标、许可证、动态链接依赖和卸载。

CLI 使用独立的 Linux x86_64 / Windows x86_64 矩阵，执行 `cargo test --locked --no-default-features`、无 GUI 的 Clippy、`cargo build --locked --release --no-default-features --bin ciallogcat-cli`，以及 `--help` / `--version` 冒烟检查与打包测试；版本输出必须等于清单版本。无需安装 GUI 开发依赖。CLI-only 要求两平台 CLI 成功且 GUI job 按范围跳过；`full` 要求 GUI 与 CLI 全部成功。任一必需 job 失败、取消或被意外跳过都不发布。开发期 Checks 中的 GUI 验证仍保留。

发布作业只下载当前运行所选的命名 artifacts。CLI-only 本地目录必须恰为六件附件，`full` 恰为十二件；每份校验文件只允许一行对应文件名与正确 SHA-256。创建草稿或恢复已有草稿后，拒绝意外或重复的远端附件，按明确文件列表上传，再下载远端附件核对完整集合与本次构建内容。公开前再次确认标签仍指向构建提交且 Release 仍为草稿。网络或认证错误不会当作 Release 不存在。

验证成功后公开 Release；预发布版本标为 Pre-release。正式 `full` 发布设为 Latest，CLI-only 和预发布不改变原 Latest。公开后核对标题、正文、预发布状态、附件清单和 Latest 指向。上传或公开前验证失败时保留草稿以便修复重试；若公开后的核对失败，Release 可能已公开，应先检查实际状态，不覆盖已公开附件或移动标签。本流程在脚本层拒绝覆盖公开版本，不代表仓库已启用 GitHub 平台级不可变保护。

## 发布附件

| 平台 | 附件 |
| --- | --- |
| Windows x86_64 | ciallogcat-cli-v1.3.1-windows-x86_64.zip |
| Linux x86_64 | ciallogcat-cli-v1.3.1-linux-x86_64.tar.gz |
| Ubuntu 24.04 x86_64 | ciallogcat-cli-v1.3.1-linux-x86_64.deb |

CLI 工作流生成 `ciallogcat-cli-v<VERSION>-linux-x86_64.tar.gz` 与 `ciallogcat-cli-v<VERSION>-windows-x86_64.zip`，名称使用校验后的实际版本标签。每个 CLI 压缩包包含版本目录下的 CLI 可执行文件、MIT `LICENSE`、使用指南 `README.md`、AOSP 许可 `LICENSE-AOSP.txt`、protobuf 声明 `PROTOBUF-NOTICE.md` 和查询指南 `agent-query.md`，不包含 GUI、字体或共享核心 `.rlib`。Linux CLI 还生成独立的 `ciallogcat-cli-v<VERSION>-linux-x86_64.deb` 与校验文件，Debian 包名为 `ciallogcat-cli`。依赖由 `dpkg-shlibdeps` 从无 GUI 的二进制自动生成，ADB 为推荐依赖；文件仅包含 `/usr/bin/ciallogcat-cli` 与自己的文档目录。干净 Ubuntu 24.04 容器验证安装、运行时链接、版本、帮助与卸载。详情见 [CLI 指南](cli.md)。

每个附件都有同名 .sha256 文件。`full` 范围额外提供 GUI ZIP、tar.gz 和 DEB 及校验文件；GUI 压缩包包含程序、文档、MIT 与字体 OFL 许可及 BUILD-INFO.txt。GUI DEB 安装程序到 /usr/bin，桌面入口到 /usr/share/applications，图标到 hicolor 图标目录，文档及许可到 /usr/share/doc/ciallogcat。

DEB 自动分析链接库依赖，并显式声明运行时动态加载的图形库；adb 和 mesa-vulkan-drivers 为推荐依赖。ADB 不嵌入程序，已有 SDK 用户可自行提供。Linux GUI 产物需要兼容 Ubuntu 24.04 的系统库以及图形桌面，不保证兼容所有 Debian/Ubuntu 版本。

## 平台验收

CI 成功不代表桌面交互和真机采集已完成验收。发布前检查设备授权与选择、开启/暂停/关闭、缓冲区多选、筛选、多行复制、跟随与浏览切换、断连重连，以及达到日志内存预算后的长期走势。安装/卸载检查不启动图形窗口。

源码采用 MIT，内置字体采用 SIL OFL 1.1。临时诊断和构建产物保存在被忽略的 target 目录。不要提交设备日志、个人配置或凭据。


## 为既有 v1.0 追加 CLI DEB

独立的 `Supplement v1.0 CLI Debian package` 工作流只能从 main 手动触发，
使用固定的原始 v1.0 提交 `fdb6956886e9ac144b43b4b6e80bb807b535fad7`
构建 CLI，检查现有标签与当前 Cargo/源码一致，并使用新的打包脚本和安装指南。
在干净 Ubuntu 24.04 容器验证与已发布 GUI DEB 的共存及独立卸载后，
仅追加缺少的 CLI DEB 和 `.sha256`。已有同名资产必须与本次文件逐字节相同；
否则失败并等待人工检查。不会移动 v1.0 标签、覆盖其他资产或重新发布 release。
