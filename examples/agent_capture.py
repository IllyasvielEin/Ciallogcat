#!/usr/bin/env python3
"""Run one external test command with bounded native log evidence (Python 3.10+).

Executor output goes to stderr; stdout is one JSON result. Device operations and
cleanup belong to the supplied executor. See docs/agent-test-workflow.md.
"""
import argparse
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


POLL_SECONDS = 0.1
TERMINAL = {"completed", "failed"}


class WorkflowError(RuntimeError):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def unix_ms():
    return time.time_ns() // 1_000_000


def remaining(deadline, cap):
    value = min(cap, deadline - time.monotonic())
    if value <= 0:
        raise WorkflowError("deadline", "phase deadline expired")
    return value


def command(cli, *args, timeout=15):
    # subprocess.run kills and reaps this direct child on timeout/error.
    result = subprocess.run([cli, *args], capture_output=True, text=True,
                            encoding="utf-8", errors="replace", timeout=timeout)
    if result.returncode:
        try:
            error = json.loads(result.stderr)["error"]
            if not isinstance(error, dict):
                raise ValueError("expected an error object")
        except (ValueError, KeyError, TypeError):
            error = {"code": "cli_error", "message": result.stderr[:1024]}
        raise WorkflowError(error.get("code", "cli_error"),
                            error.get("message") or f"CLI exited {result.returncode}")
    try:
        value = json.loads(result.stdout)
        if not isinstance(value, dict):
            raise ValueError("expected an object")
        return value
    except ValueError as error:
        raise WorkflowError("invalid_response", f"CLI returned invalid JSON: {error}") from error


def cleanup_executor(child):
    """Reap our direct child; POSIX also stops its private process group."""
    def send(sig):
        try:
            if os.name == "posix":
                os.killpg(child.pid, sig)
            elif child.poll() is None:
                child.terminate() if sig == signal.SIGTERM else child.kill()
        except ProcessLookupError:
            pass

    send(signal.SIGTERM)
    try:
        child.wait(timeout=1)
    except subprocess.TimeoutExpired:
        pass
    finally:
        # The leader may have exited while a descendant still holds resources.
        send(signal.SIGKILL if os.name == "posix" else signal.SIGTERM)
        if child.poll() is None:
            child.kill()
        child.wait(timeout=2)


def execute_action(executor, timeout):
    started = time.monotonic()
    action = {"started_unix_ms": unix_ms(), "timeout_secs": timeout,
              "exit_code": None, "timed_out": False, "interrupted": False}
    child = None
    try:
        child = subprocess.Popen(executor, stdin=subprocess.DEVNULL,
                                 stdout=sys.stderr, stderr=sys.stderr,
                                 start_new_session=(os.name == "posix"))
        try:
            child.wait(timeout=remaining(started + timeout, timeout))
        except (subprocess.TimeoutExpired, WorkflowError):
            action["timed_out"] = True
        except KeyboardInterrupt:
            action["interrupted"] = True
        finally:
            # Timestamp the observed action end/deadline before cleanup latency.
            action["ended_unix_ms"] = unix_ms()
            action["elapsed_secs"] = time.monotonic() - started
            action["exit_code"] = child.poll()
    finally:
        if child is not None:
            cleanup_executor(child)
            action["process_returncode"] = child.returncode
            action["reaped"] = True
            action["cleanup_ended_unix_ms"] = unix_ms()
    return action


def observation(status):
    return {"observed_unix_ms": unix_ms(), **{
        key: status.get(key) for key in (
            "state", "stale", "readiness", "updated_unix_ms", "record_schema_version",
            "artifact_id", "session_id", "visible_end", "reason",
            "capture_completeness", "stream_interruptions")}}


def require_running(status):
    if status.get("stale") or status.get("state") in TERMINAL | {"unknown", "stopping"}:
        raise WorkflowError("capture_not_running", "capture is stale, stopping or terminal before action")
    return (status.get("state") == "running"
            and status.get("readiness") == "adb_process_started")


