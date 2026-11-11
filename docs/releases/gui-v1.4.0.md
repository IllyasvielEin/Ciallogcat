# Ciallogcat GUI v1.4.0

## Process attribution

- Process names now also come from ActivityManager/AndroidRuntime lifecycle lines in the same log stream (`Start proc PID:NAME` until `Process NAME (pid PID) has died`, and the `Process:` line of Java crash headers). Short-lived processes that start and crash between `ps` samples are named, so package filters no longer hide the most important startup crashes. These names also apply to history rows; `ps` hints remain limited to live follow captures as before.
- Rows named this way are labelled **(log inferred)**, with an explanation in the row menu and accessibility text. Like `ps` hints, they are hints rather than verified identities, and a fresh `ps` sample that disagrees replaces a stale lifecycle name.
- Package filters also match the app zygote process `PKG_zygote`, in addition to `PKG` and `PKG:suffix` processes. WebView renderers remain attributed to the WebView provider.

The attribution logic now lives in the shared core and is the same as in CLI v1.4.0. Native archives may contain the new process source `log_lifecycle`; files written by CLI v1.4.0 open in this release.

## Versions

This release is **gui-v1.4.0** and contains only GUI downloads. CLI is released separately as [cli-v1.4.0](https://github.com/IllyasvielEin/ciallogcat/releases/tag/cli-v1.4.0); the internal core package is 1.4.0. The GUI DEB package remains `ciallogcat`.

## Validation and downloads

The release pipeline requires Linux/Windows GUI and core tests, strict Clippy, release builds, binary-version verification and packaging checks; the GUI DEB is installed and removed in a clean Ubuntu 24.04 container. New regressions cover lifecycle attribution, PID reuse after a death line and the zygote package rule. Native desktop interaction, IME, real Android devices and long-running GPU/capture performance remain outside these automated checks.

Downloads: Linux x86_64 tar.gz, Windows x86_64 ZIP, Ubuntu 24.04 x86_64 DEB and three matching SHA-256 files (six assets). Install ADB separately. This stable GUI release updates Latest.
