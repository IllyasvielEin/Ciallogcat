# Ciallogcat GUI v1.3.1

## Desktop improvements

- Switch between English and Simplified Chinese in **View → Settings… → Appearance**. New installations start in English; existing configurations without a language setting retain Chinese. Changes apply immediately and are saved.
- Open **Settings** with **Ctrl+,** to manage appearance, log reading and memory preferences. Dark, light and system themes are available; system changes apply without a restart. Older configurations keep their previous dark/light choice.
- Choose row-height presets or fine-tune the height. Memory budget edits still require **Apply budget**; cancelling, changing settings pages or closing the dialog discards unapplied changes.
- Press **F1** for shortcut help. Both languages fit the minimum 900×560 toolbar; menu shortcuts are aligned and process observations remain reachable from the package menu when no warning row is shown.
- Clarify capture options, suspected crash/ANR hints, process attribution and memory notices. Unknown diagnostic text and device output remain available unchanged.

## Independent GUI releases

GUI and CLI now use separate Cargo packages and component tags. This release is **gui-v1.3.1** and contains only GUI downloads; the existing CLI v1.3.1/v1.3.2 tags and assets are unchanged. CLI remains at **1.3.2**. The GUI DEB package remains `ciallogcat` and upgrades the previously published GUI 1.3.0. The internal core package remains 1.3.0; its source is pinned by this release commit. No native archive or CLI protocol changes are introduced.

Users upgrading from before 1.3.0 should consult the [v1.3.0 breaking changes](https://github.com/IllyasvielEin/ciallogcat/releases/tag/v1.3.0).

## Validation and downloads

The release pipeline requires Linux/Windows GUI and core tests, strict Clippy, release builds, binary-version verification and packaging checks. The GUI DEB is installed and removed in a clean Ubuntu 24.04 container. New interaction regressions cover language/config migration, translation coverage, settings dismissal, budget drafts, theme switching and minimum-window bounds. Optional offscreen rendering tooling is excluded from normal builds.

Native desktop interaction, IME, real Android devices and long-running GPU/capture performance remain outside these automated checks.

Downloads: Linux x86_64 tar.gz, Windows x86_64 ZIP, Ubuntu 24.04 x86_64 DEB and three matching SHA-256 files (six assets). Install ADB separately. This stable GUI release updates Latest; CLI downloads remain on the [CLI v1.3.2 Release](https://github.com/IllyasvielEin/ciallogcat/releases/tag/v1.3.2).
