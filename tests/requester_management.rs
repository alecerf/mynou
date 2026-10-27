//! Original Bearer API and CLI requester scenarios.
mod api_support;
mod library_support;
mod requester_support;
use api_support::{Server, TOKEN};
use library_support::Directory;
use mynou::json::{self, Value};
use requester_support::*;
use std::{collections::BTreeMap, fs};
fn authenticated(server: &Server, route: &str, v: &Value) -> api_support::Reply {
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
