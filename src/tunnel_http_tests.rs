use super::*;
use crate::{crypto::x25519_public_key, net::HttpClient, netstack::Ipv4Config};
use std::{
    collections::BTreeMap,
    net::{SocketAddr, UdpSocket},
    sync::atomic::AtomicUsize,
};

const CLIENT: Ipv4Addr = Ipv4Addr::new(10, 7, 0, 2);
const SERVER: Ipv4Addr = Ipv4Addr::new(10, 7, 0, 1);
const CLIENT_KEY: [u8; 32] = [13; 32];
const SERVER_KEY: [u8; 32] = [27; 32];

/// Explicit fixture authorization for the optional macOS host-I/O observer.
/// One append per record also works across the isolated HTTPS subprocess.
pub(crate) fn allow_outer(address: SocketAddr) {
    use std::io::Write;
    if let Some(path) = std::env::var_os("MYNOU_HOST_IO_TRACE") {
        assert!(address.is_ipv4() && address.ip().is_loopback());
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        let line = format!(
            "{{\"op\":\"allow\",\"pid\":{},\"port\":{}}}\n",
            std::process::id(),
            address.port()
        );
        assert_eq!(file.write(line.as_bytes()).unwrap(), line.len());
    }
}
/// Declares an ordinary-path loopback service (e.g. an indexer, which never
/// enters the tunnel); the observer then allows only its listener and clients.
pub(crate) fn allow_service(address: SocketAddr) {
    use std::io::Write;
    if let Some(path) = std::env::var_os("MYNOU_HOST_IO_TRACE") {
        assert!(address.is_ipv4() && address.ip().is_loopback());
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        let line = format!(
            "{{\"op\":\"allow_tcp\",\"pid\":{},\"port\":{}}}\n",
            std::process::id(),
            address.port()
        );
        assert_eq!(file.write(line.as_bytes()).unwrap(), line.len());
    }
}
/// Declare the owned socket, never an address inferred from received traffic.
pub(crate) fn allow_driver_outer(driver: &HttpTunnel) {
    allow_outer(
        driver
            .owner
            .call(None, end(), |r| r.tunnel.local_addr().map_err(failed))
            .unwrap(),
    );
}
fn end() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn config(endpoint: SocketAddr, server: bool) -> TunnelConfig {
    let ip = Ipv4Config::new(
        if server { SERVER } else { CLIENT },
        Ipv4Addr::UNSPECIFIED,
        0,
    )
    .unwrap();
    let mut config = TunnelConfig::new(
        if server { SERVER_KEY } else { CLIENT_KEY },
        x25519_public_key(if server { CLIENT_KEY } else { SERVER_KEY }),
        Endpoint::Address(endpoint),
        ip,
    )
    .with_dns(vec![SERVER]);
    config.bind = "127.0.0.1:0".parse().unwrap();
    config.initiate = !server;
    config.psk = Some([51; 32]);
    config
}

