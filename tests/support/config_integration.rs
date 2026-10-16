//! GUI configuration I/O runs in a subprocess to isolate environment overrides.
use super::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WORKER: &str = "config::integration_tests::config_worker";

struct Worker(std::process::Child);

impl Drop for Worker {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn explicit_config_directory_round_trip_is_isolated() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/config-integration")
        .join(format!("{}-{unique}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let log_path = root.join("worker.log");
    let log = fs::File::create(&log_path).unwrap();
    let mut worker = Worker(
        Command::new(env::current_exe().unwrap())
            .args(["--exact", WORKER, "--ignored", "--nocapture"])
            .env("CIALLOGCAT_CONFIG_DIR", root.join("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = worker.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "{}",
                fs::read_to_string(log_path).unwrap()
            );
            break;
        }
        assert!(Instant::now() < deadline, "config worker timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(root.join("config/config.json").is_file());
}

#[test]
#[ignore = "internal worker; outer test provides an isolated environment and deadline"]
fn config_worker() {
    let config_dir = PathBuf::from(env::var_os("CIALLOGCAT_CONFIG_DIR").unwrap());
    assert_eq!(config_path().unwrap(), config_dir.join("config.json"));
    assert!(!config_dir.exists());
    let config = AppConfig {
        query: "isolated config café 雪".into(),
        ..AppConfig::default()
    };
    config.save().unwrap();
    assert_eq!(AppConfig::load().query, config.query);
}
