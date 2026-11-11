//! Bounded crash/ANR grouping over scanned records.
//!
//! Signatures are explainable navigation hints built from well-known framework
//! log shapes, never root-cause diagnoses. Every group keeps a reference to the
//! record that opened its first occurrence so callers can drill down with
//! `get`/`context`.
use std::collections::HashMap;

use serde::Serialize;

use super::source::{Boundary, Row, Source};
use super::tokens::Signer;
use super::{Result, preview, record_ref};

/// Open incidents are closed after this many scanned records without a
/// continuation line, so interleaved output from other processes is tolerated.
const IDLE_RECORDS: u64 = 400;
const MAX_OPEN: usize = 16;
const MAX_GROUPS: usize = 500;
const MAX_SECTIONS: usize = 8;
const MAX_FRAMES: usize = 8;
const OUTPUT_FRAMES: usize = 5;
const TEXT_BYTES: usize = 240;
/// Multi-line messages (proto transport) are inspected up to this many lines.
const MAX_MESSAGE_LINES: usize = 256;
/// Native libraries skipped when choosing a signature frame (abort plumbing).
const NATIVE_PLUMBING: [&str; 4] = ["libc.so", "libbase.so", "liblog.so", "libc++.so"];

#[derive(Clone, Copy, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    JavaCrash,
    NativeCrash,
    Anr,
}

#[derive(Default)]
struct Section {
    header: String,
    frames: Vec<String>,
}

struct Incident {
    kind: Kind,
    /// PID that writes continuation lines (the crashing process for Java/ANR
    /// logging, the tombstone writer for DEBUG lines).
    writer_pid: Option<u32>,
    pid: Option<u32>,
    process: Option<String>,
    thread: Option<String>,
    sections: Vec<Section>,
    signal: Option<String>,
    abort: Option<String>,
    reason: Option<String>,
    tombstone: bool,
    first_seq: u64,
    last_seq: u64,
    first_ref: String,
    observed_unix_ms: Option<u64>,
    last_activity: u64,
}

#[derive(Serialize)]
pub(super) struct Group {
    kind: Kind,
    signature: String,
    occurrences: u64,
    first_seq: u64,
    last_seq: u64,
    first_ref: String,
    first_observed_unix_ms: Option<u64>,
    pids: Vec<u32>,
    process: Option<String>,
    thread: Option<String>,
    headline: Option<String>,
    cause: Option<String>,
    frames: Vec<String>,
}

#[derive(Default)]
pub(super) struct Collector {
    open: Vec<Incident>,
    groups: Vec<Group>,
    index: HashMap<(Kind, String), usize>,
    incidents: u64,
    seen: u64,
}

fn text(value: &str) -> String {
    preview(value.trim(), TEXT_BYTES).to_owned()
}

