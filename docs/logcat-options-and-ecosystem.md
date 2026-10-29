# Logcat 选项与软件实践调研

调研日期：2026-10-23。目标是为 Ciallogcat 选择有用的采集能力与阅读体验，而不是把 logcat 的全部开关搬进 GUI。本轮阅读官方文档及固定源码，没有连接 Android 设备或运行第三方软件。

核心判断：应明确区分**设备采集范围、主机阅读筛选、证据保存范围**。同样叫“过滤”“清空”“暂停”，不同软件实际操作的层次不同。保持这三层语义清楚，比增加一个任意命令参数输入框更有价值。

## 1. 版本与证据范围

- Android 官方命令文档最后更新 2024-01-03，要求以目标设备的 `adb logcat --help` 为准，很多选项还受权限影响。[1]
- 本次官方 `android-latest-release` manifest 指向 `android17-release`。[2] logging 的固定提交为 `d35ba620451b28512f46ebb5421f783fd77968d5`；选项与处理逻辑以它为主要依据。[3]
- logging `main` 仍停在 2025-03-25 的 `bcac7c30…`，不能仅凭分支名将它称为最新发布代码。另对照 Android 14 r1、16 r1；16 与 17 的帮助文本完全相同，17 的 protobuf schema 与项目固定的 16 schema 逐字节相同，SHA-256 为 `d709478babea7dc534dec1bfde154f1d73bc0f72e44212ba8f34d0382b3ddabd`。[4]
- 软件部分区分公开 UI 文档、固定源码分支、实际发布版本和本地项目实现。源码出现某条路径不等于已实测所有设备或已验证对应发行包。

## 2. 有哪些选项，如何理解

下列为 Android 17 固定源码中的主要公开选项；不是所有 Android 设备都会提供或允许它们。`adb -s SERIAL` 是主机选择设备，后面的 `logcat` 选项由设备端执行。

### 采集范围与起点

| 选项 | 实际作用 | 对当前项目的意义 |
| --- | --- | --- |
| `-b main/system/crash/radio/events/…` | 请求 logd 的环形缓冲区；可重复 `-b` 或逗号分隔，多 buffer 合流 | 当前 GUI 已支持主要 buffer；GUI/CLI 均显式默认 `main/system/crash`，CLI 后续可暴露 buffer 选择 |
| `-b default` / `-b all` | default 明确为 main/system/crash；all 请求所有可读 buffer | `all` 不等于绕过权限，也不保证每个 buffer 有数据。17 源码“不指定 -b”的默认还包括 kernel，和 `-b default` 不同 |
| `-d` | 排出当前可读积压后退出，不持续等待新日志 | 适合一次性诊断快照，与持续录制分开建模 |
| `-t N` / `-t TIME` | 最近 N 条或从给定时间读取，隐含 `-d` | 适合“最近一段历史”；过滤之后不保证最终恰有 N 条匹配 |
| `-T N` / `-T TIME` | 相同起点，但继续跟随 | 当前固定 `-T 1` 实际是“带最后一条历史再跟随”，并非严格从此刻开始，也不是可靠重连 cursor |
| `--pid=PID` | 单一 PID 选择；固定源码拒绝重复 `--pid` | 降低设备输出量，但进程重启换 PID 后需重新处理，多进程应用也可能漏掉子进程 |
| `--uid=UIDS` | 按数值 UID 列表选择 | 可作为明确的窄范围采集；权限、共享/隔离 UID 和用户空间意味着不能直接当成稳定 package 身份 |
| `-L` / `--last` | 从 pstore 读取上次重启前保留下来的日志 | 依赖设备构建、权限与持久化能力，不是任意设备上的“完整上一启动会话” |
| `--wrap` | 等 buffer 将满或约两小时再唤醒，适合低频取日志 | 不适合实时 GUI；可选超时参数在当前源码里不真正改变等待超时，不应包装成通用 duration |

