use capture_controller::{
    Command as CaptureCommand, Controller as CaptureController, Intent as CaptureIntent,
    Status as ControllerStatus, Target as CaptureTarget,
};
use ciallogcat::capture_controller;
use ciallogcat::{
    WakeSignal, adb, filter, log_store, model,
    session::{Session, SessionId},
};
mod level_color;
use level_color::LevelColor;
mod capture_ui;
mod config;
mod copy;
mod diagnostics_ui;
mod file_ui;
mod font_fallback;
mod history;
mod log_table_ui;
#[cfg(test)]
mod performance;
mod process_watch;
mod reading;
mod reading_ui;
mod selection;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    mpsc::{self, Receiver, SyncSender},
};
use std::time::{Duration, Instant};

use adb::{BackendEvent, DeviceInfo, LogcatEvent, LogcatHandle};
use config::AppConfig;
use eframe::egui::{
    self, Align, Color32, FontFamily, FontId, Layout, RichText, TextFormat, TextStyle,
    text::LayoutJob,
};
use egui_extras::{Column, TableBuilder};
use filter::{FilterRequest, FilterSpec};
use log_store::LogStore;
use model::{Level, LogBuffer, LogEntry, LogEntryKind, ProcessSource, SavedFilter};
use regex::Regex;

const MIB: usize = 1024 * 1024;
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(750);
// A host ps sample can only suggest a recent name, never prove PID identity.
const PROCESS_HINT_TTL: Duration = Duration::from_secs(2);
const FILTER_DEBOUNCE: Duration = Duration::from_millis(120);
const CONFIG_SAVE_DELAY: Duration = Duration::from_millis(600);
const APP_FONT_NAME: &str = "Cascadia Next SC NF";
const BACKEND_QUEUE_CAPACITY: usize = 4096;
const BACKEND_FRAME_BUDGET: Duration = Duration::from_millis(4);
const BACKEND_EVENTS_PER_FRAME: usize = 2048;
const FILTER_CATCHUP_ROWS: usize = 2048;
const FILTER_CATCHUP_BYTES: usize = 2 * MIB;
const FILTER_CATCHUP_BUDGET: Duration = Duration::from_millis(2);

#[derive(Clone, Copy)]
struct FilterProgress {
    generation: u64,
    log_epoch: u64,
    next_absolute: usize,
}

#[derive(Clone, Copy)]
enum CopyTarget {
    Selection,
    Row(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CopyStamp {
    log_epoch: u64,
    retained_base: usize,
    filter_generation: u64,
    filter_draft: Option<Instant>,
    selection_revision: u64,
    selected: Option<usize>,
}

fn memory_budget_bytes(mib: usize) -> usize {
    (mib as u64)
        .saturating_mul(MIB as u64)
        .min(usize::MAX as u64) as usize
}

fn wake_signal(ctx: &egui::Context) -> WakeSignal {
    let ctx = ctx.clone();
    WakeSignal::new(move || ctx.request_repaint())
}

fn main() -> eframe::Result {
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    // Windows delivers title-bar movement on the same thread that presents
    // frames. CIALLOGCAT_VSYNC=0 opts into a non-vsync experiment for comparing
    // movement latency. Keep the verified eframe default otherwise: the
    // experimental mode produced blank native captures on the tested driver.
    if cfg!(target_os = "windows") && std::env::var("CIALLOGCAT_VSYNC").as_deref() == Ok("0") {
        options.wgpu_options.surface.present_mode = eframe::wgpu::PresentMode::AutoNoVsync;
    }
    let surface = options.wgpu_options.surface;

    eframe::run_native(
        "Ciallogcat",
        options,
        Box::new(move |cc| {
            if let Some(path) = std::env::var_os("CIALLOGCAT_GRAPHICS_REPORT")
                && let Some(state) = &cc.wgpu_render_state
            {
                let adapter = state.adapter.get_info();
                let report = format!(
                    "adapter={}\nbackend={:?}\ndevice_type={:?}\ndriver={}\ndriver_info={}\nrequested_present_mode={:?}\nrequested_frame_latency={:?}\n",
                    adapter.name,
                    adapter.backend,
                    adapter.device_type,
                    adapter.driver,
                    adapter.driver_info,
                    surface.present_mode,
                    surface.desired_maximum_frame_latency
                );
                if let Err(error) = std::fs::write(path, report) {
                    eprintln!("Failed to write graphics report: {error}");
                }
            }
            Ok(Box::new(CiallogcatApp::new(cc)))
        }),
    )
}

#[derive(Clone, Debug)]
enum CaptureState {
    NoAdb,
    WaitingForDevice,
    Connecting,
    Running,
    Stopping,
    Reconnecting {
        deadline: Instant,
        retry: u8,
        error: String,
    },
    Paused,
    Error(String),
}

impl CaptureState {
    fn label(&self) -> &str {
        match self {
            Self::NoAdb => "未找到 ADB",
            Self::WaitingForDevice => "等待设备",
            Self::Connecting => "正在启动 Logcat",
            Self::Running => "实时 Logcat",
            Self::Stopping => "正在停止并接收剩余日志",
            Self::Reconnecting { .. } => "等待自动重连",
            Self::Paused => "已暂停采集",
            Self::Error(error) => error,
        }
    }

    fn color(&self) -> Color32 {
        match self {
            Self::Running => Color32::from_rgb(76, 194, 126),
            Self::Connecting | Self::Stopping | Self::Reconnecting { .. } => {
                Color32::from_rgb(239, 190, 82)
            }
            Self::Error(_) | Self::NoAdb => Color32::from_rgb(235, 91, 101),
            Self::WaitingForDevice => Color32::from_rgb(142, 151, 166),
            Self::Paused => Color32::from_rgb(239, 190, 82),
        }
    }
}

struct CiallogcatApp {
    entries: Arc<LogStore>,
    matches: Vec<usize>,
    selected: Option<usize>,
    selection: selection::RowSelection,
    log_columns: log_table_ui::LogColumns,
    focused_log_row: Option<(egui::Id, usize)>,
    focus_log_selection: bool,
    visible_log_rows: Vec<(egui::Id, usize)>,
    processes: HashMap<u32, Arc<str>>,
    process_snapshot: Option<(Instant, Instant)>,
    process_watch: process_watch::Watcher,
    unresolved_count: usize,
    dropped_entries: usize,
    log_epoch: u64,
    memory_limit_mib: usize,
    memory_limit_draft: Option<usize>,
    copy_notice: Option<String>,
    copy_worker: copy::CopyWorker,
    copy_pending: Option<(u64, CopyStamp)>,
    copy_ready: Option<String>,
    table_scroll_offset: f32,
    table_row_stride: f32,
    restore_scroll_offset: Option<f32>,
    buffers: Vec<LogBuffer>,
    buffer_draft: Vec<LogBuffer>,
    capture_scope: ciallogcat::capture_scope::CaptureScope,
    scope_draft: capture_ui::ScopeDraft,
    capture_attempt_scope: ciallogcat::capture_scope::CaptureScope,
    snapshot_budget: Option<capture_ui::SnapshotBudget>,
    reset_scope_budget: bool,

    package: String,
    tag_include: String,
    tag_exclude: String,
    package_popup_open: bool,
    packages: Vec<String>,
    min_level: Level,
    query: String,
    use_regex: bool,
    case_sensitive: bool,
    active_filter: FilterSpec,
    incremental_matcher: Result<Option<Regex>, String>,
    filter_tx: filter::FilterSender,
    filter_rx: filter::FilterReceiver,
    filter_generation: u64,
    filter_pending: bool,
    filter_progress: Option<FilterProgress>,
    filter_dirty_since: Option<Instant>,
    filter_error: Option<String>,
    filter_elapsed: Duration,
    highlight_key: (String, bool, bool),
    highlight_matcher: Option<Regex>,
    detail_lines: Option<(Arc<str>, Vec<std::ops::Range<usize>>)>,
    detail_search: reading::DetailState,
    context_view: Option<reading::ContextView>,

    dark: bool,
    row_height: f32,
    show_details: bool,
    follow_logs: bool,
    scroll_to_bottom: bool,
    scroll_to_selected: bool,

    backend_tx: SyncSender<BackendEvent>,
    backend_rx: Receiver<BackendEvent>,
    adb_path: Option<PathBuf>,
    device_query_error: Option<String>,
    package_query_error: Option<String>,
    process_query_error: Option<String>,
    devices: Vec<DeviceInfo>,
    selected_device: Option<String>,
    retained_device: Option<String>,
    pending_clear: bool,
    pending_device: Option<String>,
    device_query_pending: bool,
    package_query_pending: bool,
    process_query_pending: bool,
    last_device_query: Instant,
    last_process_query: Instant,
    capture_handle: Option<LogcatHandle>,
    capture_controller: CaptureController,
    live_session: Session,
    capture_facts: ciallogcat::capture_facts::CaptureFacts,
    capture_source_session: Option<SessionId>,
    files: file_ui::FileUiState,
    diagnostics: diagnostics_ui::State,
    capture_session_id: u64,
    capture_state: CaptureState,
    last_capture_error: Option<String>,

    logs_in_rate_window: usize,
    rate_window_started: Instant,
    logs_per_second: f64,

    saved_filters: Vec<SavedFilter>,
    filter_history: Vec<String>,
    query_history_pending: bool,
    query_history_open: bool,
    show_save_filter: bool,
    save_filter_name: String,
    save_filter_focused: bool,
    config_dirty_since: Option<Instant>,
    config_error: Option<String>,
}

impl CiallogcatApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::with_config(&cc.egui_ctx, AppConfig::load(), adb::find_adb())
    }

    fn with_config(ctx: &egui::Context, config: AppConfig, adb_path: Option<PathBuf>) -> Self {
        configure_fonts(ctx);
        configure_style(ctx, config.dark);

        let (filter_tx, filter_rx) = filter::spawn_filter_worker(wake_signal(ctx));

        let (backend_tx, backend_rx) = mpsc::sync_channel::<BackendEvent>(BACKEND_QUEUE_CAPACITY);
        let active_filter = FilterSpec {
            package: config.package.clone(),
            tag_include: config.tag_include.clone(),
            tag_exclude: config.tag_exclude.clone(),
            min_level: config.min_level,
            query: config.query.clone(),
            regex: config.use_regex,
            case_sensitive: config.case_sensitive,
        };
        let incremental_matcher = Ok(None);
        let now = Instant::now();
        let has_adb = adb_path.is_some();

        let mut app = Self {
            entries: Arc::new(LogStore::default()),
            matches: Vec::new(),
            selected: None,
            selection: selection::RowSelection::default(),
            log_columns: log_table_ui::LogColumns::default(),
            focused_log_row: None,
            focus_log_selection: false,
            visible_log_rows: Vec::new(),
            processes: HashMap::new(),
            process_snapshot: None,
            process_watch: process_watch::Watcher::default(),
            unresolved_count: 0,
            dropped_entries: 0,
            log_epoch: 0,
            memory_limit_mib: config.memory_limit_mib.clamp(16, 4096),
            memory_limit_draft: None,
            copy_notice: None,
            copy_worker: copy::CopyWorker::new(wake_signal(ctx)),
            copy_pending: None,
            copy_ready: None,
            table_scroll_offset: 0.0,
            table_row_stride: 0.0,
            restore_scroll_offset: None,
            buffers: config.buffers.clone(),
            buffer_draft: config.buffers,
            scope_draft: capture_ui::ScopeDraft::from(&config.capture_scope),
            capture_scope: config.capture_scope,
            capture_attempt_scope: ciallogcat::capture_scope::CaptureScope::default(),
            snapshot_budget: None,
            reset_scope_budget: true,

            package: config.package,
            tag_include: config.tag_include,
            tag_exclude: config.tag_exclude,
            package_popup_open: false,
            packages: Vec::new(),
            min_level: config.min_level,
            query: config.query,
            use_regex: config.use_regex,
            case_sensitive: config.case_sensitive,
            active_filter,
            incremental_matcher,
            filter_tx,
            filter_rx,
            filter_generation: 0,
            filter_pending: false,
            filter_progress: Some(FilterProgress {
                generation: 0,
                log_epoch: 0,
                next_absolute: 0,
            }),
            filter_dirty_since: None,
            filter_error: None,
            filter_elapsed: Duration::ZERO,
            highlight_key: (String::new(), false, false),
            highlight_matcher: None,
            detail_lines: None,
            detail_search: reading::DetailState::new(wake_signal(ctx)),
            context_view: None,

            dark: config.dark,
            row_height: config.row_height,
            show_details: config.show_details,
            follow_logs: true,
            scroll_to_bottom: false,
            scroll_to_selected: false,

            backend_tx,
            backend_rx,
            adb_path,
            device_query_error: None,
            package_query_error: None,
            process_query_error: None,
            devices: Vec::new(),
            selected_device: None,
            retained_device: None,
            pending_clear: false,
            pending_device: None,
            device_query_pending: false,
            package_query_pending: false,
            process_query_pending: false,
            last_device_query: now.checked_sub(DEVICE_POLL_INTERVAL).unwrap_or(now),
            last_process_query: now.checked_sub(PROCESS_POLL_INTERVAL).unwrap_or(now),
            capture_handle: None,
            capture_controller: CaptureController::default(),
            live_session: Session::new(),
            capture_facts: ciallogcat::capture_facts::CaptureFacts::default(),
            capture_source_session: None,
            files: file_ui::FileUiState::default(),
            diagnostics: diagnostics_ui::State::default(),
            capture_session_id: 0,
            capture_state: if has_adb {
                CaptureState::WaitingForDevice
            } else {
                CaptureState::NoAdb
            },
            last_capture_error: None,

            logs_in_rate_window: 0,
            rate_window_started: now,
            logs_per_second: 0.0,

            saved_filters: config.saved_filters,
            filter_history: config.filter_history,
            query_history_pending: false,
            query_history_open: false,
            show_save_filter: false,
            save_filter_name: String::new(),
            save_filter_focused: false,
            config_dirty_since: None,
            config_error: None,
        };
        if !app.query.is_empty() {
            app.schedule_filter();
        }
        app
    }

