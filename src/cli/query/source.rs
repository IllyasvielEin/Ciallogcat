use std::cell::Cell;
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use ciallogcat::document::{self, ArtifactEnd, DocumentHeader, MAX_RECORD_BYTES};
use ciallogcat::model::LogEntry;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::{Result, error, io_error, now, regular};
use super::tokens::hash;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Binding {
    pub path: String,
    pub identity: String,
    pub schema: u8,
    pub session: bool,
    pub prefix_len: u64,
    pub prefix_hash: String,
    pub artifact_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Boundary {
    pub offset: u64,
    pub seq: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Anchor {
    pub start: u64,
    pub bytes: u64,
    pub hash: String,
}

struct Counted {
    file: File,
    bytes: u64,
    upper: u64,
    position: u64,
}
impl Read for Counted {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let size = bytes
            .len()
            .min(self.upper.saturating_sub(self.position) as usize);
        let read = self.file.read(&mut bytes[..size])?;
        self.bytes += read as u64;
        self.position += read as u64;
        Ok(read)
    }
}
impl Seek for Counted {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.position = self.file.seek(position)?;
        Ok(self.position)
    }
}

pub(super) struct Source {
    reader: BufReader<Counted>,
    path: PathBuf,
    pub binding: Binding,
    pub upper: Boundary,
    fence: Boundary,
    pub published_count: Option<u64>,
    pub data_start: u64,
    pub header: DocumentHeader,
    pub footer: Option<ArtifactEnd>,
    pub status: Value,
    pub active: bool,
    // A closure can race a get/context read. Remember decoded sequence bounds
    // for the final consistency check without adding filesystem I/O per row.
    highest_seen_seq: Cell<u64>,
}

pub(super) struct Row {
    pub seq: u64,
    pub entry: LogEntry,
    pub kind: &'static str,
    pub metadata: document::RecordMetadata,
}

pub(super) fn invalid(message: impl Into<String>) -> super::super::Error {
    error("invalid_source", message, 3)
}
pub(super) fn stale(message: impl Into<String>) -> super::super::Error {
    error("stale_cursor", message, 3)
}

fn identity(file: &File, metadata: &Metadata) -> Result<String> {
    #[cfg(unix)]
    {
        let _ = file;
        use std::os::unix::fs::MetadataExt;
        Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let _ = metadata;
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        // SAFETY: File owns this live handle; the output points to sufficient
        // writable storage. The structure is read only after API success.
        let success =
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) };
        if success == 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        let info = unsafe { info.assume_init() };
        Ok(format!(
            "windows:{}:{}:{}",
            info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow,
        ))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        let created = metadata
            .created()
            .map_err(io_error)?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io_error)?;
        Ok(format!(
            "created:{}:{}",
            created.as_secs(),
            created.subsec_nanos()
        ))
    }
}

