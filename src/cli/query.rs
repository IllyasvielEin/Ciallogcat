//! Bounded queries over the native append-only archive format.
mod source;
mod tokens;

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;

use ciallogcat::document;
use ciallogcat::document::MAX_RECORD_BYTES;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Result, emit, error, io_error, no_flags, number, required, usage};
use source::{Anchor, Binding, Boundary, Row, Source, invalid, stale};
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
    binding: Binding,
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

#[derive(Clone, Deserialize, Serialize)]
struct TailStart {
    boundary: Boundary,
    published_count: u64,
    first_seq: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
struct RecordRef {
    version: u8,
    binding: Binding,
    upper: Boundary,
    offset: u64,
    bytes: u64,
    seq: u64,
    digest: String,
}

// Derived JSON can expand a compact payload (especially NUL/invalid UTF-8)
// beyond the archive line limit. Keep it independently bounded.
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;

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
            preview: number(flags, "--preview-bytes", 2048, 0, 16384)? as usize,
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
        let reference = record_reference(&signer, &mut flags)?;
        let view = match flags.remove("--view").as_deref().unwrap_or("stored") {
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
    let options = Options::parse(&mut flags, response_budget)?;
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
                return stream_query(path, session, &signer, &options, flags, all, format);
            }
            _ => return Err(usage("--format must be json, jsonl or text")),
        }
    }
    let response = match command {
        "query" => {
            let mut state = QueryState::open(path, session, &signer, flags)?;
            let page = state.scan_page(&signer, &options, None)?;
            state.response(&signer, page)?
        }
        "context" => {
            let reference = record_reference(&signer, &mut flags)?;
            let before = number(&mut flags, "--before", 10, 0, 100)?;
            let after = number(&mut flags, "--after", 10, 0, 100)?;
            no_flags(&flags)?;
            context(path, session, &signer, &reference, &options, before, after)?
        }
        _ => return Err(usage("unsupported query command")),
    };
    emit_response(response, response_budget)
}

fn record_reference(signer: &Signer, flags: &mut BTreeMap<String, String>) -> Result<RecordRef> {
    let reference: RecordRef = signer.open("r2", &required(flags, "--ref")?)?;
    if reference.version != 2 {
        return Err(stale("unsupported reference version"));
    }
    Ok(reference)
}

fn emit_response(response: Value, budget: usize) -> Result<()> {
    if encoded_len(&response)? > budget {
        return Err(error(
            "response_budget",
            "response envelope exceeds requested byte budget",
            2,
        ));
    }
    emit(&response)
}

