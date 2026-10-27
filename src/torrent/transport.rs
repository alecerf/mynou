//! The network a torrent operation runs on. Outbound peer, tracker and DHT
//! code receives a `Network` explicitly and never opens a socket, resolves a
//! name or builds a default HTTP client itself; inbound peers arrive through a
//! listener of the same network. `Direct` is today's unprotected
//! ordinary network; an explicitly constructed engine can supply a tunnel.
use crate::{
    Result,
    net::{HttpClient, NetworkDatagram, NetworkStream},
};
use std::{
    io,
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket},
    sync::Arc,
    time::{Duration, Instant},
};

/// A local publication lease. Dropping it lets revocation finish. It must not
/// span network I/O; implementations can serialize it with generation loss.
pub trait Publication {}
impl Publication for () {}

pub trait Network: Send + Sync {
    /// Opaque captured protected generation, including the owner identity, or
    /// None for an unprotected transport. New owners must not reuse tokens.
    /// A protected implementation must reject a revoked generation.
    fn generation(&self) -> Result<Option<String>> {
        Ok(None)
    }

    /// Admit publication of metadata/results from this captured generation.
    /// Direct and synthetic networks have no revocation by default. Protected
    /// implementations must fence old generations and serialize cancellation
    /// with leases already admitted. No sockets or DNS may be used here.
    fn publication(&self) -> Result<Box<dyn Publication + '_>> {
        Ok(Box::new(()))
    }

    /// One outgoing peer TCP connection, bounded by `timeout`. No retry.
    fn connect(&self, address: SocketAddr, timeout: Duration) -> Result<Box<dyn NetworkStream>>;
    /// Resolves a `host:port` authority. Implementations bound the lookup by
    /// `deadline` where they can; an error is final.
    fn resolve(&self, authority: &str, deadline: Instant) -> Result<Vec<SocketAddr>>;
    /// An unconnected datagram socket of one address family.
    fn datagram(&self, ipv6: bool) -> Result<Box<dyn NetworkDatagram>>;
    /// The HTTP client of tracker requests, before per-request limits.
    fn http(&self) -> HttpClient;
    /// A listener bound to `address` of this network; port 0 picks a port.
    fn listen(&self, address: SocketAddr) -> Result<Box<dyn NetworkListener>>;
}

/// Inbound peer connections of one `Network`.
pub trait NetworkListener: Send {
    /// One pending connection without blocking: `WouldBlock` when none is
    /// waiting. Accepted streams block until their configured timeouts.
    fn accept(&self) -> io::Result<(Box<dyn NetworkStream>, SocketAddr)>;
    /// The bound port, which peers and trackers are told about.
    fn port(&self) -> u16;
}

/// The ordinary network of an explicitly unprotected engine. These are the
/// only peer, listener, tracker and DHT socket sites of the torrent client.
pub struct Direct;

pub fn direct() -> Arc<dyn Network> {
    Arc::new(Direct)
}

impl Network for Direct {
    fn connect(&self, address: SocketAddr, timeout: Duration) -> Result<Box<dyn NetworkStream>> {
        let stream = TcpStream::connect_timeout(&address, timeout)
            .map_err(|_| "Could not connect to peer")?;
        stream
            .set_nodelay(true)
            .map_err(|_| "Could not configure TCP")?;
        Ok(Box::new(stream))
    }

    /// The system resolver cannot be bounded; `deadline` is not enforced.
    fn resolve(&self, authority: &str, _deadline: Instant) -> Result<Vec<SocketAddr>> {
        Ok(authority
            .to_socket_addrs()
            .map_err(|_| "Could not resolve address")?
            .take(16)
            .collect())
    }

    fn datagram(&self, ipv6: bool) -> Result<Box<dyn NetworkDatagram>> {
        let socket = UdpSocket::bind(if ipv6 { "[::]:0" } else { "0.0.0.0:0" })
            .map_err(|_| "Could not create UDP socket")?;
        Ok(Box::new(socket))
    }

    fn http(&self) -> HttpClient {
        HttpClient::new()
    }

    fn listen(&self, address: SocketAddr) -> Result<Box<dyn NetworkListener>> {
        let listener = TcpListener::bind(address).map_err(|_| "BitTorrent port unavailable")?;
        let port = listener
            .local_addr()
            .map_err(|_| "BitTorrent address unavailable")?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|_| "Could not configure the BitTorrent port")?;
        Ok(Box::new(DirectListener { listener, port }))
    }
}

struct DirectListener {
    listener: TcpListener,
    port: u16,
}

impl NetworkListener for DirectListener {
    fn accept(&self) -> io::Result<(Box<dyn NetworkStream>, SocketAddr)> {
        let (stream, address) = self.listener.accept()?;
        crate::net::blocking(&stream).map_err(io::Error::other)?;
        stream.set_nodelay(true)?;
        Ok((Box::new(stream), address))
    }

    fn port(&self) -> u16 {
        self.port
    }
}
