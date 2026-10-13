//! Inner TCP segment codec over the strict IPv4 layer: length-checked options (at
//! most the 40 bytes the data offset allows), exact
//! pseudo-header checksum (mandatory, unlike UDP) and builders bounded by a
//! caller MTU. It never allocates from a length field and has no state.

use super::ipv4::{
    self, BuildError, HEADER_LEN as IPV4_HEADER_LEN, PROTOCOL_TCP, check_mtu, fold, sum,
};
use std::net::Ipv4Addr;

pub const HEADER_LEN: usize = 20;
/// RFC 9293: the MSS assumed when the peer sent no option.
pub const DEFAULT_PEER_MSS: u16 = 536;
/// A peer MSS below this is refused instead of producing tiny segments.
pub const MIN_PEER_MSS: u16 = 64;

pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;
/// Urgent, ECN-echo and congestion-window-reduced are not supported.
const UNSUPPORTED_FLAGS: u8 = 0xe0;
const OPTION_END: u8 = 0;
const OPTION_NOP: u8 = 1;
const OPTION_MSS: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegmentError {
    Truncated,
    /// A data offset below 5 words or beyond the bytes present.
    Offset,
    /// A zero source or destination port.
    Port,
    /// A reserved or unsupported flag, or a contradictory combination.
    Flags,
    /// A nonzero urgent pointer.
    Urgent,
    /// A malformed option, or two MSS options.
    Options,
    /// An MSS option below [`MIN_PEER_MSS`].
    Mss,
    Checksum,
}

/// A validated segment. `mss` is only reported for a SYN, the one place RFC 9293
/// defines it; elsewhere the option is skipped.
#[derive(PartialEq, Eq, Debug)]
pub struct Segment<'a> {
    pub source_port: u16,
    pub destination_port: u16,
    pub sequence: u32,
    pub acknowledgment: u32,
    pub flags: u8,
    pub window: u16,
    pub mss: Option<u16>,
    pub payload: &'a [u8],
}

impl Segment<'_> {
    /// The sequence space the segment occupies: payload plus SYN and FIN.
    pub fn space(&self) -> u32 {
        let flags = u32::from(self.flags & SYN != 0) + u32::from(self.flags & FIN != 0);
        (self.payload.len() as u32).wrapping_add(flags)
    }
}

/// Pseudo-header and segment sum; the segment is last so its odd tail is padded.
fn segment_sum(source: Ipv4Addr, destination: Ipv4Addr, segment: &[u8]) -> u16 {
    let mut pseudo = [0u8; 12];
    pseudo[..4].copy_from_slice(&source.octets());
    pseudo[4..8].copy_from_slice(&destination.octets());
    pseudo[9] = PROTOCOL_TCP;
    pseudo[10..].copy_from_slice(&(segment.len() as u16).to_be_bytes());
    fold(sum(segment, sum(&pseudo, 0)))
}

fn valid_flags(flags: u8) -> bool {
    flags & UNSUPPORTED_FLAGS == 0
        && flags & (SYN | FIN) != SYN | FIN
        && flags & (SYN | RST) != SYN | RST
        && flags & (FIN | RST) != FIN | RST
}

fn parse_options(options: &[u8]) -> Result<Option<u16>, SegmentError> {
    let mut mss = None;
    let mut index = 0;
    while index < options.len() {
        match options[index] {
            OPTION_END => break,
            OPTION_NOP => index += 1,
            kind => {
                let length = usize::from(*options.get(index + 1).ok_or(SegmentError::Options)?);
                if length < 2 || index + length > options.len() {
                    return Err(SegmentError::Options);
                }
                if kind == OPTION_MSS {
                    if length != 4 || mss.is_some() {
                        return Err(SegmentError::Options);
                    }
                    mss = Some(u16::from_be_bytes([options[index + 2], options[index + 3]]));
                }
                index += length;
            }
        }
    }
    Ok(mss)
}

