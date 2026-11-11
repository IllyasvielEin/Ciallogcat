//! Bounded queries over the native append-only archive format.
mod crashes;
mod source;
mod tokens;

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ciallogcat::document;
use ciallogcat::document::MAX_RECORD_BYTES;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Result, emit, error, io_error, no_flags, number, required, usage};
use source::{Anchor, BindingRef, Boundary, Row, Source, invalid, stale};
use tokens::{Signer, hash};

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Snapshot,
    Tail,
}

#[derive(Clone, Deserialize, Serialize)]
struct Cursor {
    version: u8,
    binding: BindingRef,
    mode: Mode,
    filter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    time_anchor_ms: Option<u64>,
    upper: Boundary,
    upper_anchor: Anchor,
    offset: u64,
    last_seq: u64,
    count: u64,
    first_seq: Option<u64>,
    finalized: bool,
    anchor: Anchor,
    // End-tail queries distinguish published unscanned history from scanned rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tail_start: Option<TailStart>,
}

impl Cursor {
    /// An end-start cursor that has not scanned any record yet. It marks a
    /// published position only, so it may be resumed with any filter.
    fn pristine(&self) -> bool {
        self.count == 0
            && self
                .tail_start
                .as_ref()
                .is_some_and(|start| start.boundary.offset == self.offset)
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct TailStart {
    boundary: Boundary,
    published_count: u64,
    first_seq: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
// Opaque token payload: short keys keep per-record references small.
struct RecordRef {
    version: u8,
    #[serde(rename = "b")]
    binding: BindingRef,
    #[serde(rename = "u")]
    upper: Boundary,
    #[serde(rename = "o")]
    offset: u64,
    #[serde(rename = "y")]
    bytes: u64,
    #[serde(rename = "q")]
    seq: u64,
    #[serde(rename = "h")]
    digest: String,
}

// Derived JSON can expand a compact payload (especially NUL/invalid UTF-8)
// beyond the archive line limit. Keep it independently bounded.
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EVIDENCE_SUMMARY_BYTES: usize = 512;
/// Query API response version (query, crashes, get, context). Version 3
/// introduced compact record previews.
const RESPONSE_SCHEMA: u8 = 3;
// Token kinds and payload versions change together whenever the token layout
// changes. Older kinds are rejected as invalid_token; callers re-query.
const CURSOR_KIND: &str = "c5";
const CURSOR_VERSION: u8 = 5;
const REF_KIND: &str = "r3";
const REF_VERSION: u8 = 3;

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum View {
    Stored,
    Decoded,
}

struct Options {
    response: usize,
    preview: usize,
    scan_bytes: u64,
    scan_records: u64,
}

impl Options {
    fn parse(flags: &mut BTreeMap<String, String>, response: usize) -> Result<Self> {
        Ok(Self {
            response,
            preview: number(flags, "--preview-bytes", 512, 0, 16384)? as usize,
            // A supported maximum-size first record must always be scannable.
            scan_bytes: number(
                flags,
                "--max-scan-bytes",
                16777216,
                MAX_RECORD_BYTES + 1,
                67108864,
            )?,
            scan_records: number(flags, "--max-scan-records", 100000, 1, 1000000)?,
        })
    }
}

pub(super) fn run(command: &str, mut flags: BTreeMap<String, String>) -> Result<()> {
    let file = flags.remove("--file");
    let directory = flags.remove("--session-dir");
    let (path, session) = match (file, directory) {
        (Some(path), None) | (None, Some(path)) if path.trim().is_empty() => {
            return Err(usage("source path must not be empty"));
        }
        (Some(path), None) => (PathBuf::from(path), false),
        (None, Some(path)) => (PathBuf::from(path), true),
        _ => return Err(usage("provide exactly one of --file and --session-dir")),
    };
    let response_budget =
        number(&mut flags, "--max-response-bytes", 65536, 8192, 1048576)? as usize;
    let signer = Signer::load()?;
    if command == "get" {
        let reference = record_target(&signer, &mut flags, &path, session)?;
        // Decoded is what readers need; for text records it equals stored.
        let view = match flags.remove("--view").as_deref().unwrap_or("decoded") {
            "stored" => View::Stored,
            "decoded" => View::Decoded,
            _ => return Err(usage("--view must be stored or decoded")),
        };
        let max_offset = if view == View::Stored {
            MAX_RECORD_BYTES
        } else {
            MAX_DECODED_BYTES
        };
        let offset = number(&mut flags, "--text-offset", 0, 0, max_offset)? as usize;
        no_flags(&flags)?;
        return emit_response(
            get(path, session, &reference, response_budget, offset, view)?,
            response_budget,
        );
    }
    let wait_ms = if command == "query" {
        number(&mut flags, "--wait-ms", 0, 0, 60000)?
    } else {
        0
    };
    let options = Options::parse(&mut flags, response_budget)?;
    if command == "crashes" {
        let groups = number(&mut flags, "--limit", 20, 1, 200)? as usize;
        let mut state = QueryState::open(path, session, &signer, flags, ScanKind::Crashes)?;
        let page = state.scan_page(&signer, &options, None)?;
        let response = state.crash_response(&signer, page, groups, response_budget)?;
        return emit_response(response, response_budget);
    }
    if command == "query" {
        let all = flags.remove("--all").is_some();
        let format = flags.remove("--format");
        let format = format
            .as_deref()
            .unwrap_or(if all { "jsonl" } else { "json" });
        match format {
            "json" if !all => {}
            "json" => {
                return Err(usage(
                    "--all requires --format jsonl or text (default jsonl)",
                ));
            }
            "text" | "jsonl" => {
                return stream_query(
                    path, session, &signer, &options, flags, all, format, wait_ms,
                );
            }
            _ => return Err(usage("--format must be json, jsonl or text")),
        }
    }
    let response = match command {
        "query" => {
            let mut state = QueryState::open(path, session, &signer, flags, ScanKind::Records)?;
            state.set_wait(wait_ms)?;
            let started = Instant::now();
            let mut page = state.scan_page(&signer, &options, None)?;
            while let Some(pause) = state.wait_pause(started, page.records.is_empty()) {
                std::thread::sleep(pause);
                state.refresh_tail()?;
                page = state.scan_page(&signer, &options, None)?;
            }
            let mut response = state.response(&signer, page)?;
            state.annotate_wait(&mut response, started, false);
            response
        }
        "context" => {
            let reference = record_target(&signer, &mut flags, &path, session)?;
            let before = number(&mut flags, "--before", 10, 0, 100)?;
            let after = number(&mut flags, "--after", 10, 0, 100)?;
            let same_pid = flags.remove("--same-pid").is_some();
            no_flags(&flags)?;
            context(
                path, session, &signer, &reference, &options, before, after, same_pid,
            )?
        }
        _ => return Err(usage("unsupported query command")),
    };
    emit_response(response, response_budget)
}

/// The target of get/context: a signed `--ref`, or `--seq N` resolved inside
/// the source's current published (session) or finalized (file) boundary.
fn record_target(
    signer: &Signer,
    flags: &mut BTreeMap<String, String>,
    path: &std::path::Path,
    session: bool,
) -> Result<RecordRef> {
    let Some(seq) = flags.remove("--seq") else {
        return record_reference(signer, flags);
    };
    if flags.contains_key("--ref") {
        return Err(usage("provide exactly one of --ref and --seq"));
    }
    if seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return Err(usage("--seq must contain only ASCII digits"));
    }
    let seq: u64 = seq.parse().map_err(|_| usage("--seq is out of range"))?;
    let mut source = Source::open(path.to_path_buf(), session, None)?;
    let (offset, bytes) = source.find_seq(seq)?.ok_or_else(|| {
        error(
            "record_not_found",
            format!("no record with seq {seq} inside the source's visible boundary"),
            3,
        )
    })?;
    Ok(RecordRef {
        version: REF_VERSION,
        binding: source.binding.reference(),
        upper: source.upper,
        offset,
        bytes: bytes.len() as u64,
        seq,
        digest: hash(&bytes),
    })
}

fn record_reference(signer: &Signer, flags: &mut BTreeMap<String, String>) -> Result<RecordRef> {
    let reference: RecordRef = signer.open(REF_KIND, &required(flags, "--ref")?)?;
    if reference.version != REF_VERSION {
        return Err(stale("unsupported reference version"));
    }
    Ok(reference)
}

fn emit_response(response: Value, budget: usize) -> Result<()> {
    check_response_budget(&response, budget)?;
    emit(&response)
}

fn check_response_budget(response: &Value, budget: usize) -> Result<()> {
    if encoded_len(response)? > budget {
        return Err(error(
            "response_budget",
            "response envelope exceeds requested byte budget",
            2,
        ));
    }
    Ok(())
}

struct StreamOutput<'a> {
    writer: &'a mut dyn Write,
    text: Option<super::text_output::TextOptions>,
    format: &'a str,
    records: u64,
}
impl StreamOutput<'_> {
    fn record(&mut self, source: &Source, row: &Row, device_time: bool) -> Result<()> {
        if let Some(text) = &self.text {
            text.write_record(self.writer, &row.entry, device_time)?;
        } else {
            let bytes = document::encode_record_projection(
                &source.header,
                &row.entry,
                &row.metadata,
                MAX_DECODED_BYTES as usize,
            )
            .map_err(|e| invalid(e.to_string()))?;
            // The validated encoder already produces compact JSON with escaped
            // strings and no final LF. Do not rebuild a second full JSON tree.
            self.writer.write_all(&bytes).map_err(io_error)?;
            self.writer.write_all(b"\n").map_err(io_error)?;
        }
        self.records += 1;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_query(
    path: PathBuf,
    session: bool,
    signer: &Signer,
    options: &Options,
    mut flags: BTreeMap<String, String>,
    all: bool,
    format: &str,
    wait_ms: u64,
) -> Result<()> {
    let text = if format == "text" {
        Some(super::text_output::TextOptions::parse(&mut flags)?)
    } else {
        None
    };
    let mut stdout = io::stdout().lock();
    let mut output = StreamOutput {
        writer: &mut stdout,
        text,
        format,
        records: 0,
    };
    let mut state = QueryState::open(path, session, signer, flags, ScanKind::Records)?;
    state.set_wait(wait_ms)?;
    let started = Instant::now();
    let mut pages = 0u64;
    let mut scanned = 0u64;
    let mut scanned_bytes = 0u64;
    let mut file_bytes = 0u64;
    let mut unresolved = 0u64;
    loop {
        let mut page = state.scan_page(signer, options, Some(&mut output))?;
        pages += 1;
        scanned += page.scanned_records;
        scanned_bytes += page.scanned_bytes;
        file_bytes += page.file_bytes_read;
        unresolved += page.package_unresolved;
        if !all || state.complete() {
            if let Some(pause) = state.wait_pause(started, output.records == 0) {
                std::thread::sleep(pause);
                state.refresh_tail()?;
                continue;
            }
            page.package_unresolved = unresolved;
            let mut response = state.response(signer, page)?;
            state.annotate_wait(&mut response, started, true);
            output.writer.flush().map_err(io_error)?;
            stream_summary(
                &mut response,
                &output,
                state.predicate.device_time(),
                pages,
                scanned,
                scanned_bytes,
                file_bytes,
            );
            check_response_budget(&response, options.response)?;
            let mut stderr = io::stderr().lock();
            serde_json::to_writer(&mut stderr, &response).map_err(io_error)?;
            stderr.write_all(b"\n").map_err(io_error)?;
            return Ok(());
        }
        if page.start == state.cursor.offset {
            return Err(invalid("query made no progress before snapshot boundary"));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_summary(
    response: &mut Value,
    output: &StreamOutput<'_>,
    device_time: bool,
    pages: u64,
    scanned: u64,
    scanned_bytes: u64,
    file_bytes: u64,
) {
    let fields = response.as_object_mut().unwrap();
    fields.remove("records");
    fields.insert("type".into(), "query_summary".into());
    fields.insert("format".into(), output.format.into());
    if let Some(text) = &output.text {
        fields.insert("text_presentation".into(), text.presentation(device_time));
    }
    fields.insert("derived".into(), true.into());
    fields.insert("records_returned".into(), output.records.into());
    fields.insert("pages".into(), pages.into());
    fields.insert("total_scanned_records".into(), scanned.into());
    fields.insert("total_scanned_bytes".into(), scanned_bytes.into());
    fields.insert("total_file_bytes_read".into(), file_bytes.into());
}

fn encoded_len(value: &impl Serialize) -> Result<usize> {
    Ok(serde_json::to_vec(value).map_err(io_error)?.len() + 1)
}

fn preview(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn record_ref(
    signer: &Signer,
    source: &Source,
    upper: Boundary,
    offset: u64,
    bytes: &[u8],
    seq: u64,
) -> Result<String> {
    signer.seal(
        REF_KIND,
        &RecordRef {
            version: REF_VERSION,
            binding: source.binding.reference(),
            upper,
            offset,
            bytes: bytes.len() as u64,
            seq,
            digest: hash(bytes),
        },
    )
}

/// Compact record preview (response schema 3). Fields with default meaning are
/// omitted: `kind` defaults to `entry`; `message_bytes`/`preview_truncated`
/// appear only for truncated previews; `text_loss`, `process` and
/// `metadata_preview_truncated` appear only when present/true. Full identity
/// (session, segment, attempt) and every stored field remain available via `get`.
fn summary(
    signer: &Signer,
    source: &Source,
    upper: Boundary,
    offset: u64,
    bytes: &[u8],
    row: &Row,
    preview_bytes: usize,
) -> Result<Value> {
    let message = preview(&row.entry.message, preview_bytes);
    let mut value = json!({
        "record_ref":record_ref(signer, source, upper, offset, bytes, row.seq)?, "seq":row.seq,
    });
    let fields = value.as_object_mut().unwrap();
    if row.kind != "entry" {
        fields.insert("kind".into(), row.kind.into());
    }
    if let Some(observed) = row.entry.provenance.and_then(|p| p.observed_unix_ms) {
        fields.insert("observed_unix_ms".into(), observed.into());
    }
    if let Some(identity) = row.entry.provenance
        && source
            .header
            .session_id
            .is_none_or(|session| session != identity.session_id)
    {
        // Only records from another capture session repeat their session.
        fields.insert("session_id".into(), json!(identity.session_id));
    }
    if let Some(log) = row.entry.log_fields() {
        fields.insert("time".into(), preview(log.time, 128).into());
        fields.insert("pid".into(), log.pid.into());
        fields.insert("tid".into(), log.tid.into());
        fields.insert("level".into(), log.level.short().into());
        fields.insert("tag".into(), preview(log.tag, 256).into());
        if let Some(process) = log.process {
            fields.insert("process".into(), preview(process, 256).into());
            fields.insert("process_source".into(), json!(log.process_source));
        }
        if log.time.len() > 128 || log.tag.len() > 256 || log.process.is_some_and(|p| p.len() > 256)
        {
            fields.insert("metadata_preview_truncated".into(), true.into());
        }
    }
    // Only recorded losses are repeated; unknown or loss-free rows omit it.
    if let Some(loss) = &row.entry.text_loss
        && (loss.truncated || loss.invalid_utf8_replaced)
    {
        fields.insert("text_loss".into(), json!(loss));
    }
    fields.insert("message".into(), message.into());
    if message.len() != row.entry.message.len() {
        fields.insert("preview_truncated".into(), true.into());
        fields.insert("message_bytes".into(), row.entry.message.len().into());
    }
    Ok(value)
}

fn shrink_summary(mut value: Value, allowance: usize) -> Result<Option<Value>> {
    while encoded_len(&value)? > allowance {
        let text = value["message"].as_str().unwrap_or("").to_owned();
        if text.is_empty() {
            // Keep the stable reference even when metadata itself is large.
            for key in ["time", "tag", "process", "process_source"] {
                value.as_object_mut().unwrap().remove(key);
            }
            value["metadata_preview_truncated"] = true.into();
            return Ok((encoded_len(&value)? <= allowance).then_some(value));
        }
        if value.get("message_bytes").is_none() {
            value["message_bytes"] = text.len().into();
        }
        value["message"] = preview(&text, text.len() / 2).into();
        value["preview_truncated"] = true.into();
    }
    Ok(Some(value))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScanKind {
    Records,
    Crashes,
}

struct QueryState {
    source: Source,
    cursor: Cursor,
    predicate: super::query_filter::QueryFilter,
    limit: u64,
    scanned: bool,
    crashes: Option<crashes::Collector>,
    wait_ms: u64,
}

struct ScanPage {
    records: Vec<Value>,
    start: u64,
    scanned_bytes: u64,
    scanned_records: u64,
    stop_reason: &'static str,
    file_bytes_read: u64,
    /// Log records lacking a stored process name while --package is active.
    package_unresolved: u64,
}

const WAIT_POLL: Duration = Duration::from_millis(200);

impl QueryState {
    fn open(
        path: PathBuf,
        session: bool,
        signer: &Signer,
        mut flags: BTreeMap<String, String>,
        kind: ScanKind,
    ) -> Result<Self> {
        let start_at = flags.remove("--start-at");
        let token = flags.remove("--cursor");
        if start_at.is_some() && token.is_some() {
            return Err(usage("--start-at cannot be combined with --cursor"));
        }
        let mut previous: Option<Cursor> = token
            .map(|token| signer.open(CURSOR_KIND, &token))
            .transpose()?;
        // A cursor records its mode and --start-at end only exists in tail
        // mode, so --mode is implied there; an explicit value is still checked.
        let implied = match (&previous, start_at.as_deref()) {
            (Some(cursor), _) => cursor.mode,
            (None, Some("end")) => Mode::Tail,
            _ => Mode::Snapshot,
        };
        let mode = match flags.remove("--mode").as_deref() {
            None => implied,
            Some("snapshot") => Mode::Snapshot,
            Some("tail") => Mode::Tail,
            Some(_) => return Err(usage("--mode must be snapshot or tail")),
        };
        if mode == Mode::Tail && !session {
            return Err(usage("tail mode requires --session-dir"));
        }
        let from_end = match start_at.as_deref().unwrap_or("beginning") {
            "beginning" => false,
            "end" if mode == Mode::Tail => true,
            "end" => return Err(usage("--start-at end requires --mode tail")),
            _ => return Err(usage("--start-at must be beginning or end")),
        };
        let pristine = previous.as_ref().is_some_and(Cursor::pristine);
        if !pristine
            && previous
                .as_ref()
                .is_some_and(|cursor| cursor.time_anchor_ms.is_none())
            && (flags.contains_key("--since") || flags.contains_key("--until"))
        {
            return Err(error(
                "cursor_mismatch",
                "time-filtered cursor lacks its original anchor",
                3,
            ));
        }
        let anchor = previous
            .as_ref()
            .filter(|_| !pristine)
            .and_then(|c| c.time_anchor_ms)
            .unwrap_or_else(super::now);
        let predicate = super::query_filter::QueryFilter::parse(&mut flags, anchor)?;
        let filter = match kind {
            ScanKind::Records => predicate.fingerprint(),
            // Crash pages resume only crash scans, never record pages.
            ScanKind::Crashes => hash(format!("crashes:{}", predicate.fingerprint()).as_bytes()),
        };
        let limit = match kind {
            ScanKind::Records => number(&mut flags, "--limit", 100, 1, 1000)?,
            ScanKind::Crashes => u64::MAX,
        };
        no_flags(&flags)?;
        // A mark binds a position only. Nothing has been returned under its
        // original filter, so the first resumed request defines the filter.
        if pristine
            && let Some(cursor) = &mut previous
            && cursor.mode == mode
        {
            cursor.filter.clone_from(&filter);
            cursor.time_anchor_ms = predicate.has_time_filter().then_some(anchor);
        }
        if let Some(cursor) = &previous
            && (cursor.version != CURSOR_VERSION
                || (cursor.tail_start.is_some() && cursor.mode != Mode::Tail)
                || cursor.mode != mode
                || cursor.filter != filter)
        {
            return Err(error(
                "cursor_mismatch",
                "cursor mode or filter differs from this request",
                3,
            ));
        }
        let mut source = Source::open(path, session, previous.as_ref().map(|c| &c.binding))?;
        let cursor = if let Some(mut cursor) = previous {
            source.pin_boundary(cursor.upper)?;
            source.check_anchor(&cursor.upper_anchor)?;
            source.check_anchor(&cursor.anchor)?;
            if cursor.offset < source.data_start || cursor.offset > cursor.upper.offset {
                return Err(stale("invalid cursor offset"));
            }
            if mode == Mode::Tail {
                if cursor.finalized && cursor.upper != source.upper {
                    return Err(stale("finalized artifact was appended"));
                }
                cursor.upper = source.upper;
                source.pin_boundary(cursor.upper)?;
                cursor.upper_anchor = source.anchor(cursor.upper.offset)?;
            }
            cursor
        } else {
            let tail_start = if from_end {
                let (published_count, first_seq, finalized) = source.end_start()?;
                Some((
                    TailStart {
                        boundary: source.upper,
                        published_count,
                        first_seq,
                    },
                    finalized,
                ))
            } else {
                None
            };
            let offset = if from_end {
                source.upper.offset
            } else {
                source.data_start
            };
            Cursor {
                version: CURSOR_VERSION,
                binding: source.binding.reference(),
                mode,
                filter,
                time_anchor_ms: predicate.has_time_filter().then_some(anchor),
                upper: source.upper,
                upper_anchor: source.anchor(source.upper.offset)?,
                offset,
                last_seq: if from_end {
                    source.upper.seq.unwrap_or(0)
                } else {
                    0
                },
                count: 0,
                first_seq: None,
                finalized: tail_start.as_ref().is_some_and(|(_, finalized)| *finalized),
                anchor: source.anchor(offset)?,
                tail_start: tail_start.map(|(start, _)| start),
            }
        };
        Ok(Self {
            source,
            cursor,
            predicate,
            limit,
            scanned: false,
            crashes: (kind == ScanKind::Crashes).then(crashes::Collector::default),
            wait_ms: 0,
        })
    }

    fn set_wait(&mut self, wait_ms: u64) -> Result<()> {
        if wait_ms > 0 && self.cursor.mode != Mode::Tail {
            return Err(usage("--wait-ms requires --mode tail"));
        }
        self.wait_ms = wait_ms;
        Ok(())
    }

    /// The source cannot publish more records: footer seen, terminal or stale.
    fn ended(&self) -> bool {
        self.cursor.finalized
            || self.source.status["terminal"].as_bool() == Some(true)
            || self.source.status["stale"].as_bool() == Some(true)
    }

    /// Returns a sleep duration while a waiting tail request should poll again.
    fn wait_pause(&self, started: Instant, nothing_returned: bool) -> Option<Duration> {
        let remaining = Duration::from_millis(self.wait_ms).checked_sub(started.elapsed())?;
        (nothing_returned && self.complete() && !self.ended() && !remaining.is_zero())
            .then(|| remaining.min(WAIT_POLL))
    }

    fn annotate_wait(&self, response: &mut Value, started: Instant, streamed: bool) {
        if self.wait_ms == 0 {
            return;
        }
        let returned = if streamed {
            response["records_returned"].as_u64().unwrap_or(0) > 0
        } else {
            response["records"]
                .as_array()
                .is_some_and(|r| !r.is_empty())
        };
        let reason = if returned {
            "matched"
        } else if !self.complete() {
            "has_more"
        } else if self.ended() {
            "source_ended"
        } else {
            "deadline"
        };
        response["wait"] = json!({"requested_ms":self.wait_ms,
            "waited_ms":(started.elapsed().as_millis() as u64).min(self.wait_ms),
            "stop_reason":reason});
    }

    /// Re-read the published watermark for another bounded tail page, with the
    /// same validation as resuming the current cursor in a new request.
    fn refresh_tail(&mut self) -> Result<()> {
        let mut source = self.source.reopen()?;
        source.pin_boundary(self.cursor.upper)?;
        source.check_anchor(&self.cursor.upper_anchor)?;
        source.check_anchor(&self.cursor.anchor)?;
        if self.cursor.finalized && self.cursor.upper != source.upper {
            return Err(stale("finalized artifact was appended"));
        }
        self.cursor.upper = source.upper;
        source.pin_boundary(self.cursor.upper)?;
        self.cursor.upper_anchor = source.anchor(self.cursor.upper.offset)?;
        self.source = source;
        self.scanned = false;
        Ok(())
    }

    fn scan_page(
        &mut self,
        signer: &Signer,
        options: &Options,
        mut output: Option<&mut StreamOutput<'_>>,
    ) -> Result<ScanPage> {
        if self.scanned {
            // Revalidate identity, metadata and the original boundary between
            // chunks, while retaining the parsed predicate and typed cursor.
            self.source = self.source.reopen()?;
            self.source.pin_boundary(self.cursor.upper)?;
            self.source.check_anchor(&self.cursor.upper_anchor)?;
            self.source.check_anchor(&self.cursor.anchor)?;
        }
        self.scanned = true;
        let source = &mut self.source;
        let cursor = &mut self.cursor;
        let predicate = &self.predicate;
        let limit = self.limit;
        let mut crash_sink = self.crashes.as_mut();
        let package_active = predicate.package_active();
        let mut package_unresolved = 0u64;
        source.pin_boundary(cursor.upper)?;
        let start = cursor.offset;
        let mut entries = Vec::new();
        let mut scanned_bytes = 0u64;
        let mut scanned_records = 0u64;
        let mut stop_reason = "upper_bound";
        // Reserve the exact envelope with worst-width counters and token positions.
        // This avoids re-reading anchors or signing a new cursor for every record.
        let mut worst = cursor.clone();
        worst.offset = u64::MAX;
        worst.last_seq = u64::MAX;
        worst.count = u64::MAX;
        worst.first_seq = Some(u64::MAX);
        worst.anchor.start = u64::MAX;
        worst.anchor.bytes = 64;
        worst.finalized = false;
        let mut envelope = query_response(
            source,
            &worst,
            predicate,
            signer,
            Vec::new(),
            u64::MAX,
            u64::MAX,
            u64::MAX,
            "response_budget",
            u64::MAX,
            u64::MAX,
        )?;
        if self.wait_ms > 0 {
            envelope["wait"] = json!({"requested_ms":u64::MAX,"waited_ms":u64::MAX,
                "stop_reason":"source_ended"});
        }
        // Footer discovery can change the explanation during this scan. Reserve
        // its whole JSON-string allowance, rather than the current template.
        envelope["evidence"]["summary"] = "x".repeat(MAX_EVIDENCE_SUMMARY_BYTES - 2).into();
        if let Some(output) = output.as_deref() {
            // Streaming adds presentation metadata and accumulated counters to
            // stderr. Bound that complete envelope before emitting any records.
            stream_summary(
                &mut envelope,
                output,
                predicate.device_time(),
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
            );
            envelope["records_returned"] = u64::MAX.into();
            if output.text.is_some() {
                envelope["text_presentation"]["process_names_truncated"] = u64::MAX.into();
            }
        }
        let reserve = encoded_len(&envelope)? + 128;
        if crash_sink.is_none() && reserve + 2048 >= options.response {
            return Err(usage(
                "response budget is too small for a record reference and cursor",
            ));
        }
        let mut response_used = reserve;
        while cursor.offset < cursor.upper.offset {
            if entries.len() as u64 >= limit {
                stop_reason = "record_limit";
                break;
            }
            if scanned_records >= options.scan_records
                || options.scan_bytes.saturating_sub(scanned_bytes) < MAX_RECORD_BYTES
            {
                stop_reason = "scan_budget";
                break;
            }
            let bytes = source.line(cursor.offset, cursor.upper.offset, MAX_RECORD_BYTES)?;
            if bytes.is_empty() {
                return Err(stale("unexpected EOF before snapshot boundary"));
            }
            scanned_bytes += bytes.len() as u64;
            {
                let header = &source.header;
                #[derive(Deserialize)]
                struct Kind {
                    #[serde(rename = "type")]
                    kind: String,
                }
                let kind: Kind =
                    serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
                if kind.kind == "artifact_end" {
                    let end =
                        document::decode_end(&bytes, header).map_err(|e| invalid(e.to_string()))?;
                    // Published prefix counts are evidence from status, not records
                    // validated by this cursor. Never put them in cursor.count.
                    let count = cursor
                        .count
                        .checked_add(cursor.tail_start.as_ref().map_or(0, |s| s.published_count))
                        .ok_or_else(|| invalid("record count overflow"))?;
                    let first_seq = cursor
                        .tail_start
                        .as_ref()
                        .and_then(|s| s.first_seq)
                        .or(cursor.first_seq);
                    document::validate_end_summary(
                        header,
                        &end,
                        count,
                        first_seq,
                        (count > 0).then_some(cursor.last_seq),
                    )
                    .map_err(|e| invalid(e.to_string()))?;
                    cursor.offset += bytes.len() as u64;
                    if cursor.offset != cursor.upper.offset {
                        return Err(invalid("data after artifact_end"));
                    }
                    cursor.finalized = true;
                    break;
                }
            }
            let row = source.row(&bytes, cursor.last_seq)?;
            scanned_records += 1;
            if package_active
                && row
                    .entry
                    .log_fields()
                    .is_some_and(|fields| fields.process.is_none())
            {
                package_unresolved += 1;
            }
            if let Some(sink) = crash_sink.as_deref_mut() {
                if predicate.matches(&row.entry)? {
                    sink.feed(signer, source, cursor.upper, cursor.offset, &bytes, &row)?;
                }
            } else if predicate.matches(&row.entry)? {
                let value = summary(
                    signer,
                    source,
                    cursor.upper,
                    cursor.offset,
                    &bytes,
                    &row,
                    options.preview,
                )?;
                let value = if entries.is_empty() {
                    shrink_summary(value, options.response - response_used)?
                        .ok_or_else(|| usage("response budget cannot hold a record reference"))?
                } else {
                    value
                };
                let size = encoded_len(&value)?;
                if response_used + size > options.response {
                    stop_reason = "response_budget";
                    break;
                }
                response_used += size;
                if let Some(output) = output.as_deref_mut() {
                    output.record(source, &row, predicate.device_time())?;
                }
                entries.push(value);
            }
            cursor.offset += bytes.len() as u64;
            cursor.last_seq = row.seq;
            cursor.count += 1;
            cursor.first_seq.get_or_insert(row.seq);
        }
        if cursor.offset == cursor.upper.offset
            && cursor.upper.seq.is_some_and(|seq| seq != cursor.last_seq)
        {
            return Err(invalid("visible sequence and byte boundary disagree"));
        }
        if cursor.offset == cursor.upper.offset
            && let Some(start) = &cursor.tail_start
            && start.published_count.checked_add(cursor.count) != source.published_count
        {
            return Err(invalid("published count and scanned suffix disagree"));
        }
        cursor.anchor = source.anchor(cursor.offset)?;
        source.check_current(cursor.upper)?;
        Ok(ScanPage {
            records: entries,
            start,
            scanned_bytes,
            scanned_records,
            stop_reason,
            file_bytes_read: source.bytes_read(),
            package_unresolved,
        })
    }

    fn complete(&self) -> bool {
        self.cursor.offset == self.cursor.upper.offset
    }

    fn response(&self, signer: &Signer, page: ScanPage) -> Result<Value> {
        query_response(
            &self.source,
            &self.cursor,
            &self.predicate,
            signer,
            page.records,
            page.start,
            page.scanned_bytes,
            page.scanned_records,
            page.stop_reason,
            page.file_bytes_read,
            if self.predicate.package_active() {
                page.package_unresolved
            } else {
                0
            },
        )
    }

    fn crash_response(
        &mut self,
        signer: &Signer,
        page: ScanPage,
        limit: usize,
        budget: usize,
    ) -> Result<Value> {
        let collector = self.crashes.take().expect("crash scan state");
        let prefix_scanned = self
            .cursor
            .tail_start
            .as_ref()
            .is_none_or(|s| s.published_count == 0);
        let (mut groups, incidents) = collector.finish();
        let total = groups.len();
        groups.truncate(limit);
        let mut response = json!({"schema_version":RESPONSE_SCHEMA,"type":"crashes","mode":self.cursor.mode,
            "groups":groups,"groups_total":total,"groups_truncated":total > limit,
            "incidents":incidents,
            "next_cursor":signer.seal(CURSOR_KIND,&self.cursor)?,
            "has_more":!self.complete(),"complete":self.complete(),"snapshot":self.cursor.upper,
            "evidence":{"summary":evidence_summary(&self.source, &self.cursor, prefix_scanned)},
            "time_window":self.predicate.time_window(self.cursor.time_anchor_ms),
            "scan":{"start_offset":page.start,"next_offset":self.cursor.offset,
                "last_seq":self.cursor.last_seq,"bytes":page.scanned_bytes,
                "records":page.scanned_records,"file_bytes_read":page.file_bytes_read,
                "stop_reason":page.stop_reason},
            "heuristic":"AndroidRuntime FATAL EXCEPTION, libc Fatal signal, DEBUG tombstone and ActivityManager ANR in; navigation hints, not diagnoses"});
        if let Some(warnings) = package_warnings(&self.predicate, page.package_unresolved) {
            response["warnings"] = warnings;
        }
        // Drop the latest groups first; the earliest first_seq is usually the
        // originating incident and keeps its reference.
        while encoded_len(&response)? > budget {
            let groups = response["groups"].as_array_mut().unwrap();
            if groups.pop().is_none() {
                return Err(usage("response budget cannot hold the crash summary"));
            }
            response["groups_truncated"] = true.into();
        }
        Ok(response)
    }
}

fn package_warnings(
    predicate: &super::query_filter::QueryFilter,
    unresolved: u64,
) -> Option<Value> {
    (predicate.package_active() && unresolved > 0).then(|| {
        json!([{"code":"package_unresolved","records":unresolved,
            "message":"--package cannot match these log records because they have no stored process name. Live text captures name processes from ps samples and ActivityManager/AndroidRuntime lifecycle lines; history rows, processes missed by both, and older captures stay unnamed. Zero matches are not evidence of no app logs; filter these with --pid or --tag."}])
    })
}

#[allow(clippy::too_many_arguments)]
fn query_response(
    source: &Source,
    cursor: &Cursor,
    predicate: &super::query_filter::QueryFilter,
    signer: &Signer,
    entries: Vec<Value>,
    start: u64,
    scanned_bytes: u64,
    scanned_records: u64,
    stop_reason: &str,
    bytes_read: u64,
    package_unresolved: u64,
) -> Result<Value> {
    let prefix_scanned = cursor
        .tail_start
        .as_ref()
        .is_none_or(|s| s.published_count == 0);
    Ok(
        json!({"schema_version":RESPONSE_SCHEMA,"type":"query","mode":cursor.mode,"source_schema_version":source.binding.schema,
        "source_status":source.status,"snapshot":cursor.upper,"artifact_finalized":cursor.finalized && prefix_scanned,
        "validation":{"scope":if prefix_scanned {"full_prefix"} else {"tail_suffix"},
            "start":cursor.tail_start.as_ref().map(|s| s.boundary),
            "skipped_records":cursor.tail_start.as_ref().map_or(0, |s| s.published_count),
            "footer_seen":cursor.finalized},
        "records":entries,"next_cursor":signer.seal(CURSOR_KIND,cursor)?,"has_more":cursor.offset<cursor.upper.offset,
        "complete":cursor.offset==cursor.upper.offset,"time_anchor_unix_ms":cursor.time_anchor_ms,
        "evidence":{"summary":evidence_summary(source, cursor, prefix_scanned)},
        "time_window":predicate.time_window(cursor.time_anchor_ms),
        "scan":{"start_offset":start,"next_offset":cursor.offset,"last_seq":cursor.last_seq,
            "bytes":scanned_bytes,"records":scanned_records,"file_bytes_read":bytes_read,"stop_reason":stop_reason},
        "integrity":"append_only_identity_and_boundary_anchors"}),
    )
    .map(|mut response: Value| {
        if let Some(warnings) = package_warnings(predicate, package_unresolved) {
            response["warnings"] = warnings;
        }
        response
    })
}

fn evidence_summary(source: &Source, cursor: &Cursor, prefix_scanned: bool) -> String {
    let complete = cursor.offset == cursor.upper.offset;
    let boundary = if complete { "complete" } else { "incomplete" };
    let validation = if !prefix_scanned {
        if cursor.finalized {
            "tail suffix and footer verified; nonempty prefix unverified"
        } else {
            "tail suffix only; nonempty prefix unverified"
        }
    } else if cursor.finalized {
        "full prefix verified and finalized"
    } else if complete {
        "prefix verified to boundary; footer not seen"
    } else {
        "full prefix verification unfinished"
    };
    // Deliberately use a finite vocabulary, never unbounded source diagnostics.
    let completeness = match source.status["capture_completeness"].as_str() {
        Some("unknown") => "unknown",
        Some("partial") => "partial",
        _ => "unavailable",
    };
    let timestamp = |field: &str| {
        source.status[field]
            .as_u64()
            .map_or_else(|| "unavailable".into(), |value| value.to_string())
    };
    let observed = timestamp("observed_unix_ms");
    let capture = if source.binding.session {
        let updated = timestamp("updated_unix_ms");
        format!(
            "{completeness} (session status updated_unix_ms={updated}, observed_unix_ms={observed}; current source observation, not fixed snapshot coverage)"
        )
    } else {
        let captured = timestamp("capture_observed_unix_ms");
        format!(
            "{completeness} (artifact footer capture_observed_unix_ms={captured}, observed_unix_ms={observed})"
        )
    };
    let gaps = match source.status["known_gap_count"].as_u64() {
        Some(0) => "none reported; zero does not prove lossless",
        Some(_) => "reported; additional gaps may be unknown",
        None => "unavailable; no lossless claim",
    };
    let text = format!(
        "Query boundary: {boundary} (this request's fixed boundary). Artifact: {validation}. Capture: {capture}. Known gaps: {gaps}."
    );
    debug_assert!(serde_json::to_vec(&text).unwrap().len() <= MAX_EVIDENCE_SUMMARY_BYTES);
    text
}

fn reference_row(source: &mut Source, reference: &RecordRef) -> Result<(Vec<u8>, Row)> {
    source.pin_boundary(reference.upper)?;
    if reference.bytes == 0
        || reference.bytes > MAX_RECORD_BYTES
        || reference.offset < source.data_start
        || reference
            .offset
            .checked_add(reference.bytes)
            .is_none_or(|end| end > reference.upper.offset)
    {
        return Err(stale("invalid record reference extent"));
    }
    let bytes = source.line(
        reference.offset,
        reference.offset + reference.bytes,
        MAX_RECORD_BYTES,
    )?;
    if bytes.len() as u64 != reference.bytes || hash(&bytes) != reference.digest {
        return Err(stale("referenced record changed"));
    }
    let row = source.row(&bytes, 0)?;
    if row.seq != reference.seq {
        return Err(stale("referenced record identity changed"));
    }
    Ok((bytes, row))
}

fn get(
    path: PathBuf,
    session: bool,
    reference: &RecordRef,
    response_budget: usize,
    offset: usize,
    view: View,
) -> Result<Value> {
    let mut source = Source::open(path, session, Some(&reference.binding))?;
    let (bytes, row) = reference_row(&mut source, reference)?;
    let projected;
    let bytes = if view == View::Decoded {
        projected = document::encode_record_projection(
            &source.header,
            &row.entry,
            &row.metadata,
            MAX_DECODED_BYTES as usize,
        )
        .map_err(|e| invalid(e.to_string()))?;
        projected.as_slice()
    } else {
        bytes.strip_suffix(b"\n").unwrap_or(&bytes)
    };
    let text = std::str::from_utf8(bytes).map_err(|e| invalid(e.to_string()))?;
    if offset > text.len() || !text.is_char_boundary(offset) {
        return Err(usage(
            "--text-offset must be an in-range UTF-8 byte boundary",
        ));
    }
    let response = |end: usize| {
        json!({"schema_version":RESPONSE_SCHEMA,"type":"get","seq":row.seq,"identity":row.entry.provenance,
        "source_schema_version":source.binding.schema,"view":view,"derived":view==View::Decoded,
        "projection_profile":if view==View::Decoded && row.entry.proto.is_some() {
            Some(ciallogcat::protobuf::PROJECTION_PROFILE)
        } else {None},
        "encoding":"json_utf8","text_offset":offset,"record_json_chunk":&text[offset..end],
        "next_text_offset":if end<text.len(){Some(end)}else{None},"complete":end==text.len(),
        "record_bytes":text.len(),"source_status":source.status,"snapshot":reference.upper,
        "file_bytes_read":source.bytes_read()})
    };
    let mut low = offset;
    let mut high = text.len().min(offset + response_budget);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let end = offset + preview(&text[offset..], mid - offset).len();
        if encoded_len(&response(end))? <= response_budget {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let end = offset + preview(&text[offset..], low - offset).len();
    if end == offset && offset < text.len() {
        return Err(usage("response budget cannot fit the next UTF-8 character"));
    }
    source.check_current(reference.upper)?;
    Ok(response(end))
}

#[allow(clippy::too_many_arguments)]
fn context(
    path: PathBuf,
    session: bool,
    signer: &Signer,
    reference: &RecordRef,
    options: &Options,
    before: u64,
    after: u64,
    same_pid: bool,
) -> Result<Value> {
    let mut source = Source::open(path, session, Some(&reference.binding))?;
    let (bytes, row) = reference_row(&mut source, reference)?;
    let pid = if same_pid {
        Some(
            row.entry
                .log_fields()
                .map(|fields| fields.pid)
                .ok_or_else(|| usage("--same-pid requires a log record target"))?,
        )
    } else {
        None
    };
    // Neighbours from other processes are scanned (and budgeted) but skipped.
    let wanted =
        |row: &Row| pid.is_none_or(|pid| row.entry.log_fields().is_some_and(|f| f.pid == pid));
    let mut skipped = 0u64;
    let target = summary(
        signer,
        &source,
        reference.upper,
        reference.offset,
        &bytes,
        &row,
        options.preview,
    )?;
    let reserve = 1024usize + encoded_len(&source.status)?;
    let target = shrink_summary(target, options.response - reserve)?
        .ok_or_else(|| usage("response budget cannot hold target reference"))?;
    let mut used = reserve + encoded_len(&target)?;
    let mut records = vec![target];
    let mut scan_bytes = bytes.len() as u64;
    let mut scan_records = 1;
    let mut before_count = 0;
    let mut after_count = 0;
    let mut position = reference.offset;
    let mut next_seq = row.seq;
    while before_count < before
        && position > source.data_start
        && scan_records < options.scan_records
    {
        let available = options.scan_bytes.saturating_sub(scan_bytes);
        if available == 0 {
            break;
        }
        let read_before = source.bytes_read();
        let Some(start) = source.previous_line_start(position, source.data_start, available)?
        else {
            scan_bytes += source.bytes_read() - read_before;
            break;
        };
        scan_bytes += source.bytes_read() - read_before;
        if position - start > options.scan_bytes.saturating_sub(scan_bytes) {
            break;
        }
        let bytes = source.line(start, position, MAX_RECORD_BYTES)?;
        scan_bytes += bytes.len() as u64;
        scan_records += 1;
        let row = source.row(&bytes, 0)?;
        if row.seq >= next_seq {
            return Err(invalid("context sequences are not strictly increasing"));
        }
        if !wanted(&row) {
            skipped += 1;
            position = start;
            next_seq = row.seq;
            continue;
        }
        let value = summary(
            signer,
            &source,
            reference.upper,
            start,
            &bytes,
            &row,
            options.preview,
        )?;
        let size = encoded_len(&value)?;
        if used + size > options.response {
            break;
        }
        used += size;
        records.insert(0, value);
        before_count += 1;
        position = start;
        next_seq = row.seq;
    }
    let partial_before = before_count < before && position > source.data_start;
    position = reference.offset + reference.bytes;
    let mut last = row.seq;
    let mut ended = false;
    while after_count < after
        && position < reference.upper.offset
        && scan_records < options.scan_records
    {
        if options.scan_bytes.saturating_sub(scan_bytes) < MAX_RECORD_BYTES {
            break;
        }
        let bytes = source.line(position, reference.upper.offset, MAX_RECORD_BYTES)?;
        scan_bytes += bytes.len() as u64;
        {
            let header = &source.header;
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
            if value["type"] == "artifact_end" {
                document::decode_end(&bytes, header).map_err(|e| invalid(e.to_string()))?;
                if position + bytes.len() as u64 != reference.upper.offset {
                    return Err(invalid("data after artifact_end"));
                }
                ended = true;
                break;
            }
        }
        let row = source.row(&bytes, last)?;
        scan_records += 1;
        if !wanted(&row) {
            skipped += 1;
            position += bytes.len() as u64;
            last = row.seq;
            continue;
        }
        let value = summary(
            signer,
            &source,
            reference.upper,
            position,
            &bytes,
            &row,
            options.preview,
        )?;
        let size = encoded_len(&value)?;
        if used + size > options.response {
            break;
        }
        used += size;
        records.push(value);
        after_count += 1;
        position += bytes.len() as u64;
        last = row.seq;
    }
    source.check_current(reference.upper)?;
    Ok(
        json!({"schema_version":RESPONSE_SCHEMA,"type":"context","source_schema_version":source.binding.schema,"target_seq":reference.seq,"records":records,
        "before_returned":before_count,"after_returned":after_count,"partial_before":partial_before,
        "partial_after":after_count<after && position<reference.upper.offset && !ended,
        "snapshot":reference.upper,"source_status":source.status,
        "same_pid":pid,"skipped_other_pid":skipped,
        "scan":{"bytes":scan_bytes,"records":scan_records,"file_bytes_read":source.bytes_read()}}),
    )
}
