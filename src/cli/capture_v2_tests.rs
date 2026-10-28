//! Writer publication must match complete flushed packet boundaries.
use super::*;

#[test]
fn legacy_worker_config_keeps_default_history_and_gui_buffers() {
    let config: Config = serde_json::from_value(json!({
        "adb":"adb", "serial":"TEST-USB", "duration_secs":600,
        "max_bytes":67108864, "max_entries":10000000
    }))
    .unwrap();
    assert_eq!(config.scope.logcat_args(), ["-T", "1"]);
    assert_eq!(config.buffers, LogBuffer::defaults());
    assert_eq!(config.record_schema_version, 1);
    assert_eq!(config.transport, adb::LogTransport::Text);
}

fn packets() -> (mpsc::Sender<Packet>, mpsc::Receiver<Packet>) {
    mpsc::channel()
}

#[test]
fn v2_writer_counts_header_and_footer_bytes_without_advancing_sequence() {
    let (tx, rx) = packets();
    let header = b"header\n";
    let entry = b"record\n";
    let footer = b"footer\n";
    for (bytes, seq) in [(header, None), (entry, Some(9)), (footer, None)] {
        tx.send(Packet {
            bytes: bytes.to_vec(),
            seq,
        })
        .unwrap();
    }
    drop(tx);
    let visible = Mutex::new(Progress::default());
    // Deliberately never drain notifications: an already queued hint must not
    // block the writer or become part of the durability/visibility contract.
    let (notify, _notifications) = mpsc::sync_channel(1);
    let mut contents = Vec::new();
    write_packets(&mut contents, rx, &visible, &notify).unwrap();
    assert_eq!(
        contents,
        [header.as_slice(), entry.as_slice(), footer.as_slice()].concat()
    );
    assert_eq!(
        *visible.lock().unwrap(),
        Progress {
            entries: 1,
            bytes: contents.len() as u64,
            seq: 9
        }
    );
}

#[test]
fn v2_partial_packet_failure_preserves_both_published_boundaries() {
    struct FailAfter(usize);
    impl Write for FailAfter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::other("full disk"));
            }
            let count = self.0.min(bytes.len());
            self.0 -= count;
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (tx, rx) = packets();
    tx.send(Packet {
        bytes: b"header\n".to_vec(),
        seq: None,
    })
    .unwrap();
    tx.send(Packet {
        bytes: b"first\n".to_vec(),
        seq: Some(4),
    })
    .unwrap();
    tx.send(Packet {
        bytes: b"partial\n".to_vec(),
        seq: Some(5),
    })
    .unwrap();
    drop(tx);
    let visible = Mutex::new(Progress::default());
    let (notify, _notifications) = mpsc::sync_channel(1);
    let error = write_packets(FailAfter(7 + 6 + 3), rx, &visible, &notify).unwrap_err();
    assert!(error.to_string().contains("full disk"));
    assert_eq!(
        *visible.lock().unwrap(),
        Progress {
            entries: 1,
            bytes: 13,
            seq: 4
        }
    );
}

#[test]
fn v2_flush_failure_does_not_publish_a_whole_written_packet() {
    struct FailSecondFlush(usize);
    impl Write for FailSecondFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0 += 1;
            if self.0 == 2 {
                Err(io::Error::other("flush failed"))
            } else {
                Ok(())
            }
        }
    }
    let (tx, rx) = packets();
    tx.send(Packet {
        bytes: b"header\n".to_vec(),
        seq: None,
    })
    .unwrap();
    tx.send(Packet {
        bytes: b"entry\n".to_vec(),
        seq: Some(1),
    })
    .unwrap();
    drop(tx);
    let visible = Mutex::new(Progress::default());
    let (notify, _notifications) = mpsc::sync_channel(1);
    assert!(write_packets(FailSecondFlush(0), rx, &visible, &notify).is_err());
    assert_eq!(
        *visible.lock().unwrap(),
        Progress {
            entries: 0,
            bytes: 7,
            seq: 0
        }
    );
}

#[test]
fn v2_pair_is_published_only_after_flush_and_notifies_a_quiet_consumer() {
    struct Gate {
        written: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Write for Gate {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.written.send(()).map_err(io::Error::other)?;
            self.release.recv().map_err(io::Error::other)
        }
    }
    let (tx, rx) = packets();
    let visible = Arc::new(Mutex::new(Progress::default()));
    let published = Arc::clone(&visible);
    let (written, awaiting_flush) = mpsc::sync_channel(1);
    let (release, released) = mpsc::channel();
    let (notify, notifications) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        write_packets(
            Gate {
                written,
                release: released,
            },
            rx,
            &published,
            &notify,
        )
    });
    tx.send(Packet {
        bytes: b"entry\n".to_vec(),
        seq: Some(17),
    })
    .unwrap();
    awaiting_flush.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(*visible.lock().unwrap(), Progress::default());
    assert!(notifications.try_recv().is_err());
    release.send(()).unwrap();
    notifications.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        *visible.lock().unwrap(),
        Progress {
            entries: 1,
            bytes: 6,
            seq: 17
        }
    );
    drop(tx);
    // The final flush does not introduce a new packet or a new watermark.
    awaiting_flush.recv_timeout(Duration::from_secs(2)).unwrap();
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert!(notifications.try_recv().is_err());
}
