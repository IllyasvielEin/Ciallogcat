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
    -s SERIAL logcat -v threadtime -T 1 (-b BUFFER)+

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
* continuous: messages continuous-000000, continuous-000001, ... every 20 ms
  until killed or watchdog expiry, with each write delivered immediately.
* burst: exactly 128 marker lines, each 256 KiB including prefix/newline. Lines
  start "--------- burst-0000 ", "--------- burst-0001 ", ... and are padded
  with x bytes. Total stdout is 32 MiB. Markers isolate queue backpressure from
  threadtime-regex cost; stream_edges covers an oversized threadtime line.
  event="burst_complete", lines=128, bytes=33554432 is traced only after all
  bytes have been written; then the process exits 0.

All other commands in specialized scenarios use normal output. Every process
starts a daemon watchdog thread calling os._exit(124), even when its main thread
is blocked in os.write. No process can accidentally run indefinitely. Tests that
need executable dispatch may copy this source under ignored target/ and chmod
that copy; never place generated traces or executables in the source tree.
"""

from __future__ import annotations

import json
import os
import sys
import threading
import time

WATCHDOG_SECONDS = 8.0
WATCHDOG_EXIT = 124
SCENARIOS = frozenset(
    {
        "normal",
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
        "continuous",
        "burst",
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


def parse_command(args: list[str]) -> tuple[str, str | None]:
    if args == ["devices", "-l"]:
        return "devices", None
    if len(args) < 3 or args[0] != "-s" or not args[1] or args[1].startswith("-"):
        raise ValueError("unsupported argv")
    serial, rest = args[1], args[2:]
    if rest == ["shell", "pm", "list", "packages", "-3"]:
        return "packages", serial
    if rest == ["shell", "ps", "-A", "-o", "PID,NAME"]:
        return "ps_preferred", serial
    if rest == ["shell", "ps", "-A"]:
        return "ps_fallback", serial
    if rest[:5] == ["logcat", "-v", "threadtime", "-T", "1"]:
        buffer_args = rest[5:]
        if buffer_args and len(buffer_args) % 2 == 0 and all(
            buffer_args[index] == "-b" and buffer_args[index + 1] in BUFFERS
            for index in range(0, len(buffer_args), 2)
        ):
            return "logcat", serial
    raise ValueError("unsupported argv")


def line(message: bytes, *, newline: bool = True) -> bytes:
    return THREADTIME_PREFIX + message + (b"\n" if newline else b"")


def wait_forever() -> None:
    # The watchdog, not this loop, enforces the deadline.
    while True:
        time.sleep(3600)


def logcat(args: list[str], scenario: str) -> int:
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
    if scenario == "query_error":
        write_all(2, b"fake adb: query failed\n")
        return 23
    if scenario == "query_hang":
        wait_forever()
    if command == "devices":
        output = devices_output(scenario)
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
