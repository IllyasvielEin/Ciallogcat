use super::*;
use std::cell::Cell;
use std::io::Cursor;
use std::rc::Rc;

const BUDGET: usize = 32 * 1024 * 1024;
const TOKEN_MARKER: &str = "__raw_numeric_token__";
const NUMBERS: &[&str] = &[
    "1e-400",
    "18446744073709551617",
    "-18446744073709551617",
    "1e+400",
    "0.123456789012345678901234567890123456789",
    "1.2300e+004",
];

fn encode(doc: &Document) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_snapshot(&mut bytes, doc, &AtomicBool::new(false)).unwrap();
    bytes
}

fn reopen(doc: &Document) -> Document {
    load(Cursor::new(encode(doc)), BUDGET, &AtomicBool::new(false)).unwrap()
}

fn extension_maps(doc: &Document) -> Vec<&Extensions> {
    let meta = &doc.record_metadata[0];
    vec![
        &doc.header.extra,
        &doc.header.source.extra,
        &doc.header.normalization,
        &meta.extra,
        &meta.proto_extra,
        &meta.device_time_extra,
        &meta.process_extra,
        &meta.provenance_extra,
        &meta.text_loss_extra,
        &doc.end.extra,
        &doc.end.scope.extra,
        &doc.end.capture.extra,
    ]
}

fn numeric_fixture(token: &str) -> Vec<u8> {
    let mut doc = super::proto_tests::sample_for_stream_tests();
    let meta = &mut doc.record_metadata[0];
    for extra in [
        &mut doc.header.extra,
        &mut doc.header.source.extra,
        &mut doc.header.normalization,
        &mut meta.extra,
        &mut meta.proto_extra,
        &mut meta.device_time_extra,
        &mut meta.process_extra,
        &mut meta.provenance_extra,
        &mut meta.text_loss_extra,
        &mut doc.end.extra,
        &mut doc.end.scope.extra,
        &mut doc.end.capture.extra,
    ] {
        extra.insert("future".into(), TOKEN_MARKER.into());
        extra.insert(
            "future_tree".into(),
            serde_json::json!([TOKEN_MARKER, {"nested": TOKEN_MARKER}]),
        );
    }
    meta.projection_extra
        .insert("future".into(), TOKEN_MARKER.into());
    doc.end.scope.kind = "filtered".into();
    doc.end.scope.filter = Some(serde_json::json!({"future":TOKEN_MARKER}));
    doc.end.capture.known_gaps = vec![serde_json::json!({"future":TOKEN_MARKER})];
    // Inject literal tokens into JSON bytes, never parse the expected numbers
    // through the same Value implementation being tested.
    String::from_utf8(encode(&doc))
        .unwrap()
        .replace(&format!("\"{TOKEN_MARKER}\""), token)
        .into_bytes()
}

fn assert_numbers(doc: &Document, token: &str) {
    for (layer, extra) in extension_maps(doc).iter().enumerate() {
        assert_eq!(extra["future"].to_string(), token, "layer {layer}");
        assert_eq!(extra["future_tree"][0].to_string(), token, "layer {layer}");
        assert_eq!(
            extra["future_tree"][1]["nested"].to_string(),
            token,
            "layer {layer}"
        );
    }
    assert_eq!(
        doc.end.scope.filter.as_ref().unwrap()["future"].to_string(),
        token
    );
    assert_eq!(doc.end.capture.known_gaps[0]["future"].to_string(), token);
    let projection = encode_record_projection(
        &doc.header,
        &doc.entries[0],
        &doc.record_metadata[0],
        BUDGET,
    )
    .unwrap();
    // Check the emitted text too, so a serializer cannot silently round even
    // when the in-memory representation retained the original token.
    assert!(
        String::from_utf8(projection)
            .unwrap()
            .contains(&format!("\"future\":{token}"))
    );
}

