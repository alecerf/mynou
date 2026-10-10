//! Original Bearer API and CLI requester scenarios.
mod library_support;
mod requester_support;
mod web_support;
use library_support::Directory;
use mynou::{
    config,
    engine::Engine,
    json::{self, Value},
};
use requester_support::*;
use std::{
    collections::BTreeMap,
    fs,
    process::{Command, Stdio},
};
use web_support::{Server, TOKEN};
fn authenticated(server: &Server, route: &str, v: &Value) -> web_support::Reply {
    server.call(
        "POST",
        route,
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", "application/json"),
        ],
        &json::stringify(v),
    )
}
fn snapshot(root: &std::path::Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    fs::read_dir(root)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            p.is_file().then(|| (p.clone(), fs::read(p).unwrap()))
        })
        .collect()
}
#[test]
fn requester_api_requires_authentication_strict_scopes_and_current_guards() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let server = Server::open(accounts.config(&directory.0));
    let q = policy_query(policy(&server.engine, "alice"));
    let route = "/api/requesters/alice/control";
    assert_eq!(
        server
            .call("POST", route, &[], &json::stringify(&q.to_json()))
            .status,
        401
    );
    for patch in [
        r#"{"apply":true}"#,
        r#"{"plan_id":"bad"}"#,
        r#"{"token":"private"}"#,
        r#"{"action":"promote"}"#,
        r#"{"demand_id":"private"}"#,
    ] {
        let mut v = q.to_json();
        for (k, val) in json::parse(patch).unwrap().as_object().unwrap() {
            v.insert(k, val.clone());
        }
        assert_eq!(authenticated(&server, route, &v).status, 400);
    }
    let before = snapshot(&server.engine.config.store_dir);
    let preview = authenticated(&server, route, &q.to_json());
    assert_eq!(preview.status, 200, "{}", preview.body);
    assert_eq!(snapshot(&server.engine.config.store_dir), before);
    no_credentials(&json::parse(&preview.body).unwrap());
    enable(&server.engine, "alice");
    server.engine.sync_requesters().unwrap();
    let d = demand(&server.engine, "alice");
    assert_eq!(
        authenticated(
            &server,
            "/api/requesters/bob/control",
            &demand_query("remove", id(&d)).to_json()
        )
        .status,
        400
    );
    let q = demand_query("remove", id(&d));
    let response = authenticated(&server, route, &q.to_json());
    let mut applied = q.to_json();
    applied.insert("apply", true);
    applied.insert(
        "plan_id",
        json::parse(&response.body)
            .unwrap()
            .get("plan_id")
            .unwrap()
            .clone(),
    );
    assert_eq!(authenticated(&server, route, &applied).status, 200);
    assert_eq!(authenticated(&server, route, &applied).status, 400);
    assert_eq!(
        server
            .call(
                "GET",
                "/api/requesters/alice?limit=1&limit=2",
                &[("Authorization", &format!("Bearer {TOKEN}"))],
                ""
            )
            .status,
        400
    );
    let listing = server.call(
        "GET",
        "/api/requesters/alice?offset=0&limit=1",
        &[("Authorization", &format!("Bearer {TOKEN}"))],
        "",
    );
    assert_eq!(listing.status, 200);
    no_credentials(&json::parse(&listing.body).unwrap());
}
fn config_file(cfg: &mynou::config::Config, file: &std::path::Path) {
    let mut value = config::default_json();
    value.insert("listen", cfg.listen.clone());
    value.insert("store_dir", cfg.store_dir.to_str().unwrap());
    value.get_mut("downloads").unwrap().insert("enabled", false);
    value
        .get_mut("library")
        .unwrap()
        .insert("movies_root", cfg.movies_root.to_str().unwrap());
    value
        .get_mut("library")
        .unwrap()
        .insert("series_root", cfg.series_root.to_str().unwrap());
    let mut requesters = Value::object();
    requesters.insert(
        "accounts",
        Value::Array(
            cfg.requesters
                .accounts
                .iter()
                .map(|a| {
                    let mut v = Value::object();
                    v.insert("id", a.id.clone());
                    v.insert("expected_user_id", a.expected_user_id.clone());
                    v.insert("token_env", a.token_env.clone());
                    v.insert("identity_url", a.identity_url.clone());
                    v.insert("watchlist_url", a.watchlist_url.clone());
                    v
                })
                .collect(),
        ),
    );
    requesters.insert(
        "destinations",
        Value::Array(
            cfg.requesters
                .destinations
                .iter()
                .map(|d| {
                    let mut v = Value::object();
                    v.insert("id", d.id.clone());
                    v.insert("movies_root", d.movies_root.to_str().unwrap());
                    v.insert("series_root", d.series_root.to_str().unwrap());
                    v
                })
                .collect(),
        ),
    );
    value.insert("requesters", requesters);
    fs::write(file, json::stringify(&value)).unwrap();
}
#[test]
fn cli_offline_preview_is_read_only_and_online_apply_uses_the_same_guard() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let mut cfg = accounts.config(&directory.0);
    cfg.listen = "127.0.0.1:0".into();
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let mut p = policy(&engine, "alice");
    p.enabled = true;
    let q = policy_query(p);
    drop(engine);
    let config_path = directory.0.join("mynou.json");
    config_file(&cfg, &config_path);
    let mapping = directory.0.join("policy.json");
    let mut v = q.to_json();
    if let Value::Object(m) = &mut v {
        m.remove("apply");
    }
    fs::write(&mapping, json::stringify(&v)).unwrap();
    let command = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_mynou"))
            .args(args)
            .arg("--config")
            .arg(&config_path)
            .env("MYNOU_API_TOKEN", TOKEN)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    };
    let before = snapshot(&cfg.store_dir);
    let output = command(&[
        "requester-control",
        "alice",
        "--mapping",
        mapping.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(snapshot(&cfg.store_dir), before);
    let offline = json::parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    let guard = offline.get("plan_id").unwrap().as_str().unwrap();
    assert!(
        !command(&[
            "requester-control",
            "alice",
            "--mapping",
            mapping.to_str().unwrap(),
            "--apply",
            "--plan-id",
            guard
        ])
        .status
        .success()
    );
    assert_eq!(snapshot(&cfg.store_dir), before);
    let server = Server::open(cfg);
    let mut online_cfg = server.engine.config.clone();
    online_cfg.listen = server.authority.clone();
    config_file(&online_cfg, &config_path);
    let online = authenticated(&server, "/api/requesters/alice/control", &q.to_json());
    assert_eq!(
        json::parse(&online.body).unwrap().get("plan_id"),
        offline.get("plan_id")
    );
    let output = command(&[
        "requester-control",
        "alice",
        "--mapping",
        mapping.to_str().unwrap(),
        "--apply",
        "--plan-id",
        guard,
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(policy(&server.engine, "alice").enabled);
}
