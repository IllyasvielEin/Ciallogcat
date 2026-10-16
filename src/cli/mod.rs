//! Standalone CLI orchestration. Session files are local capabilities; PIDs are
//! descriptive only. The worker alone owns and stops its ADB child.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TrySendError};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ciallogcat::WakeSignal;
use ciallogcat::adb::{self, BackendEvent, LogcatEvent};
use ciallogcat::model::{LogBuffer, LogEntry};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const SCHEMA: u8 = 1;
const MAX_RECORD: u64 = 8 * 1024 * 1024;
const HELP: &str = "ciallogcat-cli: bounded local Android log capture (Linux/Windows)\n\nCommands:\n  devices [--adb PATH]\n  capture --serial SERIAL [--adb PATH] [--output FILE|-] [--duration-secs 600] [--max-bytes 67108864] [--max-entries N]\n  start --serial SERIAL --session-dir DIR [--adb PATH] [same capture bounds]\n  status --session-dir DIR\n  query --session-dir DIR [--after-seq 0] [--limit 100] [--query TEXT]\n  stop --session-dir DIR [--wait-secs 10]\n\nAll command results are JSON; capture writes JSONL entries (default stdout). Errors are JSON on stderr. New output files/session directories must not exist. Stop uses a private marker, never a persisted PID. Local sessions can contain sensitive device logs. No device logs are cleared.\n";