#[test]
fn numeric_extensions_survive_all_flatten_layers_load_select_and_reopen() {
    for token in NUMBERS {
        let source = load(
            Cursor::new(numeric_fixture(token)),
            BUDGET,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_numbers(&source, token);
        let selected = source
            .select(&[0], serde_json::json!({"tag":"Tag"}))
            .unwrap();
        assert_eq!(
            selected.end.scope.filter.as_ref().unwrap()["all_of"][0]["future"].to_string(),
            *token
        );
        for extra in extension_maps(&reopen(&selected)) {
            assert_eq!(extra["future"].to_string(), *token);
        }
        let reopened = reopen(&source);
        assert_numbers(&reopened, token);
        assert_eq!(
            reopened.record_metadata[0].projection_extra["future"].to_string(),
            *token
        );
        assert_eq!(
            reopened.entries[0].proto.as_ref().unwrap().raw_payload,
            source.entries[0].proto.as_ref().unwrap().raw_payload
        );
    }
}

#[test]
fn numeric_text_extensions_survive_repeated_reopen() {
    for token in NUMBERS {
        let mut native = super::tests::sample();
        let number: Value = serde_json::from_str(token).unwrap();
        native.record_metadata[0]
            .extra
            .insert("future".into(), number.clone());
        native.record_metadata[0]
            .extra
            .insert("tree".into(), serde_json::json!([{ "n": number }]));
        let reopened = reopen(&native);
        let repeated = reopen(&reopened);
        for doc in [&native, &reopened, &repeated] {
            let extra = &doc.record_metadata[0].extra;
            assert_eq!(extra["future"].to_string(), *token);
            assert_eq!(extra["tree"][0]["n"].to_string(), *token);
        }
    }
}

#[test]
fn arbitrary_precision_private_number_objects_are_rejected_before_type_loss() {
    for key in [
        "$serde_json::private::Number",
        r"\u0024serde_json::private::Number",
        r"$serde_json::private::\u004eumber",
    ] {
        let object = format!(r#"{{"nested":[{{"{key}":"1e-400"}}]}}"#);
        {
            let template = numeric_fixture("1e-400");
            let original: Vec<Value> = template
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).unwrap())
                .collect();
            let header = decode_header(&serde_json::to_vec(&original[0]).unwrap()).unwrap();
            let mut layers = vec![
                (0, "/future"),
                (0, "/source/future"),
                (0, "/normalization/future"),
                (1, "/future"),
                (1, "/proto/future"),
                (1, "/provenance/future"),
                (1, "/text_loss/future"),
                (2, "/future"),
                (2, "/scope/future"),
                (2, "/scope/filter/future"),
                (2, "/capture/future"),
                (2, "/capture/known_gaps/0/future"),
            ];
            layers.extend([
                (1, "/projection_extensions/future"),
                (1, "/projection_extensions/device_time/future"),
                (1, "/projection_extensions/process/future"),
            ]);
            for (layer, path) in layers {
                // The dangerous object enters as raw JSON bytes, never as a
                // Value that serde_json could already have changed to Number.
                let changed = replace_token(&original[layer], path, &object);
                let direct = match layer {
                    0 => decode_header(&changed).map(|_| ()),
                    1 => decode_record(&changed, &header, 0).map(|_| ()),
                    _ => decode_end(&changed, &header).map(|_| ()),
                }
                .unwrap_err();
                assert!(
                    direct.message.contains("unsupported JSON object key"),
                    "{path}: {direct}"
                );
                let mut lines: Vec<Vec<u8>> = original
                    .iter()
                    .map(|v| serde_json::to_vec(v).unwrap())
                    .collect();
                lines[layer] = changed;
                let error = load(
                    Cursor::new(lines.join(&b'\n')),
                    BUDGET,
                    &AtomicBool::new(false),
                )
                .err()
                .unwrap();
                assert_eq!(error.kind, DocumentErrorKind::InvalidData);
                assert_eq!(error.line, layer as u64 + 1);
                assert!(
                    error.message.contains("unsupported JSON object key"),
                    "{error}"
                );
            }
        }
    }
    // Quoted text, escaped backslashes, and similar keys remain ordinary JSON.
    for input in [
        r#"{"future":"$serde_json::private::Number"}"#,
        r#"{"future":"{\"$serde_json::private::Number\":\"1\"}"}"#,
        r#"{"future":{"\\u0024serde_json::private::Number":"x"}}"#,
        r#"{"future":{"$serde_json::private::Numbers":"x"}}"#,
    ] {
        let value: Value = decode_line(input.as_bytes()).unwrap();
        assert_eq!(serde_json::to_string(&value).unwrap(), input);
    }
    let mut doc = super::proto_tests::sample_for_stream_tests();
    let object = Value::Object(
        [(
            String::from("$serde_json::private::Number"),
            "1e-400".into(),
        )]
        .into_iter()
        .collect(),
    );
    doc.record_metadata[0]
        .proto_extra
        .insert("future".into(), object);
    assert!(encode_record(&doc.header, &doc.entries[0], &doc.record_metadata[0]).is_err());
    assert!(
        encode_record_projection(
            &doc.header,
            &doc.entries[0],
            &doc.record_metadata[0],
            BUDGET
        )
        .is_err()
    );
}

