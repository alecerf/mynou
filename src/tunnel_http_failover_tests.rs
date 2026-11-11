//! Two VPN servers for the failover fixtures (#407): native owners behind
//! loopback relays that record every outer datagram the client sends and can
//! go silent like a dead server, and a client owner with one profile per
//! server, tried in order. Every service is virtual behind the relays.
use super::*;

/// A relay in front of one server, which is the endpoint of one profile.
pub(crate) struct Relay {
    pub address: SocketAddr,
    /// Outer datagrams from the client, silent or not.
    pub wire: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Drops everything in both directions, like a server that died.
    pub silent: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Relay {
    fn start(server: SocketAddr) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(2)))
            .unwrap();
        let address = socket.local_addr().unwrap();
        allow_outer(address);
        let wire = Arc::new(Mutex::new(Vec::new()));
        let silent = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (wire, silent, stop) = (wire.clone(), silent.clone(), stop.clone());
            thread::spawn(move || {
                let mut client = None;
                let mut buffer = [0; 2048];
                while !stop.load(Ordering::Acquire) {
                    let Ok((length, source)) = socket.recv_from(&mut buffer) else {
                        continue;
                    };
                    let datagram = &buffer[..length];
                    let destination = if source == server {
                        client
                    } else {
                        client = Some(source);
                        wire.lock().unwrap().push(datagram.to_vec());
                        Some(server)
                    };
                    if let Some(destination) = destination
                        && !silent.load(Ordering::Acquire)
                    {
                        let _ = socket.send_to(datagram, destination);
                    }
                }
            })
        };
        Self {
            address,
            wire,
            silent,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

pub(crate) struct Failover {
    pub client: HttpTunnel,
    pub servers: Vec<HttpTunnel>,
    pub relays: Vec<Relay>,
}
impl Failover {
    /// Two servers; the client's first profile, protected on return, leads
    /// to the first. Each profile gives up after its first retry.
    pub(crate) fn new() -> Self {
        let startup = UdpSocket::bind("127.0.0.1:0").unwrap();
        let startup_endpoint = startup.local_addr().unwrap();
        allow_outer(startup_endpoint);
        let servers: Vec<HttpTunnel> = (0..2)
            .map(|_| HttpTunnel::start(config(startup_endpoint, true)).unwrap())
            .collect();
        // Each server answers through its relay before the client exists:
        // moving a server's endpoint later would revoke the client's session.
        let relays: Vec<Relay> = servers
            .iter()
            .map(|server| {
                let address = server
                    .owner
                    .call(None, end(), |r| r.tunnel.local_addr().map_err(failed))
                    .unwrap();
                allow_outer(address);
                let relay = Relay::start(address);
                let endpoint = relay.address;
                server
                    .owner
                    .call(None, end(), move |r| {
                        r.tunnel
                            .set_endpoint(Endpoint::Address(endpoint))
                            .map_err(failed)
                    })
                    .unwrap();
                relay
            })
            .collect();
        let profiles = relays
            .iter()
            .map(|relay| {
                let mut config = config(relay.address, false);
                config.reconnect_initial = Duration::from_secs(1);
                config.reconnect_max = Duration::from_secs(1);
                config.reconnect_attempts = 1;
                config
            })
            .collect();
        let client = HttpTunnel::start_profiles(profiles).unwrap();
        allow_driver_outer(&client);
        let deadline = end();
        while client.network(deadline).is_err() {
            pause(deadline).unwrap();
        }
        Self {
            client,
            servers,
            relays,
        }
    }
    pub(crate) fn report(&self) -> Report {
        self.client.report(end()).unwrap()
    }
    /// Moves only the client owner's monotonic clock forward.
    pub(crate) fn skip(&self, by: Duration) {
        self.client
            .owner
            .call(None, end(), move |r| {
                r.tunnel.clock().skip(by);
                Ok(())
            })
            .unwrap();
    }
    /// Datagrams the relays took from the client, and every one of them.
    pub(crate) fn wire(&self) -> Vec<Vec<u8>> {
        self.relays
            .iter()
            .flat_map(|relay| relay.wire.lock().unwrap().clone())
            .collect()
    }
}
impl Drop for Failover {
    fn drop(&mut self) {
        self.client.cancel();
        for server in &self.servers {
            server.cancel();
        }
    }
}

#[test]
fn the_owner_moves_to_the_next_server_and_only_wireguard_reaches_either() {
    let f = Failover::new();
    let first = f.client.network(end()).unwrap();
    let probe = first.datagram(false).unwrap();
    let tracker: SocketAddr = "10.7.0.1:6969".parse().unwrap();
    assert_eq!(f.report().status.profile, 0);

    // The first server dies; a datagram sent now is encrypted toward it.
    f.relays[0].silent.store(true, Ordering::Release);
    probe.send_to(b"failover-marker", tracker).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let report = f.report();
        if report.status.profile == 1 {
            break;
        }
        // Meanwhile nothing protected is usable, and nothing goes direct.
        if report.status.state != TunnelState::Protected {
            assert!(probe.send_to(b"failover-marker", tracker).is_err());
            assert!(first.connect(tracker, Duration::from_secs(1)).is_err());
            assert!(f.client.network(end()).is_err());
        }
        assert!(Instant::now() < deadline, "{:?}", report.status);
        f.skip(Duration::from_secs(10));
        thread::sleep(Duration::from_millis(10));
    }
    while f.report().status.state != TunnelState::Protected {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let report = f.report();
    assert_eq!(report.switches.len(), 1);
    assert_eq!((report.switches[0].from, report.switches[0].to), (0, 1));
    let second = f.client.network(end()).unwrap();
    assert!(first.publication().is_err());
    assert!(probe.send_to(b"failover-marker", tracker).is_err());
    let socket = second.datagram(false).unwrap();
    socket.send_to(b"echo-me", tracker).unwrap();

    // Every datagram the client sent reached one of its two servers' relays,
    // and each is a WireGuard message, never the plaintext.
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.wire().len() as u64 != f.report().stats.sent {
        assert!(Instant::now() < deadline, "a datagram went elsewhere");
        thread::sleep(Duration::from_millis(5));
    }
    let wire = f.wire();
    assert!(wire.iter().all(|d| matches!(d.first(), Some(1..=4))));
    assert!(
        !wire
            .iter()
            .any(|d| d.windows(15).any(|w| w == b"failover-marker"))
    );
    assert!(!f.relays[0].wire.lock().unwrap().is_empty());
    assert!(!f.relays[1].wire.lock().unwrap().is_empty());
}
