//! Original protected API and pure/offline CLI reviews.
mod api_support;
mod irc_support;
mod library_support;
use api_support::{Server, TOKEN};
use irc_support::*;
use library_support::Directory;
use mynou::json::{self, Value};
fn post(server: &Server, route: &str, value: &Value) -> api_support::Reply {
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
fn protected_api_follows_a_chronological_cursor() {
    let dir = Directory::new();
    let cfg = config(&dir.0);
    let server = Server::open(cfg.clone());
    for n in 1..=3 {
        receive(&server.engine, n);
    }
    let get = |route: &str| {
        server.call(
            "GET",
            route,
            &[("Authorization", &format!("Bearer {TOKEN}"))],
            "",
        )
    };
    let first = get("/api/irc/announcements?limit=1");
    assert_eq!(first.status, 200);
    let first = json::parse(&first.body).unwrap();
    let cursor = first
        .get("next_cursor")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(first.get("next"), first.get("next_cursor"));
    let second = get(&format!("/api/irc/announcements?after={cursor}&limit=1"));
    assert_eq!(second.status, 200);
    let second = json::parse(&second.body).unwrap();
    assert_ne!(first.get("announcements"), second.get("announcements"));
    assert_eq!(
        get(&format!("/api/irc/announcements?after={cursor}&offset=0")).status,
        400
    );
    assert_eq!(get("/api/irc/announcements?after=broken").status, 400);
}
