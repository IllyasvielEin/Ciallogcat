# 围绕外部测试失败收集日志

`examples/agent_capture.py` 把一个现有测试命令与有限的 CLI 会话组合起来。
它不执行设备点击、安装、重置或测试调度。完整可运行的 mock 和 Maestro 示例见
[examples/README.md](../examples/README.md)；Python 3.10+ 即可运行组合层自测。

## 执行过程

1. `start` 同时设置时长、记录数和文件字节预算。会话目录必须不存在，
   且调用方应独占其父目录（推荐 `mktemp -d`）。这避免多个调用方争用同一路径；
   路径存在本身不证明某个 worker 属于本次运行。
2. 在 `ready-timeout` 内确认非 stale 的 `running / adb_process_started`，并通过
   `query --mode tail --start-at end` 一次定位当前已发布水位，不扫描启动前缀。
   保存空页的 `{seq, offset}`、
   session/artifact 身份、source_status 和 tail cursor，然后才启动外部命令。
   `starting/reconnecting` 有限等待；terminal、stopping、unknown/stale 阻止动作。
   若起点已观察到 footer，同样阻止动作，即使 status 尚未更新为 terminal。
   未发布 header 的 `source_not_ready` 可以在该期限内重试。只有 header、seq=0 或
   查询没有匹配记录都合法，不需要等到第一条日志。
3. 不经 shell 运行 `--` 后的原始 argv。记录主机开始/结束 Unix 毫秒、monotonic 耗时、
   超时/中断和退出码。结束时间标记观察到退出或 deadline 的时刻；进程清理结束时间另列。
   清理后立即查询 `after_action` 发布水位；它带自己的观察时间，并不假装与动作结束原子同步。
4. 非零退出或超时后保留 `failure-tail-secs` 的有限宽限期，再请求 stop。宽限从执行器
   清理后开始，与 after_action 状态查询重叠；该查询有独立 command timeout，因此实际
   `waited_secs` 可能大于所请求宽限。已观察到 terminal 时提前停止等待，中断则直接清理。
   没有日志、没有匹配或暂时静默不会被当成失败尾部已收完。
5. `stop --wait-secs 10` 有 15 秒主机调用上限。仅使用私有会话停止协议，不向 status 中的
   `worker_pid` 发信号。若 stop 客户端被 Ctrl+C 打断，最多再尝试一次 stop，仍各限 15 秒。
   停止失败仍记录错误并尝试有期限的 status 和已发布前缀查询；收尾阶段中断保留动作结果及已收集证据。
   接着从动作前 cursor 继续读取，保留已扫描范围内最近的有限匹配记录及原始 `record_ref`。
   最后的空页不会抹掉前页的证据。

主机时间不能与设备 Logcat 时间直接比较，也不能用于证明设备事件的先后或因果关系。
`session_id/seq` 表示会话中的记录身份/采集顺序；发布水位表示完整行 flush 后的可读前缀。
动作开始前仍未发布的旧设备日志可能随后进入该范围，动作结束前的日志也可能延迟发布。
因此 `before_action` 到 `after_action` 是**主机发布观察边界**，不是精确的设备动作区间。
header/footer 可以只推进 offset 而不推进 seq。

`adb_process_started` 仅证明主机曾启动 ADB 进程。即使本次观察为 running，也不保证设备
日志服务就绪、期间不会断流或一定有日志。无日志成功不能证明设备健康。

## 预算与终止

所有 deadline 使用 monotonic 时钟，主机墙上时间回拨不会延长等待。

- 采集：`--duration-secs 150`、`--max-entries 100000`、`--max-bytes 67108864`。
  native header/footer 计入字节；所有重连共享这些预算。
- 动作：`--action-timeout 90` 秒。启动/基线共享 `--ready-timeout 15` 秒；
  普通 CLI 调用各受 `--command-timeout 15` 秒限制。动作进程清理最多再等 3 秒。
- 失败宽限：`--failure-tail-secs 2` 秒，可设为 0。采集时长必须严格大于
  `ready-timeout + action-timeout + failure-tail-secs + command-timeout + 3`，
  给启动、动作、结束观察和清理留出时间。记录/字节上限仍可能使采集提早结束，报告保留原因。
- 基线只需一次成功的 end 定位，不消耗后续 `--max-pages` 预算；未就绪时只在
  ready-timeout 内重试新的 end 定位。基线不扫描历史记录，来源验证最多读取有界首尾行/
  footer（每行 8 MiB）、两次各最多 8 MiB + 1 的反向定位及 header/anchor/缓冲余量。
  基线的 `scan.file_bytes_read` 报告最后一次定位的实际读取量，不随历史记录总量增长；
  `pages_read` 包含此前因重新观察到 reconnecting 而重试的成功响应数。
- 最终取证独享 `--max-pages 8` 页和 `--evidence-timeout 10` 秒总期限。
  每页 `--scan-records 100000`、`--scan-bytes 16777216`，并传给 CLI；
  总逻辑扫描上限为页数乘单页预算。实际文件读取另含来源验证，报告有 `file_bytes_read`。
