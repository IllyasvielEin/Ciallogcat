//! Small desktop history controls and budgets for finite snapshots.
use super::*;
use ciallogcat::capture_scope::{CaptureScope, HistoryStart};

pub(super) struct ScopeDraft {
    mode: usize,
    count: String,
    time: String,
}

impl From<&CaptureScope> for ScopeDraft {
    fn from(scope: &CaptureScope) -> Self {
        let mut draft = Self {
            mode: 0,
            count: "1".into(),
            time: String::new(),
        };
        let start = match scope {
            CaptureScope::Follow { start } => Some(start),
            CaptureScope::Dump { start } => {
                draft.mode = 2;
                start.as_ref()
            }
        };
        match start {
            Some(HistoryStart::Tail(count)) => draft.count = u32::from(*count).to_string(),
            Some(HistoryStart::Since(time)) => {
                draft.mode += 1;
                draft.time = time.as_str().into();
            }
            None => draft.mode = 4,
        }
        draft
    }
}

impl ScopeDraft {
    pub fn parse(&self) -> Result<CaptureScope, String> {
        CaptureScope::from_inputs(
            self.mode >= 2,
            matches!(self.mode, 0 | 2).then_some(self.count.trim()),
            matches!(self.mode, 1 | 3).then_some(self.time.trim()),
        )
        .map_err(|error| error.to_string())
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        const MODES: [&str; 5] = [
            "最近 N 条后跟随",
            "指定时间后跟随",
            "最近 N 条快照",
            "指定时间后快照",
            "当前 buffer 快照",
        ];
        // A submenu participates in the parent menu's open/close lifecycle.
        // A separate ComboBox popup replaces that parent and disappears with it.
        ui.menu_button(MODES[self.mode], |ui| {
            for (mode, label) in MODES.iter().enumerate() {
                ui.selectable_value(&mut self.mode, mode, *label);
            }
        });
        if matches!(self.mode, 0 | 2) {
            ui.horizontal(|ui| {
                ui.label("历史条数");
                ui.add(
                    egui::TextEdit::singleline(&mut self.count)
                        .desired_width(150.0)
                        .char_limit(10),
                );
            });
        } else if matches!(self.mode, 1 | 3) {
            ui.add(
                egui::TextEdit::singleline(&mut self.time)
                    .desired_width(310.0)
                    .char_limit(29)
                    .hint_text("2026-10-24 08:00:00.000 或 Unix 秒.小数"),
            );
            ui.label("使用设备时间；Unix 秒必须有小数点。不是精确续传游标。");
        }
        if self.mode >= 2 {
            ui.label("快照最多 15 秒 / 100,000 条；新增存储使用留存预算 90% 以内的剩余空间，上限 64 MiB。");
            ui.label("任一预算先到即停止，已有日志保留；设备历史可能已被淘汰。");
        }
        if let Err(error) = self.parse() {
            ui.colored_label(Color32::from_rgb(220, 135, 64), error);
        }
    }
}

pub(super) struct SnapshotBudget {
    pub deadline: Instant,
    pub max_bytes: usize,
    pub records: u64,
    pub bytes: usize,
    pub stop_reason: Option<&'static str>,
}

impl SnapshotBudget {
    pub fn new(now: Instant, memory_limit_mib: usize, retained_bytes: usize) -> Self {
        Self {
            deadline: now + Duration::from_secs(15),
            max_bytes: (memory_budget_bytes(memory_limit_mib) * 9 / 10)
                .saturating_sub(retained_bytes)
                .min(64 * MIB),
            records: 0,
            bytes: 0,
            stop_reason: None,
        }
    }

    pub fn check_time(&mut self, now: Instant) {
        if now >= self.deadline && self.stop_reason.is_none() {
            self.stop_reason = Some("snapshot_duration_limit");
        }
    }

    pub fn admit(&mut self, bytes: usize, now: Instant) -> bool {
        self.check_time(now);
        if self.stop_reason.is_some() {
            return false;
        }
        if self
            .bytes
            .checked_add(bytes)
            .is_none_or(|sum| sum > self.max_bytes)
        {
            self.stop_reason = Some("snapshot_byte_limit");
            return false;
        }
        self.bytes += bytes;
        self.records += 1;
        if self.records == 100_000 {
            self.stop_reason = Some("snapshot_record_limit");
        }
        true
    }
}

