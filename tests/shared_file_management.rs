//! Protected shared-file previews and guarded application through HTTP, forms and CLI.
mod automatic_pack_support;
mod library_support;
mod series_support;
#[allow(dead_code)]
mod transfer_support;
mod web_support;
use automatic_pack_support::{Provider, SECRET, no_sources, snapshot};
use library_support::Directory;
use mynou::{
    config::Config,
    engine::{Engine, lock},
    json::{self, Value},
    pack::SharedFileRequest,
};
use series_support::{Catalog, episode, id, request};
use std::{
    fs,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use transfer_support::{BLOCK, Torrent, payload};
use web_support::{Server, TOKEN};

fn setup(directory: &Directory, provider: &Provider) -> (Catalog, Config, String) {
    let catalog = Catalog::open(vec![
        episode(1, 1, Some("2024-01-01"), "First"),
        episode(1, 2, Some("2024-01-01"), "Second"),
    ]);
    let torrent = Torrent::multiple(
        &directory.0.join("metadata"),
        "Pack",
        vec![("shared.mp4".into(), payload(BLOCK, 11))],
    );
    let source = provider.metadata_url("controls", fs::read(&torrent.path).unwrap());
    let cfg = catalog.config(&directory.0.join("engine"));
    (catalog, cfg, source)
}
fn tracked(engine: &Engine) -> String {
    id(&engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap())
    .into()
}
fn body(source: &str) -> Value {
    SharedFileRequest {
        source_url: source.into(),
        file_path: "Pack/shared.mp4".into(),
        season: 1,
        episodes: vec![1, 2],
        apply: false,
        plan_id: None,
    }
    .to_json()
}

#[test]
fn bearer_routes_reject_invalid_controls_before_io_and_apply_only_the_reviewed_binding() {
    let directory = Directory::new();
    let provider = Provider::open();
    let (catalog, _cfg, source) = setup(&directory, &provider);
    let cfg = catalog.config(&directory.0.join("engine"));
    let server = Server::open(cfg);
    let series = tracked(&server.engine);
    let route = format!("/api/series/{series}/shared-file");
    assert_eq!(
        server
            .call("POST", &route, &[], &json::stringify(&body(&source)))
            .status,
        401
    );
    let auth = format!("Bearer {TOKEN}");
    let headers = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
    ];
    for change in [
        r#"{"apply":true}"#,
        r#"{"season":1.5}"#,
        r#"{"file_path":"../bad.mp4"}"#,
        r#"{"episodes":[1,1]}"#,
        r#"{"unknown":1}"#,
    ] {
        let mut value = body(&source);
        for (key, field) in json::parse(change).unwrap().as_object().unwrap() {
            value.insert(key, field.clone());
        }
        let reply = server.call("POST", &route, &headers, &json::stringify(&value));
        assert_eq!(reply.status, 400, "{}", reply.body);
    }
    assert!(provider.calls.lock().unwrap().is_empty());
    let reply = server.call("POST", &route, &headers, &json::stringify(&body(&source)));
    assert_eq!(reply.status, 200, "{}", reply.body);
    let preview = json::parse(&reply.body).unwrap();
    no_sources(&preview);
    assert!(lock(&server.engine.store).unwrap().list().is_empty());
    let mut value = body(&source);
    value.insert("apply", true);
    value.insert("plan_id", preview.get("plan_id").unwrap().clone());
    let reply = server.call("POST", &route, &headers, &json::stringify(&value));
    assert_eq!(reply.status, 200, "{}", reply.body);
    no_sources(&json::parse(&reply.body).unwrap());
    let owners = lock(&server.engine.store).unwrap().list();
    assert_eq!(owners.len(), 2);
    assert_eq!(owners[0].shared_file, owners[1].shared_file);
    assert!(
        owners
            .iter()
            .all(|j| !json::stringify(&mynou::engine::public_job(j)).contains(SECRET))
    );
}

fn config_value(cfg: &Config) -> Value {
    let mut value = mynou::config::default_json();
    value.insert("listen", cfg.listen.clone());
    value.insert("store_dir", cfg.store_dir.to_str().unwrap());
    value
        .get_mut("library")
        .unwrap()
        .insert("series_root", cfg.series_root.to_str().unwrap());
    value.get_mut("downloads").unwrap().insert("enabled", false);
    let catalog = value.get_mut("catalog").unwrap();
    catalog.insert("enabled", true);
    catalog.insert("url", cfg.catalog.url.clone());
    catalog.insert("token_env", cfg.catalog.token_env.clone());
    catalog.insert("api_key_env", cfg.catalog.api_key_env.clone());
    value
}
fn command(directory: &Directory, arguments: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(&directory.0)
        .env_remove("MYNOU_API_TOKEN")
        .env_remove("MYNOU_PLEX_TOKEN")
        .args(arguments)
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
                "Shared-file CLI timed out: {:?}",
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
fn cli_offline_preview_preserves_storage_and_online_apply_requires_its_guard() {
    let directory = Directory::new();
    let provider = Provider::open();
    let (catalog, _cfg, source) = setup(&directory, &provider);
    let mut cfg = catalog.config(&directory.0.join("engine"));
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let series = tracked(&engine);
    drop(engine);
    fs::write(
        directory.0.join("mynou.json"),
        json::stringify(&config_value(&cfg)),
    )
    .unwrap();
    fs::write(
        directory.0.join("shared.json"),
        r#"{"file_path":"Pack/shared.mp4","season":1,"episodes":[1,2]}"#,
    )
    .unwrap();
    let before = snapshot(&cfg.store_dir);
    let report = success(command(
        &directory,
        &[
            "series-shared-file",
            &series,
            "--url",
            &source,
            "--mapping",
            "shared.json",
        ],
    ));
    no_sources(&report);
    assert_eq!(snapshot(&cfg.store_dir), before);
    let plan = report.get("plan_id").unwrap().as_str().unwrap();
    assert!(
        !command(
            &directory,
            &[
                "series-shared-file",
                &series,
                "--url",
                &source,
                "--mapping",
                "shared.json",
                "--apply",
                "--plan-id",
                plan
            ]
        )
        .status
        .success()
    );
    assert_eq!(snapshot(&cfg.store_dir), before);
    let calls = provider.calls.lock().unwrap().len();
    for arguments in [
        vec![
            "series-shared-file",
            &series,
            "--url",
            &source,
            "--mapping",
            "shared.json",
            "--apply",
        ],
        vec![
            "series-shared-file",
            &series,
            "--url",
            &source,
            "--mapping",
            "shared.json",
            "--plan-id",
            "invalid",
        ],
    ] {
        assert!(!command(&directory, &arguments).status.success());
    }
    assert_eq!(provider.calls.lock().unwrap().len(), calls);
    let server = Server::open(cfg.clone());
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
    let applied = success(command(
        &directory,
        &[
            "series-shared-file",
            &series,
            "--url",
            &source,
            "--mapping",
            "shared.json",
            "--apply",
            "--plan-id",
            plan,
        ],
    ));
    no_sources(&applied);
    assert_eq!(applied.get("submitted"), Some(&Value::Number(2.0)));
    assert_eq!(lock(&server.engine.store).unwrap().list().len(), 2);
}
