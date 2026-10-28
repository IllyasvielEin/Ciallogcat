//! Stable evidence references, independent of view indices and capture epochs.
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn is_nil(self) -> bool {
        self.0.is_nil()
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordIdentity {
    pub session_id: SessionId,
    pub seq: u64,
    /// Successfully started stream, incremented after each confirmed start.
    pub segment: u64,
    /// Spawn attempt, including failed attempts; never a device process ID.
    pub attempt: u64,
    /// Host wall-clock observation in milliseconds, not device log time.
    pub observed_unix_ms: Option<u64>,
}

/// Owned by a live document, not by the rolling store or the UI selection.
/// Clear, retention, pause and reconnect must keep this same allocator.
pub struct Session {
    id: SessionId,
    seq: u64,
    segment: u64,
    attempt: u64,
    started: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            id: SessionId::new(),
            seq: 0,
            segment: 0,
            attempt: 0,
            started: false,
        }
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn last_seq(&self) -> u64 {
        self.seq
    }

    pub fn begin_attempt(&mut self) -> Option<u64> {
        self.started = false;
        self.attempt = self.attempt.checked_add(1)?;
        Some(self.attempt)
    }

    pub fn stream_started(&mut self) -> Option<u64> {
        if self.attempt == 0 || self.started {
            return None;
        }
        self.segment = self.segment.checked_add(1)?;
        self.started = true;
        Some(self.segment)
    }

    /// Refuses unstarted streams and counter exhaustion instead of reusing IDs.
    pub fn next_record(&mut self, observed_unix_ms: Option<u64>) -> Option<RecordIdentity> {
        if !self.started {
            return None;
        }
        self.seq = self.seq.checked_add(1)?;
        Some(RecordIdentity {
            session_id: self.id,
            seq: self.seq,
            segment: self.segment,
            attempt: self.attempt,
            observed_unix_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_survive_store_clear_retention_and_reconnect() {
        let mut session = Session::new();
        session.begin_attempt().unwrap();
        session.stream_started().unwrap();
        let first = session.next_record(Some(42)).unwrap();
        // Neither dropping rows nor starting a new ADB process resets seq.
        session.begin_attempt().unwrap();
        session.begin_attempt().unwrap(); // One failed start left no records.
        session.stream_started().unwrap();
        let second = session.next_record(None).unwrap();
        assert_eq!(first.session_id, second.session_id);
        assert_eq!((second.seq, second.segment, second.attempt), (2, 2, 3));
        assert_ne!(session.id(), Session::new().id());
        assert_eq!(second.observed_unix_ms, None);
        assert_eq!(
            serde_json::from_str::<RecordIdentity>(&serde_json::to_string(&second).unwrap())
                .unwrap(),
            second
        );
    }

    #[test]
    fn counters_never_wrap_or_stamp_before_start() {
        let mut session = Session::new();
        assert_eq!(session.stream_started(), None);
        assert_eq!(session.next_record(None), None);
        session.begin_attempt().unwrap();
        assert_eq!(session.next_record(None), None);
        session.stream_started().unwrap();
        assert_eq!(session.stream_started(), None);
        session.begin_attempt().unwrap();
        assert_eq!(session.next_record(None), None);
        session.stream_started().unwrap();
        session.seq = u64::MAX;
        assert_eq!(session.next_record(None), None);
        assert_eq!(session.last_seq(), u64::MAX);
        session.attempt = u64::MAX;
        assert_eq!(session.begin_attempt(), None);
        session.segment = u64::MAX;
        assert_eq!(session.stream_started(), None);
    }
}
