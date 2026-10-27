//! Read-only selection previews through the CLI and authenticated local API.
use mynou::config::{self, Config};
use mynou::engine::{Engine, lock};
use mynou::json::{self, Value};
use mynou::net::HttpClient;
use mynou::server::Api;
use mynou::store::Request;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TOKEN: &str = "selection-api-fixture-token-0123456789";
const INDEXER_SECRET: &str = "synthetic-indexer-credential";
const DOWNLOAD_SECRET: &str = "synthetic-download-credential";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mynou-selection-api-{}-{timestamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Indexer {
    url: String,
    requests: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Indexer {
    fn open(handler: impl Fn(&str) -> (u16, String) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = stopped.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let thread_requests = requests.clone();
        let thread = thread::spawn(move || {
            while !thread_stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("Cannot accept fixture request: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    assert!(header.len() < 16_384 && Instant::now() < deadline);
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                }
                let header = std::str::from_utf8(&header).unwrap();
                let first = header.lines().next().unwrap();
                let mut parts = first.split_whitespace();
                assert_eq!(parts.next(), Some("GET"));
                let path = parts.next().unwrap();
                thread_requests.fetch_add(1, Ordering::Relaxed);
                let (status, body) = handler(path);
                write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        Self {
            url,
            requests,
            stopped,
            thread: Some(thread),
        }
    }
}

impl Drop for Indexer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}

struct Server {
    engine: Arc<Engine>,
    origin: String,
    thread: Option<JoinHandle<mynou::Result<()>>>,
}

impl Server {
    fn open(config: Config) -> Self {
        let engine = Engine::open(config).unwrap();
        let api = Api::bind(engine.clone()).unwrap();
        let origin = format!("http://{}", api.address().unwrap());
        Self {
            engine,
            origin,
            thread: Some(thread::spawn(move || api.run())),
        }
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let mut headers = vec![("Content-Type".into(), "application/json".into())];
        if let Some(token) = token {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        let body = body.map_or_else(Vec::new, |value| json::stringify(value).into_bytes());
        let response = HttpClient::new()
            .without_proxy()
            .with_timeout(Duration::from_secs(5))
            .with_max_body(1_048_576)
            .request(method, &format!("{}{path}", self.origin), &headers, &body)
            .unwrap();
        let value = json::parse(std::str::from_utf8(&response.body).unwrap()).unwrap();
        (response.status, value)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.engine.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if !thread::panicking() => panic!("API fixture failed: {error}"),
                Err(error) if !thread::panicking() => std::panic::resume_unwind(error),
                _ => {}
            }
        }
    }
}

fn configuration(indexers: &[&Indexer]) -> Value {
    let mut value = config::default_json();
    value.insert("listen", "127.0.0.1:0");
    value.insert(
        "selection",
        json::parse(
            r#"{"movie_profile":"hd","episode_profile":"hd","profiles":{"hd":{"resolutions":[1080,720],"languages":["en"]}}}"#,
        )
        .unwrap(),
    );
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads section");
    };
    downloads.insert("enabled".into(), false.into());
    downloads.insert("listen_port".into(), 0_u32.into());
    downloads.insert("dht".into(), false.into());
    downloads.insert("pex".into(), false.into());
    let sources = indexers
        .iter()
        .enumerate()
        .map(|(position, indexer)| {
            let mut source = Value::object();
            source.insert("name", format!("fixture-{position}"));
            source.insert("kind", "json");
            source.insert(
                "url",
                format!("{}/search?apikey={INDEXER_SECRET}", indexer.url),
            );
            source.insert("api_key_env", "MYNOU_SELECTION_ABSENT_FIXTURE_KEY_239104");
            source
        })
        .collect();
    value.insert("indexers", Value::Array(sources));
    value
}

fn movie() -> Request {
    Request {
        kind: "movie".into(),
        title: "Fixture Movie".into(),
        year: 2024,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: None,
        source_numbering: None,
        tmdb_id: Some(42),
    }
}

fn releases() -> String {
    format!(
        r#"{{"results":[
            {{"title":"Fixture.Movie.2024.720p.WEB-DL.EN","seeders":900,"url":"http://127.0.0.1/lower.torrent?token={DOWNLOAD_SECRET}"}},
            {{"title":"Fixture.Movie.2024.1080p.WEB-DL.EN","seeders":2,"url":"http://127.0.0.1/preferred.torrent?token={DOWNLOAD_SECRET}"}},
            {{"title":"Fixture.Movie.2024.2160p.WEB-DL.EN","seeders":999,"url":"http://127.0.0.1/oversized.torrent?token={DOWNLOAD_SECRET}"}},
            {{"title":"Fixture.Movie.2024.1080p.WEB-DL.FR","seeders":999,"url":"http://127.0.0.1/language.torrent?token={DOWNLOAD_SECRET}"}},
            {{"title":"Fixture.Movie.2023.1080p.WEB-DL.EN","seeders":999,"url":"http://127.0.0.1/wrong-year.torrent?token={DOWNLOAD_SECRET}"}},
            {{"title":"Fixture.Movie.2024.1080p.WEB-DL.EN","seeders":0,"url":"http://127.0.0.1/no-seeders.torrent?token={DOWNLOAD_SECRET}"}}
        ]}}"#
    )
}

