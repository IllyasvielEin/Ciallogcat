use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc, Condvar, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::WakeSignal;
use regex::Regex;

use crate::capture_scope::CaptureScope;
use crate::model::{Level, LogBuffer, LogEntry, TextLoss};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogTransport {
    #[default]
    Text,
    Proto,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
    pub serial: String,
    pub state: String,
    pub model: Option<String>,
    pub transport: &'static str,
}

impl DeviceInfo {
    pub fn available(&self) -> bool {
        self.state == "device"
    }

    pub fn display_name(&self) -> String {
        let name = self
            .model
            .as_deref()
            .unwrap_or(&self.serial)
            .replace('_', " ");
        if self.available() {
            format!("{name} · {}", self.transport)
        } else {
            format!("{name} · {}", self.state)
        }
    }
}

pub enum BackendEvent {
    Devices(Result<Vec<DeviceInfo>, String>),
    Packages {
        serial: String,
        result: Result<Vec<String>, String>,
    },
    Processes {
        serial: String,
        session_id: u64,
        observed_from: Instant,
        observed_until: Instant,
        result: Result<HashMap<u32, String>, String>,
    },
    Logcat {
        session_id: u64,
        event: LogcatEvent,
    },
}

pub enum LogcatEvent {
    Started,
    Entry(QueuedLogEntry),
    Stderr(String),
    /// Final event for this attempt: the child has been reaped and both pipe
    /// readers have finished sending. No more events for it follow this one.
    Exited {
        code: Option<i32>,
        error: Option<String>,
        /// True only when the reported failure is a truncated protobuf frame
        /// and no other reader/process failure was observed. The caller must
        /// still distinguish unexpected EOF from a concurrent intentional stop.
        incomplete_proto_frame: bool,
    },
}

const QUEUE_BYTES: usize = 16 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
// Diagnostics need a bounded excerpt, not a full log payload. Together with the
// bounded event channel, this caps queued stderr payload independently of the
// entry-byte permits. The suffix is included in MAX_STDERR_EVENT_BYTES.
const MAX_STDERR_LINE_BYTES: usize = 4096;
const STDERR_TRUNCATION_NOTICE: &str = " [truncated: diagnostic exceeded 4 KiB]";
const MAX_STDERR_EVENT_BYTES: usize = MAX_STDERR_LINE_BYTES + STDERR_TRUNCATION_NOTICE.len();

#[derive(Default)]
struct QueueBudget {
    used: Mutex<usize>,
    available: Condvar,
}
impl QueueBudget {
    fn reserve(self: &Arc<Self>, bytes: usize) -> QueuePermit {
        let mut used = self.used.lock().unwrap();
        while *used + bytes > QUEUE_BYTES {
            used = self.available.wait(used).unwrap();
        }
        *used += bytes;
        QueuePermit {
            budget: Arc::clone(self),
            bytes,
        }
    }
}

struct QueuePermit {
    budget: Arc<QueueBudget>,
    bytes: usize,
}
impl Drop for QueuePermit {
    fn drop(&mut self) {
        *self.budget.used.lock().unwrap() -= self.bytes;
        self.budget.available.notify_all();
    }
}

pub struct QueuedLogEntry {
    entry: LogEntry,
    received_at: Instant,
    received_unix_ms: Option<u64>,
    _permit: QueuePermit,
}
impl QueuedLogEntry {
    /// Host receive time, recorded before byte-budget or event-queue waiting.
    /// This does not establish the device time at which the log was produced.
    pub fn received_at(&self) -> Instant {
        self.received_at
    }

    /// Host wall-clock observation sampled before queue/backpressure waiting.
    /// None means the clock cannot be represented as Unix milliseconds.
    pub fn received_unix_ms(&self) -> Option<u64> {
        self.received_unix_ms
    }

    pub fn into_inner(self) -> LogEntry {
        self.entry
    }
}
impl From<LogEntry> for QueuedLogEntry {
    fn from(entry: LogEntry) -> Self {
        let received_at = Instant::now();
        let received_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
        static BUDGET: OnceLock<Arc<QueueBudget>> = OnceLock::new();
        let budget = Arc::clone(BUDGET.get_or_init(|| Arc::new(QueueBudget::default())));
        // The stream parser bounds lines before allocating queued entries.
        let bytes = (entry.payload_bytes() + std::mem::size_of::<LogEntry>()).min(QUEUE_BYTES);
        Self {
            entry,
            received_at,
            received_unix_ms,
            _permit: budget.reserve(bytes),
        }
    }
}

pub struct LogcatHandle {
    child: Arc<Mutex<Child>>,
    stopped: Arc<AtomicBool>,
}

impl LogcatHandle {
    pub fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut child) = self.child.lock()
            && !matches!(child.try_wait(), Ok(Some(_)))
        {
            let _ = child.kill();
        }
    }
}

impl Drop for LogcatHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn find_adb() -> Option<PathBuf> {
    for key in ["CIALLOGCAT_ADB", "ADB"] {
        if let Some(path) = env::var_os(key)
            .map(PathBuf::from)
            .and_then(usable_adb_path)
        {
            return Some(path);
        }
    }

