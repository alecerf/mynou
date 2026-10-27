use super::*;
use crate::json::Value;
use crate::netstack::icmp::{self, Effect as IcmpEffect, Ingest as IcmpIngest};
use crate::netstack::ip;
use crate::netstack::support::{Script, binary, fixtures, number, text};
use crate::netstack::tcp::{self, ACK, Header, SACK_BLOCKS, SYN};
use crate::netstack::{
    Delivery, DnsClient, DnsConfig, Ingest, Limits, Record, Reply, ReplyError, SocketError,
    TcpConfig, TcpStack, TcpState, UdpSockets, parse_response_for, udp,
};
use std::net::Ipv4Addr;
use std::time::Duration;

const G: u64 = 3;
const LOCAL4: Ipv4Addr = Ipv4Addr::new(10, 64, 0, 2);

fn vectors() -> Value {
    fixtures().get("ipv6").unwrap().clone()
}

fn local() -> Ipv6Addr {
    text(&vectors(), "local").parse().unwrap()
}

fn remote() -> Ipv6Addr {
    text(&vectors(), "remote").parse().unwrap()
}

fn config() -> IpConfig {
    IpConfig::new(LOCAL4, Ipv4Addr::UNSPECIFIED, 0)
        .unwrap()
        .with_ipv6(local(), Ipv6Addr::UNSPECIFIED, 0)
        .unwrap()
}

fn sample() -> Vec<u8> {
    udp::build(remote(), local(), 53, 40000, b"synthetic", MAX_MTU).unwrap()
}

fn edited(edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut packet = sample();
    edit(&mut packet);
    packet
}

/// An ICMPv6 message from `router` to us, with its pseudo-header checksum.
fn icmpv6(router: Ipv6Addr, mut message: Vec<u8>) -> Vec<u8> {
    let pseudo = ip::pseudo_sum(
        IpAddr::V6(router),
        IpAddr::V6(local()),
        PROTOCOL_ICMPV6,
        message.len(),
    );
    let value = !crate::netstack::ipv4::fold(crate::netstack::ipv4::sum(&message, pseudo));
    message[2..4].copy_from_slice(&value.to_be_bytes());
    build(router, local(), PROTOCOL_ICMPV6, &message, MAX_MTU).unwrap()
}

/// Packet-too-big (type 2) or destination-unreachable (type 1) quoting the
/// first bytes of `original`, as a router may.
fn advice(kind: u8, code: u8, field: u32, original: &[u8]) -> Vec<u8> {
    let mut message = vec![kind, code, 0, 0];
    message.extend_from_slice(&field.to_be_bytes());
    message.extend_from_slice(&original[..original.len().min(1232)]);
    icmpv6("2001:db8:ffff::1".parse().unwrap(), message)
}

#[test]
fn independent_udp_and_tcp_reference_packets() {
    let vectors = vectors();
    let config = config();
    for row in vectors.get("udp").unwrap().as_array().unwrap() {
        let packet = binary(row, "packet");
        let parsed = ip::parse_protocol(&config, &packet, crate::netstack::PROTOCOL_UDP).unwrap();
        assert_eq!(parsed.source, IpAddr::V6(remote()));
        let datagram = udp::parse(parsed.source, parsed.destination, parsed.payload).unwrap();
        assert_eq!(u64::from(datagram.source_port), number(row, "source_port"));
        assert_eq!(datagram.payload, binary(row, "payload"));
        let rebuilt = udp::build(
            remote(),
            local(),
            datagram.source_port,
            datagram.destination_port,
            datagram.payload,
            MAX_MTU,
        );
        assert_eq!(rebuilt.unwrap(), packet);
    }
    for row in vectors.get("tcp").unwrap().as_array().unwrap() {
        let packet = binary(row, "packet");
        let parsed = ip::parse_protocol(&config, &packet, crate::netstack::PROTOCOL_TCP).unwrap();
        let segment = tcp::parse(parsed.source, parsed.destination, parsed.payload).unwrap();
        assert_eq!(u64::from(segment.sequence), number(row, "sequence"));
        assert_eq!(u64::from(segment.flags), number(row, "flags"));
        assert_eq!(segment.payload, binary(row, "payload"));
        let header = Header {
            source_port: segment.source_port,
            destination_port: segment.destination_port,
            sequence: segment.sequence,
            acknowledgment: segment.acknowledgment,
            flags: segment.flags,
            window: segment.window,
            mss: segment.mss,
            window_scale: None,
            sack_permitted: false,
            sack: [None; SACK_BLOCKS],
        };
        let rebuilt = tcp::build(remote(), local(), &header, segment.payload, MAX_MTU);
        assert_eq!(rebuilt.unwrap(), packet);
    }
}

