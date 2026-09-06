# GitHub 发布指南

## 仓库配置

在 GitHub 创建仓库，将仓库地址填写到 `Cargo.toml` 的 `repository` 字段，并配置本地 `origin` 远程地址。默认分支为 `main`。仓库需要启用 GitHub Actions，并允许发布作业使用 `GITHUB_TOKEN` 的 `contents: write` 权限。

提交包含源码、`Cargo.lock`、内置字体及其许可文件。构建输出和临时诊断文件位于被忽略的 `target/` 下。提交前检查 `git status --short` 和待提交差异，避免包含设备日志、个人配置或凭据。

源码采用根目录 [MIT License](../LICENSE)，内置字体采用 [SIL Open Font License 1.1](../assets/fonts/LICENSE-OFL.txt)。

## 版本标签与自动发布

[Release 工作流](../.github/workflows/ci.yml) 仅由版本标签推送触发。分支推送、Pull request 和本地创建标签不会运行 CI。

首版显示版本为 **v0.1**，Cargo 版本为 `0.1.0`，Git 标签为 `v0.1`。首版 `v0.1` 对应 Cargo `0.1.0`；其他标签必须严格等于 Cargo 版本加 `v` 前缀，例如 `v0.1.1`、`v0.2.0`；预发布版本可使用 `v0.2.0-rc.1`，Cargo 对应 `0.2.0-rc.1`。

每次发布先更新 `Cargo.toml` 的 `version`，运行 `cargo check` 更新 `Cargo.lock`，执行 [贡献指南](../CONTRIBUTING.md) 中的本地检查，并提交需要发布的源码和文档。以下以 `0.1.1` 为例：

```bash
git add Cargo.toml Cargo.lock
# 一并暂存并检查本次发布的源码与文档
git diff --cached
git commit -m "chore: prepare v0.1.1"
git push origin main
git tag -a v0.1.1 -m "Ciallogcat v0.1.1"
git push origin v0.1.1
```

首次发布使用 Cargo `0.1.0`，标签命令为 `git tag -a v0.1 -m "Ciallogcat v0.1"` 和 `git push origin v0.1`。打标签前确认本地 HEAD 是准备发布的提交。

标签推送后自动执行：

1. 校验标签与 Cargo 版本一致。
2. 在 Linux 和 Windows 上执行格式检查、测试、Clippy 和 release 构建。
3. 打包两个平台的程序、文档、MIT 与字体 OFL 许可，并生成 SHA-256 校验文件。
4. 两个平台全部成功后，下载产物并核验校验和，创建 Release 草稿、上传附件，再公开 Release。发行说明包含自动生成的变更摘要与平台验证限制。

普通版本标签发布正式 Release，含预发布后缀的标签发布 Pre-release。检查或构建失败时不进入发布作业；附件上传失败时保留草稿，可在 Actions 中重新运行失败作业。已公开的 Release 不会被工作流覆盖。

## 发布附件

| 平台 | 压缩包 |
| --- | --- |
| Windows x86_64 | `ciallogcat-v0.1-windows-x86_64.zip` |
| Linux x86_64 | `ciallogcat-v0.1-linux-x86_64.tar.gz` |

每个压缩包附有同名 `.sha256` 校验文件。压缩包包含可执行文件、README、贡献与发布指南、`LICENSE`、字体说明与 OFL 许可证，以及记录标签、提交和工具链的 `BUILD-INFO.txt`。字体已嵌入程序，ADB 由用户安装。

Linux 使用 Ubuntu 24.04 构建，打包时检查并记录动态库依赖；运行需要兼容的系统库及 X11 或 Wayland 图形桌面。产物不保证兼容所有 Linux 发行版。

## 平台验收

发布前在 Windows 和 Linux 桌面使用真机检查设备授权与选择、开启/暂停/关闭、缓冲区应用、筛选与复制、断连重连、持续采集内存走势和窗口拖动。验证范围与限制见 [UI 设计与性能](ui-review.md#验证范围)。CI 成功仅表示自动检查通过，不代表这些场景完成验收。

平台验收未完成的候选版本使用预发布后缀。公开的标签不应移动或覆盖；需要修复时发布新的补丁版本。
