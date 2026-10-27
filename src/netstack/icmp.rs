//! Bounded ICMPv4 destination-unreachable and ICMPv6 destination-unreachable
//! and packet-too-big advice, and ICMPv6 echo replies. Parsing alone never
//! changes a socket: the receiving transport must also match the quoted live
//! transmission.
use super::ip::{self, IpConfig};
use super::ipv4::{self, MAX_MTU, PacketError};
use super::ipv6::{self, PROTOCOL_ICMPV6};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub const PROTOCOL_ICMP: u8 = 1;
/// The ICMPv4 code that carries a next-hop MTU; ICMPv6 packet-too-big is
/// reported with it too.
const FRAGMENTATION_NEEDED: u8 = 4;
const ICMPV6_UNREACHABLE: u8 = 1;
const ICMPV6_TOO_BIG: u8 = 2;
const ICMPV6_ECHO_REQUEST: u8 = 128;
const ICMPV6_ECHO_REPLY: u8 = 129;

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
    /// Our address in the quoted header.
    pub local: IpAddr,
    pub remote: IpAddr,
    pub protocol: u8,
    pub transport: [u8; 8],
    pub total: usize,
    /// An ICMPv4 unreachable code; ICMPv6 codes are translated to it.
    pub code: u8,
    pub mtu: usize,
    /// The smallest MTU the family allows.
    floor: usize,
}

/// Outer source admission and checksum apply to routers as well as peers.
/// Only a quote of our canonical emitted header is eligible. Routers may
/// quote only the IP header and eight transport bytes; never parse the quote
/// as a complete datagram or allocate from its declared total length.
pub(super) fn parse(config: &IpConfig, bytes: &[u8]) -> Result<Advice, Error> {
    if bytes.first().is_some_and(|first| first >> 4 == 6) && config.local_ipv6().is_some() {
        return parse_ipv6(config, bytes);
    }
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
    if quote[0] != 0x45
        || !(minimum(protocol, ipv4::HEADER_LEN)..=MAX_MTU).contains(&total)
        || quote[4..8] != [0, 0, 0x40, 0]
        || ipv4::checksum(&quote[..20]) != 0
        || quote[12..16] != config.local().octets()
        || !config.peer(IpAddr::V4(remote))
        || !matches!(protocol, ipv4::PROTOCOL_TCP | ipv4::PROTOCOL_UDP)
    {
        return Err(Error::Quote);
    }
    Ok(Advice {
        local: IpAddr::V4(config.local()),
        remote: IpAddr::V4(remote),
        protocol,
        transport: quote[20..28].try_into().map_err(|_| Error::Quote)?,
        total,
        code: message[1],
        mtu: usize::from(u16::from_be_bytes([message[6], message[7]])),
        floor: ipv4::MIN_MTU,
    })
}

/// The smallest quoted datagram a transport could have sent.
fn minimum(protocol: u8, header: usize) -> usize {
    header
        + if protocol == ipv4::PROTOCOL_TCP {
            20
        } else {
            8
        }
}

/// The ICMPv6 message of a packet, after the IPv6 checks and its checksum
/// over the pseudo-header (RFC 4443 section 2.3).
fn icmpv6<'a>(config: &IpConfig, bytes: &'a [u8]) -> Result<(ip::Parsed<'a>, &'a [u8]), Error> {
    let packet = ipv6::parse_protocol(config, bytes, PROTOCOL_ICMPV6).map_err(Error::Ip)?;
    let message = packet.payload;
    if message.len() < 8 {
        return Err(Error::Quote);
    }
    let pseudo = ip::pseudo_sum(
        packet.source,
        packet.destination,
        PROTOCOL_ICMPV6,
        message.len(),
    );
    if ipv4::fold(ipv4::sum(message, pseudo)) != 0xffff {
        return Err(Error::Checksum);
    }
    Ok((packet, message))
}