struct StreamOutput<'a> {
    writer: &'a mut dyn Write,
    text: Option<super::text_output::TextOptions>,
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
) -> Result<()> {
    if all && flags.get("--mode").is_some_and(|mode| mode != "snapshot") {
        return Err(usage(
            "--all requires snapshot mode; tail is a single non-waiting page",
        ));
    }
    let text = if format == "text" {
        Some(super::text_output::TextOptions::parse(&mut flags)?)
    } else {
        None
    };
    let mut stdout = io::stdout().lock();
    let mut output = StreamOutput {
        writer: &mut stdout,
        text,
        records: 0,
    };
    let mut state = QueryState::open(path, session, signer, flags)?;
    let mut pages = 0u64;
    let mut scanned = 0u64;
    let mut scanned_bytes = 0u64;
    let mut file_bytes = 0u64;
    loop {
        let page = state.scan_page(signer, options, Some(&mut output))?;
        pages += 1;
        scanned += page.scanned_records;
        scanned_bytes += page.scanned_bytes;
        file_bytes += page.file_bytes_read;
        if !all || state.complete() {
            let mut response = state.response(signer, page)?;
            output.writer.flush().map_err(io_error)?;
            let fields = response.as_object_mut().unwrap();
            fields.remove("records");
            fields.insert("type".into(), "query_summary".into());
            fields.insert("format".into(), format.into());
            if let Some(text) = &output.text {
                fields.insert(
                    "text_presentation".into(),
                    json!({
                        "process_width": text.process_width(),
                        "process_names_truncated": text.process_names_truncated(),
                    }),
                );
            }
            fields.insert("derived".into(), true.into());
            fields.insert("records_returned".into(), output.records.into());
            fields.insert("pages".into(), pages.into());
            fields.insert("total_scanned_records".into(), scanned.into());
            fields.insert("total_scanned_bytes".into(), scanned_bytes.into());
            fields.insert("total_file_bytes_read".into(), file_bytes.into());
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

fn summary(
    signer: &Signer,
    source: &Source,
    upper: Boundary,
    offset: u64,
    bytes: &[u8],
    row: &Row,
    preview_bytes: usize,
) -> Result<Value> {
    let reference = RecordRef {
        version: 2,
        binding: source.binding.clone(),
        upper,
        offset,
        bytes: bytes.len() as u64,
        seq: row.seq,
        digest: hash(bytes),
    };
    let message = preview(&row.entry.message, preview_bytes);
    let mut value = json!({
        "record_ref":signer.seal("r2", &reference)?, "seq":row.seq,"kind":row.kind,
        "identity":row.entry.provenance,
        "observed_unix_ms":row.entry.provenance.and_then(|p| p.observed_unix_ms),
        "identity_scope":"capture_session",
        "message_preview":message,"message_bytes":row.entry.message.len(),
        "preview_truncated":message.len() != row.entry.message.len(),
        "text_loss":row.entry.text_loss,
    });
    if let Some(log) = row.entry.log_fields() {
        let fields = value.as_object_mut().unwrap();
        fields.insert("time".into(), preview(log.time, 128).into());
        fields.insert("pid".into(), log.pid.into());
        fields.insert("tid".into(), log.tid.into());
        fields.insert("level".into(), log.level.short().into());
        fields.insert("tag".into(), preview(log.tag, 256).into());
        fields.insert(
            "process".into(),
            json!(log.process.map(|p| preview(p, 256))),
        );
        fields.insert("process_source".into(), json!(log.process_source));
        fields.insert(
            "metadata_preview_truncated".into(),
            (log.time.len() > 128
                || log.tag.len() > 256
                || log.process.is_some_and(|p| p.len() > 256))
            .into(),
        );
    }
    Ok(value)
}

fn shrink_summary(mut value: Value, allowance: usize) -> Result<Option<Value>> {
    while encoded_len(&value)? > allowance {
        let text = value["message_preview"].as_str().unwrap_or("");
        if text.is_empty() {
            // Keep the stable reference even when metadata itself is large.
            for key in ["time", "tag", "process"] {
                value.as_object_mut().unwrap().remove(key);
            }
            value["metadata_preview_truncated"] = true.into();
            return Ok((encoded_len(&value)? <= allowance).then_some(value));
        }
        value["message_preview"] = preview(text, text.len() / 2).into();
        value["preview_truncated"] = true.into();
    }
    Ok(Some(value))
}

struct QueryState {
    source: Source,
    cursor: Cursor,
    predicate: super::query_filter::QueryFilter,
    limit: u64,
    scanned: bool,
}

struct ScanPage {
    records: Vec<Value>,
    start: u64,
    scanned_bytes: u64,
    scanned_records: u64,
    stop_reason: &'static str,
    file_bytes_read: u64,
}

impl QueryState {
    fn open(
        path: PathBuf,
        session: bool,
        signer: &Signer,
        mut flags: BTreeMap<String, String>,
    ) -> Result<Self> {
        let mode = match flags.remove("--mode").as_deref().unwrap_or("snapshot") {
            "snapshot" => Mode::Snapshot,
            "tail" if session => Mode::Tail,
            "tail" => return Err(usage("tail mode requires --session-dir")),
            _ => return Err(usage("--mode must be snapshot or tail")),
        };
        let start_at = flags.remove("--start-at");
        let token = flags.remove("--cursor");
        let from_end = match start_at.as_deref().unwrap_or("beginning") {
            "beginning" => false,
            "end" if mode == Mode::Tail => true,
            "end" => return Err(usage("--start-at end requires --mode tail")),
            _ => return Err(usage("--start-at must be beginning or end")),
        };
        if start_at.is_some() && token.is_some() {
            return Err(usage("--start-at cannot be combined with --cursor"));
        }
        let previous: Option<Cursor> = token.map(|token| signer.open("c4", &token)).transpose()?;
        if previous
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
            .and_then(|c| c.time_anchor_ms)
            .unwrap_or_else(super::now);
        let predicate = super::query_filter::QueryFilter::parse(&mut flags, anchor)?;
        let filter = predicate.fingerprint();
        let limit = number(&mut flags, "--limit", 100, 1, 1000)?;
        no_flags(&flags)?;
        if let Some(cursor) = &previous
            && (cursor.version != 4
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
                version: 4,
                binding: source.binding.clone(),
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
        })
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
        let reserve = encoded_len(&query_response(
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
        )?)? + 128;
        if reserve + 2048 >= options.response {
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
            if predicate.matches(&row.entry)? {
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
        )
    }
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
) -> Result<Value> {
    let prefix_scanned = cursor
        .tail_start
        .as_ref()
        .is_none_or(|s| s.published_count == 0);
    Ok(
        json!({"schema_version":2,"type":"query","mode":cursor.mode,"source_schema_version":source.binding.schema,
        "source_status":source.status,"snapshot":cursor.upper,"artifact_finalized":cursor.finalized && prefix_scanned,
        "validation":{"scope":if prefix_scanned {"full_prefix"} else {"tail_suffix"},
            "start":cursor.tail_start.as_ref().map(|s| s.boundary),
            "skipped_records":cursor.tail_start.as_ref().map_or(0, |s| s.published_count),
            "footer_seen":cursor.finalized},
        "records":entries,"next_cursor":signer.seal("c4",cursor)?,"has_more":cursor.offset<cursor.upper.offset,
        "complete":cursor.offset==cursor.upper.offset,"time_anchor_unix_ms":cursor.time_anchor_ms,
        "time_window":predicate.time_window(cursor.time_anchor_ms),
        "scan":{"start_offset":start,"next_offset":cursor.offset,"last_seq":cursor.last_seq,
            "bytes":scanned_bytes,"records":scanned_records,"file_bytes_read":bytes_read,"stop_reason":stop_reason},
        "integrity":"append_only_identity_and_boundary_anchors"}),
    )
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
        json!({"schema_version":2,"type":"get","seq":row.seq,"identity":row.entry.provenance,
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

fn context(
    path: PathBuf,
    session: bool,
    signer: &Signer,
    reference: &RecordRef,
    options: &Options,
    before: u64,
    after: u64,
) -> Result<Value> {
    let mut source = Source::open(path, session, Some(&reference.binding))?;
    let (bytes, row) = reference_row(&mut source, reference)?;
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
        json!({"schema_version":2,"type":"context","source_schema_version":source.binding.schema,"target_seq":reference.seq,"records":records,
        "before_returned":before_count,"after_returned":after_count,"partial_before":partial_before,
        "partial_after":after_count<after && position<reference.upper.offset && !ended,
        "snapshot":reference.upper,"source_status":source.status,
        "scan":{"bytes":scan_bytes,"records":scan_records,"file_bytes_read":source.bytes_read()}}),
    )
}
