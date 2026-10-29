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

fn run_worker(mode: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/config-integration")
        .join(format!("{mode}-{}-{unique}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let log_path = root.join("worker.log");
    let log = fs::File::create(&log_path).unwrap();
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args(["--exact", WORKER, "--ignored", "--nocapture"])
        .env("CIALLOGCAT_CONFIG_TEST_MODE", mode)
        .env("XDG_CONFIG_HOME", root.join("xdg"))
        .env("HOME", root.join("home"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    if mode == "explicit" {
        command.env("CIALLOGCAT_CONFIG_DIR", root.join("config"));
    } else {
        command.env_remove("CIALLOGCAT_CONFIG_DIR");
    }
    let mut worker = Worker(command.spawn().unwrap());
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
    root
}

#[test]
fn explicit_config_directory_round_trip_is_isolated() {
    let root = run_worker("explicit");
    assert!(root.join("config/config.json").is_file());
}

#[test]
fn old_application_directory_is_not_loaded_or_modified() {
    run_worker("current-name-only");
}

#[test]
#[ignore = "internal worker; outer test provides an isolated environment and deadline"]
fn config_worker() {
    if env::var("CIALLOGCAT_CONFIG_TEST_MODE").unwrap() == "current-name-only" {
        let root = PathBuf::from(env::var_os("XDG_CONFIG_HOME").unwrap());
        let old = root.join("ciallocat/config.json");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        let original = br#"{"query":"obsolete settings must not load"}"#;
        fs::write(&old, original).unwrap();
        assert_eq!(config_path().unwrap(), root.join("ciallogcat/config.json"));
        assert!(AppConfig::load().query.is_empty());
        assert!(!root.join("ciallogcat").exists());
        let config = AppConfig {
            query: "current settings".into(),
            ..AppConfig::default()
        };
        config.save().unwrap();
        assert_eq!(AppConfig::load().query, "current settings");
        fs::write(config_path().unwrap(), b"malformed").unwrap();
        assert!(AppConfig::load().query.is_empty());
        assert_eq!(fs::read(&old).unwrap(), original);
        return;
    }
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
