use super::*;
use crate::netstack::ipv4::{Ipv4Config, MAX_MTU, MIN_MTU, PacketError, parse_protocol};
use crate::netstack::support::{address, binary, fixtures, number};

const LOCAL: Ipv4Addr = Ipv4Addr::new(10, 64, 0, 2);
const REMOTE: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 9);

fn config(local: Ipv4Addr) -> Ipv4Config {
    Ipv4Config::new(local, Ipv4Addr::UNSPECIFIED, 0).unwrap()
}

fn header(flags: u8) -> Header {
    Header {
        source_port: 443,
        destination_port: 49152,
        sequence: 1000,
        acknowledgment: 2000,
        flags,
        window: 4096,
        mss: None,
    }
}

/// A bare TCP segment from the builder, with the IPv4 header removed.
fn segment(header: &Header, payload: &[u8]) -> Vec<u8> {
    let packet = build(REMOTE, LOCAL, header, payload, MAX_MTU).unwrap();
    packet[IPV4_HEADER_LEN..].to_vec()
}

/// Rewrites a field and repairs the checksum, so only that field is judged.
fn tamper(bytes: &mut Vec<u8>, edit: impl FnOnce(&mut Vec<u8>)) {
    edit(bytes);
    bytes[16..18].copy_from_slice(&[0, 0]);
    let value = !segment_sum(REMOTE, LOCAL, bytes);
    bytes[16..18].copy_from_slice(&value.to_be_bytes());
}

#[test]
fn independent_reference_packets() {
    let vectors = fixtures();
    let rows = vectors.get("tcp").unwrap().as_array().unwrap();
    assert_eq!(rows.len(), 14);
    for row in rows {
        let packet = binary(row, "packet");
        let destination = address(row, "destination");
        let ip = parse_protocol(&config(destination), &packet, PROTOCOL_TCP).unwrap();
        let parsed = parse(ip.source, ip.destination, ip.payload).unwrap();
        assert_eq!(ip.source, address(row, "source"));
        assert_eq!(u64::from(parsed.source_port), number(row, "source_port"));
        assert_eq!(
            u64::from(parsed.destination_port),
            number(row, "destination_port")
        );
        assert_eq!(u64::from(parsed.sequence), number(row, "sequence"));
        assert_eq!(
            u64::from(parsed.acknowledgment),
            number(row, "acknowledgment")
        );
        assert_eq!(u64::from(parsed.flags), number(row, "flags"));
        assert_eq!(u64::from(parsed.window), number(row, "window"));
        assert_eq!(parsed.mss.map(u64::from), row.get("mss").unwrap().as_u64());
        assert_eq!(parsed.payload, binary(row, "payload"));
        if row.get("canonical").unwrap().as_bool().unwrap() {
            let rebuilt = build(
                ip.source,
                destination,
                &Header {
                    source_port: parsed.source_port,
                    destination_port: parsed.destination_port,
                    sequence: parsed.sequence,
                    acknowledgment: parsed.acknowledgment,
                    flags: parsed.flags,
                    window: parsed.window,
                    mss: parsed.mss,
                },
                parsed.payload,
                MAX_MTU,
            );
            assert_eq!(rebuilt.unwrap(), packet);
        }
    }
}

#[test]
fn a_computed_zero_checksum_is_sent_as_zero() {
    let vectors = fixtures();
    let rows = vectors.get("tcp").unwrap().as_array().unwrap();
    let packet = binary(rows.last().unwrap(), "packet");
    assert_eq!(&packet[IPV4_HEADER_LEN + 16..IPV4_HEADER_LEN + 18], &[0, 0]);
}

#[test]
fn the_ip_layer_names_the_protocol() {
    let packet = build(REMOTE, LOCAL, &header(ACK), b"x", MAX_MTU).unwrap();
    let ip = parse_protocol(&config(LOCAL), &packet, PROTOCOL_TCP).unwrap();
    assert_eq!(ip.protocol, PROTOCOL_TCP);
    // The UDP entry point still refuses TCP, and the TCP one refuses UDP.
    assert_eq!(
        ipv4::parse(&config(LOCAL), &packet),
        Err(PacketError::Protocol(PROTOCOL_TCP))
    );
    let udp = crate::netstack::build_udp(REMOTE, LOCAL, 53, 40000, b"x", MAX_MTU).unwrap();
    assert_eq!(
        parse_protocol(&config(LOCAL), &udp, PROTOCOL_TCP),
        Err(PacketError::Protocol(17))
    );
}

