//! Authenticated library controls and CLI routing; all services are synthetic and local.
mod library_support;

use library_support::*;
use mynou::config;
use mynou::engine::{Engine, lock};
use mynou::json::{self, Value};
use mynou::net::HttpClient;
use mynou::server::Api;
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const HD: &str = r#"{"resolutions":[1080,720],"languages":["en"]}"#;
const BASELINE: &str = "Fixture.Movie.2024.720p.WEB-DL.EN";
const BETTER: &str = "Fixture.Movie.2024.1080p.WEB-DL.EN";

struct Server {
    engine: Arc<Engine>,
    origin: String,
    thread: Option<JoinHandle<mynou::Result<()>>>,
}

impl Server {
    fn open(directory: &Path, configuration: &Value) -> Self {
        let engine = Engine::open(config::from_json(configuration, directory).unwrap()).unwrap();
        let api = Api::bind(engine.clone(), TOKEN.into()).unwrap();
        let origin = format!("http://{}", api.address().unwrap());
        Self { engine, origin, thread: Some(thread::spawn(move || api.run())) }
    }

    fn call(&self, method: &str, route: &str, token: Option<&str>, body: Option<&Value>) -> (u16, Value) {
        let mut headers = vec![("Content-Type".into(), "application/json".into())];
        if let Some(token) = token {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        let bytes = body.map_or_else(Vec::new, |value| json::stringify(value).into_bytes());
        let response = HttpClient::new().without_proxy().with_timeout(Duration::from_secs(5))
            .request(method, &format!("{}{route}", self.origin), &headers, &bytes).unwrap();
        (response.status, json::parse(std::str::from_utf8(&response.body).unwrap()).unwrap())
    }

    fn write_cli_config(&self, directory: &Path, mut configuration: Value) {
        configuration.insert("listen", self.origin.strip_prefix("http://").unwrap());
        fs::write(directory.join("mynou.json"), json::stringify(&configuration)).unwrap();
        fs::write(directory.join(".env"), format!("MYNOU_API_TOKEN={TOKEN}\n")).unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.engine.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if !thread::panicking() => panic!("Library API fixture failed: {error}"),
                Err(error) if !thread::panicking() => std::panic::resume_unwind(error),
                _ => {}
            }
        }
    }
}

fn candidate_indexer() -> Indexer {
    Indexer::open(Value::Array(vec![release(BETTER, 2, &format!(
        "http://127.0.0.1:1/fixture.torrent?token={DOWNLOAD_SECRET}"
    ))]))
}

fn object(key: &str, value: impl Into<Value>) -> Value {
    let mut body = Value::object();
    body.insert(key, value);
    body
}

