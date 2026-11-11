//! The service log over the API: a level filter, the newest records first,
//! and every file to download, under the same access rules as other routes.
use crate::api_support::Server;
use mynou::{config, json};
use std::{fs, path::PathBuf};

fn server(name: &str) -> (Server, PathBuf) {
    let root = std::env::temp_dir().join(format!("mynou-logs-api-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for dir in ["movies", "series", "logs"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let mut value = config::default_json();
    value.insert("listen", "127.0.0.1:0");
    let library = value.get_mut("library").unwrap();
    library.insert(
        "movies_root",
        root.join("movies").to_str().unwrap().to_owned(),
    );
    library.insert(
        "series_root",
        root.join("series").to_str().unwrap().to_owned(),
    );
    value.get_mut("downloads").unwrap().insert("enabled", false);
    let mut cfg = config::from_json(&value, &root).unwrap();
    cfg.store_dir = root.join("store");
    (Server::open(cfg), root)
}

const OLDER: &str = "2026-11-10T22:00:00.000Z ERROR irc: announce channel refused\n";
const CURRENT: &str = "\
2026-11-11T01:00:00.000Z INFO  mynou: mynou started\n\
2026-11-11T01:00:01.000Z DEBUG indexers: Indexer request failed (Films): timed out\n\
2026-11-11T01:00:02.000Z WARN  requesters: Requester retry of 4 waits: busy\n\
2026-11-11T01:00:03.000Z INFO  engine: Film (2024): ready [job 4]\n";

fn entries(body: &str) -> Vec<String> {
    let value = json::parse(body).unwrap();
    value
        .get("entries")
        .and_then(json::Value::as_array)
        .unwrap()
        .iter()
        .map(|entry| {
            ["level", "source", "message"]
                .map(|key| {
                    entry
                        .get(key)
                        .and_then(json::Value::as_str)
                        .unwrap()
                        .to_owned()
                })
                .join(" ")
        })
        .collect()
}

#[test]
fn the_log_is_read_by_level_newest_first_and_downloaded_whole() {
    let (server, root) = server("read");
    fs::write(root.join("logs/mynou.1.log"), OLDER).unwrap();
    fs::write(root.join("logs/mynou.log"), CURRENT).unwrap();

    let reply = server.call("GET", "/api/logs?level=warn", &[], "");
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        entries(&reply.body),
        [
            "warn requesters Requester retry of 4 waits: busy",
            "error irc announce channel refused"
        ]
    );
    // Information and more severe by default.
    let reply = server.call("GET", "/api/logs", &[], "");
    assert_eq!(
        entries(&reply.body),
        [
            "info engine Film (2024): ready [job 4]",
            "warn requesters Requester retry of 4 waits: busy",
            "info mynou mynou started",
            "error irc announce channel refused"
        ]
    );
    let value = json::parse(&reply.body).unwrap();
    assert_eq!(
        value.get("level").and_then(json::Value::as_str),
        Some("info")
    );
    assert_eq!(
        value.get("bytes").and_then(json::Value::as_f64),
        Some((OLDER.len() + CURRENT.len()) as f64)
    );
    assert_eq!(
        value.get("limit_bytes").and_then(json::Value::as_f64),
        Some(mynou::logs::TOTAL_LIMIT as f64)
    );
    let reply = server.call("GET", "/api/logs?level=debug&limit=2", &[], "");
    assert_eq!(
        entries(&reply.body),
        [
            "info engine Film (2024): ready [job 4]",
            "warn requesters Requester retry of 4 waits: busy"
        ]
    );
    for refused in [
        "/api/logs?level=trace",
        "/api/logs?limit=0",
        "/api/logs?since=1",
    ] {
        assert_eq!(
            server.call("GET", refused, &[], "").status,
            400,
            "{refused}"
        );
    }

    let download = server.call("GET", "/api/logs/download", &[], "");
    assert_eq!(download.status, 200);
    assert_eq!(
        download.headers["content-type"],
        "text/plain; charset=utf-8"
    );
    assert!(download.headers["content-disposition"].starts_with("attachment; filename=\"mynou-"));
    assert_eq!(download.headers["cache-control"], "no-store");
    // Oldest file first.
    assert_eq!(download.body, format!("{OLDER}{CURRENT}"));

    // Another site's page reaches neither route.
    for route in ["/api/logs", "/api/logs/download"] {
        let reply = server.call("GET", route, &[("Host", "evil.example")], "");
        assert_eq!(reply.status, 403, "{route}");
        assert!(!reply.body.contains("refused"));
    }
    drop(server);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn an_installation_without_a_log_reads_as_empty() {
    let (server, root) = server("empty");
    fs::remove_dir_all(root.join("logs")).unwrap();
    let reply = server.call("GET", "/api/logs", &[], "");
    assert_eq!(reply.status, 200);
    assert!(entries(&reply.body).is_empty());
    let download = server.call("GET", "/api/logs/download", &[], "");
    assert_eq!(download.status, 200);
    assert_eq!(download.body, "");
    drop(server);
    let _ = fs::remove_dir_all(root);
}
