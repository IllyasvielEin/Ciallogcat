#!/usr/bin/env python3
"""Deterministic orchestration tests; no CLI build, ADB or device required."""
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import agent_capture as capture


def status(seq=0, state="running", **changes):
    value = {"schema_version": 1, "record_schema_version": 2,
             "artifact_id": "artifact", "session_id": "session",
             "state": state, "readiness": "adb_process_started", "stale": False,
             "visible_end": {"seq": seq, "offset": 100 + seq * 100},
             "updated_unix_ms": 1000, "reason": "stopped" if state == "completed" else None,
             "capture_completeness": "unknown", "stream_interruptions": 0,
             "diagnostics_complete": True, "omitted_by_limit": False}
    value.update(changes)
    return value


def page(sequences=(), cursor="baseline", more=False, upper=None, state="running"):
    upper = max(sequences, default=0) if upper is None else upper
    return {"schema_version": 2, "mode": "tail", "source_status": status(upper, state),
            "snapshot": status(upper)["visible_end"], "next_cursor": cursor,
            "has_more": more, "artifact_finalized": state == "completed",
            "validation": {"scope": "tail_suffix", "start": status()["visible_end"],
                           "skipped_records": 0, "footer_seen": state == "completed"},
            "records": [{"seq": seq, "record_ref": f"ref-{seq}",
                         "message_preview": f"failure-{seq}",
                         "identity": {"session_id": "session", "seq": seq}}
                        for seq in sequences],
            "scan": {"records": len(sequences), "bytes": len(sequences) * 100,
                     "next_offset": status(upper)["visible_end"]["offset"], "last_seq": upper,
                     "file_bytes_read": 200 + len(sequences) * 100}}


class Clock:
    def __init__(self):
        self.now = 10.0

    def monotonic(self):
        return self.now

    def sleep(self, duration):
        if duration < 0:
            raise AssertionError("negative sleep")
        self.now += duration

    def time_ns(self):
        # Intentionally moves backwards: wall time must not drive deadlines.
        return int((1000 - self.now) * 1_000_000_000)