def query_page(args, session, cursor, deadline, start_at=None):
    flags = ["query", "--session-dir", str(session), "--mode", "tail",
             "--query", args.query, "--limit", str(args.evidence_records),
             "--max-response-bytes", str(args.evidence_bytes), "--preview-bytes", "512",
             "--max-scan-records", str(args.scan_records),
             "--max-scan-bytes", str(args.scan_bytes)]
    if cursor:
        flags += ["--cursor", cursor]
    if start_at:
        flags += ["--start-at", start_at]
    page = command(args.cli, *flags, timeout=remaining(deadline, args.command_timeout))
    if (page.get("schema_version") != 2 or page.get("mode") != "tail"
            or not isinstance(page.get("records"), list) or not page.get("next_cursor")
            or any(not isinstance(page.get(key), dict)
                   for key in ("source_status", "snapshot", "scan", "validation"))
            or not isinstance(page.get("has_more"), bool)):
        raise WorkflowError("invalid_response", "expected a tail query page")
    return page


def establish_baseline(args, session, status, deadline):
    pages = 0
    while True:
        remaining(deadline, args.command_timeout)
        if require_running(status):
            if (status.get("record_schema_version") != 3 or not status.get("session_id")
                    or not status.get("artifact_id")):
                raise WorkflowError("invalid_response", "expected native capture identities")
            try:
                page = query_page(args, session, None, deadline, start_at="end")
            except WorkflowError as error:
                if error.code != "source_not_ready":
                    raise
            else:
                pages += 1
                ready = require_running(page["source_status"])
                if page["validation"].get("footer_seen"):
                    raise WorkflowError("capture_not_running", "capture footer is already published before action")
                if (page["records"] or page["has_more"] or page["scan"]["records"] != 0
                        or page["scan"]["bytes"] != 0
                        or page["scan"].get("next_offset") != page["snapshot"].get("offset")
                        or page["scan"].get("last_seq") != page["snapshot"].get("seq")):
                    raise WorkflowError("invalid_response", "expected an empty page at the published end")
                if ready:
                    if not all(page["source_status"].get(key) for key in ("session_id", "artifact_id")):
                        raise WorkflowError("invalid_response", "end query lacks capture identities")
                    return {"observed_unix_ms": unix_ms(),
                            "session_id": page["source_status"]["session_id"],
                            "artifact_id": page["source_status"]["artifact_id"],
                            "visible_end": page["snapshot"], "next_cursor": page["next_cursor"],
                            "source_status": page["source_status"], "pages_read": pages,
                            "start_at": "end", "scan": page["scan"],
                            "validation": page["validation"]}
        time.sleep(min(POLL_SECONDS, remaining(deadline, POLL_SECONDS)))
        status = command(args.cli, "status", "--session-dir", str(session),
                         timeout=remaining(deadline, args.command_timeout))


def collect_evidence(args, session, baseline, evidence):
    deadline = time.monotonic() + args.evidence_timeout
    cursor = baseline["next_cursor"] if baseline else None
    evidence.update({"scope": "after_baseline" if baseline else "session_prefix",
                     "records": [], "pages_read": 0, "scanned_records": 0,
                     "scanned_bytes": 0, "file_bytes_read": 0, "evicted_records": 0,
                     "complete_to_observed_watermark": False, "stop_reason": "page_limit",
                     "next_cursor": cursor})
    for _ in range(args.max_pages):
        if time.monotonic() >= deadline:
            evidence["stop_reason"] = "deadline"
            break
        page = query_page(args, session, cursor, deadline)
        cursor = page["next_cursor"]
        evidence.update({"next_cursor": cursor, "snapshot": page["snapshot"],
                         "source_status": page["source_status"],
                         "artifact_finalized": page["artifact_finalized"],
                         "validation": page["validation"],
                         "has_more": page["has_more"]})
        evidence["pages_read"] += 1
        for target, source in (("scanned_records", "records"), ("scanned_bytes", "bytes"),
                               ("file_bytes_read", "file_bytes_read")):
            evidence[target] += page["scan"][source]
        evidence["records"].extend(page["records"])
        # Keep a recent, bounded selection, even when the final page is empty.
        while (len(evidence["records"]) > args.evidence_records
               or len(json.dumps(evidence["records"], ensure_ascii=False).encode("utf-8"))
               > args.evidence_bytes):
            evidence["records"].pop(0)
            evidence["evicted_records"] += 1
        if not page["has_more"]:
            evidence["complete_to_observed_watermark"] = True
            evidence["stop_reason"] = "observed_watermark"
            break


