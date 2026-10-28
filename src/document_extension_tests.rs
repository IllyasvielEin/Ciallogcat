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

fn convert(doc: &Document, format: NativeFormat) -> Document {
    doc.convert_format(format, BUDGET, u64::MAX, &AtomicBool::new(false))
        .unwrap()
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

fn numeric_fixture(format: NativeFormat, token: &str) -> Vec<u8> {
    let mut doc = convert(&super::proto_tests::sample_for_stream_tests(), format);
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
    if format == NativeFormat::CompactV3 {
        meta.projection_extra
            .insert("future".into(), TOKEN_MARKER.into());
    }
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
fn numeric_extensions_survive_all_flatten_layers_load_select_v2_v3_v2() {
    for token in NUMBERS {
        for format in [NativeFormat::V2, NativeFormat::CompactV3] {
            let source = load(
                Cursor::new(numeric_fixture(format, token)),
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
            let compact = reopen(&convert(&source, NativeFormat::CompactV3));
            assert_numbers(&compact, token);
            if format == NativeFormat::CompactV3 {
                assert_eq!(
                    compact.record_metadata[0].projection_extra["future"].to_string(),
                    *token
                );
            }
            let back = reopen(&convert(&compact, NativeFormat::V2));
            assert_numbers(&back, token);
            if format == NativeFormat::CompactV3 {
                assert_eq!(
                    back.record_metadata[0].extra["projection_extensions"]["future"].to_string(),
                    *token
                );
            }
            assert_eq!(
                back.entries[0].proto.as_ref().unwrap().raw_payload,
                source.entries[0].proto.as_ref().unwrap().raw_payload
            );
        }
    }
}

#[test]
fn numeric_legacy_extensions_survive_v1_v3_v2() {
    for token in NUMBERS {
        let bytes = format!(
            r#"{{"schema_version":1,"type":"entry","seq":9,"time":"old","pid":1,"tid":2,"level":"I","tag":"T","message":"m","future":{token},"tree":[{{"n":{token}}}]}}"#
        );
        let legacy = load(Cursor::new(bytes), BUDGET, &AtomicBool::new(false)).unwrap();
        let compact = reopen(&convert(&legacy, NativeFormat::CompactV3));
        let back = reopen(&convert(&compact, NativeFormat::V2));
        for doc in [&legacy, &compact, &back] {
            let extra = &doc.record_metadata[0].extra["legacy_extensions"];
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
        for format in [NativeFormat::V2, NativeFormat::CompactV3] {
            let template = numeric_fixture(format, "1e-400");
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
            if format == NativeFormat::V2 {
                layers.extend([(1, "/device_time/future"), (1, "/process/future")]);
            } else {
                layers.extend([
                    (1, "/projection_extensions/future"),
                    (1, "/projection_extensions/device_time/future"),
                    (1, "/projection_extensions/process/future"),
                ]);
            }
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
                    "{format:?} {path}: {direct}"
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
        let legacy = format!(
            r#"{{"schema_version":1,"type":"entry","seq":9,"time":"old","pid":1,"tid":2,"level":"I","tag":"T","message":"m","future":[{object}]}}"#
        );
        let error = load(Cursor::new(legacy), BUDGET, &AtomicBool::new(false))
            .err()
            .unwrap();
        assert!(error.message.contains("unsupported JSON object key"));
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
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let doc = convert(&super::proto_tests::sample_for_stream_tests(), format);
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
                    "{format:?} footer {path}: {token}"
                );
            }
            for path in [
                "/schema_version",
                "/seq",
                "/provenance/seq",
                "/provenance/segment",
                "/provenance/attempt",
                "/provenance/observed_unix_ms",
                "/legacy_original_seq",
            ] {
                assert!(
                    decode_record(&replace_token(&row, path, token), &doc.header, 0).is_err(),
                    "{format:?} record {path}: {token}"
                );
            }
            if format == NativeFormat::V2 {
                for path in [
                    "/pid",
                    "/tid",
                    "/proto/time_sec",
                    "/proto/time_nsec",
                    "/proto/uid",
                    "/proto/pid",
                    "/proto/tid",
                    "/proto/priority",
                ] {
                    assert!(
                        decode_record(&replace_token(&row, path, token), &doc.header, 0).is_err(),
                        "v2 record {path}: {token}"
                    );
                }
            }
        }
        for (path, token) in [
            ("/text_loss/truncated", "0"),
            ("/text_loss/invalid_utf8_replaced", "1"),
            ("/proto/raw_payload", "0"),
        ] {
            assert!(decode_record(&replace_token(&row, path, token), &doc.header, 0).is_err());
        }
    }
    let legacy: Value = serde_json::from_str(r#"{"schema_version":1,"type":"entry","seq":9,"time":"old","pid":1,"tid":2,"level":"I","tag":"T","message":"m"}"#).unwrap();
    for token in invalid {
        for path in ["/schema_version", "/seq", "/pid", "/tid"] {
            assert!(
                load(
                    Cursor::new(replace_token(&legacy, path, token)),
                    BUDGET,
                    &AtomicBool::new(false)
                )
                .is_err(),
                "v1 {path}: {token}"
            );
        }
    }
}

#[test]
fn long_numeric_tokens_count_toward_retained_memory_and_conversion_budget() {
    let mut doc = super::proto_tests::sample_for_stream_tests();
    let before = doc.memory_bytes();
    let token = format!("0.{}1", "0".repeat(64 * 1024));
    let value: Value = serde_json::from_str(&token).unwrap();
    assert!(value_bytes(&value) >= token.len());
    doc.record_metadata[0]
        .proto_extra
        .insert("future".into(), value);
    assert!(doc.memory_bytes() >= before + token.len());
    let error = doc
        .convert_format(
            NativeFormat::CompactV3,
            before + token.len() / 2,
            u64::MAX,
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
    assert_eq!(error.kind, DocumentErrorKind::MemoryBudget);
    let back = reopen(&convert(
        &convert(&doc, NativeFormat::CompactV3),
        NativeFormat::V2,
    ));
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
fn conversion_cancels_after_first_sidecar_copy_without_copying_the_other_127_rows() {
    let source = large_sidecar();
    for from in [NativeFormat::V2, NativeFormat::CompactV3] {
        let doc = convert(&source, from);
        for to in [NativeFormat::V2, NativeFormat::CompactV3] {
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
            let result = doc.convert_format(to, BUDGET, u64::MAX, &cancel);
            drop(hook);
            assert_eq!(result.err().unwrap().kind, DocumentErrorKind::Cancelled);
            assert_eq!(
                copies.get(),
                1,
                "{from:?} -> {to:?} copied beyond cancellation"
            );
            assert_eq!(doc.record_metadata.len(), 128);
            assert_eq!(
                doc.record_metadata[127].extra["large"]
                    .as_str()
                    .unwrap()
                    .len(),
                64 * 1024
            );
            assert!(Arc::ptr_eq(&source.entries, &doc.entries));
        }
    }
}

#[test]
fn conversion_rejects_an_unaffordable_sidecar_before_deep_copying_it() {
    let source = large_sidecar();
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let reference = convert(&source, format);
        // Enough for all fixed costs and one row's extensions, not the second.
        let budget = reference.memory_bytes()
            - reference
                .record_metadata
                .iter()
                .skip(1)
                .map(row_metadata_bytes)
                .sum::<usize>();
        let copies = Rc::new(Cell::new(0));
        let hook = observe_clones({
            let copies = Rc::clone(&copies);
            move || copies.set(copies.get() + 1)
        });
        let result = source.convert_format(format, budget, u64::MAX, &AtomicBool::new(false));
        drop(hook);
        assert_eq!(result.err().unwrap().kind, DocumentErrorKind::MemoryBudget);
        assert_eq!(
            copies.get(),
            0,
            "copied a source that cannot fit the budget"
        );
    }
}
