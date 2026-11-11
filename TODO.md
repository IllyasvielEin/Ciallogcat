# TODO 与 Agent 交接

更新：2026-11-11（v1.4.0：Agent 循环体验与生命周期进程名）；此前 2026-10-29。这里是后续工作的唯一队列。先读本文，再读 [架构与方向](docs/development-plan.md)、[验证状态](docs/validation.md) 和 [迁移指南](docs/migration.md)。

## 当前状态与约定

- 2026-10-29 按用户要求移除旧 query/存档兼容路径，收敛为统一记录模型、唯一原生编码和共用有界读取；设备传输仍是显式 text/proto，不增加 auto 或版本别名。
- X27–X29 已修复并经独立复核：序号/字节双边界、非日志字段投影、最终 JSON 读写深度契约均有永久回归。原生窗口/设备验收仍与无显示器自动测试分开记录。
- 架构第一轮完整自动验证：headless 306、默认 GUI 配置 448 项通过（分别忽略 1/4 项）；双配置严格 Clippy、格式、74 项 Python 和 pinned protobuf 检查通过。随后编码去重及独立 GUI hillclimb 继续，最终合并工作树须重跑检查，不能沿用该轮数字。
- 当前基线为 `79f1e2855fd16b08e96264272abaaf68adf153a4`（1.2.2）。本轮仅本地修改，未提交、推送、改版号或发布；历史迁移与发布记录不是本轮的远端操作授权。
- 暂无可访问 Android 设备，用户已同意先用 mock，真机验收延期。Linux Xvfb/mock、交叉编译与真实平台运行必须分别记录。
- 默认文本传输，proto 显式选择；两者使用唯一原生格式，不提供版本选择或旧布局兼容。GUI/CLI 共用证据核心，设备操作交给外部执行器。

## 1. 已修复缺陷（P2，保留复现与验收条件）

### [x] X28：context/get 的已发布序号上界

位置：`src/cli/query.rs` 的 `context`、`reference_row`，对照 query 主路径。

复现：构造 native session，含 seq=1,2,3；status 的已发布字节覆盖三条，seq 上界却为 1。`query --limit 1` 返回 seq1 引用；以该引用调用 `context --before 0 --after 2`，旧实现返回 1–3，仍声明 snapshot.seq=1，且给 seq3 签发的引用能被 get 读取。query 续页则正确拒绝。

验收：context 与 get 都执行引用自身的 `upper.seq`，明确拒绝不一致来源，不能用后来增长的 session 上界代替原引用上界；覆盖离线/活动来源及正常 session 增长。此问题要求不一致 status，尚未证明正常原子发布会生成它。

### [x] X29：深层扩展布局/再筛选后不可重读

位置：`src/document.rs` 的 `projection_extensions`、`select` 的 `all_of` 包装、最终行编码预检。

历史复现（旧转换路径已移除；剩余单一编码的深度问题也已独立修复）：合法 v2 proto 行的 `device_time.future` 含 125 层短数组，load、转 CompactV3 和 write_snapshot 均成功，约 2 KiB 的输出重新 load/query 却报 recursion limit。原 v2 写回和 124 层转换是成功对照。合法 footer 的 `scope.filter` 含 124 层短数组，再筛选追加 all_of 也可写出不可读文件。

验收：统一最终布局的读写深度契约，保存和重复筛选成功输出必须能重读；不支持的深度在成功返回/发布前明确拒绝，并保留源文件和旧视图。覆盖临界层数、投影扩展、重复筛选及 CLI/GUI 读取。8 MiB 字节限制不能替代深度约束。

### [x] X27：非日志行显示内部占位字段

位置：`src/main.rs` 表格/可访问文本、`src/model.rs` 文本格式，以及详情与复制路径。

复现：合法 native event/unparsed 文件载入后 kind 仍为 Marker/Unparsed，但 GUI 表格、AccessKit 与 Ctrl+C 显示 Info、PID/TID 0/0、Tag logcat。这些是内部占位值，不是观测到的日志字段。

