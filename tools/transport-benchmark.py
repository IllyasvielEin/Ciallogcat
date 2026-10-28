#!/usr/bin/env python3
"""Synthetic Linux host capture comparison using pre-encoded replay.

Defaults to v2/text and v2/proto; --include-compact adds v3/proto. No real ADB,
device encoder, USB, GPU, or fsync/durable-storage throughput is measured.
"""
import argparse
import base64
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import statistics
import subprocess
import sys
import time


REPLAY = r'''
use std::{env,fs::File,io::{self,Write},thread,time::Duration};
fn main() {
    thread::spawn(|| { thread::sleep(Duration::from_secs(90)); std::process::exit(124); });
    let a: Vec<String> = env::args().skip(1).collect();
    if a == ["-s", "PERF-MOCK", "features"] { println!("shell_v2"); return; }
    let text = ["-s", "PERF-MOCK", "logcat", "-v", "threadtime", "-T", "1", "-b", "main", "-b", "system", "-b", "crash"];
    let proto = ["-s", "PERF-MOCK", "shell", "-T", "-n", "exec logcat --proto -v threadtime -T 1 -b main -b system -b crash '*:V'"];
    let suffix = if a == text { "text" } else if a == proto { "proto" } else { std::process::exit(2) };
    let input = env::var("CIALLOGCAT_PERF_INPUT").unwrap();
    let mut file = File::open(format!("{input}.{suffix}")).unwrap();
    let mut stdout = io::stdout().lock();
    if io::copy(&mut file, &mut stdout).is_err() { return; }
    if stdout.flush().is_err() { return; }
    // Let the CLI reach its exact record bound; EOF must not trigger retries.
    loop { thread::sleep(Duration::from_secs(1)); }
}
'''


def varint(value):
    out = bytearray()
    while value > 127:
        out.append((value & 127) | 128)
        value >>= 7
    out.append(value)
    return bytes(out)


def number(field, value):
    return varint(field << 3) + varint(value)


def blob(field, value):
    return varint((field << 3) | 2) + varint(len(value)) + value


def message(index, length):
    prefix = f"event={index:08d} request=checkout status=ok ".encode()
    return prefix + b"x" * (length - len(prefix))


def payload(index, size):
    fixed = b"".join(number(k, v) for k, v in
                     [(1, 1792760000), (2, 123000000), (3, 4),
                      (4, 10001), (5, 1234), (6, 1235)])
    fixed += blob(7, b"Perf\0")
    return fixed + blob(8, message(index, size)) + blob(9, b"com.example.bench")


def inputs(path, count, size):
    # UID, nanoseconds and process name are intentionally available only on the
    # proto path, as in the product. Message/tag/PID/TID/level are equivalent.
    with path.with_suffix(".text").open("wb") as text, path.with_suffix(".proto").open("wb") as proto:
        for index in range(count):
            body = message(index, size)
            text.write(b"10-23 20:53:20.123  1234  1235 I Perf: " + body + b"\n")
            encoded = payload(index, size)
            proto.write(len(encoded).to_bytes(8, "little") + encoded)


def require(condition, description):
    # Assertions would silently disable integrity checks under python -O.
    if not condition:
        raise ValueError(description)


def decode_bytes(value):
    require(isinstance(value, str), "missing/non-string base64 evidence")
    decoded = base64.b64decode(value, validate=True)
    require(base64.b64encode(decoded).decode("ascii") == value,
            "noncanonical base64 evidence")
    return decoded


