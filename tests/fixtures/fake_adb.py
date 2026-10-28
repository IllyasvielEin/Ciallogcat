#!/usr/bin/env python3
"""Strict, deterministic, stdlib-only ADB process fixture; never calls real adb.

Configuration:
    CIALLOGCAT_FAKE_SCENARIO selects one of SCENARIOS (default: normal).
    CIALLOGCAT_FAKE_TRACE optionally names an append-only JSONL trace file.

Accepted argv, and only these forms:
    devices -l
    -s SERIAL shell pm list packages -3
    -s SERIAL shell ps -A -o PID,NAME
    -s SERIAL shell ps -A
    -s SERIAL features
    -s SERIAL shell -T -n "exec logcat --help"
    -s SERIAL shell -T -n "exec logcat (-b BUFFER)+ (-g|-S|-p)"
    -s SERIAL shell -T -n "getprop (ro.build.version.sdk|ro.build.version.release|ro.build.fingerprint)"
    -s SERIAL logcat -v threadtime SCOPE (-b BUFFER)+
    -s SERIAL shell -T -n "exec logcat --proto -v threadtime SCOPE (-b BUFFER)+ '*:V'"

SCOPE is -T N/TIME, -t N/TIME or -d. Calendar TIME is a single quoted shell word.
The history scenario has three stored records at 10-23 12:00:01/02/03.000;
following emits a fourth record then idles. Dump selects history and exits.

SERIAL must be an available device in the selected scenario. BUFFER is one of
main, system, crash, events, radio, all. Unrecognized argv/scenarios return 2;
unavailable devices return 1. The trace logs every invocation, including invalid
ones, as {"pid": int, "args": [str, ...], "scenario": str, "event": "invocation"}.
Each record is appended with one os.write on an O_APPEND file descriptor. Extra
records have the same keys and identify completion events as described below.

Output contract (bytes, unless otherwise noted):
* normal: TEST-USB device, model Test_Phone. Package lines contain zeta, alpha,
  alpha under com.example. Both process formats map PID 1234 to
  com.example.alpha, PID 1235 to com.example.alpha:sync, and PID 2222 to
  com.example.zeta. Logcat writes MARKER then
  one THREADTIME_PREFIX + b"hello from fake adb\\n", then exits 0.
* no_devices: only b"List of devices attached\\n\\n".
* device_states: TEST-USB device, TEST-UNAUTHORIZED unauthorized, TEST-OFFLINE
  offline, in that order. Only TEST-USB can execute commands.
* multi_device: TEST-USB, 192.0.2.1:5555, emulator-5554 are available, in that
  order, with Test_Phone, Test_Wifi, Test_Emulator models respectively.
* ps_fallback: preferred ps fails with status 1 and
  b"fake adb: unsupported ps format\\n"; fallback ps returns the normal table.
* ps_empty: preferred ps returns b"PID NAME\\n"; fallback returns normal table.
* query_error: every valid query returns 23, no stdout, and
  b"fake adb: query failed\\n" on stderr. Logcat is normal.
* query_hang: every valid query blocks until the independent 8-second watchdog
  forcibly terminates the process with status 124. Logcat is normal.
* stream_edges: MARKER; messages "unicode café 雪 🚀", b"invalid \\xff utf8",
  b"oversized " + 1 MiB + 256 x bytes, "after oversized", "final partial".
  Each message uses THREADTIME_PREFIX. Only the final line lacks a newline.
* stderr_exit: no stdout; a 2 MiB x diagnostic, 4096 invalid UTF-8 bytes,
  then b"fake adb: logcat failed\\n" on stderr, status 17.
* eof_open: emits one "before stdout closes" message, closes fd 1, records
  event="stdout_closed", and remains alive until killed or watchdog expiry.
* once_then_idle: emits one "before idle" message, records event="idle", and
  keeps both output pipes open without further writes until killed/watchdog.
* stderr_tail: emits one "before stderr tail" message, closes stdout, then
  writes 128 diagnostics tail-000..tail-127 after 50 ms and exits 17. Both
  event="stdout_closed" and event="stderr_complete" are traced.
* continuous: messages continuous-000000, continuous-000001, ... every 20 ms
  until killed or watchdog expiry, with each write delivered immediately.
* burst: exactly 128 marker lines, each 256 KiB including prefix/newline. Lines
  start "--------- burst-0000 ", "--------- burst-0001 ", ... and are padded
  with x bytes. Total stdout is 32 MiB. Markers isolate queue backpressure from
  threadtime-regex cost; stream_edges covers an oversized threadtime line.
  event="burst_complete", lines=128, bytes=33554432 is traced only after all
  bytes have been written; then the process exits 0.
* proto_normal: one independently encoded protobuf frame (8-byte LE length),
  priority FATAL=8, trailing-NUL tag, embedded-NUL/invalid-UTF8 message, process.
  These synthetic bytes test transport fidelity, not device liblog preservation.
* proto_empty / proto_unknown_only: one empty / unknown-field-only payload.
* proto_zero_records: successful empty stream, with no schema evidence.
* proto_unknown_priority: numeric 7 (not protobuf Fatal) plus ordinary fields.
* proto_malformed / proto_truncated / proto_oversized: invalid wire payload /
  partial frame payload / 1 MiB + 1 declared length, without allocating payload.
* proto_unsupported / proto_permission: no stdout, diagnostic stderr, exit 1.
* proto_no_shell_v2: features lacks the exact shell_v2 token.
* proto_feature_error: features exits 23; no capability inference is possible.
* proto_feature_error_large: features exits 23 after 2 MiB of invalid-UTF8 stderr;
  this must become a bounded status diagnostic rather than unbounded metadata.
* proto_partial_then_idle: one complete frame, then a declared frame containing
  only its first payload byte. Both pipes remain open until stop/watchdog.
* diagnostic_partial: sizes succeed; statistics permission denied with retained
  stdout; prune unsupported; release has an unknown failure. Other items work.
* diagnostic_permission / diagnostic_unsupported: help returns the named failure.
* diagnostic_missing_device: fixed diagnostic queries fail with standard ADB
  device-not-found errors, echoing the exact serial (including embedded quotes).
* diagnostic_error_statements: distinguish error statements from quoted option
  arguments, incidental output, and unfamiliar/localized failures; all exit 1.
* diagnostic_truncated_statements: explanatory prefixes must not become error
  causes when capped; also emits complete reasons before a discarded suffix.
* diagnostic_silent / diagnostic_stderr_forever: help stays alive, either silent
  or continuously writing stderr. Watchdog is independent of production bounds.
* diagnostic_large: help writes distinct 224 KiB stdout/stderr with control and
  invalid UTF-8 bytes, then exits 0. diagnostic_help_stderr writes help to stderr.

All other commands in specialized scenarios use normal output. Every process
starts a daemon watchdog thread calling os._exit(124), even when its main thread
is blocked in os.write. No process can accidentally run indefinitely. Tests that
need executable dispatch may copy this source under ignored target/ and chmod
that copy; never place generated traces or executables in the source tree.
"""