时间输入支持 `MM-DD hh:mm:ss.mmm…`、带年份时间、含小数点的 Unix 秒形式（如 `1700000000.000`）。**纯数字 `-T 1700000000` 会被当作条数，不是时间。** 时间起点依赖设备时间与 logd 中仍保留的记录；同时间戳、时钟变化、环形淘汰都限制精确续传。17 源码把 `-T 0`/`-t 0` 调整为 1，因此不能把 `0` 当成“完全无历史”。

### 设备侧筛选与停止条件

| 选项 | 实际作用 | 需要保留的语义 |
| --- | --- | --- |
| `Tag:D '*:S'` | Tag 的最低级别白名单；`:S` 静默 | 是 Tag/级别规则，不是包名或任意正则；在设备进程过滤后，主机没有被删掉的上下文 |
| `'*:W'`、`-s` | 所有 Tag 至少 Warn；`-s` 把默认级别设为 Silent | 无显式规则且未设置 `ANDROID_LOG_TAGS` 时当前实现默认 `*:V`；单独 `*` 却等价 `*:D`，不能混用 |
| `-e EXPR` / `--regex=EXPR` | 对 message 做 ECMAScript 正则匹配 | 与项目 Rust regex 的语法/性能保证不同，不能把 GUI 正则原样透明下推 |
| `-m N` / `--max-count=N` | 达到匹配记录计数后退出 | 源码每个解码记录计一次，不是终端物理换行数；可以无限等到第 N 个匹配，仍需主机 deadline |
| `--print -e EXPR -m N` | 输出其他通过 Tag/级别规则的消息，但累计匹配 N 次后停止 | 适合理解“出现事件即结束并保留之前上下文”；它不是命中后再采 N 条，也不保证后续上下文 |

`-m` 不能与小写 `-t` 共用；`--print` 缺少 regex 或 max-count 会警告并忽略。`-B` 原始二进制路径绕过普通 `ProcessBuffer`：Tag/regex 筛选和匹配计数的行为不能照搬，当前代码中 `-B -m N` 也不能被当成可靠的记录上限。[3]

另有发生在记录进入 logd **之前**的编译裁剪、`log.tag.*` 系统属性与应用最低日志级别。查看器无法恢复已经没写出的日志；这些也不应混进普通视图过滤设置。[1]

### 输出格式

| 选项 | 特点 | 取舍 |
| --- | --- | --- |
| `-v threadtime` | 日期/时间、PID/TID、级别、Tag、message，逐行文本 | 当前默认，兼容广；跨物理行还原消息边界困难 |
| `-v long` | 独立元数据头，消息体，空行分隔 | 多个成熟查看器使用；仍是文本协议，分隔与边界不能等同有长度帧 |
| `brief/time/thread/process/tag/raw` | 为终端或特定用途精简字段 | raw 只剩消息，process/tag 等缺少部分身份；不适合做统一证据采集模型 |
| `--proto` | 8 字节 LE 长度 + protobuf，独立数字/bytes 字段 | AOSP 15+ 已有；支持纳秒时间、UID、消息边界，可保留收到的原始 protobuf payload。它已经过设备 liblog 转换，不是转换前 logger payload，没有逐条 buffer/lid；详见[专门研究](logcat-protobuf.md) |
| `-B` / `--binary` | 原始 `log_msg`/logger header + payload 流 | 和 protobuf 完全不同；可保留 lid 与 typed event 原始结构，但需要另一套版本/头长度/事件解码器，暂不增加产品后端 |
| `-D` / `--dividers` | 文本输出在 buffer 切换时追加分隔提示 | 未启用时也可能有首次 `beginning of` 提示。属于流中的展示标记，不是 Android 应用消息；不能据默认首次提示给后续混合记录确定 buffer |

`-v` 可以搭配多个 modifier：`year`、`zone`、`UTC`、`epoch`、`monotonic`、`usec`、`uid`、`color`、`printable`、`descriptive`。`nsec` 在核对的 14/17 liblog 中也被识别，但未列在相应 help 文本里，需实际探测，不能只扫描帮助认定不支持。[5]

