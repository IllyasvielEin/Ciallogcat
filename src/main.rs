mod adb;
mod config;
mod filter;
mod log_store;
mod model;
#[cfg(test)]
mod performance;
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
use filter::{FilterRequest, FilterResult, FilterSpec};
use log_store::LogStore;
use model::{Level, LogBuffer, LogEntry, SavedFilter};
use regex::{Regex, RegexBuilder};

const MIB: usize = 1024 * 1024;
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(750);
const FILTER_DEBOUNCE: Duration = Duration::from_millis(120);
const CONFIG_SAVE_DELAY: Duration = Duration::from_millis(600);
const APP_FONT_NAME: &str = "Cascadia Next SC NF";
const BACKEND_QUEUE_CAPACITY: usize = 4096;
const BACKEND_FRAME_BUDGET: Duration = Duration::from_millis(4);
const BACKEND_EVENTS_PER_FRAME: usize = 2048;

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
    Paused,
    Error(String),
}

impl CaptureState {
    fn label(&self) -> &str {
        match self {
            Self::NoAdb => "未找到 ADB",
            Self::WaitingForDevice => "未选择设备",
            Self::Connecting => "正在启动 Logcat",
            Self::Running => "实时 Logcat",
            Self::Paused => "已暂停采集",
            Self::Error(error) => error,
        }
    }

