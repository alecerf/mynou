//! Inner UDP codec over the strict IPv4 layer: exact length, pseudo-header
//! checksum with the RFC 768 zero rule, and builders bounded by a caller MTU.

use super::ipv4::{
    self, BuildError, HEADER_LEN as IPV4_HEADER_LEN, PROTOCOL_UDP, check_mtu, fold, sum,
};
use std::net::Ipv4Addr;

pub const HEADER_LEN: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DatagramError {
    Truncated,
    /// The UDP length field differs from the bytes the IP header delimits.
    Length,
    /// A zero source or destination port.
    Port,
    Checksum,
}

#[derive(PartialEq, Eq, Debug)]
pub struct Datagram<'a> {
    pub source_port: u16,
    pub destination_port: u16,
    pub payload: &'a [u8],
}

/// Pseudo-header and segment sum; the segment is last so its odd tail is padded.
fn segment_sum(source: Ipv4Addr, destination: Ipv4Addr, segment: &[u8]) -> u16 {
    let mut pseudo = [0u8; 12];
    pseudo[..4].copy_from_slice(&source.octets());
    pseudo[4..8].copy_from_slice(&destination.octets());
    pseudo[9] = PROTOCOL_UDP;
    pseudo[10..].copy_from_slice(&(segment.len() as u16).to_be_bytes());
    fold(sum(segment, sum(&pseudo, 0)))
}

/// Parses an IP payload that already passed [`ipv4::parse`]. A zero checksum
/// field means the sender computed none, which RFC 768 allows over IPv4.
pub fn parse(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    bytes: &[u8],
) -> Result<Datagram<'_>, DatagramError> {
    if bytes.len() < HEADER_LEN {
        return Err(DatagramError::Truncated);
    }
    if usize::from(u16::from_be_bytes([bytes[4], bytes[5]])) != bytes.len() {
        return Err(DatagramError::Length);
    }
    let source_port = u16::from_be_bytes([bytes[0], bytes[1]]);
    let destination_port = u16::from_be_bytes([bytes[2], bytes[3]]);
    if source_port == 0 || destination_port == 0 {
        return Err(DatagramError::Port);
    }
    if bytes[6..8] != [0, 0] && segment_sum(source, destination, bytes) != 0xffff {
        return Err(DatagramError::Checksum);
    }
    Ok(Datagram {
        source_port,
        destination_port,
        payload: &bytes[HEADER_LEN..],
    })
}

/// The largest UDP payload an `mtu` carries.
pub fn max_payload(mtu: usize) -> Result<usize, BuildError> {
    check_mtu(mtu)?;
    Ok(mtu - IPV4_HEADER_LEN - HEADER_LEN)
}

/// Builds a complete inner IPv4/UDP packet of at most `mtu` bytes.
pub fn build(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    source_port: u16,
    destination_port: u16,
    payload: &[u8],
    mtu: usize,
) -> Result<Vec<u8>, BuildError> {
    if payload.len() > max_payload(mtu)? {
        return Err(BuildError::TooLarge);
    }
    if source_port == 0 || destination_port == 0 {
        return Err(BuildError::Port);
    }
    let mut segment = Vec::with_capacity(HEADER_LEN + payload.len());
    segment.extend_from_slice(&source_port.to_be_bytes());
    segment.extend_from_slice(&destination_port.to_be_bytes());
    segment.extend_from_slice(&((HEADER_LEN + payload.len()) as u16).to_be_bytes());
    segment.extend_from_slice(&[0, 0]);
    segment.extend_from_slice(payload);
    // RFC 768: a computed zero is transmitted as all ones.
    let value = match !segment_sum(source, destination, &segment) {
        0 => 0xffff,
        value => value,
    };
    segment[6..8].copy_from_slice(&value.to_be_bytes());
    ipv4::build(source, destination, PROTOCOL_UDP, &segment, mtu)
}

#[cfg(test)]
#[path = "udp_tests.rs"]
mod tests;