#[test]
fn the_largest_payloads_fit_the_session_budget() {
    let at = |payload: usize| udp::build(remote(), local(), 1, 2, &vec![0; payload], MAX_MTU);
    assert_eq!(at(MAX_MTU - 48).unwrap().len(), MAX_MTU);
    assert_eq!(at(MAX_MTU - 47), Err(BuildError::TooLarge));
    let mut header = Header {
        source_port: 1,
        destination_port: 2,
        sequence: 0,
        acknowledgment: 0,
        flags: ACK,
        window: 0,
        mss: None,
        window_scale: None,
        sack_permitted: false,
        sack: [None; SACK_BLOCKS],
    };
    let segment = |header: &Header, payload: usize| {
        tcp::build(local(), remote(), header, &vec![0; payload], MAX_MTU)
    };
    assert_eq!(segment(&header, MAX_MTU - 60).unwrap().len(), MAX_MTU);
    header.flags = SYN;
    assert_eq!(segment(&header, MAX_MTU - 59), Err(BuildError::TooLarge));
}

#[test]
fn every_header_rule_is_enforced() {
    let config = config();
    let parse = |packet: &[u8]| ip::parse_protocol(&config, packet, 17).map(|_| ());
    assert_eq!(parse(&sample()), Ok(()));
    let mut padded = sample();
    padded.extend_from_slice(&[0xaa; 9]);
    let parsed = ip::parse_protocol(&config, &padded, 17).unwrap();
    assert_eq!(parsed.payload.len(), sample().len() - HEADER_LEN);
    let source = |address: &str| {
        edited(|p| p[8..24].copy_from_slice(&address.parse::<Ipv6Addr>().unwrap().octets()))
    };
    for (packet, error) in [
        (sample()[..39].to_vec(), PacketError::Truncated),
        (edited(|p| p[0] = 0x70), PacketError::Version),
        (edited(|p| p[5] += 1), PacketError::TotalLength),
        (edited(|p| p[7] = 0), PacketError::Ttl),
        (edited(|p| p[6] = 44), PacketError::Fragment),
        (edited(|p| p[6] = 0), PacketError::Extension),
        (edited(|p| p[6] = 43), PacketError::Extension),
        (edited(|p| p[6] = 60), PacketError::Extension),
        (edited(|p| p[6] = 6), PacketError::Protocol(6)),
        (edited(|p| p[6] = 59), PacketError::Protocol(59)),
        (edited(|p| p[39] ^= 1), PacketError::Destination),
        (source("ff02::1"), PacketError::Source),
        (source("fe80::1"), PacketError::Source),
        (source("::1"), PacketError::Source),
        (source("::"), PacketError::Source),
        (source("::ffff:203.0.113.9"), PacketError::Source),
        (source(&local().to_string()), PacketError::Source),
    ] {
        assert_eq!(parse(&packet), Err(error));
    }
    let narrow = IpConfig::new(LOCAL4, Ipv4Addr::UNSPECIFIED, 0)
        .unwrap()
        .with_ipv6(local(), "2001:db8::".parse().unwrap(), 32)
        .unwrap();
    assert!(ip::parse_protocol(&narrow, &sample(), 17).is_ok());
    let outside = source("2001:db9::9");
    assert_eq!(
        ip::parse_protocol(&narrow, &outside, 17).map(|_| ()),
        Err(PacketError::Source)
    );
    // Without an IPv6 address the stack refuses IPv6 as before.
    let ipv4_only = IpConfig::new(LOCAL4, Ipv4Addr::UNSPECIFIED, 0).unwrap();
    assert_eq!(
        ip::parse_protocol(&ipv4_only, &sample(), 17).map(|_| ()),
        Err(PacketError::Ipv6)
    );
}