    fn color(&self) -> Color32 {
        match self {
            Self::Running => Color32::from_rgb(76, 194, 126),
            Self::Connecting => Color32::from_rgb(239, 190, 82),
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
    unresolved_processes: HashMap<u32, Vec<usize>>,
    processes: HashMap<u32, Arc<str>>,
    dropped_entries: usize,
    memory_limit_mib: usize,
    buffers: Vec<LogBuffer>,
    buffer_draft: Vec<LogBuffer>,

    package: String,
    package_popup_open: bool,
    packages: Vec<String>,
    min_level: Level,
    query: String,
    use_regex: bool,
    case_sensitive: bool,
    active_filter: FilterSpec,
    incremental_matcher: Result<Option<Regex>, String>,
    filter_tx: filter::FilterSender,
    filter_rx: Receiver<FilterResult>,
    filter_generation: u64,
    filter_pending: bool,
    filter_dirty_since: Option<Instant>,
    filter_error: Option<String>,
    filter_elapsed: Duration,
    highlight_key: (String, bool, bool),
    highlight_matcher: Option<Regex>,
    detail_lines: Option<(Arc<str>, Vec<std::ops::Range<usize>>)>,

    dark: bool,
    row_height: f32,
    show_details: bool,
    follow_logs: bool,
    scroll_to_bottom: bool,
    scroll_to_selected: bool,

    backend_tx: SyncSender<BackendEvent>,
    backend_rx: Receiver<BackendEvent>,
    adb_path: Option<PathBuf>,
    adb_status: String,
    devices: Vec<DeviceInfo>,
    selected_device: Option<String>,
    device_query_pending: bool,
    package_query_pending: bool,
    process_query_pending: bool,
    last_device_query: Instant,
    last_process_query: Instant,
    capture_handle: Option<LogcatHandle>,
    capture_session_id: u64,
    capture_state: CaptureState,
    last_capture_error: Option<String>,

    logs_in_rate_window: usize,
    rate_window_started: Instant,
    logs_per_second: f64,

    saved_filters: Vec<SavedFilter>,
    show_save_filter: bool,
    save_filter_name: String,
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

        let (filter_result_tx, filter_rx) = mpsc::channel::<FilterResult>();
        let filter_tx = filter::spawn_filter_worker(filter_result_tx, ctx.clone());

        let (backend_tx, backend_rx) = mpsc::sync_channel::<BackendEvent>(BACKEND_QUEUE_CAPACITY);
        let active_filter = FilterSpec {
            package: config.package.clone(),
            min_level: config.min_level,
            query: config.query.clone(),
            regex: config.use_regex,
            case_sensitive: config.case_sensitive,
        };
        let incremental_matcher = filter::compile_matcher(&active_filter);
        let now = Instant::now();
        let has_adb = adb_path.is_some();

        Self {
            entries: Arc::new(LogStore::default()),
            matches: Vec::new(),
            selected: None,
            selection: selection::RowSelection::default(),
            unresolved_processes: HashMap::new(),
            processes: HashMap::new(),
            dropped_entries: 0,
            memory_limit_mib: config.memory_limit_mib.clamp(16, 4096),
            buffers: config.buffers.clone(),
            buffer_draft: config.buffers,

            package: config.package,
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
            filter_dirty_since: None,
            filter_error: None,
            filter_elapsed: Duration::ZERO,
            highlight_key: (String::new(), false, false),
            highlight_matcher: None,
            detail_lines: None,

            dark: config.dark,
            row_height: config.row_height,
            show_details: config.show_details,
            follow_logs: true,
            scroll_to_bottom: false,
            scroll_to_selected: false,

            backend_tx,
            backend_rx,
            adb_path,
            adb_status: String::new(),
            devices: Vec::new(),
            selected_device: None,
            device_query_pending: false,
            package_query_pending: false,
            process_query_pending: false,
            last_device_query: now.checked_sub(DEVICE_POLL_INTERVAL).unwrap_or(now),
            last_process_query: now.checked_sub(PROCESS_POLL_INTERVAL).unwrap_or(now),
            capture_handle: None,
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
            show_save_filter: false,
            save_filter_name: String::new(),
            config_dirty_since: None,
            config_error: None,
        }
    }

    fn current_filter(&self) -> FilterSpec {
        FilterSpec {
            package: self.package.trim().to_owned(),
            min_level: self.min_level,
            query: self.query.clone(),
            regex: self.use_regex,
            case_sensitive: self.case_sensitive,
        }
    }

    fn queue_filter(&mut self) {
        self.filter_dirty_since = Some(Instant::now());
        self.mark_config_dirty();
    }

    fn schedule_filter(&mut self) {
        self.filter_dirty_since = None;
        self.filter_generation += 1;
        self.filter_pending = true;
        self.filter_error = None;
        self.active_filter = self.current_filter();
        self.incremental_matcher = filter::compile_matcher(&self.active_filter);
        if let Err(error) = &self.incremental_matcher {
            self.filter_error = Some(error.clone());
        }
        self.filter_tx.send(FilterRequest {
            generation: self.filter_generation,
            entries: Arc::clone(&self.entries),
            spec: self.active_filter.clone(),
        });
    }

    fn poll_filter(&mut self) {
        while let Ok(mut result) = self.filter_rx.try_recv() {
            if result.generation != self.filter_generation {
                continue;
            }
            self.filter_pending = false;
            self.filter_elapsed = result.elapsed;
            self.filter_error = result.error;
            if self.filter_error.is_some() {
                continue;
            }

            if let Ok(matcher) = &self.incremental_matcher {
                result.matches.extend(
                    self.entries
                        .iter()
                        .enumerate()
                        .skip(result.processed_len)
                        .filter_map(|(index, entry)| {
                            filter::entry_matches(entry, &self.active_filter, matcher.as_ref())
                                .then_some(index)
                        }),
                );
            }
            self.matches = result.matches;
            self.selection.retain(&self.matches);
            if self
                .selected
                .is_some_and(|index| self.matches.binary_search(&index).is_err())
            {
                self.selected = None;
            }
        }
    }

    fn append_entry(&mut self, mut entry: LogEntry) {
        if entry.pid != 0
            && let Some(process) = self.processes.get(&entry.pid)
        {
            entry.process = Some(Arc::clone(process));
        }

        let index = self.entries.len();
        if entry.pid != 0 && entry.process.is_none() {
            self.unresolved_processes
                .entry(entry.pid)
                .or_default()
                .push(index);
        }
        let matches = !self.filter_pending
            && self.incremental_matcher.as_ref().is_ok_and(|matcher| {
                filter::entry_matches(&entry, &self.active_filter, matcher.as_ref())
            });
        Arc::make_mut(&mut self.entries).push(entry);
        if matches {
            self.matches.push(index);
        }
        self.logs_in_rate_window += 1;

        if self.entries.memory_bytes() > self.memory_limit_mib * MIB {
            self.trim_old_entries();
        }
    }

    fn trim_old_entries(&mut self) {
        let remove =
            Arc::make_mut(&mut self.entries).trim_to_bytes(self.memory_limit_mib * MIB * 9 / 10);
        if remove == 0 {
            return;
        }
        self.dropped_entries += remove;
        self.matches = Vec::new();
        self.selected = None;
        self.selection.clear();
        self.detail_lines = None;
        self.unresolved_processes = HashMap::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.pid != 0 && entry.process.is_none() {
                self.unresolved_processes
                    .entry(entry.pid)
                    .or_default()
                    .push(index);
            }
        }
        self.schedule_filter();
    }

