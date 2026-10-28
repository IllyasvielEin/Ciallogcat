//! Exercise document extension decoding through the real CLI, using raw JSON
//! tokens inserted only after fixture serialization. No ADB or device required.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, atomic::AtomicBool};

use ciallogcat::document::{
    self, CaptureStatus, Document, DocumentHeader, ExportScope, NativeFormat,
};
use ciallogcat::log_store::LogStore;
use ciallogcat::model::LogBuffer;
use ciallogcat::protobuf::DecodedEntry;
use ciallogcat::session::Session;
use serde_json::Value;

struct Fixture {
    root: PathBuf,
    file: PathBuf,
    lines: Vec<String>,
}

impl Fixture {
    fn new(format: NativeFormat) -> Self {
        let mut session = Session::new();
        session.begin_attempt().unwrap();
        session.stream_started().unwrap();
        let mut row = DecodedEntry::decode_payload(
            b"\x08\x01\x10\x02\x18\x08\x20\x2a\x28\x04\x30\x05\x3a\x04Tag\0\x42\x01m\x4a\x03app"
                .to_vec(),
        )
        .unwrap()
        .project();
        row.provenance = session.next_record(Some(123));
        let mut store = LogStore::default();
        store.push(row);
        let mut header = DocumentHeader::live(session.id(), "mock".into(), vec![LogBuffer::Main]);
        header.source.transport_format = Some("logcat_proto_experimental".into());
        header.set_format(format).unwrap();
        let mut doc = Document::from_snapshot(
            header,
            Arc::new(store),
            ExportScope::default(),
            CaptureStatus::default(),
        )
        .unwrap();
        for extra in [
            &mut doc.header.source.extra,
            &mut doc.record_metadata[0].proto_extra,
            &mut doc.end.capture.extra,
        ] {
            extra.insert("future".into(), "__raw_extension__".into());
        }
        let mut bytes = Vec::new();
        document::write_snapshot(&mut bytes, &doc, &AtomicBool::new(false)).unwrap();
        let root = std::env::temp_dir().join(format!(
            "ciallogcat-document-query-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&root).unwrap();
        Self {
            file: root.join("native.jsonl"),
            root,
            lines: String::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect(),
        }
    }

    fn write(&self, layer: Option<usize>, raw: &str) {
        let mut lines = self.lines.clone();
        for (index, line) in lines.iter_mut().enumerate() {
            if layer.is_none_or(|layer| layer == index) {
                *line = line.replace("\"__raw_extension__\"", raw);
            }
        }
        fs::write(&self.file, format!("{}\n", lines.join("\n"))).unwrap();
    }

    fn run(&self, command: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ciallogcat-cli"))
            .arg(command)
            .arg("--file")
            .arg(&self.file)
            .args(args)
            .env("CIALLOGCAT_CONFIG_DIR", self.root.join("config"))
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn query_rejects_raw_magic_number_objects_in_native_header_record_and_footer() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let fixture = Fixture::new(format);
        for object in [
            r#"{"$serde_json::private::Number":"1e-400"}"#,
            r#"{"nested":[{"\u0024serde_json::private::Number":"1e-400"}]}"#,
        ] {
            for layer in 0..3 {
                fixture.write(Some(layer), object);
                let output = fixture.run("query-v2", &[]);
                assert!(
                    !output.status.success(),
                    "{format:?} layer {layer}: {}",
                    String::from_utf8_lossy(&output.stdout)
                );
                let error: Value = serde_json::from_slice(&output.stderr).unwrap();
                assert_eq!(error["error"]["code"], "invalid_source");
                assert!(
                    error["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("unsupported JSON object key")
                );
            }
        }
    }
}

#[test]
fn query_decoded_view_keeps_underflow_and_large_integer_extension_tokens() {
    for format in [NativeFormat::V2, NativeFormat::CompactV3] {
        let fixture = Fixture::new(format);
        for token in ["1e-400", "18446744073709551617"] {
            fixture.write(None, token);
            let output = fixture.run("query-v2", &[]);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let page: Value = serde_json::from_slice(&output.stdout).unwrap();
            let reference = page["records"][0]["record_ref"].as_str().unwrap();
            for view in ["stored", "decoded"] {
                let output = fixture.run("get-v2", &["--ref", reference, "--view", view]);
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let response: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert!(
                    response["record_json_chunk"]
                        .as_str()
                        .unwrap()
                        .contains(&format!("\"future\":{token}"))
                );
                assert!(
                    response["next_offset"].is_null(),
                    "small fixture should fit one chunk"
                );
            }
        }
    }
}