fn replace_token(value: &Value, path: &str, token: &str) -> Vec<u8> {
    let mut value = value.clone();
    *value.pointer_mut(path).expect("existing core field") = TOKEN_MARKER.into();
    serde_json::to_string(&value)
        .unwrap()
        .replace(&format!("\"{TOKEN_MARKER}\""), token)
        .into_bytes()
}

#[test]
fn arbitrary_precision_does_not_coerce_core_integer_types_or_ranges() {
    let invalid = [
        "1e-400",
        "18446744073709551617",
        "-18446744073709551617",
        "1e+400",
        "1.0",
        "2e0",
        "\"2\"",
        "true",
        "[2]",
        "{\"n\":2}",
    ];
    for doc in [
        super::proto_tests::sample_for_stream_tests(),
        super::tests::sample(),
    ] {
        let header = serde_json::to_value(&doc.header).unwrap();
        let footer = serde_json::to_value(&doc.end).unwrap();
        let row: Value = serde_json::from_slice(
            &encode_record(&doc.header, &doc.entries[0], &doc.record_metadata[0]).unwrap(),
        )
        .unwrap();
        for token in invalid {
            assert!(decode_header(&replace_token(&header, "/schema_version", token)).is_err());
            for path in [
                "/schema_version",
                "/record_count",
                "/first_seq",
                "/last_seq",
                "/scope/source_record_count",
                "/scope/source_first_seq",
                "/scope/source_last_seq",
                "/capture/observed_unix_ms",
            ] {
                assert!(
                    decode_end(&replace_token(&footer, path, token), &doc.header).is_err(),
                    "footer {path}: {token}"
                );
            }
            for path in [
                "/schema_version",
                "/seq",
                "/provenance/seq",
                "/provenance/segment",
                "/provenance/attempt",
                "/provenance/observed_unix_ms",
            ] {
                assert!(
                    decode_record(&replace_token(&row, path, token), &doc.header, 0).is_err(),
                    "record {path}: {token}"
                );
            }
            if doc.entries[0].proto.is_none() {
                for path in ["/pid", "/tid"] {
                    assert!(
                        decode_record(&replace_token(&row, path, token), &doc.header, 0).is_err(),
                        "text record {path}: {token}"
                    );
                }
            }
        }
        for (path, token) in [
            ("/text_loss/truncated", "0"),
            ("/text_loss/invalid_utf8_replaced", "1"),
            ("/proto/raw_payload", "0"),
        ] {
            if row.pointer(path).is_some() {
                assert!(decode_record(&replace_token(&row, path, token), &doc.header, 0).is_err());
            }
        }
    }
}

#[test]
fn long_numeric_tokens_count_toward_retained_memory_and_load_budget() {
    let mut doc = super::proto_tests::sample_for_stream_tests();
    let before = doc.memory_bytes();
    let token = format!("0.{}1", "0".repeat(64 * 1024));
    let value: Value = serde_json::from_str(&token).unwrap();
    assert!(value_bytes(&value) >= token.len());
    doc.record_metadata[0]
        .proto_extra
        .insert("future".into(), value);
    assert!(doc.memory_bytes() >= before + token.len());
    let error = load(
        Cursor::new(encode(&doc)),
        before + token.len() / 2,
        &AtomicBool::new(false),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind, DocumentErrorKind::MemoryBudget);
    let back = reopen(&doc);
    assert_eq!(
        back.record_metadata[0].proto_extra["future"].to_string(),
        token
    );
}

struct CloneHook;

