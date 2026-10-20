//! Explicit shared-pack orchestration through native tunnel owners (#314).
//! No catalog, indexer, payload peer or management socket exists on the host.
use super::SharedFileRequest;
use crate::{
    bencode::{self, Value as B},
    config,
    engine::{Engine, lock},
    json::Value,
    net::NetworkStream,
    series::{Episode, Plan},
    store::{Request, now},
    torrent::transport::{Network, NetworkListener},
    tunnel_http::tests::Pair,
};
use std::{
    collections::BTreeMap,
    fs, io,
    path::PathBuf,
    sync::{Arc, atomic::Ordering, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let id = crate::crypto::random_bytes::<16>().unwrap();
        let directory = std::env::temp_dir().join(format!("mynou-pack-tunnel-{id:x?}"));
        fs::create_dir(&directory).unwrap();
        Self(directory)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn engine(directory: &Directory, network: Arc<dyn Network>) -> (Arc<Engine>, String) {
    let mut settings = config::default_json();
    settings
        .get_mut("downloads")
        .unwrap()
        .insert("enabled", true);
    let config = config::from_json(&settings, &directory.0).unwrap();
    // Enabled configuration, but deliberately no transfer workers/listener.
    let engine = Engine::open_with_network(config, network, false).unwrap();
    let plan = Plan {
        request: Request {
            kind: "series".into(),
            title: "Fixture Series".into(),
            year: 2024,
            season: 0,
            episode: 0,
            source_path: None,
            source_url: None,
            tmdb_id: Some(42),
            source_numbering: None,
        },
        episodes: (1..=2)
            .map(|episode| Episode {
                catalog_id: Some(100 + u64::from(episode)),
                season: 1,
                episode,
                title: format!("Fixture episode {episode}"),
                air_date: Some("2024-01-01".into()),
            })
            .collect(),
    };
    // Install an already accepted synthetic catalog plan, without a catalog socket.
    let series = lock(&engine.series_store)
        .unwrap()
        .subscribe(plan, false, false, false, now())
        .unwrap();
    (engine, series.id)
}

fn dictionary(fields: &[(&[u8], B)]) -> B {
    B::Dict(BTreeMap::from_iter(
        fields
            .iter()
            .map(|(key, value)| (key.to_vec(), value.clone())),
    ))
}

fn metadata() -> (Vec<u8>, String) {
    let info = dictionary(&[
        (b"name", B::Bytes(b"Pack".to_vec())),
        (b"piece length", B::Int(16384)),
        (b"pieces", B::Bytes(vec![7; 20])),
        (
            b"files",
            B::List(vec![dictionary(&[
                (b"length", B::Int(4096)),
                (b"path", B::List(vec![B::Bytes(b"shared.mp4".to_vec())])),
            ])]),
        ),
    ]);
    let hash = crate::crypto::sha1(&bencode::encode(&info))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    (bencode::encode(&dictionary(&[(b"info", info)])), hash)
}

fn accept(listener: &dyn NetworkListener) -> Box<dyn NetworkStream> {
    let end = deadline();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < end, "pack inspection never connected");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("virtual pack listener: {error}"),
        }
    }
}

fn request(stream: &mut dyn NetworkStream) -> Vec<u8> {
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
        assert!(request.len() <= 8192);
    }
    assert!(request.starts_with(b"GET /"));
    request
}

fn response(body: &[u8]) -> Vec<u8> {
    let mut reply =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    reply.extend_from_slice(body);
    reply
}

fn server(network: &dyn Network, replies: Vec<Vec<u8>>) -> (u16, JoinHandle<Vec<Vec<u8>>>) {
    let listener = network.listen("0.0.0.0:0".parse().unwrap()).unwrap();
    let port = listener.port();
    let worker = thread::spawn(move || {
        replies
            .into_iter()
            .map(|reply| {
                let mut stream = accept(&*listener);
                let request = request(&mut *stream);
                stream.write_all(&reply).unwrap();
                // Keep the virtual flow alive until its client has consumed the body.
                let _ = stream.read(&mut [0]);
                request
            })
            .collect()
    });
    (port, worker)
}

fn query(port: u16) -> SharedFileRequest {
    SharedFileRequest {
        source_url: format!("http://first.example:{port}/pack.torrent?token=synthetic-pack-secret"),
        file_path: "Pack/shared.mp4".into(),
        season: 1,
        episodes: vec![1, 2],
        apply: false,
        plan_id: None,
    }
}

fn no_acquisition(engine: &Engine) {
    assert!(lock(&engine.store).unwrap().list().unwrap().is_empty());
    assert_eq!(
        engine.transfers().unwrap_err(),
        "Native downloads are disabled"
    );
    assert!(!engine.config.downloads.data_dir.exists());
    assert!(!engine.config.downloads.state_dir.exists());
    assert!(!engine.config.series_root.exists());
}