impl Collector {
    pub(super) fn feed(
        &mut self,
        signer: &Signer,
        source: &Source,
        upper: Boundary,
        offset: u64,
        bytes: &[u8],
        row: &Row,
    ) -> Result<()> {
        self.seen += 1;
        let seen = self.seen;
        let idle: Vec<_> = self
            .open
            .iter()
            .enumerate()
            .filter(|(_, incident)| seen - incident.last_activity > IDLE_RECORDS)
            .map(|(index, _)| index)
            .collect();
        for index in idle.into_iter().rev() {
            let incident = self.open.remove(index);
            self.close(incident);
        }
        let Some(log) = row.entry.log_fields() else {
            return Ok(());
        };
        for line in row.entry.message.lines().take(MAX_MESSAGE_LINES) {
            let line = line.trim();
            let start = match log.tag {
                "AndroidRuntime" => line
                    .strip_prefix("FATAL EXCEPTION:")
                    .map(|thread| (Kind::JavaCrash, Some(text(thread)))),
                "libc" if line.starts_with("Fatal signal ") => Some((Kind::NativeCrash, None)),
                "DEBUG" if line.starts_with("*** *** ***") => {
                    // A tombstone usually follows libc's signal line for the same
                    // crash; attach to it rather than opening a second incident.
                    if let Some(open) = self
                        .open
                        .iter_mut()
                        .rev()
                        .find(|i| i.kind == Kind::NativeCrash && !i.tombstone)
                    {
                        open.tombstone = true;
                        open.writer_pid = Some(log.pid);
                        open.last_activity = seen;
                        open.last_seq = row.seq;
                        None
                    } else {
                        Some((Kind::NativeCrash, None))
                    }
                }
                "ActivityManager" if line.starts_with("ANR in ") => Some((Kind::Anr, None)),
                _ => None,
            };
            if let Some((kind, thread)) = start {
                if kind == Kind::JavaCrash
                    && let Some(index) = self
                        .open
                        .iter()
                        .position(|i| i.kind == kind && i.writer_pid == Some(log.pid))
                {
                    let incident = self.open.remove(index);
                    self.close(incident);
                }
                if self.open.len() >= MAX_OPEN {
                    let incident = self.open.remove(0);
                    self.close(incident);
                }
                let mut incident = Incident {
                    kind,
                    writer_pid: Some(log.pid),
                    pid: (kind != Kind::NativeCrash).then_some(log.pid),
                    process: log.process.map(text),
                    thread,
                    sections: Vec::new(),
                    signal: None,
                    abort: None,
                    reason: None,
                    tombstone: log.tag == "DEBUG",
                    first_seq: row.seq,
                    last_seq: row.seq,
                    first_ref: record_ref(signer, source, upper, offset, bytes, row.seq)?,
                    observed_unix_ms: row.entry.provenance.and_then(|p| p.observed_unix_ms),
                    last_activity: seen,
                };
                match kind {
                    Kind::NativeCrash if log.tag == "libc" => native_signal(&mut incident, line),
                    Kind::Anr => {
                        incident.process = Some(text(
                            line["ANR in ".len()..]
                                .split([' ', '('])
                                .next()
                                .unwrap_or_default(),
                        ));
                    }
                    _ => {}
                }
                self.open.push(incident);
                continue;
            }
            let Some(index) = self.open.iter().rposition(|incident| {
                incident.writer_pid == Some(log.pid)
                    && matches!(
                        (incident.kind, log.tag),
                        (Kind::JavaCrash, "AndroidRuntime")
                            | (Kind::NativeCrash, "DEBUG")
                            | (Kind::Anr, "ActivityManager")
                    )
            }) else {
                continue;
            };
            let incident = &mut self.open[index];
            incident.last_activity = seen;
            incident.last_seq = row.seq;
            let done = match incident.kind {
                Kind::JavaCrash => {
                    java_line(incident, line);
                    false
                }
                Kind::NativeCrash => {
                    native_line(incident, line);
                    false
                }
                Kind::Anr => anr_line(incident, line),
            };
            if done {
                let incident = self.open.remove(index);
                self.close(incident);
            }
        }
        Ok(())
    }

    fn close(&mut self, incident: Incident) {
        self.incidents += 1;
        let (signature, headline, cause, frames) = signature(&incident);
        let key = (incident.kind, signature);
        if let Some(&index) = self.index.get(&key) {
            let group = &mut self.groups[index];
            group.occurrences += 1;
            group.last_seq = group.last_seq.max(incident.last_seq);
            if let Some(pid) = incident.pid
                && !group.pids.contains(&pid)
                && group.pids.len() < 4
            {
                group.pids.push(pid);
            }
            return;
        }
        if self.groups.len() >= MAX_GROUPS {
            return;
        }
        self.index.insert(key.clone(), self.groups.len());
        self.groups.push(Group {
            kind: incident.kind,
            signature: key.1,
            occurrences: 1,
            first_seq: incident.first_seq,
            last_seq: incident.last_seq,
            first_ref: incident.first_ref,
            first_observed_unix_ms: incident.observed_unix_ms,
            pids: incident.pid.into_iter().collect(),
            process: incident.process,
            thread: incident.thread,
            headline,
            cause,
            frames,
        });
    }