    fn current_filter(&self) -> FilterSpec {
        FilterSpec {
            package: self.package.trim().to_owned(),
            tag_include: self.tag_include.clone(),
            tag_exclude: self.tag_exclude.clone(),
            min_level: self.min_level,
            query: self.query.clone(),
            regex: self.use_regex,
            case_sensitive: self.case_sensitive,
        }
    }

    fn queue_filter(&mut self) {
        self.leave_context();
        self.filter_dirty_since = Some(Instant::now());
        self.mark_config_dirty();
    }

    fn schedule_filter(&mut self) {
        self.leave_context();
        self.filter_dirty_since = None;
        self.active_filter = self.current_filter();
        self.restart_filter_snapshot();
    }

    fn restart_filter_snapshot(&mut self) {
        self.filter_generation = self.filter_generation.wrapping_add(1);
        self.filter_pending = true;
        self.filter_progress = None;
        self.filter_error = None;
        self.filter_tx.send(FilterRequest {
            generation: self.filter_generation,
            log_epoch: self.log_epoch,
            snapshot_base: self.dropped_entries,
            entries: Arc::clone(&self.entries),
            spec: self.active_filter.clone(),
        });
    }

    fn prune_classified_selection(&mut self, range: std::ops::Range<usize>) {
        if self.context_view.is_some() {
            return;
        }
        self.selection
            .retain_classified(&self.matches, range.clone());
        if self.selected.is_some_and(|index| {
            range.contains(&index) && self.matches.binary_search(&index).is_err()
        }) {
            self.selected = None;
            self.detail_lines = None;
        }
        if self.focused_log_row.is_some_and(|(_, index)| {
            range.contains(&index) && self.matches.binary_search(&index).is_err()
        }) {
            self.focused_log_row = None;
        }
    }

    fn accept_filter_result(&mut self, mut result: filter::FilterResult) {
        if result.generation != self.filter_generation || result.log_epoch != self.log_epoch {
            return;
        }
        let Some(removed) = self.dropped_entries.checked_sub(result.snapshot_base) else {
            return;
        };
        let Some(scanned_end) = result.snapshot_base.checked_add(result.processed_len) else {
            return;
        };
        self.filter_elapsed = result.elapsed;
        self.filter_error = result.error;
        if let Some(error) = &self.filter_error {
            self.incremental_matcher = Err(error.clone());
            self.matches.clear();
            self.selected = None;
            self.selection.clear();
            self.detail_lines = None;
            self.filter_pending = false;
            self.filter_progress = None;
            return;
        }
        self.incremental_matcher = Ok(result.matcher);
        result.matches.retain_mut(|index| {
            if let Some(rebased) = index
                .checked_sub(removed)
                .filter(|index| *index < self.entries.len())
            {
                *index = rebased;
                true
            } else {
                false
            }
        });
        self.matches = result.matches;
        let next_absolute = scanned_end
            .max(self.dropped_entries)
            .min(self.dropped_entries + self.entries.len());
        self.filter_progress = Some(FilterProgress {
            generation: self.filter_generation,
            log_epoch: self.log_epoch,
            next_absolute,
        });
        self.prune_classified_selection(0..next_absolute - self.dropped_entries);
        self.filter_pending = next_absolute < self.dropped_entries + self.entries.len();
    }

    fn advance_filter_catchup(&mut self, max_rows: usize, max_bytes: usize, budget: Duration) {
        let Some(mut progress) = self.filter_progress else {
            return;
        };
        if progress.generation != self.filter_generation
            || progress.log_epoch != self.log_epoch
            || !self.filter_pending
        {
            return;
        }
        progress.next_absolute = progress.next_absolute.max(self.dropped_entries);
        let start = progress.next_absolute - self.dropped_entries;
        let end = self.entries.len().min(start.saturating_add(max_rows));
        let started = Instant::now();
        let mut bytes = 0usize;
        let mut next = start;
        if let Ok(matcher) = &self.incremental_matcher {
            while next < end {
                // Direct indexing is O(1): repeating iter().skip(cursor) here
                // would rescan the prefix and starve a long moving tail.
                let entry = &self.entries[next];
                if filter::entry_matches(entry, &self.active_filter, matcher.as_ref()) {
                    self.matches.push(next);
                }
                bytes = bytes.saturating_add(entry.payload_bytes());
                next += 1;
                if bytes >= max_bytes || started.elapsed() >= budget {
                    break;
                }
            }
        }
        progress.next_absolute = self.dropped_entries + next;
        self.filter_progress = Some(progress);
        self.prune_classified_selection(start..next);
        self.filter_pending = next < self.entries.len();
    }

    fn poll_filter(&mut self) {
        while let Some(result) = self.filter_rx.take_highlight() {
            if result.key.0 == self.query
                && result.key.1 == self.use_regex
                && result.key.2 == self.case_sensitive
            {
                self.highlight_matcher = result.matcher;
                self.highlight_key = result.key;
            }
        }
        while let Some(result) = self.filter_rx.take_filter() {
            self.accept_filter_result(result);
        }
        self.advance_filter_catchup(
            FILTER_CATCHUP_ROWS,
            FILTER_CATCHUP_BYTES,
            FILTER_CATCHUP_BUDGET,
        );
    }

    fn classification_pending(&self, index: usize) -> bool {
        if self.context_view.is_some() {
            return false;
        }
        self.filter_progress.is_none_or(|progress| {
            progress.generation != self.filter_generation
                || progress.log_epoch != self.log_epoch
                || self.dropped_entries + index >= progress.next_absolute
        })
    }

    fn copy_logs(&mut self, ctx: &egui::Context, target: CopyTarget) {
        self.cancel_copy();
        let last = match target {
            CopyTarget::Selection => self.selection.last(),
            CopyTarget::Row(index) => Some(index),
        };
        if last.is_some_and(|index| self.classification_pending(index)) {
            self.copy_notice = Some("Selection is still being checked. Copy again when filtering catches up; nothing was copied.".to_owned());
            return;
        }
        let background = match target {
            CopyTarget::Selection => {
                copy::requires_background(&self.entries, self.selection.indices())
            }
            CopyTarget::Row(index) => {
                copy::requires_background(&self.entries, std::iter::once(index))
            }
        };
        match background {
            Ok(true) => {
                let indices = match target {
                    CopyTarget::Selection => self.selection.indices().collect(),
                    CopyTarget::Row(index) => vec![index],
                };
                let generation = self.copy_worker.submit(Arc::clone(&self.entries), indices);
                self.copy_pending = Some((generation, self.copy_stamp()));
                self.copy_notice = None;
                return;
            }
            Err(error) => {
                self.copy_notice = Some(error.to_string());
                return;
            }
            Ok(false) => {}
        }
        let result = match target {
            CopyTarget::Selection => copy::build_copy(
                &self.entries,
                self.selection.indices(),
                copy::MAX_COPY_BYTES,
            ),
            CopyTarget::Row(index) => {
                copy::build_copy(&self.entries, std::iter::once(index), copy::MAX_COPY_BYTES)
            }
        };
        match result {
            Ok(text) if !text.is_empty() => {
                self.copy_notice = None;
                ctx.copy_text(text);
            }
            Ok(_) => {}
            Err(error) => self.copy_notice = Some(error.to_string()),
        }
    }

    fn copy_stamp(&self) -> CopyStamp {
        CopyStamp {
            log_epoch: self.log_epoch,
            retained_base: self.dropped_entries,
            filter_generation: self.filter_generation,
            filter_draft: self.filter_dirty_since,
            selection_revision: self.selection.revision(),
            selected: self.selected,
        }
    }

    fn cancel_copy(&mut self) {
        self.copy_worker.cancel();
        self.copy_pending = None;
        self.copy_ready = None;
    }

    fn poll_copy(&mut self) {
        if self
            .copy_pending
            .is_some_and(|(_, stamp)| stamp != self.copy_stamp())
        {
            self.cancel_copy();
            self.copy_notice = Some("选择或来源已变化，复制准备已取消；剪贴板保持不变。".into());
            return;
        }
        if let Some(prepared) = self.copy_worker.take()
            && self
                .copy_pending
                .is_some_and(|(generation, _)| generation == prepared.generation)
        {
            match prepared.result {
                Ok(text) => self.copy_ready = Some(text),
                Err(error) => {
                    self.copy_pending = None;
                    self.copy_notice = Some(error.to_string());
                }
            }
        }
    }

    fn commit_prepared_copy(&mut self, ctx: &egui::Context) {
        self.poll_copy();
        if let Some(text) = self.copy_ready.take() {
            self.copy_pending = None;
            self.copy_notice = None;
            ctx.copy_text(text);
        }
    }

    fn apply_memory_budget(&mut self, draft: usize) {
        let applied = draft.clamp(16, 4096);
        self.memory_limit_draft = None;
        if self.files.document.is_some() && self.files.document_bytes > memory_budget_bytes(applied)
        {
            self.files.notice =
                Some("当前文件超出此预算；请先关闭文件。离线文件不会被静默裁剪。".into());
            return;
        }
        if applied == self.memory_limit_mib {
            return;
        }
        let downshift = applied < self.memory_limit_mib;
        self.memory_limit_mib = applied;
        if self.entries.memory_bytes() > memory_budget_bytes(applied) {
            self.trim_old_entries();
        }
        // Capacity reclamation belongs at an explicit lifecycle boundary, not
        // every hysteresis trim. Allocator RSS need not immediately shrink.
        if downshift {
            self.matches.shrink_to_fit();
            if self.filter_pending && self.filter_progress.is_none() {
                self.restart_filter_snapshot();
            }
        }
        self.mark_config_dirty();
    }

    #[cfg(test)]
    fn append_entry(&mut self, entry: LogEntry) {
        self.append_received_entry(entry, Instant::now());
    }

    fn append_received_entry(&mut self, mut entry: LogEntry, received_at: Instant) {
        self.expire_processes(Instant::now());
        if entry.kind == LogEntryKind::Log
            && self.capture_attempt_scope == ciallogcat::capture_scope::CaptureScope::default()
            && entry.process.is_none()
            && entry.pid != 0
            && self.process_snapshot.is_some_and(|(started, completed)| {
                received_at >= completed
                    && received_at
                        .checked_duration_since(started)
                        .is_some_and(|age| age <= PROCESS_HINT_TTL)
            })
            && let Some(process) = self.processes.get(&entry.pid)
        {
            entry.process = Some(Arc::clone(process));
            entry.process_source = Some(ProcessSource::PsSnapshot);
        }

        let index = self.entries.len();
        if entry.is_unresolved_process() {
            self.unresolved_count += 1;
        }
        let matches = !self.filter_pending
            && self.incremental_matcher.as_ref().is_ok_and(|matcher| {
                filter::entry_matches(&entry, &self.active_filter, matcher.as_ref())
            });
        Arc::make_mut(&mut self.entries).push(entry);
        if matches {
            self.matches.push(index);
        }
        if !self.filter_pending
            && let Some(progress) = &mut self.filter_progress
        {
            progress.next_absolute = self.dropped_entries + self.entries.len();
        }
        self.logs_in_rate_window += 1;

        if self.entries.memory_bytes() > memory_budget_bytes(self.memory_limit_mib) {
            self.trim_old_entries();
        }
    }

    fn trim_old_entries(&mut self) {
        let target = (memory_budget_bytes(self.memory_limit_mib) as u64 * 9 / 10) as usize;
        let remove = Arc::make_mut(&mut self.entries).trim_to_bytes(target);
        self.rebase_after_trim(remove);
    }

    fn rebase_after_trim(&mut self, remove: usize) {
        if remove == 0 {
            return;
        }
        let removed_context = self
            .context_view
            .as_ref()
            .map(|context| context.rows.partition_point(|index| *index < remove));
        if let Some(context) = &mut self.context_view {
            context.rows = context
                .rows
                .iter()
                .filter_map(|index| index.checked_sub(remove))
                .collect();
        }
        let removed_matches = self.matches.partition_point(|index| *index < remove);
        // Keep the previous viewport available even while following: a pointer
        // or cached accessibility action may pause follow before the next layout.
        // If follow stays on, log_table consumes and ignores this restoration.
        let previous = self
            .restore_scroll_offset
            .unwrap_or(self.table_scroll_offset);
        self.restore_scroll_offset = Some(
            (previous - removed_context.unwrap_or(removed_matches) as f32 * self.table_row_stride)
                .max(0.0),
        );
        self.dropped_entries += remove;
        self.capture_facts.retention_removed(remove);
        self.matches.drain(..removed_matches);
        for index in &mut self.matches {
            *index -= remove;
        }
        self.selection.rebase(remove);
        let expired_details = self.selected.is_some_and(|index| index < remove);
        self.selected = self.selected.and_then(|index| index.checked_sub(remove));
        self.focused_log_row = self
            .focused_log_row
            .and_then(|(id, index)| index.checked_sub(remove).map(|index| (id, index)));
        self.visible_log_rows
            .retain(|(_, absolute)| *absolute >= self.dropped_entries);
        if expired_details {
            self.detail_lines = None;
            self.copy_notice = Some("Selected log expired from the retained history.".to_owned());
        }
        if self.selected.is_none() {
            self.focus_log_selection = false;
        }
        self.unresolved_count = self.entries.unresolved_count();
        if let Some(progress) = &mut self.filter_progress {
            progress.next_absolute = progress.next_absolute.max(self.dropped_entries);
            self.filter_pending =
                progress.next_absolute < self.dropped_entries + self.entries.len();
        }
    }

    fn clear_processes(&mut self) {
        self.processes.clear();
        self.process_snapshot = None;
        self.process_watch.invalidate();
    }

    fn expire_processes(&mut self, now: Instant) {
        if self.process_snapshot.is_some_and(|(started, _)| {
            now.checked_duration_since(started)
                .is_none_or(|age| age >= PROCESS_HINT_TTL)
        }) {
            self.clear_processes();
        }
    }

    #[cfg(test)]
    fn update_processes(&mut self, processes: HashMap<u32, String>) {
        let now = Instant::now();
        self.update_process_snapshot(processes, now, now);
    }

