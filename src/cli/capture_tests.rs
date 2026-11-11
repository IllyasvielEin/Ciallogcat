//! Writer publication must match complete flushed packet boundaries.
use super::*;

#[test]
fn worker_config_requires_explicit_current_fields() {
    let full = json!({
        "adb":"adb", "serial":"TEST-USB", "duration_secs":600,
        "max_bytes":67108864, "max_entries":10000000,
        "transport":"text",
        "scope":CaptureScope::default(), "buffers":LogBuffer::defaults()
    });
    let config: Config = serde_json::from_value(full.clone()).unwrap();
    assert_eq!(config.scope.logcat_args(), ["-T", "1"]);
    assert_eq!(config.buffers, LogBuffer::defaults());
    assert_eq!(config.transport, adb::LogTransport::Text);
    for field in ["transport", "scope", "buffers"] {
        let mut missing = full.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<Config>(missing).is_err(),
            "{field}"
        );
    }
    let mut obsolete = full;
    obsolete["record_schema_version"] = 3.into();
    assert!(serde_json::from_value::<Config>(obsolete).is_err());
}

#[test]
fn native_published_packet_is_visible_with_a_buffered_writer() {
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let contents = Arc::new(Mutex::new(Vec::new()));
    let sink = Sink(Arc::clone(&contents));
    let visible = Arc::new(Mutex::new(Progress::default()));
    let published = Arc::clone(&visible);
    let (tx, rx) = packets();
    let (notify, notifications) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        write_packets(io::BufWriter::new(sink), rx, &published, &notify)
    });
    let line = b"{\"seq\":1,\"message\":\"ready\"}\n";
    tx.send(Packet {
        bytes: line.to_vec(),
        seq: Some(1),
    })
    .unwrap();
    notifications.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(&*contents.lock().unwrap(), line);
    assert_eq!(
        *visible.lock().unwrap(),
        Progress {
            entries: 1,
            bytes: line.len() as u64,
            seq: 1
        }
    );
    drop(tx);
    worker.join().unwrap().unwrap();
}

fn packets() -> (mpsc::Sender<Packet>, mpsc::Receiver<Packet>) {
    mpsc::channel()
}

#[test]
fn native_writer_counts_header_and_footer_bytes_without_advancing_sequence() {
    let (tx, rx) = packets();
    let header = b"header\n";
    let entry = b"record\n";
    let footer = b"footer\n";
    for (bytes, seq) in [(header, None), (entry, Some(9)), (footer, None)] {
        tx.send(Packet {
            bytes: bytes.to_vec(),
            seq,
        })
        .unwrap();
    }
    drop(tx);
    let visible = Mutex::new(Progress::default());
    // Deliberately never drain notifications: an already queued hint must not
    // block the writer or become part of the durability/visibility contract.
    let (notify, _notifications) = mpsc::sync_channel(1);
    let mut contents = Vec::new();
    write_packets(&mut contents, rx, &visible, &notify).unwrap();
    assert_eq!(
        contents,
        [header.as_slice(), entry.as_slice(), footer.as_slice()].concat()
    );
    assert_eq!(
        *visible.lock().unwrap(),
        Progress {
            entries: 1,
            bytes: contents.len() as u64,
            seq: 9
        }
    );
}

#[test]
fn native_partial_packet_failure_preserves_both_published_boundaries() {
    struct FailAfter(usize);
    impl Write for FailAfter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::other("full disk"));
            }
            let count = self.0.min(bytes.len());
            self.0 -= count;
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (tx, rx) = packets();
    tx.send(Packet {
        bytes: b"header\n".to_vec(),
        seq: None,
    })
    .unwrap();
    tx.send(Packet {
        bytes: b"first\n".to_vec(),
        seq: Some(4),
    })
    .unwrap();
    tx.send(Packet {
        bytes: b"partial\n".to_vec(),
        seq: Some(5),
    })
    .unwrap();
    drop(tx);
    let visible = Mutex::new(Progress::default());
    let (notify, _notifications) = mpsc::sync_channel(1);
    let error = write_packets(FailAfter(7 + 6 + 3), rx, &visible, &notify).unwrap_err();
    assert!(error.to_string().contains("full disk"));
    assert_eq!(
        *visible.lock().unwrap(),
        Progress {
            entries: 1,
            bytes: 13,
            seq: 4
        }
    );
}

