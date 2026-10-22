//! Per-title profiles through the engine, API, upgrades and restart (#338).
mod library_support;

use library_support::*;
use mynou::engine::{Engine, lock};
use mynou::json::{self, Value};
use mynou::net::HttpClient;
use mynou::profiles::ControlRequest;
use mynou::server::Api;
use mynou::store::{Job, Request};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The global profile stops upgrades at 1080p; `uhd` continues to 2160p.
const GLOBAL: &str =
    r#"{"resolutions":[2160,1080,720],"cutoff_resolution":1080,"languages":["en"]}"#;
const UHD: &str = r#"{"resolutions":[2160,1080],"cutoff_resolution":2160,"languages":["en"]}"#;
const SD: &str = r#"{"resolutions":[720],"languages":["en"]}"#;

fn configuration(directory: &Path, indexer: &Indexer) -> mynou::config::Config {
    let mut value = library_support::configuration(Some(indexer), GLOBAL);
    let Some(Value::Object(profiles)) = value
        .get_mut("selection")
        .and_then(|selection| selection.get_mut("profiles"))
    else {
        panic!("profiles section");
    };
    profiles.insert("uhd".into(), json::parse(UHD).unwrap());
    profiles.insert("sd".into(), json::parse(SD).unwrap());
    mynou::config::from_json(&value, directory).unwrap()
}

fn releases() -> Value {
    Value::Array(
        ["First", "Second"]
            .iter()
            .flat_map(|name| {
                [
                    release(
                        &format!("{name}.Movie.2024.2160p.WEB-DL.EN"),
                        5,
                        &format!("magnet:?xt=urn:btih:{}", "1".repeat(40)),
                    ),
                    release(
                        &format!("{name}.Movie.2024.720p.WEB-DL.EN"),
                        50,
                        &format!("magnet:?xt=urn:btih:{}", "2".repeat(40)),
                    ),
                ]
            })
            .collect(),
    )
}

fn identified(title: &str, tmdb_id: u64) -> Request {
    let mut request = movie(title);
    request.tmdb_id = Some(tmdb_id);
    request
}

/// An owned 1080p import with a TMDB identity, so it can be bound.
fn import(engine: &Arc<Engine>, directory: &Path, title: &str, tmdb_id: u64) -> Job {
    let source = directory.join(format!("{}.mp4", title.replace(' ', "-")));
    fs::write(&source, include_bytes!("../examples/demo.mp4")).unwrap();
    let mut request = identified(title, tmdb_id);
    request.source_path = Some(source.to_str().unwrap().into());
    let id = engine.submit(request).unwrap().remove(0).id;
    let job = run_until(engine, &id, "ready");
    let baseline = format!("{}.2024.1080p.WEB-DL.EN", title.replace(' ', "."));
    engine.set_baseline(&job.id, &baseline).unwrap()
}

fn control(engine: &Engine, text: &str) -> Value {
    let preview = engine
        .title_profile_control(&ControlRequest::from_json(&json::parse(text).unwrap()).unwrap())
        .unwrap();
    let mut apply = json::parse(text).unwrap();
    apply.insert("apply", true);
    apply.insert("plan_id", preview.get("plan_id").unwrap().clone());
    engine
        .title_profile_control(&ControlRequest::from_json(&apply).unwrap())
        .unwrap()
}

fn actions(report: &Value) -> BTreeMap<String, String> {
    report
        .get("entries")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry.get("job_id").and_then(Value::as_str).unwrap().into(),
                entry.get("action").and_then(Value::as_str).unwrap().into(),
            )
        })
        .collect()
}