- 每页完整 CLI 响应最多 `--evidence-bytes 32768` 字节，预览正文最多 512 UTF-8 字节。
  最终记录列表同时受 `--evidence-records 20` 和 `evidence-bytes` 限制；超出时丢掉最早预览。
  整个报告还含固定数量的状态、边界、cursor、错误与预算信息，不以此参数声明报告总长。
  执行器输出直接流向 stderr，不保存在报告内；调用方若重定向到文件，应另设输出配额。

本例通过 CLI 的 native end 起点建立位置，不根据任意 `{seq, offset}` 伪造 token。
启动历史再大也无需先用有限页数追上；基线未能在就绪期限内建立仍会阻止动作。
最终扫描到期或页数耗尽则保留已有证据、标明未完成和续查 cursor。此时记录只是**已扫描前缀
内最近的匹配**，不能称为整个失败区间最新的 N 条。空页也可能有 `has_more=true`。

## 报告与后续取证

本例的 stdout 报告使用 `schema_version: 2`。CLI 查询响应仍为 schema_version=2；
所有起点的 cursor 内部版本统一为 4，旧版 token 需重新查询取得；调用方不应解析 token。
旧示例的单一 `evidence_page/error` 改为有界的 `evidence/errors`：

- `action` 保留执行器退出、timeout/interrupted、主机时刻、耗时和 `reaped`。
- `readiness` 保存 start 返回的观察；真正用于放行动作的是
  `boundaries.before_action.source_status`。三个边界是 before_action、after_action、after_stop。
- `failure_tail` 保存请求时长、实际阶段耗时和终止原因；`capture` 是最终 stop 或恢复 status。
- `evidence.records` 保留带引用的摘要；`evicted_records` 表示因结果预算移出的匹配摘要数。
  `complete_to_observed_watermark` 只表示扫描追上最后一次查询水位，
  不表示采集无丢失、过滤结果覆盖所有日志或一定已扫描 footer。
  `artifact_finalized`、`validation`、`source_status`、`snapshot`、累计扫描量与 `stop_reason` 分别保留。
  若基线跳过非空历史，`artifact_finalized` 保持 false，即使 `validation.footer_seen=true`；
  footer 仍对照已发布的前缀统计和实际扫描后缀校验，但未验证前缀的逐条内容。
  `boundaries.before_action` 同样保存 start_at、validation 和实际来源读取量。
- `errors` 独立列出启动、基线、状态、停止或查询错误；不会把动作失败改写成采集错误。

退出 0 要求动作退出 0、未超时/中断、没有组合层错误、采集 `completed/stopped`、
`diagnostics_complete=true`、未触及采集预算，并追上最终查询水位。其他运行结果退出 1，
参数错误由 argparse 退出 2。`ok=true` 仍不证明源日志完整；要检查
`capture_completeness/stream_interruptions/discarded_entries`，其中 `unknown` 不能当作 complete。

下面的续查命令沿用同一源、tail 模式、query 和密钥目录。把报告中 token 作为一个完整参数
传入，不解析 token，也不要把 snapshot cursor 改成 tail 使用：

```sh
ciallogcat-cli query --session-dir /path/to/session --mode tail \
  --query 'same query as the wrapper' --cursor '<evidence.next_cursor>' \
  --limit 20 --max-response-bytes 32768
ciallogcat-cli get --session-dir /path/to/session --ref '<record_ref>'
ciallogcat-cli context --session-dir /path/to/session --ref '<record_ref>' --before 5 --after 5
```

`get` 用 `next_text_offset` 有界续取完整 JSON；`context` 固定在该 reference 创建时的
snapshot，不能用旧引用索取未来尾部。跨机器交接使用 `logs.jsonl` 和 `session_id/seq`，
复制工件或换密钥后重新查询 token。仅成功终结的原生文件适合 GUI 完整导入；停止失败时
session 查询可能仍能取得已发布前缀，但无 footer 的文件不会被冒充完整工件。

## 进程归属与验证范围

每次控制命令 timeout 都由 Python 杀死并 wait 其直接子进程。执行器的直接子进程在正常、
失败、超时或中断路径都被 wait；POSIX 还创建独立进程组，先 TERM，最多等 1 秒，再 KILL，
最多再等 2 秒。故意脱离进程组的子进程，以及 Windows 执行器的后代和设备状态清理，
仍由外部执行器负责。POSIX 即使命令成功，也会清理留在该组内的后台进程。

采集 worker 是 `start` 创建的后台进程，由 CLI 的 stop 确认及自身预算约束；如果 stop
未确认，报告明确失败，不能宣称它已回收。不会根据过期 PID 冒险杀进程。

`examples/test_agent_capture.py` 使用脚本化 CLI 响应与可回拨墙上时间的 fake clock，
覆盖正常、失败、timeout、无日志、就绪拒绝/重连等待、分页、停止失败和查询失败；
大历史起点不耗尽后续页数预算，后缀取证也保留未验证前缀的范围标记；
小量真实 Python 子进程验证直接子进程回收、POSIX 组终止和中断清理。
这些测试不需要真机，不替代 Windows 后代进程管理或真实 Android 的时序验收。