from __future__ import annotations

import json
import os
import re
import shlex
import sys
import threading
import time

WATCHDOG_SECONDS = 8.0
WATCHDOG_EXIT = 124
SCENARIOS = frozenset(
    {
        "normal",
        "history",
        "no_devices",
        "device_states",
        "multi_device",
        "ps_fallback",
        "ps_empty",
        "query_error",
        "query_hang",
        "stream_edges",
        "stderr_exit",
        "eof_open",
        "once_then_idle",
        "stderr_tail",
        "continuous",
        "burst",
        "proto_normal", "proto_empty", "proto_unknown_only", "proto_zero_records",
        "proto_unknown_priority", "proto_malformed", "proto_truncated", "proto_oversized",
        "proto_unsupported", "proto_permission", "proto_no_shell_v2", "proto_feature_error",
        "proto_partial_then_idle",
        "proto_feature_error_large",
        "diagnostic_partial", "diagnostic_permission", "diagnostic_unsupported",
        "diagnostic_silent", "diagnostic_stderr_forever", "diagnostic_large",
        "diagnostic_help_stderr", "diagnostic_slow",
        "diagnostic_missing_device", "diagnostic_error_statements",
        "diagnostic_truncated_statements",
    }
)
BUFFERS = frozenset({"main", "system", "crash", "events", "radio", "all"})
THREADTIME_PREFIX = b"10-11 10:20:30.123  1234  1235 I FakeAdb: "
MARKER = b"--------- beginning of main\n"
PACKAGES = (
    b"package:com.example.zeta\n"
    b"package:com.example.alpha\n"
    b"package:com.example.alpha\n"
)
PS_PREFERRED = (
    b"PID NAME\n1234 com.example.alpha\n"
    b"1235 com.example.alpha:sync\n2222 com.example.zeta\n"
)
PS_FALLBACK = (
    b"USER PID PPID VSZ RSS WCHAN ADDR S NAME\n"
    b"u0_a123 1234 1 123456 12345 0 0 S com.example.alpha\n"
    b"u0_a123 1235 1 123456 12345 0 0 S com.example.alpha:sync\n"
    b"u0_a124 2222 1 123456 12345 0 0 S com.example.zeta\n"
)
BURST_LINES = 128
BURST_LINE_BYTES = 256 * 1024