impl Source {
    pub fn open(path: PathBuf, session: bool, expected: Option<&Binding>) -> Result<Self> {
        let (path, status_value) = if session {
            let directory = super::super::session_path(
                path.to_str()
                    .ok_or_else(|| invalid("source path must be Unicode"))?,
            )?;
            let status = super::super::session_management::read_effective_query_status(&directory)?;
            if status["schema_version"] != 1 || status["type"] != "status" {
                return Err(invalid("unsupported session status envelope"));
            }
            (directory.join("logs.jsonl"), Some(status))
        } else {
            (path, None)
        };
        let session_schema = status_value
            .as_ref()
            .map(|value| match value["record_schema_version"].as_u64() {
                Some(schema) if schema == u64::from(document::SCHEMA_VERSION) => {
                    Ok(document::SCHEMA_VERSION)
                }
                None if value["bytes"].as_u64() == Some(0) && value["visible_end"].is_null() => {
                    Err(error(
                        "source_not_ready",
                        "native header is not visible yet",
                        3,
                    ))
                }
                _ => Err(error(
                    "unsupported_format",
                    "session uses an unsupported archive schema; current native format is required",
                    3,
                )),
            })
            .transpose()?;
        regular(&path)?;
        let path = fs::canonicalize(path).map_err(io_error)?;
        let file = File::open(&path).map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() {
            return Err(invalid("source is not a regular file"));
        }
        let id = identity(&file, &metadata)?;
        let path_hash = hash(path.as_os_str().as_encoded_bytes());
        let schema_hint = session_schema.or_else(|| expected.map(|b| b.schema));
        let (upper, status, active) = if let Some(value) = &status_value {
            let schema = session_schema.expect("validated native session schema");
            if expected.is_some_and(|binding| binding.schema != schema) {
                return Err(stale("session record schema changed"));
            }
            let offset = value["visible_end"]["offset"].as_u64();
            let seq = value["visible_end"]["seq"].as_u64();
            let upper = Boundary {
                offset: offset.ok_or_else(|| invalid("session lacks a visible byte boundary"))?,
                seq: Some(seq.ok_or_else(|| invalid("session lacks a visible sequence boundary"))?),
            };
            let state = value["state"]
                .as_str()
                .ok_or_else(|| invalid("session lacks state"))?;
            if !matches!(
                state,
                "starting"
                    | "running"
                    | "reconnecting"
                    | "stopping"
                    | "completed"
                    | "failed"
                    | "unknown"
                    | "abandoned"
            ) {
                return Err(invalid("unsupported session state"));
            }
            let updated = value["updated_unix_ms"]
                .as_u64()
                .ok_or_else(|| invalid("session lacks observation time"))?;
            let terminal = matches!(state, "completed" | "failed" | "abandoned");
            let stale = !terminal && now().saturating_sub(updated) > 5000;
            let excerpt = |key: &str| value[key].as_str().map(|s| super::preview(s, 96));
            let diagnostic_truncated = [
                "reason",
                "error",
                "readiness",
                "last_diagnostic",
                "last_stream_error",
            ]
            .iter()
            .any(|key| value[key].as_str().is_some_and(|s| s.len() > 96));
            (
                upper,
                json!({"state": if stale {"unknown"} else {state}, "updated_unix_ms":updated,
                "stale":stale,"terminal":terminal,"observed_unix_ms":now(),
                "worker_exit_confirmed":value["worker_exit_confirmed"].as_bool(),
                "management_closed_unix_ms":value["management_closed_unix_ms"].as_u64(),
                "omitted_by_limit":value["omitted_by_limit"].as_bool(),
                "discarded_entries":value["discarded_entries"].as_u64(),
                "capture_completeness":excerpt("capture_completeness"),
                "stream_interruptions":value["stream_interruptions"].as_u64(),
                "last_stream_error":excerpt("last_stream_error"),
                "diagnostics_complete":value["diagnostics_complete"].as_bool(),
                "reason":excerpt("reason"),"error":excerpt("error"),"readiness":excerpt("readiness"),
                "last_diagnostic":excerpt("last_diagnostic"),"diagnostic_preview_truncated":diagnostic_truncated}),
                true,
            )
        } else {
            (
                Boundary {
                    offset: metadata.len(),
                    seq: None,
                },
                json!({"state":"offline","observed_unix_ms":now(),"stale":false}),
                false,
            )
        };
        if upper.offset > metadata.len() {
            return Err(stale("file is shorter than the published boundary"));
        }
        if let Some(binding) = expected
            && (binding.path != path_hash || binding.identity != id || binding.session != session)
        {
            return Err(stale("token belongs to a different or replaced source"));
        }
        if expected.is_some_and(|binding| binding.prefix_len > upper.offset) {
            return Err(stale("file was truncated below its identity prefix"));
        }
        if upper.offset == 0 {
            return Err(if active {
                error("source_not_ready", "native header is not visible yet", 3)
            } else {
                invalid("empty input is not a native document")
            });
        }
        let mut reader = BufReader::new(Counted {
            file,
            bytes: 0,
            upper: upper.offset,
            position: 0,
        });
        // Header validation is counted I/O outside logical record-scan budgets.
        let first = read_line(&mut reader, active, 0, upper.offset, MAX_RECORD_BYTES)?;
        let value: Value =
            ciallogcat::json_input::from_slice(&first).map_err(|e| invalid(e.to_string()))?;
        let schema = value["schema_version"].as_u64();
        if schema != Some(u64::from(document::SCHEMA_VERSION)) {
            return Err(error(
                "unsupported_format",
                "input uses an unsupported archive schema; current native document header is required",
                3,
            ));
        }
        if schema_hint.is_some_and(|hint| Some(u64::from(hint)) != schema) {
            return Err(invalid("session and document record schemas differ"));
        }
        let header = document::decode_header(&first).map_err(|e| invalid(e.to_string()))?;
        if let Some(value) = &status_value
            && (value["artifact_id"].as_str() != Some(&header.artifact_id.to_string())
                || value["session_id"].as_str()
                    != header.session_id.map(|id| id.to_string()).as_deref())
        {
            return Err(stale("native status and document identities differ"));
        }
        let mut source = Self {
            reader,
            path,
            binding: Binding {
                path: path_hash,
                identity: id,
                schema: header.schema_version,
                session,
                prefix_len: 0,
                prefix_hash: String::new(),
                artifact_id: header.artifact_id.to_string(),
            },
            upper,
            fence: upper,
            published_count: status_value.as_ref().and_then(|v| v["entries"].as_u64()),
            data_start: first.len() as u64,
            header,
            footer: None,
            status,
            active,
            highest_seen_seq: Cell::new(0),
        };
        source.status["artifact_id"] = json!(source.header.artifact_id);
        source.status["session_id"] = json!(source.header.session_id);
        if !active {
            // Include preceding LF so an exactly maximal footer can be located.
            let start = source
                .previous_line_start(upper.offset, source.data_start, MAX_RECORD_BYTES + 1)?
                .ok_or_else(|| invalid("native artifact lacks a bounded artifact_end"))?;
            let bytes = source.line(start, upper.offset, MAX_RECORD_BYTES)?;
            let footer =
                document::decode_end(&bytes, &source.header).map_err(|e| invalid(e.to_string()))?;
            source.upper.seq = footer.last_seq.or(Some(0));
            source.fence = source.upper;
            source.status["capture_completeness"] = json!(footer.capture.completeness);
            source.status["capture_state"] = json!(super::preview(&footer.capture.state, 96));
            source.status["capture_observed_unix_ms"] = json!(footer.capture.observed_unix_ms);
            source.status["known_gap_count"] = json!(footer.capture.known_gaps.len());
            source.status["capture_status_source"] = json!("artifact_footer");
            source.footer = Some(footer);
        }
        let prefix_len = expected.map_or(upper.offset.min(512), |b| b.prefix_len);
        if prefix_len > upper.offset {
            return Err(stale("file was truncated below its identity prefix"));
        }
        source.binding.prefix_len = prefix_len;
        source.binding.prefix_hash = hash(&source.block(0, prefix_len)?);
        if expected.is_some_and(|binding| binding != &source.binding) {
            return Err(stale("source prefix, schema or artifact identity changed"));
        }
        Ok(source)
    }