#[test]
fn configuration_and_builder_bounds() {
    let base = IpConfig::new(LOCAL4, Ipv4Addr::UNSPECIFIED, 0).unwrap();
    for local in ["::", "::1", "ff02::1", "fe80::1", "::ffff:10.0.0.1"] {
        let local = local.parse().unwrap();
        assert_eq!(
            base.with_ipv6(local, Ipv6Addr::UNSPECIFIED, 0),
            Err(PacketError::Config)
        );
    }
    assert_eq!(
        base.with_ipv6(local(), Ipv6Addr::UNSPECIFIED, 129),
        Err(PacketError::Config)
    );
    assert_eq!(
        config().source(IpAddr::V4(LOCAL4)),
        Some(IpAddr::V4(LOCAL4))
    );
    assert_eq!(
        config().source(IpAddr::V6(remote())),
        Some(IpAddr::V6(local()))
    );
    assert_eq!(base.source(IpAddr::V6(remote())), None);
    assert_eq!(
        build(remote(), local(), 17, b"", 1279),
        Err(BuildError::Mtu)
    );
    assert_eq!(
        build(remote(), local(), 17, b"", MAX_MTU + 1),
        Err(BuildError::Mtu)
    );
    let mapped = "::ffff:10.0.0.1".parse().unwrap();
    assert_eq!(
        build(remote(), mapped, 17, b"", MAX_MTU),
        Err(BuildError::Destination)
    );
    assert_eq!(
        ip::build(IpAddr::V4(LOCAL4), IpAddr::V6(remote()), 17, b"", MAX_MTU),
        Err(BuildError::Destination)
    );
}

#[test]
fn udp_over_ipv6_requires_a_checksum() {
    let packet = edited(|p| p[HEADER_LEN + 6..HEADER_LEN + 8].fill(0));
    let parsed = ip::parse_protocol(&config(), &packet, 17).unwrap();
    assert_eq!(
        udp::parse(parsed.source, parsed.destination, parsed.payload),
        Err(crate::netstack::DatagramError::Checksum)
    );
}

#[test]
fn echo_requests_get_the_reference_reply_and_nothing_else_does() {
    let vectors = vectors();
    let echo = vectors.get("echo").unwrap();
    let request = binary(echo, "request");
    assert_eq!(
        icmp::echo_reply(&config(), &request, MAX_MTU),
        Some(binary(echo, "reply"))
    );
    let mut corrupt = request.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    assert_eq!(icmp::echo_reply(&config(), &corrupt, MAX_MTU), None);
    let reply = binary(echo, "reply");
    let mut swapped = reply.clone();
    swapped[8..24].copy_from_slice(&reply[24..40]);
    swapped[24..40].copy_from_slice(&reply[8..24]);
    assert_eq!(icmp::echo_reply(&config(), &swapped, MAX_MTU), None);
    let ipv4_only = IpConfig::new(LOCAL4, Ipv4Addr::UNSPECIFIED, 0).unwrap();
    assert_eq!(icmp::echo_reply(&ipv4_only, &request, MAX_MTU), None);
}

