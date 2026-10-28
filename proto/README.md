# Vendored Android logcat schema

`logcat.proto` is an unmodified copy of the Android Open Source Project schema:

- Upstream: `platform/system/logging`, `logcat/logcat.proto`.
- Release: `android-16.0.0_r1`.
- Commit: `d78b713380007d3c0dde14712cbcbec27f491ad9`.
- [Pinned source](https://android.googlesource.com/platform/system/logging/+/d78b713380007d3c0dde14712cbcbec27f491ad9/logcat/logcat.proto).
- SHA-256: `d709478babea7dc534dec1bfde154f1d73bc0f72e44212ba8f34d0382b3ddabd`.

The upstream source file has no copyright banner. Its directory's
[`Android.bp`](https://android.googlesource.com/platform/system/logging/+/d78b713380007d3c0dde14712cbcbec27f491ad9/logcat/Android.bp)
assigns `system_logging_logcat_license`, with the SPDX identifier `Apache-2.0`
and `NOTICE` as license text. `LICENSE-APACHE-2.0` is that directory's unmodified
[`NOTICE`](https://android.googlesource.com/platform/system/logging/+/d78b713380007d3c0dde14712cbcbec27f491ad9/logcat/NOTICE).
Copyright and license attribution remain with the Android Open Source Project;
these third-party materials are not relicensed under Ciallogcat's MIT license.

The schema matches the checked Android 15 r1 and main snapshots in the research
document. AOSP source support is not a guarantee about any particular device.
This schema has no buffer ID, process-instance ID, drop indicator, session ID, or
stream magic. Its framing is a separate **8-byte little-endian length**, not the
usual protobuf varint delimiter. Native `ANDROID_LOG_FATAL=7` maps to schema
`FATAL=8`; unknown wire value 7 must not be treated as Fatal.

`src/protobuf/generated.rs` is checked-in generated code. The application builds
with the Rust prost runtime and never invokes or downloads protoc. A separate,
locked maintenance tool in `tools/proto-codegen` uses:

- `prost-build = 0.14.4` (Apache-2.0).
- `protoc-bin-vendored = 3.2.0`, providing `libprotoc 31.1`.
- Its own committed `Cargo.lock`, independent of the application lock file.

Regenerate, after fetching the locked tool dependencies once:

```sh
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target/review-build cargo run --offline --locked --manifest-path tools/proto-codegen/Cargo.toml
```

Check without modifying generated code:

```sh
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=target/review-build cargo run --offline --locked --manifest-path tools/proto-codegen/Cargo.toml -- --check
```

The tool checks the protoc version and compares exact generated text. Its
temporary directory is removed after either success or failure. When updating
the schema or generator, review the pinned upstream diff, license, schema hash,
tool lock file, generated bindings, and protocol fixtures together. The
vendored compiler is a development dependency, with its own upstream licensing
and platform support; it is not part of the normal product executable.
