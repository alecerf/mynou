//! Original protected API and CLI numbering workflows. CI only.
mod api_support;
mod library_support;
mod series_support;
use api_support::{Reply, Server, TOKEN};
use library_support::Directory;
use mynou::json::{self, Value};
use series_support::{Catalog, episode, id, request};
use std::{
    fs,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn body() -> Value {
    json::parse(r#"{"changes":[{"catalog_id":11001,"catalog":{"season":1,"episode":1},"source":{"absolute":13}}]}"#).unwrap()
}
fn api(server: &Server, id: &str, body: &Value, authorized: bool) -> Reply {
    let bearer = format!("Bearer {TOKEN}");
    let mut headers = vec![("Content-Type", "application/json")];
    if authorized {
        headers.push(("Authorization", &bearer));
    }
    server.call(
        "POST",
        &format!("/api/series/{id}/numbering"),
        &headers,
        &json::stringify(body),
    )
}

#[test]
fn numbering_api_requires_authentication_and_the_reviewed_plan() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let server = Server::open(catalog.config(&directory.0));
    let record = server
        .engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let id = id(&record);
    let before = server.engine.series_record(id).unwrap();
    assert_eq!(api(&server, id, &body(), false).status, 401);
    for invalid in [
        r#"{"changes":[],"apply":true}"#,
        r#"{"changes":[],"unknown":true}"#,
        r#"{"changes":[{"catalog_id":999,"catalog":{"season":1,"episode":1},"source":{"absolute":13}}]}"#,
    ] {
        assert_eq!(
            api(&server, id, &json::parse(invalid).unwrap(), true).status,
            400
        );
    }
    assert_eq!(server.engine.series_record(id).unwrap(), before);
    let preview = api(&server, id, &body(), true);
    assert_eq!(preview.status, 200, "{}", preview.body);
    preview.no_secrets();
    assert_eq!(server.engine.series_record(id).unwrap(), before);
    let report = json::parse(&preview.body).unwrap();
    let mut apply = body();
    apply.insert("apply", true);
    apply.insert("plan_id", report.get("plan_id").unwrap().clone());
    let accepted = api(&server, id, &apply, true);
    assert_eq!(accepted.status, 200, "{}", accepted.body);
    assert_eq!(
        json::parse(&accepted.body).unwrap().get("applied"),
        Some(&Value::Bool(true))
    );
    assert_eq!(api(&server, id, &apply, true).status, 400);
    assert_eq!(
        server
            .engine
            .series_record(id)
            .unwrap()
            .get("numbering")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(!preview.body.contains(&catalog.url));
}

fn command(directory: &Directory, args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(&directory.0)
        .env_remove("MYNOU_API_TOKEN")
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
            panic!("Numbering CLI fixture timed out");
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn parse_output(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::str::from_utf8(&output.stdout).unwrap();
    assert!(!text.contains(TOKEN));
    json::parse(text).unwrap()
}

#[test]
fn cli_numbering_previews_applies_and_rejects_stale_or_unreviewed_changes() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let server = Server::open(catalog.config(&directory.0));
    let record = server
        .engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let id = id(&record);
    let mut cfg = mynou::config::default_json();
    cfg.insert("listen", server.authority.clone());
    fs::write(directory.0.join("mynou.json"), json::stringify(&cfg)).unwrap();
    fs::write(
        directory.0.join(".env"),
        format!("MYNOU_API_TOKEN={TOKEN}\n"),
    )
    .unwrap();
    fs::write(directory.0.join("numbering.json"), json::stringify(&body())).unwrap();
    let initial = server.engine.series_record(id).unwrap();
    let report = parse_output(&command(
        &directory,
        &["series-numbering", id, "--mapping", "numbering.json"],
    ));
    assert_eq!(server.engine.series_record(id).unwrap(), initial);
    assert!(
        !command(
            &directory,
            &[
                "series-numbering",
                id,
                "--mapping",
                "numbering.json",
                "--apply"
            ]
        )
        .status
        .success()
    );
    let plan = report.get("plan_id").unwrap().as_str().unwrap();
    let applied = command(
        &directory,
        &[
            "series-numbering",
            id,
            "--mapping",
            "numbering.json",
            "--apply",
            "--plan-id",
            plan,
        ],
    );
    assert_eq!(
        parse_output(&applied).get("applied"),
        Some(&Value::Bool(true))
    );
    assert!(
        !command(
            &directory,
            &[
                "series-numbering",
                id,
                "--mapping",
                "numbering.json",
                "--apply",
                "--plan-id",
                plan
            ]
        )
        .status
        .success()
    );
}

#[test]
fn offline_cli_numbering_preview_never_changes_old_storage_or_creates_new_storage() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let cfg = catalog.config(&directory.0);
    let engine = mynou::engine::Engine::open_for_management(cfg.clone()).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let id = id(&record).to_owned();
    drop(engine);
    let mut value = mynou::config::default_json();
    let catalog_config = value.get_mut("catalog").unwrap();
    catalog_config.insert("enabled", true);
    catalog_config.insert("url", format!("{}/3", catalog.url));
    catalog_config.insert("token_env", "PATH");
    catalog_config.insert("api_key_env", "MYNOU_NUMBERING_ABSENT_CATALOG_919");
    fs::write(directory.0.join("mynou.json"), json::stringify(&value)).unwrap();
    fs::write(directory.0.join("numbering.json"), json::stringify(&body())).unwrap();
    let before = library_support::files(&cfg.store_dir);
    let report = parse_output(&command(
        &directory,
        &["series-numbering", &id, "--mapping", "numbering.json"],
    ));
    assert_eq!(report.get("resolved"), Some(&Value::Bool(true)));
    assert_eq!(library_support::files(&cfg.store_dir), before);
    let plan = report.get("plan_id").unwrap().as_str().unwrap();
    assert!(
        !command(
            &directory,
            &[
                "series-numbering",
                &id,
                "--mapping",
                "numbering.json",
                "--apply",
                "--plan-id",
                plan
            ]
        )
        .status
        .success()
    );
    assert_eq!(library_support::files(&cfg.store_dir), before);
    let fresh = Directory::new();
    fs::write(fresh.0.join("mynou.json"), json::stringify(&value)).unwrap();
    assert!(!command(&fresh, &["series-numbering", &id]).status.success());
    assert!(!fresh.0.join("state").exists());
}
