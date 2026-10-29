# Synthetic capture benchmark

Linux, Python 3 standard library, `rustc` with edition 2024 support, and an
existing CLI executable are required. The tool never builds the product or
calls real ADB. It compiles a tiny Rust mock and replays pre-encoded inputs into
the real CLI. This measures the synthetic host capture/write pipeline, not
device encoding, USB, GPU, GUI retention, or durable/fsync throughput.

By default the tool measures `current/text` and `current/proto` using the current
CLI's single native storage format. Every invocation specifies its transport;
none passes a storage-format option to the current CLI. `--trials` defaults to
5 (range 1–10).

## Small smoke

```sh
python3 -B -m unittest discover -s tools -p 'test_transport_benchmark.py'
python3 -B tools/transport-benchmark.py \
  --cli target/debug/ciallogcat-cli \
  --output-dir /tmp/ciallogcat-capture-smoke --smoke
```

The output directory must not already exist. `--smoke` uses 64 messages of
128 bytes and 32 messages of 2048 bytes, with one measured round by default.
An explicit `--trials` overrides that count. Smoke timings validate the tool,
not performance; startup and 5 ms sampling dominate tiny inputs.

## Comparing a historical baseline

Pass a **separate, frozen old executable** with `--baseline-cli` to add
`baseline/text` and `baseline/proto`. Only that explicit old executable receives
its historical `--record-format v2` argument. This compares both transports at
the old default storage layout against the current native layout. Optional
`--baseline-compact` also adds `baseline-compact/proto` with the old executable's
`--record-format v3`, separating storage-layout effects from later code changes.
Unsupported old CLI options fail the run; there is no detection, silent fallback,
or pretending the new binary supports the old command line.

```sh
python3 -B tools/transport-benchmark.py \
  --cli /tmp/frozen-current/ciallogcat-cli \
  --baseline-cli /tmp/frozen-baseline/ciallogcat-cli \
  --baseline-compact \
  --output-dir /tmp/ciallogcat-comparison-smoke --smoke
```

The schema-2 validator and historical flags in this benchmark are solely for the
explicit baseline process. They are not product compatibility paths. Historical
reports retain their original names, versions and measurements; rerunning an old
command against a new binary is not a valid replacement for those results.

## Final measurement, after fixes and checks stabilize

Finish fixes, full checks and release builds first. Freeze the executable(s),
record their source revisions, then run from the repository root:

```sh
bench_root=$(mktemp -d /tmp/ciallogcat-capture-final.XXXXXX)
install -m 755 target/release/ciallogcat-cli "$bench_root/ciallogcat-cli"
python3 -B tools/transport-benchmark.py \
  --cli "$bench_root/ciallogcat-cli" \
  --output-dir "$bench_root/report" --trials 5
# Add --baseline-cli /path/to/frozen-old-cli [--baseline-compact] for comparison.
```

This runs 50,000 × 128-byte messages and 20,000 × 2048-byte messages. Each workload
generates its text/proto wire inputs once and reuses the exact files for every
build and trial. Message/tag/PID/TID/level match across transports; proto also
carries UID, integer time, process name and raw payload, while text retains its
threadtime timestamp representation. Compare the same transport and workload
between builds before attributing a change to cleanup or storage layout.

Input, executable, replay and tool hashes are recorded. Do not rebuild or replace
frozen executables during measurement: changed before/after hashes fail the report.
Each workload has one warmup round followed by the requested measured rounds.
The initial list is `current/text`, `current/proto`, then any baseline combinations
listed above; round N rotates it left by N positions, with warmup N=0.
`round_order`, `trial`, `warmup`, `order_position` and `case_run_index` expose the
actual order. Deterministic rotation is not a claim of perfectly balanced order.

## Report and integrity contract

`report.json` is atomically updated after each run. `runs.jsonl` and stdout contain
individual run records; the last stdout line gives the report path and summaries.
Report schema 3 uses the combination names above in `cases[].summary` and
`cases[].median`. Each run includes `build`, `transport`, the internal
`record_schema_version`, argv, relative artifact/stdout/stderr paths, and:

- Wall seconds, CPU user/system/total seconds, records/s and message MiB/s.
  Wall includes CLI startup, replay and process cleanup. Generation, mock
  compilation, hashing and validation are outside the timed interval. CPU is
  `wait4` usage for the CLI plus replay children it reaped, excluding Python.
- CLI-only sampled peak `VmRSS` and `VmHWM` in KiB, sample count and interval.
  No samples produces null, not a false zero. Separate `wait4_maxrss_kib` may
  include pre-exec/inherited high-water usage; it is not sampled CLI RSS or a
  process-tree sum. The older tool's `cli_sampled_peak_rss_kib` sampled VmHWM;
  that metric is now named `cli_sampled_peak_hwm_kib`.
- Artifact bytes, return code, timeout flag, runner `exit_reason`, CLI status
  and its `cli_exit_reason`. Successful runs must end at `max_entries`.
- Verified count, exact sequences/provenance, header/schema/transport, original
  base64-decoded payload bytes against each length-prefixed replay frame, and
  footer identity/count/bounds/scope/stop reason/known gaps. Current protobuf
  storage must omit redundant display/decoded fields and declare its fixed
  encoding/projection. Current text checks ordinary display fields; historical
  expanded protobuf checks both display and redundant evidence against the wire.
  CLI status entries, bytes, visible end and artifact/session IDs must match.

Successful non-warmup runs alone enter medians. Each group reports successful,
failed and expected counts, warmup status and `complete`. No successful samples
produces a null median. Any failed warmup or measurement makes the overall
command exit 1, even when other groups pass; inspect `state`, `success` and all
`complete` fields before using a report. An interrupted/in-progress report
remains `state: running`, not a completed result. Failures retain logs and any
partial artifact. Successful artifacts are retained only for the final round;
all run logs and input files remain. Filenames are
`{case}-{trial}-{build}-{transport}.{jsonl,stdout,stderr}`.

The mock flushes stdout before waiting so partial buffers are delivered. Capture
has a 60-second bound; the harness kills its own private process group at 75
seconds, and mock processes have a 90-second backstop. Monitoring reads only the
spawned CLI PID and waits for that child; there is no global process scan or
process-name kill. Keep the host otherwise quiet for final measurements. Empty
`known_gaps` certifies only this bounded fixture check, never complete device
capture. Artifact size alone is not a throughput or memory conclusion.
