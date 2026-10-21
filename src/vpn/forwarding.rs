//! Provider port forwarding inside the protected tunnel: NAT-PMP (RFC 6886)
//! mappings requested from the tunnel gateway, as ProtonVPN documents for its
//! P2P servers (UDP then TCP, public port 1, private port 0, 60 seconds,
//! renewed every 45 seconds; the client listens on the returned public port).
//! Requests are virtual UDP of one protected generation; nothing here can
//! reach the ordinary network. A missing or lost lease is its own state and
//! never a VPN failure.
use crate::{net::NetworkDatagram, torrent::transport::Network};
use std::{
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) const GATEWAY_PORT: u16 = 5351;
const UDP: u8 = 1;
const TCP: u8 = 2;
/// Proton's documented request (`natpmpc -a 1 0 <udp|tcp> 60`).
const SUGGESTED: u16 = 1;
const LIFETIME: u32 = 60;
/// Upper bound of one blocking read, so stopping never waits on a retry.
const POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// No valid answer within the retransmission schedule or budget.
    Timeout,
    /// The gateway's nonzero result code, including undefined codes.
    Refused(u16),
    /// A success answer without a port or lifetime.
    Malformed,
    /// The protected generation refused the virtual datagram, or stopping.
    Unavailable,
}

impl Failure {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Timeout => "Gateway did not answer NAT-PMP",
            Self::Refused(1) => "Gateway does not support this NAT-PMP version",
            Self::Refused(2) => "Gateway refused forwarding; use a P2P server and profile",
            Self::Refused(3) => "Gateway reported a network failure",
            Self::Refused(4) => "Gateway is out of forwarding resources",
            Self::Refused(5) => "Gateway does not support port mapping",
            Self::Refused(_) => "Gateway returned an undefined NAT-PMP result",
            Self::Malformed => "Gateway answer was malformed",
            Self::Unavailable => "Protected network unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mapping {
    /// The public TCP port, on which the torrent listener must accept.
    pub port: u16,
    pub lifetime: Duration,
}

/// RFC 6886 retransmission (first wait, doubling, attempts) within a total
/// budget, then the spacing of new attempts once no lease is held.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    pub first_wait: Duration,
    pub attempts: u32,
    pub budget: Duration,
    pub retry: Duration,
    pub retry_max: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            first_wait: Duration::from_millis(250),
            attempts: 9,
            budget: Duration::from_secs(10),
            retry: Duration::from_secs(2),
            retry_max: Duration::from_secs(60),
        }
    }
}

fn request(op: u8) -> [u8; 12] {
    let mut packet = [0; 12];
    packet[1] = op;
    packet[6..8].copy_from_slice(&SUGGESTED.to_be_bytes());
    packet[8..12].copy_from_slice(&LIFETIME.to_be_bytes());
    packet
}

/// `None` when the datagram does not answer `op`; it is then ignored.
fn answer(op: u8, packet: &[u8]) -> Option<Result<Mapping, Failure>> {
    if packet.len() != 16 || packet[0] != 0 || packet[1] != 128 + op {
        return None;
    }
    let code = u16::from_be_bytes([packet[2], packet[3]]);
    if code != 0 {
        return Some(Err(Failure::Refused(code)));
    }
    let port = u16::from_be_bytes([packet[10], packet[11]]);
    let lifetime = u32::from_be_bytes([packet[12], packet[13], packet[14], packet[15]]);
    if port == 0 || lifetime == 0 {
        return Some(Err(Failure::Malformed));
    }
    // A gateway should not grant more than requested; never trust longer.
    let lifetime = Duration::from_secs(lifetime.min(LIFETIME).into());
    Some(Ok(Mapping { port, lifetime }))
}

