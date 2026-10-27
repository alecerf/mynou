//! The inner addresses of the stack and the dispatch between the strict IPv4
//! and IPv6 codecs. IPv4 is always configured; IPv6 only when the profile
//! routes it. Without an IPv6 address, every IPv6 packet stays an explicit
//! `PacketError::Ipv6`.

use super::ipv4::{self, BuildError, PacketError};
use super::ipv6;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The inner addresses this stack owns and the sources it accepts, which are
/// the peer's `AllowedIPs` networks (`0.0.0.0/0` and `::/0` for the default
/// routes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IpConfig {
    local: Ipv4Addr,
    allowed: Ipv4Addr,
    prefix: u8,
    ipv6: Option<Ipv6Network>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Ipv6Network {
    local: Ipv6Addr,
    allowed: Ipv6Addr,
    prefix: u8,
}

impl IpConfig {
    pub fn new(local: Ipv4Addr, allowed: Ipv4Addr, prefix: u8) -> Result<Self, PacketError> {
        if prefix > 32 || ipv4::unroutable(local) {
            return Err(PacketError::Config);
        }
        Ok(Self {
            local,
            allowed,
            prefix,
            ipv6: None,
        })
    }

    /// Adds the inner IPv6 address and the IPv6 sources the peer may send.
    pub fn with_ipv6(
        mut self,
        local: Ipv6Addr,
        allowed: Ipv6Addr,
        prefix: u8,
    ) -> Result<Self, PacketError> {
        if prefix > 128 || ipv6::unroutable(local) {
            return Err(PacketError::Config);
        }
        self.ipv6 = Some(Ipv6Network {
            local,
            allowed,
            prefix,
        });
        Ok(self)
    }

    /// The inner IPv4 address.
    pub fn local(&self) -> Ipv4Addr {
        self.local
    }

    /// The inner IPv6 address, when the stack carries IPv6.
    pub fn local_ipv6(&self) -> Option<Ipv6Addr> {
        self.ipv6.map(|network| network.local)
    }

    /// Our address in the family of `remote`, if the stack carries it.
    pub fn source(&self, remote: IpAddr) -> Option<IpAddr> {
        match remote {
            IpAddr::V4(_) => Some(IpAddr::V4(self.local)),
            IpAddr::V6(_) => self.local_ipv6().map(IpAddr::V6),
        }
    }

    pub(super) fn allows(&self, source: Ipv4Addr) -> bool {
        let mask = u32::MAX
            .checked_shl(32 - u32::from(self.prefix))
            .unwrap_or(0);
        u32::from(source) & mask == u32::from(self.allowed) & mask
    }

    pub(super) fn allows_ipv6(&self, source: Ipv6Addr) -> bool {
        self.ipv6.is_some_and(|network| {
            let mask = u128::MAX
                .checked_shl(128 - u32::from(network.prefix))
                .unwrap_or(0);
            u128::from(source) & mask == u128::from(network.allowed) & mask
        })
    }

    /// A remote this stack may exchange packets with: a unicast address of a
    /// configured family, not this host and inside the allowed network.
    pub(super) fn peer(&self, remote: IpAddr) -> bool {
        match remote {
            IpAddr::V4(v4) => !ipv4::unroutable(v4) && v4 != self.local && self.allows(v4),
            IpAddr::V6(v6) => {
                !ipv6::unroutable(v6) && Some(v6) != self.local_ipv6() && self.allows_ipv6(v6)
            }
        }
    }
}

/// Addresses that are never a valid inner unicast peer, in either family.
pub(super) fn unroutable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => ipv4::unroutable(v4),
        IpAddr::V6(v6) => ipv6::unroutable(v6),
    }
}

/// The fixed IP header length in front of a transport header.
pub(super) fn header_len(address: IpAddr) -> usize {
    match address {
        IpAddr::V4(_) => ipv4::HEADER_LEN,
        IpAddr::V6(_) => ipv6::HEADER_LEN,
    }
}

/// The smallest path MTU of the family: 576 for IPv4, 1280 for IPv6.
pub(super) fn min_mtu(address: IpAddr) -> usize {
    match address {
        IpAddr::V4(_) => ipv4::MIN_MTU,
        IpAddr::V6(_) => ipv6::MIN_MTU,
    }
}

/// The running sum of the transport pseudo-header: RFC 768/9293 for IPv4,
/// RFC 8200 section 8.1 for IPv6. Both lengths are even, so a segment summed
/// after it keeps its odd tail padding.
pub(super) fn pseudo_sum(source: IpAddr, destination: IpAddr, protocol: u8, length: usize) -> u64 {
    match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            let mut pseudo = [0u8; 12];
            pseudo[..4].copy_from_slice(&source.octets());
            pseudo[4..8].copy_from_slice(&destination.octets());
            pseudo[9] = protocol;
            pseudo[10..].copy_from_slice(&(length as u16).to_be_bytes());
            ipv4::sum(&pseudo, 0)
        }
        _ => {
            let octets = |address: IpAddr| match address {
                IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
                IpAddr::V6(v6) => v6.octets(),
            };
            let mut pseudo = [0u8; 40];
            pseudo[..16].copy_from_slice(&octets(source));
            pseudo[16..32].copy_from_slice(&octets(destination));
            pseudo[32..36].copy_from_slice(&(length as u32).to_be_bytes());
            pseudo[39] = protocol;
            ipv4::sum(&pseudo, 0)
        }
    }
}

/// A validated packet of either family. `payload` ends at the IP length:
/// transport padding after it was discarded and is never delivered.
#[derive(PartialEq, Eq, Debug)]
pub(super) struct Parsed<'a> {
    pub source: IpAddr,
    pub destination: IpAddr,
    pub payload: &'a [u8],
}

/// Parses one inner packet for one transport protocol, in the family its
/// version names. IPv6 is refused unless the stack has an IPv6 address.
pub(super) fn parse_protocol<'a>(
    config: &IpConfig,
    bytes: &'a [u8],
    expected: u8,
) -> Result<Parsed<'a>, PacketError> {
    if bytes.first().is_some_and(|first| first >> 4 == 6) && config.ipv6.is_some() {
        return ipv6::parse_protocol(config, bytes, expected);
    }
    let packet = ipv4::parse_protocol(config, bytes, expected)?;
    Ok(Parsed {
        source: IpAddr::V4(packet.source),
        destination: IpAddr::V4(packet.destination),
        payload: packet.payload,
    })
}

/// Builds one unfragmented packet in the family of its addresses.
pub(super) fn build(
    source: IpAddr,
    destination: IpAddr,
    protocol: u8,
    payload: &[u8],
    mtu: usize,
) -> Result<Vec<u8>, BuildError> {
    match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            ipv4::build(source, destination, protocol, payload, mtu)
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            ipv6::build(source, destination, protocol, payload, mtu)
        }
        _ => Err(BuildError::Destination),
    }
}