struct Fixture {
    driver: HttpTunnel,
    connector: Arc<dyn HttpConnector>,
    stop: Arc<AtomicBool>,
    block: Arc<AtomicBool>,
    dns: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    wire: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Datagrams relayed toward the client, for replay probes.
    inbound: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Re-source server datagrams from `roamed`: the server endpoint roams.
    roam: Arc<AtomicBool>,
    roamed: SocketAddr,
    /// Client datagrams that arrived at the original and the roamed source.
    arrivals: Arc<[AtomicUsize; 2]>,
    /// Client listener ports the responder dials, then greets and echoes.
    dial: Arc<Mutex<Vec<u16>>>,
    worker: Option<JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_poll_interval(Duration::from_micros(200))
    }
    fn with_poll_interval(poll: Duration) -> Self {
        // The relay observes only WireGuard datagrams. All application/DNS
        // services are virtual; there is no host TCP listener at their ports.
        let relay = UdpSocket::bind("127.0.0.1:0").unwrap();
        relay.set_nonblocking(true).unwrap();
        let endpoint = relay.local_addr().unwrap();
        allow_outer(endpoint);
        // A second owned relay source, used only when a fixture roams.
        let roamed_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        roamed_socket.set_nonblocking(true).unwrap();
        let roamed = roamed_socket.local_addr().unwrap();
        allow_outer(roamed);
        let inbound = Arc::new(Mutex::new(Vec::new()));
        let roam = Arc::new(AtomicBool::new(false));
        let arrivals = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let (ready, started) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let block = Arc::new(AtomicBool::new(false));
        let dns = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let wire = Arc::new(Mutex::new(Vec::new()));
        let dial = Arc::new(Mutex::new(Vec::new()));
        let worker = {
            let stop = stop.clone();
            let block = block.clone();
            let dns = dns.clone();
            let requests = requests.clone();
            let wire = wire.clone();
            let dial = dial.clone();
            let inbound = inbound.clone();
            let roam = roam.clone();
            let arrivals = arrivals.clone();
            thread::spawn(move || {
                let mut tunnel =
                    Native::new(config(endpoint, true), OsEntropy, SystemClock::new()).unwrap();
                let server_outer = tunnel.local_addr().unwrap();
                allow_outer(server_outer);
                let mut client_outer = None;
                // Reserve virtual services before pumping any handshake/data.
                // A key confirmation and first TCP SYN can arrive in one batch;
                // binding only after Protected would already have reset the SYN.
                let listener = tunnel.tcp_listen(8080).unwrap();
                let resolver = tunnel.udp_bind(53).unwrap();
                // A torrent-style peer (TCP echo) and tracker (UDP echo).
                let peer = tunnel.tcp_listen(6881).unwrap();
                let tracker = tunnel.udp_bind(6969).unwrap();
                let mut flows = BTreeMap::new();
                let mut echoes: Vec<(VirtualFlow, Vec<u8>)> = Vec::new();
                ready.send(()).unwrap();
                while !stop.load(Ordering::Acquire) {
                    let mut buffer = [0; 2048];
                    for (index, socket) in [&relay, &roamed_socket].into_iter().enumerate() {
                        for _ in 0..64 {
                            let Ok((n, source)) = socket.recv_from(&mut buffer) else {
                                break;
                            };
                            wire.lock().unwrap().push(buffer[..n].to_vec());
                            if block.load(Ordering::Acquire) {
                                continue;
                            }
                            // The server always sees the original relay source.
                            let (out, destination) = if source == server_outer {
                                let Some(client) = client_outer else {
                                    continue;
                                };
                                inbound.lock().unwrap().push(buffer[..n].to_vec());
                                if roam.load(Ordering::Acquire) {
                                    (&roamed_socket, client)
                                } else {
                                    (&relay, client)
                                }
                            } else {
                                client_outer = Some(source);
                                arrivals[index].fetch_add(1, Ordering::Relaxed);
                                (&relay, server_outer)
                            };
                            out.send_to(&buffer[..n], destination).unwrap();
                        }
                    }
                    tunnel.pump(Duration::ZERO).unwrap();
                    if tunnel.status().state == TunnelState::Protected {
                        while let Some(query) = tunnel.udp_recv(resolver).unwrap() {
                            dns.fetch_add(1, Ordering::Relaxed);
                            let mut answer = query.payload[..2].to_vec();
                            answer.extend_from_slice(&[0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0]);
                            answer.extend_from_slice(&query.payload[12..]);
                            answer.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
                            let ip = if query.payload.windows(4).any(|s| s == b"trap") {
                                [127, 0, 0, 1]
                            } else {
                                SERVER.octets()
                            };
                            answer.extend_from_slice(&ip);
                            tunnel
                                .udp_send_to(resolver, (query.source, query.source_port), &answer)
                                .unwrap();
                        }
                        while let Some(datagram) = tunnel.udp_recv(tracker).unwrap() {
                            let mut reply = b"echo:".to_vec();
                            reply.extend_from_slice(&datagram.payload);
                            tunnel
                                .udp_send_to(
                                    tracker,
                                    (datagram.source, datagram.source_port),
                                    &reply,
                                )
                                .unwrap();
                        }
                        while let Some(flow) = tunnel.tcp_accept(peer).unwrap() {
                            echoes.push((flow, Vec::new()));
                        }
                        for port in dial.lock().unwrap().drain(..) {
                            if let Ok(flow) = tunnel.tcp_connect(CLIENT, port) {
                                echoes.push((flow, b"inbound-hello".to_vec()));
                            }
                        }
                        echoes.retain_mut(|(flow, pending)| {
                            match tunnel.tcp_state(*flow) {
                                Ok(TcpState::Established) => {}
                                Ok(TcpState::SynSent | TcpState::SynReceived) => return true,
                                _ => return false,
                            }
                            let mut buffer = [0; 4096];
                            match tunnel.tcp_recv(*flow, &mut buffer) {
                                Ok(TcpRead::Data(n)) => pending.extend_from_slice(&buffer[..n]),
                                Ok(TcpRead::WouldBlock) => {}
                                _ => return false,
                            }
                            if !pending.is_empty() {
                                match tunnel.tcp_send(*flow, pending) {
                                    Ok(n) => {
                                        pending.drain(..n);
                                    }
                                    Err(_) => return false,
                                }
                            }
                            true
                        });
                        while let Some(flow) = tunnel.tcp_accept(listener).unwrap() {
                            flows.insert(flow.remote(), (flow, Vec::new(), Vec::new()));
                        }
                        flows.retain(|_, (flow, received, pending)| {
                            let mut buffer = [0; 4096];
                            match tunnel.tcp_recv(*flow, &mut buffer) {
                                Ok(TcpRead::Data(n)) => received.extend_from_slice(&buffer[..n]),
                                Ok(TcpRead::WouldBlock) => {},
                                _ => return false,
                            }
                            if received.first() == Some(&0x16) && received.len() >= 5 {
                                let length = usize::from(u16::from_be_bytes([received[3], received[4]])) + 5;
                                if received.len() >= length {
                                    requests.lock().unwrap().push(std::mem::take(received));
                                }
                            }
                            if received.ends_with(b"\r\n\r\n") {
                                requests.lock().unwrap().push(received.clone());
                                let reply: Vec<u8> = if received.starts_with(b"GET /redirect ") {
                                    b"HTTP/1.1 302 Found\r\nLocation: http://second.example:8080/final\r\nContent-Length: 0\r\n\r\n".to_vec()
                                } else if received.starts_with(b"GET /trap ") {
                                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/secret\r\nContent-Length: 0\r\n\r\n".to_vec()
                                } else if received.starts_with(b"GET /large ") {
                                    let body = vec![b'x'; CHUNK * 3 + 23];
                                    let mut reply = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
                                    reply.extend(body); reply
                                } else if received.starts_with(b"GET /silent ") {
                                    Vec::new()
                                } else {
                                    b"HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\nprotected-http-body".to_vec()
                                };
                                pending.extend(reply);
                                received.clear();
                            }
                            if !pending.is_empty() {
                                match tunnel.tcp_send(*flow, pending) {
                                    Ok(n) => { pending.drain(..n); },
                                    Err(_) => return false,
                                }
                            }
                            true
                        });
                    }
                    thread::sleep(poll);
                }
                tunnel.cancel();
            })
        };
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let driver = HttpTunnel::start(config(endpoint, false)).unwrap();
        allow_outer(
            driver
                .owner
                .call(None, end(), |r| r.tunnel.local_addr().map_err(failed))
                .unwrap(),
        );
        let deadline = end();
        let connector = loop {
            if let Ok(connector) = driver.connector(deadline) {
                break connector;
            }
            pause(deadline).unwrap();
        };
        Self {
            driver,
            connector,
            stop,
            block,
            dns,
            requests,
            wire,
            inbound,
            roam,
            roamed,
            arrivals,
            dial,
            worker: Some(worker),
        }
    }
    fn http(&self) -> HttpClient {
        HttpClient::new()
            .with_connector(self.connector.clone())
            .with_timeout(Duration::from_secs(3))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.driver.cancel();
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

#[test]
fn http_dns_and_cross_name_redirect_use_only_encrypted_virtual_services() {
    let f = Fixture::new();
    let response = f.http().get("http://first.example:8080/redirect").unwrap();
    assert_eq!(response.body, b"protected-http-body");
    assert_eq!(f.dns.load(Ordering::Relaxed), 2);
    let requests = f.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with(b"GET /redirect "));
    assert!(requests[1].starts_with(b"GET /final "));
    drop(requests);
    let wire = f.wire.lock().unwrap();
    assert!(wire.iter().all(|d| matches!(d.first(), Some(1..=4))));
    for secret in [b"first".as_slice(), b"GET /", b"protected-http-body"] {
        assert!(
            !wire
                .iter()
                .any(|d| d.windows(secret.len()).any(|w| w == secret))
        );
    }
}

