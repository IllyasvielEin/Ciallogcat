# Synthetic transport / record-format benchmark

Linux, Python 3 standard library, `rustc` with edition 2024 support, and an
existing CLI executable are required. The tool never builds the product or
calls real ADB. It compiles a tiny Rust mock and replays pre-encoded inputs into
the real CLI. This measures the synthetic host capture/write pipeline, not
device encoding, USB, GPU, GUI retention, or durable/fsync throughput.

Existing commands remain valid: without `--include-compact`, only explicit
`v2/text` and `v2/proto` run. `--trials` still defaults to 5 (range 1–10).
Product CLI defaults are irrelevant: every invocation specifies both transport
and record format. Compact comparisons require a CLI supporting explicit
`--record-format v3 --transport proto`; unsupported combinations are failures.

## Small smoke

```sh
python3 -B -m unittest discover -s tools -p 'test_transport_benchmark.py'
python3 -B tools/transport-benchmark.py \
  --cli target/debug/ciallogcat-cli \
  --output-dir /tmp/ciallogcat-q09-smoke \
  --include-compact --smoke
```

The output directory must not already exist. `--smoke` uses 64 messages of
128 bytes and 32 messages of 2048 bytes, with one measured round by default.
An explicit `--trials` overrides that count. Smoke timings are tool validation,
not performance results; startup and 5 ms sampling dominate tiny inputs.

## Final measurement, after fixes and checks stabilize

Have the parent task finish the bug fixes, full checks, and release build first.
Freeze that executable, then run the following from the repository root:

```sh
bench_root=$(mktemp -d /tmp/ciallogcat-q09-final.XXXXXX)
install -m 755 target/release/ciallogcat-cli "$bench_root/ciallogcat-cli"
python3 -B tools/transport-benchmark.py \
  --cli "$bench_root/ciallogcat-cli" \
  --output-dir "$bench_root/report" \
  --include-compact --trials 5
```

This runs two workloads: 50,000 × 128-byte messages and 20,000 × 2048-byte
messages. Each workload generates its text/proto wire inputs once and reuses
them for all trials; both proto formats consume exactly the same `.proto`
file. Message/tag/PID/TID/level match across transports. Proto additionally
carries UID, integer time, process name, and raw payload; text has its original
threadtime timestamp representation. Input, executable, replay, and tool hashes
are recorded. Do not rebuild/replace the frozen executable during measurement;
a changed before/after hash makes the report fail.

Each workload has one warmup round, then five measured rounds: 6 warmup runs
plus 30 measured runs in total. The initial list is `v2/text`, `v2/proto`,
`v3/proto`; round N rotates it left by N positions, with warmup N=0. With two
combinations this reproduces the old alternating order. `round_order`,
`trial`, `warmup`, `order_position` and `case_run_index` record actual order.
Five measured rounds cannot balance three positions perfectly; order is
deterministic and exposed rather than claimed to be fully balanced.

## Report and integrity contract

`$bench_root/report/report.json` is atomically updated after each run.
`runs.jsonl` and stdout contain individual run records; the last stdout line
contains the report path and summaries. The report schema is now version 2:
`cases[].summary` and `cases[].median` use keys `v2/text`, `v2/proto`, `v3/proto`
(old transport-only summary keys have changed). Each run explicitly carries
`transport`, `record_format`, argv, relative artifact/stdout/stderr paths and:

- Wall seconds, CPU user/system/total seconds, records/s and message MiB/s.
  Wall includes CLI startup, replay and process cleanup. Generation, mock
  compilation, hashing and validation are outside the timed interval. CPU is
  `wait4` usage for the CLI plus replay children it reaped, excluding Python.
- CLI-only sampled peak `VmRSS` and `VmHWM` in KiB, sample count and interval.
  No samples produces null, not a false zero. Separate `wait4_maxrss_kib` may
  include pre-exec/inherited high-water usage; it is not sampled CLI RSS or a
  process-tree sum. The earlier tool's `cli_sampled_peak_rss_kib` actually
  sampled VmHWM; that is now named `cli_sampled_peak_hwm_kib`.
- Artifact bytes, return code, timeout flag, runner `exit_reason`, CLI status
  and its `cli_exit_reason`. Successful runs must end at `max_entries`.
- Verified record count, exact sequences/provenance, header/version/transport,
  original base64-decoded payload bytes against each 8-byte-length-prefixed
  replay frame, and footer identity/count/bounds/scope/stop reason/known gaps.
  V2 also checks display and decoded evidence. V3 requires raw-only compact
  fields and its encoding declarations; absent display fields are expected.
  CLI status entries, bytes, visible end and artifact/session IDs must match.

Successful non-warmup runs alone enter medians. Every group reports successful,
failed and expected counts, warmup status and `complete`. No successful samples
produces a null median. Any failed warmup or measurement makes the overall
command exit 1, even when other groups pass; inspect `state`, `success` and all
group `complete` fields before using a report. An interrupted/in-progress
report remains `state: running` and is not a completed result. Per-run failures
remain in the journal/report, with logs and any partial artifact retained.
Successful artifacts are retained only for the final round; all run logs and
input files remain. Filenames are
`{case}-{trial}-{record_format}-{transport}.{jsonl,stdout,stderr}`.

The mock explicitly flushes stdout before waiting, so even a partial buffer is
delivered. Capture has a 60-second bound, the harness kills its own private
process group at 75 seconds, and mock processes have a 90-second backstop.
Monitoring only reads the spawned CLI PID and waits for that child; there is
no global process scan or process-name kill. Keep the host otherwise quiet
for final measurements. Empty `known_gaps` certifies only this bounded fixture
check, never complete device capture.
