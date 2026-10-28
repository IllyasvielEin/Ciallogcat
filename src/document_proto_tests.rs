use super::*;
use crate::protobuf::DecodedEntry;
use crate::session::Session;
use std::io::Cursor;

const PAYLOAD: &[u8] =
    b"\x08\x01\x10\x02\x18\x08\x20\x2a\x28\x04\x30\x05\x3a\x04Tag\0\x42\x04m\0\xff\n\x4a\x03app";
const BUDGET: usize = 16 * 1024 * 1024;

pub(super) fn sample_for_stream_tests() -> Document {
    sample(PAYLOAD.to_vec())
}

fn sample(payload: Vec<u8>) -> Document {
    let mut entry = DecodedEntry::decode_payload(payload).unwrap().project();
    let mut session = Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    entry.provenance = session.next_record(Some(123));
    let mut store = LogStore::default();
    store.push(entry);
    let mut header = DocumentHeader::live(session.id(), "serial".into(), vec![LogBuffer::Main]);
    header.source.transport_format = Some("logcat_proto_experimental".into());
    header.source.requested_buffers_scope = Some("capture_configuration".into());
    header
        .normalization
        .insert("protobuf_raw_payload".into(), "preserved_base64".into());
    Document::from_snapshot(
        header,
        Arc::new(store),
        ExportScope::default(),
        CaptureStatus::default(),
    )
    .unwrap()
}

fn encode(doc: &Document) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_snapshot(&mut bytes, doc, &AtomicBool::new(false)).unwrap();
    bytes
}

fn wire(doc: &Document) -> Value {
    serde_json::to_value(WireRecord::from_row(
        &doc.entries[0],
        &doc.record_metadata[0],
    ))
    .unwrap()
}

#[test]
fn proto_native_roundtrip_preserves_raw_bytes_unknown_fields_and_attribution() {
    let mut payload = PAYLOAD.to_vec();
    payload.extend_from_slice(&[0x98, 0x06, 1]);
    let mut doc = sample(payload.clone());
    doc.record_metadata[0]
        .proto_extra
        .insert("future".into(), serde_json::json!({"nested": [1, "x"]}));
    let bytes = encode(&doc);
    let loaded = load(Cursor::new(&bytes), BUDGET, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        loaded.entries[0].proto.as_ref().unwrap().raw_payload,
        payload
    );
    assert_eq!(
        loaded.entries[0].proto.as_ref().unwrap().tag_bytes,
        b"Tag\0"
    );
    assert_eq!(
        loaded.entries[0].proto.as_ref().unwrap().message_bytes,
        b"m\0\xff\n"
    );
    assert_eq!(
        loaded.entries[0].process_source,
        Some(ProcessSource::DeviceProcessCache)
    );
    assert_eq!(
        loaded.record_metadata[0].proto_extra,
        doc.record_metadata[0].proto_extra
    );
    assert_eq!(encode(&loaded), bytes);
    assert!(wire(&doc)["proto"]["raw_payload"].is_string());
    assert_eq!(wire(&doc)["process"]["source"], "device_process_cache");
}

