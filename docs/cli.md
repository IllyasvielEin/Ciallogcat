# ciallogcat-cli

`ciallogcat-cli` is a headless frontend for Ciallogcat's shared Rust parser,
ADB capture, and message/filter code. It is a separate executable
from the desktop `ciallogcat` application. It does not require a display server, graphics drivers,
eframe, or egui. ADB is installed separately.

## Install and build

The release workflow packages the headless CLI for **Linux x86_64** and
**Windows x86_64**:

- `ciallogcat-cli-v<VERSION>-linux-x86_64.tar.gz`
- `ciallogcat-cli-v<VERSION>-windows-x86_64.zip`
- `ciallogcat-cli-v<VERSION>-linux-x86_64.deb` (Ubuntu 24.04, amd64)

Each portable archive contains a versioned directory with `ciallogcat-cli` (or
`ciallogcat-cli.exe`), `LICENSE`, this guide as `README.md`, the Agent query guide,
and AOSP protobuf attribution/license files. Verify the
accompanying `.sha256` file before extracting. The version follows the release
tag; for example, a `v1.0` tag build uses `ciallogcat-cli-v1.0-…`.
The GUI ZIP/tar.gz/DEB names are unchanged. There is no standalone core-library
release artifact. Linux builds use Ubuntu 24.04; older systems are not guaranteed
compatible.

Install the CLI DEB with `sudo apt install ./ciallogcat-cli-v<VERSION>-linux-x86_64.deb`.
It installs `/usr/bin/ciallogcat-cli` and its own license/guide under
`/usr/share/doc/ciallogcat-cli`, with no desktop launcher, icon, or GUI binary.
Runtime dependencies are determined from the headless binary by `dpkg-shlibdeps`.
ADB is recommended, matching the GUI package; use `--no-install-recommends` if
Android SDK Platform-Tools already supplies it. ADB is still needed for capture.
The GUI and CLI DEBs can coexist. Remove just the CLI with
`sudo apt remove ciallogcat-cli`; session directories remain where you created them.

Build just the CLI, without GUI dependencies:

```sh
cargo build --locked --release --no-default-features --bin ciallogcat-cli
./target/release/ciallogcat-cli --help
./target/release/ciallogcat-cli --version
```

On Windows, use `target\release\ciallogcat-cli.exe`. Windows source builds
require the MSVC Rust toolchain and Visual Studio C++ Build Tools. Linux CLI
builds need the Rust toolchain and a C linker; the GUI's X11/Wayland development
packages are unnecessary.

Install Android SDK Platform-Tools, enable USB debugging, and authorize your
computer on the device. Use `devices` to find a serial before capturing. An
unauthorized or offline device cannot produce a usable capture.

## Recommended query workflow

Use `query-v2` for both legacy v1 and native v2/v3 files or sessions. The legacy
`query` command remains compatible with sequence-based v1 callers; it now also
reports `complete` and `stop_reason`. Prefer offset cursors to repeated prefix scans.

```sh
# One bounded JSON preview page for agents; continue next_cursor while has_more.
ciallogcat-cli query-v2 --session-dir ./capture-session --query timeout --ignore-case
# A finite snapshot, scanned to the end automatically; no truncated previews.
ciallogcat-cli query-v2 --file capture.jsonl --all --format text --regex --query 'timeout|failed' --tag Network --pid 1234 --min-level W
# Full decoded JSONL on stdout, completion metadata on stderr.
ciallogcat-cli query-v2 --session-dir ./capture-session --all --format jsonl > matches.jsonl 2> summary.json
# Host receipt time, not yearless device display time (new captures).
ciallogcat-cli query-v2 --session-dir ./capture-session --all --since 5m --until 30s
```

`--all` fixes a snapshot and internally follows signed offset cursors until
`complete=true`, including empty matching pages. It does not wait for future data.
The default format with `--all` is `jsonl`; explicit `--format json` and tail mode
are rejected. Without `--all`, text/JSONL returns one page and writes its cursor
and completeness metadata to stderr. `--limit` and scan/response budgets remain
**per-page**, not a total result limit; streamed full records are not bounded by
the preview response budget. Memory is bounded by a page and a single record/view,
not by capture size or total matching rows. `complete` means the query boundary
was scanned, not that the device capture has no gaps or has stopped.

