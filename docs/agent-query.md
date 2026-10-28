# Agent 查询协议 v2

`query-v2`、`get-v2`、`context-v2` 是显式的新命令。旧 `query` 的 v1 响应不变。输入可以是已有 v1 CLI 会话、v1 离线 JSONL、原生 v2/v3 离线文件或 v2/v3 活动会话；不需要 GUI，也不会调用 ADB。旧 `query` 遇到有效 v2/v3 文档头时，返回 stderr JSON 错误 `unsupported_format`、退出码 3，并提示改用同一目录的 `query-v2 --session-dir DIR`；这不代表文件损坏。

```sh
ciallogcat-cli query-v2 --session-dir ./capture --mode snapshot --query 'timeout'
ciallogcat-cli query-v2 --session-dir ./capture --mode snapshot --query 'timeout' --cursor '<next_cursor>'
ciallogcat-cli get-v2 --session-dir ./capture --ref '<record_ref>'
ciallogcat-cli context-v2 --session-dir ./capture --ref '<record_ref>' --before 5 --after 5
ciallogcat-cli query-v2 --file ./export.jsonl
# 建立动作前的主机发布边界；续查沿用返回的 cursor 和相同 query。
ciallogcat-cli query-v2 --session-dir ./capture --mode tail --start-at end --query 'timeout'
```

每个命令必须提供 `--file PATH` 或 `--session-dir DIR`，且只能选择一个。查询字面量只匹配 message，区分大小写，不解释正则。最多 4096 UTF-8 字节。CLI 参数采用 `--name value` 形式。

`ciallogcat-cli --help` 和三个子命令各自的 `--help` / `-h` 均提供参数默认值、闭区间范围、预算适用性和续页说明。帮助输出为 stdout 纯文本，无需来源参数，不打开文件或创建查询密钥。

## 查询、边界和状态

`query-v2` 参数：

| 参数 | 默认值 | 边界 |
| --- | --- | --- |
| `--mode` | `snapshot` | `snapshot` 或 `tail`；tail 仅支持 session-dir |
| `--start-at` | `beginning` | `beginning` 或 `end`；end 仅支持 native v2/v3 session-dir 的 tail；显式起点不能与 cursor 同传 |
| `--cursor` | 从第一条开始 | 后续请求提供上一页 next_cursor |
| `--query` | 空字符串 | 同一 cursor 必须保持条件不变 |
| `--limit` | 100 | 1–1000 条返回记录 |
| `--max-response-bytes` | 65536 | 8192–1048576，包含 JSON envelope 和最后换行 |
| `--preview-bytes` | 2048 | 0–16384，UTF-8 字节上限，不截断码点 |
| `--max-scan-bytes` | 16777216 | 8388609–67108864 |
| `--max-scan-records` | 100000 | 1–1000000 |

上表的响应预算适用于三个命令；预览和扫描预算适用于 query/context。`get-v2` 为兼容共用参数解析也接受并校验预览/扫描参数，但它们不控制取回全文时的读取量或分块大小。扫描字节不是文件总 I/O 硬上限，来源校验、控制元数据和缓冲读取还有额外开销，实际日志读取量见 `file_bytes_read`。

snapshot 的首个请求固定 `{seq, offset}` 上界，后续页不会混入并发追加的记录。tail 的每次请求读取新的已发布水位，但一次请求内部上界仍固定。查询不会等待新数据；没有新增时返回空 records 和可再次使用的 cursor。

snapshot 到达上界后，要查看后续追加需省略 cursor 创建新快照；tail 到达当前上界后可稍后复用 next_cursor。两种模式默认从来源开头开始。续页必须保持来源、模式、过滤条件不变，可以调整 limit 和预算；返回条数少于 limit 不代表读取完成。

`--mode tail --start-at end` 在一次请求观察到的已发布 `{seq, offset}` 建立位置，返回空 records、`has_more=false`、逻辑扫描 bytes/records 均为 0 和稳定 cursor；不追赶正在增长的历史。后续省略 start-at，使用该 cursor 才读取新发布记录。没有新发布时 cursor 不变。仅支持 native v2/v3 session-dir（运行中或已停止均可），要求 status 有成对发布的 visible_end 和 entries；v1、离线文件、snapshot 或同时传 cursor 均报 `invalid_arguments`。header 未发布报 `source_not_ready`；只有 header 的空会话合法。非法/截断的已发布末行、错误的 seq/offset/count 或 footer 不会被略过。尚未发布的字节不参与起点定位。