    /// Close every open incident and return groups ordered by first record.
    pub(super) fn finish(mut self) -> (Vec<Group>, u64) {
        for incident in std::mem::take(&mut self.open) {
            self.close(incident);
        }
        self.groups.sort_by_key(|group| group.first_seq);
        (self.groups, self.incidents)
    }
}

fn java_line(incident: &mut Incident, line: &str) {
    if let Some(rest) = line.strip_prefix("Process: ") {
        // "Process: com.example, PID: 1234"; process names never contain ','.
        let name = rest.split(',').next().unwrap_or(rest);
        incident.process = Some(text(name));
    } else if let Some(frame) = line.strip_prefix("at ") {
        if incident.sections.is_empty() {
            incident.sections.push(Section::default());
        }
        let section = incident.sections.last_mut().unwrap();
        if section.frames.len() < MAX_FRAMES {
            section.frames.push(text(frame));
        }
    } else if let Some(cause) = line.strip_prefix("Caused by: ") {
        if incident.sections.len() >= MAX_SECTIONS {
            // Keep the outermost exception and the deepest cause seen.
            incident.sections.pop();
        }
        incident.sections.push(Section {
            header: text(cause),
            frames: Vec::new(),
        });
    } else if incident.sections.is_empty() && !line.is_empty() && !line.starts_with("...") {
        incident.sections.push(Section {
            header: text(line),
            frames: Vec::new(),
        });
    }
}

fn native_signal(incident: &mut Incident, line: &str) {
    // Fatal signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0 in tid 4321 (RenderThread), pid 4300 (com.example)
    incident.signal = between(line, "(", ")").map(text);
    incident.headline_fallback(line);
    if let Some(rest) = line.rsplit_once(", pid ").map(|(_, rest)| rest) {
        let pid = rest.split(' ').next().and_then(|pid| pid.parse().ok());
        incident.pid = pid;
        incident.process = between(rest, "(", ")").map(text);
    }
    if let Some(rest) = line.split_once(" in tid ").map(|(_, rest)| rest) {
        incident.thread = between(rest, "(", ")").map(text);
    }
}

fn native_line(incident: &mut Incident, line: &str) {
    if line.starts_with("pid: ") && line.contains(">>> ") {
        incident.pid = line["pid: ".len()..]
            .split(',')
            .next()
            .and_then(|pid| pid.trim().parse().ok());
        incident.process = between(line, ">>> ", " <<<").map(text);
        if let Some(name) = between(line, "name: ", "  ") {
            incident.thread.get_or_insert_with(|| text(name));
        }
    } else if line.starts_with("signal ") && incident.signal.is_none() {
        incident.signal = between(line, "(", ")").map(text);
    } else if let Some(message) = line.strip_prefix("Abort message: ") {
        incident.abort = Some(text(message.trim_matches('\'')));
    } else if line.starts_with('#')
        && let Some((_, frame)) = line.split_once(" pc ")
    {
        // "#00 pc 000abc  /system/lib64/libc.so (abort+164) (BuildId: …)"
        let frame = frame.trim_start();
        let frame = frame.split_once(char::is_whitespace).map_or("", |(_, f)| f);
        let frame = frame.split(" (BuildId:").next().unwrap_or(frame);
        if incident.sections.is_empty() {
            incident.sections.push(Section::default());
        }
        let section = &mut incident.sections[0];
        if section.frames.len() < MAX_FRAMES && !frame.trim().is_empty() {
            section.frames.push(text(frame));
        }
    }
}

fn anr_line(incident: &mut Incident, line: &str) -> bool {
    if let Some(pid) = line.strip_prefix("PID: ") {
        incident.pid = pid.trim().parse().ok().or(incident.pid);
    } else if let Some(reason) = line.strip_prefix("Reason: ") {
        incident.reason = Some(text(reason));
        return true;
    }
    false
}

