use super::*;
use crate::netstack::ipv4::{self, Ipv4Config, MAX_MTU, PacketError};
use crate::netstack::support::{address, binary, fixtures, number};

const LOCAL: Ipv4Addr = Ipv4Addr::new(10, 64, 0, 2);
const REMOTE: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 9);

fn config(local: Ipv4Addr) -> Ipv4Config {
    Ipv4Config::new(local, Ipv4Addr::UNSPECIFIED, 0).unwrap()
}

/// A bare UDP segment from the builder, with the IPv4 header removed.
fn segment(payload: &[u8]) -> Vec<u8> {
    let packet = build(REMOTE, LOCAL, 53, 40000, payload, MAX_MTU).unwrap();
    packet[IPV4_HEADER_LEN..].to_vec()
}

#[test]
fn independent_reference_packets() {
    let vectors = fixtures();
    let rows = vectors.get("inner").unwrap().get("udp").unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 7);
    for row in rows {
        let packet = binary(row, "packet");
        let destination = address(row, "destination");
        let ip = ipv4::parse(&config(destination), &packet).unwrap();
        let datagram = parse(ip.source, ip.destination, ip.payload).unwrap();
        assert_eq!(ip.source, address(row, "source"));
        assert_eq!(u64::from(datagram.source_port), number(row, "source_port"));
        assert_eq!(
            u64::from(datagram.destination_port),
            number(row, "destination_port")
        );
        assert_eq!(datagram.payload, binary(row, "payload"));
        if row.get("canonical").unwrap().as_bool().unwrap() {
            let rebuilt = build(
                ip.source,
                destination,
                datagram.source_port,
                datagram.destination_port,
                datagram.payload,
                MAX_MTU,
            );
            assert_eq!(rebuilt.unwrap(), packet);
        }
    }
}

#[test]
fn a_computed_zero_checksum_is_sent_as_ones() {
    let vectors = fixtures();
    let rows = vectors.get("inner").unwrap().get("udp").unwrap();
    let row = &rows.as_array().unwrap()[5];
    let packet = binary(row, "packet");
    assert_eq!(&packet[IPV4_HEADER_LEN + 6..IPV4_HEADER_LEN + 8], &[0xff, 0xff]);
}

#[test]
fn zero_checksum_means_unchecked_but_wrong_ones_fail() {
    let mut data = segment(b"payload");
    data[6..8].fill(0);
    assert!(parse(REMOTE, LOCAL, &data).is_ok());
    let mut data = segment(b"payload");
    data[6] ^= 0x10;
    assert_eq!(parse(REMOTE, LOCAL, &data), Err(DatagramError::Checksum));
}

#[test]
fn checksum_covers_the_pseudo_header() {
    let data = segment(b"payload");
    assert!(parse(REMOTE, LOCAL, &data).is_ok());
    assert_eq!(
        parse(Ipv4Addr::new(203, 0, 113, 10), LOCAL, &data),
        Err(DatagramError::Checksum)
    );
    assert_eq!(
        parse(REMOTE, Ipv4Addr::new(10, 64, 0, 3), &data),
        Err(DatagramError::Checksum)
    );
}

#[test]
fn length_and_ports_are_strict() {
    let data = segment(b"payload");
    assert_eq!(parse(REMOTE, LOCAL, &data[..7]), Err(DatagramError::Truncated));
    assert_eq!(parse(REMOTE, LOCAL, &data[..10]), Err(DatagramError::Length));
    let mut longer = data.clone();
    longer.push(0);
    assert_eq!(parse(REMOTE, LOCAL, &longer), Err(DatagramError::Length));
    for (field, index) in [("source", 0), ("destination", 2)] {
        let mut zero = data.clone();
        zero[index..index + 2].fill(0);
        zero[6..8].fill(0);
        assert_eq!(
            parse(REMOTE, LOCAL, &zero),
            Err(DatagramError::Port),
            "{field}"
        );
    }
    assert_eq!(parse(REMOTE, LOCAL, &[]), Err(DatagramError::Truncated));
}

#[test]
fn every_bit_flip_is_detected() {
    let data = segment(b"synthetic payload");
    for byte in 0..data.len() {
        for bit in 0..8 {
            let mut tampered = data.clone();
            tampered[byte] ^= 1 << bit;
            // Clearing the only set bit of the checksum field disables it.
            let disabled = tampered[6..8] == [0, 0];
            assert!(
                parse(REMOTE, LOCAL, &tampered).is_err() || (disabled && (6..8).contains(&byte)),
                "byte {byte} bit {bit}"
            );
        }
    }
}

#[test]
fn builder_honors_the_mtu_exactly() {
    assert_eq!(max_payload(MAX_MTU), Ok(1396));
    assert_eq!(max_payload(576), Ok(548));
    let full = build(REMOTE, LOCAL, 1, 2, &[7; 1396], MAX_MTU).unwrap();
    assert_eq!(full.len(), MAX_MTU);
    assert_eq!(
        build(REMOTE, LOCAL, 1, 2, &[7; 1397], MAX_MTU),
        Err(BuildError::TooLarge)
    );
    assert_eq!(
        build(REMOTE, LOCAL, 1, 2, &[7; 549], 576),
        Err(BuildError::TooLarge)
    );
    assert_eq!(build(REMOTE, LOCAL, 0, 2, b"", MAX_MTU), Err(BuildError::Port));
    assert_eq!(build(REMOTE, LOCAL, 1, 0, b"", MAX_MTU), Err(BuildError::Port));
    for mtu in [0, 575, MAX_MTU + 1] {
        assert_eq!(build(REMOTE, LOCAL, 1, 2, b"", mtu), Err(BuildError::Mtu));
        assert_eq!(max_payload(mtu), Err(BuildError::Mtu));
    }
    assert_eq!(
        build(REMOTE, Ipv4Addr::BROADCAST, 1, 2, b"", MAX_MTU),
        Err(BuildError::Destination)
    );
}

#[test]
fn full_packets_round_trip_through_both_layers() {
    for length in [0, 1, 2, 3, 511, 512, 1395, 1396] {
        let payload: Vec<u8> = (0..length).map(|index| (index * 7 % 251) as u8).collect();
        let packet = build(REMOTE, LOCAL, 6881, 49152, &payload, MAX_MTU).unwrap();
        let ip = ipv4::parse(&config(LOCAL), &packet).unwrap();
        let datagram = parse(ip.source, ip.destination, ip.payload).unwrap();
        assert_eq!(datagram.payload, payload);
    }
    let wrong = build(REMOTE, Ipv4Addr::new(10, 64, 0, 9), 1, 2, b"x", MAX_MTU).unwrap();
    assert_eq!(
        ipv4::parse(&config(LOCAL), &wrong).unwrap_err(),
        PacketError::Destination
    );
}
