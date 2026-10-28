//! Synthetic protocol and subprocess checks; these are not Android-device tests.
use super::*;
use crate::model::{LogEntryKind, ProcessSource};
use crate::protobuf::DecodedEntry;

const PAYLOAD: &[u8] =
    b"\x08\x01\x10\x02\x18\x08\x20\x2a\x28\x04\x30\x05\x3a\x04Tag\0\x42\x04m\0\xff\n\x4a\x03app";

#[test]
fn proto_adapter_preserves_bytes_and_uses_the_device_cache_label() {
    let entry = proto_entry(DecodedEntry::decode_payload(PAYLOAD.to_vec()).unwrap());
    assert_eq!(entry.kind, LogEntryKind::Log);
    assert_eq!(entry.level, Level::Fatal);
    assert_eq!(entry.time.as_ref(), "1.000000002");
    assert_eq!(entry.tag.as_ref(), "Tag");
    assert_eq!(entry.message.as_ref(), "m\0\u{fffd}\n");
    assert_eq!(entry.process.as_deref(), Some("app"));
    assert_eq!(
        entry.process_source,
        Some(ProcessSource::DeviceProcessCache)
    );
    assert_eq!(
        entry.text_loss,
        Some(TextLoss {
            truncated: false,
            invalid_utf8_replaced: true
        })
    );
    let evidence = entry.proto.unwrap();
    assert_eq!(evidence.raw_payload, PAYLOAD);
    assert_eq!(evidence.tag_bytes, b"Tag\0");
    assert_eq!(evidence.message_bytes, b"m\0\xff\n");
}

#[test]
fn proto_adapter_never_invents_typed_logs_for_unknown_or_invalid_fields() {
    for suffix in [
        &b"\x18\x07"[..],                 // 7 is unknown in this protobuf schema, not Fatal.
        &b"\x18\x09"[..],                 // Silent is not an emitted display level.
        &b"\x28\x80\x80\x80\x80\x10"[..], // PID > u32.
        &b"\x30\x80\x80\x80\x80\x10"[..], // TID > u32.
        &b"\x10\x80\x94\xeb\xdc\x03"[..], // 1_000_000_000 nanos.
    ] {
        let mut payload = PAYLOAD.to_vec();
        payload.extend_from_slice(suffix);
        let entry = proto_entry(DecodedEntry::decode_payload(payload.clone()).unwrap());
        assert_eq!(entry.kind, LogEntryKind::Unparsed);
        assert_eq!(entry.process, None);
        assert_eq!(entry.process_source, None);
        assert_eq!(entry.proto.unwrap().raw_payload, payload);
    }
    for payload in [vec![], vec![0x98, 0x06, 0x01]] {
        let entry = proto_entry(DecodedEntry::decode_payload(payload.clone()).unwrap());
        assert_eq!(entry.kind, LogEntryKind::Unparsed);
        assert!(entry.message.is_empty());
        assert_eq!(entry.proto.unwrap().raw_payload, payload);
    }
}

#[test]
fn proto_stream_reports_frame_error_without_publishing_partial_record() {
    let mut stream = (PAYLOAD.len() as u64).to_le_bytes().to_vec();
    stream.extend_from_slice(PAYLOAD);
    stream.extend_from_slice(&5u64.to_le_bytes());
    stream.push(8);
    let mut entries = Vec::new();
    let error = read_proto_stream(stream.as_slice(), |entry| entries.push(entry)).unwrap_err();
    assert_eq!(entries.len(), 1);
    assert!(error.to_string().contains("truncated"));
}

#[test]
fn proto_remote_command_keeps_serial_local_and_words_fixed() {
    let args = proto_args("serial; echo not-a-command", &[LogBuffer::Main]);
    assert_eq!(
        args,
        [
            "-s",
            "serial; echo not-a-command",
            "shell",
            "-T",
            "-n",
            "exec logcat --proto -v threadtime -T 1 -b main '*:V'"
        ]
    );
}