/// Destination unreachable (with its code translated to the closest ICMPv4
/// one) and packet too big, quoting our canonical IPv6 header.
fn parse_ipv6(config: &IpConfig, bytes: &[u8]) -> Result<Advice, Error> {
    let (_, message) = icmpv6(config, bytes)?;
    let (code, mtu) = match (message[0], message[1]) {
        // No route, beyond the source's scope: network unreachable.
        (ICMPV6_UNREACHABLE, 0 | 2) if message[4..8] == [0; 4] => (0, 0),
        // Administratively prohibited, failed policy, reject route.
        (ICMPV6_UNREACHABLE, 1 | 5 | 6) if message[4..8] == [0; 4] => (13, 0),
        (ICMPV6_UNREACHABLE, 3) if message[4..8] == [0; 4] => (1, 0),
        (ICMPV6_UNREACHABLE, 4) if message[4..8] == [0; 4] => (3, 0),
        (ICMPV6_TOO_BIG, 0) => {
            let mtu = u32::from_be_bytes([message[4], message[5], message[6], message[7]]);
            (
                FRAGMENTATION_NEEDED,
                usize::try_from(mtu).unwrap_or(usize::MAX),
            )
        }
        _ => return Err(Error::Unsupported),
    };
    let quote = &message[8..];
    if quote.len() < ipv6::HEADER_LEN + 8 {
        return Err(Error::Quote);
    }
    let total = ipv6::HEADER_LEN + usize::from(u16::from_be_bytes([quote[4], quote[5]]));
    let protocol = quote[6];
    let address = |range: std::ops::Range<usize>| -> Ipv6Addr {
        let octets: [u8; 16] = quote[range].try_into().expect("16 address bytes");
        Ipv6Addr::from(octets)
    };
    let (local, remote) = (address(8..24), address(24..40));
    if quote[..4] != [0x60, 0, 0, 0]
        || !(minimum(protocol, ipv6::HEADER_LEN)..=MAX_MTU).contains(&total)
        || Some(local) != config.local_ipv6()
        || !config.peer(IpAddr::V6(remote))
        || !matches!(protocol, ipv4::PROTOCOL_TCP | ipv4::PROTOCOL_UDP)
    {
        return Err(Error::Quote);
    }
    Ok(Advice {
        local: IpAddr::V6(local),
        remote: IpAddr::V6(remote),
        protocol,
        transport: quote[40..48].try_into().map_err(|_| Error::Quote)?,
        total,
        code,
        mtu,
        floor: ipv6::MIN_MTU,
    })
}

/// The reply to a valid ICMPv6 echo request for our address, carrying its
/// identifier, sequence and data back (RFC 4443 section 4.2); `None` for
/// anything else. One request yields at most one reply of the same size.
pub fn echo_reply(config: &IpConfig, bytes: &[u8], mtu: usize) -> Option<Vec<u8>> {
    let (packet, message) = icmpv6(config, bytes).ok()?;
    if message[..2] != [ICMPV6_ECHO_REQUEST, 0] {
        return None;
    }
    let mut reply = message.to_vec();
    reply[0] = ICMPV6_ECHO_REPLY;
    reply[2..4].fill(0);
    let pseudo = ip::pseudo_sum(
        packet.destination,
        packet.source,
        PROTOCOL_ICMPV6,
        reply.len(),
    );
    let value = !ipv4::fold(ipv4::sum(&reply, pseudo));
    reply[2..4].copy_from_slice(&value.to_be_bytes());
    ip::build(
        packet.destination,
        packet.source,
        PROTOCOL_ICMPV6,
        &reply,
        mtu,
    )
    .ok()
}

impl Advice {
    /// Ignore unknown legacy zero MTUs and increases. A floor prevents a peer
    /// from forcing tiny packets; paths below 576 bytes for IPv4 and 1280 for
    /// IPv6 remain explicitly unsupported.
    pub fn reduction(&self, current: usize) -> Option<usize> {
        let reduced = self.mtu.max(self.floor);
        (self.code == FRAGMENTATION_NEEDED
            && self.mtu != 0
            && reduced < current
            && self.total > reduced)
            .then_some(reduced)
    }

    pub fn ports(&self) -> (u16, u16) {
        (
            u16::from_be_bytes([self.transport[0], self.transport[1]]),
            u16::from_be_bytes([self.transport[2], self.transport[3]]),
        )
    }
}
