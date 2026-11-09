//! On-demand, cancellable device observations; never part of the capture hot path.
use super::*;
use ciallogcat::diagnostics::{self, CancellationToken, DiagnosticReport, Probe, ProbeStatus};

struct Job {
    serial: String,
    cancellation: CancellationToken,
    receiver: Receiver<Result<DiagnosticReport, String>>,
}

#[derive(Default)]
pub(super) struct State {
    open: bool,
    job: Option<Job>,
    pub report: Option<Arc<DiagnosticReport>>,
    error: Option<String>,
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.cancellation.cancel();
        }
    }
}

fn probe_label(probe: Probe) -> String {
    match probe {
        Probe::AdbFeatures => "ADB features".to_owned(),
        Probe::LogcatHelp => tr!("logcat 选项"),
        Probe::BufferSizes => tr!("buffer 容量"),
        Probe::Statistics => tr!("logd 统计"),
        Probe::PruneRules => tr!("logd prune 规则"),
        Probe::AndroidSdk => "Android API".to_owned(),
        Probe::AndroidRelease => tr!("Android 版本"),
        Probe::BuildFingerprint => "Build fingerprint".to_owned(),
    }
}

fn status_label(status: ProbeStatus) -> String {
    match status {
        ProbeStatus::Success => tr!("成功"),
        ProbeStatus::PermissionDenied => tr!("无权限"),
        ProbeStatus::Unsupported => tr!("不支持"),
        ProbeStatus::TimedOut => tr!("超时"),
        ProbeStatus::Cancelled => tr!("已取消"),
        ProbeStatus::Unknown => tr!("未知 / 未成功"),
    }
}

impl CiallogcatApp {
    pub(super) fn open_diagnostics(&mut self, ctx: &egui::Context) {
        self.diagnostics.open = true;
        if self.diagnostics.job.is_none() {
            self.start_diagnostics(ctx);
        }
    }

    fn start_diagnostics(&mut self, ctx: &egui::Context) {
        if self.diagnostics.job.is_some() {
            return;
        }
        let (Some(adb), Some(serial)) = (self.adb_path.clone(), self.selected_device.clone())
        else {
            self.diagnostics.error = Some(tr!("请选择设备并配置 ADB 后查询。已有报告仍可查看。"));
            return;
        };
        let buffers = self.buffers.clone();
        let cancellation = CancellationToken::default();
        let worker_cancel = cancellation.clone();
        let worker_serial = serial.clone();
        let wake = wake_signal(ctx);
        let (sender, receiver) = mpsc::sync_channel(1);
        let language = crate::i18n::current();
        let spawned = std::thread::Builder::new()
            .name("device-diagnostics".into())
            .spawn(move || {
                crate::i18n::set(language);
                let result = diagnostics::collect(
                    &adb,
                    &worker_serial,
                    &buffers,
                    &diagnostics::DiagnosticOptions::default(),
                    &worker_cancel,
                );
                let _ = sender.send(result);
                wake.notify();
            });
        match spawned {
            Ok(_) => {
                self.diagnostics.error = None;
                self.diagnostics.job = Some(Job {
                    serial,
                    cancellation,
                    receiver,
                });
            }
            Err(error) => {
                self.diagnostics.error = Some(tr!("无法启动诊断：%{error}", error = error))
            }
        }
    }