Text is a normalized readable view (multiline messages remain multiline).
By default its timestamp is readable local time, for example
`2026-10-28 11:43:20.123+08:00`. `--time-basis` selects the same
clock for display and filtering: `received` (default) or exact protobuf `device`.
The summary always identifies the clock; normal default `received` timestamps omit
a repetitive per-row label, while non-default `device` timestamps retain `[device]`.
Exceptional missing-time/calendar/timezone labels remain explicit, including their clock. Text-only `--time-format readable|epoch|rfc3339` and
`--timezone local|utc` control presentation; they may change between cursor pages
without changing the filter. Epoch ignores timezone and uses integer seconds
plus the available fraction (three digits for host milliseconds, nine for proto
nanoseconds), never floating-point conversion. RFC3339 uses `T` and an explicit
offset or `Z`; readable uses a space. The local zone is resolved once per command,
including across `--all` pages, and observes the offset/DST at each instant.

If an old record has no selected absolute clock, text prints an explicit
`received-time-unavailable` / `device-time-unavailable` label with quoted original
device text; it does not guess a year or timezone. Time **filtering** still fails
explicitly when otherwise matching records lack that clock. Out-of-calendar-range
epochs retain their exact numeric value with `calendar-unavailable`. Failed local
zone discovery explicitly falls back to UTC; historical second-resolution offsets
that RFC3339 cannot represent are likewise shown in UTC with a note, not rounded.
`--time-format`, `--timezone` and `--process-width` are rejected outside `--format text`; JSON/JSONL,
the frozen protobuf projection, archives and desktop clipboard output are unchanged.

Text process names default to a maximum of **32 display columns**, with a visible
middle ellipsis retaining the beginning and end (useful for Chromium renderer
sandbox suffixes). `--process-width N` chooses 1–4096 columns; `0` retains the full
name. Grapheme clusters are not split; wide characters count by display width.
This is deterministic for terminal and redirected output, not a terminal-size guess.
It is only a presentation abbreviation, not a unique process identifier: PID/TID
remain present, and JSONL/get retain full names. Messages are never shortened.
The stderr summary's `text_presentation` reports `process_width` and the number of
successfully written rows with shortened names (`process_names_truncated`).

The text writer adds a record-ending LF only if the message does not already
end in LF. Existing LF/CRLF, internal newlines, trailing spaces and intentional
multiple trailing newlines are preserved; it does not trim message evidence.
JSONL contains full decoded JSON including extensions; compact v3 decoded output
is a **derived view, not a native archive**. Neither format is raw device output.
A late validation, time-data, or write error may leave partial stdout; check the
exit status and the final JSON summary on stderr before treating an export as
complete. For pipelines needing transactional files, write a temporary file and
rename only after successful completion. Existing source files are never changed.

Filters combine with AND. `--query` remains literal and case-sensitive by default;
`--regex` enables Rust regex syntax, `--ignore-case` affects message matching only,
and `--case-sensitive` explicitly selects the default. `--tag`/`--tag-include`
and `--tag-exclude` use case-sensitive literal substrings as in the GUI.
`--package` uses the shared GUI process/package predicate when that metadata is
stored; `--pid` and `--tid` match exact unsigned IDs. `--min-level` accepts
V/D/I/W/E/F or full names. Metadata filters never match placeholder fields of
marker/unparsed records. Each text filter is limited to 4096 UTF-8 bytes.