#[test]
fn bound_movies_use_their_own_upgrade_cutoffs_across_changes_and_restart() {
    let directory = Directory::new();
    let indexer = Indexer::open(releases());
    let config = configuration(&directory.0, &indexer);
    let engine = Engine::open(config.clone()).unwrap();
    let first = import(&engine, &directory.0, "First Movie", 101);
    let second = import(&engine, &directory.0, "Second Movie", 102);

    // Both recorded 1080p releases reach the global cutoff without a search.
    let report = engine.check_upgrades(false).unwrap();
    assert!(actions(&report).values().all(|a| a == "cutoff_reached"));
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);

    control(
        &engine,
        r#"{"action":"set","kind":"movie","tmdb_id":101,"profile":"uhd"}"#,
    );
    let report = engine.check_upgrades(false).unwrap();
    let found = actions(&report);
    assert_eq!(found[&first.id], "upgrade_available");
    assert_eq!(found[&second.id], "cutoff_reached");

    // A 720p-only binding makes the recorded 1080p release fall outside its
    // profile, so the bound title's own best release replaces it.
    control(
        &engine,
        r#"{"action":"set","kind":"movie","tmdb_id":102,"profile":"sd"}"#,
    );
    let found = actions(&engine.check_upgrades(false).unwrap());
    assert_eq!(found[&first.id], "upgrade_available");
    assert_eq!(found[&second.id], "upgrade_available");

    // Clearing a binding returns the title to the global cutoff.
    control(
        &engine,
        r#"{"action":"clear","kind":"movie","tmdb_id":102}"#,
    );
    let found = actions(&engine.check_upgrades(false).unwrap());
    assert_eq!(found[&second.id], "cutoff_reached");
    control(
        &engine,
        r#"{"action":"set","kind":"movie","tmdb_id":102,"profile":"sd"}"#,
    );
    drop(engine);

    let engine = Engine::open(config.clone()).unwrap();
    let found = actions(&engine.check_upgrades(false).unwrap());
    assert_eq!(found[&first.id], "upgrade_available");
    assert_eq!(found[&second.id], "upgrade_available");

    // Applying queues each title's upgrade under its own bound profile.
    assert_eq!(
        engine
            .check_upgrades(true)
            .unwrap()
            .get("queued")
            .and_then(Value::as_u64),
        Some(2)
    );
    let children = |parent: &str| -> Vec<Job> {
        lock(&engine.store)
            .unwrap()
            .list()
            .unwrap()
            .into_iter()
            .filter(|job| job.upgrade_parent.as_deref() == Some(parent))
            .collect()
    };
    for (parent, profile, title) in [
        (&first.id, "uhd", "First.Movie.2024.2160p.WEB-DL.EN"),
        (&second.id, "sd", "Second.Movie.2024.720p.WEB-DL.EN"),
    ] {
        let pending = children(parent);
        assert_eq!(pending.len(), 1);
        let release = pending[0].release.clone().unwrap();
        assert_eq!(
            (release.profile.as_str(), release.title.as_str()),
            (profile, title)
        );
    }

    // A later edit leaves the already selected upgrade untouched.
    control(
        &engine,
        r#"{"action":"clear","kind":"movie","tmdb_id":101}"#,
    );
    let pending = children(&first.id);
    assert_eq!(pending[0].release.as_ref().unwrap().profile, "uhd");
    assert_eq!(
        actions(&engine.check_upgrades(false).unwrap())[&first.id],
        "upgrade_pending"
    );
}

struct Server {
    engine: Arc<Engine>,
    origin: String,
    thread: Option<JoinHandle<mynou::Result<()>>>,
}

impl Server {
    fn open(engine: Arc<Engine>) -> Self {
        let api = Api::bind(engine.clone(), TOKEN.into()).unwrap();
        let origin = format!("http://{}", api.address().unwrap());
        Self {
            engine,
            origin,
            thread: Some(thread::spawn(move || api.run())),
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let headers = vec![
            ("Content-Type".into(), "application/json".into()),
            ("Authorization".into(), format!("Bearer {TOKEN}")),
        ];
        let body = body.map_or_else(Vec::new, |value| json::stringify(value).into_bytes());
        let response = HttpClient::new()
            .without_proxy()
            .with_timeout(Duration::from_secs(5))
            .with_max_body(1_048_576)
            .request(method, &format!("{}{path}", self.origin), &headers, &body)
            .unwrap();
        let value = json::parse(std::str::from_utf8(&response.body).unwrap()).unwrap();
        (response.status, value)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.engine.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if !thread::panicking() => panic!("API fixture failed: {error}"),
                Err(error) if !thread::panicking() => std::panic::resume_unwind(error),
                _ => {}
            }
        }
    }
}