#[test]
fn native_flush_failure_does_not_publish_a_whole_written_packet() {
    struct FailSecondFlush(usize);
    impl Write for FailSecondFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0 += 1;
            if self.0 == 2 {
                Err(io::Error::other("flush failed"))
            } else {
                Ok(())
            }
        }
    }
    let (tx, rx) = packets();
    tx.send(Packet {
        bytes: b"header\n".to_vec(),
        seq: None,
    })
    .unwrap();
    tx.send(Packet {
        bytes: b"entry\n".to_vec(),
        seq: Some(1),
    })
    .unwrap();
    drop(tx);
    let visible = Mutex::new(Progress::default());
    let (notify, _notifications) = mpsc::sync_channel(1);
    assert!(write_packets(FailSecondFlush(0), rx, &visible, &notify).is_err());
    assert_eq!(
        *visible.lock().unwrap(),
        Progress {
            entries: 0,
            bytes: 7,
            seq: 0
        }
    );
}

#[test]
fn native_pair_is_published_only_after_flush_and_notifies_a_quiet_consumer() {
    struct Gate {
        written: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Write for Gate {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.written.send(()).map_err(io::Error::other)?;
            self.release.recv().map_err(io::Error::other)
        }
    }
    let (tx, rx) = packets();
    let visible = Arc::new(Mutex::new(Progress::default()));
    let published = Arc::clone(&visible);
    let (written, awaiting_flush) = mpsc::sync_channel(1);
    let (release, released) = mpsc::channel();
    let (notify, notifications) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        write_packets(
            Gate {
                written,
                release: released,
            },
            rx,
            &published,
            &notify,
        )
    });
    tx.send(Packet {
        bytes: b"entry\n".to_vec(),
        seq: Some(17),
    })
    .unwrap();
    awaiting_flush.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(*visible.lock().unwrap(), Progress::default());
    assert!(notifications.try_recv().is_err());
    release.send(()).unwrap();
    notifications.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        *visible.lock().unwrap(),
        Progress {
            entries: 1,
            bytes: 6,
            seq: 17
        }
    );
    drop(tx);
    // The final flush does not introduce a new packet or a new watermark.
    awaiting_flush.recv_timeout(Duration::from_secs(2)).unwrap();
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert!(notifications.try_recv().is_err());
}

fn stop_fixture() -> (Session, StreamEncoder) {
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let header = DocumentHeader::live(session.id(), "TEST".into(), LogBuffer::defaults());
    (session, StreamEncoder::new(header).unwrap())
}

fn pending_record(
    session: &mut Session,
    encoder: &StreamEncoder,
    message: &str,
) -> (Packet, StreamEncoder) {
    use ciallogcat::model::{Level, LogEntry};
    let mut entry = LogEntry::new("11-05 12:00:00.000", 42, 43, "Stop", Level::Info, message);
    entry.provenance = session.next_record(Some(1234));
    let mut next = encoder.clone();
    let bytes = next.record_bytes(&entry).unwrap();
    (
        Packet {
            bytes,
            seq: entry.provenance.map(|p| p.seq),
        },
        next,
    )
}

#[test]
fn stopping_keeps_pending_records_in_order_and_finalizes_the_admitted_prefix() {
    let (mut session, mut encoder) = stop_fixture();
    let mut bytes = encoder.header_bytes().unwrap();
    let mut admitted_bytes = bytes.len() as u64;
    let mut admitted_entries = 0;
    let (tx, rx) = mpsc::sync_channel(1);
    let mut pending = Some(pending_record(&mut session, &encoder, "before stop"));
    submit_pending(
        true,
        &mut pending,
        &mut encoder,
        &tx,
        &mut admitted_bytes,
        &mut admitted_entries,
    )
    .unwrap();
    let mut pending = Some(pending_record(&mut session, &encoder, "tail outcome"));
    let now = Instant::now();
    let mut stop = StopState::new("interrupted", now);
    // Writer is full when cancellation arrives. Neither the pending record nor
    // its next encoder state may be dropped merely because stopping began.
    submit_pending(
        stop.admit(now, false, false),
        &mut pending,
        &mut encoder,
        &tx,
        &mut admitted_bytes,
        &mut admitted_entries,
    )
    .unwrap();
    assert!(pending.is_some());
    assert_eq!(admitted_entries, 1);
    bytes.extend(rx.recv().unwrap().bytes);
    submit_pending(
        stop.admit(now + Duration::from_millis(50), false, false),
        &mut pending,
        &mut encoder,
        &tx,
        &mut admitted_bytes,
        &mut admitted_entries,
    )
    .unwrap();
    assert!(pending.is_none());
    bytes.extend(rx.recv().unwrap().bytes);
    assert_eq!(admitted_entries, 2);
    assert_eq!(admitted_bytes, bytes.len() as u64);
    bytes.extend(encoder.finish_bytes(CaptureStatus::default()).unwrap());
    let document =
        ciallogcat::document::load(bytes.as_slice(), 1024 * 1024, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        document
            .entries
            .iter()
            .map(|e| e.message.as_ref())
            .collect::<Vec<&str>>(),
        ["before stop", "tail outcome"]
    );
    assert_eq!(document.end.record_count, 2);
    let report = stop.finish(true, true, true, false, false);
    assert_eq!(report.trigger, "interrupted");
    assert_eq!(report.drain, DrainResult::Completed);
    assert_eq!(report.footer_write, Confirmation::Confirmed);
}

