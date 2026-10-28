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
- `record_io`: bounded CLI v1 JSONL decoding and compatible record serialization
- `session`: stable capture-session UUIDs, sequences, segments and attempts
- `document`: validated native v2/v3 snapshots, compact protobuf conversion,
  incremental encoding/decoding and legacy v1 import
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
responses, capture bounds, and local background-session protocol. The v1 log
record codec is shared; those orchestration policies are not part of the core
API. `LogEntry::copy_text` and
`append_copy_text` preserve the existing complete-record text format.

`LogEntryKind` separates parsed logs, structural markers and unparsed text.
Only parsed logs without a name contribute to `is_unresolved_process()`.
Markers pass the package condition, while level/message conditions still apply;
unparsed text cannot match a nonempty package condition. A `PsSnapshot` process
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
[native v2/v3 files](native-format.md), preserving source, identity, process hints,
text-loss observations and export scope. `file_io::write_new_file` publishes a
synced snapshot at a new path without overwriting an existing destination.

Native v3 stores one raw protobuf payload and derives its display through a
fixed projection profile. V2 remains supported for reading and explicit output.
`Document::convert_format` creates a new artifact with cancellation and memory/
output budgets; it preserves session/sequence identity and fails rather than
truncating evidence. GUI file operations and the CLI queries consume this same
codec. See the [format contract](native-format.md) and
[Agent query contract](agent-query.md) for stored versus decoded views.

`capture_scope::CaptureScope` validates history requests for GUI and CLI. A
device history count is independent of host duration, output and retention
budgets. `capture_facts::CaptureFacts` records bounded host observations without
claiming an exact device-time boundary or an unknown loss count. Read-only
diagnostics use a separate on-demand API and budgets; see
[device diagnostics](device-diagnostics.md).
These modules remain available with GUI features disabled. The CLI preserves
its v1 output/query protocol and explicitly offers native v2 recording and
[bounded Agent queries](agent-query.md). `start_logcat_with_options` also permits
a startup deadline and cancellation callback for the optional protobuf capability
probe; cancellation reaps that query before returning. An `Exited` event with
`incomplete_proto_frame: true` identifies an incomplete frame and no other reader
or process error; if observed during an intentional stop, callers can record an
explicit partial-capture gap without inventing whether the stop caused it.

`LegacyReader` accepts a `BufRead` and an explicit `InputMode`. `Offline` accepts
a valid final JSON value without a newline; `Active` reports that tail as
`Incomplete`. This is a bounded reader for one input snapshot, not a file-tail
service. The 8 MiB per-record bound counts JSON-encoded bytes. Invalid schemas,
non-increasing sequences, malformed JSON and oversized records produce an error
with line and byte location; sequence gaps are allowed. Unknown optional fields
are preserved on reserialization without being interpreted. V1 has no process provenance, record-kind identity or completeness
footer, so importing it cannot reconstruct those facts.

The ADB fake-process scenarios run headlessly on Linux; see
[ADB testing](adb-testing.md). Automated tests do not establish real-device,
interactive-desktop, accessibility or GPU behavior.
