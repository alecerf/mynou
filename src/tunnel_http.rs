//! Bounded native owner for the non-Send tunnel, its generation-bound HTTP
//! streams and a torrent `Network` (see `network`). The library engine can
//! receive this network explicitly; operator activation remains separate.
use crate::{
    net::{HttpConnector, NetworkStream},
    netstack::{Answer, Failure, Read as TcpRead, Record, TcpState},
    tunnel::{
        Endpoint, Lookup, Status, SystemClock, Tunnel, TunnelConfig, TunnelState, VirtualFlow,
        VirtualListener, VirtualUdp,
    },
    wireguard::OsEntropy,
};
use std::{
    io::{self, Read, Write},
    net::IpAddr,
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const QUEUE: usize = 32;
const BATCH: usize = 8;
const CHUNK: usize = 16 * 1024;
/// The most one read takes from a flow; the rest of it serves later reads
/// without a round trip through the owner.
const READ_AHEAD: usize = 256 * 1024;
const TICK: Duration = Duration::from_millis(2);
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the other address family may still answer once one lookup gave
/// addresses (the resolution delay of RFC 8305 section 3).
const RESOLUTION_DELAY: Duration = Duration::from_millis(50);
const QUEUED: u8 = 0;
const CLAIMED: u8 = 1;
const ABANDONED: u8 = 2;
type Native = Tunnel<OsEntropy, SystemClock>;
type Task = Box<dyn FnOnce(io::Result<&mut Runtime>) + Send>;

fn error(kind: io::ErrorKind) -> io::Error {
    let message = match kind {
        io::ErrorKind::TimedOut => "Tunnel operation deadline exceeded",
        io::ErrorKind::WouldBlock => "Tunnel command queue is full",
        io::ErrorKind::PermissionDenied => "Tunnel generation is unavailable or revoked",
        io::ErrorKind::InvalidInput => "Unsupported tunnel destination or configuration",
        _ => "Tunnel operation failed",
    };
    io::Error::new(kind, message)
}
fn failed(_: crate::tunnel::TunnelError) -> io::Error {
    error(io::ErrorKind::ConnectionAborted)
}
fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| error(io::ErrorKind::TimedOut))
}
fn pause(deadline: Instant) -> io::Result<()> {
    thread::sleep(remaining(deadline)?.min(TICK));
    Ok(())
}

struct Command {
    deadline: Instant,
    generation: Option<u64>,
    task: Task,
}

enum Resource {
    Flow(VirtualFlow),
    Lookup(Lookup),
    Udp(VirtualUdp),
    Listener(VirtualListener),
}
struct Runtime {
    tunnel: Native,
    leases: Vec<(u64, Weak<()>, Resource)>,
}
impl Runtime {
    fn collect(&mut self) {
        let generation = self.tunnel.status().generation;
        self.leases.retain(|(captured, lease, resource)| {
            if generation != Some(*captured) {
                return false;
            }
            if lease.strong_count() != 0 {
                // A retained stream can outlive a flow removed by stack
                // timers. Do not retain an unbounded history of such leases.
                return match resource {
                    Resource::Flow(flow) => self.tunnel.tcp_state(*flow).is_ok(),
                    Resource::Lookup(_) | Resource::Udp(_) | Resource::Listener(_) => true,
                };
            }
            match resource {
                Resource::Flow(flow) => {
                    let _ = self.tunnel.tcp_abort(*flow);
                }
                Resource::Lookup(lookup) => {
                    let _ = self.tunnel.cancel_lookup(*lookup);
                }
                Resource::Udp(socket) => {
                    let _ = self.tunnel.udp_close(*socket);
                }
                Resource::Listener(listener) => {
                    let _ = self.tunnel.tcp_unlisten(*listener);
                }
            }
            false
        });
    }
}