#[test]
fn proto_native_checks_every_projection_and_rejects_forged_display_fields() {
    let doc = sample(PAYLOAD.to_vec());
    let original = wire(&doc);
    for field in ["time_sec", "time_nsec", "uid", "pid", "tid", "priority"] {
        let mut changed = original.clone();
        changed["proto"][field] = 99.into();
        let error =
            decode_record(&serde_json::to_vec(&changed).unwrap(), &doc.header, 0).unwrap_err();
        assert!(
            error.message.contains("explicit fields disagree"),
            "{field}: {error}"
        );
    }
    for field in ["tag_bytes", "message_bytes", "raw_payload", "process_name"] {
        let mut changed = original.clone();
        changed["proto"][field] = "".into();
        assert!(
            decode_record(&serde_json::to_vec(&changed).unwrap(), &doc.header, 0).is_err(),
            "{field}"
        );
    }
    for (field, value) in [
        ("pid", Value::from(8)),
        ("tid", Value::from(9)),
        ("level", Value::from("I")),
        ("tag", Value::from("other")),
        ("message", Value::from("other")),
    ] {
        let mut changed = original.clone();
        changed[field] = value;
        assert!(
            decode_record(&serde_json::to_vec(&changed).unwrap(), &doc.header, 0).is_err(),
            "{field}"
        );
    }
    let mut changed = original.clone();
    changed["process"]["source"] = "ps_snapshot".into();
    assert!(decode_record(&serde_json::to_vec(&changed).unwrap(), &doc.header, 0).is_err());
    changed = original;
    changed["proto"]["raw_payload"] = "not base64!".into();
    assert!(decode_record(&serde_json::to_vec(&changed).unwrap(), &doc.header, 0).is_err());
    let duplicate = serde_json::to_string(&wire(&doc))
        .unwrap()
        .replace("\"uid\":42", "\"uid\":42,\"uid\":43");
    assert!(decode_record(duplicate.as_bytes(), &doc.header, 0).is_err());
}

#[test]
fn proto_native_preserves_unknown_priorities_and_empty_or_unknown_only_frames() {
    let mut unknown_priority = PAYLOAD.to_vec();
    unknown_priority[5] = 7;
    for payload in [unknown_priority, vec![], vec![0x98, 6, 1]] {
        let doc = sample(payload.clone());
        let loaded = load(Cursor::new(encode(&doc)), BUDGET, &AtomicBool::new(false)).unwrap();
        assert_eq!(loaded.entries[0].kind, LogEntryKind::Unparsed);
        assert_eq!(loaded.entries[0].process_source, None);
        assert_eq!(
            loaded.entries[0].proto.as_ref().unwrap().raw_payload,
            payload
        );
        assert_eq!(wire(&loaded)["pid"], Value::Null);
        assert_eq!(wire(&loaded)["level"], Value::Null);
    }
}

#[test]
fn proto_native_writer_revalidates_payload_and_bounds_before_encoding() {
    let doc = sample(PAYLOAD.to_vec());
    let mut row = doc.entries[0].clone();
    Arc::make_mut(row.proto.as_mut().unwrap()).uid = 43;
    let mut encoder = StreamEncoder::new(doc.header.clone()).unwrap();
    assert!(
        encoder
            .record_bytes(&row)
            .unwrap_err()
            .message
            .contains("explicit fields disagree")
    );
    row = doc.entries[0].clone();
    Arc::make_mut(row.proto.as_mut().unwrap()).raw_payload =
        vec![0; crate::protobuf::MAX_FRAME_BYTES as usize + 1];
    assert!(
        encoder
            .record_bytes(&row)
            .unwrap_err()
            .message
            .contains("1 MiB")
    );
    row = doc.entries[0].clone();
    row.proto = None;
    assert!(
        encoder
            .record_bytes(&row)
            .unwrap_err()
            .message
            .contains("lacks proto evidence")
    );
    let mut metadata = doc.record_metadata[0].clone();
    metadata.proto_extra.insert("uid".into(), 1.into());
    assert!(
        validate_row(&doc.header, &doc.entries[0], &metadata, 0)
            .unwrap_err()
            .message
            .contains("reserved")
    );
    metadata.proto_extra.clear();
    metadata.process_source = Some("ps_snapshot".into());
    assert!(validate_row(&doc.header, &doc.entries[0], &metadata, 0).is_err());
    metadata.process_source = Some("device_process_cache".into());
    row.process_source = None;
    assert!(validate_row(&doc.header, &row, &metadata, 0).is_err());
}