    pub fn reopen(&self) -> Result<Self> {
        let path = if self.binding.session {
            self.path
                .parent()
                .ok_or_else(|| invalid("session path lacks parent"))?
                .to_path_buf()
        } else {
            self.path.clone()
        };
        Self::open(path, self.binding.session, Some(&self.binding))
    }

    /// Pin every subsequent record/control read to the caller's original
    /// snapshot. Ref/context calls must not inherit a later session watermark.
    pub fn pin_boundary(&mut self, upper: Boundary) -> Result<()> {
        if upper.seq.is_none() {
            return Err(stale("snapshot lacks its sequence boundary"));
        }
        if upper.offset > self.upper.offset
            || upper
                .seq
                .zip(self.upper.seq)
                .is_some_and(|(requested, published)| requested > published)
            || upper.offset < self.data_start
        {
            return Err(stale("published source boundary moved backwards"));
        }
        let position = self
            .reader
            .stream_position()
            .map_err(io_error)?
            .min(upper.offset);
        self.reader
            .seek(SeekFrom::Start(position))
            .map_err(io_error)?;
        self.reader.get_mut().upper = upper.offset;
        self.fence = upper;
        Ok(())
    }

    pub fn bytes_read(&self) -> u64 {
        self.reader.get_ref().bytes
    }