struct Owner {
    identity: u128,
    /// The tunnel carries IPv6: AAAA lookups and IPv6 destinations are used.
    ipv6: bool,
    sender: SyncSender<Command>,
    cancelled: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    publication: Arc<RwLock<()>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Ok(slot) = self.thread.get_mut()
            && let Some(handle) = slot.take()
        {
            let _ = handle.join();
        }
    }
}
impl Owner {
    fn admit(&self, generation: Option<u64>) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire)
            || generation.is_some_and(|g| self.generation.load(Ordering::Acquire) != g)
        {
            return Err(error(io::ErrorKind::PermissionDenied));
        }
        Ok(())
    }
    /// Fail-fast: a full queue is `WouldBlock`.
    fn call<T: Send + 'static>(
        &self,
        generation: Option<u64>,
        deadline: Instant,
        action: impl FnOnce(&mut Runtime) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        self.dispatch(generation, deadline, false, action)
    }
    /// Stream operations wait for queue space until `deadline`, pausing
    /// between attempts, so saturated callers do not spin on `WouldBlock`.
    fn wait<T: Send + 'static>(
        &self,
        generation: Option<u64>,
        deadline: Instant,
        action: impl FnOnce(&mut Runtime) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        self.dispatch(generation, deadline, true, action)
    }
    fn dispatch<T: Send + 'static>(
        &self,
        generation: Option<u64>,
        deadline: Instant,
        patient: bool,
        action: impl FnOnce(&mut Runtime) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        remaining(deadline)?;
        self.admit(generation)?;
        let (send, recv) = mpsc::sync_channel(1);
        let epoch = self.generation.clone();
        let publication = self.publication.clone();
        // QUEUED until the owner claims it or the caller gives up. A claimed
        // action may consume or accept stream bytes, so its result is always
        // returned: dropping it would lose reads or duplicate retried writes.
        let claim = Arc::new(AtomicU8::new(QUEUED));
        let owner_claim = claim.clone();
        let mut command = Command {
            deadline,
            generation,
            task: Box::new(move |runtime| {
                let runtime = runtime.and_then(|runtime| {
                    owner_claim
                        .compare_exchange(QUEUED, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
                        .map(|_| runtime)
                        .map_err(|_| error(io::ErrorKind::TimedOut))
                });
                let result = runtime.and_then(|runtime| {
                    let result = action(runtime);
                    // An operation can itself observe expiry in flush().
                    // Publish that loss before making its result visible.
                    publish(runtime, &epoch, &publication);
                    if generation.is_some_and(|g| epoch.load(Ordering::Acquire) != g) {
                        Err(error(io::ErrorKind::PermissionDenied))
                    } else {
                        result
                    }
                });
                let _ = send.send(result);
            }),
        };
        loop {
            match self.sender.try_send(command) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(returned)) if patient => {
                    pause(deadline)?;
                    self.admit(generation)?;
                    command = returned;
                }
                Err(mpsc::TrySendError::Full(_)) => return Err(error(io::ErrorKind::WouldBlock)),
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err(error(io::ErrorKind::ConnectionAborted));
                }
            }
        }
        let wait = remaining(deadline).unwrap_or(Duration::ZERO);
        let result = match recv.recv_timeout(wait) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if claim
                    .compare_exchange(QUEUED, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Err(error(io::ErrorKind::TimedOut));
                }
                // Claimed: owner actions are short and non-blocking.
                recv.recv()
                    .map_err(|_| error(io::ErrorKind::ConnectionAborted))?
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(error(io::ErrorKind::ConnectionAborted));
            }
        };
        self.admit(generation)?;
        result
    }
}