#[test]
fn round_trip_and_syn_mss() {
    let mut syn = header(SYN);
    syn.mss = Some(1384);
    let bytes = segment(&syn, &[]);
    assert_eq!(bytes.len(), 24);
    let parsed = parse(REMOTE, LOCAL, &bytes).unwrap();
    assert_eq!(parsed.mss, Some(1384));
    assert_eq!(parsed.space(), 1);
    assert_eq!(parsed.payload, b"");
    let data = segment(&header(PSH | ACK), b"hello");
    let parsed = parse(REMOTE, LOCAL, &data).unwrap();
    assert_eq!(parsed.payload, b"hello");
    assert_eq!(parsed.space(), 5);
    assert_eq!(segment(&header(FIN | ACK), &[]).len(), 20);
    let bytes = segment(&header(FIN | ACK), b"ab");
    let fin = parse(REMOTE, LOCAL, &bytes).unwrap();
    assert_eq!(fin.space(), 3);
}

#[test]
fn every_header_field_is_checked() {
    let good = segment(&header(ACK), b"data");
    assert!(parse(REMOTE, LOCAL, &good).is_ok());
    for length in 0..HEADER_LEN {
        assert_eq!(
            parse(REMOTE, LOCAL, &good[..length]),
            Err(SegmentError::Truncated)
        );
    }
    // Data offset below five words, and beyond the bytes present.
    for offset in [0u8, 1, 4, 15] {
        let mut bytes = good.clone();
        tamper(&mut bytes, |bytes| bytes[12] = offset << 4);
        assert_eq!(
            parse(REMOTE, LOCAL, &bytes),
            Err(SegmentError::Offset),
            "offset {offset}"
        );
    }
    for range in [0..2, 2..4] {
        let mut bytes = good.clone();
        tamper(&mut bytes, |bytes| bytes[range].copy_from_slice(&[0, 0]));
        assert_eq!(parse(REMOTE, LOCAL, &bytes), Err(SegmentError::Port));
    }
    for (index, value, expected) in [
        (12, 0x51, SegmentError::Flags),
        (12, 0x58, SegmentError::Flags),
        (13, ACK | 0x20, SegmentError::Flags),
        (13, ACK | 0x40, SegmentError::Flags),
        (13, ACK | 0x80, SegmentError::Flags),
        (13, SYN | FIN, SegmentError::Flags),
        (13, SYN | RST, SegmentError::Flags),
        (13, FIN | RST, SegmentError::Flags),
        (18, 1, SegmentError::Urgent),
        (19, 1, SegmentError::Urgent),
    ] {
        let mut bytes = good.clone();
        tamper(&mut bytes, |bytes| bytes[index] = value);
        assert_eq!(
            parse(REMOTE, LOCAL, &bytes),
            Err(expected),
            "byte {index} = {value:#x}"
        );
    }
}

#[test]
fn checksum_covers_every_byte_and_the_addresses() {
    let good = segment(&header(ACK), b"data!");
    for index in 0..good.len() {
        for bit in 0..8 {
            let mut bytes = good.clone();
            bytes[index] ^= 1 << bit;
            assert!(
                parse(REMOTE, LOCAL, &bytes).is_err(),
                "byte {index} bit {bit}"
            );
        }
    }
    assert_eq!(
        parse(Ipv4Addr::new(203, 0, 113, 10), LOCAL, &good),
        Err(SegmentError::Checksum)
    );
    assert_eq!(
        parse(REMOTE, Ipv4Addr::new(10, 64, 0, 3), &good),
        Err(SegmentError::Checksum)
    );
}