Query `--since` (inclusive) and `--until` (exclusive) accept RFC3339 timestamps
with explicit timezone (e.g. `2026-10-28T02:00:00Z`) or integer ages such as
`30s`, `5m`, `2h`, `1d`. Relative bounds anchor once to host UTC and are retained
in the cursor; keep the original filter options unchanged when continuing.
Every successful query page and streaming summary includes `time_window`, even
when zero records match. It reports `basis`, `anchor_unix_ms` (null without a time
filter), `since_inclusive`, and `until_exclusive`. A bound contains exact signed
`unix_ns` as a decimal **string** and UTC `rfc3339` (null outside supported calendar
range or RFC3339 years 0000–9999). A null bound means no filter bound: absent `--until` is not silently replaced
with the anchor or snapshot end. These are resolved query parameters, **not** an
assertion about available data coverage or the reason for no matches. Relative
bounds remain anchored across cursor pages and `--all`; inspect them before
concluding that a zero-result time window means no older records exist.
The snapshot byte/sequence fence remains independent of the time window.

`--time-basis received` (default) uses persisted host `observed_unix_ms`;
`--time-basis device` requires the exact protobuf epoch. An otherwise matching
record without the requested time returns `time_unavailable`, not an invented
year/timezone or a silent omission. New default v1 captures add optional `observed_unix_ms`, sampled when the ADB
adapter observes the record, before queue/backpressure waiting (the same clock
as native provenance). This is host UTC receipt time, not device event time;
host clock adjustments remain observable. It does not create a v1 session
identity. Older v1 records lacking this optional field remain readable but
return `time_unavailable` for receipt-time filtering; ordinary text logcat
timestamps are not absolute instants. Explicit null also means unavailable;
non-integer, negative, or out-of-u64 timestamp values are invalid records. Capture/start `--since` retains
its separate device-logcat history syntax; it is not this offline query option.

## Commands

Run `ciallogcat-cli --help` for the argument reference, or `query-v2 --help`,
`get-v2 --help` and `context-v2 --help` for focused source, budget and pagination
instructions, including defaults and inclusive ranges. `-h` is also supported;
help is plain text on stdout and requires neither ADB nor a source file.
Control responses are JSON on stdout; diagnostics go to stderr.
Captured records are JSONL: one JSON object per line. `capture --output -`
(the default) streams only record JSONL to stdout for pipelines; its capture
summary goes to stderr. An output path writes the records to that file.

```sh
ciallogcat-cli devices
ciallogcat-cli capture --serial DEVICE_SERIAL --output capture.jsonl --duration-secs 30 --max-bytes 67108864
ciallogcat-cli start --serial DEVICE_SERIAL --session-dir ./capture-session --duration-secs 300 --max-bytes 67108864
ciallogcat-cli status --session-dir ./capture-session
ciallogcat-cli query --session-dir ./capture-session --after-seq 0 --limit 100 --query "timeout"
ciallogcat-cli stop --session-dir ./capture-session
```

`capture` runs in the foreground. `start` launches a local, finite background
capture worker. Session directories must be new; existing files are not silently
overwritten. `status`, `query`, and `stop` use the session directory rather than
a network service. Query records in pages, using their sequence positions to
avoid repeatedly returning the same records. Optional `--query TEXT` matches a
case-sensitive literal substring in the message, using the shared filter
semantics; it does not search tags and is not a regex. Capture defaults to the
same `main`, `system`, and `crash` buffers as the GUI. `--buffers LIST` selects
comma-separated `main,system,crash,events,radio`, or `all` alone. Names must be
exact, with no empty tokens or duplicates. There are no package, level, or regex
capture flags; message filtering is available through query commands.
`--limit` defaults to 100 (maximum 10000). Query returns an `entries` array,
`next_after_seq`, `has_more`, `partial`, `scan_limited`, `complete`, and `stop_reason`. Pass
`next_after_seq` back as `--after-seq` for the next page, including when matching
returns no entries. The cursor advances over scanned nonmatches. Per-call scan
limits are 16 MiB / 100000 candidate records after the cursor, with an
8 MiB returned-record budget, plus the JSON envelope. `scan_limited` reports a candidate-scan budget being
reached. These limits do not bound total input read: each query rereads and
parses the earlier prefix from the start of the file, with no index, so later
pages can read up to the entire configured capture size (default 1 GiB,
maximum 1 GiB). Query cost grows with that prefix. `partial` means a trailing
incomplete JSONL record was skipped and can be retried after the writer finishes.