#[test]
fn admission_deadline_releases_pending_while_writer_and_backend_are_full() {
    let (mut session, mut encoder) = stop_fixture();
    let mut admitted_bytes = 0;
    let mut admitted_entries = 0;
    let (tx, _blocked_writer) = mpsc::sync_channel(1);
    let mut pending = Some(pending_record(&mut session, &encoder, "queued"));
    submit_pending(
        true,
        &mut pending,
        &mut encoder,
        &tx,
        &mut admitted_bytes,
        &mut admitted_entries,
    )
    .unwrap();
    pending = Some(pending_record(&mut session, &encoder, "pending"));
    let (backend_tx, backend_rx) = mpsc::sync_channel(1);
    backend_tx.send(LogcatEvent::Stderr("full".into())).unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let reaper = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        backend_tx
            .send(LogcatEvent::Exited {
                code: None,
                error: None,
                incomplete_proto_frame: false,
            })
            .unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let now = Instant::now();
    let mut stop = StopState::new("stopped", now);
    let admission_deadline = stop.admission_until.unwrap();
    assert!(stop.admit(now, false, false));
    submit_pending(
        stop.admit(admission_deadline, false, false),
        &mut pending,
        &mut encoder,
        &tx,
        &mut admitted_bytes,
        &mut admitted_entries,
    )
    .unwrap();
    assert!(pending.is_none());
    assert_eq!(admitted_entries, 1);
    assert_eq!(stop.result, DrainResult::Deadline);
    assert!(matches!(
        backend_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        LogcatEvent::Stderr(_)
    ));
    assert!(matches!(
        backend_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        LogcatEvent::Exited { .. }
    ));
    reaper.join().unwrap();
    // Subsequent events cannot reopen admission or move either deadline.
    assert!(!stop.admit(now, false, false));
    assert_eq!(stop.deadline, now + Duration::from_secs(5));
    let footer: serde_json::Value =
        serde_json::from_slice(&encoder.finish_bytes(CaptureStatus::default()).unwrap()).unwrap();
    assert_eq!(footer["record_count"], 1);
    assert_eq!(footer["last_seq"], 1);
}

#[test]
fn hard_limits_and_failures_close_stop_admission_without_replacing_the_trigger() {
    let now = Instant::now();
    for reason in ["max_entries", "max_bytes", "snapshot_complete", "failed"] {
        assert!(!StopState::new(reason, now).admit(now, false, false));
    }
    for reason in ["stopped", "interrupted", "duration_limit"] {
        let mut stop = StopState::new(reason, now);
        assert!(stop.admit(now, false, false));
        assert!(!stop.admit(now, false, true));
        assert_eq!(stop.result, DrainResult::Budget);
        assert!(!stop.admit(now, false, false));
        assert!(!stop.admit(now, true, true));
        let report = stop.finish(true, false, false, true, true);
        assert_eq!(report.trigger, reason);
        assert_eq!(report.drain, DrainResult::Failed);
        assert_eq!(report.footer_write, Confirmation::Unconfirmed);
    }
}

#[test]
fn footer_flush_failure_never_confirms_finalization_even_when_packet_was_published() {
    struct FailFinalFlush(usize);
    impl Write for FailFinalFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0 += 1;
            if self.0 == 3 {
                Err(io::Error::other("final flush failed"))
            } else {
                Ok(())
            }
        }
    }
    let (tx, rx) = packets();
    for bytes in [b"header\n".to_vec(), b"footer\n".to_vec()] {
        tx.send(Packet { bytes, seq: None }).unwrap();
    }
    drop(tx);
    let visible = Mutex::new(Progress::default());
    let (notify, _notifications) = mpsc::sync_channel(1);
    let result = write_packets(FailFinalFlush(0), rx, &visible, &notify);
    assert!(result.is_err());
    assert_eq!(visible.lock().unwrap().bytes, 14);
    let report =
        StopState::new("stopped", Instant::now()).finish(true, true, result.is_ok(), true, false);
    assert_eq!(report.cleanup, Confirmation::Confirmed);
    assert_eq!(report.footer_write, Confirmation::Unconfirmed);
    assert_eq!(report.drain, DrainResult::Failed);
    let timed_out =
        StopState::new("interrupted", Instant::now()).finish(true, true, false, true, false);
    assert_eq!(timed_out.footer_write, Confirmation::Unconfirmed);
}

