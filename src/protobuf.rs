//! Experimental AOSP `logcat --proto` framing and display normalization.
//!
//! This is an eight-byte little-endian frame stream, not protobuf's varint
//! length-delimited convention. Keep this module independent of process control,
//! native artifact envelopes. See `docs/logcat-protobuf.md`.
use std::borrow::Cow;
use std::fmt;
use std::io::{self, Read};

use prost::Message;
use serde::{Deserialize, Serialize};

#[path = "protobuf/generated.rs"]
mod generated;
mod projection;

/// Frozen host projection of the pinned AOSP schema, not a device wire version.
pub const PROJECTION_PROFILE: &str = "aosp_logcat_16r1_host_v1";

/// Product admission limit, not a maximum declared by the AOSP protocol.
pub const MAX_FRAME_BYTES: u64 = 1024 * 1024;
pub const AOSP_SCHEMA_COMMIT: &str = "d78b713380007d3c0dde14712cbcbec27f491ad9";

/// Decoded fields plus the *exact received protobuf payload*. The latter retains
/// unknown fields and wire representation that prost's re-encoding would lose.
/// These bytes have already passed through device logcat's conversion; they are
/// not the original logd binary payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecodedEntry {
    pub time_sec: u64,
    pub time_nsec: u64,
    pub priority: i32,
    pub uid: u64,
    pub pid: u64,
    pub tid: u64,
    pub tag: Vec<u8>,
    pub message: Vec<u8>,
    pub process_name: Option<String>,
    pub raw_payload: Vec<u8>,
}

impl DecodedEntry {
    /// Decode one payload, preserving every input byte. Use `FrameReader` for a
    /// transport stream; never pass its eight-byte prefix to prost.
    pub fn decode_payload(payload: Vec<u8>) -> Result<Self, ProtoError> {
        if payload.len() as u64 > MAX_FRAME_BYTES {
            return Err(ProtoError::new(
                0,
                0,
                ProtoErrorKind::FrameTooLarge {
                    declared: payload.len() as u64,
                    limit: MAX_FRAME_BYTES,
                },
            ));
        }
        let message = generated::LogcatEntryProto::decode(payload.as_slice()).map_err(|error| {
            ProtoError::new(
                0,
                0,
                ProtoErrorKind::Malformed {
                    message: error.to_string(),
                },
            )
        })?;
        Ok(Self {
            time_sec: message.time_sec,
            time_nsec: message.time_nsec,
            priority: message.priority,
            uid: message.uid,
            pid: message.pid,
            tid: message.tid,
            tag: message.tag,
            message: message.message,
            process_name: message.process_name,
            raw_payload: payload,
        })
    }

    /// True means at least one known semantic field differs from its default.
    /// It is a weak observation of a recognizable record, never proof of device
    /// identity, complete schema support, live capture health or complete logs.
    /// Empty messages and UID/PID zero remain valid.
    pub fn has_known_content(&self) -> bool {
        self.time_sec != 0
            || self.time_nsec != 0
            || self.priority != 0
            || self.uid != 0
            || self.pid != 0
            || self.tid != 0
            || !self.tag.is_empty()
            || !self.message.is_empty()
            || self
                .process_name
                .as_ref()
                .is_some_and(|name| !name.is_empty())
    }

