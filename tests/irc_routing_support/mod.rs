//! Original local metadata gates and checked crash fixtures.
#![allow(dead_code)]
use mynou::{
    config::{self, Config},
    crypto::sha256,
    engine::{Engine, lock},
    json::{self, Value},
    store::{Job, Request},
};
use std::{
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub fn configuration(root: &Path, peer: u16) -> Config {
    let mut v = crate::transfer_support::configuration();
    v.insert("store_dir", root.join("jobs").to_str().unwrap());
    let mut settings = crate::irc_support::settings("irc://127.0.0.1:1");
    let Value::Array(sources) = settings.get_mut("sources").unwrap() else {
        panic!()
    };
    sources[0].insert(
        "magnet_template",
        format!("magnet:?xt={{xt}}&x.pe=127.0.0.1:{peer}"),
    );
    let Value::Array(rules) = settings.get_mut("rules").unwrap() else {
        panic!()
    };
    rules[0].insert("action", "grab");
    v.insert("irc", settings);
    config::from_json(&v, root).unwrap()
}
pub fn request(id: u64) -> Request {
    Request {
        kind: "movie".into(),
        title: "Fixture Movie".into(),
        year: 2024,
        season: 0,
        episode: 0,
        tmdb_id: Some(id),
        source_numbering: None,
        source_path: None,
        source_url: None,
    }
}
pub fn receive(engine: &Engine, hash: &str, id: u64) -> Value {
    let mut v = crate::irc_support::announcement(id);
    v.insert("info_hash", hash);
    engine
        .irc_receive("local", crate::irc_support::SENDER, "#announces", &v)
        .unwrap()
}
pub fn row_id(row: &Value) -> &str {
    crate::irc_support::record_id(row)
}
pub fn route_phase(row: &Value) -> Option<&str> {
    row.get("route")
        .and_then(|r| r.get("phase"))
        .and_then(Value::as_str)
}
pub fn job(engine: &Engine, id: &str) -> Job {
    lock(&engine.store).unwrap().get(id).unwrap()
}
pub fn no_candidate_work(engine: &Engine, id: &str) {
    let job = job(engine, id);
    assert!(job.irc_origin.is_none() && job.acquisition_url.is_none() && job.download_id.is_none());
    assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
}
pub fn no_private(report: &Value) {
    let s = json::stringify(report);
    for private in [
        "magnet:?",
        "irc-route-private-fixture",
        "magnet_template",
        "binding",
        "fingerprint",
    ] {
        assert!(
            !s.contains(private),
            "Public report contains private routing state"
        );
    }
}
pub fn read_checked(path: &Path) -> Value {
    let b = fs::read(path).unwrap();
    json::parse(std::str::from_utf8(&b[16..b.len() - 32]).unwrap()).unwrap()
}
pub fn write_checked(path: &Path, magic: &[u8; 8], v: &Value) {
    let p = json::stringify(v).into_bytes();
    let mut b = magic.to_vec();
    b.extend_from_slice(&(p.len() as u64).to_le_bytes());
    b.extend_from_slice(&p);
    b.extend_from_slice(&sha256(&b));
    fs::write(path, b).unwrap();
}
pub fn row_mut<'a>(snapshot: &'a mut Value, id: &str) -> &'a mut Value {
    let Value::Array(rows) = snapshot.get_mut("records").unwrap() else {
        panic!()
    };
    rows.iter_mut()
        .find(|r| r.get("id").and_then(Value::as_str) == Some(id))
        .unwrap()
}
pub fn only_job_mut(snapshot: &mut Value) -> &mut Value {
    let Value::Array(jobs) = snapshot.get_mut("jobs").unwrap() else {
        panic!()
    };
    assert_eq!(jobs.len(), 1);
    &mut jobs[0]
}

/// Accept before forwarding the handshake, allowing admission changes during metadata I/O.
pub struct MetadataGate {
    pub port: u16,
    pub accepted: Arc<AtomicUsize>,
    pub released: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    sockets: Arc<Mutex<Vec<TcpStream>>>,
    handle: Option<JoinHandle<()>>,
}
impl MetadataGate {
    pub fn open(seed: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let sockets = Arc::new(Mutex::new(Vec::new()));
        let (a, r, stop, streams) = (
            accepted.clone(),
            released.clone(),
            stopped.clone(),
            sockets.clone(),
        );
        let handle = thread::spawn(move || {
            let mut workers = Vec::new();
            while !stop.load(Ordering::Acquire) {
                let (client, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("Local metadata gate accept failed: {e}"),
                };
                client.set_nonblocking(false).unwrap();
                assert!(
                    a.fetch_add(1, Ordering::AcqRel) < 16,
                    "Metadata gate connection bound exceeded"
                );
                streams.lock().unwrap().push(client.try_clone().unwrap());
                let (r, stop, streams) = (r.clone(), stop.clone(), streams.clone());
                workers.push(thread::spawn(move || {
                    let end = Instant::now() + Duration::from_secs(5);
                    while !r.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                        assert!(Instant::now() < end, "Metadata gate was not released");
                        thread::sleep(Duration::from_millis(2));
                    }
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    let server = TcpStream::connect_timeout(
                        &([127, 0, 0, 1], seed).into(),
                        Duration::from_secs(1),
                    )
                    .unwrap();
                    streams.lock().unwrap().push(server.try_clone().unwrap());
                    for s in [&client, &server] {
                        s.set_read_timeout(Some(Duration::from_millis(100)))
                            .unwrap();
                        s.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                    }
                    let (reverse_in, reverse_out, reverse_stop) = (
                        server.try_clone().unwrap(),
                        client.try_clone().unwrap(),
                        stop.clone(),
                    );
                    let reverse =
                        thread::spawn(move || forward(reverse_in, reverse_out, reverse_stop));
                    forward(client, server, stop);
                    reverse.join().unwrap();
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            port,
            accepted,
            released,
            stopped,
            sockets,
            handle: Some(handle),
        }
    }
    pub fn wait(&self) {
        crate::irc_support::wait(|| self.accepted.load(Ordering::Acquire) > 0);
    }
}
fn forward(mut input: TcpStream, mut output: TcpStream, stop: Arc<AtomicBool>) {
    let mut b = [0; 4096];
    while !stop.load(Ordering::Acquire) {
        match input.read(&mut b) {
            Ok(0) => break,
            Ok(n) => {
                if output.write_all(&b[..n]).is_err() {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    let _ = input.shutdown(Shutdown::Both);
    let _ = output.shutdown(Shutdown::Both);
}
impl Drop for MetadataGate {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.released.store(true, Ordering::Release);
        for s in self.sockets.lock().unwrap().iter() {
            let _ = s.shutdown(Shutdown::Both);
        }
        if let Some(h) = self.handle.take() {
            h.join().unwrap();
        }
    }
}
