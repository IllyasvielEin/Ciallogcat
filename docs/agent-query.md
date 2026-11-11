# Agent 查询协议

`query`、`get`、`context` 共用查询 API，分别取代原 `query-v2`、`get-v2`、`context-v2`；旧序号查询实现与旧命令名均已移除，不保留别名。输入仅支持原生离线文件或会话（包括活动会话）；不需要 GUI，也不会调用 ADB。存储 v1、旧展开 schema-2、旧 legacy 导入产物和空文件不再支持，合法的零记录 native 文档仍可读取。

原 `query-v2` 调用应更改命令名并重新查询，取得当前版本的 cursor/ref；旧 cursor 和旧签名密钥不再兼容。原 `query` 调用还需迁移响应：不再接受 `--after-seq`；从 schema 1 的完整 `entries` 改为 schema 2 的 `records` 预览，以 `next_cursor` / `--cursor` 分页，`--limit` 上限从 10000 改为 1000。旧数字序号不能转换为签名 cursor。`--tag-include` 已移除，使用 `--tag`。完整字段、边界和错误语义变化见 [迁移说明](cli.md#migrating-query-callers)。

```sh
ciallogcat-cli query --session-dir ./capture --mode snapshot --query 'timeout'
ciallogcat-cli query --session-dir ./capture --mode snapshot --query 'timeout' --cursor '<next_cursor>'
ciallogcat-cli get --session-dir ./capture --ref '<record_ref>'
ciallogcat-cli context --session-dir ./capture --ref '<record_ref>' --before 5 --after 5
ciallogcat-cli query --file ./export.jsonl
# 建立动作前的主机发布边界；续查沿用返回的 cursor 和相同 query。
ciallogcat-cli query --session-dir ./capture --mode tail --start-at end --query 'timeout'
```

每个命令必须提供 `--file PATH` 或 `--session-dir DIR`，且只能选择一个。默认查询字面量只匹配 message，区分大小写；`--regex`、`--ignore-case` 显式改变语义。单个文本过滤条件最多 4096 UTF-8 字节。新增 Tag/package/level/PID/TID、时间范围及完整流式输出见 [CLI 查询指南](cli.md#recommended-query-workflow)。CLI 值参数采用 `--name value` 形式，`--all`、`--regex`、`--ignore-case`、`--case-sensitive` 为无值开关。

省略 `--min-level` 与显式 `V`/`verbose` 具有相同语义和过滤指纹：没有等级限制，消息过滤可匹配 event/unparsed。非空 package、Tag 包含/排除、PID/TID 或比 V 更严格的等级条件均排除这些没有日志元字段的记录，不以内部占位值匹配。

`ciallogcat-cli --help` 和三个子命令各自的 `--help` / `-h` 均提供参数默认值、闭区间范围、预算适用性和续页说明。帮助输出为 stdout 纯文本，无需来源参数，不打开文件或创建查询密钥。

## 查询、边界和状态

`query` 参数：

| 参数 | 默认值 | 边界 |
| --- | --- | --- |
| `--mode` | `snapshot` | `snapshot` 或 `tail`；tail 仅支持 session-dir；提供 `--cursor` 时默认取 cursor 记录的模式，`--start-at end` 隐含 tail；显式值须一致 |
| `--start-at` | `beginning` | `beginning` 或 `end`；end 仅支持 native session-dir 的 tail；显式起点不能与 cursor 同传 |
| `--cursor` | 从第一条开始 | 后续请求提供上一页 next_cursor |
| `--query` | 空字符串 | 同一 cursor 必须保持条件不变 |
| `--limit` | 100 | 1–1000 条返回记录 |
| `--max-response-bytes` | 65536 | 8192–1048576，包含 JSON envelope 和最后换行 |
| `--preview-bytes` | 512 | 0–16384，UTF-8 字节上限，不截断码点 |
| `--wait-ms` | 0 | 0–60000；仅 tail，见下文“标记与等待” |
| `--max-scan-bytes` | 16777216 | 8388609–67108864 |
| `--max-scan-records` | 100000 | 1–1000000 |

上表的响应预算适用于三个命令；预览和扫描预算仅适用于 query/context。`get` 只接受 `--max-response-bytes` 控制响应分块，明确拒绝过去接受但不使用的 `--preview-bytes`、`--max-scan-bytes`、`--max-scan-records`。扫描字节不是文件总 I/O 硬上限，来源校验、控制元数据和缓冲读取还有额外开销，实际日志读取量见 `file_bytes_read`。

snapshot 的首个请求固定 `{seq, offset}` 上界，后续页不会混入并发追加的记录。tail 的每次请求读取新的已发布水位，但一次请求内部上界仍固定。查询不会等待新数据；没有新增时返回空 records 和可再次使用的 cursor。

snapshot 到达上界后，要查看后续追加需省略 cursor 创建新快照；tail 到达当前上界后可稍后复用 next_cursor。两种模式默认从来源开头开始。续页必须保持来源、模式、过滤条件不变，可以调整 limit 和预算；返回条数少于 limit 不代表读取完成。

`--mode tail --start-at end` 在一次请求观察到的已发布 `{seq, offset}` 建立位置，返回空 records、`has_more=false`、逻辑扫描 bytes/records 均为 0 和稳定 cursor；不追赶正在增长的历史。后续省略 start-at，使用该 cursor 才读取新发布记录。没有新发布时 cursor 不变。仅支持 native session-dir（运行中或已停止均可），要求 status 有成对发布的 visible_end 和 entries；离线文件、snapshot 或同时传 cursor 均报 `invalid_arguments`；旧存储来源不再支持。header 未发布报 `source_not_ready`；只有 header 的空会话合法。非法/截断的已发布末行、错误的 seq/offset/count 或 footer 不会被略过。尚未发布的字节不参与起点定位。

尾部起点最多读取首条、末条及可选 footer，各遵守 8 MiB 编码上限；至多两次反向定位各限 8 MiB + 1 字节，加上 header、固定前缀/anchor 和缓冲余量，读取量不随历史条数增长。这些来源验证读取不占逻辑扫描预算，仍全部计入 `scan.file_bytes_read`。不会把尚未扫描的中间前缀声称为验证通过。

活动会话水位来自原子发布的 status 文件。原生会话使用 `visible_end: {seq, offset}`，同时验证 `record_schema_version`、`artifact_id`、`session_id`。offset 包含 header/footer 字节，seq 仅计公共记录。完整一行 flush 后才可推进水位；文件里已经出现但水位尚未覆盖的字节不会被读取为记录。这个保证是进程间可见性，不是逐条 fsync 或断电耐久性。

响应 `schema_version: 3` 是查询 API 版本（query、crashes、get、context 共用；3 起记录预览改为精简字段），独立于命令名和存档版本。这些命令都返回固定内部来源标记 `source_schema_version: 3`，不是用户可选格式；protobuf 摘要和消息过滤使用固定 `aosp_logcat_16r1_host_v1` 投影，与 ADB adapter 和共享文档 codec 相同。

响应的主要字段：

- `records`：精简的有界预览。每条总有 `record_ref`、`seq`、`message`（预览）；日志记录另有 `observed_unix_ms`（主机接收时间，由 adapter 在队列背压等待前采样，不是设备事件时间）、`time`（设备时间原文）、`pid`、`tid`、`level`、`tag`。只在有意义时出现：`kind`（非 entry 时，即 event/unparsed）、`process`/`process_source`（有进程名时）、`preview_truncated: true` 与 `message_bytes`（预览被截断时）、`text_loss`（记录了截断或非法 UTF-8 替换时）、`metadata_preview_truncated: true`、`session_id`（仅当记录来自与文档头不同的采集会话）。完整身份（session、segment、attempt）及全部存档字段用 `get` 取得。缺失的可选观察不会被补造；原生 event/unparsed 不添加虚构的 PID、等级、Tag。`source_status` 省略值为 null 的观察。
- `complete`：当前 snapshot/tail 水位已扫完；不代表采集结束或设备日志无缺失。`--all` 固定快照（tail 模式下固定本次请求观察到的水位）并自动续页（包括空匹配页），完整 text/JSONL 输出到 stdout，最终摘要到 stderr；内存不累计全量匹配。
- `warnings`：仅在需要时出现。`--package` 生效而扫描到没有进程名的日志记录时返回 `{code:"package_unresolved", records:N, message}`；这些记录（如历史行、两种来源都未命中的进程、旧采集）的零结果不能说明应用没有日志，应改用 `--pid` 或 `--tag`。进程名来源见 [CLI 指南](cli.md#agent-loop)。
- `next_cursor`、`has_more`：是否还有本次上界内的字节。空 records 也可能 has_more=true，例如本页扫描的记录都不匹配。每次都用返回的新 cursor 继续。
- `snapshot`：本次固定的可见序号/字节边界。
- `source_status`：本次读取的状态观察，含其更新时间与 stale 标志、readiness 与错误/诊断各最多 96 UTF-8 字节的预览及截断标志；它不等于 snapshot 创建时的采集状态。native 来源提供已经校验的 artifact_id/session_id。原生会话还提供 capture_completeness、stream_interruptions 和最后流错误预览；离线 native 文件提供 footer 的采集观察及 known_gap_count，不能把它视为当前设备健康状态。需要完整诊断时使用 `status`。
- `artifact_finalized`：所有记录已从头连续扫描且 footer 与累计统计相符。尾部起点跳过非空历史后，即使读到合法 footer，此字段仍为 false；从空会话起步则可以在后续完整扫描后为 true。无论是否 finalized，都不据此推断完整采集。
- `validation`：`scope` 为 `full_prefix` 或 `tail_suffix`，表示该 cursor 的扫描范围，不代表尚未扫描的后续页已验证。`start` 保存显式尾部起点，否则为 null；`skipped_records` 来自该起点的 status.entries，**不是扫描验证所得**。`footer_seen` 表示读到且检查了 footer。tail_suffix 仍校验 footer 结构/身份/范围、首末记录、已发布历史 count 加实际扫描后缀 count；这些检查不替代跳过前缀的逐条验证。计数与 seq 分开，不假设 seq 连续。
- `scan`：逻辑扫描字节/记录数、起止 offset、last_seq、实际日志文件读取字节和停止原因。`file_bytes_read` 包含 header、footer、身份前缀、边界 anchor 及缓冲读入；不包含 status/key 文件的固定控制读取。
- `evidence.summary`：query 响应和流式 stderr summary 的有界说明，集中解释查询水位、存档验证和来源覆盖。`complete=true`、`artifact_finalized=true`、capture completeness 为 unknown、known_gap_count 为 0 可以同时成立；空缺口列表不证明无损。标明 footer/session status 的来源及观察时间；当前 session status 不等于固定 snapshot 的覆盖情况。缺失的 gap 信息按 unavailable 处理，不从其他计数推算。

扫描为下一条最大 8 MiB 编码记录预留空间，因此可能在字节预算用满前停止。默认响应只含摘要，Tag、设备时间和 process 名称另有小预览限制；`metadata_preview_truncated` 表明这些字段被裁短或省略。首条大记录会缩小预览，必要时保留最小引用信息，并推进 cursor，避免卡在同一条。响应预算不足以容纳 envelope 和引用时明确报参数错误。

## 标记与等待

`--start-at end`（隐含 tail 模式）返回的 cursor 是一个**标记**：它尚未扫描任何记录（count=0 且位于尾部起点），只绑定来源、位置和 tail 模式，不绑定过滤条件。续查时可以使用任意过滤条件（包括时间过滤，锚点取本次请求时间），也可以反复用于多个查询和 `crashes`。一旦 cursor 扫描过记录，就重新绑定其过滤条件，改条件报 `cursor_mismatch`。snapshot 模式仍拒绝标记。

`--wait-ms N` 仅用于 tail。本页没有匹配记录且已扫到当前水位时，按约 200 ms 间隔重新读取已发布水位并续扫，直到出现匹配、来源结束（terminal、stale 或已见 footer）或到期。响应增加 `wait: {requested_ms, waited_ms, stop_reason}`，stop_reason 为 `matched`、`has_more`、`source_ended`、`deadline` 之一。每次轮询分别受扫描预算约束；返回的 cursor 可直接用于下一次等待。text/JSONL 输出同样支持，`--all` 可与 tail 组合，输出标记之后到本次水位的全部记录。

## 崩溃分组

`crashes` 与 `query` 共用来源、`--mode`、`--cursor`、`--start-at` 和全部过滤参数，在一次有界扫描内识别：Java（`AndroidRuntime` 的 `FATAL EXCEPTION:`）、native（`libc` 的 `Fatal signal`、`DEBUG` 的 tombstone）、ANR（`ActivityManager` 的 `ANR in`）。后续行按写入 PID 和 Tag 归属到打开的事件，400 条无后续即关闭；多行单条消息（proto）按行处理。

每组返回 `kind`、`signature`、`occurrences`、`first_seq`/`last_seq`、`first_ref`、最多 4 个 `pids`、`process`、`thread`、`headline`、`cause` 和最多 5 个 `frames`。Java 签名取最深 `Caused by` 的类名及其首帧；native 取信号与首个非 libc 帧（去掉偏移）；ANR 原因中的数字折叠为 `#`。签名是导航线索，不是根因判断；用 `first_ref` 调用 `get`/`context --same-pid` 取原始证据。`--limit` 默认 20、最多 200，按 first_seq 排序，超出响应预算时先丢弃后面的组并标记 `groups_truncated`。`has_more=true` 表示只覆盖已扫描范围，组不会跨调用合并；crashes 的 cursor 只能继续 crashes 扫描。

## 全文和上下文

`get (--ref TOKEN | --seq N) [--view decoded|stored] [--text-offset N] [--max-response-bytes N]` 返回 `record_json_chunk`，编码为 `json_utf8`。`--seq N` 在来源当前可见边界（会话的已发布水位或离线文件 footer 前）内按字节偏移二分定位记录，每步只读一行，读取量随记录数对数增长；找不到报 `record_not_found`。`context` 同样接受 `--seq`。默认 `--view decoded`（见下文）；text 传输的记录 decoded 与 stored 逐字节相同，proto 记录的 decoded 因同时带派生文本与原始 base64 约为 stored 的 2–3 倍，但每次响应仍受 `--max-response-bytes` 约束并分块返回。`--view stored` 返回存档原始单条 JSON 字节流，保留未知字段、空白和字段顺序，只去掉末尾 LF；protobuf 原行只有 raw payload，不含 message 等派生字段。

`--view decoded` 返回完整结构化 JSON：message 不做预览截断，包含时间、PID/TID、等级、tag、process、身份、text_loss 和 proto 全部已知标量/bytes（base64）。原生格式使用 header 声明的固定投影，并将 device_time/process 扩展还原到对应父字段；其余未知扩展也保留，包括未知 projection_extensions 子字段。响应标记 `view`、`derived`，proto decoded 还标记 `projection_profile`。这是**派生视图，不是存档原字节**，其中的 schema_version 标识来源，不可把展开 JSON 当合法 compact 存档行写回；需要原始证据应取 stored 视图，不将 decoded 结果当作存档写回。

第一块 offset 为 0，随后使用 `next_text_offset`，直到 `complete=true`。拼接所有 `record_json_chunk` 字符串后再解析 JSON；保持来源、ref 和 **view 不变**。`text-offset` 是所选视图的 UTF-8 字节偏移，必须处于码点边界。stored 范围为 0–8388608；decoded 范围为 0–67108864，均不能超过该视图实际长度。`record_bytes` 是该视图的完整字节数。

每次 get 校验引用绑定的原始记录摘要和共享 codec 约束，不扫描该记录之前的日志。存档行仍最多 8 MiB；compact 原帧中的 NUL/转义或非法 UTF-8 展开可能使 decoded JSON 超过 8 MiB，因此派生视图单独受 64 MiB 编码预算限制。每次分块会重新构建该单条视图，没有整文件转换或副本存档。`--max-response-bytes` 继续限制包括转义、envelope 和 LF 的每次响应；preview/scan 参数不控制 get 分块，来源元数据验证仍有额外 I/O。

`context --ref TOKEN [--before 10] [--after 10]` 返回以原记录为中心的有序预览。before/after 各为 0–100。它固定在 reference 创建时的 snapshot 内，不混入后续日志。向前查找使用有界反向换行扫描，不从文件头重扫。字节、记录或响应预算不足时保留目标记录，并设置 `partial_before` / `partial_after`；到达文件边界则不是 partial。上下文中的每条记录也有独立引用。没有暗中扩展查询分页来携带上下文。

`--same-pid` 只保留与目标同 PID 的邻近记录（例如堆栈后续行）；其他记录仍在预算内扫描但跳过，数量见 `skipped_other_pid`，目标须为日志记录。

上下文不沿用原 query 的消息过滤；前后两侧共用响应、扫描预算，目标记录也计入。先取 before，再取 after，反向查找换行的读取也计入扫描字节。`--max-scan-records 1` 因此只允许目标记录。上下文没有续页 cursor；需继续取证时使用返回的 record_ref。

## 管理性关闭与 text 展示

`stop --force` 在合作停止等待后仍未确认 worker 终态时，可创建独立不可变的管理关闭记录。
状态 `abandoned` / `worker_exit_confirmed=false` 表示管理上结束，并不证明 worker 或 ADB 已退出。
`--session-dir` 查询固定在关闭时已发布的前缀；旧 worker 后续更新 status 或追加日志不会扩大边界。
边界内已有 token 仍可使用，超出关闭边界的旧 snapshot/ref 明确拒绝。原生 session 不要求补造 footer；
`--file` 仍按原有离线文件规则处理。详情与兼容限制见 [CLI 指南](cli.md)。

`--format text` 默认展示 readable/local，时间来源与 `--time-basis` 一致；默认 received 不逐行重复，device 及异常回退仍标注，summary 始终声明 basis。
`--display-time received|device|both` 可独立选择带来源标签的展示，不改变筛选。received 是主机观察时间，不代表设备事件时间；device 优先使用精确 protobuf epoch，否则只展示标注未转换的 device-text 原文或 unavailable。both 分别显示两侧；同页不同记录可能使用不同的设备时间展示路径。无年份原文即使能展示，也不能用于要求精确 epoch 的 device 时间筛选，不推算年份、不改用 received。
`--time-format epoch|rfc3339`、`--timezone utc` 可显式选择；纯展示参数不进入 cursor 过滤指纹。
缺少绝对时间时仅展示明确未知与原始设备文本，不猜年份/时区；时间筛选仍按原有严格缺失规则报错。
event/unparsed 用明确类型标签，不展示伪造的日志元字段；仍保留所选时钟前缀和完整 message。
只在 message 没有末尾 LF 时追加一个，不移除内部换行、已有 CRLF 或有意的多个末尾换行。
JSON/JSONL、归档字节和固定 protobuf 投影不受这些 text 选项影响。
进程名默认限 32 个显示列，以中间省略号保留前后缀；`--process-width 0` 恢复全名。
此行为不依赖 TTY，保留字素簇、PID/TID 和完整 message，不能把缩写作为唯一进程身份。
text summary 的 `text_presentation` 报告请求的展示模式、有效展示策略、列宽和实际缩短进程名的记录数；省略 display-time 时保留原布局。展示参数不进入 cursor 指纹。

前台 SIGINT 保留 `interrupted` 与退出码 130，最终 status 的 `shutdown` 分别报告首次 trigger、ADB/readers cleanup、主机记录准入 drain、footer_write 确认。主动停止最多短暂保存一秒内可排空的完整主机记录，仍遵守硬预算；实际错误优先于取消码。footer_write 为 unconfirmed 只表示尚未确认写入及最终 flush，不表示确定缺少 footer；退出超时后的 discarded_entries 是最后观察时未确认写入的数量。完整说明见 [CLI 停止与退出契约](cli.md#json-and-exit-codes)。

每个 query page / stream summary（包括零匹配）都返回 `time_window`：`basis`、
`anchor_unix_ms`、`since_inclusive`、`until_exclusive`。边界值含精确十进制字符串
`unix_ns` 和 UTC `rfc3339`；超出日历范围时后者为 null。空边界表示未指定，
并不是 anchor 或 snapshot 末尾；无时间筛选时 anchor 为 null。相对时间沿用 cursor 的原始锚点。
它描述解析后的筛选条件，不表示实际数据覆盖范围，也不能据此推断零结果的唯一原因。


## Cursor 和 reference 的有效性

token 使用本机持久 HMAC-SHA256 密钥认证。密钥在 GUI 配置目录的 `query.key`，支持 `CIALLOGCAT_CONFIG_DIR`；不读取或迁移旧 `query-v2.key`。升级后重新查询取得 cursor/ref，不手动改名密钥或修改 token 来绕过重建。Unix 新建权限为 0600，完整写入后才发布。并发首次调用共享同一个获胜的密钥。不要将私钥加入日志或版本控制。删除密钥会使当前 token 失效；token 不是可跨主机传递的证据文件，跨工具交接应使用 native JSONL 与其 session_id/seq。

cursor 绑定规范化源路径、文件身份（Unix dev/inode；Windows volume/file index）、source/schema、固定前缀摘要、模式、过滤条件、offset、last_seq、扫描统计及上界。换源、换过滤、换模式、token 恶改、文件替换、缩短或已观察边界改变都会报错，不会悄悄从头重查。record_ref 还绑定记录本身的完整摘要；get/context 的目标校验可以发现其原地修改。

token 形如 `kind.base64url(payload).base64url(tag)`：payload 为 JSON，tag 为截断到 128 位的 HMAC-SHA256。payload 不再携带完整来源绑定，只保存 schema、session、prefix_len 和绑定摘要；校验时从当前来源重算绑定并比较摘要，任何路径、文件身份、前缀、schema 或 artifact 变化都报 `stale_cursor`。所有起点统一签发/接受 `c5`（version=5）cursor；旧 `c4` 及更早 cursor 不再接受，必须省略 cursor 重新查询。过滤条件统一使用结构化规范编码计算指纹，不再保留旧字面量专用分支。end 起点仍将未扫描历史的边界、发布条数和首条 seq 单独保存；count/first_seq 仅记录实际扫描的后缀，不把跳过的历史冒充已验证内容。record_ref 为单一 `r3`（version=3）结构，旧 `r2` 拒绝；与响应 schema 或存档版本互不替代。调用方始终把 token 当不透明字符串。尾部 cursor 到达新水位时还检查发布 entries 与累计后缀条数一致；已观察 footer 后拒绝任何新的发布追加。

性能前提是源文件只追加且不修改已观察前缀。每页的历史一致性摘要只覆盖固定大小前缀和边界 anchor，不哈希整个历史文件；有界 header/footer 的格式与身份校验另行执行。因此不声称发现任意位置、同大小的外部原地篡改。相同用户能够修改源和私钥，这也不是该用户权限内的安全隔离边界。

原生查询的 header、footer 和单条数据记录统一使用共享 codec 的 **8 MiB 编码上限（包含末尾换行，如有）**，`--file` 与 `--session-dir` 使用相同上限。header 的 source 扩展（包括完整 device_diagnostics 报告）和其他未知字段也计入，不裁短有效字段。每次打开来源（包括续页、get/context）都重新有界读取并校验 header，最多取 8 MiB + 1 字节来判断超限；实际读取还可能有有界缓冲预读，session 始终受已发布 offset 限制。header 不计入逻辑扫描字节，但计入 `file_bytes_read`，因此大 header 会增加每次请求的来源校验开销。

离线原生文件必须有合法 footer 才开始查询；来源校验从文件尾最多反向检查 8 MiB + 1 字节（额外一字节用于确认前一行的换行分隔符），再有界读取并校验 footer，不重扫历史记录。这些读取同样计入 `file_bytes_read`，属于扫描预算之外的来源校验开销；顺序扫描经过 footer 时，其完整编码长度仍计入扫描字节，并校验累计记录统计。离线原生文件允许合法 footer 没有末尾换行，超限或截断的 header/footer 则拒绝。

session-dir 始终只读取已发布前缀，包括失败或终止会话，因此允许尚无 footer，但水位必须结束于完整换行；尚未发布的 footer 字节不会作为记录读取。session 查询成功不等于整份文件通过离线 artifact 验证。离线原生文件必须有合法 footer；零字节文件不是空 native 文档。

错误继续使用 CLI 的 JSON stderr 错误 envelope，code 包括 `unsupported_format`、`invalid_token`、`cursor_mismatch`、`stale_cursor`、`invalid_source`、`source_not_ready`、`invalid_arguments`。自动化应据 code 决定重试或重新打开源，不应解析自然语言 message。

## 验证范围

`tests/query_integration.rs` 运行真实 CLI，隔离源文件和密钥目录，所有命令有期限。测试包含固定 snapshot、发布水位之外的追加、无新 tail、深页读取字节断言、巨大首条与全文分块、UTF-8 边界、token 恶改、替换/截断、上下文局部读取、原生活动/离线格式和错误水位。header/footer 回归覆盖原生 file/session 入口、超过 64 KiB 的合法扩展、恰好 8 MiB、超限一字节、截断，以及分页、get/context、活动水位与游标校验；大 header 还验证既有身份前缀摘要，footer 另覆盖离线无末尾换行。读取量使用实际 Read 计数，区分逻辑扫描和来源校验开销，避免把共享 runner 的毫秒抖动当成性能回归。

end 起点回归另覆盖空会话、发布后的追加、稀疏 seq、过滤和旧 token 拒绝、footer 前后建立起点、非法组合、过期边界/身份/计数，以及 30000 条历史的有界实际 I/O。中间前缀故意损坏的案例验证尾部 cursor 不会冒充全文验证；从头扫描仍会拒绝该损坏。

原生回归覆盖活动/离线来源、proto tail 后缀与稀疏序号、原行与 decoded 完整 UTF-8 分块、所有扩展容器、超过 8 MiB 的 NUL 派生视图，以及错误 profile、混合 schema、竞争字段和损坏 payload 拒绝。两种传输共用 footer 上限与水位回归。`examples/agent_capture.py` 直接使用当前原生会话，不选择存储版本。
