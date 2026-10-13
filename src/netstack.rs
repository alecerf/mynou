//! Offline inner IPv4/UDP/TCP codecs, virtual UDP sockets, a bounded DNS stub
//! client and virtual TCP flows. Pure and std only: no sockets, threads, clock
//! reads or tunnel coupling. The caller passes a captured generation and a
//! monotonic elapsed time, and moves inner IP datagrams in and out. This is
//! neither routing, host DNS nor VPN protection; see docs/netstack.md and
//! docs/netstack-tcp.md for the boundary and the remaining obligations.

use std::time::Duration;

#[path = "netstack/dns.rs"]
mod dns;
#[path = "netstack/ipv4.rs"]
mod ipv4;
#[path = "netstack/sockets.rs"]
mod sockets;
#[path = "netstack/stream.rs"]
mod stream;
#[path = "netstack/tcp.rs"]
mod tcp;
#[path = "netstack/udp.rs"]
mod udp;

pub use dns::{
    Answer, DnsClient, DnsConfig, DnsError, Failure, Finished, MAX_ADDRESSES, MAX_NAME,
    MAX_RESOLVERS, Poll, QueryId, Reply, ReplyError, parse_response,
};
pub use ipv4::{
    BuildError, DEFAULT_TTL, HEADER_LEN as IPV4_HEADER_LEN, Ipv4Config, MAX_MTU, MAX_OPTION_BYTES,
    MIN_MTU, PROTOCOL_TCP, PROTOCOL_UDP, Packet, PacketError, checksum, parse as parse_ipv4,
};
pub use sockets::{
    Delivery, EPHEMERAL_START, Handle, Ingest, Limits, Received, SocketError, UdpSockets,
};
pub use stream::{
    Discard, Failure as TcpFailure, FlowHandle, Ingest as TcpIngest, ListenerHandle, Poll as TcpPoll,
    Read, State as TcpState, TcpConfig, TcpError, TcpStack,
};
pub use tcp::{
    DEFAULT_PEER_MSS, HEADER_LEN as TCP_HEADER_LEN, Header as TcpHeader, MIN_PEER_MSS, Segment,
    SegmentError, build as build_tcp, max_payload as max_tcp_payload, parse as parse_tcp,
};
pub use udp::{
    Datagram, DatagramError, HEADER_LEN as UDP_HEADER_LEN, build as build_udp, max_payload,
    parse as parse_udp,
};

/// Generation and monotonic-clock fence shared by the state machines here. Any
/// violation irreversibly revokes the state, like the offline WireGuard pieces.
struct Fence {
    generation: u64,
    observed: Duration,
    revoked: bool,
}

impl Fence {
    fn new(generation: u64, now: Duration) -> Self {
        Self {
            generation,
            observed: now,
            revoked: false,
        }
    }

    /// Admits a call that carries no clock. `false` means revoked, now or before.
    fn admit_generation(&mut self, generation: u64) -> bool {
        if !self.revoked && generation != self.generation {
            self.revoked = true;
        }
        !self.revoked
    }

    /// Admits a timed call; the clock may stand still but never move backwards.
    fn admit(&mut self, generation: u64, now: Duration) -> bool {
        if self.admit_generation(generation) && now < self.observed {
            self.revoked = true;
        }
        if !self.revoked {
            self.observed = now;
        }
        !self.revoked
    }

    fn revoke(&mut self) {
        self.revoked = true;
    }
}

#[cfg(test)]
#[path = "netstack/support.rs"]
mod support;
