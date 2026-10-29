# Logcat protobuf 调研与可实施方案

调研日期：2026-10-23。本文保留一手研究与设计取舍，并在本节记录已经落地的 host 模块。证据包含 AOSP 固定版本源码、依赖项目的一手说明，以及 Linux 上的 Rust 合成数据实验；**没有进行 Android 真机或模拟器验证**。

结论：`logcat --proto` 值得作为 Android 15+ 的可选采集后端。它能改善消息边界、时间精度、UID 与多行消息，适合 GUI 和 Agent 共用的日志证据模型；它不是原始 logd 数据协议，也不能解决 PID 生命周期、日志缺口或逐条 buffer 归属。先完成现有采集可靠性与统一记录格式，再用有限设备矩阵试验 protobuf，不让它成为首批 GUI 闭环的前置条件。

[历史三组合性能复测](transport-performance-2026-10-24.md)覆盖旧构建的 v2/text、v2/proto 和 v3/proto：compact 缩小 protobuf 工件约 54%/62%，端到端吞吐并非普遍提高。该结果限固定主机合成负载，不能替代设备或 GUI 性能验收。

同日进一步完成了[选项与软件实践调研](logcat-options-and-ecosystem.md)：官方 latest-release 清单指向 Android 17，其固定 schema 与本项目 Android 16 schema 逐字节相同；Studio 固定开发源码 `ada4bc6…` 在 API ≥36 且默认 flag 开启时使用 `logcat --proto`，其余使用 long/epoch 文本。这里的 API 36 是 Studio 采用策略，不是 AOSP 协议首次可用版本；尚未据此验证具体 Studio 稳定发行包或 OEM 设备。下文较早版本表保留原研究范围，新增证据支持继续设备验证，不改变当前显式选择策略。

## 当前实现：host decoder、显式采集与原帧存档

`src/protobuf.rs` 已提供实际产品模块，运行时依赖固定的 `prost = 0.14.4`。
它独立于 ADB 子进程、GUI 与文件 DTO；传输选择和生命周期由调用方负责。
显式实验采集不得自动优选或在失败后悄悄降级文本。设备验证矩阵仍是待验收项。

- `FrameReader<R: Read>::read_next()` 返回一帧 `DecodedEntry`，只在完整帧边界 EOF 时返回 `None`。帧长度是 8 字节 LE，payload 在分配前限制为 1 MiB。头部截断、payload 截断、超限、I/O、无效 protobuf 和计数溢出具有独立错误类型、帧序号和 byte offset。任何错误后 reader 终止，禁止猜测 resync；deadline/取消/回收属于 controller。
- `DecodedEntry` 保留全部已知数字、tag/message 原始 bytes、可选 process_name，以及收到的**原始 protobuf payload**。未知字段在该 payload 中保留，不把 prost 重编码当原帧。`owned_bytes()` 计入 raw payload 与解析 bytes 的重复内存；展示字符串、队列、容器开销由调用方另计。
- `DecodedEntry::project()` 是 ADB、文档与查询共用的固定 `aosp_logcat_16r1_host_v1` 投影；adapter 不再维护第二套级别、时间、Tag 和 process 规则。
- `normalize()` 提供 `Cow<str>` 展示文本与显式 normalization flags：仅移除一个 Tag 尾 NUL，分别标记 tag/message 的非法 UTF-8 替换。message 的换行、空白、NUL 和原始 bytes 保留；process_name 不做不可逆修剪，它仍是 `device_process_cache` 提示。这里不声称 AOSP 转换前的 NUL 已被保留。
- priority 7 保持 `Unknown(7)`；Default、Silent 也不强行变成 Info。nsec 非法时 normalized time 为 None，原数值仍在。秒纳秒整合为单整数使用 checked arithmetic。不能把未知 priority、超 u32 的 PID 或坏时间强行塑造成普通日志。
- 零长度、全默认或只有未知字段的 payload 是合法 protobuf；`has_known_content()` 为 false。调用方可以记录有限诊断，不应凭“成功 decode”升级 readiness 或产生虚假正常日志。这个方法返回 true 也不证明采集健康或完整。

固定 schema 为 `proto/logcat.proto`，来源 Android 16 r1 提交
`d78b713380007d3c0dde14712cbcbec27f491ad9`。原样文件 SHA-256 为
`d709478babea7dc534dec1bfde154f1d73bc0f72e44212ba8f34d0382b3ddabd`。
`proto/LICENSE-APACHE-2.0` 保留同目录上游 NOTICE；[来源与许可证](../proto/README.md) 与生成代码一并提交。

`src/protobuf/generated.rs` 已由独立维护工具实际生成并提交：
`prost-build = 0.14.4`、`protoc-bin-vendored = 3.2.0`、`libprotoc 31.1`，
依赖使用 `tools/proto-codegen/Cargo.lock`。普通产品构建不运行 protoc，
生成工具可以离线重生成和逐字节检查：

```sh
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target/review-build cargo run --offline --locked --manifest-path tools/proto-codegen/Cargo.toml -- --check
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target/review-build cargo test --offline --locked --no-default-features --lib protobuf::
```

现有 17 组 decoder fixtures 已在 Linux 通过，包括独立手写 wire golden、所有截断位置、单字节分片与 EINTR、恰好 1 MiB/超限帧、未知枚举/字段、无效 UTF-8 string 与任意 bytes、重复字段与原帧、无效时间/超大身份值、错误后禁止继续、EOF、错误位置与计数不回绕。生成器 `--check` 也已通过。后文的“仓库外实验”是前期验证记录，不应与产品测试混为真机证据。

