//! IRC-triggered metadata lookup through native tunnel owners (#323).
//! The IRC connection is ordinary-path by design and is the only declared
//! host service; the pinned magnet's only peer is inside the tunnel.
use crate::{
    bencode::{self, Value as B},
    config,
    engine::{Engine, lock},
    json::{self, Value},
    net::NetworkStream,
    pack::tunnel_tests::{Directory, accept, dictionary},
    store::Request,
    torrent::transport::NetworkListener,
    tunnel_http::tests::{Pair, allow_service},
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const SENDER: &str = "announce!bot@fixture";
const FILE: &str = "Fixture.Movie.2024.1080p.mp4";
/// The server owner's virtual address: a literal peer reachable only inside.
const PEER: &str = "10.7.0.1";

fn wait(mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < end, "IRC tunnel fixture timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

/// A single-file movie torrent; distinct markers give distinct ids.
fn movie(marker: u8) -> (Vec<u8>, String) {
    let info = bencode::encode(&dictionary(&[
        (b"length", B::Int(4096)),
        (b"name", B::Bytes(FILE.as_bytes().to_vec())),
        (b"piece length", B::Int(16384)),
        (b"pieces", B::Bytes(vec![marker; 20])),
    ]));
    let hash = crate::crypto::sha1(&info)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    (info, hash)
}

fn message(stream: &mut dyn NetworkStream, id: u8, payload: &[u8]) {
    let length = u32::try_from(payload.len() + 1).unwrap().to_be_bytes();
    stream.write_all(&length).unwrap();
    stream.write_all(&[id]).unwrap();
    stream.write_all(payload).unwrap();
}

/// Minimal BEP 9 metadata peer on the server owner. With a hold, it signals
/// once the metadata request arrives and answers only after release.
struct Peer {
    port: u16,
    connections: Arc<AtomicUsize>,
    worker: Option<JoinHandle<()>>,
}
impl Peer {
    fn open(
        listener: Box<dyn NetworkListener>,
        info: Vec<u8>,
        mut hold: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
    ) -> Self {
        let port = listener.port();
        let connections = Arc::new(AtomicUsize::new(0));
        let counted = connections.clone();
        let worker = thread::spawn(move || {
            let mut stream = accept(&*listener);
            counted.fetch_add(1, Ordering::AcqRel);
            let mut handshake = [0; 68];
            stream.read_exact(&mut handshake).unwrap();
            assert_eq!(&handshake[1..20], b"BitTorrent protocol");
            let mut reply = handshake;
            reply[20..28].fill(0);
            reply[25] = 0x10;
            reply[48..].fill(7);
            stream.write_all(&reply).unwrap();
            let mut extensions = vec![0];
            extensions.extend(bencode::encode(&dictionary(&[
                (b"m", dictionary(&[(b"ut_metadata", B::Int(1))])),
                (b"metadata_size", B::Int(info.len() as i64)),
            ])));
            message(&mut *stream, 20, &extensions);
            loop {
                let mut length = [0; 4];
                stream.read_exact(&mut length).unwrap();
                let mut payload = vec![0; u32::from_be_bytes(length) as usize];
                stream.read_exact(&mut payload).unwrap();
                // A metadata request to our advertised ut_metadata id.
                if payload.first() == Some(&20) && payload.get(1) == Some(&1) {
                    break;
                }
            }
            let delayed = hold.take();
            if let Some((entered, released)) = &delayed {
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            let mut data = vec![crate::torrent::wire::EXT_METADATA];
            data.extend(bencode::encode(&dictionary(&[
                (b"msg_type", B::Int(1)),
                (b"piece", B::Int(0)),
                (b"total_size", B::Int(info.len() as i64)),
            ])));
            data.extend_from_slice(&info);
            let length = u32::try_from(data.len() + 1).unwrap().to_be_bytes();
            let result = stream
                .write_all(&length)
                .and_then(|()| stream.write_all(&[20]))
                .and_then(|()| stream.write_all(&data));
            if delayed.is_none() {
                result.unwrap();
            }
            // Keep the virtual flow alive until the client has consumed it.
            let _ = stream.read(&mut [0]);
        });
        Self {
            port,
            connections,
            worker: Some(worker),
        }
    }
    fn join(mut self) -> usize {
        self.worker.take().unwrap().join().unwrap();
        self.connections.load(Ordering::Acquire)
    }
}

/// Synthetic IRC server on a literal, declared loopback port. It registers one
/// client, then relays each announcement it is handed until the client closes.
struct Irc {
    port: u16,
    connections: Arc<AtomicUsize>,
    announcements: mpsc::Sender<Value>,
    worker: Option<JoinHandle<()>>,
}
fn line(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n") {
        assert!(bytes.len() < 512);
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes[..bytes.len() - 2].to_vec()).unwrap()
}
fn send(stream: &mut TcpStream, text: &str) {
    stream.write_all(format!("{text}\r\n").as_bytes()).unwrap();
}
impl Irc {
    fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        allow_service(address);
        listener.set_nonblocking(true).unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let (announcements, pending) = mpsc::channel::<Value>();
        let counted = connections.clone();
        let worker = thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < end, "IRC client never connected");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("synthetic IRC listener: {error}"),
                }
            };
            counted.fetch_add(1, Ordering::AcqRel);
            // macOS keeps the listener's non-blocking mode on accepted sockets.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(line(&mut stream), "NICK mynou");
            assert_eq!(line(&mut stream), "USER mynou 0 * :Mynou");
            send(&mut stream, ":server 001 mynou :Welcome");
            assert_eq!(line(&mut stream), "JOIN #announces");
            send(&mut stream, ":mynou!user@fixture JOIN :#announces");
            // The sender closes after the fixture is done; then wait for EOF.
            for announcement in pending {
                send(
                    &mut stream,
                    &format!(
                        ":{SENDER} PRIVMSG #announces :MYNOU {}",
                        json::stringify(&announcement)
                    ),
                );
            }
            assert!(matches!(stream.read(&mut [0]), Ok(0) | Err(_)));
        });
        Self {
            port: address.port(),
            connections,
            announcements,
            worker: Some(worker),
        }
    }
    fn close(mut self) -> usize {
        drop(self.announcements);
        self.worker.take().unwrap().join().unwrap();
        self.connections.load(Ordering::Acquire)
    }
}