fn publish(runtime: &Runtime, generation: &AtomicU64, publication: &RwLock<()>) {
    let status = runtime.tunnel.status();
    let next = if status.state == TunnelState::Protected {
        status.generation.unwrap_or(0)
    } else {
        0
    };
    if generation.swap(next, Ordering::AcqRel) != next {
        // Close admission first, then wait for already admitted local commits.
        // No new commands are dispatched while an old publication completes.
        let _fence = publication.write().unwrap_or_else(|e| e.into_inner());
    }
}
fn run(
    mut runtime: Runtime,
    receiver: Receiver<Command>,
    cancelled: &AtomicBool,
    generation: &AtomicU64,
    publication: &RwLock<()>,
) {
    // A full batch likely left commands queued: then do not wait for datagrams.
    let mut busy = false;
    while !cancelled.load(Ordering::Acquire) {
        runtime.collect();
        if runtime
            .tunnel
            .pump(if busy { Duration::ZERO } else { TICK })
            .is_err()
        {
            break;
        }
        publish(&runtime, generation, publication);
        busy = true;
        for _ in 0..BATCH {
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            let command = match receiver.try_recv() {
                Ok(command) => command,
                Err(TryRecvError::Empty) => {
                    busy = false;
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    cancelled.store(true, Ordering::Release);
                    break;
                }
            };
            // Observe timers/session loss before each command, even under load.
            let pumped = runtime.tunnel.pump(Duration::ZERO).map_err(failed);
            publish(&runtime, generation, publication);
            let admission = pumped
                .and_then(|_| remaining(command.deadline).map(|_| ()))
                .and_then(|_| {
                    if cancelled.load(Ordering::Acquire)
                        || command
                            .generation
                            .is_some_and(|g| generation.load(Ordering::Acquire) != g)
                    {
                        Err(error(io::ErrorKind::PermissionDenied))
                    } else {
                        Ok(())
                    }
                });
            (command.task)(admission.map(|_| &mut runtime));
        }
    }
    cancelled.store(true, Ordering::Release);
    generation.store(0, Ordering::Release);
    let _fence = publication.write().unwrap_or_else(|e| e.into_inner());
    runtime.tunnel.cancel();
    // Dropping the receiver wakes pending callers; dropping the tunnel closes
    // its sole outer socket. No task or inner I/O can select a direct transport.
}

/// Library driver. A connector captures one already-protected generation.
/// Keeping a connector/stream alive keeps its owner alive; cancel is permanent.
#[derive(Clone)]
pub struct HttpTunnel {
    owner: Arc<Owner>,
}
impl std::fmt::Debug for HttpTunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTunnel").finish_non_exhaustive()
    }
}
impl HttpTunnel {
    /// Starts the owner with a literal IPv4 outer endpoint. This creates no
    /// host DNS lookup; readiness is checked separately by `connector`.
    pub fn start(config: TunnelConfig) -> io::Result<Self> {
        if !matches!(config.endpoint, Endpoint::Address(a) if a.is_ipv4()) {
            return Err(error(io::ErrorKind::InvalidInput));
        }
        let ipv6 = config.ip.local_ipv6().is_some();
        let identity = u128::from_be_bytes(
            crate::crypto::random_bytes::<16>().map_err(|_| error(io::ErrorKind::Other))?,
        );
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        let cancelled = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let publication = Arc::new(RwLock::new(()));
        let (ready, started) = mpsc::sync_channel(1);
        let stop = cancelled.clone();
        let epoch = generation.clone();
        let fence = publication.clone();
        let handle = thread::Builder::new()
            .name("mynou-tunnel-http".into())
            .spawn(move || {
                // Tunnel contains Rc: construct and destroy it entirely here.
                let tunnel = match Native::new(config, OsEntropy, SystemClock::new()) {
                    Ok(tunnel) => tunnel,
                    Err(_) => {
                        let _ = ready.send(Err(error(io::ErrorKind::InvalidInput)));
                        return;
                    }
                };
                if ready.send(Ok(())).is_ok() {
                    run(
                        Runtime {
                            tunnel,
                            leases: Vec::new(),
                        },
                        receiver,
                        &stop,
                        &epoch,
                        &fence,
                    );
                }
            })
            .map_err(|_| error(io::ErrorKind::Other))?;
        let owner = Arc::new(Owner {
            identity,
            ipv6,
            sender,
            cancelled,
            generation,
            publication,
            thread: Mutex::new(Some(handle)),
        });
        started
            .recv_timeout(START_TIMEOUT)
            .map_err(|_| error(io::ErrorKind::TimedOut))??;
        Ok(Self { owner })
    }
    /// Redacted operator telemetry, queried on the owner thread.
    pub fn telemetry(
        &self,
        deadline: Instant,
    ) -> io::Result<(Status, crate::tunnel::Stats, Option<Duration>)> {
        self.owner.call(None, deadline, |r| {
            Ok((
                r.tunnel.status(),
                r.tunnel.stats(),
                r.tunnel.handshake_age(),
            ))
        })
    }
    pub fn status(&self, deadline: Instant) -> io::Result<Status> {
        self.owner.call(None, deadline, |r| Ok(r.tunnel.status()))
    }
    /// Fail immediately until protected. A stale connector never switches to a
    /// new generation automatically; obtain a new one explicitly after recovery.
    pub fn connector(&self, deadline: Instant) -> io::Result<Arc<dyn HttpConnector>> {
        Ok(Arc::new(self.protected(deadline)?))
    }
    /// The torrent network of the currently protected generation, with the
    /// same admission as `connector`; see `docs/tunnel-network.md`.
    pub fn network(
        &self,
        deadline: Instant,
    ) -> io::Result<Arc<dyn crate::torrent::transport::Network>> {
        Ok(Arc::new(network::TunnelNetwork::new(
            self.protected(deadline)?,
        )))
    }
    fn protected(&self, deadline: Instant) -> io::Result<Connector> {
        let status = self.status(deadline)?;
        let generation = status
            .generation
            .filter(|_| status.state == TunnelState::Protected)
            .ok_or_else(|| error(io::ErrorKind::PermissionDenied))?;
        self.owner.admit(Some(generation))?;
        Ok(Connector {
            owner: self.owner.clone(),
            generation,
        })
    }
    /// Close admission immediately and wait for admitted local publications,
    /// then let the owner revoke and close the tunnel. Do not call this while
    /// holding a `Network::publication` lease on the same thread.
    pub fn cancel(&self) {
        self.owner.cancelled.store(true, Ordering::Release);
        self.owner.generation.store(0, Ordering::Release);
        let _fence = self
            .owner
            .publication
            .write()
            .unwrap_or_else(|e| e.into_inner());
    }
}