- `epoch`/`year`/`zone`/`UTC` 改善时间解释；`monotonic` 是启动相关时间表示，不能无条件拿它与另一设备或另一次启动比较。
- `uid` 增加身份字段，不意味着精确 package 归属。
- `color` 是 ANSI 展示控制；`printable` 会转义不可打印字节；`descriptive` 影响 binary events 的文本化。它们不应被误当无损原始字节。
- 帮助另列实验性的命名时区 `"<ZONE>"`；未显式传 `-v` 时还会读取 `ANDROID_PRINTF_LOG`。采集器应记录实际格式，避免依赖隐含环境默认值。
- 当前 `parse_logcat_line` 支持可选年份与多位小数，但不支持 arbitrary epoch/UID/时区布局。**不能只追加一个 `-v` 开关而不升级 parser、记录模型和兼容测试。**

### 诊断、设备修改与文件

| 选项 | 范围 | 产品建议 |
| --- | --- | --- |
| `-g` / `--buffer-size` | 只读查询设备 logd buffer 容量/使用信息 | 值得加到设备诊断；和 GUI 留存内存、主机磁盘预算分开显示 |
| `-S` / `--statistics`、`-p` / `--prune` | 查询统计与裁剪规则，可能有权限限制 | 作为按需诊断原文，失败显示 unsupported/permission/unknown，避免伪造全设备完整性指标 |
| `-G SIZE`、`-P RULES` | 修改设备 logd buffer 或裁剪配置 | 不作为普通查看器默认动作；也不把吞吐问题自动“修复”为修改全局设备状态 |
| `-c` / `--clear` | 清除设备日志；与 `-f` 或 `-L` 组合时改为清目标文件或 pstore | 必须区别于“清空本机视图”；本项目 Clear 语义不应偷偷变成设备清除 |
| `-f FILE -r KiB -n COUNT` | logcat 自己写文件并轮转 | 经 adb 执行时路径在 Android 设备上；`adb logcat > host.txt` 才是主机 shell 重定向。当前主机录制不需要把输出交给设备文件轮转 |
| `--id ID` | 对 `-f` 关联文件签名变化触发清理 | 是设备文件管理机制，不是本项目的稳定 session_id |

`--debug` 及 `--grep/--head/--tail` 等保留别名不是值得增加 UI 的新能力；私有/历史选项优先留在手工诊断范围。

## 3. 常见软件怎么用

三个并行研究分别检查 Studio、独立查看器、测试工具，随后交叉核对关键命令与保存路径。下表是**固定源码行为**，不是实际运行六款软件的体验评分。

| 软件 / 样本 | 实际采集方式 | 上层做了什么 | 对本项目有用的经验 |
| --- | --- | --- | --- |
| Android Studio 开发源码 `ada4bc6…` | API ≥36 且 flag 开启：`logcat --proto`；否则 `logcat -v long`，API ≥24 再加 `-v epoch`。经 adblib shell 读取 [6] | 在有界 backlog 上查询；进程监控补包名和 started/ended 事件；崩溃与堆栈识别；保存 backlog 和查询元数据 | 协议负责记录边界，主机负责阅读；应用跟随需要进程生命周期；保存后应可解释当时的查询 |
| pidcat `61cd1ee…` | `adb logcat -v brief`，Python 按行解析 [8] | 启动时 `ps` 建 PID 集合，再解析 ActivityManager 文案更新；主机按 PID/Tag/级别过滤、终端折行 | “持续看这个应用”是好任务；旧文案解析和缺少时间/TID 的 brief 不适合证据底座 |
| LogFox `8d0cf73…` | 设备内 `logcat -v uid -v epoch`；恢复时可加 `-T <epoch>`，dump 加 `-d` [9] | UID 映射包名；客户端筛选；长行展开；筛选后录制；导出 ZIP 可附设备信息 | 借鉴简洁展开和证据包；不要把筛选后文本当全量原始记录 |
| LogcatReader `56f9a76…` | 设备内 `logcat -v long`，探测后加 `uid/year`，按选择追加多个 `-b` [10] | 多行文本组装；有界缓冲、默认 250ms poll；客户端 include/exclude；筛选后录制 | 能力探测、批量刷新有价值；显示截断、录制范围与输入丢弃必须分别说明 |
| Appium ADB 库 16.1.0，`0f71f5f…` | 默认 `adb logcat -v threadtime <filterSpecs>`，支持八种文本格式 [11] | 默认缓存最近 10,000 行；提供共享“自上次请求后”增量；外层时间为主机接收时间 | 适合轻量测试日志接入；Agent 证据还需要独立 cursor、设备时间、stderr 来源与缺口信息 |
| Tradefed Android 17，`2139853…` | 通用 receiver：`logcat -b all -v threadtime`，新 API 加 `,uid`；failure collector 自定义：`logcat -b all -T 150`，没有显式 `-v` [12] | 测试前持续采集，主机文件有界轮转；按测试边界截取失败附件，即使此时设备离线也能取已有数据 | 最值得借鉴的 Agent 流程：操作前开采集，失败后交付带上下文、有限大小的工件 |