fn announcement(title: &str, hash: &str) -> Value {
    let mut value = Value::object();
    value.insert("title", title);
    value.insert("media_title", "Fixture Movie");
    value.insert("kind", "movie");
    value.insert("year", 2024_u32);
    value.insert("tmdb_id", "7");
    value.insert("info_hash", hash);
    value
}

/// Enabled downloads with a `grab` rule, but deliberately no download workers.
/// Only the live IRC client thread is started.
fn engine(
    directory: &Directory,
    pair: &Pair,
    irc: &Irc,
    peer: u16,
) -> (Arc<Engine>, String, Vec<JoinHandle<()>>) {
    let mut settings = config::default_json();
    settings
        .get_mut("downloads")
        .unwrap()
        .insert("enabled", true);
    let mut source = Value::object();
    source.insert("id", "local");
    source.insert("url", format!("irc://127.0.0.1:{}", irc.port));
    source.insert("enabled", true);
    source.insert("nickname", "mynou");
    source.insert("channel", "#announces");
    source.insert("sender", SENDER);
    source.insert("reconnect_min_secs", 1_u32);
    source.insert("reconnect_max_secs", 1_u32);
    source.insert(
        "magnet_template",
        format!("magnet:?xt={{xt}}&x.pe={PEER}:{peer}"),
    );
    let mut rule = Value::object();
    rule.insert("id", "movies");
    rule.insert("source", "local");
    rule.insert("enabled", true);
    rule.insert("kind", "movie");
    rule.insert("profile", "any");
    rule.insert("action", "grab");
    let mut irc_settings = Value::object();
    irc_settings.insert("sources", Value::Array(vec![source]));
    irc_settings.insert("rules", Value::Array(vec![rule]));
    settings.insert("irc", irc_settings);
    let config = config::from_json(&settings, &directory.0).unwrap();
    let engine = Engine::open_with_network(config, pair.client_network.clone(), false).unwrap();
    let jobs = engine
        .submit(Request {
            kind: "movie".into(),
            title: "Fixture Movie".into(),
            year: 2024,
            season: 0,
            episode: 0,
            source_path: None,
            source_url: None,
            tmdb_id: Some(7),
            source_numbering: None,
        })
        // Discard the error without any panic or log sink: configuration
        // errors may name credential variables, so CodeQL treats them as secret.
        .unwrap_or_default();
    assert!(jobs.len() == 1, "Synthetic IRC request submission failed");
    let job = jobs[0].id.clone();
    let mut handles = Vec::new();
    crate::irc::client::start(&engine, &mut handles);
    (engine, job, handles)
}

fn received(engine: &Engine, count: usize) -> Vec<Value> {
    wait(|| {
        engine
            .irc_announcements(0, 200)
            .unwrap()
            .get("announcements")
            .unwrap()
            .as_array()
            .unwrap()
            .len()
            == count
    });
    engine
        .irc_announcements(0, 200)
        .unwrap()
        .get("announcements")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec()
}