产品 ADB adapter 默认文本传输，支持显式 `--transport proto`。两种传输使用唯一原生格式，不提供 `--record-format` 选项、自动选择或旧格式兼容；共享 capture 流水线、历史范围、预算、flush 后成对发布水位与状态：

- 每次启动前执行目标 `adb -s SERIAL features`，只有精确的 `shell_v2` token 才允许启动。查询错误保留诊断，缺少能力明确失败。这个检查证明传输能力声明，不证明设备 logcat 接受 `--proto`。
- `LogcatStartOptions` 可传入剩余 deadline 和取消回调；能力查询每 10 ms 检查一次，取采集剩余时间与查询自身 5 秒上限的较小值，取消/到期先 kill + reap 查询再返回，spawn 日志流前再次检查。查询失败的 stderr 只保留至多 4 KiB UTF-8 预览与截断后缀，避免能力错误撑坏 session status。
- 使用参数数组启动 `adb -s SERIAL shell -T -n`，远端 command 为固定词汇组成的 `exec logcat --proto -v threadtime -T 1 -b ... '*:V'`。serial 留在本地 argv，buffer 来自枚举。stdout 按二进制读取；stderr 继续独立有界读取，退出/回收沿用现有采集生命周期。
- 不自动降级、不把 stderr 解析为 protobuf。选项不支持、权限失败、协议错误均保留独立诊断；合法零记录不伪造已观察日志。空/default/unknown-only、未知级别、越界 PID/TID、坏时间进入 Unparsed，保留完整证据。
- backend 的最终 Exited 保留原始错误诊断，并以 typed `incomplete_proto_frame` 标记单纯帧头/帧体截断；其他 reader/process 错误不会被该标记掩盖。CLI 在确认主动停止且 child 已回收时，将半帧记为 `incomplete_frame_at_stop` / `concurrent_with_host_stop` 缺口并标 partial，不假称停止必然是截断原因。unexpected 截断和其他协议错误仍走失败/重连处理。
- 原生 `proto` 对象只保存一份 base64 原始 payload 及未知扩展，固定投影恢复完整展示和元字段。未知 JSON 扩展及 protobuf wire 字段都可往返，`device_process_cache` 仍只是来自原帧的进程名提示。参见 [原生格式](native-format.md#protobuf-storage-and-decoded-views)。

`query/get/context` 使用同一查询协议；响应 schema 常量与存储 schema 常量互相独立，均不是用户选项。`get` 默认 stored 返回原行；显式 `--view decoded` 返回完整派生结构（包含 message、全部 proto 字段和扩展），响应明确标记派生视图。UTF-8 分块和响应预算仍有效，展开视图单独限制 64 MiB，存档/footer 仍为 8 MiB。native tail 支持 `--start-at end` 的已发布边界，跳过前缀不宣称完成扫描。参见 [Agent 查询契约](agent-query.md)。示例 `examples/agent_capture.py` 使用当前原生格式并校验 status 的内部 schema 标记。

adapter 和 native 回归覆盖原始 bytes、空帧、未知字段帧、零记录、未知优先级、malformed、truncated、oversized、不支持选项、权限失败、缺 shell_v2、features 查询失败、巨大非法 UTF-8 错误预览，以及半帧期间主动停止。子进程场景核对 argv trace，不存在文本 fallback；挂起 features 的 deadline/cancel/预取消/过期调用验证查询已回收且没有创建日志流。另有 writer partial-write/flush 失败水位与 StreamEncoder 跨 attempt 身份/事务测试。这些是合成协议/宿主子进程验证；设备与 Windows/macOS 字节矩阵仍待验证。

## 1. 查到了哪些版本

以下指 **AOSP tag 的源码**，不能直接当作厂商设备能力承诺。

| 来源 | 固定提交 | `--proto` | 本文核对范围 |
| --- | --- | --- | --- |
| Android 13 r1 | `android-13.0.0_r1` | `logcat.cpp` 中未发现 | 选项存在性 |
| Android 14 r1 | `android-14.0.0_r1` | `logcat.cpp` 中未发现 | 选项存在性 |
| Android 15 r1 | `54d3fa266d2123c0b076c646fae60d049311052a` | 存在 | schema、写帧、转换、进程名 |
| Android 16 r1 | `d78b713380007d3c0dde14712cbcbec27f491ad9` | 存在 | schema、写帧、转换、进程名 |
| main，调研时快照 | `bcac7c30d88a3773a7c0bc9f5617a23a886331fd` | 存在 | 完整采集路径与参数 |

15、16、main 的 `logcat.proto` 相同。[1][2][3] Android SDK 版本可用于安排探测，但实际执行结果才是设备能力证据。厂商可能回移、裁剪或修改实现。能力缓存应绑定设备连接/启动上下文与所选 buffer，不能只按 Android 大版本永久缓存。

## 2. 精确 schema 与 framing

以下是 AOSP schema 的字段与枚举，保留了容易误读的数值；源文件采用 `proto3`，package 为 `com.android.logcat.proto`，并设置 Java multiple files 与 `LITE_RUNTIME`。[1]

```proto
enum LogcatPriorityProto {
  UNKNOWN = 0;
  DEFAULT = 1;
  VERBOSE = 2;
  DEBUG = 3;
  INFO = 4;
  WARN = 5;
  ERROR = 6;
  FATAL = 8;
  SILENT = 9;
}

message LogcatEntryProto {
  uint64 time_sec = 1;
  uint64 time_nsec = 2;
  LogcatPriorityProto priority = 3;
  uint64 uid = 4;
  uint64 pid = 5;
  uint64 tid = 6;
  bytes tag = 7;
  bytes message = 8;
  optional string process_name = 9;
}
```

关键约束：

- proto `FATAL=8` / `SILENT=9`，原生 Android priority 为 `FATAL=7` / `SILENT=8`。AOSP 15/16/main 的 `GetProtoPriority` 已逐项正确映射；两个 host adapter 不能共用裸整数转换。proto 的数值 7 应为 Unknown(7)，不能解释为 Fatal，也不应改成 INFO。
- `tag`、`message` 是字节，不要求 UTF-8；`process_name` 是 string，需要合法 UTF-8。
- 除 `process_name` 外，字段采用 proto3 implicit presence。缺失整数与显式 0 在普通生成类型里无法区分；UID 0 是真实 root UID，不能当作 unknown。
- schema 没有 buffer/lid、设备启动 ID、进程 starttime、全局日志序号、drop/truncation 标志、捕获 session 或源版本。
- `time_sec`/`time_nsec` 来自日志头的设备时间，保留为整数对；`time_nsec < 1_000_000_000` 是时间语义检查。不要先转浮点数或只保存本地格式化时间。主机收到时间与设备时间分开。

流格式是重复的 **8 字节 little-endian unsigned length + 该长度的 protobuf payload**。[4]

```text
[u64_le payload_len][LogcatEntryProto payload][u64_le payload_len][payload]...
```

它不是 protobuf 常见的 varint length-delimited 流：不能直接调用 `decode_length_delimited` 读取整个 stdout。外层也没有 magic、协议版本、校验和或可扫描的分隔符。AOSP 注释明确写 little endian，代码实际把本地 `uint64_t` 的 8 字节写出；本文以现行 Android 小端 ABI 为兼容范围。

## 3. `PrintToProto` 前已经发生的转换

实际路径：[4][5]

```text
logd -> log_msg
  -> --uid 过滤
  -> ProcessBuffer
       -> 普通日志转换 / binary event 文本化
       -> tag/priority filterspec
       -> regex / max-count
       -> PrintToProto
            -> 当前 /proc 查询或 PID 缓存中的 process_name
            -> 8LE 长度与序列化 payload
```

因此“protobuf 比 threadtime 更准确”成立的范围是**结构与表示**，不是所有原始信息。

### 普通日志：NUL、空消息与非法 UTF-8

`android_log_processLogBuffer` 处理 `<priority><tag>NUL<message>NUL`，在首个 tag NUL、首个 message NUL 处划定边界。[5]

- 正常 `Tag\0` 的 `tagLen` 包含末尾 NUL，`PrintToProto` 按该长度复制。因此 proto tag 很可能是 `54 61 67 00`，不能不经适配直接显示或比较。
- message 不包含结束 NUL；嵌入 NUL 后的字节不会进入正常转换结果。**合成 protobuf 可以包含 message NUL，不代表 AOSP 普通日志路径会保留它。**
- 非 UTF-8 message 字节在上述边界内可以进入 `bytes`；UTF-8 有损转换应只发生在显示层，并能区分显示文本与源字节。
- 合法空消息应保留，不能因 `message.is_empty()` 删除整条记录。空 message 字段在 protobuf 中可能完全不编码。
- 缺少 NUL 时，liblog 会尝试修复 tag 或覆盖最后一个 message 字节来补 NUL；proto 不带“已修复/已截断”的标志。主机无法从结果证明原数据完整。
- 转换返回错误且没有 `--debug` 时，`ProcessBuffer` 不输出记录；可能只有 stderr 诊断。不要启用 `--debug` 来强行取得格式异常的数据。

建议内部保留 `tag_bytes/message_bytes`；搜索与 UI 通过确定的显示转换工作。普通 tag 的兼容显示可移除一个末尾 NUL，源字节必须保留；不要无条件 `trim()`、移除所有 NUL 或把所有 buffer 的 tag 解释为同一种原始格式。JSON 导出应有明确 bytes 编码约定，如 base64 加独立的显示文本，不能用替换字符冒充原始字节。

### events / stats / security

三种 buffer 都进入 `android_log_processBinaryLogBuffer`，不是原始 binary payload 直接放进 proto.message。[4][5]

| buffer | 转换行为 | 信息边界 |
| --- | --- | --- |
| events | event tag map 查名，typed payload 文本化，priority INFO | 原始数值类型、列表结构和 tag ID 不以独立字段保留 |
| stats | 走相同 binary event 转换路径 | 不能把 proto.message 当作 stats atom 原始 protobuf |
| security | 同类转换，priority WARN | 可读权限另受 logd 限制；没有内容不表示不存在安全日志 |

转换使用 `char binaryMsgBuf[1024]`。输出容量至多约 1023 字节，未知 tag 的 `[number]` 名称还可能占用同一缓冲区。溢出时文本末尾加 `^`；错误可能加 `!` 并返回失败，默认导致记录跳过。`-v descriptive` 会影响转换内容，甚至触发溢出后重新尝试普通格式。

不能仅根据尾部 `^` 断言记录被截断：正常内容也可能以此结尾，schema 又没有 buffer 或 truncation 位。可记录“此来源经过设备端有限长度文本化”的能力边界，不能伪造逐条精确截断判定。原始 typed event 分析属于后续 `-B` 后端的范围。

logd 的 `LogReader` 在权限不足时会移除 security 的 mask。[6] 因而即使 `-b all` 命令成功，也不能推导所有 buffer 均可读。权限失败和格式不支持必须分别报告。

### kernel 与 buffer provenance

kernel 在 `ProcessBuffer` 中走普通日志转换，读取的是 logd 提供的 kernel buffer；不是直接读取主机或设备 `/dev/kmsg`，也不是完整的 dmesg 保证。

protobuf 不输出 text divider，不包含 lid。多 buffer 合流后，主机只能知道**请求的 buffer 集合**，不能逐条恢复来源。只有请求一个明确 buffer 时，才可将该选择作为 source context；若日后要准确区分 main/system/crash，每条记录都需要 lid，或改用多采集流并处理顺序/开销。首批不为此增加多个设备端进程。

AOSP main 中“未指定 `-b`”选择 main/system/crash/kernel，而 `-b default` 选择 main/system/crash。产品应明确列出所选 buffer，避免跟随默认值变化。

## 4. `process_name` 是设备端 PID 缓存提示

`ProcessNames` 先读 `/proc/PID/cmdline`，取首个参数的 basename；失败或空值再读 `/proc/PID/comm`。后者可能带换行，且 Linux comm 长度有限。读取失败时名称为空，proto 不设置该 optional 字段。[7][8]

缓存是以 **PID 为唯一键的 100 项 LRU**，没有 starttime、UID、boot ID、失效时刻或“命中缓存”标志。

| 版本 | 已核对的差异 |
| --- | --- |
| Android 15 r1 | 查询结果直接放入 PID cache；临时 zygote 名也可能被缓存 |
| Android 16 r1 / main | 不缓存 `<pre-initialized>` 或以 `zygote` 开头的名称；但仍会把本次查询结果返回给当前记录 |

所有这些版本都存在 PID 复用、读取历史日志时查到当前同 PID 进程、进程重命名、权限不足等限制。protobuf 的 UID 来自记录头，比当前 `ps` 快照更接近日志产生时的身份；UID 仍不是 package 的一一映射，也不能证明某个 PID 的生命周期。

建议把名称来源标记为 `device_process_cache`，表达“来自设备 logcat 解析或缓存”的提示级证据。它不是 `record_exact_process`。不要用后来获得的名称反向填满同 PID 的历史记录；不要对 Agent 承诺“这些日志必然属于该 package”。保留 UID/PID/TID 原始字段，package 映射作为独立、有时间范围与来源的观察。首批不承诺通过 750 ms `ps`、`/proc/starttime` 或 `track-app` 证明所有 Android 进程生命周期。

## 5. 与 `logcat -B` 的取舍

| 维度 | threadtime 文本 | `--proto` | `-B` |
| --- | --- | --- | --- |
| 记录边界 | 依赖文本解析；多行歧义 | 8LE 外帧 | logger entry header 的 `hdr_size + len` |
| 时间 / UID | 取决于格式修饰符 | 秒、纳秒、UID 数字字段 | 原始 header 字段 |
| buffer/lid | divider 有限提示 | 缺失 | header 中保留 |
| 普通 message | 已经过 liblog/text 格式化 | 已经过 liblog 消息转换 | logd payload 字节 |
| events 等 | 文本化 | 同样文本化，1024 字节临时区 | 保留 typed binary payload |
| process name | 通常没有 | 设备 PID 缓存提示 | 没有，需要另行观察 |
| 兼容成本 | 当前已实现 | 新版设备选项和帧解码 | header/ABI 版本及各类 payload 解码 |

当前 `logger_entry` 头为 len/hdr_size、pid/tid、sec/nsec、lid、uid，常见大小 28 字节；`log_msg` 最大读缓冲为 5 KiB。[9] `-B` 的边界与二进制字段是另一套协议，不能由 protobuf decoder 处理，也不能只写死 28 字节而忽略 hdr_size 和旧版本。

`-B` 在 `ProcessBuffer` 前直接写原始 `log_msg`。它仍受 logd 权限、所选 buffer、时间/PID/UID 策略影响，但绕过 tag/priority/regex 转换过滤；`max_count` 的计数在 `ProcessBuffer` 内，不能以文本参数习惯假定 `-B -m N` 会正常终止。[4]

选择：首个实验使用 `--proto`；`-B` 保留为将来确有 buffer 精确归属或 typed events 原始证据需求时的独立后端。两者都不能保证设备从未丢日志。不要把 Android wire schema 直接变成 Ciallogcat 的长期存档 schema。

## 6. 传输：不要把 exec-out 自动等同于干净二进制流

核对的 adb main 提交为 `1cf2f017d312f73b3dc53bda85ef2610e35a80e9`。[10][11][12]

| 调用方式 | 设备端 PTY | 远端 stderr | 远端 exit status | 判断 |
| --- | --- | --- | --- | --- |
| `adb exec-out ...` | 无，raw | 与 stdout 合并 | 无 shell 协议传回 | 不适合作为无条件默认；stderr 可污染 protobuf |
| `adb shell -T -n ...`，双方支持 shell_v2 | 无，raw | 与 stdout 分通道 | 可传回 | 首选候选，仍需主机平台字节矩阵验证 |
| `adb shell -t/-tt ...` | PTY | PTY 合流 | 依协议而定 | 禁止用于 protobuf |
| `adb shell -x ...` | 禁用 shell 协议，回到旧行为 | 无可靠分流 | 无 | 禁止用于 protobuf |

`exec:` service 明确使用 `SubprocessType::kRaw` 与 `SubprocessProtocol::kNone`；raw 不代表 stderr 分离。shell_v2 raw 才有独立 stderr pipe。宿主 `adb` 的 exit code 还可能反映连接错误，因此要同时保存 bounded stderr 与退出原因，不能只用 exit code 判定 unsupported。

建议直接用 Rust `Command` 获取 stdout 的 bytes pipe、独立读取 stderr，并关闭 stdin；不经过终端、不经逐行 UTF-8 reader、不把 stdout/stderr 合并。`adb shell` 将参数用空格拼接后交给远端 shell，**不会自动对每个参数转义**。[10] 构建远端命令应使用明确的 POSIX shell quoting，传输参数与用户文本不得直接拼接。产品固定命令可以采用：

```sh
adb -s SERIAL shell -T -n "exec logcat --proto -b main -v threadtime -T 1 '*:V'"
```

上例是 POSIX 主机 shell 的手工示例；Rust 应把完整、已按远端 shell 规则转义的 command 作为一个参数传递。主机 Windows cmd/PowerShell 的引号规则不同，应用代码无需经过这些 shell。Windows 上宿主 adb 的 C runtime、重定向与版本差异仍需字节探针验证，不能仅凭设备端 raw 就宣布所有主机字节保持不变；当前未做 Windows/macOS 实测。

不要为取得“好看的错误信息”将二进制 stdout 解成文本；错误输出只能来自 stderr。若不得不试 exec-out，必须显式解决远端 stderr 和 exit 状态，不能只换一个命令名。

## 7. 参数兼容与能力探测

### 已由源码确认的语义

| 参数 | protobuf 路径 |
| --- | --- |
| `-b main` 等 | 适用；必须记录请求集合，无法从 payload 得到 lid |
| `-T 1` | 最近一条起继续跟随；不是“从这一刻恰好开始” |
| `-t 1` | 非阻塞 tail 后退出；不会持续采集 |
| `-T/-t TIME` | 支持月日、完整年月日或带小数点的 epoch 时间格式；纯数字按条数解释 |
| `-T/-t 0` | 源码会警告并改为 1，不能用于“零历史”探测 |
| `--uid=10000,10001` | 在输出选择前按记录头 UID 过滤；是 numeric UID，不是 package |
| `--pid=N` | reader PID 选择；不跟随 package 重启，也不验证进程生命周期 |
| `-v/--format` | 参数会被解析；大部分显示修饰符不改变 proto 的数值字段，但 `descriptive` 会改变 binary event 文本化 |
| filterspec / `-e` | `ProcessBuffer` 过滤仍生效；不要假定 proto 自动包含所有级别 |
| `-m N` | 按匹配条件计数；与 `-t` 明确不兼容 |
| `-B` 与 `--proto` | 都设置同一 output_type，最后一次设置可能覆盖；产品应禁止组合 |
| `-D` | proto 不生成 divider records |
| `-f/-r` | 首批不用设备文件与轮转；由 host writer 管预算 |

固定 `-v threadtime` 与 `'*:V'` 是为了让环境和默认过滤策略可重复，不是让 proto 变成 threadtime。AOSP 可读取 `ANDROID_PRINTF_LOG`/`ANDROID_LOG_TAGS`；`adb logcat` 包装器还会主动传 host 的 `ANDROID_LOG_TAGS`，`adb shell ...` 路径不同，应由应用明确策略。[4][10]

源码另有可疑计数：`PrintToProto` 写出 8+payload 字节后，返回 `sizeof(size) + sizeof(data.length())`，即 8+sizeof(size_t)：通常 64 位构建为固定 16，32 位为 12；`ProcessBuffer` 用返回值更新轮转字节计数。[4] 这是源码级发现，未在设备复现。当前产品没有依赖设备 `-f/-r`；候选后端也应由 host 自己计数/写文件，不依赖设备 `-r` 实施字节预算。

### 探测状态不能只有 supported / unsupported

建议最少区分：

```text
Unknown
  -> UnsupportedOption          明确“不认识 --proto”且证据匹配
  -> TransportUnavailable       shell_v2/连接/授权等问题
  -> PermissionDenied           buffer/读取权限问题
  -> AcceptedNoRecord           成功退出、零记录：仅证明命令接受了参数
  -> DecodedRecordObserved      至少见到完整且语义可识别的帧
  -> InconclusiveTimeout        静默/超时；不等于 unsupported
  -> MalformedOrIncompatible    收到数据但 framing/schema 不符
```

1. 读取 SDK、build fingerprint、adb version/features 作为上下文，不写设备日志、不清 buffer、不主动 `adb root`。
2. 用有上限的有限探测，如 `--proto -b main -t 1 -v threadtime '*:V'`。上限由 host deadline + kill/reap 保证，而不是相信设备一定按时退出。stdout 与 stderr 分开且各自有预算。
3. 空成功只标 `AcceptedNoRecord`。即使 protobuf 空 payload 能成功 decode，也没有证明有效日志或完整 schema。静默超时不能降级成 unsupported；权限错误不能降级成格式不支持。
4. 看到有效记录后记录已观察能力，而不是宣称所有 buffer/所有设备状态都支持。probe 与正式 capture 之间可能有断开、重启或权限变化，正式采集仍需检测失败。
5. 选项明确不支持可以在开始用户采集前选择 text。正式 segment 启动后，无论尚未收到记录或已收到记录，格式切换都结束旧 attempt，等待退出/回收，写 format-change 与 gap/unknown-continuity 事件，再建立新 segment。不能在同一 reader 中把失败的二进制流当 text 继续读。

readiness 仍要区分 `process_started`、`protocol_record_observed` 与日志静默。启动成功或后来静默都不能证明完整捕获，也不能单凭静默判死。`-T timestamp` 不能提供设备全局 cursor；时间相同、时钟跳变、ring buffer 淘汰都需要缺口与可能重复的语义。

## 8. 有界 decoder 与故障策略

建议独立 `ProtoFrameReader<R: Read>`，置于当前 `read_logcat_stream` 文本 reader 的旁边；上层只看统一 source record / source diagnostic，不依赖 prost 生成类型。保持单一 writer 分配 session 内序号；设备协议不提供本项目的 `seq`。[本地 `src/adb.rs` 的 `logcat_args`、`read_logcat_stream` 是接入点。]

初始策略：

| 条件 | 明确行为 |
| --- | --- |
| 边界上 EOF，未读下一个 header 字节 | 正常边界 EOF；再结合 capture intent/child exit 判断主动停止或异常结束 |
| 只读到 1–7 个 header 字节后 EOF | `truncated_frame_header`，终止该 segment |
| 长度超过 1 MiB 或整数换算失败 | 在分配 payload 前拒绝，`frame_limit_exceeded`，终止；不按宣称长度 drain |
| payload 不足宣称长度 | `truncated_frame_payload`，终止；不发半条日志 |
| 有效 payload、正常空 message | 接受记录 |
| 长度 0 或仅解出全默认/无可识别字段 | protobuf 本身合法；作 uninformative-frame 诊断，不升级能力、不伪造真实日志；边界已知可继续 |
| protobuf malformed、非法 string UTF-8 | 保留有界失败诊断/原帧上下文，停止该 segment；不 UTF-8 lossy 后再解码 |
| 未知字段 | 库可跳过；不因此失败；若需要原始证据，要保留原始帧，因为 prost 不会保留未知字段 |
| 未知 priority 数值 | 保留数值并显示 Unknown(value)，不当作 wire 错误 |
| 时间超出本地显示能力 / nsec 非法 | 源数字保留，时间标 invalid/unrepresentable；不通过溢出产生错误时间 |
| 帧中途超时 | 保持有限缓冲并让 controller 决定 deadline；若取消则记录 partial-frame；没有字节的正常 idle 不自动算协议坏掉 |

1 MiB 是产品接收上限，**不是 AOSP 协议声明的最大帧长**。目前 logd 单条约 5 KiB，但 protobuf 加入名称、未来扩展与厂商差异，不应把“当前源码常见大小”硬编码成普遍协议极限。外层 cap、单帧分配、已解码 bytes、展示字符串、排队和留存预算都要计入；不能保留 raw payload、decoded bytes 和显示字符串后仍只按其中一份计费。

这不是能可靠 resync 的格式。错误后扫描后续字节寻找“看起来合理的长度”会产生伪记录，首批不做。保存原帧也不等于原始 logd payload；两者 provenance 必须不同。首批若不提供 raw protobuf 附件，应明确内部存档保留的是已识别字段，不承诺 wire round-trip。

本项目原有 bounded queue、保留上限和 CLI 输出预算继续适用。UI 的展开/折叠与 Agent 的 preview 属于展示预算，不能通过截掉内部消息来实现。字节解码、进程提示与 source metadata 均应进入 shared core，GUI/CLI 分别处理交互和输出，避免两个 protobuf parser。

## 9. Rust 实现与供应链选择

截至调研日期，一手依赖状态如下：[13][14][15][16]

| 候选 | 生成与构建 | 当前状态 / 代价 | 本项目判断 |
| --- | --- | --- | --- |
| `prost = 0.14.4` | runtime 为 Rust；`prost-build` 从 0.11 起调用 protoc，不再内置 protoc；checked-in bindings 的普通构建不需 protoc | Apache-2.0；MSRV 1.85；维护者标记 passively-maintained，继续缺陷/安全修复；未知枚举保留、未知字段丢弃 | 首个实验优先；小 schema、无反射需求，隔离 adapter |
| stepancheg `protobuf = 3.7.2` | Rust runtime；`protobuf-codegen` 可用 `.pure()` parser，也可用 protoc；bindings 可提交 | MIT；项目明确 approaching end of life；pure parser 文档承认覆盖与测试边界 | 不因“无需 protoc”就为新后端引入它 |
| Google 官方 `protobuf` 4.x / `google-protobuf` | upb C 或 C++ kernel；官方 codegen 检查 protoc 精确匹配版本 | 新实现，和旧 3.x API 不同；官方 README 仍称 beta；4.x 包提示迁移到 `google-protobuf` | 持续观察；首批不引入额外 native 工具链与迁移面 |

crates.io 当日显示 prost 最新 stable 0.14.4；`protobuf` 的 stable/default 仍为 3.7.2，但最新含 `4.36.2-release`。`google-protobuf` 已有 `0.36.2-release` 与 `0.37.0-rc.1`。[17] `-release` 在 SemVer 中仍是 prerelease 字符串，不能把 crate 名或 latest banner 当作旧 rust-protobuf 的平滑升级。Google 当前新代码生成器明确用 `experimental-codegen=enabled,kernel=upb` 并检查 protoc 版本。[16]

建议实现时：

1. 固定一份审阅过的 AOSP `.proto`、来源 commit、Apache-2.0 版权/许可证说明，提交生成的少量 Rust bindings；记录生成器与 protoc 版本。
2. 默认 Cargo 构建只依赖 prost runtime/derive，不在每次产品构建中下载或运行 protoc。生成步骤用独立维护命令，CI 的定向检查重生成并比较 diff；不把 generator 依赖放进产品 runtime。
3. protoc 可使用受控系统版本或版本固定的 vendored 工具；若用后者也要审查二进制平台覆盖、下载来源和许可证。checked-in 文件是去除常规构建工具依赖，不是省略可重复生成。
4. 原始 bindings 只存在 decoder 模块内部。未来替换库不改变持久化 schema 或 GUI/CLI API。当前需求不需要 gRPC、反射、动态 schema 或 tokio runtime。
5. MIT 项目可以使用这些依赖，但不能将复制的 AOSP schema 或第三方依赖重新标成项目自有 MIT；发布时保留各自适用的版权、license、NOTICE（若源材料有）。研究阶段没有复制 schema；当前实现已固定 schema、生成代码与上游 NOTICE，见本文首节。

## 10. 可重复的主机验证

早期仓库外实验已由产品 protobuf 回归和固定 schema 工具取代。新主机直接运行：

```sh
cargo test --locked --no-default-features --lib protobuf::
cargo run --locked --manifest-path tools/proto-codegen/Cargo.toml -- --check
```

覆盖分片/截断/超限、未知字段和优先级、NUL/UTF-8 及空消息等主机边界；设备转换与通道仍需下述独立验收。

## 11. 可重复的设备验收矩阵

每次记录 host OS、adb version、`adb features`、SDK、build fingerprint、设备 boot context、请求 buffer、完整安全转义后的命令、exit status、bounded stderr、总字节数、完整帧数和首末时间。保存测试 fixtures 时脱敏真实日志。

| 设备 / host | 要验证的最少内容 |
| --- | --- |
| AOSP Android 14 | 显式 proto 选择明确失败；不自动 text 回退、不把错误文本解成 proto |
| AOSP Android 15 | 帧/字段；旧进程名缓存；空成功与静默区别 |
| AOSP Android 16 | 帧/字段；与 15 schema 兼容；重连与 partial-frame |
| 至少一个 Android 15/16 厂商 user build | 真实权限、命令实现、所选 buffer；SDK 与能力可能不一致 |
| Linux / macOS / Windows 的受支持 adb 版本 | raw shell_v2 字节不变性、stdout/stderr 分离、远端 exit code、取消回收 |
| 模拟 transport/fake-adb | 任意分片、超限、非法帧、stderr 泛洪、连接在帧任意字节断开、idle、旧 attempt 迟到事件 |

先执行只读的探测命令。以下为 POSIX 手工示例，正式验收 harness 应直接捕获 binary pipe，采用 host timeout 与 kill/reap，不依赖 shell `timeout` 的跨平台差异：

```sh
adb version
adb -s SERIAL features
adb -s SERIAL shell -T -n getprop ro.build.version.sdk
adb -s SERIAL shell -T -n getprop ro.build.fingerprint
adb -s SERIAL shell -T -n "exec logcat --proto -b main -t 1 -v threadtime '*:V'" > probe.bin 2> probe.stderr
```

随后单 buffer 分别测试 main/system/crash/events/stats/security/kernel，再测试明确选定的合流集合。security 或 kernel 不可读、没有记录、命令不认识该 buffer 是不同结果，不能统一勾选“兼容”。`-T 1` 做有界实时采集；`-t 1` 做有限 dump；`--uid` 用已知测试 app 的 UID 验证 include/exclude；`-v descriptive` 仅用于确认二进制 event 转换差异，不进入默认产品命令。

在专用测试设备上，另用受控 helper/app 产生 multiline、空 message、不同 priority、快速重启、多进程和非法 bytes 样本；不在正常 capability probe 中注入日志。使用同一份已写入设备临时目录的字节 fixture，通过 `shell -T -n 'cat ...'` 与 exec-out 对照并核对 hash/原始 bytes，包含 `00 0a 0d 1a ff`；再用独立 stdout/stderr 与非零 exit 的 helper 验证通道语义。`-B`/proto 对照应捕获同一次专用测试窗口并匹配测试标识，不能假定两个先后 dump 来自完全相同的 ring buffer 快照。

验收最低标准是：不污染 stdout；不把权限/静默误报 unsupported；消息与记录边界明确；有限内存；Stop 有界且回收；切换格式留下 segment/gap；UID/PID/进程名不夸大；多 buffer 不伪造 lid。吞吐是否更好需要在相同设备、日志负载与 host 上测量 CPU、帧率、延迟、丢弃量，不能由“二进制”三个字推断。

## 12. 建议落地顺序与停止线

1. **先建立可信底座**：修复 discovery/health 混用、异常退出与回收、PID 历史回填；稳定 session/segment/seq 和 drop/gap 的共享记录格式。现有 text 后端继续可用。
2. **再合入纯 host decoder**：固定 schema/bindings、字节保留、统一 adapter、上述 framing fixtures。默认采集行为不变；不新增 gRPC/MCP 或设备端 helper。
3. **增加受控实验后端**：显式选择 proto；shell_v2 传输与能力状态；Linux 先完成 AOSP/厂商有限矩阵。缺少证据的平台不自动默认启用。
4. **通过实机比较后决定 auto**：只有参数/权限/字节传输可靠且能明显改善用户体验，才启用能力选择。fallback 必须可见且保留连续性边界。

首批明确不做：`-B` 全 ABI/typed event 实现、每 buffer 一个流、精确 PID 生命周期证明、设备端驻留服务、自动 root、自动调大/清空 logd buffer、把 protobuf 当长期唯一存档格式。作为 Agent 内置工具时，它提供可引用、可限额、带来源与缺口的日志证据；测试动作仍由外部 ADB/UiAutomator/Maestro 等工具执行，通过 host session/step 事件关联。protobuf 改善证据的表达，不扩大产品到通用实机自动化平台。

## 一手来源

1. [Android 15 r1 logcat.proto（固定提交）](https://android.googlesource.com/platform/system/logging/+/54d3fa266d2123c0b076c646fae60d049311052a/logcat/logcat.proto)
2. [Android 16 r1 logcat.proto（固定提交）](https://android.googlesource.com/platform/system/logging/+/d78b713380007d3c0dde14712cbcbec27f491ad9/logcat/logcat.proto)
3. [main logcat.proto（调研时固定提交）](https://android.googlesource.com/platform/system/logging/+/bcac7c30d88a3773a7c0bc9f5617a23a886331fd/logcat/logcat.proto)
4. [main logcat.cpp：OutputType、ProcessBuffer、PrintToProto、参数与 reader loop](https://android.googlesource.com/platform/system/logging/+/bcac7c30d88a3773a7c0bc9f5617a23a886331fd/logcat/logcat.cpp#216)；[Android 15](https://android.googlesource.com/platform/system/logging/+/54d3fa266d2123c0b076c646fae60d049311052a/logcat/logcat.cpp)；[Android 16](https://android.googlesource.com/platform/system/logging/+/d78b713380007d3c0dde14712cbcbec27f491ad9/logcat/logcat.cpp)
5. [liblog/logprint.cpp：普通与 binary buffer 转换](https://android.googlesource.com/platform/system/logging/+/bcac7c30d88a3773a7c0bc9f5617a23a886331fd/liblog/logprint.cpp#492)；[Android 15 对照](https://android.googlesource.com/platform/system/logging/+/54d3fa266d2123c0b076c646fae60d049311052a/liblog/logprint.cpp)
6. [logd/LogReader.cpp：security 权限与 read mask](https://android.googlesource.com/platform/system/logging/+/bcac7c30d88a3773a7c0bc9f5617a23a886331fd/logd/LogReader.cpp#188)
7. [Android 15 process_names.cpp](https://android.googlesource.com/platform/system/logging/+/54d3fa266d2123c0b076c646fae60d049311052a/logcat/process_names.cpp)
8. [Android 16 process_names.cpp](https://android.googlesource.com/platform/system/logging/+/d78b713380007d3c0dde14712cbcbec27f491ad9/logcat/process_names.cpp)；[cache 定义](https://android.googlesource.com/platform/system/logging/+/bcac7c30d88a3773a7c0bc9f5617a23a886331fd/logcat/process_names.h)
9. [liblog/log_read.h：logger_entry 与 log_msg](https://android.googlesource.com/platform/system/logging/+/bcac7c30d88a3773a7c0bc9f5617a23a886331fd/liblog/include/log/log_read.h)
10. [adb client/commandline.cpp：shell 参数、exec-out、logcat 包装器](https://android.googlesource.com/platform/packages/modules/adb/+/1cf2f017d312f73b3dc53bda85ef2610e35a80e9/client/commandline.cpp#696)
11. [adb daemon/services.cpp：exec service](https://android.googlesource.com/platform/packages/modules/adb/+/1cf2f017d312f73b3dc53bda85ef2610e35a80e9/daemon/services.cpp)
12. [adb daemon/shell_service.cpp：PTY/raw 与 shell protocol 的流/状态差异](https://android.googlesource.com/platform/packages/modules/adb/+/1cf2f017d312f73b3dc53bda85ef2610e35a80e9/daemon/shell_service.cpp)
13. [prost 0.14.4 README：protoc、维护状态、MSRV、许可](https://github.com/tokio-rs/prost/blob/v0.14.4/README.md)
14. [rust-protobuf README：end of life](https://github.com/stepancheg/rust-protobuf/blob/master/README.md)；[codegen parser 选择](https://github.com/stepancheg/rust-protobuf/blob/master/protobuf-codegen/src/lib.rs)；[MIT license](https://github.com/stepancheg/rust-protobuf/blob/master/LICENSE.txt)
15. [Google 官方旧 protobuf crate README：所有权与 API 变更、迁移提示](https://github.com/protocolbuffers/protobuf/blob/main/rust/release_crates/protobuf/README.md)；[google-protobuf README：kernel、beta、protoc 版本要求](https://github.com/protocolbuffers/protobuf/blob/main/rust/release_crates/google_protobuf/README.md)
16. [Google Rust codegen：版本检查与 upb 生成选项](https://github.com/protocolbuffers/protobuf/blob/main/rust/release_crates/google_protobuf_codegen/src/lib.rs)；[官方构建指南](https://protobuf.dev/reference/rust/building-rust-protos/)
17. crates.io 当日元数据：[prost](https://crates.io/api/v1/crates/prost)、[protobuf](https://crates.io/api/v1/crates/protobuf)、[google-protobuf](https://crates.io/api/v1/crates/google-protobuf)。这些版本元数据和 master/main 文档会变化，实施时应重新核对并锁定版本。