#[test]
fn packet_too_big_and_unreachable_reach_only_the_connected_socket() {
    let config = config();
    let mut sockets = UdpSockets::new(Limits::default(), G);
    let mut entropy = Script::new(&[7]);
    let socket = sockets.bind(G, 0, &mut entropy).unwrap();
    let peer = (IpAddr::V6(remote()), 6881);
    sockets.connect(G, socket, peer).unwrap();
    let send = |sockets: &mut UdpSockets, size: usize| {
        let local = IpAddr::V6(local());
        sockets.send(G, socket, local, peer, &vec![1; size], MAX_MTU)
    };
    let sent = send(&mut sockets, 1300).unwrap();
    // A forged quote of another datagram, then advice below the IPv6 floor.
    let mut forged = sent.clone();
    forged[HEADER_LEN + 6] ^= 1;
    let ingest = |sockets: &mut UdpSockets, packet: &[u8]| sockets.ingest_icmp(G, &config, packet);
    assert_eq!(
        ingest(&mut sockets, &advice(2, 0, 1280, &forged)),
        IcmpIngest::Dropped
    );
    assert_eq!(
        ingest(&mut sockets, &advice(2, 0, 600, &sent)),
        IcmpIngest::Applied(IcmpEffect::PathMtu(MIN_MTU))
    );
    assert_eq!(
        send(&mut sockets, 1300),
        Err(SocketError::Build(BuildError::TooLarge))
    );
    let sent = send(&mut sockets, 100).unwrap();
    // Port unreachable (ICMPv6 code 4) is reported as the ICMPv4 code 3.
    assert_eq!(
        ingest(&mut sockets, &advice(1, 4, 0, &sent)),
        IcmpIngest::Applied(IcmpEffect::Unreachable(3))
    );
    assert_eq!(sockets.recv(G, socket), Err(SocketError::Unreachable(3)));
    let sent = send(&mut sockets, 100).unwrap();
    assert_eq!(
        ingest(&mut sockets, &advice(1, 0, 1, &sent)),
        IcmpIngest::Invalid(icmp::Error::Unsupported)
    );
    assert_eq!(
        ingest(&mut sockets, &advice(1, 7, 0, &sent)),
        IcmpIngest::Invalid(icmp::Error::Unsupported)
    );
    assert_eq!(
        ingest(&mut sockets, &advice(2, 0, 1280, &sent[..47])),
        IcmpIngest::Invalid(icmp::Error::Quote)
    );
}

#[test]
fn tcp_connects_over_ipv6_with_the_ipv6_mss() {
    let mut stack = TcpStack::new(
        TcpConfig::new(config(), MAX_MTU),
        Script::new(&[0]),
        G,
        Duration::ZERO,
    )
    .unwrap();
    let flow = stack.connect(G, Duration::ZERO, remote(), 443).unwrap();
    assert_eq!(flow.remote(), (IpAddr::V6(remote()), 443));
    let syn = stack.poll(G, Duration::ZERO).unwrap().send.remove(0);
    assert_eq!(syn[0] >> 4, 6);
    let segment = tcp::parse(local(), remote(), &syn[HEADER_LEN..]).unwrap();
    assert_eq!(segment.mss, Some((MAX_MTU - 60) as u16));
    let header = Header {
        source_port: 443,
        destination_port: flow.local_port(),
        sequence: 9000,
        acknowledgment: segment.sequence.wrapping_add(1),
        flags: SYN | ACK,
        window: 65535,
        mss: Some(1440),
        window_scale: None,
        sack_permitted: false,
        sack: [None; SACK_BLOCKS],
    };
    let answer = tcp::build(remote(), local(), &header, &[], MAX_MTU).unwrap();
    stack.ingest(G, Duration::from_millis(5), &answer);
    assert_eq!(stack.state(G, flow), Ok(TcpState::Established));
    // An IPv4-only stack refuses IPv6 remotes before any packet.
    let ipv4_only = IpConfig::new(LOCAL4, Ipv4Addr::UNSPECIFIED, 0).unwrap();
    let mut stack = TcpStack::new(
        TcpConfig::new(ipv4_only, MAX_MTU),
        Script::new(&[0]),
        G,
        Duration::ZERO,
    )
    .unwrap();
    assert!(stack.connect(G, Duration::ZERO, remote(), 443).is_err());
    // IPv6 needs the 1280-byte minimum MTU.
    assert!(
        TcpStack::new(
            TcpConfig::new(config(), 1279),
            Script::new(&[0]),
            G,
            Duration::ZERO
        )
        .is_err()
    );
}

