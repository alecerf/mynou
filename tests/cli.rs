//! These tests launch only the Mynou binary itself.
use mynou::config;
use mynou::engine::Engine;
use mynou::json::{self, Value};
use mynou::server::Api;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mynou-cli-{}-{timestamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn command(directory: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(directory)
        .env_remove("MYNOU_API_TOKEN")
        .env_remove("MYNOU_DEMO_TOKEN")
        .env_remove("MYNOU_PLEX_TOKEN")
        .args(args)
        .output()
        .unwrap()
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn parse(output: Output) -> Value {
    json::parse(success(output).trim()).unwrap()
}

#[test]
fn cli_init_analyze_offline_management_and_no_overwrite() {
    let directory = Directory::new();
    let version = success(command(&directory.0, &["version"]));
    assert!(version.contains(env!("CARGO_PKG_VERSION")) && version.contains("0 dependencies"));
    success(command(&directory.0, &["init"]));
    assert!(directory.0.join("mynou.json").is_file());
    let env = fs::read(directory.0.join(".env")).unwrap();
    assert!(!command(&directory.0, &["init"]).status.success());
    assert_eq!(fs::read(directory.0.join(".env")).unwrap(), env);
    // The offline journal test must not contact a service running on the host.
    let path = directory.0.join("mynou.json");
    let mut value = json::parse(&fs::read_to_string(&path).unwrap()).unwrap();
    value.insert("listen", "127.0.0.1:0");
    fs::write(path, json::stringify(&value)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(directory.0.join(".env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(directory.0.join("mynou.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let source = directory.0.join("source.mp4");
    fs::write(&source, include_bytes!("../examples/demo.mp4")).unwrap();
    let analysis = parse(command(
        &directory.0,
        &["analyze", source.to_str().unwrap(), "--json"],
    ));
    assert_eq!(
        analysis.get("container").and_then(Value::as_str),
        Some("mp4")
    );
    assert_eq!(
        analysis.get("title").and_then(Value::as_str),
        Some("Mynou Demo!")
    );
    let submitted = parse(command(
        &directory.0,
        &[
            "submit",
            "--title",
            "CLI Movie",
            "--path",
            source.to_str().unwrap(),
        ],
    ));
    let id = submitted.as_array().unwrap()[0]
        .get("id")
        .unwrap()
        .as_str()
        .unwrap();
    let jobs = parse(command(&directory.0, &["jobs"]));
    assert_eq!(jobs.as_array().unwrap().len(), 1);
    assert_eq!(
        parse(command(&directory.0, &["show", id]))
            .get("state")
            .and_then(Value::as_str),
        Some("queued")
    );
    assert_eq!(
        parse(command(&directory.0, &["cancel", id]))
            .get("state")
            .and_then(Value::as_str),
        Some("cancelled")
    );
    assert_eq!(
        parse(command(&directory.0, &["retry", id]))
            .get("state")
            .and_then(Value::as_str),
        Some("queued")
    );
    assert!(
        parse(command(&directory.0, &["events", id]))
            .as_array()
            .unwrap()
            .len()
            >= 3
    );
    let status = parse(command(&directory.0, &["status"]));
    assert_eq!(
        status.get("service").and_then(Value::as_str),
        Some("stopped")
    );
    assert_eq!(status.get("jobs").and_then(Value::as_u64), Some(1));
    assert!(
        !command(&directory.0, &["analyze", "missing.mp4"])
            .status
            .success()
    );
    assert!(
        !command(
            &directory.0,
            &["submit", "--title", "Invalid", "--year", "-1"]
        )
        .status
        .success()
    );
    assert!(
        !command(&directory.0, &["jobs", "--unknown", "value"])
            .status
            .success()
    );
}

#[test]
fn cli_routes_to_running_api_while_store_is_locked() {
    let directory = Directory::new();
    let mut value = config::default_json();
    value.insert("listen", "127.0.0.1:0");
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads");
    };
    downloads.insert("enabled".into(), false.into());
    downloads.insert("listen_port".into(), 0_u32.into());
    downloads.insert("dht".into(), false.into());
    downloads.insert("pex".into(), false.into());
    let engine = Engine::open(config::from_json(&value, &directory.0).unwrap()).unwrap();
    let api = Api::bind(engine.clone(), TOKEN.into()).unwrap();
    value.insert("listen", api.address().unwrap().to_string());
    fs::write(directory.0.join("mynou.json"), json::stringify(&value)).unwrap();
    fs::write(
        directory.0.join(".env"),
        format!("MYNOU_API_TOKEN={TOKEN}\n"),
    )
    .unwrap();
    let server = thread::spawn(move || api.run());
    let result = std::panic::catch_unwind(|| {
        let source = directory.0.join("source.mp4");
        fs::write(&source, include_bytes!("../examples/demo.mp4")).unwrap();
        let submitted = parse(command(
            &directory.0,
            &[
                "submit",
                "--title",
                "Online",
                "--path",
                source.to_str().unwrap(),
            ],
        ));
        let id = submitted.as_array().unwrap()[0]
            .get("id")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(
            parse(command(&directory.0, &["jobs"]))
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            parse(command(&directory.0, &["status"]))
                .get("jobs")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            parse(command(&directory.0, &["cancel", id]))
                .get("state")
                .and_then(Value::as_str),
            Some("cancelled")
        );
    });
    engine.stopped.store(true, Ordering::Release);
    server.join().unwrap().unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[test]
fn cli_demo_completes_with_native_torrent_and_local_plex_fixture() {
    let directory = Directory::new();
    let demo = directory.0.join("demo");
    let output = parse(command(
        &directory.0,
        &["demo", "--dir", demo.to_str().unwrap()],
    ));
    assert_eq!(output.get("plex_scan_confirmed"), Some(&Value::Bool(true)));
    let job = output.get("job").unwrap();
    assert_eq!(job.get("state").and_then(Value::as_str), Some("ready"));
    assert_eq!(job.get("progress").and_then(Value::as_f64), Some(1.0));
    let imported = job.get("imports").unwrap().as_array().unwrap()[0]
        .as_str()
        .unwrap();
    assert_eq!(
        fs::read(imported).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
    assert!(
        output
            .get("events")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.get("state").and_then(Value::as_str) == Some("ready"))
    );
    assert!(
        !command(&directory.0, &["demo", "--dir", demo.to_str().unwrap()])
            .status
            .success()
    );
}