class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.session = Path(self.temp.name) / "session"
        self.args = capture.parser().parse_args([
            "--serial", "MOCK", "--session-dir", str(self.session),
            "--query", "failure", "--", "executor", "literal ; $(unchanged)"])
        self.clock = Clock()
        self.start = status()
        self.statuses = [status(2)]
        self.pages = [page(), page((1, 2), "final", state="completed")]
        self.final = status(2, "completed")
        self.action_code = 0
        self.timed_out = False
        self.start_error = None
        self.stop_error = None
        self.calls = []
        self.action_at = None
        self.stop_at = None

    def cli(self, binary, *argv, timeout):
        self.assertGreater(timeout, 0)
        verb = argv[0]
        self.calls.append((verb, argv, timeout))
        if verb == "start":
            self.session.mkdir()
            if self.start_error:
                raise self.start_error
            return copy.deepcopy(self.start)
        if verb == "status":
            return copy.deepcopy(self.statuses.pop(0) if len(self.statuses) > 1 else self.statuses[0])
        if verb == "query-v2":
            self.assertEqual(argv[argv.index("--mode") + 1], "tail")
            self.assertEqual(argv[argv.index("--query") + 1], "failure")
            result = self.pages.pop(0)
            if isinstance(result, BaseException):
                raise result
            return copy.deepcopy(result)
        if verb == "stop":
            self.stop_at = self.clock.now
            if self.stop_error:
                raise self.stop_error
            return copy.deepcopy(self.final)
        self.fail(f"unexpected CLI verb {verb}")

    def action(self, argv, timeout):
        self.assertEqual(argv, ["executor", "literal ; $(unchanged)"])
        self.action_at = self.clock.now
        started = capture.unix_ms()
        self.clock.sleep(timeout if self.timed_out else 1)
        return {"started_unix_ms": started, "ended_unix_ms": capture.unix_ms(),
                "elapsed_secs": timeout if self.timed_out else 1,
                "exit_code": None if self.timed_out else self.action_code,
                "timed_out": self.timed_out, "interrupted": False, "reaped": True}

    def run_workflow(self):
        output = io.StringIO()
        with (patch.object(capture, "command", self.cli),
              patch.object(capture, "execute_action", self.action),
              patch.object(capture.time, "monotonic", self.clock.monotonic),
              patch.object(capture.time, "sleep", self.clock.sleep),
              patch.object(capture.time, "time_ns", self.clock.time_ns),
              contextlib.redirect_stdout(output)):
            code = capture.run(self.args)
        self.assertEqual(len(output.getvalue().splitlines()), 1)
        return code, json.loads(output.getvalue())

    def test_success_header_only_baseline_and_published_boundaries(self):
        code, report = self.run_workflow()
        self.assertEqual(code, 0)
        self.assertEqual([c[0] for c in self.calls], ["start", "query-v2", "status", "stop", "query-v2"])
        self.assertEqual(report["boundaries"]["before_action"]["visible_end"], {"seq": 0, "offset": 100})
        self.assertEqual(report["boundaries"]["after_action"]["visible_end"]["seq"], 2)
        self.assertEqual(report["action"]["elapsed_secs"], 1)
        self.assertGreater(report["action"]["started_unix_ms"], report["action"]["ended_unix_ms"])
        self.assertEqual(report["failure_tail"]["waited_secs"], 0)
        self.assertIn("--max-entries", self.calls[0][1])
        self.assertIn("baseline", self.calls[-1][1])
        self.assertEqual(self.calls[1][1][-2:], ("--start-at", "end"))
        self.assertNotIn("--start-at", self.calls[-1][1])
        self.assertEqual(report["boundaries"]["before_action"]["scan"]["records"], 0)

    def test_failure_collects_delayed_tail_then_stops(self):
        self.action_code = 7
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertEqual(report["action"]["exit_code"], 7)
        self.assertAlmostEqual(report["failure_tail"]["waited_secs"], 2)
        self.assertAlmostEqual(self.stop_at - self.action_at, 3)
        self.assertEqual([r["seq"] for r in report["evidence"]["records"]], [1, 2])

    def test_timeout_keeps_outcome_and_waits_bounded_tail(self):
        self.timed_out = True
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertTrue(report["action"]["timed_out"])
        self.assertTrue(report["action"]["reaped"])
        self.assertAlmostEqual(self.stop_at - self.action_at, self.args.action_timeout + 2)

    def test_silent_capture_is_valid_but_does_not_skip_failure_tail(self):
        self.action_code = 9
        self.statuses = [status()]
        self.final = status(0, "completed")
        self.pages = [page(), page(cursor="empty-final", state="completed")]
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertEqual(report["evidence"]["records"], [])
        self.assertEqual(report["failure_tail"]["waited_secs"], 2)
        self.assertTrue(report["evidence"]["complete_to_observed_watermark"])

    def test_reconnecting_start_and_header_publication_wait_before_action(self):
        self.start = status(state="reconnecting")
        self.statuses = [status(), status(), status(2)]
        self.pages.insert(0, capture.WorkflowError("source_not_ready", "header not published"))
        code, report = self.run_workflow()
        self.assertEqual(code, 0)
        self.assertGreater(self.action_at, 10)
        self.assertEqual(report["boundaries"]["before_action"]["source_status"]["state"], "running")

    def test_terminal_or_stale_start_never_runs_executor(self):
        for state, stale in (("completed", False), ("failed", False), ("unknown", True)):
            with self.subTest(state=state):
                if self.session.exists():
                    self.session.rmdir()
                self.start = status(state=state, stale=stale)
                self.pages = [page(state="completed")]
                code, report = self.run_workflow()
                self.assertEqual(code, 1)
                self.assertIsNone(self.action_at)
                self.assertEqual(report["errors"][0]["code"], "capture_not_running")
                self.assertIsNotNone(self.stop_at)

    def test_reconnecting_deadline_is_bounded(self):
        self.start = status(state="reconnecting")
        self.statuses = [self.start]
        self.pages = [page(state="completed")]
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertIsNone(self.action_at)
        self.assertAlmostEqual(self.clock.now, 10 + self.args.ready_timeout)
        self.assertEqual(report["errors"][0]["code"], "deadline")

    def test_invalid_end_page_blocks_action(self):
        self.pages = [page(cursor="b1", more=True), page()]
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertIsNone(self.action_at)
        self.assertEqual(report["errors"][0]["code"], "invalid_response")
        self.assertEqual(report["evidence"]["scope"], "session_prefix")

    def test_large_history_baseline_preserves_the_entire_evidence_page_budget(self):
        self.args.max_pages = 1
        self.pages = [page(upper=1000000), page((1000001,), "final", state="completed")]
        self.pages[0]["validation"]["skipped_records"] = 1000000
        code, report = self.run_workflow()
        self.assertEqual(code, 0)
        self.assertEqual(report["boundaries"]["before_action"]["pages_read"], 1)
        self.assertEqual(report["boundaries"]["before_action"]["visible_end"]["seq"], 1000000)
        self.assertEqual(report["boundaries"]["before_action"]["validation"]["skipped_records"], 1000000)
        self.assertEqual(report["evidence"]["pages_read"], 1)
        self.assertEqual(report["evidence"]["records"][0]["seq"], 1000001)

    def test_reconnecting_end_observation_retries_a_fresh_end_without_scanning(self):
        self.pages.insert(0, page(upper=10000, state="reconnecting"))
        code, report = self.run_workflow()
        self.assertEqual(code, 0)
        initial = [call[1] for call in self.calls if call[0] == "query-v2"][:2]
        for argv in initial:
            self.assertEqual(argv[-2:], ("--start-at", "end"))
            self.assertNotIn("--cursor", argv)
        self.assertEqual(report["boundaries"]["before_action"]["visible_end"]["seq"], 0)

    def test_end_baseline_preserves_partial_footer_validation_in_evidence(self):
        self.pages = [page(upper=100), page((101,), "final", state="completed")]
        self.pages[1]["artifact_finalized"] = False
        self.pages[1]["validation"]["skipped_records"] = 100
        code, report = self.run_workflow()
        self.assertEqual(code, 0)
        self.assertFalse(report["evidence"]["artifact_finalized"])
        self.assertTrue(report["evidence"]["validation"]["footer_seen"])

    def test_already_published_footer_blocks_action_even_with_running_status(self):
        self.pages[0]["validation"]["footer_seen"] = True
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertIsNone(self.action_at)
        self.assertEqual(report["errors"][0]["code"], "capture_not_running")

    def test_missing_validation_is_a_reported_query_error(self):
        del self.pages[0]["validation"]
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertIsNone(self.action_at)
        self.assertEqual(report["errors"][0]["code"], "invalid_response")

    def test_nonmatching_page_is_not_end_and_recent_records_survive_empty_footer(self):
        self.args.evidence_records = 2
        self.pages = [page(), page(cursor="q1", more=True, upper=4),
                      page((1, 2), "q2", more=True, upper=4),
                      page((3, 4), "q3", more=True, upper=4),
                      page(cursor="end", upper=4, state="completed")]
        code, report = self.run_workflow()
        self.assertEqual(code, 0)
        evidence = report["evidence"]
        self.assertEqual([r["seq"] for r in evidence["records"]], [3, 4])
        self.assertEqual(evidence["evicted_records"], 2)
        self.assertEqual(evidence["next_cursor"], "end")
        self.assertEqual(evidence["pages_read"], 4)

    def test_evidence_page_budget_exposes_unfinished_scan(self):
        self.args.max_pages = 1
        self.pages = [page(), page((1,), "resume", more=True, upper=10)]
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertFalse(report["evidence"]["complete_to_observed_watermark"])
        self.assertEqual(report["evidence"]["stop_reason"], "page_limit")
        self.assertEqual(report["evidence"]["next_cursor"], "resume")

    def test_evidence_deadline_preserves_collected_records(self):
        original = self.cli
        def slow_query(binary, *argv, timeout):
            value = original(binary, *argv, timeout=timeout)
            if argv[0] == "query-v2" and self.action_at is not None:
                self.clock.sleep(self.args.evidence_timeout)
            return value
        self.cli = slow_query
        self.pages = [page(), page((1,), "resume", more=True, upper=10)]
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertEqual(report["evidence"]["stop_reason"], "deadline")
        self.assertEqual(report["evidence"]["records"][0]["record_ref"], "ref-1")

    def test_retained_records_have_a_total_utf8_byte_budget(self):
        self.args.evidence_bytes = 8192
        first = page((1,), "q1", more=True, upper=2)
        second = page((2,), "q2", upper=2, state="completed")
        for value in (first, second):
            value["records"][0]["message_preview"] = "雪" * 1600
        self.pages = [page(), first, second]
        code, report = self.run_workflow()
        self.assertEqual(code, 0)
        self.assertEqual([r["seq"] for r in report["evidence"]["records"]], [2])
        self.assertLessEqual(len(json.dumps(report["evidence"]["records"], ensure_ascii=False).encode()), 8192)

    def test_capture_limit_is_not_reported_as_a_successful_workflow(self):
        self.final = status(2, "completed", reason="max_entries", omitted_by_limit=True)
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertEqual(report["action"]["exit_code"], 0)
        self.assertEqual(report["capture"]["reason"], "max_entries")

    def test_stop_error_still_collects_published_evidence(self):
        self.stop_error = capture.WorkflowError("stale_session", "no PID signalled")
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertEqual(report["action"]["exit_code"], 0)
        self.assertEqual(report["errors"][0]["stage"], "stop")
        self.assertEqual(len(report["evidence"]["records"]), 2)

    def test_stop_interruption_retries_once_and_preserves_action(self):
        original = self.cli
        interrupted = False
        def interrupt_once(binary, *argv, timeout):
            nonlocal interrupted
            if argv[0] == "stop" and not interrupted:
                interrupted = True
                self.calls.append(("stop", argv, timeout))
                raise KeyboardInterrupt()
            return original(binary, *argv, timeout=timeout)
        self.cli = interrupt_once
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertEqual(sum(c[0] == "stop" for c in self.calls), 2)
        self.assertEqual(report["action"]["exit_code"], 0)
        self.assertEqual(report["capture"]["state"], "completed")
        self.assertEqual(report["errors"][0]["code"], "KeyboardInterrupt")

    def test_repeated_stop_interrupts_have_a_finite_retry_limit(self):
        self.stop_error = KeyboardInterrupt()
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertEqual(sum(c[0] == "stop" for c in self.calls), 2)
        self.assertEqual(report["action"]["exit_code"], 0)
        self.assertEqual([e["stage"] for e in report["errors"]], ["stop", "stop_retry"])

    def test_evidence_interruption_keeps_previous_page_and_cursor(self):
        self.pages = [page(), page((1,), "resume", more=True, upper=2), KeyboardInterrupt()]
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertEqual(report["action"]["exit_code"], 0)
        self.assertEqual(report["evidence"]["records"][0]["seq"], 1)
        self.assertEqual(report["evidence"]["next_cursor"], "resume")
        self.assertEqual(report["evidence"]["stop_reason"], "interrupted")

    def test_failed_start_still_stops_created_session_and_queries(self):
        self.start_error = subprocess.TimeoutExpired("start", 15)
        self.pages = [page(state="completed")]
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertIsNone(report["action"])
        self.assertEqual([c[0] for c in self.calls], ["start", "stop", "query-v2"])

    def test_query_error_preserves_action_and_capture(self):
        self.pages[1] = capture.WorkflowError("invalid_source", "broken source")
        code, report = self.run_workflow()
        self.assertEqual(code, 1)
        self.assertEqual(report["action"]["exit_code"], 0)
        self.assertEqual(report["capture"]["state"], "completed")
        self.assertEqual(report["errors"][0]["code"], "invalid_source")

    def test_existing_directory_is_never_stopped(self):
        self.session.mkdir()
        with self.assertRaises(ValueError), patch.object(capture, "command") as cli:
            capture.run(self.args)
        cli.assert_not_called()

    def test_invalid_time_and_byte_bounds_fail_before_spawning(self):
        for name, value in (("action_timeout", float("nan")), ("max_bytes", 8191),
                            ("duration_secs", 100), ("max_entries", 0)):
            with self.subTest(name=name):
                original = getattr(self.args, name)
                setattr(self.args, name, value)
                with self.assertRaises(ValueError), patch.object(capture, "command") as cli:
                    capture.run(self.args)
                cli.assert_not_called()
                setattr(self.args, name, original)


class OwnedProcessTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "posix", "private POSIX process groups")
    def test_exited_leader_still_has_its_owned_group_stopped(self):
        from unittest.mock import Mock
        child = Mock(pid=12345, returncode=0)
        child.poll.return_value = 0
        with patch.object(capture.os, "killpg") as killpg:
            capture.cleanup_executor(child)
        self.assertEqual([call.args for call in killpg.call_args_list],
                         [(12345, signal.SIGTERM), (12345, signal.SIGKILL)])
        child.wait.assert_any_call(timeout=2)

    def test_normal_and_nonzero_exit_are_reaped(self):
        for code in (0, 7):
            with self.subTest(code=code), patch.object(capture.subprocess, "Popen", wraps=subprocess.Popen) as spawn:
                result = capture.execute_action([sys.executable, "-c", f"raise SystemExit({code})"], 5)
                self.assertEqual(result["exit_code"], code)
                self.assertEqual(result["process_returncode"], code)
                self.assertTrue(result["reaped"])
                self.assertFalse(spawn.call_args.kwargs.get("shell", False))

    def test_timed_out_owned_child_is_reaped(self):
        result = capture.execute_action([sys.executable, "-c", "import time; time.sleep(30)"], 0.1)
        self.assertTrue(result["timed_out"])
        self.assertTrue(result["reaped"])
        self.assertIsNotNone(result["process_returncode"])

    @unittest.skipUnless(os.name == "posix", "private POSIX process groups")
    def test_timeout_terminates_group_and_executor_reaps_its_child(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "child-reaped"
            script = ("import signal, subprocess, sys, time\n"
                      "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\n"
                      "def stop(sig, frame):\n"
                      " child.wait(timeout=2)\n"
                      " open(sys.argv[1], 'w').write('reaped')\n"
                      " raise SystemExit(0)\n"
                      "signal.signal(signal.SIGTERM, stop)\n"
                      "print('executor ready', file=sys.stderr, flush=True)\n"
                      "time.sleep(30)\n")
            result = capture.execute_action([sys.executable, "-c", script, str(marker)], 1)
            self.assertTrue(result["timed_out"])
            self.assertTrue(result["reaped"])
            self.assertEqual(marker.read_text(), "reaped")

    def test_interruption_also_reaps_direct_child(self):
        real_popen = subprocess.Popen
        children = []
        def spawn(*args, **kwargs):
            child = real_popen(*args, **kwargs)
            wait = child.wait
            count = 0
            def interrupted_wait(*args, **kwargs):
                nonlocal count
                count += 1
                if count == 1:
                    raise KeyboardInterrupt()
                return wait(*args, **kwargs)
            child.wait = interrupted_wait
            children.append(child)
            return child
        with patch.object(capture.subprocess, "Popen", spawn):
            result = capture.execute_action([sys.executable, "-c", "import time; time.sleep(30)"], 5)
        self.assertTrue(result["interrupted"])
        self.assertTrue(result["reaped"])
        self.assertIsNotNone(children[0].poll())

    def test_control_timeout_reaps_direct_child(self):
        children = []
        real_popen = subprocess.Popen
        def spawn(*args, **kwargs):
            child = real_popen(*args, **kwargs)
            children.append(child)
            return child
        with patch.object(capture.subprocess, "Popen", spawn), self.assertRaises(subprocess.TimeoutExpired):
            capture.command(sys.executable, "-c", "import time; time.sleep(30)", timeout=0.1)
        self.assertIsNotNone(children[0].poll())

    def test_invalid_json_and_structured_cli_errors(self):
        with self.assertRaises(capture.WorkflowError) as error:
            capture.command(sys.executable, "-c", "print('not json')")
        self.assertEqual(error.exception.code, "invalid_response")
        with self.assertRaises(capture.WorkflowError) as error:
            capture.command(sys.executable, "-c",
                            "import sys; print('{\"error\":{\"code\":\"source_not_ready\",\"message\":\"wait\"}}', file=sys.stderr); sys.exit(3)")
        self.assertEqual(error.exception.code, "source_not_ready")


if __name__ == "__main__":
    unittest.main()
