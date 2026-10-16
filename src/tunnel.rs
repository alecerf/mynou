//! Outer-UDP tunnel runtime: one owner drives the offline WireGuard `Peer` and
//! the virtual IPv4/UDP/DNS/TCP stack over a single physical UDP socket, behind
//! one generation fence. It has no torrent caller, profile or user-visible
//! protection, and its loopback tests are not interoperability evidence; see
//! docs/tunnel.md for the boundary and the remaining gates.

use crate::netstack::{
    Answer, Delivery, DnsClient, DnsConfig, DnsError, Failure, Finished, FlowHandle, Handle,
    Ingest, Ipv4Config, Limits, ListenerHandle, MAX_MTU, MIN_MTU, PROTOCOL_TCP, PROTOCOL_UDP,
    QueryId, Read, Received, SocketError, TcpConfig, TcpError, TcpIngest, TcpStack, TcpState,
    UdpSockets,
};
use crate::wireguard::{
    Entropy, Load, MAX_DATAGRAM, MAX_QUEUED_PAYLOADS, Peer, PeerClock, PeerOutput, PeerStatus,
};
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant, SystemTime};

/// Longest accepted endpoint host name, as for DNS.
pub const MAX_ENDPOINT_NAME: usize = 253;
/// Width of the window in which handshake messages are counted for `Load`.
pub const LOAD_WINDOW: Duration = Duration::from_secs(1);
/// Larger than any acceptable outer datagram, so oversize input is detected
/// rather than silently truncated.
const RECEIVE_BUFFER: usize = 2048;
const HOUR: Duration = Duration::from_secs(3600);

/// The runtime's only time source: a monotonic elapsed time and the wall time
/// used for handshake timestamps. Production uses [`SystemClock`].
pub trait Clock {
    fn now(&self) -> Duration;
    fn wall(&self) -> SystemTime;
}

pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// The pinned outer peer. A name is resolved only by [`resolve_endpoint`],
/// explicitly, when a tunnel is created or its endpoint is replaced.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Endpoint {
    Address(SocketAddr),
    Name { host: String, port: u16 },
}

/// The one ordinary-path lookup of the runtime. Only a usable IPv4 unicast
/// answer is accepted; IPv6 is refused rather than tried.
pub fn resolve_endpoint(endpoint: &Endpoint) -> Result<SocketAddr, TunnelError> {
    let usable = |address: &SocketAddr| match address {
        SocketAddr::V4(v4) => {
            let ip = v4.ip();
            v4.port() != 0
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && ip.octets()[0] < 240
        }
        SocketAddr::V6(_) => false,
    };
    match endpoint {
        Endpoint::Address(address) if usable(address) => Ok(*address),
        Endpoint::Address(_) => Err(TunnelError::Endpoint("unusable endpoint address".into())),
        Endpoint::Name { host, port } => {
            if host.is_empty() || host.len() > MAX_ENDPOINT_NAME || *port == 0 {
                return Err(TunnelError::Endpoint("invalid endpoint name".into()));
            }
            let answers = (host.as_str(), *port)
                .to_socket_addrs()
                .map_err(|error| TunnelError::Endpoint(error.to_string()))?;
            answers
                .take(16)
                .find(usable)
                .ok_or_else(|| TunnelError::Endpoint("no usable IPv4 endpoint address".into()))
        }
    }
}

/// Every bound of the runtime. Secrets are never printed: no `Debug`.
#[derive(Clone)]
pub struct TunnelConfig {
    pub secret: [u8; 32],
    pub peer_public: [u8; 32],
    pub psk: Option<[u8; 32]>,
    pub endpoint: Endpoint,
    /// Local address of the outer socket; IPv4 only.
    pub bind: SocketAddr,
    pub ip: Ipv4Config,
    pub mtu: usize,
    /// Start a handshake at once and keep the session fresh, or only answer.
    pub initiate: bool,
    /// Interval of the empty keepalive while protected, in either role. On the
    /// initiator it also drives the time-based rekey, so `None` lets an idle
    /// session expire.
    pub keepalive: Option<Duration>,
    pub peer_queue: usize,
    pub udp: Limits,
    pub tcp: TcpConfig,
    pub dns: Option<DnsConfig>,
    /// Framed, MAC1-valid handshake messages per `LOAD_WINDOW` above which
    /// the load is `High`.
    pub handshake_load: u32,
    /// Outer socket reads attempted by one `pump`, including socket errors.
    pub datagrams_per_pump: usize,
    pub reconnect_initial: Duration,
    pub reconnect_max: Duration,
    /// Consecutive failed generations before the runtime is `Blocked`.
    pub reconnect_attempts: u32,
}

