#!/usr/bin/env python3
"""Linux CLI soak with RSS/query measurements; synthetic is not device validation."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", required=True)
    parser.add_argument("--output-dir", required=True)
    parser.add_argument("--seconds", type=int, default=1800)
    parser.add_argument("--adb", help="real-device mode: existing ADB executable")
    parser.add_argument("--serial", default="SOAK-SYNTHETIC")
    args = parser.parse_args()
    if not 5 <= args.seconds <= 3600:
        parser.error("--seconds must be 5..3600")
    root = Path(args.output_dir).absolute()
    root.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, CIALLOGCAT_CONFIG_DIR=str(root / "config"))
    if args.adb:
        adb = str(Path(args.adb).absolute())
    else:
        adb_file = root / "synthetic-adb"
        # Independent finite source; never delegates to a real ADB executable.
        adb_file.write_text('''#!/usr/bin/env python3
import os,sys,time,threading
threading.Timer(''' + str(args.seconds + 60) + ''',lambda:os._exit(124)).start()
if sys.argv[1:] != ['-s','SOAK-SYNTHETIC','logcat','-v','threadtime','-T','1','-b','main','-b','system','-b','crash']: os._exit(2)
index=0
deadline=time.monotonic()
while True:
    for _ in range(20):
        message=('probe-' if index%500==0 else 'stream-')+str(index)+' '+('x'*480)
        os.write(1,('10-23 12:00:00.000  1234  1235 I Soak: '+message+'\\n').encode())
        index+=1
    deadline+=0.1
    time.sleep(max(0,deadline-time.monotonic()))
''')
        adb_file.chmod(0o700)
        adb = str(adb_file)
    session = root / "session"
    def command(*parts):
        tick = time.monotonic()
        result = subprocess.run([args.cli, *parts], env=env, capture_output=True,
                                text=True, encoding="utf-8", timeout=20)
        if result.returncode:
            raise RuntimeError(result.stderr)
        return json.loads(result.stdout), (time.monotonic() - tick) * 1000
    capture_binary_sha256 = hashlib.sha256(Path(args.cli).read_bytes()).hexdigest()
    samples = []
    started = time.monotonic()
    failure = None
    final = None
    cursor = None
    try:
        ready, _ = command("start", "--adb", adb, "--serial", args.serial,
                           "--session-dir", str(session), "--record-format", "v2",
                           "--duration-secs", str(args.seconds + 15),
                           "--max-bytes", "1073741824")
        pid = ready["worker_pid"]
        while time.monotonic() - started < args.seconds:
            time.sleep(min(10, max(0, args.seconds - (time.monotonic() - started))))
            status, _ = command("status", "--session-dir", str(session))
            query_args = ["query-v2", "--session-dir", str(session), "--mode", "tail",
                          "--query", "probe-", "--preview-bytes", "32"]
            if cursor:
                query_args += ["--cursor", cursor]
            page, latency = command(*query_args)
            cursor = page["next_cursor"]
            rss = None
            try:
                for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                    if line.startswith("VmRSS:"):
                        rss = int(line.split()[1]) * 1024
            except FileNotFoundError:
                pass
            sample = {"elapsed_seconds":time.monotonic()-started, "rss_bytes":rss,
                      "entries":status["entries"], "bytes":status["bytes"],
                      "state":status["state"], "query_ms":latency,
                      "query_file_bytes":page["scan"]["file_bytes_read"],
                      "query_records":page["scan"]["records"], "has_more":page["has_more"]}
            samples.append(sample)
            with (root / "samples.jsonl").open("a") as output:
                output.write(json.dumps(sample) + "\n")
            if status["state"] != "running":
                raise RuntimeError(f"capture ended unexpectedly: {status}")
            print(json.dumps(sample), flush=True)
    except Exception as error:
        failure = str(error)
    finally:
        if session.exists():
            try:
                final, _ = command("stop", "--session-dir", str(session))
            except Exception as error:
                failure = f"{failure or ''}; stop: {error}"
    if final and (final["state"] != "completed" or not final["diagnostics_complete"]):
        failure = f"{failure or ''}; capture did not confirm a successful stop: {final}"
    if final and not failure:
        try:
            with (session / "logs.jsonl").open(encoding="utf-8") as stream:
                header = json.loads(next(stream))
                count = 0
                last = 0
                footer = None
                for line in stream:
                    row = json.loads(line)
                    if footer is not None:
                        raise ValueError("data after artifact_end")
                    if row["type"] == "artifact_end":
                        footer = row
                    else:
                        if row["seq"] <= last or row["provenance"]["session_id"] != header["session_id"]:
                            raise ValueError("record identity changed or repeated")
                        last = row["seq"]
                        count += 1
                if not footer or footer["record_count"] != count or final["entries"] != count or final["visible_end"]["seq"] != last or final["visible_end"]["offset"] != (session / "logs.jsonl").stat().st_size:
                    raise ValueError("artifact footer and published boundary disagree")
        except Exception as error:
            failure = f"artifact validation: {error}"
    rss = [s["rss_bytes"] for s in samples if s["rss_bytes"] is not None]
    latencies = sorted(s["query_ms"] for s in samples)
    report = {"kind":"real_device" if args.adb else "synthetic_host_only",
              "capture_binary_sha256":capture_binary_sha256,
              "requested_seconds":args.seconds, "elapsed_seconds":time.monotonic()-started,
              "samples":len(samples), "rss_first":rss[0] if rss else None,
              "rss_last":rss[-1] if rss else None, "rss_max":max(rss,default=None),
              "query_p50_ms":statistics.median(latencies) if latencies else None,
              "query_p95_ms":latencies[min(len(latencies)-1,int(len(latencies)*.95))] if latencies else None,
              "final_status":final,"error":failure}
    (root / "report.json").write_text(json.dumps(report, indent=2)+"\n")
    print(json.dumps(report), flush=True)
    return 1 if failure else 0


if __name__ == "__main__":
    raise SystemExit(main())