尾部起点最多读取首条、末条及可选 footer，各遵守 8 MiB 编码上限；至多两次反向定位各限 8 MiB + 1 字节，加上 header、固定前缀/anchor 和缓冲余量，读取量不随历史条数增长。这些来源验证读取不占逻辑扫描预算，仍全部计入 `scan.file_bytes_read`。不会把尚未扫描的中间前缀声称为验证通过。

活动会话水位来自原子发布的 status 文件。v2/v3 使用 `visible_end: {seq, offset}`，同时验证 `record_schema_version`、`artifact_id`、`session_id`。offset 包含 header/footer 字节，seq 仅计公共记录。v1 会话使用已有、成对发布的 entries/bytes。完整一行 flush 后才可推进水位；文件里已经出现但水位尚未覆盖的字节不会被读取为记录。这个保证是进程间可见性，不是逐条 fsync 或断电耐久性。

命令名及响应 `schema_version: 2` 是查询 API 版本，与存档版本分开。三个命令都返回 `source_schema_version: 1|2|3`；v3 的摘要和消息过滤使用固定 `aosp_logcat_16r1_host_v1` 投影，与 ADB adapter 和共享文档 codec 相同。

响应的主要字段：

- `records`：有界预览、`record_ref`、seq、kind 和已知的稳定身份。v1 的身份范围为 `artifact_local`，不补造 capture session、process 来源或文本损失信息。原生 event/unparsed 不添加虚构的 PID、等级、Tag。
- `next_cursor`、`has_more`：是否还有本次上界内的字节。空 records 也可能 has_more=true，例如本页扫描的记录都不匹配。每次都用返回的新 cursor 继续。
- `snapshot`：本次固定的可见边界；离线 v1 没有预先可信的最后 seq，seq 为 null。
- `source_status`：本次读取的状态观察，含其更新时间与 stale 标志、readiness 与错误/诊断各最多 96 UTF-8 字节的预览及截断标志；它不等于 snapshot 创建时的采集状态。native 来源提供已经校验的 artifact_id/session_id。v2/v3 会话还提供 capture_completeness、stream_interruptions 和最后流错误预览；离线 native 文件提供 footer 的采集观察及 known_gap_count，不能把它视为当前设备健康状态。需要完整诊断时使用 `status`。
- `artifact_finalized`：所有记录已从头连续扫描且 footer 与累计统计相符。尾部起点跳过非空历史后，即使读到合法 footer，此字段仍为 false；从空会话起步则可以在后续完整扫描后为 true。v1 没有 footer，始终不据此推断完整采集。
- `validation`：`scope` 为 `full_prefix` 或 `tail_suffix`，表示该 cursor 的扫描范围，不代表尚未扫描的后续页已验证。`start` 保存显式尾部起点，否则为 null；`skipped_records` 来自该起点的 status.entries，**不是扫描验证所得**。`footer_seen` 表示读到且检查了 footer。tail_suffix 仍校验 footer 结构/身份/范围、首末记录、已发布历史 count 加实际扫描后缀 count；这些检查不替代跳过前缀的逐条验证。计数与 seq 分开，不假设 seq 连续。
- `scan`：逻辑扫描字节/记录数、起止 offset、last_seq、实际日志文件读取字节和停止原因。`file_bytes_read` 包含 header、footer、身份前缀、边界 anchor 及缓冲读入；不包含 status/key 文件的固定控制读取。

扫描为下一条最大 8 MiB 编码记录预留空间，因此可能在字节预算用满前停止。默认响应只含摘要，Tag、设备时间和 process 名称另有小预览限制；`metadata_preview_truncated` 表明这些字段被裁短或省略。首条大记录会缩小预览，必要时保留最小引用信息，并推进 cursor，避免卡在同一条。响应预算不足以容纳 envelope 和引用时明确报参数错误。

