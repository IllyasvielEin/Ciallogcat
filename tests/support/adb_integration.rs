//! Linux subprocess integration coverage, included by `adb` only for tests.
//!
//! Every scenario runs in a separate test process: environment overrides never
//! race other tests, and a stuck backend cannot hang the outer test indefinitely.
use super::*;
use std::fs;
use std::os::unix::fs::symlink;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WORKER: &str = "adb::integration_tests::scenario_worker";
const WAIT: Duration = Duration::from_secs(5);

struct Sandbox {
    root: PathBuf,
    adb: PathBuf,
    trace: PathBuf,
}

impl Sandbox {
    fn new(scenario: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/adb-integration")
            .join(format!("{scenario}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        // A space in the executable path also checks correct argument handling.
        let adb = root.join("fake adb");
        // Use the immutable, executable fixture through an isolated path with
        // spaces. Writing a new executable while sibling tests spawn workers
        // can transiently fail exec with ETXTBSY on Linux.
        symlink(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_adb.py"),
            &adb,
        )
        .unwrap();
        Self {
            trace: root.join("trace.jsonl"),
            root,
            adb,
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Failure cleanup only targets this sandbox's still-running Python
        // processes, never arbitrary PIDs or the system's actual adb server.
        for record in trace_at(&self.trace) {
            if let Some(pid) = record["pid"].as_u64() {
                kill_owned(pid, &self.adb);
            }
        }
        // Leave traces and worker logs under ignored target for diagnosis.
    }
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn kill_owned(pid: u64, adb: &Path) {
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return;
    };
    if cmdline
        .split(|byte| *byte == 0)
        .any(|arg| arg == adb.as_os_str().as_encoded_bytes())
    {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
}

fn run_scenario(scenario: &str) {
    let sandbox = Sandbox::new(scenario);
    let log_path = sandbox.root.join("worker.log");
    let log = fs::File::create(&log_path).unwrap();
    let mut child = Worker(
        Command::new(env::current_exe().unwrap())
            .args(["--exact", WORKER, "--ignored", "--nocapture"])
            .env("CIALLOGCAT_TEST_SCENARIO", scenario)
            .env(
                "CIALLOGCAT_FAKE_SCENARIO",
                if matches!(scenario, "burst_stop" | "burst_held_stop") {
                    "burst"
                } else {
                    scenario
                },
            )
            .env("CIALLOGCAT_FAKE_TRACE", &sandbox.trace)
            .env("CIALLOGCAT_ADB", &sandbox.adb)
            .env("CIALLOGCAT_CONFIG_DIR", sandbox.root.join("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "scenario {scenario} timed out; see {}",
            log_path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.success(),
        "scenario {scenario} failed:\n{}",
        fs::read_to_string(&log_path).unwrap()
    );
    assert!(
        !trace_at(&sandbox.trace).is_empty(),
        "fixture was never invoked"
    );
}

macro_rules! scenarios {
    ($($name:ident => $scenario:literal),+ $(,)?) => {$ (
        #[test]
        fn $name() { run_scenario($scenario); }
    )+};
}

scenarios! {
    device_packages_and_processes => "normal",
    empty_device_list => "no_devices",
    unavailable_device_states => "device_states",
    multiple_device_transports => "multi_device",
    process_query_falls_back_on_error => "ps_fallback",
    process_query_falls_back_on_empty => "ps_empty",
    query_failures_are_reported => "query_error",
    hung_query_times_out_and_reaps_child => "query_hang",
    unicode_partial_invalid_and_oversized_stream => "stream_edges",
    stderr_and_nonzero_exit_are_reported => "stderr_exit",
    closed_stdout_reaps_still_running_child => "eof_open",
    repeated_pause_restart_and_drop_cleanup => "continuous",
    bounded_burst_resumes_after_byte_backpressure => "burst",
    stop_while_channel_is_saturated => "burst_stop",
    stop_reaps_before_consuming_held_entries => "burst_held_stop",
    exit_follows_stderr_tail_and_all_queued_events => "stderr_tail",
    silent_stream_tail_wakes_without_periodic_channel_poll => "once_then_idle",
}

fn wake_driven_idle_stream(adb: &Path) {
    let (tx, rx) = mpsc::sync_channel(16);
    let (wake_tx, wake_rx) = mpsc::channel();
    let wake = WakeSignal::new(move || {
        let _ = wake_tx.send(());
    });
    let mut handle = start_logcat(adb, "TEST-USB", &[LogBuffer::Main], 71, tx, wake).unwrap();
    let mut capture = Capture::default();
    let deadline = Instant::now() + WAIT;
    while capture.entries.is_empty() {
        // No periodic event-channel receive: a quiet final record must cause
        // a callback even if Started's callback preceded its arrival.
        wake_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("silent logcat tail did not wake the consumer");
        for event in rx.try_iter() {
            capture.push(event, 71);
        }
    }
    assert_eq!(capture.entries[0].message.as_ref(), "before idle");
    assert!(capture.exits.is_empty());
    handle.stop();
    capture.finish(&rx, 71);
}

fn trace_at(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn trace() -> Vec<serde_json::Value> {
    trace_at(Path::new(&env::var_os("CIALLOGCAT_FAKE_TRACE").unwrap()))
}

fn commands() -> Vec<Vec<String>> {
    trace()
        .into_iter()
        .filter(|record| record["event"] == "invocation")
        .map(|record| serde_json::from_value(record["args"].clone()).unwrap())
        .collect()
}

fn receive(rx: &Receiver<BackendEvent>) -> BackendEvent {
    rx.recv_timeout(WAIT)
        .unwrap_or_else(|error| panic!("backend event missing: {error}"))
}

fn queries(adb: &Path, scenario: &str) {
    let (tx, rx) = mpsc::sync_channel(16);
    spawn_device_query(adb.to_owned(), tx.clone(), WakeSignal::default());
    let BackendEvent::Devices(devices) = receive(&rx) else {
        panic!("wrong device event")
    };
    if scenario == "query_error" {
        assert!(devices.unwrap_err().contains("query failed"));
    } else {
        let devices = devices.unwrap();
        match scenario {
            "no_devices" => {
                assert!(devices.is_empty());
                return;
            }
            "device_states" => {
                assert_eq!(devices.len(), 3);
                assert!(devices[0].available());
                assert_eq!(devices[1].state, "unauthorized");
                assert_eq!(devices[2].state, "offline");
                assert!(!devices[1].available());
                assert!(!devices[2].available());
                return;
            }
            "multi_device" => {
                assert_eq!(devices.len(), 3);
                assert_eq!(
                    devices.iter().map(|d| d.transport).collect::<Vec<_>>(),
                    ["USB", "Wi-Fi", "Emulator"]
                );
                assert!(devices.iter().all(DeviceInfo::available));
                // Explicit selection must propagate the non-first serial.
                spawn_package_query(
                    adb.to_owned(),
                    "192.0.2.1:5555".into(),
                    tx.clone(),
                    WakeSignal::default(),
                );
                let BackendEvent::Packages { serial, result } = receive(&rx) else {
                    panic!("wrong package event")
                };
                assert_eq!(serial, "192.0.2.1:5555");
                assert!(!result.unwrap().is_empty());
                assert_eq!(&commands()[1][..2], ["-s", "192.0.2.1:5555"]);
                return;
            }
            _ => {
                assert_eq!(devices.len(), 1);
                assert_eq!(devices[0].serial, "TEST-USB");
                assert_eq!(devices[0].display_name(), "Test Phone · USB");
            }
        }
    }
    spawn_package_query(
        adb.to_owned(),
        "TEST-USB".into(),
        tx.clone(),
        WakeSignal::default(),
    );
    let BackendEvent::Packages { serial, result } = receive(&rx) else {
        panic!("wrong package event")
    };
    assert_eq!(serial, "TEST-USB");
    if scenario == "query_error" {
        assert!(result.unwrap_err().contains("query failed"));
    } else {
        assert_eq!(result.unwrap(), ["com.example.alpha", "com.example.zeta"]);
    }
    spawn_process_query(
        adb.to_owned(),
        "TEST-USB".into(),
        7,
        tx,
        WakeSignal::default(),
    );
    let BackendEvent::Processes {
        serial,
        session_id,
        observed_from,
        observed_until,
        result,
    } = receive(&rx)
    else {
        panic!("wrong process event")
    };
    assert_eq!(session_id, 7);
    assert!(observed_until >= observed_from);
    assert!(Instant::now() >= observed_until);
    assert_eq!(serial, "TEST-USB");
    if scenario == "query_error" {
        assert!(result.unwrap_err().contains("query failed"));
    } else {
        let processes = result.unwrap();
        assert_eq!(
            processes.get(&1234).map(String::as_str),
            Some("com.example.alpha")
        );
        assert_eq!(
            processes.get(&2222).map(String::as_str),
            Some("com.example.zeta")
        );
    }
    let commands = commands();
    assert_eq!(commands[0], ["devices", "-l"]);
    assert_eq!(
        commands[1],
        ["-s", "TEST-USB", "shell", "pm", "list", "packages", "-3"]
    );
    assert_eq!(
        commands[2],
        ["-s", "TEST-USB", "shell", "ps", "-A", "-o", "PID,NAME"]
    );
    if matches!(scenario, "ps_fallback" | "ps_empty" | "query_error") {
        assert_eq!(commands[3], ["-s", "TEST-USB", "shell", "ps", "-A"]);
        assert_eq!(commands.len(), 4);
    } else {
        assert_eq!(commands.len(), 3);
    }
}

#[derive(Default)]
struct Capture {
    entries: Vec<LogEntry>,
    stderr: Vec<String>,
    exits: Vec<(Option<i32>, Option<String>)>,
    started: usize,
}

impl Capture {
    fn push(&mut self, event: BackendEvent, session: u64) {
        let BackendEvent::Logcat { session_id, event } = event else {
            panic!("non-logcat event")
        };
        assert_eq!(
            session_id, session,
            "events must retain their session identity"
        );
        assert!(self.exits.is_empty(), "event arrived after terminal Exited");
        match event {
            LogcatEvent::Started => self.started += 1,
            LogcatEvent::Entry(entry) => self.entries.push(entry.into_inner()),
            LogcatEvent::Stderr(line) => self.stderr.push(line),
            LogcatEvent::Exited { code, error, .. } => self.exits.push((code, error)),
        }
    }

    fn finish(&mut self, rx: &Receiver<BackendEvent>, session: u64) {
        let deadline = Instant::now() + WAIT;
        loop {
            let timeout = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(timeout) {
                Ok(event) => self.push(event, session),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(error) => panic!("capture did not finish: {error}"),
            }
        }
        assert_eq!(self.started, 1);
        assert_eq!(self.exits.len(), 1);
    }
}

fn stream(adb: &Path, scenario: &str) {
    let (tx, rx) = mpsc::sync_channel(4096);
    let mut handle = start_logcat(
        adb,
        "TEST-USB",
        &[LogBuffer::Crash, LogBuffer::System],
        41,
        tx,
        WakeSignal::default(),
    )
    .unwrap();
    let mut capture = Capture::default();
    capture.finish(&rx, 41);
    handle.stop();
    handle.stop();
    assert_eq!(
        commands()[0],
        [
            "-s",
            "TEST-USB",
            "logcat",
            "-v",
            "threadtime",
            "-T",
            "1",
            "-b",
            "system",
            "-b",
            "crash"
        ]
    );
    match scenario {
        "stream_edges" => {
            assert_eq!(capture.entries.len(), 6);
            assert!(capture.entries[0].message.starts_with("---------"));
            assert_eq!(capture.entries[1].message.as_ref(), "unicode café 雪 🚀");
            assert_eq!(capture.entries[1].pid, 1234);
            assert_eq!(capture.entries[1].tid, 1235);
            assert!(capture.entries[2].message.contains('\u{FFFD}'));
            assert_eq!(
                capture.entries[2].text_loss,
                Some(TextLoss {
                    truncated: false,
                    invalid_utf8_replaced: true,
                })
            );
            assert!(capture.entries[3].message.contains("truncated:"));
            assert!(capture.entries[3].message.len() <= MAX_LINE_BYTES + 100);
            assert_eq!(
                capture.entries[3].text_loss,
                Some(TextLoss {
                    truncated: true,
                    invalid_utf8_replaced: false,
                })
            );
            assert_eq!(capture.entries[4].message.as_ref(), "after oversized");
            assert_eq!(capture.entries[5].message.as_ref(), "final partial");
            for index in [0, 1, 4, 5] {
                assert_eq!(capture.entries[index].text_loss, Some(TextLoss::default()));
            }
        }
        "stderr_exit" => {
            assert_eq!(capture.stderr.len(), 3);
            assert!(
                capture
                    .stderr
                    .iter()
                    .all(|line| line.len() <= MAX_STDERR_EVENT_BYTES)
            );
            assert!(capture.stderr[0].ends_with(STDERR_TRUNCATION_NOTICE));
            assert!(capture.stderr[1].contains('\u{FFFD}'));
            assert!(capture.stderr[1].ends_with(STDERR_TRUNCATION_NOTICE));
            assert!(
                capture
                    .stderr
                    .iter()
                    .any(|line| line == "fake adb: logcat failed")
            );
            assert_eq!(capture.exits[0].0, Some(17));
            assert!(capture.exits[0].1.is_none());
        }
        "eof_open" => {
            assert!(
                capture.exits[0]
                    .1
                    .as_deref()
                    .is_some_and(|error| error.contains("ended unexpectedly"))
            );
        }
        _ => unreachable!(),
    }
}

fn repeated_capture(adb: &Path) {
    for session in 1..=6 {
        let (tx, rx) = mpsc::sync_channel(4);
        let buffers = if session % 2 == 0 {
            vec![LogBuffer::All, LogBuffer::Main]
        } else {
            vec![]
        };
        let mut handle = start_logcat(
            adb,
            "TEST-USB",
            &buffers,
            session,
            tx,
            WakeSignal::default(),
        )
        .unwrap();
        let mut capture = Capture::default();
        while capture.entries.len() < 3 {
            capture.push(receive(&rx), session);
        }
        let started = Instant::now();
        if session % 2 == 0 {
            drop(handle);
        } else {
            handle.stop();
            handle.stop();
            drop(handle);
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        capture.finish(&rx, session);
        wait_for_reaped_children();
    }
    let commands = commands();
    assert_eq!(commands.len(), 6);
    for (index, args) in commands.iter().enumerate() {
        assert_eq!(
            &args[..7],
            ["-s", "TEST-USB", "logcat", "-v", "threadtime", "-T", "1"]
        );
        if index % 2 == 0 {
            assert_eq!(&args[7..], ["-b", "main", "-b", "system", "-b", "crash"]);
        } else {
            assert_eq!(&args[7..], ["-b", "all"]);
        }
    }
}

enum BurstMode {
    Complete,
    StopQueued,
    StopHeld,
}

fn burst(adb: &Path, mode: BurstMode) {
    let (tx, rx) = mpsc::sync_channel(if matches!(mode, BurstMode::StopQueued) {
        1
    } else {
        4096
    });
    let mut handle = start_logcat(adb, "TEST-USB", &[], 90, tx, WakeSignal::default()).unwrap();
    if matches!(mode, BurstMode::StopQueued) {
        // Started or stderr fills the channel; a reader then blocks trying to
        // send. Stopping must remain prompt, even without a draining UI.
        let deadline = Instant::now() + WAIT;
        while commands().is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        handle.stop();
        assert!(started.elapsed() < Duration::from_secs(1));
        // Keep the channel saturated: child reaping must be independent of
        // consumer progress, while Exited still waits for the final drain.
        wait_for_reaped_children();
        Capture::default().finish(&rx, 90);
        return;
    }
    let mut held = Vec::new();
    let mut bytes = 0;
    let mut capture = Capture::default();
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(
            Instant::now() < deadline,
            "producer never reached its byte budget: {bytes} bytes / {} entries",
            held.len()
        );
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(BackendEvent::Logcat {
                session_id: 90,
                event: LogcatEvent::Entry(entry),
            }) => {
                bytes += entry.entry.payload_bytes() + std::mem::size_of::<LogEntry>();
                held.push(entry);
            }
            Ok(event) => capture.push(event, 90),
            Err(RecvTimeoutError::Timeout) if bytes > QUEUE_BYTES - 512 * 1024 => break,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(error) => panic!("producer ended before applying backpressure: {error}"),
        }
    }
    assert!(bytes <= QUEUE_BYTES);
    assert!(held.len() < 128);
    assert!(
        !trace()
            .iter()
            .any(|record| record["event"] == "burst_complete")
    );
    let held_count = held.len();
    for (index, entry) in held.iter().enumerate() {
        assert!(
            entry
                .entry
                .message
                .starts_with(&format!("--------- burst-{index:04} "))
        );
    }
    if matches!(mode, BurstMode::StopHeld) {
        handle.stop();
        wait_for_reaped_children();
        // The consumer owns nearly 16 MiB of permits. Reaping is already
        // complete; releasing them now permits lossless reader drain and ACK.
        drop(held);
        capture.finish(&rx, 90);
        return;
    }
    drop(held); // Releasing permits must let the producer finish, without loss.
    capture.finish(&rx, 90);
    assert_eq!(held_count + capture.entries.len(), 128);
    for (offset, entry) in capture.entries.iter().enumerate() {
        let index = held_count + offset;
        assert!(
            entry
                .message
                .starts_with(&format!("--------- burst-{index:04} "))
        );
    }
    assert!(
        trace()
            .iter()
            .any(|record| record["event"] == "burst_complete")
    );
    handle.stop();
}

fn stderr_tail(adb: &Path) {
    let (tx, rx) = mpsc::sync_channel(1);
    let _handle = start_logcat(adb, "TEST-USB", &[], 91, tx, WakeSignal::default()).unwrap();
    let deadline = Instant::now() + WAIT;
    while !trace().iter().any(|row| row["event"] == "stderr_complete") {
        assert!(Instant::now() < deadline, "fixture never finished stderr");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Both readers are blocked by a one-event channel. A finished child must
    // still be reaped before the consumer starts draining either stream.
    wait_for_reaped_children();
    let mut capture = Capture::default();
    capture.finish(&rx, 91);
    assert_eq!(capture.entries.len(), 1);
    assert_eq!(
        capture.stderr,
        (0..128)
            .map(|index| format!("tail-{index:03}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(capture.exits, [(Some(17), None)]);
}

fn hung_query(adb: &Path) {
    let (tx, rx) = mpsc::sync_channel(1);
    spawn_device_query(adb.to_owned(), tx, WakeSignal::default());
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(200)),
        Err(RecvTimeoutError::Timeout)
    ));
    let BackendEvent::Devices(result) = rx
        .recv_timeout(Duration::from_secs(6))
        .expect("production timeout must precede fixture watchdog")
    else {
        panic!("wrong query event")
    };
    assert!(result.unwrap_err().contains("timed out after 5 seconds"));
}

fn wait_for_reaped_children() {
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining: Vec<_> = trace()
            .iter()
            .filter_map(|record| record["pid"].as_u64())
            .filter(|pid| Path::new(&format!("/proc/{pid}")).exists())
            .collect();
        if remaining.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fake adb processes were not reaped: {remaining:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "internal worker; outer scenario tests provide isolated environment and a deadline"]
fn scenario_worker() {
    let scenario =
        env::var("CIALLOGCAT_TEST_SCENARIO").expect("run through outer integration tests");
    let expected = PathBuf::from(env::var_os("CIALLOGCAT_ADB").unwrap());
    let adb = find_adb().expect("fixture must be discoverable through CIALLOGCAT_ADB");
    assert_eq!(adb, expected);
    match scenario.as_str() {
        "stream_edges" | "eof_open" => stream(&adb, &scenario),
        "stderr_exit" => {
            stream(&adb, &scenario);
            burst(&adb, BurstMode::StopQueued); // Also saturate stderr.
        }
        "continuous" => repeated_capture(&adb),
        "burst" => burst(&adb, BurstMode::Complete),
        "burst_stop" => burst(&adb, BurstMode::StopQueued),
        "burst_held_stop" => burst(&adb, BurstMode::StopHeld),
        "stderr_tail" => stderr_tail(&adb),
        "once_then_idle" => wake_driven_idle_stream(&adb),
        "query_hang" => hung_query(&adb),
        _ => queries(&adb, &scenario),
    }
    wait_for_reaped_children();
}