    for key in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Some(root) = env::var_os(key) {
            let candidate = PathBuf::from(root)
                .join("platform-tools")
                .join(adb_executable_name());
            if let Some(candidate) = usable_adb_path(candidate) {
                return Some(candidate);
            }
        }
    }

    if let Some(paths) = env::var_os("PATH") {
        for directory in env::split_paths(&paths) {
            let candidate = directory.join(adb_executable_name());
            if let Some(candidate) = usable_adb_path(candidate) {
                return Some(candidate);
            }
        }
    }

    let mut candidates = Vec::new();
    if let Some(home) = env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join(adb_executable_name()),
        );
    }
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local_app_data)
                .join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join(adb_executable_name()),
        );
    }
    candidates.into_iter().find_map(usable_adb_path)
}

fn usable_adb_path(path: PathBuf) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }

    #[cfg(target_os = "windows")]
    if let Ok(config) = std::fs::read_to_string(path.with_extension("shim"))
        && let Some(target) = parse_scoop_shim_target(&config)
        && target.is_file()
    {
        return Some(target);
    }

    Some(path)
}

#[cfg(target_os = "windows")]
fn parse_scoop_shim_target(config: &str) -> Option<PathBuf> {
    config.lines().find_map(|line| {
        line.trim()
            .strip_prefix("path = \"")
            .and_then(|path| path.strip_suffix('"'))
            .map(PathBuf::from)
    })
}

pub fn spawn_device_query(adb: PathBuf, event_tx: SyncSender<BackendEvent>, wake: WakeSignal) {
    std::thread::Builder::new()
        .name("adb-devices".to_owned())
        .spawn(move || {
            let result = run_adb(&adb, &["devices", "-l"]).map(|text| parse_devices(&text));
            let _ = event_tx.send(BackendEvent::Devices(result));
            wake.notify();
        })
        .expect("failed to start device query");
}

pub fn spawn_package_query(
    adb: PathBuf,
    serial: String,
    event_tx: SyncSender<BackendEvent>,
    wake: WakeSignal,
) {
    std::thread::Builder::new()
        .name("adb-packages".to_owned())
        .spawn(move || {
            let result = run_adb(
                &adb,
                &["-s", &serial, "shell", "pm", "list", "packages", "-3"],
            )
            .map(|text| parse_packages(&text));
            let _ = event_tx.send(BackendEvent::Packages {
                serial: serial.clone(),
                result,
            });
            wake.notify();
        })
        .expect("failed to start package query");
}

pub fn spawn_process_query(
    adb: PathBuf,
    serial: String,
    session_id: u64,
    event_tx: SyncSender<BackendEvent>,
    wake: WakeSignal,
) {
    std::thread::Builder::new()
        .name("adb-processes".to_owned())
        .spawn(move || {
            let observed_from = Instant::now();
            let preferred = run_adb(
                &adb,
                &["-s", &serial, "shell", "ps", "-A", "-o", "PID,NAME"],
            )
            .map(|text| parse_processes(&text));
            let result = match preferred {
                Ok(processes) if !processes.is_empty() => Ok(processes),
                _ => run_adb(&adb, &["-s", &serial, "shell", "ps", "-A"])
                    .map(|text| parse_processes(&text)),
            };
            let _ = event_tx.send(BackendEvent::Processes {
                serial: serial.clone(),
                session_id,
                observed_from,
                observed_until: Instant::now(),
                result,
            });
            wake.notify();
        })
        .expect("failed to start process query");
}

/// Start bounded Logcat capture for a caller-owned session.
///
/// Entry wakes merge within 16 ms and preserve a trailing notification even
/// when production stops. Drain available events on wake; a consumer imposing
/// a per-frame drain limit must schedule its own continuation. Channel `recv` /
/// `recv_timeout` remain available for synchronous consumers. See [`WakeSignal`].
///
/// Stop requests child termination without waiting for the consumer. Reaping
/// proceeds independently of pipe-reader backpressure. Keep receiving and
/// release/consume queued-entry permits until `Exited`: a full event channel
/// or held byte-budget permits can delay that final drain acknowledgement.
pub fn start_logcat(
    adb: &Path,
    serial: &str,
    buffers: &[LogBuffer],
    session_id: u64,
    event_tx: SyncSender<BackendEvent>,
    wake: WakeSignal,
) -> Result<LogcatHandle, String> {
    start_logcat_with_transport(
        adb,
        serial,
        buffers,
        session_id,
        event_tx,
        wake,
        LogTransport::Text,
    )
}

pub fn start_logcat_with_transport(
    adb: &Path,
    serial: &str,
    buffers: &[LogBuffer],
    session_id: u64,
    event_tx: SyncSender<BackendEvent>,
    wake: WakeSignal,
    transport: LogTransport,
) -> Result<LogcatHandle, String> {
    start_logcat_with_options(
        adb,
        serial,
        buffers,
        session_id,
        event_tx,
        wake,
        LogcatStartOptions {
            transport,
            ..LogcatStartOptions::default()
        },
    )
}

