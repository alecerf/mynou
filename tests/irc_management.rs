//! Original protected API and pure/offline CLI reviews.
mod irc_support;
mod library_support;
mod web_support;
use irc_support::*;
use library_support::Directory;
use mynou::{
    config,
    engine::Engine,
    json::{self, Value},
};
use std::{fs, process::Command};
use web_support::{Server, TOKEN};
fn post(server: &Server, route: &str, value: &Value) -> web_support::Reply {
    server.call(
        "POST",
        route,
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", "application/json"),
        ],
        &json::stringify(value),
    )
}
#[test]
fn api_requires_authentication_strict_fields_and_current_guards() {
    let dir = Directory::new();
    let server = Server::open(config(&dir.0));
    let r = receive(&server.engine, 7);
    let id = record_id(&r);
    let route = format!("/api/irc/announcements/{id}/control");
    assert_eq!(server.call("GET", "/api/irc", &[], "").status, 401);
    assert_eq!(server.call("POST", &route, &[], "{}").status, 401);
    for patch in [
        r#"{"action":"grab"}"#,
        r#"{"action":"dismiss","token":"private"}"#,
        r#"{"action":"dismiss","apply":true}"#,
        r#"{"action":"dismiss","plan_id":"bad"}"#,
    ] {
        assert_eq!(
            post(&server, &route, &json::parse(patch).unwrap()).status,
            400
        );
    }
    let before = bytes(&server.engine.config.store_dir);
    let preview = post(&server, &route, &query("dismiss").to_json());
    assert_eq!(preview.status, 200, "{}", preview.body);
    preview.no_secrets();
    no_credentials(&json::parse(&preview.body).unwrap());
    assert_eq!(bytes(&server.engine.config.store_dir), before);
    let q = reviewed(&server.engine, id, "dismiss");
    assert_eq!(post(&server, &route, &q.to_json()).status, 200);
    assert_eq!(post(&server, &route, &q.to_json()).status, 400);
    for query in [
        "limit=0",
        "limit=201",
        "offset=1001",
        "limit=1&limit=2",
        "unknown=x",
    ] {
        assert_eq!(
            server
                .call(
                    "GET",
                    &format!("/api/irc/announcements?{query}"),
                    &[("Authorization", &format!("Bearer {TOKEN}"))],
                    ""
                )
                .status,
            400
        );
    }
    let mut v = Value::object();
    v.insert("source_id", "local");
    v.insert("announcement", announcement(8));
    let before = bytes(&server.engine.config.store_dir);
    let reply = post(&server, "/api/irc/preview", &v);
    assert_eq!(reply.status, 200);
    assert_eq!(bytes(&server.engine.config.store_dir), before);
    no_credentials(&json::parse(&reply.body).unwrap());
    no_jobs(&server.engine);
}
#[test]
fn cli_preview_is_pure_and_offline_apply_requires_the_running_service() {
    let dir = Directory::new();
    let config_file = dir.0.join("mynou.json");
    let mut v = value(&dir.0, "irc://127.0.0.1:1");
    v.insert("listen", "127.0.0.1:1");
    fs::write(&config_file, json::stringify(&v)).unwrap();
    let payload = dir.0.join("announcement.json");
    fs::write(&payload, json::stringify(&announcement(7))).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args(["irc-preview", "local", "--announcement"])
        .arg(&payload)
        .arg("--config")
        .arg(&config_file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.0.join("jobs").exists());
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let row = receive(&engine, 7);
    let id = record_id(&row).to_owned();
    let plan = engine
        .irc_control(&id, &query("dismiss"))
        .unwrap()
        .get("plan_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    drop(engine);
    let before = bytes(&cfg.store_dir);
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args(["irc-control", &id, "--action", "dismiss", "--config"])
        .arg(&config_file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        json::parse(std::str::from_utf8(&out.stdout).unwrap())
            .unwrap()
            .get("plan_id")
            .unwrap()
            .as_str(),
        Some(plan.as_str())
    );
    assert_eq!(bytes(&cfg.store_dir), before);
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args([
            "irc-control",
            &id,
            "--action",
            "dismiss",
            "--apply",
            "--plan-id",
            &plan,
            "--config",
        ])
        .arg(&config_file)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("running Mynou"));
    assert_eq!(bytes(&cfg.store_dir), before);
    let mut cfg = cfg;
    cfg.listen = "127.0.0.1:0".into();
    let server = Server::open(cfg);
    v.insert("listen", server.authority.clone());
    fs::write(&config_file, json::stringify(&v)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .env("MYNOU_API_TOKEN", TOKEN)
        .args([
            "irc-control",
            &id,
            "--action",
            "dismiss",
            "--apply",
            "--plan-id",
            &plan,
            "--config",
        ])
        .arg(&config_file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        server
            .engine
            .irc_announcement(&id)
            .unwrap()
            .get("decision")
            .unwrap()
            .as_str(),
        Some("dismissed")
    );
    no_jobs(&server.engine);
}
