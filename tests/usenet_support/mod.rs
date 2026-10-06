//! Original synthetic NZB/yEnc and loopback NNTP. No external providers or content.
#![allow(dead_code)]
use mynou::{
    config,
    json::Value,
    usenet::{ProbeRequest, Settings, queue::Client},
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
pub fn crc(bytes: &[u8]) -> u32 {
    let mut n = u32::MAX;
    for b in bytes {
        n ^= u32::from(*b);
        for _ in 0..8 {
            n = if n & 1 == 0 {
                n >> 1
            } else {
                (n >> 1) ^ 0xedb8_8320
            };
        }
    }
    !n
}
pub fn source(files: usize, parts: usize) -> Vec<u8> {
    format!("<nzb>{}</nzb>",(0..files).map(|f|format!("<file subject=\"Private original queue fixture\"><groups><group>alt.binaries.fixture</group></groups><segments>{}</segments></file>",(1..=parts).map(|p|format!("<segment number=\"{p}\" bytes=\"1024\">file{f}-part{p}@fixture.test</segment>")).collect::<String>())).collect::<String>()).into_bytes()
}
pub fn article(bytes: &[u8], n: usize, count: usize) -> Vec<u8> {
    article_named(bytes, n, count, "original queue fixture.bin")
}
pub fn article_named(bytes: &[u8], n: usize, count: usize, name: &str) -> Vec<u8> {
    let begin = (n - 1) * bytes.len() / count;
    let end = n * bytes.len() / count;
    let part = &bytes[begin..end];
    let mut out = if count == 1 {
        format!("=ybegin line=128 size={} name={name}\r\n", bytes.len()).into_bytes()
    } else {
        format!("=ybegin part={n} total={count} line=128 size={} name={name}\r\n=ypart begin={} end={end}\r\n",bytes.len(),begin+1).into_bytes()
    };
    let mut width = 0;
    for b in part {
        let e = b.wrapping_add(42);
        let v = if matches!(e, 0 | 9 | 10 | 13 | 32 | 46 | 61) {
            vec![b'=', e.wrapping_add(64)]
        } else {
            vec![e]
        };
        if width + v.len() > 128 {
            out.extend_from_slice(b"\r\n");
            width = 0;
        }
        width += v.len();
        out.extend(v);
    }
    out.extend_from_slice(b"\r\n");
    let footer = if count == 1 {
        format!("=yend size={} crc32={:08x}\r\n", part.len(), crc(bytes))
    } else {
        format!(
            "=yend size={} part={n} pcrc32={:08x}{}\r\n",
            part.len(),
            crc(part),
            if n == count {
                format!(" crc32={:08x}", crc(bytes))
            } else {
                String::new()
            }
        )
    };
    out.extend_from_slice(footer.as_bytes());
    out
}
#[derive(Clone)]
pub enum Response {
    Body(Vec<u8>),
    Unavailable,
    Truncated(Vec<u8>),
}
pub struct Provider {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub gate: Arc<AtomicBool>,
    pub active: Arc<AtomicUsize>,
    pub maximum: Arc<AtomicUsize>,
    replies: Arc<Mutex<BTreeMap<String, Response>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
fn command(s: &mut TcpStream) -> Option<String> {
    let mut b = Vec::new();
    while !b.ends_with(b"\r\n") {
        let mut c = [0];
        if s.read_exact(&mut c).is_err() {
            return None;
        }
        assert!(b.len() < 8192);
        b.push(c[0]);
    }
    String::from_utf8(b[..b.len() - 2].to_vec()).ok()
}
impl Provider {
    pub fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(BTreeMap::new()));
        let gate = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (q, r, g, a, m, t) = (
            requests.clone(),
            replies.clone(),
            gate.clone(),
            active.clone(),
            maximum.clone(),
            stop.clone(),
        );
        let worker = thread::spawn(move || {
            let mut handles = Vec::new();
            while !t.load(Ordering::Acquire) {
                let (mut s, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("Original provider accept failed: {e}"),
                };
                let (q, r, g, a, m, t) = (
                    q.clone(),
                    r.clone(),
                    g.clone(),
                    a.clone(),
                    m.clone(),
                    t.clone(),
                );
                handles.push(thread::spawn(move || {
                    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    s.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                    if s.write_all(b"201 Original queue provider\r\n").is_err() {
                        return;
                    }
                    let Some(cmd) = command(&mut s) else {
                        return;
                    };
                    if cmd == "QUIT" {
                        let _ = s.write_all(b"205 Bye\r\n");
                        return;
                    }
                    let id = cmd
                        .strip_prefix("BODY <")
                        .and_then(|s| s.strip_suffix('>'))
                        .expect("BODY command")
                        .to_owned();
                    q.lock().unwrap().push(id.clone());
                    let current = a.fetch_add(1, Ordering::AcqRel) + 1;
                    m.fetch_max(current, Ordering::AcqRel);
                    let start = Instant::now();
                    while g.load(Ordering::Acquire) && !t.load(Ordering::Acquire) {
                        assert!(start.elapsed() < Duration::from_secs(3));
                        thread::sleep(Duration::from_millis(2));
                    }
                    let reply = r
                        .lock()
                        .unwrap()
                        .get(&id)
                        .cloned()
                        .unwrap_or(Response::Unavailable);
                    match reply {
                        Response::Unavailable => {
                            let _ = s.write_all(b"430 Article unavailable\r\n");
                        }
                        Response::Body(body) | Response::Truncated(body) => {
                            let complete =
                                matches!(r.lock().unwrap().get(&id), Some(Response::Body(_)));
                            let _ = write!(s, "222 0 <{id}> Body\r\n");
                            for line in body.split(|b| *b == b'\n').filter(|p| !p.is_empty()) {
                                if line.starts_with(b".") {
                                    let _ = s.write_all(b".");
                                }
                                let _ = s.write_all(line);
                                let _ = s.write_all(b"\n");
                            }
                            if complete {
                                let _ = s.write_all(b".\r\n");
                            }
                        }
                    }
                    a.fetch_sub(1, Ordering::AcqRel);
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        });
        Self {
            port,
            requests,
            gate,
            active,
            maximum,
            replies,
            stop,
            thread: Some(worker),
        }
    }
    pub fn set(&self, id: &str, reply: Response) {
        self.replies.lock().unwrap().insert(id.into(), reply);
    }
    pub fn populate(&self, files: usize, parts: usize, data: &[u8]) {
        for f in 0..files {
            for p in 1..=parts {
                self.set(
                    &format!("file{f}-part{p}@fixture.test"),
                    Response::Body(article(data, p, parts)),
                );
            }
        }
    }
    pub fn wait_requests(&self, n: usize) {
        wait(|| self.requests.lock().unwrap().len() >= n);
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.gate.store(false, Ordering::Release);
        if let Some(h) = self.thread.take() {
            h.join().unwrap();
        }
    }
}
pub fn wait(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "Original fixture timed out"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
pub fn usenet(provider: &Provider) -> Value {
    let mut s = Value::object();
    s.insert("id", "original");
    s.insert("host", "127.0.0.1");
    s.insert("port", u32::from(provider.port));
    s.insert("tls", false);
    s.insert("timeout_ms", 2000_u32);
    let mut d = Value::object();
    d.insert("enabled", true);
    d.insert("state_dir", "state/usenet");
    d.insert("max_active", 2_u32);
    d.insert("max_attempts", 3_u32);
    d.insert("max_file_bytes", 1_048_576_u32);
    let mut v = Value::object();
    v.insert("servers", Value::Array(vec![s]));
    v.insert("downloads", d);
    v
}
pub fn settings(root: &Path, p: &Provider) -> Settings {
    let mut v = config::default_json();
    v.get_mut("downloads").unwrap().insert("enabled", false);
    v.insert("usenet", usenet(p));
    config::from_json(&v, root).unwrap().usenet
}
pub fn preview() -> ProbeRequest {
    ProbeRequest {
        apply: false,
        plan_id: None,
    }
}
pub fn apply(v: &Value) -> ProbeRequest {
    ProbeRequest {
        apply: true,
        plan_id: Some(v.get("plan_id").unwrap().as_str().unwrap().into()),
    }
}
pub fn enqueue(c: &Client, src: &[u8], index: usize) -> String {
    let v = c.enqueue(src, "original", index, &preview()).unwrap();
    c.enqueue(src, "original", index, &apply(&v)).unwrap();
    v.get("id").unwrap().as_str().unwrap().to_owned()
}
pub fn control(c: &Client, id: &str, action: &str) {
    let v = c.control(id, action, &preview()).unwrap();
    c.control(id, action, &apply(&v)).unwrap();
}
pub fn record(c: &Client, id: &str) -> Value {
    c.report()
        .unwrap()
        .get("records")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v.get("id").and_then(Value::as_str) == Some(id))
        .unwrap()
        .clone()
}
