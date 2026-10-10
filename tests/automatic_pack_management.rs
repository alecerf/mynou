//! Protected automatic pack previews and acquisition through native HTTP/forms/CLI.
mod automatic_pack_support;
mod library_support;
mod series_support;
#[allow(dead_code)]
mod transfer_support;
mod api_support;
use automatic_pack_support::*;
use library_support::Directory;
use mynou::{
    config::Config,
    engine::{Engine, lock},
    json::{self, Value},
};
use series_support::{Catalog, episode, id, request};
use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use transfer_support::{BLOCK, Torrent, payload};
use api_support::{Server, TOKEN};

fn setup(directory: &Directory, provider: &Provider) -> (Catalog, Config) {
    let catalog = Catalog::open(vec![
        episode(1, 1, Some("2024-01-01"), "<img src=x>"),
        episode(1, 2, Some("2024-01-02"), "Second"),
    ]);
    let torrent = Torrent::multiple(
        &directory.0.join("metadata"),
        "Pack",
        vec![
            ("S01E01.mp4".into(), payload(BLOCK, 11)),
            ("S01E02.mp4".into(), payload(BLOCK, 17)),
        ],
    );
    let url = provider.metadata_url("controls", fs::read(&torrent.path).unwrap());
    provider.releases(vec![release("Fixture.Series.S01.1080p.<script>", &url, 1)]);
    let mut cfg = configure(catalog.config(&directory.0.join("engine")), provider, "{}");
    cfg.sources[0].name = "<script>fixture</script>".into();
    (catalog, cfg)
}
fn tracked(engine: &Engine) -> String {
    id(&engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap())
    .into()
}

