//! WireGuard session timers and roaming (#321) and host route loss (#328)
//! through the native owner. The client owner keeps OS entropy and the system
//! clock; only its monotonic time is skewed forward (test-only) to reach
//! session ages. Route loss is injected at the owner's outer socket calls; the
//! host routing table is never changed. Every service is virtual behind the
//! fixture relay, so the host sees only outer UDP.
use super::*;
use crate::{
    torrent::transport::Network,
    tunnel::{Reason, Stats},
    wireguard::{REJECT_AFTER_TIME, REKEY_AFTER_TIME},
};
use std::net::SocketAddr;

fn peer() -> SocketAddr {
    "10.7.0.1:6881".parse().unwrap()
}
fn network(f: &Fixture) -> Arc<dyn Network> {
    f.driver.network(end()).unwrap()
}
fn telemetry(f: &Fixture) -> (Status, Stats) {
    let (status, stats, _) = f.driver.telemetry(end()).unwrap();
    (status, stats)
}
/// Advances only the client owner's monotonic clock.
fn skip(f: &Fixture, by: Duration) {
    f.driver
        .owner
        .call(None, end(), move |r| {
            r.tunnel.clock().skip(by);
            Ok(())
        })
        .unwrap();
}
/// Emulates a host route change on the client owner's outer socket.
fn lose_route(f: &Fixture, kind: Option<io::ErrorKind>) {
    f.driver
        .owner
        .call(None, end(), move |r| {
            r.tunnel.lose_route(kind);
            Ok(())
        })
        .unwrap();
}
fn kinds(f: &Fixture, kind: u8) -> usize {
    let wire = f.wire.lock().unwrap();
    wire.iter().filter(|d| d.first() == Some(&kind)).count()
}
fn pattern(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + seed * 7) % 251) as u8)
        .collect()
}
/// Sends a distinct pattern through the echo peer and verifies every byte.
fn echo(stream: &mut dyn NetworkStream, seed: usize, len: usize) {
    let data = pattern(seed, len);
    stream.write_all(&data).unwrap();
    let mut back = vec![0; len];
    stream.read_exact(&mut back).unwrap();
    assert!(back == data, "echoed bytes differ");
}
fn connect(network: &dyn Network) -> Box<dyn NetworkStream> {
    let stream = network.connect(peer(), Duration::from_secs(3)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
}
fn get(connector: Arc<dyn HttpConnector>) -> crate::Result<Vec<u8>> {
    HttpClient::new()
        .with_connector(connector)
        .with_timeout(Duration::from_secs(3))
        .get("http://first.example:8080/redirect")
        .map(|response| response.body)
}
fn wait(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "condition not reached");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn rekey_concurrent_with_new_operations_keeps_the_generation_and_stream_bytes() {
    let f = Fixture::new();
    let network = network(&f);
    let token = network.generation().unwrap();
    let mut stream = connect(&*network);
    echo(&mut *stream, 1, 16 * 1024);
    let (before, stats) = telemetry(&f);
    let initiations = kinds(&f, 1);
    assert_eq!(before.state, TunnelState::Protected);

    // The session reaches the rekey age; new operations race the handshake.
    skip(&f, REKEY_AFTER_TIME + Duration::from_secs(1));
    let workers: Vec<_> = (0..3)
        .map(|index| {
            let network = network.clone();
            let connector = f.connector.clone();
            thread::spawn(move || match index {
                0 => assert_eq!(
                    network.resolve("rekey.example:6881", end()).unwrap(),
                    [peer()]
                ),
                1 => echo(&mut *connect(&*network), 2, 8 * 1024),
                _ => assert_eq!(get(connector).unwrap(), b"protected-http-body"),
            })
        })
        .collect();
    echo(&mut *stream, 3, 64 * 1024);
    for worker in workers {
        worker.join().unwrap();
    }
    wait(|| {
        kinds(&f, 1) > initiations
            && f.driver
                .telemetry(end())
                .unwrap()
                .2
                .is_some_and(|age| age < REKEY_AFTER_TIME)
    });

    // The fresh session carries the existing stream and handles unchanged.
    echo(&mut *stream, 4, 32 * 1024);
    let (after, after_stats) = telemetry(&f);
    assert_eq!(after.state, TunnelState::Protected);
    assert_eq!(after.generation, before.generation);
    assert_eq!(after_stats.generations, stats.generations);
    assert_eq!(network.generation().unwrap(), token);
    assert_eq!(get(f.connector.clone()).unwrap(), b"protected-http-body");
    let wire = f.wire.lock().unwrap();
    assert!(wire.iter().all(|d| matches!(d.first(), Some(1..=4))));
    assert!(!wire.iter().any(|d| d.windows(5).any(|w| w == b"rekey")));
}

#[test]
fn session_expiry_revokes_new_operations_and_reconnect_needs_a_fresh_single_listener() {
    let f = Fixture::new();
    let network = network(&f);
    let listener = network
        .listen(SocketAddr::from(([0, 0, 0, 0], 7001)))
        .unwrap();
    let mut stream = connect(&*network);
    echo(&mut *stream, 1, 8 * 1024);
    let (before, stats) = telemetry(&f);
    let generation = before.generation.unwrap();

    // The outer path goes silent, a read is pending, then the session expires.
    f.block.store(true, Ordering::Release);
    let pending = thread::spawn(move || {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.read(&mut [0; 16])
    });
    thread::sleep(Duration::from_millis(50));
    skip(&f, REJECT_AFTER_TIME + Duration::from_secs(1));
    let mut lost = None;
    wait(|| {
        let (status, _) = telemetry(&f);
        let done = status.generation != Some(generation);
        lost = Some(status);
        done
    });
    let lost = lost.unwrap();
    assert_ne!(lost.state, TunnelState::Protected);
    assert_eq!(lost.reason, Some(Reason::Expired));
    assert!(
        !matches!(pending.join().unwrap(), Ok(n) if n > 0),
        "a revoked stream returned data"
    );

    // New operations concurrent with the loss fail; nothing falls back.
    assert!(network.connect(peer(), Duration::from_secs(1)).is_err());
    assert!(network.resolve("expired.example:6881", end()).is_err());
    assert!(network.datagram(false).is_err());
    assert!(
        network
            .listen(SocketAddr::from(([0, 0, 0, 0], 7002)))
            .is_err()
    );
    assert_ne!(
        listener.accept().err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(get(f.connector.clone()).is_err());
    assert_eq!(
        f.driver.network(end()).err().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(f.driver.connector(end()).is_err());

    // The path returns. A scheduled retry builds the fresh generation; the
    // clock moves on gradually so a handshake retransmission is soon due.
    f.block.store(false, Ordering::Release);
    wait(|| {
        if telemetry(&f).0.state == TunnelState::Protected {
            return true;
        }
        skip(&f, Duration::from_secs(1));
        thread::sleep(Duration::from_millis(50));
        false
    });
    let (after, after_stats) = telemetry(&f);
    assert!(after.generation.unwrap() > generation);
    assert!(after_stats.generations > stats.generations);
    let fresh = f.driver.network(end()).unwrap();

    // Old handles stay revoked; the listener port is acquired exactly once.
    assert!(network.generation().is_err());
    assert!(network.connect(peer(), Duration::from_secs(1)).is_err());
    assert!(
        network
            .listen(SocketAddr::from(([0, 0, 0, 0], 7001)))
            .is_err()
    );
    assert_ne!(
        listener.accept().err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(get(f.connector.clone()).is_err());
    let single = fresh
        .listen(SocketAddr::from(([0, 0, 0, 0], 7001)))
        .unwrap();
    assert_eq!(single.port(), 7001);
    assert!(
        fresh
            .listen(SocketAddr::from(([0, 0, 0, 0], 7001)))
            .is_err(),
        "duplicate listener"
    );
    f.dial.lock().unwrap().push(7001);
    let mut inbound = None;
    wait(|| {
        inbound = single.accept().ok();
        inbound.is_some()
    });
    let (mut inbound, _) = inbound.unwrap();
    inbound
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut hello = [0; 13];
    inbound.read_exact(&mut hello).unwrap();
    assert_eq!(&hello, b"inbound-hello");
    echo(&mut *connect(&*fresh), 2, 32 * 1024);
    assert_eq!(
        get(f.driver.connector(end()).unwrap()).unwrap(),
        b"protected-http-body"
    );
    let wire = f.wire.lock().unwrap();
    assert!(wire.iter().all(|d| matches!(d.first(), Some(1..=4))));
}

#[test]
fn authenticated_roaming_moves_the_outer_endpoint_without_a_new_generation() {
    let f = Fixture::new();
    let network = network(&f);
    let mut stream = connect(&*network);
    echo(&mut *stream, 1, 8 * 1024);
    let (before, stats) = telemetry(&f);
    let original = before.remote.unwrap();
    assert_ne!(original, f.roamed);
    assert_eq!(f.arrivals[1].load(Ordering::Relaxed), 0);
    // Captured before roaming: the client has already accepted all of these.
    let replay = f
        .inbound
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|d| d.first() == Some(&4) && d.len() > 32)
        .unwrap()
        .clone();

    // Server datagrams now come from a new source; the stream continues.
    f.roam.store(true, Ordering::Release);
    echo(&mut *stream, 2, 32 * 1024);
    let (moved, moved_stats) = telemetry(&f);
    assert_eq!(moved.remote, Some(f.roamed));
    assert_eq!(moved_stats.roamed, stats.roamed + 1);
    assert_eq!(moved.generation, before.generation);
    assert_eq!(moved_stats.generations, stats.generations);

    // Later client datagrams leave only toward the roamed source.
    thread::sleep(Duration::from_millis(50));
    let stale = f.arrivals[0].load(Ordering::Relaxed);
    let roamed = f.arrivals[1].load(Ordering::Relaxed);
    assert!(roamed > 0);
    assert_eq!(
        network.resolve("moved.example:6881", end()).unwrap(),
        [peer()]
    );
    echo(&mut *connect(&*network), 3, 8 * 1024);
    assert_eq!(get(f.connector.clone()).unwrap(), b"protected-http-body");
    assert_eq!(f.arrivals[0].load(Ordering::Relaxed), stale);
    assert!(f.arrivals[1].load(Ordering::Relaxed) > roamed);

    // A replayed authentic datagram and a forgery from another owned source
    // are refused and never redirect the client.
    let spoof = UdpSocket::bind("127.0.0.1:0").unwrap();
    allow_outer(spoof.local_addr().unwrap());
    let client = f
        .driver
        .owner
        .call(None, end(), |r| r.tunnel.local_addr().map_err(failed))
        .unwrap();
    let rejected = telemetry(&f).1.rejected;
    spoof.send_to(&replay, client).unwrap();
    spoof.send_to(&[4; 64], client).unwrap();
    wait(|| telemetry(&f).1.rejected >= rejected + 2);
    echo(&mut *stream, 4, 8 * 1024);
    let (after, after_stats) = telemetry(&f);
    assert_eq!(after.remote, Some(f.roamed));
    assert_eq!(after_stats.roamed, stats.roamed + 1);
    assert_eq!(after_stats.rejected, rejected + 2);
    assert_eq!(after.generation, before.generation);
    assert_eq!(f.arrivals[0].load(Ordering::Relaxed), stale);
}

#[test]
fn engine_pause_selection_and_verified_bytes_survive_rekey() {
    let f = Fixture::new();
    crate::vpn::tests::retained_engine_session_change(f.driver.clone(), false, |engine| {
        let initiations = kinds(&f, 1);
        skip(&f, REKEY_AFTER_TIME + Duration::from_secs(1));
        assert_eq!(
            engine
                .torrent_network
                .resolve("retained-rekey.example:6881", end())
                .unwrap(),
            [peer()]
        );
        echo(&mut *connect(&*engine.torrent_network), 7, 32 * 1024);
        wait(|| {
            kinds(&f, 1) > initiations
                && f.driver
                    .telemetry(end())
                    .unwrap()
                    .2
                    .is_some_and(|age| age < REKEY_AFTER_TIME)
        });
        echo(&mut *connect(&*engine.torrent_network), 8, 8 * 1024);
    });
}

#[test]
fn engine_pause_selection_and_verified_bytes_survive_authenticated_roaming() {
    let f = Fixture::new();
    crate::vpn::tests::retained_engine_session_change(f.driver.clone(), false, |engine| {
        let before = telemetry(&f).1.roamed;
        let mut stream = connect(&*engine.torrent_network);
        echo(&mut *stream, 9, 8 * 1024);
        f.roam.store(true, Ordering::Release);
        echo(&mut *stream, 10, 32 * 1024);
        wait(|| telemetry(&f).0.remote == Some(f.roamed));
        assert_eq!(telemetry(&f).1.roamed, before + 1);
        assert_eq!(
            engine
                .torrent_network
                .resolve("retained-roam.example:6881", end())
                .unwrap(),
            [peer()]
        );
        echo(&mut *connect(&*engine.torrent_network), 11, 8 * 1024);
        assert!(f.arrivals[1].load(Ordering::Relaxed) > 0);
    });
}

#[test]
fn engine_pause_selection_and_verified_bytes_survive_expiry_and_replacement() {
    let f = Fixture::new();
    crate::vpn::tests::retained_engine_session_change(f.driver.clone(), true, |engine| {
        let generation = telemetry(&f).0.generation;
        f.block.store(true, Ordering::Release);
        skip(&f, REJECT_AFTER_TIME + Duration::from_secs(1));
        wait(|| telemetry(&f).0.generation != generation);
        assert_eq!(telemetry(&f).0.reason, Some(Reason::Expired));
        assert!(engine.vpn_needs_reload());
        assert!(engine.torrent_publication().is_err());
        assert!(!engine.tick().unwrap());
        assert!(
            engine
                .torrent_network
                .resolve("expired-retained.example:80", end())
                .is_err()
        );
        assert!(
            engine
                .torrent_network
                .connect(peer(), Duration::from_secs(1))
                .is_err()
        );
        f.block.store(false, Ordering::Release);
        wait(|| {
            if telemetry(&f).0.state == TunnelState::Protected {
                return true;
            }
            skip(&f, Duration::from_secs(1));
            thread::sleep(Duration::from_millis(50));
            false
        });
        assert!(telemetry(&f).0.generation > generation);
        assert!(engine.torrent_publication().is_err());
    });
}

#[test]
fn host_route_loss_shorter_than_the_session_keeps_the_generation_and_stream_bytes() {
    let f = Fixture::new();
    let network = network(&f);
    let token = network.generation().unwrap();
    let mut stream = connect(&*network);
    echo(&mut *stream, 1, 8 * 1024);
    let (before, stats) = telemetry(&f);
    assert_eq!(before.state, TunnelState::Protected);

    // The route disappears: outer sends fail and reads report the network
    // down. New operations and stream bytes are started during the loss.
    lose_route(&f, Some(io::ErrorKind::NetworkDown));
    let arrived = f.arrivals[0].load(Ordering::Relaxed);
    let workers: Vec<_> = (0..3)
        .map(|index| {
            let network = network.clone();
            let connector = f.connector.clone();
            thread::spawn(move || match index {
                0 => assert_eq!(
                    network.resolve("route.example:6881", end()).unwrap(),
                    [peer()]
                ),
                1 => echo(&mut *connect(&*network), 2, 8 * 1024),
                _ => assert_eq!(get(connector).unwrap(), b"protected-http-body"),
            })
        })
        .collect();
    let pending = thread::spawn(move || {
        echo(&mut *stream, 3, 32 * 1024);
        stream
    });
    wait(|| telemetry(&f).1.send_errors >= stats.send_errors + 8);
    thread::sleep(Duration::from_millis(200));
    // Nothing left the outer socket; the owner is still serving commands.
    assert_eq!(f.arrivals[0].load(Ordering::Relaxed), arrived);
    let (lost, _) = telemetry(&f);
    assert_eq!(lost.generation, before.generation);

    // The route returns within the session: the same generation carries the
    // pending stream bytes and the operations started during the loss.
    lose_route(&f, None);
    let mut stream = pending.join().unwrap();
    for worker in workers {
        worker.join().unwrap();
    }
    echo(&mut *stream, 4, 16 * 1024);
    let (after, after_stats) = telemetry(&f);
    assert_eq!(after.state, TunnelState::Protected);
    assert_eq!(after.generation, before.generation);
    assert_eq!(after_stats.generations, stats.generations);
    assert_eq!(network.generation().unwrap(), token);
    assert!(f.arrivals[0].load(Ordering::Relaxed) > arrived);
    let wire = f.wire.lock().unwrap();
    assert!(wire.iter().all(|d| matches!(d.first(), Some(1..=4))));
    assert!(!wire.iter().any(|d| d.windows(5).any(|w| w == b"route")));
}

#[test]
fn host_route_loss_past_session_expiry_needs_a_fresh_generation_without_fallback() {
    let f = Fixture::new();
    let network = network(&f);
    let listener = network
        .listen(SocketAddr::from(([0, 0, 0, 0], 7001)))
        .unwrap();
    echo(&mut *connect(&*network), 1, 8 * 1024);
    let (before, stats) = telemetry(&f);
    let generation = before.generation.unwrap();

    // The route stays away until the session expires; the owner survives.
    lose_route(&f, Some(io::ErrorKind::NetworkUnreachable));
    let arrived = f.arrivals[0].load(Ordering::Relaxed);
    skip(&f, REJECT_AFTER_TIME + Duration::from_secs(1));
    let mut lost = None;
    wait(|| {
        let (status, _) = telemetry(&f);
        let done = status.generation != Some(generation);
        lost = Some(status);
        done
    });
    let lost = lost.unwrap();
    assert_ne!(lost.state, TunnelState::Protected);
    assert_eq!(lost.reason, Some(Reason::Expired));

    // New operations during the loss fail; nothing falls back or leaves.
    assert!(network.connect(peer(), Duration::from_secs(1)).is_err());
    assert!(network.resolve("lost.example:6881", end()).is_err());
    assert!(network.datagram(false).is_err());
    assert_ne!(
        listener.accept().err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(get(f.connector.clone()).is_err());
    assert_eq!(
        f.driver.network(end()).err().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(f.arrivals[0].load(Ordering::Relaxed), arrived);
    assert!(telemetry(&f).1.send_errors > stats.send_errors);

    // The route returns; a scheduled retry builds the fresh generation.
    lose_route(&f, None);
    wait(|| {
        if telemetry(&f).0.state == TunnelState::Protected {
            return true;
        }
        skip(&f, Duration::from_secs(1));
        thread::sleep(Duration::from_millis(50));
        false
    });
    let (after, after_stats) = telemetry(&f);
    assert!(after.generation.unwrap() > generation);
    assert!(after_stats.generations > stats.generations);
    let fresh = f.driver.network(end()).unwrap();

    // Old handles stay revoked; the listener port is acquired exactly once.
    assert!(network.generation().is_err());
    assert!(network.connect(peer(), Duration::from_secs(1)).is_err());
    assert!(get(f.connector.clone()).is_err());
    let single = fresh
        .listen(SocketAddr::from(([0, 0, 0, 0], 7001)))
        .unwrap();
    assert!(
        fresh
            .listen(SocketAddr::from(([0, 0, 0, 0], 7001)))
            .is_err(),
        "duplicate listener"
    );
    f.dial.lock().unwrap().push(7001);
    let mut inbound = None;
    wait(|| {
        inbound = single.accept().ok();
        inbound.is_some()
    });
    let (mut inbound, _) = inbound.unwrap();
    inbound
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut hello = [0; 13];
    inbound.read_exact(&mut hello).unwrap();
    assert_eq!(&hello, b"inbound-hello");
    echo(&mut *connect(&*fresh), 2, 32 * 1024);
    assert_eq!(
        get(f.driver.connector(end()).unwrap()).unwrap(),
        b"protected-http-body"
    );
    let wire = f.wire.lock().unwrap();
    assert!(wire.iter().all(|d| matches!(d.first(), Some(1..=4))));
}