验收：表格、详情、可访问文本和复制采用统一 kind-aware 表达，明确非日志类型，缺失日志字段留空或未知；`copy_text_bytes` 与实际 UTF-8 输出一致。覆盖真实 egui frame、Copy 事件、普通日志对照及原生窗口复验；保持文件证据不变。

## 2. 最终验证与交付

- [ ] X23–X26 最终集成验收（本轮全量自动测试通过，release 重建及原生交互复验仍待完成）：X23 截断末行不参与诊断分类；X24 成功 dump 后同 session 恢复仍记未知暂停；X25 Failed 不调度过期快照 deadline；X26 footer 诊断按 JSON 编码字节限额。独立定向复核已通过，勿重复作为未修问题。
- [ ] 编码去重与 GUI hillclimb 合并后重新运行 [贡献指南](CONTRIBUTING.md) 的 GUI/headless 测试、双配置 Clippy、格式检查、Python fixture/Agent/benchmark/打包测试和 protobuf 再生成检查。
- [ ] 重建 GUI 与 headless CLI release，记录源码状态、工具链和二进制 hash；针对受影响交互做 mock 原生复验。旧 372/236 测试数及旧 release 不代表最终工作树通过。
- [ ] 需要性能结论时用冻结的新构建复测 text/proto，并通过显式旧二进制比较历史基线，保留 wall/CPU/RSS、完整性与原 payload 核对；不将存档缩小等同吞吐提升。
- [ ] 整理提交并推送，确认新远端 CI（包括 Windows）实际完成，再按 [发布指南](docs/release.md) 决定版本/发布。本轮尚未获得推送或发布授权；先报告最终本地验证结果，分别确认后续操作。

## 3. 平台与真机（条件具备后）