## 全文和上下文

`get-v2 --ref TOKEN [--view stored|decoded] [--text-offset N] [--max-response-bytes N]` 返回 `record_json_chunk`，编码为 `json_utf8`。默认 `--view stored` 保持原有行为：返回存档原始单条 JSON 字节流，保留未知字段、空白和字段顺序，只去掉末尾 LF。v3 proto 原行只有 raw payload，不含 message 等派生字段。

显式 `--view decoded` 返回完整结构化 JSON：message 不做预览截断，包含时间、PID/TID、等级、tag、process、身份、text_loss 和 proto 全部已知标量/bytes（base64）。v3 使用 header 声明的固定投影，并将 device_time/process 扩展还原到对应父字段；其余未知扩展也保留，包括未知 projection_extensions 子字段。v1 decoded 重编码完整原 JSON，不补造原生字段。响应标记 `view`、`derived`，proto decoded 还标记 `projection_profile`。这是**派生视图，不是存档原字节**，其中的 schema_version 标识来源，不可把展开的 v3 JSON 当合法 compact 存档行写回；需要格式转换应使用共享文档转换接口。

第一块 offset 为 0，随后使用 `next_text_offset`，直到 `complete=true`。拼接所有 `record_json_chunk` 字符串后再解析 JSON；保持来源、ref 和 **view 不变**。`text-offset` 是所选视图的 UTF-8 字节偏移，必须处于码点边界。stored 范围为 0–8388608；decoded 范围为 0–67108864，均不能超过该视图实际长度。`record_bytes` 是该视图的完整字节数。

每次 get 校验引用绑定的原始记录摘要和共享 codec 约束，不扫描该记录之前的日志。存档行仍最多 8 MiB；compact 原帧中的 NUL/转义或非法 UTF-8 展开可能使 decoded JSON 超过 8 MiB，因此派生视图单独受 64 MiB 编码预算限制。每次分块会重新构建该单条视图，没有整文件转换或副本存档。`--max-response-bytes` 继续限制包括转义、envelope 和 LF 的每次响应；preview/scan 参数不控制 get 分块，来源元数据验证仍有额外 I/O。

`context-v2 --ref TOKEN [--before 10] [--after 10]` 返回以原记录为中心的有序预览。before/after 各为 0–100。它固定在 reference 创建时的 snapshot 内，不混入后续日志。向前查找使用有界反向换行扫描，不从文件头重扫。字节、记录或响应预算不足时保留目标记录，并设置 `partial_before` / `partial_after`；到达文件边界则不是 partial。上下文中的每条记录也有独立引用。没有暗中扩展查询分页来携带上下文。

上下文不沿用原 query 的消息过滤；前后两侧共用响应、扫描预算，目标记录也计入。先取 before，再取 after，反向查找换行的读取也计入扫描字节。`--max-scan-records 1` 因此只允许目标记录。上下文没有续页 cursor；需继续取证时使用返回的 record_ref。

## Cursor 和 reference 的有效性

token 使用本机持久 HMAC-SHA256 密钥认证。密钥在 GUI 配置目录的 `query-v2.key`，支持 `CIALLOGCAT_CONFIG_DIR`；Unix 新建权限为 0600，完整写入后才发布。并发首次调用共享同一个获胜的密钥。不要将私钥加入日志或版本控制。删除密钥会使已有 token 失效；token 不是可跨主机传递的证据文件，跨工具交接应使用 native JSONL 与其 session_id/seq。

cursor 绑定规范化源路径、文件身份（Unix dev/inode；Windows volume/file index）、source/schema、固定前缀摘要、模式、过滤条件、offset、last_seq、扫描统计及上界。换源、换过滤、换模式、token 恶改、文件替换、缩短或已观察边界改变都会报错，不会悄悄从头重查。record_ref 还绑定记录本身的完整摘要；get/context 的目标校验可以发现其原地修改。

