//! Strict inner IPv4 header codec for the offline userspace stack. Only plain UDP
//! datagrams or TCP segments (the caller names which) for the configured inner
//! address are accepted; IPv6, fragments, unknown options and everything else are
//! rejected explicitly.

use std::net::Ipv4Addr;

pub const HEADER_LEN: usize = 20;
/// Options are limited to End-of-Options and No-Operation padding.
pub const MAX_OPTION_BYTES: usize = 12;
pub const PROTOCOL_UDP: u8 = 17;
pub const PROTOCOL_TCP: u8 = 6;
pub const DEFAULT_TTL: u8 = 64;
/// Every IPv4 host must accept 576 bytes; smaller budgets are refused.
pub const MIN_MTU: usize = 576;
/// The datagram budget of an offline WireGuard session payload
/// (`wireguard::MAX_PAYLOAD`, asserted equal by a test). Longer input is rejected.
pub const MAX_MTU: usize = 1424;
const DONT_FRAGMENT: u16 = 0x4000;
// Reserved flag, more-fragments and offset: only DF may be set.
const FRAGMENT_BITS: u16 = 0xbfff;

/// The inner address this stack owns and the sources it accepts, which is the
/// peer's `AllowedIPs` network (`0.0.0.0/0` for the default route).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ipv4Config {
    local: Ipv4Addr,
    allowed: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Config {
    pub fn new(local: Ipv4Addr, allowed: Ipv4Addr, prefix: u8) -> Result<Self, PacketError> {
        if prefix > 32 || unroutable(local) {
            return Err(PacketError::Config);
        }
        Ok(Self {
            local,
            allowed,
            prefix,
        })
    }

    pub fn local(&self) -> Ipv4Addr {
        self.local
    }

    pub(super) fn allows(&self, source: Ipv4Addr) -> bool {
        let mask = u32::MAX
            .checked_shl(32 - u32::from(self.prefix))
            .unwrap_or(0);
        u32::from(source) & mask == u32::from(self.allowed) & mask
    }
}

/// Addresses that are never a valid inner unicast peer: this host network,
/// loopback, multicast, broadcast and the reserved class E.
pub(super) fn unroutable(address: Ipv4Addr) -> bool {
    address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address.octets()[0] >= 240
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PacketError {
    Config,
    Truncated,
    /// An IPv6 packet: not supported.
    Ipv6,
    Version,
    HeaderLength,
    /// An option other than End-of-Options or No-Operation.
    Options,
    TotalLength,
    Checksum,
    Ttl,
    /// More-fragments set or a nonzero offset; there is no reassembly.
    Fragment,
    Protocol(u8),
    Source,
    Destination,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuildError {
    Mtu,
    TooLarge,
    Port,
    Source,
    Destination,
}

/// A validated packet. `payload` ends at the IP total length: transport padding
/// after it was discarded and is never delivered.
#[derive(PartialEq, Eq, Debug)]
pub struct Packet<'a> {
    pub source: Ipv4Addr,
    pub destination: Ipv4Addr,
    pub protocol: u8,
    pub ttl: u8,
    pub payload: &'a [u8],
}

/// Adds big-endian 16-bit words to a running sum. Every call but the last must
/// pass an even length; an odd final byte is padded with zero (RFC 1071).
pub(super) fn sum(bytes: &[u8], mut total: u64) -> u64 {
    for pair in bytes.chunks(2) {
        let low = pair.get(1).copied().unwrap_or(0);
        total += u64::from(u16::from_be_bytes([pair[0], low]));
    }
    total
}

pub(super) fn fold(mut total: u64) -> u16 {
    while total >> 16 != 0 {
        total = (total & 0xffff) + (total >> 16);
    }
    total as u16
}

/// The RFC 1071 Internet checksum of `bytes`.
pub fn checksum(bytes: &[u8]) -> u16 {
    !fold(sum(bytes, 0))
}

pub fn parse<'a>(config: &Ipv4Config, bytes: &'a [u8]) -> Result<Packet<'a>, PacketError> {
    parse_protocol(config, bytes, PROTOCOL_UDP)
}

/// [`parse`] for one transport protocol; any other protocol number is rejected.
pub(super) fn parse_protocol<'a>(
    config: &Ipv4Config,
    bytes: &'a [u8],
    expected: u8,
) -> Result<Packet<'a>, PacketError> {
    let first = *bytes.first().ok_or(PacketError::Truncated)?;
    match first >> 4 {
        4 => {}
        6 => return Err(PacketError::Ipv6),
        _ => return Err(PacketError::Version),
    }
    if bytes.len() < HEADER_LEN {
        return Err(PacketError::Truncated);
    }
    let header_len = usize::from(first & 0x0f) * 4;
    if header_len < HEADER_LEN || header_len - HEADER_LEN > MAX_OPTION_BYTES {
        return Err(PacketError::HeaderLength);
    }
    if bytes.len() < header_len {
        return Err(PacketError::Truncated);
    }
    let total = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
    if total < header_len || total > bytes.len() || total > MAX_MTU {
        return Err(PacketError::TotalLength);
    }
    if fold(sum(&bytes[..header_len], 0)) != 0xffff {
        return Err(PacketError::Checksum);
    }
    if bytes[HEADER_LEN..header_len]
        .iter()
        .any(|&option| option > 1)
    {
        return Err(PacketError::Options);
    }
    if bytes[8] == 0 {
        return Err(PacketError::Ttl);
    }
    if u16::from_be_bytes([bytes[6], bytes[7]]) & FRAGMENT_BITS != 0 {
        return Err(PacketError::Fragment);
    }
    let protocol = bytes[9];
    if protocol != expected {
        return Err(PacketError::Protocol(protocol));
    }
    let source = Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]);
    let destination = Ipv4Addr::new(bytes[16], bytes[17], bytes[18], bytes[19]);
    if destination != config.local {
        return Err(PacketError::Destination);
    }
    if unroutable(source) || source == config.local || !config.allows(source) {
        return Err(PacketError::Source);
    }
    Ok(Packet {
        source,
        destination,
        protocol,
        ttl: bytes[8],
        payload: &bytes[header_len..total],
    })
}

pub(super) fn check_mtu(mtu: usize) -> Result<(), BuildError> {
    if (MIN_MTU..=MAX_MTU).contains(&mtu) {
        Ok(())
    } else {
        Err(BuildError::Mtu)
    }
}

/// Builds one unfragmented packet (DF set, identification zero, RFC 6864) that
/// fits `mtu` exactly or fails; it never truncates or fragments.
pub fn build(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    protocol: u8,
    payload: &[u8],
    mtu: usize,
) -> Result<Vec<u8>, BuildError> {
    check_mtu(mtu)?;
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
    packet.extend_from_slice(&[0x45, 0]);
    packet.extend_from_slice(&(total as u16).to_be_bytes());
    packet.extend_from_slice(&[0, 0]);
    packet.extend_from_slice(&DONT_FRAGMENT.to_be_bytes());
    packet.extend_from_slice(&[DEFAULT_TTL, protocol, 0, 0]);
    packet.extend_from_slice(&source.octets());
    packet.extend_from_slice(&destination.octets());
    let header = checksum(&packet).to_be_bytes();
    packet[10..12].copy_from_slice(&header);
    packet.extend_from_slice(payload);
    Ok(packet)
}

#[cfg(test)]
#[path = "ipv4_tests.rs"]
mod tests;
