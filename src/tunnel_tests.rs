use super::*;
use crate::crypto::{blake2s, keyed_blake2s, x25519_public_key};
use crate::netstack::TcpState;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::UNIX_EPOCH;

const CLIENT_SECRET: [u8; 32] = [17; 32];
const SERVER_SECRET: [u8; 32] = [29; 32];
const CLIENT_IP: Ipv4Addr = Ipv4Addr::new(10, 7, 0, 2);
const SERVER_IP: Ipv4Addr = Ipv4Addr::new(10, 7, 0, 1);
const MARKER: &[u8] = b"inner-plaintext-marker";

/// Scripted monotonic and wall time shared by both tunnels of a test.
#[derive(Clone)]
struct Manual(Rc<Cell<Duration>>);

impl Manual {
    fn advance(&self, by: Duration) {
        self.0.set(self.0.get() + by);
    }
}

impl Clock for Manual {
    fn now(&self) -> Duration {
        self.0.get()
    }

    fn wall(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000) + self.0.get()
    }
}

/// Deterministic independent streams; clones share one counter and can be
/// made to fail.
#[derive(Clone)]
struct Stream(Rc<RefCell<(u64, bool)>>);

impl Stream {
    fn new(seed: u64) -> Self {
        Self(Rc::new(RefCell::new((seed, false))))
    }

    fn fail(&self, fail: bool) {
        self.0.borrow_mut().1 = fail;
    }
}

impl Entropy for Stream {
    fn bytes<const N: usize>(&mut self) -> crate::Result<[u8; N]> {
        let mut state = self.0.borrow_mut();
        if state.1 {
            return Err("scripted entropy failure".into());
        }
        let mut bytes = [0; N];
        for chunk in bytes.chunks_mut(32) {
            state.0 += 1;
            let block = blake2s(&state.0.to_le_bytes())?;
            chunk.copy_from_slice(&block[..chunk.len()]);
        }
        Ok(bytes)
    }
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// A loopback middlebox between the tunnels: it sees every outer datagram and
/// can drop, duplicate, reorder, hold or re-source them.
struct Relay {
    sockets: Vec<UdpSocket>,
    /// The socket used toward the server; changing it makes the client roam.
    active: usize,
    client: Option<SocketAddr>,
    server: Option<SocketAddr>,
    loss: usize,
    duplicate: bool,
    reorder: bool,
    block: bool,
    capture: bool,
    held: Option<(Vec<u8>, bool)>,
    captured: Vec<(Vec<u8>, bool)>,
    count: usize,
    /// Every datagram seen, with its direction (true: toward the client).
    seen: Vec<(Vec<u8>, bool)>,
}

impl Relay {
    fn new() -> Self {
        let socket = UdpSocket::bind(loopback()).unwrap();
        socket.set_nonblocking(true).unwrap();
        Self {
            sockets: vec![socket],
            active: 0,
            client: None,
            server: None,
            loss: 0,
            duplicate: false,
            reorder: false,
            block: false,
            capture: false,
            held: None,
            captured: Vec::new(),
            count: 0,
            seen: Vec::new(),
        }
    }

    fn address(&self, index: usize) -> SocketAddr {
        self.sockets[index].local_addr().unwrap()
    }

    fn roam(&mut self) {
        let socket = UdpSocket::bind(loopback()).unwrap();
        socket.set_nonblocking(true).unwrap();
        self.sockets.push(socket);
        self.active = self.sockets.len() - 1;
    }

    fn forward(&self, datagram: &[u8], to_client: bool) {
        let (socket, target) = if to_client {
            (&self.sockets[0], self.client)
        } else {
            (&self.sockets[self.active], self.server)
        };
        if let Some(target) = target {
            let _ = socket.send_to(datagram, target);
        }
    }

    fn step(&mut self) {
        let mut buffer = [0u8; 4096];
        let mut arrived = Vec::new();
        for socket in &self.sockets {
            while let Ok((length, source)) = socket.recv_from(&mut buffer) {
                let to_client = Some(source) == self.server;
                if !to_client {
                    self.client = Some(source);
                }
                arrived.push((buffer[..length].to_vec(), to_client));
            }
        }
        // A datagram held back by the previous step leaves after this step's.
        let late = self.held.take();
        for (datagram, to_client) in arrived {
            self.seen.push((datagram.clone(), to_client));
            self.count += 1;
            if self.block || (self.loss > 0 && self.count.is_multiple_of(self.loss)) {
                continue;
            }
            if self.capture {
                self.captured.push((datagram, to_client));
                continue;
            }
            if self.reorder && self.held.is_none() && self.count.is_multiple_of(3) {
                self.held = Some((datagram, to_client));
                continue;
            }
            self.forward(&datagram, to_client);
            if self.duplicate {
                self.forward(&datagram, to_client);
            }
        }
        if let Some((datagram, to_client)) = late {
            self.forward(&datagram, to_client);
        }
    }