def validate(path, count, size, transport, record_format, source):
    """Check this synthetic fixture, including exact on-disk replay bytes.

    This is deliberately not a general native-file/protobuf reader. For v3 the
    original payload is the evidence; absent display fields are required.
    """
    version = int(record_format[1:])
    verified = 0
    capture = None
    try:
        with ExitStack() as stack:
            stream = stack.enter_context(path.open(encoding="utf-8"))
            wire = stack.enter_context(source.with_suffix("." + transport).open("rb"))
            header = json.loads(next(stream))
            require(header["type"] == "document" and header["schema_version"] == version,
                    "header type/schema mismatch")
            require(bool(header["artifact_id"]) and bool(header["session_id"]),
                    "missing artifact/session identity")
            expected_transport = "threadtime" if transport == "text" else "logcat_proto_experimental"
            require(header["source"]["transport_format"] == expected_transport,
                    "header transport mismatch")
            require(header["source"]["device_serial"] == "PERF-MOCK", "source serial mismatch")
            if transport == "proto":
                require(header["normalization"]["protobuf_raw_payload"] == "preserved_base64",
                        "missing raw payload declaration")
            if version == 3:
                require(transport == "proto" and header["record_encoding"] == "proto_raw_v1"
                        and header["normalization"]["proto_projection"] == "aosp_logcat_16r1_host_v1",
                        "invalid compact encoding declarations")
            for index in range(count):
                row = json.loads(next(stream))
                require(row["type"] == "entry" and row["schema_version"] == version,
                        "record type/schema mismatch")
                require(type(row["seq"]) is int and row["seq"] == index + 1,
                        "missing/duplicate/out-of-order sequence")
                identity = row["provenance"]
                require(identity["seq"] == row["seq"]
                        and identity["session_id"] == header["session_id"]
                        and identity["segment"] == identity["attempt"] == 1,
                        "provenance mismatch or replay reconnect")
                body = message(index, size)
                if transport == "proto":
                    prefix = wire.read(8)
                    require(len(prefix) == 8, "truncated replay frame prefix")
                    length = int.from_bytes(prefix, "little")
                    require(0 < length <= 1048576, "invalid replay frame length")
                    original = wire.read(length)
                    require(len(original) == length and original == payload(index, size),
                            "replay payload differs from synthetic fixture")
                    evidence = row["proto"]
                    require(decode_bytes(evidence["raw_payload"]) == original,
                            "raw payload differs from replay frame")
                    if version == 2:
                        for key, value in {"time_sec": 1792760000, "time_nsec": 123000000,
                                           "priority": 4, "uid": 10001, "pid": 1234, "tid": 1235,
                                           "process_name": "com.example.bench"}.items():
                            require(evidence[key] == value, f"proto.{key} mismatch")
                        require(decode_bytes(evidence["tag_bytes"]) == b"Perf\0"
                                and decode_bytes(evidence["message_bytes"]) == body,
                                "decoded evidence mismatch")
                        require(row["device_time"]["text"] == "1792760000.123000000"
                                and row["process"]["name"] == "com.example.bench"
                                and row["process"]["source"] == "device_process_cache",
                                "proto display metadata mismatch")
                    else:
                        require(not set(evidence).intersection({"time_sec", "time_nsec", "priority",
                                "uid", "pid", "tid", "tag_bytes", "message_bytes", "process_name"}),
                                "redundant v3 proto fields")
                        require(not set(row).intersection({"device_time", "pid", "tid", "level",
                                                          "tag", "message", "process"}),
                                "redundant v3 display fields")
                else:
                    require(wire.readline() == b"10-23 20:53:20.123  1234  1235 I Perf: " + body + b"\n",
                            "text replay differs from synthetic fixture")
                    require(row.get("proto") is None, "unexpected proto evidence on text path")
                if version == 2:
                    require(row["message"].encode() == body and row["pid"] == 1234
                            and row["tid"] == 1235 and row["tag"] == "Perf" and row["level"] == "I",
                            "v2 display mismatch")
                loss = row.get("text_loss")
                require(loss is None or (loss["truncated"] is False
                        and loss["invalid_utf8_replaced"] is False), "unexpected text loss")
                verified += 1
            require(wire.read(1) == b"", "extra replay data")
            footer = json.loads(next(stream))
            capture = footer.get("capture")
            require(footer["type"] == "artifact_end" and footer["schema_version"] == version
                    and footer["artifact_id"] == header["artifact_id"], "footer identity/schema mismatch")
            require(footer["finalized"] is True and footer["record_count"] == count
                    and footer["first_seq"] == 1 and footer["last_seq"] == count,
                    "footer count/bounds/finalization mismatch")
            scope = footer["scope"]
            require(scope["kind"] == "retained" and scope["source_record_count"] == count
                    and scope["source_first_seq"] == 1 and scope["source_last_seq"] == count
                    and scope["omitted_before"] == 0 and scope.get("filter") is None,
                    "footer scope mismatch")
            require(capture["state"] == "stopped" and capture["stop_reason"] == "max_entries"
                    and capture["known_gaps"] == [] and capture["completeness"] == "unknown"
                    and capture["attempts"] == 1, "capture did not reach exact bound without known gaps")
            require(stream.read(1) == "", "data after footer")
        return {"success": True, "records_verified": verified, "source": header["source"],
                "artifact_id": header["artifact_id"], "session_id": header["session_id"],
                "capture": capture, "all_messages_and_sequences_verified": True,
                "raw_payloads_verified": count if transport == "proto" else 0}
    except (OSError, ValueError, KeyError, TypeError, AttributeError, StopIteration) as error:
        return {"success": False, "records_verified": verified, "capture": capture,
                "error": f"after {verified} verified records: {type(error).__name__}: {error}"}


