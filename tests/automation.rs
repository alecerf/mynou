//! Local end-to-end workflow: authenticated API, durable journal and media import.
use mynou::config::{self, Config};
use mynou::engine::{Engine, lock};
use mynou::json::{self, Value};
use mynou::net::HttpClient;
use mynou::server::Api;
use mynou::store::{self, Request};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mynou-automation-{}-{}-{}",
            std::process::id(),
            store::now(),
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

fn local_config(directory: &Path) -> Config {
    let mut value = config::default_json();
    value.insert("listen", "127.0.0.1:0");
    value.insert("workers", 1_u32);
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads section");
    };
    downloads.insert("enabled".into(), false.into());
    downloads.insert("listen_port".into(), 0_u32.into());
    downloads.insert("dht".into(), false.into());
    downloads.insert("pex".into(), false.into());
    config::from_json(&value, directory).unwrap()
}

struct Server {
    engine: Arc<Engine>,
    origin: String,
    thread: Option<JoinHandle<mynou::Result<()>>>,
}
impl Server {
    fn open(config: Config) -> Self {
        let engine = Engine::open(config).unwrap();
        let api = Api::bind(engine.clone(), TOKEN.to_owned()).unwrap();
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
        route: &str,
        token: Option<&str>,
        value: Option<&Value>,
    ) -> (u16, Value) {
        let mut headers = vec![("Content-Type".into(), "application/json".into())];
        if let Some(token) = token {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        let body = value.map_or_else(Vec::new, |value| json::stringify(value).into_bytes());
        let response = HttpClient::new()
            .without_proxy()
            .with_timeout(Duration::from_secs(5))
            .request(method, &format!("{}{route}", self.origin), &headers, &body)
            .unwrap();
        let value = json::parse(std::str::from_utf8(&response.body).unwrap()).unwrap();
        (response.status, value)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.engine.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap().unwrap();
        }
    }
}

fn local_request(path: &Path, title: &str) -> Request {
    Request {
        kind: "movie".into(),
        title: title.into(),
        year: 2026,
        season: 0,
        episode: 0,
        source_path: Some(path.to_str().unwrap().into()),
        source_url: None,
        source_numbering: None,
        tmdb_id: None,
    }
}

fn submit(server: &Server, request: &Request) -> String {
    let (status, value) = server.call("POST", "/api/jobs", Some(TOKEN), Some(&request.to_json()));
    assert_eq!(status, 201, "{}", json::stringify(&value));
    let jobs = value.as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].get("lease_id").is_none());
    jobs[0].get("id").unwrap().as_str().unwrap().to_owned()
}

#[test]
fn authenticated_api_import_dedup_events_and_restart() {
    let directory = Directory::new();
    let source = directory.0.join("source.mp4");
    let bytes = include_bytes!("../examples/demo.mp4");
    fs::write(&source, bytes).unwrap();
    let config = local_config(&directory.0);
    let server = Server::open(config.clone());
    assert_eq!(server.call("GET", "/healthz", None, None).0, 200);
    assert_eq!(server.call("GET", "/readyz", None, None).0, 200);
    assert_eq!(server.call("GET", "/api/jobs", None, None).0, 401);
    assert_eq!(
        server.call("GET", "/api/jobs", Some("incorrect"), None).0,
        401
    );
    assert_eq!(server.call("GET", "/api/unknown", Some(TOKEN), None).0, 404);
    let request = local_request(&source, "Local Movie");
    let id = submit(&server, &request);
    assert_eq!(submit(&server, &request), id);
    let (_, jobs) = server.call("GET", "/api/jobs", Some(TOKEN), None);
    assert_eq!(jobs.as_array().unwrap().len(), 1);
    assert!(server.engine.tick().unwrap());
    assert_eq!(
        lock(&server.engine.store).unwrap().get(&id).unwrap().state,
        "imported"
    );
    assert!(server.engine.tick().unwrap());
    let (_, job) = server.call("GET", &format!("/api/jobs/{id}"), Some(TOKEN), None);
    assert_eq!(job.get("state").and_then(Value::as_str), Some("ready"));
    assert_eq!(job.get("progress").and_then(Value::as_f64), Some(1.0));
    let imported = PathBuf::from(
        job.get("imports").unwrap().as_array().unwrap()[0]
            .as_str()
            .unwrap(),
    );
    assert!(imported.starts_with(&config.movies_root));
    assert_eq!(fs::read(&imported).unwrap(), bytes);
    assert_eq!(fs::read(&source).unwrap(), bytes);
    let (_, events) = server.call("GET", &format!("/api/jobs/{id}/events"), Some(TOKEN), None);
    let events = events.as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.get("state").and_then(Value::as_str) == Some("imported"))
    );
    assert_eq!(
        events.last().unwrap().get("state").and_then(Value::as_str),
        Some("ready")
    );
    drop(server);
    let restarted = Server::open(config);
    let (_, resumed) = restarted.call("GET", &format!("/api/jobs/{id}"), Some(TOKEN), None);
    assert_eq!(resumed, job);
    assert_eq!(submit(&restarted, &request), id);
    assert!(!restarted.engine.tick().unwrap());
}