def run(args):
    session = Path(args.session_dir).absolute()
    if session.exists():
        raise ValueError("session directory must not exist")
    executor = args.executor[1:] if args.executor[:1] == ["--"] else args.executor
    if not executor:
        raise ValueError("provide a test executor after --")
    ranges = {"duration_secs": (1, 86400), "max_entries": (1, 10000000),
              "max_bytes": (8192, 1073741824), "action_timeout": (0.01, 86400),
              "ready_timeout": (1, 60), "command_timeout": (1, 60),
              "failure_tail_secs": (0, 30), "evidence_timeout": (1, 60),
              "max_pages": (1, 100), "evidence_records": (1, 1000),
              "evidence_bytes": (8192, 1048576), "scan_records": (1, 1000000),
              "scan_bytes": (8388609, 67108864)}
    for name, (low, high) in ranges.items():
        value = getattr(args, name)
        if not math.isfinite(value) or not low <= value <= high:
            raise ValueError(f"{name.replace('_', '-')} must be in {low}..{high}")
    # Include an after-action status command and executor cleanup allowance.
    if (args.ready_timeout + args.action_timeout + args.failure_tail_secs
            + args.command_timeout + 3 >= args.duration_secs):
        raise ValueError("duration-secs must exceed ready-timeout + action-timeout + "
                         "failure-tail-secs + command-timeout + 3 seconds cleanup reserve")
    if len(args.query.encode("utf-8")) > 4096:
        raise ValueError("query must not exceed 4096 UTF-8 bytes")

    report = {"schema_version": 2, "session_dir": str(session), "action": None,
              "readiness": None, "boundaries": {"before_action": None, "after_action": None,
              "after_stop": None}, "failure_tail": {"requested_secs": args.failure_tail_secs,
              "waited_secs": 0, "reason": "not_needed"}, "capture": None, "evidence": {},
              "limits": {name: getattr(args, name) for name in ranges}, "errors": []}
    def problem(stage, error):
        report["errors"].append({"stage": stage, "code": getattr(error, "code", type(error).__name__),
                                 "message": str(error)[:2048]})

    stage = "start"
    baseline = None
    start_attempted = False
    try:
        flags = ["start", "--serial", args.serial, "--session-dir", str(session),
                 "--duration-secs", str(args.duration_secs),
                 "--max-bytes", str(args.max_bytes), "--max-entries", str(args.max_entries)]
        if args.adb:
            flags += ["--adb", args.adb]
        ready_deadline = time.monotonic() + args.ready_timeout
        start_attempted = True
        status = command(args.cli, *flags, timeout=remaining(ready_deadline, args.command_timeout))
        report["readiness"] = observation(status)
        stage = "baseline"
        baseline = establish_baseline(args, session, status, ready_deadline)
        report["boundaries"]["before_action"] = baseline
        stage = "executor"
        action = execute_action(executor, args.action_timeout)
        report["action"] = action
        failed = action["exit_code"] != 0 or action["timed_out"]
        tail_started = time.monotonic()
        # The grace period starts after executor cleanup and overlaps status IO.
        tail_deadline = tail_started + (args.failure_tail_secs if failed else 0)
        stage = "after_action_status"
        try:
            status = command(args.cli, "status", "--session-dir", str(session),
                             timeout=args.command_timeout)
            report["boundaries"]["after_action"] = observation(status)
        except (WorkflowError, OSError, subprocess.TimeoutExpired) as error:
            status = {}
            problem(stage, error)
        if failed and not action["interrupted"]:
            report["failure_tail"]["reason"] = "capture_terminal" if status.get("state") in TERMINAL else "deadline"
            if status.get("state") not in TERMINAL:
                # Do not mistake an empty/no-match query for completion.
                while time.monotonic() < tail_deadline:
                    time.sleep(max(0, min(POLL_SECONDS, tail_deadline - time.monotonic())))
            report["failure_tail"]["waited_secs"] = time.monotonic() - tail_started
        elif action["interrupted"]:
            report["failure_tail"]["reason"] = "interrupted"
    except (WorkflowError, OSError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
        problem(stage, error)
    finally:
        if start_attempted and session.is_dir():
            # One interrupt may kill the stop client before it writes the marker.
            # Retry this idempotent operation once, with the same finite bound.
            for attempt in range(2):
                try:
                    # Never signal worker_pid. The CLI worker owns and reaps ADB.
                    report["capture"] = command(args.cli, "stop", "--session-dir", str(session),
                                                "--wait-secs", "10", timeout=15)
                except KeyboardInterrupt as error:
                    problem("stop" if attempt == 0 else "stop_retry", error)
                    continue
                except (WorkflowError, OSError, subprocess.TimeoutExpired) as error:
                    problem("stop", error)
                break
            if report["capture"] is None:
                try:
                    report["capture"] = command(args.cli, "status", "--session-dir", str(session),
                                                timeout=args.command_timeout)
                except (WorkflowError, OSError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
                    problem("recovery_status", error)
            if report["capture"]:
                report["boundaries"]["after_stop"] = observation(report["capture"])
            try:
                collect_evidence(args, session, baseline, report["evidence"])
            except (WorkflowError, OSError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
                report["evidence"]["stop_reason"] = "interrupted" if isinstance(error, KeyboardInterrupt) else "query_error"
                problem("evidence", error)

    action, capture = report["action"], report["capture"] or {}
    complete_scan = report["evidence"].get("complete_to_observed_watermark", False)
    report["ok"] = bool(not report["errors"] and action and action["exit_code"] == 0
                        and not action["timed_out"] and not action["interrupted"]
                        and capture.get("state") == "completed" and capture.get("reason") == "stopped"
                        and capture.get("diagnostics_complete")
                        and not capture.get("omitted_by_limit") and complete_scan)
    print(json.dumps(report, ensure_ascii=False))
    return 0 if report["ok"] else 1


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--cli", default="ciallogcat-cli")
    result.add_argument("--adb")
    result.add_argument("--serial", required=True)
    result.add_argument("--session-dir", required=True)
    result.add_argument("--duration-secs", type=int, default=150)
    result.add_argument("--max-entries", type=int, default=100000)
    result.add_argument("--max-bytes", type=int, default=67108864)
    result.add_argument("--action-timeout", type=float, default=90)
    result.add_argument("--ready-timeout", type=float, default=15)
    result.add_argument("--command-timeout", type=float, default=15)
    result.add_argument("--failure-tail-secs", type=float, default=2)
    result.add_argument("--evidence-timeout", type=float, default=10)
    result.add_argument("--max-pages", type=int, default=8)
    result.add_argument("--evidence-records", type=int, default=20)
    result.add_argument("--evidence-bytes", type=int, default=32768)
    result.add_argument("--scan-records", type=int, default=100000)
    result.add_argument("--scan-bytes", type=int, default=16777216)
    result.add_argument("--query", default="")
    result.add_argument("executor", nargs=argparse.REMAINDER)
    return result


def main():
    arguments = parser()
    try:
        return run(arguments.parse_args())
    except (ValueError, OSError) as error:
        arguments.error(str(error))


if __name__ == "__main__":
    sys.exit(main())