The legacy `query` command reads v1 sessions only. If `logs.jsonl` begins with a
valid native v2/v3 header, it exits 3 with JSON error code `unsupported_format` and
directs you to `query-v2 --session-dir DIR` using the same directory. This is a
command/format mismatch, not evidence of file corruption. Malformed v1 records
retain `invalid_session` errors. See the [v2 query contract](agent-query.md) for
bounded responses, scan budgets and reference-based retrieval.

Both duration and output-byte bounds remain active for each capture:
`--duration-secs` defaults to 600 (maximum 86400), and `--max-bytes` defaults
to 1073741824 (1 GiB; also the maximum). This is a disk/output cap, not a RAM allocation, and reaching it stops capture; no rolling overwrite is introduced. Both must be positive. Optional
`--max-entries` lowers the positive record bound (default and maximum 10000000). A v1 session ends when a bound is reached, ADB
exits, a stop is requested, or an error occurs. V2 applies bounded reconnects as described below. This is a bounded capture tool,
not an indefinitely running daemon. Foreground Ctrl-C requests graceful
shutdown and stops the CLI's owned ADB child. It does not install a service, autostart
entry, remote endpoint, or HTTP server.

### Device history and finite snapshots

`capture` and `start` accept the same history options for v1/v2/v3 and their supported transports:

- No history options: request the most recent record, then follow (`-T 1`).
- `--tail N`: request the most recent N records, then follow (`-T N`). N is an
  ASCII integer in `1..=2147483647`; it is independent of `--max-entries`.
- `--since TIME`: request records from the specified **device time**, then
  follow (`-T TIME`). It is mutually exclusive with `--tail`.
- `--dump`: take a finite snapshot. Without `--tail`/`--since`, request all
  available history (`-d`); with either selector, use `-t N` or `-t TIME`.

TIME accepts `MM-DD HH:MM:SS.fraction`, `YYYY-MM-DD HH:MM:SS.fraction`, or Unix
`SECONDS.fraction`, with 1–9 fractional digits. Quote calendar timestamps as a
single argument. Calendar fields are validated, and no host timezone conversion
is performed; omitted years are interpreted by the device. Unix seconds must
include the decimal point: integer `1792742400` would be ambiguous with a count
and is rejected. Signed numbers, shell expressions and extra arguments are
rejected. Device logcat support still determines whether a valid request succeeds.

```sh
ciallogcat-cli capture --serial DEVICE_SERIAL --tail 500 --duration-secs 30
ciallogcat-cli capture --serial DEVICE_SERIAL --since '10-23 12:30:00.000' --record-format v2 --output since.jsonl
ciallogcat-cli capture --serial DEVICE_SERIAL --dump --tail 2000 --buffers main,crash --record-format v2 --output snapshot.jsonl
ciallogcat-cli start --serial DEVICE_SERIAL --session-dir ./history --dump --since 1792742400.0 --record-format v2
```

Host duration, record and byte budgets apply during both history retrieval and
following. They can stop a snapshot before device EOF. Clean snapshot EOF after
the child is reaped and readers drain reports `reason: "snapshot_complete"` and
does not reconnect; nonzero exits and reader errors retain error/interruption
handling. This confirms the requested dump ended, not that logd retained every
original record. Native completeness remains conservative. The v2/v3 header records
`source.capture_scope` (typed mode/start), `source.logcat_scope_args`, actual
`source.requested_buffers`, and `source.host_budgets`. A resumed follow attempt
reissues its selected history request; there is no cross-attempt deduplication.