/// Startup bounds apply to capability queries and are checked immediately
/// before spawning the log stream. The callback runs on this calling thread.
/// Once started, cancellation belongs to `LogcatHandle::stop` and its owner.
#[derive(Default)]
pub struct LogcatStartOptions<'a> {
    pub transport: LogTransport,
    pub deadline: Option<Instant>,
    pub cancelled: Option<&'a dyn Fn() -> bool>,
}

impl LogcatStartOptions<'_> {
    fn check(&self) -> Result<(), String> {
        if self.cancelled.is_some_and(|cancelled| cancelled()) {
            Err("logcat startup cancelled".into())
        } else if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Err("logcat startup deadline exceeded".into())
        } else {
            Ok(())
        }
    }
}

pub fn start_logcat_with_options(
    adb: &Path,
    serial: &str,
    buffers: &[LogBuffer],
    session_id: u64,
    event_tx: SyncSender<BackendEvent>,
    wake: WakeSignal,
    options: LogcatStartOptions<'_>,
) -> Result<LogcatHandle, String> {
    start_logcat_with_scope(
        adb,
        serial,
        buffers,
        session_id,
        event_tx,
        wake,
        &CaptureScope::default(),
        options,
    )
}

/// Starts the validated device history scope. Existing wrappers keep `-T 1`.
/// A dump's successful `Exited` is expected completion; callers must still
/// check the exit code and reader error, then finish instead of reconnecting.
// Keep the existing wrappers source-compatible while sharing a validated scope.
#[allow(clippy::too_many_arguments)]
pub fn start_logcat_with_scope(
    adb: &Path,
    serial: &str,
    buffers: &[LogBuffer],
    session_id: u64,
    event_tx: SyncSender<BackendEvent>,
    wake: WakeSignal,
    scope: &CaptureScope,
    options: LogcatStartOptions<'_>,
) -> Result<LogcatHandle, String> {
    options.check()?;
    let transport = options.transport;
    if transport == LogTransport::Proto {
        let features =
            run_adb_controlled(adb, &["-s", serial, "features"], &options).map_err(|error| {
                format!("cannot verify protobuf transport shell_v2 capability: {error}")
            })?;
        if !features
            .split(|character: char| character == ',' || character.is_whitespace())
            .any(|feature| feature == "shell_v2")
        {
            return Err("protobuf transport requires target ADB shell_v2 support; no fallback was attempted".into());
        }
    }
    options.check()?;
    let mut command = adb_command(adb);
    command
        .args(match transport {
            LogTransport::Text => scoped_logcat_args(serial, buffers, scope),
            LogTransport::Proto => scoped_proto_args(serial, buffers, scope),
        })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start adb logcat: {error}"))?;
    let Some(stdout) = child.stdout.take() else {
        let _ = kill_and_reap(&mut child);
        return Err("adb logcat did not provide stdout".to_owned());
    };
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));

    let stderr_worker = if let Some(stderr) = stderr {
        let error_tx = event_tx.clone();
        let error_wake = wake.clone();
        Some(
            std::thread::Builder::new()
                .name("adb-logcat-stderr".to_owned())
                .spawn(move || {
                    read_logcat_stderr(BufReader::new(stderr), |line| {
                        let _ = error_tx.send(BackendEvent::Logcat {
                            session_id,
                            event: LogcatEvent::Stderr(line),
                        });
                        error_wake.notify();
                    })
                })
                .map_err(|error| failed_logcat_start(&child, "stderr reader", error))?,
        )
    } else {
        None
    };

    let output_tx = event_tx.clone();
    let output_wake = wake.clone();
    let stdout_worker = std::thread::Builder::new()
        .name("adb-logcat".to_owned())
        .spawn(move || {
            let _ = output_tx.send(BackendEvent::Logcat {
                session_id,
                event: LogcatEvent::Started,
            });
            output_wake.notify();
            let entry_wake = output_wake.coalesced(Duration::from_millis(16));
            let on_entry = |entry: LogEntry| {
                let _ = output_tx.send(BackendEvent::Logcat {
                    session_id,
                    event: LogcatEvent::Entry(entry.into()),
                });
                entry_wake.notify();
            };
            match transport {
                LogTransport::Text => read_logcat_stream(BufReader::new(stdout), on_entry),
                LogTransport::Proto => read_proto_stream(BufReader::new(stdout), on_entry),
            }
        })
        .map_err(|error| failed_logcat_start(&child, "stdout reader", error))?;

    let reaper_child = Arc::clone(&child);
    let stopped = Arc::new(AtomicBool::new(false));
    let stop_requested = Arc::clone(&stopped);
    std::thread::Builder::new()
        .name("adb-logcat-drain".to_owned())
        .spawn(move || {
            let mut stdout_worker = Some(stdout_worker);
            let mut stderr_worker = stderr_worker;
            let mut stdout_finished = None;
            let mut error = None;
            let mut incomplete_proto_frame = false;
            // Child ownership must not depend on a reader acquiring a queue
            // permit or sending into a full consumer channel.
            let status = loop {
                if stdout_worker
                    .as_ref()
                    .is_some_and(|worker| worker.is_finished())
                {
                    if let Some(failure) =
                        finish_logcat_reader(stdout_worker.take().unwrap(), "stdout")
                    {
                        record_reader_failure(&mut error, &mut incomplete_proto_frame, failure);
                    }
                    stdout_finished = Some(Instant::now());
                }
                if stderr_worker
                    .as_ref()
                    .is_some_and(|worker| worker.is_finished())
                    && let Some(failure) =
                        finish_logcat_reader(stderr_worker.take().unwrap(), "stderr")
                {
                    record_reader_failure(&mut error, &mut incomplete_proto_frame, failure);
                }
                {
                    let mut child = reaper_child
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match child.try_wait() {
                        Ok(Some(status)) => break status,
                        Ok(None) => (),
                        Err(failure) => {
                            incomplete_proto_frame = false;
                            error.get_or_insert_with(|| {
                                format!("failed to query ADB process status: {failure}")
                            });
                        }
                    }
                    // A closed stdout is an abnormal live stream, but allow
                    // natural exit and its final stderr a short grace period.
                    if stdout_finished
                        .is_some_and(|finished| finished.elapsed() >= Duration::from_millis(250))
                    {
                        incomplete_proto_frame = false;
                        error.get_or_insert_with(|| "Logcat output ended unexpectedly".to_owned());
                    }
                    if (stop_requested.load(Ordering::Acquire) || error.is_some())
                        && let Err(failure) = child.kill()
                        && !matches!(child.try_wait(), Ok(Some(_)))
                    {
                        incomplete_proto_frame = false;
                        error.get_or_insert_with(|| format!("failed to stop ADB: {failure}"));
                    }
                }
                // Do not publish Exited on a failed wait/kill: its contract
                // requires confirmed reaping, even if the OS keeps failing.
                // stop() kills immediately; this interval only bounds reaping
                // and avoids waking 200 times per second on an idle stream.
                std::thread::sleep(Duration::from_millis(25));
            };
            for (worker, pipe) in [(stdout_worker, "stdout"), (stderr_worker, "stderr")] {
                if let Some(worker) = worker
                    && let Some(failure) = finish_logcat_reader(worker, pipe)
                {
                    record_reader_failure(&mut error, &mut incomplete_proto_frame, failure);
                }
            }
            let _ = event_tx.send(BackendEvent::Logcat {
                session_id,
                event: LogcatEvent::Exited {
                    code: status.code(),
                    error,
                    incomplete_proto_frame,
                },
            });
            wake.notify();
        })
        .map_err(|error| failed_logcat_start(&child, "drain worker", error))?;

    Ok(LogcatHandle { child, stopped })
}

