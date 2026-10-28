# Ciallogcat

An Android Logcat viewer and headless capture tool, built with Rust.

English | [简体中文](README.zh-CN.md)

Use the desktop app to browse live logs or offline files, or the CLI to capture and query logs from scripts and local agents. Both use the same Rust core; live capture requires external ADB. Linux is the primary desktop target; release packaging supports Linux and Windows x86_64.

- Live logs with timestamps, levels, PID/TID, process names, tags, and full messages
- Desktop filters for package, minimum level, and keywords, including regex and case-sensitive matching
- Saved filters, keyword history, multiple Logcat buffers, and manual device selection
- Multi-row copying, keyboard navigation, and live follow that pauses while you browse
- Light/dark themes, bundled Chinese/English fonts, and a configurable log memory budget
- A separate, display-free CLI with JSONL capture and finite local background sessions
- CLI v1 / native v2 file opening, retained snapshots and filtered exports with source metadata and stable record references

## Table of Contents

- [Background](#background)
- [Install](#install)
- [Desktop usage](#desktop-usage)
- [Headless CLI](#headless-cli)
- [Configuration](#configuration)
- [Documentation](#documentation)
- [Platform status](#platform-status)
- [Maintainers](#maintainers)
- [Contributing](#contributing)
- [License](#license)

## Background

Ciallogcat focuses on Android app debugging. The desktop app, `ciallogcat`, reads live Logcat through ADB and filters locally. The separate `ciallogcat-cli` executable writes normalized JSONL for scripts and agents without opening a window.

Open JSONL through **File** or drag and drop, without ADB. File views are read-only; saving and filtered export use native v2. The app does not show devices side by side, and currently mixes English and Chinese labels. See the [file workflow](docs/file-workflow.md).

## Install

### ADB prerequisite

Install [Android SDK Platform-Tools](https://developer.android.com/tools/releases/platform-tools) and put `adb` on your `PATH`, or use an existing Android SDK. **ADB is not bundled.** Enable USB debugging, connect your Android device, and accept its authorization prompt. Check the connection with:

```sh
adb devices
```

### Release packages

Check [GitHub Releases](https://github.com/IllyasvielEin/Ciallogcat/releases) for published downloads. This source tree defines the following packages; older releases may not include the CLI. `<TAG>` below is the release tag, such as `v1.0`, rather than a literal part of the filename.

- **Desktop GUI:** `ciallogcat-<TAG>-windows-x86_64.zip`, `ciallogcat-<TAG>-linux-x86_64.tar.gz`, or `ciallogcat-<TAG>-linux-x86_64.deb`
- **Headless CLI:** `ciallogcat-cli-<TAG>-windows-x86_64.zip` or `ciallogcat-cli-<TAG>-linux-x86_64.tar.gz`; Ubuntu 24.04 DEB: `ciallogcat-cli-<TAG>-linux-x86_64.deb`

Each archive/package has a `.sha256` checksum. GUI and CLI downloads are separate; the CLI DEB is independent of the GUI DEB, and there is no standalone core-library download.

Extract a portable archive and run `ciallogcat` / `ciallogcat.exe` for the desktop app, or `ciallogcat-cli` / `ciallogcat-cli.exe` for the CLI. From the extracted directory, launch the GUI with `./ciallogcat` on Linux or `.\ciallogcat.exe` in Windows PowerShell. For CLI help, use `./ciallogcat-cli --help` or `.\ciallogcat-cli.exe --help`, respectively. Add that directory to `PATH` to use the bare command names below.

On Ubuntu 24.04, install the downloaded GUI DEB with `sudo apt install ./<downloaded-filename>.deb`, then launch **Ciallogcat** from the application menu or run `ciallogcat`. ADB is a recommended package dependency; if you provide it through the Android SDK, use `--no-install-recommends`. Uninstall with `sudo apt remove ciallogcat`; user settings are retained.

Install the headless CLI DEB with `sudo apt install ./ciallogcat-cli-<TAG>-linux-x86_64.deb`, then run `ciallogcat-cli --help`. It can coexist with the GUI DEB; remove it with `sudo apt remove ciallogcat-cli`. ADB is recommended and can also be supplied by Android SDK Platform-Tools.

Linux packages are built on Ubuntu 24.04 x86_64; older distributions are not guaranteed compatible. **The GUI** needs an X11 or Wayland desktop, compatible graphics drivers, and runtime libraries. **The CLI** needs no display server, GPU, or GUI libraries.

### Build from source

Install current [Rust stable](https://rustup.rs/) with Cargo, and Git. Windows builds use the MSVC toolchain, Visual Studio Build Tools' **Desktop development with C++** workload, and the Windows SDK. Linux CLI builds need a C linker; GUI builds additionally need window-system development libraries. On Ubuntu / Debian, install the GUI build dependencies with:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libudev-dev libx11-dev libxi-dev libxrandr-dev libxcursor-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libegl1-mesa-dev
```

Clone the repository, then choose a frontend:

```sh
git clone https://github.com/IllyasvielEin/Ciallogcat.git
cd Ciallogcat

# Run the desktop app
cargo run --locked --release --bin ciallogcat

# Build only the CLI, without GUI dependencies
cargo build --locked --release --no-default-features --bin ciallogcat-cli
./target/release/ciallogcat-cli --help
```

To build the GUI without launching it, use `cargo build --locked --release --bin ciallogcat`. Binaries are in `target/release/`, with an `.exe` suffix on Windows. Rust consumers can also use the [shared core](docs/headless-core.md) with default features disabled.

## Desktop usage

1. Launch `ciallogcat`, connect and authorize a device, then select it in **Device**. Selection is manual; use **刷新** (Refresh) if needed. Selecting an available device starts capture.
2. Choose or enter a package name, set the minimum level, and enter a keyword. `.*` toggles regex and **Aa** toggles case sensitivity. **Saved filters** stores combinations; keyword history remembers recent searches.
3. Click a row for its full message. **Shift+click** selects a range, **Ctrl+click** toggles rows, and **Ctrl+C** copies. **Ctrl+F** or **/** focuses search.
4. Browsing or selecting pauses live follow without stopping capture. Use **Ctrl+End** or **回到最新** to follow new logs again.
5. Use **View** for themes, row height, details, and the log memory budget. Budget edits take effect only after **Apply budget**.
6. **File → 打开日志** / **Ctrl+O** opens JSONL. **另存保留日志** / **Ctrl+Shift+S** saves retained rows, and **导出当前筛选结果** exports a filtered snapshot. File operations currently use path input or drag and drop; save to a new destination.

A few useful details:

- **暂停** (Pause) stops capture but keeps logs; **开启** (Start) resumes without backfilling the paused interval. **关闭** (Close) also deselects the device. Same-device reconnects preserve retained logs; switching devices asks before clearing them.
- Device-discovery errors retain the last successful device list and your selection. An offline or missing-device snapshot does not stop a stream that is still running; Pause remains available.
- An exited stream retries at 1, 2 and 4 seconds, up to three times; Pause or Close cancels retries. Restart waits for the old child and both output pipes to finish. A stop acknowledgement taking over five seconds blocks a new start and reports an error.
- Buffers default to `main + system + crash`. Changes take effect after **应用** (Apply). Starts and restarts use `-T 1`, so they do not backfill history and may leave gaps.
- Package filtering includes subprocesses such as `com.example.app:sync`. GUI process names are recent `ps` hints, not verified identities; failed or expired samples are discarded, and later samples never fill unknown historical rows. Unknown or short-lived processes can be hidden by that filter; clear the package restriction to inspect their logs. Structural Logcat banners remain subject to level and message filters, but bypass the package condition and do not count as unknown processes.
- Invalid regex shows an error until corrected. **Clear** removes local records after confirmation, never the device's Logcat buffers.

### Memory retention

- The default retained-log budget is **300 MiB**, adjustable from **16–4096 MiB**. It is not a total-process memory limit. Oldest records are removed when the budget is exceeded.
- Incoming lines are limited to **1 MiB** and truncation is marked. Full-record copying has a **16 MiB UTF-8** limit; oversized selections report an error without changing the clipboard.

See the [UI guide](docs/ui-review.md) for more on keyboard navigation, selection behavior, memory accounting, and graphics diagnostics.

## Headless CLI

`ciallogcat-cli` is useful for terminal workflows, scripts, and local agents. After installing it or adding `target/release` to `PATH`, find your device and replace `DEVICE_SERIAL` below with its serial:

```sh
ciallogcat-cli devices

# Foreground capture to a new file
ciallogcat-cli capture --serial DEVICE_SERIAL --output capture.jsonl --duration-secs 30

# A finite local background session, using a new directory
ciallogcat-cli start --serial DEVICE_SERIAL --session-dir ./capture-session --duration-secs 300
ciallogcat-cli status --session-dir ./capture-session
ciallogcat-cli query --session-dir ./capture-session --after-seq 0 --limit 100 --query "timeout"
ciallogcat-cli stop --session-dir ./capture-session
```

- Control responses are JSON. Captured records are **normalized JSONL**, not raw Logcat bytes. `capture --output -` streams records to stdout and sends its summary to stderr.
- Both time and output-size bounds remain active: defaults are **600 seconds** and **64 MiB**. Use `--duration-secs` and `--max-bytes` to change them. Sessions end on a limit, ADB exit, a stop request, or an error; this is not an indefinite daemon.
- `start` reporting `adb_process_started` means the worker and ADB process started. It does **not** prove device readiness or that logs will arrive. Inspect `status` and query the output.
- `query --query` matches a case-sensitive literal substring of the message. For another page, pass its `next_after_seq` as `--after-seq`. The CLI does not expose all desktop filters.
- Output files and session directories must be new. Keep captures private and review them before sharing; device logs can contain sensitive information.

The [CLI guide](docs/cli.md) covers options, paging limits, exit codes, shutdown, and packaging. Run `ciallogcat-cli --help` for a quick reference.

## Configuration

Desktop settings, saved filters, and keyword history are stored locally:

- Linux: `$XDG_CONFIG_HOME/ciallogcat/config.json`, or `~/.config/ciallogcat/config.json` when unset
- Windows: `%APPDATA%\Ciallogcat\config.json`

Set `CIALLOGCAT_CONFIG_DIR` to change the desktop configuration directory. Both frontends support `CIALLOGCAT_ADB` for the ADB executable; the CLI also accepts `--adb PATH`. Otherwise, ADB discovery checks SDK environment variables, `PATH`, and common SDK locations. CLI sessions use the directory specified by `--session-dir`.

New Agent integrations can use `capture/start --record-format v2` with `query-v2/get-v2/context-v2`, preserving identities in files the GUI can open. Experimental `--transport proto` is explicit; see the [CLI guide](docs/cli.md).

## Documentation

Detailed guides are in English or Chinese:

- [Documentation index and migration (Chinese)](docs/README.md)
- [Desktop behavior and diagnostics](docs/ui-review.md)
- [CLI commands and sessions](docs/cli.md)
- [Shared Rust core](docs/headless-core.md)
- [Agent v2 queries, full records and context](docs/agent-query.md)
- [Development direction and implementation progress (Chinese)](docs/development-plan.md)
- [Open tasks and Agent handoff (Chinese)](TODO.md)
- [Validation status and limits (Chinese)](docs/validation.md)
- [Text/protobuf/compact performance comparison (Chinese)](docs/transport-performance-2026-10-24.md)
- [Read-only device diagnostics](docs/device-diagnostics.md)
- [Bounded evidence around external tests](docs/agent-test-workflow.md)
- [Logcat protobuf research and compatibility (Chinese)](docs/logcat-protobuf.md)
- [Logcat options and software practices (Chinese)](docs/logcat-options-and-ecosystem.md)
- [ADB subprocess tests and mocks](docs/adb-testing.md)
- [Release workflow and packaging](docs/release.md)
- [Contribution guide](CONTRIBUTING.md)
- [Bundled font provenance and licenses](assets/fonts/README.md)

## Platform status

Release jobs target **Linux and Windows x86_64**. Linux fake-ADB tests exercise capture and session lifecycles; a shared native Rust fixture is included in Windows CI as well. Local validation includes Linux execution and Windows cross-compilation; hosted Windows execution and device behavior still require confirmation. These tests do not establish real-device reliability or long-running stability.

Desktop checks have exercised ordinary interaction on Linux X11 and nested Wayland with mock logs. Known limits include Chinese IME candidate placement on the tested X11/XIM path, incomplete Wayland IME verification, and a nested Weston resize check blocked by a compositor crash. Windows window dragging may stutter. See the [verification notes](docs/ui-review.md#验证范围).

## Maintainers

[@IllyasvielEin](https://github.com/IllyasvielEin)

## Contributing

[Open an issue](https://github.com/IllyasvielEin/Ciallogcat/issues) or submit a pull request. Read [CONTRIBUTING.md](CONTRIBUTING.md) for reporting guidelines and repository layout. Before submitting changes, run:

```sh
cargo fmt --all --check
cargo test --locked
cargo test --locked --no-default-features
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --no-default-features --all-targets -- -D warnings
cargo build --locked --release
cargo build --locked --release --no-default-features --bin ciallogcat-cli
```

Mock ADB tests require Python 3 on Linux, not an Android device. See the [test guide](docs/adb-testing.md) and [CLI validation guide](docs/cli.md#validation-and-packaging). The **Checks** workflow runs on pull requests, pushes to `main`, and manual dispatch; a native Rust fake-ADB fixture is included for Windows capture/session lifecycles, while local cross-compilation does not establish Windows runtime results. The normal release workflow remains version-tag-only. A separate manual workflow can append only the CLI DEB/checksum to the immutable v1.0 release after clean-container validation. See the [release guide](docs/release.md) before publishing.

## License

[MIT](LICENSE) © 2026 Ciallogcat contributors.

Bundled fonts are licensed separately under [SIL Open Font License 1.1](assets/fonts/LICENSE-OFL.txt).