    fn update_process_snapshot(
        &mut self,
        processes: HashMap<u32, String>,
        observed_from: Instant,
        observed_until: Instant,
    ) {
        if observed_until < observed_from || observed_until > Instant::now() {
            self.clear_processes();
            return;
        }
        if self.process_snapshot.is_none_or(|(started, _)| {
            observed_until.saturating_duration_since(started) >= PROCESS_HINT_TTL
        }) {
            self.process_watch.invalidate();
        }
        self.processes.clear();
        self.process_snapshot = Some((observed_from, observed_until));
        self.expire_processes(Instant::now());
        if self.process_snapshot.is_some() {
            self.processes = processes
                .into_iter()
                .map(|(pid, name)| (pid, Arc::from(name)))
                .collect();
            for change in self.process_watch.observe(&self.package, &self.processes) {
                self.capture_facts.process_observation(
                    serde_json::to_value(change).expect("bounded process observation"),
                    self.live_session.last_seq(),
                );
            }
        }
        // Never let a later PID sample rewrite an unknown historical record.
    }

    fn poll_backend(&mut self, ctx: &egui::Context) {
        self.poll_diagnostics(ctx);
        self.expire_processes(Instant::now());
        let started = Instant::now();
        for processed in 0..BACKEND_EVENTS_PER_FRAME {
            if started.elapsed() >= BACKEND_FRAME_BUDGET {
                ctx.request_repaint();
                break;
            }
            let Ok(event) = self.backend_rx.try_recv() else {
                break;
            };
            if processed + 1 == BACKEND_EVENTS_PER_FRAME {
                ctx.request_repaint();
            }
            match event {
                BackendEvent::Devices(result) => {
                    self.device_query_pending = false;
                    match result {
                        Ok(devices) => {
                            self.device_query_error = None;
                            self.devices = devices;
                            self.reconcile_device_selection(ctx);
                        }
                        Err(error) => {
                            // Discovery is a separate observation from the live
                            // stream. Keep the last successful list and intent.
                            self.device_query_error = Some(error);
                        }
                    }
                }
                BackendEvent::Packages { serial, result } => {
                    if self.selected_device.as_deref() != Some(&serial) {
                        continue;
                    }
                    self.package_query_pending = false;
                    match result {
                        Ok(packages) => {
                            self.packages = packages;
                            self.package_query_error = None;
                        }
                        Err(error) => self.package_query_error = Some(error),
                    }
                }
                BackendEvent::Processes {
                    serial,
                    session_id,
                    observed_from,
                    observed_until,
                    result,
                } => {
                    if self.selected_device.as_deref() != Some(&serial)
                        || session_id != self.capture_session_id
                        || self.capture_controller.active_id() != Some(session_id)
                        || self.capture_controller.is_stopping()
                    {
                        continue;
                    }
                    self.process_query_pending = false;
                    match result {
                        Ok(processes) => {
                            self.process_query_error = None;
                            self.update_process_snapshot(processes, observed_from, observed_until);
                        }
                        Err(error) => {
                            self.clear_processes();
                            self.process_query_error = Some(error);
                        }
                    }
                }
                BackendEvent::Logcat { session_id, event } => {
                    if self.capture_controller.active_id() != Some(session_id) {
                        continue;
                    }
                    match event {
                        LogcatEvent::Started => {
                            if self.capture_controller.started(session_id, Instant::now())
                                && self.accepts_capture_data(session_id)
                            {
                                if let Some(segment) = self.live_session.stream_started() {
                                    self.capture_facts
                                        .stream_started(segment, self.live_session.last_seq());
                                } else {
                                    self.halt_capture(
                                        "日志 segment 计数已耗尽，采集已停止。".to_owned(),
                                    );
                                }
                            }
                        }
                        LogcatEvent::Entry(entry) => {
                            if !self.accepts_capture_data(session_id) {
                                continue;
                            }
                            let received_at = entry.received_at();
                            let observed_unix_ms = entry.received_unix_ms();
                            let mut entry = entry.into_inner();
                            let retained_cost = self.entries.next_push_memory_bytes(&entry);
                            if self.capture_attempt_scope.is_dump()
                                && self.entries.memory_bytes().saturating_add(retained_cost)
                                    > memory_budget_bytes(self.memory_limit_mib)
                                && let Some(budget) = &mut self.snapshot_budget
                            {
                                budget.stop_reason.get_or_insert("snapshot_retention_limit");
                            }
                            if self.capture_attempt_scope.is_dump()
                                && let Some(budget) = &mut self.snapshot_budget
                                && !budget.admit(retained_cost, Instant::now())
                            {
                                self.capture_facts.record_discarded();
                                self.stop_snapshot_if_limited();
                                continue;
                            }
                            let Some(identity) = self.live_session.next_record(observed_unix_ms)
                            else {
                                self.halt_capture("无法分配唯一日志身份，采集已停止。".to_owned());
                                continue;
                            };
                            entry.provenance = Some(identity);
                            self.append_received_entry(entry, received_at);
                            self.stop_snapshot_if_limited();
                        }
                        LogcatEvent::Stderr(error) => self.last_capture_error = Some(error),
                        LogcatEvent::Exited { code, error, .. } => {
                            let completed_snapshot = self.capture_attempt_scope.is_dump()
                                && code == Some(0)
                                && error.is_none();
                            let message = error
                                .or_else(|| self.last_capture_error.take())
                                .unwrap_or_else(|| match code {
                                    Some(code) => format!("Logcat stopped ({code})"),
                                    None => "Logcat stopped".to_owned(),
                                });
                            if self.accepts_capture_data(session_id) {
                                if completed_snapshot && !self.capture_controller.is_stopping() {
                                    self.capture_facts
                                        .snapshot_completed(self.live_session.last_seq());
                                    self.files.notice =
                                        Some("历史快照已完成；未开启持续跟随。".into());
                                }
                                self.capture_facts.exited(
                                    code,
                                    &message,
                                    self.live_session.last_seq(),
                                );
                            }
                            let ended = if completed_snapshot {
                                self.capture_controller
                                    .completed(session_id, Instant::now())
                            } else {
                                self.capture_controller
                                    .exited(session_id, message, Instant::now())
                            };
                            if ended {
                                self.capture_handle = None;
                                self.clear_processes();
                                self.process_query_pending = false;
                            }
                        }
                    }
                }
            }
        }

        let elapsed = self.rate_window_started.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.logs_per_second = self.logs_in_rate_window as f64 / elapsed.as_secs_f64();
            self.logs_in_rate_window = 0;
            self.rate_window_started = Instant::now();
        }

        if self
            .filter_dirty_since
            .is_some_and(|started| started.elapsed() >= FILTER_DEBOUNCE)
        {
            self.schedule_filter();
        }
        if self
            .config_dirty_since
            .is_some_and(|started| started.elapsed() >= CONFIG_SAVE_DELAY)
        {
            self.persist_config();
        }

