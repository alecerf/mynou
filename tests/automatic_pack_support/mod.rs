//! Original bounded indexer, metadata and tracker fixtures for CI-only pack journeys.
#![allow(dead_code)]
use mynou::{
    config::{Config, Source},
    json::{self, Value},
    pack::AutoPackRequest,
    selection::{Profile, SelectionConfig},
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const SECRET: &str = "automatic-pack-fixture-secret";
type Responses = BTreeMap<String, (u16, Vec<u8>)>;
pub struct Provider {
    pub url: String,
    pub calls: Arc<Mutex<Vec<String>>>,
    pub blocked: Arc<AtomicBool>,
    responses: Arc<Mutex<Responses>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Provider {
    pub fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(Responses::new()));
        let blocked = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (requests, records, gate, stopped) = (
            calls.clone(),
            responses.clone(),
            blocked.clone(),
            stop.clone(),
        );
        let handle = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("Pack provider accept failed: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut header = Vec::new();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !header.ends_with(b"\r\n\r\n") {
                    assert!(header.len() < 16384 && Instant::now() < deadline);
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                }
                let request = std::str::from_utf8(&header)
                    .unwrap()
                    .split("\r\n")
                    .next()
                    .unwrap()
                    .to_owned();
                let target = request.split(' ').nth(1).unwrap();
                let path = target.split('?').next().unwrap().to_owned();
                requests.lock().unwrap().push(request.clone());
                while path.starts_with("/metadata")
                    && gate.load(Ordering::Acquire)
                    && !stopped.load(Ordering::Acquire)
                {
                    assert!(
                        Instant::now() < deadline,
                        "Pack metadata gate exceeded its deadline"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
                let (status, body) = records
                    .lock()
                    .unwrap()
                    .get(&path)
                    .cloned()
                    .unwrap_or((404, b"unknown fixture".to_vec()));
                // Deadline scenarios deliberately close the socket before the gate opens.
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        Self {
            url,
            calls,
            blocked,
            responses,
            stop,
            thread: Some(handle),
        }
    }
    pub fn response(&self, path: &str, status: u16, body: Vec<u8>) {
        self.responses
            .lock()
            .unwrap()
            .insert(path.into(), (status, body));
    }
    pub fn releases(&self, entries: Vec<Value>) {
        self.response(
            "/indexer",
            200,
            json::stringify(&Value::Array(entries)).into_bytes(),
        );
    }
    pub fn metadata_url(&self, name: &str, encoded: Vec<u8>) -> String {
        self.response(&format!("/metadata-{name}"), 200, encoded);
        format!("{}/metadata-{name}?token={SECRET}", self.url)
    }
    pub fn source(&self, kind: &str) -> Source {
        Source {
            options: Default::default(),
            name: "Pack fixture".into(),
            kind: kind.into(),
            url: format!("{}/indexer?apikey={SECRET}", self.url),
            api_key_env: "MYNOU_PACK_ABSENT_INDEXER_KEY_911305".into(),
        }
    }
    pub fn wait_metadata(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if self
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("/metadata"))
            {
                return;
            }
            assert!(Instant::now() < deadline, "No metadata request arrived");
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.blocked.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}
pub fn release(title: &str, url: &str, seeders: u32) -> Value {
    let mut value = Value::object();
    value.insert("title", title);
    value.insert("download_url", url);
    value.insert("seeders", seeders);
    value
}
pub fn configure(mut config: Config, provider: &Provider, profile: &str) -> Config {
    config.sources = vec![provider.source("json")];
    config.minimum_seeders = 1;
    config.selection = SelectionConfig {
        profiles: BTreeMap::from([(
            "pack".into(),
            Profile::from_json(&json::parse(profile).unwrap()).unwrap(),
        )]),
        movie_profile: "pack".into(),
        episode_profile: "pack".into(),
    };
    config
}
pub fn preview(season: u32) -> AutoPackRequest {
    AutoPackRequest {
        season,
        apply: false,
        scope_id: None,
        candidate_id: None,
    }
}
pub fn apply(report: &Value) -> AutoPackRequest {
    AutoPackRequest {
        season: report.get("season").unwrap().as_u64().unwrap() as u32,
        apply: true,
        scope_id: Some(report.get("scope_id").unwrap().as_str().unwrap().into()),
        candidate_id: Some(
            report
                .get("selected_candidate_id")
                .unwrap()
                .as_str()
                .unwrap()
                .into(),
        ),
    }
}
pub fn selected_title(report: &Value) -> &str {
    let selected = report.get("selected_candidate_id").unwrap();
    report
        .get("accepted")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row.get("id") == Some(selected))
        .unwrap()
        .get("title")
        .unwrap()
        .as_str()
        .unwrap()
}
pub fn no_sources(report: &Value) {
    let text = json::stringify(report);
    for forbidden in [
        SECRET,
        "download_url",
        "acquisition_url",
        "magnet:",
        "http://",
    ] {
        assert!(!text.contains(forbidden), "Pack report exposed {forbidden}");
    }
}
pub fn snapshot(path: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.extend(snapshot(&path));
            } else {
                result.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    result
}
