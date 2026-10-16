# GitHub 发布指南

## 版本与触发

当前版本为 v1.0，Cargo 版本为 1.0.0。完整标签必须等于 Cargo 版本加 v 前缀；补丁号为零的正式版本也接受短标签，例如 v1.0 对应 1.0.0。预发布标签使用完整版本，例如 v1.0.0-rc.1。

Release 工作流仅在推送版本标签时触发构建、打包和发布，不提供手动触发入口。

分支推送和 Pull request 不触发构建或发布。重跑旧标签任务仍使用该标签的旧代码。公开标签不移动或覆盖，修复通过新版本发布。

## v1.0 更新

- 将 ADB 采集、解析、筛选和日志存储抽为共享核心；`--no-default-features` 构建无需 GUI 依赖。
- 新增独立 `ciallogcat-cli`，支持设备查询、有界前台采集，以及本地后台会话的启动、状态、查询和停止。
- CLI 使用 JSON / JSONL 输出、独立会话目录和停止标记，不依赖持久化 PID 结束进程，也不清空设备日志。
- 保留默认桌面应用，新增 Linux / Windows CLI 测试和独立压缩包；共享核心 `.rlib` 不作为发布附件。

## 发布流程

1. 更新 Cargo.toml 版本并通过 cargo check 更新 Cargo.lock。
2. 执行贡献指南中的格式、测试、Clippy 和 release 构建检查，检查待提交差异。
3. 提交源码、资源和文档并推送待发布分支；普通分支推送不触发 CI。
4. 创建并推送对应版本标签，例如：

```bash
git tag -a v1.0 -m "Ciallogcat v1.0"
git push origin v1.0
```

CI 校验标签，然后在 Windows 和 Ubuntu 24.04 执行格式检查、测试、Clippy、release 构建和打包。Linux 使用固定版本 cargo-deb 3.8.0 额外生成 DEB，在干净 Ubuntu 24.04 容器中验证安装、桌面入口、图标、许可证、动态链接依赖和卸载。

CLI 使用独立的 Linux x86_64 / Windows x86_64 矩阵，执行 `cargo test --locked --no-default-features`、无 GUI 的 Clippy、`cargo build --locked --release --no-default-features --bin ciallogcat-cli`，以及 `--help` / `--version` 冒烟检查与打包测试。无需安装 GUI 开发依赖。GUI 与 CLI 的两个平台作业全部成功后才允许发布；标签触发规则与草稿上传后公开的流程不变。此变更不补发或覆盖旧的公开 Release。

GUI 与 CLI 的四个矩阵作业成功后，发布作业核验所有 SHA-256 校验文件，创建草稿、上传附件，再公开 Release；预发布版本公开为 Pre-release。失败时不发布，上传失败可保留草稿后重试。已公开的 Release 不会被覆盖。

## 发布附件

| 平台 | 附件 |
| --- | --- |
| Windows x86_64 | ciallogcat-v1.0-windows-x86_64.zip |
| Linux x86_64 | ciallogcat-v1.0-linux-x86_64.tar.gz |
| Ubuntu 24.04 x86_64 | ciallogcat-v1.0-linux-x86_64.deb |

CLI 工作流另行生成 `ciallogcat-cli-v<VERSION>-linux-x86_64.tar.gz` 与 `ciallogcat-cli-v<VERSION>-windows-x86_64.zip`，名称使用校验后的实际版本标签。每个 CLI 压缩包仅包含版本目录下的 CLI 可执行文件、MIT `LICENSE` 和使用指南 `README.md`，不包含 GUI、字体或共享核心 `.rlib`。CLI 不生成 DEB。详情见 [CLI 指南](cli.md)。

每个附件都有同名 .sha256 文件。GUI ZIP 和 tar.gz 包含程序、文档、MIT 与字体 OFL 许可及 BUILD-INFO.txt。DEB 安装程序到 /usr/bin，桌面入口到 /usr/share/applications，图标到 hicolor 图标目录，文档及许可到 /usr/share/doc/ciallogcat。

DEB 自动分析链接库依赖，并显式声明运行时动态加载的图形库；adb 和 mesa-vulkan-drivers 为推荐依赖。ADB 不嵌入程序，已有 SDK 用户可自行提供。Linux GUI 产物需要兼容 Ubuntu 24.04 的系统库以及图形桌面，不保证兼容所有 Debian/Ubuntu 版本。

## 平台验收

CI 成功不代表桌面交互和真机采集已完成验收。发布前检查设备授权与选择、开启/暂停/关闭、缓冲区多选、筛选、多行复制、跟随与浏览切换、断连重连，以及达到日志内存预算后的长期走势。安装/卸载检查不启动图形窗口。

源码采用 MIT，内置字体采用 SIL OFL 1.1。临时诊断和构建产物保存在被忽略的 target 目录。不要提交设备日志、个人配置或凭据。