版本判断需保留三个细节：

- Studio 的 `logcat.protobuf.enable=COMPLETE:2025` 在这份源码的各通常发布渠道配置中默认启用，仍可被覆盖；旧代码旁的 “Disabled” 注释已不符合 flag 配置。[7] **这证明当前公开开发源码已采用 proto，不等于本次验证了某个稳定发行包。** API 36 是 Studio 的选择门槛，AOSP 15 已提供 proto，二者不能混为协议最低版本。
- pidcat 默认分支最后提交为 2022-01-31，作为历史设计参考；LogFox 的固定 head 为 2026-10-15（本地化变更），LogcatReader 为 2026-05-25。近期提交不自动代表协议或性能领先。
- 原 `appium/appium-adb` 于 2026-10-02 归档并指向 `appium/appium-android` monorepo；本次读的是迁移后的实现，不把旧仓库归档误判为功能停止维护。[11]

### 需要借鉴的是使用流程

**交互式排错：采集 → 过滤 → 看上下文 → 定位代码。** Studio 的 `package:mine`、`is:crash`、`age:`、AND/OR 等是主机查询语言，不是 logcat 的命令参数。修改条件会重新筛选保留的 backlog，因此能回看被过滤掉的邻近消息；但缓存淘汰后不能恢复。崩溃识别依靠具体的 Java/native Tag、级别、消息及堆栈规则，是排查入口，不是根因判定。Ciallogcat 可先做可解释的崩溃入口和原文上下文，无需一次移植全部 DSL 与 IDE 索引。[13]

**应用重启：保持包名关注，更新进程集合。** Studio 的进程监控和 pidcat 的历史实现都说明，一个固定 `--pid` 不能完成整个任务。应同时保留原 PID/UID、已知映射来源与时间边界；共享 UID、多进程、隔离进程等情况下允许未知或多个候选。包名是关联结果，不应冒充每条原始日志自带的确证身份。

**录制交接：要先定义“保存了什么”。** Studio Save 导出全部仍保留的 backlog 与筛选/项目标识，不只当前命中项；LogFox、LogcatReader 的所审路径录制筛选后的消息。二者都是合理的产品选择，但需要标明范围。Studio 保存的项目 package IDs 还能让 `package:mine` 在另一项目重开时转换成明确包名，这是可移植查询的好设计。[14]

**暂停与清空：同名按钮并不具有相同语义。** Studio Pause 取消采集任务，resume 重启并重读设备 buffer；Clear 通常执行设备 `logcat -c`（API 26 有例外）。LogcatReader pause 则暂停 poll，读取线程仍向有限 pending 队列写入；Tradefed receiver 的 clear 只清主机临时文件。[10][12][15] 本项目应明确区分暂停跟随、停止采集、清空本机视图、清除设备日志，不能由一个含糊的按钮承担这些动作。

