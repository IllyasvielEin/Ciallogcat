//! Background file operations and source switching for the desktop viewer.
use std::fs::File;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
};

use super::*;
use ciallogcat::{
    document::{self, CaptureStatus, Document, DocumentHeader, ExportScope},
    file_io,
};

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Open,
    Save,
    Export,
    SaveBeforeOpen,
}

struct PathDialog {
    action: Action,
    path: String,
    focused: bool,
}

fn filter_description(spec: &FilterSpec) -> serde_json::Value {
    serde_json::json!({"package":spec.package,"tag_include":spec.tag_include,"tag_exclude":spec.tag_exclude,"minimum_level":spec.min_level.short(),"query":spec.query,"regex":spec.regex,"case_sensitive":spec.case_sensitive})
}
struct Job {
    cancel: Arc<AtomicBool>,
    receiver: Receiver<Result<Completed, String>>,
    path: PathBuf,
    action: Action,
}
enum Completed {
    Loaded(Box<Document>, usize),
    Saved(document::WriteSummary),
}
#[derive(Clone, Copy, PartialEq)]
enum Replace {
    Discard,
    Save,
}

#[derive(Default)]
pub(super) struct FileUiState {
    pub document: Option<Arc<Document>>,
    path: Option<PathBuf>,
    pub document_bytes: usize,
    dialog: Option<PathDialog>,
    job: Option<Job>,
    candidate: Option<(Document, PathBuf, usize)>,
    replace: Option<Replace>,
    pub notice: Option<String>,
    retry: Option<(Action, PathBuf)>,
}

impl FileUiState {
    pub fn is_modal(&self) -> bool {
        self.dialog.is_some() || self.candidate.is_some()
    }

    fn busy(&self) -> bool {
        self.job.is_some() || self.is_modal()
    }
}

impl Drop for FileUiState {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Release);
        }
    }
}

enum Snapshot {
    File(Arc<Document>),
    Live(Box<LiveSnapshot>),
}

struct LiveSnapshot {
    header: DocumentHeader,
    entries: Arc<LogStore>,
    scope: ExportScope,
    capture: CaptureStatus,
}

impl Snapshot {
    fn document(self, cancel: &AtomicBool) -> Result<Arc<Document>, String> {
        match self {
            Self::File(document) => Ok(document),
            Self::Live(snapshot) => {
                let LiveSnapshot {
                    header,
                    entries,
                    scope,
                    capture,
                } = *snapshot;
                Document::from_snapshot_cancellable(header, entries, scope, capture, cancel)
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            }
        }
    }
}