#[test]
fn device_scope_command_matrix_preserves_default_wrappers_and_buffer_normalization() {
    for (dump, tail, since, expected) in [
        (false, None, None, vec!["-T", "1"]),
        (false, Some("37"), None, vec!["-T", "37"]),
        (
            false,
            None,
            Some("10-23 12:30:00.123"),
            vec!["-T", "10-23 12:30:00.123"],
        ),
        (true, None, None, vec!["-d"]),
        (true, Some("37"), None, vec!["-t", "37"]),
        (true, None, Some("1792742400.0"), vec!["-t", "1792742400.0"]),
    ] {
        let scope = CaptureScope::from_inputs(dump, tail, since).unwrap();
        let args = scoped_logcat_args(
            "serial; echo harmless",
            &[LogBuffer::Radio, LogBuffer::All],
            &scope,
        );
        assert_eq!(
            &args[..5],
            ["-s", "serial; echo harmless", "logcat", "-v", "threadtime"]
        );
        assert_eq!(&args[5..5 + expected.len()], expected);
        assert_eq!(&args[5 + expected.len()..], ["-b", "all"]);
        let proto = scoped_proto_args("serial; echo harmless", &[], &scope);
        assert_eq!(
            &proto[..5],
            ["-s", "serial; echo harmless", "shell", "-T", "-n"]
        );
        assert!(!proto[5].contains("serial"));
        assert!(proto[5].ends_with("-b main -b system -b crash '*:V'"));
    }
}

#[cfg(unix)]
#[test]
fn device_calendar_time_survives_real_remote_shell_word_parsing() {
    for dump in [false, true] {
        let scope =
            CaptureScope::from_inputs(dump, None, Some("2024-02-29 23:59:59.123456789")).unwrap();
        let args = scoped_proto_args("outside;remote", &[LogBuffer::Main], &scope);
        // Exercise an actual POSIX shell's argv interpretation without running
        // adb or logcat. The remote command's argument tail is unchanged.
        let words = args[5].strip_prefix("exec logcat ").unwrap();
        let output = Command::new("/bin/sh")
            .args(["-c", &format!("set -- {words}; printf '%s\\0' \"$@\"")])
            .output()
            .unwrap();
        assert!(output.status.success());
        let words: Vec<_> = output
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(
            words,
            [
                b"--proto".as_slice(),
                b"-v",
                b"threadtime",
                if dump { b"-t" } else { b"-T" },
                b"2024-02-29 23:59:59.123456789",
                b"-b",
                b"main",
                b"*:V"
            ]
        );
    }
}

#[test]
fn proto_truncation_classification_uses_typed_cause_not_error_text() {
    for (bytes, expected) in [
        (vec![1], true),
        ([1u64.to_le_bytes().as_slice(), &[0]].concat(), false),
    ] {
        let failure = finish_logcat_reader(
            std::thread::spawn(move || read_proto_stream(bytes.as_slice(), |_| {})),
            "stdout",
        )
        .unwrap();
        assert_eq!(failure.incomplete_proto_frame, expected);
    }
    let failure = finish_logcat_reader(
        std::thread::spawn(|| Err(io::Error::other("unrelated truncated diagnostic"))),
        "stderr",
    )
    .unwrap();
    assert!(!failure.incomplete_proto_frame);
    let mut error = Some("a truncated protobuf frame".into());
    let mut incomplete = true;
    record_reader_failure(&mut error, &mut incomplete, failure);
    assert!(
        !incomplete,
        "other reader failures must never be downgraded on stop"
    );
}

#[test]
fn query_failure_preview_bounds_invalid_utf8_expansion_and_keeps_a_loss_notice() {
    for bytes in [vec![0xff; 2 * 1024 * 1024], "雪".repeat(4096).into_bytes()] {
        let preview = bounded_query_diagnostic(&bytes);
        assert!(preview.len() <= MAX_STDERR_EVENT_BYTES);
        assert!(preview.ends_with(STDERR_TRUNCATION_NOTICE));
        assert!(std::str::from_utf8(preview.as_bytes()).is_ok());
    }
    assert_eq!(
        bounded_query_diagnostic(b"  Permission denied\n"),
        "Permission denied"
    );
    assert_eq!(bounded_query_diagnostic(b" \t\n"), "");
}