    /// Steps until `count` datagrams were seen; loopback delivery can lag.
    fn wait_for(&mut self, count: usize) {
        for _ in 0..200 {
            self.step();
            if self.seen.len() >= count {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Lets datagrams still in flight arrive before counting silence.
    fn quiesce(&mut self) {
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(2));
            self.step();
        }
    }

    fn release(&mut self) {
        for (datagram, to_client) in std::mem::take(&mut self.captured) {
            self.forward(&datagram, to_client);
        }
    }

    fn kinds(&self, kind: u8) -> usize {
        self.seen
            .iter()
            .filter(|(datagram, _)| datagram.first() == Some(&kind))
            .count()
    }
}

struct World {
    clock: Manual,
    client: Tunnel<Stream, Manual>,
    server: Tunnel<Stream, Manual>,
    relay: Relay,
}

fn client_config(endpoint: SocketAddr) -> TunnelConfig {
    let ip = Ipv4Config::new(CLIENT_IP, Ipv4Addr::UNSPECIFIED, 0).unwrap();
    let mut config = TunnelConfig::new(
        CLIENT_SECRET,
        x25519_public_key(SERVER_SECRET),
        Endpoint::Address(endpoint),
        ip,
    )
    .with_dns(vec![SERVER_IP]);
    config.psk = Some([53; 32]);
    config.bind = loopback();
    config
}

fn server_config(endpoint: SocketAddr) -> TunnelConfig {
    let ip = Ipv4Config::new(SERVER_IP, Ipv4Addr::new(10, 7, 0, 0), 24).unwrap();
    let mut config = TunnelConfig::new(
        SERVER_SECRET,
        x25519_public_key(CLIENT_SECRET),
        Endpoint::Address(endpoint),
        ip,
    );
    config.psk = Some([53; 32]);
    config.bind = loopback();
    config.initiate = false;
    config.keepalive = None;
    config
}

impl World {
    fn new(configure: impl FnOnce(&mut TunnelConfig, &mut TunnelConfig)) -> Self {
        let clock = Manual(Rc::new(Cell::new(Duration::from_secs(10))));
        let mut relay = Relay::new();
        let mut client = client_config(relay.address(0));
        let mut server = server_config(relay.address(0));
        configure(&mut client, &mut server);
        let server = Tunnel::new(server, Stream::new(1_000_000), clock.clone()).unwrap();
        relay.server = Some(server.local_addr().unwrap());
        let client = Tunnel::new(client, Stream::new(10), clock.clone()).unwrap();
        Self {
            clock,
            client,
            server,
            relay,
        }
    }

    /// One exchange round. Loopback delivery can lag a send slightly (macOS),
    /// so each hop waits a little real time before reading.
    fn step(&mut self) {
        let hop = || std::thread::sleep(Duration::from_micros(300));
        self.client.pump(Duration::ZERO).unwrap();
        hop();
        self.relay.step();
        hop();
        self.server.pump(Duration::ZERO).unwrap();
        hop();
        self.relay.step();
        hop();
        self.client.pump(Duration::ZERO).unwrap();
    }

