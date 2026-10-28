//! Standalone CLI orchestration. Session files are local capabilities; PIDs are
//! descriptive only. The worker alone owns and stops its ADB child.
mod capture_v2;
mod query_filter;
mod query_v2;
mod session_management;
mod text_output;
mod writer;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TrySendError};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ciallogcat::WakeSignal;
use ciallogcat::adb::{self, BackendEvent, LogcatEvent};
use ciallogcat::capture_scope::{CaptureScope, parse_buffers};
use ciallogcat::model::{LogBuffer, LogEntry};
use ciallogcat::record_io::{
    InputMode, LegacyReader, LegacyRecord, MAX_RECORD_BYTES, ReadOutcome, RecordErrorKind,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

const SCHEMA: u8 = 1;
const MAX_RECORD: u64 = MAX_RECORD_BYTES;
const HELP: &str = "ciallogcat-cli: bounded local Android log capture (Linux/Windows)

Commands:
  devices [--adb PATH]
  diagnose --serial SERIAL [--adb PATH] [--buffers LIST] [--timeout-ms 10000]
  capture --serial SERIAL [--adb PATH] [--output FILE|-] [--duration-secs 600] [--max-bytes 1073741824] [--max-entries N]
  start --serial SERIAL --session-dir DIR [--adb PATH] [same capture bounds]
  capture/start options: [--record-format v1|v2|v3] [--transport text|proto]
  v3 requires explicit --transport proto; defaults remain v1 and text.
  capture/start history: [--tail N | --since TIME] [--dump] [--buffers LIST]
  status --session-dir DIR
  query --session-dir DIR [--after-seq 0] [--limit 100] [--query TEXT] (v1 only)
  query-v2 (--file FILE | --session-dir DIR) [OPTIONS]
  get-v2 (--file FILE | --session-dir DIR) --ref TOKEN [OPTIONS]
  context-v2 (--file FILE | --session-dir DIR) --ref TOKEN [OPTIONS]
  stop --session-dir DIR [--wait-secs 10] [--force]

Use query-v2 --help, get-v2 --help or context-v2 --help for focused help.
Options use --name VALUE; --dump, --all, --regex, --ignore-case,
--case-sensitive, --force and --help/-h take no value. Help is plain text on stdout.
Command results are JSON; capture writes JSONL records (default stdout).
Errors are JSON on stderr. New output files/session directories must not exist.
Stop requests cooperative exit even for stale sessions, never signals a persisted PID.
After the wait, --force administratively closes an unacknowledged session with a
frozen published query boundary; it does NOT kill or confirm worker/ADB exit.
Local sessions can contain
sensitive device logs. No device logs are cleared.

";
const CAPTURE_SCOPE_HELP: &str = "Device history (capture and start, all record formats):
  --tail N        Recent N records, then follow; default 1 (-T 1).
                  ASCII digits only; range 1..=2147483647.
  --since TIME    Follow from device time; mutually exclusive with --tail.
                  MM-DD HH:MM:SS.fraction or YYYY-MM-DD HH:MM:SS.fraction,
                  or Unix SECONDS.fraction; fraction requires 1..=9 digits.
                  Quote calendar times; they use the device clock/timezone.
                  Integer Unix seconds are rejected (logcat treats them as N).
  --dump          Finite snapshot: all available history (-d), or --tail/--since
                  selection (-t N/TIME). Successful EOF completes the snapshot.
  --buffers LIST  Comma-separated main,system,crash,events,radio or all alone.
                  Default main,system,crash (same as GUI); no duplicates.
Host bounds remain active for history and follow: --duration-secs (600; 1..86400),
--max-entries (10000000; 1..10000000), --max-bytes (1073741824; 1..1073741824,
v2/v3 minimum 8192). --tail is device history selection, not a host record limit.

";
const DIAGNOSE_HELP: &str = "diagnose --serial SERIAL [OPTIONS]
  --adb PATH             ADB executable (otherwise normal ADB discovery).
  --buffers LIST         main,system,crash by default; same names as capture.
  --timeout-ms N         Total deadline, default 10000; range 1..=60000.
  --probe-timeout-ms N   Per-query timeout, default 2000; range 1..=60000.
  --max-output-bytes N   Raw bytes per stdout AND stderr, per query.
                         Default 16384; range 1..=65536, independently capped.
Reads actual logcat --help, -g, -S, -p, ADB features and Android build/API
properties. No device configuration is changed and no logs are cleared.
JSON retains each query's stdout/stderr, exit, status, truncation and host time.
Exit 0 means the report was produced; inspect complete and observations for
permission_denied/unsupported/timed_out/unknown. Ctrl-C emits a partial report
after child cleanup and exits 130. Host budgets are not device logd capacity.
Missing help entries and Android SDK level do not prove OEM feature support.
Without advertised shell_v2, remote stderr/exit separation is not guaranteed.

";
const V2_SOURCE_HELP: &str = "Source (required, exactly one):
  --file FILE          Offline v1 JSONL or finalized native v2/v3 artifact.
  --session-dir DIR    Published prefix of a v1/v2/v3 session, including active capture.
These commands do not call ADB. Tokens bind the source file and local signing key;
after copying a file, query it again to obtain new references/cursors.

";
const V2_RESPONSE_HELP: &str = "Response budget (all v2 commands):
  --max-response-bytes N  Default 65536; range 8192..=1048576 bytes.
                         Includes JSON envelope, escaped strings and final LF.
                         A budget too small for the envelope/reference is an error.

";
const V2_SCAN_HELP: &str =
    "Preview/scan budgets (query-v2 and context-v2; accepted but unused by get-v2):
  --preview-bytes N     Default 2048; range 0..=16384 UTF-8 message bytes per record.
                       Preserves code points; response budget may shorten previews.
  --max-scan-bytes N    Default 16777216; range 8388609..=67108864 bytes.
  --max-scan-records N  Default 100000; range 1..=1000000 candidate records.
Scan budgets bound candidate work, not total file I/O; validation/control reads
are extra. Inspect scan.file_bytes_read for actual log-file reads. Forward scans
reserve space for an 8 MiB record and may stop before using the full byte budget.

";
const QUERY_V2_HELP: &str = "query-v2 (--file FILE | --session-dir DIR) [OPTIONS]
  --mode snapshot|tail  Default snapshot. Snapshot fixes the first page's upper
                        bound; tail refreshes it per call and requires --session-dir.
  --query TEXT          Default empty (all); case-sensitive literal message match,
                        at most 4096 UTF-8 bytes. Same message predicate as GUI.
  --regex               Interpret --query as a Rust regular expression.
  --ignore-case         Case-insensitive message matching; default case-sensitive.
  --case-sensitive      Explicit default; conflicts with --ignore-case.
  --tag TEXT            Case-sensitive literal Tag substring (--tag-include alias).
  --tag-exclude TEXT    Exclude a case-sensitive literal Tag substring.
  --package TEXT        Shared GUI package/process matching (when stored).
  --pid N --tid N       Exact unsigned process/thread IDs, combined with AND.
  --min-level LEVEL     V/D/I/W/E/F or full name; default V.
  --since TIME          Inclusive UTC instant; --until TIME is exclusive.
                        RFC3339 with timezone, or integer age: 30s, 5m, 2h, 1d.
                        Relative bounds anchor once and remain fixed in cursor.
  --time-basis received|device  Default received: host observed_unix_ms.
                        Device requires exact protobuf epoch. Missing requested
                        time on otherwise matching records is time_unavailable;
                        old/yearless text times are never assigned a guessed year.
  --format json|jsonl|text  Default json: existing bounded preview page.
                        jsonl/text stream FULL decoded records, not previews;
                        continuation/summary JSON goes only to stderr.
  --time-format readable|epoch|rfc3339  Text only; default readable.
                        Uses --time-basis (received by default) for display too.
                        Missing absolute time is explicitly labeled, never guessed.
  --timezone local|utc  Text only; default local. Epoch ignores this setting.
  --process-width N     Text process column display width; default 32, 0 unlimited.
                        Middle ellipsis preserves prefix/suffix, not full identity.
  --all                 Automatically scan every page of a fixed snapshot.
                        Defaults to jsonl; requires jsonl/text, not tail mode.
                        Without --all, even text/jsonl returns only one page.
  --limit N             Default 100; range 1..=1000 returned records per page.
  --cursor TOKEN        Continue with next_cursor; cannot combine with --start-at.
  --start-at beginning|end  Default beginning. End requires native v2/v3 session
                           + --mode tail, without --cursor. Returns an empty page
                           at the published boundary; the prefix is not scanned.
Both modes default to the first record. Keep source, --mode and all filters unchanged
when resuming; --limit and budgets may change. Response/scan budgets
can stop a page before --limit. Follow next_cursor while has_more, even if records
is empty. scan.stop_reason reports upper_bound/record_limit/scan_budget/response_budget.
complete means this snapshot/current tail boundary was scanned, NOT capture completeness.
Tail does not wait: when has_more=false, reuse next_cursor later for new data.
An exhausted snapshot stays fixed; omit --cursor to see new data in a new snapshot.
JSON pages contain previews and record_ref; use get-v2 for full original record JSON.
Streaming keeps bounded pages/records in memory, never the entire result. Page budgets
control work per chunk, not total streaming stdout bytes; --limit is per page.
Text is normalized and JSONL is decoded/derived, not a lossless native archive.
A late error can leave partial stdout; require exit 0 and complete stderr summary.

";
const GET_V2_HELP: &str = "get-v2 (--file FILE | --session-dir DIR) --ref TOKEN [OPTIONS]
  --ref TOKEN           Required record_ref from query-v2 or context-v2.
  --view stored|decoded Default stored: the archive's original JSON line.
                        Decoded: full structured JSON with message and metadata,
                        using the fixed protobuf projection; a derived view,
                        not the archive's original bytes. Preserves extensions.
  --text-offset N       Default 0; stored range 0..=8388608, decoded 0..=67108864.
                        Within the selected view's UTF-8 bytes, on a code-point
                        boundary. Keep --view unchanged while joining chunks.
Returns record_json_chunk (encoding=json_utf8), not just message text. Continue
with next_text_offset until complete=true, then concatenate chunks and parse JSON.
The final LF is excluded. Decoded JSON is bounded to 64 MiB, independently of
the 8 MiB archive line limit. Each call validates the whole target
record (at most 8 MiB) plus source metadata; it does not scan the preceding logs.
Only --max-response-bytes controls chunk size; preview/scan budgets do not apply.

";
const CONTEXT_V2_HELP: &str = "context-v2 (--file FILE | --session-dir DIR) --ref TOKEN [OPTIONS]
  --ref TOKEN           Required record_ref from query-v2 or context-v2.
  --before N            Default 10; range 0..=100 preceding records.
  --after N             Default 10; range 0..=100 following records.
Returns ordered previews within the reference's original snapshot, without a
message filter. Before/after share response and scan budgets, including the target;
preceding records are collected first. Reverse scanning also counts toward bytes.
The target is retained if neighbors exceed a budget; inspect before_returned,
after_returned and partial_before/partial_after. There is no context cursor;
use returned record_ref values for further retrieval.

";

fn print_help(command: &str) -> Result<()> {
    match command {
        "diagnose" => {
            print!("{DIAGNOSE_HELP}");
            return Ok(());
        }
        "query-v2" => print!("{QUERY_V2_HELP}"),
        "get-v2" => print!("{GET_V2_HELP}"),
        "context-v2" => print!("{CONTEXT_V2_HELP}"),
        "help" | "--help" | "-h" | "devices" | "capture" | "start" | "status" | "query"
        | "stop" | "version" | "--version" => {
            print!(
                "{HELP}{CAPTURE_SCOPE_HELP}{DIAGNOSE_HELP}{QUERY_V2_HELP}{GET_V2_HELP}{CONTEXT_V2_HELP}"
            );
        }
        _ => return Err(usage(format!("unknown command: {command}; use --help"))),
    }
    print!("{V2_SOURCE_HELP}{V2_RESPONSE_HELP}{V2_SCAN_HELP}");
    Ok(())
}

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
    #[serde(default = "default_record_version")]
    record_schema_version: u8,
    #[serde(default)]
    transport: adb::LogTransport,
    #[serde(default)]
    scope: CaptureScope,
    #[serde(default = "LogBuffer::defaults")]
    buffers: Vec<LogBuffer>,
}
fn default_record_version() -> u8 {
    1
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct VisibleEnd {
    seq: u64,
    offset: u64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    record_schema_version: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact_id: Option<ciallogcat::session::SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<ciallogcat::session::SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    visible_end: Option<VisibleEnd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capture_completeness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream_interruptions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_stream_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_exit_confirmed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    management_closed_unix_ms: Option<u64>,
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
            record_schema_version: None,
            artifact_id: None,
            session_id: None,
            visible_end: None,
            capture_completeness: None,
            stream_interruptions: None,
            last_stream_error: None,
            worker_exit_confirmed: None,
            management_closed_unix_ms: None,
        }
    }
    fn terminal(&self) -> bool {
        matches!(self.state.as_str(), "completed" | "failed" | "abandoned")
    }

    fn update_progress(&mut self, visible: &Mutex<writer::Progress>) {
        let progress = *visible.lock().unwrap();
        self.entries = progress.entries;
        self.bytes = progress.bytes;
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
    let mut status: Status =
        serde_json::from_value(session_management::read_effective_status(dir)?)
            .map_err(|e| error("invalid_session", e.to_string(), 3))?;
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
        if matches!(
            flag.as_str(),
            "--help"
                | "-h"
                | "--dump"
                | "--all"
                | "--regex"
                | "--ignore-case"
                | "--case-sensitive"
                | "--force"
        ) {
            let name = if flag == "-h" { "--help" } else { &flag };
            if flags.insert(name.into(), String::new()).is_some() {
                return Err(usage(format!("duplicate option: {name}")));
            }
            continue;
        }
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
        Some(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => s
            .parse()
            .map_err(|_| usage(format!("{name} must be an integer")))?,
        Some(_) => return Err(usage(format!("{name} must contain only ASCII digits"))),
        None => default,
    };
    if !(min..=max).contains(&n) {
        return Err(usage(format!("{name} must be between {min} and {max}")));
    }
    Ok(n)
}
fn config(flags: &mut BTreeMap<String, String>) -> Result<Config> {
    let dump = flags.remove("--dump").is_some();
    let tail = flags.remove("--tail");
    let since = flags.remove("--since");
    let scope = CaptureScope::from_inputs(dump, tail.as_deref(), since.as_deref())
        .map_err(|e| usage(e.to_string()))?;
    let buffers = flags
        .remove("--buffers")
        .map(|v| parse_buffers(&v))
        .transpose()
        .map_err(|e| usage(e.to_string()))?
        .unwrap_or_else(LogBuffer::defaults);
    let record_schema_version = match flags.remove("--record-format").as_deref().unwrap_or("v1") {
        "v1" => 1,
        "v2" => 2,
        "v3" => 3,
        _ => return Err(usage("--record-format must be v1, v2 or v3")),
    };
    let transport = match flags.remove("--transport").as_deref().unwrap_or("text") {
        "text" if record_schema_version != 3 => adb::LogTransport::Text,
        "proto" if matches!(record_schema_version, 2 | 3) => adb::LogTransport::Proto,
        _ => {
            return Err(usage(
                "--transport must be text for v1, text or proto for v2, and explicit proto for v3",
            ));
        }
    };
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
        max_bytes: number(
            flags,
            "--max-bytes",
            1073741824,
            if matches!(record_schema_version, 2 | 3) {
                8192
            } else {
                1
            },
            1073741824,
        )?,
        max_entries: number(flags, "--max-entries", 10000000, 1, 10000000)?,
        record_schema_version,
        transport,
        scope,
        buffers,
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
    if flags.remove("--help").is_some() {
        return print_help(&command);
    }
    match command.as_str() {
        "help" | "--help" | "-h" => {
            no_flags(&flags)?;
            print_help(&command)
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
        "diagnose" => diagnose(&mut flags),
        "capture" => {
            let config = config(&mut flags)?;
            let output = flags.remove("--output").unwrap_or_else(|| "-".into());
            no_flags(&flags)?;
            let writer: Box<dyn Write + Send> = if output == "-" {
                Box::new(io::stdout())
            } else {
                Box::new(new_file(Path::new(&output))?)
            };
            let status = capture_selected(config, None, writer)?;
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
            // Only the owner may publish failure. A duplicate worker that fails
            // create_new must never overwrite the active owner's status.
            let _lock = new_file(&dir.join("worker.lock"))?;
            let result = (|| {
                let bytes = read_limited(&dir.join("config.json"), 16384)?;
                let mut config: Config = serde_json::from_slice(&bytes)
                    .map_err(|e| error("invalid_session", e.to_string(), 3))?;
                config.buffers = parse_buffers(
                    &config
                        .buffers
                        .iter()
                        .map(|b| b.name())
                        .collect::<Vec<_>>()
                        .join(","),
                )
                .map_err(|e| error("invalid_session", e.to_string(), 3))?;
                if config.duration_secs == 0
                    || config.duration_secs > 86400
                    || config.max_bytes == 0
                    || config.max_bytes > 1073741824
                    || config.max_entries == 0
                    || config.max_entries > 10000000
                    || config.serial.is_empty()
                    || config.serial.len() > 1024
                    || config.serial.starts_with('-')
                    || config.serial.chars().any(char::is_control)
                {
                    return Err(error(
                        "invalid_session",
                        "invalid capture bounds or serial",
                        3,
                    ));
                }
                if !matches!(config.record_schema_version, 1..=3)
                    || (config.record_schema_version == 1
                        && config.transport != adb::LogTransport::Text)
                    || (config.record_schema_version == 3
                        && config.transport != adb::LogTransport::Proto)
                    || (matches!(config.record_schema_version, 2 | 3) && config.max_bytes < 8192)
                {
                    return Err(error(
                        "invalid_session",
                        "invalid record format or transport",
                        3,
                    ));
                }
                let writer = Box::new(new_file(&dir.join("logs.jsonl"))?);
                capture_selected(config, Some(&dir), writer).map(|_| ())
            })();
            if let Err(e) = &result {
                let mut status = load_status(&dir).unwrap_or_else(|_| Status::new(&dir));
                if !status.terminal() {
                    status.state = "failed".into();
                    status.reason = Some(e.code.into());
                    status.error = Some(e.message.clone());
                    let _ = write_status(&dir, &mut status);
                }
            }
            result
        }
        "query-v2" | "get-v2" | "context-v2" => query_v2::run(&command, flags),
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
                    let force = flags.remove("--force").is_some();
                    no_flags(&flags)?;
                    let status = load_status(&dir)?;
                    if status.terminal() {
                        return emit(&status);
                    }
                    match new_file(&dir.join("stop")) {
                        Ok(_) => (),
                        Err(_) if dir.join("stop").is_file() => regular(&dir.join("stop"))?,
                        Err(e) => return Err(e),
                    }
                    let deadline = Instant::now() + Duration::from_secs(wait);
                    loop {
                        let status = load_status(&dir)?;
                        if status.terminal() {
                            return emit(&status);
                        }
                        if Instant::now() >= deadline {
                            if force {
                                return emit(&session_management::close_administratively(&dir)?);
                            }
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
fn diagnose(flags: &mut BTreeMap<String, String>) -> Result<()> {
    use ciallogcat::diagnostics::{self, CancellationToken, DiagnosticOptions};
    let serial = required(flags, "--serial")?;
    let buffers = flags
        .remove("--buffers")
        .map(|value| parse_buffers(&value))
        .transpose()
        .map_err(|e| usage(e.to_string()))?
        .unwrap_or_else(LogBuffer::defaults);
    let options = DiagnosticOptions {
        timeout: Duration::from_millis(number(flags, "--timeout-ms", 10000, 1, 60000)?),
        probe_timeout: Duration::from_millis(number(flags, "--probe-timeout-ms", 2000, 1, 60000)?),
        max_output_bytes: number(flags, "--max-output-bytes", 16384, 1, 65536)? as usize,
    };
    let explicit_adb = flags.remove("--adb");
    no_flags(flags)?;
    let adb = explicit_adb
        .map(PathBuf::from)
        .or_else(adb::find_adb)
        .ok_or_else(|| error("adb_unavailable", "ADB not found; provide --adb PATH", 4))?;
    let cancel = CancellationToken::default();
    let signal = cancel.clone();
    ctrlc::set_handler(move || signal.cancel()).map_err(io_error)?;
    let report = diagnostics::collect(&adb, &serial, &buffers, &options, &cancel).map_err(usage)?;
    let mut bytes = serde_json::to_vec(&report).map_err(io_error)?;
    bytes.push(b'\n');
    if bytes.len() > report.max_json_bytes {
        return Err(io_error("diagnostic response exceeded its JSON budget"));
    }
    io::stdout().lock().write_all(&bytes).map_err(io_error)?;
    if report.cancelled {
        Err(error(
            "interrupted",
            "device diagnostics cancelled after child cleanup",
            130,
        ))
    } else {
        Ok(())
    }
}

fn record(entry: LogEntry, seq: u64) -> LegacyRecord {
    LegacyRecord::from_entry(&entry, seq)
}

fn capture_selected(
    config: Config,
    dir: Option<&Path>,
    writer: Box<dyn Write + Send>,
) -> Result<Status> {
    if matches!(config.record_schema_version, 2 | 3) {
        capture_v2::capture(config, dir, writer)
    } else {
        capture(config, dir, writer)
    }
}

fn capture(config: Config, dir: Option<&Path>, writer: Box<dyn Write + Send>) -> Result<Status> {
    let mut status = Status::new(dir.unwrap_or_else(|| Path::new("")));
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&interrupted);
    ctrlc::set_handler(move || {
        signal_flag.store(true, Ordering::Release);
    })
    .map_err(io_error)?;
    let visible = Arc::new(Mutex::new(writer::Progress::default()));
    let writer_visible = Arc::clone(&visible);
    let (write_tx, write_rx) = mpsc::sync_channel::<Vec<u8>>(4);
    let (done_tx, done_rx) = mpsc::channel();
    let (progress_tx, progress_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = writer::run(writer, write_rx, &writer_visible, &progress_tx);
        let _ = done_tx.send(result);
    });
    let (tx, rx) = mpsc::sync_channel(128);
    let mut child = match adb::start_logcat_with_scope(
        &config.adb,
        &config.serial,
        &config.buffers,
        1,
        tx,
        WakeSignal::default(),
        &config.scope,
        adb::LogcatStartOptions::default(),
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
    let poll_writer = || match done_rx.try_recv() {
        Ok(result) => Some(result),
        Err(mpsc::TryRecvError::Empty) => None,
        Err(mpsc::TryRecvError::Disconnected) => {
            Some(Err(io::Error::other("output writer disconnected")))
        }
    };
    let mut saw_exited = false;
    'capture: loop {
        // Output may fail on the only record from an otherwise idle device.
        // Keep its actual result for cleanup instead of waiting for another log
        // to fill the write queue or for the capture deadline to expire.
        if let Some(result) = poll_writer() {
            writer_done = Some(result);
            break;
        }
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
                    // Preserve the host observation sampled by the ADB reader,
                    // before queue/backpressure waiting, without inventing a
                    // native session identity for the legacy v1 format.
                    let observed_unix_ms = entry.received_unix_ms();
                    let mut record = record(entry.into_inner(), accepted_entries + 1);
                    record.observed_unix_ms = observed_unix_ms;
                    let mut bytes = serde_json::to_vec(&record).map_err(io_error)?;
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
                                writer_done = poll_writer();
                                break 'capture;
                            }
                            Err(TrySendError::Full(pending)) => {
                                bytes = pending;
                                if let Some(result) = poll_writer() {
                                    writer_done = Some(result);
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
                                    status.update_progress(&visible);
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
                LogcatEvent::Exited { code, error: e, .. } => {
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
                        status.reason = Some(
                            if config.scope.is_dump() {
                                "snapshot_complete"
                            } else {
                                "adb_exit"
                            }
                            .into(),
                        );
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
            status.update_progress(&visible);
            if let Some(dir) = dir {
                write_status(dir, &mut status)?;
            }
            heartbeat = Instant::now();
        }
    }
    if let Some(result) = &writer_done {
        let message = match result {
            Ok(()) => "output writer stopped before capture ended".into(),
            Err(e) => e.to_string(),
        };
        failure.get_or_insert_with(|| error("output_error", message, 5));
    }
    child.stop();
    // Both backend readers own the only senders. Disconnection proves diagnostic
    // EOF, and receiving Exited proves the backend reaped its owned ADB child.
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = drain_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            failure.get_or_insert_with(|| {
                error(
                    "adb_error",
                    "backend readers did not finish within two seconds",
                    4,
                )
            });
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
                    failure.get_or_insert_with(|| error("adb_error", e, 4));
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
                failure.get_or_insert_with(|| {
                    error(
                        "adb_error",
                        "backend readers did not finish within two seconds",
                        4,
                    )
                });
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
        Ok(Err(e)) => {
            failure.get_or_insert_with(|| error("output_error", e.to_string(), 5));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            failure.get_or_insert_with(|| error("output_error", "output writer disconnected", 5));
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            failure.get_or_insert_with(|| {
                error(
                    "output_backpressure",
                    "output writer did not drain within two seconds",
                    5,
                )
            });
        }
    }
    status.update_progress(&visible);
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
    let status = load_status(dir)?;
    let frozen = status.management_closed_unix_ms.is_some();
    let path = dir.join("logs.jsonl");
    regular(&path)?;
    let mut file = File::open(path).map_err(io_error)?;
    if file.metadata().map_err(io_error)?.len() > 1073741824 {
        return Err(error(
            "invalid_session",
            "logs exceed session size limit",
            3,
        ));
    }
    let matcher = ciallogcat::filter::compile_query(needle, false, true).map_err(usage)?;
    let size = file.metadata().map_err(io_error)?.len();
    if frozen && status.bytes > size {
        return Err(error(
            "invalid_session",
            "file is shorter than frozen boundary",
            3,
        ));
    }
    let upper = if frozen { status.bytes } else { u64::MAX };
    let mut reader = LegacyReader::new(BufReader::new((&mut file).take(upper)), InputMode::Active);
    let mut entries = Vec::new();
    let mut next = after;
    let mut partial = false;
    let mut more = false;
    let mut total = 0u64;
    let mut scanned_bytes = 0u64;
    let mut scanned_records = 0u64;
    let mut scan_limited = false;
    let mut last_seen = 0;
    let mut read_end = 0;
    loop {
        let outcome = match reader.read_record() {
            Ok(outcome) => outcome,
            Err(e) => {
                if e.kind == RecordErrorKind::Io {
                    return Err(io_error(e));
                }
                if e.offset == 0 {
                    // Only inspect a failed first record: valid v1 queries keep
                    // their existing read cost, and corrupt v1 stays corrupt v1.
                    drop(reader);
                    file.rewind().map_err(io_error)?;
                    let mut first = Vec::new();
                    BufReader::new(file)
                        .take(MAX_RECORD + 1)
                        .read_until(b'\n', &mut first)
                        .map_err(io_error)?;
                    if let Ok(header) = ciallogcat::document::decode_header(&first) {
                        return Err(error(
                            "unsupported_format",
                            format!(
                                "query reads v1 records, but logs.jsonl has a native v{} document header; use query-v2 --session-dir DIR with the same session directory",
                                header.schema_version
                            ),
                            3,
                        ));
                    }
                }
                return Err(error("invalid_session", e.to_string(), 3));
            }
        };
        let located = match outcome {
            ReadOutcome::Record(record) => record,
            ReadOutcome::End => break,
            ReadOutcome::Incomplete => {
                partial = true;
                break;
            }
        };
        let size = located.encoded_bytes;
        read_end = located.offset + size;
        let value = located.record;
        let seq = value.seq;
        if frozen && seq > status.entries {
            return Err(error(
                "invalid_session",
                "record exceeds frozen sequence boundary",
                3,
            ));
        }
        last_seen = seq;
        if seq <= after {
            continue;
        }
        if scanned_bytes + size > 16 * 1024 * 1024 || scanned_records >= 100000 {
            scan_limited = true;
            more = true;
            break;
        }
        let matches = matcher
            .as_ref()
            .is_none_or(|matcher| matcher.is_match(&value.message));
        if matches && (entries.len() as u64 >= limit || total + size > MAX_RECORD) {
            more = true;
            break;
        }
        next = seq;
        scanned_records += 1;
        scanned_bytes += size;
        if matches {
            total += size;
            entries.push(value);
        }
    }
    if frozen && !more && (partial || last_seen != status.entries) {
        return Err(error(
            "invalid_session",
            "frozen byte and sequence boundaries disagree",
            3,
        ));
    }
    // An administrative close racing this request can make its old upper bound
    // stale; never return a successful page beyond the now-frozen prefix.
    if let Some(closed) = session_management::closure_status(dir)?
        && (closed["bytes"]
            .as_u64()
            .is_some_and(|bytes| read_end > bytes)
            || closed["entries"]
                .as_u64()
                .is_some_and(|seq| last_seen > seq))
    {
        return Err(error(
            "stale_session",
            "session was administratively closed while querying",
            3,
        ));
    }
    emit(
        &json!({"schema_version":SCHEMA,"type":"query","entries":entries,"next_after_seq":next,"partial":partial,"complete":!more && !partial,"stop_reason":if partial {"incomplete_tail"} else if scan_limited {"scan_budget"} else if more {"result_budget"} else {"end_of_file"},"has_more":more,"scan_limited":scan_limited,"scanned_records":scanned_records}),
    )
}
