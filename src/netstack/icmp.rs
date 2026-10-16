//! Bounded ICMPv4 destination-unreachable advice. Parsing alone never changes a
//! socket: the receiving transport must also match the quoted live transmission.
use super::ipv4::{self, Ipv4Config, MAX_MTU, MIN_MTU, PacketError};
use std::net::Ipv4Addr;

pub const PROTOCOL_ICMP: u8 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    Ip(PacketError),
    Checksum,
    Unsupported,
    Quote,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    PathMtu(usize),
    Unreachable(u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ingest {
    Applied(Effect),
    Dropped,
    Invalid(Error),
    Revoked,
}

pub(super) struct Advice {
    pub remote: Ipv4Addr,
    pub protocol: u8,
    pub transport: [u8; 8],
    pub total: usize,
    pub code: u8,
    pub mtu: usize,
}

/// Outer IPv4 source admission and checksum apply to routers as well as peers.
/// Only a quote of our canonical, DF, option-free emitted header is eligible.
/// Routers may quote only the IP header and eight transport bytes; never parse
/// the quote as a complete datagram or allocate from its declared total length.
pub(super) fn parse(config: &Ipv4Config, bytes: &[u8]) -> Result<Advice, Error> {
    let packet = ipv4::parse_protocol(config, bytes, PROTOCOL_ICMP).map_err(Error::Ip)?;
    let message = packet.payload;
    if message.len() < 36 {
        return Err(Error::Quote);
    }
    if ipv4::checksum(message) != 0 {
        return Err(Error::Checksum);
    }
    if message[0] != 3 || message[1] > 15 || message[4..6] != [0, 0] {
        return Err(Error::Unsupported);
    }
    let quote = &message[8..];
    let total = usize::from(u16::from_be_bytes([quote[2], quote[3]]));
    let protocol = quote[9];
    let remote = Ipv4Addr::new(quote[16], quote[17], quote[18], quote[19]);
    let minimum = if protocol == ipv4::PROTOCOL_TCP {
        40
    } else {
        28
    };
    if quote[0] != 0x45
        || !(minimum..=MAX_MTU).contains(&total)
        || quote[4..8] != [0, 0, 0x40, 0]
        || ipv4::checksum(&quote[..20]) != 0
        || quote[12..16] != config.local().octets()
        || ipv4::unroutable(remote)
        || remote == config.local()
        || !config.allows(remote)
        || !matches!(protocol, ipv4::PROTOCOL_TCP | ipv4::PROTOCOL_UDP)
    {
        return Err(Error::Quote);
    }
    Ok(Advice {
        remote,
        protocol,
        transport: quote[20..28].try_into().map_err(|_| Error::Quote)?,
        total,
        code: message[1],
        mtu: usize::from(u16::from_be_bytes([message[6], message[7]])),
    })
}

impl Advice {
    /// Ignore unknown legacy zero MTUs and increases. A floor prevents a peer
    /// from forcing tiny packets; paths below 576 remain explicitly unsupported.
    pub fn reduction(&self, current: usize) -> Option<usize> {
        let reduced = self.mtu.max(MIN_MTU);
        (self.code == 4 && self.mtu != 0 && reduced < current && self.total > reduced)
            .then_some(reduced)
    }

    pub fn ports(&self) -> (u16, u16) {
        (
            u16::from_be_bytes([self.transport[0], self.transport[1]]),
            u16::from_be_bytes([self.transport[2], self.transport[3]]),
        )
    }
}
