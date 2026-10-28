//! Publish complete snapshots without exposing partial files or overwriting a
//! destination that appeared while a background save was running.
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::session::SessionId;

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Save as a new path. Existing files are never replaced. The sibling temporary
/// file is flushed and synced before publication. Hard-link publication is
/// atomic and does not clobber; filesystems without hard links fail safely.
/// This does not promise power-loss durability of the parent directory entry.
pub fn write_new_file<T>(
    path: &Path,
    cancel: &AtomicBool,
    write: impl FnOnce(&mut BufWriter<File>) -> io::Result<T>,
) -> io::Result<T> {
    check_cancel(cancel)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "choose a file name"))?;
    if path.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination already exists; choose a new file name",
        ));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".{}.tmp", SessionId::new()));
    let temp_path = parent.join(temp_name);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&temp_path)?;
    let temporary = TemporaryFile(temp_path);
    let mut writer = BufWriter::new(file);
    let result = write(&mut writer)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    check_cancel(cancel)?;
    // Drop the file handle before linking/removing for Windows compatibility.
    drop(writer);
    fs::hard_link(&temporary.0, path)?;
    Ok(result)
}

pub fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "operation cancelled",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("ciallogcat-save-test-{}", SessionId::new()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn failed_and_cancelled_writes_leave_no_partial_destination() {
        let directory = TestDirectory::new();
        let path = directory.0.join("logs.jsonl");
        let cancel = AtomicBool::new(false);
        let result = write_new_file(&path, &cancel, |writer| {
            writer.write_all(b"partial")?;
            Err::<(), _>(io::Error::other("encoding failed"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
        let result = write_new_file(&path, &cancel, |writer| {
            writer.write_all(b"complete but cancelled")?;
            cancel.store(true, Ordering::Release);
            Ok(())
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
    }

    #[test]
    fn publication_never_replaces_a_concurrent_destination() {
        let directory = TestDirectory::new();
        let path = directory.0.join("logs.jsonl");
        let cancel = AtomicBool::new(false);
        let result = write_new_file(&path, &cancel, |writer| {
            writer.write_all(b"snapshot")?;
            fs::write(&path, b"created concurrently")?;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"created concurrently");
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
        fs::remove_file(&path).unwrap();
        write_new_file(&path, &cancel, |writer| writer.write_all(b"complete\n")).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"complete\n");
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }
}