fn failed_logcat_start(child: &Mutex<Child>, worker: &str, error: io::Error) -> String {
    let mut child = child
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cleanup = kill_and_reap(&mut child).err();
    let mut message = format!("failed to start logcat {worker}: {error}");
    if let Some(cleanup) = cleanup {
        message.push_str(&format!("; {cleanup}"));
    }
    message
}

struct ReaderFailure {
    message: String,
    incomplete_proto_frame: bool,
}

fn record_reader_failure(
    error: &mut Option<String>,
    incomplete: &mut bool,
    failure: ReaderFailure,
) {
    if error.is_none() {
        *incomplete = failure.incomplete_proto_frame;
        *error = Some(failure.message);
    } else {
        // Never downgrade a different I/O/process failure just because an
        // incomplete frame was also observed while draining the pipes.
        *incomplete &= failure.incomplete_proto_frame;
    }
}

fn finish_logcat_reader(
    worker: std::thread::JoinHandle<io::Result<()>>,
    pipe: &str,
) -> Option<ReaderFailure> {
    match worker.join() {
        Ok(Ok(())) => None,
        Ok(Err(error)) => {
            use crate::protobuf::{ProtoError, ProtoErrorKind};
            let incomplete_proto_frame = pipe == "stdout"
                && error
                    .get_ref()
                    .and_then(|source| source.downcast_ref::<ProtoError>())
                    .is_some_and(|error| {
                        matches!(
                            error.kind,
                            ProtoErrorKind::TruncatedHeader { .. }
                                | ProtoErrorKind::TruncatedPayload { .. }
                        )
                    });
            Some(ReaderFailure {
                message: format!("failed to read logcat {pipe}: {error}"),
                incomplete_proto_frame,
            })
        }
        Err(_) => Some(ReaderFailure {
            message: format!("logcat {pipe} reader panicked"),
            incomplete_proto_frame: false,
        }),
    }
}

#[cfg(test)]
fn logcat_args(serial: &str, buffers: &[LogBuffer]) -> Vec<String> {
    scoped_logcat_args(serial, buffers, &CaptureScope::default())
}

