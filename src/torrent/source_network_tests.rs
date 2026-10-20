use super::*;
use crate::net::{HttpClient, HttpConnector, NetworkDatagram, NetworkStream};
use std::{collections::VecDeque, io, net::TcpListener, time::Duration};

#[derive(Default)]
struct Fake {
    resolved: Mutex<Vec<(String, Instant)>>,
    answers: Vec<SocketAddr>,
    late: bool,
    http: Arc<Http>,
}

#[derive(Default)]
struct Http {
    calls: Mutex<Vec<(String, u16, Instant)>>,
    replies: Mutex<VecDeque<Vec<u8>>>,
}

struct Stream(io::Cursor<Vec<u8>>);

impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl NetworkStream for Stream {
    fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
    fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
}

impl HttpConnector for Http {
    fn connect(&self, host: &str, port: u16, deadline: Instant) -> Result<Box<dyn NetworkStream>> {
        self.calls
            .lock()
            .unwrap()
            .push((host.into(), port, deadline));
        let reply = self.replies.lock().unwrap().pop_front().ok_or("Denied")?;
        Ok(Box::new(Stream(io::Cursor::new(reply))))
    }
}

struct Listener;

impl transport::NetworkListener for Listener {
    fn accept(&self) -> io::Result<(Box<dyn NetworkStream>, SocketAddr)> {
        Err(io::ErrorKind::WouldBlock.into())
    }
    fn port(&self) -> u16 {
        6881
    }
}

impl transport::Network for Fake {
    fn connect(&self, _: SocketAddr, _: Duration) -> Result<Box<dyn NetworkStream>> {
        Err("No peers in this fixture".into())
    }
    fn resolve(&self, authority: &str, deadline: Instant) -> Result<Vec<SocketAddr>> {
        self.resolved
            .lock()
            .unwrap()
            .push((authority.into(), deadline));
        if self.late {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        }
        if self.answers.is_empty() {
            Err("Denied".into())
        } else {
            Ok(self.answers.clone())
        }
    }
    fn datagram(&self, _: bool) -> Result<Box<dyn NetworkDatagram>> {
        Err("No datagrams in this fixture".into())
    }
    fn listen(&self, _: SocketAddr) -> Result<Box<dyn transport::NetworkListener>> {
        Ok(Box::new(Listener))
    }
    fn http(&self) -> HttpClient {
        HttpClient::new().with_connector(self.http.clone())
    }
}

fn end() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

fn magnet(suffix: &str) -> String {
    format!("magnet:?xt=urn:btih:{}{suffix}", "1".repeat(40))
}

fn metadata() -> Vec<u8> {
    crate::bencode::encode(&Value::Dict(BTreeMap::from([(
        b"info".to_vec(),
        Value::Dict(BTreeMap::from([
            (b"name".to_vec(), Value::Bytes(b"empty.bin".to_vec())),
            (b"length".to_vec(), Value::Int(0)),
            (b"piece length".to_vec(), Value::Int(16_384)),
            (b"pieces".to_vec(), Value::Bytes(Vec::new())),
        ])),
    )])))
}

fn response(body: &[u8]) -> Vec<u8> {
    let mut reply =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    reply.extend_from_slice(body);
    reply
}

#[test]
fn remote_inspection_uses_the_network_across_redirects_and_bounds_its_deadline() {
    let network = Fake::default();
    network.http.replies.lock().unwrap().extend([
        b"HTTP/1.1 302 Found\r\nLocation: http://second.invalid/final\r\nContent-Length: 0\r\n\r\n"
            .to_vec(),
        response(&metadata()),
    ]);
    let deadline = end();
    let report = inspect_metadata(&network, "http://first.invalid/source", deadline).unwrap();
    assert_eq!(report.files[0].path, "empty.bin");
    let calls = network.http.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "first.invalid");
    assert_eq!(calls[1].0, "second.invalid");
    assert_eq!(calls[0].2, calls[1].2);
    // HttpClient starts its relative timeout after Source calculates it.
    assert!(calls[0].2 <= deadline + Duration::from_millis(10));
    assert!(network.resolved.lock().unwrap().is_empty());
}