#[derive(Debug)]
struct Error {
    code: &'static str,
    message: String,
    exit: i32,
}
type Result<T> = std::result::Result<T, Error>;
fn error(code: &'static str, message: impl Into<String>, exit: i32) -> Error {
    Error {
        code,
        message: message.into(),
        exit,
    }
}
fn io_error(e: impl std::fmt::Display) -> Error {
    error("io_error", e.to_string(), 5)
}
fn usage(message: impl Into<String>) -> Error {
    error("invalid_arguments", message, 2)
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Serialize, Deserialize)]
struct Config {
    adb: PathBuf,
    serial: String,
    duration_secs: u64,
    max_bytes: u64,
    max_entries: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct Status {
    schema_version: u8,
    #[serde(rename = "type")]
    kind: String,
    session: String,
    worker_pid: u32,
    state: String,
    readiness: Option<String>,
    entries: u64,
    bytes: u64,
    reason: Option<String>,
    error: Option<String>,
    updated_unix_ms: u64,
    stale: bool,
    omitted_by_limit: bool,
    diagnostic_lines: u64,
    last_diagnostic: Option<String>,
    diagnostics_complete: bool,
    discarded_entries: u64,
    diagnostic_dropped_count: u64,
}
impl Status {
    fn new(session: &Path) -> Self {
        Self {
            schema_version: SCHEMA,
            kind: "status".into(),
            session: session.to_string_lossy().into_owned(),
            worker_pid: std::process::id(),
            state: "starting".into(),
            readiness: None,
            entries: 0,
            bytes: 0,
            reason: None,
            error: None,
            updated_unix_ms: now(),
            stale: false,
            omitted_by_limit: false,
            diagnostic_lines: 0,
            last_diagnostic: None,
            diagnostics_complete: false,
            discarded_entries: 0,
            diagnostic_dropped_count: 0,
        }
    }
    fn terminal(&self) -> bool {
        self.state == "completed" || self.state == "failed"
    }
}
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
fn new_file(path: &Path) -> Result<File> {
    private_options().open(path).map_err(io_error)
}
fn write_status(dir: &Path, status: &mut Status) -> Result<()> {
    status.updated_unix_ms = now();
    let temp = dir.join("status.next");
    let mut file = new_file(&temp)?;
    serde_json::to_writer(&mut file, status).map_err(io_error)?;
    file.write_all(b"\n").map_err(io_error)?;
    file.flush().map_err(io_error)?;
    drop(file);
    // Windows readers can briefly hold a sharing handle. Never expose a
    // partially rewritten status and never remove the published destination.
    let mut last = None;
    for _ in 0..10 {
        match fs::rename(&temp, dir.join("status.json")) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    let _ = fs::remove_file(&temp);
    Err(io_error(last.expect("rename attempted")))
}
fn regular(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(io_error)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(error(
            "invalid_session",
            "session file is not a regular file",
            3,
        ));
    }
    Ok(())
}
fn session_path(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    let meta =
        fs::symlink_metadata(&path).map_err(|e| error("invalid_session", e.to_string(), 3))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(error(
            "invalid_session",
            "session directory must be a real directory",
            3,
        ));
    }
    fs::canonicalize(path).map_err(io_error)
}
fn read_limited(path: &Path, max: u64) -> Result<Vec<u8>> {
    regular(path)?;
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(io_error)?
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > max {
        return Err(error(
            "invalid_session",
            "session metadata exceeds size limit",
            3,
        ));
    }
    Ok(bytes)
}
fn load_status(dir: &Path) -> Result<Status> {
    let path = dir.join("status.json");
    regular(&path)?;
    let bytes = read_limited(&path, 131072)?;
    let mut status: Status =
        serde_json::from_slice(&bytes).map_err(|e| error("invalid_session", e.to_string(), 3))?;
    if status.schema_version != SCHEMA {
        return Err(error("invalid_session", "unsupported session schema", 3));
    }
    if !status.terminal() && now().saturating_sub(status.updated_unix_ms) > 5000 {
        status.state = "unknown".into();
        status.stale = true;
    }
    Ok(status)
}
fn emit(value: &impl Serialize) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value).map_err(io_error)?;
    out.write_all(b"\n").map_err(io_error)
}
fn parse_args() -> Result<(String, BTreeMap<String, String>)> {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| usage("arguments must be valid Unicode"))
        })
        .collect::<Result<_>>()?;
    let mut args = args.into_iter();
    let command = args.next().unwrap_or_else(|| "help".into());
    let mut flags = BTreeMap::new();
    while let Some(flag) = args.next() {
        if !flag.starts_with("--") {
            return Err(usage(format!("unexpected argument: {flag}")));
        }
        let value = args
            .next()
            .ok_or_else(|| usage(format!("missing value for {flag}")))?;
        if value.starts_with("--") {
            return Err(usage(format!("missing value for {flag}")));
        }
        if flags.insert(flag.clone(), value).is_some() {
            return Err(usage(format!("duplicate option: {flag}")));
        }
    }
    Ok((command, flags))
}
fn required(flags: &mut BTreeMap<String, String>, name: &str) -> Result<String> {
    flags
        .remove(name)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| usage(format!("required option: {name}")))
}
fn number(
    flags: &mut BTreeMap<String, String>,
    name: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64> {
    let n = match flags.remove(name) {
        Some(s) => s
            .parse()
            .map_err(|_| usage(format!("{name} must be an integer")))?,
        None => default,
    };
    if !(min..=max).contains(&n) {
        return Err(usage(format!("{name} must be between {min} and {max}")));
    }
    Ok(n)
}
fn config(flags: &mut BTreeMap<String, String>) -> Result<Config> {
    let serial = required(flags, "--serial")?;
    if serial.len() > 1024 || serial.starts_with('-') || serial.chars().any(char::is_control) {
        return Err(usage("invalid serial"));
    }
    let adb = match flags.remove("--adb") {
        Some(s) => fs::canonicalize(s).map_err(|e| error("adb_unavailable", e.to_string(), 4))?,
        None => fs::canonicalize(
            adb::find_adb()
                .ok_or_else(|| error("adb_unavailable", "ADB not found; provide --adb PATH", 4))?,
        )
        .map_err(io_error)?,
    };
    Ok(Config {
        adb,
        serial,
        duration_secs: number(flags, "--duration-secs", 600, 1, 86400)?,
        max_bytes: number(flags, "--max-bytes", 67108864, 1, 1073741824)?,
        max_entries: number(flags, "--max-entries", 10000000, 1, 10000000)?,
    })
}
fn no_flags(flags: &BTreeMap<String, String>) -> Result<()> {
    if let Some(key) = flags.keys().next() {
        Err(usage(format!("unknown option: {key}")))
    } else {
        Ok(())
    }
}

pub fn run() -> i32 {
    let result = execute();
    match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(
                io::stderr(),
                "{}",
                json!({"schema_version":SCHEMA,"type":"error","error":{"code":e.code,"message":e.message}})
            );
            e.exit
        }
    }
}
fn execute() -> Result<()> {
    let (command, mut flags) = parse_args()?;
    match command.as_str() {
        "help" | "--help" | "-h" => {
            no_flags(&flags)?;
            print!("{HELP}");
            Ok(())
        }
        "--version" | "version" => {
            no_flags(&flags)?;
            emit(
                &json!({"schema_version":SCHEMA,"type":"version","version":env!("CARGO_PKG_VERSION")}),
            )
        }
        "devices" => {
            let adb = match flags.remove("--adb") {
                Some(p) => PathBuf::from(p),
                None => adb::find_adb().ok_or_else(|| {
                    error("adb_unavailable", "ADB not found; provide --adb PATH", 4)
                })?,
            };
            no_flags(&flags)?;
            let (tx, rx) = mpsc::sync_channel(16);
            adb::spawn_device_query(adb, tx, WakeSignal::default());
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(BackendEvent::Devices(Ok(devices))) => emit(
                    &json!({"schema_version":SCHEMA,"type":"devices","devices":devices.into_iter().map(|d| json!({"serial":d.serial,"state":d.state,"model":d.model,"transport":d.transport})).collect::<Vec<_>>()}),
                ),
                Ok(BackendEvent::Devices(Err(e))) => Err(error("adb_error", e, 4)),
                _ => Err(error("timeout", "device discovery timed out", 6)),
            }
        }
        "capture" => {
            let config = config(&mut flags)?;
            let output = flags.remove("--output").unwrap_or_else(|| "-".into());
            no_flags(&flags)?;
            let writer: Box<dyn Write + Send> = if output == "-" {
                Box::new(io::stdout())
            } else {
                Box::new(new_file(Path::new(&output))?)
            };
            let status = capture(config, None, writer)?;
            writeln!(
                io::stderr(),
                "{}",
                serde_json::to_string(&status).map_err(io_error)?
            )
            .map_err(io_error)
        }
        "start" => {
            let config = config(&mut flags)?;
            let dir = PathBuf::from(required(&mut flags, "--session-dir")?);
            no_flags(&flags)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&dir)
                    .map_err(io_error)?;
            }
            #[cfg(not(unix))]
            fs::create_dir(&dir).map_err(io_error)?;
            let dir = fs::canonicalize(dir).map_err(io_error)?;
            serde_json::to_writer(new_file(&dir.join("config.json"))?, &config)
                .map_err(io_error)?;
            let mut status = Status::new(&dir);
            write_status(&dir, &mut status)?;
            let mut cmd = Command::new(std::env::current_exe().map_err(io_error)?);
            cmd.arg("__worker")
                .arg("--session-dir")
                .arg(&dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000);
            }
            let mut worker = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    status.state = "failed".into();
                    status.reason = Some("worker_spawn".into());
                    status.error = Some(e.to_string());
                    write_status(&dir, &mut status)?;
                    return Err(io_error(e));
                }
            };
            let until = Instant::now() + Duration::from_secs(10);
            loop {
                let current = load_status(&dir)?;
                if current.state == "failed" {
                    return Err(error(
                        "adb_error",
                        current
                            .error
                            .unwrap_or_else(|| "capture worker failed".into()),
                        4,
                    ));
                }
                if current.readiness.is_some() {
                    std::thread::sleep(Duration::from_millis(100));
                    let current = load_status(&dir)?;
                    if current.state == "failed" {
                        return Err(error(
                            "adb_error",
                            current
                                .error
                                .unwrap_or_else(|| "capture worker failed".into()),
                            4,
                        ));
                    }
                    return emit(&current);
                }
                if worker.try_wait().map_err(io_error)?.is_some() {
                    return Err(error(
                        "session_failed",
                        "capture worker exited before readiness",
                        3,
                    ));
                }
                if Instant::now() >= until {
                    let _ = new_file(&dir.join("stop"));
                    return Err(error(
                        "timeout",
                        "worker readiness timed out; stop requested",
                        6,
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        "__worker" => {
            let dir = session_path(&required(&mut flags, "--session-dir")?)?;
            no_flags(&flags)?;
            regular(&dir.join("config.json"))?;
            // create_new prevents two workers from owning the same session.
            let _lock = new_file(&dir.join("worker.lock"))?;
            let bytes = read_limited(&dir.join("config.json"), 16384)?;
            if bytes.len() > 16384 {
                return Err(error("invalid_session", "config exceeds size limit", 3));
            }
            let config: Config = serde_json::from_slice(&bytes).map_err(io_error)?;
            if config.duration_secs == 0
                || config.duration_secs > 86400
                || config.max_bytes == 0
                || config.max_bytes > 1073741824
                || config.max_entries == 0
                || config.max_entries > 10000000
            {
                return Err(error("invalid_session", "invalid capture bounds", 3));
            }
            let writer = Box::new(new_file(&dir.join("logs.jsonl"))?);
            capture(config, Some(&dir), writer).map(|_| ())
        }
        "status" | "stop" | "query" => {
            let dir_value = required(&mut flags, "--session-dir")?;
            let (after, limit, needle) = if command == "query" {
                (
                    number(&mut flags, "--after-seq", 0, 0, u64::MAX)?,
                    number(&mut flags, "--limit", 100, 1, 10000)?,
                    flags.remove("--query").unwrap_or_default(),
                )
            } else {
                (0, 100, String::new())
            };
            let dir = session_path(&dir_value)?;
            match command.as_str() {
                "status" => {
                    no_flags(&flags)?;
                    emit(&load_status(&dir)?)
                }
                "query" => {
                    no_flags(&flags)?;
                    query(&dir, after, limit, &needle)
                }
                _ => {
                    let wait = number(&mut flags, "--wait-secs", 10, 1, 30)?;
                    no_flags(&flags)?;
                    let status = load_status(&dir)?;
                    if status.terminal() {
                        return emit(&status);
                    }
                    if status.stale {
                        return Err(error(
                            "stale_session",
                            "worker heartbeat is stale; no PID was signalled",
                            3,
                        ));
                    }
                    match new_file(&dir.join("stop")) {
                        Ok(_) => (),
                        Err(_) if dir.join("stop").is_file() => (),
                        Err(e) => return Err(e),
                    }
                    let deadline = Instant::now() + Duration::from_secs(wait);
                    loop {
                        let status = load_status(&dir)?;
                        if status.terminal() {
                            return emit(&status);
                        }
                        if Instant::now() >= deadline {
                            return Err(error(
                                "timeout",
                                "stop requested but worker has not acknowledged completion",
                                6,
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
            }
        }
        _ => Err(usage(format!("unknown command: {command}; use --help"))),
    }
}
fn record(entry: LogEntry, seq: u64) -> Value {
    json!({"schema_version":SCHEMA,"type":"entry","seq":seq,"time":entry.time.as_ref(),"pid":entry.pid,"tid":entry.tid,"level":entry.level.short(),"tag":entry.tag.as_ref(),"message":entry.message.as_ref()})
}

fn capture(
    config: Config,
    dir: Option<&Path>,
    mut writer: Box<dyn Write + Send>,
) -> Result<Status> {
    let mut status = Status::new(dir.unwrap_or_else(|| Path::new("")));
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&interrupted);
    ctrlc::set_handler(move || {
        signal_flag.store(true, Ordering::Release);
    })
    .map_err(io_error)?;
    let confirmed_entries = Arc::new(AtomicU64::new(0));
    let confirmed_bytes = Arc::new(AtomicU64::new(0));
    let wc = Arc::clone(&confirmed_entries);
    let wb = Arc::clone(&confirmed_bytes);
    let (write_tx, write_rx) = mpsc::sync_channel::<Vec<u8>>(4);
    let (done_tx, done_rx) = mpsc::channel();
    let (progress_tx, progress_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result: io::Result<()> = (|| {
            for bytes in write_rx {
                writer.write_all(&bytes)?;
                wb.fetch_add(bytes.len() as u64, Ordering::Release);
                wc.fetch_add(1, Ordering::Release);
                let _ = progress_tx.try_send(());
            }
            writer.flush()
        })();
        let _ = done_tx.send(result);
    });
    let (tx, rx) = mpsc::sync_channel(128);
    let mut child = match adb::start_logcat(
        &config.adb,
        &config.serial,
        &LogBuffer::defaults(),
        1,
        tx,
        WakeSignal::default(),
    ) {
        Ok(child) => child,
        Err(e) => {
            status.state = "failed".into();
            status.reason = Some("adb_spawn".into());
            status.error = Some(e.clone());
            if let Some(dir) = dir {
                write_status(dir, &mut status)?;
            }
            return Err(error("adb_error", e, 4));
        }
    };
    let deadline = Instant::now() + Duration::from_secs(config.duration_secs);
    let mut heartbeat = Instant::now();
    let mut failure = None;
    let mut received_entries = 0u64;
    let mut accepted_entries = 0;
    let mut accepted_bytes = 0;
    let mut writer_done = None;
    let mut saw_exited = false;
    'capture: loop {
        if interrupted.load(Ordering::Acquire) {
            status.reason = Some("interrupted".into());
            break;
        }
        if Instant::now() >= deadline {
            status.reason = Some("duration_limit".into());
            break;
        }
        if dir.is_some_and(|d| d.join("stop").exists()) {
            status.reason = Some("stopped".into());
            break;
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(BackendEvent::Logcat { event, .. }) => match event {
                LogcatEvent::Started => {
                    status.state = "running".into();
                    status.readiness = Some("adb_process_started".into());
                    if let Some(dir) = dir {
                        write_status(dir, &mut status)?;
                    }
                }
                LogcatEvent::Entry(entry) => {
                    received_entries += 1;
                    let mut bytes =
                        serde_json::to_vec(&record(entry.into_inner(), accepted_entries + 1))
                            .map_err(io_error)?;
                    bytes.push(b'\n');
                    let size = bytes.len() as u64;
                    if accepted_bytes + size > config.max_bytes {
                        status.reason = Some("max_bytes".into());
                        status.omitted_by_limit = true;
                        break;
                    }
                    loop {
                        match write_tx.try_send(bytes) {
                            Ok(()) => break,
                            Err(TrySendError::Disconnected(_)) => {
                                failure =
                                    Some(error("output_error", "output writer disconnected", 5));
                                break 'capture;
                            }
                            Err(TrySendError::Full(pending)) => {
                                bytes = pending;
                                if let Ok(result) = done_rx.try_recv() {
                                    writer_done = Some(result);
                                    failure =
                                        Some(error("output_error", "output writer failed", 5));
                                    break 'capture;
                                }
                                if Instant::now() >= deadline {
                                    failure = Some(error(
                                        "output_backpressure",
                                        "output stalled until capture deadline",
                                        5,
                                    ));
                                    break 'capture;
                                }
                                if dir.is_some_and(|d| d.join("stop").exists()) {
                                    status.reason = Some("stopped".into());
                                    break 'capture;
                                }
                                if heartbeat.elapsed() >= Duration::from_millis(500) {
                                    status.entries = confirmed_entries.load(Ordering::Acquire);
                                    status.bytes = confirmed_bytes.load(Ordering::Acquire);
                                    if let Some(dir) = dir {
                                        write_status(dir, &mut status)?;
                                    }
                                    heartbeat = Instant::now();
                                }
                                if interrupted.load(Ordering::Acquire) {
                                    status.reason = Some("interrupted".into());
                                    break 'capture;
                                }
                                let _ = progress_rx.recv_timeout(Duration::from_millis(25));
                            }
                        }
                    }
                    accepted_entries += 1;
                    accepted_bytes += size;
                    if accepted_entries >= config.max_entries {
                        status.reason = Some("max_entries".into());
                        status.omitted_by_limit = true;
                        break;
                    }
                }
                LogcatEvent::Stderr(line) => {
                    status.diagnostic_lines += 1;
                    status.last_diagnostic = Some(line);
                }
                LogcatEvent::Exited { code, error: e } => {
                    saw_exited = true;
                    if interrupted.load(Ordering::Acquire) {
                        status.reason = Some("interrupted".into());
                    } else if e.is_some() || code != Some(0) {
                        failure = Some(error(
                            "adb_error",
                            e.unwrap_or_else(|| format!("ADB exited with code {code:?}")),
                            4,
                        ));
                    } else {
                        status.reason = Some("adb_exit".into());
                    }
                    break;
                }
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                failure = Some(error("adb_error", "backend channel disconnected", 4));
                break;
            }
            _ => (),
        }
        if heartbeat.elapsed() >= Duration::from_millis(500) {
            status.entries = confirmed_entries.load(Ordering::Acquire);
            status.bytes = confirmed_bytes.load(Ordering::Acquire);
            if let Some(dir) = dir {
                write_status(dir, &mut status)?;
            }
            heartbeat = Instant::now();
        }
    }
    child.stop();
    // Both backend readers own the only senders. Disconnection proves diagnostic
    // EOF, and receiving Exited proves the backend reaped its owned ADB child.
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = drain_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            failure = Some(error(
                "adb_error",
                "backend readers did not finish within two seconds",
                4,
            ));
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(BackendEvent::Logcat {
                event: LogcatEvent::Stderr(line),
                ..
            }) => {
                status.diagnostic_lines += 1;
                status.last_diagnostic = Some(line);
            }
            Ok(BackendEvent::Logcat {
                event: LogcatEvent::Exited { error: e, .. },
                ..
            }) => {
                saw_exited = true;
                if let Some(e) = e {
                    failure = Some(error("adb_error", e, 4));
                }
            }
            Ok(BackendEvent::Logcat {
                event: LogcatEvent::Entry(_),
                ..
            }) => {
                received_entries += 1;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                status.diagnostics_complete = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                failure = Some(error(
                    "adb_error",
                    "backend readers did not finish within two seconds",
                    4,
                ));
                break;
            }
            _ => (),
        }
    }
    if !saw_exited && failure.is_none() {
        failure = Some(error(
            "adb_error",
            "backend ended without confirming owned ADB child exit",
            4,
        ));
    }
    drop(rx);
    drop(write_tx);
    let result = match writer_done {
        Some(r) => Ok(r),
        None => done_rx.recv_timeout(Duration::from_secs(2)),
    };
    match result {
        Ok(Ok(())) => (),
        Ok(Err(e)) => failure = Some(error("output_error", e.to_string(), 5)),
        Err(_) => {
            failure = Some(error(
                "output_backpressure",
                "output writer did not drain within two seconds",
                5,
            ))
        }
    }
    status.entries = confirmed_entries.load(Ordering::Acquire);
    status.bytes = confirmed_bytes.load(Ordering::Acquire);
    status.state = if failure.is_some() {
        "failed"
    } else {
        "completed"
    }
    .into();
    if let Some(ref mut e) = failure {
        if e.code == "adb_error"
            && let Some(ref diagnostic) = status.last_diagnostic
        {
            e.message.push_str(": ");
            e.message.push_str(diagnostic);
        }
        status.reason = Some(e.code.into());
        status.error = Some(e.message.clone());
    }
    status.discarded_entries = received_entries.saturating_sub(status.entries);
    status.diagnostic_dropped_count = status.diagnostic_lines.saturating_sub(1);
    if let Some(dir) = dir {
        write_status(dir, &mut status)?;
    } else if failure.is_some() {
        let _ = writeln!(
            io::stderr(),
            "{}",
            serde_json::to_string(&status).map_err(io_error)?
        );
    }
    if let Some(e) = failure {
        Err(e)
    } else if interrupted.load(Ordering::Acquire) {
        if dir.is_none() {
            let _ = writeln!(
                io::stderr(),
                "{}",
                serde_json::to_string(&status).map_err(io_error)?
            );
        }
        Err(error(
            "interrupted",
            "capture interrupted; owned ADB child stopped",
            130,
        ))
    } else {
        Ok(status)
    }
}
fn query(dir: &Path, after: u64, limit: u64, needle: &str) -> Result<()> {
    let _ = load_status(dir)?;
    let path = dir.join("logs.jsonl");
    regular(&path)?;
    let file = File::open(path).map_err(io_error)?;
    if file.metadata().map_err(io_error)?.len() > 1073741824 {
        return Err(error(
            "invalid_session",
            "logs exceed session size limit",
            3,
        ));
    }
    let matcher = ciallogcat::filter::compile_query(needle, false, true).map_err(usage)?;
    let mut reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut next = after;
    let mut partial = false;
    let mut more = false;
    let mut total = 0u64;
    let mut previous_seq = 0u64;
    let mut scanned_bytes = 0u64;
    let mut scanned_records = 0u64;
    let mut scan_limited = false;
    loop {
        let mut line = Vec::new();
        let n = reader
            .by_ref()
            .take(MAX_RECORD + 1)
            .read_until(b'\n', &mut line)
            .map_err(io_error)?;
        if n == 0 {
            break;
        }
        if line.len() as u64 > MAX_RECORD {
            return Err(error("invalid_session", "record exceeds size limit", 3));
        }
        if !line.ends_with(b"\n") {
            partial = true;
            break;
        }
        let value: Value = serde_json::from_slice(&line)
            .map_err(|e| error("invalid_session", e.to_string(), 3))?;
        let seq = value["seq"]
            .as_u64()
            .ok_or_else(|| error("invalid_session", "record has no sequence", 3))?;
        if value["schema_version"] != SCHEMA
            || value["type"] != "entry"
            || seq <= previous_seq
            || !value["message"].is_string()
        {
            return Err(error(
                "invalid_session",
                "invalid log record schema or sequence",
                3,
            ));
        }
        previous_seq = seq;
        if seq <= after {
            continue;
        }
        if scanned_bytes + line.len() as u64 > 16 * 1024 * 1024 || scanned_records >= 100000 {
            scan_limited = true;
            more = true;
            break;
        }
        let matches = matcher.as_ref().is_none_or(|matcher| {
            value["message"]
                .as_str()
                .is_some_and(|s| matcher.is_match(s))
        });
        if matches && (entries.len() as u64 >= limit || total + line.len() as u64 > MAX_RECORD) {
            more = true;
            break;
        }
        next = seq;
        scanned_records += 1;
        scanned_bytes += line.len() as u64;
        if matches {
            total += line.len() as u64;
            entries.push(value);
        }
    }
    emit(
        &json!({"schema_version":SCHEMA,"type":"query","entries":entries,"next_after_seq":next,"partial":partial,"has_more":more,"scan_limited":scan_limited,"scanned_records":scanned_records}),
    )
}
