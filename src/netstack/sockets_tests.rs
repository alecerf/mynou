use super::*;
use crate::netstack::MAX_MTU;
use crate::netstack::support::{Failing, Script};

const G: u64 = 7;
const LOCAL: Ipv4Addr = Ipv4Addr::new(10, 64, 0, 2);
const PEER: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 9);

fn config() -> Ipv4Config {
    Ipv4Config::new(LOCAL, Ipv4Addr::UNSPECIFIED, 0).unwrap()
}

fn table(limits: Limits) -> UdpSockets {
    UdpSockets::new(limits, G)
}

fn inbound(source_port: u16, destination_port: u16, payload: &[u8]) -> Vec<u8> {
    udp::build(PEER, LOCAL, source_port, destination_port, payload, MAX_MTU).unwrap()
}

#[test]
fn explicit_and_ephemeral_binds() {
    let mut sockets = table(Limits::default());
    let mut zero = Script::new(&[0]);
    let fixed = sockets.bind(G, 6881, &mut zero).unwrap();
    assert_eq!(fixed.port(), 6881);
    assert_eq!(
        sockets.bind(G, 6881, &mut zero),
        Err(SocketError::PortInUse)
    );
    assert_eq!(zero.calls(), 0);
    let ephemeral = sockets.bind(G, 0, &mut zero).unwrap();
    assert_eq!(ephemeral.port(), EPHEMERAL_START);
    assert_eq!(sockets.len(), 2);
    // 0xffff maps to the last port; a taken draw is retried with the next one.
    let mut scripted = Script::new(&[0xffff, 16383, 16384, 1]);
    assert_eq!(sockets.bind(G, 0, &mut scripted).unwrap().port(), 65535);
    assert_eq!(sockets.bind(G, 0, &mut scripted).unwrap().port(), 49153);
    assert_eq!(scripted.calls(), 4);
}

#[test]
fn ephemeral_binding_is_bounded_and_never_falls_back() {
    let mut sockets = table(Limits::default());
    let mut constant = Script::new(&[5]);
    sockets.bind(G, 0, &mut constant).unwrap();
    assert_eq!(
        sockets.bind(G, 0, &mut constant),
        Err(SocketError::PortsExhausted)
    );
    assert_eq!(constant.calls(), 1 + BIND_ATTEMPTS);
    assert_eq!(
        sockets.bind(G, 0, &mut Failing),
        Err(SocketError::Entropy("entropy failed".into()))
    );
    assert_eq!(sockets.len(), 1);
    // An explicit port needs no randomness at all.
    assert!(sockets.bind(G, 6881, &mut Failing).is_ok());
}

#[test]
fn table_size_is_limited() {
    let limits = Limits {
        sockets: 2,
        ..Limits::default()
    };
    let mut sockets = table(limits);
    let first = sockets.bind(G, 1000, &mut Failing).unwrap();
    sockets.bind(G, 1001, &mut Failing).unwrap();
    assert_eq!(
        sockets.bind(G, 1002, &mut Failing),
        Err(SocketError::TableFull)
    );
    sockets.close(G, first).unwrap();
    assert!(sockets.bind(G, 1002, &mut Failing).is_ok());
}

#[test]
fn closed_handles_stay_dead_after_the_port_is_reused() {
    let mut sockets = table(Limits::default());
    let old = sockets.bind(G, 4000, &mut Failing).unwrap();
    sockets.close(G, old).unwrap();
    assert_eq!(sockets.close(G, old), Err(SocketError::Closed));
    assert_eq!(sockets.recv(G, old), Err(SocketError::Closed));
    let new = sockets.bind(G, 4000, &mut Failing).unwrap();
    assert_ne!(old, new);
    assert_eq!(sockets.recv(G, old), Err(SocketError::Closed));
    assert_eq!(
        sockets.connect(G, old, (PEER, 53)),
        Err(SocketError::Closed)
    );
    assert_eq!(sockets.recv(G, new), Ok(None));
}

#[test]
fn datagrams_are_queued_in_order_and_accounted() {
    let mut sockets = table(Limits::default());
    let handle = sockets.bind(G, 4000, &mut Failing).unwrap();
    for payload in [&b"one"[..], b"two", b""] {
        assert_eq!(
            sockets.deliver(G, PEER, 53, 4000, payload),
            Delivery::Queued
        );
    }
    assert_eq!(sockets.queued(), 3);
    let first = sockets.recv(G, handle).unwrap().unwrap();
    assert_eq!(first.source, PEER);
    assert_eq!(first.source_port, 53);
    assert_eq!(first.payload, b"one");
    assert_eq!(sockets.recv(G, handle).unwrap().unwrap().payload, b"two");
    assert_eq!(sockets.queued(), 1);
    sockets.close(G, handle).unwrap();
    assert_eq!(sockets.queued(), 0);
    assert_eq!(
        sockets.deliver(G, PEER, 53, 4000, b"late"),
        Delivery::NoSocket
    );
}

