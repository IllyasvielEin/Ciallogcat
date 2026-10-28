use super::*;
use crate::session::Session;
use std::io::Cursor;

#[test]
fn streaming_encoder_rejected_or_uncommitted_rows_do_not_change_footer() {
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let header = DocumentHeader::live(session.id(), "serial".into(), vec![LogBuffer::Main]);
    let mut encoder = StreamEncoder::new(header.clone()).unwrap();
    let mut row = LogEntry::new("time", 1, 2, "Tag", Level::Info, "first");
    row.provenance = session.next_record(None);
    let first = encoder.record_bytes(&row).unwrap();
    let initial_end = encoder.finish_bytes(CaptureStatus::default()).unwrap();
    assert!(encoder.record_bytes(&row).is_err()); // duplicate seq
    assert_eq!(
        encoder.finish_bytes(CaptureStatus::default()).unwrap(),
        initial_end
    );
    row.provenance = session.next_record(None);
    row.message = "x".repeat(MAX_RECORD_BYTES as usize).into();
    assert!(encoder.record_bytes(&row).is_err()); // encoded line exceeds cap
    assert_eq!(
        encoder.finish_bytes(CaptureStatus::default()).unwrap(),
        initial_end
    );
    row.message = "not admitted".into();
    let mut candidate = encoder.clone();
    candidate.record_bytes(&row).unwrap();
    assert_eq!(
        encoder.finish_bytes(CaptureStatus::default()).unwrap(),
        initial_end
    );
    assert_ne!(
        candidate.finish_bytes(CaptureStatus::default()).unwrap(),
        initial_end
    );
    let bytes = [encoder.header_bytes().unwrap(), first, initial_end].concat();
    let doc = load(
        Cursor::new(bytes),
        16 * 1024 * 1024,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(doc.entries.len(), 1);
    assert_eq!(doc.end.first_seq, Some(1));
    assert_eq!(doc.end.last_seq, Some(1));
}

#[test]
fn streaming_encoder_preserves_identity_across_attempts_and_sequence_gaps() {
    let mut session = Session::new();
    let header = DocumentHeader::live(session.id(), "serial".into(), vec![LogBuffer::Main]);
    let mut encoder = StreamEncoder::new(header.clone()).unwrap();
    let mut bytes = encoder.header_bytes().unwrap();
    for attempt in 1..=3 {
        session.begin_attempt().unwrap();
        session.stream_started().unwrap();
        // Dropped candidates consume identity but never claim a saved record.
        session.next_record(None).unwrap();
        let mut row = LogEntry::unparsed(format!("attempt {attempt}"));
        row.provenance = session.next_record(Some(attempt));
        bytes.extend(encoder.record_bytes(&row).unwrap());
    }
    bytes.extend(
        encoder
            .finish_bytes(CaptureStatus {
                completeness: "partial".into(),
                ..CaptureStatus::default()
            })
            .unwrap(),
    );
    let doc = load(
        Cursor::new(bytes),
        16 * 1024 * 1024,
        &AtomicBool::new(false),
    )
    .unwrap();
    for (index, row) in doc.entries.iter().enumerate() {
        let identity = row.provenance.unwrap();
        assert_eq!(identity.session_id, session.id());
        assert_eq!(identity.seq, (index as u64 + 1) * 2);
        assert_eq!(identity.attempt, index as u64 + 1);
        assert_eq!(identity.segment, index as u64 + 1);
    }
    assert_eq!(doc.end.record_count, 3);
    assert_eq!((doc.end.first_seq, doc.end.last_seq), (Some(2), Some(6)));
}

#[test]
fn compact_stream_snapshot_and_generic_decoders_agree_for_mixed_rows_and_empty_files() {
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let mut header = DocumentHeader::live(session.id(), "serial".into(), vec![LogBuffer::Main]);
    assert_eq!(header.schema_version, 2);
    header.set_format(NativeFormat::CompactV3).unwrap();
    let mut encoder = StreamEncoder::new(header.clone()).unwrap();
    let mut bytes = encoder.header_bytes().unwrap();
    let mut store = LogStore::default();
    for mut row in [
        crate::protobuf::DecodedEntry::decode_payload(b"\x18\x04\x42\x03hi\n".to_vec())
            .unwrap()
            .project(),
        LogEntry::new("time", 7, 8, "Text", Level::Info, " text\0 "),
        LogEntry::marker("--------- beginning of main"),
        LogEntry::unparsed("unknown text"),
        crate::protobuf::DecodedEntry::decode_payload(vec![])
            .unwrap()
            .project(),
    ] {
        session.next_record(None); // A gap does not become capture loss.
        row.provenance = session.next_record(None);
        let line = encoder.record_bytes(&row).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&line).unwrap()["schema_version"],
            3
        );
        let (decoded, meta) = decode_record(&line, &header, 0).unwrap();
        assert_eq!(decoded.kind, row.kind);
        assert_eq!(decoded.message, row.message);
        assert_eq!(meta.seq, row.provenance.unwrap().seq);
        bytes.extend(line);
        store.push(row);
    }
    let capture = CaptureStatus {
        completeness: "partial".into(),
        known_gaps: vec![serde_json::json!({"kind":"disconnect"})],
        ..CaptureStatus::default()
    };
    let footer = encoder.finish_bytes(capture.clone()).unwrap();
    assert_eq!(decode_end(&footer, &header).unwrap().schema_version, 3);
    bytes.extend(footer);
    let doc = load(
        Cursor::new(&bytes),
        16 * 1024 * 1024,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(doc.end.record_count, 5);
    assert_eq!((doc.end.first_seq, doc.end.last_seq), (Some(2), Some(10)));
    let snapshot = Document::from_snapshot(
        header.clone(),
        Arc::new(store),
        ExportScope {
            omitted_before: Some(0),
            ..ExportScope::default()
        },
        capture,
    )
    .unwrap();
    let mut saved = Vec::new();
    write_snapshot(&mut saved, &snapshot, &AtomicBool::new(false)).unwrap();
    assert_eq!(saved, bytes); // Both writers use the same actual encoder.
    let selected = doc
        .select(&[0, 2, 4], serde_json::json!({"selection":[2,6,10]}))
        .unwrap();
    let mut saved = Vec::new();
    write_snapshot(&mut saved, &selected, &AtomicBool::new(false)).unwrap();
    let reopened = load(
        Cursor::new(saved),
        16 * 1024 * 1024,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(reopened.end.scope.source_record_count, 5);
    assert_eq!(reopened.end.capture.known_gaps, doc.end.capture.known_gaps);
    assert_eq!(reopened.end.record_count, 3);
    let empty = StreamEncoder::new(header).unwrap();
    let bytes = [
        empty.header_bytes().unwrap(),
        empty.finish_bytes(CaptureStatus::default()).unwrap(),
    ]
    .concat();
    assert_eq!(
        load(
            Cursor::new(bytes),
            16 * 1024 * 1024,
            &AtomicBool::new(false)
        )
        .unwrap()
        .end
        .record_count,
        0
    );
}

#[test]
fn native_versions_cannot_mix_header_data_or_footer_and_v2_never_guesses_compact() {
    let v2 = super::proto_tests::sample_for_stream_tests();
    let v3 = v2
        .convert_format(
            NativeFormat::CompactV3,
            usize::MAX,
            u64::MAX,
            &AtomicBool::new(false),
        )
        .unwrap();
    for (doc, other) in [(&v2, &v3), (&v3, &v2)] {
        let mut row: Value = serde_json::from_slice(
            &encode_record(&doc.header, &doc.entries[0], &doc.record_metadata[0]).unwrap(),
        )
        .unwrap();
        row["schema_version"] = other.header.schema_version.into();
        let line = encode_line(&row).unwrap();
        assert!(decode_record(&line, &doc.header, 0).is_err());
        let bytes = [
            encode_line(&doc.header).unwrap(),
            line,
            encode_line(&doc.end).unwrap(),
        ]
        .concat();
        assert_eq!(
            load(
                Cursor::new(bytes),
                16 * 1024 * 1024,
                &AtomicBool::new(false)
            )
            .err()
            .unwrap()
            .line,
            2
        );
        let mut end = doc.end.clone();
        end.schema_version = other.header.schema_version;
        assert!(decode_end(&encode_line(&end).unwrap(), &doc.header).is_err());
        assert!(validate_end_summary(&doc.header, &end, 1, Some(1), Some(1)).is_err());
        let bytes = [
            encode_line(&doc.header).unwrap(),
            encode_record(&doc.header, &doc.entries[0], &doc.record_metadata[0]).unwrap(),
            encode_line(&end).unwrap(),
        ]
        .concat();
        assert_eq!(
            load(
                Cursor::new(bytes),
                16 * 1024 * 1024,
                &AtomicBool::new(false)
            )
            .err()
            .unwrap()
            .line,
            3
        );
    }
    let mut compact: Value = serde_json::from_slice(
        &encode_record(&v3.header, &v3.entries[0], &v3.record_metadata[0]).unwrap(),
    )
    .unwrap();
    compact["schema_version"] = 2.into();
    assert!(decode_record(&encode_line(&compact).unwrap(), &v2.header, 0).is_err());
    let mut invalid = v3.header.clone();
    invalid
        .normalization
        .insert("proto_projection".into(), "unknown".into());
    assert!(StreamEncoder::new(invalid.clone()).is_err());
    assert!(
        decode_record(
            &encode_record(&v3.header, &v3.entries[0], &v3.record_metadata[0]).unwrap(),
            &invalid,
            0
        )
        .is_err()
    );
}

#[test]
fn compact_preserves_legacy_unknowns_and_writer_revalidates_mutable_evidence() {
    let cancel = AtomicBool::new(false);
    let legacy = load(Cursor::new(br#"{"schema_version":1,"type":"entry","seq":9,"time":"old","pid":1,"tid":2,"level":"I","tag":"T","message":"m","future":true}"#), 16*1024*1024, &cancel).unwrap();
    let compact = legacy
        .convert_format(NativeFormat::CompactV3, 16 * 1024 * 1024, u64::MAX, &cancel)
        .unwrap();
    let mut bytes = Vec::new();
    write_snapshot(&mut bytes, &compact, &cancel).unwrap();
    let reopened = load(Cursor::new(bytes), 16 * 1024 * 1024, &cancel).unwrap();
    assert_eq!(reopened.header.source.kind, "legacy_v1");
    assert!(reopened.header.session_id.is_none());
    assert!(reopened.entries[0].provenance.is_none());
    assert!(reopened.entries[0].text_loss.is_none());
    assert!(reopened.entries[0].proto.is_none());
    assert_eq!(reopened.record_metadata[0].legacy_original_seq, Some(9));
    assert_eq!(
        reopened.record_metadata[0].extra["legacy_extensions"]["future"],
        true
    );

    let source = super::proto_tests::sample_for_stream_tests();
    let compact = source
        .convert_format(NativeFormat::CompactV3, usize::MAX, u64::MAX, &cancel)
        .unwrap();
    let mut stream = StreamEncoder::new(compact.header.clone()).unwrap();
    let initial_footer = stream.finish_bytes(CaptureStatus::default()).unwrap();
    let mut forged = compact.entries[0].clone();
    Arc::make_mut(forged.proto.as_mut().unwrap()).raw_payload = vec![];
    assert!(stream.record_bytes(&forged).is_err());
    assert_eq!(
        stream.finish_bytes(CaptureStatus::default()).unwrap(),
        initial_footer
    );
    forged = compact.entries[0].clone();
    forged.message = "forged".into();
    assert!(stream.record_bytes(&forged).is_err());
    assert_eq!(
        stream.finish_bytes(CaptureStatus::default()).unwrap(),
        initial_footer
    );
    cancel.store(true, Ordering::Release);
    assert_eq!(
        compact
            .convert_format(NativeFormat::V2, usize::MAX, u64::MAX, &cancel)
            .err()
            .unwrap()
            .kind,
        DocumentErrorKind::Cancelled
    );
}