    /// Establish an end position using only bounded first/last rows and an
    /// optional footer. The published count is NOT a verified prefix count.
    pub fn end_start(&mut self) -> Result<(u64, Option<u64>, bool)> {
        let count = self
            .published_count
            .ok_or_else(|| invalid("native session lacks a published record count"))?;
        let mut end = self.upper.offset;
        let mut footer = None;
        let mut last = None;
        if end > self.data_start {
            let start = self
                .previous_line_start(end, self.data_start, MAX_RECORD_BYTES + 1)?
                .ok_or_else(|| invalid("published final line exceeds its encoded byte limit"))?;
            let bytes = self.line(start, end, MAX_RECORD_BYTES)?;
            #[derive(Deserialize)]
            struct Kind {
                #[serde(rename = "type")]
                kind: String,
            }
            let kind: Kind = serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
            if kind.kind == "artifact_end" {
                footer = Some(
                    document::decode_end(&bytes, &self.header)
                        .map_err(|e| invalid(e.to_string()))?,
                );
                end = start;
            } else {
                last = Some((start, self.row(&bytes, 0)?.seq));
            }
        }
        if last.is_none() && end > self.data_start {
            let start = self
                .previous_line_start(end, self.data_start, MAX_RECORD_BYTES + 1)?
                .ok_or_else(|| {
                    invalid("record before artifact_end exceeds its encoded byte limit")
                })?;
            let bytes = self.line(start, end, MAX_RECORD_BYTES)?;
            last = Some((start, self.row(&bytes, 0)?.seq));
        }
        let (first_seq, last_seq) = if let Some((last_offset, last_seq)) = last {
            let first_seq = if last_offset == self.data_start {
                last_seq
            } else {
                let bytes = self.line(self.data_start, last_offset, MAX_RECORD_BYTES)?;
                self.row(&bytes, 0)?.seq
            };
            if first_seq > last_seq
                || count == 0
                || count > last_seq - first_seq + 1
                || (last_offset == self.data_start && count != 1)
                || (last_offset != self.data_start && (count < 2 || first_seq == last_seq))
            {
                return Err(invalid(
                    "published count disagrees with first/last record bounds",
                ));
            }
            (Some(first_seq), Some(last_seq))
        } else {
            if count != 0 {
                return Err(invalid(
                    "empty native session has a nonzero published count",
                ));
            }
            (None, None)
        };
        if self.upper.seq != Some(last_seq.unwrap_or(0)) {
            return Err(invalid("visible sequence and byte boundary disagree"));
        }
        if let Some(footer) = &footer {
            document::validate_end_summary(&self.header, footer, count, first_seq, last_seq)
                .map_err(|e| invalid(e.to_string()))?;
        }
        Ok((count, first_seq, footer.is_some()))
    }

    pub fn block(&mut self, start: u64, size: u64) -> Result<Vec<u8>> {
        if start
            .checked_add(size)
            .is_none_or(|end| end > self.fence.offset)
        {
            return Err(stale("requested bytes are outside the visible boundary"));
        }
        self.reader.seek(SeekFrom::Start(start)).map_err(io_error)?;
        let mut bytes = vec![0; size as usize];
        // seek discarded the buffered read-ahead. Control anchors read exactly
        // their requested bytes, rather than pulling another 8 KiB each time.
        self.reader
            .get_mut()
            .read_exact(&mut bytes)
            .map_err(io_error)?;
        Ok(bytes)
    }

    pub fn line(&mut self, start: u64, upper: u64, limit: u64) -> Result<Vec<u8>> {
        if start > upper || upper > self.fence.offset {
            return Err(stale("invalid record boundary"));
        }
        read_line(&mut self.reader, self.active, start, upper, limit)
    }

