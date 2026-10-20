use super::*;
use crate::torrent::transport::Network;
use std::net::SocketAddr;

fn network(f: &Fixture) -> Arc<dyn Network> {
    f.driver.network(end()).unwrap()
}

fn read_exact(stream: &mut dyn NetworkStream, len: usize) -> Vec<u8> {
    let mut data = vec![0; len];
    stream.read_exact(&mut data).unwrap();
    data
}

fn leases(f: &Fixture) -> usize {
    f.driver
        .owner
        .call(None, end(), |r| Ok(r.leases.len()))
        .unwrap()
}

#[test]
fn network_waits_for_protection_and_has_no_direct_fallback() {
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let driver = HttpTunnel::start(config(sink.local_addr().unwrap(), false)).unwrap();
    assert_eq!(
        driver.network(end()).err().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    driver.cancel();
    assert!(driver.network(end()).is_err());
}

#[test]
fn outgoing_peers_resolve_and_echo_through_the_tunnel_only() {
    let f = Fixture::new();
    let network = network(&f);
    let peers = network.resolve("peer.example:6881", end()).unwrap();
    assert_eq!(peers, ["10.7.0.1:6881".parse::<SocketAddr>().unwrap()]);
    assert_eq!(f.dns.load(Ordering::Relaxed), 1);
    assert_eq!(
        network.resolve("10.7.0.1:6881", end()).unwrap(),
        peers,
        "literal IPv4 needs no lookup"
    );
    assert_eq!(f.dns.load(Ordering::Relaxed), 1);

    let mut stream = network.connect(peers[0], Duration::from_secs(3)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(b"\x13BitTorrent protocol").unwrap();
    assert_eq!(read_exact(&mut *stream, 20), b"\x13BitTorrent protocol");
    drop(stream);

    let wire = f.wire.lock().unwrap();
    assert!(wire.iter().all(|d| matches!(d.first(), Some(1..=4))));
    for secret in [b"BitTorrent".as_slice(), b"peer.example"] {
        assert!(
            !wire
                .iter()
                .any(|d| d.windows(secret.len()).any(|w| w == secret))
        );
    }
}

#[test]
fn inbound_peers_reach_the_virtual_listener_through_the_tunnel() {
    let f = Fixture::new();
    let network = network(&f);
    let listener = network.listen(SocketAddr::from(([0, 0, 0, 0], 0))).unwrap();
    let port = listener.port();
    assert!(port >= crate::netstack::EPHEMERAL_START);
    assert_eq!(
        listener.accept().err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
    f.dial.lock().unwrap().push(port);
    let deadline = end();
    let (mut stream, remote) = loop {
        match listener.accept() {
            Ok(accepted) => break accepted,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => pause(deadline).unwrap(),
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(remote.ip(), IpAddr::V4(SERVER));
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    assert_eq!(read_exact(&mut *stream, 13), b"inbound-hello");
    stream.write_all(b"seed").unwrap();
    assert_eq!(read_exact(&mut *stream, 4), b"seed");

    // A fixed port is honoured once; a second listener on it is refused.
    let fixed = network
        .listen(SocketAddr::from(([127, 0, 0, 1], 7000)))
        .unwrap();
    assert_eq!(fixed.port(), 7000);
    assert!(
        network
            .listen(SocketAddr::from(([0, 0, 0, 0], 7000)))
            .is_err()
    );
    drop(fixed);
    let deadline = end();
    while network
        .listen(SocketAddr::from(([0, 0, 0, 0], 7000)))
        .is_err()
    {
        pause(deadline).unwrap();
    }
}

#[test]
fn datagrams_carry_tracker_exchanges_and_refuse_ipv6() {
    let f = Fixture::new();
    let network = network(&f);
    assert!(network.datagram(true).is_err());
    let socket = network.datagram(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let tracker: SocketAddr = "10.7.0.1:6969".parse().unwrap();
    assert_eq!(socket.send_to(b"announce", tracker).unwrap(), 8);
    let mut buffer = [0; 64];
    let (n, from) = socket.recv_from(&mut buffer).unwrap();
    assert_eq!((&buffer[..n], from), (b"echo:announce".as_slice(), tracker));

    // Truncation like an OS socket, then a bounded empty read.
    socket.send_to(b"truncated", tracker).unwrap();
    let mut small = [0; 4];
    assert_eq!(socket.recv_from(&mut small).unwrap().0, 4);
    socket
        .set_read_timeout(Some(Duration::from_millis(40)))
        .unwrap();
    let start = Instant::now();
    assert_eq!(
        socket.recv_from(&mut buffer).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(socket.set_read_timeout(Some(Duration::ZERO)).is_err());

    let wire = f.wire.lock().unwrap();
    assert!(!wire.iter().any(|d| d.windows(8).any(|w| w == b"announce")));
}

#[test]
fn tracker_http_uses_the_same_generation_bound_connector() {
    let f = Fixture::new();
    let response = network(&f)
        .http()
        .with_timeout(Duration::from_secs(3))
        .get("http://first.example:8080/redirect")
        .unwrap();
    assert_eq!(response.body, b"protected-http-body");
    assert_eq!(f.dns.load(Ordering::Relaxed), 2);
}

#[test]
fn unsupported_destinations_and_names_never_leave_the_tunnel() {
    let f = Fixture::new();
    let network = network(&f);
    for address in [
        "127.0.0.1:6881",
        "0.0.0.0:6881",
        "169.254.1.1:6881",
        "224.0.0.1:6881",
        "255.255.255.255:6881",
        "10.7.0.1:0",
        "[::1]:6881",
        "[2001:db8::1]:6881",
    ] {
        let address: SocketAddr = address.parse().unwrap();
        assert!(
            network.connect(address, Duration::from_secs(1)).is_err(),
            "{address}"
        );
    }
    let socket = network.datagram(false).unwrap();
    for target in ["127.0.0.1:6969", "[::1]:6969", "10.7.0.1:0"] {
        assert!(
            socket.send_to(b"x", target.parse().unwrap()).is_err(),
            "{target}"
        );
    }
    for authority in [
        "[::1]:6881",
        "[2001:db8::1]:6881",
        "::1",
        "peer.example",
        "peer.example:0",
        "peer.example:x",
        "127.0.0.1:6881",
        "trap.example:6881",
    ] {
        assert!(network.resolve(authority, end()).is_err(), "{authority}");
    }
    // Only trap.example reached the tunnel resolver; its loopback answer failed.
    assert_eq!(f.dns.load(Ordering::Relaxed), 1);
    for address in ["[::]:6881", "10.7.0.2:6881", "192.0.2.1:6881"] {
        assert!(
            network.listen(address.parse().unwrap()).is_err(),
            "{address}"
        );
    }
}

#[test]
fn loss_and_cancellation_fence_every_handle_without_fallback() {
    let f = Fixture::with_poll_interval(Duration::from_millis(10));
    let network = network(&f);
    let listener = network.listen(SocketAddr::from(([0, 0, 0, 0], 0))).unwrap();
    let socket = network.datagram(false).unwrap();
    let mut stream = network
        .connect("10.7.0.1:6881".parse().unwrap(), Duration::from_secs(3))
        .unwrap();
    let old_generation = f.driver.status(end()).unwrap().generation;
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
    let fresh = loop {
        let status = f.driver.status(deadline).unwrap();
        if status.state == TunnelState::Protected && status.generation != old_generation {
            break f.driver.network(deadline).unwrap();
        }
        pause(deadline).unwrap();
    };
    let tracker: SocketAddr = "10.7.0.1:6969".parse().unwrap();
    assert!(stream.write(b"old").is_err());
    assert!(socket.send_to(b"old", tracker).is_err());
    assert!(socket.recv_from(&mut [0; 8]).is_err());
    assert_ne!(
        listener.accept().err().unwrap().kind(),
        io::ErrorKind::WouldBlock,
        "a stale listener must stop the accept loop"
    );
    assert!(network.resolve("peer.example:6881", end()).is_err());
    assert!(network.datagram(false).is_err());
    assert!(network.listen(SocketAddr::from(([0, 0, 0, 0], 0))).is_err());
    assert!(network.http().get("http://10.7.0.1:8080/").is_err());
    assert_eq!(leases(&f), 0, "old generation resources are dropped");

    // The fresh generation works until cancellation, then fails the same way.
    let fresh_socket = fresh.datagram(false).unwrap();
    fresh_socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    fresh_socket.send_to(b"new", tracker).unwrap();
    assert_eq!(fresh_socket.recv_from(&mut [0; 16]).unwrap().0, 8);
    f.driver.cancel();
    assert!(fresh_socket.send_to(b"late", tracker).is_err());
    assert!(fresh.connect(tracker, Duration::from_secs(1)).is_err());
    assert!(fresh.resolve("10.7.0.1:6881", end()).is_err());
}

#[test]
fn dropped_datagram_sockets_and_listeners_are_reclaimed() {
    let f = Fixture::new();
    let network = network(&f);
    for _ in 0..8 {
        drop(network.datagram(false).unwrap());
        drop(network.listen(SocketAddr::from(([0, 0, 0, 0], 0))).unwrap());
    }
    let deadline = end();
    while leases(&f) != 0 {
        pause(deadline).unwrap();
    }
    // A reclaimed fixed port can be listened on again.
    drop(
        network
            .listen(SocketAddr::from(([0, 0, 0, 0], 7001)))
            .unwrap(),
    );
    while leases(&f) != 0 {
        pause(deadline).unwrap();
    }
    network
        .listen(SocketAddr::from(([0, 0, 0, 0], 7001)))
        .unwrap();
}
