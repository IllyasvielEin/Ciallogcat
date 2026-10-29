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
- `session`: stable capture-session UUIDs, sequences, segments and attempts
- `document`: validated native snapshots, compact protobuf evidence,
  incremental encoding/decoding and the shared encoded-line limit
- `capture_controller`: shared capture intent, stop acknowledgement and bounded reconnect policy
- `capture_scope`: validated device history starts, finite dumps and buffer arguments
- `capture_facts`: bounded host observations, pauses, interruptions and export facts
- `diagnostics`: cancellable, bounded read-only device observations
- `protobuf`: fixed-schema, bounded experimental decoder retaining original wire payloads
- `file_io`: atomic publication to a new destination without replacement
- `WakeSignal`: an optional cloneable worker notification callback

`WakeSignal::default()` is a no-op, suitable for consumers that block on channels
or poll results themselves. `WakeSignal::new(callback)` lets an event loop wake
when results are published. The callback executes on a worker thread and must be
fast, non-blocking and safe to call from multiple threads. Callbacks should not
panic. Notifications may be coalesced or spurious and are not event counts or
completion acknowledgements. Logcat entries use `WakeSignal::coalesced` with one
timer worker and one pending 16 ms deadline per capture stream. Later entries do
not postpone that deadline, and the final notification is preserved if the
producer becomes silent. Delivery still depends on OS scheduling. Drain available
results when woken; if a per-frame work budget ends the drain, schedule its
continuation explicitly. Synchronous consumers can use channel `recv` /
`recv_timeout` instead. The GUI uses worker callbacks and specific deadlines,
without an unconditional 100 ms pump.

The caller creates the bounded `SyncSender<adb::BackendEvent>` channel, drains it,
keeps the `adb::LogcatHandle` alive, and calls `stop` or drops it to stop capture.
`start_logcat` takes an explicit ADB path, device serial, selected buffers and
caller-owned session ID. Events carry that ID so a consumer can reject stale
sessions. Filter requests likewise retain generation, log epoch and snapshot
base metadata; the consumer is responsible for accepting or discarding results.

Application/session orchestration stays in the frontend: process attribution,
active-device reconciliation, event polling, filtering catch-up, configuration,
selection and clipboard policy. The
library itself does not start a GUI or configure an event loop. The separate
[`ciallogcat-cli` frontend](cli.md) consumes this library and owns its command
responses, capture bounds, and local background-session protocol. The native
document codec is shared; those orchestration policies are not part of the core
API. `LogEntry::copy_text` and `append_copy_text` preserve complete messages.
Parsed logs keep their full-record text format; structural rows use explicit
`[event]` / `[unparsed]` labels rather than invented log metadata.

`LogEntryKind` separates parsed logs, structural markers and unparsed text.
Only parsed logs without a name contribute to `is_unresolved_process()`.
Markers and unparsed rows have no log metadata: nonempty package/Tag filters or
a level stricter than Verbose exclude them. Default Verbose and message-only
filters still include matching structural rows; explicit Verbose is equivalent. A `PsSnapshot` process
source is an observation hint, not an exact historical application identity.
`BackendEvent::Processes` supplies the host observation window;
`QueuedLogEntry::received_at()` is captured before queue backpressure. Neither
timestamp is the time the Android event occurred. The GUI applies fresh hints to
newly received logs and does not backfill historical unknown records.

`received_unix_ms()` supplies the corresponding host wall-clock observation when
available. `LogEntry::text_loss` is populated by the stream adapter with observed
host truncation and UTF-8 replacement facts; `None` means unknown. The text
suffix alone is not used as evidence of truncation.

`LogcatEvent::Exited` is delivered after confirmed child reaping and both output
readers have finished enqueueing events. Keep draining the channel after `stop`
and release queued payloads: backpressure can delay this final acknowledgement,
although it no longer prevents the child from being reaped. A caller must not
treat a stop request itself as confirmation that output has finished.

The `session` module allocates stable UUID/sequence references separately from
the numeric transport attempt used by `BackendEvent`. The GUI owns this
allocator across Clear, retention and reconnect. `document` implements bounded
[native files](native-format.md), preserving source, identity, process hints,
text-loss observations and export scope. `file_io::write_new_file` publishes a
synced snapshot at a new path without overwriting an existing destination.

Native documents store one raw protobuf payload and derive its display through
a fixed projection profile. Text, event and unparsed rows use the same document
layout. There is no format selector, version-conversion API or separate origin
variant. GUI file operations and CLI queries consume this same codec, with
cancellation and budgets that fail rather than truncate evidence. See the [format contract](native-format.md) and
[Agent query contract](agent-query.md) for stored versus decoded views.

`capture_scope::CaptureScope` validates history requests for GUI and CLI. A
device history count is independent of host duration, output and retention
budgets. `capture_facts::CaptureFacts` records bounded host observations without
claiming an exact device-time boundary or an unknown loss count. Read-only
diagnostics use a separate on-demand API and budgets; see
[device diagnostics](device-diagnostics.md).
These modules remain available with GUI features disabled. The CLI uses
text transport by default, offers explicit experimental protobuf transport, retains
schema-1 control responses, and uses [bounded queries](agent-query.md)
for all supported archive formats
through the single `query` command. `start_logcat_with_options` also permits
a startup deadline and cancellation callback for the optional protobuf capability
probe; cancellation reaps that query before returning. An `Exited` event with
`incomplete_proto_frame: true` identifies an incomplete frame and no other reader
or process error; if observed during an intentional stop, callers can record an
explicit partial-capture gap without inventing whether the stop caused it.

`document::MAX_RECORD_BYTES` supplies the shared 8 MiB encoded-line bound for
native header/data/footer readers and writers. Completed files require a header
and consistent footer; a valid footer may omit its trailing newline. Active
session queries instead read complete records within the published boundary.
Storage-v1 decoding, its `record_io` module, and legacy-import metadata are
removed rather than silently upgraded. Empty byte streams are rejected; valid
zero-record native documents remain supported.

The ADB fake-process scenarios run headlessly on Linux; see
[ADB testing](adb-testing.md). Automated tests do not establish real-device,
interactive-desktop, accessibility or GPU behavior.
