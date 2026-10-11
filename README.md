# Ciallogcat

A desktop Android Logcat viewer built with Rust and egui

English | [简体中文](README.zh-CN.md)

Ciallogcat is a standalone tool for debugging your Android apps, with Linux as its primary desktop target and Windows builds available. The Cargo package and executable use the lowercase name `ciallogcat`.

- Live logs with timestamp, level, PID/TID, process name, tag, and message
- Package, minimum-level, and keyword filters, including regular expressions and case-sensitive matching
- Saved filters, keyword history, multiple Logcat buffers, and manual device selection
- Multi-row selection and copying, full-message details, and independent live-follow controls
- Light/dark themes, adjustable row height, and bundled Chinese/English fonts
- Virtualized log display and a configurable memory budget

## Table of Contents

- [Background](#background)
- [Install](#install)
- [Usage](#usage)
- [Configuration](#configuration)
- [Documentation](#documentation)
- [Platform status](#platform-status)
- [Maintainers](#maintainers)
- [Contributing](#contributing)
- [License](#license)

## Background

Ciallogcat focuses on live Logcat debugging for personal Android app development. It runs ADB as a subprocess and filters captured logs locally. It does not currently support offline log files, side-by-side devices, log export, or other ADB utilities.

The interface currently mixes English and Chinese labels. The instructions below include the labels used in the app.

## Install

### Dependencies

Install [Android SDK Platform-Tools](https://developer.android.com/tools/releases/platform-tools) and make `adb` available on your `PATH`, or use an existing Android SDK installation. ADB is not bundled. Enable USB debugging on your device and accept its authorization prompt, then check:

```sh
adb devices
```

Linux requires a graphical X11 or Wayland session, compatible graphics drivers, and system libraries. Linux release packages target Ubuntu 24.04 x86_64; compatibility with older Ubuntu or other Debian-based distributions is not guaranteed. See [Platform status](#platform-status) for verification limits.

### Release packages

Choose an available version from [GitHub Releases](https://github.com/IllyasvielEin/Ciallogcat/releases). The release workflow packages Windows x86_64 ZIP, Linux x86_64 tar.gz, and Ubuntu 24.04 x86_64 DEB files, each with a `.sha256` checksum. The examples below use the v0.4 filenames; use the filename of the version you downloaded.

On Ubuntu 24.04, install the DEB from its download directory:

```sh
sudo apt install ./ciallogcat-v0.4-linux-x86_64.deb
ciallogcat
```

You can also launch it from the application menu. ADB is a recommended package dependency; if you already provide it through Android SDK Platform-Tools, install with `--no-install-recommends`. Uninstall with `sudo apt remove ciallogcat`; your user configuration is retained.

For the portable Linux archive:

```sh
tar -xzf ciallogcat-v0.4-linux-x86_64.tar.gz
cd ciallogcat-v0.4-linux-x86_64
./ciallogcat
```

Install its runtime libraries and ADB separately. On Windows, extract the ZIP and run `ciallogcat.exe` inside the extracted folder.

### Build from source

Install current [Rust stable](https://rustup.rs/), including Cargo, and Git. On Windows, use the MSVC toolchain with Visual Studio Build Tools' **Desktop development with C++** workload and Windows SDK.

On Ubuntu / Debian, install build dependencies:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libudev-dev libx11-dev libxi-dev libxrandr-dev libxcursor-dev libxkbcommon-dev libwayland-dev libegl1-mesa-dev
```

Use equivalent packages on other distributions. Clone and run:

```sh
git clone https://github.com/IllyasvielEin/Ciallogcat.git
cd Ciallogcat
cargo run --locked --release
```

To build without launching, use `cargo build --locked --release`. The executable is `target/release/ciallogcat` on Linux or `target/release/ciallogcat.exe` on Windows.

## Usage

Launch the installed application from your desktop menu or terminal:

```sh
ciallogcat
```

1. Connect and authorize an Android device, then select it in **Device**. Selection is manual; use **刷新** (Refresh) if needed. Capture starts when you select an available device.
2. Focus the package field to choose an installed third-party app, or enter a package name. Set a minimum log level and enter a keyword. `.*` toggles regular expressions; **Aa** toggles case sensitivity.
3. Click a log row to read its full message. Use **Ctrl+C** to copy, **Shift+click** for a range, or **Ctrl+click** to toggle individual rows.
4. Use **Saved filters** to save or restore a filter combination. Keyword history appears when the keyword field is focused and stores up to 500 entries; it does not store the other filter settings.
5. Use **View** to change the theme, row height, detail display, or log memory budget.

### Capture and filtering

- **暂停** (Pause) stops capture while keeping the device and logs. **开启** (Start) resumes from recent logs without replaying the paused interval. **关闭** (Close) stops capture and deselects the device; retained logs remain readable.
- Reselecting the same paused device does not resume capture. Reconnecting to the same device preserves retained logs. Switching to another device asks before clearing them.
- Buffers default to `main + system + crash`. You can also select `events`, `radio`, or exclusive `all`. Changes take effect after **应用** (Apply); active capture restarts, while paused capture stays paused. Existing logs remain, so the view can contain records from previous buffer selections. Starts and restarts use `-T 1`, with no historical backfill; switching may leave a short gap.
- Package filtering includes subprocesses such as `com.example.app:sync`. It uses known process names, not message contents. Short-lived processes may go unidentified; their logs are retained but hidden by a package filter. Clear the package restriction to inspect them. Keyword and level filters still apply.
- An invalid regular expression shows an error and clears the current results until corrected. **Clear** asks for confirmation and removes local logs only; it never clears the device's Logcat buffers.

### Keyboard and live follow

- **Ctrl+F** or **/**: focus keyword search
- **↑ / ↓**, **Home / End**: select logs when an input or popup is not active
- **Ctrl+A**: select all filtered logs; **Shift+navigation**: extend selection
- **Ctrl+C**: copy selected text first, otherwise selected full log records in display order
- **Esc**: dismiss input/popups or cancel row selection without clearing filters
- **Ctrl+End** or **回到最新**: return to the latest logs and resume following

Browsing or selecting logs pauses automatic following without stopping capture. Input fields keep their normal editing shortcuts. Filtering keeps only selections that still match; clearing or memory trimming cancels row selection.

### Memory retention

The default log budget is **300 MiB**, adjustable from **16–4096 MiB** in **View**. When exceeded, the oldest records are removed until the estimate falls to 90% of the budget. This covers retained log data, **not total process RSS**. Graphics, fonts, temporary work, and shared background snapshots add overhead.

Individual incoming lines are limited to 1 MiB; longer lines are truncated and marked. Long-message previews are shortened, but filtering and copying use the full received text. See [log storage and rendering](docs/ui-review.md#日志存储与绘制) for queue, snapshot, and rendering details.

## Configuration

Settings, saved filters, and keyword history are stored locally:

- Linux: `$XDG_CONFIG_HOME/ciallogcat/config.json`, or `~/.config/ciallogcat/config.json` when unset
- Windows: `%APPDATA%\Ciallogcat\config.json`

Set `CIALLOGCAT_CONFIG_DIR` to use another configuration directory, or `CIALLOGCAT_ADB` to specify an ADB executable. Otherwise, the app searches SDK environment variables, `PATH`, and common SDK locations. Graphics diagnostic variables are documented in the [UI guide](docs/ui-review.md#图形诊断).

## Documentation

The detailed guides are currently in Chinese:

- [UI behavior, performance, and graphics diagnostics](docs/ui-review.md)
- [ADB subprocess tests and mocks](docs/adb-testing.md)
- [Release workflow and packaging](docs/release.md)
- [Contribution guide](CONTRIBUTING.md)
- [Bundled font provenance and licenses](assets/fonts/README.md)

## Platform status

Linux X11 and nested Wayland desktop checks have exercised ordinary interaction with mock logs. These checks do not establish real Android-device capture, long-running stability, or compatibility with every desktop.

Known limits include misplaced Chinese IME candidates on the tested X11/XIM path, incomplete Wayland IME verification, and a nested Weston resize test blocked by a compositor crash. Windows window dragging may stutter. See the [verification notes](docs/ui-review.md#验证范围) for scope and details.

## Maintainers

[@IllyasvielEin](https://github.com/IllyasvielEin)

## Contributing

[Open an issue](https://github.com/IllyasvielEin/Ciallogcat/issues) or submit a pull request. Read [CONTRIBUTING.md](CONTRIBUTING.md) for reporting guidelines and repository layout. Before submitting changes, run:

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

ADB subprocess tests use a Python 3 mock and currently run on Linux; they do not need an Android device. See the [test guide](docs/adb-testing.md) for commands and isolation boundaries. Synthetic [performance tests](docs/ui-review.md#性能测量) measure CPU-side work, not GPU frame rate or real-device throughput.

The release workflow runs checks and packaging on version-tag pushes only. Branch pushes and pull requests do not trigger CI. See the [release guide](docs/release.md) before publishing.

## License

[MIT](LICENSE) © 2026 Ciallogcat contributors.

Bundled fonts are licensed separately under [SIL Open Font License 1.1](assets/fonts/LICENSE-OFL.txt).