#[test]
fn options_are_length_checked() {
    let mss = |option: &[u8], syn: bool| {
        let mut bytes = segment(&header(if syn { SYN } else { ACK }), &[]);
        tamper(&mut bytes, |bytes| {
            let words = (HEADER_LEN + option.len()).div_ceil(4);
            bytes.resize(words * 4, 0);
            bytes[HEADER_LEN..HEADER_LEN + option.len()].copy_from_slice(option);
            bytes[12] = (words as u8) << 4;
        });
        parse(REMOTE, LOCAL, &bytes).map(|parsed| parsed.mss)
    };
    assert_eq!(mss(&[2, 4, 5, 0x78], true), Ok(Some(1400)));
    assert_eq!(mss(&[1, 1, 2, 4, 5, 0x78], true), Ok(Some(1400)));
    // MSS only counts on a SYN.
    assert_eq!(mss(&[2, 4, 5, 0x78], false), Ok(None));
    assert_eq!(mss(&[1, 1, 1, 2], true), Err(SegmentError::Options));
    assert_eq!(mss(&[1, 2, 4, 5], true), Err(SegmentError::Options));
    assert_eq!(mss(&[2, 3, 5, 0], true), Err(SegmentError::Options));
    assert_eq!(mss(&[3, 1, 0, 0], true), Err(SegmentError::Options));
    assert_eq!(mss(&[3, 0, 0, 0], true), Err(SegmentError::Options));
    assert_eq!(mss(&[9, 9, 0, 0], true), Err(SegmentError::Options));
    assert_eq!(
        mss(&[2, 4, 5, 0x78, 2, 4, 5, 0x78], true),
        Err(SegmentError::Options)
    );
    assert_eq!(mss(&[2, 4, 0, 63], true), Err(SegmentError::Mss));
    assert_eq!(mss(&[2, 4, 0, 64], true), Ok(Some(64)));
    // Unknown well-formed options are skipped; End-of-Options stops the scan.
    assert_eq!(mss(&[77, 2, 1, 0], true), Ok(None));
    assert_eq!(mss(&[0, 9, 9, 9], true), Ok(None));
    // The full 40 bytes of options parse.
    let mut forty = vec![1u8; 36];
    forty.extend_from_slice(&[2, 4, 5, 0x78]);
    assert_eq!(mss(&forty, true), Ok(Some(1400)));
}

#[test]
fn builder_limits() {
    let mut zero_port = header(ACK);
    zero_port.source_port = 0;
    assert_eq!(
        build(REMOTE, LOCAL, &zero_port, &[], MAX_MTU),
        Err(BuildError::Port)
    );
    zero_port.source_port = 1;
    zero_port.destination_port = 0;
    assert_eq!(
        build(REMOTE, LOCAL, &zero_port, &[], MAX_MTU),
        Err(BuildError::Port)
    );
    for mtu in [0, MIN_MTU - 1, MAX_MTU + 1] {
        assert_eq!(
            build(REMOTE, LOCAL, &header(ACK), &[], mtu),
            Err(BuildError::Mtu)
        );
        assert_eq!(max_payload(mtu), Err(BuildError::Mtu));
    }
    assert_eq!(max_payload(MAX_MTU), Ok(MAX_MTU - 40));
    let full = vec![7u8; MAX_MTU - 40];
    let packet = build(REMOTE, LOCAL, &header(ACK), &full, MAX_MTU).unwrap();
    assert_eq!(packet.len(), MAX_MTU);
    let mut larger = full.clone();
    larger.push(0);
    assert_eq!(
        build(REMOTE, LOCAL, &header(ACK), &larger, MAX_MTU),
        Err(BuildError::TooLarge)
    );
    // The SYN option takes four bytes of the budget.
    let mut syn = header(SYN);
    syn.mss = Some(1384);
    assert_eq!(
        build(REMOTE, LOCAL, &syn, &full, MAX_MTU),
        Err(BuildError::TooLarge)
    );
    assert!(build(REMOTE, LOCAL, &syn, &full[..full.len() - 4], MAX_MTU).is_ok());
    assert_eq!(
        build(Ipv4Addr::LOCALHOST, LOCAL, &header(ACK), &[], MAX_MTU),
        Err(BuildError::Source)
    );
    assert_eq!(
        build(REMOTE, Ipv4Addr::BROADCAST, &header(ACK), &[], MAX_MTU),
        Err(BuildError::Destination)
    );
}
