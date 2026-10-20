//! Automatic season-pack search through native tunnel owners (#319).
//! The indexer is ordinary-path by design and is the only declared host
//! service; candidate metadata travels only through the tunnel.
use super::{
    AutoPackRequest,
    tunnel_tests::{
        Directory, accept, deadline, dictionary, engine_with, no_acquisition, request, response,
        server,
    },
};
use crate::{
    bencode::{self, Value as B},
    config,
    engine::{Engine, lock},
    json::{self, Value},
    tunnel_http::tests::{Pair, allow_service},
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const SECRET: &str = "synthetic-pack-secret";

/// Synthetic JSON indexer on a literal loopback port: no host name lookup.
struct Indexer {
    port: u16,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Indexer {
    fn open(download_url: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        allow_service(address);
        listener.set_nonblocking(true).unwrap();
        let mut release = Value::object();
        release.insert("title", "Fixture.Series.S01.Complete.1080p.WEB-DL");
        release.insert("download_url", download_url);
        release.insert("seeders", 50_u32);
        let body = json::stringify(&Value::Array(vec![release])).into_bytes();
        let (calls, stop) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let worker = {
            let (calls, stop) = (calls.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut head = Vec::new();
                    let mut byte = [0];
                    while !head.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).is_ok() {
                        head.push(byte[0]);
                    }
                    assert!(head.starts_with(b"GET /indexer"));
                    calls.fetch_add(1, Ordering::AcqRel);
                    let mut reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    reply.extend_from_slice(&body);
                    let _ = stream.write_all(&reply);
                }
            })
        };
        Self {
            port: address.port(),
            calls,
            stop,
            worker: Some(worker),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}
impl Drop for Indexer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && let Err(error) = worker.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}

/// Ranked resolutions and one declared indexer; still no download workers.
fn engine(directory: &Directory, pair: &Pair, indexer: &Indexer) -> (Arc<Engine>, String) {
    let mut settings = config::default_json();
    let selection = json::parse(
        r#"{"profiles":{"any":{"resolutions":[2160,1080,720,480]}},"movie_profile":"any","episode_profile":"any"}"#,
    )
    .unwrap();
    settings.insert("selection", selection);
    let mut source = Value::object();
    source.insert("name", "Pack fixture");
    source.insert("kind", "json");
    source.insert("url", format!("http://127.0.0.1:{}/indexer", indexer.port));
    source.insert("api_key_env", "MYNOU_PACK_ABSENT_INDEXER_KEY_319");
    settings.insert("indexers", Value::Array(vec![source]));
    engine_with(directory, pair.client_network.clone(), settings)
}

/// `Fixture.Series.S01/S01E0{1,2}.mp4`, one explicit episode per file.
fn season_metadata() -> (Vec<u8>, String) {
    let file = |name: &str| {
        dictionary(&[
            (b"length", B::Int(4096)),
            (b"path", B::List(vec![B::Bytes(name.as_bytes().to_vec())])),
        ])
    };
    let info = dictionary(&[
        (b"name", B::Bytes(b"Fixture.Series.S01".to_vec())),
        (b"piece length", B::Int(16384)),
        (b"pieces", B::Bytes(vec![9; 20])),
        (
            b"files",
            B::List(vec![file("S01E01.mp4"), file("S01E02.mp4")]),
        ),
    ]);
    let hash = crate::crypto::sha1(&bencode::encode(&info))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    (bencode::encode(&dictionary(&[(b"info", info)])), hash)
}

fn preview() -> AutoPackRequest {
    AutoPackRequest {
        season: 1,
        apply: false,
        scope_id: None,
        candidate_id: None,
    }
}

