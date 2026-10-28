use super::*;
use std::io::Cursor;

// Independent golden bytes: protobuf wire fields written by hand from the
// published schema, rather than encoded by the decoder's generated Rust type.
// time=(1,2), priority=FATAL(8), uid=0, pid=42, tid=43,
// tag=b"T\0", message=b"a\n\0\xff", process_name="p".
const GOLDEN: &[u8] = &[
    0x08, 0x01, 0x10, 0x02, 0x18, 0x08, 0x28, 0x2a, 0x30, 0x2b, 0x3a, 0x02, 0x54, 0x00, 0x42, 0x04,
    0x61, 0x0a, 0x00, 0xff, 0x4a, 0x01, 0x70,
];

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut bytes = (payload.len() as u64).to_le_bytes().to_vec();
    bytes.extend_from_slice(payload);
    bytes
}
fn message() -> generated::LogcatEntryProto {
    generated::LogcatEntryProto {
        time_sec: 1_700_000_000,
        time_nsec: 123_456_789,
        priority: 4,
        uid: 10123,
        pid: 456,
        tid: 457,
        tag: b"Tag\0".to_vec(),
        message: b"first\nsecond".to_vec(),
        process_name: Some("com.example:worker".into()),
    }
}

#[test]
fn independent_golden_frame_and_fields_are_preserved() {
    let wire = frame(GOLDEN);
    assert_eq!(&wire[..8], &[23, 0, 0, 0, 0, 0, 0, 0]);
    let mut reader = FrameReader::new(Cursor::new(wire));
    let row = reader.read_next().unwrap().unwrap();
    assert_eq!(
        (row.time_sec, row.time_nsec, row.uid, row.pid, row.tid),
        (1, 2, 0, 42, 43)
    );
    assert_eq!(row.priority, 8);
    assert_eq!(row.tag, b"T\0");
    assert_eq!(row.message, b"a\n\0\xff");
    assert_eq!(row.process_name.as_deref(), Some("p"));
    assert_eq!(row.raw_payload, GOLDEN);
    assert_eq!((reader.frames_read(), reader.offset()), (1, 31));
    assert!(row.has_known_content());
    assert!(reader.read_next().unwrap().is_none());
    assert!(reader.read_next().unwrap().is_none());
}

#[test]
fn display_normalization_preserves_message_boundaries_and_source_bytes() {
    let row = DecodedEntry::decode_payload(GOLDEN.to_vec()).unwrap();
    let display = row.normalize();
    assert_eq!(display.tag, "T");
    assert_eq!(display.message, "a\n\0\u{fffd}");
    assert_eq!(display.priority, Priority::Fatal);
    assert_eq!(
        display.device_time,
        Some(DeviceTime {
            seconds: 1,
            nanoseconds: 2
        })
    );
    assert_eq!(
        display.normalization,
        Normalization {
            tag_trailing_nul_removed: true,
            tag_invalid_utf8_replaced: false,
            message_invalid_utf8_replaced: true,
        }
    );
    assert_eq!(row.raw_payload, GOLDEN);
    assert_eq!(row.message, b"a\n\0\xff");
}

#[test]
fn normalization_removes_exactly_one_tag_nul_and_does_not_trim_process_or_message() {
    let mut m = message();
    m.tag = b"\xffTag\0\0".to_vec();
    m.message = b" \nmessage\r\n\0 ".to_vec();
    m.process_name = Some("kernel-thread\n".into());
    let row = DecodedEntry::decode_payload(m.encode_to_vec()).unwrap();
    let normalized = row.normalize();
    assert_eq!(normalized.tag, "\u{fffd}Tag\0");
    assert_eq!(normalized.message, " \nmessage\r\n\0 ");
    assert_eq!(normalized.process_name, Some("kernel-thread\n"));
    assert!(normalized.normalization.tag_invalid_utf8_replaced);
    assert!(!normalized.normalization.message_invalid_utf8_replaced);
    assert_eq!(row.tag, b"\xffTag\0\0");
}

#[test]
fn valid_utf8_display_borrows_and_empty_fields_stay_valid() {
    let mut m = message();
    m.tag = b"Tag".to_vec();
    m.message.clear();
    m.process_name = None;
    let row = DecodedEntry::decode_payload(m.encode_to_vec()).unwrap();
    let normalized = row.normalize();
    assert!(matches!(normalized.tag, Cow::Borrowed("Tag")));
    assert!(matches!(normalized.message, Cow::Borrowed("")));
    assert_eq!(normalized.normalization, Normalization::default());
    assert_eq!(normalized.process_name, None);
    assert!(row.has_known_content());
}