impl Incident {
    fn headline_fallback(&mut self, line: &str) {
        if self.sections.is_empty() {
            self.sections.push(Section {
                header: text(line),
                frames: Vec::new(),
            });
        }
    }
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let (_, rest) = text.split_once(open)?;
    Some(rest.split_once(close).map_or(rest, |(inner, _)| inner))
}

/// The balanced parenthesised symbol after a native frame's path, which may
/// itself contain parentheses (`Game::tick()+48`).
fn native_symbol(frame: &str) -> Option<&str> {
    let (_, rest) = frame.split_once(" (")?;
    let mut depth = 1usize;
    for (index, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[..index]);
                }
            }
            _ => {}
        }
    }
    Some(rest)
}

/// Drop "+offset" from a native symbol so rebuilt layouts group together.
fn native_frame_key(frame: &str) -> String {
    let path = frame.split(" (").next().unwrap_or(frame);
    let library = path.rsplit('/').next().unwrap_or(path);
    let symbol = native_symbol(frame).map(|symbol| match symbol.rsplit_once('+') {
        Some((name, offset)) if offset.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => symbol,
    });
    match symbol {
        Some(symbol) => format!("{library} ({symbol})"),
        None => library.to_owned(),
    }
}

fn digits_to_hash(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_digit() {
            if !output.ends_with('#') {
                output.push('#');
            }
        } else {
            output.push(c);
        }
    }
    output
}

type Signature = (String, Option<String>, Option<String>, Vec<String>);

fn signature(incident: &Incident) -> Signature {
    let headline = incident.sections.first().map(|s| s.header.clone());
    let headline = headline.filter(|h| !h.is_empty());
    match incident.kind {
        Kind::JavaCrash => {
            let root = incident
                .sections
                .iter()
                .rev()
                .find(|s| !s.header.is_empty())
                .or(incident.sections.first());
            let class = root
                .map(|s| s.header.split(':').next().unwrap_or("").trim().to_owned())
                .filter(|class| !class.is_empty())
                .unwrap_or_else(|| "unknown exception".into());
            let frames = root
                .filter(|s| !s.frames.is_empty())
                .or(incident.sections.first())
                .map(|s| s.frames.clone())
                .unwrap_or_default();
            let signature = match frames.first() {
                Some(frame) => format!("{class} @ {frame}"),
                None => class,
            };
            let cause = (incident.sections.len() > 1)
                .then(|| root.map(|s| s.header.clone()))
                .flatten();
            (
                signature,
                headline,
                cause,
                frames.into_iter().take(OUTPUT_FRAMES).collect(),
            )
        }
        Kind::NativeCrash => {
            let frames = incident
                .sections
                .first()
                .map(|s| s.frames.clone())
                .unwrap_or_default();
            let keyed = frames
                .iter()
                .map(|frame| native_frame_key(frame))
                .find(|key| !NATIVE_PLUMBING.iter().any(|lib| key.starts_with(lib)))
                .or_else(|| frames.first().map(|frame| native_frame_key(frame)));
            let signal = incident
                .signal
                .clone()
                .unwrap_or_else(|| "native crash".into());
            let signature = match keyed {
                Some(frame) => format!("{signal} @ {frame}"),
                None => signal,
            };
            let headline = incident.abort.clone().or(headline);
            (
                signature,
                headline,
                None,
                frames.into_iter().take(OUTPUT_FRAMES).collect(),
            )
        }
        Kind::Anr => {
            let process = incident.process.clone().unwrap_or_default();
            let reason = incident.reason.as_deref().map(digits_to_hash);
            let signature = match &reason {
                Some(reason) => format!("ANR in {process}: {}", preview(reason, 160)),
                None => format!("ANR in {process}"),
            };
            (signature, incident.reason.clone(), None, Vec::new())
        }
    }
}
