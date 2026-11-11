# Ciallogcat CLI v1.4.0

This release makes the headless CLI practical inside agent loops: smaller responses, a position mark taken before an action, waiting for its effects, crash grouping and working package filters. It contains **CLI downloads only**; the GUI is released separately as [gui-v1.4.0](https://github.com/IllyasvielEin/ciallogcat/releases/tag/gui-v1.4.0).

## Agent loop

```sh
ciallogcat-cli query --session-dir DIR --start-at end            # mark: keep next_cursor
# ...perform the action with another tool...
ciallogcat-cli query --session-dir DIR --cursor MARK --min-level E --wait-ms 20000
ciallogcat-cli crashes --session-dir DIR --cursor MARK
ciallogcat-cli context --session-dir DIR --seq N --same-pid --after 30
```

- **Marks.** An unconsumed `--start-at end` cursor binds only a position, so it can be resumed with any filters and reused. `--cursor` implies its recorded mode and `--start-at end` implies tail mode.
- **Waiting.** `query --wait-ms N` (tail, at most 60 s) returns as soon as a record matches, the source ends or the deadline passes, reporting `wait.stop_reason`. Tail mode also supports `--all`.
- **Crash grouping.** New `crashes` command groups Java crashes, native crashes (signal and tombstone) and ANRs by signature, with process, frames and a reference to the first record. Signatures are navigation hints, not diagnoses.
- **Drill-down.** `get` and `context` accept `--seq N`, located by bisection rather than scanning history. `context --same-pid` follows one process, for example a stack trace.

## Process names and package filters

Text captures now name processes from ActivityManager/AndroidRuntime lifecycle lines in the same stream (`log_lifecycle`, also covering processes that start and crash within a second) and, for live follow captures, from `ps` samples applied with the same rule as the GUI (`ps_snapshot`). `--package PKG` therefore works on CLI captures and matches `PKG`, every `PKG:suffix` process (services, isolated or sandboxed renderers) and the app zygote `PKG_zygote`. Records that remain unnamed are reported as `warnings[].code=package_unresolved` instead of producing a silent empty result. Protobuf captures keep the device process cache.

## Smaller responses

- Query response schema **3**: compact record previews (`message`, metadata only when meaningful) and status observations without nulls. Full identity and stored fields remain available through `get`.
- Cursors (`c5`) and references (`r3`) use base64url, a 128-bit HMAC tag and a digest of the source binding: references shrink from about 1 KB to about 260 bytes. Older tokens are rejected; query again.
- `--preview-bytes` defaults to 512. `--help` is a short index with the agent loop; each command's `--help` shows only its own options.
- In a lab crash investigation the data an agent reads fell from about 47 KB to about 11 KB.

## Official agent skill

`skills/ciallogcat-cli/SKILL.md` is included in the archives and DEB documentation, and `ciallogcat-cli skill` prints the copy embedded in the binary, so the skill always matches the installed version.

## Breaking changes

- Response `schema_version` is 3: `message_preview` became `message`; `identity`, `identity_scope`, null and default-valued fields are omitted from previews.
- `get` defaults to `--view decoded` (identical to stored for text records; for protobuf records about two to three times larger, still chunked by `--max-response-bytes`).
- Cursor and reference tokens from earlier versions are rejected.
- The no-op `--case-sensitive` flag was removed.

Native archives remain readable by both frontends; they may now contain the additional process source `log_lifecycle`. The `examples/agent_capture.py` report is schema 3 and adds crash groups (`--crash-groups`, default 10).

## Validation and downloads

Automated checks cover Linux/Windows CLI and core tests, strict Clippy, headless release builds and packaging. Validation used mock ADB fixtures and a synthetic lab device; real-device verification of lifecycle line formats across Android versions and OEM builds is still pending.

Downloads: Linux x86_64 tar.gz, Windows x86_64 ZIP, Ubuntu 24.04 amd64 DEB and three SHA-256 files (six assets). Install ADB separately. This CLI release does not change the repository's Latest marker.
