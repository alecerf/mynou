//! Bounded table of virtual UDP sockets keyed by local port. Every call carries
//! the caller's captured generation; a different one irreversibly revokes the
//! table, so a stale worker cannot touch a newer generation's sockets.

use super::udp::{self, DatagramError};
use super::{Fence, Ipv4Config, PacketError, ipv4};
use crate::wireguard::Entropy;
use std::collections::{BTreeMap, VecDeque};
use std::net::Ipv4Addr;
use std::time::Duration;

/// Ephemeral ports are drawn uniformly from 49152 to 65535.
pub const EPHEMERAL_START: u16 = 49152;
const EPHEMERAL_COUNT: u16 = 16384;
const BIND_ATTEMPTS: usize = 32;

/// Explicit bounds. A datagram carries at most `MAX_MTU - 28` payload bytes, so
/// the queued bytes are bounded by `queued_total` times that.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    pub sockets: usize,
    pub queued_per_socket: usize,
    pub queued_total: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            sockets: 64,
            queued_per_socket: 16,
            queued_total: 256,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum SocketError {
    Revoked,
    TableFull,
    PortInUse,
    PortsExhausted,
    /// The handle is closed, unknown or belongs to a recycled port.
    Closed,
    /// The socket is connected to a different remote.
    Remote,
    Entropy(String),
    Build(ipv4::BuildError),
}

/// A socket token. Closing a socket invalidates every copy, also after the port
/// is bound again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Handle {
    port: u16,
    serial: u64,
}