#[test]
fn unknown_fields_and_unknown_enum_numbers_survive_in_evidence() {
    for numeric in [0, 7, 123, -1] {
        let mut m = message();
        m.priority = numeric;
        let mut payload = m.encode_to_vec();
        payload.extend([0xa0, 0x06, 0x7b]); // unknown field 100, varint 123
        let row = DecodedEntry::decode_payload(payload.clone()).unwrap();
        assert_eq!(row.priority, numeric);
        assert_eq!(row.normalize().priority, Priority::Unknown(numeric));
        assert_eq!(row.raw_payload, payload);
        assert_ne!(
            generated::LogcatEntryProto::decode(payload.as_slice())
                .unwrap()
                .encode_to_vec(),
            payload
        );
    }
    assert_eq!(Priority::from_wire(8), Priority::Fatal);
    assert_eq!(Priority::from_wire(9), Priority::Silent);
    assert_eq!(Priority::from_wire(1), Priority::Default);
}

#[test]
fn empty_or_unknown_only_payload_is_not_evidence_of_a_known_record() {
    for payload in [vec![], vec![0xa0, 0x06, 0x7b], vec![0x4a, 0x00]] {
        let mut reader = FrameReader::new(Cursor::new(frame(&payload)));
        let row = reader.read_next().unwrap().unwrap();
        assert!(!row.has_known_content());
        assert_eq!(row.raw_payload, payload);
        assert!(reader.read_next().unwrap().is_none());
    }
}

#[test]
fn string_utf8_is_validated_while_bytes_utf8_is_not() {
    let good = DecodedEntry::decode_payload(vec![0x3a, 1, 0xff, 0x42, 1, 0xff]).unwrap();
    assert_eq!(good.tag, [0xff]);
    assert_eq!(good.message, [0xff]);
    assert!(matches!(
        DecodedEntry::decode_payload(vec![0x4a, 1, 0xff])
            .unwrap_err()
            .kind,
        ProtoErrorKind::Malformed { .. }
    ));
}

#[test]
fn invalid_time_and_large_identifiers_remain_available_without_integer_overflow() {
    let mut m = message();
    m.time_sec = u64::MAX;
    m.time_nsec = u64::MAX;
    m.uid = u64::MAX;
    m.pid = u64::MAX;
    m.tid = u64::MAX;
    let row = DecodedEntry::decode_payload(m.encode_to_vec()).unwrap();
    assert_eq!(row.time_sec, u64::MAX);
    assert_eq!(row.time_nsec, u64::MAX);
    assert_eq!((row.uid, row.pid, row.tid), (u64::MAX, u64::MAX, u64::MAX));
    assert_eq!(row.normalize().device_time, None);
    assert_eq!(
        DeviceTime {
            seconds: u64::MAX,
            nanoseconds: 0
        }
        .unix_nanos(),
        None
    );
    assert_eq!(
        DeviceTime {
            seconds: 1,
            nanoseconds: 1_000_000_000
        }
        .unix_nanos(),
        None
    );
    assert_eq!(
        DeviceTime {
            seconds: 1,
            nanoseconds: 2
        }
        .unix_nanos(),
        Some(1_000_000_002)
    );
}

#[test]
fn every_truncated_header_or_payload_boundary_is_distinguished() {
    let complete = frame(GOLDEN);
    for n in 1..8 {
        let error = FrameReader::new(Cursor::new(&complete[..n]))
            .read_next()
            .unwrap_err();
        assert!(
            matches!(error.kind, ProtoErrorKind::TruncatedHeader { received } if received == n)
        );
        assert_eq!((error.frame, error.offset), (0, 0));
    }
    for n in 8..complete.len() {
        let error = FrameReader::new(Cursor::new(&complete[..n]))
            .read_next()
            .unwrap_err();
        assert!(
            matches!(error.kind, ProtoErrorKind::TruncatedPayload { declared: 23, received } if received == n - 8)
        );
    }
    assert!(FrameReader::new(io::empty()).read_next().unwrap().is_none());
}

#[test]
fn one_byte_fragmentation_and_interrupted_reads_are_handled() {
    struct Fragmented {
        source: Cursor<Vec<u8>>,
        interrupt: bool,
    }
    impl Read for Fragmented {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            let amount = out.len().min(1);
            self.source.read(&mut out[..amount])
        }
    }
    let mut reader = FrameReader::new(Fragmented {
        source: Cursor::new(frame(GOLDEN)),
        interrupt: false,
    });
    assert_eq!(reader.read_next().unwrap().unwrap().raw_payload, GOLDEN);
    assert!(reader.read_next().unwrap().is_none());
}

