//! Standalone CLI orchestration. Session files are local capabilities; PIDs are
//! descriptive only. The worker alone owns and stops its ADB child.
mod capture;
mod query;
mod query_filter;
mod session_management;
mod text_output;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ciallogcat::WakeSignal;
use ciallogcat::adb::{self, BackendEvent};
use ciallogcat::capture_scope::{CaptureScope, parse_buffers};
use ciallogcat::model::LogBuffer;
use serde::{Deserialize, Serialize};
use serde_json::json;

const SCHEMA: u8 = 1;
const HELP: &str = "ciallogcat-cli: bounded local Android log capture (Linux/Windows)

Commands:
  devices [--adb PATH]
  diagnose --serial SERIAL [--adb PATH] [--buffers LIST] [--timeout-ms 10000]
  capture --serial SERIAL [--adb PATH] [--output FILE|-] [--duration-secs 600] [--max-bytes 1073741824] [--max-entries N]
  start --serial SERIAL --session-dir DIR [--adb PATH] [same capture bounds]
  capture/start transport: [--transport text|proto] (default text; proto is experimental)
  One native document format is used for both transports.
  capture/start history: [--tail N | --since TIME] [--dump] [--buffers LIST]
  status --session-dir DIR
  query (--file FILE | --session-dir DIR) [OPTIONS]
  get (--file FILE | --session-dir DIR) --ref TOKEN [OPTIONS]
  context (--file FILE | --session-dir DIR) --ref TOKEN [OPTIONS]
  stop --session-dir DIR [--wait-secs 10] [--force]

Use query --help, get --help or context --help for focused help.
Options use --name VALUE; --dump, --all, --regex, --ignore-case,
--case-sensitive, --force and --help/-h take no value. Help is plain text on stdout.
Command results are JSON; capture writes a native JSONL document (default stdout),
including its header and final footer. Capture summaries go to stderr.
Errors are JSON on stderr. New output files/session directories must not exist.
Stop requests cooperative exit even for stale sessions, never signals a persisted PID.
After the wait, --force administratively closes an unacknowledged session with a
frozen published query boundary; it does NOT kill or confirm worker/ADB exit.
Local sessions can contain
sensitive device logs. No device logs are cleared.

";
const CAPTURE_SCOPE_HELP: &str = "Device history (capture and start):
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
--max-entries (10000000; 1..10000000), --max-bytes (1073741824; 8192..1073741824). --tail is device history selection, not a host record limit.

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
const QUERY_SOURCE_HELP: &str = "Source (required, exactly one):
  --file FILE          Finalized native artifact.
  --session-dir DIR    Published prefix of a native session, including active capture.
These commands do not call ADB. Tokens bind the source file and local signing key;
after copying a file, query it again to obtain new references/cursors.

";
const QUERY_RESPONSE_HELP: &str = "Response budget (query, get and context):
  --max-response-bytes N  Default 65536; range 8192..=1048576 bytes.
                         Includes JSON envelope, escaped strings and final LF.
                         A budget too small for the envelope/reference is an error.

";
const QUERY_SCAN_HELP: &str = "Preview/scan budgets (query and context only; rejected by get):
  --preview-bytes N     Default 2048; range 0..=16384 UTF-8 message bytes per record.
                       Preserves code points; response budget may shorten previews.
  --max-scan-bytes N    Default 16777216; range 8388609..=67108864 bytes.
  --max-scan-records N  Default 100000; range 1..=1000000 candidate records.
Scan budgets bound candidate work, not total file I/O; validation/control reads
are extra. Inspect scan.file_bytes_read for actual log-file reads. Forward scans
reserve space for an 8 MiB record and may stop before using the full byte budget.