fn guarded(report: &Value) -> AutoPackRequest {
    AutoPackRequest {
        season: 1,
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

fn search(engine: &Engine, series: &str, query: &AutoPackRequest) -> crate::Result<Value> {
    engine.search_packs_before(series, query, deadline())
}

#[test]
fn automatic_pack_preview_and_guarded_apply_use_tunnel_metadata_without_workers() {
    let directory = Directory::new();
    let pair = Pair::new();
    let (metadata, hash) = season_metadata();
    let (port, worker) = server(&*pair.server_network, vec![response(&metadata); 2]);
    let redirect = format!("HTTP/1.1 302 Found\r\nLocation: http://metadata.example:{port}/source.torrent\r\nContent-Length: 0\r\n\r\n").into_bytes();
    let (redirect_port, redirect_worker) = server(&*pair.server_network, vec![redirect; 2]);
    let indexer = Indexer::open(format!(
        "http://first.example:{redirect_port}/pack.torrent?token={SECRET}"
    ));
    let (engine, series) = engine(&directory, &pair, &indexer);
    let before = lock(&engine.series_store).unwrap().get(&series).unwrap();

    let report = search(&engine, &series, &preview()).unwrap();
    assert_eq!(
        report.get("torrent_id").and_then(Value::as_str),
        Some(hash.as_str())
    );
    assert_eq!(report.get("mapping").unwrap().as_array().unwrap().len(), 2);
    assert!(report.get("submission").is_none());
    assert!(!json::stringify(&report).contains(SECRET));
    no_acquisition(&engine);
    assert_eq!(
        lock(&engine.series_store).unwrap().get(&series).unwrap(),
        before
    );

    let applied = search(&engine, &series, &guarded(&report)).unwrap();
    assert_eq!(
        applied.get("selected_candidate_id"),
        report.get("selected_candidate_id")
    );
    let jobs = lock(&engine.store).unwrap().list().unwrap();
    assert_eq!(jobs.len(), 2);
    assert!(
        jobs.iter()
            .all(|job| job.state == "queued" && job.attempts == 0 && job.download_id.is_none())
    );
    assert!(
        jobs.iter()
            .all(|job| job.pack_origin.as_ref().unwrap().torrent_id == hash)
    );
    assert_eq!(
        jobs.iter()
            .map(|job| (job.request.episode, job.pack_file.clone().unwrap()))
            .collect::<std::collections::BTreeSet<_>>(),
        [
            (1, "Fixture.Series.S01/S01E01.mp4".into()),
            (2, "Fixture.Series.S01/S01E02.mp4".into())
        ]
        .into()
    );
    assert_eq!(
        engine.transfers().unwrap_err(),
        "Native downloads are disabled"
    );
    assert!(!engine.config.downloads.data_dir.exists());
    assert!(!engine.config.downloads.state_dir.exists());
    assert_eq!(worker.join().unwrap().len(), 2);
    assert_eq!(redirect_worker.join().unwrap().len(), 2);
    assert_eq!(indexer.calls(), 2);
    assert!(pair.dns.load(Ordering::Relaxed) >= 4);
}

#[test]
fn cancellation_during_automatic_pack_metadata_refuses_preview_apply_and_retry() {
    for apply in [false, true] {
        cancel_pending_metadata(apply);
    }
}

fn cancel_pending_metadata(apply: bool) {
    let directory = Directory::new();
    let pair = Pair::new();
    let (metadata, _) = season_metadata();
    let listener = pair
        .server_network
        .listen("0.0.0.0:0".parse().unwrap())
        .unwrap();
    let indexer = Indexer::open(format!(
        "http://metadata.example:{}/pack.torrent?token={SECRET}",
        listener.port()
    ));
    let (engine, series) = engine(&directory, &pair, &indexer);
    let (entered, waiting) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        // First fetch supplies valid preview guards; the second fetch is held.
        for delay in [false, true] {
            let mut stream = accept(&*listener);
            request(&mut *stream);
            if delay {
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            let result = stream.write_all(&response(&metadata));
            if !delay {
                result.unwrap();
            }
            let _ = stream.read(&mut [0]);
        }
    });
    let report = search(&engine, &series, &preview()).unwrap();
    no_acquisition(&engine);
    let query = if apply { guarded(&report) } else { preview() };
    let before = lock(&engine.series_store).unwrap().get(&series).unwrap();
    let pending = {
        let (engine, series, query) = (engine.clone(), series.clone(), query.clone());
        thread::spawn(move || search(&engine, &series, &query))
    };
    waiting.recv_timeout(Duration::from_secs(10)).unwrap();
    pair.client.cancel();
    release.send(()).unwrap();
    assert!(
        pending.join().unwrap().is_err(),
        "late metadata published a selection or committed owners"
    );
    let started = Instant::now();
    assert!(search(&engine, &series, &query).is_err());
    assert!(search(&engine, &series, &preview()).is_err());
    assert!(started.elapsed() < Duration::from_secs(10));
    no_acquisition(&engine);
    assert_eq!(
        lock(&engine.series_store).unwrap().get(&series).unwrap(),
        before
    );
    server.join().unwrap();
    // Retries still reach the ordinary indexer, never ordinary metadata.
    assert_eq!(indexer.calls(), 4);
    assert!(pair.dns.load(Ordering::Relaxed) >= 2);
}
