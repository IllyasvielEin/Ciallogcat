# GitHub 发布指南

## 版本与触发

当前版本为 v0.2，Cargo 版本为 0.2.0。完整标签必须等于 Cargo 版本加 v 前缀；补丁号为零的正式版本也接受短标签，例如 v0.2 对应 0.2.0。预发布标签使用完整版本，例如 v0.3.0-rc.1。

Release 工作流支持版本标签推送和手动运行。手动运行填写与 Cargo 匹配的 release_tag，执行检查和打包但不发布 Release：

```bash
gh workflow run ci.yml --ref main -f release_tag=v0.2
```

分支推送和 Pull request 不触发发布。重跑旧标签任务仍使用该标签的旧代码。公开标签不移动或覆盖，修复通过新版本发布。

## 发布流程

1. 更新 Cargo.toml 版本并通过 cargo check 更新 Cargo.lock。
2. 执行贡献指南中的格式、测试、Clippy 和 release 构建检查，检查待提交差异。
3. 提交源码、资源和文档并推送 main，建议先手动运行双平台验证。
4. 创建并推送对应版本标签，例如：

```bash
git tag -a v0.2 -m "Ciallogcat v0.2"
git push origin v0.2
```

CI 校验标签，然后在 Windows 和 Ubuntu 24.04 执行格式检查、测试、Clippy、release 构建和打包。Linux 使用固定版本 cargo-deb 3.8.0 额外生成 DEB，在干净 Ubuntu 24.04 容器中验证安装、桌面入口、图标、许可证、动态链接依赖和卸载。

两个平台成功后，发布作业核验所有 SHA-256 校验文件，创建草稿、上传附件，再公开 Release；预发布版本公开为 Pre-release。失败时不发布，上传失败可保留草稿后重试。已公开的 Release 不会被覆盖。

## 发布附件

| 平台 | 附件 |
| --- | --- |
| Windows x86_64 | ciallogcat-v0.2-windows-x86_64.zip |
| Linux x86_64 | ciallogcat-v0.2-linux-x86_64.tar.gz |
| Ubuntu 24.04 x86_64 | ciallogcat-v0.2-linux-x86_64.deb |

每个附件都有同名 .sha256 文件。ZIP 和 tar.gz 包含程序、文档、MIT 与字体 OFL 许可及 BUILD-INFO.txt。DEB 安装程序到 /usr/bin，桌面入口到 /usr/share/applications，图标到 hicolor 图标目录，文档及许可到 /usr/share/doc/ciallogcat。

DEB 自动分析链接库依赖，并显式声明运行时动态加载的图形库；adb 和 mesa-vulkan-drivers 为推荐依赖。ADB 不嵌入程序，已有 SDK 用户可自行提供。Linux 产物需要兼容 Ubuntu 24.04 的系统库以及图形桌面，不保证兼容所有 Debian/Ubuntu 版本。

## 平台验收

CI 成功不代表桌面交互和真机采集已完成验收。发布前检查设备授权与选择、开启/暂停/关闭、缓冲区多选、筛选、多行复制、跟随与浏览切换、断连重连，以及达到日志内存预算后的长期走势。安装/卸载检查不启动图形窗口。

源码采用 MIT，内置字体采用 SIL OFL 1.1。临时诊断和构建产物保存在被忽略的 target 目录。不要提交设备日志、个人配置或凭据。
