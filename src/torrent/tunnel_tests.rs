//! Real torrent protocol paths over two native WireGuard owners. No host peer,
//! tracker, DNS service or HTTP server is bound by these fixtures.
use super::*;
use crate::{bencode, config, engine::Engine, tunnel_http::tests::Pair};
use std::io;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mynou-torrent-tunnel-{}",
            hex(&crate::crypto::random_bytes::<8>().unwrap())
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
fn end() -> Instant {
    Instant::now() + Duration::from_secs(15)
}
fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = end();
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "torrent tunnel fixture timed out"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn configuration(root: &Path, seed: bool) -> crate::config::Config {
    let mut value = config::default_json();
    let downloads = value.get_mut("downloads").unwrap();
    downloads.insert("enabled", true);
    downloads.insert("listen_port", 0_u32);
    downloads.insert("dht", false);
    downloads.insert("pex", true);
    downloads.insert("seed", seed);
    downloads.insert("max_peers", 2_u32);
    config::from_json(&value, root).unwrap()
}
fn metadata() -> (Vec<u8>, String, Vec<u8>) {
    let payload: Vec<_> = (0..24_000).map(|n| (n % 251) as u8).collect();
    let pieces = payload
        .chunks(BLOCK)
        .flat_map(crate::crypto::sha1)
        .collect();
    let info = discovery::value_dict(&[
        (b"name", Value::Bytes(b"fixture.bin".to_vec())),
        (b"length", Value::Int(payload.len() as i64)),
        (b"piece length", Value::Int(BLOCK as i64)),
        (b"pieces", Value::Bytes(pieces)),
        (b"private", Value::Int(0)),
    ]);
    let hash = hex(&crate::crypto::sha1(&bencode::encode(&info)));
    let encoded = bencode::encode(&discovery::value_dict(&[(b"info", info)]));
    (encoded, hash, payload)
}
fn accept(listener: &dyn transport::NetworkListener) -> Box<dyn crate::net::NetworkStream> {
    let deadline = end();
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
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("virtual listener: {e}"),
        }
    }
}
fn http_server(
    network: &dyn transport::Network,
    replies: Vec<Vec<u8>>,
) -> (u16, JoinHandle<Vec<Vec<u8>>>) {
    let listener = network.listen("0.0.0.0:0".parse().unwrap()).unwrap();
    let port = listener.port();
    let worker = thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let mut stream = accept(&*listener);
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() <= 65_536);
            }
            requests.push(request);
            stream.write_all(&reply).unwrap();
            stream.flush().unwrap();
            // Keep the virtual flow alive until the client consumes the body.
            // Dropping a NetworkStream aborts its flow rather than half-closing.
            let _ = stream.read(&mut [0]);
        }
        requests
    });
    (port, worker)
}
fn response(body: &[u8]) -> Vec<u8> {
    let mut bytes =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

#[test]
fn real_magnet_metadata_payload_and_inbound_seeding_use_only_the_native_network() {
    for max_peers in [1, 2] {
        let pair = Pair::new();
        let directory = Directory::new();
        let (encoded, hash, payload) = metadata();
        let path = directory.0.join("source.torrent");
        fs::write(&path, &encoded).unwrap();
        let mut cfg = configuration(&directory.0.join("seed"), true).downloads;
        cfg.max_peers = max_peers;
        let base = cfg.data_dir.join(&hash);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("fixture.bin"), &payload).unwrap();
        let seed = Client::open_with_policy_and_admission(
            cfg,
            TransferPolicy::default(),
            Arc::new(|_| Ok(true)),
            pair.server_network.clone(),
        )
        .unwrap();
        seed.ensure(path.to_str().unwrap()).unwrap();
        wait(|| seed.check(&hash).unwrap().ready);
        let magnet = format!(
            "magnet:?xt=urn:btih:{hash}&x.pe=peer.example:{}",
            seed.listen_port()
        );
        let report = inspect_metadata(&*pair.client_network, &magnet, end()).unwrap();
        assert_eq!(report.id, hash);
        assert_eq!(report.files[0].length, payload.len() as u64);
        assert!(pair.dns.load(Ordering::Relaxed) > 0);
        let mut cfg = configuration(&directory.0.join("download"), false);
        cfg.downloads.max_peers = max_peers;
        let engine =
            Engine::open_with_network(cfg.clone(), pair.client_network.clone(), true).unwrap();
        assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
        let job = engine
            .submit(crate::store::Request {
                kind: "movie".into(),
                title: "Synthetic tunnel payload".into(),
                year: 2024,
                season: 0,
                episode: 0,
                tmdb_id: Some(42),
                source_url: Some(magnet.clone()),
                source_path: None,
                source_numbering: None,
            })
            .unwrap()
            .remove(0);
        engine.tick().unwrap();
        wait(|| {
            engine.transfer(&hash).unwrap().get("ready") == Some(&crate::json::Value::Bool(true))
        });
        let downloaded = cfg.downloads.data_dir.join(&hash).join("fixture.bin");
        assert_eq!(fs::read(&downloaded).unwrap(), payload);
        assert_eq!(
            crate::engine::lock(&engine.store)
                .unwrap()
                .get(&job.id)
                .unwrap()
                .unwrap()
                .download_id
                .as_deref(),
            Some(hash.as_str())
        );
        // The seed accepted an inbound BitTorrent connection and supplied data.
        assert!(
            seed.jobs
                .lock()
                .unwrap()
                .get(&hash)
                .unwrap()
                .counters
                .uploaded
                .load(Ordering::Relaxed)
                > 0
        );
        pair.client.cancel();
        assert!(inspect_metadata(&*pair.client_network, path.to_str().unwrap(), end()).is_err());
        assert_eq!(fs::read(&downloaded).unwrap(), payload);
        drop(engine);
        assert!(Engine::open_with_network(cfg, pair.client_network.clone(), true).is_err());
    }
}

