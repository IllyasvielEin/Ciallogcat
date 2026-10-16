# ciallogcat-cli

`ciallogcat-cli` is a headless frontend for Ciallogcat's shared Rust parser,
ADB capture, and literal message-filter code. It is a separate executable
from the desktop `ciallogcat` application. It does not require a display server, graphics drivers,
eframe, or egui. ADB is installed separately.

## Install and build

The release workflow packages CLI archives for **Linux x86_64** and
**Windows x86_64** only:

- `ciallogcat-cli-v<VERSION>-linux-x86_64.tar.gz`
- `ciallogcat-cli-v<VERSION>-windows-x86_64.zip`

Each contains a versioned directory with `ciallogcat-cli` (or
`ciallogcat-cli.exe`), `LICENSE`, and this guide as `README.md`. Verify the
accompanying `.sha256` file before extracting. The version follows the release
tag; for example, a `v1.0` tag build uses `ciallogcat-cli-v1.0-…`.
Adding CLI packaging does not change assets already published under old tags.
The GUI ZIP/tar.gz/DEB names are unchanged. There is no standalone core-library
release artifact or CLI DEB. Linux builds use Ubuntu 24.04; older systems are
not guaranteed compatible.

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

## Commands

Run `ciallogcat-cli --help` for the argument reference. Control responses are JSON on stdout; diagnostics go to stderr.
Captured records are JSONL: one JSON object per line. `capture --output -`
(the default) streams only entry JSONL to stdout for pipelines; its capture
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
semantics; it does not search tags and is not a regex. Capture uses the fixed
`main`, `system`, and `crash` buffers. The CLI has no buffer-selection, package,
level, or regex flags; message filtering is available only through `query`.
`--limit` defaults to 100 (maximum 10000). Query returns an `entries` array,
`next_after_seq`, `has_more`, `partial`, and `scan_limited`. Pass
`next_after_seq` back as `--after-seq` for the next page, including when matching
returns no entries. The cursor advances over scanned nonmatches. Per-call scan
limits are 16 MiB / 100000 candidate records after the cursor, with an
8 MiB returned-record budget, plus the JSON envelope. `scan_limited` reports a candidate-scan budget being
reached. These limits do not bound total input read: each query rereads and
parses the earlier prefix from the start of the file, with no index, so later
pages can read up to the entire configured capture size (default 64 MiB,
maximum 1 GiB). Query cost grows with that prefix. `partial` means a trailing
incomplete JSONL record was skipped and can be retried after the writer finishes.

Both duration and output-byte bounds remain active for each capture:
`--duration-secs` defaults to 600 (maximum 86400), and `--max-bytes` defaults
to 67108864 (64 MiB; maximum 1073741824). Both must be positive. Optional
`--max-entries` lowers the positive record bound (default and maximum 10000000). A session ends when a bound is reached, ADB
exits, a stop is requested, or an error occurs. This is a bounded capture tool,
not an indefinitely running daemon. Foreground Ctrl-C requests graceful
shutdown and stops the CLI's owned ADB child. It does not install a service, autostart
entry, remote endpoint, or HTTP server.

A successful `start` reports `adb_process_started`: the worker and ADB process
started. It cannot prove
that the Android device's log daemon is ready or that records will arrive.
Check `status` and query the output. Silence is not proof of a healthy device.
`stop` requests cooperative shutdown through the session directory; the worker
owns and reaps its ADB child rather than killing an arbitrary process by PID.
`stop --wait-secs N` waits for acknowledgement (default 10 seconds, range 1–30).
A stale heartbeat becomes `state: "unknown"` with `stale: true`; it is not proof
that a process is alive or dead, and stop does not signal a stored PID.

## JSON and exit codes

Control and record JSON use `schema_version: 1` and a `type` discriminator.
Entry fields include `seq`, `time`, `pid`, `tid`, `level`, `tag`, and `message`.
Status includes `state`, `readiness`, `entries`, `bytes`, `reason`, and `stale`.
Normal completion reasons include `max_entries`, `max_bytes`, `duration_limit`,
`stopped`, and `adb_exit`; a failed capture reports its error instead.
`omitted_by_limit: true` means a configured output/entry limit was reached and
capture may be truncated; it does not prove additional records existed.
`discarded_entries` counts known parsed records received but not confirmed
written. It excludes unread device records and cannot establish end-to-end
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
  notices apply. Missing process names retain the shared core's behavior.
  There is no guarantee that every device record is captured during startup,
  process discovery, disconnection, or shutdown.
- Mock-based tests and release smoke tests do not establish real-device
  reliability, prolonged memory stability, or compatibility with every host.

## Validation and packaging

The existing release workflow still runs only on version-tag pushes. It has
separate GUI and headless CLI Linux/Windows jobs. The CLI job does not install
GUI development packages and runs headless tests, Clippy, the explicit CLI
release build, `--help`/`--version` smoke checks, and packaging contract tests.
Linux fake-ADB integration tests exercise capture/session process lifecycles.
Windows runs portable CLI help/version/argument tests and packaging tests;
Windows fake-ADB lifecycle and real-device behavior have not been verified.
Release publication waits for both GUI and CLI jobs. It uploads CLI archives and
checksums alongside existing GUI assets; it never uploads Cargo `.rlib` files.

```sh
cargo test --locked --no-default-features
cargo clippy --locked --no-default-features --all-targets -- -D warnings
python -m unittest discover -s .github/scripts -p test_package_cli.py
```

The packaging tests check Linux and Windows names and allowlists, executable
permissions in the Linux archive, checksums, invalid tags/platforms, and missing
CLI binaries. Creating a tag or running the release workflow is a separate,
explicit publication action.