def write_all(fd: int, data: bytes) -> None:
    """Use unbuffered binary writes, including for deliberately invalid UTF-8."""
    pending = memoryview(data)
    while pending:
        count = os.write(fd, pending)
        if count <= 0:
            raise OSError("write made no progress")
        pending = pending[count:]


def trace(args: list[str], scenario: str, event: str, **details: object) -> None:
    path = os.environ.get("CIALLOGCAT_FAKE_TRACE")
    if not path:
        return
    record = {"pid": os.getpid(), "args": args, "scenario": scenario, "event": event}
    record.update(details)
    data = (json.dumps(record, ensure_ascii=True, separators=(",", ":")) + "\n").encode()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        # Do not split a record into several writes: concurrent fixtures share it.
        if os.write(fd, data) != len(data):
            raise OSError("incomplete trace write")
    finally:
        os.close(fd)


def device_rows(scenario: str) -> list[tuple[str, str, str]]:
    rows = [("TEST-USB", "device", "Test_Phone")]
    if scenario == "no_devices":
        return []
    if scenario == "device_states":
        rows += [
            ("TEST-UNAUTHORIZED", "unauthorized", "Test_Unauthorized"),
            ("TEST-OFFLINE", "offline", "Test_Offline"),
        ]
    elif scenario == "multi_device":
        rows += [
            ("192.0.2.1:5555", "device", "Test_Wifi"),
            ("emulator-5554", "device", "Test_Emulator"),
        ]
    return rows


def devices_output(scenario: str) -> bytes:
    rows = ["List of devices attached\n"]
    for index, (serial, state, model) in enumerate(device_rows(scenario), 1):
        rows.append(
            f"{serial}\t{state} product:fake model:{model} "
            f"device:fake transport_id:{index}\n"
        )
    rows.append("\n")
    return "".join(rows).encode("ascii")


def scope_and_buffers(args: list[str]) -> bool:
    if args[:1] == ["-d"]:
        remaining = args[1:]
    elif len(args) >= 2 and args[0] in ("-t", "-T") and re.fullmatch(
        r"[1-9][0-9]*|[0-9]+\.[0-9]{1,9}|(?:[0-9]{4}-)?[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{1,9}", args[1]
    ):
        remaining = args[2:]
    else:
        return False
    return bool(remaining) and len(remaining) % 2 == 0 and all(
        remaining[i] == "-b" and remaining[i + 1] in BUFFERS
        for i in range(0, len(remaining), 2)
    )


