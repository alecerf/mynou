//! The service log over the API: a level filter, the newest records first,
//! and the debug report, under the same access rules as other routes.
use crate::api_support::Server;
use mynou::{config, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    time::Duration,
};

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
fn the_log_is_read_by_level_newest_first() {
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

    // Another site's page reads neither the log nor the report.
    for route in ["/api/logs", "/api/report"] {
        let reply = server.call("GET", route, &[("Host", "evil.example")], "");
        assert_eq!(reply.status, 403, "{route}");
        assert!(!reply.body.contains("refused"));
    }
    drop(server);
    let _ = fs::remove_dir_all(root);
}

/// The raw answer to `GET path`: its head and its body as bytes.
fn fetch(server: &Server, path: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(&server.authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        server.authority
    )
    .unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(bytes[..split].to_vec()).unwrap();
    (head, bytes[split + 4..].to_vec())
}

#[test]
fn the_debug_report_holds_the_log_the_archive_and_the_state() {
    let (server, root) = server("report");
    fs::write(root.join("logs/mynou.log"), CURRENT).unwrap();
    fs::create_dir(root.join("logs/archive")).unwrap();
    let archived = [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3, 1, 2, 3];
    fs::write(root.join("logs/archive/mynou-2026-11-10.log.gz"), archived).unwrap();

    let (head, body) = fetch(&server, "/api/report");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let header = |name: &str| {
        head.lines()
            .find_map(|line| line.strip_prefix(&format!("{name}: ")))
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(header("Content-Type"), "application/x-tar");
    assert_eq!(header("Content-Length"), body.len().to_string());
    assert!(header("Content-Disposition").starts_with("attachment; filename=\"mynou-report-"));
    assert_eq!(header("Cache-Control"), "no-store");

    // The tar file: each member's name and contents.
    let mut members = Vec::new();
    let mut at = 0;
    while at + 512 <= body.len() && body[at] != 0 {
        let name = String::from_utf8_lossy(&body[at..at + 100])
            .trim_end_matches('\0')
            .to_owned();
        let size =
            usize::from_str_radix(std::str::from_utf8(&body[at + 124..at + 135]).unwrap(), 8)
                .unwrap();
        members.push((name, body[at + 512..at + 512 + size].to_vec()));
        at += 512 + size.div_ceil(512) * 512;
    }
    assert_eq!(body.len(), at + 1024);
    let names: Vec<&str> = members
        .iter()
        .map(|(name, _)| name.split_once('/').unwrap().1)
        .collect();
    assert_eq!(names[0], "README.txt");
    for expected in [
        "state/status.json",
        "state/settings.json",
        "state/jobs.json",
        "logs/archive/mynou-2026-11-10.log.gz",
        "logs/mynou.log.gz",
    ] {
        assert!(names.contains(&expected), "{expected} in {names:?}");
    }
    assert!(!names.iter().any(|name| name.contains("secrets")));
    let member = |name: &str| {
        &members
            .iter()
            .find(|(member, _)| member.ends_with(name))
            .unwrap()
            .1
    };
    let readme = String::from_utf8(member("README.txt").clone()).unwrap();
    assert!(readme.starts_with("Mynou debug report"), "{readme}");
    assert!(readme.contains("Tokens, keys and passwords are hidden."));
    // The archive goes as it is; the newest records are compressed.
    assert_eq!(member("archive/mynou-2026-11-10.log.gz"), &archived);
    assert_eq!(&member("logs/mynou.log.gz")[..2], &[0x1f, 0x8b]);
    let status = json::parse(std::str::from_utf8(member("state/status.json")).unwrap()).unwrap();
    assert_eq!(
        status.get("status").and_then(json::Value::as_f64),
        Some(200.0)
    );
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
    drop(server);
    let _ = fs::remove_dir_all(root);
}