impl Handle {
    pub fn port(&self) -> u16 {
        self.port
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct Received {
    pub source: Ipv4Addr,
    pub source_port: u16,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Delivery {
    Queued,
    /// Nothing is bound to the destination port.
    NoSocket,
    /// The socket is connected and this is not its remote.
    Filtered,
    QueueFull,
    Revoked,
}

/// Why an inbound packet did not reach a socket.
#[derive(PartialEq, Eq, Debug)]
pub enum Ingest {
    Delivered(Delivery),
    Ip(PacketError),
    Udp(DatagramError),
}

struct Socket {
    serial: u64,
    remote: Option<(Ipv4Addr, u16)>,
    queue: VecDeque<Received>,
}

pub struct UdpSockets {
    limits: Limits,
    fence: Fence,
    sockets: BTreeMap<u16, Socket>,
    queued: usize,
    serial: u64,
}

impl UdpSockets {
    pub fn new(limits: Limits, generation: u64) -> Self {
        Self {
            limits,
            fence: Fence::new(generation, Duration::ZERO),
            sockets: BTreeMap::new(),
            queued: 0,
            serial: 0,
        }
    }

    /// A generation change revokes the table and drops everything it held.
    fn admit(&mut self, generation: u64) -> bool {
        let admitted = self.fence.admit_generation(generation);
        if !admitted {
            self.sockets.clear();
            self.queued = 0;
        }
        admitted
    }

    pub fn len(&self) -> usize {
        self.sockets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sockets.is_empty()
    }

    pub fn queued(&self) -> usize {
        self.queued
    }

    /// Binds `port`, or a random free ephemeral port when it is zero. Entropy
    /// failure fails the bind and never falls back to a predictable port.
    pub fn bind<E: Entropy>(
        &mut self,
        generation: u64,
        port: u16,
        entropy: &mut E,
    ) -> Result<Handle, SocketError> {
        if !self.admit(generation) {
            return Err(SocketError::Revoked);
        }
        if self.sockets.len() >= self.limits.sockets {
            return Err(SocketError::TableFull);
        }
        let port = if port != 0 {
            if self.sockets.contains_key(&port) {
                return Err(SocketError::PortInUse);
            }
            port
        } else {
            self.ephemeral(entropy)?
        };
        self.serial = self.serial.saturating_add(1);
        self.sockets.insert(
            port,
            Socket {
                serial: self.serial,
                remote: None,
                queue: VecDeque::new(),
            },
        );
        Ok(Handle {
            port,
            serial: self.serial,
        })
    }

    fn ephemeral<E: Entropy>(&self, entropy: &mut E) -> Result<u16, SocketError> {
        for _ in 0..BIND_ATTEMPTS {
            let raw = entropy
                .bytes::<2>()
                .map_err(SocketError::Entropy)
                .map(u16::from_be_bytes)?;
            let port = EPHEMERAL_START + raw % EPHEMERAL_COUNT;
            if !self.sockets.contains_key(&port) {
                return Ok(port);
            }
        }
        Err(SocketError::PortsExhausted)
    }

    fn socket(&mut self, generation: u64, handle: Handle) -> Result<&mut Socket, SocketError> {
        if !self.admit(generation) {
            return Err(SocketError::Revoked);
        }
        match self.sockets.get_mut(&handle.port) {
            Some(socket) if socket.serial == handle.serial => Ok(socket),
            _ => Err(SocketError::Closed),
        }
    }

    /// Restricts the socket to one remote: datagrams from anywhere else are
    /// filtered before they can occupy its queue.
    pub fn connect(
        &mut self,
        generation: u64,
        handle: Handle,
        remote: (Ipv4Addr, u16),
    ) -> Result<(), SocketError> {
        self.socket(generation, handle)?.remote = Some(remote);
        Ok(())
    }

    pub fn close(&mut self, generation: u64, handle: Handle) -> Result<(), SocketError> {
        let queued = self.socket(generation, handle)?.queue.len();
        self.sockets.remove(&handle.port);
        self.queued = self.queued.saturating_sub(queued);
        Ok(())
    }

    pub fn recv(
        &mut self,
        generation: u64,
        handle: Handle,
    ) -> Result<Option<Received>, SocketError> {
        let received = self.socket(generation, handle)?.queue.pop_front();
        if received.is_some() {
            self.queued = self.queued.saturating_sub(1);
        }
        Ok(received)
    }

    /// Builds the inner packet for one datagram from this socket. A connected
    /// socket sends only to its remote.
    pub fn send(
        &mut self,
        generation: u64,
        handle: Handle,
        local: Ipv4Addr,
        remote: (Ipv4Addr, u16),
        payload: &[u8],
        mtu: usize,
    ) -> Result<Vec<u8>, SocketError> {
        let socket = self.socket(generation, handle)?;
        if socket.remote.is_some_and(|connected| connected != remote) {
            return Err(SocketError::Remote);
        }
        udp::build(local, remote.0, handle.port, remote.1, payload, mtu).map_err(SocketError::Build)
    }

    /// Queues one validated datagram, within the per-socket and global bounds.
    pub fn deliver(
        &mut self,
        generation: u64,
        source: Ipv4Addr,
        source_port: u16,
        destination_port: u16,
        payload: &[u8],
    ) -> Delivery {
        if !self.admit(generation) {
            return Delivery::Revoked;
        }
        let Some(socket) = self.sockets.get_mut(&destination_port) else {
            return Delivery::NoSocket;
        };
        if socket
            .remote
            .is_some_and(|remote| remote != (source, source_port))
        {
            return Delivery::Filtered;
        }
        if socket.queue.len() >= self.limits.queued_per_socket
            || self.queued >= self.limits.queued_total
        {
            return Delivery::QueueFull;
        }
        socket.queue.push_back(Received {
            source,
            source_port,
            payload: payload.to_vec(),
        });
        self.queued += 1;
        Delivery::Queued
    }

    /// Validates one decrypted inner packet and delivers it. The configuration's
    /// inner address and allowed source network are the only admission rules.
    pub fn ingest(&mut self, generation: u64, config: &Ipv4Config, packet: &[u8]) -> Ingest {
        let packet = match ipv4::parse(config, packet) {
            Ok(packet) => packet,
            Err(error) => return Ingest::Ip(error),
        };
        match udp::parse(packet.source, packet.destination, packet.payload) {
            Ok(datagram) => Ingest::Delivered(self.deliver(
                generation,
                packet.source,
                datagram.source_port,
                datagram.destination_port,
                datagram.payload,
            )),
            Err(error) => Ingest::Udp(error),
        }
    }

    /// Irreversibly drops every socket and queued datagram.
    pub fn revoke(&mut self) {
        self.fence.revoke();
        self.sockets.clear();
        self.queued = 0;
    }
}

#[cfg(test)]
#[path = "sockets_tests.rs"]
mod tests;