def parse_command(args: list[str]) -> tuple[str, str | None]:
    if args == ["devices", "-l"]:
        return "devices", None
    if len(args) < 3 or args[0] != "-s" or not args[1] or args[1].startswith("-"):
        raise ValueError("unsupported argv")
    serial, rest = args[1], args[2:]
    if rest == ["features"]:
        return "features", serial
    if rest == ["shell", "pm", "list", "packages", "-3"]:
        return "packages", serial
    if rest == ["shell", "ps", "-A", "-o", "PID,NAME"]:
        return "ps_preferred", serial
    if rest == ["shell", "ps", "-A"]:
        return "ps_fallback", serial
    if len(rest) == 4 and rest[:3] == ["shell", "-T", "-n"]:
        # Parse quoting without executing a shell; reject all extra words.
        remote = shlex.split(rest[3])
        if remote == ["exec", "logcat", "--help"]:
            return "diagnostic_help", serial
        properties = {"ro.build.version.sdk": "sdk", "ro.build.version.release": "release",
                      "ro.build.fingerprint": "fingerprint"}
        if len(remote) == 2 and remote[0] == "getprop" and remote[1] in properties:
            return "diagnostic_" + properties[remote[1]], serial
        if remote[:2] == ["exec", "logcat"] and remote[-1:] in (["-g"], ["-S"], ["-p"]):
            selected = remote[2:-1]
            if selected and len(selected) % 2 == 0 and all(
                selected[i] == "-b" and selected[i + 1] in BUFFERS
                for i in range(0, len(selected), 2)
            ):
                return {"-g": "diagnostic_sizes", "-S": "diagnostic_statistics",
                        "-p": "diagnostic_prune"}[remote[-1]], serial
        if remote[:5] == ["exec", "logcat", "--proto", "-v", "threadtime"] and remote[-1:] == ["*:V"] and scope_and_buffers(remote[5:-1]):
            return "proto", serial
    if rest[:3] == ["logcat", "-v", "threadtime"] and scope_and_buffers(rest[3:]):
        return "logcat", serial
    raise ValueError("unsupported argv")


def line(message: bytes, *, newline: bool = True) -> bytes:
    return THREADTIME_PREFIX + message + (b"\n" if newline else b"")


def wait_forever() -> None:
    # The watchdog, not this loop, enforces the deadline.
    while True:
        time.sleep(3600)


def proto_payload(priority: int = 8) -> bytes:
    # Hand-encoded official fields, intentionally independent of Rust/prost.
    return (b"\x08\x01\x10\x02\x18" + bytes([priority])
            + b"\x20\x2a\x28\x04\x30\x05\x3a\x04Tag\x00"
            + b"\x42\x04m\x00\xff\n\x4a\x03app")


def proto_frame(payload: bytes) -> bytes:
    return len(payload).to_bytes(8, "little") + payload


def diagnostic(command: str, args: list[str], scenario: str) -> int:
    if scenario == "diagnostic_truncated_statements":
        stdout, stderr = {
            "diagnostic_help": (b"", b"Permission denied is an example from the help, not the error cause.\nUnrecognized failure.\n"),
            "diagnostic_sizes": (b"not supported is an example, not the failure reason.\n", b""),
            "diagnostic_statistics": (b"", b"Permission denied\n" + b"x" * 80),
            "diagnostic_prune": (b"unknown option '-p'\n" + b"x" * 80, b""),
            "diagnostic_sdk": (b"Permission denied", b""),
            "diagnostic_release": (b"", b"not supported\n" + b"x" * 80),
            "diagnostic_fingerprint": (b"", b"unknown optional format\n"),
        }[command]
        write_all(1, stdout)
        write_all(2, stderr)
        return 1
    if scenario == "diagnostic_error_statements":
        stdout, stderr = {
            "diagnostic_help": (b"Usage example: 'permission denied' / 'unknown option'\n",
                                b"logcat: unexpected failure\n"),
            "diagnostic_sizes": (b"", b"logcat: unrecognized option '--permission denied'\n"),
            "diagnostic_statistics": (b"logcat: Permission denied\n", b""),
            "diagnostic_prune": (b"logcat: unknown option '-p'\n", b""),
            "diagnostic_sdk": (b"", "诊断失败：permission denied\n".encode()),
            "diagnostic_release": (b"", b"logcat: unknown optional format\n"),
            "diagnostic_fingerprint": (b"", b"logcat: failed to inspect 'permission denied': unexpected failure\n"),
        }[command]
        write_all(1, stdout)
        write_all(2, stderr)
        return 1
    if scenario == "diagnostic_slow":
        time.sleep(0.15)
    if command == "diagnostic_help":
        if scenario == "diagnostic_silent":
            trace(args, scenario, "diagnostic_idle")
            wait_forever()
        if scenario == "diagnostic_stderr_forever":
            trace(args, scenario, "diagnostic_flood")
            while True:
                write_all(2, b"continuous diagnostic stderr\n" * 256)
        if scenario == "diagnostic_large":
            write_all(1, b"OUT\x00\"\\\n" * 32768)
            write_all(2, b"ERR\xff\r\t\n" * 32768)
            return 0
        if scenario == "diagnostic_permission":
            write_all(2, b"logcat: Permission denied\n")
            return 1
        if scenario == "diagnostic_unsupported":
            write_all(2, b"logcat: unrecognized option '--help'\n")
            return 1
    if scenario == "diagnostic_partial":
        if command == "diagnostic_statistics":
            write_all(1, b"partial statistics header\n")
            write_all(2, b"logcat: failed to read statistics: Permission denied\n")
            return 13
        if command == "diagnostic_prune":
            write_all(2, b"logcat: invalid option -- 'p'\n")
            return 2
        if command == "diagnostic_release":
            write_all(2, b"adb: transport disconnected\n")
            return 17
    output = {
        # Deliberately omit nsec and proto, while reporting a recent SDK. Help
        # absence and API-level observations cannot establish OEM support.
        "diagnostic_help": b"Usage: logcat [options]\n -g buffer sizes\n -S statistics\n -p prune list\n -v threadtime,epoch\n",
        "diagnostic_sizes": b"main: ring buffer is 256 KiB (64 KiB consumed), max entry is 5120 B\n",
        "diagnostic_statistics": b"size/num main system crash\nTotal 1234/42 98/2 0/0\n",
        "diagnostic_prune": b"~! ~1000/!\n",
        "diagnostic_sdk": b"35\n",
        "diagnostic_release": b"15\n",
        "diagnostic_fingerprint": b"mock/vendor/device:15/BUILD/123:user/release-keys\n",
    }[command]
    write_all(2 if scenario == "diagnostic_help_stderr" and command == "diagnostic_help" else 1, output)
    return 0


