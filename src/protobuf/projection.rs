use std::sync::Arc;

use super::{DecodedEntry, Priority};
use crate::model::{Level, LogEntry, ProcessSource, ProtoEvidence, TextLoss};

impl DecodedEntry {
    /// Project the fixed `PROJECTION_PROFILE` into a row, moving the original
    /// bytes into evidence. Callers assign provenance separately. Future changes
    /// to these rules require another profile; old archives retain this one.
    pub fn project(self) -> LogEntry {
        let normalized = self.normalize();
        let level = match normalized.priority {
            Priority::Verbose => Some(Level::Verbose),
            Priority::Debug => Some(Level::Debug),
            Priority::Info => Some(Level::Info),
            Priority::Warn => Some(Level::Warn),
            Priority::Error => Some(Level::Error),
            Priority::Fatal => Some(Level::Fatal),
            _ => None,
        };
        let mut entry = if let (true, Some(level), Ok(pid), Ok(tid), Some(time)) = (
            self.has_known_content(),
            level,
            u32::try_from(self.pid),
            u32::try_from(self.tid),
            normalized.device_time,
        ) {
            let mut row = LogEntry::new(
                format!("{}.{:09}", time.seconds, time.nanoseconds),
                pid,
                tid,
                normalized.tag.as_ref(),
                level,
                normalized.message.as_ref(),
            );
            if let Some(name) = normalized.process_name.filter(|name| !name.is_empty()) {
                row.process = Some(name.into());
                row.process_source = Some(ProcessSource::DeviceProcessCache);
            }
            row
        } else {
            LogEntry::unparsed(normalized.message.as_ref())
        };
        entry.text_loss = Some(TextLoss {
            truncated: false,
            invalid_utf8_replaced: normalized.normalization.tag_invalid_utf8_replaced
                || normalized.normalization.message_invalid_utf8_replaced,
        });
        entry.proto = Some(Arc::new(ProtoEvidence {
            time_sec: self.time_sec,
            time_nsec: self.time_nsec,
            uid: self.uid,
            pid: self.pid,
            tid: self.tid,
            priority: self.priority,
            raw_payload: self.raw_payload,
            tag_bytes: self.tag,
            message_bytes: self.message,
            process_name: self.process_name,
        }));
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LogEntryKind;

    #[test]
    fn frozen_projection_preserves_integer_precision_whitespace_and_wire_occurrences() {
        // u64::MAX seconds/UID, nsec=2, Fatal=8, pid=4 then overlong 6,
        // tid=5, tag=invalid UTF-8 plus two NULs, message with whitespace/NUL,
        // process name with whitespace, and an unknown field. No prost encoder.
        let payload = b"\x08\xff\xff\xff\xff\xff\xff\xff\xff\xff\x01\x10\x02\x18\x08\x20\xff\xff\xff\xff\xff\xff\xff\xff\xff\x01\x28\x04\x30\x05\x3a\x04\xffT\0\0\x42\x05 \nm\0 \x4a\x03 p\n\x28\x86\x00\x98\x06\x01";
        let row = DecodedEntry::decode_payload(payload.to_vec())
            .unwrap()
            .project();
        assert_eq!(row.kind, LogEntryKind::Log);
        assert_eq!(row.time.as_ref(), "18446744073709551615.000000002");
        assert_eq!((row.pid, row.tid, row.level), (6, 5, Level::Fatal));
        assert_eq!(row.tag.as_ref(), "�T\0");
        assert_eq!(row.message.as_ref(), " \nm\0 ");
        assert_eq!(row.process.as_deref(), Some(" p\n"));
        assert_eq!(row.process_source, Some(ProcessSource::DeviceProcessCache));
        assert_eq!(
            row.text_loss,
            Some(TextLoss {
                truncated: false,
                invalid_utf8_replaced: true
            })
        );
        let proto = row.proto.unwrap();
        assert_eq!(proto.raw_payload, payload);
        assert_eq!(proto.uid, u64::MAX);
        assert_eq!(proto.tag_bytes, b"\xffT\0\0");
    }

    #[test]
    fn fixed_priority_vocabulary_and_optional_process_presence_are_preserved() {
        for priority in 0..=10 {
            let row = DecodedEntry::decode_payload(vec![0x18, priority, 0x4a, 0])
                .unwrap()
                .project();
            let expected = match priority {
                2 => Some(Level::Verbose),
                3 => Some(Level::Debug),
                4 => Some(Level::Info),
                5 => Some(Level::Warn),
                6 => Some(Level::Error),
                8 => Some(Level::Fatal),
                _ => None,
            };
            assert_eq!(
                row.kind == LogEntryKind::Log,
                expected.is_some(),
                "{priority}"
            );
            if let Some(level) = expected {
                assert_eq!(row.level, level);
            }
            assert!(row.process.is_none());
            assert!(row.process_source.is_none());
            assert_eq!(row.proto.unwrap().process_name.as_deref(), Some(""));
        }
    }
}
