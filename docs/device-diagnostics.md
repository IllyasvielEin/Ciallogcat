# Read-only device diagnostics

Q08 provides on-demand observations through `ciallogcat::diagnostics` and
`ciallogcat-cli diagnose`. It does not run during normal capture unless the
caller requests it. Real-device/OEM validation remains outstanding; the current
evidence is bounded subprocess mock coverage on Linux.

```sh
ciallogcat-cli diagnose --serial DEVICE_SERIAL
ciallogcat-cli diagnose --serial DEVICE_SERIAL --buffers events,radio \
  --timeout-ms 10000 --probe-timeout-ms 2000 --max-output-bytes 16384
ciallogcat-cli diagnose --help
```

The fixed probe list, in execution order, is:

| Probe | Read-only ADB request after `-s SERIAL` |
| --- | --- |
| `adb_features` | `features` |
| `logcat_help` | `shell -T -n 'exec logcat --help'` |
| `buffer_sizes` | `shell -T -n 'exec logcat -b BUFFER … -g'` |
| `statistics` | `shell -T -n 'exec logcat -b BUFFER … -S'` |
| `prune_rules` | `shell -T -n 'exec logcat -b BUFFER … -p'` |
| `android_sdk` | `shell -T -n 'getprop ro.build.version.sdk'` |
| `android_release` | `shell -T -n 'getprop ro.build.version.release'` |
| `build_fingerprint` | `shell -T -n 'getprop ro.build.fingerprint'` |

The command list is closed. There is no arbitrary command option, no `setprop`,
and no `-c`, `-G`, or `-P` mutation. Remote command words are fixed literals or
typed buffer names. Serial is a separate host ADB argument, never part of the
remote shell command. `-n` disconnects stdin; `-T` disables a PTY. Selected
buffers default to `main,system,crash`; accepted names match capture, including
`all` alone. Prune output can describe daemon-wide policy even when buffers are
selected; it is retained as returned, without inferring per-buffer policies.

## Report and interpretation

The JSON report has `schema_version: 1`, `type: "device_diagnostics"`, serial,
requested buffers, host start/end Unix milliseconds, monotonic elapsed time,
budgets, `shell_v2`, `complete`, `cancelled`, `deadline_exceeded`, and eight
`observations`. A query failure never removes successful observations.

Each observation includes its probe name, actual arguments, `attempted`,
`status`, host ADB `exit` (code, success and description), optional bounded
`error`, start/end/elapsed times, and independent `stdout` and `stderr` objects.
Each output object contains:

- `text`: the retained prefix rendered as UTF-8, without trimming or merging.
- `retained_bytes`: original bytes retained before decoding.
- `observed_bytes`: bytes drained, including discarded excess; a lower bound
  on what the command would have emitted if interrupted.
- `truncated`: observed bytes exceeded the retained prefix.
- `lossy_utf8`: decoding replaced invalid UTF-8, including a code point split
  by the raw-byte retention limit. This is not exact binary evidence.
- `eof`: EOF was actually observed; false means output may be incomplete even
  when no observed bytes were dropped and `truncated` is false.
- `read_error`: an independent bounded pipe-read error, or null.

`status` is `success`, `permission_denied`, `unsupported`, `timed_out`,
`cancelled`, or `unknown`. For failed host exits, recognizable ADB transport
errors take precedence and remain `unknown`: for example, `error: device
'permission denied' not found` or a missing device named `unknown option`
describes device availability, not the probe's permissions or supported options.
The `adb: error: failed to get feature set:` wrapper follows the same rule.
Permission and unsupported classifications match explicit error statements on
individual stdout/stderr lines, after known tool/error prefixes, rather than
substrings anywhere in the output. Whole permission reasons (including the final
reason in `failed to …: Permission denied`) and option-error statements such as
`logcat: unrecognized option '--help'` are recognized. Quoted arguments and
incidental mentions do not establish the cause; unfamiliar/localized diagnostics
remain unknown. Preserve the raw observations when presenting classifications.
If a stream is truncated or has not reached EOF, its unterminated final line
does not participate in error classification. Earlier LF-terminated lines
remain usable. A complete stream may end with an error reason without a final
LF. Retained text, byte counts and truncation flags are unchanged by this rule.
`exit.code` is null for signal termination or absent process status; `exit`
itself is null if spawning failed or a probe was not attempted. After total
timeout/cancellation, remaining probes are still included with `attempted:
false` and the relevant status. A per-probe timeout permits later probes to run
while the original total deadline still has time remaining.

`shell_v2` is `advertised` only after a successful, nonempty, untruncated,
valid-UTF-8 features response containing the exact token. Otherwise it is
`not_advertised` for a complete valid response lacking the token, or `unknown`
when the observation is insufficient. Host pipes are always separate. Without
advertised shell_v2, ADB can merge **remote** stderr into host stdout and may
not propagate the remote command's exit status. In that case an ADB exit 0
alone does not prove the remote command succeeded. The report does not silently
claim remote separation or upgrade an unknown capability.

`complete` means every probe had a successful host exit, reached both EOFs,
and retained all observed text without decoding loss or read error. It is not
a certificate of device support, permission, capture completeness or device
health. Missing `nsec`, `--proto`, or other entries in actual help remains an
absence of evidence, **not proof of unsupported functionality**. SDK level and
build properties are observations; they cannot establish all OEM behavior.
The report retains actual help and buffer/statistics output without inventing
normalized capacities from SDK level or a hard-coded Android version table.