impl CiallogcatApp {
    pub(super) fn file_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("文件", |ui| {
            let ready = !self.files.busy();
            if ui
                .add_enabled(ready, egui::Button::new("打开日志…    Ctrl+O"))
                .clicked()
            {
                self.file_dialog(Action::Open);
                ui.close();
            }
            let has_source = self.files.document.is_some() || self.retained_device.is_some();
            if ui
                .add_enabled(
                    ready && has_source,
                    egui::Button::new("另存保留日志…    Ctrl+Shift+S"),
                )
                .clicked()
            {
                self.file_dialog(Action::Save);
                ui.close();
            }
            if ui
                .add_enabled(
                    ready
                        && has_source
                        && !self.filter_pending
                        && self.filter_dirty_since.is_none(),
                    egui::Button::new("导出当前筛选结果…"),
                )
                .clicked()
            {
                self.file_dialog(Action::Export);
                ui.close();
            }
            if ui
                .add_enabled(
                    ready && self.files.document.is_some(),
                    egui::Button::new("关闭文件"),
                )
                .clicked()
            {
                self.files.document = None;
                self.files.path = None;
                self.clear_logs();
                self.retained_device = None;
                self.live_session = Session::new();
                self.capture_facts = ciallogcat::capture_facts::CaptureFacts::default();
                self.snapshot_budget = None;
                self.reset_scope_budget = true;
                self.process_watch = process_watch::Watcher::default();
                ui.close();
            }
            ui.separator();
            ui.label("可将 原生日志 JSONL 文件拖入窗口打开。");
            ui.label("另存为新文件；不会覆盖已有文件。");
        });
    }

    fn file_dialog(&mut self, action: Action) {
        let path = if let Some((_, path)) = self
            .files
            .retry
            .as_ref()
            .filter(|(failed, _)| *failed == action)
        {
            path.to_string_lossy().into_owned()
        } else if action == Action::Open {
            String::new()
        } else {
            let name = format!("ciallogcat-{}.jsonl", ciallogcat::session::SessionId::new());
            self.files
                .path
                .as_ref()
                .and_then(|p| p.parent())
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(name)
                .to_string_lossy()
                .into_owned()
        };
        self.files.dialog = Some(PathDialog {
            action,
            path,
            focused: false,
        });
        self.package_popup_open = false;
        self.query_history_open = false;
    }

    pub(super) fn file_shortcuts(&mut self, ctx: &egui::Context) {
        if self.files.busy()
            || self.pending_clear
            || self.pending_device.is_some()
            || self.show_save_filter
            || ctx.current_pass_index() != 0
        {
            return;
        }
        let action = if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
            Some(Action::Open)
        } else if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::S,
            )
        }) && (self.files.document.is_some() || self.retained_device.is_some())
        {
            Some(Action::Save)
        } else {
            None
        };
        if let Some(action) = action {
            egui::Popup::close_all(ctx);
            self.file_dialog(action);
        }
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if !dropped.is_empty() && !self.files.busy() {
            if dropped.len() == 1 {
                self.start_file_job(Action::Open, dropped[0].path().to_path_buf(), ctx);
            } else {
                self.files.notice = Some("请一次打开一个本地 JSONL 文件。".into());
            }
        }
    }

    fn snapshot_for_save(&self) -> Result<Snapshot, String> {
        if let Some(document) = &self.files.document {
            return Ok(Snapshot::File(Arc::clone(document)));
        }
        let serial = self.retained_device.clone().ok_or("没有可保存的设备会话")?;
        let mut header = DocumentHeader::live(self.live_session.id(), serial, self.buffers.clone());
        if let Some(report) = self.diagnostic_context() {
            header
                .source
                .extra
                .insert("device_diagnostics".into(), report);
        }
        header.source.extra.insert(
            "capture_scope".into(),
            serde_json::json!(self.capture_scope),
        );
        header.source.extra.insert(
            "logcat_scope_args".into(),
            serde_json::json!(self.capture_scope.logcat_args()),
        );
        if let Some(budget) = &self.snapshot_budget {
            header.source.extra.insert("snapshot_host_budget".into(), serde_json::json!({
                "duration_secs":15,"max_records":100_000,"max_accounted_bytes":budget.max_bytes,
                "byte_semantics":"incremental_retained_storage_estimate_including_chunks_not_wire_or_file_bytes",
                "capture_scope":self.capture_attempt_scope,
                "admitted_records":budget.records,"admitted_accounted_bytes":budget.bytes,"stop_reason":budget.stop_reason,
            }));
        }
        header.extra.insert(
            "view_filter".into(),
            filter_description(&self.active_filter),
        );
        header.source.extra.insert(
            "capturing_host".into(),
            serde_json::json!({
                "os":std::env::consts::OS,"architecture":std::env::consts::ARCH,
                "application_version":env!("CARGO_PKG_VERSION"),
            }),
        );
        if let Some(device) = self
            .devices
            .iter()
            .find(|device| Some(&device.serial) == self.retained_device.as_ref())
        {
            header.source.extra.insert(
                "device_discovery".into(),
                serde_json::json!({
                    "model":device.model,"state":device.state,"transport":device.transport,
                    "source":"latest_successful_discovery_snapshot",
                }),
            );
        }
        header.normalization.insert(
            "process_attribution".into(),
            serde_json::json!(
                "ps_snapshot_hint_only_for_default_follow;historical_scopes_remain_unknown"
            ),
        );
        let scope = ExportScope {
            omitted_before: Some(
                self.live_session
                    .last_seq()
                    .saturating_sub(self.entries.len() as u64),
            ),
            extra: self.capture_facts.retention_scope(),
            ..ExportScope::default()
        };
        let capture = self.capture_facts.snapshot(match self.capture_state {
            CaptureState::Running => "running",
            CaptureState::Connecting => "starting",
            CaptureState::Stopping => "stopping",
            CaptureState::Reconnecting { .. } => "reconnecting",
            CaptureState::Error(_) => "failed",
            CaptureState::Paused => "paused",
            _ => "stopped",
        });
        Ok(Snapshot::Live(Box::new(LiveSnapshot {
            header,
            entries: Arc::clone(&self.entries),
            scope,
            capture,
        })))
    }

    fn start_file_job(&mut self, action: Action, path: PathBuf, ctx: &egui::Context) {
        if self.files.job.is_some() {
            return;
        }
        let snapshot = if action == Action::Open {
            None
        } else {
            match self.snapshot_for_save() {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    self.file_operation_failed(action, path, error, false);
                    return;
                }
            }
        };
        let spec = (action == Action::Export).then(|| self.active_filter.clone());
        let budget = memory_budget_bytes(self.memory_limit_mib);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_path = path.clone();
        let wake = wake_signal(ctx);
        let (sender, receiver) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("log-file".into())
            .spawn(move || {
                let result = (|| -> Result<Completed, String> {
                    if action == Action::Open {
                        if !worker_path.metadata().map_err(|e| e.to_string())?.is_file() {
                            return Err("请选择普通日志文件".into());
                        }
                        let reader =
                            BufReader::new(File::open(&worker_path).map_err(|e| e.to_string())?);
                        return document::load(reader, budget, &worker_cancel)
                            .map(|document| {
                                let bytes = document.memory_bytes();
                                Completed::Loaded(Box::new(document), bytes)
                            })
                            .map_err(|e| e.to_string());
                    }
                    file_io::check_cancel(&worker_cancel).map_err(|e| e.to_string())?;
                    let mut document = snapshot.expect("save snapshot").document(&worker_cancel)?;
                    if let Some(spec) = spec {
                        let matcher = filter::compile_matcher(&spec)?;
                        let mut indices = Vec::new();
                        for (index, entry) in document.entries.iter().enumerate() {
                            file_io::check_cancel(&worker_cancel).map_err(|e| e.to_string())?;
                            if filter::entry_matches(entry, &spec, matcher.as_ref()) {
                                indices.push(index);
                            }
                        }
                        let description = filter_description(&spec);
                        let mut selected = document
                            .select_cancellable(&indices, description.clone(), &worker_cancel)
                            .map_err(|e| e.to_string())?;
                        // Only the new artifact owns this view; keep the opened document intact.
                        // scope.filter also retains any earlier selections of the source.
                        selected
                            .header
                            .extra
                            .insert("view_filter".into(), description);
                        document = Arc::new(selected);
                    }
                    file_io::write_new_file(&worker_path, &worker_cancel, |writer| {
                        document::write_snapshot(writer, &document, &worker_cancel)
                            .map_err(io::Error::other)
                    })
                    .map(Completed::Saved)
                    .map_err(|e| e.to_string())
                })();
                let _ = sender.send(result);
                wake.notify();
            });
        match spawned {
            Ok(_) => {
                self.files.notice = None;
                self.files.retry = None;
                self.files.job = Some(Job {
                    cancel,
                    receiver,
                    path,
                    action,
                });
            }
            Err(error) => {
                self.file_operation_failed(
                    action,
                    path,
                    format!("无法启动文件任务：{error}"),
                    false,
                );
            }
        }
    }

    fn file_operation_failed(
        &mut self,
        action: Action,
        path: PathBuf,
        error: String,
        cancelled: bool,
    ) {
        let status = if action == Action::SaveBeforeOpen {
            self.files.replace = None;
            self.files.candidate = None;
            "当前日志已保留，采集已停止。重新保存不会自动打开原候选文件。"
        } else {
            "当前日志已保留。"
        };
        self.files.notice = Some(format!(
            "文件操作未完成：{error}。{status} 路径：{}",
            path.display()
        ));
        // Retain only an editable path/action, never an old snapshot or a latent
        // source replacement. The user must confirm again against the current view.
        self.files.retry = (!cancelled).then_some((
            if action == Action::SaveBeforeOpen {
                Action::Save
            } else {
                action
            },
            path,
        ));
    }

    pub(super) fn poll_files(&mut self, ctx: &egui::Context) {
        let result = self
            .files
            .job
            .as_ref()
            .and_then(|job| match job.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err("文件任务意外结束".into())),
            });
        if let Some(result) = result {
            let job = self.files.job.take().expect("completed job");
            match result {
                Ok(Completed::Loaded(document, bytes)) if !job.cancel.load(Ordering::Acquire) => {
                    self.files.candidate = Some((*document, job.path, bytes));
                    if self.entries.is_empty() && self.capture_handle.is_none() {
                        self.close_capture();
                        self.install_candidate();
                    }
                }
                Ok(Completed::Loaded(..)) => {
                    self.files.notice = Some("已取消打开；当前日志保持不变。".into());
                }
                Ok(Completed::Saved(summary)) => {
                    self.files.notice = Some(format!(
                        "已保存 {} 条记录（{} 字节）：{}",
                        summary.records,
                        summary.bytes,
                        job.path.display()
                    ));
                    if job.action == Action::SaveBeforeOpen {
                        if job.cancel.load(Ordering::Acquire) {
                            self.files.candidate = None;
                            self.files.replace = None;
                            self.files
                                .notice
                                .as_mut()
                                .unwrap()
                                .push_str("；已取消打开，原视图保持不变，采集已停止。");
                        } else {
                            self.install_candidate();
                        }
                    }
                }
                Err(error) => {
                    self.file_operation_failed(
                        job.action,
                        job.path,
                        error,
                        job.cancel.load(Ordering::Acquire),
                    );
                }
            }
        }
        if let Some(replace) = self.files.replace {
            if self.capture_handle.is_none() {
                self.files.replace = None;
                match replace {
                    Replace::Discard => self.install_candidate(),
                    Replace::Save => self.file_dialog(Action::SaveBeforeOpen),
                }
            } else if matches!(self.capture_state, CaptureState::Error(_)) {
                self.files.replace = None;
                self.files.candidate = None;
                self.files.notice =
                    Some("无法确认旧采集停止，未替换当前日志。请先处理采集错误。".into());
            }
        }
        if self.files.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn install_candidate(&mut self) {
        if self.capture_handle.is_some() {
            return;
        }
        let Some((document, path, bytes)) = self.files.candidate.take() else {
            return;
        };
        if bytes > memory_budget_bytes(self.memory_limit_mib) {
            self.files.replace = None;
            self.files.notice = Some("候选文件超出当前预算，未替换现有日志。".into());
            return;
        }
        self.clear_logs();
        self.entries = Arc::clone(&document.entries);
        self.unresolved_count = self.entries.unresolved_count();
        self.retained_device = None;
        self.live_session = Session::new();
        self.capture_facts = ciallogcat::capture_facts::CaptureFacts::default();
        self.snapshot_budget = None;
        self.reset_scope_budget = true;
        self.process_watch = process_watch::Watcher::default();
        self.files.document = Some(Arc::new(document));
        self.files.path = Some(path);
        self.files.document_bytes = bytes;
        self.files.replace = None;
        self.follow_logs = false;
        self.package.clear();
        self.mark_config_dirty();
        self.scroll_to_bottom = false;
        self.restore_scroll_offset = Some(0.0);
        self.schedule_filter();
    }

    pub(super) fn file_notice(&mut self, root_ui: &mut egui::Ui) {
        if self.files.document.is_none() && self.files.job.is_none() && self.files.notice.is_none()
        {
            return;
        }
        let mut retry = false;
        let mut dismiss = false;
        egui::Panel::top("file_notice").show(root_ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if let Some(document) = &self.files.document {
                    let completeness = if document.end.capture.completeness == "partial" {
                        "已知不完整"
                    } else {
                        "未知"
                    };
                    ui.label(format!(
                        "文件 · {} · {} 条 · 采集完整性：{completeness}",
                        self.files
                            .path
                            .as_ref()
                            .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                            .unwrap_or_default(),
                        document.entries.len()
                    ));
                }
                if let Some(job) = &self.files.job {
                    ui.spinner();
                    ui.label(if job.action == Action::Open {
                        "正在验证并打开…"
                    } else {
                        "正在保存固定快照…"
                    });
                    if ui.button("取消操作").clicked() {
                        job.cancel.store(true, Ordering::Release);
                    }
                }
                if let Some(notice) = &self.files.notice {
                    // One bounded row: long paths must not wrap the actions into
                    // separate rows or take reading space away from the log table.
                    ui.horizontal(|ui| {
                        if let Some((action, _)) = &self.files.retry {
                            let has_source = self.files.document.is_some() || self.retained_device.is_some();
                            let filter_ready = *action != Action::Export
                                || (!self.filter_pending && self.filter_dirty_since.is_none());
                            retry = ui.add_enabled(
                                !self.files.busy() && filter_ready && (*action == Action::Open || has_source),
                                egui::Button::new("修改路径重试…"),
                            ).on_hover_text("保留失败路径供修改；再次确认后使用当前视图，不会自动重试或覆盖已有文件。").clicked();
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            dismiss = ui.small_button("×").on_hover_text("关闭文件通知").clicked();
                            Self::status_diagnostic(ui, "file_notice_diagnostic", "详情", notice);
                            ui.add(egui::Label::new(notice).truncate()).on_hover_text(notice);
                        });
                    });
                }
            });
        });
        if dismiss {
            self.files.notice = None;
            self.files.retry = None;
        } else if retry && let Some((action, _)) = self.files.retry {
            self.file_dialog(action);
        }
    }

    pub(super) fn file_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(mut dialog) = self.files.dialog.take() {
            let mut submit = false;
            let mut cancel =
                ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            let modal = egui::Modal::new(egui::Id::new("log_file_path")).show(ctx, |ui| {
                ui.set_width(540.0);
                ui.heading(match dialog.action {
                    Action::Open => "打开日志文件",
                    Action::Export => "导出当前筛选结果",
                    _ => "另存保留日志",
                });
                ui.label(if dialog.action == Action::Open {
                    "输入本地 原生日志 JSONL 文件路径，也可取消后将文件拖入窗口。"
                } else {
                    "输入新文件路径。保存固定快照，不包含后续到达的日志；已有文件不会被覆盖。"
                });
                if dialog.action != Action::Open {
                    let retained = self.entries.len();
                    if dialog.action == Action::Export {
                        ui.label(format!(
                            "当前匹配 {} / 保留 {retained} 条；导出保留筛选条件及采集事实。",
                            self.matches.len()
                        ));
                    } else {
                        ui.label(format!("当前保留 {retained} 条；包括未匹配筛选的记录。"));
                    }
                    ui.label("记录范围以点击保存时为准；已淘汰或清空的历史不会恢复。");
                    let first = (!self.entries.is_empty())
                        .then(|| self.entries[0].provenance)
                        .flatten()
                        .map(|id| id.seq)
                        .or_else(|| {
                            self.files
                                .document
                                .as_ref()?
                                .record_metadata
                                .first()
                                .map(|record| record.seq)
                        });
                    let last = self
                        .entries
                        .len()
                        .checked_sub(1)
                        .and_then(|index| self.entries[index].provenance)
                        .map(|id| id.seq)
                        .or_else(|| {
                            self.files
                                .document
                                .as_ref()?
                                .record_metadata
                                .last()
                                .map(|record| record.seq)
                        });
                    if let (Some(first), Some(last)) = (first, last) {
                        ui.label(format!(
                            "来源保留范围：seq {first}–{last}；范围内可能有筛选或历史缺口。"
                        ));
                    }
                }
                let edit = ui
                    .add(egui::TextEdit::singleline(&mut dialog.path).desired_width(f32::INFINITY));
                if !dialog.focused {
                    edit.request_focus();
                    dialog.focused = true;
                }
                let enter = edit.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    cancel |= ui.button("取消").clicked();
                    submit = ui
                        .add_enabled(
                            !dialog.path.trim().is_empty(),
                            egui::Button::new(if dialog.action == Action::Open {
                                "打开并验证"
                            } else {
                                "保存"
                            }),
                        )
                        .clicked()
                        || (enter && !dialog.path.trim().is_empty());
                });
            });
            if cancel || modal.should_close() {
                if dialog.action == Action::SaveBeforeOpen {
                    self.files.candidate = None;
                    self.files.replace = None;
                }
            } else if submit {
                self.start_file_job(dialog.action, PathBuf::from(dialog.path.trim()), ctx);
            } else {
                self.files.dialog = Some(dialog);
            }
            return;
        }
        if let Some(job) = &self.files.job
            && job.action == Action::SaveBeforeOpen
        {
            egui::Modal::new(egui::Id::new("save_before_open_progress")).show(ctx, |ui| {
                ui.heading("正在保存当前日志…");
                ui.spinner();
                ui.label("保存成功后打开文件。当前采集已停止。");
                if ui.button("取消保存与打开").clicked() {
                    job.cancel.store(true, Ordering::Release);
                }
            });
            return;
        }
        if let Some((document, path, _)) = &self.files.candidate {
            let mut choice = None;
            let mut cancel = false;
            let modal = egui::Modal::new(egui::Id::new("replace_log_document")).show(ctx, |ui| {
                ui.set_width(540.0);
                ui.heading("打开文件并替换当前视图？");
                ui.label(format!("{} 已验证，共 {} 条记录。", path.display(), document.entries.len()));
                if self.files.replace.is_some() { ui.spinner(); ui.label("等待当前采集停止并接收剩余日志…"); }
                else {
                    ui.label("替换后当前内存日志将被移除。可以先停止采集并保存，再打开文件；保存失败会保留原视图。");
                    ui.horizontal(|ui| {
                        cancel = ui.button("取消").clicked();
                        if ui.button("保存后打开…").clicked() { choice = Some(Replace::Save); }
                        if ui.button("直接替换").clicked() { choice = Some(Replace::Discard); }
                    });
                }
            });
            if self.files.replace.is_none() {
                if cancel || modal.should_close() {
                    self.files.candidate = None;
                } else if let Some(choice) = choice {
                    self.files.replace = Some(choice);
                    self.close_capture();
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "file_ui_tests.rs"]
mod tests;