impl TunnelConfig {
    pub fn new(
        secret: [u8; 32],
        peer_public: [u8; 32],
        endpoint: Endpoint,
        ip: Ipv4Config,
    ) -> Self {
        Self {
            secret,
            peer_public,
            psk: None,
            endpoint,
            bind: SocketAddr::from(([0, 0, 0, 0], 0)),
            ip,
            mtu: MAX_MTU,
            initiate: true,
            keepalive: Some(Duration::from_secs(25)),
            peer_queue: MAX_QUEUED_PAYLOADS,
            udp: Limits::default(),
            tcp: TcpConfig::new(ip, MAX_MTU),
            dns: None,
            handshake_load: 32,
            datagrams_per_pump: 64,
            reconnect_initial: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
            reconnect_attempts: 8,
        }
    }

    /// Adds the inner resolvers, with the DNS client defaults.
    pub fn with_dns(mut self, resolvers: Vec<Ipv4Addr>) -> Self {
        self.dns = Some(DnsConfig::new(self.ip, resolvers, self.mtu));
        self
    }

    fn validate(&self) -> Result<(), TunnelError> {
        let timer = |value: Duration| !value.is_zero() && value <= HOUR;
        let keepalive = self.keepalive.is_none_or(|value| {
            value.subsec_nanos() == 0 && (1..=65535).contains(&value.as_secs())
        });
        let dns = self
            .dns
            .as_ref()
            .is_none_or(|dns| dns.ip == self.ip && dns.mtu == self.mtu);
        let valid = (MIN_MTU..=MAX_MTU).contains(&self.mtu)
            && self.tcp.ip == self.ip
            && self.tcp.mtu == self.mtu
            && dns
            && keepalive
            && self.bind.is_ipv4()
            && (1..=MAX_QUEUED_PAYLOADS).contains(&self.peer_queue)
            && (1..=1024).contains(&self.handshake_load)
            && (1..=1024).contains(&self.datagrams_per_pump)
            && timer(self.reconnect_initial)
            && timer(self.reconnect_max)
            && self.reconnect_initial <= self.reconnect_max
            && (1..=64).contains(&self.reconnect_attempts);
        if valid {
            Ok(())
        } else {
            Err(TunnelError::Config)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TunnelError {
    Config,
    Endpoint(String),
    Io(String),
    /// No generation is live: reconnecting, blocked or closed.
    Unavailable,
    /// The handle belongs to a revoked generation.
    Revoked,
    /// The peer's bounded outbound queue is full.
    QueueFull,
    /// DNS pending queries plus unclaimed results reached the bound.
    Busy,
    NoDns,
    Peer(String),
    Udp(SocketError),
    Tcp(TcpError),
    Dns(DnsError),
}

/// `Protected` means an authenticated current session with a live virtual
/// stack, never that any destination is reachable or that traffic is isolated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TunnelState {
    Connecting,
    Protected,
    Reconnecting,
    Blocked,
    Closed,
}

/// Why the last generation was revoked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    /// The handshake attempt budget ran out without a session.
    GaveUp,
    /// The session expired, for example after endpoint silence.
    Expired,
    /// The peer or a stack refused further use: entropy or clock failure.
    Failed,
    EndpointChanged,
    Cancelled,
    /// `reconnect_attempts` consecutive generations failed.
    Exhausted,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Status {
    pub state: TunnelState,
    pub generation: Option<u64>,
    pub reason: Option<Reason>,
    /// Consecutive failed generations since the last protected one.
    pub attempt: u32,
    pub retry_at: Option<Duration>,
    /// Where outer datagrams go: the pinned endpoint, or the last
    /// authenticated source after roaming.
    pub remote: Option<SocketAddr>,
}

/// Counters only; no packet content is kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub generations: u64,
    pub received: u64,
    pub sent: u64,
    /// Outer datagrams the peer refused: framing, authentication, replay.
    pub rejected: u64,
    pub oversized: u64,
    /// Outer datagrams that arrived while no generation was live.
    pub unowned: u64,
    pub send_errors: u64,
    /// Decrypted inner packets that failed IPv4/UDP/TCP validation.
    pub inner_rejected: u64,
    /// Valid inner packets lost to a full queue, in either direction.
    pub inner_dropped: u64,
    pub roamed: u64,
    /// Handshake messages metered while the load was `High`.
    pub high_load: u64,
    /// Type 1/2 datagrams with invalid framing or MAC1, refused before any
    /// Noise work and never metered as load. Also counted in `rejected`.
    pub handshake_invalid: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VirtualUdp {
    generation: u64,
    handle: Handle,
}

impl VirtualUdp {
    pub fn port(&self) -> u16 {
        self.handle.port()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VirtualFlow {
    generation: u64,
    flow: FlowHandle,
}

impl VirtualFlow {
    pub fn local_port(&self) -> u16 {
        self.flow.local_port()
    }

    pub fn remote(&self) -> (Ipv4Addr, u16) {
        self.flow.remote()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VirtualListener {
    generation: u64,
    listener: ListenerHandle,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lookup {
    generation: u64,
    id: QueryId,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pumped {
    pub received: usize,
    pub status: Status,
}

/// Everything one generation owns. Revocation drops it as a whole.
struct Live<E: Entropy> {
    generation: u64,
    peer: Peer<E>,
    udp: UdpSockets,
    tcp: TcpStack<E>,
    dns: Option<DnsClient<E>>,
    results: Vec<Finished>,
    remote: SocketAddr,
    keepalive_at: Option<Duration>,
    next: [Option<Duration>; 3],
}

impl<E: Entropy> Live<E> {
    fn revoke(&mut self) {
        self.peer.revoke();
        self.udp.revoke();
        self.tcp.revoke();
        if let Some(dns) = self.dns.as_mut() {
            dns.revoke();
        }
        self.results.clear();
    }
}

/// Single owner of the outer socket, the clock, load verdicts and the current
/// generation. Not `Send`: a later integration owns it from one thread, so
/// revocation and new operations are serialized by construction.
pub struct Tunnel<E: Entropy + Clone, C: Clock> {
    config: TunnelConfig,
    entropy: E,
    clock: C,
    socket: UdpSocket,
    endpoint: SocketAddr,
    generations: u64,
    live: Option<Live<E>>,
    state: TunnelState,
    reason: Option<Reason>,
    attempt: u32,
    retry_at: Option<Duration>,
    window: Duration,
    handshakes: u32,
    stats: Stats,
}

fn transmit(socket: &UdpSocket, remote: SocketAddr, datagrams: Vec<Vec<u8>>, stats: &mut Stats) {
    for datagram in datagrams {
        match socket.send_to(&datagram, remote) {
            Ok(_) => stats.sent += 1,
            Err(_) => stats.send_errors += 1,
        }
    }
}

impl<E: Entropy + Clone, C: Clock> Tunnel<E, C> {
    /// Validates, resolves the endpoint, binds the one outer socket and starts
    /// the first generation. Nothing else touches the ordinary network.
    pub fn new(config: TunnelConfig, entropy: E, clock: C) -> Result<Self, TunnelError> {
        config.validate()?;
        let endpoint = resolve_endpoint(&config.endpoint)?;
        let socket =
            UdpSocket::bind(config.bind).map_err(|error| TunnelError::Io(error.to_string()))?;
        let window = clock.now();
        let mut tunnel = Self {
            config,
            entropy,
            clock,
            socket,
            endpoint,
            generations: 0,
            live: None,
            state: TunnelState::Connecting,
            reason: None,
            attempt: 0,
            retry_at: None,
            window,
            handshakes: 0,
            stats: Stats::default(),
        };
        tunnel.start();
        Ok(tunnel)
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TunnelError> {
        self.socket
            .local_addr()
            .map_err(|error| TunnelError::Io(error.to_string()))
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn status(&self) -> Status {
        Status {
            state: self.state,
            generation: self.live.as_ref().map(|live| live.generation),
            reason: self.reason,
            attempt: self.attempt,
            retry_at: self.retry_at,
            remote: self.live.as_ref().map(|live| live.remote),
        }
    }

    fn peer_clock(&self, generation: u64, now: Duration) -> PeerClock {
        PeerClock {
            generation,
            now,
            wall: self.clock.wall(),
        }
    }

    /// Builds a complete fresh generation, or schedules the next attempt.
    fn start(&mut self) {
        let now = self.clock.now();
        self.generations += 1;
        let generation = self.generations;
        self.retry_at = None;
        match self.build(generation, now) {
            Ok(live) => {
                self.live = Some(live);
                self.stats.generations += 1;
                self.state = TunnelState::Connecting;
                if self.config.initiate {
                    // An empty payload is the keepalive that confirms the session.
                    let _ = self.encrypt(Vec::new(), now);
                }
                self.observe(now);
            }
            Err(_) => self.fail(Reason::Failed, now),
        }
    }

    fn build(&self, generation: u64, now: Duration) -> Result<Live<E>, TunnelError> {
        let config = &self.config;
        let peer = Peer::new(
            config.secret,
            config.peer_public,
            config.psk,
            self.endpoint,
            self.entropy.clone(),
            config.peer_queue,
            self.peer_clock(generation, now),
        )
        .map_err(TunnelError::Peer)?;
        let tcp = TcpStack::new(config.tcp, self.entropy.clone(), generation, now)
            .map_err(TunnelError::Tcp)?;
        let dns = match &config.dns {
            Some(dns) => Some(
                DnsClient::new(dns.clone(), self.entropy.clone(), generation, now)
                    .map_err(TunnelError::Dns)?,
            ),
            None => None,
        };
        Ok(Live {
            generation,
            peer,
            udp: UdpSockets::new(config.udp, generation),
            tcp,
            dns,
            results: Vec::new(),
            remote: self.endpoint,
            keepalive_at: None,
            next: [None; 3],
        })
    }

    /// Revokes the peer, every virtual socket, flow, listener and pending or
    /// finished lookup of the live generation in one step.
    fn revoke(&mut self) {
        if let Some(mut live) = self.live.take() {
            live.revoke();
        }
    }

    /// Revokes and schedules a bounded, exponentially spaced new generation.
    fn fail(&mut self, reason: Reason, now: Duration) {
        self.revoke();
        self.reason = Some(reason);
        if self.attempt >= self.config.reconnect_attempts {
            self.state = TunnelState::Blocked;
            self.reason = Some(Reason::Exhausted);
            self.retry_at = None;
            return;
        }
        let shift = self.attempt.min(16);
        self.attempt += 1;
        let delay = self
            .config
            .reconnect_initial
            .saturating_mul(1 << shift)
            .min(self.config.reconnect_max);
        self.retry_at = Some(now.saturating_add(delay));
        self.state = TunnelState::Reconnecting;
    }

    /// Whether the session this generation relied on is gone. The peer can
    /// expire its keys and start a new handshake within one call, so a
    /// protected generation that is no longer `ProtectedReady` lost its session
    /// whatever the final status says.
    fn lost(&self, status: PeerStatus) -> bool {
        matches!(
            status,
            PeerStatus::GaveUp | PeerStatus::Expired | PeerStatus::Revoked
        ) || (self.state == TunnelState::Protected && status != PeerStatus::ProtectedReady)
    }

    /// Follows the peer status; any loss of the session revokes the generation.
    fn observe(&mut self, now: Duration) {
        let Some(live) = self.live.as_ref() else {
            return;
        };
        let status = live.peer.status();
        if self.state == TunnelState::Protected
            && matches!(status, PeerStatus::NoSession | PeerStatus::Handshaking)
        {
            self.fail(Reason::Expired, now);
            return;
        }
        let Some(live) = self.live.as_mut() else {
            return;
        };
        match status {
            PeerStatus::ProtectedReady => {
                if self.state != TunnelState::Protected {
                    // No representable full interval remains on overflow. Disable
                    // this timer rather than send early at Duration::MAX.
                    live.keepalive_at = self
                        .config
                        .keepalive
                        .and_then(|every| now.checked_add(every));
                }
                self.state = TunnelState::Protected;
                self.reason = None;
                self.attempt = 0;
            }
            PeerStatus::NoSession | PeerStatus::Handshaking => {
                self.state = TunnelState::Connecting;
            }
            PeerStatus::GaveUp => self.fail(Reason::GaveUp, now),
            PeerStatus::Expired => self.fail(Reason::Expired, now),
            PeerStatus::Revoked => self.fail(Reason::Failed, now),
        }
    }

    /// Publishes one peer output: datagrams to the remote, plaintext to the
    /// stacks. Nothing of an output that lost the session is published; the
    /// generation is revoked instead and `false` is returned. `roam` is true
    /// only for handshake responses and transport data: both are bound to a
    /// fresh session and cannot be replayed, while an initiation can be once a
    /// new generation has forgotten its timestamp fence.
    fn deliver(&mut self, output: PeerOutput, now: Duration, roam: bool) -> bool {
        let Some(live) = self.live.as_ref() else {
            return false;
        };
        if self.lost(live.peer.status()) {
            self.observe(now);
            return false;
        }
        let Some(live) = self.live.as_mut() else {
            return false;
        };
        if roam
            && let Some(source) = output.authenticated_source
            && source != live.remote
        {
            live.remote = source;
            self.stats.roamed += 1;
        }
        live.next[0] = output.next;
        transmit(&self.socket, live.remote, output.datagrams, &mut self.stats);
        for payload in output.inner_payloads {
            dispatch(live, &self.config.ip, payload, now, &mut self.stats);
        }
        true
    }

    /// Encrypts one inner packet. Without a session the peer queues it; a full
    /// queue drops it and nothing ever leaves unencrypted.
    fn encrypt(&mut self, packet: Vec<u8>, now: Duration) -> Result<(), TunnelError> {
        let live = self.live.as_mut().ok_or(TunnelError::Unavailable)?;
        let clock = PeerClock {
            generation: live.generation,
            now,
            wall: self.clock.wall(),
        };
        match live.peer.send(&packet, clock) {
            Ok(output) => {
                if self.deliver(output, now, false) {
                    Ok(())
                } else {
                    Err(TunnelError::Revoked)
                }
            }
            Err(error) => {
                if live.peer.status() == PeerStatus::Revoked {
                    self.observe(now);
                    return Err(TunnelError::Revoked);
                }
                if live.peer.queued_payloads() >= self.config.peer_queue {
                    self.stats.inner_dropped += 1;
                    Err(TunnelError::QueueFull)
                } else {
                    Err(TunnelError::Peer(error))
                }
            }
        }
    }

    /// Weighs one handshake message that passed framing and MAC1: only those
    /// could cost Noise work, so junk cannot hold the runtime under load.
    fn meter(&mut self, now: Duration) -> Load {
        if now.saturating_sub(self.window) >= LOAD_WINDOW {
            self.window = now;
            self.handshakes = 0;
        }
        self.handshakes = self.handshakes.saturating_add(1);
        if self.handshakes > self.config.handshake_load {
            self.stats.high_load += 1;
            Load::High
        } else {
            Load::Normal
        }
    }

    fn inbound(&mut self, packet: &[u8], source: SocketAddr) {
        let now = self.clock.now();
        self.stats.received += 1;
        if packet.len() > MAX_DATAGRAM || packet.is_empty() {
            self.stats.oversized += 1;
            return;
        }
        let wall = self.clock.wall();
        let Some(live) = self.live.as_mut() else {
            self.stats.unowned += 1;
            return;
        };
        let load = if matches!(packet[0], 1 | 2) {
            if live.peer.screen(packet).is_err() {
                self.stats.handshake_invalid += 1;
                self.stats.rejected += 1;
                return;
            }
            self.meter(now)
        } else {
            Load::Normal
        };
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let clock = PeerClock {
            generation: live.generation,
            now,
            wall,
        };
        match live.peer.receive(packet, source, load, clock) {
            Ok(output) => {
                self.deliver(output, now, matches!(packet[0], 2 | 4));
            }
            Err(_) => self.stats.rejected += 1,
        }
        self.observe(now);
    }

    /// Runs every timer at `now`: reconnection, keepalive, TCP, DNS and the
    /// peer, encrypting whatever the stacks emit.
    fn maintain(&mut self, now: Duration) {
        if self.state == TunnelState::Reconnecting && self.retry_at.is_some_and(|at| now >= at) {
            self.start();
        }
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let generation = live.generation;
        let mut packets = Vec::new();
        let mut failed = false;
        match live.tcp.poll(generation, now) {
            Ok(poll) => {
                packets.extend(poll.send);
                live.next[1] = poll.next;
            }
            Err(_) => failed = true,
        }
        if let Some(dns) = live.dns.as_mut() {
            match dns.poll(&mut live.udp, generation, now) {
                Ok(poll) => {
                    packets.extend(poll.send);
                    live.results.extend(poll.finished);
                    live.next[2] = poll.next;
                }
                Err(_) => failed = true,
            }
        }
        if self.state == TunnelState::Protected && live.keepalive_at.is_some_and(|at| now >= at) {
            live.keepalive_at = self
                .config
                .keepalive
                .and_then(|every| now.checked_add(every));
            packets.push(Vec::new());
        }
        if failed {
            self.fail(Reason::Failed, now);
            return;
        }
        for packet in packets {
            if self.encrypt(packet, now) == Err(TunnelError::Revoked) {
                return;
            }
        }
        let wall = self.clock.wall();
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let clock = PeerClock {
            generation: live.generation,
            now,
            wall,
        };
        // A failed poll (entropy) revokes the peer, which `observe` turns into
        // the loss of the whole generation.
        if let Ok(output) = live.peer.poll(clock) {
            self.deliver(output, now, false);
        }
        self.observe(now);
    }

    /// The earliest time at which `pump` has timer work to do.
    pub fn next_deadline(&self) -> Option<Duration> {
        let live = self.live.as_ref();
        let mut deadlines = vec![self.retry_at];
        if let Some(live) = live {
            deadlines.extend(live.next);
            deadlines.push(live.keepalive_at);
        }
        deadlines.into_iter().flatten().min()
    }

    /// Waits at most `max_wait` (and no longer than the next deadline) for
    /// outer datagrams, attempts at most `datagrams_per_pump` reads (including
    /// socket errors), then runs every due timer.
    pub fn pump(&mut self, max_wait: Duration) -> Result<Pumped, TunnelError> {
        self.pump_with_receive(max_wait, |socket, buffer| socket.recv_from(buffer))
    }

    // Keep read accounting testable independently of platform-specific ICMP
    // delivery, while production always reads from the one owned UDP socket.
    fn pump_with_receive(
        &mut self,
        max_wait: Duration,
        mut receive: impl FnMut(&UdpSocket, &mut [u8]) -> std::io::Result<(usize, SocketAddr)>,
    ) -> Result<Pumped, TunnelError> {
        if self.state == TunnelState::Closed {
            return Err(TunnelError::Unavailable);
        }
        let now = self.clock.now();
        self.maintain(now);
        let wait = self
            .next_deadline()
            .map_or(max_wait, |at| at.saturating_sub(now).min(max_wait));
        let mut buffer = [0u8; RECEIVE_BUFFER];
        let mut received = 0;
        for attempt in 0..self.config.datagrams_per_pump {
            let blocking = attempt == 0 && !wait.is_zero();
            let io = |error: std::io::Error| TunnelError::Io(error.to_string());
            self.socket.set_nonblocking(!blocking).map_err(io)?;
            if blocking {
                self.socket.set_read_timeout(Some(wait)).map_err(io)?;
            }
            match receive(&self.socket, &mut buffer) {
                Ok((length, source)) => {
                    received += 1;
                    self.inbound(&buffer[..length], source);
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    break;
                }
                // An ICMP error from an earlier send; the next datagram may be fine.
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset
                    ) =>
                {
                    self.stats.send_errors += 1;
                    if blocking {
                        break;
                    }
                }
                Err(error) => return Err(io(error)),
            }
        }
        let now = self.clock.now();
        self.maintain(now);
        Ok(Pumped {
            received,
            status: self.status(),
        })
    }

    /// Replaces the pinned endpoint: the generation is revoked and a fresh one
    /// starts toward the newly resolved address.
    pub fn set_endpoint(&mut self, endpoint: Endpoint) -> Result<(), TunnelError> {
        if self.state == TunnelState::Closed {
            return Err(TunnelError::Unavailable);
        }
        let address = resolve_endpoint(&endpoint)?;
        self.revoke();
        self.config.endpoint = endpoint;
        self.endpoint = address;
        self.reason = Some(Reason::EndpointChanged);
        self.attempt = 0;
        self.start();
        Ok(())
    }

    /// Stops reconnecting: the generation is revoked and the runtime stays
    /// `Blocked` until `reconnect`.
    pub fn cancel(&mut self) {
        if self.state == TunnelState::Closed {
            return;
        }
        self.revoke();
        self.state = TunnelState::Blocked;
        self.reason = Some(Reason::Cancelled);
        self.retry_at = None;
    }

    /// Leaves `Blocked` with a fresh generation and a fresh attempt budget.
    pub fn reconnect(&mut self) -> Result<(), TunnelError> {
        if self.state != TunnelState::Blocked {
            return Err(TunnelError::Unavailable);
        }
        self.attempt = 0;
        self.start();
        Ok(())
    }

    /// Revokes everything and closes the outer socket. No thread exists.
    pub fn shutdown(mut self) -> Stats {
        self.revoke();
        self.state = TunnelState::Closed;
        self.stats
    }

    fn current(&mut self, generation: u64) -> Result<&mut Live<E>, TunnelError> {
        match self.live.as_mut() {
            Some(live) if live.generation == generation => Ok(live),
            _ => Err(TunnelError::Revoked),
        }
    }

    fn live(&mut self) -> Result<&mut Live<E>, TunnelError> {
        self.live.as_mut().ok_or(TunnelError::Unavailable)
    }

    /// Sends whatever the stacks emit right away rather than at the next pump.
    fn flush(&mut self) {
        let now = self.clock.now();
        self.maintain(now);
    }

    pub fn udp_bind(&mut self, port: u16) -> Result<VirtualUdp, TunnelError> {
        let mut entropy = self.entropy.clone();
        let live = self.live()?;
        let handle = live
            .udp
            .bind(live.generation, port, &mut entropy)
            .map_err(TunnelError::Udp)?;
        Ok(VirtualUdp {
            generation: live.generation,
            handle,
        })
    }

    pub fn udp_connect(
        &mut self,
        socket: VirtualUdp,
        remote: (Ipv4Addr, u16),
    ) -> Result<(), TunnelError> {
        let live = self.current(socket.generation)?;
        live.udp
            .connect(live.generation, socket.handle, remote)
            .map_err(TunnelError::Udp)
    }

    pub fn udp_send_to(
        &mut self,
        socket: VirtualUdp,
        remote: (Ipv4Addr, u16),
        payload: &[u8],
    ) -> Result<(), TunnelError> {
        let (local, mtu) = (self.config.ip.local(), self.config.mtu);
        let live = self.current(socket.generation)?;
        let packet = live
            .udp
            .send(live.generation, socket.handle, local, remote, payload, mtu)
            .map_err(TunnelError::Udp)?;
        let now = self.clock.now();
        self.encrypt(packet, now)
    }

    pub fn udp_recv(&mut self, socket: VirtualUdp) -> Result<Option<Received>, TunnelError> {
        let live = self.current(socket.generation)?;
        live.udp
            .recv(live.generation, socket.handle)
            .map_err(TunnelError::Udp)
    }

    pub fn udp_close(&mut self, socket: VirtualUdp) -> Result<(), TunnelError> {
        let live = self.current(socket.generation)?;
        live.udp
            .close(live.generation, socket.handle)
            .map_err(TunnelError::Udp)
    }

    /// Starts an A lookup through the inner resolvers only.
    pub fn resolve(&mut self, name: &str) -> Result<Lookup, TunnelError> {
        let now = self.clock.now();
        let limit = self.config.dns.as_ref().map_or(0, |dns| dns.max_pending);
        let live = self.live.as_mut().ok_or(TunnelError::Unavailable)?;
        let generation = live.generation;
        let dns = live.dns.as_mut().ok_or(TunnelError::NoDns)?;
        if dns.pending() + live.results.len() >= limit {
            return Err(TunnelError::Busy);
        }
        let (id, packet) = dns
            .start(&mut live.udp, generation, name, now)
            .map_err(TunnelError::Dns)?;
        self.encrypt(packet, now).or_else(|error| match error {
            // The client retries on its own schedule; the lookup stands.
            TunnelError::QueueFull | TunnelError::Peer(_) => Ok(()),
            other => Err(other),
        })?;
        Ok(Lookup { generation, id })
    }

    /// Takes a finished lookup's result; `None` while it is pending.
    pub fn lookup_result(
        &mut self,
        lookup: Lookup,
    ) -> Result<Option<Result<Answer, Failure>>, TunnelError> {
        let live = self.current(lookup.generation)?;
        let index = live.results.iter().position(|done| done.id == lookup.id);
        Ok(index.map(|index| live.results.remove(index).result))
    }

    pub fn cancel_lookup(&mut self, lookup: Lookup) -> Result<(), TunnelError> {
        let live = self.current(lookup.generation)?;
        live.results.retain(|done| done.id != lookup.id);
        if let Some(dns) = live.dns.as_mut() {
            dns.cancel(&mut live.udp, live.generation, lookup.id)
                .map_err(TunnelError::Dns)?;
        }
        Ok(())
    }

    pub fn tcp_connect(&mut self, remote: Ipv4Addr, port: u16) -> Result<VirtualFlow, TunnelError> {
        let now = self.clock.now();
        let live = self.live()?;
        let generation = live.generation;
        let flow = live
            .tcp
            .connect(generation, now, remote, port)
            .map_err(TunnelError::Tcp)?;
        self.flush();
        Ok(VirtualFlow { generation, flow })
    }

    pub fn tcp_listen(&mut self, port: u16) -> Result<VirtualListener, TunnelError> {
        let live = self.live()?;
        let listener = live
            .tcp
            .listen(live.generation, port)
            .map_err(TunnelError::Tcp)?;
        Ok(VirtualListener {
            generation: live.generation,
            listener,
        })
    }

    pub fn tcp_accept(
        &mut self,
        listener: VirtualListener,
    ) -> Result<Option<VirtualFlow>, TunnelError> {
        let live = self.current(listener.generation)?;
        let accepted = live
            .tcp
            .accept(live.generation, listener.listener)
            .map_err(TunnelError::Tcp)?;
        Ok(accepted.map(|flow| VirtualFlow {
            generation: listener.generation,
            flow,
        }))
    }

    pub fn tcp_unlisten(&mut self, listener: VirtualListener) -> Result<(), TunnelError> {
        let live = self.current(listener.generation)?;
        live.tcp
            .unlisten(live.generation, listener.listener)
            .map_err(TunnelError::Tcp)?;
        self.flush();
        Ok(())
    }

    pub fn tcp_state(&mut self, flow: VirtualFlow) -> Result<TcpState, TunnelError> {
        let live = self.current(flow.generation)?;
        live.tcp
            .state(live.generation, flow.flow)
            .map_err(TunnelError::Tcp)
    }

    /// Queues bytes and returns how many fit; zero means the buffer is full.
    pub fn tcp_send(&mut self, flow: VirtualFlow, data: &[u8]) -> Result<usize, TunnelError> {
        let live = self.current(flow.generation)?;
        let taken = live
            .tcp
            .send(live.generation, flow.flow, data)
            .map_err(TunnelError::Tcp)?;
        self.flush();
        Ok(taken)
    }

    pub fn tcp_recv(&mut self, flow: VirtualFlow, buffer: &mut [u8]) -> Result<Read, TunnelError> {
        let live = self.current(flow.generation)?;
        let read = live
            .tcp
            .recv(live.generation, flow.flow, buffer)
            .map_err(TunnelError::Tcp)?;
        // Reading may reopen the window; announce it now.
        self.flush();
        Ok(read)
    }

    pub fn tcp_close(&mut self, flow: VirtualFlow) -> Result<(), TunnelError> {
        let now = self.clock.now();
        let live = self.current(flow.generation)?;
        live.tcp
            .close(live.generation, now, flow.flow)
            .map_err(TunnelError::Tcp)?;
        self.flush();
        Ok(())
    }

    pub fn tcp_abort(&mut self, flow: VirtualFlow) -> Result<(), TunnelError> {
        let live = self.current(flow.generation)?;
        live.tcp
            .abort(live.generation, flow.flow)
            .map_err(TunnelError::Tcp)?;
        self.flush();
        Ok(())
    }
}

/// Routes one authenticated padded inner packet to the UDP table or the TCP
/// stack; each validates the whole packet before anything is delivered.
fn dispatch<E: Entropy>(
    live: &mut Live<E>,
    ip: &Ipv4Config,
    packet: Vec<u8>,
    now: Duration,
    stats: &mut Stats,
) {
    let protocol = match packet.first() {
        Some(first) if first >> 4 == 4 && packet.len() >= 20 => packet[9],
        _ => {
            stats.inner_rejected += 1;
            return;
        }
    };
    match protocol {
        PROTOCOL_UDP => match live.udp.ingest(live.generation, ip, &packet) {
            Ingest::Delivered(Delivery::Queued) => {}
            // A full queue, no bound socket, a filtered source or revocation.
            Ingest::Delivered(_) => stats.inner_dropped += 1,
            Ingest::Ip(_) | Ingest::Udp(_) => stats.inner_rejected += 1,
        },
        PROTOCOL_TCP => match live.tcp.ingest(live.generation, now, &packet) {
            TcpIngest::Ip(_) | TcpIngest::Segment(_) => stats.inner_rejected += 1,
            TcpIngest::Dropped(_) | TcpIngest::Revoked => stats.inner_dropped += 1,
            _ => {}
        },
        crate::netstack::PROTOCOL_ICMP => {
            use crate::netstack::IcmpIngest;
            let tcp = live.tcp.ingest_icmp(live.generation, now, &packet);
            let outcome = if tcp == IcmpIngest::Dropped {
                live.udp.ingest_icmp(live.generation, ip, &packet)
            } else {
                tcp
            };
            match outcome {
                IcmpIngest::Applied(_) => {}
                IcmpIngest::Invalid(_) => stats.inner_rejected += 1,
                IcmpIngest::Dropped | IcmpIngest::Revoked => stats.inner_dropped += 1,
            }
        }
        _ => stats.inner_rejected += 1,
    }
}

#[cfg(test)]
#[path = "tunnel_tests.rs"]
mod tests;