impl CiallogcatApp {
    pub(super) fn stop_snapshot_if_limited(&mut self) {
        if self.capture_controller.is_stopping() || self.reset_scope_budget {
            return;
        }
        if let Some(reason) = self
            .snapshot_budget
            .as_ref()
            .and_then(|budget| budget.stop_reason)
            && matches!(self.capture_controller.intent(), CaptureIntent::Capture(_))
        {
            self.capture_facts
                .stop_requested(reason, self.live_session.last_seq());
            self.set_capture_intent(CaptureIntent::Paused, false);
            let limit = match reason {
                "snapshot_duration_limit" => "15 秒期限",
                "snapshot_record_limit" => "100,000 条上限",
                _ => "本机留存空间上限",
            };
            self.files.notice = Some(format!("快照达到{limit}，正在停止并接收剩余诊断。"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_snapshot_counts_container_cost_and_never_evicts_its_accepted_prefix() {
        let ctx = egui::Context::default();
        let mut app = CiallogcatApp::with_config(
            &ctx,
            AppConfig {
                memory_limit_mib: 16,
                ..AppConfig::default()
            },
            None,
        );
        app.append_entry(LogEntry::marker("already retained"));
        let existing_bytes = app.entries.memory_bytes();
        app.capture_scope = CaptureScope::from_inputs(true, None, None).unwrap();
        let id = app.begin_test_capture();
        for _ in 0..60_000 {
            app.backend_tx
                .send(BackendEvent::Logcat {
                    session_id: id,
                    event: LogcatEvent::Entry(
                        LogEntry::new("", 1, 1, "T", Level::Info, "short").into(),
                    ),
                })
                .unwrap();
            app.poll_backend(&ctx);
        }
        assert_eq!(app.dropped_entries, 0);
        assert_eq!(app.entries[0].message.as_ref(), "already retained");
        let budget = app.snapshot_budget.as_ref().unwrap();
        assert_eq!(app.entries.len() as u64, budget.records + 1);
        assert_eq!(app.entries.memory_bytes(), existing_bytes + budget.bytes);
        assert!(budget.stop_reason.is_some());
        assert!(app.entries.memory_bytes() <= memory_budget_bytes(16) * 9 / 10);
        assert!(
            app.capture_facts
                .snapshot("stopping")
                .known_gaps
                .iter()
                .any(|gap| gap["kind"] == "host_discarded_records")
        );
    }

    #[test]
    fn historical_scopes_leave_current_pid_mapping_unassigned() {
        for scope in [
            CaptureScope::from_inputs(true, None, None).unwrap(),
            CaptureScope::from_inputs(false, Some("200"), None).unwrap(),
            CaptureScope::from_inputs(false, None, Some("1700000000.000")).unwrap(),
        ] {
            let ctx = egui::Context::default();
            let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
            app.capture_scope = scope;
            let id = app.begin_test_capture();
            app.update_processes(HashMap::from([(10, "com.newowner".into())]));
            app.backend_tx
                .send(BackendEvent::Logcat {
                    session_id: id,
                    event: LogcatEvent::Entry(
                        LogEntry::new("01-01 00:00:00.000", 10, 10, "T", Level::Info, "historical")
                            .into(),
                    ),
                })
                .unwrap();
            app.poll_backend(&ctx);
            assert!(app.entries[0].process.is_none());
            assert!(!app.entries[0].matches_package("com.newowner"));
        }
    }

    #[test]
    fn switching_to_unavailable_device_does_not_export_previous_snapshot_budget() {
        let ctx = egui::Context::default();
        let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
        app.capture_scope = CaptureScope::from_inputs(true, None, None).unwrap();
        app.begin_test_capture();
        app.snapshot_budget.as_mut().unwrap().records = 7;
        app.switch_device("not-connected".into(), &ctx);
        assert!(app.snapshot_budget.is_none());
        assert!(app.reset_scope_budget);
    }

    #[test]
    fn snapshot_budget_stops_before_oversized_record_and_on_silence() {
        let now = Instant::now();
        let mut budget = SnapshotBudget::new(now, 16, 0);
        assert!(budget.admit(budget.max_bytes, now));
        assert!(!budget.admit(1, now));
        assert_eq!(budget.records, 1);
        assert_eq!(budget.stop_reason, Some("snapshot_byte_limit"));
        let mut quiet = SnapshotBudget::new(now, 16, 0);
        quiet.check_time(now + Duration::from_secs(15));
        assert_eq!(quiet.stop_reason, Some("snapshot_duration_limit"));
        assert!(!quiet.admit(1, now + Duration::from_secs(16)));
        let mut rows = SnapshotBudget::new(now, 16, 0);
        rows.records = 99_999;
        assert!(rows.admit(1, now));
        assert!(!rows.admit(1, now));
        assert_eq!(rows.records, 100_000);
    }

    fn exit(app: &mut CiallogcatApp, ctx: &egui::Context, code: i32) {
        app.backend_tx
            .send(BackendEvent::Logcat {
                session_id: app.capture_session_id,
                event: LogcatEvent::Exited {
                    code: Some(code),
                    error: None,
                    incomplete_proto_frame: false,
                },
            })
            .unwrap();
        app.poll_backend(ctx);
    }

    #[test]
    fn successful_dump_ends_without_reconnect_and_keeps_completed_scope_fact() {
        let ctx = egui::Context::default();
        let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
        app.capture_scope = CaptureScope::from_inputs(true, Some("200"), None).unwrap();
        app.begin_test_capture();
        // The settings may change after startup; EOF belongs to the started scope.
        app.capture_scope = CaptureScope::default();
        exit(&mut app, &ctx, 0);
        assert!(matches!(
            app.capture_controller.status(),
            ControllerStatus::Paused
        ));
        let capture = app.capture_facts.snapshot("stopped");
        assert!(capture.known_gaps.is_empty());
        let observations = capture.extra["recent_observations"].as_array().unwrap();
        assert!(
            observations
                .iter()
                .any(|event| event["kind"] == "snapshot_completed")
        );
        assert_eq!(observations[0]["details"]["capture_scope"]["mode"], "dump");
        let later = app.snapshot_budget.as_ref().unwrap().deadline + Duration::from_secs(1);
        app.advance_capture(&ctx, later);
        assert_eq!(app.snapshot_budget.as_ref().unwrap().stop_reason, None);
    }

    #[test]
    fn scope_change_cannot_turn_an_unexpected_follow_exit_into_snapshot_success() {
        let ctx = egui::Context::default();
        let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
        app.begin_test_capture();
        app.capture_scope = CaptureScope::from_inputs(true, None, None).unwrap();
        exit(&mut app, &ctx, 0);
        assert!(matches!(
            app.capture_controller.status(),
            ControllerStatus::Backoff { .. }
        ));
        assert_eq!(
            app.capture_facts.snapshot("reconnecting").known_gaps[0]["kind"],
            "stream_interruptions"
        );
    }

    #[test]
    fn completed_dump_then_follow_records_an_unknown_interval_in_the_same_session() {
        let ctx = egui::Context::default();
        let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
        app.capture_scope = CaptureScope::from_inputs(true, Some("200"), None).unwrap();
        let id = app.begin_test_capture();
        let session = app.live_session.id();
        app.backend_tx
            .send(BackendEvent::Logcat {
                session_id: id,
                event: LogcatEvent::Entry(LogEntry::marker("before snapshot EOF").into()),
            })
            .unwrap();
        exit(&mut app, &ctx, 0);
        assert!(app.capture_facts.snapshot("paused").known_gaps.is_empty());
        app.capture_scope = CaptureScope::default();
        app.reset_scope_budget = true;
        let resumed = app.begin_test_capture();
        app.backend_tx
            .send(BackendEvent::Logcat {
                session_id: resumed,
                event: LogcatEvent::Entry(LogEntry::marker("after resume").into()),
            })
            .unwrap();
        app.poll_backend(&ctx);
        assert_eq!(app.live_session.id(), session);
        assert_eq!(app.entries.len(), 2);
        let capture = app.capture_facts.snapshot("running");
        assert_eq!(capture.completeness, "partial");
        assert!(capture.known_gaps.iter().any(|gap| {
            gap["kind"] == "resumed_host_pauses" && gap["count"] == 1 && gap["loss"] == "unknown"
        }));
    }

    #[test]
    fn exhausted_snapshot_retries_do_not_schedule_an_expired_deadline() {
        let ctx = egui::Context::default();
        let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
        app.capture_scope = CaptureScope::from_inputs(true, Some("200"), None).unwrap();
        let first = app.begin_test_capture();
        let initial = Instant::now();
        app.capture_controller
            .exited(first, "test failure".into(), initial);
        for offset in [1, 3, 7] {
            let tick = initial + Duration::from_secs(offset);
            let Some(CaptureCommand::Start { id, .. }) = app.capture_controller.poll(tick, true)
            else {
                panic!("expected retry");
            };
            app.capture_controller
                .exited(id, "test failure".into(), tick);
        }
        assert!(matches!(
            app.capture_controller.status(),
            ControllerStatus::Failed(_)
        ));
        let expired = app.snapshot_budget.as_ref().unwrap().deadline + Duration::from_secs(1);
        let mut last = Duration::ZERO;
        for _ in 0..5 {
            let mut output = ctx.run_ui(egui::RawInput::default(), |_ui| {
                app.advance_capture(&ctx, expired);
            });
            last = output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .repaint_delay;
            output.textures_delta.clear();
        }
        assert_ne!(last, Duration::ZERO);
        assert!(app.snapshot_budget.as_ref().unwrap().stop_reason.is_none());
    }

    #[test]
    fn silent_dump_stops_on_deadline_and_drained_budget_rejections_are_counted() {
        let ctx = egui::Context::default();
        let mut app = CiallogcatApp::with_config(&ctx, AppConfig::default(), None);
        app.capture_scope = CaptureScope::from_inputs(true, None, None).unwrap();
        let id = app.begin_test_capture();
        let now = Instant::now();
        app.snapshot_budget.as_mut().unwrap().deadline = now;
        app.advance_capture(&ctx, now);
        assert!(app.capture_controller.is_stopping());
        assert!(matches!(
            app.capture_controller.intent(),
            CaptureIntent::Paused
        ));
        app.backend_tx
            .send(BackendEvent::Logcat {
                session_id: id,
                event: LogcatEvent::Entry(LogEntry::marker("queued beyond deadline").into()),
            })
            .unwrap();
        app.poll_backend(&ctx);
        assert!(app.entries.is_empty());
        assert!(
            app.capture_facts
                .snapshot("stopping")
                .known_gaps
                .iter()
                .any(|gap| gap["kind"] == "host_discarded_records" && gap["count"] == 1)
        );
        exit(&mut app, &ctx, 0);
        assert!(matches!(
            app.capture_controller.status(),
            ControllerStatus::Paused
        ));
    }
}
