//! On-demand, read-only device observations. Call [`collect`] on a background
//! worker; cancellation never depends on the caller draining an event queue.
//! Commands are fixed here: there is no public shell/command execution API.

use std::io::{self, Read};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::adb::{adb_command, kill_and_reap};
use crate::model::LogBuffer;

pub const DEFAULT_OUTPUT_BYTES: usize = 16 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Debug)]
pub struct DiagnosticOptions {
    /// One monotonic deadline shared by all probes, including pipe draining.
    pub timeout: Duration,
    /// Also cap each probe, so a stuck query need not consume the whole run.
    pub probe_timeout: Duration,
    /// Raw bytes retained for EACH stdout and stderr, independently, per probe.
    /// Excess bytes are drained/discarded until exit, cancellation or deadline.
    pub max_output_bytes: usize,
}

impl Default for DiagnosticOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            probe_timeout: Duration::from_secs(2),
            max_output_bytes: DEFAULT_OUTPUT_BYTES,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Probe {
    AdbFeatures,
    LogcatHelp,
    BufferSizes,
    Statistics,
    PruneRules,
    AndroidSdk,
    AndroidRelease,
    BuildFingerprint,
}

const PROBES: [Probe; 8] = [
    Probe::AdbFeatures,
    Probe::LogcatHelp,
    Probe::BufferSizes,
    Probe::Statistics,
    Probe::PruneRules,
    Probe::AndroidSdk,
    Probe::AndroidRelease,
    Probe::BuildFingerprint,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    Success,
    PermissionDenied,
    Unsupported,
    TimedOut,
    Cancelled,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityObservation {
    Advertised,
    NotAdvertised,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutputObservation {
    /// Lossy UTF-8 rendering of the retained raw prefix; not trimmed or merged.
    pub text: String,
    pub retained_bytes: usize,
    pub observed_bytes: u64,
    pub truncated: bool,
    pub lossy_utf8: bool,
    /// EOF observed. False also covers cancellation before any output arrived.
    pub eof: bool,
    pub read_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExitObservation {
    /// ADB host process exit. Remote exit propagation requires shell_v2.
    pub code: Option<i32>,
    pub success: bool,
    pub description: String,
}

impl From<ExitStatus> for ExitObservation {
    fn from(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            success: status.success(),
            description: status.to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProbeObservation {
    pub probe: Probe,
    /// Actual ADB argv, including serial, for this fixed read-only command.
    pub arguments: Vec<String>,
    pub attempted: bool,
    pub status: ProbeStatus,
    pub exit: Option<ExitObservation>,
    pub stdout: OutputObservation,
    pub stderr: OutputObservation,
    pub error: Option<String>,
    pub started_unix_ms: Option<u64>,
    pub finished_unix_ms: Option<u64>,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiagnosticReport {
    pub schema_version: u8,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub serial: String,
    pub requested_buffers: Vec<LogBuffer>,
    pub started_unix_ms: Option<u64>,
    pub finished_unix_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    pub probe_timeout_ms: u64,
    pub max_output_bytes_per_stream: usize,
    /// Upper bound including JSON escaping, metadata and the CLI's final LF.
    pub max_json_bytes: usize,
    pub shell_v2: CapabilityObservation,
    /// False if any probe failed, was skipped, lost text, or exceeded its cap.
    pub complete: bool,
    pub cancelled: bool,
    pub deadline_exceeded: bool,
    pub observations: Vec<ProbeObservation>,
}

/// Validate before spawning anything; returns observations even if every query
/// fails. Only invalid input returns Err. Buffers must be nonempty, unique, and
/// `All` must occur alone. Empty/invalid values are never silently normalized.
///
/// Each child is killed and reaped on stop; no blocking pipe reader threads are
/// created. The deadline bounds collection, followed only by OS child cleanup.
/// Host stdout/stderr stay separate. On devices without advertised shell_v2,
/// remote stderr can still be merged by ADB and its exit code is host-only.
pub fn collect(
    adb: &Path,
    serial: &str,
    buffers: &[LogBuffer],
    options: &DiagnosticOptions,
    cancellation: &CancellationToken,
) -> Result<DiagnosticReport, String> {
    let started = Instant::now();
    let started_unix_ms = unix_ms();
    if serial.trim().is_empty()
        || serial.len() > 1024
        || serial.starts_with('-')
        || serial.chars().any(char::is_control)
    {
        return Err("invalid serial".into());
    }
    if buffers.is_empty()
        || buffers.len() > LogBuffer::CHOICES.len()
        || buffers
            .iter()
            .enumerate()
            .any(|(i, b)| buffers[..i].contains(b))
        || (buffers.contains(&LogBuffer::All) && buffers.len() != 1)
    {
        return Err("buffers must be unique and nonempty; all must occur alone".into());
    }
    if options.timeout.is_zero()
        || options.timeout > MAX_TIMEOUT
        || options.probe_timeout.is_zero()
        || options.probe_timeout > MAX_TIMEOUT
        || !(1..=MAX_OUTPUT_BYTES).contains(&options.max_output_bytes)
    {
        return Err(
            "diagnostic timeouts must be positive and <=60s; output cap must be 1..=65536 bytes"
                .into(),
        );
    }
    let deadline = started + options.timeout;
    let observations: Vec<_> = PROBES
        .into_iter()
        .map(|probe| observe(adb, serial, buffers, probe, options, deadline, cancellation))
        .collect();
    let features = &observations[0];
    let shell_v2 = if features.status != ProbeStatus::Success
        || features.stdout.lossy_utf8
        || features.stdout.truncated
        || !features.stdout.eof
        || features.stdout.text.trim().is_empty()
    {
        CapabilityObservation::Unknown
    } else if features
        .stdout
        .text
        .split(|c: char| c == ',' || c.is_whitespace())
        .any(|s| s == "shell_v2")
    {
        CapabilityObservation::Advertised
    } else {
        CapabilityObservation::NotAdvertised
    };
    Ok(DiagnosticReport {
        schema_version: 1,
        kind: "device_diagnostics",
        serial: serial.into(),
        requested_buffers: buffers.to_vec(),
        started_unix_ms,
        finished_unix_ms: unix_ms(),
        elapsed_ms: millis(started.elapsed()),
        timeout_ms: millis(options.timeout),
        probe_timeout_ms: millis(options.probe_timeout),
        max_output_bytes_per_stream: options.max_output_bytes,
        // At most 6 encoded bytes per input byte (control-character escaping),
        // including invalid UTF-8 replacement (3). Fixed/validated metadata
        // including bounded errors/serial/argv fits within the extra 256 KiB.
        max_json_bytes: PROBES.len() * 2 * options.max_output_bytes * 6 + 262144,
        shell_v2,
        complete: observations.iter().all(|o| {
            o.status == ProbeStatus::Success
                && [&o.stdout, &o.stderr]
                    .iter()
                    .all(|s| s.eof && !s.truncated && !s.lossy_utf8 && s.read_error.is_none())
        }),
        cancelled: observations
            .iter()
            .any(|o| o.status == ProbeStatus::Cancelled),
        deadline_exceeded: Instant::now() >= deadline,
        observations,
    })
}

fn arguments(serial: &str, buffers: &[LogBuffer], probe: Probe) -> Vec<String> {
    let mut args = vec!["-s".into(), serial.into()];
    if probe == Probe::AdbFeatures {
        args.push("features".into());
        return args;
    }
    let remote = match probe {
        Probe::LogcatHelp => "exec logcat --help".into(),
        Probe::BufferSizes | Probe::Statistics | Probe::PruneRules => {
            let mut remote = "exec logcat".to_owned();
            for buffer in buffers {
                remote.push_str(" -b ");
                remote.push_str(buffer.name());
            }
            remote.push_str(match probe {
                Probe::BufferSizes => " -g",
                Probe::Statistics => " -S",
                _ => " -p",
            });
            remote
        }
        Probe::AndroidSdk => "getprop ro.build.version.sdk".into(),
        Probe::AndroidRelease => "getprop ro.build.version.release".into(),
        Probe::BuildFingerprint => "getprop ro.build.fingerprint".into(),
        Probe::AdbFeatures => unreachable!(),
    };
    // All remote words are literals or typed buffer names. Serial is an ADB
    // argv value, never interpolated into the device shell command.
    args.extend(["shell".into(), "-T".into(), "-n".into(), remote]);
    args
}

fn observe(
    adb: &Path,
    serial: &str,
    buffers: &[LogBuffer],
    probe: Probe,
    options: &DiagnosticOptions,
    total_deadline: Instant,
    cancellation: &CancellationToken,
) -> ProbeObservation {
    let started = Instant::now();
    let deadline = total_deadline.min(started + options.probe_timeout);
    let args = arguments(serial, buffers, probe);
    let mut out = PipeCapture::default();
    let mut err = PipeCapture::default();
    let mut result = ProbeObservation {
        probe,
        arguments: args,
        attempted: false,
        status: ProbeStatus::Unknown,
        exit: None,
        stdout: PipeCapture::default().finish(),
        stderr: PipeCapture::default().finish(),
        error: None,
        started_unix_ms: unix_ms(),
        finished_unix_ms: None,
        elapsed_ms: 0,
    };
    if let Some(status) = stop_reason(cancellation, deadline) {
        result.status = status;
    } else {
        result.attempted = true;
        match adb_command(adb)
            .args(&result.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Err(error) => result.error = Some(bounded_error(error)),
            Ok(mut child) => {
                let mut stdout = child.stdout.take().expect("piped stdout");
                let mut stderr = child.stderr.take().expect("piped stderr");
                loop {
                    if let Some(status) = stop_reason(cancellation, deadline) {
                        result.status = status;
                        break;
                    }
                    let progress = out.read(&mut stdout, options.max_output_bytes)
                        | err.read(&mut stderr, options.max_output_bytes);
                    match child.try_wait() {
                        Ok(Some(exit)) => result.exit = Some(exit.into()),
                        Ok(None) => (),
                        Err(error) => {
                            result.error = Some(bounded_error(error));
                            break;
                        }
                    }
                    if result.exit.is_some() && out.done() && err.done() {
                        break;
                    }
                    if !progress {
                        std::thread::sleep(
                            POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                        );
                    }
                }
                if result.exit.is_none() {
                    match kill_and_reap(&mut child) {
                        Ok(exit) => result.exit = Some(exit.into()),
                        Err(error) => result.error = Some(bounded_error(error)),
                    }
                }
                // Read only immediately available tail bytes after termination.
                // Never wait for descendant-held pipes or spawn detached readers.
                for _ in 0..32 {
                    if !(out.read(&mut stdout, options.max_output_bytes)
                        | err.read(&mut stderr, options.max_output_bytes))
                    {
                        break;
                    }
                }
            }
        }
    }
    result.stdout = out.finish();
    result.stderr = err.finish();
    if result.status == ProbeStatus::Unknown {
        result.status = classify(&result);
    }
    result.finished_unix_ms = unix_ms();
    result.elapsed_ms = millis(started.elapsed());
    result
}

fn stop_reason(cancellation: &CancellationToken, deadline: Instant) -> Option<ProbeStatus> {
    if cancellation.is_cancelled() {
        Some(ProbeStatus::Cancelled)
    } else if Instant::now() >= deadline {
        Some(ProbeStatus::TimedOut)
    } else {
        None
    }
}

fn complete_error_lines(output: &OutputObservation) -> &str {
    if output.eof && !output.truncated {
        return &output.text;
    }
    // A retained prefix can end halfway through a statement. Only LF-terminated
    // lines establish a cause unless the entire stream reached EOF unchanged.
    output
        .text
        .rfind('\n')
        .map_or("", |end| &output.text[..=end])
}

fn classify(result: &ProbeObservation) -> ProbeStatus {
    if result.error.is_some()
        || result.stdout.read_error.is_some()
        || result.stderr.read_error.is_some()
    {
        return ProbeStatus::Unknown;
    }
    let Some(exit) = &result.exit else {
        return ProbeStatus::Unknown;
    };
    if exit.success {
        return ProbeStatus::Success;
    }
    let stderr = complete_error_lines(&result.stderr).to_ascii_lowercase();
    let stdout = complete_error_lines(&result.stdout).to_ascii_lowercase();
    let messages = stderr.lines().chain(stdout.lines()).map(|line| {
        let line = line.trim();
        let message = ["adb: ", "logcat: ", "getprop: "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .unwrap_or(line);
        message.strip_prefix("error: ").unwrap_or(message)
    });
    // Host transport failures say nothing about the remote probe's permission
    // or options. Match the outer structure: a serial may itself contain quotes
    // and error phrases. `features` can wrap the same error in another prefix.
    if messages.clone().any(|message| {
        let message = message
            .strip_prefix("failed to get feature set: ")
            .unwrap_or(message);
        (message.starts_with("device '") && message.ends_with("' not found"))
            || matches!(
                message,
                "device offline"
                    | "device still connecting"
                    | "device unauthorized."
                    | "no devices/emulators found"
                    | "more than one device/emulator"
                    | "transport disconnected"
            )
    }) {
        return ProbeStatus::Unknown;
    }
    let mut status = ProbeStatus::Unknown;
    for message in messages {
        // Accept a whole reason or the final reason of a failure statement,
        // e.g. `logcat: failed to read statistics: Permission denied`.
        let cause = message
            .strip_prefix("failed to ")
            .and_then(|failure| failure.rsplit_once(": ").map(|(_, cause)| cause))
            .unwrap_or(message);
        if matches!(
            cause,
            "permission denied"
                | "operation not permitted"
                | "not authorized"
                | "unauthorized"
                | "insufficient permissions"
        ) {
            return ProbeStatus::PermissionDenied;
        }
        if matches!(
            cause,
            "not supported" | "inaccessible or not found" | "command not found"
        ) || [
            "unknown option",
            "unrecognized option",
            "invalid option",
            "unsupported option",
            "unknown command",
        ]
        .iter()
        .any(|reason| {
            message
                .strip_prefix(reason)
                .is_some_and(|tail| tail.is_empty() || tail.starts_with([' ', ':']))
        }) {
            status = ProbeStatus::Unsupported;
        }
    }
    status
}

#[derive(Default)]
struct PipeCapture {
    bytes: Vec<u8>,
    observed: u64,
    eof: bool,
    error: Option<String>,
}

impl PipeCapture {
    fn done(&self) -> bool {
        self.eof || self.error.is_some()
    }

    fn read(&mut self, pipe: &mut impl PollPipe, cap: usize) -> bool {
        if self.done() {
            return false;
        }
        let result: io::Result<Option<usize>> = (|| {
            let Some(available) = pipe.available()? else {
                return Ok(None);
            };
            let mut buffer = [0; 8192];
            let length = available.min(buffer.len());
            let n = if length == 0 {
                0
            } else {
                pipe.read(&mut buffer[..length])?
            };
            self.observed = self.observed.saturating_add(n as u64);
            self.bytes
                .extend_from_slice(&buffer[..n.min(cap.saturating_sub(self.bytes.len()))]);
            Ok(Some(n))
        })();
        match result {
            Ok(Some(0)) => self.eof = true,
            Ok(Some(_)) => return true,
            Ok(None) => (),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
            Err(error) => self.error = Some(bounded_error(error)),
        }
        false
    }

    fn finish(self) -> OutputObservation {
        OutputObservation {
            text: String::from_utf8_lossy(&self.bytes).into_owned(),
            retained_bytes: self.bytes.len(),
            observed_bytes: self.observed,
            truncated: self.observed > self.bytes.len() as u64,
            lossy_utf8: std::str::from_utf8(&self.bytes).is_err(),
            eof: self.eof,
            read_error: self.error,
        }
    }
}

/// Read only when ready, with a single reader owning each pipe. No blocking
/// read-to-end or reader thread can survive a cancelled query.
trait PollPipe: Read {
    fn available(&self) -> io::Result<Option<usize>>;
}

#[cfg(unix)]
impl<T: Read + std::os::fd::AsRawFd> PollPipe for T {
    fn available(&self) -> io::Result<Option<usize>> {
        #[repr(C)]
        struct PollFd {
            fd: i32,
            events: i16,
            revents: i16,
        }
        unsafe extern "C" {
            fn poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> i32;
        }
        let mut fd = PollFd {
            fd: self.as_raw_fd(),
            events: 1,
            revents: 0,
        };
        // SAFETY: one initialized pollfd, live exclusively owned pipe, timeout 0.
        let result = unsafe { poll(&mut fd, 1, 0) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else if fd.revents & 32 != 0 {
            Err(io::Error::other("invalid diagnostic pipe"))
        } else {
            // POLLIN | POLLERR | POLLHUP: read bytes or observe EOF.
            Ok((fd.revents & (1 | 8 | 16) != 0).then_some(8192))
        }
    }
}

#[cfg(windows)]
impl<T: Read + std::os::windows::io::AsRawHandle> PollPipe for T {
    fn available(&self) -> io::Result<Option<usize>> {
        use std::ffi::c_void;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn PeekNamedPipe(
                handle: *mut c_void,
                buffer: *mut c_void,
                size: u32,
                read: *mut u32,
                available: *mut u32,
                left: *mut u32,
            ) -> i32;
        }
        let mut available = 0;
        // SAFETY: live owned pipe handle, output pointer valid; other optional
        // pointers null. PeekNamedPipe also supports anonymous child pipes.
        let result = unsafe {
            PeekNamedPipe(
                self.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if result != 0 {
            Ok((available != 0).then_some(available as usize))
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(109) {
                Ok(Some(0))
            } else {
                Err(error)
            }
        }
    }
}

fn bounded_error(error: impl std::fmt::Display) -> String {
    let value = error.to_string();
    let mut end = value.len().min(1024);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].into()
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn unix_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(millis)
}