#[test]
fn partial_stream_chunks_handle_large_http_bodies() {
    let f = Fixture::new();
    let response = f.http().get("http://10.7.0.1:8080/large").unwrap();
    assert_eq!(response.body, vec![b'x'; CHUNK * 3 + 23]);
    assert_eq!(f.dns.load(Ordering::Relaxed), 0);
}

#[test]
fn unsupported_destinations_and_hostile_redirects_never_connect_directly() {
    let f = Fixture::new();
    for host in [
        "127.0.0.1",
        "0.0.0.0",
        "169.254.1.1",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "[::1]",
        "fe80::1%en0",
        "bad:host",
    ] {
        assert!(f.connector.connect(host, 8080, end()).is_err(), "{host}");
    }
    assert!(f.connector.connect("10.7.0.1", 0, end()).is_err());
    assert!(f.http().get("http://first.example:8080/trap").is_err());
    assert!(f.connector.connect("trap.example", 8080, end()).is_err());
    assert_eq!(f.requests.lock().unwrap().len(), 1);
}

#[test]
fn startup_requires_literal_endpoint_and_protected_readiness() {
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut cfg = config(sink.local_addr().unwrap(), false);
    cfg.endpoint = Endpoint::Address("[::1]:9".parse().unwrap());
    assert_eq!(
        HttpTunnel::start(cfg).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    let driver = HttpTunnel::start(config(sink.local_addr().unwrap(), false)).unwrap();
    assert!(driver.connector(end()).is_err());
    assert_eq!(driver.status(end()).unwrap().state, TunnelState::Connecting);
    driver.cancel();
    assert!(driver.status(end()).is_err());
    assert_eq!(format!("{driver:?}"), "HttpTunnel { .. }");
}

#[test]
fn cancellation_fences_existing_streams_cloned_connectors_and_inflight_reads() {
    let f = Fixture::new();
    let mut stream = f.connector.connect("10.7.0.1", 8080, end()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let (send, recv) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let result = stream.read(&mut [0]);
        send.send(result).unwrap();
    });
    thread::sleep(Duration::from_millis(20));
    f.driver.cancel();
    assert!(recv.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
    reader.join().unwrap();
    assert!(f.connector.connect("10.7.0.1", 8080, end()).is_err());
    assert!(f.http().get("http://first.example:8080/").is_err());
}

#[test]
fn absolute_deadlines_and_silent_peer_reads_are_bounded() {
    let f = Fixture::new();
    assert!(
        f.connector
            .connect("first.example", 8080, Instant::now())
            .is_err()
    );
    assert_eq!(f.dns.load(Ordering::Relaxed), 0);
    let mut stream = f.connector.connect("10.7.0.1", 8080, end()).unwrap();
    assert!(stream.set_read_timeout(Some(Duration::ZERO)).is_err());
    assert!(stream.set_write_timeout(Some(Duration::ZERO)).is_err());
    stream
        .set_read_timeout(Some(Duration::from_millis(40)))
        .unwrap();
    let start = Instant::now();
    assert_eq!(
        stream.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    // HTTP itself keeps one absolute deadline across the entire response.
    let start = Instant::now();
    assert!(
        f.http()
            .with_timeout(Duration::from_millis(80))
            .get("http://10.7.0.1:8080/silent")
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn abandoned_connects_and_lookups_are_reclaimed_by_the_owner() {
    let f = Fixture::new();
    let deadline = end();
    // Dropping a stream must release its virtual flow without another RPC.
    for _ in 0..8 {
        drop(f.connector.connect("10.7.0.1", 8080, deadline).unwrap());
    }
    f.block.store(true, Ordering::Release);
    for _ in 0..4 {
        let _ = f.connector.connect(
            "never.example",
            8080,
            Instant::now() + Duration::from_millis(15),
        );
    }
    loop {
        let leases = f
            .driver
            .owner
            .call(None, deadline, |r| Ok(r.leases.len()))
            .unwrap();
        if leases == 0 {
            break;
        }
        pause(deadline).unwrap();
    }
    f.block.store(false, Ordering::Release);
    assert_eq!(f.http().get("http://10.7.0.1:8080/").unwrap().status, 200);
}

#[test]
fn full_queue_is_fail_fast_and_dropped_last_owner_joins() {
    // No worker consuming: deterministic saturation without timing races.
    let (sender, receiver) = mpsc::sync_channel(QUEUE);
    let owner = Owner {
        identity: 1,
        sender,
        cancelled: Arc::new(AtomicBool::new(false)),
        generation: Arc::new(AtomicU64::new(1)),
        publication: Arc::new(RwLock::new(())),
        thread: Mutex::new(None),
    };
    for _ in 0..QUEUE {
        owner
            .sender
            .try_send(Command {
                deadline: end(),
                generation: Some(1),
                task: Box::new(|_| {}),
            })
            .unwrap();
    }
    assert_eq!(
        owner.call(Some(1), end(), |_| Ok(())).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(receiver);
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let driver = HttpTunnel::start(config(sink.local_addr().unwrap(), false)).unwrap();
    let stopped = driver.owner.cancelled.clone();
    let start = Instant::now();
    drop(driver);
    assert!(stopped.load(Ordering::Acquire));
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn replaced_generations_refuse_old_connectors_and_prune_live_stale_leases() {
    // Force relay batching like a loaded runner: confirmation and the first
    // TCP SYN can reach the responder in the same pump.
    let f = Fixture::with_poll_interval(Duration::from_millis(10));
    let mut old_stream = f.connector.connect("10.7.0.1", 8080, end()).unwrap();
    let old_generation = f.driver.status(end()).unwrap().generation;
    // Exercise the same runtime replacement used for endpoint/config changes.
    f.driver
        .owner
        .call(None, end(), |r| {
            let endpoint = r.tunnel.status().remote.unwrap();
            r.tunnel
                .set_endpoint(Endpoint::Address(endpoint))
                .map_err(failed)
        })
        .unwrap();
    let deadline = end();
    let new_connector = loop {
        let status = f.driver.status(deadline).unwrap();
        if status.state == TunnelState::Protected && status.generation != old_generation {
            break f.driver.connector(deadline).unwrap();
        }
        pause(deadline).unwrap();
    };
    assert!(old_stream.read(&mut [0]).is_err());
    assert!(old_stream.write(b"old").is_err());
    assert!(old_stream.flush().is_err());
    assert!(f.connector.connect("10.7.0.1", 8080, end()).is_err());
    let count = f
        .driver
        .owner
        .call(None, deadline, |r| Ok(r.leases.len()))
        .unwrap();
    assert_eq!(
        count, 0,
        "live old streams must not retain leases across generations"
    );
    assert_eq!(
        HttpClient::new()
            .with_connector(new_connector)
            .get("http://10.7.0.1:8080/")
            .unwrap()
            .status,
        200
    );
}

#[test]
fn stream_operations_wait_for_queue_space_until_their_deadline() {
    let (sender, receiver) = mpsc::sync_channel(QUEUE);
    let owner = Owner {
        identity: 1,
        sender,
        cancelled: Arc::new(AtomicBool::new(false)),
        generation: Arc::new(AtomicU64::new(1)),
        publication: Arc::new(RwLock::new(())),
        thread: Mutex::new(None),
    };
    for _ in 0..QUEUE {
        owner
            .sender
            .try_send(Command {
                deadline: end(),
                generation: Some(1),
                task: Box::new(|_| {}),
            })
            .unwrap();
    }
    let started = Instant::now();
    let refused = owner.wait(Some(1), Instant::now() + Duration::from_millis(30), |_| {
        Ok(())
    });
    assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_millis(500));
    // Space appears later: the waiting call is queued and answered, not refused.
    let drainer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        while let Ok(command) = receiver.recv_timeout(Duration::from_secs(1)) {
            (command.task)(Err(io::Error::other("drained")));
        }
    });
    let answered = owner.wait(Some(1), end(), |_| Ok(()));
    assert_eq!(answered.unwrap_err().to_string(), "drained");
    drop(owner);
    drainer.join().unwrap();
}

#[test]
fn a_claimed_action_returns_its_result_after_the_caller_deadline() {
    // A stream read that already consumed bytes must not be reported as a
    // timeout: the caller would lose them, or resend accepted writes.
    let f = Fixture::new();
    let runs = Arc::new(AtomicUsize::new(0));
    let counted = runs.clone();
    let result = f.driver.owner.call(
        None,
        Instant::now() + Duration::from_millis(50),
        move |_| {
            thread::sleep(Duration::from_millis(150));
            Ok(counted.fetch_add(1, Ordering::AcqRel) + 1)
        },
    );
    assert_eq!(result.unwrap(), 1);
    assert_eq!(runs.load(Ordering::Acquire), 1);
}

#[test]
fn an_abandoned_queued_action_never_runs() {
    let f = Fixture::new();
    let busy = f.driver.clone();
    let blocker = thread::spawn(move || {
        busy.owner.call(None, end(), |_| {
            thread::sleep(Duration::from_millis(300));
            Ok(())
        })
    });
    thread::sleep(Duration::from_millis(50));
    let touched = Arc::new(AtomicBool::new(false));
    let touched_task = touched.clone();
    let started = Instant::now();
    let result = f.driver.owner.call(
        None,
        Instant::now() + Duration::from_millis(50),
        move |_| {
            touched_task.store(true, Ordering::Release);
            Ok(())
        },
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_millis(250));
    blocker.join().unwrap().unwrap();
    // The owner reaches the abandoned command after the blocker and skips it.
    f.driver.owner.call(None, end(), |_| Ok(())).unwrap();
    assert!(!touched.load(Ordering::Acquire));
}

#[test]
fn expired_queued_work_is_refused_before_side_effects() {
    let f = Fixture::new();
    let (send, recv) = mpsc::sync_channel(1);
    let touched = Arc::new(AtomicBool::new(false));
    let touched_task = touched.clone();
    f.driver
        .owner
        .sender
        .try_send(Command {
            deadline: Instant::now(),
            generation: None,
            task: Box::new(move |runtime| {
                let result = runtime.map(|_| {
                    touched_task.store(true, Ordering::Release);
                });
                send.send(result).unwrap();
            }),
        })
        .unwrap();
    assert_eq!(
        recv.recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert!(!touched.load(Ordering::Acquire));
}

#[test]
fn a_retained_stream_does_not_keep_a_released_native_flow_in_the_lease_table() {
    let f = Fixture::new();
    let mut stream = f.connector.connect("10.7.0.1", 8080, end()).unwrap();
    f.driver
        .owner
        .call(None, end(), |r| {
            let flow = r
                .leases
                .iter()
                .find_map(|(_, _, resource)| match resource {
                    Resource::Flow(flow) => Some(*flow),
                    _ => None,
                })
                .unwrap();
            r.tunnel.tcp_abort(flow).map_err(failed)?;
            r.collect();
            assert!(r.leases.is_empty());
            Ok(())
        })
        .unwrap();
    assert!(stream.write(b"released").is_err());
}

#[test]
fn tls_client_hello_uses_the_virtual_stream_and_preserves_the_url_name() {
    let f = Fixture::new();
    assert!(
        f.http()
            .with_timeout(Duration::from_millis(200))
            .get("https://tls.example:8080/")
            .is_err()
    );
    let requests = f.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0][0], 0x16);
    assert!(
        requests[0]
            .windows(b"tls.example".len())
            .any(|w| w == b"tls.example")
    );
    assert_eq!(f.dns.load(Ordering::Relaxed), 1);
}

#[test]
fn tunnel_http_ignores_environment_proxies() {
    const CHILD: &str = "MYNOU_TUNNEL_HTTP_PROXY_FIXTURE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let f = Fixture::new();
        assert_eq!(
            f.http()
                .get("http://first.example:8080/redirect")
                .unwrap()
                .status,
            200
        );
        assert_eq!(f.dns.load(Ordering::Relaxed), 2);
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tunnel_http::tests::tunnel_http_ignores_environment_proxies",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("https_proxy", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("all_proxy", "http://127.0.0.1:9")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn loss_observed_inside_a_command_fences_that_commands_result() {
    let f = Fixture::new();
    let generation = f.driver.status(end()).unwrap().generation;
    let result = f.driver.owner.call(generation, end(), |r| {
        // Model tcp_recv/flush observing expiry after it read buffered bytes.
        r.tunnel.cancel();
        Ok(b"late old-generation result")
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    assert!(f.connector.connect("10.7.0.1", 8080, end()).is_err());
}

#[test]
fn writes_are_chunked_and_backpressure_respects_the_write_timeout() {
    let f = Fixture::new();
    let mut stream = f.connector.connect("10.7.0.1", 8080, end()).unwrap();
    f.block.store(true, Ordering::Release);
    stream
        .set_write_timeout(Some(Duration::from_millis(40)))
        .unwrap();
    let input = vec![b'x'; CHUNK * 2];
    let start = Instant::now();
    let mut sent = 0;
    loop {
        match stream.write(&input) {
            Ok(n) => {
                assert!(n > 0 && n <= CHUNK);
                sent += n;
                assert!(sent <= 65_536 + CHUNK, "bounded native send buffer");
            }
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::TimedOut);
                break;
            }
        }
    }
    assert!(sent > 0);
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[path = "tunnel_network_tests.rs"]
mod network;

#[path = "tunnel_load_tests.rs"]
mod load;

#[path = "tunnel_session_tests.rs"]
mod sessions;

/// Two native owners, used by the torrent protocol acceptance fixtures. Only
/// their WireGuard UDP endpoints exist on the host; every service is virtual.
pub(crate) struct Pair {
    pub client: HttpTunnel,
    pub server: HttpTunnel,
    pub client_network: Arc<dyn crate::torrent::transport::Network>,
    pub server_network: Arc<dyn crate::torrent::transport::Network>,
    pub dns: Arc<AtomicUsize>,
    dns_worker: Option<JoinHandle<()>>,
}
impl Pair {
    pub(crate) fn replace_client_generation(&self) {
        self.client
            .owner
            .call(None, end(), |r| {
                let remote = r
                    .tunnel
                    .status()
                    .remote
                    .ok_or_else(|| error(io::ErrorKind::NotConnected))?;
                r.tunnel
                    .set_endpoint(Endpoint::Address(remote))
                    .map_err(failed)
            })
            .unwrap();
    }
    pub fn new() -> Self {
        Self::with(|_, _| {})
    }
    /// Adjusts the client and server configurations before either starts.
    pub fn with(configure: impl FnOnce(&mut TunnelConfig, &mut TunnelConfig)) -> Self {
        // Own even the temporary startup endpoint: the observer includes
        // datagrams sent before the client endpoint has been installed.
        let startup = UdpSocket::bind("127.0.0.1:0").unwrap();
        let startup_endpoint = startup.local_addr().unwrap();
        allow_outer(startup_endpoint);
        let (mut client_config, mut server_config) = (
            config(startup_endpoint, false),
            config(startup_endpoint, true),
        );
        configure(&mut client_config, &mut server_config);
        let server = HttpTunnel::start(server_config).unwrap();
        let endpoint = server
            .owner
            .call(None, end(), |r| r.tunnel.local_addr().map_err(failed))
            .unwrap();
        client_config.endpoint = Endpoint::Address(endpoint);
        let client = HttpTunnel::start(client_config).unwrap();
        let client_address = client
            .owner
            .call(None, end(), |r| r.tunnel.local_addr().map_err(failed))
            .unwrap();
        allow_outer(endpoint);
        allow_outer(client_address);
        server
            .owner
            .call(None, end(), move |r| {
                r.tunnel
                    .set_endpoint(Endpoint::Address(client_address))
                    .map_err(failed)
            })
            .unwrap();
        client
            .owner
            .call(None, end(), move |r| {
                r.tunnel
                    .set_endpoint(Endpoint::Address(endpoint))
                    .map_err(failed)
            })
            .unwrap();
        let deadline = end();
        let client_network = loop {
            if let Ok(network) = client.network(deadline) {
                break network;
            }
            pause(deadline).unwrap();
        };
        let server_network = loop {
            if let Ok(network) = server.network(deadline) {
                break network;
            }
            pause(deadline).unwrap();
        };
        let socket = server
            .owner
            .call(None, end(), |r| r.tunnel.udp_bind(53).map_err(failed))
            .unwrap();
        let dns = Arc::new(AtomicUsize::new(0));
        let driver = server.clone();
        let count = dns.clone();
        let dns_worker = thread::spawn(move || {
            while !driver.owner.cancelled.load(Ordering::Acquire) {
                let count = count.clone();
                let result = driver.owner.call(None, end(), move |r| {
                    if let Some(query) = r.tunnel.udp_recv(socket).map_err(failed)? {
                        count.fetch_add(1, Ordering::Relaxed);
                        let mut answer = query.payload[..2].to_vec();
                        answer.extend_from_slice(&[0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0]);
                        answer.extend_from_slice(&query.payload[12..]);
                        answer.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
                        answer.extend_from_slice(&SERVER.octets());
                        r.tunnel
                            .udp_send_to(socket, (query.source, query.source_port), &answer)
                            .map_err(failed)?;
                    }
                    Ok(())
                });
                if result.is_err() {
                    break;
                }
                thread::sleep(TICK);
            }
        });
        Self {
            client,
            server,
            client_network,
            server_network,
            dns,
            dns_worker: Some(dns_worker),
        }
    }
}
impl Drop for Pair {
    fn drop(&mut self) {
        self.client.cancel();
        self.server.cancel();
        if let Some(worker) = self.dns_worker.take() {
            worker.join().unwrap();
        }
    }
}

pub(crate) fn udp_protocol_server(driver: &HttpTunnel) -> (u16, JoinHandle<()>) {
    let socket = driver
        .owner
        .call(None, end(), |r| r.tunnel.udp_bind(0).map_err(failed))
        .unwrap();
    let port = socket.port();
    let driver = driver.clone();
    let worker = thread::spawn(move || {
        for step in 0..4 {
            let deadline = end();
            loop {
                let done = driver
                    .owner
                    .call(None, deadline, move |r| {
                        let Some(query) = r.tunnel.udp_recv(socket).map_err(failed)? else {
                            return Ok(false);
                        };
                        let bytes = &query.payload;
                        let reply = match step {
                            0 => {
                                assert_eq!(bytes.len(), 16);
                                let mut reply = 0_u32.to_be_bytes().to_vec();
                                reply.extend_from_slice(&bytes[12..16]);
                                reply.extend_from_slice(&12345_u64.to_be_bytes());
                                reply
                            }
                            1 => {
                                assert_eq!(bytes.len(), 98);
                                assert_eq!(&bytes[56..64], &0_u64.to_be_bytes());
                                assert_eq!(&bytes[64..72], &1_u64.to_be_bytes());
                                assert_eq!(&bytes[80..84], &3_u32.to_be_bytes());
                                let mut reply = 1_u32.to_be_bytes().to_vec();
                                reply.extend_from_slice(&bytes[12..16]);
                                reply.extend_from_slice(&1800_u32.to_be_bytes());
                                reply.extend_from_slice(&[0; 8]);
                                reply.extend_from_slice(&[10, 7, 0, 1, 0x1a, 0xe1]);
                                reply
                            }
                            2 => {
                                use crate::bencode::Value;
                                let value = crate::bencode::parse(bytes).unwrap();
                                let Value::Dict(fields) = value else { panic!() };
                                assert_eq!(
                                    fields.get(b"q".as_slice()),
                                    Some(&Value::Bytes(b"get_peers".to_vec()))
                                );
                                crate::bencode::encode(&Value::Dict(BTreeMap::from([
                                    (b"t".to_vec(), fields[b"t".as_slice()].clone()),
                                    (b"y".to_vec(), Value::Bytes(b"r".to_vec())),
                                    (
                                        b"r".to_vec(),
                                        Value::Dict(BTreeMap::from([
                                            (
                                                b"values".to_vec(),
                                                Value::List(vec![Value::Bytes(vec![
                                                    10, 7, 0, 1, 0x1a, 0xe1,
                                                ])]),
                                            ),
                                            (b"token".to_vec(), Value::Bytes(b"token".to_vec())),
                                        ])),
                                    ),
                                ])))
                            }
                            _ => {
                                let value = crate::bencode::parse(bytes).unwrap();
                                let crate::bencode::Value::Dict(fields) = value else {
                                    panic!()
                                };
                                assert_eq!(
                                    fields.get(b"q".as_slice()),
                                    Some(&crate::bencode::Value::Bytes(b"announce_peer".to_vec()))
                                );
                                return Ok(true);
                            }
                        };
                        r.tunnel
                            .udp_send_to(socket, (query.source, query.source_port), &reply)
                            .map_err(failed)?;
                        Ok(true)
                    })
                    .unwrap();
                if done {
                    break;
                }
                pause(deadline).unwrap();
            }
        }
    });
    (port, worker)
}
