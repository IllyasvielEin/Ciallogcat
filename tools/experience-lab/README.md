# Native experience lab

`scenario_adb.py` is a stdlib-only, synthetic ADB peer for model-driven operation
of the real Ciallogcat binaries. It never launches ADB or contacts Android
hardware. Each participant needs a separate state directory and config directory.
Keep generated captures and reports under ignored `target/experience-2026-10-23/`.

```sh
chmod +x tools/experience-lab/scenario_adb.py
export CIALLOGCAT_LAB_DIR="$PWD/target/experience-2026-10-23/my-case/lab"
export CIALLOGCAT_CONFIG_DIR="$PWD/target/experience-2026-10-23/my-case/config"
python3 tools/experience-lab/scenario_adb.py --control "$CIALLOGCAT_LAB_DIR" --action init
tools/experience-lab/scenario_adb.py devices -l
```

Point the GUI's ADB path or CLI `--adb` argument at the absolute fixture path.
The default serial is `LAB-ANDROID`. `CIALLOGCAT_LAB_SERIAL` can override it on
initialization. Initialized devices start online. The lab directory must be
initialized before launching the application. Do not reinitialize a live case.

Control actions use a second process:

```sh
python3 tools/experience-lab/scenario_adb.py --control "$CIALLOGCAT_LAB_DIR" --action checkout
```

| Action | Operational effect |
| --- | --- |
| `init` | Reset state, create facilitator-only `oracle.json`; existing trace remains |
| `checkout` | Emit a short, correlated purchase failure investigation scene |
| `crash` | Emit a multiprocess crash scene, Unicode, stack, bounded long diagnostic |
| `offline` | Discovery shows offline; existing streams exit 17; device queries fail |
| `online` | Device available again; the application can reconnect |
| `discovery-error` | Only `devices -l` exits 23; established log streams remain live |
| `discovery-ok` | Restore discovery |
| `quiet` | Pause all stream emission while retaining open pipes |
| `recover` | Resume, advance application PIDs, emit recovery activity |
| `stop` | End current streams with exit 17; reject new streams until online/recover |

Ordinary traffic runs at approximately 20 records/second across shop, shop
payments, chat, and system processes. `pm list packages -3` returns the two
third-party packages; `system_server` is present only in process discovery.
Events are delivered to streams already running when an action is submitted.
New streams begin with current activity and do not replay past actions. Allow
about one second for checkout/crash sequences to complete before interrupting.
All output is synthetic and contains no personal data. A stream has a hard
10-minute watchdog and supports normal process termination.

Accepted argv are deliberately strict: `devices -l`, `-s SERIAL features`,
`-s SERIAL shell pm list packages -3`, both product `ps -A` forms, product text
logcat argv, and product `shell -T -n` explicit protobuf command. Unknown argv
exit 2, unavailable devices exit 1. Text and protobuf share scenario records;
protobuf frames use independent stdlib encoding with 8-byte little-endian sizes.

`state.json` is atomically replaced. Control calls serialize with a bounded
directory lock. If a control process is forcibly killed while holding the lock,
the directory can remain and later controls return `control lock busy`. Once
that case has no active control process, remove only its empty `.control-lock`
directory with `rmdir`, or start a new case directory. `trace.jsonl` records
timestamp, PID, argv, action, and phase.
Facilitators may read `oracle.json`; participants should investigate from captured
evidence and receive task charters without the oracle's answers. This is a native
application experience simulation, not human usability research or hardware
acceptance testing.

## Isolated native desktops

The harness operates an actual native GUI executable with XTest keyboard/mouse
input and own-window screenshots. Each session gets a private network namespace
and X11 socket directory, so multiple sessions never share focus or clipboard.
The host filesystem remains shared; these namespaces are for independent trusted
test applications, not a filesystem sandbox for untrusted executables.
It uses Xvfb/software graphics; this does not measure GPU latency, Windows, IME,
or actual Android hardware. The JSON input is a test transport, not a product API.

Host prerequisites: Linux user namespaces, bubblewrap, distro Python with
`python3-gi` / Gtk 3 introspection, and X11/XTest/XRes libraries. On a Debian/Ubuntu
host, prepare only local copies of Xvfb and its extra runtime dependencies:

```sh
python3 tools/experience-lab/prepare.py \
  --gui target/release/ciallogcat --cli target/release/ciallogcat-cli
python3 tools/experience-lab/scenario_adb.py \
  --control target/experience-2026-10-23/live/lab --action init
python3 tools/experience-lab/launch.py \
  --run-dir target/experience-2026-10-23/live
```

`prepare.py` uses `apt-get download` and `dpkg-deb -x`; it does not install packages
or require root. It records package and binary SHA-256 values. Network-restricted
runners may need their normal approval mechanism for the download and namespace
operations. `--cached-packages` reuses packages already in the selected lab.
`--lab-root` chooses another run collection. Do not re-prepare a runtime while
sessions use it. Existing binaries remain untouched; the lab runs frozen copies.
Cached packages must contain one archive per package/architecture; preparation
rejects duplicates rather than choosing a version by filename ordering.

`launch.py` starts a terminal-style session accepting one JSON command per line.
Wait for its `ready` result and inspect the screenshot before choosing coordinates.
For example (each object is a separate stdin line):

```json
{"op":"key","key":"o","mods":["Control_L"],"name":"open-dialog"}
{"op":"paste","text":"/absolute/path/to/evidence.jsonl","name":"path-entered"}
{"op":"click","x":250,"y":320,"name":"after-click"}
{"op":"shot","name":"current-state"}
{"op":"clipboard","name":"copied-reference"}
{"op":"quit"}
```

Coordinates above illustrate syntax, not universal button locations. Other
commands: `hover` (`x`,`y`), `scroll` (`steps`,`x`,`y`), `resize` (`width`,`height`),
and `text` (slow ASCII key input). `key` uses X keysyms such as `Return`, `Escape`,
`F3`, `Down`; modifiers include `Control_L` and `Shift_L`. `paste` uses the private
clipboard and native Ctrl+V, so it is not an IME test. Optional `wait` is a settling
delay in seconds and `name` requests an own-window PNG. A clipboard read verifies
that its owner belongs to the test GUI. Actions and results are timestamped in
`actions.jsonl`; app/Xvfb stderr stays alongside screenshots.

Use `--scale 1.5` in a separate run directory for forced X11 DPI. Window managers,
desktop portals, and monitor switching are outside this harness. `quit`/EOF stop
only the owned GUI process group and Xvfb; a 30-minute watchdog bounds forgotten
sessions. The mock stream additionally has its own 10-minute watchdog. For a
completed evidence file, stop capture in the UI/CLI before quitting the harness.

Keep participant task briefs separate from the oracle and from other reports.
Record whether public documentation or source code was consulted. Reproduce
findings on a fresh session before classifying them as product defects. See
[the experience protocol](../../docs/experience-testing.md).
