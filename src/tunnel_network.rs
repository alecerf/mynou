//! The torrent `Network` of one protected tunnel generation: virtual TCP peers
//! and listeners, virtual UDP and tunnel DNS, all through the native owner.
//! Nothing here reaches the ordinary network: refused destinations and lost
//! generations fail, and there is no `Direct` fallback.
use super::{Connector, Resource, Stream, error, failed, pause, remaining, usable};
use crate::{
    net::{HttpClient, HttpConnector, NetworkDatagram, NetworkStream},
    netstack::{EPHEMERAL_START, TcpError},
    torrent::transport::{Network, NetworkListener},
    tunnel::{TunnelError, VirtualListener, VirtualUdp},
    wireguard::{Entropy, OsEntropy},
};
use std::{
    io,
    net::{IpAddr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Answers kept per resolved authority, as for `Direct`.
const ANSWERS: usize = 16;
/// Random listener ports tried when port 0 is requested.
const LISTEN_ATTEMPTS: usize = 32;
/// One non-blocking accept waits at most this long for the owner.
const ACCEPT_WAIT: Duration = Duration::from_millis(500);

fn message(error: io::Error) -> String {
    error.to_string()
}

/// A literal, usable IPv4 destination with a nonzero port.
fn destination(address: SocketAddr) -> io::Result<SocketAddrV4> {
    match address {
        SocketAddr::V4(v4) if v4.port() != 0 && usable(*v4.ip()) => Ok(v4),
        _ => Err(error(io::ErrorKind::InvalidInput)),
    }
}

pub(super) struct TunnelNetwork {
    connector: Arc<Connector>,
}

impl TunnelNetwork {
    pub(super) fn new(connector: Connector) -> Self {
        Self {
            connector: Arc::new(connector),
        }
    }

    fn resolve_authority(&self, authority: &str, deadline: Instant) -> io::Result<Vec<SocketAddr>> {
        remaining(deadline)?;
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or_else(|| error(io::ErrorKind::InvalidInput))?;
        if !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(error(io::ErrorKind::InvalidInput));
        }
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| error(io::ErrorKind::InvalidInput))?;
        let generation = Some(self.connector.generation);
        // A literal needs no owner call; admit it like any lookup, and reject
        // an answer observed after loss or cancellation.
        self.connector.owner.admit(generation)?;
        let addresses = self
            .connector
            .resolve(host, deadline)?
            .into_iter()
            .take(ANSWERS)
            .map(|ip| SocketAddr::from((ip, port)))
            .collect();
        remaining(deadline)?;
        self.connector.owner.admit(generation)?;
        Ok(addresses)
    }

    fn bind_datagram(&self, ipv6: bool) -> io::Result<Datagram> {
        // The tunnel carries IPv4 only; IPv6 is refused, never sent elsewhere.
        if ipv6 {
            return Err(error(io::ErrorKind::InvalidInput));
        }
        let (connector, lease) = (&self.connector, Arc::new(()));
        let weak = Arc::downgrade(&lease);
        let socket = connector.owner.call(
            Some(connector.generation),
            Instant::now() + ACCEPT_WAIT,
            move |r| {
                let socket = r.tunnel.udp_bind(0).map_err(failed)?;
                r.leases.push((
                    r.tunnel.status().generation.unwrap_or(0),
                    weak,
                    Resource::Udp(socket),
                ));
                Ok(socket)
            },
        )?;
        Ok(Datagram {
            connector: connector.clone(),
            socket,
            _lease: lease,
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
        })
    }

    fn bind_listener(&self, address: SocketAddr) -> io::Result<Listener> {
        // Unspecified and loopback binds both name the virtual interface: the
        // listener is reachable only through the tunnel, never on the host.
        let requested = match address {
            SocketAddr::V4(v4) if v4.ip().is_unspecified() || v4.ip().is_loopback() => v4.port(),
            _ => return Err(error(io::ErrorKind::InvalidInput)),
        };
        let (connector, lease) = (&self.connector, Arc::new(()));
        let weak = Arc::downgrade(&lease);
        let (listener, port) = connector.owner.call(
            Some(connector.generation),
            Instant::now() + ACCEPT_WAIT,
            move |r| {
                let mut attempt = |port| match r.tunnel.tcp_listen(port) {
                    Ok(listener) => Ok(Some((listener, port))),
                    Err(TunnelError::Tcp(TcpError::PortInUse)) if requested == 0 => Ok(None),
                    Err(e) => Err(failed(e)),
                };
                let mut bound = None;
                if requested != 0 {
                    bound = attempt(requested)?;
                } else {
                    for _ in 0..LISTEN_ATTEMPTS {
                        let random = OsEntropy
                            .bytes::<2>()
                            .map_err(|_| error(io::ErrorKind::Other))?;
                        let span = u16::MAX - EPHEMERAL_START + 1;
                        bound = attempt(EPHEMERAL_START + u16::from_be_bytes(random) % span)?;
                        if bound.is_some() {
                            break;
                        }
                    }
                }
                let (listener, port) = bound.ok_or_else(|| error(io::ErrorKind::AddrInUse))?;
                r.leases.push((
                    r.tunnel.status().generation.unwrap_or(0),
                    weak,
                    Resource::Listener(listener),
                ));
                Ok((listener, port))
            },
        )?;
        Ok(Listener {
            connector: connector.clone(),
            listener,
            port,
            _lease: lease,
        })
    }
}

struct PublicationLease<'a> {
    _guard: std::sync::RwLockReadGuard<'a, ()>,
}
impl crate::torrent::transport::Publication for PublicationLease<'_> {}