fn scoped_logcat_args(serial: &str, buffers: &[LogBuffer], scope: &CaptureScope) -> Vec<String> {
    let mut buffers = buffers.to_vec();
    LogBuffer::normalize(&mut buffers);
    let mut args: Vec<String> = ["-s", serial, "logcat", "-v", "threadtime"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    args.extend(scope.logcat_args());
    for buffer in buffers {
        args.extend(["-b".to_owned(), buffer.name().to_owned()]);
    }
    args
}

#[cfg(test)]
fn proto_args(serial: &str, buffers: &[LogBuffer]) -> Vec<String> {
    scoped_proto_args(serial, buffers, &CaptureScope::default())
}

fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b))
    {
        word.into()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn scoped_proto_args(serial: &str, buffers: &[LogBuffer], scope: &CaptureScope) -> Vec<String> {
    let mut buffers = buffers.to_vec();
    LogBuffer::normalize(&mut buffers);
    // Values originate in validated types, and every word is shell quoted.
    // In particular device calendar times contain a space. Serial stays local.
    let mut words: Vec<String> = ["exec", "logcat", "--proto", "-v", "threadtime"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    words.extend(scope.logcat_args());
    for buffer in buffers {
        words.extend(["-b".into(), buffer.name().into()]);
    }
    words.push("*:V".into());
    let remote = words
        .iter()
        .map(|word| shell_word(word))
        .collect::<Vec<_>>()
        .join(" ");
    vec![
        "-s".into(),
        serial.into(),
        "shell".into(),
        "-T".into(),
        "-n".into(),
        remote,
    ]
}

fn read_proto_stream(reader: impl Read, mut on_entry: impl FnMut(LogEntry)) -> io::Result<()> {
    let mut reader = crate::protobuf::FrameReader::new(reader);
    while let Some(decoded) = reader.read_next().map_err(io::Error::other)? {
        on_entry(proto_entry(decoded));
    }
    Ok(())
}

fn proto_entry(decoded: crate::protobuf::DecodedEntry) -> LogEntry {
    decoded.project()
}

fn read_logcat_stream(reader: impl BufRead, mut on_entry: impl FnMut(LogEntry)) -> io::Result<()> {
    read_bounded_lines(reader, MAX_LINE_BYTES, |line, truncated| {
        let mut line = String::from_utf8_lossy(line);
        // Capture decoding loss before appending the truncation notice turns a
        // borrowed, valid UTF-8 line into an owned string too.
        let invalid_utf8_replaced = matches!(&line, std::borrow::Cow::Owned(_));
        if truncated {
            line.to_mut()
                .push_str(" [truncated: log line exceeded 1 MiB]");
        }
        if let Some(mut entry) = parse_logcat_line(&line) {
            entry.text_loss = Some(TextLoss {
                truncated,
                invalid_utf8_replaced,
            });
            on_entry(entry);
        }
    })
}

fn read_logcat_stderr(reader: impl BufRead, mut on_line: impl FnMut(String)) -> io::Result<()> {
    read_bounded_lines(reader, MAX_STDERR_LINE_BYTES, |line, mut truncated| {
        let decoded = String::from_utf8_lossy(line);
        // Replacement characters can expand invalid UTF-8. Bound decoded bytes
        // too, and pre-size the final string so the suffix cannot double capacity.
        let mut end = decoded.len().min(MAX_STDERR_LINE_BYTES);
        while !decoded.is_char_boundary(end) {
            end -= 1;
        }
        truncated |= end < decoded.len();
        let suffix = if truncated {
            STDERR_TRUNCATION_NOTICE
        } else {
            ""
        };
        let mut text = String::with_capacity(end + suffix.len());
        text.push_str(&decoded[..end]);
        text.push_str(suffix);
        debug_assert!(text.len() <= MAX_STDERR_EVENT_BYTES);
        if !text.trim().is_empty() {
            on_line(text);
        }
    })
}

// Drain an oversized line through its newline before yielding the bounded prefix.
// A final partial line is yielded too; empty/CRLF handling matches logcat's parser.
fn read_bounded_lines(
    mut reader: impl BufRead,
    limit: usize,
    mut on_line: impl FnMut(&[u8], bool),
) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut truncated = false;
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                break;
            }
            let take = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            let ended = available[take - 1] == b'\n';
            let keep = take.min(limit.saturating_sub(line.len()));
            line.extend_from_slice(&available[..keep]);
            truncated |= keep < take;
            reader.consume(take);
            if ended {
                break;
            }
        }
        if line.is_empty() {
            return Ok(());
        }
        while line
            .last()
            .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
        {
            line.pop();
        }
        on_line(&line, truncated);
    }
}

const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;

fn run_adb(adb: &Path, args: &[&str]) -> Result<String, String> {
    run_adb_controlled(adb, args, &LogcatStartOptions::default())
}