";
const QUERY_HELP: &str = "query (--file FILE | --session-dir DIR) [OPTIONS]
  --mode snapshot|tail  Default snapshot. Snapshot fixes the first page's upper
                        bound; tail refreshes it per call and requires --session-dir.
  --query TEXT          Default empty (all); case-sensitive literal message match,
                        at most 4096 UTF-8 bytes. Same message predicate as GUI.
  --regex               Interpret --query as a Rust regular expression.
  --ignore-case         Case-insensitive message matching; default case-sensitive.
  --case-sensitive      Explicit default; conflicts with --ignore-case.
  --tag TEXT            Case-sensitive literal Tag substring.
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
  --start-at beginning|end  Default beginning. End requires a native session
                           + --mode tail, without --cursor. Returns an empty page
                           at the published boundary; the prefix is not scanned.
Both modes default to the first record. Keep source, --mode and all filters unchanged
when resuming; --limit and budgets may change. Response/scan budgets
can stop a page before --limit. Follow next_cursor while has_more, even if records
is empty. scan.stop_reason reports upper_bound/record_limit/scan_budget/response_budget.
complete means this snapshot/current tail boundary was scanned, NOT capture completeness.
Tail does not wait: when has_more=false, reuse next_cursor later for new data.
An exhausted snapshot stays fixed; omit --cursor to see new data in a new snapshot.
JSON pages contain previews and record_ref; use get for full original record JSON.
Streaming keeps bounded pages/records in memory, never the entire result. Page budgets
control work per chunk, not total streaming stdout bytes; --limit is per page.
Text is normalized and JSONL is decoded/derived, not a lossless native archive.
A late error can leave partial stdout; require exit 0 and complete stderr summary.

";
const GET_HELP: &str = "get (--file FILE | --session-dir DIR) --ref TOKEN [OPTIONS]
  --ref TOKEN           Required record_ref from query or context.
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
Only --max-response-bytes controls chunk size; preview/scan budget flags are rejected.

";
const CONTEXT_HELP: &str = "context (--file FILE | --session-dir DIR) --ref TOKEN [OPTIONS]
  --ref TOKEN           Required record_ref from query or context.
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
        "query" => print!("{QUERY_HELP}"),
        "get" => print!("{GET_HELP}"),
        "context" => print!("{CONTEXT_HELP}"),
        "help" | "--help" | "-h" | "devices" | "capture" | "start" | "status" | "stop"
        | "version" | "--version" => {
            print!("{HELP}{CAPTURE_SCOPE_HELP}{DIAGNOSE_HELP}{QUERY_HELP}{GET_HELP}{CONTEXT_HELP}");
        }
        _ => return Err(usage(format!("unknown command: {command}; use --help"))),
    }
    print!("{QUERY_SOURCE_HELP}{QUERY_RESPONSE_HELP}");
    if command != "get" {
        print!("{QUERY_SCAN_HELP}");
    }
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
#[serde(deny_unknown_fields)]
struct Config {
    adb: PathBuf,
    serial: String,
    duration_secs: u64,
    max_bytes: u64,
    max_entries: u64,
    transport: adb::LogTransport,
    scope: CaptureScope,
    buffers: Vec<LogBuffer>,
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
    let transport = match flags.remove("--transport").as_deref().unwrap_or("text") {
        "text" => adb::LogTransport::Text,
        "proto" => adb::LogTransport::Proto,
        _ => return Err(usage("--transport must be text or proto")),
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
        max_bytes: number(flags, "--max-bytes", 1073741824, 8192, 1073741824)?,
        max_entries: number(flags, "--max-entries", 10000000, 1, 10000000)?,
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
            let status = capture::run(config, None, writer)?;
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
                    || config.max_bytes < 8192
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
                let writer = Box::new(new_file(&dir.join("logs.jsonl"))?);
                capture::run(config, Some(&dir), writer).map(|_| ())
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
        "query" | "get" | "context" => query::run(&command, flags),
        "status" | "stop" => {
            let dir_value = required(&mut flags, "--session-dir")?;
            let dir = session_path(&dir_value)?;
            match command.as_str() {
                "status" => {
                    no_flags(&flags)?;
                    emit(&load_status(&dir)?)
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