#[test]
fn torrent_http_tracker_and_redirected_remote_metadata_use_virtual_dns_and_http() {
    let pair = Pair::new();
    let (encoded, hash, _) = metadata();
    let tracker = bencode::encode(&discovery::value_dict(&[
        (b"interval", Value::Int(1800)),
        (b"peers", Value::Bytes(vec![10, 7, 0, 1, 0x1a, 0xe1])),
    ]));
    let (port, worker) = http_server(
        &*pair.server_network,
        vec![response(&tracker), response(&encoded)],
    );
    let request = discovery::TrackerRequest {
        hash: &[7; 20],
        peer_id: &[8; 20],
        port: 6881,
        left: 1,
        uploaded: 0,
        downloaded: 0,
        event: discovery::TrackerEvent::Stopped,
    };
    let reply = discovery::tracker_before(
        &*pair.client_network,
        &format!("http://tracker.example:{port}/announce"),
        &request,
        end(),
    )
    .unwrap();
    assert_eq!(
        reply.peers,
        ["10.7.0.1:6881".parse::<SocketAddr>().unwrap()]
    );
    let (redirect, redir_worker) = http_server(&*pair.server_network, vec![format!("HTTP/1.1 302 Found\r\nLocation: http://metadata.example:{port}/source\r\nContent-Length: 0\r\n\r\n").into_bytes()]);
    let report = inspect_metadata(
        &*pair.client_network,
        &format!("http://first.example:{redirect}/source"),
        end(),
    )
    .unwrap();
    assert_eq!(report.id, hash);
    let requests = worker.join().unwrap();
    assert!(String::from_utf8_lossy(&requests[0]).contains("event=stopped"));
    redir_worker.join().unwrap();
    assert!(pair.dns.load(Ordering::Relaxed) >= 3);
}

#[test]
fn real_udp_tracker_and_dht_get_peers_and_announce_stay_inside_the_tunnel() {
    let pair = Pair::new();
    // An adapter socket uses an ephemeral virtual port, visible only in-tunnel.
    let driver = pair.server.clone();
    let (port, worker) = crate::tunnel_http::tests::udp_protocol_server(&driver);
    let request = discovery::TrackerRequest {
        hash: &[7; 20],
        peer_id: &[8; 20],
        port: 6881,
        left: 1,
        uploaded: 0,
        downloaded: 0,
        event: discovery::TrackerEvent::Stopped,
    };
    let reply = discovery::tracker_before(
        &*pair.client_network,
        &format!("udp://tracker.example:{port}/announce"),
        &request,
        end(),
    )
    .unwrap();
    assert_eq!(
        reply.peers,
        ["10.7.0.1:6881".parse::<SocketAddr>().unwrap()]
    );
    let peers = discovery::dht_lookup(
        &*pair.client_network,
        &[7; 20],
        6881,
        true,
        &AtomicBool::new(false),
        &[SocketAddr::from(([10, 7, 0, 1], port))],
    )
    .unwrap();
    assert_eq!(peers, reply.peers);
    worker.join().unwrap();
}

#[test]
fn https_torrent_tracker_authenticates_the_host_over_the_native_tunnel() {
    const CHILD: &str = "MYNOU_TORRENT_TUNNEL_HTTPS_FIXTURE";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "torrent::tunnel_tests::https_torrent_tracker_authenticates_the_host_over_the_native_tunnel", "--nocapture"])
            .env(CHILD, "1")
            .env("MYNOU_CA_FILE", Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pki/fixtures/rsa-root.pem"))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let pair = Pair::new();
    let listener = pair
        .server_network
        .listen("0.0.0.0:0".parse().unwrap())
        .unwrap();
    let port = listener.port();
    let worker = thread::spawn(move || {
        let mut stream = accept(&*listener);
        let tracker = bencode::encode(&discovery::value_dict(&[
            (b"interval", Value::Int(1800)),
            (b"peers", Value::Bytes(Vec::new())),
        ]));
        crate::tls::tests::serve_fixture(
            &mut stream,
            crate::tls::tests::Fault::HttpOk,
            Some((b"GET /announce?", &response(&tracker))),
        )
        .unwrap();
        let _ = stream.read(&mut [0]);
    });
    // DNS maps localhost to the synthetic remote tunnel IPv4, not host loopback.
    let request = discovery::TrackerRequest {
        hash: &[7; 20],
        peer_id: &[8; 20],
        port: 6881,
        left: 1,
        uploaded: 0,
        downloaded: 0,
        event: discovery::TrackerEvent::Stopped,
    };
    let result = discovery::tracker_before(
        &*pair.client_network,
        &format!("https://localhost:{port}/announce"),
        &request,
        end(),
    )
    .unwrap();
    assert!(result.peers.is_empty());
    worker.join().unwrap();
    assert!(pair.dns.load(Ordering::Relaxed) > 0);
}
