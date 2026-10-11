//! Bounded original catalog fixture; only local TCP and synthetic metadata.
#![allow(dead_code)]
use mynou::{
    config::{self, Config},
    json::{self, Value},
    store::Request,
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type Responses = BTreeMap<String, (u16, Value)>;

pub struct Catalog {
    pub url: String,
    pub calls: Arc<AtomicUsize>,
    pub blocked: Arc<AtomicBool>,
    responses: Arc<Mutex<Responses>>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Catalog {
    pub fn open(episodes: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let responses = Arc::new(Mutex::new(BTreeMap::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let blocked = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let server_responses = responses.clone();
        let server_stop = stopped.clone();
        let server_block = blocked.clone();
        let server_calls = calls.clone();
        let thread = thread::spawn(move || {
            while !server_stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("Catalog accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !bytes.ends_with(b"\r\n\r\n") {
                    assert!(bytes.len() < 16384 && Instant::now() < deadline);
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                let header = std::str::from_utf8(&bytes).unwrap();
                assert!(header.starts_with("GET "));
                let path = header
                    .split(' ')
                    .nth(1)
                    .unwrap()
                    .split('?')
                    .next()
                    .unwrap()
                    .to_owned();
                if path.starts_with("/3/") {
                    assert!(header.contains("Authorization: Bearer "));
                }
                server_calls.fetch_add(1, Ordering::Release);
                while server_block.load(Ordering::Acquire) && !server_stop.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline, "Catalog gate timed out");
                    thread::sleep(Duration::from_millis(2));
                }
                let response = server_responses.lock().unwrap().get(&path).cloned();
                let (status, body) =
                    response.unwrap_or_else(|| panic!("Unexpected catalog fixture route: {path}"));
                let body = json::stringify(&body);
                write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let result = Self {
            url,
            responses,
            stopped,
            blocked,
            calls,
            thread: Some(thread),
        };
        result.response("/3/tv/42", 200, json::parse(r#"{"id":42,"name":"Fixture Series","first_air_date":"2024-01-01","seasons":[{"season_number":0},{"season_number":1}]}"#).unwrap());
        result.episodes(1, episodes);
        result.episodes(
            0,
            vec![episode(0, 1, Some("2024-01-01"), "Catalog Special")],
        );
        result.response(
            "/3/search/tv",
            200,
            json::parse(
                r#"{"results":[{"id":42,"name":"Fixture Series","first_air_date":"2024-01-01"}]}"#,
            )
            .unwrap(),
        );
        result.response(
            "/watchlist",
            200,
            json::parse(r#"{"MediaContainer":{"Metadata":[]}}"#).unwrap(),
        );
        result
    }
    pub fn response(&self, path: &str, status: u16, body: Value) {
        self.responses
            .lock()
            .unwrap()
            .insert(path.to_owned(), (status, body));
    }
    pub fn episodes(&self, season: u32, episodes: Vec<Value>) {
        let mut value = Value::object();
        value.insert("season_number", season);
        value.insert("episodes", Value::Array(episodes));
        self.response(&format!("/3/tv/42/season/{season}"), 200, value);
    }
    pub fn config(&self, directory: &Path) -> Config {
        let mut value = config::default_json();
        value.insert("listen", "127.0.0.1:0");
        value.get_mut("downloads").unwrap().insert("enabled", false);
        let mut config = config::from_json(&value, directory).unwrap();
        config.catalog.enabled = true;
        config.catalog.url = format!("{}/3", self.url);
        // Existing nonsecret PATH supplies a fixture-only Bearer value, without mutating process environment.
        config.catalog.token_env = "PATH".into();
        config.catalog.api_key_env = "MYNOU_SERIES_ABSENT_KEY_89171".into();
        config
    }
    pub fn wait_for_calls(&self, previous: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.calls.load(Ordering::Acquire) <= previous {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Catalog {
    fn drop(&mut self) {
        self.blocked.store(false, Ordering::Release);
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}
pub fn episode(season: u32, number: u32, air: Option<&str>, title: &str) -> Value {
    let mut value = Value::object();
    value.insert("id", 10000 + season * 1000 + number);
    value.insert("season_number", season);
    value.insert("episode_number", number);
    value.insert("name", title);
    value.insert("air_date", air.map_or(Value::Null, Value::from));
    value
}
pub fn request() -> Request {
    Request {
        kind: "series".into(),
        title: "Fixture Series".into(),
        year: 2024,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: None,
        source_numbering: None,
        tmdb_id: Some(42),
    }
}
pub fn id(value: &Value) -> &str {
    value.get("id").unwrap().as_str().unwrap()
}
pub fn episodes(value: &Value) -> &[Value] {
    value.get("episodes").unwrap().as_array().unwrap()
}

fn collect(path: &Path, prefix: &str, files: &mut Vec<(String, Vec<u8>)>) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let mut names: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        for name in names {
            let child = format!("{prefix}/{}", name.to_string_lossy());
            collect(&path.join(&name), &child, files);
        }
    } else {
        files.push((prefix.to_owned(), std::fs::read(path).unwrap()));
    }
}

/// Every byte of series storage: the marker or snapshot and the whole index.
pub fn series_state(store_dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    for name in ["series.json", "series.idx", "series.idx.migrating"] {
        collect(&store_dir.join(name), name, &mut files);
    }
    files
}

/// A pre-index `series.json` (schema 1 omits numbering history) rebuilt from
/// the public records of a series.
pub fn legacy_snapshot(records: &[Value], schema: u32) -> Vec<u8> {
    let payload = Value::Array(
        records
            .iter()
            .map(|record| {
                let mut record = record.clone();
                if let Some(Value::Array(episodes)) = record.get_mut("episodes") {
                    for episode in episodes {
                        if let Value::Object(fields) = episode {
                            fields.remove("monitored");
                            fields.remove("excluded");
                        }
                    }
                }
                if schema == 1
                    && let Value::Object(fields) = &mut record
                {
                    fields.remove("anchors");
                    fields.remove("numbering");
                }
                record
            })
            .collect(),
    );
    let digest: String = mynou::crypto::sha256(json::stringify(&payload).as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut envelope = Value::object();
    envelope.insert("schema_version", schema);
    envelope.insert("digest", digest);
    envelope.insert("records", payload);
    json::stringify(&envelope).into_bytes()
}

/// Turns a store directory back into what 0.23 and earlier left behind.
pub fn restore_legacy(store_dir: &Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_dir_all(store_dir.join("series.idx"));
    let path = store_dir.join("series.json");
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
