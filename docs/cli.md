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
cargo build --locked --release -p ciallogcat-cli
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

Use `query` for native files or sessions. It uses the
bounded query engine, with signed offset cursors rather than repeated
prefix scans. It is the only query command; existing callers should read the
[migration notes](#migrating-query-callers).

```sh
# One bounded JSON preview page for agents; continue next_cursor while has_more.
ciallogcat-cli query --session-dir ./capture-session --query timeout --ignore-case
# A finite snapshot, scanned to the end automatically; no truncated previews.
ciallogcat-cli query --file capture.jsonl --all --format text --regex --query 'timeout|failed' --tag Network --pid 1234 --min-level W
# Full decoded JSONL on stdout, completion metadata on stderr.
ciallogcat-cli query --session-dir ./capture-session --all --format jsonl > matches.jsonl 2> summary.json
# Host receipt time, not yearless device display time (new captures).
ciallogcat-cli query --session-dir ./capture-session --all --since 5m --until 30s
```

`--all` fixes a snapshot (in tail mode: the watermark observed by this request)
and internally follows signed offset cursors until `complete=true`, including
empty matching pages. It waits for future data only with `--wait-ms` in tail mode.
The default format with `--all` is `jsonl`; explicit `--format json` is
rejected. Without `--all`, text/JSONL returns one page and writes its cursor
and completeness metadata to stderr. `--limit` and scan/response budgets remain
**per-page**, not a total result limit; streamed full records are not bounded by
the preview response budget. Memory is bounded by a page and a single record/view,
not by capture size or total matching rows. `complete` means the query boundary
was scanned, not that the device capture has no gaps or has stopped.

Text is a normalized readable view (multiline messages remain multiline).
By default its timestamp is readable local time, for example
`2026-10-28 11:43:20.123+08:00`. `--time-basis` selects the same
clock for filtering and, unless `--display-time` is supplied, display:
`received` (default) or exact protobuf `device`.
The summary always identifies the clock; normal default `received` timestamps omit
a repetitive per-row label, while non-default `device` timestamps retain `[device]`.
Exceptional missing-time/calendar/timezone labels remain explicit, including their clock. Text-only `--time-format readable|epoch|rfc3339` and
`--timezone local|utc` control presentation; they may change between cursor pages
without changing the filter. Epoch ignores timezone and uses integer seconds
plus the available fraction (three digits for host milliseconds, nine for proto
nanoseconds), never floating-point conversion. RFC3339 uses `T` and an explicit
offset or `Z`; readable uses a space. The local zone is resolved once per command,
including across `--all` pages, and observes the offset/DST at each instant.

Use text-only `--display-time received|device|both` to select labeled display
clocks independently of filtering. For example:

```sh
ciallogcat-cli query --file capture.jsonl --all --format text --display-time both
```

Received time is the persisted host observation, not device event time. Device
display uses the exact protobuf epoch when available; otherwise it shows quoted
`device-text` with `no-absolute-time; unconverted`, or an unavailable label.
Yearless device text remains unconverted even with epoch/RFC3339 formatting.
Both displays each side separately; either can be unavailable. No missing clock
silently substitutes another, and a host/device difference is not application
latency. Displaying device text does not make it usable for device time filtering.
Omitting the option preserves the previous text prefix and clock selection.
`text_presentation` reports the requested mode and effective per-record policy;
`time_window.basis` continues to describe filtering. Display options can change
between cursor pages without changing record identities or the matching set.

If an old record has no selected absolute clock, text prints an explicit
`received-time-unavailable` / `device-time-unavailable` label with quoted original
device text; it does not guess a year or timezone. Time **filtering** still fails
explicitly when otherwise matching records lack that clock. Out-of-calendar-range
epochs retain their exact numeric value with `calendar-unavailable`. Failed local
zone discovery explicitly falls back to UTC; historical second-resolution offsets
that RFC3339 cannot represent are likewise shown in UTC with a note, not rounded.
`--display-time`, `--time-format`, `--timezone` and `--process-width` are rejected outside `--format text`; JSON/JSONL record data,
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
JSONL contains full decoded JSON including extensions; protobuf decoded output
is a **derived view, not a native archive**. Neither format is raw device output.
A late validation, time-data, or write error may leave partial stdout; check the
exit status and the final JSON summary on stderr before treating an export as
complete. For pipelines needing transactional files, write a temporary file and
rename only after successful completion. Existing source files are never changed.

Filters combine with AND. `--query` remains literal and case-sensitive by default;
`--regex` enables Rust regex syntax and `--ignore-case` affects message matching
only (the former no-op `--case-sensitive` flag was removed). `--tag`
and `--tag-exclude` use case-sensitive literal substrings as in the GUI.
`--package` uses the shared GUI process/package predicate when that metadata is
stored; `--pid` and `--tid` match exact unsigned IDs. `--min-level` accepts
V/D/I/W/E/F or full names. Omitted minimum level and explicit `V`/`verbose` are
the same unrestricted threshold, including for cursor fingerprints. Message-only
queries can include marker/unparsed records; package, Tag inclusion/exclusion,
PID/TID or a level stricter than V exclude them because no corresponding log
metadata exists. Each text filter is limited to 4096 UTF-8 bytes.

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
year/timezone or a silent omission. Native captures store `observed_unix_ms` in
provenance, sampled when the ADB adapter observes the record, before
queue/backpressure waiting. This is host UTC receipt time, not device event time;
host clock adjustments remain observable. A missing host observation returns
`time_unavailable` for receipt-time filtering; ordinary text logcat timestamps
are not absolute instants. Explicit null also means unavailable;
non-integer, negative, or out-of-u64 timestamp values are invalid records. Capture/start `--since` retains
its separate device-logcat history syntax; it is not this offline query option.

### Agent-loop update (unreleased)

- Cursors are now `c5` (payload version 5) and references `r3` (version 3):
  base64url payloads with a 128-bit HMAC-SHA256 tag and a digest of the source
  binding instead of the full binding. Typical references shrink from about 1 KB
  to about 330 bytes. Older `c4`/`r2` tokens are rejected; query again.
- `--preview-bytes` defaults to 512 (was 2048). Pass it explicitly to keep longer
  previews.
- `--help` prints a short index with the agent loop; each command's `--help` now
  prints only that command's options.
- `--mode` is implied by `--cursor` (the cursor's recorded mode) and by
  `--start-at end` (tail); an explicit `--mode` must still agree.
- Response schema 3: record previews are compact. `message` replaces
  `message_preview`; `identity`, `identity_scope` and the duplicate
  `observed_unix_ms` inside `identity` are gone (use `get` for full identity);
  `kind` is omitted for ordinary entries; `preview_truncated`/`message_bytes`,
  `text_loss`, `process`/`process_source` and `metadata_preview_truncated` appear
  only when meaningful; `source_status` omits null observations.
- `get` defaults to `--view decoded` (was stored). `get`/`context` accept `--seq N`.
- Text captures now name processes (`log_lifecycle`, `ps_snapshot`), so
  `--package` works on CLI captures; it also matches `PKG_zygote`.
- `--case-sensitive` (a no-op default) was removed.
- `ciallogcat-cli skill` prints the official agent skill for this version.
- New: `crashes`, `query --wait-ms`, tail-mode `--all`, reusable marks,
  `context --same-pid` and `warnings[].code=package_unresolved`. Existing
  response fields are unchanged; new fields are additive.

### Migrating query callers

`query` now runs the former `query-v2` implementation. The old sequence-scanning
query implementation and the `query-v2` command name are removed; there is no
compatibility alias. `get-v2` and `context-v2` are likewise replaced by the sole
names `get` and `context`. Historical release notes describe the commands
available in those releases, not the current command set.

For former `query-v2` callers, use `query` and start a fresh query. Its
snapshot/tail modes, `--all`, full text/JSONL output, text presentation,
resolved `time_window`, and response schema remain available. Snapshot byte and
sequence fences now apply consistently to `query`, `get`, and `context`.
The shared kind-aware filter policy treats omitted `--min-level` and explicit
`V`/`verbose` identically: both permit event/unparsed message matches. Metadata
predicates and stricter levels exclude those rows rather than matching internal
placeholder fields. Cursor payloads
now use version 4 and one canonical structured filter fingerprint; old version
2/3 cursors and the literal-only fingerprint compatibility path are unsupported.
The signing key is now `query.key`; the old `query-v2.key` is neither read nor
migrated. Restart pagination without `--cursor` and obtain new `record_ref`
values rather than reusing saved tokens. Tokens remain opaque, local handles,
not portable evidence. Do not rename keys or hand-edit tokens to bypass this reset.

Use `--tag` instead of the removed `--tag-include` alias. `get` rejects
`--preview-bytes`, `--max-scan-bytes`, and `--max-scan-records`, which previously
were accepted but unused; retain only `--max-response-bytes` to control its
response chunks. `query` and `context` continue to support preview/scan budgets.

For callers of the old `query`, this is a breaking argument and JSON contract
change:

| Old sequence query | Current `query` |
| --- | --- |
| `--after-seq N` / `next_after_seq` | Omit the cursor to start; continue with opaque `next_cursor` via `--cursor`. Numeric sequence positions cannot be converted into signed cursors. |
| `schema_version: 1`, full `entries` | `schema_version: 2`, bounded `records` previews with `message_preview` and `record_ref`. Use `get` or `--all --format jsonl` for full records. |
| `--limit` up to 10000 | `--limit` is 1–1000 (default 100), subject to per-page response/scan budgets. |
| Top-level `scanned_records`, `scan_limited`, `stop_reason`, `partial` | Use `scan.records`, `scan.stop_reason`, `has_more`, `complete`, and the explicit `snapshot` boundary; old fields are not emitted. |
| Reading the current file tail on each sequence page | Default `snapshot` fixes the first page's published boundary. Use `--mode tail` with a session to read later publications with the returned cursor; tail waits for future logs only with `--wait-ms`. |

Restart old sequence pagination as a fresh query and update response parsing;
do not pass a saved sequence number to `--cursor` or assume previews are full
messages. Continue empty matching pages while `has_more=true`. Session queries
read only the status-published native prefix; an unpublished
partial tail is ignored, while a malformed published boundary fails explicitly
instead of returning the old `partial` flag. Source/token failures use the current query API
error codes described in the [query contract](agent-query.md), rather than the
old v1-query error contract.

Older storage layouts are also removed: neither CLI queries nor GUI import accepts
v1 or expanded schema-2 JSONL/session sources. Native documents created by the old legacy importer
are rejected too (`source.kind: legacy_v1`, `scope.kind: legacy_import`, or
`legacy_original_seq` metadata). These are not generic JSONL import paths.
Preserve old evidence separately; do not relabel its schema or fabricate native
provenance to make it load. There is no automatic conversion or fallback.
Current sources must be valid native documents or sessions; zero-byte files
are invalid, while a header and matching zero-record footer form a valid empty
document. The on-disk schema marker is an internal protocol constant, not a format option.

### Migrating capture callers

`capture` and `start` use one native document format. `--record-format` is removed,
not renamed or ignored. Text transport remains the default; `--transport proto`
explicitly selects experimental protobuf transport without changing the storage
format. The CLI neither chooses transports automatically nor silently falls back.
Remove old format flags and start a new session using the current CLI.

Update JSONL consumers to handle the document header, `entry`/`event`/`unparsed`
records, and final footer rather than assuming every line is a v1 entry. Native
identity, process/provenance, text-loss and capture observations are retained.
`--max-bytes` must be at least 8192 and includes header/footer bytes; 4096 bytes
are reserved for the footer. Unexpected stream exits now follow the bounded
native retry policy described below, rather than the former v1 exit behavior.

Saved worker `config.json` files must contain the complete current transport,
history scope and buffer fields, with no obsolete record-format field. Missing fields are no longer filled
with old defaults. Start a new session with the current CLI; do not revive an
old incomplete worker configuration or reuse an existing session directory.

## Commands

Run `ciallogcat-cli --help` for the argument reference, or `query --help`,
`get --help` and `context --help` for focused source, budget and pagination
instructions, including defaults and inclusive ranges. `-h` is also supported;
help is plain text on stdout and requires neither ADB nor a source file.
Control responses are JSON on stdout; diagnostics go to stderr.
Captured records are JSONL: one JSON object per line. `capture --output -`
(the default) streams native document JSONL, including header/footer, to stdout
for pipelines; its capture summary goes to stderr. An output path writes the
document to that file.

```sh
ciallogcat-cli devices
ciallogcat-cli capture --serial DEVICE_SERIAL --output capture.jsonl --duration-secs 30 --max-bytes 67108864
ciallogcat-cli start --serial DEVICE_SERIAL --session-dir ./capture-session --duration-secs 300 --max-bytes 67108864
ciallogcat-cli status --session-dir ./capture-session
ciallogcat-cli query --session-dir ./capture-session --limit 100 --query "timeout"
ciallogcat-cli stop --session-dir ./capture-session
```

`capture` runs in the foreground. `start` launches a local, finite background
capture worker. Session directories must be new; existing files are not silently
overwritten. `status`, `query`, and `stop` use the session directory rather than
a network service; `query` also accepts an offline `--file`. Query records in
pages, passing `next_cursor` as `--cursor` with the same source, mode and filters.
Optional `--query TEXT` defaults to a case-sensitive literal substring in the
message, using the shared filter semantics; `--regex` explicitly enables regex
matching. Capture defaults to the
same `main`, `system`, and `crash` buffers as the GUI. `--buffers LIST` selects
comma-separated `main,system,crash,events,radio`, or `all` alone. Names must be
exact, with no empty tokens or duplicates. There are no package, level, or regex
capture flags; filtering is available through `query`.
`--limit` defaults to 100 (maximum 1000) preview records per page. Query returns
`records`, `next_cursor`, `has_more`, `complete`, `time_window`, and nested `scan`
metadata. Continue while `has_more`, even if a page contains no matching records.
Default per-page scan budgets are 16 MiB / 100000 candidate records; the JSON
response budget is 64 KiB including the envelope. Scan budgets do not include
all source-validation I/O; `scan.file_bytes_read` reports actual log-file reads.
Use `get` for full records or `--all --format text|jsonl` to stream a finite
snapshot. See the [query contract](agent-query.md) for budget ranges, snapshot/tail
boundaries and reference-based retrieval.

Both duration and output-byte bounds remain active for each capture:
`--duration-secs` defaults to 600 (maximum 86400), and `--max-bytes` defaults
to 1073741824 (1 GiB; also the maximum), with a minimum of 8192. This is a disk/output cap, not a RAM allocation, and reaching it stops capture; no rolling overwrite is introduced. Duration must be positive. Optional
`--max-entries` lowers the positive record bound (default and maximum 10000000).
A session ends on a bound, a stop request, successful dump completion, or a
terminal error; unexpected stream exits use bounded reconnects as described below. This is a bounded capture tool,
not an indefinitely running daemon. Foreground Ctrl-C requests graceful
shutdown and stops the CLI's owned ADB child. It does not install a service, autostart
entry, remote endpoint, or HTTP server.

### Device history and finite snapshots

`capture` and `start` accept the same history options for both supported transports:

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
ciallogcat-cli capture --serial DEVICE_SERIAL --since '10-23 12:30:00.000' --output since.jsonl
ciallogcat-cli capture --serial DEVICE_SERIAL --dump --tail 2000 --buffers main,crash --output snapshot.jsonl
ciallogcat-cli start --serial DEVICE_SERIAL --session-dir ./history --dump --since 1792742400.0
```

Host duration, record and byte budgets apply during both history retrieval and
following. They can stop a snapshot before device EOF. Clean snapshot EOF after
the child is reaped and readers drain reports `reason: "snapshot_complete"` and
does not reconnect; nonzero exits and reader errors retain error/interruption
handling. This confirms the requested dump ended, not that logd retained every
original record. Native completeness remains conservative. The native header records
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
Existing references/cursors within that boundary remain usable; ones whose snapshot exceeds it are rejected
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

## Agent loop

An agent investigating an action needs four things from logs: a position before
the action, a way to wait for what the action causes, a short summary of crashes,
and the evidence around one record. Each is one bounded command:

```sh
ciallogcat-cli start --serial DEVICE_SERIAL --session-dir ./case --duration-secs 600
# 1. Mark the published end before acting; keep next_cursor as MARK.
ciallogcat-cli query --session-dir ./case --start-at end
# 2. ...install, launch or tap with your own tool...
# 3. Wait up to 20 s for the first error after the mark.
ciallogcat-cli query --session-dir ./case --cursor "$MARK" --min-level E --wait-ms 20000
# 4. Group crashes/ANRs after the mark; drill into one group's first_ref.
ciallogcat-cli crashes --session-dir ./case --cursor "$MARK"
ciallogcat-cli context --session-dir ./case --ref "$FIRST_REF" --same-pid --after 20
# 5. Read the whole window as text when needed.
ciallogcat-cli query --session-dir ./case --cursor "$MARK" --all --format text
ciallogcat-cli stop --session-dir ./case
```

- **Mark.** A `--start-at end` cursor has scanned nothing, so it binds only the
  position and tail mode. It may be resumed with *any* filters and reused for
  several queries, including `crashes`. Once a cursor has scanned records it is
  bound to its filter again (`cursor_mismatch` otherwise).
- **Wait.** `--wait-ms N` (tail only, at most 60000) polls the published
  watermark until a record matches, the source ends (terminal, stale or footer)
  or the deadline passes. `wait.stop_reason` is `matched`, `has_more`,
  `source_ended` or `deadline`; the returned cursor stays valid for the next wait.
  Scan budgets apply to each poll.
- **Crashes.** `crashes` groups Java (`AndroidRuntime` `FATAL EXCEPTION`), native
  (`libc` `Fatal signal`, `DEBUG` tombstone) and ANR (`ActivityManager` `ANR in`)
  incidents found by one bounded scan. A group reports `kind`, `signature`,
  `occurrences`, `first_seq`/`last_seq`, `first_ref`, up to four `pids`,
  `process`, `thread`, `headline`, `cause` and up to five `frames`. Java
  signatures use the deepest `Caused by` class and its top frame; native ones use
  the signal and first non-libc frame (offsets stripped); ANR reasons have digits
  folded to `#`. These are navigation hints, not diagnoses. `--limit` (default 20,
  at most 200) caps returned groups; `has_more=true` means only the scanned range
  is covered, and groups are not merged across calls. Crash cursors resume only
  crash scans. Filters such as `--since` or `--pid` narrow the input records.
- **Same-PID context.** `context --same-pid` keeps only neighbours from the
  target's PID (for example the rest of a stack trace); other records are still
  scanned within budgets and counted in `skipped_other_pid`.
- **Package filter.** Capture is never restricted to one app: like the GUI, the
  CLI records every process and filters on the host. `--package P` matches the
  process `P`, every `P:suffix` process (services, isolated and sandboxed
  renderers such as `com.android.chrome:sandboxed_process0:…`) and the app zygote
  `P_zygote`. WebView renderers belong to the WebView provider
  (`com.google.android.webview:…`), not to the host app. Protobuf records carry
  the device's process cache. Text records are named while capturing from two
  labelled hints: `log_lifecycle` (ActivityManager `Start proc PID:NAME/…` until
  `Process NAME (pid PID) has died`, and AndroidRuntime `Process: NAME, PID: PID`)
  and, for live follow captures only, `ps_snapshot` (a `ps -A -o PID,NAME`
  sample about once per second, applied to records received within two seconds
  of it, as in the GUI). History rows (`--tail N`, `--since`, `--dump`) get
  lifecycle names only. Records left unnamed cannot match `--package`; the
  response then carries `warnings: [{code: "package_unresolved", records: N}]`
  instead of a silent empty result. Use `--pid` or `--tag` for them.

### Agent skill

The repository ships an official agent skill at
[`skills/ciallogcat-cli/SKILL.md`](../skills/ciallogcat-cli/SKILL.md) (also
`SKILL.md` in CLI release archives and the DEB documentation). The binary embeds
the same file, so the skill always matches the installed version:

```sh
mkdir -p ~/.claude/skills/ciallogcat-cli
ciallogcat-cli skill > ~/.claude/skills/ciallogcat-cli/SKILL.md
```

Other agents that read `SKILL.md` skills use their own skills directory. There
is no MCP server: the JSON commands are already the tool interface.

## Native capture and Agent queries

`capture` and `start` record the same native documents for text and protobuf
transport. Native storage preserves record kinds, stable
session/sequence references, host observation times and text-loss observations;
its header and final footer let the GUI open the same evidence file. The shared
`query` command reads native sources using the [Agent query contract](agent-query.md).

```sh
ciallogcat-cli start --serial DEVICE_SERIAL --session-dir ./case-001 --duration-secs 120
ciallogcat-cli query --session-dir ./case-001 --mode tail --query timeout
ciallogcat-cli stop --session-dir ./case-001
ciallogcat-cli query --file ./case-001/logs.jsonl --mode snapshot
```

Both transports share the same capture pipeline, which retries unexpected stream exits at 1, 2 and 4 seconds, at most three times;
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

All attempts share the original duration, entry and byte limits. The minimum
`--max-bytes` is 8192; header and footer bytes count toward it, and 4096 bytes are
reserved for the bounded footer. Metadata does not count as a log entry.
Each footer diagnostic is limited to 1024 encoded JSON bytes, including JSON
escaping, with an explicit truncation flag. This preserves space for the
aggregate capture facts even when diagnostics contain control characters.
Status carries an internal `record_schema_version` marker, `artifact_id`, `session_id` and
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
`--transport proto`. It requires target `shell_v2` support and
uses `adb shell -T -n` with separate stdout/stderr and no PTY. Unsupported devices
or malformed frames fail without silent text fallback. It preserves each bounded
protobuf payload as base64, retaining original tag/message bytes inside that
payload independently of normalized display text. AOSP Android 15+ has the schema; vendor and host-channel
compatibility still needs device validation. See [protobuf findings](logcat-protobuf.md).

The native format stores each protobuf payload once and derives display fields
with the frozen `aosp_logcat_16r1_host_v1` projection. Text entries, events and
unparsed rows use ordinary native fields in the same format. Transport choice
does not select a storage version. Both transports share history options, total
budgets, retry policy, flush-before-publication watermarks and status handling.

```sh
ciallogcat-cli start --serial DEVICE_SERIAL --session-dir ./compact --transport proto --duration-secs 120
ciallogcat-cli query --session-dir ./compact --mode tail --start-at end
ciallogcat-cli query --session-dir ./compact --mode tail --cursor '<next_cursor>'
ciallogcat-cli get --session-dir ./compact --ref '<record_ref>' --view decoded
```

`query`, `crashes`, `get` and `context` use response schema version 3, independently
of command names and the internal source schema marker. `--start-at beginning|end`
defaults to beginning; end requires a native session in tail mode without
a cursor. It returns an empty page at the published boundary, with the skipped
prefix explicitly unscanned. Continue with the returned cursor.

`get --view decoded` is the default and returns full structured JSON, including
complete message, metadata and extensions, marked as a derived view rather than
archived bytes. For text-transport records it is byte-for-byte the stored line;
for protobuf records it is roughly 2–3 times larger because it carries the
derived text beside the raw base64 payload. `--view stored` returns the original
archive JSON line. `get` and `context` accept `--seq N` instead of `--ref`: the
record is located by bisection inside the current visible boundary
(`record_not_found` if absent), reading a few lines per step rather than the
history.
It is not a stored archive record for writing back. Join UTF-8 chunks using
`next_text_offset`, keeping the same view. Stored lines retain the 8 MiB limit;
decoded views have a separate 64 MiB cap to allow escaped protobuf text to expand.
Every response still obeys `--max-response-bytes`. See the
[query contract](agent-query.md) for bounds and validation semantics.

## Integrate with an existing test executor

[examples/agent_capture.py](../examples/agent_capture.py) starts a finite native
session using the same current format as the CLI,
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
record previews and continuation/reference tokens; use `query`, `get` and
`context` to request more. A test passing does not imply capture succeeded.
The example reaps the direct executor and, on POSIX, terminates its private process
group. On Windows, descendants remain the executor's responsibility. Device cleanup
always belongs to the supplied executor. See the [workflow contract](agent-test-workflow.md)
for deadlines, failure tail and the difference between host action times and device logs.
Open `case-002/logs.jsonl` in the GUI after a finalized stop to inspect the same
session/sequence identities. This example adds no resident service or model API.

## JSON and exit codes

Capture/control responses use `schema_version: 1` and a `type` discriminator.
This control-envelope version does not enable storage-v1 reading or writing.
Query/get/context responses use version 2, independently of the source archive
layout. The fixed internal archive marker is `source_schema_version: 3`;
control/query and archive markers describe different protocols, not user choices.
Entry fields include `seq`, `time`, `pid`, `tid`, `level`, `tag`, and `message`.
Status includes `state`, `readiness`, `entries`, `bytes`, `reason`, and `stale`.
The current writer publishes `entries` and `bytes` together after a complete
JSONL record is written and flushed from user-space buffers. A partial write or
failed flush does not advance that confirmed prefix. This is visibility to
other processes, not a per-record disk-sync guarantee. Session queries use this
published boundary for all record formats; snapshot pagination fixes its initial
boundary and administrative closure caps reads at the immutable frozen prefix.
Normal completion reasons include `max_entries`, `max_bytes`, `duration_limit`,
`stopped`, and `snapshot_complete`; a failed capture reports its error instead.
`omitted_by_limit: true` means a configured output/entry limit was reached and
capture may be truncated; it does not prove additional records existed.
`discarded_entries` counts backend records received but not confirmed written,
including structural and unparsed records. It
excludes unread device records and cannot establish end-to-end
losslessness. After writer timeout, it is the count not confirmed written at the
last observed progress, rather than a proven permanent loss count: the writer
may still make progress. The footer's `host_discarded_records` instead counts
received records not admitted to the writer. Diagnostics retain only the last line; `diagnostic_dropped_count`
is the number of earlier received diagnostic lines no longer retained.

SIGINT, cooperative `stop`, and the duration limit terminate the owned ADB child
immediately, then admit complete buffered host records for up to one second.
Entry/byte limits and the 4096-byte footer reserve still apply. Reaching a hard
limit or an actual failure closes admission; admission timeout releases pending
records so backend cleanup can continue despite a slow writer. No reconnection
occurs after stopping. Duration is a stop trigger, not a device event-time cutoff.
Normal dump completion needs no additional admission window. ADB/reader exit
confirmation has a five-second limit, followed by up to two seconds each for
footer enqueue and writer completion; this is not a five-second total shutdown
guarantee. Unread device/queue losses may be unknown; protobuf partial-frame gaps
and observed host discards remain reported.

Final status includes `shutdown`:

- `trigger`: the original stop reason, retained even if a later failure changes `reason`.
- `cleanup`: `confirmed` means no owned ADB child/readers await cleanup, including
  when no child was started. It does not confirm writer completion.
- `drain`: `completed`, `deadline`, `budget`, or `failed` describes host record
  admission, with failure taking precedence over budget, then deadline.
- `footer_write`: `confirmed` requires successful footer enqueue and confirmed
  writer completion/final flush. `unconfirmed` does not prove the footer is absent.

Missing shutdown fields from older status files mean unavailable. These
confirmations do not promise disk durability or device-side losslessness.
Successful SIGINT cleanup may produce `state: "completed"`, a finalized document,
and the `interrupted` error envelope with exit 130. That exit denotes user
cancellation; actual ADB/output failures retain their own error and exit code.

Query JSON and text/JSONL stderr summaries include bounded `evidence.summary` to
explain three independent questions together:

| Field | Meaning |
| --- | --- |
| `complete` | The current query boundary was scanned, even if capture is active. |
| `artifact_finalized` | All records from the beginning and the footer were validated. Nonempty skipped history prevents this claim. |
| `source_status.capture_completeness` | The source capture observation is `unknown` or `partial`; neither asserts losslessness. |

For example, `complete: true`, `artifact_finalized: true`, capture completeness
`unknown`, and `known_gap_count: 0` are consistent: the query and artifact are
complete, source coverage remains unknown, and no known gaps were reported.
An empty gap list does not prove a lossless capture. Missing gap information is
unavailable rather than zero. Footer observations and session status are labeled
with their observation times; current session status may be newer than the fixed
snapshot and does not describe its exact coverage. A tail suffix can see a valid
footer while leaving skipped nonempty history unverified.

Errors on stderr have `type: "error"` and an `error` object containing `code`
and `message`. Do not treat a successful status/query command as proof that
the capture itself succeeded: inspect its state and reason.

- 0: command completed
- 2: invalid command or arguments
- 3: invalid, failed, or stale session
- 4: ADB discovery, launch, or capture error
- 5: filesystem or output error
- 6: timeout
- 130: foreground capture cancelled with Ctrl-C after cleanup/finalization;
  inspect final status for confirmation, rather than interpreting 130 as artifact corruption

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
  There is no guarantee that every device record is captured during startup,
  process discovery, disconnection, or shutdown.

The shared `document` codec reads native with an 8 MiB encoded-line limit
for headers, records and footers. It validates required identity, increasing
sequences, export scope and footer consistency. Sequence gaps are permitted,
and unknown optional fields are preserved unless they are reserved or explicitly
unsupported. Invalid records fail rather than being silently skipped. Offline
files require a valid footer, which may omit its trailing newline. Session
queries read only the published prefix and reject incomplete records within that
boundary; an unpublished partial tail is not read. Neither a valid footer nor a
successful query proves continuous capture.
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
cargo test --locked -p ciallogcat-cli -p ciallogcat-core
cargo clippy --locked -p ciallogcat-cli -p ciallogcat-core --all-targets -- -D warnings
python -m unittest discover -s .github/scripts -p test_package_cli.py
```

The packaging tests check Linux and Windows names and allowlists, executable
permissions in the Linux archive, DEB metadata/file allowlists/root ownership,
checksums, invalid tags/platforms, missing CLI binaries, and dependency-discovery
failures. Mock dependency fixtures do not replace actual CI dependency discovery. Creating a tag or running the release workflow is a separate,
explicit publication action.
