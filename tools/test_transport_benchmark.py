"""Adversarial checks for benchmark evidence, using an independent wire fixture."""
import base64
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


spec = importlib.util.spec_from_file_location(
    "transport_benchmark", Path(__file__).with_name("transport-benchmark.py"))
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)

# A hand-encoded AOSP frame: seconds, nanoseconds, Info, UID, PID, TID,
# NUL-terminated tag, 48-byte message and process name. No benchmark encoder is
# used to produce test inputs or archived evidence.
BODY = b"event=00000000 request=checkout status=ok xxxxxx"
PAYLOAD = (bytes.fromhex("08c0b1edd60610c0a9d33a180420914e28d20930d3093a0550657266004230")
           + BODY + b"\x4a\x11com.example.bench")
FRAME = len(PAYLOAD).to_bytes(8, "little") + PAYLOAD
SESSION = "bd40f655-e5b3-4ac2-ab4e-e6d7c7157f99"
ARTIFACT = "7711f5d0-570f-427f-8a0c-08bcb17c2dde"


def fixture(version=3, transport="proto"):
    header = {"schema_version": version, "type": "document", "artifact_id": ARTIFACT,
              "session_id": SESSION, "source": {"kind": "adb", "device_serial": "PERF-MOCK",
              "transport_format": "logcat_proto_experimental" if transport == "proto" else "threadtime"},
              "normalization": {"protobuf_raw_payload": "preserved_base64"}}
    if version == 3:
        header["record_encoding"] = "proto_raw_v1"
        header["normalization"]["proto_projection"] = "aosp_logcat_16r1_host_v1"
    row = {"schema_version": version, "type": "entry", "seq": 1,
           "provenance": {"session_id": SESSION, "seq": 1, "segment": 1, "attempt": 1},
           "proto": {"raw_payload": base64.b64encode(PAYLOAD).decode()}}
    if version == 2:
        row.update(message=BODY.decode(), pid=1234, tid=1235, tag="Perf", level="I",
                   device_time={"text": "1792760000.123000000"},
                   process={"name": "com.example.bench", "source": "device_process_cache"})
        row["proto"].update(time_sec=1792760000, time_nsec=123000000, priority=4,
                            uid=10001, pid=1234, tid=1235, process_name="com.example.bench",
                            tag_bytes="UGVyZgA=", message_bytes=base64.b64encode(BODY).decode())
    if transport == "text":
        del row["proto"]
        row["device_time"] = {"text": "10-23 20:53:20.123"}
        row["process"] = None
        header["normalization"] = {}
    footer = {"schema_version": version, "type": "artifact_end", "artifact_id": ARTIFACT,
              "record_count": 1, "first_seq": 1, "last_seq": 1, "finalized": True,
              "scope": {"kind": "retained", "source_record_count": 1,
                        "source_first_seq": 1, "source_last_seq": 1, "omitted_before": 0},
              "capture": {"state": "stopped", "stop_reason": "max_entries", "attempts": 1,
                          "completeness": "unknown", "known_gaps": []}}
    return [header, row, footer]


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.source = self.root / "source"
        self.source.with_suffix(".proto").write_bytes(FRAME)
        self.source.with_suffix(".text").write_bytes(
            b"10-23 20:53:20.123  1234  1235 I Perf: " + BODY + b"\n")

    def check(self, rows, version=3, transport="proto", trailer=""):
        artifact = self.root / "artifact.jsonl"
        artifact.write_text("".join(json.dumps(row) + "\n" for row in rows) + trailer)
        return bench.validate(artifact, 1, 48, transport, f"v{version}", self.source)

    def test_all_three_formats_accept_independent_fixture(self):
        for version, transport in ((2, "text"), (2, "proto"), (3, "proto")):
            with self.subTest(version=version, transport=transport):
                result = self.check(fixture(version, transport), version, transport)
                self.assertTrue(result["success"], result)
                self.assertEqual(result["records_verified"], 1)
                self.assertEqual(result["raw_payloads_verified"], int(transport == "proto"))

    def test_display_cannot_hide_corrupt_or_length_prefixed_payload(self):
        for version in (2, 3):
            for replacement in (FRAME, PAYLOAD[:-1], PAYLOAD.replace(b"checkout", b"checkouX")):
                with self.subTest(version=version, replacement=replacement):
                    rows = fixture(version)
                    rows[1]["proto"]["raw_payload"] = base64.b64encode(replacement).decode()
                    self.assertFalse(self.check(rows, version)["success"])

    def test_v2_redundant_evidence_must_match_payload(self):
        for key, bad in (("uid", 1), ("message_bytes", ""), ("tag_bytes", "UGVyZg==")):
            with self.subTest(key=key):
                rows = fixture(2)
                rows[1]["proto"][key] = bad
                self.assertFalse(self.check(rows, 2)["success"])

    def test_compact_redundant_null_fields_and_missing_declarations_fail(self):
        for key in ("message", "pid", "device_time"):
            rows = fixture()
            rows[1][key] = None
            self.assertFalse(self.check(rows)["success"])
        rows = fixture()
        rows[1]["proto"]["message_bytes"] = None
        self.assertFalse(self.check(rows)["success"])
        for key in ("proto_projection", "protobuf_raw_payload"):
            rows = fixture()
            del rows[0]["normalization"][key]
            self.assertFalse(self.check(rows)["success"])
        rows = fixture()
        del rows[0]["record_encoding"]
        self.assertFalse(self.check(rows)["success"])

    def test_sequence_provenance_and_mixed_version_fail(self):
        for section, key, bad in ((1, "seq", 2), (1, "seq", True), (1, "schema_version", 2),
                                  (0, "schema_version", 2), (2, "schema_version", 2)):
            rows = fixture()
            rows[section][key] = bad
            self.assertFalse(self.check(rows)["success"])
        for key, bad in (("seq", 2), ("session_id", ARTIFACT), ("attempt", 2)):
            rows = fixture()
            rows[1]["provenance"][key] = bad
            self.assertFalse(self.check(rows)["success"])

    def test_missing_duplicate_extra_records_and_trailing_data_fail(self):
        rows = fixture()
        for broken in (rows[:-1], [rows[0], rows[2]], [*rows[:2], rows[1], rows[2]]):
            self.assertFalse(self.check(broken)["success"])
        for trailer in ("\n", "{}\n", "not json"):
            self.assertFalse(self.check(rows, trailer=trailer)["success"])

    def test_footer_identity_bounds_stop_and_gap_failures(self):
        for key, bad in (("record_count", 2), ("first_seq", 2), ("last_seq", 2),
                         ("artifact_id", SESSION), ("finalized", False)):
            rows = fixture()
            rows[2][key] = bad
            self.assertFalse(self.check(rows)["success"])
        for key, bad in (("stop_reason", "duration"), ("known_gaps", [{"kind": "stream_interruptions"}]),
                         ("attempts", 2), ("completeness", "partial")):
            rows = fixture()
            rows[2]["capture"][key] = bad
            self.assertFalse(self.check(rows)["success"])
        rows = fixture()
        rows[2]["scope"]["source_record_count"] = 2
        self.assertFalse(self.check(rows)["success"])

    def test_actual_replay_framing_and_bytes_are_checked(self):
        for broken in (FRAME[:7], FRAME[:-1], FRAME + b"x", b"\xff" * 8,
                       FRAME.replace(b"checkout", b"checkouX")):
            with self.subTest(wire=broken):
                self.source.with_suffix(".proto").write_bytes(broken)
                self.assertFalse(self.check(fixture())["success"])

    def test_malformed_evidence_is_a_failure_result_not_a_crash(self):
        for bad in (None, "?", "", "AA==\n", [], {}):
            rows = fixture()
            rows[1]["proto"]["raw_payload"] = bad
            self.assertFalse(self.check(rows)["success"])
        for section in (0, 1, 2):
            rows = fixture()
            rows[section] = None
            self.assertFalse(self.check(rows)["success"])
        rows = fixture(2)
        rows[1]["message"] = 42
        self.assertFalse(self.check(rows, 2)["success"])

    def test_failed_and_warmup_measurements_cannot_contaminate_summary(self):
        good = {"combination": "v3/proto", "warmup": False, "success": True,
                "wall_s": 2, "records_per_s": 50, "message_mib_per_s": 1,
                "cpu_s": 1, "cpu_user_s": .6, "cpu_system_s": .4,
                "cli_sampled_peak_rss_kib": None, "cli_sampled_peak_hwm_kib": None,
                "wait4_maxrss_kib": 100, "artifact_bytes": 1000}
        warmup = dict(good, warmup=True, wall_s=100)
        failed = dict(good, success=False, wall_s=1000)
        result = bench.summarize([good, warmup, failed], ["v3/proto"], 2)["v3/proto"]
        self.assertEqual(result["median"]["wall_s"], 2)
        self.assertIsNone(result["median"]["cli_sampled_peak_rss_kib"])
        self.assertEqual((result["successful_runs"], result["failed_runs"]), (1, 1))
        self.assertFalse(result["complete"])
        failed["warmup"] = True
        self.assertFalse(bench.summarize([good, failed], ["v3/proto"], 1)["v3/proto"]["complete"])
        failed["warmup"] = False
        self.assertIsNone(bench.summarize([failed], ["v3/proto"], 1)["v3/proto"]["median"])


if __name__ == "__main__":
    unittest.main()