    fn update_processes(&mut self, processes: HashMap<u32, String>) {
        let processes: HashMap<u32, Arc<str>> = processes
            .into_iter()
            .map(|(pid, name)| (pid, Arc::from(name)))
            .collect();
        let mut resolved = Vec::new();
        for (pid, process) in &processes {
            if let Some(indices) = self.unresolved_processes.remove(pid) {
                resolved.push((indices, Arc::clone(process)));
            }
        }
        self.processes = processes;

        if resolved.is_empty() {
            return;
        }
        let entries = Arc::make_mut(&mut self.entries);
        for (indices, process) in resolved {
            for index in indices {
                entries.set_process(index, Arc::clone(&process));
            }
        }
        if !self.package.trim().is_empty() {
            self.schedule_filter();
        }
        if self.entries.memory_bytes() > self.memory_limit_mib * MIB {
            self.trim_old_entries();
        }
    }

    fn poll_backend(&mut self, ctx: &egui::Context) {
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
                            self.adb_status.clear();
                            self.devices = devices;
                            self.reconcile_device_selection(ctx);
                        }
                        Err(error) => {
                            self.adb_status = error;
                            self.devices.clear();
                            self.reconcile_device_selection(ctx);
                        }
                    }
                }
                BackendEvent::Packages { serial, result } => {
                    if self.selected_device.as_deref() != Some(&serial) {
                        continue;
                    }
                    self.package_query_pending = false;
                    match result {
                        Ok(packages) => self.packages = packages,
                        Err(error) => self.adb_status = error,
                    }
                }
                BackendEvent::Processes { serial, result } => {
                    if self.selected_device.as_deref() != Some(&serial) {
                        continue;
                    }
                    self.process_query_pending = false;
                    if let Ok(processes) = result {
                        self.update_processes(processes);
                    }
                }
                BackendEvent::Logcat { session_id, event } => {
                    if session_id != self.capture_session_id {
                        continue;
                    }
                    match event {
                        LogcatEvent::Started => self.capture_state = CaptureState::Running,
                        LogcatEvent::Entry(entry) => self.append_entry(entry.into_inner()),
                        LogcatEvent::Stderr(error) => self.last_capture_error = Some(error),
                        LogcatEvent::Exited { code, error } => {
                            self.capture_handle = None;
                            let message = error
                                .or_else(|| self.last_capture_error.take())
                                .unwrap_or_else(|| match code {
                                    Some(code) => format!("Logcat stopped ({code})"),
                                    None => "Logcat stopped".to_owned(),
                                });
                            self.capture_state = CaptureState::Error(message);
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

        self.schedule_periodic_queries(ctx);
    }

    fn schedule_periodic_queries(&mut self, ctx: &egui::Context) {
        let Some(adb_path) = self.adb_path.clone() else {
            return;
        };
        if !self.device_query_pending && self.last_device_query.elapsed() >= DEVICE_POLL_INTERVAL {
            self.device_query_pending = true;
            self.last_device_query = Instant::now();
            adb::spawn_device_query(adb_path.clone(), self.backend_tx.clone(), ctx.clone());
        }
        if let Some(serial) = self.selected_device.clone()
            && self.selected_device_available()
            && self.capture_handle.is_some()
            && !self.process_query_pending
            && self.last_process_query.elapsed() >= PROCESS_POLL_INTERVAL
        {
            self.process_query_pending = true;
            self.last_process_query = Instant::now();
            adb::spawn_process_query(adb_path, serial, self.backend_tx.clone(), ctx.clone());
        }
    }

    fn reconcile_device_selection(&mut self, ctx: &egui::Context) {
        if self.selected_device_available() {
            if self.capture_handle.is_none()
                && matches!(self.capture_state, CaptureState::WaitingForDevice)
            {
                self.start_capture(ctx);
            }
            return;
        }

        if self.capture_handle.is_some() {
            self.stop_capture();
        }
        if self.selected_device.take().is_some() {
            self.package_query_pending = false;
            self.process_query_pending = false;
            self.packages.clear();
            self.processes.clear();
            self.unresolved_processes.clear();
        }
        self.capture_state = if self.adb_path.is_some() {
            CaptureState::WaitingForDevice
        } else {
            CaptureState::NoAdb
        };
    }

    fn selected_device_available(&self) -> bool {
        self.selected_device.as_deref().is_some_and(|serial| {
            self.devices
                .iter()
                .any(|device| device.serial == serial && device.available())
        })
    }

    fn switch_device(&mut self, serial: String, ctx: &egui::Context) {
        if self.selected_device.as_deref() == Some(&serial) && self.capture_handle.is_some() {
            return;
        }
        self.stop_capture();
        self.selected_device = Some(serial);
        self.package_query_pending = false;
        self.process_query_pending = false;
        self.packages.clear();
        self.processes.clear();
        self.unresolved_processes.clear();
        self.clear_logs();

        if self.selected_device_available() {
            self.query_packages(ctx);
            self.last_process_query = Instant::now()
                .checked_sub(PROCESS_POLL_INTERVAL)
                .unwrap_or_else(Instant::now);
            self.start_capture(ctx);
        } else {
            self.capture_state = CaptureState::WaitingForDevice;
        }
    }

    fn start_capture(&mut self, ctx: &egui::Context) {
        if self.capture_handle.is_some() || !self.selected_device_available() {
            return;
        }
        let (Some(adb_path), Some(serial)) =
            (self.adb_path.as_ref(), self.selected_device.as_deref())
        else {
            return;
        };
        self.capture_session_id += 1;
        self.last_capture_error = None;
        self.capture_state = CaptureState::Connecting;
        match adb::start_logcat(
            adb_path,
            serial,
            &self.buffers,
            self.capture_session_id,
            self.backend_tx.clone(),
            ctx.clone(),
        ) {
            Ok(handle) => self.capture_handle = Some(handle),
            Err(error) => self.capture_state = CaptureState::Error(error),
        }
    }

    fn stop_capture(&mut self) {
        self.capture_session_id += 1;
        if let Some(mut handle) = self.capture_handle.take() {
            handle.stop();
        }
    }

    fn pause_capture(&mut self) {
        self.stop_capture();
        self.capture_state = CaptureState::Paused;
        self.logs_per_second = 0.0;
        self.logs_in_rate_window = 0;
    }

    fn close_capture(&mut self) {
        self.stop_capture();
        self.selected_device = None;
        self.packages.clear();
        self.processes.clear();
        self.unresolved_processes.clear();
        self.package_query_pending = false;
        self.process_query_pending = false;
        self.capture_state = if self.adb_path.is_some() {
            CaptureState::WaitingForDevice
        } else {
            CaptureState::NoAdb
        };
        self.logs_per_second = 0.0;
        self.logs_in_rate_window = 0;
    }

    fn apply_buffers(&mut self, ctx: &egui::Context) {
        LogBuffer::normalize(&mut self.buffer_draft);
        if self.buffers == self.buffer_draft {
            return;
        }
        let restart = self.capture_handle.is_some();
        self.stop_capture();
        self.buffers.clone_from(&self.buffer_draft);
        self.mark_config_dirty();
        if restart {
            self.start_capture(ctx);
        }
    }

    fn retry_capture(&mut self, ctx: &egui::Context) {
        self.stop_capture();
        if self.selected_device_available() {
            self.start_capture(ctx);
        } else {
            self.capture_state = CaptureState::WaitingForDevice;
        }
    }

    fn clear_logs(&mut self) {
        self.entries = Arc::new(LogStore::default());
        self.matches = Vec::new();
        self.selected = None;
        self.selection.clear();
        self.detail_lines = None;
        self.unresolved_processes = HashMap::new();
        self.dropped_entries = 0;
        self.filter_generation += 1;
        self.filter_pending = false;
        self.filter_elapsed = Duration::ZERO;
    }

    fn query_packages(&mut self, ctx: &egui::Context) {
        let (Some(adb_path), Some(serial)) = (self.adb_path.clone(), self.selected_device.clone())
        else {
            return;
        };
        self.package_query_pending = true;
        adb::spawn_package_query(adb_path, serial, self.backend_tx.clone(), ctx.clone());
    }

    fn refresh_devices(&mut self, ctx: &egui::Context) {
        if self.device_query_pending {
            return;
        }
        let Some(adb_path) = adb::find_adb() else {
            self.adb_path = None;
            self.devices.clear();
            self.adb_status.clear();
            self.reconcile_device_selection(ctx);
            return;
        };
        self.adb_path = Some(adb_path.clone());
        self.adb_status.clear();
        if matches!(self.capture_state, CaptureState::NoAdb) {
            self.capture_state = CaptureState::WaitingForDevice;
        }
        self.device_query_pending = true;
        self.last_device_query = Instant::now();
        adb::spawn_device_query(adb_path, self.backend_tx.clone(), ctx.clone());
    }

    fn apply_saved_filter(&mut self, saved: SavedFilter) {
        self.package = saved.package;
        self.min_level = saved.min_level;
        self.query = saved.query;
        self.use_regex = saved.regex;
        self.case_sensitive = saved.case_sensitive;
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

    fn app_config(&self) -> AppConfig {
        AppConfig {
            memory_limit_mib: self.memory_limit_mib,
            buffers: self.buffers.clone(),
            package: self.package.clone(),
            min_level: self.min_level,
            query: self.query.clone(),
            use_regex: self.use_regex,
            case_sensitive: self.case_sensitive,
            dark: self.dark,
            row_height: self.row_height,
            show_details: self.show_details,
            saved_filters: self.saved_filters.clone(),
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

        egui::Panel::top("toolbar")
            .exact_size(48.0)
            .show(root_ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new("Device").weak());

                    let selected_label = self
                        .selected_device
                        .as_deref()
                        .and_then(|serial| {
                            self.devices.iter().find(|device| device.serial == serial)
                        })
                        .map(DeviceInfo::display_name)
                        .unwrap_or_else(|| {
                            if self.devices.is_empty() {
                                "无设备".to_owned()
                            } else {
                                "选择设备".to_owned()
                            }
                        });
                    egui::ComboBox::from_id_salt("device")
                        .width(if compact { 130.0 } else { 170.0 })
                        .truncate()
                        .selected_text(selected_label)
                        .show_ui(ui, |ui| {
                            if self.devices.is_empty() {
                                ui.add_enabled(false, egui::Button::new("无设备"));
                            }
                            for device in &self.devices {
                                let response = ui.add_enabled(
                                    device.available(),
                                    egui::Button::selectable(
                                        self.selected_device.as_deref() == Some(&device.serial),
                                        device.display_name(),
                                    ),
                                );
                                if response.clicked() {
                                    response.request_focus();
                                    selected_device = Some(device.serial.clone());
                                    ui.close();
                                }
                            }
                        });
                    if ui
                        .add_enabled(!self.device_query_pending, egui::Button::new("↻ 刷新"))
                        .on_hover_text("刷新设备")
                        .clicked()
                    {
                        self.refresh_devices(&ctx);
                    }

                    let package_response = ui.add_sized(
                        [if compact { 160.0 } else { 190.0 }, 28.0],
                        egui::TextEdit::singleline(&mut self.package)
                            .hint_text("Package · all processes")
                            .font(TextStyle::Monospace)
                            .id(egui::Id::new("package_filter")),
                    );
                    filter_changed |= package_response.changed();
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
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
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

                    let query_width =
                        (ui.available_width() - 250.0).max(if compact { 100.0 } else { 160.0 });
                    let query_response = ui.add_sized(
                        [query_width, 28.0],
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Filter messages · press / to focus")
                            .font(TextStyle::Monospace)
                            .id(egui::Id::new("query_shortcut")),
                    );
                    filter_changed |= query_response.changed();
                    filter_changed |= ui
                        .toggle_value(&mut self.use_regex, ".*")
                        .on_hover_text("Use a regular expression")
                        .changed();
                    filter_changed |= ui
                        .toggle_value(&mut self.case_sensitive, "Aa")
                        .on_hover_text("Match case")
                        .changed();

                    egui::ComboBox::from_id_salt("saved_filters")
                        .width(112.0)
                        .selected_text("Saved filters")
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

                    ui.add_space(4.0);
                });
            });

        if let Some(serial) = selected_device {
            self.switch_device(serial, &ctx);
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

        egui::Panel::top("capture_controls").exact_size(40.0).show(root_ui, |ui| {
            ui.horizontal_centered(|ui| {
                let capturing = self.capture_handle.is_some();
                if ui.add_enabled(self.selected_device_available(), egui::Button::new(if capturing { "暂停" } else { "开启" }))
                    .on_hover_text("暂停会停止接收；开启后从最新日志继续，不回补暂停期间的日志").clicked() {
                    if capturing { self.pause_capture(); } else { self.start_capture(&ctx); }
                }
                if ui.add_enabled(self.selected_device.is_some(), egui::Button::new("关闭"))
                    .on_hover_text("结束当前设备会话，保留已有日志；重新选择设备后可再次采集").clicked() {
                    self.close_capture();
                }
                ui.separator();
                let summary = self.buffers.iter().map(|buffer| buffer.name()).collect::<Vec<_>>().join("+");
                egui::containers::menu::MenuButton::new(format!("缓冲区: {summary}"))
                    .config(egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside))
                    .ui(ui, |ui| {
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
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(format!("日志内存 {:.1} / {} MiB", self.entries.memory_bytes() as f64 / MIB as f64, self.memory_limit_mib))
                        .on_hover_text("文本、存储结构与索引的估算预算；不包含图形后端等进程开销。可在 View 中调整。");
                });
            });
        });

        egui::Panel::top("sub_toolbar")
            .exact_size(40.0)
            .show(root_ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new("Minimum level").weak().size(12.0));
                    for level in Level::ALL {
                        let response = ui
                            .add_sized(
                                [28.0, 28.0],
                                egui::Button::selectable(
                                    self.min_level == level,
                                    RichText::new(level.short())
                                        .strong()
                                        .color(level.color(self.dark)),
                                ),
                            )
                            .on_hover_text(format!("{level:?} and above"));
                        if response.clicked() {
                            response.request_focus();
                            self.min_level = level;
                            filter_changed = true;
                        }
                    }

                    ui.separator();
                    ui.menu_button("View", |ui| {
                        ui.horizontal(|ui| {
                            ui.label("日志内存预算");
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.memory_limit_mib)
                                        .range(16..=4096)
                                        .suffix(" MiB"),
                                )
                                .changed()
                            {
                                if self.entries.memory_bytes() > self.memory_limit_mib * MIB {
                                    self.trim_old_entries();
                                }
                                self.mark_config_dirty();
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

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(!self.entries.is_empty(), egui::Button::new("Clear"))
                            .on_hover_text("Clear this view only")
                            .clicked()
                        {
                            self.clear_logs();
                        }
                        ui.separator();
                        if ui
                            .button(if self.follow_logs {
                                "↓ 正在跟随"
                            } else {
                                "↓ 回到最新"
                            })
                            .on_hover_text("回到最新并恢复自动跟随 · Ctrl+End；浏览时继续采集")
                            .clicked()
                        {
                            self.follow_logs = true;
                            self.scroll_to_bottom = true;
                        }
                        if matches!(self.capture_state, CaptureState::Error(_))
                            && ui.button("Retry").clicked()
                        {
                            self.retry_capture(&ctx);
                        }
                        ui.separator();
                        if let Some(error) = &self.filter_error {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(error).color(Color32::from_rgb(235, 91, 101)),
                                )
                                .truncate(),
                            )
                            .on_hover_text(error);
                        } else if self.filter_pending || self.filter_dirty_since.is_some() {
                            ui.spinner();
                            ui.label(RichText::new("Filtering…").weak());
                        } else {
                            ui.label(
                                RichText::new(if compact {
                                    format!("{} matches", format_count(self.matches.len()))
                                } else {
                                    format!(
                                        "{} matches · {:.1} ms",
                                        format_count(self.matches.len()),
                                        self.filter_elapsed.as_secs_f64() * 1000.0
                                    )
                                })
                                .weak()
                                .monospace(),
                            );
                        }
                    });
                });
            });

        if filter_changed {
            self.queue_filter();
        }
    }

    fn details(&mut self, root_ui: &mut egui::Ui) {
        if !self.show_details || self.selected.is_none() {
            return;
        }
        let ctx = root_ui.ctx().clone();

        egui::Panel::bottom("details")
            .resizable(true)
            .default_size(132.0)
            .size_range(84.0..=260.0)
            .show(root_ui, |ui| {
                ui.add_space(6.0);
                if let Some(index) = self.selected {
                    let entry = &self.entries[index];
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Selected log").strong());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .button("Copy")
                                .on_hover_text("Copy complete log · Ctrl+C")
                                .clicked()
                            {
                                ctx.copy_text(entry.copy_text());
                            }
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!(
                                        "{}  ·  {}/{}  ·  {}  ·  {}",
                                        entry.time,
                                        entry.pid,
                                        entry.tid,
                                        entry.process_name(),
                                        entry.tag
                                    ))
                                    .weak()
                                    .monospace(),
                                )
                                .truncate(),
                            );
                        });
                    });
                    ui.separator();
                    if entry.message.len() > 16_384 {
                        if self
                            .detail_lines
                            .as_ref()
                            .is_none_or(|(message, _)| !Arc::ptr_eq(message, &entry.message))
                        {
                            self.detail_lines =
                                Some((Arc::clone(&entry.message), detail_ranges(&entry.message)));
                        }
                        let (message, ranges) = self.detail_lines.as_ref().unwrap();
                        ui.label(
                            RichText::new(
                                "Long message · shown in segments · Copy preserves the original",
                            )
                            .weak()
                            .size(11.0),
                        );
                        egui::ScrollArea::both().id_salt("long_details").show_rows(
                            ui,
                            ui.text_style_height(&TextStyle::Monospace),
                            ranges.len(),
                            |ui, visible| {
                                for index in visible {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(
                                                message[ranges[index].clone()]
                                                    .trim_end_matches(['\r', '\n']),
                                            )
                                            .monospace(),
                                        )
                                        .wrap_mode(egui::TextWrapMode::Extend)
                                        .selectable(true),
                                    );
                                }
                            },
                        );
                    } else {
                        self.detail_lines = None;
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(entry.message.as_ref())
                                        .monospace()
                                        .color(entry.level.color(self.dark)),
                                )
                                .selectable(true),
                            );
                        });
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            RichText::new("Select a row to inspect and copy the full message")
                                .weak(),
                        );
                    });
                }
            });
    }

    fn log_table(&mut self, root_ui: &mut egui::Ui) {
        let ctx = root_ui.ctx().clone();
        let compact = root_ui.available_width() < 1050.0;
        if self.highlight_key.0 != self.query
            || self.highlight_key.1 != self.use_regex
            || self.highlight_key.2 != self.case_sensitive
        {
            self.highlight_matcher =
                compile_highlight_regex(&self.query, self.use_regex, self.case_sensitive);
            self.highlight_key = (self.query.clone(), self.use_regex, self.case_sensitive);
        }
        let highlight = self.highlight_matcher.as_ref();
        let selected = self.selected;
        let selection = &self.selection;
        let row_height = self.row_height;
        let dark = self.dark;
        let entries = Arc::clone(&self.entries);
        let matches = &self.matches;
        let mut clicked_row = None;
        let mut filter_process = None;

        egui::CentralPanel::default().show(root_ui, |ui| {
            ui.style_mut().interaction.selectable_labels = false;
            // Stop before laying out rows: a press and release can span many
            // capture updates, so waiting for clicked() lets the target move.
            let browsing = ctx.input(|input| {
                input.pointer.hover_pos().is_some_and(|pos| ui.max_rect().contains(pos))
                    && (input.pointer.any_pressed() || input.smooth_scroll_delta.y > 0.0)
            });
            if browsing {
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
                        "No matching logs. Check the package, minimum level and message filter."
                    } else {
                        match self.capture_state {
                            CaptureState::NoAdb => "Install Android platform-tools, then click Refresh to find ADB.",
                            CaptureState::WaitingForDevice => "Connect and authorize an Android device, then select it above.",
                            CaptureState::Error(_) => "Capture stopped. Click Retry to resume.",
                            CaptureState::Paused => "采集已暂停。点击开启继续接收最新日志。",
                            _ => "Waiting for logs from the selected device…",
                        }
                    });
                });
                return;
            }
            let mut table = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::exact(145.0))
                .column(Column::exact(32.0))
                .column(Column::exact(90.0))
                .column(Column::initial(if compact { 140.0 } else { 210.0 }).at_least(100.0))
                .column(Column::initial(if compact { 110.0 } else { 165.0 }).at_least(80.0))
                .column(Column::remainder().at_least(180.0))
                .min_scrolled_height(0.0)
                .sense(egui::Sense::click())
                .animate_scrolling(false)
                .stick_to_bottom(self.follow_logs);
            if self.scroll_to_bottom && !matches.is_empty() {
                table = table.scroll_to_row(matches.len() - 1, Some(Align::BOTTOM));
            } else if self.scroll_to_selected
                && let Some(position) = selected.and_then(|index| matches.binary_search(&index).ok())
            {
                table = table.scroll_to_row(position, None);
            }

            table
                .header(30.0, |mut header| {
                    for title in ["TIME", "LV", "PID / TID", "PROCESS", "TAG", "MESSAGE"] {
                        header.col(|ui| {
                            ui.label(RichText::new(title).weak().size(11.0).strong());
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_height, matches.len(), |mut row| {
                        let match_index = row.index();
                        let entry_index = matches[match_index];
                        let entry = &entries[entry_index];
                        row.set_selected(selection.contains(entry_index));

                        row.col(|ui| {
                            ui.label(RichText::new(entry.time.as_ref()).monospace().size(12.0));
                        });
                        row.col(|ui| {
                            ui.label(
                                RichText::new(entry.level.short())
                                    .monospace()
                                    .strong()
                                    .color(entry.level.color(dark)),
                            );
                        });
                        row.col(|ui| {
                            ui.label(
                                RichText::new(format!("{} / {}", entry.pid, entry.tid))
                                    .monospace()
                                    .size(12.0),
                            );
                        });
                        row.col(|ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(entry.process_name()).monospace().size(12.0),
                                )
                                .truncate(),
                            );
                        });
                        row.col(|ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(entry.tag.as_ref())
                                        .monospace()
                                        .size(12.0)
                                        .color(entry.level.color(dark)),
                                )
                                .truncate(),
                            );
                        });
                        row.col(|ui| {
                            let job = highlighted_message(
                                preview_message(entry.first_line()),
                                highlight,
                                ui.visuals().text_color(),
                                dark,
                            );
                            ui.add(egui::Label::new(job).truncate());
                        });

                        let response = row.response();
                        if response.clicked() {
                            response.request_focus();
                            clicked_row = Some((entry_index, ctx.input(|input| input.modifiers)));
                        }
                        response.context_menu(|ui| {
                            if selection.contains(entry_index) && ui.button("Copy selected logs").clicked() {
                                ctx.copy_text(selection.copy_text(&entries));
                                ui.close();
                            }
                            if ui.button("Copy full log").clicked() {
                                ctx.copy_text(entry.copy_text());
                                ui.close();
                            }
                            if entry.process.is_some() && ui.button("Filter this process").clicked()
                            {
                                filter_process = entry.process.as_deref().map(ToOwned::to_owned);
                                ui.close();
                            }
                        });
                    });
                });
        });

        self.scroll_to_bottom = false;
        self.scroll_to_selected = false;
        if let Some((index, modifiers)) = clicked_row {
            self.selection
                .select(&self.matches, index, modifiers.shift, modifiers.command);
            self.selected = self.selection.contains(index).then_some(index);
            self.scroll_to_selected = true;
        }
        if let Some(process) = filter_process {
            self.package = process;
            self.queue_filter();
        }
    }

    fn status_bar(&self, root_ui: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .exact_size(26.0)
            .show(root_ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("●")
                            .color(self.capture_state.color())
                            .size(10.0),
                    );
                    ui.label(RichText::new(self.capture_state.label()).weak().size(11.0));
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
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(error) = self.config_error.as_ref() {
                            ui.label(RichText::new(error).color(Color32::from_rgb(235, 91, 101)));
                        } else if !self.adb_status.is_empty() {
                            ui.label(RichText::new(&self.adb_status).weak().size(11.0));
                        }
                    });
                });
            });
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.show_save_filter || ctx.current_pass_index() != 0 {
            return;
        }
        let text_focused = ctx.text_edit_focused();
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::F))
            || (!text_focused
                && ctx
                    .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Slash)))
        {
            ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("query_shortcut")));
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            if text_focused
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
            } else {
                self.selected = None;
                self.selection.clear();
            }
            return;
        }
        if text_focused || self.package_popup_open || egui::Popup::is_any_open(ctx) {
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::End)) {
            self.follow_logs = true;
            self.scroll_to_bottom = true;
            return;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::A)) {
            self.follow_logs = false;
            self.selection.select_all(&self.matches);
            self.selected = self.selected.or_else(|| self.matches.first().copied());
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
                self.selected = navigated_selection(&self.matches, self.selected, key);
                if let Some(index) = self.selected {
                    self.selection.select(&self.matches, index, extend, false);
                }
                self.scroll_to_selected = true;
                break;
            }
        }
    }

    fn copy_shortcut(&self, ctx: &egui::Context) {
        if self.show_save_filter || ctx.text_edit_focused() || egui::Popup::is_any_open(ctx) {
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
            ctx.copy_text(self.selection.copy_text(&self.entries));
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.show_save_filter {
            let mut open = true;
            let mut save = false;
            let mut cancel = false;
            egui::Window::new("Save filter")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Name");
                    let response = ui.add_sized(
                        [320.0, 28.0],
                        egui::TextEdit::singleline(&mut self.save_filter_name)
                            .hint_text("e.g. Network failures"),
                    );
                    response.request_focus();
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
            self.show_save_filter = open && !cancel;
            if save {
                self.save_current_filter();
            }
        }
    }
}

impl eframe::App for CiallogcatApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_filter();
        self.poll_backend(ctx);
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();

        self.shortcuts(&ctx);

        self.toolbar(root_ui);
        self.status_bar(root_ui);
        self.details(root_ui);
        self.log_table(root_ui);
        self.dialogs(&ctx);
        self.copy_shortcut(&ctx);
    }
}

impl Drop for CiallogcatApp {
    fn drop(&mut self) {
        self.stop_capture();
        #[cfg(not(test))]
        let _ = self.app_config().save();
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

    ctx.set_fonts(fonts);
}

fn compile_highlight_regex(query: &str, use_regex: bool, case_sensitive: bool) -> Option<Regex> {
    if query.is_empty() {
        return None;
    }
    let pattern = if use_regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .ok()
}

fn highlighted_message(
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