    /// Loopback delivery can lag a send slightly (macOS), so settling also
    /// waits a little real time between steps.
    fn settle(&mut self) {
        for _ in 0..6 {
            self.step();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn advance(&mut self, by: Duration) {
        self.clock.advance(by);
        self.step();
    }

    fn protected(&self) -> bool {
        self.client.status().state == TunnelState::Protected
            && self.server.status().state == TunnelState::Protected
    }

    /// Steps in `tick` increments until both sides are protected.
    fn connect(&mut self, tick: Duration, limit: usize) {
        for _ in 0..limit {
            if self.protected() {
                break;
            }
            self.advance(tick);
        }
        assert!(
            self.protected(),
            "{:?} {:?}",
            self.client.status(),
            self.server.status()
        );
        self.settle();
    }

    /// Sends `count` copies of each datagram to the server from a stranger.
    fn flood(&self, datagrams: &[Vec<u8>], count: usize) {
        let spoof = UdpSocket::bind(loopback()).unwrap();
        for _ in 0..count {
            for datagram in datagrams {
                spoof
                    .send_to(datagram, self.server.local_addr().unwrap())
                    .unwrap();
            }
        }
    }
}

/// Handshake-type datagrams that fail framing or MAC1 at the server.
fn junk_handshakes() -> Vec<Vec<u8>> {
    let mut initiation = vec![0u8; 148];
    initiation[0] = 1;
    let mut response = vec![0u8; 92];
    response[0] = 2;
    // Framing is checked on its own: these two carry a valid MAC1.
    let reserved = mac1_valid_initiation([1, 1, 0, 0]);
    let mut short = mac1_valid_initiation([1, 0, 0, 0]);
    short.truncate(147);
    vec![initiation, response, reserved, short]
}

/// An initiation with a valid MAC1 for the server but no valid Noise content,
/// as anyone who knows the server's public key can send.
fn mac1_valid_initiation(header: [u8; 4]) -> Vec<u8> {
    let mut label = b"mac1----".to_vec();
    label.extend_from_slice(&x25519_public_key(SERVER_SECRET));
    let key = blake2s(&label).unwrap();
    let mut initiation = vec![7u8; 148];
    initiation[..4].copy_from_slice(&header);
    let mac = keyed_blake2s(&key, &initiation[..116], 16).unwrap();
    initiation[116..132].copy_from_slice(&mac);
    initiation
}

fn drain(tunnel: &mut Tunnel<Stream, Manual>, socket: VirtualUdp) -> Vec<Received> {
    let mut all = Vec::new();
    while let Some(datagram) = tunnel.udp_recv(socket).unwrap() {
        all.push(datagram);
    }
    all
}

/// A DNS answer of one A record for the question of `query`, TTL 60 seconds.
fn answer(query: &[u8], address: [u8; 4]) -> Vec<u8> {
    let mut reply = query[..2].to_vec();
    reply.extend_from_slice(&[0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0]);
    reply.extend_from_slice(&query[12..]);
    reply.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
    reply.extend_from_slice(&address);
    reply
}

/// One TCP flow from the client to a server listener: `size` bytes up, a
/// reply down, then a graceful close on both sides.
fn exchange(world: &mut World, port: u16, size: usize, tick: Duration) {
    let listener = world.server.tcp_listen(port).unwrap();
    let flow = world.client.tcp_connect(SERVER_IP, port).unwrap();
    let data: Vec<u8> = (0..size).map(|index| (index * 7 % 251) as u8).collect();
    let mut accepted = None;
    let (mut sent, mut received, mut reply) = (0, Vec::new(), Vec::new());
    let mut buffer = [0u8; 8192];
    for _ in 0..20_000 {
        world.advance(tick);
        if accepted.is_none() {
            accepted = world.server.tcp_accept(listener).unwrap();
        }
        if sent < size && world.client.tcp_state(flow).unwrap() == TcpState::Established {
            sent += world.client.tcp_send(flow, &data[sent..]).unwrap();
        }
        let Some(peer) = accepted else { continue };
        while let Read::Data(count) = world.server.tcp_recv(peer, &mut buffer).unwrap() {
            received.extend_from_slice(&buffer[..count]);
        }
        if received.len() == size && reply.is_empty() {
            assert_eq!(world.server.tcp_send(peer, b"done").unwrap(), 4);
            world.server.tcp_close(peer).unwrap();
            reply.push(0);
        }
        if !reply.is_empty() {
            while let Read::Data(count) = world.client.tcp_recv(flow, &mut buffer).unwrap() {
                reply.extend_from_slice(&buffer[..count]);
            }
            if reply.len() == 5 {
                break;
            }
        }
    }
    assert_eq!(received.len(), size);
    assert!(received == data);
    assert_eq!(&reply[1..], b"done");
    world.client.tcp_close(flow).unwrap();
    for _ in 0..200 {
        world.advance(tick);
        if world.client.tcp_state(flow).is_err() {
            break;
        }
        if matches!(
            world.client.tcp_state(flow).unwrap(),
            TcpState::TimeWait | TcpState::Closed
        ) {
            break;
        }
    }
    assert!(world.client.tcp_state(flow).map_or(true, |state| matches!(
        state,
        TcpState::TimeWait | TcpState::Closed
    )));
}

#[test]
fn udp_dns_and_tcp_cross_an_encrypted_loopback_tunnel() {
    let mut world = World::new(|_, _| {});
    world.connect(Duration::from_millis(100), 20);
    assert_eq!(world.client.status().generation, Some(1));
    assert_eq!(world.client.status().remote, Some(world.relay.address(0)));

    let service = world.server.udp_bind(7000).unwrap();
    let local = world.client.udp_bind(0).unwrap();
    world
        .client
        .udp_send_to(local, (SERVER_IP, 7000), MARKER)
        .unwrap();
    world.settle();
    let request = drain(&mut world.server, service);
    assert_eq!(request.len(), 1);
    assert_eq!(request[0].source, CLIENT_IP);
    assert_eq!(request[0].source_port, local.port());
    assert_eq!(request[0].payload, MARKER);
    world
        .server
        .udp_send_to(service, (CLIENT_IP, local.port()), b"pong")
        .unwrap();
    world.settle();
    let reply = drain(&mut world.client, local);
    assert_eq!(reply.len(), 1);
    assert_eq!((reply[0].source, reply[0].source_port), (SERVER_IP, 7000));
    assert_eq!(reply[0].payload, b"pong");

    let resolver = world.server.udp_bind(53).unwrap();
    let lookup = world.client.resolve("Tracker.Example").unwrap();
    world.settle();
    assert_eq!(world.client.lookup_result(lookup).unwrap(), None);
    let query = drain(&mut world.server, resolver);
    assert_eq!(query.len(), 1);
    let response = answer(&query[0].payload, [10, 9, 8, 7]);
    world
        .server
        .udp_send_to(resolver, (CLIENT_IP, query[0].source_port), &response)
        .unwrap();
    world.settle();
    let resolved = world
        .client
        .lookup_result(lookup)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(resolved.addresses, vec![Ipv4Addr::new(10, 9, 8, 7)]);
    assert_eq!(resolved.ttl, Duration::from_secs(60));
    assert_eq!(world.client.lookup_result(lookup).unwrap(), None);

    exchange(&mut world, 8080, 200_000, Duration::from_millis(10));

    // Every outer datagram is a WireGuard message; no inner bytes are visible.
    assert!(world.relay.seen.iter().all(|(datagram, _)| {
        matches!(datagram.first(), Some(1..=4))
            && !datagram
                .windows(MARKER.len())
                .any(|window| window == MARKER)
            && !datagram
                .windows(9)
                .any(|window| window == b"\x07tracker\x07")
    }));
    let stats = world.client.stats();
    assert_eq!(
        (stats.rejected, stats.inner_rejected, stats.oversized),
        (0, 0, 0)
    );
    assert_eq!(stats.generations, 1);
}

#[test]
fn loss_reordering_and_duplication_are_survived() {
    let mut world = World::new(|_, server| server.udp.queued_per_socket = 64);
    world.relay.loss = 7;
    world.relay.duplicate = true;
    world.relay.reorder = true;
    world.connect(Duration::from_millis(500), 200);

    let service = world.server.udp_bind(7000).unwrap();
    let local = world.client.udp_bind(0).unwrap();
    for index in 0..30u8 {
        world
            .client
            .udp_send_to(local, (SERVER_IP, 7000), &[index])
            .unwrap();
        world.step();
    }
    world.settle();
    let got = drain(&mut world.server, service);
    // Loss removes some; duplicates never get through the replay window.
    let mut payloads: Vec<u8> = got.iter().map(|datagram| datagram.payload[0]).collect();
    let count = payloads.len();
    payloads.sort_unstable();
    payloads.dedup();
    assert_eq!(payloads.len(), count);
    assert!((20..30).contains(&count), "{count}");
    assert!(world.server.stats().rejected > 0);

    exchange(&mut world, 8081, 60_000, Duration::from_millis(200));
    assert_eq!(world.client.status().generation, Some(1));
}

#[test]
fn rekey_under_traffic_keeps_the_generation() {
    let mut world = World::new(|_, _| {});
    world.connect(Duration::from_millis(100), 20);
    let service = world.server.udp_bind(7000).unwrap();
    let local = world.client.udp_bind(0).unwrap();
    let mut echoed = std::collections::BTreeSet::new();
    let echo = |world: &mut World, echoed: &mut std::collections::BTreeSet<Vec<u8>>| {
        for datagram in drain(&mut world.server, service) {
            world
                .server
                .udp_send_to(service, (CLIENT_IP, local.port()), &datagram.payload)
                .unwrap();
        }
        world.step();
        for datagram in drain(&mut world.client, local) {
            assert!(echoed.insert(datagram.payload));
        }
    };
    for second in 0..300u32 {
        world
            .client
            .udp_send_to(local, (SERVER_IP, 7000), &second.to_be_bytes())
            .unwrap();
        world.advance(Duration::from_secs(1));
        echo(&mut world, &mut echoed);
        assert_eq!(world.client.status().state, TunnelState::Protected);
    }
    // Loopback delivery may lag a pump (macOS); late echoes still count, lost
    // ones never arrive.
    for _ in 0..50 {
        if echoed.len() == 300 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
        echo(&mut world, &mut echoed);
    }
    assert_eq!(echoed.len(), 300);
    assert!(world.relay.kinds(1) >= 3, "{}", world.relay.kinds(1));
    assert_eq!(world.client.status().generation, Some(1));
    assert_eq!(world.server.status().generation, Some(1));
}

#[test]
fn cookie_reply_under_load_then_mac2_admission() {
    let mut world = World::new(|_, server| server.handshake_load = 2);
    for _ in 0..40 {
        if world.protected() {
            break;
        }
        world.flood(&[mac1_valid_initiation([1, 0, 0, 0])], 3);
        world.advance(Duration::from_millis(500));
    }
    assert!(world.protected());
    assert!(world.relay.kinds(3) >= 1);
    let stats = world.server.stats();
    assert!(stats.high_load >= 3);
    assert!(stats.rejected >= 2);
    assert_eq!(stats.handshake_invalid, 0);
    assert_eq!(world.server.status().generation, Some(1));
}

#[test]
fn junk_handshakes_are_refused_without_counting_as_load() {
    let mut world = World::new(|_, server| server.handshake_load = 2);
    // The flood lands before the genuine initiation, in the same load window.
    world.flood(&junk_handshakes(), 10);
    world.connect(Duration::from_millis(100), 20);
    let stats = world.server.stats();
    assert_eq!(stats.handshake_invalid, 40);
    assert!(stats.rejected >= 40);
    assert_eq!(stats.high_load, 0);
    assert_eq!(world.relay.kinds(3), 0);
    assert_eq!(world.relay.kinds(1), 1);
    assert_eq!(world.client.status().generation, Some(1));
    assert_eq!(world.server.status().generation, Some(1));

    // Under a continuing flood the protected session still carries traffic.
    let service = world.server.udp_bind(7000).unwrap();
    let local = world.client.udp_bind(0).unwrap();
    world.flood(&junk_handshakes(), 10);
    world
        .client
        .udp_send_to(local, (SERVER_IP, 7000), b"through")
        .unwrap();
    world.settle();
    assert_eq!(drain(&mut world.server, service).len(), 1);
    assert_eq!(world.server.stats().handshake_invalid, 80);
    assert_eq!(world.server.stats().high_load, 0);
}

#[test]
fn roaming_follows_authenticated_packets_only() {
    let mut world = World::new(|_, _| {});
    world.connect(Duration::from_millis(100), 20);
    let first = world.relay.address(0);
    assert_eq!(world.server.status().remote, Some(first));

    let service = world.server.udp_bind(7000).unwrap();
    let local = world.client.udp_bind(0).unwrap();
    world.relay.roam();
    let moved = world.relay.address(1);
    world
        .client
        .udp_send_to(local, (SERVER_IP, 7000), b"moved")
        .unwrap();
    world.settle();
    assert_eq!(drain(&mut world.server, service).len(), 1);
    assert_eq!(world.server.status().remote, Some(moved));
    assert_eq!(world.server.stats().roamed, 1);
    world
        .server
        .udp_send_to(service, (CLIENT_IP, local.port()), b"back")
        .unwrap();
    world.settle();
    assert_eq!(drain(&mut world.client, local).len(), 1);

    // A replayed authentic datagram and garbage from elsewhere cannot redirect.
    let replay = world
        .relay
        .seen
        .iter()
        .rev()
        .find(|(datagram, to_client)| !to_client && datagram[0] == 4)
        .unwrap()
        .0
        .clone();
    let spoof = UdpSocket::bind(loopback()).unwrap();
    let target = world.server.local_addr().unwrap();
    let rejected = world.server.stats().rejected;
    spoof.send_to(&replay, target).unwrap();
    spoof.send_to(&[4; 64], target).unwrap();
    spoof.send_to(&[3; 64], target).unwrap();
    world.settle();
    assert_eq!(world.server.stats().rejected, rejected + 3);
    assert_eq!(world.server.status().remote, Some(moved));
    assert_eq!(world.server.stats().roamed, 1);
}

#[test]
fn endpoint_silence_revokes_then_reconnects_a_fresh_generation() {
    let mut world = World::new(|client, server| {
        client.reconnect_max = Duration::from_secs(4);
        server.reconnect_initial = Duration::from_secs(1);
    });
    world.connect(Duration::from_millis(100), 20);
    let old = world.client.udp_bind(0).unwrap();
    let flow = world.client.tcp_connect(SERVER_IP, 80).unwrap();
    world.relay.block = true;
    for _ in 0..120 {
        if world.client.status().state != TunnelState::Protected {
            break;
        }
        world.advance(Duration::from_secs(5));
    }
    let status = world.client.status();
    assert_eq!(status.state, TunnelState::Reconnecting);
    assert!(matches!(
        status.reason,
        Some(Reason::Expired | Reason::GaveUp)
    ));
    assert_eq!(status.generation, None);
    assert_eq!(
        world.client.udp_send_to(old, (SERVER_IP, 7), b"x"),
        Err(TunnelError::Revoked)
    );
    assert_eq!(world.client.tcp_state(flow), Err(TunnelError::Revoked));
    assert_eq!(world.client.udp_bind(0), Err(TunnelError::Unavailable));

    world.relay.block = false;
    world.connect(Duration::from_secs(1), 600);
    let generation = world.client.status().generation.unwrap();
    assert!(generation >= 2);
    assert_eq!(world.client.udp_recv(old), Err(TunnelError::Revoked));
    let fresh = world.client.udp_bind(old.port()).unwrap();
    let service = world.server.udp_bind(7000).unwrap();
    world
        .client
        .udp_send_to(fresh, (SERVER_IP, 7000), b"again")
        .unwrap();
    world.settle();
    assert_eq!(drain(&mut world.server, service)[0].payload, b"again");
    assert_eq!(world.client.status().attempt, 0);
}

#[test]
fn reconnection_backs_off_boundedly_and_can_be_cancelled() {
    let mut world = World::new(|client, _| {
        client.reconnect_initial = Duration::from_secs(2);
        client.reconnect_max = Duration::from_secs(4);
        client.reconnect_attempts = 3;
    });
    world.relay.block = true;
    let mut delays = Vec::new();
    let mut last = None;
    for _ in 0..2_000 {
        let status = world.client.status();
        if status.state == TunnelState::Blocked {
            break;
        }
        if let Some(at) = status.retry_at
            && status.retry_at != last
        {
            delays.push(at - world.clock.now());
            last = status.retry_at;
        }
        world.advance(Duration::from_secs(1));
    }
    let status = world.client.status();
    assert_eq!(status.state, TunnelState::Blocked);
    assert_eq!(status.reason, Some(Reason::Exhausted));
    assert_eq!(
        delays,
        [2, 4, 4].map(Duration::from_secs).to_vec(),
        "{delays:?}"
    );
    assert_eq!(world.client.stats().generations, 4);

    world.relay.quiesce();
    let seen = world.relay.seen.len();
    for _ in 0..200 {
        world.advance(Duration::from_secs(1));
    }
    assert_eq!(world.relay.seen.len(), seen);

    world.client.reconnect().unwrap();
    assert_eq!(world.client.status().state, TunnelState::Connecting);
    world.relay.wait_for(seen + 1);
    assert!(world.relay.seen.len() > seen);
    world.client.cancel();
    let status = world.client.status();
    assert_eq!(
        (status.state, status.reason),
        (TunnelState::Blocked, Some(Reason::Cancelled))
    );
    world.relay.quiesce();
    let seen = world.relay.seen.len();
    for _ in 0..200 {
        world.advance(Duration::from_secs(1));
    }
    assert_eq!(world.relay.seen.len(), seen);
    assert_eq!(world.client.udp_bind(0), Err(TunnelError::Unavailable));
}

#[test]
fn revocation_wins_against_delayed_old_generation_results() {
    let mut world = World::new(|_, _| {});
    world.connect(Duration::from_millis(100), 20);
    let resolver = world.server.udp_bind(53).unwrap();
    let service = world.server.udp_bind(7000).unwrap();
    let listener = world.server.tcp_listen(8080).unwrap();
    let local = world.client.udp_bind(40000).unwrap();
    let lookup = world.client.resolve("tracker.example").unwrap();
    let flow = world.client.tcp_connect(SERVER_IP, 8080).unwrap();
    world.settle();
    let query = drain(&mut world.server, resolver).remove(0);
    let peer = world.server.tcp_accept(listener).unwrap().unwrap();

    // Answers leave the server but are held in flight.
    world.relay.capture = true;
    let response = answer(&query.payload, [10, 9, 8, 7]);
    world
        .server
        .udp_send_to(resolver, (CLIENT_IP, query.source_port), &response)
        .unwrap();
    world
        .server
        .udp_send_to(service, (CLIENT_IP, 40000), b"stale")
        .unwrap();
    world.server.tcp_send(peer, b"stale bytes").unwrap();
    world.settle();
    let held = world
        .relay
        .captured
        .iter()
        .filter(|(_, to_client)| *to_client)
        .count();
    assert!(held >= 3);

    // Endpoint replacement revokes everything in one step.
    world
        .client
        .set_endpoint(Endpoint::Address(world.relay.address(0)))
        .unwrap();
    let status = world.client.status();
    assert_eq!(status.generation, Some(2));
    assert_eq!(status.state, TunnelState::Connecting);
    assert_eq!(
        world.client.lookup_result(lookup),
        Err(TunnelError::Revoked)
    );
    assert_eq!(world.client.udp_recv(local), Err(TunnelError::Revoked));
    assert_eq!(world.client.tcp_state(flow), Err(TunnelError::Revoked));
    // A new operation of the new generation races the delayed old results.
    let fresh = world.client.udp_bind(40000).unwrap();
    world
        .client
        .udp_send_to(fresh, (SERVER_IP, 7000), b"new")
        .unwrap();
    let rejected = world.client.stats().rejected;
    world.relay.capture = false;
    world.relay.release();
    for _ in 0..200 {
        world.client.pump(Duration::ZERO).unwrap();
        if world.client.stats().rejected >= rejected + held as u64 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(world.client.stats().rejected, rejected + held as u64);
    assert_eq!(world.client.udp_recv(fresh), Ok(None));
    assert_eq!(
        world.client.lookup_result(lookup),
        Err(TunnelError::Revoked)
    );

    world.connect(Duration::from_millis(100), 50);
    assert_eq!(world.client.status().generation, Some(2));
    assert_eq!(drain(&mut world.server, service)[0].payload, b"new");
    assert_eq!(world.client.udp_recv(fresh), Ok(None));
}

#[test]
fn queues_stay_bounded_under_flood() {
    let mut world = World::new(|_, _| {});
    world.connect(Duration::from_millis(100), 20);

    // Unauthenticated outer flood: at most one batch per pump, all rejected.
    let spoof = UdpSocket::bind(loopback()).unwrap();
    let target = world.client.local_addr().unwrap();
    spoof.send_to(&[4; 1600], target).unwrap();
    for index in 0..150u32 {
        let mut datagram = vec![4u8; 96];
        datagram[8..12].copy_from_slice(&index.to_le_bytes());
        spoof.send_to(&datagram, target).unwrap();
    }
    let (mut total, mut idle) = (0, 0);
    while idle < 3 {
        let pumped = world.client.pump(Duration::ZERO).unwrap();
        assert!(pumped.received <= 64);
        total += pumped.received;
        if pumped.received == 0 {
            idle += 1;
            std::thread::sleep(Duration::from_millis(2));
        } else {
            idle = 0;
        }
    }
    let stats = world.client.stats();
    assert!(total > 0);
    assert_eq!(stats.oversized, 1);
    assert_eq!(stats.rejected + stats.oversized, total as u64);
    assert_eq!(world.client.status().state, TunnelState::Protected);

    // Authenticated inner flood to a socket that never reads.
    let sink = world.client.udp_bind(9000).unwrap();
    let source = world.server.udp_bind(7000).unwrap();
    for _ in 0..100 {
        world
            .server
            .udp_send_to(source, (CLIENT_IP, 9000), b"flood")
            .unwrap();
        world.relay.step();
        world.client.pump(Duration::ZERO).unwrap();
    }
    world.settle();
    assert_eq!(drain(&mut world.client, sink).len(), 16);
    assert_eq!(world.client.stats().inner_dropped, 84);
    // Inner packets of unsupported protocols are refused, not delivered.
    let mut icmp = vec![0x45, 0, 0, 28, 0, 0, 0x40, 0, 64, 1, 0, 0];
    icmp.extend_from_slice(&SERVER_IP.octets());
    icmp.extend_from_slice(&CLIENT_IP.octets());
    icmp.extend_from_slice(&[8; 8]);
    let now = world.clock.now();
    world.server.encrypt(icmp, now).unwrap();
    world.server.encrypt(vec![0x60; 40], now).unwrap();
    world.settle();
    assert_eq!(world.client.stats().inner_rejected, 2);
}

#[test]
fn the_peer_queue_bounds_traffic_before_a_session() {
    let mut world = World::new(|_, server| server.udp.queued_per_socket = 64);
    world.relay.block = true;
    let local = world.client.udp_bind(0).unwrap();
    let mut accepted = 0;
    for _ in 0..40 {
        match world.client.udp_send_to(local, (SERVER_IP, 7000), b"early") {
            Ok(()) => accepted += 1,
            Err(error) => assert_eq!(error, TunnelError::QueueFull),
        }
    }
    // The initiator's confirming keepalive holds the first slot.
    assert_eq!(accepted, MAX_QUEUED_PAYLOADS - 1);
    assert!(
        world
            .relay
            .seen
            .iter()
            .all(|(datagram, _)| datagram[0] == 1)
    );
    world.relay.block = false;
    let service = world.server.udp_bind(7000).unwrap();
    world.connect(Duration::from_secs(1), 20);
    world.settle();
    assert_eq!(drain(&mut world.server, service).len(), accepted);
}

#[test]
fn shutdown_closes_the_outer_socket() {
    let mut world = World::new(|_, _| {});
    world.connect(Duration::from_millis(100), 20);
    let client = world.client.local_addr().unwrap();
    let server = world.server.local_addr().unwrap();
    let World {
        client: tunnel,
        server: other,
        ..
    } = world;
    let stats = tunnel.shutdown();
    assert_eq!(stats.generations, 1);
    assert!(stats.sent > 0 && stats.received > 0);
    other.shutdown();
    UdpSocket::bind(client).unwrap();
    UdpSocket::bind(server).unwrap();
}

#[test]
fn configuration_and_endpoint_are_validated() {
    let clock = Manual(Rc::new(Cell::new(Duration::ZERO)));
    let target = SocketAddr::from(([127, 0, 0, 1], 51820));
    let build = |change: &dyn Fn(&mut TunnelConfig)| {
        let mut config = client_config(target);
        change(&mut config);
        Tunnel::new(config, Stream::new(5), clock.clone()).map(|_| ())
    };
    assert_eq!(build(&|_| {}), Ok(()));
    let other = Ipv4Config::new(Ipv4Addr::new(10, 7, 0, 3), Ipv4Addr::UNSPECIFIED, 0).unwrap();
    let invalid: [&dyn Fn(&mut TunnelConfig); 9] = [
        &|config| config.tcp.ip = other,
        &|config| config.mtu = 500,
        &|config| config.keepalive = Some(Duration::from_millis(1500)),
        &|config| config.bind = "[::1]:0".parse().unwrap(),
        &|config| config.peer_queue = 0,
        &|config| config.reconnect_attempts = 0,
        &|config| config.reconnect_initial = Duration::from_secs(120),
        &|config| config.handshake_load = 0,
        &|config| config.dns.as_mut().unwrap().mtu = 1280,
    ];
    for change in invalid {
        assert_eq!(build(change), Err(TunnelError::Config));
    }
    let endpoint = |endpoint: Endpoint| build(&|config| config.endpoint = endpoint.clone());
    for refused in [
        Endpoint::Address("[::1]:51820".parse().unwrap()),
        Endpoint::Address("127.0.0.1:0".parse().unwrap()),
        Endpoint::Address("0.0.0.0:51820".parse().unwrap()),
        Endpoint::Address("224.0.0.1:51820".parse().unwrap()),
        Endpoint::Name {
            host: String::new(),
            port: 51820,
        },
        Endpoint::Name {
            host: "127.0.0.1".into(),
            port: 0,
        },
        Endpoint::Name {
            host: "::1".into(),
            port: 51820,
        },
        Endpoint::Name {
            host: "a".repeat(254),
            port: 51820,
        },
    ] {
        assert!(matches!(endpoint(refused), Err(TunnelError::Endpoint(_))));
    }
    let literal = Endpoint::Name {
        host: "127.0.0.1".into(),
        port: 51820,
    };
    assert_eq!(resolve_endpoint(&literal), Ok(target));
}

#[test]
fn clock_and_entropy_failures_revoke_without_fallback() {
    let mut world = World::new(|_, _| {});
    world.connect(Duration::from_millis(100), 20);
    let local = world.client.udp_bind(0).unwrap();
    // A monotonic clock that moves backwards revokes the whole generation.
    world
        .clock
        .0
        .set(world.clock.now() - Duration::from_secs(1));
    world.client.pump(Duration::ZERO).unwrap();
    let status = world.client.status();
    assert_eq!(status.state, TunnelState::Reconnecting);
    assert_eq!(status.reason, Some(Reason::Failed));
    assert_eq!(world.client.udp_recv(local), Err(TunnelError::Revoked));

    // Entropy failure: every new generation fails to start until the budget
    // runs out; nothing is sent meanwhile.
    let stream = Stream::new(77);
    let mut config = client_config(world.relay.address(0));
    config.reconnect_attempts = 2;
    let mut tunnel = Tunnel::new(config, stream.clone(), world.clock.clone()).unwrap();
    world.settle();
    world.relay.quiesce();
    let seen = world.relay.seen.len();
    stream.fail(true);
    tunnel
        .set_endpoint(Endpoint::Address(world.relay.address(0)))
        .unwrap();
    assert_eq!(tunnel.status().state, TunnelState::Reconnecting);
    assert_eq!(tunnel.status().reason, Some(Reason::Failed));
    for _ in 0..20 {
        world.clock.advance(Duration::from_secs(1));
        tunnel.pump(Duration::ZERO).unwrap();
        world.relay.step();
    }
    world.relay.quiesce();
    assert_eq!(tunnel.status().state, TunnelState::Blocked);
    assert_eq!(tunnel.status().reason, Some(Reason::Exhausted));
    assert_eq!(world.relay.seen.len(), seen);
    stream.fail(false);
    tunnel.reconnect().unwrap();
    assert_eq!(tunnel.status().state, TunnelState::Connecting);
    world.relay.wait_for(seen + 1);
    world.relay.quiesce();
    assert_eq!(world.relay.seen.len(), seen + 1);
}

#[test]
fn an_expiry_seen_late_still_revokes_the_generation() {
    let mut world = World::new(|_, _| {});
    world.connect(Duration::from_millis(100), 20);
    let old = world.client.udp_bind(0).unwrap();
    world.relay.block = true;
    // The pump comes late: the peer expires its keys and starts a new
    // handshake within the same call.
    world.clock.advance(Duration::from_secs(200));
    world.client.pump(Duration::ZERO).unwrap();
    let status = world.client.status();
    assert_eq!(status.state, TunnelState::Reconnecting);
    assert_eq!(status.reason, Some(Reason::Expired));
    assert_eq!(world.client.udp_recv(old), Err(TunnelError::Revoked));
    world.relay.block = false;
    world.connect(Duration::from_millis(500), 200);
    assert_eq!(world.client.status().generation, Some(2));
    assert_eq!(world.client.udp_recv(old), Err(TunnelError::Revoked));
}

#[test]
fn a_send_after_idle_expiry_is_refused_and_revokes() {
    let mut world = World::new(|client, _| client.keepalive = None);
    world.connect(Duration::from_millis(100), 20);
    let old = world.client.udp_bind(0).unwrap();
    world.relay.quiesce();
    let seen = world.relay.seen.len();
    world.clock.advance(Duration::from_secs(200));
    assert_eq!(
        world.client.udp_send_to(old, (SERVER_IP, 7000), b"x"),
        Err(TunnelError::Revoked)
    );
    let status = world.client.status();
    assert_eq!(status.state, TunnelState::Reconnecting);
    assert_eq!(status.reason, Some(Reason::Expired));
    assert_eq!(world.client.udp_recv(old), Err(TunnelError::Revoked));
    // The doomed generation published nothing, not even its new initiation.
    world.relay.quiesce();
    assert_eq!(world.relay.seen.len(), seen);
}

#[test]
fn a_replayed_initiation_never_moves_the_remote() {
    let mut world = World::new(|_, _| {});
    world.connect(Duration::from_millis(100), 20);
    let initiation = world
        .relay
        .seen
        .iter()
        .find(|(datagram, to_client)| !to_client && datagram[0] == 1)
        .unwrap()
        .0
        .clone();
    // A fresh generation forgets the timestamp fence of the old one.
    world
        .server
        .set_endpoint(Endpoint::Address(world.relay.address(0)))
        .unwrap();
    let spoof = UdpSocket::bind(loopback()).unwrap();
    spoof.set_nonblocking(true).unwrap();
    spoof
        .send_to(&initiation, world.server.local_addr().unwrap())
        .unwrap();
    for _ in 0..20 {
        world.server.pump(Duration::ZERO).unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut buffer = [0u8; 2048];
    assert!(spoof.recv_from(&mut buffer).is_err());
    assert_eq!(world.server.status().remote, Some(world.relay.address(0)));
    assert_eq!(world.server.stats().roamed, 0);
    // The real client still reconnects the server's new generation.
    world
        .client
        .set_endpoint(Endpoint::Address(world.relay.address(0)))
        .unwrap();
    world.connect(Duration::from_millis(500), 100);
    assert_eq!(world.server.status().remote, Some(world.relay.address(0)));
}

#[test]
fn every_read_attempt_consumes_the_pump_budget() {
    let mut world = World::new(|_, server| server.datagrams_per_pump = 4);
    // Exhaustively mix datagrams, refused sends and connection resets. These
    // injected reads exercise the production pump loop without relying on how
    // a platform reports ICMP errors on an unconnected UDP socket.
    for mut pattern in 0..3usize.pow(4) {
        let before = world.server.stats();
        let mut reads = 0;
        let mut datagrams = 0;
        let pumped = world
            .server
            .pump_with_receive(Duration::ZERO, |_, buffer| {
                reads += 1;
                assert!(reads <= 4, "the pump attempted a read beyond its budget");
                let outcome = pattern % 3;
                pattern /= 3;
                match outcome {
                    0 => {
                        datagrams += 1;
                        buffer[0] = 0;
                        Ok((1, loopback()))
                    }
                    1 => Err(std::io::Error::from(ErrorKind::ConnectionRefused)),
                    _ => Err(std::io::Error::from(ErrorKind::ConnectionReset)),
                }
            })
            .unwrap();
        assert_eq!(reads, 4);
        assert_eq!(pumped.received, datagrams);
        let after = world.server.stats();
        assert_eq!(after.received - before.received, datagrams as u64);
        assert_eq!(
            after.send_errors - before.send_errors,
            (4 - datagrams) as u64
        );
    }

    // A blocking first read still returns promptly on an ICMP error, and an
    // empty socket stops immediately regardless of the remaining budget.
    for (wait, error) in [
        (Duration::from_secs(1), ErrorKind::ConnectionRefused),
        (Duration::from_secs(1), ErrorKind::ConnectionReset),
        (Duration::ZERO, ErrorKind::WouldBlock),
        (Duration::from_secs(1), ErrorKind::TimedOut),
    ] {
        let mut reads = 0;
        let pumped = world
            .server
            .pump_with_receive(wait, |_, _| {
                reads += 1;
                Err(std::io::Error::from(error))
            })
            .unwrap();
        assert_eq!(reads, 1);
        assert_eq!(pumped.received, 0);
    }
}

/// Huge monotonic values must not overflow the independent wall clock: the
/// injectable Clock contract does not require these two epochs to coincide.
#[derive(Clone)]
struct LimitClock(Manual);

impl Clock for LimitClock {
    fn now(&self) -> Duration {
        self.0.now()
    }

    fn wall(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000)
    }
}

#[test]
fn keepalive_overflow_never_sends_early_or_panics() {
    // Cover overflow on becoming protected, overflow on rescheduling, and a
    // deadline exactly at MAX (which is representable and fires just once).
    for interval in [25, 5, 6] {
        // Leave room for the peer's initial handshake retry deadline.
        let start = Duration::MAX - Duration::from_secs(6);
        let clock = LimitClock(Manual(Rc::new(Cell::new(start))));
        let mut relay = Relay::new();
        let mut server = Tunnel::new(
            server_config(relay.address(0)),
            Stream::new(1_000_000),
            clock.clone(),
        )
        .unwrap();
        relay.server = Some(server.local_addr().unwrap());
        let mut config = client_config(relay.address(0));
        config.keepalive = Some(Duration::from_secs(interval));
        let mut client = Tunnel::new(config, Stream::new(10), clock.clone()).unwrap();
        for _ in 0..20 {
            relay.step();
            server.pump(Duration::ZERO).unwrap();
            relay.step();
            client.pump(Duration::ZERO).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(client.status().state, TunnelState::Protected);
        assert_eq!(client.status().generation, Some(1));
        let expected = start.checked_add(Duration::from_secs(interval));
        assert_eq!(client.live.as_ref().unwrap().keepalive_at, expected);
        let sent = client.stats().sent;
        if let Some(deadline) = expected {
            clock.0.0.set(deadline - Duration::from_nanos(1));
            client.pump(Duration::ZERO).unwrap();
            assert_eq!(client.stats().sent, sent);
            clock.0.0.set(deadline);
            client.pump(Duration::ZERO).unwrap();
            assert_eq!(client.stats().sent, sent + 1);
            assert_eq!(client.live.as_ref().unwrap().keepalive_at, None);
        }
        let sent = client.stats().sent;
        clock.0.0.set(Duration::MAX);
        for _ in 0..3 {
            client.pump(Duration::ZERO).unwrap();
        }
        assert_eq!(client.stats().sent, sent, "no truncated keepalive interval");
        assert_eq!(client.status().generation, Some(1));
        assert_eq!(client.status().state, TunnelState::Protected);
        let local = client.udp_bind(0).unwrap();
        client
            .udp_send_to(local, (SERVER_IP, 7000), b"at the clock limit")
            .unwrap();
        assert_eq!(client.stats().sent, sent + 1);
        assert_eq!(client.status().generation, Some(1));

        // An extreme clock still obeys the generation fence: moving backwards
        // revokes every old handle and publishes nothing from the lost session.
        let sent = client.stats().sent;
        clock.0.0.set(Duration::MAX - Duration::from_secs(1));
        client.pump(Duration::ZERO).unwrap();
        assert_eq!(client.status().state, TunnelState::Reconnecting);
        assert_eq!(client.status().reason, Some(Reason::Failed));
        assert_eq!(client.status().generation, None);
        assert_eq!(client.stats().sent, sent);
        assert_eq!(client.udp_recv(local), Err(TunnelError::Revoked));
        assert_eq!(
            client.udp_send_to(local, (SERVER_IP, 7000), b"revoked"),
            Err(TunnelError::Revoked)
        );
    }
}

#[test]
fn truncated_dns_falls_back_to_tcp_inside_the_encrypted_tunnel() {
    let mut world = World::new(|_, _| {});
    world.connect(Duration::from_millis(100), 20);
    let udp = world.server.udp_bind(53).unwrap();
    let listener = world.server.tcp_listen(53).unwrap();
    let lookup = world.client.resolve("Tracker.Example").unwrap();
    world.settle();
    let question = drain(&mut world.server, udp).remove(0);
    let mut truncated = question.payload.clone();
    truncated[2..4].copy_from_slice(&[0x83, 0x80]);
    world
        .server
        .udp_send_to(udp, (CLIENT_IP, question.source_port), &truncated)
        .unwrap();
    world.settle();
    let flow = world.server.tcp_accept(listener).unwrap().unwrap();
    let mut request = [0; 512];
    let Read::Data(length) = world.server.tcp_recv(flow, &mut request).unwrap() else {
        panic!("no virtual TCP question")
    };
    assert_eq!(
        u16::from_be_bytes([request[0], request[1]]) as usize,
        length - 2
    );
    assert_eq!(&request[2..length], &question.payload);
    let response = answer(&request[2..length], [10, 9, 8, 7]);
    let mut framed = (response.len() as u16).to_be_bytes().to_vec();
    framed.extend_from_slice(&response);
    world.server.tcp_send(flow, &framed).unwrap();
    world.settle();
    let result = world
        .client
        .lookup_result(lookup)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.addresses, vec![Ipv4Addr::new(10, 9, 8, 7)]);
    assert_eq!(world.client.stats().inner_rejected, 0);
}
