use super::*;
use crate::netstack::support::{address, binary, bytes, fixtures, number, text};

const LOCAL: Ipv4Addr = Ipv4Addr::new(10, 64, 0, 2);
const REMOTE: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 9);

fn config() -> Ipv4Config {
    Ipv4Config::new(LOCAL, Ipv4Addr::UNSPECIFIED, 0).unwrap()
}

fn sample(payload: &[u8]) -> Vec<u8> {
    build(REMOTE, LOCAL, PROTOCOL_UDP, payload, MAX_MTU).unwrap()
}

/// Rewrites the header checksum so a field edit is the only defect.
fn reseal(packet: &mut [u8]) {
    let header = (usize::from(packet[0] & 0x0f) * 4).min(packet.len());
    packet[10..12].fill(0);
    let value = checksum(&packet[..header]).to_be_bytes();
    packet[10..12].copy_from_slice(&value);
}

fn edited(edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut packet = sample(b"synthetic payload");
    edit(&mut packet);
    reseal(&mut packet);
    packet
}

#[test]
fn budget_matches_the_session_payload() {
    assert_eq!(MAX_MTU, crate::wireguard::MAX_PAYLOAD);
}

#[test]
fn builds_and_parses_a_plain_packet() {
    let packet = sample(b"hello");
    assert_eq!(packet.len(), 25);
    assert_eq!(&packet[..2], &[0x45, 0]);
    assert_eq!(&packet[6..8], &[0x40, 0]);
    let parsed = parse(&config(), &packet).unwrap();
    assert_eq!(parsed.source, REMOTE);
    assert_eq!(parsed.destination, LOCAL);
    assert_eq!(parsed.protocol, PROTOCOL_UDP);
    assert_eq!(parsed.ttl, DEFAULT_TTL);
    assert_eq!(parsed.payload, b"hello");
}

#[test]
fn transport_padding_is_discarded() {
    let mut packet = sample(b"hello");
    packet.extend_from_slice(&[0xaa; 11]);
    assert_eq!(parse(&config(), &packet).unwrap().payload, b"hello");
    let mut empty = sample(b"");
    empty.extend_from_slice(&[0; 16]);
    assert!(parse(&config(), &empty).unwrap().payload.is_empty());
}

#[test]
fn independent_checksum_and_packet_vectors() {
    let vectors = fixtures();
    let inner = vectors.get("inner").unwrap();
    let rows = inner.get("checksum").unwrap().as_array().unwrap();
    assert_eq!(rows.len(), 13);
    for row in rows {
        let expected = u16::from_str_radix(text(row, "checksum"), 16).unwrap();
        assert_eq!(checksum(&binary(row, "data")), expected);
    }
    let rows = inner.get("ipv4").unwrap().as_array().unwrap();
    assert_eq!(rows.len(), 6);
    for row in rows {
        let packet = binary(row, "packet");
        let config = Ipv4Config::new(address(row, "destination"), Ipv4Addr::UNSPECIFIED, 0);
        let config = config.unwrap();
        let parsed = parse(&config, &packet).unwrap();
        assert_eq!(parsed.source, address(row, "source"));
        assert_eq!(parsed.protocol, number(row, "protocol") as u8);
        assert_eq!(parsed.ttl, number(row, "ttl") as u8);
        assert_eq!(parsed.payload, binary(row, "payload"));
        let padded = binary(row, "padded");
        assert_eq!(parse(&config, &padded).unwrap(), parsed);
        if row.get("canonical").unwrap().as_bool().unwrap() {
            let rebuilt = build(parsed.source, LOCAL, PROTOCOL_UDP, parsed.payload, MAX_MTU);
            assert_eq!(rebuilt.unwrap(), packet);
        }
    }
}