fn phase(engine: &Engine, row: &Value) -> Option<String> {
    let id = row.get("id").unwrap().as_str().unwrap();
    engine
        .irc_announcement(id)
        .unwrap()
        .get("route")
        .and_then(|route| route.get("phase"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn no_transfer(engine: &Engine) {
    assert_eq!(
        engine.transfers().unwrap_err(),
        "Native downloads are disabled"
    );
    assert!(!engine.config.downloads.data_dir.exists());
    assert!(!engine.config.downloads.state_dir.exists());
}

fn stop(engine: &Engine, handles: Vec<JoinHandle<()>>, irc: Irc) {
    engine.stopped.store(true, Ordering::Release);
    for handle in handles {
        handle.join().unwrap();
    }
    // Exactly one ordinary connection: the declared IRC client session.
    assert_eq!(irc.close(), 1);
}

#[test]
fn irc_announcement_routes_through_tunnel_metadata_without_workers() {
    let directory = Directory::new();
    let pair = Pair::new();
    let (info, hash) = movie(5);
    let listener = pair
        .server_network
        .listen("0.0.0.0:0".parse().unwrap())
        .unwrap();
    let peer = Peer::open(listener, info, None);
    let irc = Irc::open();
    let (engine, job, handles) = engine(&directory, &pair, &irc, peer.port);
    irc.announcements
        .send(announcement("Fixture.Movie.2024.1080p.WEB-DL.x264", &hash))
        .unwrap();
    let rows = received(&engine, 1);
    assert_eq!(phase(&engine, &rows[0]), None);

    let report = engine.irc_route_pending().unwrap();
    assert_eq!(
        report.get("outcome").and_then(Value::as_str),
        Some("routed"),
        "{report:?}"
    );
    assert!(!json::stringify(&report).contains("magnet:?"));
    assert_eq!(phase(&engine, &rows[0]).as_deref(), Some("routed"));
    let routed = lock(&engine.store).unwrap().get(&job).unwrap().unwrap();
    let origin = routed.irc_origin.as_ref().unwrap();
    assert_eq!(origin.torrent_id, hash);
    assert_eq!((origin.file.as_str(), origin.file_length), (FILE, 4096));
    assert_eq!(routed.state, "queued");
    assert!(routed.download_id.is_none() && routed.attempts == 0);
    assert!(
        routed
            .acquisition_url
            .as_deref()
            .unwrap()
            .ends_with(&format!("&x.pe={PEER}:{}", peer.port))
    );
    no_transfer(&engine);
    assert_eq!(peer.join(), 1);
    stop(&engine, handles, irc);
}

#[test]
fn cancellation_during_irc_metadata_refuses_routing_and_later_announcements() {
    let directory = Directory::new();
    let pair = Pair::new();
    let (info, hash) = movie(6);
    let listener = pair
        .server_network
        .listen("0.0.0.0:0".parse().unwrap())
        .unwrap();
    let (entered, waiting) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let peer = Peer::open(listener, info, Some((entered, released)));
    let irc = Irc::open();
    let (engine, job, handles) = engine(&directory, &pair, &irc, peer.port);
    let before = lock(&engine.store).unwrap().get(&job).unwrap().unwrap();
    irc.announcements
        .send(announcement("Fixture.Movie.2024.1080p.WEB-DL.x264", &hash))
        .unwrap();
    let first = received(&engine, 1);
    let pending = {
        let engine = engine.clone();
        thread::spawn(move || engine.irc_route_pending())
    };
    waiting.recv_timeout(Duration::from_secs(10)).unwrap();
    pair.client.cancel();
    release.send(()).unwrap();
    let report = pending.join().unwrap().unwrap();
    // The held exchange fails as unavailable metadata; nothing is reserved.
    assert_eq!(
        report.get("outcome").and_then(Value::as_str),
        Some("metadata_unavailable"),
        "late metadata reserved or routed the announcement: {report:?}"
    );
    assert_eq!(peer.join(), 1);

    // A later release (another info hash, so another claim) over the same live
    // IRC session fails before any metadata attempt and never falls back to
    // the ordinary network.
    let (_, later) = movie(7);
    irc.announcements
        .send(announcement("Fixture.Movie.2024.1080p.BluRay.x264", &later))
        .unwrap();
    let rows = received(&engine, 2);
    let started = Instant::now();
    let report = engine.irc_route_pending().unwrap();
    // Refused at the cancelled transport's publication check, before metadata.
    assert_eq!(
        report.get("outcome").and_then(Value::as_str),
        Some("admission_or_storage_rejected"),
        "{report:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    for row in rows.iter().chain(&first) {
        assert_eq!(phase(&engine, row), None);
    }
    assert_eq!(
        lock(&engine.store).unwrap().get(&job).unwrap().unwrap(),
        before
    );
    no_transfer(&engine);
    stop(&engine, handles, irc);
}