fn run_adb_controlled(
    adb: &Path,
    args: &[&str],
    options: &LogcatStartOptions<'_>,
) -> Result<String, String> {
    options.check()?;
    let mut child = adb_command(adb)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to run {}: {error}", adb.display()))?;
    let query_deadline = Instant::now() + QUERY_TIMEOUT;
    let deadline = options
        .deadline
        .map_or(query_deadline, |end| end.min(query_deadline));
    // Drain both pipes concurrently to avoid a full stderr pipe deadlocking
    // stdout. Each query's buffered output is bounded independently.
    fn read_pipe(pipe: impl Read + Send + 'static) -> mpsc::Receiver<io::Result<Vec<u8>>> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe
                .take(QUERY_OUTPUT_LIMIT + 1)
                .read_to_end(&mut bytes)
                .and_then(|_| {
                    if bytes.len() as u64 > QUERY_OUTPUT_LIMIT {
                        Err(io::Error::other("ADB query output exceeded 16 MiB"))
                    } else {
                        Ok(bytes)
                    }
                });
            let _ = tx.send(result);
        });
        rx
    }
    let stdout = read_pipe(child.stdout.take().expect("piped stdout"));
    let stderr = read_pipe(child.stderr.take().expect("piped stderr"));
    let status = loop {
        if let Err(reason) = options.check() {
            let cleanup = kill_and_reap(&mut child);
            return Err(match cleanup {
                Ok(_) => reason,
                Err(error) => format!("{reason}; {error}"),
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let cleanup = kill_and_reap(&mut child);
                let reason = match result {
                    Err(error) => format!("failed to wait for ADB query: {error}"),
                    _ => "ADB query timed out after 5 seconds".to_owned(),
                };
                return Err(match cleanup {
                    Ok(_) => reason,
                    Err(error) => format!("{reason}; {error}"),
                });
            }
        }
    };
    let collect = |rx: mpsc::Receiver<io::Result<Vec<u8>>>| -> Result<Vec<u8>, String> {
        loop {
            options.check()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining.min(Duration::from_millis(10))) {
                Ok(result) => {
                    return result.map_err(|error| format!("failed to read ADB query: {error}"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => (),
                Err(_) => return Err("ADB query output timed out after 5 seconds".into()),
            }
        }
    };
    let stdout = collect(stdout)?;
    let stderr = collect(stderr)?;
    if !status.success() {
        let stderr = bounded_query_diagnostic(&stderr);
        return Err(if stderr.is_empty() {
            format!("adb exited with {status}")
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Query output has a larger transport allowance, but an error becomes status
/// metadata. Limit the preview before lossy decoding and retain UTF-8 boundaries.
fn bounded_query_diagnostic(bytes: &[u8]) -> String {
    let retained = bytes.len().min(MAX_STDERR_LINE_BYTES);
    let decoded = String::from_utf8_lossy(&bytes[..retained]);
    let mut end = decoded.len().min(MAX_STDERR_LINE_BYTES);
    while !decoded.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = retained < bytes.len() || end < decoded.len();
    let prefix = decoded[..end].trim();
    let suffix = if truncated {
        STDERR_TRUNCATION_NOTICE
    } else {
        ""
    };
    let mut result = String::with_capacity(prefix.len() + suffix.len());
    result.push_str(prefix);
    result.push_str(suffix);
    result
}

// Never enter blocking wait after a failed kill of a still-running child.
pub(crate) fn kill_and_reap(child: &mut Child) -> Result<ExitStatus, String> {
    match child.kill() {
        Ok(()) => child
            .wait()
            .map_err(|error| format!("failed to wait for ADB: {error}")),
        Err(error) => match child.try_wait() {
            Ok(Some(status)) => Ok(status),
            _ => Err(format!("failed to stop ADB: {error}")),
        },
    }
}

pub(crate) fn adb_command(path: &Path) -> Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = Command::new(path);
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(target_os = "windows"))]
    {
        Command::new(path)
    }
}

fn adb_executable_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "adb.exe"
    } else {
        "adb"
    }
}

fn parse_devices(output: &str) -> Vec<DeviceInfo> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
                return None;
            }
            let mut fields = line.split_whitespace();
            let serial = fields.next()?.to_owned();
            let state = fields.next()?.to_owned();
            let model = fields
                .find_map(|field| field.strip_prefix("model:"))
                .map(ToOwned::to_owned);
            let transport = if serial.starts_with("emulator-") {
                "Emulator"
            } else if serial.contains(':') {
                "Wi-Fi"
            } else {
                "USB"
            };
            Some(DeviceInfo {
                serial,
                state,
                model,
                transport,
            })
        })
        .collect()
}

fn parse_packages(output: &str) -> Vec<String> {
    let mut packages: Vec<_> = output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .filter_map(|line| line.split_whitespace().next())
        .map(ToOwned::to_owned)
        .collect();
    packages.sort_unstable();
    packages.dedup();
    packages
}

fn parse_processes(output: &str) -> HashMap<u32, String> {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let Some(header) = lines.next() else {
        return HashMap::new();
    };
    let columns: Vec<_> = header.split_whitespace().collect();
    let Some(pid_index) = columns.iter().position(|column| *column == "PID") else {
        return HashMap::new();
    };
    let name_index = columns
        .iter()
        .position(|column| matches!(*column, "NAME" | "CMDLINE" | "ARGS"))
        .unwrap_or(columns.len().saturating_sub(1));

    lines
        .filter_map(|line| {
            let values: Vec<_> = line.split_whitespace().collect();
            let pid = values.get(pid_index)?.parse().ok()?;
            let name = *values.get(name_index)?;
            Some((pid, name.to_owned()))
        })
        .collect()
}