impl Drop for CloneHook {
    fn drop(&mut self) {
        RECORD_METADATA_CLONE_HOOK.with_borrow_mut(|hook| *hook = None);
    }
}

fn observe_clones(callback: impl FnMut() + 'static) -> CloneHook {
    RECORD_METADATA_CLONE_HOOK.with_borrow_mut(|hook| {
        assert!(hook.is_none());
        *hook = Some(Box::new(callback));
    });
    CloneHook
}

fn large_sidecar() -> Document {
    let seed = super::proto_tests::sample_for_stream_tests();
    let mut store = LogStore::default();
    for seq in 1..=128 {
        let mut entry = seed.entries[0].clone();
        entry.provenance.as_mut().unwrap().seq = seq;
        store.push(entry);
    }
    let mut doc = Document::from_snapshot(
        seed.header,
        Arc::new(store),
        ExportScope::default(),
        CaptureStatus::default(),
    )
    .unwrap();
    for meta in &mut doc.record_metadata {
        meta.extra
            .insert("large".into(), "x".repeat(64 * 1024).into());
    }
    doc
}

#[test]
fn selection_cancels_after_first_sidecar_copy_without_copying_other_rows() {
    let source = large_sidecar();
    let before = encode(&source);
    let cancel = Arc::new(AtomicBool::new(false));
    let copies = Rc::new(Cell::new(0));
    let hook = observe_clones({
        let cancel = Arc::clone(&cancel);
        let copies = Rc::clone(&copies);
        move || {
            copies.set(copies.get() + 1);
            cancel.store(true, Ordering::Release);
        }
    });
    let result = source.select_cancellable(
        &(0..128).collect::<Vec<_>>(),
        serde_json::json!({"tag":"Tag"}),
        &cancel,
    );
    drop(hook);
    assert_eq!(result.err().unwrap().kind, DocumentErrorKind::Cancelled);
    assert_eq!(copies.get(), 1, "copied beyond cancellation");
    assert_eq!(source.record_metadata.len(), 128);
    assert_eq!(encode(&source), before);
}

#[test]
fn load_rejects_unaffordable_sidecars_without_mutating_source() {
    let source = large_sidecar();
    let bytes = encode(&source);
    let budget = source.memory_bytes()
        - source
            .record_metadata
            .iter()
            .skip(1)
            .map(row_metadata_bytes)
            .sum::<usize>();
    let result = load(Cursor::new(&bytes), budget, &AtomicBool::new(false));
    assert_eq!(result.err().unwrap().kind, DocumentErrorKind::MemoryBudget);
    assert_eq!(encode(&source), bytes);
}

fn nested_arrays(depth: usize) -> Value {
    let mut value = Value::Null;
    for _ in 0..depth {
        value = Value::Array(vec![value]);
    }
    value
}

#[test]
fn final_encoded_layouts_obey_the_readers_depth_limit() {
    for layer in ["header", "text", "raw_proto", "derived_proto", "footer"] {
        let mut accepted = 0;
        let mut rejected = 0;
        for depth in 115..=132 {
            let mut doc = if matches!(layer, "raw_proto" | "derived_proto") {
                super::proto_tests::sample_for_stream_tests()
            } else {
                super::tests::sample()
            };
            let value = nested_arrays(depth);
            match layer {
                "header" => {
                    doc.header.extra.insert("nested".into(), value);
                }
                "text" => {
                    doc.record_metadata[0].extra.insert("nested".into(), value);
                }
                "raw_proto" => {
                    doc.record_metadata[0]
                        .proto_extra
                        .insert("nested".into(), value);
                }
                "derived_proto" => {
                    doc.record_metadata[0]
                        .device_time_extra
                        .insert("nested".into(), value);
                }
                "footer" => {
                    doc.end.scope.kind = "filtered".into();
                    doc.end.scope.filter = Some(value);
                }
                _ => unreachable!(),
            }
            // Serialize without the publication guard and use the actual
            // reader as oracle. In particular, derived proto fields acquire
            // extra containers only when the compact layout is constructed.
            let readable = match layer {
                "header" => decode_header(&encode_line(&doc.header).unwrap()).is_ok(),
                "footer" => decode_end(&encode_line(&doc.end).unwrap(), &doc.header).is_ok(),
                _ => decode_record(
                    &encode_record_layout(&doc.entries[0], &doc.record_metadata[0]).unwrap(),
                    &doc.header,
                    0,
                )
                .is_ok(),
            };
            let mut bytes = Vec::new();
            let written = write_snapshot(&mut bytes, &doc, &AtomicBool::new(false));
            assert_eq!(written.is_ok(), readable, "{layer} depth {depth}");
            if readable {
                accepted += 1;
                load(Cursor::new(bytes), BUDGET, &AtomicBool::new(false)).unwrap();
            } else {
                rejected += 1;
                assert_eq!(written.unwrap_err().kind, DocumentErrorKind::InvalidData);
            }
        }
        assert!(
            accepted > 0 && rejected > 0,
            "{layer}: boundary not covered"
        );
    }
}