    /// Owned heap data plus this value, before any caller's Arc/queue/container
    /// overhead. Count normalized owned strings separately if retaining them.
    pub fn owned_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.tag.capacity())
            .saturating_add(self.message.capacity())
            .saturating_add(self.process_name.as_ref().map_or(0, String::capacity))
            .saturating_add(self.raw_payload.capacity())
    }

    /// Display policy only. Raw bytes remain on this entry; no trimming, newline
    /// flattening, process-name rewriting, or priority coercion is performed.
    pub fn normalize(&self) -> NormalizedEntry<'_> {
        let (tag, trailing_nul) = match self.tag.strip_suffix(&[0]) {
            Some(tag) => (tag, true),
            None => (self.tag.as_slice(), false),
        };
        let tag_invalid_utf8_replaced = std::str::from_utf8(tag).is_err();
        let message_invalid_utf8_replaced = std::str::from_utf8(&self.message).is_err();
        NormalizedEntry {
            tag: String::from_utf8_lossy(tag),
            message: String::from_utf8_lossy(&self.message),
            process_name: self.process_name.as_deref(),
            priority: Priority::from_wire(self.priority),
            device_time: (self.time_nsec < 1_000_000_000).then_some(DeviceTime {
                seconds: self.time_sec,
                nanoseconds: self.time_nsec as u32,
            }),
            normalization: Normalization {
                tag_trailing_nul_removed: trailing_nul,
                tag_invalid_utf8_replaced,
                message_invalid_utf8_replaced,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceTime {
    pub seconds: u64,
    pub nanoseconds: u32,
}

impl DeviceTime {
    /// Checked conversion for consumers needing one nanosecond integer. JSON
    /// frontends should preserve the integer pair or encode this value as text.
    pub fn unix_nanos(self) -> Option<u64> {
        if self.nanoseconds >= 1_000_000_000 {
            return None;
        }
        self.seconds
            .checked_mul(1_000_000_000)?
            .checked_add(u64::from(self.nanoseconds))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Priority {
    Unknown(i32),
    Default,
    Verbose,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    Silent,
}

impl Priority {
    pub fn from_wire(value: i32) -> Self {
        use generated::LogcatPriorityProto as P;
        match P::try_from(value) {
            Ok(P::Default) => Self::Default,
            Ok(P::Verbose) => Self::Verbose,
            Ok(P::Debug) => Self::Debug,
            Ok(P::Info) => Self::Info,
            Ok(P::Warn) => Self::Warn,
            Ok(P::Error) => Self::Error,
            Ok(P::Fatal) => Self::Fatal,
            Ok(P::Silent) => Self::Silent,
            Ok(P::Unknown) | Err(_) => Self::Unknown(value),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Normalization {
    pub tag_trailing_nul_removed: bool,
    pub tag_invalid_utf8_replaced: bool,
    pub message_invalid_utf8_replaced: bool,
}

#[derive(Debug)]
pub struct NormalizedEntry<'a> {
    pub tag: Cow<'a, str>,
    pub message: Cow<'a, str>,
    /// A device logcat /proc cache hint. Never an exact process-instance claim.
    pub process_name: Option<&'a str>,
    pub priority: Priority,
    /// None for invalid nanoseconds; raw seconds/nanoseconds remain available.
    pub device_time: Option<DeviceTime>,
    pub normalization: Normalization,
}

#[derive(Debug)]
pub struct ProtoError {
    /// Zero-based frame index and byte offset of that frame's length prefix.
    pub frame: u64,
    pub offset: u64,
    pub kind: ProtoErrorKind,
}

#[derive(Debug)]
pub enum ProtoErrorKind {
    Io(io::Error),
    TruncatedHeader { received: usize },
    FrameTooLarge { declared: u64, limit: u64 },
    TruncatedPayload { declared: u64, received: usize },
    Malformed { message: String },
    CounterOverflow,
    ReaderTerminated,
}

impl ProtoError {
    fn new(frame: u64, offset: u64, kind: ProtoErrorKind) -> Self {
        Self {
            frame,
            offset,
            kind,
        }
    }
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "protobuf frame {}, byte {}: ", self.frame, self.offset)?;
        match &self.kind {
            ProtoErrorKind::Io(error) => write!(f, "read failed: {error}"),
            ProtoErrorKind::TruncatedHeader { received } => {
                write!(f, "truncated 8-byte header ({received} bytes received)")
            }
            ProtoErrorKind::FrameTooLarge { declared, limit } => {
                write!(f, "declared payload {declared} exceeds {limit}-byte limit")
            }
            ProtoErrorKind::TruncatedPayload { declared, received } => write!(
                f,
                "truncated payload ({received} of {declared} bytes received)"
            ),
            ProtoErrorKind::Malformed { message } => write!(f, "invalid protobuf: {message}"),
            ProtoErrorKind::CounterOverflow => f.write_str("stream position counter exhausted"),
            ProtoErrorKind::ReaderTerminated => {
                f.write_str("reader terminated after previous error")
            }
        }
    }
}
impl std::error::Error for ProtoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ProtoErrorKind::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// Synchronous, bounded decoder. A caller owns deadlines, cancellation and child
/// termination. Any error terminally poisons the reader; binary data is never
/// resynchronized heuristically or retried as text. Boundary EOF stays EOF.
pub struct FrameReader<R> {
    reader: R,
    offset: u64,
    frame: u64,
    state: ReaderState,
}

#[derive(Clone, Copy)]
enum ReaderState {
    Reading,
    Eof,
    Failed,
}

impl<R: Read> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            offset: 0,
            frame: 0,
            state: ReaderState::Reading,
        }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn frames_read(&self) -> u64 {
        self.frame
    }

    pub fn read_next(&mut self) -> Result<Option<DecodedEntry>, ProtoError> {
        match self.state {
            ReaderState::Eof => return Ok(None),
            ReaderState::Failed => {
                return Err(ProtoError::new(
                    self.frame,
                    self.offset,
                    ProtoErrorKind::ReaderTerminated,
                ));
            }
            ReaderState::Reading => {}
        }
        // Errors cannot be skipped even when the full bad payload was received.
        self.state = ReaderState::Failed;
        let mut header = [0; 8];
        let received = read_until_full(&mut self.reader, &mut header)
            .map_err(|error| ProtoError::new(self.frame, self.offset, ProtoErrorKind::Io(error)))?;
        if received == 0 {
            self.state = ReaderState::Eof;
            return Ok(None);
        }
        if received != header.len() {
            return Err(ProtoError::new(
                self.frame,
                self.offset,
                ProtoErrorKind::TruncatedHeader { received },
            ));
        }
        let declared = u64::from_le_bytes(header);
        if declared > MAX_FRAME_BYTES {
            return Err(ProtoError::new(
                self.frame,
                self.offset,
                ProtoErrorKind::FrameTooLarge {
                    declared,
                    limit: MAX_FRAME_BYTES,
                },
            ));
        }
        let length = usize::try_from(declared).map_err(|_| {
            ProtoError::new(
                self.frame,
                self.offset,
                ProtoErrorKind::FrameTooLarge {
                    declared,
                    limit: MAX_FRAME_BYTES,
                },
            )
        })?;
        let mut payload = vec![0; length];
        let received = read_until_full(&mut self.reader, &mut payload)
            .map_err(|error| ProtoError::new(self.frame, self.offset, ProtoErrorKind::Io(error)))?;
        if received != length {
            return Err(ProtoError::new(
                self.frame,
                self.offset,
                ProtoErrorKind::TruncatedPayload { declared, received },
            ));
        }
        let decoded = DecodedEntry::decode_payload(payload).map_err(|mut error| {
            error.frame = self.frame;
            error.offset = self.offset;
            error
        })?;
        let next_offset = self.offset.checked_add(8 + declared).ok_or_else(|| {
            ProtoError::new(self.frame, self.offset, ProtoErrorKind::CounterOverflow)
        })?;
        let next_frame = self.frame.checked_add(1).ok_or_else(|| {
            ProtoError::new(self.frame, self.offset, ProtoErrorKind::CounterOverflow)
        })?;
        self.offset = next_offset;
        self.frame = next_frame;
        self.state = ReaderState::Reading;
        Ok(Some(decoded))
    }
}

fn read_until_full(reader: &mut impl Read, mut buffer: &mut [u8]) -> io::Result<usize> {
    let mut received = 0;
    while !buffer.is_empty() {
        match reader.read(buffer) {
            Ok(0) => break,
            Ok(count) => {
                received += count;
                buffer = &mut buffer[count..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(received)
}

#[cfg(test)]
#[path = "protobuf/tests.rs"]
mod tests;