#[test]
fn shared_pack_preview_and_guarded_apply_use_tunnel_metadata_without_workers() {
    let directory = Directory::new();
    let pair = Pair::new();
    let (engine, series) = engine(&directory, pair.client_network.clone());
    let (metadata, hash) = metadata();
    let (port, worker) = server(&*pair.server_network, vec![response(&metadata); 2]);
    let redirect = format!("HTTP/1.1 302 Found\r\nLocation: http://metadata.example:{port}/source.torrent\r\nContent-Length: 0\r\n\r\n").into_bytes();
    let (redirect_port, redirect_worker) = server(&*pair.server_network, vec![redirect; 2]);
    let mut query = query(redirect_port);
    let before = lock(&engine.series_store).unwrap().get(&series).unwrap();
    let preview = engine
        .shared_file_before(&series, &query, deadline())
        .unwrap();
    assert_eq!(preview.get("new_owners").and_then(Value::as_u64), Some(2));
    assert_eq!(preview.get("submitted").and_then(Value::as_u64), Some(0));
    assert_eq!(
        preview
            .get("binding")
            .unwrap()
            .get("torrent_id")
            .and_then(Value::as_str),
        Some(hash.as_str())
    );
    assert!(!crate::json::stringify(&preview).contains("synthetic-pack-secret"));
    no_acquisition(&engine);
    assert_eq!(
        lock(&engine.series_store).unwrap().get(&series).unwrap(),
        before
    );

    query.apply = true;
    query.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    let applied = engine
        .shared_file_before(&series, &query, deadline())
        .unwrap();
    assert_eq!(applied.get("submitted").and_then(Value::as_u64), Some(2));
    let jobs = lock(&engine.store).unwrap().list().unwrap();
    assert_eq!(jobs.len(), 2);
    assert!(
        jobs.iter()
            .all(|job| job.state == "queued" && job.attempts == 0 && job.download_id.is_none())
    );
    assert!(
        jobs.iter()
            .all(|job| job.shared_file.as_ref().unwrap().torrent_id == hash)
    );
    assert_eq!(
        jobs.iter()
            .map(|job| job.request.episode)
            .collect::<std::collections::BTreeSet<_>>(),
        [1, 2].into()
    );
    assert_eq!(
        engine.transfers().unwrap_err(),
        "Native downloads are disabled"
    );
    assert!(!engine.config.downloads.data_dir.exists());
    assert!(!engine.config.downloads.state_dir.exists());
    assert_eq!(worker.join().unwrap().len(), 2);
    assert_eq!(redirect_worker.join().unwrap().len(), 2);
    assert!(pair.dns.load(Ordering::Relaxed) >= 4);
}

#[test]
fn cancellation_during_shared_pack_metadata_refuses_preview_apply_and_retry() {
    for apply in [false, true] {
        cancel_pending_inspection(apply);
    }
}

fn cancel_pending_inspection(apply: bool) {
    let directory = Directory::new();
    let pair = Pair::new();
    let (engine, series) = engine(&directory, pair.client_network.clone());
    let (metadata, _) = metadata();
    let listener = pair
        .server_network
        .listen("0.0.0.0:0".parse().unwrap())
        .unwrap();
    let mut query = query(listener.port());
    let (entered, waiting) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        // First fetch supplies a valid preview guard; the second fetch is held.
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
    let preview = engine
        .shared_file_before(&series, &query, deadline())
        .unwrap();
    no_acquisition(&engine);
    query.apply = apply;
    query.plan_id = apply.then(|| preview.get("plan_id").unwrap().as_str().unwrap().into());
    let before = lock(&engine.series_store).unwrap().get(&series).unwrap();
    let pending = {
        let (engine, series, query) = (engine.clone(), series.clone(), query.clone());
        thread::spawn(move || engine.shared_file_before(&series, &query, deadline()))
    };
    waiting.recv_timeout(Duration::from_secs(10)).unwrap();
    pair.client.cancel();
    release.send(()).unwrap();
    assert!(
        pending.join().unwrap().is_err(),
        "late response published a preview or committed owners"
    );
    assert!(
        engine
            .shared_file_before(&series, &query, deadline())
            .is_err()
    );
    query.apply = false;
    query.plan_id = None;
    assert!(
        engine
            .shared_file_before(&series, &query, deadline())
            .is_err()
    );
    no_acquisition(&engine);
    assert_eq!(
        lock(&engine.series_store).unwrap().get(&series).unwrap(),
        before
    );
    server.join().unwrap();
    assert!(pair.dns.load(Ordering::Relaxed) >= 2);
}
