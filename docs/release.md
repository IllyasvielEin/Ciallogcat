# GitHub 发布指南

## 版本与触发

当前源码目标版本为 v1.2.2，Cargo 版本为 1.2.2；最新公开版本需以 GitHub Releases 为准。完整标签必须等于 Cargo 版本加 v 前缀；补丁号为零的正式版本也接受短标签，例如 v1.0 对应 1.0.0。预发布标签使用完整版本，例如 v1.0.0-rc.1。

Release 工作流仅在推送版本标签时触发构建、打包和发布，不提供手动触发入口。

独立的 [Checks 工作流](../.github/workflows/checks.yml) 在 Pull request、推送 main 和手动触发时执行开发期验证，不打包或发布。其他分支推送本身不触发检查，可通过 PR 或手动运行验证。重跑旧标签任务仍使用该标签的旧代码。公开标签不移动或覆盖，修复通过新版本发布。

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

1. 更新 Cargo.toml 版本并通过 cargo check 更新 Cargo.lock。
2. 执行贡献指南中的格式、测试、Clippy 和 release 构建检查，检查待提交差异。
3. 提交源码、资源和文档并推送待发布分支，通过 PR 或手动运行 Checks；合并到 main 后也会自动检查。
4. 创建并推送对应版本标签，例如：

```bash
git tag -a v1.2.2 -m "Ciallogcat v1.2.2"
git push origin v1.2.2
```

CI 校验标签，然后在 Windows 和 Ubuntu 24.04 执行格式检查、测试、Clippy、release 构建和打包。Linux 使用固定版本 cargo-deb 3.8.0 额外生成 DEB，在干净 Ubuntu 24.04 容器中验证安装、桌面入口、图标、许可证、动态链接依赖和卸载。

CLI 使用独立的 Linux x86_64 / Windows x86_64 矩阵，执行 `cargo test --locked --no-default-features`、无 GUI 的 Clippy、`cargo build --locked --release --no-default-features --bin ciallogcat-cli`，以及 `--help` / `--version` 冒烟检查与打包测试。无需安装 GUI 开发依赖。GUI 与 CLI 的两个平台作业全部成功后才允许发布；标签触发规则与草稿上传后公开的流程不变。此变更不补发或覆盖旧的公开 Release。

GUI 与 CLI 的四个矩阵作业成功后，发布作业核验所有 SHA-256 校验文件，创建草稿、上传附件，再公开 Release；预发布版本公开为 Pre-release。失败时不发布，上传失败可保留草稿后重试。已公开的 Release 不会被覆盖。

## 发布附件

| 平台 | 附件 |
| --- | --- |
| Windows x86_64 | ciallogcat-v1.2.2-windows-x86_64.zip |
| Linux x86_64 | ciallogcat-v1.2.2-linux-x86_64.tar.gz |
| Ubuntu 24.04 x86_64 | ciallogcat-v1.2.2-linux-x86_64.deb |

CLI 工作流另行生成 `ciallogcat-cli-v<VERSION>-linux-x86_64.tar.gz` 与 `ciallogcat-cli-v<VERSION>-windows-x86_64.zip`，名称使用校验后的实际版本标签。每个 CLI 压缩包仅包含版本目录下的 CLI 可执行文件、MIT `LICENSE` 和使用指南 `README.md`，不包含 GUI、字体或共享核心 `.rlib`。Linux CLI 还生成独立的 `ciallogcat-cli-v<VERSION>-linux-x86_64.deb` 与校验文件，Debian 包名为 `ciallogcat-cli`。依赖由 `dpkg-shlibdeps` 从无 GUI 的二进制自动生成，ADB 为推荐依赖；文件仅包含 `/usr/bin/ciallogcat-cli` 与自己的文档目录。干净 Ubuntu 24.04 容器验证安装、运行时链接、版本、帮助与卸载。详情见 [CLI 指南](cli.md)。

每个附件都有同名 .sha256 文件。GUI ZIP 和 tar.gz 包含程序、文档、MIT 与字体 OFL 许可及 BUILD-INFO.txt。DEB 安装程序到 /usr/bin，桌面入口到 /usr/share/applications，图标到 hicolor 图标目录，文档及许可到 /usr/share/doc/ciallogcat。

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