def measure(command, env, stdout_path, stderr_path, deadline=75):
    """Only wait/sample our Popen child; only signal its private process group."""
    peak_rss = peak_hwm = samples = 0
    usage = None
    timed_out = False
    started = time.monotonic()
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(command, env=env, stdout=stdout, stderr=stderr,
                                   start_new_session=True)
        try:
            while True:
                # Leave the PID reserved until cleanup, so killpg cannot target
                # an unrelated process that reused an already-reaped PID.
                exited = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                if exited is not None:
                    break
                if time.monotonic() - started >= deadline:
                    timed_out = True
                    break
                try:
                    status = Path(f"/proc/{process.pid}/status").read_text()
                    values = {line.split()[0]: int(line.split()[1])
                              for line in status.splitlines() if line.startswith(("VmRSS:", "VmHWM:"))}
                    if "VmRSS:" in values:
                        peak_rss = max(peak_rss, values["VmRSS:"])
                        peak_hwm = max(peak_hwm, values.get("VmHWM:", 0))
                        samples += 1
                except FileNotFoundError:
                    pass
                time.sleep(.005)
        finally:
            # Also clean up replay children after an abnormal CLI exit. The
            # replay itself has a 90-second backstop and never runs real ADB.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            _, status, usage = os.wait4(process.pid, 0)
            process.returncode = os.waitstatus_to_exitcode(status)
    return {"wall_s": time.monotonic() - started, "returncode": process.returncode,
            "timed_out": timed_out, "cpu_user_s": usage.ru_utime, "cpu_system_s": usage.ru_stime,
            "cpu_s": usage.ru_utime + usage.ru_stime,
            "cli_sampled_peak_rss_kib": peak_rss if samples else None,
            "cli_sampled_peak_hwm_kib": peak_hwm if samples else None, "rss_samples": samples,
            "wait4_maxrss_kib": usage.ru_maxrss}


def read_status(path):
    status = None
    with path.open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            try:
                value = json.loads(line)
                if isinstance(value, dict) and value.get("type") == "status":
                    status = value
            except ValueError:
                pass
    return status


