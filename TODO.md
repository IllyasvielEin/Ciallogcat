# TODO 与 Agent 交接

更新：2026-10-28。这里是后续工作的唯一队列。先读本文，再读 [架构与方向](docs/development-plan.md)、[验证状态](docs/validation.md) 和 [迁移指南](docs/migration.md)。

## 当前状态与约定

- 2026-10-28 用户要求整理并提交迁移成果、准备继续开发。本轮恢复工作区、保存功能基线并更新交接；下一轮按 X28 → X29 → X27 的顺序推进。此前暂停是历史状态，本轮未修改这些产品缺陷。
- Q01–Q09、X01–X22 已完成原定实现及 mock 验收；X23–X26 已修复且独立定向复核通过，本轮全量自动检查已通过，仍欠 release 重建和受影响交互复验。X27–X29 是已确认未修的 P2，未确认 P0/P1。
- 最近一次独立审查为 gpt-6.1-sol / high 三路团队检查，已结束，无需恢复旧子任务。
- 迁移包基线为 `853976f69ae7f362a6bbe5c12781172f5cd86403`；2026-10-28 重新获取远端，`origin/main` 仍为该提交。本轮将快照中的 116 个文件变更保存为本地功能提交 `cb5e9fe`；尚未推送，远端仍不含这些成果。原迁移包保留在工作区根目录，由 `.git/info/exclude` 本地排除。
- 暂无可访问 Android 设备，用户已同意先用 mock，真机验收延期。Linux Xvfb/mock、交叉编译与真实平台运行必须分别记录。
- 默认传输/格式策略保持现状；proto 和 compact v3 显式选择。GUI/CLI 共用证据核心，设备操作交给外部执行器。

## 1. 已确认缺陷（P2，按处理顺序）

### [ ] X28：context/get 的已发布序号上界

位置：`src/cli/query_v2.rs` 的 `context`、`reference_row`，对照 query 主路径。

复现：构造 v1/v2/v3 session，各含 seq=1,2,3；status 的已发布字节覆盖三条，seq 上界却为 1。`query-v2 --limit 1` 返回 seq1 引用；以该引用调用 `context-v2 --before 0 --after 2`，当前返回 1–3，仍声明 snapshot.seq=1，且给 seq3 签发的引用能被 get-v2 读取。原 query 续页则正确拒绝。

验收：context 与 get 都执行引用自身的 `upper.seq`，明确拒绝不一致来源，不能用后来增长的 session 上界代替原引用上界；覆盖三个来源版本及正常 session 增长。此问题要求不一致 status，尚未证明正常原子发布会生成它。

### [ ] X29：深层扩展转换/再筛选后不可重读

位置：`src/document.rs` 的格式转换、`projection_extensions`、`select` 的 `all_of` 包装、最终行编码预检。

复现：合法 v2 proto 行的 `device_time.future` 含 125 层短数组，load、转 CompactV3 和 write_snapshot 均成功，约 2 KiB 的输出重新 load/query 却报 recursion limit。原 v2 写回和 124 层转换是成功对照。合法 footer 的 `scope.filter` 含 124 层短数组，再筛选追加 all_of 也可写出不可读文件。

验收：统一最终布局的读写深度契约，转换和重复筛选成功输出必须能重读；不支持的深度在成功返回/发布前明确拒绝，并保留源文件和旧视图。覆盖临界层数、v2/v3 转换、重复筛选及 CLI/GUI 读取。8 MiB 字节限制不能替代深度约束。

### [ ] X27：非日志行显示内部占位字段

位置：`src/main.rs` 表格/可访问文本、`src/model.rs` 文本格式，以及详情与复制路径。

复现：合法 native event/unparsed 文件载入后 kind 仍为 Marker/Unparsed，但 GUI 表格、AccessKit 与 Ctrl+C 显示 Info、PID/TID 0/0、Tag logcat。这些是内部占位值，不是观测到的日志字段。

验收：表格、详情、可访问文本和复制采用统一 kind-aware 表达，明确非日志类型，缺失日志字段留空或未知；`copy_text_bytes` 与实际 UTF-8 输出一致。覆盖真实 egui frame、Copy 事件、普通日志对照及原生窗口复验；保持文件证据不变。