#[test]
fn proto_native_header_accepts_only_documented_transport_and_scope_vocabulary() {
    let doc = sample(PAYLOAD.to_vec());
    for scope in ["snapshot_configuration", "capture_configuration"] {
        for format in ["threadtime", "logcat_proto_experimental"] {
            let mut header = doc.header.clone();
            header.source.requested_buffers_scope = Some(scope.into());
            header.source.transport_format = Some(format.into());
            validate_header(&header).unwrap();
        }
    }
    let mut header = doc.header.clone();
    header.source.requested_buffers_scope = Some("every_record_actual_buffer".into());
    assert!(validate_header(&header).is_err());
    header = doc.header.clone();
    header.source.transport_format = Some("guessed_proto".into());
    assert!(validate_header(&header).is_err());
}

#[test]
fn proto_snapshot_selection_load_and_save_cancel_between_records_without_publication() {
    let mut payload = PAYLOAD[..18].to_vec();
    payload.extend_from_slice(&[0x42, 0x80, 0x80, 0x10]); // 256 KiB message
    payload.extend(std::iter::repeat_n(b'x', 256 * 1024));
    payload.extend_from_slice(&[0x4a, 3, b'a', b'p', b'p']);
    let seed = sample(payload);
    let count = 64;
    let mut entries = LogStore::default();
    for index in 0..count {
        let mut entry = seed.entries[0].clone();
        entry.provenance.as_mut().unwrap().seq = index as u64 + 1;
        entries.push(entry);
    }
    let entries = Arc::new(entries);
    let document = Document::from_snapshot(
        seed.header.clone(),
        Arc::clone(&entries),
        ExportScope::default(),
        CaptureStatus::default(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    CANCEL_AFTER_CHECKS.set(Some(4));
    let result = Document::from_snapshot_cancellable(
        seed.header,
        entries,
        ExportScope::default(),
        CaptureStatus::default(),
        &cancel,
    );
    assert!(matches!(
        result,
        Err(DocumentError {
            kind: DocumentErrorKind::Cancelled,
            ..
        })
    ));
    cancel.store(false, Ordering::Release);
    CANCEL_AFTER_CHECKS.set(Some(4));
    let result = document.select_cancellable(
        &(0..count).collect::<Vec<_>>(),
        serde_json::json!({"query":"x"}),
        &cancel,
    );
    assert!(matches!(
        result,
        Err(DocumentError {
            kind: DocumentErrorKind::Cancelled,
            ..
        })
    ));

    // Header + records + footer + EOF + post-loop + validation entry, then
    // cancel on validation's third proto record. No elapsed-time assumption.
    let bytes = encode(&document);
    cancel.store(false, Ordering::Release);
    CANCEL_AFTER_CHECKS.set(Some(count + 8));
    let result = load(Cursor::new(bytes), 256 * 1024 * 1024, &cancel);
    assert!(matches!(
        result,
        Err(DocumentError {
            kind: DocumentErrorKind::Cancelled,
            ..
        })
    ));
    assert!(cancel.load(Ordering::Acquire));

    let directory =
        std::env::temp_dir().join(format!("ciallogcat-proto-cancel-{}", SessionId::new()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("never-published.jsonl");
    cancel.store(false, Ordering::Release);
    CANCEL_AFTER_CHECKS.set(Some(5)); // During pre-write validation, before encoding.
    let result = crate::file_io::write_new_file(&path, &cancel, |writer| {
        write_snapshot(writer, &document, &cancel).map_err(std::io::Error::other)
    });
    assert!(result.is_err());
    assert!(cancel.load(Ordering::Acquire));
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    std::fs::remove_dir(directory).unwrap();
}

fn compact(doc: &Document) -> Document {
    doc.convert_format(
        NativeFormat::CompactV3,
        usize::MAX,
        u64::MAX,
        &AtomicBool::new(false),
    )
    .unwrap()
}

fn compact_wire(doc: &Document) -> Value {
    serde_json::from_slice(
        &encode_record(&doc.header, &doc.entries[0], &doc.record_metadata[0]).unwrap(),
    )
    .unwrap()
}

#[test]
fn compact_wire_goldens_project_exact_bytes_and_classification_without_loss_invention() {
    // All are hand-written wire bytes, independent of prost's encoder. The
    // overlong varint and duplicate pid/message occurrences must also survive.
    let mut duplicate = PAYLOAD.to_vec();
    duplicate.extend_from_slice(b"\x28\x86\x00\x42\x02z\0\x98\x06\x01");
    let mut uid_zero = PAYLOAD.to_vec();
    uid_zero[7] = 0;
    let mut unknown = PAYLOAD.to_vec();
    unknown[5] = 7;
    let mut default = PAYLOAD.to_vec();
    default[5] = 1;
    let mut silent = PAYLOAD.to_vec();
    silent[5] = 9;
    let mut bad_time = PAYLOAD.to_vec();
    bad_time.extend_from_slice(b"\x10\x80\x94\xeb\xdc\x03"); // nsec = 1e9
    let mut big_pid = PAYLOAD.to_vec();
    big_pid.extend_from_slice(b"\x28\x80\x80\x80\x80\x10"); // u32::MAX + 1
    let mut invalid_tag = PAYLOAD.to_vec();
    invalid_tag.extend_from_slice(b"\x3a\x04\xffT\0\0\x4a\x03 p\n");
    for (payload, kind, message) in [
        (PAYLOAD.to_vec(), LogEntryKind::Log, "m\0�\n"),
        (duplicate, LogEntryKind::Log, "z\0"),
        (uid_zero, LogEntryKind::Log, "m\0�\n"),
        (unknown, LogEntryKind::Unparsed, "m\0�\n"),
        (default, LogEntryKind::Unparsed, "m\0�\n"),
        (silent, LogEntryKind::Unparsed, "m\0�\n"),
        (bad_time, LogEntryKind::Unparsed, "m\0�\n"),
        (big_pid, LogEntryKind::Unparsed, "m\0�\n"),
        (invalid_tag, LogEntryKind::Log, "m\0�\n"),
        (vec![], LogEntryKind::Unparsed, ""),
        (vec![0x98, 6, 1], LogEntryKind::Unparsed, ""),
    ] {
        let v2 = sample(payload.clone());
        let v3 = compact(&v2);
        let value = compact_wire(&v3);
        for key in [
            "device_time",
            "pid",
            "tid",
            "level",
            "tag",
            "message",
            "process",
        ] {
            assert!(value.get(key).is_none(), "{key}");
        }
        assert_eq!(value["proto"].as_object().unwrap().len(), 1);
        let loaded = load(Cursor::new(encode(&v3)), BUDGET, &AtomicBool::new(false)).unwrap();
        assert_eq!(loaded.origin, DocumentOrigin::NativeV3);
        assert_eq!(loaded.entries[0].kind, kind);
        assert_eq!(loaded.entries[0].message.as_ref(), message);
        assert_eq!(
            loaded.entries[0].proto.as_ref().unwrap().raw_payload,
            payload
        );
        let back = loaded
            .convert_format(NativeFormat::V2, BUDGET, u64::MAX, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(wire(&back), wire(&v2));
        let reloaded = load(Cursor::new(encode(&back)), BUDGET, &AtomicBool::new(false)).unwrap();
        assert_eq!(wire(&reloaded), wire(&v2));
    }
    let mut value = compact_wire(&compact(&sample(PAYLOAD.to_vec())));
    value["text_loss"] = Value::Null;
    let header = compact(&sample(PAYLOAD.to_vec())).header;
    // Use the matching session identity, without fabricating a no-loss fact.
    value["provenance"]["session_id"] = serde_json::to_value(header.session_id).unwrap();
    let (row, _) = decode_record(&serde_json::to_vec(&value).unwrap(), &header, 0).unwrap();
    assert!(row.text_loss.is_none());
}

#[test]
fn compact_conversion_preserves_nested_extensions_scope_gaps_and_original_identity() {
    let mut v2 = sample(PAYLOAD.to_vec());
    for extensions in [
        &mut v2.header.extra,
        &mut v2.header.source.extra,
        &mut v2.header.normalization,
        &mut v2.end.extra,
        &mut v2.end.scope.extra,
        &mut v2.end.capture.extra,
    ] {
        extensions.insert("future".into(), serde_json::json!({"list":[1,"未知"]}));
    }
    let meta = &mut v2.record_metadata[0];
    for extensions in [
        &mut meta.extra,
        &mut meta.device_time_extra,
        &mut meta.process_extra,
        &mut meta.provenance_extra,
        &mut meta.text_loss_extra,
        &mut meta.proto_extra,
    ] {
        extensions.insert(
            "future".into(),
            serde_json::json!({"type":"extension","values":[true,null]}),
        );
    }
    v2.end
        .capture
        .known_gaps
        .push(serde_json::json!({"kind":"interruption","after_seq":0}));
    v2.end.capture.completeness = "partial".into();
    v2.end.scope.omitted_before = Some(42);
    let selected = v2.select(&[0], serde_json::json!({"tag":"Tag"})).unwrap();
    let v3 = compact(&selected);
    assert_eq!(
        v3.header.derived_from_artifact,
        Some(selected.header.artifact_id)
    );
    assert_ne!(v3.header.artifact_id, selected.header.artifact_id);
    let value = compact_wire(&v3);
    assert_eq!(
        value["projection_extensions"]["device_time"]["future"],
        wire(&v2)["device_time"]["future"]
    );
    let loaded = load(Cursor::new(encode(&v3)), BUDGET, &AtomicBool::new(false)).unwrap();
    let filtered = loaded
        .select(&[0], serde_json::json!({"query":"m"}))
        .unwrap();
    assert_eq!(filtered.header.schema_version, 3);
    assert_eq!(filtered.end.schema_version, 3);
    assert_eq!(
        filtered.end.scope.filter,
        Some(serde_json::json!({"all_of":[{"tag":"Tag"},{"query":"m"}]}))
    );
    let back = loaded
        .convert_format(NativeFormat::V2, BUDGET, u64::MAX, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(wire(&back), wire(&v2));
    assert_eq!(
        serde_json::to_value(&back.end.scope).unwrap(),
        serde_json::to_value(&selected.end.scope).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&back.end.capture).unwrap(),
        serde_json::to_value(&selected.end.capture).unwrap()
    );
    assert_eq!(back.header.extra, v2.header.extra);
    assert_eq!(back.header.source.extra, v2.header.source.extra);
    assert_eq!(back.header.normalization, v2.header.normalization);
    assert_eq!(back.end.extra, v2.end.extra);
}

#[test]
fn compact_rejects_redundant_fields_even_null_unknown_profiles_and_duplicate_core_keys() {
    let doc = compact(&sample(PAYLOAD.to_vec()));
    let original = compact_wire(&doc);
    for key in [
        "device_time",
        "pid",
        "tid",
        "level",
        "tag",
        "message",
        "process",
    ] {
        let mut value = original.clone();
        value[key] = Value::Null;
        assert!(
            decode_record(&serde_json::to_vec(&value).unwrap(), &doc.header, 0).is_err(),
            "{key}"
        );
    }
    for key in [
        "time_sec",
        "time_nsec",
        "priority",
        "uid",
        "pid",
        "tid",
        "tag_bytes",
        "message_bytes",
        "process_name",
    ] {
        let mut value = original.clone();
        value["proto"][key] = Value::Null;
        assert!(
            decode_record(&serde_json::to_vec(&value).unwrap(), &doc.header, 0).is_err(),
            "{key}"
        );
    }
    for (path, core) in [
        ("device_time", "text"),
        ("process", "name"),
        ("process", "source"),
    ] {
        let mut value = original.clone();
        value["projection_extensions"] = serde_json::json!({path:{core:"forged"}});
        assert!(decode_record(&serde_json::to_vec(&value).unwrap(), &doc.header, 0).is_err());
    }
    for (before, after) in [
        ("\"seq\":1", "\"seq\":1,\"seq\":1"),
        ("\"raw_payload\":", "\"raw_payload\":\"\",\"raw_payload\":"),
        (
            "\"truncated\":false",
            "\"truncated\":false,\"truncated\":false",
        ),
        (
            "\"type\":\"entry\"",
            "\"type\":\"entry\",\"type\":\"entry\"",
        ),
    ] {
        let original = original.to_string();
        let duplicate = original.replace(before, after);
        assert_ne!(original, duplicate);
        assert!(
            decode_record(duplicate.as_bytes(), &doc.header, 0).is_err(),
            "{before}"
        );
    }
    for key in [
        "record_encoding",
        "proto_projection",
        "protobuf_raw_payload",
    ] {
        let mut header = serde_json::to_value(&doc.header).unwrap();
        let parent = if key == "record_encoding" {
            &mut header
        } else {
            &mut header["normalization"]
        };
        parent[key] = "future_profile".into();
        assert!(decode_header(&serde_json::to_vec(&header).unwrap()).is_err());
        parent_remove_key(&mut header, key);
        assert!(decode_header(&serde_json::to_vec(&header).unwrap()).is_err());
        let encoded = serde_json::to_string(&doc.header).unwrap();
        let duplicate = encoded.replace(
            &format!("\"{key}\":"),
            &format!("\"{key}\":\"fake\",\"{key}\":"),
        );
        assert!(decode_header(duplicate.as_bytes()).is_err());
    }
}

fn parent_remove_key(header: &mut Value, key: &str) {
    let parent = if key == "record_encoding" {
        header
    } else {
        &mut header["normalization"]
    };
    parent.as_object_mut().unwrap().remove(key);
}

#[test]
fn compact_conversion_validates_v2_before_dropping_fields_and_never_overwrites_extensions() {
    let seed = sample(PAYLOAD.to_vec());
    let check = |doc: &Document| {
        assert!(
            doc.convert_format(
                NativeFormat::CompactV3,
                BUDGET,
                u64::MAX,
                &AtomicBool::new(false)
            )
            .is_err()
        )
    };
    let mut invalid = seed.clone();
    let mut row = invalid.entries[0].clone();
    row.message = "forged".into();
    let mut store = LogStore::default();
    store.push(row);
    invalid.entries = Arc::new(store);
    check(&invalid);
    for key in [
        "record_encoding",
        "proto_projection",
        "projection_extensions",
    ] {
        let mut conflicting = seed.clone();
        match key {
            "record_encoding" => &mut conflicting.header.extra,
            "proto_projection" => &mut conflicting.header.normalization,
            _ => &mut conflicting.record_metadata[0].extra,
        }
        .insert(key.into(), serde_json::json!({"old_unknown":true}));
        // Remains a valid v2 extension, but conversion may not overwrite it.
        load(
            Cursor::new(encode(&conflicting)),
            BUDGET,
            &AtomicBool::new(false),
        )
        .unwrap();
        check(&conflicting);
    }
    let mut v3 = compact(&seed);
    v3.record_metadata[0]
        .projection_extra
        .insert("future_namespace".into(), serde_json::json!({"x":1}));
    let loaded = load(Cursor::new(encode(&v3)), BUDGET, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        loaded.record_metadata[0].projection_extra,
        v3.record_metadata[0].projection_extra
    );
    let back = loaded
        .convert_format(NativeFormat::V2, BUDGET, u64::MAX, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(
        wire(&back)["projection_extensions"]["future_namespace"]["x"],
        1
    );
    check(&back); // Same-name v2 extension remains opaque; no silent guessing.
}

#[test]
fn compact_bad_payloads_classifications_and_bounded_base64_fail_with_location() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let doc = compact(&sample(PAYLOAD.to_vec()));
    let original = compact_wire(&doc);
    for raw in [
        Value::Null,
        Value::from("!"),
        Value::from("YQ"),
        Value::from("AA=="),
        Value::from(STANDARD.encode([0x4a, 1, 0xff])), // string UTF-8 must still reject
        Value::from(STANDARD.encode([0x80])),
        Value::from("A".repeat((crate::protobuf::MAX_FRAME_BYTES as usize).div_ceil(3) * 4 + 4)),
        Value::from(STANDARD.encode(vec![0; crate::protobuf::MAX_FRAME_BYTES as usize + 1])),
    ] {
        let mut value = original.clone();
        value["proto"]["raw_payload"] = raw;
        let header = encode_line(&doc.header).unwrap();
        let bytes = [
            header.clone(),
            encode_line(&value).unwrap(),
            encode_line(&doc.end).unwrap(),
        ]
        .concat();
        let error = load(
            Cursor::new(bytes),
            256 * 1024 * 1024,
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert_eq!((error.line, error.offset), (2, header.len() as u64));
    }
    let mut missing = original.clone();
    missing["proto"]
        .as_object_mut()
        .unwrap()
        .remove("raw_payload");
    assert!(decode_record(&serde_json::to_vec(&missing).unwrap(), &doc.header, 0).is_err());
    for kind in ["event", "unparsed"] {
        let mut value = original.clone();
        value["type"] = kind.into();
        assert!(decode_record(&serde_json::to_vec(&value).unwrap(), &doc.header, 0).is_err());
    }
    let mut value = original.clone();
    value["text_loss"]["truncated"] = true.into();
    assert!(decode_record(&serde_json::to_vec(&value).unwrap(), &doc.header, 0).is_err());
    value = original;
    value["text_loss"]["invalid_utf8_replaced"] = false.into();
    assert!(decode_record(&serde_json::to_vec(&value).unwrap(), &doc.header, 0).is_err());
    let unparsed = compact(&sample(vec![]));
    for parent in ["process", "device_time"] {
        let mut value = compact_wire(&unparsed);
        value["projection_extensions"] = serde_json::json!({parent:{}});
        assert!(decode_record(&serde_json::to_vec(&value).unwrap(), &unparsed.header, 0).is_err());
    }
}

#[test]
fn compact_large_nul_payload_fits_v3_but_v2_export_fails_instead_of_truncating() {
    let length = crate::protobuf::MAX_FRAME_BYTES as usize - 6;
    let mut payload = vec![
        0x18,
        8,
        0x42,
        (length as u8) | 0x80,
        ((length >> 7) as u8) | 0x80,
        (length >> 14) as u8,
    ];
    payload.resize(crate::protobuf::MAX_FRAME_BYTES as usize, 0);
    let v3 = compact(&sample(payload.clone()));
    let bytes = encode(&v3);
    assert!(bytes.len() < 2 * 1024 * 1024);
    let loaded = load(
        Cursor::new(&bytes),
        256 * 1024 * 1024,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(loaded.entries[0].message.len(), length);
    assert_eq!(
        loaded.entries[0].proto.as_ref().unwrap().raw_payload,
        payload
    );
    assert!(loaded.memory_bytes() > bytes.len() * 2);
    let error = loaded
        .convert_format(
            NativeFormat::V2,
            usize::MAX,
            u64::MAX,
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
    assert!(error.message.contains("8 MiB"), "{error}");
    let error = loaded
        .convert_format(
            NativeFormat::CompactV3,
            loaded.memory_bytes() - 1,
            u64::MAX,
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
    assert_eq!(error.kind, DocumentErrorKind::MemoryBudget);
    assert!(
        load(
            Cursor::new(bytes),
            loaded.memory_bytes() - 1,
            &AtomicBool::new(false)
        )
        .is_err()
    );
    let small = compact(&sample(PAYLOAD.to_vec()));
    let back = small
        .convert_format(NativeFormat::V2, BUDGET, u64::MAX, &AtomicBool::new(false))
        .unwrap();
    let exact = encode(&back).len() as u64;
    assert!(
        small
            .convert_format(NativeFormat::V2, BUDGET, exact, &AtomicBool::new(false))
            .is_ok()
    );
    assert!(
        small
            .convert_format(NativeFormat::V2, BUDGET, exact - 1, &AtomicBool::new(false))
            .is_err()
    );
}
