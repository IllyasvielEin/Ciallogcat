---
name: ciallogcat-cli
description: Capture and query Android logcat evidence with ciallogcat-cli while developing or debugging an Android app. Use when you need device logs around an action you take (install, launch, tap, test run), want to know whether something crashed or hit an ANR, or need the stack trace and surrounding logs for one record. Requires ciallogcat-cli and adb on PATH and an authorized device.
---

# ciallogcat-cli

`ciallogcat-cli` records a bounded local logcat session and answers small,
paged JSON queries over it. It never taps, installs or clears anything on the
device; use your other tools for device actions. Every command prints one JSON
object on stdout; errors are JSON on stderr with a stable `error.code`. Decide
on `code`, never on message text.

## The loop

```sh
ciallogcat-cli devices                                   # pick SERIAL (state "device")
ciallogcat-cli start --serial SERIAL --session-dir "$DIR" --duration-secs 900
ciallogcat-cli query --session-dir "$DIR" --start-at end # save next_cursor as MARK
# ... perform the action with another tool ...
ciallogcat-cli query --session-dir "$DIR" --cursor "$MARK" --min-level E --wait-ms 20000
ciallogcat-cli crashes --session-dir "$DIR" --cursor "$MARK"
ciallogcat-cli context --session-dir "$DIR" --seq 1234 --same-pid --after 30
ciallogcat-cli stop --session-dir "$DIR"
```

- `$DIR` must not exist yet; use a fresh temporary directory per session.
  Sessions end on their own at `--duration-secs` (default 600), so pick a
  duration that covers your whole task or start a new session later.
- `start` reporting `adb_process_started` does not prove logs will arrive. If
  later queries stay empty, check `status` (`state`, `stale`,
  `last_stream_error`) before concluding anything.

## Marks: take one before every action

`query --start-at end` returns an empty page whose `next_cursor` is a mark at
the current end of the log. A mark has not scanned anything, so you may resume
it with any filters, and reuse it for several queries and for `crashes`. Once a
cursor has returned or scanned records it is bound to its filters: changing
filters then fails with `cursor_mismatch`. Go back to the mark instead.

## Waiting

`--wait-ms N` (at most 60000) waits for the first matching record after the
cursor. Read `wait.stop_reason`: `matched`, `deadline` (nothing matched yet),
`source_ended` (capture stopped) or `has_more` (a scan budget ended the page;
call again with `next_cursor`). Do not sleep-and-poll yourself.

## Reading results economically

- JSON pages hold short previews (`message`, 512 bytes by default). A record
  with `preview_truncated: true` has `message_bytes` total; fetch the full
  record with `get --seq N` (or `--ref`), which returns decoded JSON in chunks:
  continue with `--text-offset <next_text_offset>` until `complete: true`.
- For a readable dump of a window use
  `query --session-dir "$DIR" --cursor "$MARK" --all --format text`; stdout is
  text, the summary JSON goes to stderr. Add filters (`--min-level W`, `--tag`,
  `--pid`) first, because `--all` streams every matching record.
- Follow `next_cursor` while `has_more` is true, even if `records` is empty.
- `complete: true` only means this query reached its boundary. It does not mean
  the device captured everything; read `evidence.summary` once if completeness
  matters, and never claim lossless capture.

## Filters

`--query TEXT` (literal, case-sensitive; add `--regex`, `--ignore-case`),
`--tag`, `--tag-exclude`, `--pid`, `--tid`, `--min-level V|D|I|W|E|F`,
`--since 5m` / `--until 30s` (host receive time), `--package PKG`.

`--package com.example.app` also matches `com.example.app:anything` (services,
sandboxed renderers) and `com.example.app_zygote`. Process names are hints
gathered while capturing. If a response contains
`warnings[].code == "package_unresolved"`, some records had no name: zero
matches then do not mean the app was silent; filter those with `--pid`/`--tag`.
WebView renderers run as `com.google.android.webview:…`, not as the host app.

## Crashes and ANRs

`crashes --cursor "$MARK"` groups Java crashes (`AndroidRuntime FATAL
EXCEPTION`), native crashes (`libc Fatal signal`, `DEBUG` tombstones) and ANRs
after the mark. Each group has `signature`, `occurrences`, `process`, `pids`,
`headline`, `cause`, `frames` and `first_seq`/`first_ref`. Treat the signature as
a pointer, not a diagnosis: read the evidence with
`context --seq <first_seq> --same-pid --after 40` (stack lines come from the
crashing PID) before explaining a cause.

## Errors worth handling

| code | meaning | do |
| --- | --- | --- |
| `source_not_ready` | session header not published yet | retry shortly |
| `cursor_mismatch` | filters/mode differ from the cursor | reuse the mark, or drop `--cursor` |
| `stale_cursor` / `invalid_token` | source changed or token from another version | query again without `--cursor` |
| `record_not_found` | `--seq` not inside the visible boundary | check the seq, or wait for publication |
| `invalid_arguments` | bad flag or value | read `COMMAND --help` |
| `adb_unavailable` / `adb_error` | ADB missing or device problem | check `devices`, USB authorization |

## Safety

Logs can contain personal data, tokens and account details. Keep session
directories private, quote only what the task needs, and do not upload capture
files unless the user asks. `ciallogcat-cli` never clears device logs.