def run_capture(cli, replay, root, source, count, size, trial, combo):
    record_format, transport = combo.split("/")
    label = f"{source.name}-{trial}-{record_format}-{transport}"
    output = root / (label + ".jsonl")
    stdout, stderr = root / (label + ".stdout"), root / (label + ".stderr")
    command = [str(cli), "capture", "--adb", str(replay), "--serial", "PERF-MOCK",
               "--record-format", record_format, "--transport", transport,
               "--max-entries", str(count), "--duration-secs", "60", "--max-bytes", "536870912",
               "--output", str(output)]
    env = dict(os.environ, CIALLOGCAT_CONFIG_DIR=str(root / "config"),
               CIALLOGCAT_PERF_INPUT=str(source))
    run = {"trial": trial, "warmup": trial == 0, "combination": combo,
           "transport": transport, "record_format": record_format, "success": False,
           "argv": command, "artifact": output.name, "stdout": stdout.name, "stderr": stderr.name,
           "records_per_s": None, "message_mib_per_s": None, "exit_reason": "runner_error"}
    try:
        run.update(measure(command, env, stdout, stderr))
        run["artifact_bytes"] = output.stat().st_size if output.exists() else 0
        status = read_status(stderr)
        run["cli_status"] = status
        run["cli_exit_reason"] = status.get("reason") if status else None
        verified = validate(output, count, size, transport, record_format, source)
        run["verification"] = verified
        run["exit_reason"] = ("host_deadline" if run["timed_out"] else
                              "cli_nonzero_exit" if run["returncode"] else "integrity_failure")
        require(not run["timed_out"], "host deadline exceeded")
        require(run["returncode"] == 0, f"CLI exit {run['returncode']}; see {stderr.name}")
        require(verified["success"], verified.get("error", "integrity failure"))
        require(status is not None and status["state"] == "completed" and status["reason"] == "max_entries"
                and status["error"] is None and status["entries"] == count
                and status["bytes"] == run["artifact_bytes"] and status["discarded_entries"] == 0
                and status["stream_interruptions"] == 0 and status["diagnostics_complete"] is True
                and status["record_schema_version"] == int(record_format[1:])
                and status["artifact_id"] == verified["artifact_id"]
                and status["session_id"] == verified["session_id"]
                and status["visible_end"] == {"seq": count, "offset": run["artifact_bytes"]},
                "CLI status disagrees with complete artifact")
        run.update(success=True, exit_reason="max_entries", records_per_s=count / run["wall_s"],
                   message_mib_per_s=count * size / (1024 ** 2 * run["wall_s"]))
    except (OSError, ValueError, KeyError, TypeError) as error:
        run["error"] = f"{type(error).__name__}: {error}"
    return run


