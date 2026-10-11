//! A declarative `mapping` reads any JSON indexer. Synthetic loopback fixtures only.
mod library_support;
use library_support::Directory;
use mynou::{
    config, integrations,
    json::{self, Value},
    store::Request,
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread::{self, JoinHandle},
    time::Duration,
};

/// Answers one request with `body` and returns the request line it saw.
fn serve(body: &'static str) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/api", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            assert!(head.len() < 16_384);
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        head.lines().next().unwrap().to_owned()
    });
    (url, handle)
}

fn movie() -> Request {
    Request {
        kind: "movie".into(),
        title: "Fixture Movie".into(),
        year: 2024,
        season: 0,
        episode: 0,
        tmdb_id: Some(7),
        source_numbering: None,
        source_path: None,
        source_url: None,
    }
}

fn configuration(directory: &Directory, url: &str, mapping: Option<&str>) -> Value {
    let mut v = config::default_json();
    v.insert("listen", "127.0.0.1:0");
    v.insert("store_dir", directory.0.join("jobs").to_str().unwrap());
    v.get_mut("downloads").unwrap().insert("enabled", false);
    let mut source = Value::object();
    source.insert("name", "mapped");
    source.insert("kind", "json");
    source.insert("url", url);
    source.insert("api_key_env", "PWD");
    if let Some(mapping) = mapping {
        source.insert("mapping", json::parse(mapping).unwrap());
    }
    v.insert("indexers", Value::Array(vec![source]));
    v
}

const MAPPING: &str = r#"{
    "results": "data.torrents",
    "title": "meta.name",
    "url": ["links.missing", "links.download"],
    "seeders": "stats.seeders",
    "params": {"query": "search", "kind": "category", "year": "", "api_key": "key"},
    "kind_values": {"movie": "2000"},
    "fixed_params": {"limit": "50"}
}"#;

const MAPPED_BODY: &str = r#"{"data":{"torrents":[{"meta":{"name":"Fixture.Movie.2024.1080p"},"links":{"download":"magnet:?xt=urn:btih:0123456789012345678901234567890123456789"},"stats":{"seeders":"9"}}]}}"#;

#[test]
fn mapping_renames_parameters_and_reads_nested_fields_without_leaking_the_key() {
    let directory = Directory::new();
    let (url, server) = serve(MAPPED_BODY);
    let c = config::from_json(&configuration(&directory, &url, Some(MAPPING)), &directory.0)
        .unwrap();
    let report = json::stringify(&integrations::search_report(&c, &movie()).unwrap());
    let line = server.join().unwrap();
    assert!(
        line.starts_with("GET /api?search=Fixture%20Movie&category=2000&season=0&episode=0&limit=50&key="),
        "unexpected request: {line}"
    );
    assert!(!line.contains("year=") && !line.contains("apikey="));
    assert!(report.contains("Fixture.Movie.2024.1080p"));
    assert!(!report.contains(&std::env::var("PWD").unwrap()));
    let health = mynou::indexers::report(&c.sources);
    let health = json::stringify(&health);
    assert!(!health.contains("data.torrents") && !health.contains("limit"));
}

#[test]
fn a_source_without_mapping_keeps_the_built_in_names() {
    let directory = Directory::new();
    let (url, server) = serve(r#"[{"title":"Fixture.Movie.2024.1080p","seeders":5,"url":"magnet:?xt=urn:btih:0123456789012345678901234567890123456789"}]"#);
    let mut v = configuration(&directory, &url, None);
    let Value::Array(sources) = v.get_mut("indexers").unwrap() else {
        panic!()
    };
    sources[0].insert("api_key_env", "MYNOU_ABSENT_MAPPING_FIXTURE_KEY_5521");
    let c = config::from_json(&v, &directory.0).unwrap();
    let report = json::stringify(&integrations::search_report(&c, &movie()).unwrap());
    assert_eq!(
        server.join().unwrap(),
        "GET /api?q=Fixture%20Movie&kind=movie&year=2024&season=0&episode=0 HTTP/1.1"
    );
    assert!(report.contains("Fixture.Movie.2024.1080p"));
}

#[test]
fn a_response_that_does_not_match_the_mapping_is_a_parse_failure() {
    let directory = Directory::new();
    let (url, server) = serve(r#"{"results":[{"title":"Fixture.Movie.2024.1080p"}]}"#);
    let c = config::from_json(&configuration(&directory, &url, Some(MAPPING)), &directory.0)
        .unwrap();
    let _ = integrations::search_report(&c, &movie());
    server.join().unwrap();
    let health = mynou::indexers::report(&c.sources);
    let source = &health.get("sources").unwrap().as_array().unwrap()[0];
    assert_eq!(source.get("last_parse"), Some(&Value::Bool(false)));
}

#[test]
fn invalid_mappings_are_rejected_by_configuration() {
    let directory = Directory::new();
    for mapping in [
        r#"{"unknown":"x"}"#,
        r#"{"title":"a..b"}"#,
        r#"{"params":{"query":"same","year":"same"}}"#,
        r#"{"fixed_params":{"q":"x"}}"#,
    ] {
        let v = configuration(&directory, "http://127.0.0.1:9/api", Some(mapping));
        assert!(config::from_json(&v, &directory.0).is_err(), "{mapping}");
    }
    let mut v = configuration(&directory, "http://127.0.0.1:9/api", Some("{}"));
    let Value::Array(sources) = v.get_mut("indexers").unwrap() else {
        panic!()
    };
    sources[0].insert("kind", "torznab");
    assert!(config::from_json(&v, &directory.0).is_err());
}