fn preferred_indexer() -> Indexer {
    Indexer::open(|path| {
        assert!(path.contains("apikey=synthetic-indexer-credential"));
        assert!(path.contains("q=Fixture%20Movie"));
        assert!(path.contains("kind=movie"));
        assert!(path.contains("year=2024"));
        (200, releases())
    })
}

fn assert_preferred(report: &Value, configured: u64, failed: u64) {
    assert_eq!(report.get("profile").and_then(Value::as_str), Some("hd"));
    assert_eq!(report.get("manual_override"), Some(&Value::Bool(false)));
    let indexers = report.get("indexers").unwrap();
    assert_eq!(
        indexers.get("configured").and_then(Value::as_u64),
        Some(configured)
    );
    assert_eq!(indexers.get("successful").and_then(Value::as_u64), Some(1));
    assert_eq!(indexers.get("failed").and_then(Value::as_u64), Some(failed));
    assert_eq!(
        report.get("candidate_count").and_then(Value::as_u64),
        Some(6)
    );
    assert_eq!(
        report.get("reported_count").and_then(Value::as_u64),
        Some(6)
    );
    assert_eq!(report.get("truncated"), None);
    assert_eq!(report.get("next"), Some(&Value::Null));
    let accepted = report.get("accepted").unwrap().as_array().unwrap();
    let rejected = report.get("rejected").unwrap().as_array().unwrap();
    assert_eq!(accepted.len(), 2);
    assert_eq!(rejected.len(), 4);
    assert_eq!(
        accepted[0].get("title").and_then(Value::as_str),
        Some("Fixture.Movie.2024.1080p.WEB-DL.EN")
    );
    assert_eq!(accepted[0].get("seeders").and_then(Value::as_u64), Some(2));
    assert_eq!(
        accepted[1].get("title").and_then(Value::as_str),
        Some("Fixture.Movie.2024.720p.WEB-DL.EN")
    );
    assert_eq!(report.get("selected_candidate_id"), accepted[0].get("id"));
    for (candidates, expected) in [(accepted, true), (rejected, false)] {
        for candidate in candidates {
            let id = candidate.get("id").unwrap().as_str().unwrap();
            assert_eq!(id.len(), 64);
            assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert_eq!(
                candidate.get("source").and_then(Value::as_str),
                Some("fixture-0")
            );
            let assessment = candidate.get("assessment").unwrap();
            assert_eq!(assessment.get("accepted"), Some(&Value::Bool(expected)));
            if !expected {
                assert!(
                    !assessment
                        .get("reasons")
                        .unwrap()
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }
    assert_redacted(report);
}

fn assert_redacted(value: &Value) {
    let text = json::stringify(value);
    for forbidden in [
        INDEXER_SECRET,
        DOWNLOAD_SECRET,
        TOKEN,
        "http://",
        "https://",
        "magnet:",
        "apikey=",
        "token=",
        "source_url",
        "download_url",
    ] {
        assert!(
            !text.contains(forbidden),
            "Public preview contains {forbidden}"
        );
    }
}

fn files(directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, directory: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    collect(directory, directory, &mut out);
    out
}

#[test]
fn api_preview_works_while_the_engine_journal_is_locked() {
    let directory = Directory::new();
    let indexer = preferred_indexer();
    let failed = Indexer::open(|_| (503, "unavailable".into()));
    let value = configuration(&[&indexer, &failed]);
    let server = Server::open(config::from_json(&value, &directory.0).unwrap());
    let before = files(&directory.0);
    let mut store = lock(&server.engine.store).unwrap();
    let (status, profiles) = server.call("GET", "/api/profiles", Some(TOKEN), None);
    assert_eq!(status, 200);
    assert_eq!(profiles, server.engine.config.selection.to_json());
    let (status, report) =
        server.call("POST", "/api/search", Some(TOKEN), Some(&movie().to_json()));
    assert_eq!(status, 200, "{}", json::stringify(&report));
    assert_preferred(&report, 2, 1);
    // Cursor pages concatenate to the full report in the same order.
    let ids = |report: &Value| -> Vec<Value> {
        ["accepted", "rejected"]
            .iter()
            .flat_map(|key| report.get(key).unwrap().as_array().unwrap())
            .map(|candidate| candidate.get("id").unwrap().clone())
            .collect()
    };
    let (status, first) = server.call(
        "POST",
        "/api/search?limit=2",
        Some(TOKEN),
        Some(&movie().to_json()),
    );
    assert_eq!(status, 200, "{}", json::stringify(&first));
    assert_eq!(first.get("reported_count").and_then(Value::as_u64), Some(2));
    assert_eq!(
        first.get("selected_candidate_id"),
        report.get("selected_candidate_id")
    );
    let next = first.get("next").unwrap().as_str().unwrap().to_owned();
    let (status, rest) = server.call(
        "POST",
        &format!("/api/search?after={next}&limit=200"),
        Some(TOKEN),
        Some(&movie().to_json()),
    );
    assert_eq!(status, 200, "{}", json::stringify(&rest));
    assert_eq!(rest.get("next"), Some(&Value::Null));
    let mut paged = ids(&first);
    paged.extend(ids(&rest));
    assert_eq!(paged, ids(&report));
    assert_eq!(paged.len(), 6);
    let (status, error) = server.call(
        "POST",
        "/api/search?after=invalid",
        Some(TOKEN),
        Some(&movie().to_json()),
    );
    assert_eq!(status, 400, "{}", json::stringify(&error));
    assert_eq!(indexer.requests.load(Ordering::Relaxed), 3);
    assert_eq!(failed.requests.load(Ordering::Relaxed), 3);
    assert!(store.list().unwrap().is_empty());
    assert_eq!(files(&directory.0), before);
}

#[test]
fn api_manual_override_preview_preserves_privacy_without_submitting_or_searching() {
    let directory = Directory::new();
    let indexer = preferred_indexer();
    let server =
        Server::open(config::from_json(&configuration(&[&indexer]), &directory.0).unwrap());
    let before = files(&directory.0);
    let mut request = movie();
    request.source_url = Some(format!(
        "http://127.0.0.1/explicit.torrent?token={DOWNLOAD_SECRET}"
    ));
    let (status, report) =
        server.call("POST", "/api/search", Some(TOKEN), Some(&request.to_json()));
    assert_eq!(status, 200);
    assert_eq!(report.get("manual_override"), Some(&Value::Bool(true)));
    assert_eq!(report.get("profile"), Some(&Value::Null));
    assert_eq!(report.get("selected_candidate_id"), Some(&Value::Null));
    assert_eq!(
        report.get("candidate_count").and_then(Value::as_u64),
        Some(0)
    );
    assert!(
        report
            .get("accepted")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        report
            .get("rejected")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_redacted(&report);
    assert_eq!(indexer.requests.load(Ordering::Relaxed), 0);
    assert!(
        lock(&server.engine.store)
            .unwrap()
            .list()
            .unwrap()
            .is_empty()
    );
    assert_eq!(files(&directory.0), before);
}

#[test]
fn invalid_authenticated_search_requests_fail_before_indexer_access_or_journal_writes() {
    let directory = Directory::new();
    let indexer = preferred_indexer();
    let server =
        Server::open(config::from_json(&configuration(&[&indexer]), &directory.0).unwrap());
    let before = files(&directory.0);
    let mut unsupported_kind = movie();
    unsupported_kind.kind = "series".into();
    let mut invalid_year = movie();
    invalid_year.year = 10000;
    let mut invalid_url = movie();
    invalid_url.source_url = Some(format!("ftp://127.0.0.1/{DOWNLOAD_SECRET}"));
    let mut ambiguous_source = movie();
    ambiguous_source.source_path = Some("fixture.mp4".into());
    ambiguous_source.source_url = Some(format!(
        "http://127.0.0.1/explicit.torrent?token={DOWNLOAD_SECRET}"
    ));
    for request in [
        Value::Null,
        Value::object(),
        unsupported_kind.to_json(),
        invalid_year.to_json(),
        invalid_url.to_json(),
        ambiguous_source.to_json(),
    ] {
        let (status, error) = server.call("POST", "/api/search", Some(TOKEN), Some(&request));
        assert_eq!(status, 400, "{}", json::stringify(&error));
        assert!(error.get("error").and_then(Value::as_str).is_some());
        assert_redacted(&error);
    }
    assert_eq!(indexer.requests.load(Ordering::Relaxed), 0);
    assert!(
        lock(&server.engine.store)
            .unwrap()
            .list()
            .unwrap()
            .is_empty()
    );
    assert_eq!(files(&directory.0), before);
}