    pub(super) fn poll_diagnostics(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.diagnostics.job
            && self.selected_device.as_deref() != Some(job.serial.as_str())
        {
            job.cancellation.cancel();
        }
        let result = self
            .diagnostics
            .job
            .as_ref()
            .and_then(|job| match job.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(tr!("诊断工作线程意外结束"))),
            });
        if let Some(result) = result {
            self.diagnostics.job = None;
            match result {
                Ok(report) => self.diagnostics.report = Some(Arc::new(report)),
                Err(error) => self.diagnostics.error = Some(error),
            }
        }
        if self.diagnostics.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    pub(super) fn diagnostic_context(&self) -> Option<serde_json::Value> {
        let report = self.diagnostics.report.as_ref()?;
        (self.retained_device.as_deref() == Some(report.serial.as_str()))
            .then(|| serde_json::to_value(report.as_ref()).expect("bounded diagnostic report"))
    }

    pub(super) fn diagnostics_dialog(&mut self, ctx: &egui::Context) {
        if !self.diagnostics.open {
            return;
        }
        let mut open = true;
        let mut refresh = false;
        egui::Window::new(tr!("只读设备诊断")).id(egui::Id::new("device_diagnostics"))
            .open(&mut open).default_width(660.0).resizable(true).show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    refresh = ui.add_enabled(self.diagnostics.job.is_none() && self.selected_device.is_some(), egui::Button::new(tr!("重新查询"))).clicked();
                    if let Some(job) = &self.diagnostics.job {
                        ui.spinner();
                        ui.label(tr!("查询 %{serial} · 总期限 10 秒", serial = job.serial));
                        if ui.button(tr!("取消查询")).clicked() { job.cancellation.cancel(); }
                    }
                    if let Some(report) = &self.diagnostics.report
                        && ui.button(tr!("复制诊断报告")).clicked() {
                        let text = serde_json::to_string_pretty(report.as_ref()).expect("diagnostic JSON");
                        self.cancel_copy();
                        ctx.copy_text(text);
                    }
                });
                ui.label(tr!("本机日志内存预算 %{limit} MiB，不含队列、快照、剪贴板或总 RSS；设备 logd buffer 容量见下方。", limit = self.memory_limit_mib));
                if let Some(error) = &self.diagnostics.error { ui.label(error); }
                if let Some(report) = &self.diagnostics.report {
                    ui.label(tr!("设备 %{serial} · 完成于 %{time} · 耗时 %{elapsed} ms", serial = report.serial,
                        time = report.finished_unix_ms.map_or_else(|| tr!("未知"), |time| time.to_string()), elapsed = report.elapsed_ms));
                    if self.selected_device.as_deref() != Some(report.serial.as_str()) { ui.label(tr!("这是另一设备的历史报告。切换设备后请重新查询。")); }
                    ui.label(if report.complete { tr!("各项查询已完成。") } else { tr!("部分查询未完成或输出受限；下方逐项保留结果。") });
                    ui.label(tr!("输出分别保留主机 stdout / stderr；旧 ADB 通道可能已在设备端合并。帮助中未列出的选项仍可能被设备接受。"));
                    egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                        for observation in &report.observations {
                            egui::CollapsingHeader::new(format!("{} · {}", probe_label(observation.probe), status_label(observation.status)))
                                .id_salt(probe_label(observation.probe)).default_open(matches!(observation.probe, Probe::BufferSizes)).show(ui, |ui| {
                                    if let Some(error) = &observation.error { ui.label(error); }
                                    for (label,output) in [("stdout",&observation.stdout),("stderr",&observation.stderr)] {
                                        if !output.text.is_empty() || output.truncated || output.read_error.is_some() {
                                            ui.label(tr!("%{label} · 保留 %{retained} / 实际 %{observed} 字节%{truncated}%{lossy}", label = label, retained = output.retained_bytes, observed = output.observed_bytes,
                                                truncated = if output.truncated { tr!(" · 已截断") } else { String::new() }, lossy = if output.lossy_utf8 { tr!(" · 显示发生 UTF-8 替换") } else { String::new() }));
                                            ui.add(egui::Label::new(RichText::new(&output.text).monospace()).selectable(true).wrap());
                                            if let Some(error) = &output.read_error { ui.label(error); }
                                        }
                                    }
                                });
                        }
                    });
                }
            });
        self.diagnostics.open = open;
        if !open && let Some(job) = &self.diagnostics.job {
            job.cancellation.cancel();
        }
        if refresh {
            self.start_diagnostics(ctx);
        }
    }
}