        self.advance_capture(ctx, Instant::now());
        self.schedule_periodic_queries(ctx);
    }

    fn schedule_periodic_queries(&mut self, ctx: &egui::Context) {
        let Some(adb_path) = self.adb_path.clone() else {
            return;
        };
        if !self.device_query_pending && self.last_device_query.elapsed() >= DEVICE_POLL_INTERVAL {
            self.device_query_pending = true;
            self.last_device_query = Instant::now();
            adb::spawn_device_query(adb_path.clone(), self.backend_tx.clone(), wake_signal(ctx));
        }
        if let Some(serial) = self.selected_device.clone()
            && self.selected_device_available()
            && self.capture_handle.is_some()
            && !self.capture_controller.is_stopping()
            && !self.process_query_pending
            && self.last_process_query.elapsed() >= PROCESS_POLL_INTERVAL
        {
            self.process_query_pending = true;
            self.last_process_query = Instant::now();
            adb::spawn_process_query(
                adb_path,
                serial,
                self.capture_session_id,
                self.backend_tx.clone(),
                wake_signal(ctx),
            );
        }
    }

    /// Deadlines for work owned by the UI thread. Workers notify after publishing
    /// results; waiting on them alone does not require a periodic pump.
    fn next_wake_deadline(&self, now: Instant) -> Option<Instant> {
        let mut deadline = None;
        let mut include = |candidate| {
            deadline = Some(deadline.map_or(candidate, |current: Instant| current.min(candidate)));
        };
        if let Some(started) = self.filter_dirty_since {
            include(started + FILTER_DEBOUNCE);
        }
        if let Some(started) = self.config_dirty_since {
            include(started + CONFIG_SAVE_DELAY);
        }
        if self.adb_path.is_some() {
            if !self.device_query_pending {
                include(self.last_device_query + DEVICE_POLL_INTERVAL);
            }
            if self.selected_device_available()
                && self.capture_handle.is_some()
                && !self.capture_controller.is_stopping()
                && !self.process_query_pending
            {
                include(self.last_process_query + PROCESS_POLL_INTERVAL);
            }
        }
        if let Some((started, _)) = self.process_snapshot {
            include(started + PROCESS_HINT_TTL);
        }
        if self.logs_in_rate_window > 0 || self.logs_per_second > 0.0 {
            include(self.rate_window_started + Duration::from_secs(1));
        }
        if let Some(capture) = self.capture_controller.next_deadline(now) {
            include(capture);
        }
        // A retry can become due between logic's poll and the end of UI layout.
        // Wake immediately only if it can actually start; a missing device must
        // wait for discovery rather than spin on an expired retry deadline.
        if let ControllerStatus::Backoff { deadline, .. } = self.capture_controller.status()
            && deadline <= now
            && self.files.document.is_none()
            && self.adb_path.is_some()
            && self.selected_device_available()
        {
            include(now);
        }
        if self.filter_pending && self.filter_progress.is_some() {
            include(now); // Bounded UI catch-up has real work remaining.
        }
        deadline
    }

    fn schedule_next_wake(&self, ctx: &egui::Context) {
        let now = Instant::now();
        if let Some(deadline) = self.next_wake_deadline(now) {
            ctx.request_repaint_after(deadline.saturating_duration_since(now));
        }
    }

    fn reconcile_device_selection(&mut self, ctx: &egui::Context) {
        if !self.selected_device_available() {
            self.clear_processes();
        }
        // Discovery never changes intent or stops an existing attempt.
        self.advance_capture(ctx, Instant::now());
    }

    fn selected_device_available(&self) -> bool {
        self.selected_device.as_deref().is_some_and(|serial| {
            self.devices
                .iter()
                .any(|device| device.serial == serial && device.available())
        })
    }

    fn switch_device(&mut self, serial: String, ctx: &egui::Context) {
        self.pending_clear = false;
        self.pending_device = None;
        if self.selected_device.as_deref() == Some(&serial) {
            return;
        }
        let same_history =
            self.files.document.is_none() && self.retained_device.as_deref() == Some(&serial);
        self.files.document = None;
        self.retained_device = Some(serial.clone());
        self.selected_device = Some(serial);
        self.package_query_pending = false;
        self.process_query_pending = false;
        self.package_query_error = None;
        self.packages.clear();
        self.clear_processes();
        if !same_history {
            self.clear_logs();
            self.live_session = Session::new();
            self.capture_facts = ciallogcat::capture_facts::CaptureFacts::default();
            self.process_watch = process_watch::Watcher::default();
            self.snapshot_budget = None;
            self.reset_scope_budget = true;
        }

        if self.selected_device_available() {
            self.query_packages(ctx);
            self.last_process_query = Instant::now()
                .checked_sub(PROCESS_POLL_INTERVAL)
                .unwrap_or_else(Instant::now);
        }
        self.start_capture(ctx);
    }

    fn start_capture(&mut self, ctx: &egui::Context) {
        if self.files.document.is_some() {
            return;
        }
        if let Some(serial) = self.selected_device.clone() {
            self.reset_scope_budget = true;
            self.set_capture_intent(
                CaptureIntent::Capture(CaptureTarget {
                    serial,
                    buffers: self.buffers.clone(),
                }),
                false,
            );
        }
        self.advance_capture(ctx, Instant::now());
    }

    fn stop_capture(&mut self) {
        self.set_capture_intent(CaptureIntent::Paused, false);
    }

    fn set_capture_intent(&mut self, intent: CaptureIntent, restart: bool) {
        let reason = match &intent {
            CaptureIntent::Paused => "user_pause",
            CaptureIntent::Closed => "source_closed",
            CaptureIntent::Capture(_) => "capture_reconfigured",
        };
        self.process_query_pending = false;
        self.clear_processes();
        self.process_query_error = None;
        if let Some(CaptureCommand::Stop { id }) =
            self.capture_controller
                .request(intent, restart, Instant::now())
        {
            if self.accepts_capture_data(id) {
                self.capture_facts
                    .stop_requested(reason, self.live_session.last_seq());
            }
            if let Some(handle) = &mut self.capture_handle {
                handle.stop();
            }
        }
        self.sync_capture_state();
    }

    fn accepts_capture_data(&self, id: u64) -> bool {
        self.files.document.is_none()
            && self.capture_controller.active_id() == Some(id)
            && self.capture_source_session == Some(self.live_session.id())
            && self
                .capture_controller
                .active_target()
                .is_some_and(|target| {
                    self.retained_device.as_deref() == Some(target.serial.as_str())
                })
    }

    fn halt_capture(&mut self, error: String) {
        let now = Instant::now();
        self.capture_facts.fault(self.live_session.last_seq());
        if let Some(CaptureCommand::Stop { id }) = self.capture_controller.halt(error.clone(), now)
        {
            if let Some(handle) = &mut self.capture_handle {
                handle.stop();
            } else {
                self.capture_controller.exited(id, error, now);
            }
        }
        self.sync_capture_state();
    }

    fn snapshot_budget_active(&self) -> bool {
        !self.reset_scope_budget
            && matches!(
                self.capture_controller.status(),
                ControllerStatus::Starting
                    | ControllerStatus::Running
                    | ControllerStatus::Backoff { .. }
                    | ControllerStatus::Waiting
            )
    }

    fn advance_capture(&mut self, ctx: &egui::Context, now: Instant) {
        if self.snapshot_budget_active()
            && let Some(budget) = &mut self.snapshot_budget
        {
            budget.check_time(now);
        }
        self.stop_snapshot_if_limited();
        let ready = self.files.document.is_none()
            && self.adb_path.is_some()
            && self.selected_device_available();
        if let Some(CaptureCommand::Start { id, target }) = self.capture_controller.poll(now, ready)
        {
            debug_assert!(self.capture_handle.is_none());
            self.capture_session_id = id;
            self.capture_source_session = Some(self.live_session.id());
            self.capture_attempt_scope = self.capture_scope.clone();
            if self.reset_scope_budget {
                self.snapshot_budget = self.capture_scope.is_dump().then(|| {
                    capture_ui::SnapshotBudget::new(
                        now,
                        self.memory_limit_mib,
                        self.entries.memory_bytes(),
                    )
                });
                self.reset_scope_budget = false;
            }
            self.clear_processes();
            self.process_query_error = None;
            self.process_query_pending = false;
            self.last_capture_error = None;
            let Some(attempt) = self.live_session.begin_attempt() else {
                self.halt_capture("日志 attempt 计数已耗尽，采集已停止。".to_owned());
                return;
            };
            self.capture_facts.begin_attempt_with_scope(
                attempt,
                &target.buffers,
                &self.capture_attempt_scope,
                self.live_session.last_seq(),
            );
            match adb::start_logcat_with_scope(
                self.adb_path.as_ref().unwrap(),
                &target.serial,
                &target.buffers,
                id,
                self.backend_tx.clone(),
                wake_signal(ctx),
                &self.capture_attempt_scope,
                adb::LogcatStartOptions {
                    transport: adb::LogTransport::Text,
                    deadline: self.snapshot_budget.as_ref().map(|budget| budget.deadline),
                    cancelled: None,
                },
            ) {
                Ok(handle) => self.capture_handle = Some(handle),
                Err(error) => {
                    self.capture_facts
                        .exited(None, &error, self.live_session.last_seq());
                    self.capture_controller.exited(id, error, now);
                }
            }
        }
        self.sync_capture_state();
        if let Some(deadline) = self.capture_controller.next_deadline(now) {
            ctx.request_repaint_after(deadline.saturating_duration_since(now));
        }
        if self.snapshot_budget_active()
            && let Some(budget) = &self.snapshot_budget
        {
            ctx.request_repaint_after(budget.deadline.saturating_duration_since(now));
        }
    }

    fn sync_capture_state(&mut self) {
        self.capture_state = match self.capture_controller.status() {
            ControllerStatus::Closed | ControllerStatus::Waiting => {
                if self.adb_path.is_some() {
                    CaptureState::WaitingForDevice
                } else {
                    CaptureState::NoAdb
                }
            }
            ControllerStatus::Paused => CaptureState::Paused,
            ControllerStatus::Starting => CaptureState::Connecting,
            ControllerStatus::Running => CaptureState::Running,
            ControllerStatus::Stopping => CaptureState::Stopping,
            ControllerStatus::Backoff {
                deadline,
                retry,
                error,
            } => CaptureState::Reconnecting {
                deadline,
                retry,
                error: error.to_owned(),
            },
            ControllerStatus::Failed(error) => CaptureState::Error(error.to_owned()),
        };
    }

    /// Registers a stream without spawning ADB, for deterministic event tests.
    #[cfg(test)]
    fn begin_test_capture(&mut self) -> u64 {
        assert!(self.capture_controller.active_id().is_none());
        let serial = self
            .selected_device
            .clone()
            .unwrap_or_else(|| "fixture".to_owned());
        self.selected_device = Some(serial.clone());
        self.retained_device = Some(serial.clone());
        let now = Instant::now();
        self.capture_controller.request(
            CaptureIntent::Capture(CaptureTarget {
                serial,
                buffers: self.buffers.clone(),
            }),
            false,
            now,
        );
        let Some(CaptureCommand::Start { id, .. }) = self.capture_controller.poll(now, true) else {
            panic!("test capture must start")
        };
        self.capture_session_id = id;
        self.capture_source_session = Some(self.live_session.id());
        self.capture_attempt_scope = self.capture_scope.clone();
        if self.reset_scope_budget {
            self.snapshot_budget = self.capture_scope.is_dump().then(|| {
                capture_ui::SnapshotBudget::new(
                    now,
                    self.memory_limit_mib,
                    self.entries.memory_bytes(),
                )
            });
            self.reset_scope_budget = false;
        }
        let attempt = self.live_session.begin_attempt().unwrap();
        self.capture_facts.begin_attempt_with_scope(
            attempt,
            &self.buffers,
            &self.capture_attempt_scope,
            self.live_session.last_seq(),
        );
        self.capture_controller.started(id, now);
        let segment = self.live_session.stream_started().unwrap();
        self.capture_facts
            .stream_started(segment, self.live_session.last_seq());
        self.sync_capture_state();
        id
    }

    fn pause_capture(&mut self) {
        self.stop_capture();
        self.logs_per_second = 0.0;
        self.logs_in_rate_window = 0;
    }

    fn close_capture(&mut self) {
        self.pending_clear = false;
        self.pending_device = None;
        self.set_capture_intent(CaptureIntent::Closed, false);
        self.selected_device = None;
        self.packages.clear();
        self.package_query_error = None;
        self.package_query_pending = false;
        self.process_query_pending = false;
        self.logs_per_second = 0.0;
        self.logs_in_rate_window = 0;
    }

    fn apply_buffers(&mut self, ctx: &egui::Context) {
        let scope = match self.scope_draft.parse() {
            Ok(scope) => scope,
            Err(error) => {
                self.files.notice = Some(error);
                return;
            }
        };
        LogBuffer::normalize(&mut self.buffer_draft);
        if self.buffers == self.buffer_draft && self.capture_scope == scope {
            return;
        }
        let restart = matches!(self.capture_controller.intent(), CaptureIntent::Capture(_));
        self.buffers.clone_from(&self.buffer_draft);
        self.capture_scope = scope;
        self.reset_scope_budget = true;
        self.mark_config_dirty();
        if restart {
            if let Some(serial) = self.selected_device.clone() {
                self.set_capture_intent(
                    CaptureIntent::Capture(CaptureTarget {
                        serial,
                        buffers: self.buffers.clone(),
                    }),
                    true,
                );
            }
            self.advance_capture(ctx, Instant::now());
        }
    }

    fn retry_capture(&mut self, ctx: &egui::Context) {
        if let Some(serial) = self.selected_device.clone() {
            self.reset_scope_budget = true;
            self.set_capture_intent(
                CaptureIntent::Capture(CaptureTarget {
                    serial,
                    buffers: self.buffers.clone(),
                }),
                true,
            );
        }
        self.advance_capture(ctx, Instant::now());
    }

    fn clear_logs(&mut self) {
        if self.files.document.is_none() {
            self.capture_facts
                .view_cleared(self.entries.len(), self.live_session.last_seq());
        }
        self.context_view = None;
        self.detail_search.reset();
        self.log_epoch = self.log_epoch.wrapping_add(1);
        self.visible_log_rows.clear();
        self.entries = Arc::new(LogStore::default());
        self.matches = Vec::new();
        self.focused_log_row = None;
        self.focus_log_selection = false;
        self.selected = None;
        self.selection.clear();
        self.detail_lines = None;
        self.unresolved_count = 0;
        self.dropped_entries = 0;
        self.filter_generation = self.filter_generation.wrapping_add(1);
        self.filter_elapsed = Duration::ZERO;
        self.filter_tx.cancel();
        self.restore_scroll_offset = None;
        self.table_scroll_offset = 0.0;
        self.copy_notice = None;
        self.filter_progress = Some(FilterProgress {
            generation: self.filter_generation,
            log_epoch: self.log_epoch,
            next_absolute: 0,
        });
        if self.filter_pending {
            // The active expression is still compiling; do not append with the
            // previous generation's matcher after invalidating its snapshot.
            self.filter_progress = None;
            self.filter_tx.send(FilterRequest {
                generation: self.filter_generation,
                log_epoch: self.log_epoch,
                snapshot_base: self.dropped_entries,
                entries: Arc::clone(&self.entries),
                spec: self.active_filter.clone(),
            });
        }
    }

    fn query_packages(&mut self, ctx: &egui::Context) {
        let (Some(adb_path), Some(serial)) = (self.adb_path.clone(), self.selected_device.clone())
        else {
            return;
        };
        self.package_query_pending = true;
        adb::spawn_package_query(adb_path, serial, self.backend_tx.clone(), wake_signal(ctx));
    }

    fn refresh_devices(&mut self, ctx: &egui::Context) {
        if self.device_query_pending {
            return;
        }
        let Some(adb_path) = adb::find_adb() else {
            self.adb_path = None;
            self.devices.clear();
            self.reconcile_device_selection(ctx);
            return;
        };
        self.adb_path = Some(adb_path.clone());
        if matches!(self.capture_state, CaptureState::NoAdb) {
            self.capture_state = CaptureState::WaitingForDevice;
        }
        self.device_query_pending = true;
        self.last_device_query = Instant::now();
        adb::spawn_device_query(adb_path, self.backend_tx.clone(), wake_signal(ctx));
    }

    fn apply_saved_filter(&mut self, saved: SavedFilter) {
        self.record_query_history();
        self.package = saved.package;
        self.tag_include = saved.tag_include;
        self.tag_exclude = saved.tag_exclude;
        self.min_level = saved.min_level;
        self.query = saved.query;
        self.use_regex = saved.regex;
        self.case_sensitive = saved.case_sensitive;
        self.query_history_pending = true;
        self.record_query_history();
        self.schedule_filter();
        self.mark_config_dirty();
    }

    fn save_current_filter(&mut self) {
        let name = self.save_filter_name.trim();
        if name.is_empty() {
            return;
        }
        let saved = SavedFilter {
            name: name.to_owned(),
            package: self.package.trim().to_owned(),
            tag_include: self.tag_include.clone(),
            tag_exclude: self.tag_exclude.clone(),
            min_level: self.min_level,
            query: self.query.clone(),
            regex: self.use_regex,
            case_sensitive: self.case_sensitive,
        };
        if let Some(existing) = self
            .saved_filters
            .iter_mut()
            .find(|existing| existing.name == saved.name)
        {
            *existing = saved;
        } else {
            self.saved_filters.push(saved);
        }
        self.saved_filters
            .sort_by_key(|saved| saved.name.to_lowercase());
        self.show_save_filter = false;
        self.save_filter_name.clear();
        self.mark_config_dirty();
    }

    fn mark_config_dirty(&mut self) {
        self.config_dirty_since = Some(Instant::now());
    }

    fn apply_history_query(&mut self, query: String) {
        self.record_query_history();
        self.query = query;
        self.query_history_pending = true;
        self.record_query_history();
        self.queue_filter();
    }

    fn record_query_history(&mut self) {
        if self.query_history_pending {
            history::record(&mut self.filter_history, &self.query);
            self.query_history_pending = false;
            self.mark_config_dirty();
        }
    }

    fn app_config(&self) -> AppConfig {
        AppConfig {
            memory_limit_mib: self.memory_limit_mib,
            buffers: self.buffers.clone(),
            capture_scope: self.capture_scope.clone(),
            package: self.package.clone(),
            tag_include: self.tag_include.clone(),
            tag_exclude: self.tag_exclude.clone(),
            min_level: self.min_level,
            query: self.query.clone(),
            use_regex: self.use_regex,
            case_sensitive: self.case_sensitive,
            dark: self.dark,
            row_height: self.row_height,
            show_details: self.show_details,
            saved_filters: self.saved_filters.clone(),
            filter_history: self.filter_history.clone(),
        }
    }

    fn persist_config(&mut self) {
        self.config_dirty_since = None;
        match self.app_config().save() {
            Ok(()) => self.config_error = None,
            Err(error) => self.config_error = Some(format!("Failed to save settings: {error}")),
        }
    }

    fn toolbar(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        let compact = root_ui.available_width() < 1050.0;
        let mut filter_changed = false;
        let mut selected_device = None;
        let mut apply_saved = None;
        let mut delete_saved = None;
        let mut refresh_packages = false;

        egui::Panel::top("toolbar").exact_size(44.0).show(root_ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = if compact { 4.0 } else { 8.0 };
                    let device_label = ui.label(RichText::new("Device").weak());

                    let selected_label = self
                        .selected_device
                        .as_deref()
                        .and_then(|serial| {
                            self.devices.iter().find(|device| device.serial == serial)
                        })
                        .map(DeviceInfo::display_name)
                        .unwrap_or_else(|| {
                            if let Some(serial) = &self.selected_device {
                                format!("{serial} · 未发现")
                            } else if self.devices.is_empty() {
                                "无设备".to_owned()
                            } else {
                                "选择设备".to_owned()
                            }
                        });
                    // ComboBox::width is a minimum; constrain its child layout so a
                    // long model cannot consume the space reserved for filter controls.
                    let device_width = if compact { 130.0 } else { 170.0 };
                    ui.allocate_ui_with_layout(
                        egui::vec2(device_width, 28.0),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            egui::ComboBox::from_id_salt("device")
                                .width(device_width)
                                .truncate()
                                .selected_text(&selected_label)
                                .show_ui(ui, |ui| {
                                    if self.devices.is_empty() {
                                        ui.add_enabled(false, egui::Button::new("无设备"));
                                    }
                                    for device in &self.devices {
                                        let response = ui.add_enabled(
                                            device.available(),
                                            egui::Button::selectable(
                                                self.selected_device.as_deref()
                                                    == Some(&device.serial),
                                                device.display_name(),
                                            ),
                                        );
                                        if response.clicked() {
                                            response.request_focus();
                                            selected_device = Some(device.serial.clone());
                                            ui.close();
                                        }
                                    }
                                })
                                .response
                                .labelled_by(device_label.id)
                                .on_hover_text(&selected_label);
                        },
                    );
                    if ui
                        .add_enabled(!self.device_query_pending, egui::Button::new("↻ 刷新"))
                        .on_hover_text("刷新设备")
                        .clicked()
                    {
                        self.refresh_devices(&ctx);
                    }

                let capture_requested = matches!(self.capture_controller.intent(), CaptureIntent::Capture(_));
                let can_start = !self.capture_controller.is_stopping() && self.files.document.is_none() && self.selected_device_available();
                if ui.add_enabled(capture_requested || can_start, egui::Button::new(if capture_requested { "暂停采集" } else { "开始采集" }))
                    .on_hover_text("暂停会停止采集并接收剩余日志；再次开始使用当前采集设置，停止期间的日志不保证回补").clicked() {
                    if capture_requested { self.pause_capture(); } else { self.start_capture(&ctx); }
                }
                if ui.add_enabled(self.selected_device.is_some(), egui::Button::new("断开"))
                    .on_hover_text("结束当前设备会话，保留已有日志；重新选择设备后可再次采集").clicked() {
                    self.close_capture();
                }
                ui.separator();

                egui::containers::menu::MenuButton::new("采集设置")
                    .config(egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside))
                    .ui(ui, |ui| {
                    ui.set_max_width(420.0);
                    if ui.button("只读设备诊断…").clicked() { self.open_diagnostics(&ctx); ui.close(); }
                    ui.separator();
                    self.scope_draft.ui(ui);
                    ui.separator();
                    for buffer in LogBuffer::CHOICES {
                        let mut enabled = self.buffer_draft.contains(&buffer);
                        if ui.checkbox(&mut enabled, buffer.name()).changed() {
                            if enabled {
                                if buffer == LogBuffer::All { self.buffer_draft.clear(); }
                                else { self.buffer_draft.retain(|value| *value != LogBuffer::All); }
                                self.buffer_draft.push(buffer);
                            } else { self.buffer_draft.retain(|value| *value != buffer); }
                        }
                    }
                    ui.separator();
                    ui.label("影响后续采集；已有日志保留");
                    if ui.add_enabled(!self.buffer_draft.is_empty(), egui::Button::new("应用")).clicked() {
                        self.apply_buffers(&ctx);
                        ui.close();
                    }
                });
                    let view = egui::containers::menu::MenuButton::new("View")
                        .config(egui::containers::menu::MenuConfig::new()
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside))
                        .ui(ui, |ui| {
                        if ui.add_enabled(self.entries.incident_count() > 0 && !self.filter_pending && self.filter_dirty_since.is_none(), egui::Button::new(format!("下一条崩溃 / ANR 线索（{}） · F8", self.entries.incident_count())))
                            .on_hover_text("在全部保留日志中定位已知签名并打开前后上下文；包含筛选外记录，不能替代故障诊断").clicked() {
                            self.jump_to_incident();
                            ui.close();
                        }
                        ui.separator();
                        let mut draft = self.memory_limit_draft.unwrap_or(self.memory_limit_mib);
                        ui.horizontal(|ui| {
                            ui.label("日志内存预算");
                            ui.add(egui::DragValue::new(&mut draft).range(16..=4096).suffix(" MiB"));
                        });
                        self.memory_limit_draft = Some(draft);
                        ui.label("Retained-log estimate only; excludes snapshots, queues, clipboard and process RSS.");
                        if self.entries.memory_bytes() > memory_budget_bytes(draft) {
                            ui.colored_label(Color32::from_rgb(239, 190, 82), "Apply removes oldest logs immediately to about 90% of this budget. This cannot be undone.");
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Apply budget").clicked() {
                                self.apply_memory_budget(draft);
                                ui.close();
                            }
                            if ui.button("Cancel budget").clicked() {
                                self.memory_limit_draft = None;
                                ui.close();
                            }
                        });
                        ui.separator();
                        if ui
                            .checkbox(&mut self.show_details, "Details on selection")
                            .changed()
                        {
                            self.mark_config_dirty();
                        }
                        if ui
                            .add(
                                egui::Slider::new(&mut self.row_height, 21.0..=34.0)
                                    .text("Row height"),
                            )
                            .changed()
                        {
                            self.mark_config_dirty();
                        }
                        if ui.checkbox(&mut self.dark, "Dark theme").changed() {
                            configure_style(&ctx, self.dark);
                            self.mark_config_dirty();
                        }
                        ui.separator();
                        ui.label("Ctrl+F /    Find messages");
                        ui.label("↑ ↓ Home End    Select log");
                        ui.label("Ctrl+C    Copy selected logs");
                        ui.label("Shift+Click / Shift+Up/Down    Select range");
                        ui.label("Ctrl+Click    Toggle row");
                        ui.label("Ctrl+A    Select all filtered logs");
                        ui.label("Esc    Leave input / selection");
                    });
                    if view.1.is_none() { self.memory_limit_draft = None; }
                self.file_menu(ui);
                    egui::ComboBox::from_id_salt("saved_filters")
                        .width(if compact { 80.0 } else { 112.0 })
                        .selected_text("筛选方案")
                        .show_ui(ui, |ui| {
                            for (index, saved) in self.saved_filters.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    if ui.selectable_label(false, &saved.name).clicked() {
                                        apply_saved = Some(saved.clone());
                                        ui.close();
                                    }
                                    if ui
                                        .button("×")
                                        .on_hover_text("Delete saved filter")
                                        .clicked()
                                    {
                                        delete_saved = Some(index);
                                    }
                                });
                            }
                            if !self.saved_filters.is_empty() {
                                ui.separator();
                            }
                            if ui.button("Save current…").clicked() {
                                self.show_save_filter = true;
                                ui.close();
                            }
                        });

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(!self.entries.is_empty() && self.files.document.is_none(), egui::Button::new("清空视图"))
                            .on_hover_text("清空本地保留日志；确认后无法恢复，不影响设备日志")
                            .clicked()
                        {
                            self.pending_clear = true;
                        }
                        ui.separator();
                        if ui
                            .button(if self.follow_logs {
                                "暂停跟随"
                            } else {
                                "↓ 回到最新"
                            })
                            .on_hover_text("只控制视图滚动，浏览时继续采集；Ctrl+End 回到最新")
                            .clicked()
                        {
                            if self.follow_logs {
                                self.follow_logs = false;
                                self.scroll_to_bottom = false;
                            } else {
                                self.leave_context();
                                self.follow_logs = true;
                                self.scroll_to_bottom = true;
                            }
                        }
                        if matches!(self.capture_state, CaptureState::Error(_))
                            && ui.button("Retry").clicked()
                        {
                            self.retry_capture(&ctx);
                        }
                        ui.separator();
                });
            });
        });
        egui::Panel::top("sub_toolbar")
            .exact_size(44.0)
            .show(root_ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = if compact { 4.0 } else { 8.0 };
                    let package_response = ui.add_sized(
                        [if compact { 160.0 } else { 190.0 }, 28.0],
                        egui::TextEdit::singleline(&mut self.package)
                            .hint_text("Package · all processes")
                            .font(TextStyle::Monospace)
                            .id(egui::Id::new("package_filter")),
                    );
                    ctx.accesskit_node_builder(package_response.id, |node| {
                        node.set_label("Package filter");
                    });
                    filter_changed |= package_response.changed();
                    if package_response.gained_focus() {
                        self.query_history_open = false;
                    }
                    let mut package_popup_open = self.package_popup_open;
                    if package_response.clicked()
                        || package_response.gained_focus()
                        || package_response.changed()
                    {
                        package_popup_open = true;
                    }
                    let mut package_choice = None;
                    egui::Popup::from_response(&package_response)
                        .id(egui::Id::new("package_suggestions"))
                        .open_bool(&mut package_popup_open)
                        .close_behavior(input_popup_close_behavior(&package_response))
                        .layout(Layout::top_down_justified(Align::Min))
                        .width(320.0)
                        .show(|ui| {
                            ui.set_min_width(304.0);
                            if ui
                                .selectable_label(self.package.trim().is_empty(), "All processes")
                                .clicked()
                            {
                                package_choice = Some(String::new());
                                ui.close();
                            }

                            ui.separator();
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    let mut found_package = false;
                                    for package in self.packages.iter().filter(|package| {
                                        package_matches_search(package, &self.package)
                                    }) {
                                        found_package = true;
                                        if ui
                                            .selectable_label(self.package == *package, package)
                                            .clicked()
                                        {
                                            package_choice = Some(package.clone());
                                            ui.close();
                                        }
                                    }
                                    if !found_package {
                                        ui.label(
                                            RichText::new("No matching installed packages").weak(),
                                        );
                                    }
                                });

                            ui.separator();
                            if self.package_query_pending {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(RichText::new("Loading packages…").weak());
                                });
                            } else if ui.button("Refresh package list").clicked() {
                                refresh_packages = true;
                            }
                        });
                    if let Some(package) = package_choice {
                        self.package = package;
                        filter_changed = true;
                        package_popup_open = false;
                        package_response.surrender_focus();
                    }
                    self.package_popup_open = package_popup_open;

                    let query_width = (ui.available_width() - 332.0).max(100.0);
                    let query_response = ui.add_sized(
                        [query_width, 28.0],
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Filter messages · press / to focus")
                            .font(TextStyle::Monospace)
                            .id(egui::Id::new("query_shortcut")),
                    );
                    ctx.accesskit_node_builder(query_response.id, |node| {
                        node.set_label("Message filter");
                    });
                    filter_changed |= query_response.changed();
                    if query_response.gained_focus() {
                        self.package_popup_open = false;
                    }
                    self.query_history_pending |= query_response.changed();
                    if query_response.has_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    {
                        self.record_query_history();
                    }
                    let mut history_choice = None;
                    let mut delete_history = None;
                    let mut clear_history = false;
                    let mut history_open = self.query_history_open;
                    if query_response.gained_focus()
                        || query_response.clicked()
                        || query_response.changed()
                    {
                        history_open = true;
                    }
                    if (query_response.has_focus() || query_response.lost_focus())
                        && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    {
                        history_open = false;
                    }
                    egui::Popup::from_response(&query_response)
                        .id(egui::Id::new("query_history"))
                        .open_bool(&mut history_open)
                        .close_behavior(input_popup_close_behavior(&query_response))
                        .layout(Layout::top_down_justified(Align::Min))
                        .width(360.0)
                        .show(|ui| {
                            ui.set_max_width(360.0);
                            if self.filter_history.is_empty() {
                                ui.label("暂无筛选历史");
                            }
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .show(ui, |ui| {
                                    for (index, query) in self.filter_history.iter().enumerate() {
                                        ui.horizontal(|ui| {
                                            if ui
                                                .add_sized(
                                                    [290.0, 24.0],
                                                    egui::Button::selectable(
                                                        self.query == *query,
                                                        query,
                                                    )
                                                    .truncate(),
                                                )
                                                .on_hover_text(query)
                                                .clicked()
                                            {
                                                history_choice = Some(query.clone());
                                                ui.close();
                                            }
                                            if ui
                                                .button("×")
                                                .on_hover_text("删除此条历史")
                                                .clicked()
                                            {
                                                delete_history = Some(index);
                                            }
                                        });
                                    }
                                });
                            if !self.filter_history.is_empty() {
                                ui.separator();
                                if ui.button("清空历史").clicked() {
                                    clear_history = true;
                                    ui.close();
                                }
                            }
                        });
                    if let Some(index) = delete_history {
                        self.filter_history.remove(index);
                        self.mark_config_dirty();
                    }
                    if clear_history {
                        self.filter_history.clear();
                        self.query_history_pending = false;
                        self.mark_config_dirty();
                    }
                    if let Some(query) = history_choice {
                        self.apply_history_query(query);
                        history_open = false;
                        query_response.surrender_focus();
                    }
                    self.query_history_open = history_open;
                    // Keep row identity stable while the user interacts with history.
                    // Recording on TextEdit blur would insert the pending query between
                    // a history button's press and release, selecting the wrong row.
                    if !query_response.has_focus() && !history_open {
                        self.record_query_history();
                    }
                    let regex_response = ui
                        .toggle_value(&mut self.use_regex, ".*")
                        .on_hover_text("Use a regular expression");
                    regex_response.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::Checkbox,
                            regex_response.enabled(),
                            self.use_regex,
                            "Use a regular expression",
                        )
                    });
                    if regex_response.clicked() {
                        regex_response.request_focus();
                    }
                    filter_changed |= regex_response.changed();
                    let case_response = ui
                        .toggle_value(&mut self.case_sensitive, "Aa")
                        .on_hover_text("Match case");
                    case_response.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::Checkbox,
                            case_response.enabled(),
                            self.case_sensitive,
                            "Match case",
                        )
                    });
                    if case_response.clicked() {
                        case_response.request_focus();
                    }
                    filter_changed |= case_response.changed();

                    egui::containers::menu::MenuButton::new(
                        if self.tag_include.is_empty() && self.tag_exclude.is_empty() {
                            "Tag"
                        } else {
                            "Tag •"
                        },
                    )
                    .config(
                        egui::containers::menu::MenuConfig::new()
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| {
                        ui.set_min_width(260.0);
                        ui.label("Tag 字面子串 · 区分大小写");
                        ui.label("包含");
                        let include = ui.add(
                            egui::TextEdit::singleline(&mut self.tag_include)
                                .id(egui::Id::new("tag_include"))
                                .hint_text("留空不限"),
                        );
                        ctx.accesskit_node_builder(include.id, |node| {
                            node.set_label("Tag contains")
                        });
                        ui.label("排除");
                        let exclude = ui.add(
                            egui::TextEdit::singleline(&mut self.tag_exclude)
                                .id(egui::Id::new("tag_exclude"))
                                .hint_text("留空不排除"),
                        );
                        ctx.accesskit_node_builder(exclude.id, |node| {
                            node.set_label("Tag excludes")
                        });
                        filter_changed |= include.changed() || exclude.changed();
                        ui.label("与包名、级别及消息条件同时生效；排除优先。");
                        if ui.button("清除 Tag 条件").clicked() {
                            self.tag_include.clear();
                            self.tag_exclude.clear();
                            filter_changed = true;
                        }
                    });
                    for level in Level::ALL {
                        let response = ui
                            .add_sized(
                                [28.0, 28.0],
                                egui::Button::selectable(
                                    self.min_level == level,
                                    RichText::new(level.short()).strong().color(
                                        selected_level_color(
                                            level,
                                            self.dark,
                                            self.min_level == level,
                                        ),
                                    ),
                                ),
                            )
                            .on_hover_text(format!("{level:?} and above"));
                        response.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::RadioButton,
                                response.enabled(),
                                self.min_level == level,
                                format!("Minimum level: {level:?} and above"),
                            )
                        });
                        if response.clicked() {
                            response.request_focus();
                            self.min_level = level;
                            filter_changed = true;
                        }
                    }
                });
            });
        if let Some(serial) = selected_device {
            self.request_device_switch(serial, &ctx);
        }
        if let Some(saved) = apply_saved {
            self.apply_saved_filter(saved);
        }
        if let Some(index) = delete_saved {
            self.saved_filters.remove(index);
            self.mark_config_dirty();
        }
        if refresh_packages {
            self.query_packages(&ctx);
        }

        if filter_changed {
            self.queue_filter();
        }
    }

    fn details(&mut self, root_ui: &mut egui::Ui) {
        if !self.show_details {
            return;
        }
        let Some(index) = self.selected.filter(|index| *index < self.entries.len()) else {
            return;
        };
        let message = Arc::clone(&self.entries[index].message);
        self.update_detail_hits(&message);
        let ctx = root_ui.ctx().clone();
        let checking = self.classification_pending(index);
        let reference = self.evidence_reference(index);
        let mut copy_requested = false;
        let mut reference_requested = false;
        let mut context_requested = false;
        egui::Panel::bottom("details")
            .resizable(true)
            .default_size(152.0)
            .size_range(100.0..=300.0)
            .show(root_ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if checking {
                            "Selected log · checking filter…"
                        } else {
                            "Selected log"
                        })
                        .strong(),
                    );
                    if ui
                        .add_enabled(
                            self.context_view.is_none()
                                && !self.filter_pending
                                && self.filter_dirty_since.is_none(),
                            egui::Button::new("查看上下文"),
                        )
                        .clicked()
                    {
                        context_requested = true;
                    }
                    if self.detail_search.pending {
                        ui.spinner();
                        ui.label("正在定位命中…");
                    } else if self.detail_search.key.is_some() {
                        let has_hits = !self.detail_search.hits.is_empty();
                        if ui
                            .add_enabled(has_hits, egui::Button::new("上一个"))
                            .on_hover_text("上一处消息命中")
                            .clicked()
                        {
                            self.move_detail_hit(true);
                        }
                        if ui
                            .add_enabled(has_hits, egui::Button::new("下一个"))
                            .on_hover_text("下一处消息命中；包括预览之外的内容")
                            .clicked()
                        {
                            self.move_detail_hit(false);
                        }
                        let current = if self.detail_search.current < self.detail_search.hits.len()
                        {
                            self.detail_search.current + 1
                        } else {
                            0
                        };
                        ui.label(format!(
                            "{current}/{}{}",
                            self.detail_search.hits.len(),
                            if self.detail_search.capped {
                                "+（定位上限）"
                            } else {
                                ""
                            }
                        ));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .button("Copy")
                            .on_hover_text("Copy complete log · Ctrl+C")
                            .clicked()
                        {
                            copy_requested = true;
                        }
                    });
                });
                let entry = &self.entries[index];
                ui.horizontal(|ui| {
                    if let Some((label, tooltip, _)) = &reference {
                        reference_requested =
                            ui.small_button("复制引用").on_hover_text(tooltip).clicked();
                        ui.add(egui::Label::new(RichText::new(label).weak()).truncate());
                    }
                    if let Some(hint) = ciallogcat::incident::classify(entry) {
                        ui.label(
                            RichText::new(hint.label()).color(Color32::from_rgb(220, 135, 64)),
                        )
                        .on_hover_text(format!(
                            "匹配依据：{}。\n这是排查线索；原文与上下文保留。",
                            hint.reason()
                        ));
                    }
                    ui.add(
                        egui::Label::new(
                            RichText::new(if let Some(fields) = entry.log_fields() {
                                format!(
                                    "{} · {}/{} · {}{} · {}",
                                    fields.time,
                                    fields.pid,
                                    fields.tid,
                                    preview_message(fields.process_name()),
                                    if fields.process_source == Some(ProcessSource::PsSnapshot) {
                                        " (ps 推定)"
                                    } else {
                                        ""
                                    },
                                    preview_message(fields.tag)
                                )
                            } else {
                                format!("[{}] · 非日志记录；无日志元字段", entry.kind.label())
                            })
                            .weak()
                            .monospace(),
                        )
                        .truncate(),
                    );
                });
                ui.separator();
                if message.len() > 16_384 || !self.detail_search.hits.is_empty() {
                    if self
                        .detail_lines
                        .as_ref()
                        .is_none_or(|(old, _)| !Arc::ptr_eq(old, &message))
                    {
                        self.detail_lines = Some((Arc::clone(&message), detail_ranges(&message)));
                    }
                    let (_, ranges) = self.detail_lines.as_ref().unwrap();
                    let line_height = ui.text_style_height(&TextStyle::Monospace);
                    let stride = line_height + ui.spacing().item_spacing.y;
                    let mut scroll = egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .id_salt("long_details");
                    if self.detail_search.jump
                        && let Some(hit) = self.detail_search.hits.get(self.detail_search.current)
                    {
                        let row = ranges
                            .partition_point(|range| range.end <= hit.start)
                            .min(ranges.len().saturating_sub(1));
                        if let Some(range) = ranges.get(row) {
                            let prefix_end = hit.start.min(range.end).max(range.start);
                            let width = ui.fonts_mut(|fonts| {
                                fonts
                                    .layout_no_wrap(
                                        message[range.start..prefix_end].to_owned(),
                                        FontId::new(12.5, FontFamily::Monospace),
                                        ui.visuals().text_color(),
                                    )
                                    .size()
                                    .x
                            });
                            scroll = scroll
                                .vertical_scroll_offset(row as f32 * stride)
                                .horizontal_scroll_offset(
                                    (width - ui.available_width() * 0.2).max(0.0),
                                );
                        }
                        self.detail_search.jump = false;
                    }
                    let hits = &self.detail_search.hits;
                    let current = (self.detail_search.current < hits.len())
                        .then_some(self.detail_search.current);
                    scroll.show_rows(ui, line_height, ranges.len(), |ui, visible| {
                        for row in visible {
                            let range = &ranges[row];
                            let text = message[range.clone()].trim_end_matches(['\r', '\n']);
                            let job = reading_ui::detail_layout(
                                text,
                                range.start,
                                hits,
                                current,
                                ui.visuals().text_color(),
                                self.dark,
                            );
                            ui.add(
                                egui::Label::new(job)
                                    .wrap_mode(egui::TextWrapMode::Extend)
                                    .selectable(true),
                            );
                        }
                    });
                } else {
                    self.detail_lines = None;
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(message.as_ref()).monospace().color(
                                        entry
                                            .log_fields()
                                            .map_or(ui.visuals().text_color(), |fields| {
                                                fields.level.color(self.dark)
                                            }),
                                    ),
                                )
                                .selectable(true),
                            );
                        });
                }
            });
        if copy_requested {
            self.copy_logs(&ctx, CopyTarget::Row(index));
        }
        if reference_requested && let Some((_, _, text)) = reference {
            self.cancel_copy();
            self.copy_notice = None;
            ctx.copy_text(text);
        }
        if context_requested {
            self.show_context(index);
        }
    }

    fn log_table(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        let interaction_blocked = self.files.is_modal()
            || self.show_save_filter
            || self.pending_clear
            || self.pending_device.is_some()
            // File diagnostics float over the table. Their clicks must not
            // freeze the underlying live view or move its accessible selection.
            || egui::Popup::is_id_open(&ctx, egui::Id::new("file_notice_diagnostic"));
        // A valid action can arrive with new capture rows. Pause before the
        // ScrollArea schedules its tail offset, not only after focus resolves.
        // The cache contains only last frame's viewport, never the whole store.
        let accessible_target = ctx.input(|input| {
            if interaction_blocked {
                return None;
            }
            input.events.iter().find_map(|event| {
                let egui::Event::AccessKitActionRequest(request) = event else {
                    return None;
                };
                if request.target_tree != egui::accesskit::TreeId::ROOT
                    || !matches!(
                        request.action,
                        egui::accesskit::Action::Focus | egui::accesskit::Action::Click
                    )
                {
                    return None;
                }
                self.visible_log_rows.iter().find_map(|(id, absolute)| {
                    if id.accesskit_id() != request.target_node {
                        return None;
                    }
                    absolute
                        .checked_sub(self.dropped_entries)
                        .filter(|index| self.reading_rows().binary_search(index).is_ok())
                })
            })
        });
        let accessible_browsing = accessible_target.is_some();
        if accessible_browsing {
            self.follow_logs = false;
            self.scroll_to_bottom = false;
            self.scroll_to_selected = false;
            self.focus_log_selection = false;
        }
        let mut visible_log_rows = Vec::new();
        let compact = root_ui.available_width() < 1050.0;
        if self.highlight_key.0 != self.query
            || self.highlight_key.1 != self.use_regex
            || self.highlight_key.2 != self.case_sensitive
        {
            self.highlight_matcher = None;
            self.highlight_key = (self.query.clone(), self.use_regex, self.case_sensitive);
            if !self.query.is_empty() {
                self.filter_tx.send_highlight(self.highlight_key.clone());
            }
        }
        let highlight = self.highlight_matcher.as_ref();
        let selected = self.selected;
        let selection = &self.selection;
        let row_height = self.row_height;
        let dark = self.dark;
        let entries = Arc::clone(&self.entries);
        let matches = self
            .context_view
            .as_ref()
            .map_or(&self.matches, |context| &context.rows);
        let mut clicked_row = None;
        let mut copy_requested = None;
        let mut filter_process = None;
        let mut context_requested = None;
        let mut focus_selected = self.focus_log_selection && !ctx.text_edit_focused();
        let mut focused_selected = false;
        let mut focused_log_row = None;
        let mut row_focus_gained = false;

        egui::CentralPanel::default().show(root_ui, |ui| {
            // Panel insertion (for example details opening) must not change
            // table/row identity or invalidate a native accessibility focus event.
            ui.scope_builder(egui::UiBuilder::new().id(egui::Id::new("log_table_content")), |ui| {
            ui.style_mut().interaction.selectable_labels = false;
            // Stop before laying out rows: a press and release can span many
            // capture updates, so waiting for clicked() lets the target move.
            // Modal pointer presses belong to its buttons/backdrop, never to
            // the obscured table. Preserve the existing follow choice on Cancel.
            let browsing = !interaction_blocked && ctx.input(|input| {
                input.pointer.hover_pos().is_some_and(|pos| ui.max_rect().contains(pos))
                    && (input.pointer.any_pressed() || input.smooth_scroll_delta.y > 0.0)
            });
            if browsing {
                focus_selected = false;
                self.follow_logs = false;
                self.scroll_to_bottom = false;
                self.scroll_to_selected = false;
            }
            if matches.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(if self.filter_pending || self.filter_dirty_since.is_some() {
                        "Filtering logs…"
                    } else if self.filter_error.is_some() {
                        "Invalid regular expression. Correct the query or turn off .* ."
                    } else if !entries.is_empty() {
                        "No matching logs. Check the package, Tag include/exclude, minimum level and message filter."
                    } else if self.files.document.is_some() {
                        "文件中没有日志记录。可以打开其他文件或选择设备开始采集。"
                    } else {
                        match self.capture_state {
                            CaptureState::NoAdb => "Install Android platform-tools, then click Refresh to find ADB.\nOr open a native JSONL file via File / Ctrl+O; no device required.",
                            CaptureState::WaitingForDevice => "Connect and authorize an Android device, then select it above.\nOr open a native JSONL file via File / Ctrl+O; no device required.",
                            CaptureState::Error(_) => "Capture stopped. Click Retry to resume.",
                            CaptureState::Paused => "采集已暂停。点击开始采集继续接收日志。",
                            _ => "Waiting for logs from the selected device…",
                        }
                    });
                });
                return;
            }
            let table_id = ui.id().with(egui::IdSalt::new("log_table"));
            let table_rect = ui.max_rect();
            let table_clip = ui.clip_rect();
            let focus_stroke = egui::Stroke::new(1.0, ui.visuals().text_color());
            let table_node = ui.unique_id();
            ctx.accesskit_node_builder(table_node, |node| {
                node.set_role(egui::accesskit::Role::Table);
                node.set_label("Filtered logs");
                node.set_row_count(matches.len() + 1);
                node.set_column_count(6);
                node.set_multiselectable();
                node.set_description("Use arrow keys to select logs. Select a row for complete details; Copy full log includes the complete message.");
            });
            let reset_columns = self.log_columns.prepare(compact);
            let widths = self.log_columns.widths(compact);
            let mut painted_widths = [0.0; 2];
            let mut header_ids = [egui::Id::NULL; 6];
            let mut table = TableBuilder::new(ui)
                .id_salt("log_table")
                .striped(true)
                .resizable(true)
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::exact(145.0))
                .column(Column::exact(32.0))
                .column(Column::exact(90.0))
                .column(Column::initial(widths[0]).at_least(100.0))
                .column(Column::initial(widths[1]).at_least(80.0))
                .column(Column::remainder().at_least(180.0))
                .min_scrolled_height(0.0)
                .sense(egui::Sense::hover())
                .animate_scrolling(false)
                .stick_to_bottom(self.follow_logs);
            if reset_columns {
                table.reset();
            }
            if self.scroll_to_bottom && !matches.is_empty() {
                table = table.scroll_to_row(matches.len() - 1, Some(Align::BOTTOM));
            } else if self.scroll_to_selected
                && let Some(position) = selected.and_then(|index| matches.binary_search(&index).ok())
            {
                table = table.scroll_to_row(position, None);
            } else if let Some(position) = accessible_target.and_then(|index| matches.binary_search(&index).ok()) {
                table = table.scroll_to_row(position, None);
            }

            let restore = self.restore_scroll_offset.take();
            if !self.follow_logs && !self.scroll_to_bottom && !self.scroll_to_selected
                && let Some(offset) = restore {
                table = table.vertical_scroll_offset(offset);
            }
            let table_output = table
                .header(30.0, |mut header| {
                    let mut header_row = egui::Id::NULL;
                    for (column, title) in log_table_ui::HEADERS.iter().enumerate() {
                        header.col(|ui| {
                            if column == 0 {
                                header_row = ui.unique_id();
                                ctx.accesskit_node_builder(header_row, |node| {
                                    node.set_role(egui::accesskit::Role::Row);
                                    node.set_row_index(0);
                                });
                            }
                            if (3..=4).contains(&column) {
                                painted_widths[column - 3] = ui.max_rect().width();
                            }
                            header_ids[column] = log_table_ui::cell(
                                ui, header_row, 0, column, title, None, |ui| {
                                    ui.label(RichText::new(*title).weak().size(11.0).strong());
                                },
                            );
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_height, matches.len(), |mut row| {
                        let match_index = row.index();
                        let entry_index = matches[match_index];
                        let entry = &entries[entry_index];
                        let is_selected = selection.contains(entry_index);
                        row.set_selected(is_selected);

                        let mut row_id = egui::Id::NULL;
                        let mut semantic_response = None;
                        let fields = entry.log_fields();
                        let pid_tid = fields.map_or_else(String::new, |fields| format!("{} / {}", fields.pid, fields.tid));
                        let raw_preview = preview_first_line(&entry.message);
                        let kind_prefix = if fields.is_some() { String::new() } else { format!("[{}] ", entry.kind.label()) };
                        let message = if kind_prefix.is_empty() {
                            std::borrow::Cow::Borrowed(raw_preview)
                        } else {
                            std::borrow::Cow::Owned(format!("{kind_prefix}{raw_preview}"))
                        };
                        let values = [fields.map_or("", |fields| fields.time), fields.map_or("", |fields| fields.level.short()), &pid_tid,
                            fields.map_or("", |fields| preview_message(fields.process_name())), fields.map_or("", |fields| preview_message(fields.tag)),
                            message.as_ref()];
                        for (column, value) in values.iter().enumerate() {
                            row.col(|ui| {
                                if column == 0 {
                                    let absolute = self.dropped_entries + entry_index;
                                    let identity = egui::Id::new(("log_row", self.log_epoch, absolute));
                                    visible_log_rows.push((identity, absolute));
                                    let response = log_table_ui::row_response(ui, identity, table_rect, table_clip);
                                    row_id = response.id;
                                    semantic_response = Some(response);
                                    ctx.accesskit_node_builder(row_id, |node| {
                                        node.set_role(egui::accesskit::Role::Row);
                                        node.set_row_index(match_index + 1);
                                    });
                                }
                                log_table_ui::cell(ui, row_id, match_index + 1, column, value,
                                    Some(header_ids[column]), |ui| {
                                        if column == 5 {
                                            let job = highlighted_message(&kind_prefix, raw_preview, highlight, ui.visuals().text_color(), dark);
                                            ui.add(egui::Label::new(job).truncate());
                                        } else {
                                            let mut text = RichText::new(*value).monospace().size(12.0);
                                            if let Some(fields) = fields {
                                                if column == 1 || column == 4 {
                                                    text = text.color(selected_level_color(fields.level, dark, is_selected));
                                                }
                                                if column == 1 { text = RichText::new(*value).monospace().strong().color(selected_level_color(fields.level, dark, is_selected)); }
                                            }
                                            ui.add(egui::Label::new(text).truncate());
                                        }
                                    });
                            });
                        }

                        let semantic_response = semantic_response.expect("first column creates row");
                        let row_bounds = semantic_response.rect;
                        let response = semantic_response | row.response();
                        if focus_selected && selected == Some(entry_index) {
                            response.request_focus();
                            focused_selected = true;
                        }
                        let truncated = fields.is_some_and(|fields| values[3].len() < fields.process_name().len()
                            || values[4].len() < fields.tag.len())
                            || raw_preview.len() < entry.message.len();
                        response.widget_info(|| egui::WidgetInfo::selected(
                            egui::WidgetType::SelectableLabel, true, is_selected,
                            if let Some(fields) = fields {
                                format!("Log row {}. Time {}. Level {:?}. PID / TID {}. Process {}{}. Tag {}. Message {}{}",
                                    match_index + 1, values[0], fields.level, values[2], values[3],
                                    if fields.process_source == Some(ProcessSource::PsSnapshot) { " (sampled ps hint, unverified)" } else { "" },
                                    values[4], values[5],
                                    if truncated { ". Preview truncated. Select for full details or use Copy full log." } else { "" })
                            } else {
                                format!("{} row {}. Message {}{}", entry.kind.label(), match_index + 1, raw_preview,
                                    if truncated { ". Preview truncated. Select for full details or use Copy full log." } else { "" })
                            },
                        ));
                        ctx.accesskit_node_builder(row_id, |node| {
                            node.set_role(egui::accesskit::Role::Row);
                            node.set_row_index(match_index + 1);
                            node.set_selected(is_selected);
                            node.set_bounds(egui::accesskit::Rect {
                                x0: row_bounds.min.x as f64, y0: row_bounds.min.y as f64,
                                x1: row_bounds.max.x as f64, y1: row_bounds.max.y as f64,
                            });
                        });
                        if response.clicked() {
                            response.request_focus();
                            clicked_row = Some((entry_index, ctx.input(|input| input.modifiers)));
                        }
                        if response.has_focus() {
                            row_focus_gained |= response.gained_focus();
                            focused_log_row = Some((response.id, entry_index));
                            if !is_selected {
                                ctx.layer_painter(response.layer_id)
                                    .with_clip_rect(response.interact_rect.intersect(table_clip))
                                    .rect_stroke(row_bounds.shrink(0.5), 0.0, focus_stroke, egui::StrokeKind::Inside);
                            }
                        }
                        response.context_menu(|ui| {
                            if ui.add_enabled(self.context_view.is_none() && !self.filter_pending && self.filter_dirty_since.is_none(), egui::Button::new("查看上下文")).clicked() { context_requested = Some(entry_index); ui.close(); }
                            if fields.is_some_and(|fields| fields.process_source == Some(ProcessSource::PsSnapshot)) {
                                ui.label("进程名来自近期 ps 快照，仅为推定；不能证明历史 PID 归属。");
                                ui.separator();
                            }
                            if selection.contains(entry_index) && ui.button("Copy selected logs").clicked() {
                                copy_requested = Some(CopyTarget::Selection);
                                ui.close();
                            }
                            if ui.button("Copy full log").clicked() {
                                copy_requested = Some(CopyTarget::Row(entry_index));
                                ui.close();
                            }
                            if let Some(process) = fields.and_then(|fields| fields.process)
                                && ui.button("Filter this process").clicked()
                            {
                                filter_process = Some(process.to_owned());
                                ui.close();
                            }
                        });
                    });
                });
            self.table_scroll_offset = table_output.state.offset.y;
            self.table_row_stride = row_height + ui.spacing().item_spacing.y;
            self.log_columns.finish(&ctx, table_id, painted_widths);
            });
        });

        if let Some(target) = copy_requested {
            self.copy_logs(&ctx, target);
        }
        self.visible_log_rows = visible_log_rows;
        if row_focus_gained {
            // Assistive/Tab browsing pauses the moving tail just like pointer
            // browsing. Do not pause on every focused frame: Ctrl+End resumes.
            self.follow_logs = false;
        }
        self.focused_log_row = focused_log_row;
        self.focus_log_selection = focus_selected && !focused_selected && self.selected.is_some();
        self.scroll_to_bottom = false;
        self.scroll_to_selected = false;
        if let Some((index, modifiers)) = clicked_row
            && self.selection.select(
                self.context_view
                    .as_ref()
                    .map_or(&self.matches, |context| &context.rows),
                index,
                modifiers.shift,
                modifiers.command,
            )
        {
            self.selected = self.selection.contains(index).then_some(index);
            self.scroll_to_selected = true;
        }
        if let Some(index) = context_requested {
            self.show_context(index);
        }
        if let Some(process) = filter_process {
            self.package = process;
            self.queue_filter();
        }
    }

    fn status_bar(&mut self, root_ui: &mut egui::Ui) {
        self.poll_copy();
        egui::Panel::bottom("status")
            .exact_size(26.0)
            .show(root_ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("●")
                            .color(if self.files.document.is_some() { Color32::from_rgb(142, 151, 166) } else { self.capture_state.color() })
                            .size(10.0),
                    );
                    if self.files.document.is_some() {
                        ui.label(RichText::new("文件 · 只读").weak().size(11.0));
                    } else if let CaptureState::Error(error) = &self.capture_state {
                        Self::status_diagnostic(ui, "capture_diagnostic", "采集错误", error);
                    } else if let CaptureState::Reconnecting { deadline, retry, error } = &self.capture_state {
                        let seconds = deadline.saturating_duration_since(Instant::now()).as_secs_f32().ceil() as u32;
                        let label = if self.selected_device_available() { format!("{seconds}s 后重连 {retry}/3") } else { format!("等待设备以重连 {retry}/3") };
                        Self::status_diagnostic(ui, "capture_reconnect", &label, error);
                    } else {
                        ui.label(RichText::new(self.capture_state.label()).weak().size(11.0));
                    }
                    ui.separator();
                    let dropped = if self.dropped_entries > 0 {
                        format!(" · {} dropped", format_count(self.dropped_entries))
                    } else {
                        String::new()
                    };
                    ui.label(
                        RichText::new(format!(
                            "{} total · {:.0}/s{dropped}",
                            format_count(self.entries.len()),
                            self.logs_per_second
                        ))
                        .weak()
                        .monospace()
                        .size(11.0),
                    );
                    ui.label(RichText::new(format!("{} matches", format_count(self.matches.len()))).weak().monospace().size(11.0))
                        .on_hover_text(format!("Retained ≈ {:.1} / {} MiB", self.entries.memory_bytes() as f64 / MIB as f64, self.memory_limit_mib));
                    // Independent compact indicators keep one long diagnostic from
                    // hiding counts or an unrelated settings/query failure.
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.copy_pending.is_some() {
                            if ui.small_button("取消复制").clicked() {
                                self.cancel_copy();
                            } else if self.copy_ready.is_some() {
                                if ui.button("复制已准备好").on_hover_text("点击后才写入剪贴板；来源、筛选或选择变化会取消。").clicked() {
                                    self.commit_prepared_copy(ui.ctx());
                                }
                            } else {
                                ui.spinner().on_hover_text("正在准备完整日志；完成后需点击复制，不会自动覆盖剪贴板。");
                            }
                        }
                        if let Some(error) = &self.filter_error {
                            Self::status_diagnostic(ui, "filter_diagnostic", "筛选错误", error);
                        } else if self.filter_pending || self.filter_dirty_since.is_some() {
                            ui.spinner().on_hover_text("筛选中，当前结果可能尚未完整");
                        }
                        if let Some(error) = &self.device_query_error {
                            Self::status_diagnostic(ui, "device_diagnostic", "设备发现错误", error);
                        } else if self.selected_device.is_some() && !self.selected_device_available() {
                            Self::status_diagnostic(ui, "device_observation", "设备未就绪", "设备列表当前未报告所选设备就绪；保留选择，已有采集会继续，直到日志流退出或手动暂停。");
                        }
                        if let Some(error) = &self.process_query_error {
                            Self::status_diagnostic(ui, "process_diagnostic", "进程查询错误", error);
                        }
                        if let Some(notice) = self.copy_notice.as_ref() {
                            Self::status_diagnostic(ui, "copy_notice", "Copy / history", notice);
                        }
                        if let Some(error) = self.config_error.as_ref() {
                            Self::status_diagnostic(
                                ui,
                                "settings_diagnostic",
                                "设置保存失败",
                                error,
                            );
                        }
                        if let Some(error) = &self.package_query_error {
                            Self::status_diagnostic(
                                ui,
                                "package_diagnostic",
                                "包列表查询错误",
                                error,
                            );
                        }
                    });
                });
            });
    }

    fn status_diagnostic(ui: &mut egui::Ui, id: &'static str, label: &str, diagnostic: &str) {
        ui.push_id(id, |ui| {
            let response = ui.small_button(label).on_hover_text("查看并复制完整诊断");
            if response.clicked() {
                response.request_focus();
            }
            egui::Popup::from_toggle_button_response(&response)
                .id(egui::Id::new(id))
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .width(520.0)
                .show(|ui| {
                    ui.set_max_width(520.0);
                    ui.label(RichText::new(label).strong());
                    if ui.button("Copy diagnostic").clicked() {
                        ui.ctx().copy_text(diagnostic.to_owned());
                    }
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .show(ui, |ui| {
                            ui.add(egui::Label::new(diagnostic).wrap().selectable(true));
                        });
                });
        });
    }

    fn request_device_switch(&mut self, serial: String, ctx: &egui::Context) {
        if !self.entries.is_empty() && self.retained_device.as_deref() != Some(&serial) {
            self.pending_device = Some(serial);
        } else {
            self.switch_device(serial, ctx);
        }
    }

    fn clear_confirmation(&mut self, ctx: &egui::Context) {
        if !self.pending_clear && self.pending_device.is_none() {
            return;
        }
        let mut confirm = false;
        let mut cancel =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let response = egui::Modal::new(egui::Id::new("clear_logs_confirmation")).show(ctx, |ui| {
            ui.heading("清空本地保留日志？");
            if let Some(serial) = &self.pending_device {
                ui.label(format!("切换到 {serial} 会清空当前本地保留日志。"));
            } else {
                ui.label("清空所有本地保留日志，确认后无法恢复。采集状态不变。");
            }
            ui.label("不会清空设备上的日志，也不会重新补回这些历史记录。");
            ui.horizontal(|ui| {
                cancel |= ui.button("Cancel").clicked();
                confirm = ui.button("清空并继续").clicked();
            });
        });
        if cancel || response.should_close() {
            self.pending_clear = false;
            self.pending_device = None;
        } else if confirm {
            self.pending_clear = false;
            if let Some(serial) = self.pending_device.take() {
                self.switch_device(serial, ctx);
            } else {
                self.clear_logs();
            }
        }
    }

    fn filter_notice(&mut self, root_ui: &mut egui::Ui) {
        if self.package.trim().is_empty() {
            return;
        }
        egui::Panel::top("unresolved_notice").show(root_ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if self.capture_attempt_scope != ciallogcat::capture_scope::CaptureScope::default()
                    && self.files.document.is_none()
                {
                    ui.label("历史采集不套用当前 ps 名称；未知归属可能被包名筛选隐藏。");
                }
                ui.label(format!(
                    "包名基于近期 ps 推定；{} 条已解析日志归属未知，可能被隐藏（含崩溃）。",
                    self.unresolved_count
                ));
                if ui.button("清除包名限制以查看").clicked() {
                    self.package.clear();
                    self.schedule_filter();
                    self.mark_config_dirty();
                }
                ui.menu_button("进程变化", |ui| {
                    ui.set_max_width(440.0);
                    ui.label("来自定期 ps 快照；出现/消失是主机观察，不是精确启动/退出时间。");
                    if self.process_watch.package != self.package.trim()
                        || !self.process_watch.current_known
                    {
                        ui.label("等待当前应用的新进程快照；未知不等于没有进程。");
                    }
                    if self.process_watch.truncated_sample {
                        ui.label("本次最多跟踪 128 个进程，已达上限。");
                    }
                    if self.process_watch.package == self.package.trim() {
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .show(ui, |ui| {
                                for change in self.process_watch.recent.iter().rev() {
                                    ui.label(format!(
                                        "{} · PID {} · {}{}",
                                        change.label(),
                                        change.pid,
                                        change.name,
                                        if change.name_truncated { "…" } else { "" }
                                    ));
                                }
                            });
                        if self.process_watch.omitted_changes > 0 {
                            ui.label(format!(
                                "另有 {} 条较早观察已从此列表移除。",
                                self.process_watch.omitted_changes
                            ));
                        }
                    }
                });
            });
        });
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.files.is_modal()
            || self.show_save_filter
            || self.pending_clear
            || self.pending_device.is_some()
            || ctx.current_pass_index() != 0
        {
            return;
        }
        if self.context_view.is_some()
            && !ctx.text_edit_focused()
            && !egui::Popup::is_any_open(ctx)
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.leave_context();
            return;
        }
        let text_focused = ctx.text_edit_focused();
        if !text_focused
            && !egui::Popup::is_any_open(ctx)
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F8))
        {
            self.jump_to_incident();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::F))
            || (!text_focused
                && ctx
                    .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Slash)))
        {
            egui::Popup::close_all(ctx);
            self.package_popup_open = false;
            self.query_history_open = true;
            ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("query_shortcut")));
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            // Escape dismisses the active input popup/menu before touching rows.
            // open_bool popups are not included in Popup::is_any_open.
            if text_focused
                || self.query_history_open
                || self.package_popup_open
                || egui::Popup::is_any_open(ctx)
                || ctx.memory(|memory| {
                    memory.had_focus_last_frame(egui::Id::new("query_shortcut"))
                        || memory.had_focus_last_frame(egui::Id::new("package_filter"))
                })
            {
                ctx.memory_mut(|memory| {
                    memory.surrender_focus(egui::Id::new("query_shortcut"));
                    memory.surrender_focus(egui::Id::new("package_filter"));
                });
                self.package_popup_open = false;
                self.query_history_open = false;
                egui::Popup::close_all(ctx);
            } else {
                self.selected = None;
                self.selection.clear();
            }
            return;
        }
        if text_focused
            || self.query_history_open
            || self.package_popup_open
            || egui::Popup::is_any_open(ctx)
        {
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::End)) {
            self.leave_context();
            self.follow_logs = true;
            self.scroll_to_bottom = true;
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)) {
            if self.context_view.is_some() {
                self.leave_context();
            } else if let Some(index) = self.selected {
                self.show_context(index);
            }
            return;
        }
        let previous_hit =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::F3));
        if previous_hit
            || ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F3))
        {
            if let Some(index) = self.selected.filter(|index| *index < self.entries.len()) {
                self.show_details = true;
                let message = Arc::clone(&self.entries[index].message);
                self.update_detail_hits(&message);
                self.move_detail_hit(previous_hit);
            }
            return;
        }
        let rows = self
            .context_view
            .as_ref()
            .map_or(&self.matches, |context| &context.rows);
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::A)) {
            self.follow_logs = false;
            self.selection.select_all(rows);
            self.selected = self
                .selected
                .filter(|index| self.selection.contains(*index))
                .or_else(|| rows.first().copied());
            return;
        }
        for key in [
            egui::Key::ArrowUp,
            egui::Key::ArrowDown,
            egui::Key::Home,
            egui::Key::End,
        ] {
            let extend = ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, key));
            if extend || ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, key)) {
                self.follow_logs = false;
                // This shortcut owns row navigation; egui must not also move
                // directional focus at end-of-pass after we focus the target.
                ctx.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
                let origin = self
                    .focused_log_row
                    .filter(|(id, index)| {
                        ctx.memory(|memory| memory.has_focus(*id))
                            && rows.binary_search(index).is_ok()
                    })
                    .map(|(_, index)| index)
                    .or(self.selected);
                if let Some(index) = navigated_selection(rows, origin, key)
                    && self.selection.select(rows, index, extend, false)
                {
                    self.selected = Some(index);
                    self.scroll_to_selected = true;
                    self.focus_log_selection = true;
                }
                break;
            }
        }
    }

    fn copy_shortcut(&mut self, ctx: &egui::Context) {
        // A newer text/diagnostic clipboard action supersedes prepared logs too.
        if ctx.output(|output| {
            output
                .commands
                .iter()
                .any(|command| matches!(command, egui::OutputCommand::CopyText(_)))
        }) {
            self.cancel_copy();
            return;
        }
        if self.files.is_modal()
            || self.show_save_filter
            || self.pending_clear
            || self.pending_device.is_some()
            || ctx.text_edit_focused()
            || self.query_history_open
            || self.package_popup_open
            || egui::Popup::is_any_open(ctx)
        {
            return;
        }
        // Native integrations translate Ctrl+C into Event::Copy. Let selectable
        // text handle it first; only fall back to the full row when no text copied.
        let requested = ctx.input(|input| input.events.contains(&egui::Event::Copy))
            || ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::C));
        let text_already_copied = ctx.output(|output| {
            output
                .commands
                .iter()
                .any(|command| matches!(command, egui::OutputCommand::CopyText(_)))
        });
        if requested && !text_already_copied && !self.selection.is_empty() {
            self.copy_logs(ctx, CopyTarget::Selection);
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        self.diagnostics_dialog(ctx);
        if self.files.is_modal() {
            self.file_dialogs(ctx);
            return;
        }
        if self.pending_clear || self.pending_device.is_some() {
            self.show_save_filter = false;
            self.save_filter_focused = false;
            self.clear_confirmation(ctx);
            return;
        }
        if self.show_save_filter {
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                self.show_save_filter = false;
                self.save_filter_focused = false;
                return;
            }
            let mut save = false;
            let mut cancel = false;
            let modal = egui::Modal::new(egui::Id::new("save_filter_dialog")).show(ctx, |ui| {
                ui.heading("Save filter");
                ui.label("Name");
                let response = ui.add_sized(
                    [320.0, 28.0],
                    egui::TextEdit::singleline(&mut self.save_filter_name)
                        .id(egui::Id::new("save_filter_name"))
                        .hint_text("e.g. Network failures"),
                );
                if !self.save_filter_focused {
                    response.request_focus();
                    self.save_filter_focused = true;
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.save_filter_name.trim().is_empty(),
                            egui::Button::new("Save"),
                        )
                        .clicked()
                        || (response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                    {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
            self.show_save_filter = !modal.should_close() && !cancel;
            if save {
                self.save_current_filter();
            }
        }
        if !self.show_save_filter {
            self.save_filter_focused = false;
        }
    }
}

impl eframe::App for CiallogcatApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_filter();
        self.poll_backend(ctx);
        self.poll_files(ctx);
        self.poll_copy();
        self.schedule_next_wake(ctx);
    }

    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();

        self.file_shortcuts(&ctx);
        self.shortcuts(&ctx);

        // egui's modal layer normally takes effect on the next frame. A file
        // candidate may arrive between pointer press and release, so explicitly
        // disable the obscured controls immediately. Modals create their own Ui.
        if self.files.is_modal()
            || self.pending_clear
            || self.pending_device.is_some()
            || self.show_save_filter
        {
            root_ui.disable();
        }

        self.toolbar(root_ui);
        self.file_notice(root_ui);
        self.context_notice(root_ui);
        self.filter_notice(root_ui);
        self.status_bar(root_ui);
        self.details(root_ui);
        self.log_table(root_ui);
        self.dialogs(&ctx);
        self.copy_shortcut(&ctx);
        self.poll_copy();
        // Input handlers can create debounce/save/controller deadlines after
        // logic has run, so include their changes before returning this frame.
        self.schedule_next_wake(&ctx);
    }
}

