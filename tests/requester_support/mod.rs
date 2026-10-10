//! Original bounded account service, using nonsecret inherited fixture values.
#![allow(dead_code)]
use mynou::{
    config::{self, Config},
    crypto::sha256,
    engine::{Engine, lock},
    json::{self, Value},
    requesters::{Account, ControlRequest, Destination, Policy},
    store::{Job, Request},
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub struct Accounts {
    pub url: String,
    pub calls: Arc<Mutex<Vec<String>>>,
    responses: Arc<Mutex<BTreeMap<String, (u16, Value)>>>,
    pub blocked: Arc<AtomicBool>,
    pub remove_on_request: Arc<Mutex<Option<(String, PathBuf)>>>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
pub fn container(items: Vec<Value>) -> Value {
    let mut c = Value::object();
    c.insert("size", items.len() as u32);
    c.insert("totalSize", items.len() as u32);
    c.insert("Metadata", Value::Array(items));
    let mut v = Value::object();
    v.insert("MediaContainer", c);
    v
}
impl Accounts {
    pub fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let responses = Arc::new(Mutex::new(BTreeMap::<String, (u16, Value)>::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let blocked = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let remove_on_request = Arc::new(Mutex::new(None::<(String, PathBuf)>));
        let removal = remove_on_request.clone();
        let values = responses.clone();
        let requests = calls.clone();
        let delay = blocked.clone();
        let stop = stopped.clone();
        let thread = thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("Account fixture accept failed: {e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 16384);
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    head.push(byte[0]);
                }
                let header = String::from_utf8(head).unwrap();
                let target = header.split(' ').nth(1).unwrap();
                let path = target.split('?').next().unwrap();
                let expected = if path.starts_with("/bob/") {
                    std::env::var("PWD").unwrap()
                } else {
                    std::env::var("PATH").unwrap()
                };
                assert!(
                    header
                        .lines()
                        .any(|h| h.eq_ignore_ascii_case(&format!("X-Plex-Token: {expected}"))),
                    "Account token header missing"
                );
                requests.lock().unwrap().push(path.into());
                let mut pending = removal.lock().unwrap();
                if pending.as_ref().is_some_and(|(route, _)| route == path) {
                    let (_, file) = pending.take().unwrap();
                    fs::remove_file(file).unwrap();
                }
                drop(pending);
                if path == "/alice/identity" {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while delay.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(2));
                    }
                }
                let (status, value) = values
                    .lock()
                    .unwrap()
                    .get(path)
                    .cloned()
                    .unwrap_or((404, Value::Null));
                let body = json::stringify(&value);
                write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        let server = Self {
            url,
            calls,
            responses,
            blocked,
            remove_on_request,
            stopped,
            thread: Some(thread),
        };
        for (name, id) in [("alice", 101_u32), ("bob", 202)] {
            let mut identity = Value::object();
            identity.insert("id", id);
            server.response(&format!("/{name}/identity"), 200, identity);
            server.watchlist(name, Vec::new());
        }
        server
    }
    pub fn response(&self, path: &str, status: u16, value: Value) {
        self.responses
            .lock()
            .unwrap()
            .insert(path.into(), (status, value));
    }
    pub fn watchlist(&self, id: &str, items: Vec<Value>) {
        self.response(&format!("/{id}/watchlist"), 200, container(items));
    }
    pub fn config(&self, root: &Path) -> Config {
        let mut v = config::default_json();
        v.insert("listen", "127.0.0.1:0");
        let d = v.get_mut("downloads").unwrap();
        d.insert("enabled", false);
        d.insert("listen_port", 0_u32);
        d.insert("dht", false);
        d.insert("pex", false);
        let mut c = config::from_json(&v, root).unwrap();
        c.requesters.accounts = vec![
            Account {
                id: "alice".into(),
                expected_user_id: "101".into(),
                token_env: "PATH".into(),
                identity_url: format!("{}/alice/identity", self.url),
                watchlist_url: format!("{}/alice/watchlist", self.url),
            },
            Account {
                id: "bob".into(),
                expected_user_id: "202".into(),
                token_env: "PWD".into(),
                identity_url: format!("{}/bob/identity", self.url),
                watchlist_url: format!("{}/bob/watchlist", self.url),
            },
        ];
        c.requesters.destinations.push(Destination {
            id: "family".into(),
            movies_root: root.join("family/movies"),
            series_root: root.join("family/series"),
        });
        c
    }
    pub fn wait(&self, old: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.calls.lock().unwrap().len() <= old {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Accounts {
    fn drop(&mut self) {
        self.blocked.store(false, Ordering::Release);
        self.stopped.store(true, Ordering::Release);
        if let Some(h) = self.thread.take()
            && let Err(e) = h.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(e)
        }
    }
}
pub fn movie(id: u32, title: &str) -> Value {
    let mut v = Value::object();
    v.insert("type", "movie");
    v.insert("title", title);
    v.insert("year", 2024_u32);
    v.insert("guid", format!("tmdb://{id}"));
    v
}
pub fn show() -> Value {
    let mut v = Value::object();
    v.insert("type", "show");
    v.insert("title", "Fixture Series");
    v.insert("year", 2024_u32);
    v.insert("guid", "tmdb://42");
    v
}
pub fn policy(engine: &Engine, id: &str) -> Policy {
    Policy::from_json(engine.requester(id, 0, 100).unwrap().get("policy").unwrap()).unwrap()
}
pub fn policy_query(policy: Policy) -> ControlRequest {
    ControlRequest {
        action: "policy".into(),
        policy: Some(policy),
        demand_id: None,
        apply: false,
        plan_id: None,
    }
}
pub fn demand_query(action: &str, id: &str) -> ControlRequest {
    ControlRequest {
        action: action.into(),
        policy: None,
        demand_id: Some(id.into()),
        apply: false,
        plan_id: None,
    }
}
pub fn apply(engine: &Engine, id: &str, mut q: ControlRequest) -> Value {
    let preview = engine.requester_control(id, &q).unwrap();
    q.apply = true;
    q.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    engine.requester_control(id, &q).unwrap()
}
pub fn enable(engine: &Engine, id: &str) {
    let mut p = policy(engine, id);
    p.enabled = true;
    apply(engine, id, policy_query(p));
}
pub fn demands(engine: &Engine, id: &str) -> Vec<Value> {
    engine
        .requester(id, 0, 200)
        .unwrap()
        .get("demands")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec()
}
pub fn demand(engine: &Engine, id: &str) -> Value {
    demands(engine, id).remove(0)
}
pub fn id(v: &Value) -> &str {
    v.get("id").unwrap().as_str().unwrap()
}
pub fn job(engine: &Engine, id: &str) -> Job {
    let d = demand(engine, id);
    lock(&engine.store)
        .unwrap()
        .get(d.get("job_id").unwrap().as_str().unwrap())
        .unwrap()
}
pub fn ready(engine: &Engine, id: &str) -> Job {
    let mut job = lock(&engine.store).unwrap().get(id).unwrap();
    let capture = &job.requester.as_ref().unwrap().capture;
    let root = if job.request.kind == "movie" {
        &capture.movies_root
    } else {
        &capture.series_root
    };
    let path = PathBuf::from(root).join(format!("fixture-{id}.mp4"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, include_bytes!("../../examples/demo.mp4")).unwrap();
    job.imports = vec![path.to_str().unwrap().into()];
    job.state = "ready".into();
    job.progress = 1.0;
    lock(&engine.store).unwrap().update(job.clone()).unwrap();
    job
}
pub fn read_snapshot(directory: &Path) -> Value {
    let bytes = fs::read(directory.join("requesters.bin")).unwrap();
    json::parse(std::str::from_utf8(&bytes[16..bytes.len() - 32]).unwrap()).unwrap()
}
pub fn write_snapshot(directory: &Path, value: &Value) {
    let payload = json::stringify(value).into_bytes();
    let mut bytes = b"MYNOUR01".to_vec();
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&sha256(&bytes));
    fs::write(directory.join("requesters.bin"), bytes).unwrap();
}
pub fn no_credentials(v: &Value) {
    let s = json::stringify(v);
    for name in ["PATH", "PWD"] {
        assert!(!s.contains(&std::env::var(name).unwrap()));
    }
    assert!(!s.contains("X-Plex-Token"));
    assert!(!s.contains("/identity"));
    assert!(!s.contains("/watchlist"));
}
pub fn request(id: u64, title: &str) -> Request {
    Request {
        kind: "movie".into(),
        title: title.into(),
        year: 2024,
        season: 0,
        episode: 0,
        tmdb_id: Some(id),
        source_path: None,
        source_url: None,
        source_numbering: None,
    }
}
