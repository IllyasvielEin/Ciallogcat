//! Publish the visible JSONL boundary only after a complete record is flushed.
//! This is process visibility, not a per-record fsync/durability guarantee.
use std::io::{self, Write};
use std::sync::{
    Mutex,
    mpsc::{Receiver, SyncSender},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Progress {
    pub entries: u64,
    pub bytes: u64,
}

pub(super) fn run(
    mut writer: impl Write,
    records: Receiver<Vec<u8>>,
    visible: &Mutex<Progress>,
    notify: &SyncSender<()>,
) -> io::Result<()> {
    let mut completed = Progress::default();
    for bytes in records {
        writer.write_all(&bytes)?;
        // Write may be buffered. A successful write_all alone does not mean
        // another process can read the claimed boundary from the file.
        writer.flush()?;
        completed.entries += 1;
        completed.bytes += bytes.len() as u64;
        *visible.lock().unwrap() = completed;
        let _ = notify.try_send(());
    }
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn published_record_is_visible_even_with_a_buffered_writer() {
        let contents = Arc::new(Mutex::new(Vec::new()));
        let sink = Sink(Arc::clone(&contents));
        let visible = Arc::new(Mutex::new(Progress::default()));
        let published = Arc::clone(&visible);
        let (tx, rx) = mpsc::channel();
        let (notify, notifications) = mpsc::sync_channel(1);
        let worker =
            std::thread::spawn(move || run(io::BufWriter::new(sink), rx, &published, &notify));
        let line = b"{\"seq\":1,\"message\":\"ready\"}\n";
        tx.send(line.to_vec()).unwrap();
        notifications.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(&*contents.lock().unwrap(), line);
        assert_eq!(
            *visible.lock().unwrap(),
            Progress {
                entries: 1,
                bytes: line.len() as u64,
            }
        );
        drop(tx);
        worker.join().unwrap().unwrap();
    }

    struct PartialFailure {
        remaining: usize,
    }
    impl Write for PartialFailure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other("simulated full disk"));
            }
            let len = bytes.len().min(self.remaining);
            self.remaining -= len;
            Ok(len)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_record_failure_does_not_advance_either_boundary() {
        let first = b"{\"seq\":1}\n";
        let (tx, rx) = mpsc::channel();
        tx.send(first.to_vec()).unwrap();
        tx.send(b"{\"seq\":2}\n".to_vec()).unwrap();
        drop(tx);
        let visible = Mutex::new(Progress::default());
        let (notify, _notifications) = mpsc::sync_channel(1);
        let result = run(
            PartialFailure {
                remaining: first.len() + 3,
            },
            rx,
            &visible,
            &notify,
        );
        assert!(result.is_err());
        assert_eq!(
            *visible.lock().unwrap(),
            Progress {
                entries: 1,
                bytes: first.len() as u64,
            }
        );
    }

    #[test]
    fn failed_flush_does_not_publish_buffered_bytes() {
        struct FlushFailure;
        impl Write for FlushFailure {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("simulated flush failure"))
            }
        }
        let (tx, rx) = mpsc::channel();
        tx.send(b"{\"seq\":1}\n".to_vec()).unwrap();
        drop(tx);
        let visible = Mutex::new(Progress::default());
        let (notify, _notifications) = mpsc::sync_channel(1);
        assert!(run(FlushFailure, rx, &visible, &notify).is_err());
        assert_eq!(*visible.lock().unwrap(), Progress::default());
    }
}