    pub fn row(&self, bytes: &[u8], previous: u64) -> Result<Row> {
        if self.active && !bytes.ends_with(b"\n") {
            return Err(invalid("published boundary contains an incomplete record"));
        }
        let (entry, meta) = document::decode_record(bytes, &self.header, previous)
            .map_err(|e| invalid(e.to_string()))?;
        let kind = match entry.kind {
            ciallogcat::model::LogEntryKind::Log => "entry",
            ciallogcat::model::LogEntryKind::Marker => "event",
            ciallogcat::model::LogEntryKind::Unparsed => "unparsed",
        };
        let row = Row {
            seq: meta.seq,
            entry,
            kind,
            metadata: meta,
        };
        self.highest_seen_seq
            .set(self.highest_seen_seq.get().max(row.seq));
        if self.fence.seq.is_some_and(|seq| row.seq > seq) {
            return Err(invalid("record exceeds pinned snapshot sequence boundary"));
        }
        Ok(row)
    }

    pub fn anchor(&mut self, end: u64) -> Result<Anchor> {
        let start = end.saturating_sub(64);
        let bytes = end - start;
        Ok(Anchor {
            start,
            bytes,
            hash: hash(&self.block(start, bytes)?),
        })
    }

    pub fn check_anchor(&mut self, anchor: &Anchor) -> Result<()> {
        if anchor.bytes > 64 || hash(&self.block(anchor.start, anchor.bytes)?) != anchor.hash {
            return Err(stale("previously observed boundary changed"));
        }
        Ok(())
    }

    pub fn check_current(&self, upper: Boundary) -> Result<()> {
        if self.binding.session
            && let Some(closed) = super::super::session_management::closure_status(
                self.path
                    .parent()
                    .ok_or_else(|| invalid("session path lacks parent"))?,
            )?
        {
            let offset = closed["visible_end"]["offset"].as_u64();
            let seq = closed["visible_end"]["seq"].as_u64();
            if offset.is_none_or(|value| upper.offset > value)
                || upper.seq.zip(seq).is_some_and(|(a, b)| a > b)
                || seq.is_none_or(|value| self.highest_seen_seq.get() > value)
            {
                return Err(stale(
                    "session was administratively closed before this boundary",
                ));
            }
        }
        regular(&self.path)?;
        let file = File::open(&self.path).map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        if identity(&file, &metadata)? != self.binding.identity || metadata.len() < upper.offset {
            return Err(stale("source was replaced or truncated while querying"));
        }
        Ok(())
    }

    /// Find one preceding record boundary without scanning the file prefix.
    /// None means the provided backward byte budget was insufficient.
    pub fn previous_line_start(
        &mut self,
        end: u64,
        lower: u64,
        budget: u64,
    ) -> Result<Option<u64>> {
        if end <= lower {
            return Ok(None);
        }
        let minimum = end.saturating_sub(budget).max(lower);
        let mut position = end;
        while position > minimum {
            let start = position.saturating_sub(4096).max(minimum);
            let bytes = self.block(start, position - start)?;
            let content = if position == end {
                bytes.strip_suffix(b"\n").unwrap_or(&bytes)
            } else {
                &bytes
            };
            if let Some(index) = content.iter().rposition(|byte| *byte == b'\n') {
                return Ok(Some(start + index as u64 + 1));
            }
            position = start;
        }
        Ok((minimum == lower).then_some(lower))
    }
}

fn read_line(
    reader: &mut BufReader<Counted>,
    active: bool,
    start: u64,
    upper: u64,
    limit: u64,
) -> Result<Vec<u8>> {
    if reader.stream_position().map_err(io_error)? != start {
        reader.seek(SeekFrom::Start(start)).map_err(io_error)?;
    }
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(upper.saturating_sub(start).min(limit + 1))
        .read_until(b'\n', &mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("record or metadata exceeds its encoded byte limit"));
    }
    if active && !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(invalid("published boundary contains an incomplete line"));
    }
    Ok(bytes)
}