pub fn parse_logcat_line(line: &str) -> Option<LogEntry> {
    if line.trim().is_empty() {
        return None;
    }
    if line.starts_with("---------") {
        return Some(LogEntry::marker(line.trim().to_owned()));
    }

    static THREADTIME: OnceLock<Regex> = OnceLock::new();
    let matcher = THREADTIME.get_or_init(|| {
        Regex::new(
            r"^\s*((?:\d{4}-)?\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2}\.\d+)\s+(\d+)\s+(\d+)\s+([VDIWEFAS])\s+(.+?):\s?(.*)$",
        )
        .expect("valid threadtime parser")
    });
    let Some(captures) = matcher.captures(line) else {
        return Some(LogEntry::unparsed(line.to_owned()));
    };

    let level = captures
        .get(4)
        .and_then(|value| value.as_str().chars().next())
        .and_then(Level::from_letter);
    let pid = captures
        .get(2)
        .and_then(|value| value.as_str().parse().ok());
    let tid = captures
        .get(3)
        .and_then(|value| value.as_str().parse().ok());
    let (Some(level), Some(pid), Some(tid)) = (level, pid, tid) else {
        return Some(LogEntry::unparsed(line.to_owned()));
    };
    Some(LogEntry::new(
        captures.get(1)?.as_str().trim().to_owned(),
        pid,
        tid,
        captures.get(5)?.as_str().trim().to_owned(),
        level,
        captures.get(6)?.as_str().to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_buffers_are_explicit_logcat_arguments() {
        assert_eq!(
            logcat_args("device", &[LogBuffer::System, LogBuffer::Crash]),
            [
                "-s",
                "device",
                "logcat",
                "-v",
                "threadtime",
                "-T",
                "1",
                "-b",
                "system",
                "-b",
                "crash"
            ]
        );
        let args = logcat_args("device", &[LogBuffer::All, LogBuffer::Main]);
        assert_eq!(&args[7..], ["-b", "all"]);
        assert!(!args.iter().any(|arg| arg == "-c"));
        assert_eq!(
            &logcat_args("device", &[])[7..],
            ["-b", "main", "-b", "system", "-b", "crash"]
        );
    }

    #[test]
    fn oversized_stream_line_is_bounded_and_next_line_is_read() {
        let mut input = b"09-06 12:34:56.789 1234 1235 I Tag: ".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES * 2));
        input.extend_from_slice(b"\n09-06 12:34:56.790 1234 1235 I Tag: next\n");
        let mut entries = Vec::new();
        read_logcat_stream(input.as_slice(), |entry| entries.push(entry)).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].message.len() < MAX_LINE_BYTES + 100);
        assert!(entries[0].message.contains("truncated:"));
        assert_eq!(
            entries[0].text_loss,
            Some(TextLoss {
                truncated: true,
                invalid_utf8_replaced: false,
            })
        );
        assert_eq!(entries[1].message.as_ref(), "next");
        assert_eq!(entries[1].text_loss, Some(TextLoss::default()));
    }

    #[test]
    fn consuming_queue_entry_releases_byte_reservation() {
        let budget = Arc::new(QueueBudget::default());
        *budget.used.lock().unwrap() = 123;
        let entry = QueuedLogEntry {
            entry: LogEntry::marker("data"),
            received_at: Instant::now(),
            received_unix_ms: Some(1234),
            _permit: QueuePermit {
                budget: Arc::clone(&budget),
                bytes: 123,
            },
        };
        assert_eq!(entry.received_unix_ms(), Some(1234));
        let log = entry.into_inner();
        assert_eq!(*budget.used.lock().unwrap(), 0);
        assert_eq!(log.message.as_ref(), "data");
    }

    #[test]
    fn queue_byte_budget_blocks_producer_until_space_is_released() {
        let budget = Arc::new(QueueBudget::default());
        let full = budget.reserve(QUEUE_BYTES);
        let producer_budget = Arc::clone(&budget);
        let (tx, rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            let permit = producer_budget.reserve(1);
            tx.send(permit).unwrap();
        });
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(full);
        let permit = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(*budget.used.lock().unwrap(), 1);
        drop(permit);
        producer.join().unwrap();
        assert_eq!(*budget.used.lock().unwrap(), 0);
    }

    #[test]
    fn devices_include_transport_and_state() {
        let devices = parse_devices(
            "List of devices attached\nABC123 device product:foo model:Pixel_9 device:foo transport_id:1\n192.168.1.4:5555 unauthorized\n",
        );
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].display_name(), "Pixel 9 · USB");
        assert!(!devices[1].available());
        assert_eq!(devices[1].transport, "Wi-Fi");
    }

    #[test]
    fn package_list_is_sorted_and_deduplicated() {
        assert_eq!(
            parse_packages("package:z.app\npackage:a.app\npackage:z.app\n"),
            ["a.app", "z.app"]
        );
    }

    #[test]
    fn process_table_supports_compact_and_android_ps_headers() {
        let compact = parse_processes("PID NAME\n123 com.example.app\n");
        assert_eq!(
            compact.get(&123).map(String::as_str),
            Some("com.example.app")
        );

        let full = parse_processes(
            "USER PID PPID VSZ RSS WCHAN ADDR S NAME\nu0_a1 456 1 0 0 0 0 S com.example.app:sync\n",
        );
        assert_eq!(
            full.get(&456).map(String::as_str),
            Some("com.example.app:sync")
        );
    }

    #[test]
    fn threadtime_log_is_structured() {
        let entry =
            parse_logcat_line("08-29 15:42:10.123  1234  1260 W MainActivity: request timeout")
                .unwrap();
        assert_eq!(entry.time.as_ref(), "08-29 15:42:10.123");
        assert_eq!(entry.pid, 1234);
        assert_eq!(entry.tid, 1260);
        assert_eq!(entry.level, Level::Warn);
        assert_eq!(entry.tag.as_ref(), "MainActivity");
        assert_eq!(entry.message.as_ref(), "request timeout");
        assert_eq!(entry.text_loss, None);
    }

    #[test]
    fn logcat_stream_replaces_invalid_utf8_and_keeps_reading() {
        let mut input = b"08-29 15:42:10.123  1234  1260 I MainActivity: before\n".to_vec();
        input.extend_from_slice(b"08-29 15:42:10.124  1234  1260 W MainActivity: damaged byte ");
        input.push(0xD5);
        input.extend_from_slice(b"\n08-29 15:42:10.125  1234  1260 I MainActivity: after\n");

        let mut entries = Vec::new();
        read_logcat_stream(input.as_slice(), |entry| entries.push(entry)).unwrap();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].message.as_ref(), "before");
        assert!(entries[1].message.contains('\u{FFFD}'));
        assert_eq!(entries[2].message.as_ref(), "after");
        assert_eq!(entries[0].text_loss, Some(TextLoss::default()));
        assert_eq!(
            entries[1].text_loss,
            Some(TextLoss {
                truncated: false,
                invalid_utf8_replaced: true,
            })
        );
        assert_eq!(entries[2].text_loss, Some(TextLoss::default()));
    }

    #[test]
    fn stream_text_loss_is_observed_in_bytes_not_in_message_text() {
        let input = concat!(
            "08-29 15:42:10.123 1234 1260 I Tag: literal ",
            "[truncated: log line exceeded 1 MiB] \u{FFFD}\n",
            "--------- beginning of main\n",
            "unparsed but valid UTF-8 雪\n",
        );
        let mut entries = Vec::new();
        read_logcat_stream(input.as_bytes(), |entry| entries.push(entry)).unwrap();

        assert_eq!(entries.len(), 3);
        assert!(entries[0].message.contains("truncated:"));
        assert!(entries[0].message.contains('\u{FFFD}'));
        assert_eq!(entries[1].kind, crate::model::LogEntryKind::Marker);
        assert_eq!(entries[2].kind, crate::model::LogEntryKind::Unparsed);
        assert!(
            entries
                .iter()
                .all(|entry| entry.text_loss == Some(TextLoss::default()))
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn scoop_shim_points_to_real_adb() {
        assert_eq!(
            parse_scoop_shim_target(
                "path = \"C:\\Users\\me\\scoop\\apps\\adb\\current\\adb.exe\"\n"
            ),
            Some(PathBuf::from(r"C:\Users\me\scoop\apps\adb\current\adb.exe"))
        );
    }

    #[test]
    fn stopping_logcat_process_is_fast_and_idempotent() {
        #[cfg(target_os = "windows")]
        let child = adb_command(Path::new("ping.exe"))
            .args(["-t", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        #[cfg(not(target_os = "windows"))]
        let child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let child = Arc::new(Mutex::new(child));
        let mut handle = LogcatHandle {
            child: Arc::clone(&child),
            stopped: Arc::new(AtomicBool::new(false)),
        };
        let started = std::time::Instant::now();
        handle.stop();
        handle.stop();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        child.lock().unwrap().wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn logcat_subprocess_streams_entries() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fake_adb = env::temp_dir().join(format!(
            "ciallogcat-fake-adb-{}-{unique}",
            std::process::id()
        ));
        fs::write(
            &fake_adb,
            "#!/bin/sh\nprintf '%s\\n' '08-29 15:42:10.123  1234  1260 I MainActivity: ready'\n",
        )
        .unwrap();
        fs::set_permissions(&fake_adb, fs::Permissions::from_mode(0o700)).unwrap();

        let (event_tx, event_rx) = mpsc::sync_channel(4096);
        let mut handle = start_logcat(
            &fake_adb,
            "test-device",
            &LogBuffer::defaults(),
            7,
            event_tx,
            WakeSignal::default(),
        )
        .unwrap();

        let mut entry = None;
        let mut exited = false;
        while !exited {
            match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                BackendEvent::Logcat {
                    session_id: 7,
                    event: LogcatEvent::Entry(log),
                } => entry = Some(log.into_inner()),
                BackendEvent::Logcat {
                    session_id: 7,
                    event: LogcatEvent::Exited { .. },
                } => exited = true,
                _ => {}
            }
        }

        handle.stop();
        fs::remove_file(fake_adb).unwrap();
        assert_eq!(entry.unwrap().message.as_ref(), "ready");
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "../tests/support/adb_integration.rs"]
mod integration_tests;

#[cfg(test)]
#[path = "adb_proto_tests.rs"]
mod proto_tests;
