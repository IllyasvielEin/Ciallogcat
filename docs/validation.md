# 验证状态与边界

历史证据截至 2026-10-24；本次新主机自动检查更新于 2026-10-28。未完成项统一见 [TODO](../TODO.md)。

## 2026-10-28 新主机检查

验证源码为本地功能提交 `cb5e9fe`，工具链为 rustc 1.99.1 / cargo 1.99.1，运行于 Linux；使用 mock，不涉及真机。迁移包与源码文件哈希、权限已校验。

- `cargo test --locked --all-targets`：376 通过、4 忽略。
- `cargo test --locked --no-default-features --all-targets`：238 通过、1 忽略。两组覆盖重叠，不能相加。
- 两种配置的严格 Clippy 均通过。初次检查发现 Rust 1.99 新增的 `chunks_exact_to_as_chunks` 警告；将 token 十六进制解码改为等价的固定数组分块后，两套 Clippy 及 28 项 query-v2 集成测试复查通过。
- 项目与 protobuf 工具格式检查、protobuf 生成代码一致性检查均通过。
- Python fake ADB 22 项、Agent 33 项、benchmark 10 项、打包 8 项全部通过。
- `git diff --cached --check` 在原迁移功能提交中仅报告上游 Apache NOTICE 末尾空行；按 `proto/README.md` 保留第三方原文。

本轮原始日志在忽略目录 `target/handoff-validation/`；`results.json` 记录初次执行结果，Clippy 修正后的成功结果在 `*-clippy-recheck.log`，查询复查在 `query-v2-recheck.log`。

本轮未执行 release 重建、原生窗口交互、Windows 运行或 Android 真机验收，也未推送或运行远端 CI。全量现有测试通过不代表 X27–X29 已修复，它们仍是明确待办。以下旧构建哈希和历史体验证据不代表本轮生成的新工件。

## 2026-10-28 v1.1.0 发布准备

用户随后要求推送并发布 1.1.0。已核对最新公开 Release 为 v1.0，版本提交将 Cargo.toml/Cargo.lock 更新为 1.1.0，工作流从版本专属文档读取发布说明并保留已知问题。

- `cargo check`、锁文件版本核对、格式检查和发布脚本 Bash 语法检查通过；产品代码相对上述全量验证仅变化包版本号。
- `cargo build --locked --release` 和独立 headless CLI release 构建通过；CLI `--help` 成功，`--version` 返回 1.1.0。
- 本机 GUI SHA-256：`907cc33a7482c5a7e2464d26c039fc428eea57468997e2e6329ffd73401ddab0`。
- 本机 headless CLI SHA-256：`e2819846a7a57b79dc2aa3e8bba54085191c90178fe91193a2b0b85df9a5b319`。

本机构建不作为 GitHub 发行附件；附件由标签触发的 Linux/Windows CI 生成并校验。写入本记录时尚未成功推送（本机缺少 GitHub 凭据），远端发布结果需另行核对。原生窗口和真机验收仍待完成。

## 已执行的基线

Q01–Q09 与 X01–X22 的原定实现及 mock 验收完成。审查前冻结构建：

- GUI SHA-256：`c3d468e5c8e5da9a3f6e6f4bc743ca96f93f87ed44f9f37483bc48244a2ab463`。
- CLI SHA-256：`ad9fc2abb214ffad82e0662b64d52b2efb578ee1614b4108cf19d0e720091351`。
- 该基线默认测试 372 通过、4 忽略；headless 236 通过、1 忽略。忽略项包含由外层调用的 worker 和手动性能实验，两组数字重叠，不能相加。
- 双配置严格 Clippy、格式、release 构建、Windows MSVC 交叉检查，以及 Agent、benchmark、fixture、打包协议测试曾通过。新远端 CI 尚无执行确认。

Linux Xvfb 软件渲染原生体验覆盖采集/暂停/重连、历史范围/有限快照、全文/上下文、900×560 逻辑尺寸及 1.5 scale、₿ 字形、系统剪贴板、v3 保存/兼容 v2、身份/扩展/间隙交接。首轮发现范围菜单、诊断窗口不可见及离线筛选元数据过期，均修复并在新构建复验。诊断 normal/partial/large/silent 显示、复制、取消和来源隔离通过；69,150-byte header 经 GUI 保存后可由 CLI 查询。

一次 large 会话的交互 harness 异常退出未归因产品，独立新会话保存复验成功。900×560 physical 在 1.5 scale 下只有 600×373 logical，不能当作支持的 900×560 布局验收。历史 30 分钟合成 CLI 流只说明当时构建/负载的主机行为，不代表最新版本或真机长期稳定。

三种传输/存档组合 6 次预热、30 次测量通过；固定构建、数值和复现方式保留在 [性能报告](transport-performance-2026-10-24.md)。

## 最新独立审查

三路 gpt-6.1-sol/high 只读检查完成，未确认 P0/P1：

- 采集：60 项定向检查和 12 组 CLI mock，X24–X26 复核通过；footer 最坏合成编码 2934 B，小于 4096 B 预留。
- 格式/查询：92 项现有定向检查和 15 组分页检查；确认 X28 上界与 X29 深度问题。
- GUI/诊断/质量：123 项定向检查，X23 修复复核通过，X27 仍可复现。

这些检查存在重叠且包含确认缺陷存在的 PoC，不能汇总为“全部产品通过”。截至该次历史审查，X23–X26 源码已修复，但尚欠最后全量与重建；2026-10-28 全量自动检查已通过，重建与原生体验仍待完成，X27–X29 仍未修。普通 egui 帧验证与较早真实窗口体验分别记录，不冒称本轮新原生桌面验收。

## 可复现入口与局限

检查命令见 [贡献指南](../CONTRIBUTING.md)，模拟 ADB 见 [测试指南](adb-testing.md)，场景体验见 [协议](experience-testing.md) 和 [工具](../tools/experience-lab/README.md)。依赖已归入仓库源码/锁文件，历史 target 工件可选保留。

真实 Android/OEM、原生 Windows 运行、GPU、多屏 DPI、系统 IME/拖放尚未完整验收。已知 X11/XIM 候选框位置异常仍待处理。Linux mock、软件渲染和交叉编译不替代这些环境；详细待做项与验收条件见 [TODO](../TODO.md)。