A successful `start` reports `adb_process_started`: the worker and ADB process
started. It cannot prove
that the Android device's log daemon is ready or that records will arrive.
Check `status` and query the output. Silence is not proof of a healthy device.
`stop` requests cooperative shutdown through the session directory; the worker
owns and reaps its ADB child rather than killing an arbitrary process by PID.
`stop --wait-secs N` waits for acknowledgement (default 10 seconds, range 1–30).
A stale heartbeat becomes `state: "unknown"` with `stale: true`; it is not proof
that a process is alive or dead, and stop does not signal a stored PID. Even a
stale session receives the cooperative stop marker and gets the normal wait;
without acknowledgement the command exits 6 (`timeout`), not a claim of death.

For an orphaned session after an upgrade/crash:

```sh
ciallogcat-cli stop --session-dir ./old-session --force --wait-secs 1
```

`--force` first requests cooperative shutdown and waits. If no terminal status is
acknowledged, it creates `management-closure.json` atomically and reports
`state: "abandoned"`, `reason: "administratively_closed"`,
`worker_exit_confirmed: false`, and `management_closed_unix_ms`. This is a terminal
**management view**, not a kill operation or proof that the worker/ADB has exited.
A genuine completed/failed status is returned unchanged; repeated or concurrent
force calls reuse the first closure. No persisted PID is ever signalled.

The sidecar freezes the last observed published bytes/entries (native
`visible_end`) and preserves the original status snapshot. It never overwrites
`status.json`, `status.next`, `worker.lock` or logs, repairs a partial log, fabricates
a footer, or restarts a writer in the same directory. A stale lock file or abandoned
`status.next` is not proof of death and is left intact. Publication uses a fully
written unique temporary file and atomic first-writer-wins hard linking; unsupported
filesystems/permissions fail closed without a non-atomic overwrite fallback.

`status` and session-based query/get/context commands in v1.2.1+ honor the immutable
closure even if an old worker later publishes a newer status or appends records.
The legacy `query` also caps reads to the frozen prefix. Existing references/cursors
within that boundary remain usable; ones whose snapshot exceeds it are rejected
rather than silently expanded/truncated. A query racing closure may fail and should
be reopened against the frozen session. Streaming may already have emitted a
partial prefix, including bytes beyond a concurrently frozen boundary, before
the final consistency check reports an error; discard that failed output. Only
a successful query and its completion summary establish the stated boundary. `complete` means this query boundary was
scanned, not complete capture or confirmed process exit. Native sessions remain
queryable without a footer; the `--file` offline validation contract is unchanged.
Older CLI binaries and direct/raw file readers may not honor the sidecar. Keep
using `--session-dir` for the frozen administrative view; the underlying file is
untouched and may still grow if an old worker is alive.

## Read-only device diagnostics

`diagnose --serial DEVICE_SERIAL` returns a bounded JSON report of actual
logcat help, buffer sizes (`-g`), statistics (`-S`), prune rules (`-p`), ADB
shell_v2 advertisement, and Android build/API properties. It never clears logs
or changes device configuration. `--buffers` has the same names/defaults as
capture. Use `diagnose --help` for focused help.

```sh
ciallogcat-cli diagnose --serial DEVICE_SERIAL --buffers main,system,crash \
  --timeout-ms 10000 --probe-timeout-ms 2000 --max-output-bytes 16384
```

The total deadline defaults to 10 seconds, with at most 2 seconds per probe;
both options accept 1–60000 milliseconds. `--max-output-bytes` defaults to
16384 (range 1–65536) **for each stdout and stderr independently, per probe**.
Each observation retains output, exit/status, truncation and host observation
times. Failed probes do not discard other results; exit 0 means the report was
produced, so inspect `complete` and each observation's status. Ctrl-C emits
partial JSON after child cleanup and exits 130.

Absent help entries do not prove an option is unsupported, and SDK level does
not establish every OEM capability. Without advertised shell_v2, remote stderr
and exit status may not remain separate even though host pipes do. Device logd
capacity is distinct from host capture queues, GUI retention, file budgets and
these diagnostic output caps. See the [diagnostic API and report contract](device-diagnostics.md)
for statuses, cancellation, serialized response bounds and mock-only validation.

## Native capture and Agent queries