/// Parses an IP payload that already passed [`ipv4::parse_protocol`] for TCP.
pub fn parse(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    bytes: &[u8],
) -> Result<Segment<'_>, SegmentError> {
    if bytes.len() < HEADER_LEN {
        return Err(SegmentError::Truncated);
    }
    let offset = usize::from(bytes[12] >> 4) * 4;
    if offset < HEADER_LEN || offset > bytes.len() {
        return Err(SegmentError::Offset);
    }
    let source_port = u16::from_be_bytes([bytes[0], bytes[1]]);
    let destination_port = u16::from_be_bytes([bytes[2], bytes[3]]);
    if source_port == 0 || destination_port == 0 {
        return Err(SegmentError::Port);
    }
    let flags = bytes[13];
    // The low nibble of byte 12 holds the reserved bits and the NS flag.
    if bytes[12] & 0x0f != 0 || !valid_flags(flags) {
        return Err(SegmentError::Flags);
    }
    if bytes[18..20] != [0, 0] {
        return Err(SegmentError::Urgent);
    }
    if segment_sum(source, destination, bytes) != 0xffff {
        return Err(SegmentError::Checksum);
    }
    let mss = parse_options(&bytes[HEADER_LEN..offset])?;
    if mss.is_some_and(|mss| mss < MIN_PEER_MSS) {
        return Err(SegmentError::Mss);
    }
    Ok(Segment {
        source_port,
        destination_port,
        sequence: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        acknowledgment: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        flags,
        window: u16::from_be_bytes([bytes[14], bytes[15]]),
        mss: if flags & SYN != 0 { mss } else { None },
        payload: &bytes[offset..],
    })
}

/// The largest TCP payload an `mtu` carries without options.
pub fn max_payload(mtu: usize) -> Result<usize, BuildError> {
    check_mtu(mtu)?;
    Ok(mtu - IPV4_HEADER_LEN - HEADER_LEN)
}

/// The fields of a segment to build.
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub source_port: u16,
    pub destination_port: u16,
    pub sequence: u32,
    pub acknowledgment: u32,
    pub flags: u8,
    pub window: u16,
    /// Emitted as a 4-byte MSS option; only meaningful on a SYN.
    pub mss: Option<u16>,
}

/// Builds a complete inner IPv4/TCP packet of at most `mtu` bytes. The
/// acknowledgment is written as given; callers set it only with ACK.
pub fn build(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    header: &Header,
    payload: &[u8],
    mtu: usize,
) -> Result<Vec<u8>, BuildError> {
    check_mtu(mtu)?;
    if header.source_port == 0 || header.destination_port == 0 {
        return Err(BuildError::Port);
    }
    let options = if header.mss.is_some() { 4 } else { 0 };
    let length = HEADER_LEN + options + payload.len();
    if IPV4_HEADER_LEN + length > mtu {
        return Err(BuildError::TooLarge);
    }
    let mut segment = Vec::with_capacity(length);
    segment.extend_from_slice(&header.source_port.to_be_bytes());
    segment.extend_from_slice(&header.destination_port.to_be_bytes());
    segment.extend_from_slice(&header.sequence.to_be_bytes());
    segment.extend_from_slice(&header.acknowledgment.to_be_bytes());
    segment.push(((HEADER_LEN + options) as u8 / 4) << 4);
    segment.push(header.flags);
    segment.extend_from_slice(&header.window.to_be_bytes());
    segment.extend_from_slice(&[0, 0, 0, 0]);
    if let Some(mss) = header.mss {
        segment.extend_from_slice(&[OPTION_MSS, 4]);
        segment.extend_from_slice(&mss.to_be_bytes());
    }
    segment.extend_from_slice(payload);
    let value = !segment_sum(source, destination, &segment);
    segment[16..18].copy_from_slice(&value.to_be_bytes());
    ipv4::build(source, destination, PROTOCOL_TCP, &segment, mtu)
}

#[cfg(test)]
#[path = "tcp_tests.rs"]
mod tests;