默认起点继续签发/接受原有 version=2 cursor，缺少新增字段的旧 token 保持兼容。end 起点使用 version=3 cursor（认证外壳仍是 c2），将未扫描历史的边界、发布条数和首条 seq 单独保存；原 count/first_seq 仅记录实际扫描的后缀。旧程序会拒绝 version=3，不会误解为已完整验证的 version=2。调用方始终把 token 当不透明字符串。尾部 cursor 到达新水位时还检查发布 entries 与累计后缀条数一致；已观察 footer 后拒绝任何新的发布追加。

性能前提是源文件只追加且不修改已观察前缀。每页的历史一致性摘要只覆盖固定大小前缀和边界 anchor，不哈希整个历史文件；有界 header/footer 的格式与身份校验另行执行。因此不声称发现任意位置、同大小的外部原地篡改。相同用户能够修改源和私钥，这也不是该用户权限内的安全隔离边界。

原生 v2/v3 查询的 header、footer 和单条数据记录统一使用共享 codec 的 **8 MiB 编码上限（包含末尾换行，如有）**，`--file` 与 `--session-dir` 使用相同上限。header 的 source 扩展（包括完整 device_diagnostics 报告）和其他未知字段也计入，不裁短有效字段。每次打开来源（包括续页、get/context）都重新有界读取并校验 header，最多取 8 MiB + 1 字节来判断超限；实际读取还可能有有界缓冲预读，session 始终受已发布 offset 限制。header 不计入逻辑扫描字节，但计入 `file_bytes_read`，因此大 header 会增加每次请求的来源校验开销。

离线 v2/v3 必须有合法 footer 才开始查询；来源校验从文件尾最多反向检查 8 MiB + 1 字节（额外一字节用于确认前一行的换行分隔符），再有界读取并校验 footer，不重扫历史记录。这些读取同样计入 `file_bytes_read`，属于扫描预算之外的来源校验开销；顺序扫描经过 footer 时，其完整编码长度仍计入扫描字节，并校验累计记录统计。离线 v2/v3 允许合法 footer 没有末尾换行，超限或截断的 header/footer 则拒绝。

session-dir 始终只读取已发布前缀，包括失败或终止会话，因此允许尚无 footer，但水位必须结束于完整换行；尚未发布的 footer 字节不会作为记录读取。session 查询成功不等于整份文件通过离线 artifact 验证。离线 v1 接受最后一条合法 JSON 无换行，不能由此断言采集完整。

错误继续使用 CLI 的 JSON stderr 错误 envelope，code 包括 `invalid_token`、`cursor_mismatch`、`stale_cursor`、`invalid_source`、`source_not_ready`、`invalid_arguments`。自动化应据 code 决定重试或重新打开源，不应解析自然语言 message。

## 验证范围

`tests/query_v2_integration.rs` 运行真实 CLI，隔离源文件和密钥目录，所有命令有期限。测试包含固定 snapshot、发布水位之外的追加、无新 tail、深页读取字节断言、巨大首条与全文分块、UTF-8 边界、token 恶改、替换/截断、上下文局部读取、原生活动/离线格式和错误水位。header/footer 回归覆盖 v2/v3 的 file/session 入口、超过 64 KiB 的合法扩展、恰好 8 MiB、超限一字节、截断，以及分页、get/context、活动水位与游标校验；大 header 还验证既有身份前缀摘要，footer 另覆盖离线无末尾换行。读取量使用实际 Read 计数，区分逻辑扫描和来源校验开销，避免把共享 runner 的毫秒抖动当成性能回归。

end 起点回归另覆盖空会话、发布后的追加、稀疏 seq、过滤和旧 token 兼容、footer 前后建立起点、非法组合、过期边界/身份/计数，以及 30000 条历史的有界实际 I/O。中间前缀故意损坏的案例验证尾部 cursor 不会冒充全文验证；从头扫描仍会拒绝该损坏。

新增 v3 回归覆盖活动/离线来源、proto tail 后缀与稀疏序号、原行与 decoded 完整 UTF-8 分块、所有扩展容器、超过 8 MiB 的 NUL 派生视图，以及错误 profile、混合 schema、竞争字段和损坏 payload 拒绝。v2/v3 共用 footer 上限与水位回归。`examples/agent_capture.py` 当前固定创建 v2、仅接受 record_schema_version=2，不提供 v3 入口。