Use `--record-format v2` on `capture` or `start` to record a native document.
The default remains v1 for existing scripts. V2 preserves record kinds, stable
session/sequence references, host observation times and text-loss observations;
its header and final footer let the GUI open the same evidence file. The v1
`query` command does not read v2/v3 files: use the [Agent query commands](agent-query.md).

```sh
ciallogcat-cli start --serial DEVICE_SERIAL --session-dir ./case-001 --record-format v2 --duration-secs 120
ciallogcat-cli query-v2 --session-dir ./case-001 --mode tail --query timeout
ciallogcat-cli stop --session-dir ./case-001
ciallogcat-cli query-v2 --file ./case-001/logs.jsonl --mode snapshot
```

V2 and V3 share the same capture pipeline, which retries unexpected stream exits at 1, 2 and 4 seconds, at most three times;
30 seconds of stable running restores the retry allowance. One session UUID and
increasing sequence survive reconnects. `attempt` and `segment` distinguish
streams. The owned child must be reaped and both readers drained before a new
child starts. Stop or the total duration cancels backoff. There is no automatic
history recovery or deduplication across interruptions; the footer records
aggregate interruptions and known host discards. A partial protobuf frame
observed concurrently with an intentional stop is recorded as
`incomplete_frame_at_stop`; its cause is not attributed to the stop with certainty.
Other decoding or cleanup failures remain errors. `finalized: true` only confirms
that this artifact ended consistently, not that capture was complete.

All attempts share the original duration, entry and byte limits. V2/V3's minimum
`--max-bytes` is 8192; header and footer bytes count toward it, and 4096 bytes are
reserved for the bounded footer. Metadata does not count as a log entry.
Each footer diagnostic is limited to 1024 encoded JSON bytes, including JSON
escaping, with an explicit truncation flag. This preserves space for the
aggregate capture facts even when diagnostics contain control characters.
The status adds `record_schema_version: 2|3`, `artifact_id`, `session_id` and
`visible_end: {seq, offset}`, `capture_completeness`, `stream_interruptions` and
`last_stream_error`. Completion of the bounded command can coexist with a
partial source capture; inspect these observations, not only the exit code.
The position pair advances only after a complete JSONL
packet is flushed. A footer advances offset while preserving the last sequence.
Output errors preserve the last confirmed prefix and may leave an unfinalized
file; active-session queries can still inspect that published prefix. GUI
complete-file import rejects missing footers. Readiness still only confirms
that the host ADB process started; the first query may legitimately be empty.

An explicit experimental transport is available with
`--record-format v2 --transport proto`. It requires target `shell_v2` support and
uses `adb shell -T -n` with separate stdout/stderr and no PTY. Unsupported devices
or malformed frames fail without silent text fallback. It preserves bounded
protobuf payloads and original tag/message bytes as base64, independently of
normalized display text. AOSP Android 15+ has the schema; vendor and host-channel
compatibility still needs device validation. See [protobuf findings](logcat-protobuf.md).

Explicit `--record-format v3 --transport proto` records compact native v3: each
proto payload is stored once and display fields are derived with the frozen
`aosp_logcat_16r1_host_v1` projection. V3 with text (including an omitted
`--transport`) is rejected before capture. Defaults remain v1/text; explicit
v2/text and v2/proto retain their existing storage contracts. V3 shares the v2
history options, total budgets, retry policy, flush-before-publication watermarks
and status handling.

```sh
ciallogcat-cli start --serial DEVICE_SERIAL --session-dir ./compact --record-format v3 --transport proto --duration-secs 120
ciallogcat-cli query-v2 --session-dir ./compact --mode tail --start-at end
ciallogcat-cli query-v2 --session-dir ./compact --mode tail --cursor '<next_cursor>'
ciallogcat-cli get-v2 --session-dir ./compact --ref '<record_ref>' --view decoded
```

`query-v2`, `get-v2` and `context-v2` retain their API names and response version
2, and report `source_schema_version: 3` for v3. `--start-at beginning|end`
defaults to beginning; end requires a native v2/v3 session in tail mode without
a cursor. It returns an empty page at the published boundary, with the skipped
prefix explicitly unscanned. Continue with the returned cursor.