#[test]
fn native_torrent_cancel_and_retry_preserve_identity() {
    let directory = Directory::new();
    let mut config = local_config(&directory.0);
    config.downloads_enabled = true;
    let server = Server::open(config.clone());
    let request = Request {
        kind: "movie".into(),
        title: "Paused Local Download".into(),
        year: 2026,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: Some("magnet:?xt=urn:btih:0123456789012345678901234567890123456789".into()),
        source_numbering: None,
        tmdb_id: None,
    };
    let id = submit(&server, &request);
    assert!(server.engine.tick().unwrap());
    let download_id = lock(&server.engine.store)
        .unwrap()
        .get(&id)
        .unwrap()
        .download_id
        .unwrap();
    assert_eq!(
        server
            .call("POST", &format!("/api/jobs/{id}/cancel"), Some(TOKEN), None)
            .0,
        200
    );
    let pause = config
        .downloads
        .state_dir
        .join(format!("{download_id}.paused"));
    assert!(pause.exists());
    assert_eq!(
        server
            .call("POST", &format!("/api/jobs/{id}/retry"), Some(TOKEN), None)
            .0,
        200
    );
    assert!(server.engine.tick().unwrap());
    assert!(!pause.exists());
    let job = lock(&server.engine.store).unwrap().get(&id).unwrap();
    assert_eq!(job.download_id.as_deref(), Some(download_id.as_str()));
    assert_eq!(job.state, "downloading");
    assert_eq!(job.attempts, 0);
}

#[test]
fn cancellation_invalidates_lease_and_retry_recovers_failure() {
    let directory = Directory::new();
    let source = directory.0.join("invalid.mp4");
    fs::write(&source, b"invalid file").unwrap();
    let server = Server::open(local_config(&directory.0));
    let id = submit(&server, &local_request(&source, "Recovery"));
    let stale = lock(&server.engine.store)
        .unwrap()
        .claim(store::now(), 60)
        .unwrap()
        .unwrap();
    let (status, job) = server.call("POST", &format!("/api/jobs/{id}/cancel"), Some(TOKEN), None);
    assert_eq!(status, 200);
    assert_eq!(job.get("state").and_then(Value::as_str), Some("cancelled"));
    assert!(lock(&server.engine.store).unwrap().update(stale).is_err());
    assert!(!server.engine.tick().unwrap());
    assert_eq!(
        server
            .call("POST", &format!("/api/jobs/{id}/retry"), Some(TOKEN), None)
            .0,
        200
    );
    assert!(server.engine.tick().unwrap());
    let (_, failed) = server.call("GET", &format!("/api/jobs/{id}"), Some(TOKEN), None);
    assert_eq!(failed.get("state").and_then(Value::as_str), Some("failed"));
    assert_eq!(failed.get("attempts").and_then(Value::as_u64), Some(1));
    assert!(failed.get("last_error").and_then(Value::as_str).is_some());
    fs::write(&source, include_bytes!("../examples/demo.mp4")).unwrap();
    assert_eq!(
        server
            .call("POST", &format!("/api/jobs/{id}/retry"), Some(TOKEN), None)
            .0,
        200
    );
    assert!(server.engine.tick().unwrap());
    assert!(server.engine.tick().unwrap());
    let (_, ready) = server.call("GET", &format!("/api/jobs/{id}"), Some(TOKEN), None);
    assert_eq!(ready.get("state").and_then(Value::as_str), Some("ready"));
    assert_eq!(ready.get("last_error"), Some(&Value::Null));
    assert_eq!(
        server
            .call("POST", &format!("/api/jobs/{id}/cancel"), Some(TOKEN), None)
            .0,
        400
    );
}

#[test]
fn api_rejects_ambiguous_requests_and_non_json_media_types() {
    let directory = Directory::new();
    let source = directory.0.join("source.mp4");
    fs::write(&source, include_bytes!("../examples/demo.mp4")).unwrap();
    let server = Server::open(local_config(&directory.0));
    let mut request = local_request(&source, "Ambiguous");
    request.source_url = Some("http://127.0.0.1/source.torrent".into());
    assert_eq!(
        server
            .call("POST", "/api/jobs", Some(TOKEN), Some(&request.to_json()))
            .0,
        400
    );
    let response = HttpClient::new()
        .without_proxy()
        .with_timeout(Duration::from_secs(5))
        .request(
            "POST",
            &format!("{}/api/jobs", server.origin),
            &[
                ("Authorization".into(), format!("Bearer {TOKEN}")),
                ("Content-Type".into(), "text/plain".into()),
            ],
            b"{}",
        )
        .unwrap();
    assert_eq!(response.status, 415);
    assert!(lock(&server.engine.store).unwrap().list().is_empty());
}

