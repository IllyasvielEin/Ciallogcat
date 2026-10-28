//! Bounded detail search and stable navigation references for the GUI.
use ciallogcat::{WakeSignal, session::RecordIdentity};
use regex::Regex;
use std::{
    ops::Range,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub const MAX_DETAIL_HITS: usize = 4096;

#[derive(Clone, Copy)]
pub struct RecordRef {
    pub epoch: u64,
    pub absolute: usize,
    pub identity: Option<RecordIdentity>,
}

pub struct ContextView {
    pub rows: Vec<usize>,
    pub origin: RecordRef,
    pub viewport: Option<RecordRef>,
    pub fraction: f32,
    pub reveal_after_notice: bool,
}

pub struct HitResult {
    pub generation: u64,
    pub hits: Vec<Range<usize>>,
    pub capped: bool,
}

struct Request {
    generation: u64,
    message: Arc<str>,
    matcher: Regex,
}
#[derive(Default)]
struct Pending {
    request: Option<Request>,
    closed: bool,
}
#[derive(Default)]
struct Mailbox {
    pending: Mutex<Pending>,
    ready: Condvar,
    revision: AtomicU64,
    result: Mutex<Option<HitResult>>,
}
pub struct DetailWorker(Arc<Mailbox>);
impl DetailWorker {
    pub fn new(wake: WakeSignal) -> Self {
        let mailbox = Arc::new(Mailbox::default());
        let worker = Arc::clone(&mailbox);
        std::thread::Builder::new()
            .name("detail-search".into())
            .spawn(move || {
                loop {
                    let request = {
                        let mut pending = worker.pending.lock().unwrap();
                        while pending.request.is_none() && !pending.closed {
                            pending = worker.ready.wait(pending).unwrap();
                        }
                        if pending.closed {
                            break;
                        }
                        pending.request.take().unwrap()
                    };
                    let mut hits = Vec::new();
                    let mut capped = false;
                    for found in request.matcher.find_iter(&request.message) {
                        if worker.revision.load(Ordering::Acquire) != request.generation {
                            break;
                        }
                        if hits.len() == MAX_DETAIL_HITS {
                            capped = true;
                            break;
                        }
                        hits.push(found.range());
                    }
                    if worker.revision.load(Ordering::Acquire) == request.generation {
                        *worker.result.lock().unwrap() = Some(HitResult {
                            generation: request.generation,
                            hits,
                            capped,
                        });
                        wake.notify();
                    }
                }
            })
            .expect("failed to start detail-search worker");
        Self(mailbox)
    }
    pub fn submit(&self, message: Arc<str>, matcher: Regex) -> u64 {
        let generation = self
            .0
            .revision
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.0.pending.lock().unwrap().request = Some(Request {
            generation,
            message,
            matcher,
        });
        self.0.ready.notify_one();
        generation
    }
    pub fn cancel(&self) {
        self.0.revision.fetch_add(1, Ordering::AcqRel);
        self.0.pending.lock().unwrap().request = None;
        self.0.result.lock().unwrap().take();
    }
    pub fn take(&self) -> Option<HitResult> {
        self.0.result.lock().unwrap().take()
    }
}
impl Drop for DetailWorker {
    fn drop(&mut self) {
        self.cancel();
        self.0.pending.lock().unwrap().closed = true;
        self.0.ready.notify_one();
    }
}

pub struct DetailState {
    pub worker: DetailWorker,
    pub key: Option<(Arc<str>, (String, bool, bool))>,
    pub generation: u64,
    pub pending: bool,
    pub hits: Vec<Range<usize>>,
    pub capped: bool,
    pub current: usize,
    pub jump: bool,
}
impl DetailState {
    pub fn new(wake: WakeSignal) -> Self {
        Self {
            worker: DetailWorker::new(wake),
            key: None,
            generation: 0,
            pending: false,
            hits: Vec::new(),
            capped: false,
            current: 0,
            jump: false,
        }
    }
    pub fn reset(&mut self) {
        self.worker.cancel();
        self.key = None;
        self.pending = false;
        self.hits.clear();
        self.capped = false;
        self.current = 0;
        self.jump = false;
    }
}
