//! Authenticated library controls and CLI routing; all services are synthetic and local.
mod jobs_state;
mod library_support;

use library_support::*;
use mynou::config;
use mynou::engine::{Engine, lock};
use mynou::json::{self, Value};
use mynou::net::HttpClient;
use mynou::server::Api;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
        let api = Api::bind(engine.clone()).unwrap();
        let origin = format!("http://{}", api.address().unwrap());
        Self {
            engine,
            origin,
            thread: Some(thread::spawn(move || api.run())),
        }
    }

    fn call(
        &self,
        method: &str,
        route: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let mut headers = vec![("Content-Type".into(), "application/json".into())];
        if let Some(token) = token {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        let bytes = body.map_or_else(Vec::new, |value| json::stringify(value).into_bytes());
        let response = HttpClient::new()
            .without_proxy()
            .with_timeout(Duration::from_secs(5))
            .request(method, &format!("{}{route}", self.origin), &headers, &bytes)
            .unwrap();
        (
            response.status,
            json::parse(std::str::from_utf8(&response.body).unwrap()).unwrap(),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.engine.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if !thread::panicking() => {
                    panic!("Library API fixture failed: {error}")
                }
                Err(error) if !thread::panicking() => std::panic::resume_unwind(error),
                _ => {}
            }
        }
    }
}

fn candidate_indexer() -> Indexer {
    Indexer::open(Value::Array(vec![release(
        BETTER,
        2,
        &format!("http://127.0.0.1:1/fixture.torrent?token={DOWNLOAD_SECRET}"),
    )]))
}

fn object(key: &str, value: impl Into<Value>) -> Value {
    let mut body = Value::object();
    body.insert(key, value);
    body
}

#[test]
fn api_exposes_owned_entries_and_requires_explicit_application_of_upgrades() {
    let directory = Directory::new();
    let indexer = candidate_indexer();
    let server = Server::open(&directory.0, &configuration(Some(&indexer), HD));
    let original = local_import(&server.engine, &directory.0, "Fixture Movie");
    let (status, library) = server.call("GET", "/api/library", Some(TOKEN), None);
    assert_eq!(status, 200);
    let entry = &library.get("library").unwrap().as_array().unwrap()[0];
    assert_eq!(
        entry.get("id").and_then(Value::as_str),
        Some(original.id.as_str())
    );
    assert_eq!(entry.get("baseline_required"), Some(&Value::Bool(true)));
    assert_eq!(entry.get("imports_present"), Some(&Value::Bool(true)));
    assert_eq!(entry.get("pending_upgrade_id"), Some(&Value::Null));
    assert!(entry.get("lease_id").is_none());
    assert_redacted(&library);

    let baseline_route = format!("/api/library/{}/baseline", original.id);
    let (status, job) = server.call(
        "POST",
        &baseline_route,
        Some(TOKEN),
        Some(&object("release_title", BASELINE)),
    );
    assert_eq!(status, 200, "{}", json::stringify(&job));
    assert_eq!(
        job.get("release")
            .unwrap()
            .get("title")
            .and_then(Value::as_str),
        Some(BASELINE)
    );
    assert_redacted(&job);
    let before = files(&directory.0);
    let (status, preview) =
        server.call("POST", "/api/upgrades", Some(TOKEN), Some(&Value::object()));
    assert_eq!(status, 200, "{}", json::stringify(&preview));
    assert_eq!(preview.get("apply"), Some(&Value::Bool(false)));
    assert_eq!(preview.get("queued").and_then(Value::as_u64), Some(0));
    assert_eq!(
        preview.get("entries").unwrap().as_array().unwrap()[0]
            .get("action")
            .and_then(Value::as_str),
        Some("upgrade_available")
    );
    assert_eq!(files(&directory.0), before);
    assert_eq!(lock(&server.engine.store).unwrap().list().unwrap().len(), 1);
    assert_redacted(&preview);

    let (status, applied) = server.call(
        "POST",
        "/api/upgrades",
        Some(TOKEN),
        Some(&object("apply", true)),
    );
    assert_eq!(status, 200, "{}", json::stringify(&applied));
    assert_eq!(applied.get("queued").and_then(Value::as_u64), Some(1));
    assert_redacted(&applied);
    let (_, library) = server.call("GET", "/api/library", Some(TOKEN), None);
    let library = library.get("library").unwrap();
    assert_eq!(library.as_array().unwrap().len(), 1);
    let pending = library.as_array().unwrap()[0]
        .get("pending_upgrade_id")
        .unwrap()
        .as_str()
        .unwrap();
    assert_ne!(pending, original.id);
    let child = lock(&server.engine.store)
        .unwrap()
        .get(pending)
        .unwrap()
        .unwrap();
    assert_eq!(child.upgrade_parent.as_deref(), Some(original.id.as_str()));
    assert_eq!(child.state, "queued");
    assert_redacted(library);
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
        (
            "/api/upgrades",
            vec![
                Value::Null,
                Value::Array(Vec::new()),
                object("apply", "true"),
                object("unexpected", true),
            ],
        ),
        (
            monitor_route.as_str(),
            vec![
                Value::object(),
                object("enabled", "false"),
                object("enabled", 1_u32),
                object("unexpected", false),
            ],
        ),
        (
            baseline_route.as_str(),
            vec![
                Value::object(),
                object("release_title", ""),
                object("release_title", 42_u32),
                object("release_title", "Another.Movie.2024.720p.EN"),
                object("unexpected", BASELINE),
            ],
        ),
    ] {
        for body in bodies {
            let (status, error) = server.call("POST", route, Some(TOKEN), Some(&body));
            assert_eq!(
                status,
                400,
                "Accepted {route}: {} => {}",
                json::stringify(&body),
                json::stringify(&error)
            );
            assert!(error.get("error").and_then(Value::as_str).is_some());
            assert_redacted(&error);
        }
    }
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
    assert_eq!(files(&directory.0), before);
    assert_eq!(
        lock(&server.engine.store)
            .unwrap()
            .get(&original.id)
            .unwrap()
            .unwrap(),
        original
    );
}
