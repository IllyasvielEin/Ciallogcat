# Pinned logcat bindings generator

This independent maintenance crate is not in the application's dependency graph.
It compiles the checked-in AOSP schema using fixed `prost-build` and vendored
`protoc`, then writes `src/protobuf/generated.rs`. The default application build
does not run it. See [schema provenance and commands](../../proto/README.md).

Use `cargo fetch --locked --manifest-path tools/proto-codegen/Cargo.toml` once to
populate the development-tool cache if needed; regeneration and `--check` can
then run with `--offline --locked`. The tool's lock file pins dependencies on
every supported vendored-compiler host, so downloads may include platforms other
than the current machine. Supported-host limitations are explicit tool errors;
they do not affect building the application from its checked-in bindings.