impl Drop for CiallogcatApp {
    fn drop(&mut self) {
        self.record_query_history();
        self.stop_capture();
        #[cfg(not(test))]
        let _ = self.app_config().save();
    }
}

// TextEdit gains focus on press, before a click completes on release. Treat the
// anchor click as part of its popup, or egui dismisses the newly opened popup as
// an outside click on the release frame. Genuine outside clicks still dismiss it.
fn input_popup_close_behavior(response: &egui::Response) -> egui::PopupCloseBehavior {
    if response.clicked() {
        egui::PopupCloseBehavior::IgnoreClicks
    } else {
        egui::PopupCloseBehavior::CloseOnClickOutside
    }
}

// Neutral selected text preserves contrast while the severity letter remains explicit.
fn selected_level_color(level: Level, dark: bool, selected: bool) -> Color32 {
    if selected {
        if dark { Color32::WHITE } else { Color32::BLACK }
    } else {
        level.color(dark)
    }
}

fn configure_style(ctx: &egui::Context, dark: bool) {
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
    if dark {
        ctx.set_visuals(egui::Visuals::dark());
    } else {
        ctx.set_visuals(egui::Visuals::light());
    }

    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(8.0, 5.0);
        style.spacing.interact_size.y = 27.0;
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(13.0, FontFamily::Proportional));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(12.5, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Monospace,
            FontId::new(12.5, FontFamily::Monospace),
        );
    });
}

fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        APP_FONT_NAME.to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/CascadiaNextSCNF-Regular.ttf"
        ))),
    );

    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, APP_FONT_NAME.to_owned());
    }

    font_fallback::install(&mut fonts);
    ctx.set_fonts(fonts);
}

fn highlighted_message(
    prefix: &str,
    text: &str,
    matcher: Option<&Regex>,
    text_color: Color32,
    dark: bool,
) -> LayoutJob {
    let base = TextFormat {
        font_id: FontId::new(12.5, FontFamily::Monospace),
        color: text_color,
        ..Default::default()
    };
    let highlight = TextFormat {
        font_id: FontId::new(12.5, FontFamily::Monospace),
        color: if dark {
            Color32::from_rgb(255, 221, 123)
        } else {
            Color32::from_rgb(120, 74, 0)
        },
        background: if dark {
            Color32::from_rgb(87, 70, 24)
        } else {
            Color32::from_rgb(255, 232, 159)
        },
        ..Default::default()
    };

    let mut job = LayoutJob::default();
    // Kind labels are presentation, never part of the message search corpus.
    if !prefix.is_empty() {
        job.append(prefix, 0.0, base.clone());
    }
    let Some(matcher) = matcher else {
        job.append(text, 0.0, base);
        return job;
    };

    let mut cursor = 0;
    for found in matcher.find_iter(text).take(24) {
        if found.start() > cursor {
            job.append(&text[cursor..found.start()], 0.0, base.clone());
        }
        job.append(&text[found.start()..found.end()], 0.0, highlight.clone());
        cursor = found.end();
        if found.is_empty() {
            break;
        }
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, base);
    }
    job
}

