//! Strict inner IPv6 header codec beside the IPv4 one. Only the fixed header
//! is accepted: every extension header, fragments included, is rejected
//! explicitly, and nothing is reassembled. Bounds and source admission are
//! those of IPv4.

use super::ip::{IpConfig, Parsed};
use super::ipv4::{BuildError, MAX_MTU, PacketError, check_mtu};
use std::net::{IpAddr, Ipv6Addr};

pub const HEADER_LEN: usize = 40;
/// RFC 8200 section 5: every IPv6 link carries 1280 bytes.
pub const MIN_MTU: usize = 1280;
pub const PROTOCOL_ICMPV6: u8 = 58;
const HOP_LIMIT: u8 = 64;
const FRAGMENT: u8 = 44;
/// Hop-by-hop, routing, destination options, authentication, ESP, mobility,
/// HIP, shim6 and the experimental values: none is processed here.
const EXTENSIONS: [u8; 10] = [0, 43, 60, 51, 50, 135, 139, 140, 253, 254];

/// Addresses that are never a valid inner unicast peer: unspecified,
/// loopback, multicast, link-local, and IPv4-mapped or -compatible forms that
/// would let an IPv4 destination slip past the IPv4 rules.
pub(super) fn unroutable(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    address.is_multicast()
        || segments[0] & 0xffc0 == 0xfe80
        || address.to_ipv4_mapped().is_some()
        || segments[..6] == [0; 6]
}

/// Parses a packet whose version nibble is 6 for one transport protocol.
pub(super) fn parse_protocol<'a>(
    config: &IpConfig,
    bytes: &'a [u8],
    expected: u8,
) -> Result<Parsed<'a>, PacketError> {
    let first = *bytes.first().ok_or(PacketError::Truncated)?;
    if first >> 4 != 6 {
        return Err(PacketError::Version);
    }
    let local = config.local_ipv6().ok_or(PacketError::Ipv6)?;
    if bytes.len() < HEADER_LEN {
        return Err(PacketError::Truncated);
    }
    let total = HEADER_LEN + usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
    if total > bytes.len() || total > MAX_MTU {
        return Err(PacketError::TotalLength);
    }
    let protocol = bytes[6];
    if protocol != expected {
        return Err(match protocol {
            FRAGMENT => PacketError::Fragment,
            protocol if EXTENSIONS.contains(&protocol) => PacketError::Extension,
            protocol => PacketError::Protocol(protocol),
        });
    }
    if bytes[7] == 0 {
        return Err(PacketError::Ttl);
    }
    let octets = |range: std::ops::Range<usize>| -> [u8; 16] {
        bytes[range].try_into().expect("16 address bytes")
    };
    let source = Ipv6Addr::from(octets(8..24));
    let destination = Ipv6Addr::from(octets(24..40));
    if destination != local {
        return Err(PacketError::Destination);
    }
    if unroutable(source) || source == local || !config.allows_ipv6(source) {
        return Err(PacketError::Source);
    }
    Ok(Parsed {
        source: IpAddr::V6(source),
        destination: IpAddr::V6(destination),
        payload: &bytes[HEADER_LEN..total],
    })
}

/// Builds one packet with the fixed header only (traffic class and flow label
/// zero) that fits `mtu`, which must carry IPv6, or fails.
pub fn build(
    source: Ipv6Addr,
    destination: Ipv6Addr,
    protocol: u8,
    payload: &[u8],
    mtu: usize,
) -> Result<Vec<u8>, BuildError> {
    check_mtu(mtu)?;
    if mtu < MIN_MTU {
        return Err(BuildError::Mtu);
    }
    if unroutable(source) {
        return Err(BuildError::Source);
    }
    if unroutable(destination) {
        return Err(BuildError::Destination);
    }
    let total = HEADER_LEN + payload.len();
    if total > mtu {
        return Err(BuildError::TooLarge);
    }
    let mut packet = Vec::with_capacity(total);
    packet.extend_from_slice(&[0x60, 0, 0, 0]);
    packet.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    packet.extend_from_slice(&[protocol, HOP_LIMIT]);
    packet.extend_from_slice(&source.octets());
    packet.extend_from_slice(&destination.octets());
    packet.extend_from_slice(payload);
    Ok(packet)
}

#[cfg(test)]
#[path = "ipv6_tests.rs"]
mod tests;
