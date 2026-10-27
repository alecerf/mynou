//! Protected shared-file previews and guarded application through HTTP, forms and CLI.
mod api_support;
mod automatic_pack_support;
mod library_support;
mod series_support;
#[allow(dead_code)]
mod transfer_support;
use api_support::{Server, TOKEN};
use automatic_pack_support::{Provider, SECRET, no_sources};
use library_support::Directory;
use mynou::{
    config::Config,
    engine::{Engine, lock},
    json::{self, Value},
    pack::SharedFileRequest,
};
use series_support::{Catalog, episode, id, request};
use std::fs;
use transfer_support::{BLOCK, Torrent, payload};

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
    assert!(
        lock(&server.engine.store)
            .unwrap()
            .list()
            .unwrap()
            .is_empty()
    );
    let mut value = body(&source);
    value.insert("apply", true);
    value.insert("plan_id", preview.get("plan_id").unwrap().clone());
    let reply = server.call("POST", &route, &headers, &json::stringify(&value));
    assert_eq!(reply.status, 200, "{}", reply.body);
    no_sources(&json::parse(&reply.body).unwrap());
    let owners = lock(&server.engine.store).unwrap().list().unwrap();
    assert_eq!(owners.len(), 2);
    assert_eq!(owners[0].shared_file, owners[1].shared_file);
    assert!(
        owners
            .iter()
            .all(|j| !json::stringify(&mynou::engine::public_job(j)).contains(SECRET))
    );
}