// The table is a preview. Limit shaping work on pathological single lines;
// selection and copying continue to use the original, complete message.
fn preview_message(text: &str) -> &str {
    let mut end = text.len().min(2048);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

// Search only the display prefix. A CR immediately before a newline at the
// byte cap must be stripped exactly as str::lines would strip the full line.
fn preview_first_line(text: &str) -> &str {
    let prefix = preview_message(text);
    let end = prefix.find('\n').unwrap_or(prefix.len());
    let line = &prefix[..end];
    if text.as_bytes().get(end) == Some(&b'\n') {
        line.strip_suffix('\r').unwrap_or(line)
    } else {
        line
    }
}

fn detail_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + 512).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if let Some(newline) = text[start..end].find('\n') {
            end = start + newline + 1;
        }
        ranges.push(start..end);
        start = end;
    }
    ranges
}

fn navigated_selection(
    matches: &[usize],
    selected: Option<usize>,
    key: egui::Key,
) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let position = selected.and_then(|index| matches.binary_search(&index).ok());
    let target = match key {
        egui::Key::Home => 0,
        egui::Key::End => matches.len() - 1,
        egui::Key::ArrowUp => position.map_or(matches.len() - 1, |index| index.saturating_sub(1)),
        egui::Key::ArrowDown => position.map_or(0, |index| (index + 1).min(matches.len() - 1)),
        _ => return selected,
    };
    Some(matches[target])
}

