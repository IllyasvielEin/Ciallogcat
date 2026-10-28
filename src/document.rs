//! Offline, bounded JSONL documents. Parsing never mutates a caller's live view.
//!
//! A finalized artifact is not a claim that its source capture was complete.
use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::log_store::LogStore;
use crate::model::{
    Level, LogBuffer, LogEntry, LogEntryKind, ProcessSource, ProtoEvidence, TextLoss,
};
use crate::record_io::MAX_RECORD_BYTES;
use crate::session::{RecordIdentity, SessionId};

/// Arbitrary-precision JSON numbers retain extension values through flatten,
/// selection and format conversion; core DTO fields keep their typed bounds.
pub type Extensions = BTreeMap<String, Value>;

pub const COMPACT_RECORD_ENCODING: &str = "proto_raw_v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeFormat {
    V2,
    CompactV3,
}

impl NativeFormat {
    fn version(self) -> u8 {
        match self {
            Self::V2 => 2,
            Self::CompactV3 => 3,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceMetadata {
    pub kind: String,
    pub device_serial: Option<String>,
    pub requested_buffers: Option<Vec<LogBuffer>>,
    /// The requested list describes the snapshot configuration, not every row.
    pub requested_buffers_scope: Option<String>,
    pub transport_format: Option<String>,
    #[serde(flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DocumentHeader {
    pub schema_version: u8,
    #[serde(rename = "type")]
    pub record_type: String,
    pub artifact_id: SessionId,
    pub session_id: Option<SessionId>,
    pub source: SourceMetadata,
    pub derived_from_artifact: Option<SessionId>,
    #[serde(default)]
    pub normalization: Extensions,
    #[serde(flatten)]
    pub extra: Extensions,
}

impl DocumentHeader {
    /// Select the encoding for a new snapshot or stream. Existing documents
    /// must use `Document::convert_format` to validate before removing fields.
    /// V2's unknown declarations are never overwritten during conversion.
    pub fn set_format(&mut self, format: NativeFormat) -> Result<(), DocumentError> {
        validate_header(self)?;
        if self.schema_version == format.version() {
            return Ok(());
        }
        if format == NativeFormat::CompactV3 {
            no_reserved(&self.extra, &["record_encoding"])?;
            no_reserved(&self.normalization, &["proto_projection"])?;
            if self
                .normalization
                .get("protobuf_raw_payload")
                .is_some_and(|v| v != "preserved_base64")
            {
                return Err(DocumentError::invalid(
                    "conflicting protobuf_raw_payload declaration",
                ));
            }
            self.extra
                .insert("record_encoding".into(), COMPACT_RECORD_ENCODING.into());
            self.normalization.insert(
                "proto_projection".into(),
                crate::protobuf::PROJECTION_PROFILE.into(),
            );
            self.normalization
                .insert("protobuf_raw_payload".into(), "preserved_base64".into());
        } else {
            self.extra.remove("record_encoding");
            self.normalization.remove("proto_projection");
        }
        self.schema_version = format.version();
        Ok(())
    }

    pub fn live(session_id: SessionId, device_serial: String, buffers: Vec<LogBuffer>) -> Self {
        Self {
            schema_version: 2,
            record_type: "document".into(),
            artifact_id: SessionId::new(),
            session_id: Some(session_id),
            source: SourceMetadata {
                kind: "adb".into(),
                device_serial: Some(device_serial),
                requested_buffers: Some(buffers),
                requested_buffers_scope: Some("snapshot_configuration".into()),
                transport_format: Some("threadtime".into()),
                extra: Extensions::new(),
            },
            derived_from_artifact: None,
            normalization: Extensions::new(),
            extra: Extensions::new(),
        }
    }

    fn legacy() -> Self {
        Self {
            schema_version: 2,
            record_type: "document".into(),
            artifact_id: SessionId::new(),
            session_id: None,
            source: SourceMetadata {
                kind: "legacy_v1".into(),
                device_serial: None,
                requested_buffers: None,
                requested_buffers_scope: None,
                transport_format: None,
                extra: Extensions::new(),
            },
            derived_from_artifact: None,
            normalization: Extensions::new(),
            extra: Extensions::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExportScope {
    /// `retained`, `filtered`, or `legacy_import`. Sequence gaps are not loss proof.
    pub kind: String,
    pub source_first_seq: Option<u64>,
    pub source_last_seq: Option<u64>,
    pub source_record_count: u64,
    /// Known records removed before the retained range; None means unknown.
    pub omitted_before: Option<u64>,
    pub filter: Option<Value>,
    #[serde(flatten)]
    pub extra: Extensions,
}

impl Default for ExportScope {
    fn default() -> Self {
        Self {
            kind: "retained".into(),
            source_first_seq: None,
            source_last_seq: None,
            source_record_count: 0,
            omitted_before: None,
            filter: None,
            extra: Extensions::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CaptureStatus {
    /// A source observation such as running/stopped/unknown, never artifact status.
    pub state: String,
    /// Only `unknown` or `partial`: this backend cannot prove complete capture.
    pub completeness: String,
    pub observed_unix_ms: Option<u64>,
    #[serde(default)]
    pub known_gaps: Vec<Value>,
    #[serde(flatten)]
    pub extra: Extensions,
}

impl Default for CaptureStatus {
    fn default() -> Self {
        Self {
            state: "unknown".into(),
            completeness: "unknown".into(),
            observed_unix_ms: None,
            known_gaps: Vec::new(),
            extra: Extensions::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArtifactEnd {
    pub schema_version: u8,
    #[serde(rename = "type")]
    pub record_type: String,
    pub artifact_id: SessionId,
    pub finalized: bool,
    pub record_count: u64,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub scope: ExportScope,
    pub capture: CaptureStatus,
    #[serde(flatten)]
    pub extra: Extensions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentOrigin {
    NativeV2,
    NativeV3,
    LegacyV1,
}

fn native_origin(version: u8) -> DocumentOrigin {
    if version == 3 {
        DocumentOrigin::NativeV3
    } else {
        DocumentOrigin::NativeV2
    }
}

/// Sidecar aligned with `entries`; it does not duplicate log text.
#[derive(Debug, Default)]
#[cfg_attr(not(test), derive(Clone))]
pub struct RecordMetadata {
    pub seq: u64,
    pub legacy_original_seq: Option<u64>,
    pub event_kind: Option<String>,
    pub extra: Extensions,
    pub device_time_extra: Extensions,
    pub process_extra: Extensions,
    /// Retains an attribution vocabulary newer than the current UI understands.
    pub process_source: Option<String>,
    pub provenance_extra: Extensions,
    pub text_loss_extra: Extensions,
    pub proto_extra: Extensions,
    /// Unknown members of v3's projection_extensions container.
    pub projection_extra: Extensions,
}

// Exercise the actual sidecar deep-copy boundary in cancellation regressions,
// including copies made indirectly by Document::clone.
#[cfg(test)]
impl Clone for RecordMetadata {
    fn clone(&self) -> Self {
        let cloned = Self {
            seq: self.seq,
            legacy_original_seq: self.legacy_original_seq,
            event_kind: self.event_kind.clone(),
            extra: self.extra.clone(),
            device_time_extra: self.device_time_extra.clone(),
            process_extra: self.process_extra.clone(),
            process_source: self.process_source.clone(),
            provenance_extra: self.provenance_extra.clone(),
            text_loss_extra: self.text_loss_extra.clone(),
            proto_extra: self.proto_extra.clone(),
            projection_extra: self.projection_extra.clone(),
        };
        RECORD_METADATA_CLONE_HOOK.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook();
            }
        });
        cloned
    }
}

#[derive(Clone)]
pub struct Document {
    pub header: DocumentHeader,
    pub entries: Arc<LogStore>,
    pub record_metadata: Vec<RecordMetadata>,
    pub end: ArtifactEnd,
    pub origin: DocumentOrigin,
}

impl Document {
    /// Takes a fixed COW snapshot; zero rows is a valid artifact.
    pub fn from_snapshot(
        header: DocumentHeader,
        entries: Arc<LogStore>,
        scope: ExportScope,
        capture: CaptureStatus,
    ) -> Result<Self, DocumentError> {
        Self::from_snapshot_cancellable(header, entries, scope, capture, &AtomicBool::new(false))
    }

    /// Build a fixed snapshot, checking cancellation between metadata and
    /// validation records. Decoding an individual proto payload is indivisible.
    pub fn from_snapshot_cancellable(
        header: DocumentHeader,
        entries: Arc<LogStore>,
        mut scope: ExportScope,
        capture: CaptureStatus,
        cancel: &AtomicBool,
    ) -> Result<Self, DocumentError> {
        cancelled(cancel)?;
        let metadata: Vec<_> = entries
            .iter()
            .map(|entry| {
                cancelled(cancel)?;
                let identity = entry
                    .provenance
                    .ok_or_else(|| DocumentError::invalid("live snapshot record lacks identity"))?;
                Ok(RecordMetadata {
                    seq: identity.seq,
                    event_kind: (entry.kind == LogEntryKind::Marker)
                        .then(|| "logcat_marker".into()),
                    ..RecordMetadata::default()
                })
            })
            .collect::<Result<_, DocumentError>>()?;
        scope.source_first_seq = metadata.first().map(|row| row.seq);
        scope.source_last_seq = metadata.last().map(|row| row.seq);
        scope.source_record_count = metadata.len() as u64;
        let end = make_end(&header, &metadata, scope, capture);
        let origin = native_origin(header.schema_version);
        let doc = Self {
            header,
            entries,
            record_metadata: metadata,
            end,
            origin,
        };
        doc.validate_cancellable(cancel)?;
        Ok(doc)
    }

    /// Selects increasing snapshot indices; references retain their original sequence.
    pub fn select(&self, indices: &[usize], filter: Value) -> Result<Self, DocumentError> {
        self.select_cancellable(indices, filter, &AtomicBool::new(false))
    }

    /// Cancellable form used by background GUI exports. Selection, metadata
    /// cloning and final validation all yield at record boundaries.
    pub fn select_cancellable(
        &self,
        indices: &[usize],
        filter: Value,
        cancel: &AtomicBool,
    ) -> Result<Self, DocumentError> {
        cancelled(cancel)?;
        if filter.is_null() {
            return Err(DocumentError::invalid(
                "filtered export requires a non-null filter description",
            ));
        }
        let mut store = LogStore::default();
        let mut metadata = Vec::with_capacity(indices.len());
        let mut previous = None;
        for &index in indices {
            cancelled(cancel)?;
            if index >= self.entries.len() || previous.is_some_and(|p| p >= index) {
                return Err(DocumentError::invalid(
                    "selected indices must be in bounds and strictly increasing",
                ));
            }
            store.push(self.entries[index].clone());
            metadata.push(self.record_metadata[index].clone());
            previous = Some(index);
        }
        let mut header = self.header.clone();
        header.derived_from_artifact = Some(header.artifact_id);
        header.artifact_id = SessionId::new();
        let mut scope = self.end.scope.clone();
        scope.kind = "filtered".into();
        scope.filter = Some(match scope.filter.take() {
            Some(parent) => serde_json::json!({ "all_of": [parent, filter] }),
            None => filter,
        });
        let mut end = make_end(&header, &metadata, scope, self.end.capture.clone());
        end.extra = self.end.extra.clone();
        let doc = Self {
            header,
            entries: Arc::new(store),
            record_metadata: metadata,
            end,
            origin: self.origin,
        };
        doc.validate_cancellable(cancel)?;
        Ok(doc)
    }

    /// Conservative owned-data accounting, not an exact allocator/RSS measurement.
    pub fn memory_bytes(&self) -> usize {
        let header = serde_json::to_value(&self.header).map_or(usize::MAX, |v| value_bytes(&v));
        let end = serde_json::to_value(&self.end).map_or(usize::MAX, |v| value_bytes(&v));
        self.entries
            .memory_bytes()
            .saturating_add(header)
            .saturating_add(end)
            .saturating_add(metadata_bytes(&self.record_metadata))
    }

    /// Create a distinct artifact in the requested format. Preflight the actual
    /// encoder, including header/footer and expanded v2 rows, against both the
    /// per-line cap and total output budget. No data is truncated or published.
    pub fn convert_format(
        &self,
        format: NativeFormat,
        memory_budget: usize,
        output_budget: u64,
        cancel: &AtomicBool,
    ) -> Result<Self, DocumentError> {
        self.validate_cancellable(cancel)?;
        let check_budget = |used| {
            if used > memory_budget {
                Err(DocumentError::budget(memory_budget, used))
            } else {
                Ok(())
            }
        };
        // Check fixed candidate costs before cloning even the header/footer or
        // allocating the sidecar vector. Each of these objects is one JSON line.
        let metadata_capacity = self
            .record_metadata
            .len()
            .saturating_mul(std::mem::size_of::<RecordMetadata>());
        cancelled(cancel)?;
        let header_bytes = typed_bytes(&self.header);
        cancelled(cancel)?;
        let end_bytes = typed_bytes(&self.end);
        cancelled(cancel)?;
        let mut source_used = self
            .entries
            .memory_bytes()
            .saturating_add(header_bytes)
            .saturating_add(end_bytes)
            .saturating_add(
                self.record_metadata
                    .capacity()
                    .saturating_mul(std::mem::size_of::<RecordMetadata>()),
            );
        check_budget(source_used)?;
        // Retain the source-budget preflight without memory_bytes()'s
        // uninterrupted walk over every row's unknown JSON tree.
        for meta in &self.record_metadata {
            cancelled(cancel)?;
            source_used = source_used.saturating_add(row_metadata_bytes(meta));
            check_budget(source_used)?;
        }
        cancelled(cancel)?;
        let mut header = self.header.clone();
        header.set_format(format)?;
        header.derived_from_artifact = Some(self.header.artifact_id);
        header.artifact_id = SessionId::new();
        cancelled(cancel)?;
        let mut end = self.end.clone();
        end.artifact_id = header.artifact_id;
        end.schema_version = format.version();
        cancelled(cancel)?;
        let header_bytes = typed_bytes(&header);
        cancelled(cancel)?;
        let mut used = self
            .entries
            .memory_bytes()
            .saturating_add(header_bytes)
            .saturating_add(typed_bytes(&end));
        cancelled(cancel)?;
        check_budget(used.saturating_add(metadata_capacity))?;
        let mut record_metadata = Vec::with_capacity(self.record_metadata.len());
        used = used.saturating_add(
            record_metadata
                .capacity()
                .saturating_mul(std::mem::size_of::<RecordMetadata>()),
        );
        check_budget(used)?;
        for original in &self.record_metadata {
            cancelled(cancel)?;
            // Admit the next row before its deep copy; recheck the actual owned
            // cost after conversion, which may add an extension container.
            check_budget(used.saturating_add(row_metadata_bytes(original)))?;
            cancelled(cancel)?;
            let mut meta = original.clone();
            cancelled(cancel)?;
            if self.header.schema_version == 3
                && format == NativeFormat::V2
                && !meta.projection_extra.is_empty()
            {
                no_reserved(&meta.extra, &["projection_extensions"])?;
                meta.extra.insert(
                    "projection_extensions".into(),
                    Value::Object(
                        std::mem::take(&mut meta.projection_extra)
                            .into_iter()
                            .collect(),
                    ),
                );
            }
            used = used.saturating_add(row_metadata_bytes(&meta));
            check_budget(used)?;
            record_metadata.push(meta);
        }
        let converted = Self {
            header,
            entries: Arc::clone(&self.entries),
            record_metadata,
            end,
            origin: native_origin(format.version()),
        };
        converted.validate_cancellable(cancel)?;
        let mut total = 0u64;
        let mut admit = |line: Vec<u8>| -> Result<(), DocumentError> {
            total = total
                .checked_add(line.len() as u64)
                .ok_or_else(|| DocumentError::invalid("output byte count overflow"))?;
            if total > output_budget {
                return Err(DocumentError::invalid(
                    "conversion exceeds output byte budget",
                ));
            }
            Ok(())
        };
        admit(encode_line(&converted.header)?)?;
        for (entry, meta) in converted.entries.iter().zip(&converted.record_metadata) {
            cancelled(cancel)?;
            admit(encode_record(&converted.header, entry, meta)?)?;
        }
        admit(encode_line(&converted.end)?)?;
        cancelled(cancel)?;
        Ok(converted)
    }

    fn validate_cancellable(&self, cancel: &AtomicBool) -> Result<(), DocumentError> {
        cancelled(cancel)?;
        validate_header(&self.header)?;
        if self.entries.len() != self.record_metadata.len() {
            return Err(DocumentError::invalid(
                "record metadata is not aligned with entries",
            ));
        }
        let mut previous = 0;
        for (entry, meta) in self.entries.iter().zip(&self.record_metadata) {
            cancelled(cancel)?;
            validate_row(&self.header, entry, meta, previous)?;
            previous = meta.seq;
        }
        cancelled(cancel)?;
        validate_end(&self.header, &self.end, &self.record_metadata)
    }
}

#[derive(Debug)]
pub struct DocumentError {
    pub kind: DocumentErrorKind,
    pub line: u64,
    pub offset: u64,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentErrorKind {
    Io,
    InvalidData,
    MemoryBudget,
    Cancelled,
}

impl DocumentError {
    fn invalid(message: impl fmt::Display) -> Self {
        Self {
            kind: DocumentErrorKind::InvalidData,
            line: 0,
            offset: 0,
            message: message.to_string(),
        }
    }
    fn at(mut self, line: u64, offset: u64) -> Self {
        self.line = line;
        self.offset = offset;
        self
    }
    fn budget(budget: usize, estimate: usize) -> Self {
        Self {
            kind: DocumentErrorKind::MemoryBudget,
            line: 0,
            offset: 0,
            message: format!(
                "import memory budget exceeded: budget {budget}, required estimate {estimate}"
            ),
        }
    }
}
impl fmt::Display for DocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, byte {}: {}",
            self.line, self.offset, self.message
        )
    }
}
impl std::error::Error for DocumentError {}

#[cfg(test)]
#[path = "document_proto_tests.rs"]
mod proto_tests;

#[cfg(test)]
#[path = "document_stream_tests.rs"]
mod stream_tests;

#[cfg(test)]
#[path = "document_extension_tests.rs"]
mod extension_tests;

// Deterministic test injection: cancel on a chosen record checkpoint, on this
// thread only. Production uses only the caller's atomic cancellation flag.
#[cfg(test)]
thread_local! {
    static CANCEL_AFTER_CHECKS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static RECORD_METADATA_CLONE_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
}

fn cancelled(cancel: &AtomicBool) -> Result<(), DocumentError> {
    #[cfg(test)]
    CANCEL_AFTER_CHECKS.with(|remaining| {
        if let Some(count) = remaining.get() {
            if count <= 1 {
                remaining.set(None);
                cancel.store(true, Ordering::Release);
            } else {
                remaining.set(Some(count - 1));
            }
        }
    });
    if cancel.load(Ordering::Relaxed) {
        Err(DocumentError {
            kind: DocumentErrorKind::Cancelled,
            line: 0,
            offset: 0,
            message: "file operation cancelled".into(),
        })
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DeviceTime {
    text: String,
    #[serde(flatten)]
    extra: Extensions,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ProcessAttribution {
    name: String,
    source: Option<String>,
    #[serde(flatten)]
    extra: Extensions,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct WireIdentity {
    session_id: SessionId,
    seq: u64,
    segment: u64,
    attempt: u64,
    observed_unix_ms: Option<u64>,
    #[serde(flatten)]
    extra: Extensions,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct WireTextLoss {
    truncated: bool,
    invalid_utf8_replaced: bool,
    #[serde(flatten)]
    extra: Extensions,
}
#[derive(Debug, Deserialize, Serialize)]
struct WireProtoEvidence {
    #[serde(flatten)]
    evidence: ProtoEvidence,
    #[serde(flatten)]
    extra: Extensions,
}
#[derive(Debug, Deserialize, Serialize)]
struct WireRecord {
    schema_version: u8,
    #[serde(rename = "type")]
    record_type: String,
    seq: u64,
    provenance: Option<WireIdentity>,
    text_loss: Option<WireTextLoss>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proto: Option<WireProtoEvidence>,
    legacy_original_seq: Option<u64>,
    device_time: Option<DeviceTime>,
    pid: Option<u32>,
    tid: Option<u32>,
    level: Option<String>,
    tag: Option<String>,
    message: String,
    process: Option<ProcessAttribution>,
    event_kind: Option<String>,
    #[serde(flatten)]
    extra: Extensions,
}

impl WireRecord {
    fn into_row(self) -> Result<(LogEntry, RecordMetadata), DocumentError> {
        if !matches!(self.schema_version, 2 | 3) {
            return Err(DocumentError::invalid("unsupported record schema"));
        }
        let mut meta = RecordMetadata {
            seq: self.seq,
            legacy_original_seq: self.legacy_original_seq,
            event_kind: self.event_kind,
            extra: self.extra,
            ..RecordMetadata::default()
        };
        let mut entry = match self.record_type.as_str() {
            "entry" => {
                let time = self
                    .device_time
                    .ok_or_else(|| DocumentError::invalid("entry lacks device_time"))?;
                meta.device_time_extra = time.extra;
                let level = self
                    .level
                    .as_deref()
                    .and_then(parse_level)
                    .ok_or_else(|| DocumentError::invalid("entry has invalid level"))?;
                LogEntry::new(
                    time.text,
                    self.pid
                        .ok_or_else(|| DocumentError::invalid("entry lacks pid"))?,
                    self.tid
                        .ok_or_else(|| DocumentError::invalid("entry lacks tid"))?,
                    self.tag
                        .ok_or_else(|| DocumentError::invalid("entry lacks tag"))?,
                    level,
                    self.message,
                )
            }
            "event" | "unparsed" => {
                if self.device_time.is_some()
                    || self.pid.is_some()
                    || self.tid.is_some()
                    || self.level.is_some()
                    || self.tag.is_some()
                    || self.process.is_some()
                {
                    return Err(DocumentError::invalid(
                        "event/unparsed contains fabricated log fields",
                    ));
                }
                if self.record_type == "event" {
                    LogEntry::marker(self.message)
                } else {
                    LogEntry::unparsed(self.message)
                }
            }
            _ => return Err(DocumentError::invalid("unsupported record type")),
        };
        if let Some(identity) = self.provenance {
            entry.provenance = Some(RecordIdentity {
                session_id: identity.session_id,
                seq: identity.seq,
                segment: identity.segment,
                attempt: identity.attempt,
                observed_unix_ms: identity.observed_unix_ms,
            });
            meta.provenance_extra = identity.extra;
        }
        if let Some(process) = self.process {
            entry.process = Some(process.name.into());
            entry.process_source = match process.source.as_deref() {
                Some("ps_snapshot") => Some(ProcessSource::PsSnapshot),
                Some("device_process_cache") => Some(ProcessSource::DeviceProcessCache),
                _ => None,
            };
            meta.process_source = process.source;
            meta.process_extra = process.extra;
        }
        if let Some(loss) = self.text_loss {
            entry.text_loss = Some(TextLoss {
                truncated: loss.truncated,
                invalid_utf8_replaced: loss.invalid_utf8_replaced,
            });
            meta.text_loss_extra = loss.extra;
        }
        if let Some(proto) = self.proto {
            entry.proto = Some(Arc::new(proto.evidence));
            meta.proto_extra = proto.extra;
        }
        Ok((entry, meta))
    }

    fn from_row(entry: &LogEntry, meta: &RecordMetadata) -> Self {
        let log = entry.kind == LogEntryKind::Log;
        Self {
            schema_version: 2,
            record_type: match entry.kind {
                LogEntryKind::Log => "entry",
                LogEntryKind::Marker => "event",
                LogEntryKind::Unparsed => "unparsed",
            }
            .into(),
            seq: meta.seq,
            provenance: entry.provenance.map(|p| WireIdentity {
                session_id: p.session_id,
                seq: p.seq,
                segment: p.segment,
                attempt: p.attempt,
                observed_unix_ms: p.observed_unix_ms,
                extra: meta.provenance_extra.clone(),
            }),
            text_loss: entry.text_loss.map(|loss| WireTextLoss {
                truncated: loss.truncated,
                invalid_utf8_replaced: loss.invalid_utf8_replaced,
                extra: meta.text_loss_extra.clone(),
            }),
            proto: entry.proto.as_ref().map(|evidence| WireProtoEvidence {
                evidence: (**evidence).clone(),
                extra: meta.proto_extra.clone(),
            }),
            legacy_original_seq: meta.legacy_original_seq,
            device_time: log.then(|| DeviceTime {
                text: entry.time.to_string(),
                extra: meta.device_time_extra.clone(),
            }),
            pid: log.then_some(entry.pid),
            tid: log.then_some(entry.tid),
            level: log.then(|| entry.level.short().into()),
            tag: log.then(|| entry.tag.to_string()),
            message: entry.message.to_string(),
            process: entry.process.as_ref().map(|name| ProcessAttribution {
                name: name.to_string(),
                source: entry
                    .process_source
                    .map(|source| match source {
                        ProcessSource::PsSnapshot => "ps_snapshot".into(),
                        ProcessSource::DeviceProcessCache => "device_process_cache".into(),
                    })
                    .or_else(|| meta.process_source.clone()),
                extra: meta.process_extra.clone(),
            }),
            event_kind: meta.event_kind.clone(),
            extra: meta.extra.clone(),
        }
    }
}

#[derive(Default, Deserialize)]
struct ProjectionExtensions {
    device_time: Option<Extensions>,
    process: Option<Extensions>,
    #[serde(flatten)]
    extra: Extensions,
}

#[derive(Deserialize)]
struct RawProto {
    #[serde(with = "crate::model::binary_base64")]
    raw_payload: Vec<u8>,
    #[serde(flatten)]
    extra: Extensions,
}

#[derive(Deserialize)]
struct CompactRecord {
    schema_version: u8,
    #[serde(rename = "type")]
    record_type: String,
    seq: u64,
    provenance: Option<WireIdentity>,
    text_loss: Option<WireTextLoss>,
    proto: RawProto,
    legacy_original_seq: Option<u64>,
    event_kind: Option<String>,
    #[serde(default)]
    projection_extensions: ProjectionExtensions,
    #[serde(flatten)]
    extra: Extensions,
}

impl CompactRecord {
    fn into_row(self) -> Result<(LogEntry, RecordMetadata), DocumentError> {
        if self.schema_version != 3 {
            return Err(DocumentError::invalid("unsupported compact record schema"));
        }
        // Even null redundant fields are a conflict. Do not silently discard
        // them before validating this raw-only representation.
        no_reserved(
            &self.extra,
            &[
                "device_time",
                "pid",
                "tid",
                "level",
                "tag",
                "message",
                "process",
            ],
        )?;
        no_reserved(
            &self.proto.extra,
            &[
                "time_sec",
                "time_nsec",
                "priority",
                "uid",
                "pid",
                "tid",
                "tag_bytes",
                "message_bytes",
                "process_name",
            ],
        )?;
        let mut row = crate::protobuf::DecodedEntry::decode_payload(self.proto.raw_payload)
            .map_err(DocumentError::invalid)?
            .project();
        let expected_type = if row.kind == LogEntryKind::Log {
            "entry"
        } else {
            "unparsed"
        };
        if self.record_type != expected_type {
            return Err(DocumentError::invalid(
                "compact type disagrees with proto projection",
            ));
        }
        if (self.projection_extensions.device_time.is_some() && row.kind != LogEntryKind::Log)
            || (self.projection_extensions.process.is_some() && row.process.is_none())
        {
            return Err(DocumentError::invalid(
                "projection extensions lack their derived parent",
            ));
        }
        let mut meta = RecordMetadata {
            seq: self.seq,
            legacy_original_seq: self.legacy_original_seq,
            event_kind: self.event_kind,
            extra: self.extra,
            device_time_extra: self.projection_extensions.device_time.unwrap_or_default(),
            process_extra: self.projection_extensions.process.unwrap_or_default(),
            projection_extra: self.projection_extensions.extra,
            proto_extra: self.proto.extra,
            ..RecordMetadata::default()
        };
        if let Some(p) = self.provenance {
            row.provenance = Some(RecordIdentity {
                session_id: p.session_id,
                seq: p.seq,
                segment: p.segment,
                attempt: p.attempt,
                observed_unix_ms: p.observed_unix_ms,
            });
            meta.provenance_extra = p.extra;
        }
        let observed = self.text_loss.map(|loss| {
            meta.text_loss_extra = loss.extra;
            TextLoss {
                truncated: loss.truncated,
                invalid_utf8_replaced: loss.invalid_utf8_replaced,
            }
        });
        if observed.is_some() && observed != row.text_loss {
            return Err(DocumentError::invalid(
                "proto host text loss disagrees with decoded payload",
            ));
        }
        row.text_loss = observed; // Unknown must stay unknown.
        meta.process_source = row.process_source.map(|_| "device_process_cache".into());
        Ok((row, meta))
    }
}

/// Borrow the only stored payload; compact writes never clone the evidence or
/// allocate the redundant v2 display/base64 fields.
#[derive(Serialize)]
struct RawProtoRef<'a> {
    #[serde(serialize_with = "crate::model::binary_base64::serialize")]
    raw_payload: &'a [u8],
    #[serde(flatten)]
    extra: &'a Extensions,
}

#[derive(Serialize)]
struct ProjectionExtensionsRef<'a> {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    device_time: &'a Extensions,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    process: &'a Extensions,
    #[serde(flatten)]
    extra: &'a Extensions,
}

#[derive(Serialize)]
struct CompactRecordRef<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    record_type: &'static str,
    seq: u64,
    provenance: Option<WireIdentity>,
    text_loss: Option<WireTextLoss>,
    proto: RawProtoRef<'a>,
    legacy_original_seq: Option<u64>,
    event_kind: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    projection_extensions: Option<ProjectionExtensionsRef<'a>>,
    #[serde(flatten)]
    extra: &'a Extensions,
}

fn encode_record(
    header: &DocumentHeader,
    entry: &LogEntry,
    meta: &RecordMetadata,
) -> Result<Vec<u8>, DocumentError> {
    if header.schema_version == 3
        && let Some(proto) = &entry.proto
    {
        encode_line(&CompactRecordRef {
            schema_version: 3,
            record_type: if entry.kind == LogEntryKind::Log {
                "entry"
            } else {
                "unparsed"
            },
            seq: meta.seq,
            provenance: entry.provenance.map(|p| WireIdentity {
                session_id: p.session_id,
                seq: p.seq,
                segment: p.segment,
                attempt: p.attempt,
                observed_unix_ms: p.observed_unix_ms,
                extra: meta.provenance_extra.clone(),
            }),
            text_loss: entry.text_loss.map(|loss| WireTextLoss {
                truncated: loss.truncated,
                invalid_utf8_replaced: loss.invalid_utf8_replaced,
                extra: meta.text_loss_extra.clone(),
            }),
            proto: RawProtoRef {
                raw_payload: &proto.raw_payload,
                extra: &meta.proto_extra,
            },
            legacy_original_seq: meta.legacy_original_seq,
            event_kind: &meta.event_kind,
            projection_extensions: (!meta.device_time_extra.is_empty()
                || !meta.process_extra.is_empty()
                || !meta.projection_extra.is_empty())
            .then_some(ProjectionExtensionsRef {
                device_time: &meta.device_time_extra,
                process: &meta.process_extra,
                extra: &meta.projection_extra,
            }),
            extra: &meta.extra,
        })
    } else {
        let mut wire = WireRecord::from_row(entry, meta);
        wire.schema_version = header.schema_version;
        encode_line(&wire)
    }
}

fn parse_level(value: &str) -> Option<Level> {
    if matches!(value, "V" | "D" | "I" | "W" | "E" | "F") {
        value.chars().next().and_then(Level::from_letter)
    } else {
        None
    }
}

fn validate_header(header: &DocumentHeader) -> Result<(), DocumentError> {
    no_reserved(
        &header.extra,
        &[
            "schema_version",
            "type",
            "artifact_id",
            "session_id",
            "source",
            "derived_from_artifact",
            "normalization",
        ],
    )?;
    no_reserved(
        &header.source.extra,
        &[
            "kind",
            "device_serial",
            "requested_buffers",
            "requested_buffers_scope",
            "transport_format",
        ],
    )?;
    if !matches!(header.schema_version, 2 | 3)
        || header.record_type != "document"
        || header.artifact_id.is_nil()
        || header.session_id.is_some_and(SessionId::is_nil)
        || header.derived_from_artifact.is_some_and(SessionId::is_nil)
    {
        return Err(DocumentError::invalid(
            "invalid document schema or identity",
        ));
    }
    if header.schema_version == 3
        && (header.extra.get("record_encoding").and_then(Value::as_str)
            != Some(COMPACT_RECORD_ENCODING)
            || header
                .normalization
                .get("proto_projection")
                .and_then(Value::as_str)
                != Some(crate::protobuf::PROJECTION_PROFILE)
            || header
                .normalization
                .get("protobuf_raw_payload")
                .and_then(Value::as_str)
                != Some("preserved_base64"))
    {
        return Err(DocumentError::invalid(
            "unsupported compact encoding or proto projection profile",
        ));
    }
    match header.source.kind.as_str() {
        "adb"
            if header.session_id.is_some()
                && header
                    .source
                    .device_serial
                    .as_ref()
                    .is_some_and(|s| !s.is_empty())
                && header
                    .source
                    .requested_buffers
                    .as_ref()
                    .is_some_and(|b| !b.is_empty())
                && matches!(
                    header.source.requested_buffers_scope.as_deref(),
                    Some("snapshot_configuration" | "capture_configuration")
                )
                && matches!(
                    header.source.transport_format.as_deref(),
                    Some("threadtime" | "logcat_proto_experimental")
                ) =>
        {
            Ok(())
        }
        "legacy_v1"
            if header.session_id.is_none()
                && header.source.device_serial.is_none()
                && header.source.requested_buffers.is_none()
                && header.source.requested_buffers_scope.is_none()
                && header.source.transport_format.is_none() =>
        {
            Ok(())
        }
        _ => Err(DocumentError::invalid(
            "invalid or unsupported source metadata",
        )),
    }
}

fn validate_row(
    header: &DocumentHeader,
    entry: &LogEntry,
    meta: &RecordMetadata,
    previous: u64,
) -> Result<(), DocumentError> {
    validate_row_fields(header, entry, meta, previous)?;
    if let Some(evidence) = &entry.proto {
        validate_proto(entry, evidence)?;
    }
    Ok(())
}

fn validate_row_fields(
    header: &DocumentHeader,
    entry: &LogEntry,
    meta: &RecordMetadata,
    previous: u64,
) -> Result<(), DocumentError> {
    no_reserved(
        &meta.extra,
        &[
            "schema_version",
            "type",
            "seq",
            "provenance",
            "text_loss",
            "proto",
            "legacy_original_seq",
            "device_time",
            "pid",
            "tid",
            "level",
            "tag",
            "message",
            "process",
            "event_kind",
        ],
    )?;
    if header.schema_version == 3 {
        no_reserved(&meta.extra, &["projection_extensions"])?;
    }
    no_reserved(&meta.projection_extra, &["device_time", "process"])?;
    if !meta.projection_extra.is_empty() && (header.schema_version != 3 || entry.proto.is_none()) {
        return Err(DocumentError::invalid(
            "projection extensions require a compact proto parent",
        ));
    }
    no_reserved(&meta.device_time_extra, &["text"])?;
    no_reserved(&meta.process_extra, &["name", "source"])?;
    no_reserved(
        &meta.proto_extra,
        &[
            "time_sec",
            "time_nsec",
            "uid",
            "pid",
            "tid",
            "priority",
            "raw_payload",
            "tag_bytes",
            "message_bytes",
            "process_name",
        ],
    )?;
    no_reserved(
        &meta.text_loss_extra,
        &["truncated", "invalid_utf8_replaced"],
    )?;
    no_reserved(
        &meta.provenance_extra,
        &[
            "session_id",
            "seq",
            "segment",
            "attempt",
            "observed_unix_ms",
        ],
    )?;
    if meta.seq == 0 || meta.seq <= previous {
        return Err(DocumentError::invalid(
            "record sequence must be positive and strictly increasing",
        ));
    }
    match (header.session_id, entry.provenance) {
        (Some(session), Some(p))
            if p.session_id == session
                && p.seq == meta.seq
                && p.segment > 0
                && p.attempt >= p.segment
                && meta.legacy_original_seq.is_none() => {}
        (None, None)
            if header.source.kind == "legacy_v1" && meta.legacy_original_seq == Some(meta.seq) => {}
        _ => {
            return Err(DocumentError::invalid(
                "record identity does not match document source/session",
            ));
        }
    }
    if (entry.kind == LogEntryKind::Marker) != meta.event_kind.is_some()
        || meta.event_kind.as_ref().is_some_and(|s| s.is_empty())
    {
        return Err(DocumentError::invalid(
            "event_kind must describe only event records",
        ));
    }
    if entry.process.is_none() && (entry.process_source.is_some() || meta.process_source.is_some())
    {
        return Err(DocumentError::invalid(
            "process attribution source has no process name",
        ));
    }
    if entry.kind != LogEntryKind::Log
        && (entry.process.is_some()
            || entry.process_source.is_some()
            || meta.process_source.is_some())
    {
        return Err(DocumentError::invalid(
            "event/unparsed cannot carry process attribution",
        ));
    }
    if (entry.kind != LogEntryKind::Log && !meta.device_time_extra.is_empty())
        || (entry.process.is_none() && !meta.process_extra.is_empty())
        || (entry.provenance.is_none() && !meta.provenance_extra.is_empty())
        || (entry.text_loss.is_none() && !meta.text_loss_extra.is_empty())
        || (entry.proto.is_none() && !meta.proto_extra.is_empty())
    {
        return Err(DocumentError::invalid(
            "optional extensions lack their parent field",
        ));
    }
    if entry.proto.is_some() {
        if meta
            .process_source
            .as_deref()
            .is_some_and(|source| source != "device_process_cache")
        {
            return Err(DocumentError::invalid(
                "proto process source metadata disagrees with device cache attribution",
            ));
        }
    } else if entry.process_source == Some(ProcessSource::DeviceProcessCache)
        || meta.process_source.as_deref() == Some("device_process_cache")
    {
        return Err(DocumentError::invalid(
            "device process cache attribution lacks proto evidence",
        ));
    }
    Ok(())
}

/// An artifact carries the original wire payload as well as useful projections.
/// Re-decoding prevents those two representations from silently disagreeing.
fn validate_proto(entry: &LogEntry, evidence: &ProtoEvidence) -> Result<(), DocumentError> {
    use crate::protobuf::{DecodedEntry, MAX_FRAME_BYTES};
    let limit = MAX_FRAME_BYTES as usize;
    if evidence.raw_payload.len() > limit
        || evidence.tag_bytes.len() > limit
        || evidence.message_bytes.len() > limit
        || evidence
            .process_name
            .as_ref()
            .is_some_and(|name| name.len() > limit)
    {
        return Err(DocumentError::invalid(
            "proto evidence exceeds 1 MiB field/frame limit",
        ));
    }
    let decoded = DecodedEntry::decode_payload(evidence.raw_payload.clone())
        .map_err(|error| DocumentError::invalid(format!("invalid proto payload: {error}")))?;
    if decoded.time_sec != evidence.time_sec
        || decoded.time_nsec != evidence.time_nsec
        || decoded.uid != evidence.uid
        || decoded.pid != evidence.pid
        || decoded.tid != evidence.tid
        || decoded.priority != evidence.priority
        || decoded.tag != evidence.tag_bytes
        || decoded.message != evidence.message_bytes
        || decoded.process_name != evidence.process_name
    {
        return Err(DocumentError::invalid(
            "proto explicit fields disagree with raw payload",
        ));
    }
    let projected = decoded.project();
    if projected.kind == LogEntryKind::Log {
        if entry.kind != LogEntryKind::Log
            || entry.level != projected.level
            || entry.pid != projected.pid
            || entry.tid != projected.tid
            || entry.tag != projected.tag
            || entry.time != projected.time
            || entry.process != projected.process
            || entry.process_source != projected.process_source
        {
            return Err(DocumentError::invalid(
                "proto display fields disagree with decoded payload",
            ));
        }
    } else if entry.kind != LogEntryKind::Unparsed {
        return Err(DocumentError::invalid(
            "uninterpretable proto payload must remain unparsed",
        ));
    }
    if entry.message != projected.message {
        return Err(DocumentError::invalid(
            "proto message disagrees with decoded payload",
        ));
    }
    if entry.text_loss.is_some() && entry.text_loss != projected.text_loss {
        return Err(DocumentError::invalid(
            "proto host text loss disagrees with decoded payload",
        ));
    }
    Ok(())
}

fn make_end(
    header: &DocumentHeader,
    metadata: &[RecordMetadata],
    scope: ExportScope,
    capture: CaptureStatus,
) -> ArtifactEnd {
    ArtifactEnd {
        schema_version: header.schema_version,
        record_type: "artifact_end".into(),
        artifact_id: header.artifact_id,
        finalized: true,
        record_count: metadata.len() as u64,
        first_seq: metadata.first().map(|m| m.seq),
        last_seq: metadata.last().map(|m| m.seq),
        scope,
        capture,
        extra: Extensions::new(),
    }
}

fn validate_end(
    header: &DocumentHeader,
    end: &ArtifactEnd,
    metadata: &[RecordMetadata],
) -> Result<(), DocumentError> {
    validate_end_summary(
        header,
        end,
        metadata.len() as u64,
        metadata.first().map(|m| m.seq),
        metadata.last().map(|m| m.seq),
    )
}

/// Validate a footer against counts maintained by a streaming reader/writer.
pub fn validate_end_summary(
    header: &DocumentHeader,
    end: &ArtifactEnd,
    count: u64,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
) -> Result<(), DocumentError> {
    validate_header(header)?;
    no_reserved(
        &end.extra,
        &[
            "schema_version",
            "type",
            "artifact_id",
            "finalized",
            "record_count",
            "first_seq",
            "last_seq",
            "scope",
            "capture",
        ],
    )?;
    no_reserved(
        &end.scope.extra,
        &[
            "kind",
            "source_first_seq",
            "source_last_seq",
            "source_record_count",
            "omitted_before",
            "filter",
        ],
    )?;
    no_reserved(
        &end.capture.extra,
        &["state", "completeness", "observed_unix_ms", "known_gaps"],
    )?;
    if end.schema_version != header.schema_version
        || end.record_type != "artifact_end"
        || !end.finalized
        || end.artifact_id != header.artifact_id
        || end.record_count != count
        || end.first_seq != first_seq
        || end.last_seq != last_seq
        || (count == 0) != (first_seq.is_none() && last_seq.is_none())
        || (count > 0 && (first_seq.is_none() || last_seq.is_none()))
        || first_seq.is_some_and(|first| {
            first == 0 || last_seq.is_none_or(|last| last < first || count > last - first + 1)
        })
    {
        return Err(DocumentError::invalid(
            "artifact_end identity/count/range does not match document",
        ));
    }
    if !matches!(end.capture.completeness.as_str(), "unknown" | "partial")
        || end.capture.state.is_empty()
    {
        return Err(DocumentError::invalid(
            "capture completeness must remain unknown or partial",
        ));
    }
    let scope = &end.scope;
    if !matches!(
        scope.kind.as_str(),
        "retained" | "filtered" | "legacy_import"
    ) || (scope.kind == "filtered") != scope.filter.is_some()
        || scope.filter.as_ref().is_some_and(Value::is_null)
        || scope.source_record_count < end.record_count
        || (scope.source_record_count == 0)
            != (scope.source_first_seq.is_none() && scope.source_last_seq.is_none())
    {
        return Err(DocumentError::invalid("invalid export scope"));
    }
    if scope.source_record_count > 0 {
        let (Some(first), Some(last)) = (scope.source_first_seq, scope.source_last_seq) else {
            return Err(DocumentError::invalid("source scope lacks sequence bounds"));
        };
        if first == 0
            || first > last
            || scope.source_record_count > last.saturating_sub(first).saturating_add(1)
            || end.first_seq.is_some_and(|s| s < first)
            || end.last_seq.is_some_and(|s| s > last)
        {
            return Err(DocumentError::invalid(
                "record range is outside export source scope",
            ));
        }
    }
    if scope.kind != "filtered"
        && (scope.source_record_count != end.record_count
            || scope.source_first_seq != end.first_seq
            || scope.source_last_seq != end.last_seq)
    {
        return Err(DocumentError::invalid(
            "unfiltered scope must cover all retained source records",
        ));
    }
    Ok(())
}

fn decode_line<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, DocumentError> {
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(DocumentError::invalid("line exceeds 8 MiB encoded limit"));
    }
    reject_ambiguous_number_key(bytes)?;
    serde_json::from_slice(bytes).map_err(DocumentError::invalid)
}

/// serde_json's arbitrary_precision visitor represents numbers using a private
/// map key. A literal JSON object with that key can otherwise become a number
/// after flatten buffering. Reject this unsupported spelling before it loses
/// its type. Scan string boundaries only; serde_json still validates the JSON.
pub(crate) fn reject_ambiguous_number_key(bytes: &[u8]) -> Result<(), DocumentError> {
    crate::json_input::reject_ambiguous_number_key(bytes).map_err(DocumentError::invalid)
}

pub fn decode_header(bytes: &[u8]) -> Result<DocumentHeader, DocumentError> {
    let header: DocumentHeader = decode_line(bytes)?;
    if header.schema_version == 3 {
        // These were open extension keys in v2. Parse their v3 contract directly
        // from bytes, so duplicate declarations cannot be hidden by map parsing.
        #[derive(Deserialize)]
        struct Normalization {
            proto_projection: String,
            protobuf_raw_payload: String,
        }
        #[derive(Deserialize)]
        struct Declarations {
            record_encoding: String,
            normalization: Normalization,
        }
        let declarations: Declarations = decode_line(bytes)?;
        if declarations.record_encoding != COMPACT_RECORD_ENCODING
            || declarations.normalization.proto_projection != crate::protobuf::PROJECTION_PROFILE
            || declarations.normalization.protobuf_raw_payload != "preserved_base64"
        {
            return Err(DocumentError::invalid(
                "unsupported compact encoding or proto projection profile",
            ));
        }
    }
    validate_header(&header)?;
    Ok(header)
}

pub fn decode_record(
    bytes: &[u8],
    header: &DocumentHeader,
    previous_seq: u64,
) -> Result<(LogEntry, RecordMetadata), DocumentError> {
    validate_header(header)?;
    #[derive(Deserialize)]
    struct Envelope {
        schema_version: u8,
        proto: Option<serde::de::IgnoredAny>,
    }
    let envelope: Envelope = decode_line(bytes)?;
    if envelope.schema_version != header.schema_version {
        return Err(DocumentError::invalid(
            "record schema does not match document",
        ));
    }
    let (entry, metadata) = if header.schema_version == 3 && envelope.proto.is_some() {
        let wire: CompactRecord = decode_line(bytes)?;
        let (entry, metadata) = wire.into_row()?;
        // The payload was just decoded/projected once by this private path.
        // Public model writes always revalidate the mutable evidence separately.
        validate_row_fields(header, &entry, &metadata, previous_seq)?;
        (entry, metadata)
    } else {
        let wire: WireRecord = decode_line(bytes)?;
        let (entry, metadata) = wire.into_row()?;
        validate_row(header, &entry, &metadata, previous_seq)?;
        (entry, metadata)
    };
    Ok((entry, metadata))
}

/// Encode a full structured, derived view of one native row, without a final LF.
/// This is not an archive record: v3 proto fields are expanded using the fixed
/// projection, and its schema_version identifies the source schema. Unknown
/// fields survive, with device_time/process extensions restored to their parents.
/// The independent budget permits compact records to expand beyond 8 MiB.
pub fn encode_record_projection(
    header: &DocumentHeader,
    entry: &LogEntry,
    metadata: &RecordMetadata,
    output_budget: usize,
) -> Result<Vec<u8>, DocumentError> {
    validate_header(header)?;
    validate_row(header, entry, metadata, 0)?;
    let mut wire = WireRecord::from_row(entry, metadata);
    wire.schema_version = header.schema_version;
    if !metadata.projection_extra.is_empty() {
        no_reserved(&wire.extra, &["projection_extensions"])?;
        wire.extra.insert(
            "projection_extensions".into(),
            serde_json::to_value(&metadata.projection_extra).map_err(DocumentError::invalid)?,
        );
    }
    struct Limited {
        bytes: Vec<u8>,
        budget: usize,
    }
    impl Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.bytes.len().saturating_add(bytes.len()) > self.budget {
                return Err(std::io::Error::other(
                    "decoded view exceeds output byte budget",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Limited {
        bytes: Vec::new(),
        budget: output_budget,
    };
    serde_json::to_writer(&mut output, &wire).map_err(DocumentError::invalid)?;
    reject_ambiguous_number_key(&output.bytes)?;
    Ok(output.bytes)
}

/// Checks footer shape/identity. A sequential reader must additionally call
/// `validate_end_summary` with its observed count and bounds.
pub fn decode_end(bytes: &[u8], header: &DocumentHeader) -> Result<ArtifactEnd, DocumentError> {
    let end: ArtifactEnd = decode_line(bytes)?;
    validate_end_summary(header, &end, end.record_count, end.first_seq, end.last_seq)?;
    Ok(end)
}

/// Incremental encoding without retaining capture history in memory.
#[derive(Clone)]
pub struct StreamEncoder {
    header: Arc<DocumentHeader>,
    count: u64,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
}

impl StreamEncoder {
    pub fn new(header: DocumentHeader) -> Result<Self, DocumentError> {
        validate_header(&header)?;
        Ok(Self {
            header: Arc::new(header),
            count: 0,
            first_seq: None,
            last_seq: None,
        })
    }

    pub fn header_bytes(&self) -> Result<Vec<u8>, DocumentError> {
        encode_line(self.header.as_ref())
    }

    /// Advances only after valid bounded encoding; the caller separately tracks
    /// the confirmed written prefix and must not finalize after a write error.
    pub fn record_bytes(&mut self, entry: &LogEntry) -> Result<Vec<u8>, DocumentError> {
        let identity = entry
            .provenance
            .ok_or_else(|| DocumentError::invalid("stream record lacks identity"))?;
        let metadata = RecordMetadata {
            seq: identity.seq,
            event_kind: (entry.kind == LogEntryKind::Marker).then(|| "logcat_marker".into()),
            ..RecordMetadata::default()
        };
        validate_row(&self.header, entry, &metadata, self.last_seq.unwrap_or(0))?;
        let bytes = encode_record(&self.header, entry, &metadata)?;
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| DocumentError::invalid("record count exhausted"))?;
        self.first_seq.get_or_insert(identity.seq);
        self.last_seq = Some(identity.seq);
        Ok(bytes)
    }

    pub fn finish_bytes(&self, capture: CaptureStatus) -> Result<Vec<u8>, DocumentError> {
        let end = ArtifactEnd {
            schema_version: self.header.schema_version,
            record_type: "artifact_end".into(),
            artifact_id: self.header.artifact_id,
            finalized: true,
            record_count: self.count,
            first_seq: self.first_seq,
            last_seq: self.last_seq,
            scope: ExportScope {
                source_first_seq: self.first_seq,
                source_last_seq: self.last_seq,
                source_record_count: self.count,
                omitted_before: Some(0),
                ..ExportScope::default()
            },
            capture,
            extra: Extensions::new(),
        };
        validate_end_summary(
            &self.header,
            &end,
            self.count,
            self.first_seq,
            self.last_seq,
        )?;
        encode_line(&end)
    }
}

/// Reads a completed offline artifact; errors return no partial candidate.
/// Empty input is accepted as an empty legacy file with unknown capture state.
pub fn load(
    mut reader: impl BufRead,
    memory_budget: usize,
    cancel: &AtomicBool,
) -> Result<Document, DocumentError> {
    let mut buffer = Vec::new();
    let mut line = 1u64;
    let mut offset = 0u64;
    let mut header = None;
    let mut footer = None;
    let mut footer_location = None;
    let mut origin = DocumentOrigin::LegacyV1;
    let mut store = LogStore::default();
    let mut metadata = Vec::new();
    let mut meta_cost = 0usize;
    let mut header_cost = 0usize;
    loop {
        cancelled(cancel).map_err(|e| e.at(line, offset))?;
        buffer.clear();
        reader
            .by_ref()
            .take(MAX_RECORD_BYTES + 1)
            .read_until(b'\n', &mut buffer)
            .map_err(|e| DocumentError {
                kind: DocumentErrorKind::Io,
                line,
                offset,
                message: e.to_string(),
            })?;
        if buffer.is_empty() {
            break;
        }
        if buffer.len() as u64 > MAX_RECORD_BYTES {
            return Err(DocumentError::invalid("line exceeds 8 MiB encoded limit").at(line, offset));
        }
        if footer.is_some() {
            return Err(DocumentError::invalid("data after artifact_end").at(line, offset));
        }
        // Reserve a conservative parser working set before allocating arbitrary
        // optional JSON trees. It intentionally rejects some large benign rows.
        let retained = store
            .memory_bytes()
            .saturating_add(meta_cost)
            .saturating_add(header_cost)
            .saturating_add(
                metadata
                    .capacity()
                    .saturating_mul(std::mem::size_of::<RecordMetadata>()),
            );
        let estimate = retained
            .saturating_add(buffer.len().saturating_mul(64))
            .saturating_add(1024);
        if estimate > memory_budget {
            return Err(DocumentError::budget(memory_budget, estimate).at(line, offset));
        }
        #[derive(Deserialize)]
        struct Probe {
            schema_version: u8,
            #[serde(rename = "type")]
            kind: String,
        }
        let probe: Probe = serde_json::from_slice(&buffer)
            .map_err(|e| DocumentError::invalid(e).at(line, offset))?;
        if line == 1 && matches!(probe.schema_version, 2 | 3) && probe.kind == "document" {
            let parsed = decode_header(&buffer).map_err(|e| e.at(line, offset))?;
            header_cost = typed_bytes(&parsed);
            origin = native_origin(parsed.schema_version);
            header = Some(parsed);
        } else if origin != DocumentOrigin::LegacyV1 && probe.kind == "artifact_end" {
            footer_location = Some((line, offset));
            footer = Some(
                decode_end(&buffer, header.as_ref().expect("native header"))
                    .map_err(|e| e.at(line, offset))?,
            );
        } else {
            let previous = metadata.last().map_or(0, |m: &RecordMetadata| m.seq);
            let (entry, meta) = if origin != DocumentOrigin::LegacyV1 {
                decode_record(&buffer, header.as_ref().expect("native header"), previous)
                    .map_err(|e| e.at(line, offset))?
            } else {
                if header.is_none() {
                    let h = DocumentHeader::legacy();
                    header_cost = typed_bytes(&h);
                    header = Some(h);
                }
                let (entry, meta) = legacy_row(&buffer).map_err(|e| e.at(line, offset))?;
                validate_row(
                    header.as_ref().expect("legacy header"),
                    &entry,
                    &meta,
                    previous,
                )
                .map_err(|e| e.at(line, offset))?;
                (entry, meta)
            };
            meta_cost = meta_cost.saturating_add(row_metadata_bytes(&meta));
            store.push(entry);
            metadata.push(meta);
            let used = store
                .memory_bytes()
                .saturating_add(meta_cost)
                .saturating_add(header_cost)
                .saturating_add(
                    metadata
                        .capacity()
                        .saturating_mul(std::mem::size_of::<RecordMetadata>()),
                );
            if used > memory_budget {
                return Err(DocumentError::budget(memory_budget, used).at(line, offset));
            }
        }
        offset = offset
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| DocumentError::invalid("file offset overflow"))?;
        line += 1;
    }
    cancelled(cancel)?;
    let header = header.unwrap_or_else(DocumentHeader::legacy);
    let end = if origin != DocumentOrigin::LegacyV1 {
        footer.ok_or_else(|| {
            DocumentError::invalid("native artifact lacks artifact_end").at(line, offset)
        })?
    } else {
        let scope = ExportScope {
            kind: "legacy_import".into(),
            source_first_seq: metadata.first().map(|m| m.seq),
            source_last_seq: metadata.last().map(|m| m.seq),
            source_record_count: metadata.len() as u64,
            ..ExportScope::default()
        };
        make_end(&header, &metadata, scope, CaptureStatus::default())
    };
    let doc = Document {
        header,
        entries: Arc::new(store),
        record_metadata: metadata,
        end,
        origin,
    };
    let validation_location = footer_location.unwrap_or((line, offset));
    // V3 rows were already derived and validated by decode_record; no public
    // mutation can occur during load. Do not decode their payloads a second time.
    let validation = if origin == DocumentOrigin::NativeV3 {
        validate_end(&doc.header, &doc.end, &doc.record_metadata)
    } else {
        doc.validate_cancellable(cancel)
    };
    validation.map_err(|e| e.at(validation_location.0, validation_location.1))?;
    let used = doc.memory_bytes();
    if used > memory_budget {
        return Err(DocumentError::budget(memory_budget, used).at(line, offset));
    }
    cancelled(cancel)?;
    Ok(doc)
}

fn legacy_row(bytes: &[u8]) -> Result<(LogEntry, RecordMetadata), DocumentError> {
    reject_ambiguous_number_key(bytes)?;
    let record: crate::record_io::LegacyRecord =
        serde_json::from_slice(bytes).map_err(DocumentError::invalid)?;
    let mut value: Value = serde_json::from_slice(bytes).map_err(DocumentError::invalid)?;
    if value.get("schema_version").and_then(Value::as_u64) != Some(1)
        || value.get("type").and_then(Value::as_str) != Some("entry")
        || record.seq == 0
    {
        return Err(DocumentError::invalid("invalid legacy record schema"));
    }
    let level =
        parse_level(&record.level).ok_or_else(|| DocumentError::invalid("invalid legacy level"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| DocumentError::invalid("record is not an object"))?;
    for key in [
        "schema_version",
        "type",
        "seq",
        "time",
        "pid",
        "tid",
        "level",
        "tag",
        "message",
    ] {
        object.remove(key);
    }
    let mut meta = RecordMetadata {
        seq: record.seq,
        legacy_original_seq: Some(record.seq),
        ..RecordMetadata::default()
    };
    if !object.is_empty() {
        meta.extra.insert("legacy_extensions".into(), value);
    }
    Ok((
        LogEntry::new(
            record.time,
            record.pid,
            record.tid,
            record.tag,
            level,
            record.message,
        ),
        meta,
    ))
}

#[derive(Clone, Copy, Debug)]
pub struct WriteSummary {
    pub records: u64,
    pub bytes: u64,
}

/// Writes a validated fixed snapshot and flushes it. File sync/atomic publication
/// belongs to the caller; a failed write may leave a partial temporary stream.
pub fn write_snapshot(
    mut writer: impl Write,
    doc: &Document,
    cancel: &AtomicBool,
) -> Result<WriteSummary, DocumentError> {
    cancelled(cancel)?;
    doc.validate_cancellable(cancel)?;
    let mut bytes = write_line(&mut writer, &doc.header)?;
    for (entry, meta) in doc.entries.iter().zip(&doc.record_metadata) {
        cancelled(cancel)?;
        bytes = bytes
            .checked_add(write_encoded(
                &mut writer,
                &encode_record(&doc.header, entry, meta)?,
            )?)
            .ok_or_else(|| DocumentError::invalid("output byte count overflow"))?;
    }
    cancelled(cancel)?;
    bytes = bytes
        .checked_add(write_line(&mut writer, &doc.end)?)
        .ok_or_else(|| DocumentError::invalid("output byte count overflow"))?;
    writer.flush().map_err(|e| DocumentError {
        kind: DocumentErrorKind::Io,
        line: 0,
        offset: bytes,
        message: e.to_string(),
    })?;
    cancelled(cancel)?;
    Ok(WriteSummary {
        records: doc.entries.len() as u64,
        bytes,
    })
}

fn write_line(writer: &mut impl Write, value: &impl Serialize) -> Result<u64, DocumentError> {
    let encoded = encode_line(value)?;
    write_encoded(writer, &encoded)
}

fn write_encoded(writer: &mut impl Write, encoded: &[u8]) -> Result<u64, DocumentError> {
    writer.write_all(encoded).map_err(|e| DocumentError {
        kind: DocumentErrorKind::Io,
        line: 0,
        offset: 0,
        message: e.to_string(),
    })?;
    Ok(encoded.len() as u64)
}

fn encode_line(value: &impl Serialize) -> Result<Vec<u8>, DocumentError> {
    struct Limited(Vec<u8>);
    impl Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len().saturating_add(bytes.len()) >= MAX_RECORD_BYTES as usize {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line exceeds 8 MiB encoded limit",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut encoded = Limited(Vec::new());
    serde_json::to_writer(&mut encoded, value).map_err(DocumentError::invalid)?;
    reject_ambiguous_number_key(&encoded.0)?;
    encoded.0.push(b'\n');
    Ok(encoded.0)
}

fn value_bytes(value: &Value) -> usize {
    let children = match value {
        // arbitrary_precision stores numeric tokens in a String. Its capacity
        // is private; allow for geometric growth and the parser's minimum 16 B.
        Value::Number(number) => number.as_str().len().saturating_mul(2).max(16),
        Value::String(s) => s.capacity(),
        Value::Array(values) => values
            .capacity()
            .saturating_mul(std::mem::size_of::<Value>())
            .saturating_add(
                values
                    .iter()
                    .map(value_bytes)
                    .fold(0usize, usize::saturating_add),
            ),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| {
                key.capacity()
                    .saturating_add(128)
                    .saturating_add(value_bytes(value))
            })
            .fold(0usize, usize::saturating_add),
        _ => 0,
    };
    std::mem::size_of::<Value>().saturating_add(children)
}
fn extra_bytes(extra: &Extensions) -> usize {
    extra
        .iter()
        .map(|(key, value)| {
            key.capacity()
                .saturating_add(128)
                .saturating_add(value_bytes(value))
        })
        .fold(0usize, usize::saturating_add)
}
fn row_metadata_bytes(meta: &RecordMetadata) -> usize {
    meta.event_kind
        .as_ref()
        .map_or(0, String::capacity)
        .saturating_add(meta.process_source.as_ref().map_or(0, String::capacity))
        .saturating_add(extra_bytes(&meta.extra))
        .saturating_add(extra_bytes(&meta.device_time_extra))
        .saturating_add(extra_bytes(&meta.process_extra))
        .saturating_add(extra_bytes(&meta.provenance_extra))
        .saturating_add(extra_bytes(&meta.text_loss_extra))
        .saturating_add(extra_bytes(&meta.proto_extra))
        .saturating_add(extra_bytes(&meta.projection_extra))
}
fn metadata_bytes(metadata: &Vec<RecordMetadata>) -> usize {
    metadata
        .capacity()
        .saturating_mul(std::mem::size_of::<RecordMetadata>())
        .saturating_add(
            metadata
                .iter()
                .map(row_metadata_bytes)
                .fold(0usize, usize::saturating_add),
        )
}
fn typed_bytes(value: &impl Serialize) -> usize {
    serde_json::to_value(value).map_or(usize::MAX, |v| value_bytes(&v))
}

fn no_reserved(extra: &Extensions, reserved: &[&str]) -> Result<(), DocumentError> {
    if let Some(key) = reserved.iter().find(|key| extra.contains_key(**key)) {
        Err(DocumentError::invalid(format!(
            "optional extension collides with reserved field {key}"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    use std::io::Cursor;

    const BUDGET: usize = 16 * 1024 * 1024;
    fn cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn sample() -> Document {
        let mut session = Session::new();
        session.begin_attempt().unwrap();
        session.stream_started().unwrap();
        let mut store = LogStore::default();
        let mut entry = LogEntry::new(
            "10-23 15:03:02.001",
            42,
            43,
            "标签",
            Level::Error,
            "first\nsecond\0🦀",
        );
        entry.process = Some("com.example:worker".into());
        entry.process_source = Some(ProcessSource::PsSnapshot);
        for mut row in [
            entry,
            LogEntry::marker("--------- beginning of main"),
            LogEntry::unparsed("unrecognized source text"),
        ] {
            row.provenance = session.next_record(Some(1_700_000_000_000));
            store.push(row);
        }
        Document::from_snapshot(
            DocumentHeader::live(session.id(), "test-device".into(), vec![LogBuffer::Main]),
            Arc::new(store),
            ExportScope::default(),
            CaptureStatus {
                state: "running".into(),
                ..CaptureStatus::default()
            },
        )
        .unwrap()
    }
    fn encode(doc: &Document) -> Vec<u8> {
        let mut bytes = Vec::new();
        let summary = write_snapshot(&mut bytes, doc, &cancel()).unwrap();
        assert_eq!(summary.bytes, bytes.len() as u64);
        bytes
    }
    fn values(doc: &Document) -> Vec<Value> {
        encode(doc)
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect()
    }
    fn serialize_values(values: &[Value]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            serde_json::to_writer(&mut bytes, value).unwrap();
            bytes.push(b'\n');
        }
        bytes
    }
    fn reject(bytes: Vec<u8>) -> DocumentError {
        load(Cursor::new(bytes), BUDGET, &cancel())
            .err()
            .expect("must reject complete candidate")
    }

    #[test]
    fn all_record_kinds_round_trip_with_identity_and_unknown_extensions() {
        let mut lines = values(&sample());
        lines[0]["future_header"] = serde_json::json!({"nested": [null, "future"]});
        lines[0]["source"]["sdk"] = 99.into();
        lines[1]["device_time"]["future_precision"] = "nanoseconds".into();
        lines[1]["process"]["source"] = "future_device_hint".into();
        lines[1]["process"]["sample_window"] = serde_json::json!([10, 20]);
        lines[1]["provenance"]["future_clock"] = 123.into();
        lines[1]["future_record"] = serde_json::json!({"bytes": [0, 255]});
        lines[4]["future_footer"] = true.into();
        let doc = load(Cursor::new(serialize_values(&lines)), BUDGET, &cancel()).unwrap();
        assert_eq!(doc.entries[0].message.as_ref(), "first\nsecond\0🦀");
        assert_eq!(doc.entries[0].process_source, None);
        assert_eq!(doc.entries[1].kind, LogEntryKind::Marker);
        assert_eq!(doc.entries[2].kind, LogEntryKind::Unparsed);
        assert_eq!(values(&doc), lines);
        assert_eq!(doc.end.capture.state, "running");
        assert_eq!(doc.end.capture.completeness, "unknown");
    }

    #[test]
    fn empty_native_and_offline_footer_without_newline_are_valid() {
        let header = DocumentHeader::live(SessionId::new(), "serial".into(), vec![LogBuffer::Main]);
        let doc = Document::from_snapshot(
            header,
            Arc::new(LogStore::default()),
            ExportScope::default(),
            CaptureStatus::default(),
        )
        .unwrap();
        let mut bytes = encode(&doc);
        bytes.pop();
        let read = load(Cursor::new(bytes), BUDGET, &cancel()).unwrap();
        assert!(read.entries.is_empty());
        assert!(read.end.finalized);
        assert_eq!(read.end.first_seq, None);
    }

    #[test]
    fn v1_import_and_native_resave_do_not_invent_source_or_process() {
        let row = LogEntry::new(
            "12-31 23:59:59.123",
            0,
            0,
            "logcat",
            Level::Info,
            "--------- beginning of main",
        );
        let mut value =
            serde_json::to_value(crate::record_io::LegacyRecord::from_entry(&row, 9)).unwrap();
        value["unknown"] = serde_json::json!({"a": "b"});
        value["process"] = "not an established v1 field".into();
        let doc = load(
            Cursor::new(serde_json::to_vec(&value).unwrap()),
            BUDGET,
            &cancel(),
        )
        .unwrap();
        assert_eq!(doc.origin, DocumentOrigin::LegacyV1);
        assert!(doc.header.session_id.is_none());
        assert!(doc.header.source.device_serial.is_none());
        assert!(doc.entries[0].provenance.is_none());
        assert!(doc.entries[0].process.is_none());
        assert_eq!(doc.entries[0].kind, LogEntryKind::Log); // Original kind is unknown.
        assert_eq!(doc.record_metadata[0].legacy_original_seq, Some(9));
        let again = load(Cursor::new(encode(&doc)), BUDGET, &cancel()).unwrap();
        assert!(again.entries[0].provenance.is_none());
        assert_eq!(
            again.record_metadata[0].extra["legacy_extensions"]["unknown"],
            value["unknown"]
        );
        assert_eq!(again.end.capture.completeness, "unknown");
    }

    #[test]
    fn missing_wrong_or_followed_footer_rejects_whole_native_artifact() {
        let lines = values(&sample());
        assert!(
            reject(serialize_values(&lines[..4]))
                .message
                .contains("artifact_end")
        );
        let mut wrong = lines.clone();
        wrong[4]["record_count"] = 999.into();
        assert!(
            reject(serialize_values(&wrong))
                .message
                .contains("count/range")
        );
        wrong = lines.clone();
        wrong[4]["artifact_id"] = SessionId::new().to_string().into();
        assert!(
            reject(serialize_values(&wrong))
                .message
                .contains("identity")
        );
        let mut trailing = serialize_values(&lines);
        trailing.extend(b"\n");
        assert!(reject(trailing).message.contains("after artifact_end"));
        wrong = lines;
        wrong[4]["capture"]["completeness"] = "complete".into();
        assert!(
            reject(serialize_values(&wrong))
                .message
                .contains("completeness")
        );
    }

    #[test]
    fn invalid_sequence_foreign_session_and_fabricated_event_fields_fail() {
        for (row, key, value) in [
            (1, "seq", serde_json::json!(0)),
            (2, "seq", serde_json::json!(1)),
            (2, "pid", serde_json::json!(0)),
        ] {
            let mut lines = values(&sample());
            lines[row][key] = value;
            assert_eq!(
                reject(serialize_values(&lines)).kind,
                DocumentErrorKind::InvalidData
            );
        }
        let mut lines = values(&sample());
        lines[1]["provenance"]["session_id"] = SessionId::new().to_string().into();
        assert!(
            reject(serialize_values(&lines))
                .message
                .contains("identity")
        );
        let mut lines = values(&sample());
        lines[1]["provenance"]["segment"] = 0.into();
        assert!(
            reject(serialize_values(&lines))
                .message
                .contains("identity")
        );
    }

    #[test]
    fn filtered_export_keeps_refs_and_composes_subsequent_filters() {
        let original = sample();
        let first = original
            .select(&[0, 2], serde_json::json!({"query": "a"}))
            .unwrap();
        let second = first
            .select(&[1], serde_json::json!({"query": "b"}))
            .unwrap();
        assert_eq!(second.record_metadata[0].seq, 3);
        assert_eq!(second.entries[0].provenance, original.entries[2].provenance);
        assert_ne!(second.header.artifact_id, first.header.artifact_id);
        assert_eq!(
            second.header.derived_from_artifact,
            Some(first.header.artifact_id)
        );
        assert_eq!(second.end.scope.source_record_count, 3);
        assert_eq!(
            second.end.scope.filter,
            Some(serde_json::json!({"all_of": [{"query":"a"}, {"query":"b"}]}))
        );
        assert!(first.select(&[1, 0], Value::Null).is_err());
        assert!(first.select(&[8], Value::Null).is_err());
        assert!(first.select(&[0], Value::Null).is_err());
        assert!(load(Cursor::new(encode(&second)), BUDGET, &cancel()).is_ok());
    }

    #[test]
    fn memory_budget_counts_extensions_and_failure_keeps_previous_snapshot() {
        let old = sample();
        let before = old.entries[0].message.clone();
        let mut lines = values(&old);
        lines[1]["large_optional_tree"] = serde_json::json!(vec!["large optional field"; 1000]);
        let bytes = serialize_values(&lines);
        let error = load(Cursor::new(bytes.clone()), 512 * 1024, &cancel())
            .err()
            .unwrap();
        assert_eq!(error.kind, DocumentErrorKind::MemoryBudget);
        let loaded = load(Cursor::new(bytes), BUDGET, &cancel()).unwrap();
        assert!(loaded.memory_bytes() > old.memory_bytes());
        assert_eq!(old.entries[0].message, before);
        assert_eq!(old.entries.len(), 3);
    }

    #[test]
    fn cancellation_and_encoded_line_limit_fail_without_success() {
        let doc = sample();
        let stopped = AtomicBool::new(true);
        assert_eq!(
            load(Cursor::new(encode(&doc)), BUDGET, &stopped)
                .err()
                .unwrap()
                .kind,
            DocumentErrorKind::Cancelled
        );
        let mut output = Vec::new();
        assert_eq!(
            write_snapshot(&mut output, &doc, &stopped)
                .err()
                .unwrap()
                .kind,
            DocumentErrorKind::Cancelled
        );
        assert!(output.is_empty());
        let oversized = vec![b'x'; MAX_RECORD_BYTES as usize + 1];
        assert!(reject(oversized).message.contains("8 MiB"));
        let mut huge = sample();
        let mut store = LogStore::default();
        let mut row = huge.entries[0].clone();
        row.message = "\0".repeat(MAX_RECORD_BYTES as usize / 6).into();
        store.push(row);
        huge.entries = Arc::new(store);
        huge.record_metadata.truncate(1);
        huge.end = make_end(
            &huge.header,
            &huge.record_metadata,
            ExportScope {
                source_first_seq: Some(1),
                source_last_seq: Some(1),
                source_record_count: 1,
                ..ExportScope::default()
            },
            CaptureStatus::default(),
        );
        assert!(
            write_snapshot(Vec::new(), &huge, &cancel())
                .err()
                .unwrap()
                .message
                .contains("8 MiB")
        );
    }

    #[test]
    fn duplicate_known_json_fields_and_reserved_extensions_are_rejected() {
        let encoded = String::from_utf8(encode(&sample())).unwrap();
        let duplicate = encoded.replacen(
            "\"schema_version\":2",
            "\"schema_version\":2,\"schema_version\":2",
            1,
        );
        assert!(reject(duplicate.into_bytes()).message.contains("duplicate"));
        let mut doc = sample();
        doc.record_metadata[0]
            .extra
            .insert("message".into(), "override".into());
        assert!(
            write_snapshot(Vec::new(), &doc, &cancel())
                .err()
                .unwrap()
                .message
                .contains("reserved")
        );
    }

    #[test]
    fn writer_reports_io_failure_instead_of_finalization_success() {
        struct Fail;
        impl Write for Fail {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            write_snapshot(Fail, &sample(), &cancel())
                .err()
                .unwrap()
                .kind,
            DocumentErrorKind::Io
        );
    }

    #[test]
    fn host_text_loss_round_trips_without_inventing_legacy_observations() {
        let mut lines = values(&sample());
        lines[1]["text_loss"] = serde_json::json!({
            "truncated": true, "invalid_utf8_replaced": false, "future_host_observation": "kept"
        });
        lines[2]["text_loss"] =
            serde_json::json!({"truncated": false, "invalid_utf8_replaced": true});
        lines[3]["text_loss"] =
            serde_json::json!({"truncated": false, "invalid_utf8_replaced": false});
        let doc = load(Cursor::new(serialize_values(&lines)), BUDGET, &cancel()).unwrap();
        assert_eq!(
            doc.entries[0].text_loss,
            Some(TextLoss {
                truncated: true,
                invalid_utf8_replaced: false
            })
        );
        assert_eq!(
            doc.entries[1].text_loss,
            Some(TextLoss {
                truncated: false,
                invalid_utf8_replaced: true
            })
        );
        assert_eq!(doc.entries[2].text_loss, Some(TextLoss::default()));
        assert_eq!(values(&doc), lines);
        let legacy = crate::record_io::LegacyRecord::from_entry(&doc.entries[0], 7);
        let old = load(
            Cursor::new(serde_json::to_vec(&legacy).unwrap()),
            BUDGET,
            &cancel(),
        )
        .unwrap();
        assert_eq!(old.entries[0].text_loss, None);
        let reread = load(Cursor::new(encode(&old)), BUDGET, &cancel()).unwrap();
        assert_eq!(reread.entries[0].text_loss, None);
        lines[1]["text_loss"] = serde_json::json!({"truncated": true});
        assert_eq!(
            reject(serialize_values(&lines)).kind,
            DocumentErrorKind::InvalidData
        );
    }

    #[test]
    fn writer_rejects_nonlog_attribution_and_orphaned_extensions_symmetrically() {
        for index in [1, 2] {
            let mut doc = sample();
            let mut store = LogStore::default();
            for (position, mut row) in doc.entries.iter().cloned().enumerate() {
                if position == index {
                    row.process = Some("cannot-belong-to-marker".into());
                    row.process_source = Some(ProcessSource::PsSnapshot);
                }
                store.push(row);
            }
            doc.entries = Arc::new(store);
            let error = write_snapshot(Vec::new(), &doc, &cancel()).err().unwrap();
            assert!(error.message.contains("cannot carry process"));
        }
        for field in ["device_time", "process", "text_loss"] {
            let mut doc = sample();
            let meta = &mut doc.record_metadata[1];
            let extra = match field {
                "device_time" => &mut meta.device_time_extra,
                "process" => &mut meta.process_extra,
                _ => &mut meta.text_loss_extra,
            };
            extra.insert("extension".into(), true.into());
            assert!(
                write_snapshot(Vec::new(), &doc, &cancel())
                    .err()
                    .unwrap()
                    .message
                    .contains("parent field")
            );
        }
        let row = crate::record_io::LegacyRecord::from_entry(&sample().entries[0], 7);
        let mut legacy = load(
            Cursor::new(serde_json::to_vec(&row).unwrap()),
            BUDGET,
            &cancel(),
        )
        .unwrap();
        legacy.record_metadata[0]
            .provenance_extra
            .insert("extension".into(), true.into());
        assert!(
            write_snapshot(Vec::new(), &legacy, &cancel())
                .err()
                .unwrap()
                .message
                .contains("parent field")
        );
    }
}