`get-v2 --view stored` is the default and returns the original archive JSON line.
`--view decoded` returns full structured JSON, including complete message,
metadata and extensions, marked as a derived view rather than archived bytes.
It is not a v3 archive record for writing back. Join UTF-8 chunks using
`next_text_offset`, keeping the same view. Stored lines retain the 8 MiB limit;
decoded views have a separate 64 MiB cap to allow escaped protobuf text to expand.
Every response still obeys `--max-response-bytes`. See the
[query contract](agent-query.md) for bounds and validation semantics.

## Integrate with an existing test executor

[examples/agent_capture.py](../examples/agent_capture.py) starts a finite v2
session (fixed record_schema_version=2; it does not accept or create v3),
invokes an explicitly supplied command without a shell, stops capture
in `finally`, and returns a schema-2 workflow report with test outcome, capture
state, action boundaries and a bounded evidence window separately. It establishes
the pre-action cursor directly at the published tail; earlier history is not
claimed as scanned. The executor can be Maestro, an instrumentation runner, or
another existing device tool. The tool owns device actions; Ciallogcat owns logs.

```sh
python3 examples/agent_capture.py --serial DEVICE_SERIAL --session-dir ./case-002 --query timeout -- maestro test flow.yaml
```

The executor's output streams to stderr. JSON on stdout contains at most 20
record previews and continuation/reference tokens; use `query-v2`, `get-v2` and
`context-v2` to request more. A test passing does not imply capture succeeded.
The example reaps the direct executor and, on POSIX, terminates its private process
group. On Windows, descendants remain the executor's responsibility. Device cleanup
always belongs to the supplied executor. See the [workflow contract](agent-test-workflow.md)
for deadlines, failure tail and the difference between host action times and device logs.
Open `case-002/logs.jsonl` in the GUI after a finalized stop to inspect the same
session/sequence identities. This example adds no resident service or model API.

## JSON and exit codes

Existing control responses and v1 records use `schema_version: 1` and a `type` discriminator.
V2 record files and the new query responses explicitly use version 2.
Entry fields include `seq`, `time`, `pid`, `tid`, `level`, `tag`, and `message`.
Status includes `state`, `readiness`, `entries`, `bytes`, `reason`, and `stale`.
The current writer publishes `entries` and `bytes` together after a complete
JSONL record is written and flushed from user-space buffers. A partial write or
failed flush does not advance that confirmed prefix. This is visibility to
other processes, not a per-record disk-sync guarantee. Existing v1 `query`
still scans the file and reports an incomplete tail; it does not yet implement
fixed-snapshot pagination using the status boundary, except that administratively
closed sessions are strictly capped at their immutable published prefix.
Normal completion reasons include `max_entries`, `max_bytes`, `duration_limit`,
`stopped`, and `adb_exit`; a failed capture reports its error instead.
`omitted_by_limit: true` means a configured output/entry limit was reached and
capture may be truncated; it does not prove additional records existed.
`discarded_entries` counts backend records received but not confirmed written,
including structural and fallback records represented as v1 entries. It
excludes unread device records and cannot establish end-to-end
losslessness. Diagnostics retain only the last line; `diagnostic_dropped_count`
is the number of earlier received diagnostic lines no longer retained.

Errors on stderr have `type: "error"` and an `error` object containing `code`
and `message`. Do not treat a successful status/query command as proof that
the capture itself succeeded: inspect its state and reason.

- 0: command completed
- 2: invalid command or arguments
- 3: invalid, failed, or stale session
- 4: ADB discovery, launch, or capture error
- 5: filesystem or output error
- 6: timeout
- 130: foreground capture interrupted with Ctrl-C after owned ADB cleanup

Help is human-readable text; version output is JSON. Scripts should parse JSON
fields rather than match diagnostic prose.

## Safety and limits

- Keep session directories private. Unix session directories use mode 0700
  and created files use 0600; Windows uses inherited filesystem ACLs. Choose a
  private parent directory. Device identifiers and captured application logs
  may contain personal information, credentials, or business data. Review and
  redact them before sharing.