fn format_count(value: usize) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(character);
    }
    formatted
}

fn package_matches_search(package: &str, query: &str) -> bool {
    let query = query.trim().as_bytes();
    query.is_empty()
        || package
            .as_bytes()
            .windows(query.len())
            .any(|part| part.eq_ignore_ascii_case(query))
}

#[cfg(test)]
mod ui_tests {
    use super::{configure_style, egui, package_matches_search};

    #[test]
    fn configured_theme_overrides_system_theme() {
        let ctx = egui::Context::default();
        for dark in [true, false, true] {
            configure_style(&ctx, dark);
            let input = egui::RawInput {
                system_theme: Some(if dark {
                    egui::Theme::Light
                } else {
                    egui::Theme::Dark
                }),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let ctx = ui.ctx();
                assert_eq!(ctx.global_style().visuals.dark_mode, dark);
                assert_eq!(
                    ctx.global_style().spacing.item_spacing,
                    egui::vec2(8.0, 6.0)
                );
            });
            output.textures_delta.clear();
        }
    }

    #[test]
    fn package_search_is_case_insensitive_substring_matching() {
        assert!(package_matches_search("com.Example.Ciallogcat", "example"));
        assert!(package_matches_search(
            "com.example.ciallogcat",
            " CIALLOG "
        ));
        assert!(package_matches_search("com.example.ciallogcat", ""));
        assert!(!package_matches_search("com.example.ciallogcat", "other"));
    }
}

#[cfg(test)]
mod review_regressions;

#[cfg(test)]
mod popup_regressions;

#[cfg(test)]
mod ui_test_support;

#[cfg(test)]
mod ui_audit_regressions;

#[cfg(test)]
mod log_table_regressions;

#[cfg(test)]
mod memory_regressions;

#[cfg(test)]
mod clear_cancel_regressions;

#[cfg(test)]
mod reading_regressions;

#[cfg(test)]
mod copy_regressions;
