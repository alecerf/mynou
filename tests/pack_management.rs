//! Bearer pack journeys; original local catalog fixtures, CI only.
mod library_support;
mod series_support;
mod web_support;
use library_support::Directory;
use mynou::{
    engine::lock,
    json::{self, Value},
};
use series_support::{Catalog, episode, id, request};
use web_support::{Server, TOKEN};

fn catalog() -> Catalog {
    Catalog::open(vec![
        episode(1, 1, Some("2024-01-01"), "First"),
        episode(1, 2, Some("2024-01-02"), "Second"),
    ])
}
const EPISODES: &str = r#"[{"season":1,"episode":1,"file_path":"Pack/001.mp4"},{"season":1,"episode":2,"file_path":"Pack/002.mp4"}]"#;
const SOURCE: &str = "https://provider.invalid/pack.torrent?token=library-download-fixture-secret";

#[test]
fn bearer_pack_api_checks_entire_scope_and_redacts_source_urls() {
    let directory = Directory::new();
    let catalog = catalog();
    let server = Server::open(catalog.config(&directory.0));
    let body = json::stringify(&json::parse(r#"{"request":{"kind":"series","title":"Fixture Series","tmdb_id":42},"enabled":false}"#).unwrap());
    let authorization = format!("Bearer {TOKEN}");
    let headers = [
        ("Authorization", authorization.as_str()),
        ("Content-Type", "application/json"),
    ];
    let created = server.call("POST", "/api/series", &headers, &body);
    assert_eq!(created.status, 201);
    let record = json::parse(&created.body).unwrap();
    assert_eq!(record.get("monitored"), Some(&Value::Bool(false)));
    assert!(lock(&server.engine.store).unwrap().list().is_empty());
    let route = format!("/api/series/{}/packs", id(&record));
    let mut payload = Value::object();
    payload.insert("source_url", SOURCE);
    payload.insert("episodes", json::parse(EPISODES).unwrap());
    let valid = json::stringify(&payload);
    assert_eq!(
        server
            .call(
                "POST",
                &route,
                &[("Content-Type", "application/json")],
                &valid
            )
            .status,
        401
    );
    let bad = valid.replace("\"episode\":2", "\"episode\":99");
    assert_eq!(server.call("POST", &route, &headers, &bad).status, 400);
    assert!(lock(&server.engine.store).unwrap().list().is_empty());
    let reply = server.call("POST", &route, &headers, &valid);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(!reply.body.contains(TOKEN));
    assert!(!reply.body.contains("library-download-fixture-secret"));
    assert_eq!(
        json::parse(&reply.body)
            .unwrap()
            .get("jobs")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("request")
            .unwrap()
            .get("source_url"),
        Some(&Value::String("[configured source]".into()))
    );
    assert!(!reply.body.contains("provider.invalid"));
    assert_eq!(
        json::parse(&reply.body).unwrap().get("submitted"),
        Some(&Value::Number(2.0))
    );
    assert_eq!(
        json::parse(&server.call("POST", &route, &headers, &valid).body)
            .unwrap()
            .get("reused"),
        Some(&Value::Number(2.0))
    );
    assert_eq!(lock(&server.engine.store).unwrap().list().len(), 2);
}

#[test]
fn pack_source_key_collisions_between_series_are_rejected_without_partial_writes() {
    let directory = Directory::new();
    let catalog = catalog();
    let server = Server::open(catalog.config(&directory.0));
    let record = server
        .engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let mut foreign = request();
    foreign.kind = "episode".into();
    foreign.season = 1;
    foreign.episode = 2;
    foreign.tmdb_id = Some(43);
    foreign.source_url = Some(SOURCE.into());
    server.engine.submit(foreign).unwrap();
    let mut body = Value::object();
    body.insert("source_url", SOURCE);
    body.insert("episodes", json::parse(EPISODES).unwrap());
    let pack = mynou::pack::PackSubmission::from_json(&body).unwrap();
    assert!(
        server
            .engine
            .submit_pack(id(&record), &pack)
            .unwrap_err()
            .contains("conflicts")
    );
    assert_eq!(lock(&server.engine.store).unwrap().list().len(), 1);
}