struct RuntimeFixture(PathBuf);
impl RuntimeFixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("ciallogcat-stop-{}", Session::new().id()));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
    fn config(&self) -> Config {
        Config {
            adb: self.0.join("unused-injected-adb"),
            serial: "TEST".into(),
            duration_secs: 30,
            max_bytes: 1024 * 1024,
            max_entries: 1000,
            transport: adb::LogTransport::Text,
            scope: CaptureScope::default(),
            buffers: LogBuffer::defaults(),
        }
    }
    fn status(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.0.join("status.json")).unwrap()).unwrap()
    }
}
impl Drop for RuntimeFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct TestHandle(mpsc::Sender<()>);
impl CaptureHandle for TestHandle {
    fn stop(&mut self) {
        let _ = self.0.send(());
    }
}

fn runtime_entry(index: usize) -> LogcatEvent {
    use ciallogcat::model::{Level, LogEntry};
    LogcatEvent::Entry(
        LogEntry::new(
            "11-05 12:00:00.000",
            42,
            43,
            "Stop",
            Level::Info,
            format!("record-{index}"),
        )
        .into(),
    )
}

#[test]
fn capture_loop_recovers_from_full_writer_and_backend_and_honors_stop_limits() {
    struct GatedWriter {
        contents: Arc<Mutex<Vec<u8>>>,
        flushes: usize,
        blocked: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Write for GatedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.contents.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == 2 {
                self.blocked.send(()).map_err(io::Error::other)?;
                self.release
                    .recv_timeout(Duration::from_secs(8))
                    .map_err(io::Error::other)?;
            }
            Ok(())
        }
    }
    for scenario in ["deadline", "entries", "bytes", "dump"] {
        let hit_limit = matches!(scenario, "entries" | "bytes");
        let fixture = RuntimeFixture::new();
        let mut config = fixture.config();
        if scenario == "entries" {
            config.max_entries = 3;
        } else if scenario == "bytes" {
            config.max_bytes = 8192;
        } else if scenario == "dump" {
            config.scope = CaptureScope::Dump { start: None };
        }
        let max_bytes = config.max_bytes;
        let contents = Arc::new(Mutex::new(Vec::new()));
        let interrupted = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&interrupted);
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let (progress_tx, progress_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let dir = fixture.0.clone();
        let writer = GatedWriter {
            contents: Arc::clone(&contents),
            flushes: 0,
            blocked: blocked_tx,
            release: release_rx,
        };
        let worker = std::thread::spawn(move || {
            let mut channels = Some((continue_rx, progress_tx, stop_tx));
            let result = run_capture(
                &config,
                Some(&dir),
                Box::new(writer),
                interrupted,
                move |id, _, events, _, _| {
                    let (continue_rx, progress_tx, stop_tx) =
                        channels.take().expect("stop must not reconnect");
                    std::thread::spawn(move || {
                        let send = |event| {
                            events
                                .send(BackendEvent::Logcat {
                                    session_id: id,
                                    event,
                                })
                                .unwrap()
                        };
                        send(LogcatEvent::Started);
                        for index in 1..=3 {
                            send(runtime_entry(index));
                        }
                        continue_rx.recv_timeout(Duration::from_secs(8)).unwrap();
                        for index in 4..=132 {
                            send(runtime_entry(index));
                            if index == 131 {
                                progress_tx.send("full").unwrap();
                            }
                        }
                        send(LogcatEvent::Exited {
                            code: Some(0),
                            error: None,
                            incomplete_proto_frame: false,
                        });
                        progress_tx.send("exited").unwrap();
                    });
                    Ok(TestHandle(stop_tx))
                },
                |_| false,
            );
            done_tx.send(result).unwrap();
        });
        blocked_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        if hit_limit {
            cancel.store(true, Ordering::Release);
            stop_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            // Cancellation is latched before writer progress can admit record 3.
            release_tx.send(()).unwrap();
            continue_tx.send(()).unwrap();
        } else {
            continue_tx.send(()).unwrap();
            assert_eq!(
                progress_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
                "full"
            );
            // Exactly 3 entries were consumed (writer in flight, writer slot,
            // pending); the other 128 fill the real backend channel. Entry 132
            // and Exited cannot advance until the real loop closes admission.
            cancel.store(true, Ordering::Release);
            stop_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            assert_eq!(
                progress_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
                "exited"
            );
            release_tx.send(()).unwrap();
        }
        let error = done_rx
            .recv_timeout(Duration::from_secs(8))
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.exit, 130);
        worker.join().unwrap();
        let status = fixture.status();
        assert_eq!(status["shutdown"]["trigger"], "interrupted");
        assert_eq!(status["shutdown"]["cleanup"], "confirmed");
        assert_eq!(status["shutdown"]["footer_write"], "confirmed");
        assert_eq!(
            status["shutdown"]["drain"],
            if hit_limit { "budget" } else { "deadline" }
        );
        let bytes = contents.lock().unwrap();
        let document =
            ciallogcat::document::load(bytes.as_slice(), 1024 * 1024, &AtomicBool::new(false))
                .unwrap();
        let expected = match scenario {
            "entries" => 3,
            "bytes" => {
                assert!((3..132).contains(&document.entries.len()));
                document.entries.len()
            }
            _ => 2,
        };
        assert_eq!(document.entries.len(), expected);
        assert!(bytes.len() as u64 <= max_bytes);
        for (index, entry) in document.entries.iter().enumerate() {
            assert_eq!(&*entry.message, format!("record-{}", index + 1));
        }
        assert_eq!(status["entries"], expected);
        assert_eq!(status["bytes"], bytes.len());
        assert_eq!(status["discarded_entries"], 132 - expected);
        assert_eq!(document.end.capture.known_gaps[0]["count"], 132 - expected);
        assert_eq!(status["omitted_by_limit"], hit_limit);
    }
}