impl Network for TunnelNetwork {
    fn generation(&self) -> crate::Result<Option<String>> {
        self.connector
            .owner
            .admit(Some(self.connector.generation))
            .map_err(message)?;
        Ok(Some(format!(
            "{:032x}:{}",
            self.connector.owner.identity, self.connector.generation
        )))
    }

    fn publication(&self) -> crate::Result<Box<dyn crate::torrent::transport::Publication + '_>> {
        let owner = &self.connector.owner;
        owner
            .admit(Some(self.connector.generation))
            .map_err(message)?;
        let lease = owner
            .publication
            .read()
            .map_err(|_| "Tunnel publication lock poisoned")?;
        owner
            .admit(Some(self.connector.generation))
            .map_err(message)?;
        Ok(Box::new(PublicationLease { _guard: lease }))
    }

    fn connect(
        &self,
        address: SocketAddr,
        timeout: Duration,
    ) -> crate::Result<Box<dyn NetworkStream>> {
        let open = || -> io::Result<Box<dyn NetworkStream>> {
            let address = destination(address)?;
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| error(io::ErrorKind::InvalidInput))?;
            Ok(Box::new(self.connector.open(
                *address.ip(),
                address.port(),
                deadline,
            )?))
        };
        open().map_err(message)
    }

    /// Tunnel DNS only, bounded by `deadline`; literal IPv4 needs no lookup.
    fn resolve(&self, authority: &str, deadline: Instant) -> crate::Result<Vec<SocketAddr>> {
        self.resolve_authority(authority, deadline).map_err(message)
    }

    fn datagram(&self, ipv6: bool) -> crate::Result<Box<dyn NetworkDatagram>> {
        Ok(Box::new(self.bind_datagram(ipv6).map_err(message)?))
    }

    fn http(&self) -> HttpClient {
        let connector: Arc<dyn HttpConnector> = self.connector.clone();
        HttpClient::new().with_connector(connector)
    }

    fn listen(&self, address: SocketAddr) -> crate::Result<Box<dyn NetworkListener>> {
        Ok(Box::new(self.bind_listener(address).map_err(message)?))
    }
}

struct Datagram {
    connector: Arc<Connector>,
    socket: VirtualUdp,
    _lease: Arc<()>,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
}

impl Datagram {
    fn deadline(slot: &Mutex<Option<Duration>>) -> io::Result<Instant> {
        Stream::deadline(*slot.lock().map_err(|_| error(io::ErrorKind::Other))?)
    }
}

impl NetworkDatagram for Datagram {
    fn send_to(&self, packet: &[u8], target: SocketAddr) -> io::Result<usize> {
        let target = destination(target)?;
        // Bound the command allocation before copying a caller-supplied slice.
        // The native socket additionally checks its configured/path MTU.
        let max_payload = crate::netstack::MAX_MTU
            - crate::netstack::IPV4_HEADER_LEN
            - crate::netstack::UDP_HEADER_LEN;
        if packet.len() > max_payload {
            return Err(error(io::ErrorKind::InvalidInput));
        }
        let deadline = Self::deadline(&self.write_timeout)?;
        let (socket, payload) = (self.socket, packet.to_vec());
        // Like a blocking OS socket, wait for queue space: a datagram refused
        // at once by a busy tunnel looks like a lost reply to callers such as
        // the DHT, which then give up on that node.
        self.connector
            .owner
            .wait(Some(self.connector.generation), deadline, move |r| {
                r.tunnel
                    .udp_send_to(socket, (*target.ip(), target.port()), &payload)
                    .map_err(failed)
            })?;
        Ok(packet.len())
    }

    /// Like an OS socket: the datagram is truncated to `buffer`.
    fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let deadline = Self::deadline(&self.read_timeout)?;
        let socket = self.socket;
        loop {
            let received =
                self.connector
                    .owner
                    .wait(Some(self.connector.generation), deadline, move |r| {
                        r.tunnel.udp_recv(socket).map_err(failed)
                    })?;
            if let Some(received) = received {
                let n = received.payload.len().min(buffer.len());
                buffer[..n].copy_from_slice(&received.payload[..n]);
                return Ok((n, SocketAddr::from((received.source, received.source_port))));
            }
            pause(deadline)?;
        }
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        super::set_timeout(&self.read_timeout, timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        super::set_timeout(&self.write_timeout, timeout)
    }
}

struct Listener {
    connector: Arc<Connector>,
    listener: VirtualListener,
    port: u16,
    _lease: Arc<()>,
}

impl NetworkListener for Listener {
    fn accept(&self) -> io::Result<(Box<dyn NetworkStream>, SocketAddr)> {
        let (connector, listener, lease) = (&self.connector, self.listener, Arc::new(()));
        let weak = Arc::downgrade(&lease);
        let accepted = connector.owner.call(
            Some(connector.generation),
            Instant::now() + ACCEPT_WAIT,
            move |r| {
                let flow = r.tunnel.tcp_accept(listener).map_err(failed)?;
                if let Some(flow) = flow {
                    r.leases.push((
                        r.tunnel.status().generation.unwrap_or(0),
                        weak,
                        Resource::Flow(flow),
                    ));
                }
                Ok(flow)
            },
        )?;
        let flow = accepted.ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;
        let (ip, port) = flow.remote();
        Ok((
            Box::new(Stream::new(connector, flow, lease)),
            SocketAddr::from((IpAddr::V4(ip), port)),
        ))
    }

    fn port(&self) -> u16 {
        self.port
    }
}