#[test]
fn aaaa_lookups_through_an_ipv6_resolver() {
    let vectors = vectors();
    let dns = vectors.get("dns").unwrap();
    let id = number(dns, "id") as u16;
    let mut wire = Vec::new();
    for label in text(dns, "name").split('.') {
        wire.push(label.len() as u8);
        wire.extend_from_slice(label.as_bytes());
    }
    wire.push(0);
    let response = binary(dns, "response");
    let addresses: Vec<IpAddr> = ["2001:db8::10", "2001:db8::11"]
        .iter()
        .map(|address| address.parse().unwrap())
        .collect();
    let expected = Reply::Addresses {
        addresses: addresses.clone(),
        ttl: number(dns, "ttl") as u32,
    };
    assert_eq!(
        parse_response_for(Record::Aaaa, id, &wire, &response),
        Ok(expected)
    );
    // The question type must match, and an A record never answers AAAA.
    assert_eq!(
        parse_response_for(Record::A, id, &wire, &response),
        Err(ReplyError::Question)
    );
    assert_eq!(
        parse_response_for(Record::Aaaa, id, &wire, &binary(dns, "a_in_aaaa")),
        Err(ReplyError::Type)
    );

    // End to end: the query leaves over IPv6 UDP to the IPv6 resolver.
    let resolver = remote();
    let ip = config();
    let dns_config = DnsConfig::new(ip, vec![IpAddr::V6(resolver)], MAX_MTU);
    let mut sockets = UdpSockets::new(Limits::default(), G);
    let mut tcp = TcpStack::new(
        TcpConfig::new(ip, MAX_MTU),
        Script::new(&[0]),
        G,
        Duration::ZERO,
    )
    .unwrap();
    let txid = id;
    let mut client =
        DnsClient::new(dns_config, Script::new(&[txid, 1]), G, Duration::ZERO).unwrap();
    let (query, packet) = client
        .start_record(
            &mut sockets,
            G,
            text(dns, "name"),
            Record::Aaaa,
            Duration::ZERO,
        )
        .unwrap();
    assert_eq!(&packet[24..40], &resolver.octets());
    let source_port = u16::from_be_bytes([packet[HEADER_LEN], packet[HEADER_LEN + 1]]);
    assert_eq!(&packet[HEADER_LEN + 8..], binary(dns, "query"));
    let reply = udp::build(resolver, local(), 53, source_port, &response, MAX_MTU).unwrap();
    assert_eq!(
        sockets.ingest(G, &ip, &reply),
        Ingest::Delivered(Delivery::Queued)
    );
    let poll = client
        .poll(&mut sockets, &mut tcp, G, Duration::ZERO)
        .unwrap();
    assert_eq!(poll.finished.len(), 1);
    assert_eq!(poll.finished[0].id, query);
    assert_eq!(
        poll.finished[0].result.as_ref().unwrap().addresses,
        addresses
    );
    // IPv6 resolvers need an IPv6 address.
    let ipv4_only = IpConfig::new(LOCAL4, Ipv4Addr::UNSPECIFIED, 0).unwrap();
    let refused = DnsConfig::new(ipv4_only, vec![IpAddr::V6(resolver)], MAX_MTU);
    assert!(DnsClient::new(refused, Script::new(&[1]), G, Duration::ZERO).is_err());
}
