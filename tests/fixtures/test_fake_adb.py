#!/usr/bin/env python3
"""Independent fixture protocol tests; run with:

    python3 -B -m unittest discover -s tests/fixtures -p 'test_*.py' -v

No device, SDK, third-party Python package, network, or GUI is required. All
subprocess waits have deadlines. Generated traces and copied executables live
under the repository's ignored target/ directory and are removed after tests.
"""

from __future__ import annotations

import concurrent.futures
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

# Avoid putting import-time bytecode beside committed fixture source.
sys.dont_write_bytecode = True
SOURCE = Path(__file__).with_name("fake_adb.py").resolve()
spec = importlib.util.spec_from_file_location("fake_adb_fixture", SOURCE)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
ROOT = SOURCE.parents[2]
DEVICES = ["devices", "-l"]
PACKAGES = ["-s", "TEST-USB", "shell", "pm", "list", "packages", "-3"]
PS = ["-s", "TEST-USB", "shell", "ps", "-A", "-o", "PID,NAME"]
PS_FALLBACK = ["-s", "TEST-USB", "shell", "ps", "-A"]
LOGCAT = ["-s", "TEST-USB", "logcat", "-v", "threadtime", "-T", "1", "-b", "main"]


class FakeAdbTests(unittest.TestCase):
    def setUp(self) -> None:
        target = ROOT / "target"
        target.mkdir(exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(prefix="fake-adb-selftest-", dir=target)
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.script = self.directory / "fake-adb"
        shutil.copyfile(SOURCE, self.script)
        self.script.chmod(0o700)
        self.trace_path = self.directory / "trace.jsonl"
        self.children: list[subprocess.Popen] = []
        self.addCleanup(self.stop_children)

    def stop_children(self) -> None:
        for child in self.children:
            if child.poll() is None:
                child.kill()
            child.communicate(timeout=2)

    def environment(self, scenario: str) -> dict[str, str]:
        env = os.environ.copy()
        env["CIALLOGCAT_FAKE_SCENARIO"] = scenario
        env["CIALLOGCAT_FAKE_TRACE"] = str(self.trace_path)
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        return env

    def command(self, args: list[str]) -> list[str]:
        # Windows does not dispatch .py shebangs through CreateProcess.
        prefix = [str(self.script)] if os.name == "posix" else [sys.executable, str(self.script)]
        return prefix + args

    def run_fake(
        self, args: list[str], scenario: str = "normal", timeout: float = 3
    ) -> subprocess.CompletedProcess:
        return subprocess.run(
            self.command(args),
            env=self.environment(scenario),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )

    def spawn(self, args: list[str], scenario: str) -> subprocess.Popen:
        child = subprocess.Popen(
            self.command(args),
            env=self.environment(scenario),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.children.append(child)
        return child

    def records(self) -> list[dict]:
        if not self.trace_path.exists():
            return []
        return [json.loads(record) for record in self.trace_path.read_text().splitlines()]

    def wait_for_trace(self, child: subprocess.Popen, event: str) -> dict:
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            for record in self.records():
                if record["pid"] == child.pid and record["event"] == event:
                    return record
            if child.poll() is not None:
                self.fail(f"fixture exited {child.returncode} before {event}")
            time.sleep(0.01)
        self.fail(f"fixture did not record {event} within 2 seconds")

    def test_normal_queries_and_trace(self) -> None:
        commands = [DEVICES, PACKAGES, PS, PS_FALLBACK]
        expected = [
            b"List of devices attached\n"
            b"TEST-USB\tdevice product:fake model:Test_Phone device:fake transport_id:1\n\n",
            fixture.PACKAGES,
            fixture.PS_PREFERRED,
            fixture.PS_FALLBACK,
        ]
        for args, output in zip(commands, expected):
            with self.subTest(args=args):
                result = self.run_fake(args)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, output)
                self.assertEqual(result.stderr, b"")
        records = self.records()
        self.assertEqual([record["args"] for record in records], commands)
        self.assertTrue(all(record["scenario"] == "normal" for record in records))
        self.assertTrue(all(record["event"] == "invocation" for record in records))
        self.assertTrue(all(isinstance(record["pid"], int) and record["pid"] > 0 for record in records))
        self.assertEqual(len({record["pid"] for record in records}), len(commands))
        self.assertIn(b"1235 com.example.alpha:sync\n", fixture.PS_PREFERRED)

    def test_device_scenarios(self) -> None:
        self.assertEqual(self.run_fake(DEVICES, "no_devices").stdout, b"List of devices attached\n\n")
        states = self.run_fake(DEVICES, "device_states")
        self.assertEqual(states.returncode, 0)
        self.assertIn(b"TEST-USB\tdevice ", states.stdout)
        self.assertIn(b"TEST-UNAUTHORIZED\tunauthorized ", states.stdout)
        self.assertIn(b"TEST-OFFLINE\toffline ", states.stdout)
        multi = self.run_fake(DEVICES, "multi_device")
        self.assertEqual(multi.returncode, 0)
        for serial in ("TEST-USB", "192.0.2.1:5555", "emulator-5554"):
            self.assertIn((serial + "\tdevice ").encode(), multi.stdout)
            result = self.run_fake(["-s", serial] + PACKAGES[2:], "multi_device")
            self.assertEqual(result.returncode, 0, result.stderr)
        for serial in ("TEST-UNAUTHORIZED", "TEST-OFFLINE"):
            result = self.run_fake(["-s", serial] + PACKAGES[2:], "device_states")
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stdout, b"")

    def test_strict_arguments_never_fall_through(self) -> None:
        invalid = [
            [], ["version"], ["devices"], ["devices", "-l", "extra"],
            ["shell", "pm", "list", "packages", "-3"],
            ["-s", "", "shell", "ps", "-A"],
            ["-s", "OTHER", "shell", "ps", "-A"],
            ["-s", "TEST-USB", "shell", "pm", "list", "packages"],
            ["-s", "TEST-USB", "shell", "ps", "-A", "-o", "NAME,PID"],
            ["-s", "TEST-USB", "logcat"], LOGCAT[:-2], LOGCAT[:-1],
            LOGCAT[:-1] + ["unknown"], LOGCAT + ["--unexpected"],
            LOGCAT[:5] + ["brief"] + LOGCAT[6:],
            LOGCAT[:6] + ["2"] + LOGCAT[7:],
            ["-s", "TEST-USB", "install", "malicious.apk"],
            ["forward", "tcp:1234", "tcp:5678"],
        ]
        for args in invalid:
            with self.subTest(args=args):
                result = self.run_fake(args)
                self.assertEqual(result.returncode, 2, (args, result.stderr))
                self.assertEqual(result.stdout, b"")
                self.assertTrue(result.stderr.startswith(b"fake adb:"))
        self.assertEqual([record["args"] for record in self.records()], invalid)
        result = self.run_fake(DEVICES, "typo-scenario")
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stderr, b"fake adb: unknown scenario: typo-scenario\n")

    def test_preferred_process_failure_and_empty_output_have_working_fallback(self) -> None:
        preferred = self.run_fake(PS, "ps_fallback")
        self.assertEqual(preferred.returncode, 1)
        self.assertEqual(preferred.stdout, b"")
        self.assertEqual(preferred.stderr, b"fake adb: unsupported ps format\n")
        preferred = self.run_fake(PS, "ps_empty")
        self.assertEqual(preferred.returncode, 0)
        self.assertEqual(preferred.stdout, b"PID NAME\n")
        for scenario in ("ps_fallback", "ps_empty"):
            with self.subTest(scenario=scenario):
                fallback = self.run_fake(PS_FALLBACK, scenario)
                self.assertEqual(fallback.returncode, 0)
                self.assertEqual(fallback.stdout, fixture.PS_FALLBACK)

    def test_query_errors_are_nonzero_and_stderr_only(self) -> None:
        for args in (DEVICES, PACKAGES, PS, PS_FALLBACK):
            with self.subTest(args=args):
                result = self.run_fake(args, "query_error")
                self.assertEqual(result.returncode, 23)
                self.assertEqual(result.stdout, b"")
                self.assertEqual(result.stderr, b"fake adb: query failed\n")

    def test_logcat_buffers_normal_output_and_stderr_exit(self) -> None:
        for buffers in (["main", "system", "crash"], ["all"], ["events", "radio"]):
            args = LOGCAT[:-2] + [part for buffer in buffers for part in ("-b", buffer)]
            with self.subTest(buffers=buffers):
                result = self.run_fake(args)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.stdout, fixture.MARKER + fixture.line(b"hello from fake adb"))
                self.assertEqual(result.stderr, b"")
                self.assertEqual(self.records()[-1]["args"], args)
        result = self.run_fake(LOGCAT, "stderr_exit")
        self.assertEqual(result.returncode, 17)
        self.assertEqual(result.stdout, b"")
        self.assertEqual(result.stderr, b"x" * (2 * 1024 * 1024) + b"\n" + b"\xff" * 4096 + b"\nfake adb: logcat failed\n")

    def test_stream_edges_are_literal_bytes_and_preserve_boundary_lines(self) -> None:
        result = self.run_fake(LOGCAT, "stream_edges")
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.split(b"\n")
        self.assertEqual(len(lines), 6)
        self.assertEqual(lines[0] + b"\n", fixture.MARKER)
        self.assertEqual(lines[1], fixture.line("unicode café 雪 🚀".encode(), newline=False))
        self.assertEqual(lines[2], fixture.line(b"invalid \xff utf8", newline=False))
        self.assertGreater(len(lines[3]), 1024 * 1024)
        self.assertEqual(lines[3], fixture.line(b"oversized " + b"x" * (1024 * 1024 + 256), newline=False))
        self.assertEqual(lines[4], fixture.line(b"after oversized", newline=False))
        self.assertEqual(lines[5], fixture.line(b"final partial", newline=False))
        self.assertFalse(result.stdout.endswith(b"\n"))

    def test_stdout_eof_does_not_imply_process_exit(self) -> None:
        child = self.spawn(LOGCAT, "eof_open")
        self.wait_for_trace(child, "stdout_closed")
        self.assertIsNone(child.poll())
        # Keep the read bounded even if the fixture stops closing its pipe.
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
            future = executor.submit(child.stdout.read)
            try:
                output = future.result(timeout=1)
            except concurrent.futures.TimeoutError:
                child.kill()
                raise AssertionError("stdout did not reach EOF within 1 second")
        self.assertEqual(output, fixture.line(b"before stdout closes"))
        self.assertIsNone(child.poll())
        child.kill()
        child.wait(timeout=2)

    def test_continuous_stream_flushes_and_can_be_killed(self) -> None:
        child = self.spawn(LOGCAT, "continuous")
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
            future = executor.submit(child.stdout.readline)
            try:
                first = future.result(timeout=2)
            except concurrent.futures.TimeoutError:
                child.kill()
                raise AssertionError("continuous stream did not flush within 2 seconds")
        self.assertEqual(first, fixture.line(b"continuous-000000"))
        self.assertIsNone(child.poll())
        child.kill()
        output, errors = child.communicate(timeout=2)
        self.assertEqual(errors, b"")
        self.assertNotEqual(child.returncode, 0)

    def test_burst_exceeds_queue_budget_and_traces_completion_after_writes(self) -> None:
        result = self.run_fake(LOGCAT, "burst", timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(result.stdout), 32 * 1024 * 1024)
        self.assertGreater(len(result.stdout), 16 * 1024 * 1024)
        lines = result.stdout.splitlines(keepends=True)
        self.assertEqual(len(lines), 128)
        for number, data in enumerate(lines):
            self.assertEqual(len(data), 256 * 1024)
            self.assertTrue(data.startswith(f"--------- burst-{number:04d} ".encode()))
            self.assertTrue(data.endswith(b"\n"))
        invocation, completed = self.records()
        self.assertEqual(completed["event"], "burst_complete")
        self.assertEqual(completed["pid"], invocation["pid"])
        self.assertEqual(completed["lines"], 128)
        self.assertEqual(completed["bytes"], 32 * 1024 * 1024)

    def test_watchdog_bounds_hung_query_and_blocked_stdout_independently(self) -> None:
        started = time.monotonic()
        hung = self.spawn(DEVICES, "query_hang")
        blocked = self.spawn(LOGCAT, "burst")
        self.wait_for_trace(hung, "invocation")
        self.wait_for_trace(blocked, "invocation")
        time.sleep(0.1)
        self.assertIsNone(hung.poll())
        self.assertIsNone(blocked.poll())
        self.assertFalse(any(record["event"] == "burst_complete" for record in self.records()))
        # Never read the burst stdout here: it must fill its pipe and block.
        # A second independent process demonstrates the query watchdog too.
        deadline = started + 10
        for child in (hung, blocked):
            self.assertEqual(child.wait(timeout=max(0.1, deadline - time.monotonic())), 124)
        self.assertLess(time.monotonic() - started, 10)
        output, errors = hung.communicate(timeout=1)
        self.assertEqual((output, errors), (b"", b""))
        self.assertFalse(any(record["event"] == "burst_complete" for record in self.records()))

    def test_concurrent_trace_records_are_complete_json_objects(self) -> None:
        children = [self.spawn(DEVICES, "normal") for _ in range(12)]
        for child in children:
            output, errors = child.communicate(timeout=3)
            self.assertEqual(child.returncode, 0, errors)
            self.assertIn(b"TEST-USB\tdevice", output)
        records = self.records()
        self.assertEqual(len(records), len(children))
        self.assertEqual({record["pid"] for record in records}, {child.pid for child in children})
        self.assertTrue(all(record["args"] == DEVICES for record in records))


if __name__ == "__main__":
    unittest.main()
