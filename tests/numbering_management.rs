//! Original protected API and CLI numbering workflows. CI only.
mod api_support;
mod library_support;
mod series_support;
use api_support::{Reply, Server, TOKEN};
use library_support::Directory;
use mynou::json::{self, Value};
use series_support::{Catalog, episode, id, request};

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