#[test]
fn repeated_selection_fails_before_returning_an_unreadable_filter() {
    let mut source = super::tests::sample();
    source.end.scope.kind = "filtered".into();
    source.end.scope.filter = Some(nested_arrays(115));
    let indices: Vec<_> = (0..source.entries.len()).collect();
    let mut successful = 0;
    for _ in 0..16 {
        let before = encode(&source);
        match source.select(&indices, serde_json::json!({"query":"unchanged"})) {
            Ok(selected) => {
                source = reopen(&selected);
                successful += 1;
            }
            Err(error) => {
                assert_eq!(error.kind, DocumentErrorKind::InvalidData);
                assert!(successful > 0);
                assert_eq!(encode(&source), before);
                return;
            }
        }
    }
    panic!("composed filter never reached the reader's depth boundary");
}

#[test]
fn decoded_projection_validates_its_final_extension_containers() {
    let mut accepted = 0;
    let mut rejected = 0;
    for depth in 115..=132 {
        let mut doc = super::proto_tests::sample_for_stream_tests();
        doc.record_metadata[0]
            .projection_extra
            .insert("nested".into(), nested_arrays(depth));
        let mut wire = WireRecord::from_row(&doc.entries[0], &doc.record_metadata[0]);
        wire.extra.insert(
            "projection_extensions".into(),
            serde_json::to_value(&doc.record_metadata[0].projection_extra).unwrap(),
        );
        let raw = serde_json::to_vec(&wire).unwrap();
        let readable = serde_json::from_slice::<WireRecord>(&raw).is_ok();
        let encoded = encode_record_projection(
            &doc.header,
            &doc.entries[0],
            &doc.record_metadata[0],
            BUDGET,
        );
        assert_eq!(encoded.is_ok(), readable, "depth {depth}");
        if readable {
            accepted += 1;
            let encoded = encoded.unwrap();
            serde_json::from_slice::<Value>(&encoded).unwrap();
            assert_eq!(encoded, raw);
        } else {
            rejected += 1;
        }
    }
    assert!(accepted > 0 && rejected > 0);
}

#[test]
fn stream_header_and_footer_reject_final_encoded_depth() {
    let mut doc = super::tests::sample();
    doc.header.extra.insert("nested".into(), nested_arrays(132));
    assert!(StreamEncoder::new(doc.header).is_err());
    let doc = super::tests::sample();
    let mut stream = StreamEncoder::new(doc.header).unwrap();
    for entry in doc.entries.iter() {
        stream.record_bytes(entry).unwrap();
    }
    let mut capture = doc.end.capture;
    capture.known_gaps.push(nested_arrays(132));
    assert!(stream.finish_bytes(capture).is_err());
    // Failed finalization does not consume the valid stream state.
    stream.finish_bytes(CaptureStatus::default()).unwrap();
}

