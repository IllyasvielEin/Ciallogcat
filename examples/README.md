# External test executor + Ciallogcat

`agent_capture.py` runs one command supplied after `--`, surrounds it with a
finite native capture, and prints one JSON report. Python 3.10+ and a
`ciallogcat-cli` build with `query --start-at end` are required. Device actions remain in the
external command. See the [workflow and report contract](../docs/agent-test-workflow.md).

Before running the command, the wrapper creates a tail cursor at the current
published boundary in one bounded request. Startup history does not consume the
later evidence page budget. Evidence follows that cursor with explicit page,
scan, response and time limits. These are host publication observations; delayed
device logs may cross them. Skipped history remains unverified, even if the
suffix reaches a valid footer (`validation.footer_seen=true` while
`artifact_finalized=false`).

From the repository root, run the deterministic tests without a device or build:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s examples -p test_agent_capture.py -v
```

A complete Linux/macOS mock example follows. The fixture never contacts a device;
it has its own eight-second watchdog, so this example keeps the action and tail
short. `mktemp` gives each invocation a private parent directory; `session` must
not already exist. The deliberately failing executor makes the wrapper exit 1.

```sh
cargo build --locked --release --no-default-features --bin ciallogcat-cli
case_dir="$(mktemp -d)"
cp tests/fixtures/fake_adb.py "$case_dir/mock-adb"
chmod +x "$case_dir/mock-adb"
CIALLOGCAT_CONFIG_DIR="$case_dir/config" \
CIALLOGCAT_FAKE_SCENARIO=continuous \
python3 examples/agent_capture.py \
  --cli ./target/release/ciallogcat-cli --adb "$case_dir/mock-adb" \
  --serial TEST-USB --session-dir "$case_dir/session" \
  --duration-secs 30 --ready-timeout 5 --command-timeout 5 \
  --action-timeout 2 --failure-tail-secs 1 \
  --max-entries 2000 --max-bytes 1048576 \
  -- python3 -c 'import sys,time; time.sleep(0.25); sys.exit(7)' \
  > "$case_dir/report.json"
python3 -m json.tool "$case_dir/report.json"
```

The report separates executor exit 7 from the capture result. After an
acknowledged stop, open `$case_dir/session/logs.jsonl` in the GUI. Preserve the
report with that artifact. A successful wrapper exit does not establish complete
capture or device health; inspect the capture observations.

For a real device with ADB and an existing Maestro flow:

```sh
case_dir="$(mktemp -d)"
python3 examples/agent_capture.py \
  --serial DEVICE_SERIAL --session-dir "$case_dir/session" \
  --duration-secs 150 --action-timeout 90 --failure-tail-secs 2 \
  --max-entries 100000 --max-bytes 67108864 \
  -- maestro test flow.yaml > "$case_dir/report.json"
```

Executor stdout/stderr both go to the wrapper's stderr; stdout stays machine
readable. Arguments are passed literally without a shell. Use an executor that
finishes its own device cleanup and child processes within the deadline. POSIX
process groups also receive bounded cleanup from the wrapper; Windows guarantees
cleanup of the direct executor process only.
