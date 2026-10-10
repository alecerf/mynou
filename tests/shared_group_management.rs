//! CI-only protected whole-group JSON, form and CLI controls.
mod automatic_pack_support;
mod library_support;
mod series_support;
mod shared_group_support;
#[allow(dead_code)]
mod transfer_support;
mod web_support;
use automatic_pack_support::{Provider, SECRET, no_sources, snapshot};
use library_support::Directory;
use mynou::{
    config::{self, Config},
    engine::{Engine, lock},
    json::{self, Value},
};
use shared_group_support::{Fixture, baseline, baseline_query};
use std::{
    fs,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use web_support::{Server, TOKEN};

fn headers() -> [(String, String); 2] {
    [
        ("Authorization".into(), format!("Bearer {TOKEN}")),
        ("Content-Type".into(), "application/json".into()),
    ]
}

#[test]
fn authenticated_group_routes_require_complete_scope_and_guarded_quality_decisions() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let provider = Provider::open();
    let server = Server::open(fixture.cfg.clone());
    let parents = fixture.parents(&server.engine);
    let route = format!("/api/library/{}/group", parents[0].id);
    let headers = headers();
    let auth: Vec<_> = headers
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    assert_eq!(
        server
            .call(
                "POST",
                &route,
                &[],
                &json::stringify(&baseline_query().to_json())
            )
            .status,
        401
    );
    for patch in [
        r#"{"apply":true}"#,
        r#"{"plan_id":"bad"}"#,
        r#"{"owners":[1]}"#,
        r#"{"source_url":"private"}"#,
        r#"{"action":"promote"}"#,
    ] {
        let mut body = baseline_query().to_json();
        for (key, value) in json::parse(patch).unwrap().as_object().unwrap() {
            body.insert(key, value.clone());
        }
        let reply = server.call("POST", &route, &auth, &json::stringify(&body));
        assert_eq!(reply.status, 400, "{}", reply.body);
    }
    assert!(provider.calls.lock().unwrap().is_empty());
    let before = snapshot(&fixture.cfg.store_dir);
    let reply = server.call(
        "POST",
        &route,
        &auth,
        &json::stringify(&baseline_query().to_json()),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(snapshot(&fixture.cfg.store_dir), before);
    let preview = json::parse(&reply.body).unwrap();
    no_sources(&preview);
    let mut body = baseline_query().to_json();
    body.insert("apply", true);
    body.insert("plan_id", preview.get("plan_id").unwrap().clone());
    assert_eq!(
        server
            .call("POST", &route, &auth, &json::stringify(&body))
            .status,
        200
    );
    assert!(
        lock(&server.engine.store)
            .unwrap()
            .shared_group(&parents[0].id)
            .unwrap()
            .iter()
            .all(|job| job.release.is_some())
    );
    let mut query = fixture.replacement_query();
    query.source_url =
        Some(provider.metadata_url("replacement", fs::read(&fixture.replacement.path).unwrap()));
    let preview = server.call("POST", &route, &auth, &json::stringify(&query.to_json()));
    assert_eq!(preview.status, 200, "{}", preview.body);
    assert!(!preview.body.contains(SECRET));
    let preview = json::parse(&preview.body).unwrap();
    no_sources(&preview);
    query.apply = true;
    query.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    let reply = server.call("POST", &route, &auth, &json::stringify(&query.to_json()));
    assert_eq!(reply.status, 200, "{}", reply.body);
    no_sources(&json::parse(&reply.body).unwrap());
    assert_eq!(lock(&server.engine.store).unwrap().list().len(), 4);
    assert_eq!(
        server
            .call(
                "POST",
                &format!("/api/library/{}/baseline", parents[0].id),
                &auth,
                r#"{"release_title":"Fixture.Series.S01E01.720p.WEB-DL"}"#
            )
            .status,
        400
    );
}

fn config_value(cfg: &Config) -> Value {
    let mut value = config::default_json();
    value.insert("listen", cfg.listen.clone());
    value.insert("store_dir", cfg.store_dir.to_str().unwrap().to_owned());
    let library = value.get_mut("library").unwrap();
    library.insert("movies_root", cfg.movies_root.to_str().unwrap().to_owned());
    library.insert("series_root", cfg.series_root.to_str().unwrap().to_owned());
    value.get_mut("downloads").unwrap().insert("enabled", false);
    value.insert("selection", cfg.selection.to_json());
    value
}
fn command(directory: &Directory, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(&directory.0)
        .env_remove("MYNOU_API_TOKEN")
        .env_remove("MYNOU_PLEX_TOKEN")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!(
                "Group CLI timed out: {:?}",
                child.wait_with_output().unwrap()
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn success(result: Output) -> Value {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    json::parse(std::str::from_utf8(&result.stdout).unwrap()).unwrap()
}

#[test]
fn cli_offline_preview_preserves_state_and_service_apply_uses_the_same_guard() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    baseline(&engine, &parents[0].id);
    let mapping = fixture.replacement_query().to_json();
    let before = snapshot(&fixture.cfg.store_dir);
    drop(engine);
    fs::write(
        directory.0.join("mynou.json"),
        json::stringify(&config_value(&fixture.cfg)),
    )
    .unwrap();
    let Value::Object(mut mapping) = mapping else {
        panic!("group query");
    };
    mapping.remove("apply");
    fs::write(
        directory.0.join("group.json"),
        json::stringify(&Value::Object(mapping)),
    )
    .unwrap();
    let owner = parents[0].id.as_str();
    let preview = success(command(
        &directory,
        &["library-group", owner, "--mapping", "group.json"],
    ));
    no_sources(&preview);
    assert_eq!(snapshot(&fixture.cfg.store_dir), before);
    let plan = preview.get("plan_id").unwrap().as_str().unwrap();
    assert!(
        !command(
            &directory,
            &[
                "library-group",
                owner,
                "--mapping",
                "group.json",
                "--apply",
                "--plan-id",
                plan
            ]
        )
        .status
        .success()
    );
    assert_eq!(snapshot(&fixture.cfg.store_dir), before);
    assert!(
        !command(
            &directory,
            &["library-group", owner, "--mapping", "group.json", "--apply"]
        )
        .status
        .success()
    );
    let server = Server::open(fixture.cfg.clone());
    let mut cfg = fixture.cfg.clone();
    cfg.listen = server.authority.clone();
    fs::write(
        directory.0.join("mynou.json"),
        json::stringify(&config_value(&cfg)),
    )
    .unwrap();
    fs::write(
        directory.0.join(".env"),
        format!("MYNOU_API_TOKEN={TOKEN}\n"),
    )
    .unwrap();
    let created = success(command(
        &directory,
        &[
            "library-group",
            owner,
            "--mapping",
            "group.json",
            "--apply",
            "--plan-id",
            plan,
        ],
    ));
    no_sources(&created);
    assert_eq!(created.get("submitted"), Some(&Value::Number(2.0)));
    assert_eq!(lock(&server.engine.store).unwrap().list().len(), 4);
}
