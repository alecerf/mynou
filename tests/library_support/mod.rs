//! Shared bounded local fixtures for the library integration test targets.
#![allow(dead_code)]

use mynou::config::{self, Config};
use mynou::engine::{Engine, lock};
use mynou::json::{self, Value};
use mynou::store::{Job, Request};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const TOKEN: &str = "library-fixture-token-01234567890123456789";
pub const INDEXER_SECRET: &str = "library-indexer-fixture-secret";
pub const DOWNLOAD_SECRET: &str = "library-download-fixture-secret";
static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct Directory(pub PathBuf);

impl Directory {
    pub fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mynou-library-{}-{timestamp}-{}",
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

pub struct Indexer {
    pub url: String,
    pub calls: Arc<AtomicUsize>,
    response: Arc<Mutex<(u16, String)>>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Indexer {
    pub fn open(releases: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let response = Arc::new(Mutex::new((200, json::stringify(&releases))));
        let thread_response = response.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = stopped.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let thread_calls = calls.clone();
        let thread = thread::spawn(move || {
            while !thread_stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("Cannot accept indexer fixture request: {error}"),
                };
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
                assert!(header.starts_with("GET "));
                assert!(header.contains(INDEXER_SECRET));
                thread_calls.fetch_add(1, Ordering::Relaxed);
                let (status, body) = thread_response.lock().unwrap().clone();
                write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ).unwrap();
            }
        });
        Self {
            url,
            calls,
            response,
            stopped,
            thread: Some(thread),
        }
    }

    pub fn replace(&self, releases: Value) {
        *self.response.lock().unwrap() = (200, json::stringify(&releases));
    }

    pub fn fail(&self) {
        *self.response.lock().unwrap() = (503, "fixture unavailable".into());
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

pub fn configuration(indexer: Option<&Indexer>, profile: &str) -> Value {
    let mut value = config::default_json();
    value.insert("listen", "127.0.0.1:0");
    value.insert("workers", 1_u32);
    value.insert("max_attempts", 1_u32);
    value.insert("selection", json::parse(&format!(
        r#"{{"movie_profile":"fixture","episode_profile":"fixture","profiles":{{"fixture":{profile}}}}}"#
    )).unwrap());
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads section");
    };
    downloads.insert("enabled".into(), false.into());
    downloads.insert("listen_port".into(), 0_u32.into());
    downloads.insert("dht".into(), false.into());
    downloads.insert("pex".into(), false.into());
    if let Some(indexer) = indexer {
        let mut source = Value::object();
        source.insert("name", "local fixture");
        source.insert("kind", "json");
        source.insert(
            "url",
            format!("{}/indexer?apikey={INDEXER_SECRET}", indexer.url),
        );
        source.insert("api_key_env", "MYNOU_LIBRARY_ABSENT_FIXTURE_KEY_592744");
        value.insert("indexers", Value::Array(vec![source]));
    }
    value
}

pub fn config(directory: &Path, indexer: Option<&Indexer>, profile: &str) -> Config {
    config::from_json(&configuration(indexer, profile), directory).unwrap()
}

pub fn release(title: &str, seeders: u32, source: &str) -> Value {
    let mut value = Value::object();
    value.insert("title", title);
    value.insert("seeders", seeders);
    value.insert("url", source);
    value
}

pub fn movie(title: &str) -> Request {
    Request {
        kind: "movie".into(),
        title: title.into(),
        year: 2024,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: None,
        tmdb_id: None,
    }
}

pub fn local_import(engine: &Arc<Engine>, directory: &Path, title: &str) -> Job {
    let source = directory.join(format!("{}.mp4", title.replace(' ', "-")));
    fs::write(&source, include_bytes!("../../examples/demo.mp4")).unwrap();
    let mut request = movie(title);
    request.source_path = Some(source.to_str().unwrap().into());
    let id = engine.submit(request).unwrap().remove(0).id;
    run_until(engine, &id, "ready")
}

pub fn run_until(engine: &Arc<Engine>, id: &str, state: &str) -> Job {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let job = lock(&engine.store).unwrap().get(id).unwrap();
        if job.state == state {
            return job;
        }
        assert!(
            job.state != "failed" && Instant::now() < deadline,
            "Job {id} did not reach {state}: {} {:?}",
            job.state,
            job.last_error
        );
        engine.tick().unwrap();
        thread::sleep(Duration::from_millis(5));
    }
}

pub fn library_ids(engine: &Engine) -> Vec<String> {
    lock(&engine.store)
        .unwrap()
        .library_jobs()
        .into_iter()
        .map(|job| job.id)
        .collect()
}

pub fn files(directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
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

pub fn assert_redacted(value: &Value) {
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
    ] {
        assert!(
            !text.contains(forbidden),
            "Library output contains {forbidden}"
        );
    }
}