#[test]
fn capture_finalization_reports_actual_flush_failure_and_timeout_before_cancellation() {
    struct FooterWriter {
        footer: bool,
        release: mpsc::Receiver<()>,
        dropped: mpsc::Sender<()>,
        cancel: Arc<AtomicBool>,
        timeout: bool,
    }
    impl Write for FooterWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            self.footer = value["type"] == "artifact_end";
            if self.footer {
                self.cancel.store(true, Ordering::Release);
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.footer {
                if self.timeout {
                    self.release
                        .recv_timeout(Duration::from_secs(6))
                        .map_err(io::Error::other)?;
                    self.footer = false;
                } else {
                    return Err(io::Error::other("footer flush failed"));
                }
            }
            Ok(())
        }
    }
    impl Drop for FooterWriter {
        fn drop(&mut self) {
            let _ = self.dropped.send(());
        }
    }
    for timeout in [false, true] {
        let fixture = RuntimeFixture::new();
        let mut config = fixture.config();
        config.scope = CaptureScope::Dump { start: None };
        let cancelled = Arc::new(AtomicBool::new(false));
        let (release_tx, release_rx) = mpsc::channel();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let writer = FooterWriter {
            footer: false,
            release: release_rx,
            dropped: dropped_tx,
            cancel: Arc::clone(&cancelled),
            timeout,
        };
        let started = Instant::now();
        let result = run_capture(
            &config,
            Some(&fixture.0),
            Box::new(writer),
            cancelled,
            |id, _, events, _, _| {
                std::thread::spawn(move || {
                    for event in [
                        LogcatEvent::Started,
                        runtime_entry(1),
                        LogcatEvent::Exited {
                            code: Some(0),
                            error: None,
                            incomplete_proto_frame: false,
                        },
                    ] {
                        events
                            .send(BackendEvent::Logcat {
                                session_id: id,
                                event,
                            })
                            .unwrap();
                    }
                });
                let (stop_tx, _stop_rx) = mpsc::channel();
                Ok(TestHandle(stop_tx))
            },
            |_| false,
        );
        let error = result.err().unwrap();
        assert_eq!(error.exit, 5);
        assert_eq!(
            error.code,
            if timeout {
                "output_backpressure"
            } else {
                "output_error"
            }
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        let status = fixture.status();
        assert_eq!(status["shutdown"]["trigger"], "snapshot_complete");
        assert_eq!(status["shutdown"]["cleanup"], "confirmed");
        assert_eq!(status["shutdown"]["footer_write"], "unconfirmed");
        assert_eq!(status["shutdown"]["drain"], "failed");
        if timeout {
            release_tx.send(()).unwrap();
        }
        dropped_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    }
}
