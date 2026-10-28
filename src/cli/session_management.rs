//! Administrative closure is deliberately independent of worker status writes.
//! An old or paused worker may still exist: this is not a process-death proof.
use std::fs;
use std::io::Write;
use std::path::Path;

use serde_json::{Value, json};

use super::{Result, Status, error, io_error, new_file, now, read_limited};

const CLOSURE: &str = "management-closure.json";
const STATUS_LIMIT: u64 = 131072;
const CLOSURE_LIMIT: u64 = 262144;

fn invalid(message: impl Into<String>) -> super::Error {
    error("invalid_session", message, 3)
}

fn parse_status(bytes: &[u8], query_source: bool) -> Result<Value> {
    // Keep the existing query protocol's invalid_source distinction for JSON
    // and envelope errors; filesystem/size errors retain their original codes.
    let invalid_status = |message: String| {
        error(
            if query_source {
                "invalid_source"
            } else {
                "invalid_session"
            },
            message,
            3,
        )
    };
    let value: Value =
        ciallogcat::json_input::from_slice(bytes).map_err(|e| invalid_status(e.to_string()))?;
    if value["schema_version"] != 1 || value["type"] != "status" || !value.is_object() {
        return Err(invalid_status("unsupported session status envelope".into()));
    }
    Ok(value)
}

pub(super) fn closure_status(dir: &Path) -> Result<Option<Value>> {
    let path = dir.join(CLOSURE);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(e)),
        Ok(_) => (),
    }
    let value: Value = ciallogcat::json_input::from_slice(&read_limited(&path, CLOSURE_LIMIT)?)
        .map_err(|e| invalid(e.to_string()))?;
    if value["schema_version"] != 1
        || value["type"] != "administrative_closure"
        || value["closed_unix_ms"].as_u64().is_none()
    {
        return Err(invalid("invalid administrative closure envelope"));
    }
    let mut snapshot = value["snapshot"].clone();
    // Validate the stored snapshot as a real status, without filling in missing
    // counters, identities or visibility bounds. Unknown extensions survive.
    let status: Status =
        serde_json::from_value(snapshot.clone()).map_err(|e| invalid(e.to_string()))?;
    if status.schema_version != 1 || status.kind != "status" {
        return Err(invalid("invalid administrative closure snapshot"));
    }
    let fields = snapshot
        .as_object_mut()
        .ok_or_else(|| invalid("invalid closure snapshot"))?;
    fields.insert("state".into(), "abandoned".into());
    fields.insert("reason".into(), "administratively_closed".into());
    fields.insert("stale".into(), false.into());
    fields.insert("worker_exit_confirmed".into(), false.into());
    fields.insert(
        "management_closed_unix_ms".into(),
        value["closed_unix_ms"].clone(),
    );
    Ok(Some(snapshot))
}

pub(super) fn read_effective_status(dir: &Path) -> Result<Value> {
    read_effective(dir, false)
}

pub(super) fn read_effective_query_status(dir: &Path) -> Result<Value> {
    read_effective(dir, true)
}

fn read_effective(dir: &Path, query_source: bool) -> Result<Value> {
    if let Some(status) = closure_status(dir)? {
        return Ok(status);
    }
    let status = parse_status(
        &read_limited(&dir.join("status.json"), STATUS_LIMIT)?,
        query_source,
    )?;
    // Prefer a closure that won while status.json was being read.
    Ok(closure_status(dir)?.unwrap_or(status))
}

pub(super) fn close_administratively(dir: &Path) -> Result<Value> {
    let snapshot = read_effective_status(dir)?;
    let status: Status =
        serde_json::from_value(snapshot.clone()).map_err(|e| invalid(e.to_string()))?;
    if status.terminal() {
        return Ok(snapshot);
    }
    let closed = json!({"schema_version":1,"type":"administrative_closure",
        "closed_unix_ms":now(),"snapshot":snapshot});
    let bytes = serde_json::to_vec(&closed).map_err(io_error)?;
    if bytes.len() as u64 + 1 > CLOSURE_LIMIT {
        return Err(invalid("administrative closure exceeds size limit"));
    }
    let _: Value = ciallogcat::json_input::from_slice(&bytes)
        .map_err(|e| invalid(format!("closure snapshot cannot be reread: {e}")))?;
    let temporary = dir.join(format!(".management-closure-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = new_file(&temporary)?;
        file.write_all(&bytes).map_err(io_error)?;
        file.write_all(b"\n").map_err(io_error)?;
        file.flush().map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        drop(file);
        // Best effort acknowledgement recheck immediately before publication.
        // The worker may still race afterwards; the response never claims exit.
        let current = read_effective_status(dir)?;
        let current_status: Status =
            serde_json::from_value(current.clone()).map_err(|e| invalid(e.to_string()))?;
        if current_status.terminal() {
            return Ok(current);
        }
        // Atomic first-writer-wins publication of a fully written inode. No
        // rename-overwrite fallback: unsupported filesystems fail closed.
        match fs::hard_link(&temporary, dir.join(CLOSURE)) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(io_error(e)),
        }
        closure_status(dir)?.ok_or_else(|| invalid("published closure disappeared"))
    })();
    let _ = fs::remove_file(&temporary);
    result
}