## 2. 最终验证与交付

- [ ] X23–X26 最终集成验收（本轮全量自动测试通过，release 重建及原生交互复验仍待完成）：X23 截断末行不参与诊断分类；X24 成功 dump 后同 session 恢复仍记未知暂停；X25 Failed 不调度过期快照 deadline；X26 footer 诊断按 JSON 编码字节限额。独立定向复核已通过，勿重复作为未修问题。
- [ ] 完成 X27–X29 修复后，运行 [贡献指南](CONTRIBUTING.md) 的 GUI/headless 测试、双配置 Clippy、格式检查、Python fixture/Agent/benchmark/打包测试和 protobuf 再生成检查。
- [ ] 重建 GUI 与 headless CLI release，记录源码状态、工具链和二进制 hash；针对受影响交互做 mock 原生复验。旧 372/236 测试数及旧 release 不代表最终工作树通过。
- [ ] 需要性能结论时用同一新构建复测三组合，保留 wall/CPU/RSS、完整性与原 payload 核对；不将存档缩小等同吞吐提升。
- [ ] 整理提交并推送，确认新远端 CI（包括 Windows）实际完成，再按 [发布指南](docs/release.md) 决定版本/发布。本轮已保存本地功能提交；用户随后要求发布 1.1.0，版本、发布说明及本机 GUI/headless release 构建已准备，推送因缺少 GitHub 凭据受阻，远端 CI 和发布尚待完成。

## 3. 平台与真机（条件具备后）

- [ ] 已知 X11/XIM 中文候选框左上角定位异常：应用已输出非零且随光标变化坐标，继续定位 XIM 传递；验证候选框、提交与取消。
- [ ] 原生 Windows：采集生命周期、子进程回收、GUI/剪贴板/窗口拖动和 GPU 延迟；交叉编译不等于运行通过。Agent 示例只保证直接执行器回收，Windows 后代进程归外部执行器负责。
- [ ] Wayland/系统 IME、原生拖放、多屏动态 DPI、窗口最小尺寸与真实 GPU 验收。旧嵌套 Weston resize 出现合成器崩溃且无 text-input-v3；不能据此认定产品通过或失败。
- [ ] 设备矩阵：AOSP 14 不支持时明确失败；15/16 proto 与至少一台 OEM user build；17 有条件补测。同一支持设备验证 Linux/Windows ADB 字节通道，macOS 仅作为实验、不扩大当前发行支持。
- [ ] 设备验收包含 shell_v2 stdout/stderr/exit、`00 0a 0d 1a ff` 字节、framing、NUL/进程名缓存、buffer 权限、授权/拔插/重启、半帧停止与回收；不自动降级、不清空或调整 logd 掩盖问题。
- [ ] 最终构建实机 30–60 分钟 RSS/吞吐/延迟/丢弃量；记录 host/adb/SDK/fingerprint、命令、预算、格式/profile、产品 hash。text/proto 两次 dump 不假设 ring buffer 内容完全相同。详细方法见 [protobuf 设备矩阵](docs/logcat-protobuf.md#11-可重复的设备验收矩阵)。

## 4. 可选方向（需求成立再排期）

- [ ] 原生系统文件选择器：当前支持路径与拖放，需独立依赖和平台兼容评估。
- [ ] GUI 原始/解码/展示对象的重复持有与转换峰值内存：先测再优化，v3 目前只去重磁盘存档。
- [ ] 自动选 proto、独立多 buffer 流、数据库索引、MCP 封装：以实际使用场景决定；暂不扩展为设备测试编排平台。

## 历史证据

阶段报告已合并清理。问题触发与验收条件在本文完整保留，不依赖旧主机绝对路径。可选的原始 PoC、截图与日志仍在旧主机忽略目录 `target/sol-high-review-2026-10-24/`、`target/astra-xhigh-review-2026-10-24/`、`target/queue-experience-2026-10-24*/` 等；它们不是构建依赖，迁移可另行归档。新主机应从源码重新产生验证证据。
