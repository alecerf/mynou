//! API and CLI against synthetic catalog data. CI only.
mod api_support;
mod library_support;
mod series_support;
use api_support::{Reply, Server, TOKEN};
use library_support::Directory;
use mynou::{
    engine::lock,
    json::{self, Value},
};
use series_support::{Catalog, episode, id, request};

fn api(
    server: &Server,
    method: &str,
    route: &str,
    body: Option<&Value>,
    authenticated: bool,
) -> Reply {
    let bearer = format!("Bearer {TOKEN}");
    let mut headers = vec![("Content-Type", "application/json")];
    if authenticated {
        headers.push(("Authorization", &bearer));
    }
    server.call(
        method,
        route,
        &headers,
        &body.map_or_else(String::new, json::stringify),
    )
}
fn parse(reply: &Reply) -> Value {
    json::parse(&reply.body).unwrap()
}
fn create() -> Value {
    let mut value = Value::object();
    value.insert("request", request().to_json());
    value.insert("future_only", true);
    value
}

#[test]
fn series_api_requires_bearer_and_validates_settings_before_changing_records() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let server = Server::open(catalog.config(&directory.0));
    let created = api(&server, "POST", "/api/series", Some(&create()), true);
    assert_eq!(created.status, 201, "{}", created.body);
    let record = parse(&created);
    let id = id(&record);
    let original = server.engine.series_record(id).unwrap();
    for (operation, body) in [
        ("monitor", r#"{"enabled":"true"}"#),
        ("monitor", r#"{"start_date":"2024-02-30"}"#),
        ("monitor", r#"{"enabled":false,"unknown":true}"#),
        ("monitor", "{}"),
        ("episodes", r#"{"season":1,"episode":999,"enabled":false}"#),
        ("episodes", r#"{"season":1.5,"episode":1,"enabled":true}"#),
        ("refresh", r#"{"unknown":true}"#),
    ] {
        let reply = api(
            &server,
            "POST",
            &format!("/api/series/{id}/{operation}"),
            Some(&json::parse(body).unwrap()),
            true,
        );
        assert_eq!(reply.status, 400);
        assert_eq!(server.engine.series_record(id).unwrap(), original);
    }
    let updated = api(
        &server,
        "POST",
        &format!("/api/series/{id}/monitor"),
        Some(&json::parse(r#"{"enabled":false}"#).unwrap()),
        true,
    );
    assert_eq!(updated.status, 200);
    assert_eq!(parse(&updated).get("monitored"), Some(&Value::Bool(false)));
    assert!(
        lock(&server.engine.store)
            .unwrap()
            .list()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn series_and_calendar_pages_follow_cursors_and_reject_inconsistent_queries() {
    let directory = Directory::new();
    let catalog = Catalog::open(
        (1..=80)
            .map(|number| episode(1, number, Some("2024-02-29"), "Leap day"))
            .collect(),
    );
    let server = Server::open(catalog.config(&directory.0));
    let created = parse(&api(&server, "POST", "/api/series", Some(&create()), true));
    let series = id(&created).to_owned();

    let page = parse(&api(&server, "GET", "/api/series?limit=1", None, true));
    assert_eq!(page.get("total"), Some(&Value::Number(1.0)));
    assert_eq!(page.get("next"), Some(&Value::Null));
    assert_eq!(page.get("series").unwrap().as_array().unwrap().len(), 1);
    let after = format!("/api/series?limit=5&after={series}");
    let empty = parse(&api(&server, "GET", &after, None, true));
    assert!(empty.get("series").unwrap().as_array().unwrap().is_empty());
    // The shape never depends on the inventory size.
    let bare = parse(&api(&server, "GET", "/api/series", None, true));
    assert_eq!(bare.get("series").unwrap().as_array().unwrap().len(), 1);
    assert_eq!(bare.get("total"), Some(&Value::Number(1.0)));
    assert_eq!(bare.get("next"), Some(&Value::Null));
    for route in [
        "/api/series?limit=0",
        "/api/series?limit=201",
        "/api/series?after=nothex",
        "/api/series?limit=5&limit=6",
        "/api/series?unknown=1",
    ] {
        assert_eq!(
            api(&server, "GET", route, None, true).status,
            400,
            "{route}"
        );
    }

    let window = format!("/api/calendar?from=2024-02-29&to=2024-03-01&series_id={series}&limit=30");
    let mut seen = Vec::new();
    let mut route = window.clone();
    loop {
        let reply = api(&server, "GET", &route, None, true);
        assert_eq!(reply.status, 200);
        let value = parse(&reply);
        seen.extend(
            value
                .get("episodes")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row.get("episode").unwrap().as_u64().unwrap()),
        );
        match value.get("next").and_then(Value::as_str) {
            Some(cursor) => route = format!("{window}&after={cursor}"),
            None => break,
        }
    }
    assert_eq!(seen, (1..=80).collect::<Vec<_>>());
    assert_eq!(
        api(
            &server,
            "GET",
            &format!("{window}&offset=5&after=2024-02-29.{series}.1.1"),
            None,
            true
        )
        .status,
        400
    );
    assert_eq!(
        api(
            &server,
            "GET",
            &format!("{window}&after=garbage"),
            None,
            true
        )
        .status,
        400
    );
}