fn map(
    socket: &dyn NetworkDatagram,
    gateway: SocketAddr,
    op: u8,
    timing: &Timing,
    stop: &AtomicBool,
    deadline: Instant,
) -> Result<Mapping, Failure> {
    let packet = request(op);
    let mut wait = timing.first_wait;
    let mut buffer = [0; 64];
    for _ in 0..timing.attempts {
        if stop.load(Ordering::Acquire) {
            return Err(Failure::Unavailable);
        }
        socket
            .send_to(&packet, gateway)
            .map_err(|_| Failure::Unavailable)?;
        let until = (Instant::now() + wait).min(deadline);
        loop {
            let now = Instant::now();
            if now >= until || stop.load(Ordering::Acquire) {
                break;
            }
            socket
                .set_read_timeout(Some((until - now).min(POLL)))
                .map_err(|_| Failure::Unavailable)?;
            match socket.recv_from(&mut buffer) {
                // Replies from any other source are discarded (RFC 6886 3.1).
                Ok((length, source)) if source == gateway => {
                    if let Some(result) = answer(op, &buffer[..length]) {
                        return result;
                    }
                }
                Ok(_) => {}
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => return Err(Failure::Unavailable),
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        wait = wait.saturating_mul(2);
    }
    Err(Failure::Timeout)
}

/// Requests the UDP then the TCP mapping, as documented; both must succeed.
pub(crate) fn acquire(
    network: &dyn Network,
    gateway: Ipv4Addr,
    timing: &Timing,
    stop: &AtomicBool,
) -> Result<Mapping, Failure> {
    let socket = network.datagram(false).map_err(|_| Failure::Unavailable)?;
    let gateway = SocketAddr::from((gateway, GATEWAY_PORT));
    let deadline = Instant::now() + timing.budget;
    let udp = map(&*socket, gateway, UDP, timing, stop, deadline)?;
    let tcp = map(&*socket, gateway, TCP, timing, stop, deadline)?;
    Ok(Mapping {
        port: tcp.port,
        lifetime: udp.lifetime.min(tcp.lifetime),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Acquiring,
    Active(u16),
    Unavailable(&'static str),
}

struct Lease {
    status: Status,
    expiry: Option<Instant>,
}

struct Shared {
    lease: Mutex<Lease>,
    stop: AtomicBool,
    /// The port the torrent listener took, once it was read.
    advertised: Mutex<Option<Option<u16>>>,
}

impl Shared {
    fn status(&self) -> Status {
        let Ok(lease) = self.lease.lock() else {
            return Status::Unavailable("Forwarding state unavailable");
        };
        match (lease.status, lease.expiry) {
            (Status::Active(_), Some(at)) if Instant::now() >= at => {
                Status::Unavailable("Forwarded port expired")
            }
            (status, _) => status,
        }
    }
    fn set(&self, status: Status, expiry: Option<Instant>) {
        if let Ok(mut lease) = self.lease.lock() {
            *lease = Lease { status, expiry };
        }
    }
}

/// Keeps one generation's lease: renews at three quarters of its lifetime,
/// keeps a held port until it expires, and retries with a bounded backoff.
pub(crate) struct Forwarder {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Forwarder {
    pub(crate) fn start(network: Arc<dyn Network>, gateway: Ipv4Addr, timing: Timing) -> Self {
        let shared = Arc::new(Shared {
            lease: Mutex::new(Lease {
                status: Status::Acquiring,
                expiry: None,
            }),
            stop: AtomicBool::new(false),
            advertised: Mutex::new(None),
        });
        let worker = {
            let shared = shared.clone();
            thread::spawn(move || run(&shared, &*network, gateway, timing))
        };
        Self {
            shared,
            worker: Mutex::new(Some(worker)),
        }
    }

    pub(crate) fn status(&self) -> Status {
        self.shared.status()
    }

    fn port(&self) -> Option<u16> {
        match self.status() {
            Status::Active(port) => Some(port),
            _ => None,
        }
    }

    /// The port the torrent listener uses, recorded as advertised.
    pub(crate) fn advertise(&self) -> Option<u16> {
        let port = self.port();
        if let Ok(mut advertised) = self.shared.advertised.lock() {
            *advertised = Some(port);
        }
        port
    }

    /// The advertised port no longer matches the lease: it changed, expired
    /// or was granted after the listener started without one.
    pub(crate) fn stale(&self) -> bool {
        self.shared
            .advertised
            .lock()
            .is_ok_and(|advertised| advertised.is_some_and(|port| port != self.port()))
    }

    pub(crate) fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.get_mut().ok().and_then(Option::take) {
            let _ = worker.join();
        }
    }
}

fn run(shared: &Shared, network: &dyn Network, gateway: Ipv4Addr, timing: Timing) {
    let mut retry = timing.retry;
    let mut expiry: Option<Instant> = None;
    while !shared.stop.load(Ordering::Acquire) {
        let wait = match acquire(network, gateway, &timing, &shared.stop) {
            Ok(mapping) => {
                let at = Instant::now() + mapping.lifetime;
                expiry = Some(at);
                shared.set(Status::Active(mapping.port), expiry);
                retry = timing.retry;
                mapping.lifetime * 3 / 4
            }
            Err(failure) => match expiry {
                // The held port stays valid until its lease ends.
                Some(at) if Instant::now() < at => retry.min(at - Instant::now()),
                _ => {
                    expiry = None;
                    shared.set(Status::Unavailable(failure.reason()), None);
                    let wait = retry;
                    retry = retry.saturating_mul(2).min(timing.retry_max);
                    wait
                }
            },
        };
        let until = Instant::now() + wait;
        while !shared.stop.load(Ordering::Acquire) && Instant::now() < until {
            thread::sleep((until - Instant::now()).min(POLL));
        }
    }
}

#[cfg(test)]
#[path = "forwarding_tests.rs"]
mod tests;
