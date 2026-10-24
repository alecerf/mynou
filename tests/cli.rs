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

/// Every file below `root`.
fn files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files(&path));
        } else {
            found.push(path);
        }
    }
    found
}

fn command(directory: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(directory)
        .env("MYNOU_HOME", directory)
        .env("MYNOU_API_TOKEN", TOKEN)
        .env_remove("MYNOU_DEMO_TOKEN")
        .env_remove("MYNOU_PLEX_TOKEN")
        .args(args)
        .output()
        .unwrap()
}
/// A command given a secret on standard input.
fn with_input(directory: &Path, args: &[&str], value: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(directory)
        .env("MYNOU_HOME", directory)
        .env("MYNOU_API_TOKEN", TOKEN)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(value.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
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
fn cli_token_reset_analyze_and_offline_management() {
    let directory = Directory::new();
    let version = success(command(&directory.0, &["version"]));
    assert!(version.contains(env!("CARGO_PKG_VERSION")) && version.contains("0 dependencies"));
    // A token reset creates a private data directory and prints a new token,
    // whose digest alone is stored.
    let home = directory.0.join("home");
    let reset = |dir: &Path| {
        let text = success(command(
            &directory.0,
            &["token", "--reset", "--dir", dir.to_str().unwrap()],
        ));
        text.lines().next().unwrap().to_owned()
    };
    let first = reset(&home);
    assert_eq!(first.len(), 64);
    let second = reset(&home);
    assert_ne!(first, second);
    let digest = mynou::access::stored(&config::store_dir(&home)).unwrap();
    assert_eq!(digest, Some(mynou::crypto::sha256(second.as_bytes())));
    for file in files(&home) {
        let bytes = fs::read(file).unwrap();
        assert!(
            !bytes
                .windows(64)
                .any(|w| w == first.as_bytes() || w == second.as_bytes())
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert!(!command(&directory.0, &["token"]).status.success());
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
    assert_eq!(jobs.get("jobs").unwrap().as_array().unwrap().len(), 1);
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
fn cli_saves_settings_and_secrets_in_storage() {
    let directory = Directory::new();
    assert_eq!(
        parse(command(&directory.0, &["settings"])),
        config::default_json()
    );
    let mut document = config::default_json();
    document.insert("unknown", "field");
    let refused = with_input(
        &directory.0,
        &["settings", "--save"],
        &json::stringify(&document),
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("unknown"));
    assert_eq!(
        parse(command(&directory.0, &["settings"])),
        config::default_json()
    );
    let mut document = config::default_json();
    document.insert("minimum_seeders", 3_u32);
    document.insert(
        "library",
        json::parse(r#"{"movies_root":"films","series_root":"tv"}"#).unwrap(),
    );
    parse(with_input(
        &directory.0,
        &["settings", "--save"],
        &json::stringify(&document),
    ));
    assert_eq!(parse(command(&directory.0, &["settings"])), document);
    let doctor = parse(command(&directory.0, &["doctor"]));
    assert_eq!(
        doctor.get("data_dir").and_then(Value::as_str),
        directory.0.to_str()
    );
    let saved = success(with_input(
        &directory.0,
        &["secrets", "--set", "MYNOU_PLEX_TOKEN"],
        "plex-value-kept-private\n",
    ));
    assert!(saved.contains("\"stored\"") && !saved.contains("plex-value"));
    let listed = success(command(&directory.0, &["secrets"]));
    assert!(listed.contains("MYNOU_PLEX_TOKEN") && !listed.contains("plex-value"));
    success(command(
        &directory.0,
        &["secrets", "--delete", "MYNOU_PLEX_TOKEN"],
    ));
    assert!(!success(command(&directory.0, &["secrets"])).contains("\"stored\""));
    // Another data directory has its own settings.
    let other = Directory::new();
    assert_eq!(
        parse(command(
            &directory.0,
            &["settings", "--dir", other.0.to_str().unwrap()]
        )),
        config::default_json()
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
    // A running service records where it listens; commands find it there.
    fs::write(
        directory.0.join("state/endpoint"),
        api.address().unwrap().to_string(),
    )
    .unwrap();
    let server = thread::spawn(move || api.run());
    let result = std::panic::catch_unwind(|| {
        // Without the token, commands explain how to reach the service.
        let anonymous = Command::new(env!("CARGO_BIN_EXE_mynou"))
            .env("MYNOU_HOME", &directory.0)
            .env_remove("MYNOU_API_TOKEN")
            .arg("jobs")
            .output()
            .unwrap();
        assert!(!anonymous.status.success());
        assert!(String::from_utf8_lossy(&anonymous.stderr).contains("set MYNOU_API_TOKEN"));
        let reset = command(&directory.0, &["token", "--reset"]);
        assert!(!reset.status.success());
        assert!(String::from_utf8_lossy(&reset.stderr).contains("Stop Mynou"));
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
                .get("jobs")
                .unwrap()
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
        // Settings and secrets go through the running service.
        let mut document = value.clone();
        document.insert("workers", 4_u32);
        parse(with_input(
            &directory.0,
            &["settings", "--save"],
            &json::stringify(&document),
        ));
        assert_eq!(parse(command(&directory.0, &["settings"])), document);
        assert!(
            !with_input(&directory.0, &["secrets", "--set", "MYNOU_API_TOKEN"], "x")
                .status
                .success()
        );
        let saved = success(with_input(
            &directory.0,
            &["secrets", "--set", "MYNOU_ONLINE_TEST"],
            "online-value\n",
        ));
        assert!(saved.contains("MYNOU_ONLINE_TEST") && !saved.contains("online-value"));
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

#[test]
fn private_vpn_import_and_explicit_controls_survive_offline_cli_restart_with_redacted_output() {
    use std::os::unix::fs::PermissionsExt;
    let directory = Directory::new();
    let profile = directory.0.join("private.conf");
    fs::write(&profile, "[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nAddress = 10.23.0.2/24\nDNS = 10.23.0.1\n[Peer]\nPublicKey = CQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\nAllowedIPs = 0.0.0.0/0\nEndpoint = 192.0.2.10:51820\n").unwrap();
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o600)).unwrap();
    let output = command(
        &directory.0,
        &[
            "vpn",
            "--action",
            "import",
            "--profile",
            profile.to_str().unwrap(),
        ],
    );
    assert!(output.status.success());
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains("AQEBAQE"));
        assert!(!text.contains("192.0.2.10"));
        assert!(!text.contains(profile.to_str().unwrap()));
    }
    let status = parse(command(&directory.0, &["vpn"]));
    assert_eq!(status.get("state").and_then(Value::as_str), Some("blocked"));
    assert_eq!(status.get("enabled").and_then(Value::as_bool), Some(true));
    for action in ["revoke", "enable", "reconnect", "disable"] {
        parse(command(&directory.0, &["vpn", "--action", action]));
    }
    assert_eq!(
        parse(command(&directory.0, &["vpn"]))
            .get("state")
            .and_then(Value::as_str),
        Some("disabled")
    );
    let failed = command(
        &directory.0,
        &[
            "vpn",
            "--action",
            "import",
            "--profile",
            "absent-secret-file",
        ],
    );
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains("absent-secret-file"));
}