- ADB is external; the CLI does not grant device authorization or bundle ADB.
- Byte limits bound serialized record output, not raw device log size, total
  process RSS, or total session-directory size. Metadata and temporary
  processing also consume space.
- Records are normalized parsed output, not forensic raw capture. Invalid
  UTF-8 is replaced; the shared core's 1 MiB input-line limit and truncation
  notices apply. Nonempty undecodable lines are retained as fallback records.
  V1 preserves its existing entry shape, so it cannot distinguish those lines
  from structural markers or carry the core's process-hint provenance.
  There is no guarantee that every device record is captured during startup,
  process discovery, disconnection, or shutdown.

The shared `record_io` codec reads this v1 format with an 8 MiB encoded-record
limit and validates required fields and increasing sequences. Sequence gaps
are permitted, and unknown optional fields are preserved. The query now rejects
incomplete or wrongly typed required v1 fields that the old permissive reader
could pass through; records produced by the CLI retain their original shape.
Invalid complete records report their line/byte position; no
malformed line is silently skipped. Explicit offline readers accept a valid
last record without a newline, as allowed by JSON Lines. Active session queries
keep the existing `partial` behavior for a non-newline-terminated tail. V1 has
no completeness footer, so a valid file alone does not prove continuous capture.
- Mock-based tests and release smoke tests do not establish real-device
  reliability, prolonged memory stability, or compatibility with every host.

## Validation and packaging

The separate `Checks` workflow runs on pull requests, pushes to `main`, and
manual dispatch. Its Linux headless job runs the no-default-feature tests and
Clippy, fake-ADB Python self-tests, and CLI packaging contract tests without GUI
development packages. A Linux GUI job covers desktop behavior and default-feature
Clippy; Windows runs default-feature tests and Clippy, then builds and smoke-tests
the headless CLI. Formatting runs once. Jobs have timeouts and retain available
fixture diagnostics for seven days on failure. These checks do not publish packages.

The normal release workflow runs only on version-tag pushes. It has
separate GUI and headless CLI Linux/Windows jobs. The CLI job does not install
GUI development packages and runs headless tests, Clippy, the explicit CLI
release build, `--help`/`--version` smoke checks, and packaging contract tests.
Linux fake-ADB integration tests exercise capture/session process lifecycles.
Windows also runs the shared native Rust fake-ADB integration suite, covering
binary pipes and owned-child lifecycle. The current local evidence includes
Linux execution and MSVC cross-compilation; actual Windows runner execution
and real-device behavior remain separate verification.
Release publication waits for both GUI and CLI jobs. It uploads CLI archives, the Linux CLI DEB, and
checksums alongside GUI assets; it never uploads Cargo `.rlib` files.
DEB dependency discovery fails closed on unresolved libraries or an unverified
libc minimum. A clean Ubuntu 24.04 container checks installation, runtime linkage,
help/version, and removal without installing the package on the build host.

A separate, manually dispatched `Supplement v1.0 CLI Debian package` workflow
can append only the missing v1.0 CLI DEB and checksum to the existing public
release. It builds the headless binary from the pinned original v1.0 commit,
verifies that current Cargo/source inputs are unchanged, uses the new packaging
helper, and checks coexistence with the published GUI DEB. It never moves the
v1.0 tag, replaces published assets, or republishes the release. Existing asset
bytes must match before a partial upload can proceed.

```sh
cargo test --locked --no-default-features
cargo clippy --locked --no-default-features --all-targets -- -D warnings
python -m unittest discover -s .github/scripts -p test_package_cli.py
```

The packaging tests check Linux and Windows names and allowlists, executable
permissions in the Linux archive, DEB metadata/file allowlists/root ownership,
checksums, invalid tags/platforms, missing CLI binaries, and dependency-discovery
failures. Mock dependency fixtures do not replace actual CI dependency discovery. Creating a tag or running the release workflow is a separate,
explicit publication action.