struct DelayedMetadata {
    url: String,
    requested: Receiver<()>,
    release: Option<Sender<()>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DelayedMetadata {
    fn open(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/source.torrent", listener.local_addr().unwrap());
        let (requested_tx, requested) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            let mut stream = loop {
                if thread_stop.load(Ordering::Acquire) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") && request.len() < 16_384 {
                if stream.read_exact(&mut byte).is_err() {
                    return;
                }
                request.push(byte[0]);
            }
            if !request.starts_with(b"GET /source.torrent HTTP/1.1\r\n") {
                return;
            }
            if requested_tx.send(()).is_err()
                || released.recv_timeout(Duration::from_secs(10)).is_err()
            {
                return;
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-bittorrent\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream
                .write_all(head.as_bytes())
                .and_then(|()| stream.write_all(&body));
        });
        Self {
            url,
            requested,
            release: Some(release),
            stop,
            thread: Some(thread),
        }
    }

    fn release(&mut self) {
        self.release.take().unwrap().send(()).unwrap();
    }
}

impl Drop for DelayedMetadata {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn cancellation_during_metadata_fetch_pauses_native_transfer_and_survives_restart() {
    use mynou::bencode::{self, Value as B};
    use mynou::crypto;
    use std::collections::BTreeMap;

    let bytes = include_bytes!("../examples/demo.mp4");
    let info = B::Dict(BTreeMap::from([
        (b"length".to_vec(), B::Int(bytes.len() as i64)),
        (b"name".to_vec(), B::Bytes(b"Delayed.Demo.mp4".to_vec())),
        (b"piece length".to_vec(), B::Int(16_384)),
        (
            b"pieces".to_vec(),
            B::Bytes(bytes.chunks(16_384).flat_map(crypto::sha1).collect()),
        ),
        (b"private".to_vec(), B::Int(1)),
    ]));
    let expected_id: String = crypto::sha1(&bencode::encode(&info))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let metainfo = bencode::encode(&B::Dict(BTreeMap::from([(b"info".to_vec(), info)])));
    let mut metadata = DelayedMetadata::open(metainfo);
    let directory = Directory::new();
    let mut config = local_config(&directory.0);
    config.downloads_enabled = true;
    config.downloads.seed = false;
    let server = Server::open(config.clone());
    let request = Request {
        kind: "movie".into(),
        title: "Delayed Metadata".into(),
        year: 2026,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: Some(metadata.url.clone()),
        source_numbering: None,
        tmdb_id: None,
    };
    let id = submit(&server, &request);
    let engine = server.engine.clone();
    let tick = thread::spawn(move || engine.tick());
    metadata
        .requested
        .recv_timeout(Duration::from_secs(5))
        .expect("the fixture must block the HTTP metadata fetch");
    assert!(
        lock(&server.engine.store)
            .unwrap()
            .get(&id)
            .unwrap()
            .download_id
            .is_none()
    );
    let (status, cancelled) =
        server.call("POST", &format!("/api/jobs/{id}/cancel"), Some(TOKEN), None);
    assert_eq!(status, 200);
    assert_eq!(
        cancelled.get("state").and_then(Value::as_str),
        Some("cancelled")
    );
    metadata.release();
    assert!(tick.join().unwrap().unwrap());
    let saved = lock(&server.engine.store).unwrap().get(&id).unwrap();
    assert_eq!(saved.state, "cancelled");
    assert_eq!(saved.download_id.as_deref(), Some(expected_id.as_str()));
    assert!(saved.lease_id.is_none());
    let pause = config
        .downloads
        .state_dir
        .join(format!("{expected_id}.paused"));
    assert_eq!(fs::read(&pause).unwrap(), b"paused\n");
    assert!(
        config
            .downloads
            .state_dir
            .join(format!("{expected_id}.torrent"))
            .is_file()
    );
    drop(server);
    // Simulate an interruption between recording cancellation in the journal
    // and writing the native pause marker. The journal is authoritative.
    fs::remove_file(&pause).unwrap();
    let restarted = Server::open(config);
    let resumed = lock(&restarted.engine.store).unwrap().get(&id).unwrap();
    assert_eq!(resumed.state, "cancelled");
    assert_eq!(resumed.download_id, saved.download_id);
    assert_eq!(fs::read(pause).unwrap(), b"paused\n");
    assert!(!restarted.engine.tick().unwrap());
    assert_eq!(
        restarted
            .engine
            .status()
            .unwrap()
            .get("active")
            .and_then(Value::as_u64),
        Some(0)
    );
}
