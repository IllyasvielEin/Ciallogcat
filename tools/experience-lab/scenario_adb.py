#!/usr/bin/env python3
"""Synthetic continuous ADB peer. Uses only stdlib; never executes real ADB."""
from __future__ import annotations

import argparse
import datetime as dt
import json
import os
from pathlib import Path
import re
import signal
import sys
import tempfile
import threading
import time

ACTIONS = ("init", "checkout", "crash", "offline", "online", "discovery-error",
           "discovery-ok", "quiet", "recover", "stop")
BUFFERS = frozenset(("main", "system", "crash", "events", "radio", "all"))
WATCHDOG_SECONDS = 600
INTERVAL = 0.05
ARGS = sys.argv[1:]
LAB: Path | None = None


def now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="milliseconds")


def trace(event: str, **details: object) -> None:
    if LAB is None:
        return
    record = {"at": now(), "pid": os.getpid(), "argv": ARGS, "event": event, **details}
    data = (json.dumps(record, ensure_ascii=False, separators=(",", ":")) + "\n").encode()
    fd = os.open(LAB / "trace.jsonl", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        if os.write(fd, data) != len(data):
            raise OSError("short trace write")
    finally:
        os.close(fd)


def atomic_json(path: Path, value: object) -> None:
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            json.dump(value, output, ensure_ascii=False, indent=2)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def initial() -> dict:
    serial = os.environ.get("CIALLOGCAT_LAB_SERIAL", "LAB-ANDROID")
    if not re.fullmatch(r"[A-Za-z0-9_.:-]{1,128}", serial) or serial.startswith("-"):
        raise ValueError("invalid CIALLOGCAT_LAB_SERIAL")
    return {"version": 1, "serial": serial, "online": True, "discovery_error": False,
            "quiet": False, "stopped": False, "generation": 0, "revision": 0,
            "phase": "idle", "updated_at": now(), "events": []}


def read_state() -> dict:
    assert LAB is not None
    with (LAB / "state.json").open(encoding="utf-8") as source:
        return json.load(source)


def oracle() -> dict:
    return {"synthetic": True, "private_to_facilitator": True,
            "checkout": {"request": "req-checkout-042", "order": "ORDER-1042",
                         "root_cause": "Payment worker uses expired credential version 17 after credential version 18 is committed; retry reuses stale cached credential and ends with AUTH_EXPIRED, translated by UI to timeout.",
                         "red_herring": "com.example.chat timeout is a separate request chat-poll-900, not checkout.",
                         "expected_chain": ["CredentialStore", "PaymentWorker", "HttpClient", "RetryPolicy", "CheckoutUI"]},
            "crash": {"process": "com.example.shop:payments", "root_cause": "Native receipt renderer receives unsupported currency glyph and throws IllegalArgumentException; tail of bounded long diagnostic contains RECEIPT_GLYPH_U+20BF.",
                      "tail_marker": "ROOT_CAUSE=RECEIPT_GLYPH_U+20BF", "unicode": "支付失败：币种符号 ₿ / 雪 / 🚀"},
            "pids_before_recover": processes(0), "pids_after_first_recover": processes(1),
            "limits": {"interval_seconds": INTERVAL, "watchdog_seconds": WATCHDOG_SECONDS,
                       "max_pending_action_events": 128, "long_message_bytes_under": 16384}}


def control(action: str) -> int:
    assert LAB is not None
    lock = LAB / ".control-lock"
    deadline = time.monotonic() + 3
    while True:
        try:
            lock.mkdir()
            break
        except FileExistsError:
            if time.monotonic() > deadline:
                raise ValueError("control lock busy; serialize actions")
            time.sleep(0.01)
    try:
        state = initial() if action == "init" or not (LAB / "state.json").exists() else read_state()
        if action == "init":
            atomic_json(LAB / "oracle.json", oracle())
        elif action == "offline":
            state["online"] = False
        elif action == "online":
            state["online"] = True
            state["stopped"] = False
        elif action == "discovery-error":
            state["discovery_error"] = True
        elif action == "discovery-ok":
            state["discovery_error"] = False
        elif action == "quiet":
            state["quiet"] = True
        elif action == "recover":
            state["generation"] += 1
            state["quiet"] = False
            state["stopped"] = False
            state["online"] = True
        elif action == "stop":
            state["stopped"] = True
        elif action in ("checkout", "crash"):
            state["quiet"] = False
        state["revision"] += 1
        state["phase"] = action
        state["updated_at"] = now()
        state["events"] = (state["events"] + [{"id": state["revision"], "action": action,
                                                  "at": state["updated_at"],
                                                  "generation": state["generation"]}])[-128:]
        atomic_json(LAB / "state.json", state)
        trace("action", action=action, phase=state["phase"], revision=state["revision"])
        print(json.dumps({"ok": True, "action": action, "state": state}, ensure_ascii=False))
    finally:
        lock.rmdir()
    return 0


def processes(generation: int) -> list[tuple[int, str]]:
    base = generation * 1000
    return [(4100 + base, "com.example.shop"), (4101 + base, "com.example.shop:payments"),
            (5200 + base, "com.example.chat"), (1000, "system_server")]


def parse_command(args: list[str]) -> tuple[str, str | None]:
    if args == ["devices", "-l"]:
        return "devices", None
    if len(args) < 3 or args[0] != "-s" or not args[1] or args[1].startswith("-"):
        raise ValueError("unsupported argv")
    serial, rest = args[1], args[2:]
    forms = ((["features"], "features"),
             (["shell", "pm", "list", "packages", "-3"], "packages"),
             (["shell", "ps", "-A", "-o", "PID,NAME"], "ps_preferred"),
             (["shell", "ps", "-A"], "ps_fallback"))
    for words, command in forms:
        if rest == words:
            return command, serial
    command, tail = "", []
    if rest[:5] == ["logcat", "-v", "threadtime", "-T", "1"]:
        command, tail = "text", rest[5:]
    elif len(rest) == 4 and rest[:3] == ["shell", "-T", "-n"]:
        prefix, suffix = "exec logcat --proto -v threadtime -T 1 ", " '*:V'"
        if rest[3].startswith(prefix) and rest[3].endswith(suffix):
            command, tail = "proto", rest[3][len(prefix):-len(suffix)].split(" ")
    if command and tail and len(tail) % 2 == 0 and all(
        tail[i] == "-b" and tail[i + 1] in BUFFERS for i in range(0, len(tail), 2)
    ):
        return command, serial
    raise ValueError("unsupported argv")


# Each row is (process index, level, tag, message). All identities are synthetic.
def scenario_rows(action: str) -> list[tuple[int, str, str, str]]:
    if action == "checkout":
        return [
            (0, "I", "CheckoutUI", "checkout tapped request=req-checkout-042 order=ORDER-1042 cart_items=3"),
            (0, "I", "CredentialStore", "credential refresh committed version=18 previous=17 request=refresh-017"),
            (1, "D", "PaymentWorker", "request=req-checkout-042 order=ORDER-1042 credential_cache_version=17 invalidation_pending=true"),
            (2, "E", "ChatSync", "timeout request=chat-poll-900 endpoint=/chat/poll duration_ms=30000 retryable=true"),
            (3, "D", "ConnectivityService", "validated network=wifi dns_ok=true transport_ready=true"),
            (1, "I", "HttpClient", "request=req-checkout-042 attempt=1 POST /payments authorize credential_version=17"),
            (1, "W", "HttpClient", "request=req-checkout-042 attempt=1 status=401 code=AUTH_EXPIRED server_expected_version=18 elapsed_ms=82"),
            (1, "W", "RetryPolicy", "request=req-checkout-042 attempt=2 reason=authorization_failure reused_cached_credential=true backoff_ms=250"),
            (0, "D", "CatalogCache", "catalog refresh completed item_count=42"),
            (1, "I", "HttpClient", "request=req-checkout-042 attempt=2 POST /payments authorize credential_version=17"),
            (1, "E", "HttpClient", "request=req-checkout-042 attempt=2 status=401 code=AUTH_EXPIRED elapsed_ms=79"),
            (1, "E", "PaymentWorker", "request=req-checkout-042 order=ORDER-1042 terminal=AUTH_EXPIRED attempts=2 network_roundtrip_ms=161"),
            (0, "E", "CheckoutUI", "request=req-checkout-042 order=ORDER-1042 user_message=payment timeout mapped_from=AUTH_EXPIRED"),
            (0, "W", "CheckoutUI", "支付失败，请稍后重试 request=req-checkout-042 order=ORDER-1042"),
        ]
    if action == "crash":
        diagnostic = ("receipt diagnostic request=req-receipt-043 order=ORDER-1042 支付失败：币种符号 ₿ / 雪 / 🚀 "
                      + "layout-cell=ok;" * 500
                      + " ROOT_CAUSE=RECEIPT_GLYPH_U+20BF renderer=ReceiptNativeRenderer currency=BTC")
        return [
            (0, "I", "ReceiptUI", "open receipt request=req-receipt-043 order=ORDER-1042 currency=BTC"),
            (1, "D", "ReceiptNativeRenderer", "render start request=req-receipt-043 currency_symbol=₿ locale=zh-CN"),
            (2, "W", "ChatSync", "timeout request=chat-poll-901 endpoint=/chat/poll; unrelated background retry"),
            (1, "E", "ReceiptNativeRenderer", diagnostic),
            (1, "F", "AndroidRuntime", "FATAL EXCEPTION: receipt-worker-1"),
            (1, "F", "AndroidRuntime", "Process: com.example.shop:payments, request=req-receipt-043"),
            (1, "F", "AndroidRuntime", "java.lang.IllegalArgumentException: unsupported currency glyph U+20BF"),
            (1, "F", "AndroidRuntime", "    at com.example.shop.receipt.ReceiptNativeRenderer.render(ReceiptNativeRenderer.kt:88)"),
            (1, "F", "AndroidRuntime", "    at com.example.shop.payments.ReceiptWorker.run(ReceiptWorker.kt:41)"),
            (3, "W", "ActivityManager", "Process com.example.shop:payments has died; receipt transaction incomplete"),
            (0, "E", "ReceiptUI", "支付进程已退出 🚀 request=req-receipt-043 receipt_display_failed=true"),
        ]
    if action == "recover":
        return [(3, "I", "ActivityManager", "restarted shop and chat processes; new PID generation active"),
                (0, "I", "CredentialStore", "credential cache repaired version=18 invalidation_pending=false"),
                (1, "I", "PaymentWorker", "recovery probe request=req-recovery-044 authorization status=200"),
                (0, "I", "CheckoutUI", "恢复成功 ✓ order=ORDER-1042")]
    return []


def baseline(index: int) -> tuple[int, str, str, str]:
    options = [(0, "D", "ShopHeartbeat", "shop idle ui_responsive=true"),
               (1, "D", "PaymentWorker", "payment queue pending=0"),
               (2, "I", "ChatSync", "chat heartbeat connected=true"),
               (3, "D", "ActivityManager", "system health background_tasks=4")]
    process, level, tag, message = options[index % len(options)]
    return process, level, tag, f"{message} tick={index:06d}"


def varint(value: int) -> bytes:
    result = bytearray()
    while value > 127:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def proto_bytes(pid: int, name: str, level: str, tag: str, message: str, stamp: int) -> bytes:
    priority = {"V": 2, "D": 3, "I": 4, "W": 5, "E": 6, "F": 8}[level]
    uid = 1000 if name == "system_server" else (10042 if name.startswith("com.example.shop") else 10043)
    numbers = ((1, stamp // 1_000_000_000), (2, stamp % 1_000_000_000),
               (3, priority), (4, uid),
               (5, pid), (6, pid + 10))
    payload = b"".join(varint(field << 3) + varint(value) for field, value in numbers)
    for field, value in ((7, tag.encode() + b"\0"), (8, message.encode()), (9, name.encode())):
        payload += varint((field << 3) | 2) + varint(len(value)) + value
    return len(payload).to_bytes(8, "little") + payload


def write_all(data: bytes) -> None:
    pending = memoryview(data)
    while pending:
        count = os.write(1, pending)
        if count <= 0:
            raise OSError("stdout write made no progress")
        pending = pending[count:]


def stream(command: str) -> int:
    state = read_state()
    last_event = state["revision"]
    queue: list[tuple[int, str, str, str]] = []
    index = 0
    trace("stream_started", transport=command, phase=state["phase"], generation=state["generation"])
    if command == "text":
        write_all(b"--------- beginning of main\n")
    while True:
        state = read_state()
        if not state["online"] or state["stopped"]:
            reason = "device offline" if not state["online"] else "lab stream stopped"
            print(f"scenario_adb: {reason}", file=sys.stderr, flush=True)
            trace("stream_exit", phase=state["phase"], reason=reason, code=17)
            return 17
        for event in state["events"]:
            if event["id"] > last_event:
                queue.extend(scenario_rows(event["action"]))
                last_event = event["id"]
                trace("phase", phase=event["action"], revision=event["id"], queued=len(queue))
        if not state["quiet"]:
            row = queue.pop(0) if queue else baseline(index)
            process, level, tag, message = row
            pid, name = processes(state["generation"])[process]
            stamp = time.time_ns()
            if command == "proto":
                data = proto_bytes(pid, name, level, tag, message, stamp)
            else:
                timestamp = dt.datetime.fromtimestamp(stamp / 1e9).strftime("%m-%d %H:%M:%S.%f")[:-3]
                data = f"{timestamp} {pid:5d} {pid + 10:5d} {level} {tag}: {message}\n".encode()
            write_all(data)
            index += 1
        time.sleep(INTERVAL)


def main() -> int:
    global LAB
    is_control = bool(ARGS and ARGS[0] == "--control")
    if is_control:
        parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
        parser.add_argument("--control", required=True)
        parser.add_argument("--action", required=True, choices=ACTIONS)
        parsed = parser.parse_args(ARGS)
        LAB = Path(parsed.control).resolve()
    else:
        directory = os.environ.get("CIALLOGCAT_LAB_DIR")
        if not directory:
            raise ValueError("CIALLOGCAT_LAB_DIR is required; use a separate directory per participant")
        LAB = Path(directory).resolve()
    LAB.mkdir(parents=True, exist_ok=True)
    trace("invocation")
    if is_control:
        return control(parsed.action)
    if not (LAB / "state.json").exists():
        # Initial setup is explicit, so concurrent discovery never resets a live case.
        raise ValueError("lab is not initialized; run --control DIR --action init")
    command, serial = parse_command(ARGS)
    state = read_state()
    trace("command", command=command, phase=state["phase"])
    if command == "devices":
        if state["discovery_error"]:
            print("scenario_adb: discovery service temporarily unavailable", file=sys.stderr)
            return 23
        status = "device" if state["online"] else "offline"
        print(f"List of devices attached\n{state['serial']}\t{status} product:lab model:Lab_Phone device:lab transport_id:1\n")
        return 0
    if serial != state["serial"] or not state["online"] or state["stopped"]:
        print("scenario_adb: selected device unavailable", file=sys.stderr)
        return 1
    if command == "packages":
        print("package:com.example.shop\npackage:com.example.chat")
    elif command == "features":
        print("shell_v2,cmd,stat_v2")
    elif command == "ps_preferred":
        print("PID NAME")
        for pid, name in processes(state["generation"]):
            print(f"{pid} {name}")
    elif command == "ps_fallback":
        print("USER PID PPID VSZ RSS WCHAN ADDR S NAME")
        for pid, name in processes(state["generation"]):
            print(f"u0_a42 {pid} 1 123456 12345 0 0 S {name}")
    else:
        return stream(command)
    return 0


def watchdog() -> None:
    time.sleep(WATCHDOG_SECONDS)
    trace("watchdog", code=124)
    os._exit(124)


def terminated(signum: int, _frame: object) -> None:
    trace("terminated", signal=signum)
    raise SystemExit(128 + signum)


if __name__ == "__main__":
    threading.Thread(target=watchdog, daemon=True).start()
    signal.signal(signal.SIGTERM, terminated)
    signal.signal(signal.SIGINT, terminated)
    try:
        raise SystemExit(main())
    except (ValueError, OSError, json.JSONDecodeError) as error:
        trace("error", message=str(error))
        print(f"scenario_adb: {error}", file=sys.stderr)
        raise SystemExit(2)