#[test]
fn per_socket_and_global_queues_are_bounded() {
    let limits = Limits {
        sockets: 8,
        queued_per_socket: 2,
        queued_total: 3,
    };
    let mut sockets = table(limits);
    let first = sockets.bind(G, 4000, &mut Failing).unwrap();
    let second = sockets.bind(G, 4001, &mut Failing).unwrap();
    assert_eq!(sockets.deliver(G, PEER, 1, 4000, b"a"), Delivery::Queued);
    assert_eq!(sockets.deliver(G, PEER, 1, 4000, b"b"), Delivery::Queued);
    assert_eq!(sockets.deliver(G, PEER, 1, 4000, b"c"), Delivery::QueueFull);
    assert_eq!(sockets.deliver(G, PEER, 1, 4001, b"d"), Delivery::Queued);
    assert_eq!(sockets.deliver(G, PEER, 1, 4001, b"e"), Delivery::QueueFull);
    assert_eq!(sockets.queued(), 3);
    // Draining one frees exactly one slot, for any socket.
    sockets.recv(G, first).unwrap().unwrap();
    assert_eq!(sockets.deliver(G, PEER, 1, 4001, b"f"), Delivery::Queued);
    assert_eq!(sockets.recv(G, second).unwrap().unwrap().payload, b"d");
}

#[test]
fn a_connected_socket_filters_other_remotes() {
    let mut sockets = table(Limits::default());
    let handle = sockets.bind(G, 4000, &mut Failing).unwrap();
    sockets.connect(G, handle, (PEER, 53)).unwrap();
    let other = Ipv4Addr::new(203, 0, 113, 10);
    assert_eq!(
        sockets.deliver(G, other, 53, 4000, b"x"),
        Delivery::Filtered
    );
    assert_eq!(sockets.deliver(G, PEER, 54, 4000, b"x"), Delivery::Filtered);
    assert_eq!(sockets.queued(), 0);
    assert_eq!(sockets.deliver(G, PEER, 53, 4000, b"x"), Delivery::Queued);
    sockets.connect(G, handle, (other, 53)).unwrap();
    assert_eq!(sockets.deliver(G, PEER, 53, 4000, b"y"), Delivery::Filtered);
}

#[test]
fn send_builds_a_checked_packet_from_the_socket_port() {
    let mut sockets = table(Limits::default());
    let handle = sockets.bind(G, 4000, &mut Failing).unwrap();
    let packet = sockets
        .send(G, handle, LOCAL, (PEER, 53), b"query", MAX_MTU)
        .unwrap();
    let remote = Ipv4Config::new(PEER, Ipv4Addr::UNSPECIFIED, 0).unwrap();
    let ip = ipv4::parse(&remote, &packet).unwrap();
    let datagram = udp::parse(ip.source, ip.destination, ip.payload).unwrap();
    assert_eq!((ip.source, ip.destination), (LOCAL, PEER));
    assert_eq!(
        (datagram.source_port, datagram.destination_port),
        (4000, 53)
    );
    assert_eq!(datagram.payload, b"query");
    sockets.connect(G, handle, (PEER, 53)).unwrap();
    assert_eq!(
        sockets.send(G, handle, LOCAL, (PEER, 54), b"x", MAX_MTU),
        Err(SocketError::Remote)
    );
    assert_eq!(
        sockets.send(G, handle, LOCAL, (PEER, 53), &[0; 1397], MAX_MTU),
        Err(SocketError::Build(ipv4::BuildError::TooLarge))
    );
}

#[test]
fn ingest_validates_both_layers_before_delivery() {
    let mut sockets = table(Limits::default());
    let handle = sockets.bind(G, 4000, &mut Failing).unwrap();
    let config = config();
    let good = inbound(53, 4000, b"answer");
    assert_eq!(
        sockets.ingest(G, &config, &good),
        Ingest::Delivered(Delivery::Queued)
    );
    assert_eq!(sockets.recv(G, handle).unwrap().unwrap().payload, b"answer");
    assert_eq!(
        sockets.ingest(G, &config, &inbound(53, 4001, b"x")),
        Ingest::Delivered(Delivery::NoSocket)
    );
    let mut corrupt = good.clone();
    corrupt[15] ^= 1;
    assert_eq!(
        sockets.ingest(G, &config, &corrupt),
        Ingest::Ip(PacketError::Checksum)
    );
    let misrouted = udp::build(PEER, Ipv4Addr::new(10, 64, 0, 3), 53, 4000, b"x", MAX_MTU);
    assert_eq!(
        sockets.ingest(G, &config, &misrouted.unwrap()),
        Ingest::Ip(PacketError::Destination)
    );
    let mut bad_udp = good;
    let last = bad_udp.len() - 1;
    bad_udp[last] ^= 1;
    assert_eq!(
        sockets.ingest(G, &config, &bad_udp),
        Ingest::Udp(DatagramError::Checksum)
    );
    assert_eq!(sockets.queued(), 0);
}

