//! API, native browser forms and CLI against synthetic catalog data. CI only.
mod library_support;
mod series_support;
mod web_support;
use library_support::Directory;
use mynou::{
    date,
    engine::lock,
    json::{self, Value},
};
use series_support::{Catalog, episode, id, request};
use std::{
    fs,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use web_support::{Browser, Reply, Server, TOKEN};

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
    for route in ["/api/series", "/api/calendar"] {
        assert_eq!(api(&server, "GET", route, None, false).status, 401);
    }
    assert_eq!(
        api(&server, "POST", "/api/series", Some(&create()), false).status,
        401
    );
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
    assert!(lock(&server.engine.store).unwrap().list().is_empty());
}

#[test]
fn browser_series_calendar_forms_inherit_authentication_and_escape_catalog_titles() {
    let directory = Directory::new();
    let today = date::today();
    let catalog = Catalog::open(vec![
        episode(1, 1, Some(&today), "<img src=x onerror='alert(1)'>"),
        episode(1, 2, None, "Unknown"),
        episode(
            1,
            3,
            Some(&today),
            "https://provider.invalid/file?token=library-download-fixture-secret",
        ),
    ]);
    let server = Server::open(catalog.config(&directory.0));
    let browser = Browser::login(&server);
    let created = browser.post(
        &server,
        "/ui/series/track",
        &[
            ("kind", "series"),
            ("title", "Fixture Series"),
            ("tmdb_id", "42"),
            ("future_only", "true"),
        ],
    );
    assert_eq!(created.status, 303, "{}", created.body);
    assert_eq!(created.headers["location"], "/ui/series");
    let records = server.engine.series().unwrap();
    let id = id(&records.as_array().unwrap()[0]);
    let detail = browser.get(&server, &format!("/ui/series/{id}"));
    assert_eq!(detail.status, 200);
    assert!(detail.body.contains("&lt;img"));
    assert!(!detail.body.contains("<img"));
    assert!(detail.body.contains("Unknown air date"));
    detail.no_secrets();
    let calendar = browser.get(
        &server,
        &format!("/ui/calendar?from={today}&to={today}&series_id={id}"),
    );
    assert_eq!(calendar.status, 200);
    assert!(calendar.body.contains("&lt;img"));
    assert!(calendar.body.contains("[redacted]"));
    calendar.no_secrets();
    assert_eq!(
        server
            .call("GET", "/api/series", &[("Cookie", &browser.cookie)], "")
            .status,
        401
    );
    let snapshot = server.engine.series_record(id).unwrap();
    assert_eq!(
        browser
            .raw_post(
                &server,
                "/ui/series/settings",
                &format!("csrf=wrong&id={id}&enabled=false&include_specials=false")
            )
            .status,
        403
    );
    assert_eq!(server.engine.series_record(id).unwrap(), snapshot);
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/series/episodes",
                &[
                    ("id", id),
                    ("season", "1"),
                    ("episode", "2"),
                    ("enabled", "false")
                ]
            )
            .status,
        303
    );
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/series/settings",
                &[
                    ("id", id),
                    ("enabled", "false"),
                    ("include_specials", "false"),
                    ("start_date", "")
                ]
            )
            .status,
        303
    );
    assert_eq!(
        server.engine.series_record(id).unwrap().get("monitored"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        browser
            .post(&server, "/ui/series/refresh", &[("id", id)])
            .status,
        303
    );
}

#[test]
fn calendar_and_episode_forms_reject_invalid_dates_numbers_and_bulk_identifiers() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let server = Server::open(catalog.config(&directory.0));
    let record = server.engine.track_series(&request(), false, true).unwrap();
    let id = id(&record);
    let browser = Browser::login(&server);
    let before = server.engine.series_record(id).unwrap();
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/series/action",
                &[("action", "unmonitor"), ("id", id), ("id", "invalid")]
            )
            .status,
        400
    );
    assert_eq!(server.engine.series_record(id).unwrap(), before);
    for route in [
        "/ui/calendar?from=2024-02-30",
        "/ui/calendar?from=2024-01-01&to=2026-01-01",
        "/ui/calendar?page=0",
        "/ui/calendar?series_id=invalid",
        "/ui/series?page=0",
    ] {
        assert_eq!(browser.get(&server, route).status, 400);
    }
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/series/episodes",
                &[
                    ("id", id),
                    ("season", "1"),
                    ("episode", "1"),
                    ("enabled", "invalid")
                ]
            )
            .status,
        400
    );
    assert_eq!(server.engine.series_record(id).unwrap(), before);
    let missing = "f".repeat(32);
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/series/action",
                &[("action", "unmonitor"), ("id", id), ("id", &missing)]
            )
            .status,
        303
    );
    assert!(
        browser
            .get(&server, "/ui/series")
            .body
            .contains("1 of 2 action(s) succeeded")
    );
}

#[test]
fn browser_series_submission_registers_monitoring_even_without_aired_episodes() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let server = Server::open(catalog.config(&directory.0));
    let browser = Browser::login(&server);
    let result = browser.post(
        &server,
        "/ui/requests",
        &[
            ("kind", "series"),
            ("title", "Fixture Series"),
            ("tmdb_id", "42"),
            ("source_kind", "auto"),
        ],
    );
    assert_eq!(result.status, 303);
    assert_eq!(result.headers["location"], "/ui/series");
    assert_eq!(server.engine.series().unwrap().as_array().unwrap().len(), 1);
    assert!(lock(&server.engine.store).unwrap().list().is_empty());
}

fn command(directory: &Directory, args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(&directory.0)
        .env_remove("MYNOU_API_TOKEN")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("Series CLI fixture timed out");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn cli_routes_series_creation_controls_and_calendar_to_the_running_service() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let server = Server::open(catalog.config(&directory.0));
    let mut value = mynou::config::default_json();
    value.insert("listen", server.authority.clone());
    fs::write(directory.0.join("mynou.json"), json::stringify(&value)).unwrap();
    fs::write(
        directory.0.join(".env"),
        format!("MYNOU_API_TOKEN={TOKEN}\n"),
    )
    .unwrap();
    let result = command(
        &directory,
        &[
            "track-series",
            "--title",
            "Fixture Series",
            "--tmdb-id",
            "42",
            "--future-only",
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let record = json::parse(std::str::from_utf8(&result.stdout).unwrap()).unwrap();
    let id = id(&record);
    for args in [
        vec!["series"],
        vec!["series", id],
        vec!["series-unmonitor", id],
        vec!["episode-unmonitor", id, "--season", "1", "--episode", "1"],
        vec!["series-monitor", id],
        vec!["series-refresh", id],
        vec![
            "calendar",
            "--from",
            "2200-01-01",
            "--to",
            "2200-01-02",
            "--series-id",
            id,
        ],
    ] {
        let output = command(&directory, &args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = std::str::from_utf8(&output.stdout).unwrap();
        json::parse(text).unwrap();
        assert!(!text.contains(TOKEN));
        assert!(!text.contains(&catalog.url));
    }
    assert!(
        !command(&directory, &["calendar", "--limit", "201"])
            .status
            .success()
    );
    assert!(
        !command(&directory, &["episode-monitor", id, "--episode", "1"])
            .status
            .success()
    );
}