#[test]
fn rejects_each_header_field() {
    let config = config();
    let cases: Vec<(Vec<u8>, PacketError)> = vec![
        (vec![], PacketError::Truncated),
        (vec![0x45; 19], PacketError::Truncated),
        (edited(|p| p[0] = 0x55), PacketError::Version),
        (edited(|p| p[0] = 0x65), PacketError::Ipv6),
        (edited(|p| p[0] = 0x44), PacketError::HeaderLength),
        (edited(|p| p[0] = 0x4f), PacketError::HeaderLength),
        (edited(|p| p[0] = 0x49), PacketError::HeaderLength),
        (edited(|p| p[3] = 0), PacketError::TotalLength),
        (edited(|p| p[3] = 0xff), PacketError::TotalLength),
        (edited(|p| p[8] = 0), PacketError::Ttl),
        (edited(|p| p[6] = 0x60), PacketError::Fragment),
        (edited(|p| p[7] = 1), PacketError::Fragment),
        (edited(|p| p[9] = 6), PacketError::Protocol(6)),
        (edited(|p| p[9] = 1), PacketError::Protocol(1)),
        (edited(|p| p[19] = 3), PacketError::Destination),
        (edited(|p| p[12] = 127), PacketError::Source),
        (edited(|p| p[12] = 224), PacketError::Source),
        (edited(|p| p[12..16].fill(255)), PacketError::Source),
        (edited(|p| p[12..16].fill(0)), PacketError::Source),
        (
            edited(|p| p[12..16].copy_from_slice(&[10, 64, 0, 2])),
            PacketError::Source,
        ),
    ];
    for (index, (packet, expected)) in cases.iter().enumerate() {
        assert_eq!(
            parse(&config, packet).unwrap_err(),
            *expected,
            "case {index}"
        );
    }
    let mut broken = sample(b"x");
    broken[10] ^= 1;
    assert_eq!(parse(&config, &broken).unwrap_err(), PacketError::Checksum);
    // Reserved flag and DSCP/ECN bits are ignored; DF is accepted.
    assert!(parse(&config, &edited(|p| p[1] = 0xff)).is_ok());
}

#[test]
fn header_length_needs_its_own_bytes() {
    let mut short = sample(b"abc");
    short[0] = 0x46;
    assert_eq!(
        parse(&config(), &short[..23]).unwrap_err(),
        PacketError::Truncated
    );
}

#[test]
fn total_length_is_bounded_by_budget_and_input() {
    let payload = vec![0u8; MAX_MTU - HEADER_LEN];
    let packet = sample(&payload);
    assert_eq!(packet.len(), MAX_MTU);
    assert!(parse(&config(), &packet).is_ok());
    let mut larger = packet;
    larger.push(0);
    larger[2..4].copy_from_slice(&((MAX_MTU + 1) as u16).to_be_bytes());
    reseal(&mut larger);
    assert_eq!(
        parse(&config(), &larger).unwrap_err(),
        PacketError::TotalLength
    );
}

#[test]
fn only_padding_options_are_accepted() {
    let with_options = |options: &[u8]| {
        let mut packet = vec![
            0x40 | (5 + (options.len() / 4) as u8),
            0,
            0,
            0,
            0,
            0,
            0x40,
            0,
        ];
        packet.extend_from_slice(&[64, PROTOCOL_UDP, 0, 0, 203, 0, 113, 9, 10, 64, 0, 2]);
        packet.extend_from_slice(options);
        packet.extend_from_slice(b"data");
        let total = (packet.len() as u16).to_be_bytes();
        packet[2..4].copy_from_slice(&total);
        reseal(&mut packet);
        packet
    };
    let parsed = with_options(&[1, 1, 1, 0]);
    assert_eq!(parse(&config(), &parsed).unwrap().payload, b"data");
    assert!(parse(&config(), &with_options(&[1; 12])).is_ok());
    for option in [2u8, 7, 0x83, 0x94, 0xff] {
        let packet = with_options(&[1, 1, 1, option]);
        assert_eq!(parse(&config(), &packet).unwrap_err(), PacketError::Options);
    }
    assert_eq!(
        parse(&config(), &with_options(&[1; 16])).unwrap_err(),
        PacketError::HeaderLength
    );
}