#[test]
fn a_different_generation_revokes_everything_irreversibly() {
    let mut sockets = table(Limits::default());
    let handle = sockets.bind(G, 4000, &mut Failing).unwrap();
    sockets.deliver(G, PEER, 1, 4000, b"x");
    assert_eq!(sockets.recv(G + 1, handle), Err(SocketError::Revoked));
    assert_eq!((sockets.len(), sockets.queued()), (0, 0));
    assert_eq!(sockets.recv(G, handle), Err(SocketError::Revoked));
    assert_eq!(
        sockets.bind(G, 4000, &mut Failing),
        Err(SocketError::Revoked)
    );
    assert_eq!(sockets.deliver(G, PEER, 1, 4000, b"x"), Delivery::Revoked);
    assert_eq!(
        sockets.ingest(G, &config(), &inbound(1, 4000, b"x")),
        Ingest::Delivered(Delivery::Revoked)
    );
}

#[test]
fn stale_generation_blocks_every_operation() {
    for operation in 0..5 {
        let mut sockets = table(Limits::default());
        let handle = sockets.bind(G, 4000, &mut Failing).unwrap();
        let stale = G + 1;
        match operation {
            0 => assert_eq!(sockets.close(stale, handle), Err(SocketError::Revoked)),
            1 => assert_eq!(
                sockets.connect(stale, handle, (PEER, 1)),
                Err(SocketError::Revoked)
            ),
            2 => assert_eq!(
                sockets.deliver(stale, PEER, 1, 4000, b""),
                Delivery::Revoked
            ),
            3 => assert_eq!(
                sockets.bind(stale, 4001, &mut Failing),
                Err(SocketError::Revoked)
            ),
            _ => assert_eq!(
                sockets.send(stale, handle, LOCAL, (PEER, 1), b"", MAX_MTU),
                Err(SocketError::Revoked)
            ),
        }
        assert!(sockets.is_empty(), "operation {operation}");
    }
}

#[test]
fn explicit_revocation_drops_queues() {
    let mut sockets = table(Limits::default());
    sockets.bind(G, 4000, &mut Failing).unwrap();
    sockets.deliver(G, PEER, 1, 4000, b"x");
    sockets.revoke();
    assert_eq!((sockets.len(), sockets.queued()), (0, 0));
    assert_eq!(sockets.deliver(G, PEER, 1, 4000, b"x"), Delivery::Revoked);
}

#[test]
fn icmp_udp_matches_only_the_last_connected_transmission_and_caps_mtu() {
    use crate::netstack::support::icmp_unreachable;
    use crate::netstack::{IcmpEffect, IcmpIngest, MAX_MTU};
    let local = Ipv4Addr::new(10, 64, 0, 2);
    let remote = Ipv4Addr::new(192, 0, 2, 1);
    let router = Ipv4Addr::new(192, 0, 2, 2);
    let ip = Ipv4Config::new(local, Ipv4Addr::UNSPECIFIED, 0).unwrap();
    let mut sockets = UdpSockets::new(Limits::default(), 1);
    let mut entropy = Script::new(&[1]);
    let socket = sockets.bind(1, 10000, &mut entropy).unwrap();
    sockets.connect(1, socket, (remote, 53)).unwrap();
    let packet = sockets
        .send(1, socket, local, (remote, 53), &[7; 1200], MAX_MTU)
        .unwrap();
    let advice = icmp_unreachable(router, &packet, 4, 800);
    for length in 0..advice.len() {
        assert!(!matches!(
            sockets.ingest_icmp(1, &ip, &advice[..length]),
            IcmpIngest::Applied(_)
        ));
    }
    for index in 0..advice.len() {
        let mut corrupt = advice.clone();
        corrupt[index] ^= 1;
        assert!(!matches!(
            sockets.ingest_icmp(1, &ip, &corrupt),
            IcmpIngest::Applied(_)
        ));
    }
    assert_eq!(
        sockets.ingest_icmp(1, &ip, &advice),
        IcmpIngest::Applied(IcmpEffect::PathMtu(800))
    );
    assert_eq!(sockets.ingest_icmp(1, &ip, &advice), IcmpIngest::Dropped);
    assert_eq!(
        sockets.send(1, socket, local, (remote, 53), &[7; 773], MAX_MTU),
        Err(SocketError::Build(ipv4::BuildError::TooLarge))
    );
    let packet = sockets
        .send(1, socket, local, (remote, 53), b"query", MAX_MTU)
        .unwrap();
    let error = icmp_unreachable(router, &packet, 3, 0);
    assert_eq!(
        sockets.ingest_icmp(1, &ip, &error),
        IcmpIngest::Applied(IcmpEffect::Unreachable(3))
    );
    assert_eq!(sockets.recv(1, socket), Err(SocketError::Unreachable(3)));
    assert_eq!(sockets.recv(1, socket), Ok(None));
    sockets.close(1, socket).unwrap();
    let socket = sockets.bind(1, 10000, &mut entropy).unwrap();
    sockets.connect(1, socket, (remote, 53)).unwrap();
    assert_eq!(sockets.ingest_icmp(1, &ip, &error), IcmpIngest::Dropped);
    assert_eq!(sockets.ingest_icmp(2, &ip, &error), IcmpIngest::Revoked);
    assert!(sockets.is_empty());
}