#[test]
fn bearer_pack_routes_validate_before_network_io_and_keep_origin_private_data_redacted() {
    let directory = Directory::new();
    let provider = Provider::open();
    let (_catalog, cfg) = setup(&directory, &provider);
    let server = Server::open(cfg);
    let series = tracked(&server.engine);
    let route = format!("/api/series/{series}/pack-search");
    assert_eq!(
        server.call("POST", &route, &[], r#"{"season":1}"#).status,
        401
    );
    let auth = format!("Bearer {TOKEN}");
    let headers = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
    ];
    let before = provider.calls.lock().unwrap().len();
    for body in [
        r#"{}"#,
        r#"{"season":1.5}"#,
        r#"{"season":1,"apply":"true"}"#,
        r#"{"season":1,"unknown":true}"#,
        r#"{"season":1,"candidate_id":"wrong","apply":true}"#,
    ] {
        let reply = server.call("POST", &route, &headers, body);
        assert_eq!(reply.status, 400, "{}", reply.body);
    }
    assert_eq!(provider.calls.lock().unwrap().len(), before);
    assert!(lock(&server.engine.store).unwrap().list().is_empty());
    let response = server.call("POST", &route, &headers, r#"{"season":1}"#);
    assert_eq!(response.status, 200, "{}", response.body);
    let report = json::parse(&response.body).unwrap();
    no_sources(&report);
    let mut body = Value::object();
    body.insert("season", 1_u32);
    body.insert("apply", true);
    body.insert(
        "candidate_id",
        report.get("selected_candidate_id").unwrap().clone(),
    );
    body.insert("scope_id", report.get("scope_id").unwrap().clone());
    let response = server.call("POST", &route, &headers, &json::stringify(&body));
    assert_eq!(response.status, 200, "{}", response.body);
    let applied = json::parse(&response.body).unwrap();
    no_sources(&applied);
    let jobs = lock(&server.engine.store).unwrap().list();
    assert_eq!(jobs.len(), 2);
    assert!(
        jobs.iter()
            .all(|job| job.pack_origin.is_some() && job.release.is_none())
    );
    for job in jobs {
        assert!(!json::stringify(&mynou::engine::public_job(&job)).contains(SECRET));
    }
}

fn cli_config(config: &Config) -> Value {
    let mut value = mynou::config::default_json();
    value.insert("listen", config.listen.clone());
    value.insert("store_dir", config.store_dir.to_str().unwrap());
    value.insert("selection", config.selection.to_json());
    value.get_mut("downloads").unwrap().insert("enabled", false);
    let catalog = value.get_mut("catalog").unwrap();
    catalog.insert("enabled", config.catalog.enabled);
    catalog.insert("url", config.catalog.url.clone());
    catalog.insert("token_env", config.catalog.token_env.clone());
    catalog.insert("api_key_env", config.catalog.api_key_env.clone());
    value.insert(
        "indexers",
        Value::Array(
            config
                .sources
                .iter()
                .map(|source| {
                    let mut item = Value::object();
                    item.insert("name", source.name.clone());
                    item.insert("kind", source.kind.clone());
                    item.insert("url", source.url.clone());
                    item.insert("api_key_env", source.api_key_env.clone());
                    item
                })
                .collect(),
        ),
    );
    value
}

fn command(directory: &Path, arguments: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(directory)
        .env_remove("MYNOU_API_TOKEN")
        .env_remove("MYNOU_PLEX_TOKEN")
        .env_remove("MYNOU_DEMO_TOKEN")
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
            let result = child.wait_with_output().unwrap();
            panic!(
                "Automatic pack CLI timed out: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    json::parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap()
}

#[test]
fn offline_cli_preview_is_read_only_and_online_guarded_apply_uses_the_live_service() {
    let directory = Directory::new();
    let provider = Provider::open();
    let (_catalog, mut cfg) = setup(&directory, &provider);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let series = tracked(&engine);
    drop(engine);
    let config_path = directory.0.join("mynou.json");
    fs::write(&config_path, json::stringify(&cli_config(&cfg))).unwrap();
    let file = config_path.to_str().unwrap();
    let before = snapshot(&cfg.store_dir);
    let report = success(command(
        &directory.0,
        &[
            "series-pack-search",
            &series,
            "--season",
            "1",
            "--config",
            file,
        ],
    ));
    no_sources(&report);
    assert_eq!(snapshot(&cfg.store_dir), before);
    let calls = provider.calls.lock().unwrap().len();
    for arguments in [
        vec!["series-pack-search", &series, "--config", file],
        vec![
            "series-pack-search",
            &series,
            "--season",
            "1.5",
            "--config",
            file,
        ],
        vec![
            "series-pack-search",
            &series,
            "--season",
            "1",
            "--apply",
            "--config",
            file,
        ],
        vec![
            "series-pack-search",
            &series,
            "--season",
            "1",
            "--candidate-id",
            "abc",
            "--config",
            file,
        ],
    ] {
        assert!(!command(&directory.0, &arguments).status.success());
    }
    assert_eq!(provider.calls.lock().unwrap().len(), calls);
    assert_eq!(snapshot(&cfg.store_dir), before);
    let server = Server::open(cfg.clone());
    cfg.listen = server.authority.clone();
    fs::write(&config_path, json::stringify(&cli_config(&cfg))).unwrap();
    fs::write(
        directory.0.join(".env"),
        format!("{}={TOKEN}\n", cfg.api_token_env),
    )
    .unwrap();
    let online = success(command(
        &directory.0,
        &[
            "series-pack-search",
            &series,
            "--season",
            "1",
            "--config",
            file,
        ],
    ));
    assert_eq!(
        online.get("selected_candidate_id"),
        report.get("selected_candidate_id")
    );
    let scope = online.get("scope_id").unwrap().as_str().unwrap();
    let candidate = online
        .get("selected_candidate_id")
        .unwrap()
        .as_str()
        .unwrap();
    let applied = success(command(
        &directory.0,
        &[
            "series-pack-search",
            &series,
            "--season",
            "1",
            "--apply",
            "--scope-id",
            scope,
            "--candidate-id",
            candidate,
            "--config",
            file,
        ],
    ));
    no_sources(&applied);
    assert_eq!(
        applied.get("submission").unwrap().get("submitted"),
        Some(&Value::Number(2.0))
    );
}