def proto_logcat(args: list[str], scenario: str) -> int:
    if scenario == "proto_partial_then_idle":
        write_all(1, proto_frame(proto_payload()) + (5).to_bytes(8, "little") + b"\x08")
        trace(args, scenario, "proto_partial")
        wait_forever()
    if scenario in ("proto_unsupported", "proto_permission"):
        diagnostic = (b"logcat: unrecognized option '--proto'\n"
                      if scenario == "proto_unsupported" else b"logcat: Permission denied\n")
        write_all(2, diagnostic)
        return 1
    if scenario == "proto_zero_records":
        return 0
    if scenario == "proto_oversized":
        data = (1024 * 1024 + 1).to_bytes(8, "little")
    elif scenario == "proto_truncated":
        data = proto_frame(proto_payload())[:-1]
    else:
        payload = {"proto_empty": b"", "proto_unknown_only": b"\x98\x06\x01",
                   "proto_malformed": b"\x00",
                   "proto_unknown_priority": proto_payload(7)}.get(scenario, proto_payload())
        data = proto_frame(payload)
    write_all(1, data)
    return 0


def logcat(args: list[str], scenario: str) -> int:
    if scenario == "history":
        flag = args[5]
        records = [(f"10-23 12:00:0{i}.000", f"history-{i}") for i in range(1, 4)]
        if flag != "-d":
            value = args[6]
            if value.isdigit():
                records = records[-int(value):]
            else:
                records = [(t, msg) for t, msg in records if t >= value]
        for timestamp, message in records:
            write_all(1, f"{timestamp} 1234 1235 I History: {message}\n".encode())
        if flag == "-T":
            write_all(1, line(b"live-after-history"))
            wait_forever()
        write_all(2, b"snapshot drained\n")
        return 0
    if scenario == "stderr_exit":
        write_all(2, b"x" * (2 * 1024 * 1024) + b"\n")
        write_all(2, b"\xff" * 4096 + b"\n")
        write_all(2, b"fake adb: logcat failed\n")
        return 17
    if scenario == "stream_edges":
        write_all(1, MARKER)
        write_all(1, line("unicode café 雪 🚀".encode("utf-8")))
        write_all(1, line(b"invalid \xff utf8"))
        write_all(1, line(b"oversized " + b"x" * (1024 * 1024 + 256)))
        write_all(1, line(b"after oversized"))
        write_all(1, line(b"final partial", newline=False))
        return 0
    if scenario == "eof_open":
        write_all(1, line(b"before stdout closes"))
        os.close(1)
        trace(args, scenario, "stdout_closed")
        wait_forever()
    if scenario == "once_then_idle":
        write_all(1, line(b"before idle"))
        trace(args, scenario, "idle")
        wait_forever()
    if scenario == "stderr_tail":
        write_all(1, line(b"before stderr tail"))
        os.close(1)
        trace(args, scenario, "stdout_closed")
        time.sleep(0.05)
        write_all(2, b"".join(f"tail-{index:03d}\n".encode() for index in range(128)))
        trace(args, scenario, "stderr_complete")
        return 17
    if scenario == "continuous":
        number = 0
        while True:
            write_all(1, line(f"continuous-{number:06d}".encode("ascii")))
            number += 1
            time.sleep(0.02)
    if scenario == "burst":
        for number in range(BURST_LINES):
            prefix = f"--------- burst-{number:04d} ".encode("ascii")
            padding = BURST_LINE_BYTES - len(prefix) - 1
            write_all(1, prefix + b"x" * padding + b"\n")
        trace(
            args,
            scenario,
            "burst_complete",
            lines=BURST_LINES,
            bytes=BURST_LINES * BURST_LINE_BYTES,
        )
        return 0
    write_all(1, MARKER + line(b"hello from fake adb"))
    return 0


