# CLI issue 反馈评估与优化方案

日期：2026-11-05。源码基线：`f4956892082aa55f48ec648e7ebec7b0e251ce84`，CLI 1.3.1。
本文保留已审核的设计决策；后续本地实现与验收记录见末节，不表示功能已发布。任务排期仍以 `TODO.md` 为准。

## 反馈是否值得采纳

已通过现有 GitHub CLI 登录读取仓库全部 3 个 issues（包括正文和评论），并与 `gpt-6-astra`、`medium` 独立评估结果核对。

| Issue | 当前状态与证据 | 处理建议 |
| --- | --- | --- |
| [#3：时间、停止、完整性反馈](https://github.com/IllyasvielEin/Ciallogcat/issues/3) | 开放；源码支持其中的展示缺口和停止时丢弃机制；报告中的具体丢弃数量尚未独立复现 | 本轮优化依据，优先处理 |
| [#2：活动采集文件查询报 artifact_id 缺失](https://github.com/IllyasvielEin/Ciallogcat/issues/2) | 已关闭；1.3.1 有未完成文件/实际 footer 偏移诊断及回归 | 保持回归，原报告唯一根因未被证明 |
| [#1：筛选历史](https://github.com/IllyasvielEin/Ciallogcat/issues/1) | 已关闭；已有关键词历史及筛选方案 | 本轮未发现新缺口，保留现有行为 |

不能从 #3 推断时间戳保存错误、主机/设备时间差就是应用延迟，或 SIGINT 必须返回 0。反馈主要证明用户难以理解现有信息，以及主动停止时仍有主机侧记录保留空间。

## 已确认的实现边界

- `src/cli/text_output.rs:109`：默认显示主机观察时间，正常 received 行不标来源；device 路径使用 protobuf epoch。没有同时展示两种时间的模式。
- `src/cli/query.rs:196`、`src/cli/query_filter.rs:75`：`--time-basis received|device` 同时决定筛选和当前文本展示。summary 已有 `time_window.basis`；设备范围筛选要求精确 epoch，不推算无年份文本。
- `src/cli/capture.rs:195`：已有停止、ADB 回收与事件排空期限；进入停止时清除 pending，后续 Entry 只累计 received、不写入（约 211、334 行）。这是清理排空与保存尾部记录之间的实际缺口。
- `src/cli/capture.rs:545`：没有实际 failure 时，SIGINT 也会在输出最终 status 后返回 `interrupted`、130；footer 和 completed status 可以与该退出码并存。
- `src/cli/query.rs:703`：`complete` 表示当前查询边界扫完；`artifact_finalized` 还受前缀验证范围约束。`docs/native-format.md:222` 已明确 unknown/partial 与空 known_gaps 的保守含义，说明目前分散在多个位置。

## R1：集中呈现完整性与停止结果（P1，先做）

在 `docs/cli.md`、`docs/agent-query.md` 和相关 CLI help 中放置同一个三层解释：查询是否扫完固定边界、存档是否完成结构验证、设备来源覆盖能否确定。这三层分别回答不同问题。

示例说明应明确：查询完成、存档从头验证并已结束、来源完整性未知、未报告已知缺口可以同时成立；空缺口列表只说明没有记录到已知缺口。

查询 JSON 响应及 text/JSONL 的 stderr summary 增补 `evidence.summary`，为最多 512 个 JSON 编码字节的简短说明，保留现有字段和含义。说明只从现有验证结果推导，依次解释 query boundary、artifact validation、capture completeness 和 known-gap 状态；使用有限模板，不新增另一套相互独立的完整性布尔值。不能只在文档中改措辞而使实际 CLI 仍需跨字段推理。

离线来源已有 footer 的 `known_gap_count`；活动来源若无法得到某项信息，显示 unavailable/unknown，不能将缺失字段当成 0 或空数组，也不从 discarded_entries 或 stream_interruptions 推导 known_gap_count。摘要标明 capture 信息来自 footer 还是 session status，并保留其观察时间；续页读到的 status 可能比固定 snapshot 更新，它不表示该 snapshot 的覆盖结论。footer 元数据可用也不等于查询已完成全前缀验证。当前 session 扫描到 footer 不会自动保存其中的 capture 信息，R1 不应假定已经具备该能力。

tail 跳过非空前缀后，即使 `footer_seen=true`，仍保持 `artifact_finalized=false` 并解释为仅验证后缀；从空前缀开始的 tail 可以最终验证全部记录，不能笼统说所有 tail 都未验证前缀。

前台 capture 保留现有 stdout/stderr 分工及 `interrupted` 错误类型、退出码 130。最终 status 增补可选 `shutdown` 对象，R1 先提供：

- `trigger`：首次停止原因，后续错误不能覆盖。
- `cleanup`：confirmed/unconfirmed，仅说明 owned ADB 进程与读者均无待确认清理；从未启动子进程属于无需清理。不暗示 writer 已结束。
- `footer_write`：confirmed/unconfirmed。只有 footer 确已成功入队、按同一 writer 顺序写入且最终 flush 成功确认，才是 confirmed；未入队、写入失败或等待超时均为 unconfirmed，不能据此声称确定没有 footer。它不代表磁盘持久化、设备无损或查询的全前缀验证。

R3 实施记录保留后再新增可选 `drain=completed|deadline|budget|failed`，限定为停止阶段的主机记录准入结果，多个条件并存按 failed > budget > deadline > completed 归并。completed 仅表示该准入阶段正常结束，不表示设备无损。R1 不提前声称已保存停止尾部；旧 status 缺少 shutdown 或 drain 时均按 unavailable 处理。

真实输出错误、回收超时等仍返回对应失败码并优先于 130；保留最初停止触发原因供诊断。无实际 failure 的 SIGINT 表达为用户取消、清理已完成，不能仅因外层 error envelope 就标记采集故障。

新增查询说明须计入既有响应预算：扫描前预留最长有限模板、状态变化后的最长说明，以及完整 text/JSONL stderr summary envelope 的额外字段，不能只限制说明字符串为 512 字节。最终序列化后仍校验完整响应预算；在预览、`--all` summary、空页和最小预算下验证。不要追加无限诊断文本、完整 gap 数组或向日志 stdout 混入说明。

## R2：分离时间展示与范围筛选（P1）

新增仅适用于 `--format text` 的 `--display-time received|device|both`。省略时保持现有展示时钟选择及输出布局，继续跟随 `--time-basis`；显式指定时独立决定展示，并为每个展示值标注来源。`both` 不属于筛选枚举。

这样旧调用保持兼容，新调用可以用 received 筛选、同时核对设备原文；帮助和文档优先示范带显式来源的模式。旧模式的 stderr summary 增补清楚的展示来源及“主机观察时间，不代表设备事件时间”的说明，明确默认时间来源，而不悄悄修改既有文本前缀。改变默认逐行标签应作为以后单独的兼容性决策。

展示规则：

- received：使用持久化 `observed_unix_ms`，标记为主机观察时间。
- device：有精确 protobuf epoch 则按 `--time-format`/`--timezone` 展示；只有设备时间原文则显示带引号的 `device-text`，明确无绝对时刻，不能推算年份、时区或 epoch。
- both：分别显示 received 和 device/device-text；某一侧缺失时明确 unavailable，不静默用另一时钟冒充。
- 无绝对时刻的 device-text 即使在 epoch/rfc3339 展示请求下也只作为原文附注，不宣称已转换；非日志记录可能两侧均不可用。

summary 的 `text_presentation` 记录请求模式和有效展示策略；同页可混合出现 device epoch、device-text 与 unavailable，不能以单一来源字段宣称每行均有精确设备时间。`time_window.basis` 继续只解释筛选；即使 device-text 可以展示，device 范围筛选仍要求精确 epoch，不得改用 received 或推算年份。展示参数不进入 cursor 过滤指纹，不改变匹配结果、snapshot、record_ref 或存档；同时保留分页时原有筛选参数校验。

预期改动：`src/cli/text_output.rs`、`src/cli/query.rs`、`src/cli/mod.rs`，以及 CLI/Agent 文档。无需调整 native 文件格式、设备传输或 GUI。

## R3：停止时保存可排空的完整记录（P1，独立实施）

用独立的停止原因与记录准入策略代替一个 stopping 布尔值承担全部决策。收到停止后立即关闭重连意图、请求终止 owned ADB；不等待设备继续产生日志。

| 停止原因 | 记录准入策略 |
| --- | --- |
| SIGINT、合作 stop、duration 到期 | 允许在短期限内保存 pending 及随后从后端队列/主机管道排出的完整记录，仍逐条检查条数、字节预算 |
| snapshot_complete | dump 已正常结束；处理已有 pending 后收尾，不新增等待窗口 |
| max_entries、max_bytes | 立即停止新增准入，继续消费事件并报告已观测丢弃；不能突破硬上限 |
| writer、编码、回收等实际 failure | 进入失败清理，继续回收和计数，不把不确定输出包装成成功 |

duration 是请求停止的触发时刻，允许保存主机侧已在途记录，因此不是设备事件时间的严格截止边界。范围必须在 help 和文档中说明。

建议初始使用固定 1 秒的记录准入窗口，包含在现有 5 秒 ADB/读者退出确认期内；实现前以慢写 fixture 核对该值，不增加用户调参项。全部以首次停止的单调时钟为锚，重复信号或后续事件不延长窗口。准入到期或硬预算耗尽后转为仅消费/计数，避免写入背压阻塞退出确认。footer 入队与 writer 等待仍沿用现有各 2 秒的上限；现有流程不是总计 5 秒退出契约，不应如此宣传。

保留停止前的 pending，且只有成功加入 writer 队列才提交对应 encoder 状态和 admitted 计数；有序处理后端事件，保持现有有界队列和单 pending，不能另建无界缓存。主机完整记录不等于设备无损：`stop()` 会终止 ADB，远端或尚未完整读出的内容仍可能丢失。

每轮先检查实际 failure、硬预算和首次停止锚定的准入期限，再尝试 pending 入队。期限届满或禁止准入时，释放未入队 pending，不提交其 encoder 状态或 admitted 计数，并立即恢复后端消费。禁止使用阻塞 send，或等待 writer 完成后才继续消费后端事件。正常结束必须同时满足后端退出已确认和 pending 已处理，不能仅因 active_id 为空跳过 pending；后续限制或故障关闭准入，但不覆盖首次 trigger。

footer 必须在已准入记录之后进入同一 writer 队列，继续预留 4096 字节。footer 的 host_discarded_records 维持 received 减 admitted 的含义；最终 status 的 discarded_entries 维持 received 减 confirmed-written 的含义。writer 故障或超时可能使两者不同，必须解释，不能伪称精确覆盖所有损失。writer 等待超时不会取消该线程，超时后它仍可能写入；最终 entries 是最后一次已确认进度，discarded_entries 是当时未确认写入的数量，不等于永久丢失量。

若因期限而停止接收、丢弃接收器或未收到最终 Exited，未消费队列与设备未读量保持未知；必要时增补“数量未知”的停止/输出缺口，不能只用已消费记录的差值声称丢弃总数。保留 protobuf 半帧、重连、实际失败和硬限制的既有证据。

预期改动主要在 `src/cli/capture.rs` 与 Status；不必重写共享 Controller/ADB 生命周期。若实现中确需修改共享层，追加 GUI/headless 生命周期回归验证。

## 验收与交付顺序

1. R1：集中说明和有界 summary。覆盖 finalized+complete+unknown+空 gaps；活动 session 的 complete 与未结束；非空 tail suffix 已见 footer 但未验证前缀，以及空前缀 tail；活动 snapshot 创建后采集结束再续页；信息不可用、来源观察时间与完整响应预算。SIGINT 明确断言 130、清理与 footer 状态；测试 footer 已入队但最终 flush 失败/超时；真实错误保留原失败语义。
2. R2：覆盖 text/proto、缺失时钟、yearless 原文、epoch/rfc3339、时区/DST/跨年、多行消息、同页混合时钟可用性、单页及 `--all`。device 原文可展示但不能满足精确时间筛选；同一 cursor 只改变展示后保持身份与匹配集合；非 text 输出拒绝展示选项；旧默认行为保持回归。
3. R3：先用可控 writer/后端事件证明 pending、已排队完整记录在可写且预算充足时被保留，并校对身份、顺序、footer 统计。再覆盖慢写/断管、准入到期、条数/字节上限、stop/SIGINT/duration/dump、重复信号、意外退出、半帧及 ADB 回收。特别构造 writer 持续阻塞、后端队列已满、Exited 无法发送的握手 fixture，断言准入到期释放 pending 后恢复消费、总等待有界、footer 不越过已准入记录。使用屏障或 fixture 握手，不依赖随机 sleep 制造队列竞争，也不要求真机 discarded 恒为 0。
4. 运行贡献指南中的受影响 headless 测试及必要的 GUI/Clippy/构建检查。Linux fixture、Windows 原生测试与 Android 真机分别记录；本轮本地核对不代表后两者通过。

方案评估阶段曾执行现有回归：`cargo test --locked --offline --no-default-features --test cli_text_output`（10 项）、前台 SIGINT 回收（1 项）、未完成离线文件/session 前缀（1 项）、footer 实际偏移（1 项），共 13 项通过。当时的 SIGINT 测试允许 0 或 130，不能代替新增契约验收；实施阶段的结果另见末节。

## 方案审核

独立 `gpt-6-astra`、`medium` 对初稿的审核结论为“修正后可实施”，没有要求重写共享生命周期。审核指出两项高优先级设计缺口（准入截止时 pending 的处置、shutdown 的确认语义），以及摘要证据来源/预算和混合时间展示两项补充。

主线程核对相关源码后，已将四项意见整合到本文，并补充对应验收条件：明确释放 pending 的顺序与正常退出条件；使用 footer_write 的 confirmed/unconfirmed；区分 ADB 清理与 writer 完成；标明 status/footer 来源及时点并预留完整响应预算；保留混合时间可用性和严格设备筛选规则。R1 先提供可证实的停止反馈，记录保留结果随 R3 交付。

以上为方案阶段的审核记录，不把原有 13 项测试等同新功能通过。

## 2026-11-06 本地实施与验收

用户随后授权开始优化，R1–R3 已在当前工作树实现：查询新增 evidence.summary，text 新增独立 display-time，停止流程保留有界的主机完整记录并分别确认 cleanup/drain/footer_write。默认文本布局、SIGINT 130、查询身份与存档格式保持既有契约。

独立 astra-medium 实现审核未发现可确认的正确性、死锁或协议兼容缺陷，指出两处主循环验收缺口。主线程随后将信号注册与实际采集循环分开，使用可控 backend/writer 运行同一个生产循环，补充了队列满时的准入截止、停止窗口内条数/字节限制、dump/stop 并发，以及实际 footer flush 失败/超时的回归。补充用例均通过，失败输出优先于 130，footer 未确认时不会声称写入完成。

完整自动检查、构建及平台范围记于 [验证状态](validation.md#2026-11-06-cli-反馈优化)。该阶段只完成本地实现；用户随后授权推送并以 CLI-only 1.3.2 发布，范围及说明见 [发布指南](release.md)。
