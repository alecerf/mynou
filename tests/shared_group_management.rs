//! CI-only protected whole-group JSON, form and CLI controls.
mod api_support;
mod automatic_pack_support;
mod library_support;
mod series_support;
mod shared_group_support;
#[allow(dead_code)]
mod transfer_support;
use api_support::{Server, TOKEN};
use automatic_pack_support::{Provider, SECRET, no_sources, snapshot};
use library_support::Directory;
use mynou::{
    engine::lock,
    json::{self},
};
use shared_group_support::{Fixture, baseline_query};
use std::fs;

fn headers() -> [(String, String); 2] {
    [
        ("Authorization".into(), format!("Bearer {TOKEN}")),
        ("Content-Type".into(), "application/json".into()),
    ]
}

#[test]
fn authenticated_group_routes_require_complete_scope_and_guarded_quality_decisions() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let provider = Provider::open();
    let server = Server::open(fixture.cfg.clone());
    let parents = fixture.parents(&server.engine);
    let route = format!("/api/library/{}/group", parents[0].id);
    let headers = headers();
    let auth: Vec<_> = headers
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    for patch in [
        r#"{"apply":true}"#,
        r#"{"plan_id":"bad"}"#,
        r#"{"owners":[1]}"#,
        r#"{"source_url":"private"}"#,
        r#"{"action":"promote"}"#,
    ] {
        let mut body = baseline_query().to_json();
        for (key, value) in json::parse(patch).unwrap().as_object().unwrap() {
            body.insert(key, value.clone());
        }
        let reply = server.call("POST", &route, &auth, &json::stringify(&body));
        assert_eq!(reply.status, 400, "{}", reply.body);
    }
    assert!(provider.calls.lock().unwrap().is_empty());
    let before = snapshot(&fixture.cfg.store_dir);
    let reply = server.call(
        "POST",
        &route,
        &auth,
        &json::stringify(&baseline_query().to_json()),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(snapshot(&fixture.cfg.store_dir), before);
    let preview = json::parse(&reply.body).unwrap();
    no_sources(&preview);
    let mut body = baseline_query().to_json();
    body.insert("apply", true);
    body.insert("plan_id", preview.get("plan_id").unwrap().clone());
    assert_eq!(
        server
            .call("POST", &route, &auth, &json::stringify(&body))
            .status,
        200
    );
    assert!(
        lock(&server.engine.store)
            .unwrap()
            .shared_group(&parents[0].id)
            .unwrap()
            .iter()
            .all(|job| job.release.is_some())
    );
    let mut query = fixture.replacement_query();
    query.source_url =
        Some(provider.metadata_url("replacement", fs::read(&fixture.replacement.path).unwrap()));
    let preview = server.call("POST", &route, &auth, &json::stringify(&query.to_json()));
    assert_eq!(preview.status, 200, "{}", preview.body);
    assert!(!preview.body.contains(SECRET));
    let preview = json::parse(&preview.body).unwrap();
    no_sources(&preview);
    query.apply = true;
    query.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    let reply = server.call("POST", &route, &auth, &json::stringify(&query.to_json()));
    assert_eq!(reply.status, 200, "{}", reply.body);
    no_sources(&json::parse(&reply.body).unwrap());
    assert_eq!(lock(&server.engine.store).unwrap().list().unwrap().len(), 4);
    assert_eq!(
        server
            .call(
                "POST",
                &format!("/api/library/{}/baseline", parents[0].id),
                &auth,
                r#"{"release_title":"Fixture.Series.S01E01.720p.WEB-DL"}"#
            )
            .status,
        400
    );
}