struct Connector {
    owner: Arc<Owner>,
    generation: u64,
}
impl Connector {
    /// IPv4 addresses first, then IPv6 ones when the tunnel carries IPv6.
    /// Both lookups run at once; once one gives addresses, the other has
    /// `RESOLUTION_DELAY` more to answer, so a resolver that drops AAAA
    /// queries does not delay IPv4.
    fn resolve(&self, host: &str, deadline: Instant) -> io::Result<Vec<IpAddr>> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return match ip {
                ip if usable(ip) && (ip.is_ipv4() || self.owner.ipv6) => Ok(vec![ip]),
                _ => Err(error(io::ErrorKind::InvalidInput)),
            };
        }
        // Reject IPv6 literals in other forms (bracketed, scoped) and
        // malformed names before creating a virtual DNS query.
        if host.is_empty() || host.len() > 253 || host.contains([':', '[', ']']) {
            return Err(error(io::ErrorKind::InvalidInput));
        }
        let lease = Arc::new(());
        let weak = Arc::downgrade(&lease);
        let name = host.to_owned();
        let records = if self.owner.ipv6 {
            vec![Record::A, Record::Aaaa]
        } else {
            vec![Record::A]
        };
        let lookups = self.owner.wait(Some(self.generation), deadline, move |r| {
            let mut lookups = Vec::new();
            for record in records {
                let lookup = r.tunnel.resolve_record(&name, record).map_err(failed)?;
                r.leases.push((
                    r.tunnel.status().generation.unwrap_or(0),
                    weak.clone(),
                    Resource::Lookup(lookup),
                ));
                lookups.push(lookup);
            }
            Ok(lookups)
        })?;
        let mut results: Vec<Option<Result<Answer, Failure>>> =
            lookups.iter().map(|_| None).collect();
        let mut settle: Option<Instant> = None;
        while results.iter().any(Option::is_none) && settle.is_none_or(|at| Instant::now() < at) {
            let open: Vec<_> = lookups
                .iter()
                .zip(&results)
                .filter(|(_, result)| result.is_none())
                .map(|(lookup, _)| *lookup)
                .collect();
            let done = self.owner.wait(Some(self.generation), deadline, move |r| {
                let mut done = Vec::new();
                for lookup in open {
                    if let Some(result) = r.tunnel.lookup_result(lookup).map_err(failed)? {
                        r.leases.retain(|(_, _, resource)| !matches!(resource, Resource::Lookup(id) if *id == lookup));
                        done.push((lookup, result));
                    }
                }
                Ok(done)
            })?;
            for (lookup, result) in done {
                if result.is_ok() {
                    settle.get_or_insert_with(|| Instant::now() + RESOLUTION_DELAY);
                }
                if let Some(index) = lookups.iter().position(|known| *known == lookup) {
                    results[index] = Some(result);
                }
            }
            if results.iter().any(Option::is_none) {
                pause(deadline)?;
            }
        }
        // A lookup still open here is cancelled when `lease` is dropped.
        let answers: Vec<_> = results.into_iter().flatten().collect();
        if answers.iter().all(Result::is_err) {
            return Err(error(io::ErrorKind::NotFound));
        }
        let addresses: Vec<_> = answers
            .into_iter()
            .flatten()
            .flat_map(|answer| answer.addresses)
            .filter(|ip| usable(*ip))
            .collect();
        if addresses.is_empty() {
            return Err(error(io::ErrorKind::InvalidInput));
        }
        Ok(addresses)
    }
    fn open(&self, address: IpAddr, port: u16, deadline: Instant) -> io::Result<Stream> {
        let lease = Arc::new(());
        let weak = Arc::downgrade(&lease);
        let flow = self.owner.wait(Some(self.generation), deadline, move |r| {
            let flow = r.tunnel.tcp_connect(address, port).map_err(failed)?;
            r.leases.push((
                r.tunnel.status().generation.unwrap_or(0),
                weak,
                Resource::Flow(flow),
            ));
            Ok(flow)
        })?;
        let stream = Stream::new(self, flow, lease);
        loop {
            let state = self.owner.wait(Some(self.generation), deadline, move |r| {
                r.tunnel.tcp_state(flow).map_err(failed)
            })?;
            match state {
                TcpState::Established => return Ok(stream),
                TcpState::SynSent | TcpState::SynReceived => pause(deadline)?,
                _ => return Err(error(io::ErrorKind::ConnectionAborted)),
            }
        }
    }
}
// LAN addresses travel only inside the tunnel. Loopback/link-local and unusable
// addresses are rejected, independent of literal versus virtual DNS source.
// IPv4-mapped and -compatible IPv6 forms would bypass the IPv4 rules.
fn usable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_multicast()
                && ip.octets()[0] != 0
                && ip.octets()[0] < 240
        }
        IpAddr::V6(ip) => {
            !ip.is_multicast()
                && !ip.is_unicast_link_local()
                && ip.to_ipv4_mapped().is_none()
                && ip.segments()[..6] != [0; 6]
        }
    }
}
impl HttpConnector for Connector {
    fn connect(
        &self,
        host: &str,
        port: u16,
        deadline: Instant,
    ) -> crate::Result<Box<dyn NetworkStream>> {
        let connect = || -> io::Result<Box<dyn NetworkStream>> {
            remaining(deadline)?;
            self.owner.admit(Some(self.generation))?;
            if port == 0 {
                return Err(error(io::ErrorKind::InvalidInput));
            }
            let addresses = self.resolve(host, deadline)?;
            let mut last = error(io::ErrorKind::ConnectionAborted);
            for address in addresses {
                match self.open(address, port, deadline) {
                    Ok(stream) => return Ok(Box::new(stream)),
                    Err(e) => last = e,
                }
            }
            Err(last)
        };
        connect().map_err(|e| e.to_string())
    }
}