**实机测试：先保留故障前的数据。** Tradefed 的 failure collector 在 API ≥20 配置生效时提前开流；case/run 截取上限 4 MiB，module 为 40 MiB，并保留边界前约 10 KB 上下文。其重连重新执行原命令，源码明确承认可能重复。LogFox 的 `lastLogTime + 1ms` 也不能保证没有丢掉同毫秒内尚未保存的记录。两者都不能当作精确 cursor；本项目应继续用 session/seq 引用已保存记录，单独记重连分段、可能重叠和未知缺口。[9][12]

### 可以直接验证的命令组合

以下命令是手工诊断示例，不是新加的 Ciallogcat CLI 接口；先读取目标设备帮助。设备过滤会减少可保留上下文，尤其不应把级别筛选默认为完整故障证据。

```sh
# 看设备实际支持哪些选项；查询设备 buffer
adb -s SERIAL logcat --help
adb -s SERIAL logcat -g

# 少量历史 + 后续实时日志，明确三个常用 buffer
adb -s SERIAL logcat -b main -b system -b crash -v threadtime -T 200

# 最近 200 条的有限历史快照；请求所有可读 buffer
adb -s SERIAL logcat -b all -v threadtime -t 200

# 仅特定 Tag：这是采集范围限制，不是 GUI 的可撤销视图筛选
adb -s SERIAL logcat -v threadtime 'MyApp:D' '*:S'

# 保留通过默认 Tag/级别规则的上下文，在第二条匹配时结束
# 无匹配时仍会等待；自动化调用另设主机 deadline
adb -s SERIAL logcat -v threadtime --print -e 'FATAL EXCEPTION|ANR in' -m 2
```

命令里的 `SERIAL` 与 `MyApp` 需替换成实际设备和 Tag。数值历史上限不代替主机的时间/字节预算；查看器仍需处理异常退出与 stderr。

## 4. 对 Ciallogcat 的选择

1. **先补采集起点与有限快照。** 将“最近 N 条后跟随”“从设备时间后跟随”“一次性历史快照”建模成明确意图，并在证据中保存范围。快照也保留 timeout、最大字节和记录数；Agent 调用不能只靠设备 `-m` 等待。
2. **采集尽量宽，阅读筛选留在主机。** 当前按包名/Tag/级别过滤可快速改变且能查看被筛选外的上下文，这个方向合理。设备 PID/UID/Tag 过滤仅作为明确的“限制采集范围”，切换后建立新 segment，导出中披露范围。
3. **增加只读设备诊断。** 展示实际 buffer 请求、格式/能力、buffer 大小与独立 stderr；把 logd 环形容量、主机队列预算、GUI 留存、文件上限分别表达。不要把任何一个限额说成总进程 RSS 或完整采集保证。
4. **继续推进 protobuf 的设备验证。** Studio 当前源码的采用进一步支持这条方向；Android 17 schema 也与项目固定版本一致。但[当前性能对照](transport-performance-2026-10-24.md)只证明特定宿主负载，原生紧凑编码已完成存档去重。保持当前文本默认、proto 显式选择，完成设备矩阵后再讨论默认策略。
5. **围绕事故交接做少量体验增强。** 明确复制引用、崩溃链路聚合、筛选/上下文切换、动作前后书签、可移植证据包比几十个参数开关更有价值。证据包可包含设备构建信息与采集选项，但应以本项目 session/seq 和完整性事实为主线。
6. **将自动化测试集成保持小而清楚。** 测试操作前启动采集，记录主机动作与已发布水位，失败后再保留有限尾部，关联为同一工件。时间相近只是关联线索；既有 session/cursor 继续承担引用和范围判断。设备动作由外部测试工具负责。

暂不增加：默认设备 `-c/-G/-P`、任意 shell 参数输入、以 UID/PID 宣称绝对 package 身份、原始 `-B`、用时间戳假装精确续传、为了跟随竞品引入完整测试编排/IDE 系统。