#[test]
fn oversized_length_is_rejected_before_payload_read() {
    struct HeaderOnly(Cursor<[u8; 8]>);
    impl Read for HeaderOnly {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            assert!(self.0.position() < 8, "oversized payload must not be read");
            self.0.read(out)
        }
    }
    for length in [MAX_FRAME_BYTES + 1, u64::MAX, 0x0100_0000_0000_0000] {
        let error = FrameReader::new(HeaderOnly(Cursor::new(length.to_le_bytes())))
            .read_next()
            .unwrap_err();
        assert!(
            matches!(error.kind, ProtoErrorKind::FrameTooLarge { declared, limit: MAX_FRAME_BYTES } if declared == length)
        );
    }
}

#[test]
fn frame_at_exact_product_limit_is_accepted() {
    // Unknown length-delimited field 100; tag=2 bytes, length=3 bytes.
    let length = MAX_FRAME_BYTES as usize - 5;
    let mut payload = vec![
        0xa2,
        0x06,
        (length as u8) | 0x80,
        ((length >> 7) as u8) | 0x80,
        (length >> 14) as u8,
    ];
    payload.resize(MAX_FRAME_BYTES as usize, 0);
    let row = FrameReader::new(Cursor::new(frame(&payload)))
        .read_next()
        .unwrap()
        .unwrap();
    assert_eq!(row.raw_payload.len(), MAX_FRAME_BYTES as usize);
    assert!(!row.has_known_content());
    assert!(row.owned_bytes() >= MAX_FRAME_BYTES as usize);
}

#[test]
fn malformed_wire_types_lengths_and_varints_are_terminal() {
    for payload in [
        vec![0x80],
        vec![0],
        vec![0x0a, 0],
        vec![0x42, 0x7f, 0x61],
        vec![0xff; 20],
    ] {
        let mut bytes = frame(&payload);
        bytes.extend(frame(GOLDEN));
        let mut reader = FrameReader::new(Cursor::new(bytes));
        assert!(matches!(
            reader.read_next().unwrap_err().kind,
            ProtoErrorKind::Malformed { .. }
        ));
        assert!(matches!(
            reader.read_next().unwrap_err().kind,
            ProtoErrorKind::ReaderTerminated
        ));
        assert_eq!((reader.offset(), reader.frames_read()), (0, 0));
    }
}

#[test]
fn io_errors_are_not_misclassified_as_clean_or_truncated_eof() {
    struct Fails;
    impl Read for Fails {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::ConnectionReset))
        }
    }
    let error = FrameReader::new(Fails).read_next().unwrap_err();
    assert!(
        matches!(error.kind, ProtoErrorKind::Io(ref e) if e.kind() == io::ErrorKind::ConnectionReset)
    );
    assert!(std::error::Error::source(&error).is_some());
}

#[test]
fn sequential_frame_error_reports_its_own_start_offset() {
    let first = frame(GOLDEN);
    let mut bytes = first.clone();
    bytes.extend([5, 0]);
    let mut reader = FrameReader::new(Cursor::new(bytes));
    assert!(reader.read_next().unwrap().is_some());
    let error = reader.read_next().unwrap_err();
    assert_eq!((error.frame, error.offset), (1, first.len() as u64));
    assert!(matches!(
        error.kind,
        ProtoErrorKind::TruncatedHeader { received: 2 }
    ));
}

#[test]
fn duplicate_fields_follow_protobuf_semantics_but_original_payload_is_retained() {
    let mut payload = GOLDEN.to_vec();
    payload.extend([0x28, 99]); // last pid wins
    let row = DecodedEntry::decode_payload(payload.clone()).unwrap();
    assert_eq!(row.pid, 99);
    assert_eq!(row.raw_payload, payload);
}

#[test]
fn position_counters_never_wrap() {
    let mut reader = FrameReader::new(Cursor::new(frame(&[])));
    reader.offset = u64::MAX;
    assert!(matches!(
        reader.read_next().unwrap_err().kind,
        ProtoErrorKind::CounterOverflow
    ));
    let mut reader = FrameReader::new(Cursor::new(frame(&[])));
    reader.frame = u64::MAX;
    assert!(matches!(
        reader.read_next().unwrap_err().kind,
        ProtoErrorKind::CounterOverflow
    ));
}