struct Stream {
    owner: Arc<Owner>,
    generation: u64,
    flow: VirtualFlow,
    _lease: Arc<()>,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
    /// Bytes already taken from the flow, from `offset` on.
    received: Vec<u8>,
    offset: usize,
}
impl Stream {
    fn new(connector: &Connector, flow: VirtualFlow, lease: Arc<()>) -> Self {
        Self {
            owner: connector.owner.clone(),
            generation: connector.generation,
            flow,
            _lease: lease,
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
            received: Vec::new(),
            offset: 0,
        }
    }
    fn deadline(timeout: Option<Duration>) -> io::Result<Instant> {
        Instant::now()
            .checked_add(timeout.unwrap_or(Duration::from_secs(20)))
            .ok_or_else(|| error(io::ErrorKind::InvalidInput))
    }
}
impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.owner.admit(Some(self.generation))?;
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.offset < self.received.len() {
            let n = buffer.len().min(self.received.len() - self.offset);
            buffer[..n].copy_from_slice(&self.received[self.offset..self.offset + n]);
            self.offset += n;
            return Ok(n);
        }
        let deadline = Self::deadline(
            *self
                .read_timeout
                .lock()
                .map_err(|_| error(io::ErrorKind::Other))?,
        )?;
        let flow = self.flow;
        loop {
            let (read, data) = self.owner.wait(Some(self.generation), deadline, move |r| {
                let ready = r.tunnel.tcp_readable(flow).map_err(failed)?;
                let mut data = vec![0; ready.clamp(1, READ_AHEAD)];
                let read = r.tunnel.tcp_recv(flow, &mut data).map_err(failed)?;
                if let TcpRead::Data(n) = read {
                    data.truncate(n);
                } else {
                    data.clear();
                }
                Ok((read, data))
            })?;
            match read {
                TcpRead::Data(_) => {
                    let n = buffer.len().min(data.len());
                    buffer[..n].copy_from_slice(&data[..n]);
                    self.received = data;
                    self.offset = n;
                    return Ok(n);
                }
                TcpRead::Eof => return Ok(0),
                TcpRead::WouldBlock => pause(deadline)?,
            }
        }
    }
}
impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.owner.admit(Some(self.generation))?;
        if buffer.is_empty() {
            return Ok(0);
        }
        let deadline = Self::deadline(
            *self
                .write_timeout
                .lock()
                .map_err(|_| error(io::ErrorKind::Other))?,
        )?;
        let flow = self.flow;
        let bytes = &buffer[..buffer.len().min(CHUNK)];
        loop {
            let data = bytes.to_vec();
            let sent = self.owner.wait(Some(self.generation), deadline, move |r| {
                r.tunnel.tcp_send(flow, &data).map_err(failed)
            })?;
            if sent != 0 {
                return Ok(sent);
            }
            pause(deadline)?;
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        // tcp_send queues bytes like an OS TCP send; pump drives transmission.
        // There is no additional userspace stream buffer to flush.
        let flow = self.flow;
        self.owner.wait(
            Some(self.generation),
            Self::deadline(
                *self
                    .write_timeout
                    .lock()
                    .map_err(|_| error(io::ErrorKind::Other))?,
            )?,
            move |r| r.tunnel.tcp_state(flow).map_err(failed).map(|_| ()),
        )
    }
}
impl NetworkStream for Stream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        set_timeout(&self.read_timeout, timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        set_timeout(&self.write_timeout, timeout)
    }
}

fn set_timeout(slot: &Mutex<Option<Duration>>, timeout: Option<Duration>) -> io::Result<()> {
    if timeout.is_some_and(|d| d.is_zero()) {
        return Err(error(io::ErrorKind::InvalidInput));
    }
    *slot.lock().map_err(|_| error(io::ErrorKind::Other))? = timeout;
    Ok(())
}

#[path = "tunnel_network.rs"]
mod network;

#[cfg(test)]
#[path = "tunnel_http_tests.rs"]
pub(crate) mod tests;