#[test]
fn denied_remote_http_and_https_never_fall_back_to_a_direct_trap() {
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let network = Fake::default();
    for scheme in ["http", "https"] {
        let url = format!("{scheme}://{}/private", trap.local_addr().unwrap());
        assert_eq!(
            inspect_metadata(&network, &url, end()).unwrap_err(),
            "Torrent source metadata could not be read or authenticated"
        );
    }
    assert_eq!(network.http.calls.lock().unwrap().len(), 2);
    assert!(matches!(trap.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    assert!(inspect_metadata(&network, "http://expired.invalid/", Instant::now()).is_err());
    assert_eq!(network.http.calls.lock().unwrap().len(), 2);
}

#[test]
fn restored_magnet_names_and_ip_families_resolve_only_through_the_active_network() {
    let source = Source::parse_magnet_with_deadline(
        &magnet("&x.pe=peer.invalid:6881&x.pe=192.0.2.1:6881&x.pe=[2001:db8::1]:6881"),
        None,
    )
    .unwrap();
    let network = Fake {
        answers: vec![
            "198.51.100.7:6881".parse().unwrap(),
            "[2001:db8::7]:6881".parse().unwrap(),
        ],
        ..Fake::default()
    };
    let deadline = end();
    let addresses = source.peer_addresses(&network, deadline).unwrap();
    assert_eq!(addresses.len(), 6);
    let calls = network.resolved.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["peer.invalid:6881", "192.0.2.1:6881", "[2001:db8::1]:6881"]
    );
    assert!(calls.iter().all(|(_, at)| *at == deadline));
    assert!(source.peer_addresses(&Fake::default(), end()).is_err());
}

#[test]
fn magnet_resolution_caps_answers_and_rejects_expired_and_late_results() {
    let text = magnet(&"&x.pe=peer.invalid:6881".repeat(256));
    let source = Source::parse_magnet_with_deadline(&text, None).unwrap();
    let network = Fake {
        answers: vec!["192.0.2.1:6881".parse().unwrap(); 16],
        ..Fake::default()
    };
    assert_eq!(source.peer_addresses(&network, end()).unwrap().len(), 256);
    assert_eq!(network.resolved.lock().unwrap().len(), 64);
    assert!(source.peer_addresses(&network, Instant::now()).is_err());
    assert_eq!(network.resolved.lock().unwrap().len(), 64);
    let network = Fake {
        late: true,
        answers: network.answers,
        ..Fake::default()
    };
    assert!(
        source
            .peer_addresses(&network, Instant::now() + Duration::from_millis(10))
            .is_err()
    );
    assert_eq!(network.resolved.lock().unwrap().len(), 1);
    assert!(
        Source::parse_magnet_with_deadline(&magnet(&"&x.pe=peer.invalid:6881".repeat(257)), None)
            .is_err()
    );
}

#[test]
fn malformed_magnet_authorities_are_rejected_without_resolution() {
    for authority in [
        "peer.invalid",
        "peer.invalid:0",
        "peer.invalid:65536",
        "peer.invalid:",
        "user@peer.invalid:6881",
        "peer.invalid:6881/path",
        "peer.invalid:6881?x",
        "peer.invalid:6881#x",
        "[::1:6881",
        "::1:6881",
        "bad host:6881",
    ] {
        assert!(
            Source::parse_magnet_with_deadline(&magnet(&format!("&x.pe={authority}")), None)
                .is_err(),
            "{authority}"
        );
    }
}

#[test]
fn transfer_admission_and_restart_keep_magnet_names_offline_and_use_custom_http() {
    let root = std::env::temp_dir().join(format!(
        "mynou-source-network-{}",
        hex(&crate::crypto::random_bytes::<16>().unwrap())
    ));
    let config = DownloadConfig {
        state_dir: root.join("state"),
        data_dir: root.join("data"),
        listen_port: 0,
        seed: false,
        dht: false,
        pex: false,
        max_active: 1,
        max_peers: 1,
    };
    let network = Arc::new(Fake::default());
    network
        .http
        .replies
        .lock()
        .unwrap()
        .push_back(response(&metadata()));
    let open = || {
        Client::open_with_policy_and_admission(
            config.clone(),
            TransferPolicy::default(),
            Arc::new(|_| Ok(false)),
            network.clone(),
        )
        .unwrap()
    };
    let client = open();
    let remote = client.ensure("http://metadata.invalid/file").unwrap();
    let pending = client
        .ensure(&magnet("&x.pe=offline.invalid:6881"))
        .unwrap();
    assert_eq!(network.http.calls.lock().unwrap().len(), 1);
    assert!(network.resolved.lock().unwrap().is_empty());
    drop(client);
    let client = open();
    let mut jobs = client.jobs.lock().unwrap();
    client.load(&mut jobs, &pending.id).unwrap();
    client.load(&mut jobs, &remote.id).unwrap();
    assert_eq!(jobs[&pending.id].source.peers, ["offline.invalid:6881"]);
    assert_eq!(network.http.calls.lock().unwrap().len(), 1);
    assert!(network.resolved.lock().unwrap().is_empty());
    let cancel = jobs[&pending.id].cancel.clone();
    drop(jobs);
    assert_eq!(
        download_sequential(
            &client.config,
            &client.jobs,
            &cancel,
            &pending.id,
            &[1; 20],
            &client.policy
        )
        .unwrap_err(),
        "Could not resolve magnet peer"
    );
    assert_eq!(
        parallel::download(
            &client.config,
            &client.jobs,
            &cancel,
            &pending.id,
            &[1; 20],
            &client.policy
        )
        .unwrap_err(),
        "Could not resolve magnet peer"
    );
    assert_eq!(
        inspect_metadata(&*network, &magnet("&x.pe=offline.invalid:6881"), end()).unwrap_err(),
        "Could not resolve magnet peer"
    );
    assert_eq!(network.resolved.lock().unwrap().len(), 3);
    drop(client);
    fs::remove_dir_all(root).unwrap();
}
