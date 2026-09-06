use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc::SyncSender};

use eframe::egui::Context;
use regex::Regex;

use crate::model::{Level, LogBuffer, LogEntry};

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
    Exited {
        code: Option<i32>,
        error: Option<String>,
    },
}

const QUEUE_BYTES: usize = 16 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;

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
    _permit: QueuePermit,
}
impl QueuedLogEntry {
    pub fn into_inner(self) -> LogEntry {
        self.entry
    }
}
impl From<LogEntry> for QueuedLogEntry {
    fn from(entry: LogEntry) -> Self {
        static BUDGET: OnceLock<Arc<QueueBudget>> = OnceLock::new();
        let budget = Arc::clone(BUDGET.get_or_init(|| Arc::new(QueueBudget::default())));
        // The stream parser bounds lines before allocating queued entries.
        let bytes = (entry.payload_bytes() + std::mem::size_of::<LogEntry>()).min(QUEUE_BYTES);
        Self {
            entry,
            _permit: budget.reserve(bytes),
        }
    }
}

pub struct LogcatHandle {
    child: Arc<Mutex<Child>>,
    stopped: bool,
}

impl LogcatHandle {
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
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

pub fn spawn_device_query(adb: PathBuf, event_tx: SyncSender<BackendEvent>, ctx: Context) {
    std::thread::Builder::new()
        .name("adb-devices".to_owned())
        .spawn(move || {
            let result = run_adb(&adb, &["devices", "-l"]).map(|text| parse_devices(&text));
            let _ = event_tx.send(BackendEvent::Devices(result));
            ctx.request_repaint();
        })
        .expect("failed to start device query");
}

pub fn spawn_package_query(
    adb: PathBuf,
    serial: String,
    event_tx: SyncSender<BackendEvent>,
    ctx: Context,
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
            ctx.request_repaint();
        })
        .expect("failed to start package query");
}

pub fn spawn_process_query(
    adb: PathBuf,
    serial: String,
    event_tx: SyncSender<BackendEvent>,
    ctx: Context,
) {
    std::thread::Builder::new()
        .name("adb-processes".to_owned())
        .spawn(move || {
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
                result,
            });
            ctx.request_repaint();
        })
        .expect("failed to start process query");
}

pub fn start_logcat(
    adb: &Path,
    serial: &str,
    buffers: &[LogBuffer],
    session_id: u64,
    event_tx: SyncSender<BackendEvent>,
    ctx: Context,
) -> Result<LogcatHandle, String> {
    let mut command = adb_command(adb);
    command
        .args(logcat_args(serial, buffers))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start adb logcat: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "adb logcat did not provide stdout".to_owned())?;
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));

    if let Some(stderr) = stderr {
        let error_tx = event_tx.clone();
        let error_ctx = ctx.clone();
        std::thread::Builder::new()
            .name("adb-logcat-stderr".to_owned())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let _ = error_tx.send(BackendEvent::Logcat {
                        session_id,
                        event: LogcatEvent::Stderr(line),
                    });
                    error_ctx.request_repaint();
                }
            })
            .expect("failed to start adb stderr reader");
    }

    let reader_child = Arc::clone(&child);
    std::thread::Builder::new()
        .name("adb-logcat".to_owned())
        .spawn(move || {
            let _ = event_tx.send(BackendEvent::Logcat {
                session_id,
                event: LogcatEvent::Started,
            });
            ctx.request_repaint();
            let mut last_repaint = std::time::Instant::now();
            let stream_error = read_logcat_stream(BufReader::new(stdout), |entry| {
                let _ = event_tx.send(BackendEvent::Logcat {
                    session_id,
                    event: LogcatEvent::Entry(entry.into()),
                });
                if last_repaint.elapsed() >= std::time::Duration::from_millis(16) {
                    ctx.request_repaint();
                    last_repaint = std::time::Instant::now();
                }
            })
            .err()
            .map(|error| format!("failed to read logcat output: {error}"));

            let (code, error) = match reader_child.lock() {
                Ok(mut child) => match child.try_wait() {
                    Ok(Some(status)) => (status.code(), stream_error),
                    Ok(None) => {
                        let reason = stream_error
                            .unwrap_or_else(|| "Logcat output ended unexpectedly".to_owned());
                        match child.kill() {
                            Ok(()) => match child.wait() {
                                Ok(status) => (status.code(), Some(reason)),
                                Err(error) => (
                                    None,
                                    Some(format!("{reason}; failed to wait for ADB: {error}")),
                                ),
                            },
                            Err(error) => {
                                (None, Some(format!("{reason}; failed to stop ADB: {error}")))
                            }
                        }
                    }
                    Err(error) => (
                        None,
                        Some(format!("failed to query ADB process status: {error}")),
                    ),
                },
                Err(_) => (None, Some("adb process lock was poisoned".to_owned())),
            };
            let _ = event_tx.send(BackendEvent::Logcat {
                session_id,
                event: LogcatEvent::Exited { code, error },
            });
            ctx.request_repaint();
        })
        .map_err(|error| format!("failed to start logcat reader: {error}"))?;

    Ok(LogcatHandle {
        child,
        stopped: false,
    })
}

fn logcat_args(serial: &str, buffers: &[LogBuffer]) -> Vec<String> {
    let mut buffers = buffers.to_vec();
    LogBuffer::normalize(&mut buffers);
    let mut args: Vec<String> = ["-s", serial, "logcat", "-v", "threadtime", "-T", "1"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for buffer in buffers {
        args.extend(["-b".to_owned(), buffer.name().to_owned()]);
    }
    args
}

fn read_logcat_stream(
    mut reader: impl BufRead,
    mut on_entry: impl FnMut(LogEntry),
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
            let keep = take.min(MAX_LINE_BYTES.saturating_sub(line.len()));
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
        if truncated {
            line.extend_from_slice(b" [truncated: log line exceeded 1 MiB]");
        }
        let line = String::from_utf8_lossy(&line);
        if let Some(entry) = parse_logcat_line(&line) {
            on_entry(entry);
        }
    }
}

fn run_adb(adb: &Path, args: &[&str]) -> Result<String, String> {
    let output = adb_command(adb)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("failed to run {}: {error}", adb.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if stderr.is_empty() {
            format!("adb exited with {}", output.status)
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn adb_command(path: &Path) -> Command {
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
        return Some(LogEntry::marker(line.trim().to_owned()));
    };

    let level = captures
        .get(4)
        .and_then(|value| value.as_str().chars().next())
        .and_then(Level::from_letter)?;
    Some(LogEntry::new(
        captures.get(1)?.as_str().trim().to_owned(),
        captures.get(2)?.as_str().parse().ok()?,
        captures.get(3)?.as_str().parse().ok()?,
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
        assert_eq!(entries[1].message.as_ref(), "next");
    }

    #[test]
    fn consuming_queue_entry_releases_byte_reservation() {
        let budget = Arc::new(QueueBudget::default());
        *budget.used.lock().unwrap() = 123;
        let entry = QueuedLogEntry {
            entry: LogEntry::marker("data"),
            _permit: QueuePermit {
                budget: Arc::clone(&budget),
                bytes: 123,
            },
        };
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
            stopped: false,
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
            Context::default(),
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
