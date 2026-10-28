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

fn probe_label(probe: Probe) -> &'static str {
    match probe {
        Probe::AdbFeatures => "ADB 能力",
        Probe::LogcatHelp => "设备 logcat 选项",
        Probe::BufferSizes => "设备 buffer 容量",
        Probe::Statistics => "设备日志统计",
        Probe::PruneRules => "设备裁剪规则",
        Probe::AndroidSdk => "Android API",
        Probe::AndroidRelease => "Android 版本",
        Probe::BuildFingerprint => "系统构建标识",
    }
}

fn status_label(status: ProbeStatus) -> &'static str {
    match status {
        ProbeStatus::Success => "成功",
        ProbeStatus::PermissionDenied => "无权限",
        ProbeStatus::Unsupported => "不支持",
        ProbeStatus::TimedOut => "超时",
        ProbeStatus::Cancelled => "已取消",
        ProbeStatus::Unknown => "未知 / 未成功",
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
            self.diagnostics.error = Some("请选择设备并配置 ADB 后查询。已有报告仍可查看。".into());
            return;
        };
        let buffers = self.buffers.clone();
        let cancellation = CancellationToken::default();
        let worker_cancel = cancellation.clone();
        let worker_serial = serial.clone();
        let wake = wake_signal(ctx);
        let (sender, receiver) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("device-diagnostics".into())
            .spawn(move || {
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
            Err(error) => self.diagnostics.error = Some(format!("无法启动诊断：{error}")),
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
                Err(mpsc::TryRecvError::Disconnected) => Some(Err("诊断工作线程意外结束".into())),
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
        egui::Window::new("只读设备诊断").id(egui::Id::new("device_diagnostics"))
            .open(&mut open).default_width(660.0).resizable(true).show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    refresh = ui.add_enabled(self.diagnostics.job.is_none() && self.selected_device.is_some(), egui::Button::new("重新查询")).clicked();
                    if let Some(job) = &self.diagnostics.job {
                        ui.spinner();
                        ui.label(format!("查询 {} · 总期限 10 秒", job.serial));
                        if ui.button("取消查询").clicked() { job.cancellation.cancel(); }
                    }
                    if let Some(report) = &self.diagnostics.report
                        && ui.button("复制诊断报告").clicked() {
                        let text = serde_json::to_string_pretty(report.as_ref()).expect("diagnostic JSON");
                        self.cancel_copy();
                        ctx.copy_text(text);
                    }
                });
                ui.label(format!("本机留存预算 {} MiB，不含队列、快照、剪贴板或总 RSS；设备 logd 容量见下方。", self.memory_limit_mib));
                if let Some(error) = &self.diagnostics.error { ui.label(error); }
                if let Some(report) = &self.diagnostics.report {
                    ui.label(format!("报告来源：{} · 观察时间 {} · {} ms", report.serial,
                        report.finished_unix_ms.map_or_else(|| "未知".into(), |time| time.to_string()), report.elapsed_ms));
                    if self.selected_device.as_deref() != Some(report.serial.as_str()) { ui.label("这是另一设备的历史报告。切换设备后请重新查询。"); }
                    ui.label(if report.complete { "各项查询已完成。" } else { "部分查询未完成或输出受限；下方逐项保留结果。" });
                    ui.label("输出分别保留主机 stdout / stderr；旧 ADB 通道可能已在设备端合并。帮助中未列出的选项仍可能被设备接受。");
                    egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                        for observation in &report.observations {
                            egui::CollapsingHeader::new(format!("{} · {}", probe_label(observation.probe), status_label(observation.status)))
                                .id_salt(probe_label(observation.probe)).default_open(matches!(observation.probe, Probe::BufferSizes)).show(ui, |ui| {
                                    if let Some(error) = &observation.error { ui.label(error); }
                                    for (label,output) in [("stdout",&observation.stdout),("stderr",&observation.stderr)] {
                                        if !output.text.is_empty() || output.truncated || output.read_error.is_some() {
                                            ui.label(format!("{label} · 保留 {} / 观察到 {} 字节{}{}",output.retained_bytes,output.observed_bytes,
                                                if output.truncated { " · 已截断" } else { "" }, if output.lossy_utf8 { " · 显示发生 UTF-8 替换" } else { "" }));
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