#[cfg(target_os = "linux")]
#[test]
fn proto_startup_deadline_and_cancellation_reap_query_without_spawning_stream() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = env::temp_dir().join(format!(
        "ciallogcat-proto-startup-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&directory).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(directory.clone());
    let fixture = directory.join("fixture.py");
    fs::write(&fixture, include_bytes!("../tests/fixtures/fake_adb.py")).unwrap();
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    for mode in ["deadline", "cancel", "already_cancelled", "already_expired"] {
        let trace = directory.join(format!("{mode}.jsonl"));
        let adb = directory.join(format!("{mode}.sh"));
        fs::write(&adb, format!("#!/bin/sh\nCIALLOGCAT_FAKE_SCENARIO=query_hang CIALLOGCAT_FAKE_TRACE={} exec python3 {} \"$@\"\n", quote(&trace), quote(&fixture))).unwrap();
        fs::set_permissions(&adb, fs::Permissions::from_mode(0o700)).unwrap();
        let start = Instant::now();
        let cancelled = || {
            mode == "already_cancelled"
                || (mode == "cancel" && start.elapsed() >= Duration::from_millis(120))
        };
        let deadline = match mode {
            "already_expired" => Some(start),
            "deadline" => Some(start + Duration::from_millis(120)),
            _ => None,
        };
        let (tx, rx) = mpsc::sync_channel(16);
        let error = start_logcat_with_options(
            &adb,
            "TEST-USB",
            &[LogBuffer::Main],
            3,
            tx,
            WakeSignal::default(),
            LogcatStartOptions {
                transport: LogTransport::Proto,
                deadline,
                cancelled: Some(&cancelled),
            },
        )
        .err()
        .expect("bounded startup should fail");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{mode} waited for the five-second query timeout"
        );
        assert!(
            error.contains(if mode.contains("cancel") {
                "cancelled"
            } else {
                "deadline"
            }),
            "{error}"
        );
        assert!(
            rx.try_recv().is_err(),
            "no Started or Entry before capability success"
        );
        if mode.starts_with("already") {
            assert!(!trace.exists());
        } else {
            let calls: Vec<serde_json::Value> = fs::read_to_string(&trace)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0]["args"],
                serde_json::json!(["-s", "TEST-USB", "features"])
            );
            let pid = calls[0]["pid"].as_u64().unwrap();
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "query child was not reaped"
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn proto_subprocess_capability_errors_and_frames_never_fall_back() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        env::temp_dir().join(format!("ciallogcat-proto-{}-{unique}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(directory.clone());
    let fixture = directory.join("fixture.py");
    fs::write(&fixture, include_bytes!("../tests/fixtures/fake_adb.py")).unwrap();
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    for scenario in [
        "proto_normal",
        "proto_empty",
        "proto_unknown_only",
        "proto_zero_records",
        "proto_unknown_priority",
        "proto_malformed",
        "proto_truncated",
        "proto_oversized",
        "proto_unsupported",
        "proto_permission",
        "proto_no_shell_v2",
        "proto_feature_error",
        "proto_feature_error_large",
        "proto_partial_then_idle",
    ] {
        let trace = directory.join(format!("{scenario}.jsonl"));
        let adb = directory.join(format!("{scenario}.sh"));
        fs::write(&adb, format!("#!/bin/sh\nCIALLOGCAT_FAKE_SCENARIO='{scenario}' CIALLOGCAT_FAKE_TRACE={} exec python3 {} \"$@\"\n", quote(&trace), quote(&fixture))).unwrap();
        fs::set_permissions(&adb, fs::Permissions::from_mode(0o700)).unwrap();
        let (tx, rx) = mpsc::sync_channel(16);
        let result = start_logcat_with_transport(
            &adb,
            "TEST-USB",
            &[LogBuffer::Main],
            9,
            tx,
            WakeSignal::default(),
            LogTransport::Proto,
        );
        if matches!(
            scenario,
            "proto_no_shell_v2" | "proto_feature_error" | "proto_feature_error_large"
        ) {
            let error = result
                .err()
                .expect("capability failure must prevent spawning stream");
            assert!(
                error.contains(if scenario.starts_with("proto_feature_error") {
                    "features query failed"
                } else {
                    "requires target ADB shell_v2"
                }),
                "{error}"
            );
            if scenario == "proto_feature_error_large" {
                assert!(error.len() < 4300);
                assert!(error.ends_with(STDERR_TRUNCATION_NOTICE));
            }
        } else {
            let mut handle = result.unwrap();
            let mut entries = Vec::new();
            let mut diagnostics = Vec::new();
            let (code, error, incomplete) = loop {
                match rx.recv_timeout(Duration::from_secs(3)).expect(scenario) {
                    BackendEvent::Logcat {
                        event: LogcatEvent::Entry(entry),
                        ..
                    } => {
                        entries.push(entry.into_inner());
                        if scenario == "proto_partial_then_idle" {
                            handle.stop();
                        }
                    }
                    BackendEvent::Logcat {
                        event: LogcatEvent::Stderr(line),
                        ..
                    } => diagnostics.push(line),
                    BackendEvent::Logcat {
                        event:
                            LogcatEvent::Exited {
                                code,
                                error,
                                incomplete_proto_frame,
                            },
                        ..
                    } => break (code, error, incomplete_proto_frame),
                    _ => {}
                }
            };
            handle.stop();
            assert_eq!(
                incomplete,
                matches!(scenario, "proto_truncated" | "proto_partial_then_idle"),
                "{scenario}"
            );
            match scenario {
                "proto_partial_then_idle" => {
                    assert_eq!(entries.len(), 1);
                    assert!(error.as_ref().unwrap().contains("truncated"));
                    let records: Vec<serde_json::Value> = fs::read_to_string(&trace)
                        .unwrap()
                        .lines()
                        .map(|line| serde_json::from_str(line).unwrap())
                        .collect();
                    let pid = records
                        .iter()
                        .find(|row| row["args"][2] == "shell")
                        .unwrap()["pid"]
                        .as_u64()
                        .unwrap();
                    assert!(
                        !Path::new(&format!("/proc/{pid}")).exists(),
                        "Exited must confirm child reaping"
                    );
                }
                "proto_normal" => {
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0].kind, LogEntryKind::Log);
                    assert_eq!(entries[0].proto.as_ref().unwrap().raw_payload, PAYLOAD);
                }
                "proto_empty" | "proto_unknown_only" | "proto_unknown_priority" => {
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0].kind, LogEntryKind::Unparsed);
                }
                "proto_zero_records" => {
                    assert!(entries.is_empty());
                    assert_eq!(code, Some(0));
                    assert!(error.is_none());
                }
                "proto_unsupported" | "proto_permission" => {
                    assert!(entries.is_empty());
                    assert_eq!(code, Some(1));
                    assert!(
                        diagnostics
                            .join(" ")
                            .contains(if scenario == "proto_permission" {
                                "Permission denied"
                            } else {
                                "unrecognized option"
                            })
                    );
                }
                _ => {
                    assert!(entries.is_empty());
                    assert!(error.is_some(), "{scenario}");
                }
            }
        }
        let calls: Vec<serde_json::Value> = fs::read_to_string(&trace)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|record: &serde_json::Value| record["event"] == "invocation")
            .collect();
        assert_eq!(
            calls[0]["args"],
            serde_json::json!(["-s", "TEST-USB", "features"])
        );
        assert_eq!(
            calls.len(),
            if matches!(
                scenario,
                "proto_no_shell_v2" | "proto_feature_error" | "proto_feature_error_large"
            ) {
                1
            } else {
                2
            }
        );
        if calls.len() == 2 {
            assert_eq!(
                calls[1]["args"],
                serde_json::json!(proto_args("TEST-USB", &[LogBuffer::Main]))
            );
        }
    }
}