def main() -> int:
    # os._exit is essential: sys.exit in a thread only exits that thread, and
    # buffered I/O/finalizers could themselves block while stdout is full.
    watchdog = threading.Timer(WATCHDOG_SECONDS, os._exit, args=(WATCHDOG_EXIT,))
    watchdog.daemon = True
    watchdog.start()
    args = sys.argv[1:]
    scenario = os.environ.get("CIALLOGCAT_FAKE_SCENARIO", "normal")
    trace(args, scenario, "invocation")
    if scenario not in SCENARIOS:
        write_all(2, f"fake adb: unknown scenario: {scenario}\n".encode())
        return 2
    try:
        command, serial = parse_command(args)
    except ValueError:
        write_all(2, ("fake adb: unsupported argv: " + json.dumps(args) + "\n").encode())
        return 2
    if scenario == "diagnostic_missing_device" and (
        command == "features" or command.startswith("diagnostic_")
    ):
        prefix = "adb: error: failed to get feature set: " if command == "features" else "error: "
        write_all(2, f"{prefix}device '{serial}' not found\n".encode())
        return 1
    if serial is not None:
        states = {item[0]: item[1] for item in device_rows(scenario)}
        if serial not in states:
            write_all(2, f"fake adb: unknown serial: {serial}\n".encode())
            return 2
        if states[serial] != "device":
            write_all(2, f"fake adb: device {serial} is {states[serial]}\n".encode())
            return 1
    if command == "logcat":
        return logcat(args, scenario)
    if command == "proto":
        return proto_logcat(args, scenario)
    if command.startswith("diagnostic_"):
        return diagnostic(command, args, scenario)
    if scenario == "query_error":
        write_all(2, b"fake adb: query failed\n")
        return 23
    if scenario == "query_hang":
        wait_forever()
    if command == "devices":
        output = devices_output(scenario)
    elif command == "features":
        if scenario == "proto_feature_error_large":
            write_all(2, b"fake adb: features query failed " + b"\xff" * (2 * 1024 * 1024))
            return 23
        if scenario == "proto_feature_error":
            write_all(2, b"fake adb: features query failed\n")
            return 23
        output = (b"cmd,not_shell_v2,shell_v2_extra\n" if scenario == "proto_no_shell_v2"
                  else b"cmd,shell_v2,stat_v2\n")
    elif command == "packages":
        output = PACKAGES
    elif command == "ps_preferred":
        if scenario == "ps_fallback":
            write_all(2, b"fake adb: unsupported ps format\n")
            return 1
        output = b"PID NAME\n" if scenario == "ps_empty" else PS_PREFERRED
    else:
        output = PS_FALLBACK
    write_all(1, output)
    return 0


if __name__ == "__main__":
    try:
        status = main()
    except BrokenPipeError:
        status = 32
    except OSError as error:
        # A broken trace/output destination is a fixture failure, never a reason
        # to fall through to a locally installed adb.
        try:
            write_all(2, f"fake adb: I/O failure: {error}\n".encode())
        except OSError:
            pass
        status = 74
    os._exit(status)