def summarize(runs, combinations, trials):
    summary = {}
    for combo in combinations:
        measured = [r for r in runs if r["combination"] == combo and not r["warmup"]]
        good = [r for r in measured if r["success"]]
        medians = {}
        if good:
            for key in ("wall_s", "records_per_s", "message_mib_per_s", "cpu_s", "cpu_user_s",
                        "cpu_system_s", "cli_sampled_peak_rss_kib", "cli_sampled_peak_hwm_kib",
                        "wait4_maxrss_kib", "artifact_bytes"):
                values = [r[key] for r in good if r[key] is not None]
                medians[key] = statistics.median(values) if values else None
        warmups = [r for r in runs if r["combination"] == combo and r["warmup"]]
        summary[combo] = {"successful_runs": len(good), "failed_runs": len(measured) - len(good),
                          "expected_runs": trials, "warmup_success": bool(warmups) and all(r["success"] for r in warmups),
                          "complete": len(good) == trials and bool(warmups) and all(r["success"] for r in warmups),
                          "median": medians or None}
    return summary


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def save_report(root, report):
    temporary = root / "report.json.tmp"
    temporary.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    temporary.replace(root / "report.json")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--trials", type=int, help="measured rounds (default 5; 1 with --smoke)")
    parser.add_argument("--include-compact", action="store_true", help="also compare explicit v3/proto")
    parser.add_argument("--smoke", action="store_true", help="64 short / 32 long records; tool validation only")
    args = parser.parse_args()
    if args.trials is None:
        args.trials = 1 if args.smoke else 5
    if platform.system() != "Linux" or not 1 <= args.trials <= 10:
        parser.error("requires Linux and 1..10 trials")
    cli = args.cli.resolve()
    if not cli.is_file() or not os.access(cli, os.X_OK):
        parser.error("--cli must name an executable file")
    root = args.output_dir.resolve()
    root.mkdir(parents=True, exist_ok=False)
    (root / "replay.rs").write_text(REPLAY)
    replay = root / "replay-adb"
    subprocess.run(["rustc", "--edition=2024", "-O", str(root / "replay.rs"),
                    "-o", str(replay)], check=True, timeout=60)
    combinations = ["v2/text", "v2/proto"] + (["v3/proto"] if args.include_compact else [])
    report = {"report_schema_version": 2, "scope": "synthetic Linux host pipeline; no device/USB/GPU measurement",
              "started_utc": datetime.now(timezone.utc).isoformat(), "state": "running", "smoke": args.smoke,
              "cli": str(cli), "cli_sha256": sha256(cli), "tool_sha256": sha256(Path(__file__)),
              "replay_sha256": sha256(replay), "combinations": combinations,
              "platform": platform.platform(), "trials": args.trials,
              "warmup_rounds": 1, "order_policy": "left-rotate combination list by round index (warmup=0); two combinations alternate",
              "host_deadline_s": 75, "replay_deadline_s": 90, "rss_sample_interval_s": .005,
              "clock_ticks_per_second": os.sysconf("SC_CLK_TCK"),
              "wall_measurement": "Popen through child cleanup/reap; includes startup and replay; excludes generation, compilation, hashing and validation",
              "cpu_measurement": "wait4 user+system includes CLI and replay children it reaped; excludes Python harness",
              "rss_measurement": "only owned CLI PID VmRSS and VmHWM sampled at 5ms; no sample is null; wait4 maxrss is separate, not a process-tree sum",
              "disk_measurement": "buffered local-file writes, no fsync guarantee",
              "aggregation": "successful non-warmup runs only; incomplete groups explicitly marked; any failed run exits 1",
              "cases": []}
    save_report(root, report)
    counts = (64, 32) if args.smoke else (50000, 20000)
    for name, count, size in [("short-128B", counts[0], 128), ("long-2048B", counts[1], 2048)]:
        source = root / name
        inputs(source, count, size)
        case = {"name": name, "records": count, "message_bytes": size,
                "wire_bytes": {t: source.with_suffix("." + t).stat().st_size
                               for t in ("text", "proto")},
                "input_sha256": {t: sha256(source.with_suffix("." + t)) for t in ("text", "proto")},
                "round_order": [], "runs": []}
        report["cases"].append(case)
        for trial in range(args.trials + 1):
            offset = trial % len(combinations)
            order = combinations[offset:] + combinations[:offset]
            case["round_order"].append({"trial": trial, "warmup": trial == 0, "combinations": order})
            for position, combo in enumerate(order, 1):
                run = run_capture(cli, replay, root, source, count, size, trial, combo)
                run.update(order_position=position, case_run_index=len(case["runs"]) + 1)
                # Keep every failure and successful final-round artifact;
                # stdout/stderr and per-run measurements always survive.
                output = root / run["artifact"]
                run["artifact_retained"] = output.exists()
                if run["success"] and trial != args.trials:
                    output.unlink()
                    run["artifact_retained"] = False
                case["runs"].append(run)
                case["summary"] = summarize(case["runs"], combinations, args.trials)
                case["median"] = {combo: stats["median"] for combo, stats in case["summary"].items()}
                save_report(root, report)
                with (root / "runs.jsonl").open("a", encoding="utf-8") as journal:
                    journal.write(json.dumps({"case": name, **run}) + "\n")
                print(json.dumps({"case": name, **run}), flush=True)
    report["cli_sha256_after"] = sha256(cli)
    report["cli_unchanged"] = report["cli_sha256_after"] == report["cli_sha256"]
    report["success"] = report["cli_unchanged"] and all(r["success"] for c in report["cases"] for r in c["runs"])
    report["state"] = "completed" if report["success"] else "failed"
    report["finished_utc"] = datetime.now(timezone.utc).isoformat()
    save_report(root, report)
    print(json.dumps({"report": str(root / "report.json"),
                      "success": report["success"],
                      "results": [{"case": c["name"], "summary": c["summary"]} for c in report["cases"]]}))
    return 0 if report["success"] else 1


if __name__ == "__main__":
    sys.exit(main())