`-g` reports device logd storage. The host capture queue, GUI retention limits,
native/JSONL file-byte budgets, and diagnostic output-prefix budgets are separate
resources. They must not be presented as the device buffer capacity. Statistics
and prune output also do not prove all original device logs were retained.

## GUI workflow

Open **采集设置 → 只读设备诊断** after selecting a device. Collection runs in a
background worker with cancellation; closing the report or changing the source
cancels the owned collection. Each probe exposes its status, bounded stdout/stderr, and truncation.
**复制诊断报告** copies the JSON report, which also records per-probe timestamps
and exact arguments. Host retention/capture budgets are displayed separately from device
logd buffer capacity.

A completed report is attached to subsequent native snapshots only when its
serial matches the retained live source. Changing to an unrelated device or
opening a file cannot attach the old report to that new source. Reports can
make headers larger than 64 KiB; native loading and queries share the 8 MiB
encoded-line cap.

## Core API for GUI workers

```rust,no_run
use ciallogcat::{diagnostics, model::LogBuffer};
use std::{path::PathBuf, thread};

let adb = PathBuf::from("adb");
let serial = "DEVICE_SERIAL".to_owned();
let buffers = LogBuffer::defaults();
let options = diagnostics::DiagnosticOptions::default();
let cancel = diagnostics::CancellationToken::default();
let worker_cancel = cancel.clone();
let worker = thread::spawn(move || {
    diagnostics::collect(&adb, &serial, &buffers, &options, &worker_cancel)
});
// A close, refresh or device change may call cancel.cancel() from the UI.
// Receive/join only after the worker finishes; do not block the GUI event loop.
let report = worker.join().unwrap().unwrap();
assert_eq!(report.observations.len(), 8);
```

`collect` is synchronous and requires no `BackendEvent` change. The caller owns
its worker and result channel. Keep the original serial/buffers or a request
generation with the result so a late response cannot replace a newer device's
view. A fresh cancellation token is needed for each request; cancellation is
sticky and tokens can be cloned across threads. Use `cancel()` before closing
the view or replacing the request, then let the worker finish cleanup.

Typed buffers must be nonempty, unique, and contain `All` alone if present.
Serial must be nonblank, at most 1024 bytes, not begin with `-`, and contain no
control characters. Invalid inputs fail before spawning. Launch/command/pipe
failures are observations inside an otherwise successful `collect` result.

Default options are a **10-second total** monotonic deadline, **2 seconds per
probe**, and **16 KiB for each output stream per probe**. Both timeout durations
must be positive and at most 60 seconds; each stream cap is 1–65536 bytes.
The total deadline is computed once at `collect` entry and covers all probes,
waiting and normal pipe draining. It is never refreshed between probes.

Both pipes are polled with bounded reads: POSIX `poll` or Windows
`PeekNamedPipe`, with no blocking reader threads. The loop checks cancellation
and deadline between reads, even for continuous stderr. Excess output is
discarded while pipes continue draining. On stop, the owned child is killed and
reaped, immediately available tail bytes are drained in a fixed bounded pass,
and pipes close. OS spawn/kill/reap latency can extend wall time slightly beyond
the collection deadline; the API does not claim a real-time OS guarantee. It
does not wait indefinitely for descendant-held pipe EOF or kill unrelated PIDs.

The report's `max_json_bytes` is a conservative serialized bound:
`8 probes × 2 streams × per-stream cap × 6 + 262144`, including JSON escaping,
bounded error/argument metadata, invalid-UTF-8 replacements and the final LF.
CLI serialization checks this bound before writing. Output caps bound retained
evidence and response size, not total bytes ADB may produce before the deadline.

CLI exit 0 means the report was produced, including partial failures/timeouts;
inspect statuses. Invalid arguments exit 2, unavailable ADB discovery exits 4,
and output errors exit 5. An explicitly supplied missing executable produces
per-probe unknown launch failures. Ctrl-C emits the partial JSON report after
owned-child cleanup and exits 130 with a JSON interruption error on stderr.

## Validation

The Python fixture strictly allowlists the diagnostic commands and rejects
mutating or injected commands. Actual CLI subprocess regressions cover normal
output, permission/unsupported/unknown errors, retained stdout on a failed
query, missing-device errors with quoted serials containing permission/option
phrases, error statements on either stream, incidental/localized phrases,
help on stderr, independent truncation, invalid UTF-8, shell_v2 token
boundaries, per-query and total deadlines, and Ctrl-C during silence or endless
stderr. Lifecycle tests signal only the CLI and assert all invoked ADB children
have been reaped. The fixture has an independent eight-second watchdog so a
broken implementation cannot leave tests running forever.

```sh
cargo test --locked --no-default-features --test cli_integration device_diagnostics
python3 -m unittest discover -s tests/fixtures -p test_fake_adb.py
cargo clippy --locked --no-default-features --lib --bin ciallogcat-cli --test cli_integration -- -D warnings
```

Mock Linux execution does not establish Windows runtime or real-device behavior.
In particular OEM help/format differences, remote shell_v2 channel behavior,
restricted logd permissions, and real-device buffer/statistics content still
need manual device validation.