#[test]
fn source_must_be_in_the_allowed_network() {
    let allowed = |network: [u8; 4], prefix: u8| {
        Ipv4Config::new(LOCAL, Ipv4Addr::from(network), prefix).unwrap()
    };
    let packet = sample(b"x");
    assert!(parse(&allowed([203, 0, 113, 0], 24), &packet).is_ok());
    assert!(parse(&allowed([203, 0, 113, 9], 32), &packet).is_ok());
    assert!(parse(&allowed([203, 0, 113, 8], 29), &packet).is_ok());
    assert_eq!(
        parse(&allowed([203, 0, 113, 10], 32), &packet).unwrap_err(),
        PacketError::Source
    );
    assert_eq!(
        parse(&allowed([198, 51, 100, 0], 24), &packet).unwrap_err(),
        PacketError::Source
    );
    assert_eq!(
        parse(&allowed([203, 0, 112, 0], 24), &packet).unwrap_err(),
        PacketError::Source
    );
}

#[test]
fn configuration_is_validated() {
    let any = Ipv4Addr::UNSPECIFIED;
    assert_eq!(Ipv4Config::new(LOCAL, any, 33), Err(PacketError::Config));
    for local in [
        any,
        Ipv4Addr::LOCALHOST,
        Ipv4Addr::BROADCAST,
        Ipv4Addr::new(224, 0, 0, 1),
    ] {
        assert_eq!(Ipv4Config::new(local, any, 0), Err(PacketError::Config));
    }
    assert_eq!(config().local(), LOCAL);
}

#[test]
fn every_header_bit_flip_is_detected() {
    let packet = sample(b"synthetic payload");
    for byte in 0..HEADER_LEN {
        for bit in 0..8 {
            let mut tampered = packet.clone();
            tampered[byte] ^= 1 << bit;
            assert!(
                parse(&config(), &tampered).is_err(),
                "byte {byte} bit {bit}"
            );
        }
    }
}

#[test]
fn builder_is_bounded_by_the_mtu() {
    let exact = vec![0u8; 576 - HEADER_LEN];
    assert_eq!(
        build(REMOTE, LOCAL, PROTOCOL_UDP, &exact, 576)
            .unwrap()
            .len(),
        576
    );
    assert_eq!(
        build(REMOTE, LOCAL, PROTOCOL_UDP, &[0; 557], 576),
        Err(BuildError::TooLarge)
    );
    for mtu in [0, 575, MAX_MTU + 1, usize::MAX] {
        assert_eq!(
            build(REMOTE, LOCAL, PROTOCOL_UDP, b"", mtu),
            Err(BuildError::Mtu)
        );
    }
    assert_eq!(
        build(Ipv4Addr::UNSPECIFIED, LOCAL, PROTOCOL_UDP, b"", MAX_MTU),
        Err(BuildError::Source)
    );
    for destination in [
        Ipv4Addr::BROADCAST,
        Ipv4Addr::LOCALHOST,
        Ipv4Addr::new(239, 1, 1, 1),
    ] {
        assert_eq!(
            build(REMOTE, destination, PROTOCOL_UDP, b"", MAX_MTU),
            Err(BuildError::Destination)
        );
    }
}

#[test]
fn checksum_handles_odd_lengths_and_carries() {
    assert_eq!(checksum(&bytes("0001f203f4f5f6f7")), 0x220d);
    assert_eq!(checksum(&[]), 0xffff);
    assert_eq!(checksum(&[0xff; 8]), 0);
    assert_eq!(checksum(&[0xab]), !0xab00);
    assert_eq!(fold(u64::MAX), 0xffff);
}

#[test]
fn every_fragment_offset_and_reserved_flag_is_refused_without_retained_state() {
    let config = config();
    for bits in 0u16..=u16::MAX {
        let mut packet = sample(b"synthetic");
        packet[6..8].copy_from_slice(&bits.to_be_bytes());
        reseal(&mut packet);
        if bits == 0 || bits == 0x4000 {
            assert!(parse(&config, &packet).is_ok());
        } else {
            assert_eq!(parse(&config, &packet), Err(PacketError::Fragment));
        }
    }
    let mut ipv6 = sample(b"synthetic");
    ipv6[0] = 0x60;
    assert_eq!(parse(&config, &ipv6), Err(PacketError::Ipv6));
}