fn command(directory: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(directory).env_remove("MYNOU_API_TOKEN")
        .env_remove("MYNOU_PLEX_TOKEN").env_remove("MYNOU_DEMO_TOKEN")
        .env_remove("MYNOU_LIBRARY_ABSENT_FIXTURE_KEY_592744")
        .args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if child.try_wait().unwrap().is_some() { return child.wait_with_output().unwrap(); }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("Library CLI timed out: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn successful(output: Output) -> Value {
    assert!(output.status.success(), "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    json::parse(std::str::from_utf8(&output.stdout).unwrap().trim()).unwrap()
}

#[test]
fn library_routes_authenticate_before_reading_mutating_or_searching() {
    let directory = Directory::new();
    let indexer = candidate_indexer();
    let server = Server::open(&directory.0, &configuration(Some(&indexer), HD));
    let original = local_import(&server.engine, &directory.0, "Fixture Movie");
    let before = files(&directory.0);
    let upgrades = object("apply", true);
    let monitor = object("enabled", false);
    let baseline = object("release_title", BASELINE);
    let monitor_route = format!("/api/library/{}/monitor", original.id);
    let baseline_route = format!("/api/library/{}/baseline", original.id);
    for token in [None, Some("incorrect-token")] {
        for (method, route, body) in [
            ("GET", "/api/library", None),
            ("POST", "/api/upgrades", Some(&upgrades)),
            ("POST", monitor_route.as_str(), Some(&monitor)),
            ("POST", baseline_route.as_str(), Some(&baseline)),
        ] {
            let (status, error) = server.call(method, route, token, body);
            assert_eq!(status, 401);
            assert_eq!(error.get("error").and_then(Value::as_str), Some("Authentication required"));
        }
    }
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
    assert_eq!(files(&directory.0), before);
    assert_eq!(lock(&server.engine.store).unwrap().get(&original.id).unwrap(), original);
}

#[test]
fn api_exposes_owned_entries_and_requires_explicit_application_of_upgrades() {
    let directory = Directory::new();
    let indexer = candidate_indexer();
    let server = Server::open(&directory.0, &configuration(Some(&indexer), HD));
    let original = local_import(&server.engine, &directory.0, "Fixture Movie");
    let (status, library) = server.call("GET", "/api/library", Some(TOKEN), None);
    assert_eq!(status, 200);
    let entry = &library.as_array().unwrap()[0];
    assert_eq!(entry.get("id").and_then(Value::as_str), Some(original.id.as_str()));
    assert_eq!(entry.get("baseline_required"), Some(&Value::Bool(true)));
    assert_eq!(entry.get("imports_present"), Some(&Value::Bool(true)));
    assert_eq!(entry.get("pending_upgrade_id"), Some(&Value::Null));
    assert!(entry.get("lease_id").is_none());
    assert_redacted(&library);

    let baseline_route = format!("/api/library/{}/baseline", original.id);
    let (status, job) = server.call("POST", &baseline_route, Some(TOKEN), Some(&object("release_title", BASELINE)));
    assert_eq!(status, 200, "{}", json::stringify(&job));
    assert_eq!(job.get("release").unwrap().get("title").and_then(Value::as_str), Some(BASELINE));
    assert_redacted(&job);
    let before = files(&directory.0);
    let (status, preview) = server.call("POST", "/api/upgrades", Some(TOKEN), Some(&Value::object()));
    assert_eq!(status, 200, "{}", json::stringify(&preview));
    assert_eq!(preview.get("apply"), Some(&Value::Bool(false)));
    assert_eq!(preview.get("queued").and_then(Value::as_u64), Some(0));
    assert_eq!(preview.get("entries").unwrap().as_array().unwrap()[0].get("action").and_then(Value::as_str), Some("upgrade_available"));
    assert_eq!(files(&directory.0), before);
    assert_eq!(lock(&server.engine.store).unwrap().list().len(), 1);
    assert_redacted(&preview);

    let (status, applied) = server.call("POST", "/api/upgrades", Some(TOKEN), Some(&object("apply", true)));
    assert_eq!(status, 200, "{}", json::stringify(&applied));
    assert_eq!(applied.get("queued").and_then(Value::as_u64), Some(1));
    assert_redacted(&applied);
    let (_, library) = server.call("GET", "/api/library", Some(TOKEN), None);
    assert_eq!(library.as_array().unwrap().len(), 1);
    let pending = library.as_array().unwrap()[0].get("pending_upgrade_id").unwrap().as_str().unwrap();
    assert_ne!(pending, original.id);
    let child = lock(&server.engine.store).unwrap().get(pending).unwrap();
    assert_eq!(child.upgrade_parent.as_deref(), Some(original.id.as_str()));
    assert_eq!(child.state, "queued");
    assert_redacted(&library);
}

#[test]
fn api_rejects_malformed_controls_without_modifying_state() {
    let directory = Directory::new();
    let indexer = candidate_indexer();
    let server = Server::open(&directory.0, &configuration(Some(&indexer), HD));
    let original = local_import(&server.engine, &directory.0, "Fixture Movie");
    let baseline_route = format!("/api/library/{}/baseline", original.id);
    let monitor_route = format!("/api/library/{}/monitor", original.id);
    let before = files(&directory.0);
    for (route, bodies) in [
        ("/api/upgrades", vec![Value::Null, Value::Array(Vec::new()), object("apply", "true"), object("unexpected", true)]),
        (monitor_route.as_str(), vec![Value::object(), object("enabled", "false"), object("enabled", 1_u32), object("unexpected", false)]),
        (baseline_route.as_str(), vec![Value::object(), object("release_title", ""), object("release_title", 42_u32), object("release_title", "Another.Movie.2024.720p.EN"), object("unexpected", BASELINE)]),
    ] {
        for body in bodies {
            let (status, error) = server.call("POST", route, Some(TOKEN), Some(&body));
            assert_eq!(status, 400, "Accepted {route}: {} => {}", json::stringify(&body), json::stringify(&error));
            assert!(error.get("error").and_then(Value::as_str).is_some());
            assert_redacted(&error);
        }
    }
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
    assert_eq!(files(&directory.0), before);
    assert_eq!(lock(&server.engine.store).unwrap().get(&original.id).unwrap(), original);
}

#[test]
fn online_cli_uses_the_api_while_the_engine_owns_the_journal_lock() {
    let directory = Directory::new();
    let indexer = candidate_indexer();
    let configuration = configuration(Some(&indexer), HD);
    let server = Server::open(&directory.0, &configuration);
    let original = local_import(&server.engine, &directory.0, "Fixture Movie");
    server.write_cli_config(&directory.0, configuration);
    let before = files(&directory.0);
    let library = successful(command(&directory.0, &["library"]));
    assert_eq!(library, server.engine.library().unwrap());
    assert_eq!(files(&directory.0), before);
    let baseline = successful(command(&directory.0, &["baseline", &original.id, "--release-title", BASELINE]));
    assert_eq!(baseline.get("release").unwrap().get("title").and_then(Value::as_str), Some(BASELINE));
    let unmonitored = successful(command(&directory.0, &["unmonitor", &original.id]));
    assert_eq!(unmonitored.get("monitored"), Some(&Value::Bool(false)));
    let paused = successful(command(&directory.0, &["upgrades", "--apply"]));
    assert_eq!(paused.get("queued").and_then(Value::as_u64), Some(0));
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
    let monitored = successful(command(&directory.0, &["monitor", &original.id]));
    assert_eq!(monitored.get("monitored"), Some(&Value::Bool(true)));
    let before = files(&directory.0);
    let preview = successful(command(&directory.0, &["upgrades"]));
    assert_eq!(preview.get("apply"), Some(&Value::Bool(false)));
    assert_eq!(files(&directory.0), before);
    let applied = successful(command(&directory.0, &["upgrades", "--apply"]));
    assert_eq!(applied.get("queued").and_then(Value::as_u64), Some(1));
    assert_eq!(lock(&server.engine.store).unwrap().list().len(), 2);
    for output in [&library, &baseline, &unmonitored, &paused, &monitored, &preview, &applied] { assert_redacted(output); }
}

#[test]
fn offline_cli_controls_persist_and_previews_leave_the_journal_unchanged() {
    let directory = Directory::new();
    let indexer = candidate_indexer();
    let configuration = configuration(Some(&indexer), HD);
    fs::write(directory.0.join("mynou.json"), json::stringify(&configuration)).unwrap();
    let engine = Engine::open(config::from_json(&configuration, &directory.0).unwrap()).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    let journal = engine.config.store_dir.join("journal.bin");
    drop(engine);
    let before = fs::read(&journal).unwrap();
    let library = successful(command(&directory.0, &["library"]));
    assert_eq!(library.as_array().unwrap().len(), 1);
    assert_eq!(fs::read(&journal).unwrap(), before);
    successful(command(&directory.0, &["baseline", &original.id, "--release-title", BASELINE]));
    successful(command(&directory.0, &["unmonitor", &original.id]));
    let preview = successful(command(&directory.0, &["upgrades"]));
    assert_eq!(preview.get("queued").and_then(Value::as_u64), Some(0));
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
    successful(command(&directory.0, &["monitor", &original.id]));
    let before = fs::read(&journal).unwrap();
    let preview = successful(command(&directory.0, &["upgrades"]));
    assert_eq!(preview.get("apply"), Some(&Value::Bool(false)));
    assert_eq!(fs::read(&journal).unwrap(), before);
    assert_redacted(&preview);
    let engine = Engine::open(config::from_json(&configuration, &directory.0).unwrap()).unwrap();
    let resumed = lock(&engine.store).unwrap().get(&original.id).unwrap();
    assert!(resumed.monitored);
    assert_eq!(resumed.release.unwrap().title, BASELINE);
    assert_eq!(resumed.imports, original.imports);
}

#[test]
fn invalid_cli_controls_fail_before_searching_or_changing_state() {
    let directory = Directory::new();
    let indexer = candidate_indexer();
    let configuration = configuration(Some(&indexer), HD);
    let server = Server::open(&directory.0, &configuration);
    let original = local_import(&server.engine, &directory.0, "Fixture Movie");
    server.write_cli_config(&directory.0, configuration);
    let before = files(&directory.0);
    for args in [
        vec!["library", "unexpected"], vec!["library", "--apply"],
        vec!["upgrades", "unexpected"], vec!["upgrades", "--apply", "--apply"],
        vec!["upgrades", "--unknown"], vec!["monitor"], vec!["unmonitor"],
        vec!["monitor", original.id.as_str(), "extra"],
        vec!["baseline", original.id.as_str()], vec!["baseline", original.id.as_str(), "--release-title"],
    ] {
        let output = command(&directory.0, &args);
        assert!(!output.status.success(), "Accepted invalid CLI arguments: {args:?}");
        assert!(!output.stderr.is_empty(), "Missing CLI diagnostic: {args:?}");
    }
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
    assert_eq!(files(&directory.0), before);
}

#[test]
fn offline_previews_of_a_missing_store_create_no_files_or_directories() {
    let directory = Directory::new();
    let indexer = candidate_indexer();
    fs::write(directory.0.join("mynou.json"), json::stringify(&configuration(Some(&indexer), HD))).unwrap();
    let before = files(&directory.0);
    let library = successful(command(&directory.0, &["library"]));
    assert!(library.as_array().unwrap().is_empty());
    let preview = successful(command(&directory.0, &["upgrades"]));
    assert_eq!(preview.get("apply"), Some(&Value::Bool(false)));
    assert_eq!(preview.get("queued").and_then(Value::as_u64), Some(0));
    assert_eq!(preview.get("checked").and_then(Value::as_u64), Some(0));
    assert!(preview.get("entries").unwrap().as_array().unwrap().is_empty());
    assert_eq!(files(&directory.0), before);
    for name in ["state", "library", "downloads"] {
        assert!(!directory.0.join(name).exists(), "Preview created {name}");
    }
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn offline_previews_refuse_partial_journal_recovery_without_changing_existing_bytes() {
    let directory = Directory::new();
    let indexer = candidate_indexer();
    let configuration = configuration(Some(&indexer), HD);
    fs::write(directory.0.join("mynou.json"), json::stringify(&configuration)).unwrap();
    let engine = Engine::open(config::from_json(&configuration, &directory.0).unwrap()).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    engine.set_baseline(&original.id, BASELINE).unwrap();
    let journal = engine.config.store_dir.join("journal.bin");
    drop(engine);
    let mut incomplete = fs::read(&journal).unwrap();
    // Two bytes cannot contain a complete four-byte record length.
    incomplete.extend_from_slice(&[0, 1]);
    fs::write(&journal, &incomplete).unwrap();
    let before = files(&directory.0);
    for args in [vec!["library"], vec!["upgrades"]] {
        let output = command(&directory.0, &args);
        assert!(!output.status.success(), "Preview silently recovered a partial journal: {args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("recovery"));
        assert_eq!(fs::read(&journal).unwrap(), incomplete);
        assert_eq!(files(&directory.0), before);
    }
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn relative_nested_config_keeps_library_controls_and_preview_on_the_same_store() {
    let directory = Directory::new();
    let nested = directory.0.join("nested");
    fs::create_dir(&nested).unwrap();
    let indexer = candidate_indexer();
    let configuration = configuration(Some(&indexer), HD);
    fs::write(nested.join("mynou.json"), json::stringify(&configuration)).unwrap();
    let engine = Engine::open(config::from_json(&configuration, &nested).unwrap()).unwrap();
    let original = local_import(&engine, &nested, "Fixture Movie");
    let journal = engine.config.store_dir.join("journal.bin");
    drop(engine);
    let library = successful(command(&directory.0, &["library", "--config", "nested/mynou.json"]));
    assert_eq!(library.as_array().unwrap()[0].get("id").and_then(Value::as_str), Some(original.id.as_str()));
    successful(command(&directory.0, &["baseline", &original.id, "--release-title", BASELINE, "--config", "nested/mynou.json"]));
    let before = fs::read(&journal).unwrap();
    let preview = successful(command(&directory.0, &["upgrades", "--config", "nested/mynou.json"]));
    assert_eq!(preview.get("entries").unwrap().as_array().unwrap()[0].get("action").and_then(Value::as_str), Some("upgrade_available"));
    assert_eq!(fs::read(&journal).unwrap(), before);
    assert!(!directory.0.join("state").exists());
    assert!(!directory.0.join("library").exists());
    assert_redacted(&library);
    assert_redacted(&preview);
}
