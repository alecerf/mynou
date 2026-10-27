//! Protected automatic pack previews and acquisition through native HTTP/forms/CLI.
mod api_support;
mod automatic_pack_support;
mod library_support;
mod series_support;
#[allow(dead_code)]
mod transfer_support;
use api_support::{Server, TOKEN};
use automatic_pack_support::*;
use library_support::Directory;
use mynou::{
    config::Config,
    engine::{Engine, lock},
    json::{self, Value},
};
use series_support::{Catalog, episode, id, request};
use std::fs;
use transfer_support::{BLOCK, Torrent, payload};

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
    assert!(
        lock(&server.engine.store)
            .unwrap()
            .list()
            .unwrap()
            .is_empty()
    );
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
    let jobs = lock(&server.engine.store).unwrap().list().unwrap();
    assert_eq!(jobs.len(), 2);
    assert!(
        jobs.iter()
            .all(|job| job.pack_origin.is_some() && job.release.is_none())
    );
    for job in jobs {
        assert!(!json::stringify(&mynou::engine::public_job(&job)).contains(SECRET));
    }
}
