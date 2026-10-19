//! The network a torrent operation runs on. Outbound peer, tracker and DHT
//! code receives a `Network` explicitly and never opens a socket, resolves a
//! name or builds a default HTTP client itself. `Direct` is today's unprotected
//! ordinary network; a protected engine will supply a tunnel implementation.
use crate::{
    Result,
    net::{HttpClient, NetworkDatagram, NetworkStream},
};
use std::{
    net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket},
    sync::Arc,
    time::{Duration, Instant},
};

pub trait Network: Send + Sync {
    /// One outgoing peer TCP connection, bounded by `timeout`. No retry.
    fn connect(&self, address: SocketAddr, timeout: Duration) -> Result<Box<dyn NetworkStream>>;
    /// Resolves a `host:port` authority. Implementations bound the lookup by
    /// `deadline` where they can; an error is final.
    fn resolve(&self, authority: &str, deadline: Instant) -> Result<Vec<SocketAddr>>;
    /// An unconnected datagram socket of one address family.
    fn datagram(&self, ipv6: bool) -> Result<Box<dyn NetworkDatagram>>;
    /// The HTTP client of tracker requests, before per-request limits.
    fn http(&self) -> HttpClient;
}

/// The ordinary network of an explicitly unprotected engine. These are the
/// only outbound peer, tracker and DHT socket sites of the torrent client.
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
}
