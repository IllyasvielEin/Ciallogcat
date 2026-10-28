//! Bounded native documents. Reconnects share one identity and one total budget.
use super::*;
use ciallogcat::capture_controller::{
    Command as Action, Controller, Intent, Status as Phase, Target,
};
use ciallogcat::document::{CaptureStatus, DocumentHeader, NativeFormat, StreamEncoder};
use ciallogcat::session::Session;

// Header/footer count toward the file budget. Reserve enough for our fixed,
// aggregate footer (never unbounded stderr or a list of every reconnect).
const FOOTER_RESERVE: u64 = 4096;
const FOOTER_DIAGNOSTIC_JSON_BYTES: usize = 1024;

fn footer_diagnostic(message: &str) -> (&str, bool) {
    // Count the encoded JSON string, including its quotes. Control characters
    // can occupy six bytes each even when their raw UTF-8 representation is one.
    let mut encoded = 2;
    let mut end = 0;
    for (index, character) in message.char_indices() {
        let bytes = match character {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            '\u{0}'..='\u{1f}' => 6,
            _ => character.len_utf8(),
        };
        if encoded + bytes > FOOTER_DIAGNOSTIC_JSON_BYTES {
            break;
        }
        encoded += bytes;
        end = index + character.len_utf8();
    }
    (&message[..end], end < message.len())
}

#[cfg(test)]
#[path = "capture_v2_tests.rs"]
mod tests;

struct Packet {
    bytes: Vec<u8>,
    seq: Option<u64>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Progress {
    entries: u64,
    bytes: u64,
    seq: u64,
}
fn write_packets(
    mut writer: impl Write,
    rx: mpsc::Receiver<Packet>,
    visible: &Mutex<Progress>,
    notify: &mpsc::SyncSender<()>,
) -> io::Result<()> {
    let mut progress = Progress::default();
    for packet in rx {
        writer.write_all(&packet.bytes)?;
        writer.flush()?;
        progress.bytes += packet.bytes.len() as u64;
        if let Some(seq) = packet.seq {
            progress.entries += 1;
            progress.seq = seq;
        }
        *visible.lock().unwrap() = progress;
        let _ = notify.try_send(());
    }
    writer.flush()
}
fn progress(status: &mut Status, visible: &Mutex<Progress>) {
    let published = *visible.lock().unwrap();
    status.entries = published.entries;
    status.bytes = published.bytes;
    status.visible_end = Some(VisibleEnd {
        seq: published.seq,
        offset: published.bytes,
    });
}

pub(super) fn capture(
    config: Config,
    dir: Option<&Path>,
    writer: Box<dyn Write + Send>,
) -> Result<Status> {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(config.duration_secs);
    let mut session = Session::new();
    let buffers = config.buffers.clone();
    let mut header = DocumentHeader::live(session.id(), config.serial.clone(), buffers.clone());
    header.source.requested_buffers_scope = Some("capture_configuration".into());
    header
        .source
        .extra
        .insert("capture_scope".into(), json!(config.scope));
    header.source.extra.insert(
        "logcat_scope_args".into(),
        json!(config.scope.logcat_args()),
    );
    header.source.extra.insert("host_budgets".into(), json!({
        "duration_secs": config.duration_secs, "max_entries": config.max_entries, "max_bytes": config.max_bytes
    }));
    if config.transport == adb::LogTransport::Proto {
        header.source.transport_format = Some("logcat_proto_experimental".into());
        header
            .normalization
            .insert("protobuf_raw_payload".into(), json!("preserved_base64"));
    }
    if config.record_schema_version == 3 {
        header
            .set_format(NativeFormat::CompactV3)
            .map_err(io_error)?;
    }
    let mut encoder = StreamEncoder::new(header.clone()).map_err(io_error)?;
    let header_bytes = encoder.header_bytes().map_err(io_error)?;
    if header_bytes.len() as u64 + FOOTER_RESERVE > config.max_bytes {
        return Err(usage(
            "native --max-bytes must fit its document header plus a 4096-byte footer reserve",
        ));
    }
    let mut status = Status::new(dir.unwrap_or_else(|| Path::new("")));
    status.record_schema_version = Some(header.schema_version);
    status.artifact_id = Some(header.artifact_id);
    status.session_id = Some(session.id());
    status.visible_end = Some(VisibleEnd::default());
    status.capture_completeness = Some("unknown".into());
    status.stream_interruptions = Some(0);
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&interrupted);
    ctrlc::set_handler(move || signal_flag.store(true, Ordering::Release)).map_err(io_error)?;