#[test]
fn the_api_reviews_bindings_and_search_previews_select_under_them() {
    let directory = Directory::new();
    let indexer = Indexer::open(releases());
    let engine = Engine::open(configuration(&directory.0, &indexer)).unwrap();
    let server = Server::open(engine);
    let search = |tmdb_id: u64| {
        let (status, report) = server.call(
            "POST",
            "/api/search",
            Some(&identified("First Movie", tmdb_id).to_json()),
        );
        assert_eq!(status, 200, "{}", json::stringify(&report));
        let accepted = report.get("accepted").and_then(Value::as_array).unwrap();
        let selected = accepted
            .iter()
            .find(|c| c.get("id") == report.get("selected_candidate_id"))
            .map(|c| c.get("title").and_then(Value::as_str).unwrap().to_owned());
        (
            report
                .get("profile")
                .and_then(Value::as_str)
                .unwrap()
                .to_owned(),
            report.get("title_profile").cloned().unwrap(),
            selected,
        )
    };
    assert_eq!(
        search(101),
        (
            "fixture".into(),
            Value::Null,
            Some("First.Movie.2024.2160p.WEB-DL.EN".into())
        )
    );

    let mut body =
        json::parse(r#"{"action":"set","kind":"movie","tmdb_id":101,"profile":"sd"}"#).unwrap();
    let (status, preview) = server.call("POST", "/api/title-profiles/control", Some(&body));
    assert_eq!(status, 200, "{}", json::stringify(&preview));
    assert_eq!(preview.get("bind").and_then(Value::as_u64), Some(1));
    let (_, listed) = server.call("GET", "/api/title-profiles", None);
    assert_eq!(listed.get("total").and_then(Value::as_u64), Some(0));

    body.insert("apply", true);
    body.insert("plan_id", preview.get("plan_id").unwrap().clone());
    let (status, applied) = server.call("POST", "/api/title-profiles/control", Some(&body));
    assert_eq!(status, 200, "{}", json::stringify(&applied));
    let (status, listed) = server.call("GET", "/api/title-profiles?limit=10", None);
    assert_eq!(status, 200);
    let bindings = listed.get("bindings").and_then(Value::as_array).unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        bindings[0].get("profile").and_then(Value::as_str),
        Some("sd")
    );
    assert_eq!(bindings[0].get("resolved"), Some(&Value::Bool(true)));
    assert_eq!(listed.get("next"), Some(&Value::Null));

    // The bound title selects the 720p release; another identity is unaffected.
    assert_eq!(
        search(101),
        (
            "sd".into(),
            Value::String("sd".into()),
            Some("First.Movie.2024.720p.WEB-DL.EN".into())
        )
    );
    assert_eq!(search(999).0, "fixture");

    // A replayed plan and an unconfigured profile are refused without writes.
    let (status, _) = server.call("POST", "/api/title-profiles/control", Some(&body));
    assert_eq!(status, 400);
    let unknown =
        json::parse(r#"{"action":"set","kind":"movie","tmdb_id":5,"profile":"absent"}"#).unwrap();
    let (status, _) = server.call("POST", "/api/title-profiles/control", Some(&unknown));
    assert_eq!(status, 400);
    let (status, _) = server.call("GET", "/api/title-profiles?after=episode:1", None);
    assert_eq!(status, 400);
    let (_, listed) = server.call("GET", "/api/title-profiles", None);
    assert_eq!(listed.get("total").and_then(Value::as_u64), Some(1));
}