- [ ] 已知 X11/XIM 中文候选框左上角定位异常：应用已输出非零且随光标变化坐标，继续定位 XIM 传递；验证候选框、提交与取消。
- [ ] 原生 Windows：采集生命周期、子进程回收、GUI/剪贴板/窗口拖动和 GPU 延迟；交叉编译不等于运行通过。Agent 示例只保证直接执行器回收，Windows 后代进程归外部执行器负责。
- [ ] Wayland/系统 IME、原生拖放、多屏动态 DPI、窗口最小尺寸与真实 GPU 验收。旧嵌套 Weston resize 出现合成器崩溃且无 text-input-v3；不能据此认定产品通过或失败。
- [ ] 设备矩阵：AOSP 14 不支持时明确失败；15/16 proto 与至少一台 OEM user build；17 有条件补测。同一支持设备验证 Linux/Windows ADB 字节通道，macOS 仅作为实验、不扩大当前发行支持。
- [ ] 设备验收包含 shell_v2 stdout/stderr/exit、`00 0a 0d 1a ff` 字节、framing、NUL/进程名缓存、buffer 权限、授权/拔插/重启、半帧停止与回收；不自动降级、不清空或调整 logd 掩盖问题。
- [ ] 最终构建实机 30–60 分钟 RSS/吞吐/延迟/丢弃量；记录 host/adb/SDK/fingerprint、命令、预算、格式/profile、产品 hash。text/proto 两次 dump 不假设 ring buffer 内容完全相同。详细方法见 [protobuf 设备矩阵](docs/logcat-protobuf.md#11-可重复的设备验收矩阵)。

## 4. Agent 循环体验（2026-11-11，随 v1.4.0 发布）

已完成（`tests/agent_loop.rs` 及既有 query/CLI 回归；fmt、双配置 Clippy、workspace 全量测试、Python 示例/fixture 测试通过，均为 mock，无真机）：

- token 瘦身：`c5`/`r3`，base64url + 128 位截断 HMAC，载荷只带来源绑定摘要；ref 约 974→260 字节，cursor 约 1.6 KB→0.7 KB。旧 token 拒绝。
- `--preview-bytes` 默认 512；全局 `--help` 改为约 2 KB 的索引加 Agent 循环示例，各子命令只显示自身帮助。
- `--package` 遇到没有进程名的记录时返回 `warnings[].code=package_unresolved`，不再静默零结果。
- 标记：未扫描的 `--start-at end` cursor 可换任意过滤条件复用；`--cursor` 和 `--start-at end` 隐含 mode。
- `query --wait-ms`（tail，≤60 s）；tail 模式允许 `--all`。
- `crashes`：Java/native/ANR 按签名分组，带 `first_ref`；`context --same-pid`。
- 同一 lab 崩溃场景下一次排查的响应量约 47 KB→11 KB（含同 PID 上下文约 19 KB）。

第二轮（2026-11-11，mock 验证）：

- [x] A1 进程名：CLI text 采集用 `log_lifecycle`（ActivityManager `Start proc`/`has died`、AndroidRuntime `Process:`）和 live follow 下的 `ps_snapshot`（与 GUI 同规则）标注；`--package` 在 CLI 采集上可用，并新增 `包名_zygote` 归属。proto 继续使用设备缓存。
- [x] A2 记录 JSON 精简为响应 schema 3；`source_status` 省略 null。
- [x] A3 `get/context --seq N`，按字节偏移二分定位（2 万条历史约十几 KiB 读取）。
- [x] A4 官方 skill `skills/ciallogcat-cli/SKILL.md`，随包分发，`ciallogcat-cli skill` 输出；不做 MCP。`agent_capture.py` 报告附崩溃分组。
- [x] A5 `get` 默认 decoded（text 记录与 stored 相同；proto 约 2–3 倍，仍按响应预算分块）。
- [x] A7 删除无操作的 `--case-sensitive`。

待讨论：

- [x] A1 后续：GUI 通过共享核心 `process_names` 使用同一 `log_lifecycle` + ps 归属（v1.4.0）。
- [ ] A1 真机：验证 Android 8 前后的 `Start proc` 格式、isolated/sandboxed 进程名和 OEM 差异；评估 CLI 每秒一次 `ps` 的开销是否需要开关。
- [ ] A6-1 模板摘要（Drain 式）：先做离线评估，用同一批问题比较原始日志、模板摘要的回答正确率；未证明不误导前不进入 CLI。
- [ ] A6-2 启动耗时：`ActivityTaskManager`/`ActivityManager` 的 `Displayed`/`Fully drawn` 行可像 crashes 一样做 `launches` 汇总；体量小，可作为下一个 incident 类型。
- [ ] A6-3 脱敏：只做可解释、可计数的显式类别（token/JWT/邮箱/手机号等），输出标注“尽力而为，不保证无漏”；默认不开启。
- [ ] A6-4 证据包：以 native JSONL + `diagnose` 报告 + 查询条件 + crashes 摘要为主体，不复制 `adb bugreport` 的全部内容；bugreport 可作为可选附件路径。
- [ ] A8 不做：自动启动会话、设备点击/安装编排、默认 `logcat -c`、MCP 服务。

## 5. 可选方向（需求成立再排期）

- [ ] 原生系统文件选择器：当前支持路径与拖放，需独立依赖和平台兼容评估。
- [ ] GUI 原始/解码/展示对象的重复持有与峰值内存：先测再优化，紧凑编码目前只去重磁盘存档。
- [ ] 自动选 proto、独立多 buffer 流、数据库索引：以实际使用场景决定；暂不扩展为设备测试编排平台（MCP 见 A4）。

## 历史证据

阶段报告已合并清理。问题触发与验收条件在本文完整保留，不依赖旧主机绝对路径。可选的原始 PoC、截图与日志仍在旧主机忽略目录 `target/sol-high-review-2026-10-24/`、`target/astra-xhigh-review-2026-10-24/`、`target/queue-experience-2026-10-24*/` 等；它们不是构建依赖，迁移可另行归档。新主机应从源码重新产生验证证据。