    let visible = Arc::new(Mutex::new(Progress::default()));
    let published = Arc::clone(&visible);
    let (write_tx, write_rx) = mpsc::sync_channel::<Packet>(1);
    let (done_tx, done_rx) = mpsc::channel();
    let (progress_tx, progress_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = done_tx.send(write_packets(writer, write_rx, &published, &progress_tx));
    });
    let mut admitted_bytes = header_bytes.len() as u64;
    write_tx
        .send(Packet {
            bytes: header_bytes,
            seq: None,
        })
        .map_err(io_error)?;
    let mut admitted_entries = 0u64;
    let mut pending: Option<(Packet, StreamEncoder)> = None;
    let (tx, rx) = mpsc::sync_channel(128);
    let mut controller = Controller::default();
    controller.request(
        Intent::Capture(Target {
            serial: config.serial.clone(),
            buffers,
        }),
        false,
        start,
    );
    let mut child: Option<adb::LogcatHandle> = None;
    let mut stopping = false;
    let mut stop_deadline = None;
    let mut failure = None;
    let mut writer_done = None;
    let mut received = 0u64;
    let mut attempts = 0u64;
    let mut reconnects = 0u64;
    let mut last_backend_error = None;
    let mut incomplete_frame_at_stop = None;
    let mut heartbeat = start - Duration::from_secs(1);

    loop {
        let tick = Instant::now();
        if writer_done.is_none() {
            match done_rx.try_recv() {
                Ok(result) => {
                    let message = result.as_ref().err().map_or_else(
                        || "output writer ended unexpectedly".into(),
                        ToString::to_string,
                    );
                    if failure
                        .as_ref()
                        .is_none_or(|e: &Error| e.code == "output_error")
                    {
                        failure = Some(error("output_error", message, 5));
                    }
                    writer_done = Some(result);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    failure.get_or_insert_with(|| {
                        error("output_error", "output writer disconnected", 5)
                    });
                    writer_done = Some(Err(io::Error::other("writer disconnected")));
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        if !stopping {
            let reason = if failure.is_some() {
                Some("failed")
            } else if interrupted.load(Ordering::Acquire) {
                Some("interrupted")
            } else if dir.is_some_and(|d| d.join("stop").exists()) {
                Some("stopped")
            } else if tick >= deadline {
                Some("duration_limit")
            } else {
                status.reason.as_deref()
            };
            if let Some(reason) = reason {
                status.reason = Some(reason.to_owned());
                stopping = true;
                stop_deadline = Some(tick + Duration::from_secs(5));
                pending = None;
                controller.request(Intent::Closed, false, tick);
                if let Some(child) = &mut child {
                    child.stop();
                }
            }
        }
        if let Some((packet, next_encoder)) = pending.take() {
            let size = packet.bytes.len() as u64;
            match write_tx.try_send(packet) {
                Ok(()) => {
                    encoder = next_encoder;
                    admitted_bytes += size;
                    admitted_entries += 1;
                    if admitted_entries >= config.max_entries {
                        status.reason = Some("max_entries".into());
                        status.omitted_by_limit = true;
                    }
                }
                Err(TrySendError::Full(packet)) => pending = Some((packet, next_encoder)),
                Err(TrySendError::Disconnected(_)) => {
                    failure.get_or_insert_with(|| {
                        error("output_error", "output writer disconnected", 5)
                    });
                }
            }
        }
        if stopping && controller.active_id().is_none() {
            break;
        }
        if stop_deadline.is_some_and(|end| tick >= end) {
            failure.get_or_insert_with(|| {
                error(
                    "adb_error",
                    "owned ADB child did not acknowledge stop within five seconds",
                    4,
                )
            });
            break;
        }
        if let Some(Action::Start { id, target }) = controller.poll(tick, true) {
            attempts += 1;
            if session.begin_attempt().is_none() {
                failure = Some(error(
                    "identity_exhausted",
                    "capture attempt counter exhausted",
                    5,
                ));
                controller.exited(id, "identity exhausted".into(), tick);
                continue;
            }
            let cancelled = || {
                interrupted.load(Ordering::Acquire) || dir.is_some_and(|d| d.join("stop").exists())
            };
            match adb::start_logcat_with_scope(
                &config.adb,
                &target.serial,
                &target.buffers,
                id,
                tx.clone(),
                WakeSignal::default(),
                &config.scope,
                adb::LogcatStartOptions {
                    transport: config.transport,
                    deadline: Some(deadline),
                    cancelled: Some(&cancelled),
                },
            ) {
                Ok(handle) => child = Some(handle),
                Err(message) => {
                    if !cancelled() {
                        last_backend_error = Some(message.clone());
                        status.last_stream_error = Some(message.clone());
                    }
                    controller.exited(id, message, Instant::now());
                }
            }
        }
        match controller.status() {
            Phase::Failed(message) => {
                failure.get_or_insert_with(|| error("adb_error", message, 4));
            }
            Phase::Backoff { .. } => status.state = "reconnecting".into(),
            Phase::Running => status.state = "running".into(),
            Phase::Starting => status.state = "starting".into(),
            _ => (),
        }
        if stopping {
            status.state = "stopping".into();
        }
        if heartbeat.elapsed() >= Duration::from_millis(500) {
            progress(&mut status, &visible);
            if let Some(dir) = dir
                && let Err(e) = write_status(dir, &mut status)
            {
                failure.get_or_insert(e);
            }
            heartbeat = Instant::now();
        }
        // Do not read a second record while the single pending write is full.
        // This propagates backpressure through the bounded backend channel.
        if pending.is_some() {
            let _ = progress_rx.recv_timeout(Duration::from_millis(25));
            continue;
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(BackendEvent::Logcat {
                session_id: id,
                event,
            }) if controller.active_id() == Some(id) => match event {
                LogcatEvent::Started => {
                    if session.stream_started().is_none() {
                        failure = Some(error(
                            "identity_exhausted",
                            "capture segment counter exhausted",
                            5,
                        ));
                        continue;
                    }
                    controller.started(id, Instant::now());
                    status.readiness = Some("adb_process_started".into());
                    heartbeat = Instant::now() - Duration::from_secs(1);
                }
                LogcatEvent::Entry(queued) => {
                    received += 1;
                    if stopping || status.reason.is_some() || failure.is_some() {
                        continue;
                    }
                    let observed = queued.received_unix_ms();
                    let mut entry = queued.into_inner();
                    entry.provenance = session.next_record(observed);
                    let mut next_encoder = encoder.clone();
                    match next_encoder.record_bytes(&entry) {
                        Ok(bytes)
                            if admitted_bytes + bytes.len() as u64 + FOOTER_RESERVE
                                <= config.max_bytes =>
                        {
                            let seq = entry.provenance.map(|identity| identity.seq);
                            pending = Some((Packet { bytes, seq }, next_encoder));
                        }
                        Ok(_) => {
                            status.reason = Some("max_bytes".into());
                            status.omitted_by_limit = true;
                        }
                        Err(e) => {
                            failure = Some(error("record_error", e.to_string(), 5));
                        }
                    }
                }
                LogcatEvent::Stderr(line) => {
                    status.diagnostic_lines += 1;
                    status.last_diagnostic = Some(line);
                }
                LogcatEvent::Exited {
                    code,
                    error: message,
                    incomplete_proto_frame,
                } => {
                    child = None;
                    if !stopping && config.scope.is_dump() && code == Some(0) && message.is_none() {
                        controller.completed(id, Instant::now());
                        status.reason = Some("snapshot_complete".into());
                        continue;
                    }
                    let unexpected = !stopping;
                    // Exited is emitted after reaping and both pipe readers.
                    let exit_message = message
                        .clone()
                        .unwrap_or_else(|| format!("ADB stream exited with code {code:?}"));
                    if unexpected {
                        last_backend_error = Some(exit_message.clone());
                        status.last_stream_error = Some(exit_message.clone());
                    }
                    controller.exited(id, exit_message, Instant::now());
                    if unexpected {
                        reconnects += 1;
                        status.stream_interruptions = Some(reconnects);
                        status.capture_completeness = Some("partial".into());
                    } else if let Some(message) = message {
                        if incomplete_proto_frame {
                            // Reaping is confirmed, but the frame may have been
                            // cut before or by our stop. Preserve that ambiguity.
                            status.last_stream_error = Some(message.clone());
                            incomplete_frame_at_stop = Some(message);
                        } else {
                            failure.get_or_insert_with(|| error("adb_error", message, 4));
                        }
                    }
                }
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                failure
                    .get_or_insert_with(|| error("adb_error", "backend channel disconnected", 4));
            }
            _ => (),
        }
    }
    if let Some(child) = &mut child {
        child.stop();
    }
    status.diagnostics_complete = controller.active_id().is_none();
    drop(rx);
    drop(tx);
    if (status.readiness.is_none() || (admitted_entries == 0 && reconnects > 0))
        && let Some(message) = &last_backend_error
    {
        failure.get_or_insert_with(|| error("adb_error", message.clone(), 4));
    }
    // Finalization describes the successfully admitted prefix. It never claims
    // complete device capture, and failed output intentionally has no footer.
    if writer_done.is_none() {
        let mut capture = CaptureStatus {
            state: if failure.is_some() {
                "failed"
            } else {
                "stopped"
            }
            .into(),
            completeness: if received > admitted_entries
                || reconnects > 0
                || failure.is_some()
                || incomplete_frame_at_stop.is_some()
            {
                "partial"
            } else {
                "unknown"
            }
            .into(),
            observed_unix_ms: Some(now()),
            ..CaptureStatus::default()
        };
        capture
            .extra
            .insert("stop_reason".into(), json!(status.reason));
        capture.extra.insert("attempts".into(), json!(attempts));
        if let Some(message) = last_backend_error {
            let (diagnostic, truncated) = footer_diagnostic(&message);
            capture
                .extra
                .insert("last_stream_error".into(), json!(diagnostic));
            capture
                .extra
                .insert("last_stream_error_truncated".into(), json!(truncated));
        }
        status.capture_completeness = Some(capture.completeness.clone());
        if received > admitted_entries {
            capture
                .known_gaps
                .push(json!({"kind":"host_discarded_records","count":received-admitted_entries}));
        }
        if reconnects > 0 {
            capture
                .known_gaps
                .push(json!({"kind":"stream_interruptions","count":reconnects,"loss":"unknown"}));
        }
        if let Some(diagnostic) = incomplete_frame_at_stop {
            let (diagnostic, truncated) = footer_diagnostic(&diagnostic);
            capture.known_gaps.push(json!({"kind":"incomplete_frame_at_stop","cause":"concurrent_with_host_stop","diagnostic":diagnostic,"diagnostic_truncated":truncated}));
        }
        match encoder.finish_bytes(capture).map_err(io_error) {
            Ok(bytes) if bytes.len() as u64 <= FOOTER_RESERVE => {
                let mut packet = Packet { bytes, seq: None };
                let until = Instant::now() + Duration::from_secs(2);
                loop {
                    match write_tx.try_send(packet) {
                        Ok(()) => break,
                        Err(TrySendError::Disconnected(_)) => break,
                        Err(TrySendError::Full(p)) => {
                            packet = p;
                            if Instant::now() >= until {
                                failure.get_or_insert_with(|| {
                                    error(
                                        "output_backpressure",
                                        "output stalled before document finalization",
                                        5,
                                    )
                                });
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                }
            }
            Ok(_) => {
                failure.get_or_insert_with(|| {
                    error("record_error", "footer exceeded reserved space", 5)
                });
            }
            Err(e) => {
                failure.get_or_insert(e);
            }
        }
    }
    drop(write_tx);
    match writer_done
        .map(Ok)
        .unwrap_or_else(|| done_rx.recv_timeout(Duration::from_secs(2)))
    {
        Ok(Ok(())) => (),
        Ok(Err(e)) => {
            if failure.as_ref().is_none_or(|e| e.code == "output_error") {
                failure = Some(error("output_error", e.to_string(), 5));
            }
        }
        Err(e) => {
            failure.get_or_insert_with(|| error("output_backpressure", e.to_string(), 5));
        }
    }
    progress(&mut status, &visible);
    status.state = if failure.is_some() {
        "failed"
    } else {
        "completed"
    }
    .into();
    status.discarded_entries = received.saturating_sub(status.entries);
    if status.discarded_entries > 0 || failure.is_some() {
        status.capture_completeness = Some("partial".into());
    }
    status.diagnostic_dropped_count = status.diagnostic_lines.saturating_sub(1);
    if let Some(e) = &mut failure {
        if e.code == "adb_error"
            && let Some(diagnostic) = &status.last_diagnostic
        {
            e.message.push_str(": ");
            e.message.push_str(diagnostic);
        }
        status.reason = Some(e.code.into());
        status.error = Some(e.message.clone());
    }
    if let Some(dir) = dir {
        write_status(dir, &mut status)?;
    }
    if failure.is_some() || interrupted.load(Ordering::Acquire) {
        if dir.is_none() {
            let _ = writeln!(
                io::stderr(),
                "{}",
                serde_json::to_string(&status).map_err(io_error)?
            );
        }
        Err(failure.unwrap_or_else(|| {
            error(
                "interrupted",
                "capture interrupted; owned ADB child stopped",
                130,
            )
        }))
    } else {
        Ok(status)
    }
}
