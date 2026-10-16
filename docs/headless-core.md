# Headless shared core

The desktop application and other Rust consumers now share one library in the
existing `ciallogcat` package. The default `gui` feature preserves the desktop
binary. Disable default features to build the library and standalone CLI without
`eframe`, `egui`, window-system or GPU dependencies. Add `--lib` to check only the
library:

```sh
cargo check --locked --no-default-features --lib
cargo test --locked --no-default-features
cargo tree --locked --no-default-features
```

A Rust consumer can depend on this package with `default-features = false`.
The library exposes:

- `model`: log levels, buffer selection, log entries and complete-record text
  formatting
- `adb`: ADB discovery, asynchronous device/package/process queries, bounded
  Logcat events, parsing, and capture handles
- `filter`: matching, background filtering and highlighting, cancellation and
  latest-result mailboxes
- `log_store`: chunked copy-on-write snapshots and byte-accounted retention
- `WakeSignal`: an optional cloneable worker notification callback

`WakeSignal::default()` is a no-op, suitable for consumers that block on channels
or poll results themselves. `WakeSignal::new(callback)` lets an event loop wake
when results are published. The callback executes on a worker thread and must be
fast, non-blocking and safe to call from multiple threads. Callbacks should not
panic. Notifications are best-effort hints, may be coalesced or spurious, and do
not guarantee per-event or trailing notification. Logcat entry notifications
retain the existing 16 ms throttle without a trailing timer: entries can arrive
after a wake and then remain idle without another wake. Backend consumers must
use channel `recv` / `recv_timeout` or an independent periodic pump, never wait
solely for this callback. Drain available results when woken. The GUI adapts its
repaint request to this hook.

The caller creates the bounded `SyncSender<adb::BackendEvent>` channel, drains it,
keeps the `adb::LogcatHandle` alive, and calls `stop` or drops it to stop capture.
`start_logcat` takes an explicit ADB path, device serial, selected buffers and
caller-owned session ID. Events carry that ID so a consumer can reject stale
sessions. Filter requests likewise retain generation, log epoch and snapshot
base metadata; the consumer is responsible for accepting or discarding results.

This extraction leaves application/session orchestration in the desktop app:
process attribution, active-device reconciliation, event polling, filtering
catch-up, configuration, selection and clipboard policy are unchanged. The
library itself does not start a GUI or configure an event loop. The separate
[`ciallogcat-cli` frontend](cli.md) consumes this library and owns its JSON /
JSONL output, capture bounds, and local background-session protocol. Those
policies are not part of the shared core API. `LogEntry::copy_text` and
`append_copy_text` preserve the existing complete-record text format.

The ADB fake-process scenarios run headlessly on Linux; see
[ADB testing](adb-testing.md). Automated tests do not establish real-device,
interactive-desktop, accessibility or GPU behavior.