上述调研建议随后已实施为共享采集事实、历史范围、诊断与 compact 存档。当前状态见[架构方向](development-plan.md)，剩余工作见[TODO](../TODO.md)。

## 5. 一手资料

1. [Android 官方：Logcat command-line tool](https://developer.android.com/tools/logcat)
2. [AOSP android-latest-release manifest](https://android.googlesource.com/platform/manifest/+/refs/heads/android-latest-release/default.xml)（移动分支；已保存本次内容）
3. [Android 17 固定 logcat.cpp](https://android.googlesource.com/platform/system/logging/+/d35ba620451b28512f46ebb5421f783fd77968d5/logcat/logcat.cpp)：选项 L368、处理路径 L219、计数 L245、默认 buffers L916、读取/原始二进制 L1173。
4. [Android 17 固定 logcat.proto](https://android.googlesource.com/platform/system/logging/+/d35ba620451b28512f46ebb5421f783fd77968d5/logcat/logcat.proto)
5. [Android 17 固定 liblog/logprint.cpp](https://android.googlesource.com/platform/system/logging/+/d35ba620451b28512f46ebb5421f783fd77968d5/liblog/logprint.cpp)：`android_log_formatFromString` 和格式化路径。
6. [Studio LogcatServiceImpl.kt](https://android.googlesource.com/platform/tools/adt/idea/+/ada4bc6a52c19aa65163df48d1a0691c8d8f6593/logcat/src/com/android/tools/idea/logcat/service/LogcatServiceImpl.kt#42)：API 门槛、两种命令；[proto collector](https://android.googlesource.com/platform/tools/adt/idea/+/ada4bc6a52c19aa65163df48d1a0691c8d8f6593/logcat/src/com/android/tools/idea/logcat/service/LogcatProtoShellCollector.kt#149)：长度帧读取。
7. Studio [feature_flags.txt](https://android.googlesource.com/platform/tools/adt/idea/+/ada4bc6a52c19aa65163df48d1a0691c8d8f6593/studio-flags/flags/resources/feature_flags.txt#116)、[COMPLETE 配置](https://android.googlesource.com/platform/tools/adt/idea/+/ada4bc6a52c19aa65163df48d1a0691c8d8f6593/studio-flags/src/com/android/tools/idea/flags/FeatureConfiguration.kt#54)、[配置求值](https://android.googlesource.com/platform/tools/adt/idea/+/ada4bc6a52c19aa65163df48d1a0691c8d8f6593/studio-flags/src/com/android/tools/idea/flags/overrides/FeatureConfigurationProvider.kt#59)。
8. [pidcat.py 固定源码](https://github.com/JakeWharton/pidcat/blob/61cd1ee1beabfa14eb5fbe21eb90c192d96aebc5/pidcat.py)：命令、初始 ps、ActivityManager 正则、过滤与展示。
9. LogFox [命令](https://github.com/F0x1d/LogFox/blob/8d0cf730bc0a875f23477dc9015294bb6e024eb9/feature/logging/api/src/main/kotlin/com/f0x1d/logfox/feature/logging/api/data/LoggingRepository.kt#L7)、[恢复起点](https://github.com/F0x1d/LogFox/blob/8d0cf730bc0a875f23477dc9015294bb6e024eb9/feature/logging/impl/src/main/kotlin/com/f0x1d/logfox/feature/logging/impl/domain/StartLoggingUseCaseImpl.kt#L16)、[录制范围](https://github.com/F0x1d/LogFox/blob/8d0cf730bc0a875f23477dc9015294bb6e024eb9/feature/recordings/impl/src/main/kotlin/com/f0x1d/logfox/feature/recordings/impl/data/RecordingLocalDataSourceImpl.kt#L64)、[ZIP 导出](https://github.com/F0x1d/LogFox/blob/8d0cf730bc0a875f23477dc9015294bb6e024eb9/feature/recordings/impl/src/main/kotlin/com/f0x1d/logfox/feature/recordings/impl/domain/ExportRecordingZipUseCaseImpl.kt#L22)。
10. [LogcatReader LogcatSession.kt](https://github.com/darshanparajuli/LogcatReader/blob/56f9a762c65c751e7c9d570bde71af64f1b3af5c/logcat/src/main/java/com/dp/logcat/LogcatSession.kt)：命令 L155、poll L242、录制 L321、能力探测 L426。
11. [Appium 新 monorepo 的 logcat.ts](https://github.com/appium/appium-android/blob/0f71f5ff7f1c7b1861b9ec0f48ea6c49fb35e0f1/packages/adb/lib/logcat.ts)：argv L74、缓存读取 L126、接收时间 L169；[库版本](https://github.com/appium/appium-android/blob/0f71f5ff7f1c7b1861b9ec0f48ea6c49fb35e0f1/packages/adb/package.json)；[旧仓库迁移说明](https://github.com/appium/appium-adb)。
12. Tradefed [LogcatReceiver](https://android.googlesource.com/platform/tools/tradefederation/+/2139853a80ff1058cb333efd5e9984d3e09a14ec/src/com/android/tradefed/device/LogcatReceiver.java#98)、[LogcatOnFailureCollector](https://android.googlesource.com/platform/tools/tradefederation/+/2139853a80ff1058cb333efd5e9984d3e09a14ec/src/com/android/tradefed/device/metric/LogcatOnFailureCollector.java#68)、[重连行为](https://android.googlesource.com/platform/tools/tradefederation/+/2139853a80ff1058cb333efd5e9984d3e09a14ec/src/com/android/tradefed/device/BackgroundDeviceAction.java#78)、[主机缓冲清空](https://android.googlesource.com/platform/tools/tradefederation/+/2139853a80ff1058cb333efd5e9984d3e09a14ec/src/com/android/tradefed/device/LargeOutputReceiver.java#175)。
13. [Studio 官方使用文档](https://developer.android.com/studio/debug/logcat)（2026-03-06）、[查询 parser](https://android.googlesource.com/platform/tools/adt/idea/+/ada4bc6a52c19aa65163df48d1a0691c8d8f6593/logcat/src/com/android/tools/idea/logcat/filters/LogcatFilterParser.kt#304)、[崩溃规则](https://android.googlesource.com/platform/tools/adt/idea/+/ada4bc6a52c19aa65163df48d1a0691c8d8f6593/logcat/src/com/android/tools/idea/logcat/filters/LogcatFilter.kt#318)、[进程生命周期](https://android.googlesource.com/platform/tools/adt/idea/+/ada4bc6a52c19aa65163df48d1a0691c8d8f6593/logcat/src/com/android/tools/idea/logcat/service/ProjectAppMonitor.kt#34)。
14. Studio [SaveLogcatAction](https://android.googlesource.com/platform/tools/adt/idea/+/ada4bc6a52c19aa65163df48d1a0691c8d8f6593/logcat/src/com/android/tools/idea/logcat/actions/SaveLogcatAction.kt#78)、[导入后 package:mine 的转换](https://android.googlesource.com/platform/tools/adt/idea/+/ada4bc6a52c19aa65163df48d1a0691c8d8f6593/logcat/src/com/android/tools/idea/logcat/files/LogcatFileData.kt#32)。
15. [Studio LogcatMainPanel.kt](https://android.googlesource.com/platform/tools/adt/idea/+/ada4bc6a52c19aa65163df48d1a0691c8d8f6593/logcat/src/com/android/tools/idea/logcat/LogcatMainPanel.kt)：暂停 L523、清空 L825、重启 L909。

主机归档：工作区相邻 `ciallogcat-review-2026-10-23/logcat-landscape/`。保留浏览器正文、官方分支 metadata、源码快照与各研究者报告；仓库仅新增综合说明和索引，不复制第三方代码进入产品构建。