#[test]
fn unreadable_encoded_footer_is_never_atomically_published() {
    let directory =
        std::env::temp_dir().join(format!("ciallogcat-depth-test-{}", SessionId::new()));
    std::fs::create_dir(&directory).unwrap();
    let original = directory.join("source.jsonl");
    let destination = directory.join("output.jsonl");
    let mut doc = super::tests::sample();
    let before = encode(&doc);
    std::fs::write(&original, &before).unwrap();
    doc.end.scope.kind = "filtered".into();
    doc.end.scope.filter = Some(nested_arrays(132));
    let cancel = AtomicBool::new(false);
    let result = crate::file_io::write_new_file(&destination, &cancel, |writer| {
        write_snapshot(writer, &doc, &cancel).map_err(std::io::Error::other)
    });
    assert!(result.is_err());
    assert!(!destination.exists());
    assert_eq!(std::fs::read(&original).unwrap(), before);
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn every_extension_map_requires_final_depth_admission() {
    let doc = super::proto_tests::sample_for_stream_tests();
    let original = &doc.record_metadata[0];
    assert!(!original.has_extensions());
    for index in 0..7 {
        let mut deep = Value::Null;
        for _ in 0..130 {
            deep = Value::Array(vec![deep]);
        }
        for (value, valid) in [
            (serde_json::json!({}), true),
            (serde_json::json!([]), true),
            (deep, false),
        ] {
            let mut metadata = original.clone();
            [
                &mut metadata.extra,
                &mut metadata.device_time_extra,
                &mut metadata.process_extra,
                &mut metadata.provenance_extra,
                &mut metadata.text_loss_extra,
                &mut metadata.proto_extra,
                &mut metadata.projection_extra,
            ]
            .into_iter()
            .nth(index)
            .unwrap()
            .insert("future".into(), value);
            // An empty container VALUE is still an extension: do not inspect
            // only its children when deciding which final shape must be checked.
            assert!(metadata.has_extensions(), "map {index}");
            let archived = encode_record(&doc.header, &doc.entries[0], &metadata);
            let projected =
                encode_record_projection(&doc.header, &doc.entries[0], &metadata, BUDGET);
            if valid {
                decode_record(&archived.unwrap(), &doc.header, 0).unwrap();
                serde_json::from_slice::<WireRecord>(&projected.unwrap()).unwrap();
            } else {
                let error = archived.unwrap_err();
                assert!(
                    error.message.contains("recursion limit"),
                    "map {index}: {error}"
                );
                let error = projected.unwrap_err();
                assert!(
                    error.message.contains("recursion limit"),
                    "projection map {index}: {error}"
                );
            }
        }
    }
}

#[test]
fn fixed_record_layouts_are_reader_admissible_despite_json_like_strings() {
    let mut session = crate::session::Session::new();
    session.begin_attempt().unwrap();
    session.stream_started().unwrap();
    let header = DocumentHeader::live(session.id(), "serial".into(), vec![LogBuffer::Main]);
    let body = format!("{}\"\\\n\0中{}", "[{".repeat(160), "}]".repeat(160));
    let proto = super::proto_tests::sample_for_stream_tests();
    for mut entry in [
        LogEntry::new(body.clone(), 0, 0, body.clone(), Level::Info, body.clone()),
        LogEntry::marker(body.clone()),
        LogEntry::unparsed(body),
        proto.entries[0].clone(),
        crate::protobuf::DecodedEntry::decode_payload(Vec::new())
            .unwrap()
            .project(),
    ] {
        entry.provenance = session.next_record(None);
        let metadata = RecordMetadata {
            seq: entry.provenance.unwrap().seq,
            event_kind: (entry.kind == LogEntryKind::Marker).then(|| "event".into()),
            ..RecordMetadata::default()
        };
        assert!(!metadata.has_extensions());
        let bytes = encode_record(&header, &entry, &metadata).unwrap();
        assert_eq!(bytes, encode_record_layout(&entry, &metadata).unwrap());
        let (decoded, decoded_metadata) = decode_record(&bytes, &header, 0).unwrap();
        assert_eq!(decoded.kind, entry.kind);
        assert_eq!(decoded.message, entry.message);
        assert_eq!(decoded_metadata.seq, metadata.seq);
        let projection = encode_record_projection(&header, &entry, &metadata, BUDGET).unwrap();
        serde_json::from_slice::<WireRecord>(&projection).unwrap();
        let json: Value = serde_json::from_slice(&projection).unwrap();
        fn depth(value: &Value) -> usize {
            match value {
                Value::Object(fields) => 1 + fields.values().map(depth).max().unwrap_or(0),
                Value::Array(values) => 1 + values.iter().map(depth).max().unwrap_or(0),
                _ => 0,
            }
        }
        assert!(depth(&json) <= 2);
    }
}
