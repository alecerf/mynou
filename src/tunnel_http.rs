//! Bounded native owner for the non-Send tunnel, its generation-bound HTTP
//! streams and a torrent `Network` (see `network`). Library only: production
//! torrent traffic is not wired to this adapter.
use crate::{
    net::{HttpConnector, NetworkStream},
    netstack::{Read as TcpRead, TcpState},
    tunnel::{
        Endpoint, Lookup, Status, SystemClock, Tunnel, TunnelConfig, TunnelState, VirtualFlow,
        VirtualListener, VirtualUdp,
    },
    wireguard::OsEntropy,
};
use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const QUEUE: usize = 32;
const BATCH: usize = 8;
const CHUNK: usize = 16 * 1024;
const TICK: Duration = Duration::from_millis(2);
const START_TIMEOUT: Duration = Duration::from_secs(5);
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
    sender: SyncSender<Command>,
    cancelled: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
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
    fn call<T: Send + 'static>(
        &self,
        generation: Option<u64>,
        deadline: Instant,
        action: impl FnOnce(&mut Runtime) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        remaining(deadline)?;
        self.admit(generation)?;
        let (send, recv) = mpsc::sync_channel(1);
        let epoch = self.generation.clone();
        self.sender
            .try_send(Command {
                deadline,
                generation,
                task: Box::new(move |runtime| {
                    let result = runtime.and_then(|runtime| {
                        let result = action(runtime);
                        // An operation can itself observe expiry in flush().
                        // Publish that loss before making its result visible.
                        publish(runtime, &epoch);
                        if generation.is_some_and(|g| epoch.load(Ordering::Acquire) != g) {
                            Err(error(io::ErrorKind::PermissionDenied))
                        } else {
                            result
                        }
                    });
                    let _ = send.send(result);
                }),
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => error(io::ErrorKind::WouldBlock),
                mpsc::TrySendError::Disconnected(_) => error(io::ErrorKind::ConnectionAborted),
            })?;
        let result = recv
            .recv_timeout(remaining(deadline)?)
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => error(io::ErrorKind::TimedOut),
                mpsc::RecvTimeoutError::Disconnected => error(io::ErrorKind::ConnectionAborted),
            })?;
        remaining(deadline)?;
        self.admit(generation)?;
        result
    }
}

fn publish(runtime: &Runtime, generation: &AtomicU64) {
    let status = runtime.tunnel.status();
    generation.store(
        if status.state == TunnelState::Protected {
            status.generation.unwrap_or(0)
        } else {
            0
        },
        Ordering::Release,
    );
}
fn run(
    mut runtime: Runtime,
    receiver: Receiver<Command>,
    cancelled: &AtomicBool,
    generation: &AtomicU64,
) {
    while !cancelled.load(Ordering::Acquire) {
        runtime.collect();
        if runtime.tunnel.pump(TICK).is_err() {
            break;
        }
        publish(&runtime, generation);
        for _ in 0..BATCH {
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            let command = match receiver.try_recv() {
                Ok(command) => command,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    cancelled.store(true, Ordering::Release);
                    break;
                }
            };
            // Observe timers/session loss before each command, even under load.
            let pumped = runtime.tunnel.pump(Duration::ZERO).map_err(failed);
            publish(&runtime, generation);
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
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        let cancelled = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let (ready, started) = mpsc::sync_channel(1);
        let stop = cancelled.clone();
        let epoch = generation.clone();
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
                    );
                }
            })
            .map_err(|_| error(io::ErrorKind::Other))?;
        let owner = Arc::new(Owner {
            sender,
            cancelled,
            generation,
            thread: Mutex::new(Some(handle)),
        });
        started
            .recv_timeout(START_TIMEOUT)
            .map_err(|_| error(io::ErrorKind::TimedOut))??;
        Ok(Self { owner })
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
    /// Fence callers immediately; the owner then revokes and closes the tunnel.
    pub fn cancel(&self) {
        self.owner.cancelled.store(true, Ordering::Release);
        self.owner.generation.store(0, Ordering::Release);
    }
}

struct Connector {
    owner: Arc<Owner>,
    generation: u64,
}
impl Connector {
    fn resolve(&self, host: &str, deadline: Instant) -> io::Result<Vec<Ipv4Addr>> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return match ip {
                IpAddr::V4(ip) if usable(ip) => Ok(vec![ip]),
                _ => Err(error(io::ErrorKind::InvalidInput)),
            };
        }
        // Reject IPv6 literals (including bracketed and scoped forms) and
        // malformed names before creating a virtual DNS query.
        if host.is_empty() || host.len() > 253 || host.contains([':', '[', ']']) {
            return Err(error(io::ErrorKind::InvalidInput));
        }
        let lease = Arc::new(());
        let weak = Arc::downgrade(&lease);
        let name = host.to_owned();
        let lookup = self.owner.call(Some(self.generation), deadline, move |r| {
            let lookup = r.tunnel.resolve(&name).map_err(failed)?;
            r.leases.push((
                r.tunnel.status().generation.unwrap_or(0),
                weak,
                Resource::Lookup(lookup),
            ));
            Ok(lookup)
        })?;
        loop {
            let result = self.owner.call(Some(self.generation), deadline, move |r| {
                let result = r.tunnel.lookup_result(lookup).map_err(failed)?;
                if result.is_some() {
                    r.leases.retain(|(_, _, resource)| !matches!(resource, Resource::Lookup(id) if *id == lookup));
                }
                Ok(result)
            })?;
            if let Some(result) = result {
                let answer = result.map_err(|_| error(io::ErrorKind::NotFound))?;
                let addresses: Vec<_> = answer
                    .addresses
                    .into_iter()
                    .filter(|ip| usable(*ip))
                    .collect();
                if addresses.is_empty() {
                    return Err(error(io::ErrorKind::InvalidInput));
                }
                return Ok(addresses);
            }
            pause(deadline)?;
        }
    }
    fn open(&self, address: Ipv4Addr, port: u16, deadline: Instant) -> io::Result<Stream> {
        let lease = Arc::new(());
        let weak = Arc::downgrade(&lease);
        let flow = self.owner.call(Some(self.generation), deadline, move |r| {
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
            let state = self.owner.call(Some(self.generation), deadline, move |r| {
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
// IPv4 are rejected, independent of literal versus virtual DNS source.
fn usable(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_multicast()
        && ip.octets()[0] != 0
        && ip.octets()[0] < 240
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
        let deadline = Self::deadline(
            *self
                .read_timeout
                .lock()
                .map_err(|_| error(io::ErrorKind::Other))?,
        )?;
        let flow = self.flow;
        let max = buffer.len().min(CHUNK);
        loop {
            let (read, data) = self.owner.call(Some(self.generation), deadline, move |r| {
                let mut data = vec![0; max];
                let read = r.tunnel.tcp_recv(flow, &mut data).map_err(failed)?;
                if let TcpRead::Data(n) = read {
                    data.truncate(n);
                } else {
                    data.clear();
                }
                Ok((read, data))
            })?;
            match read {
                TcpRead::Data(n) => {
                    buffer[..n].copy_from_slice(&data);
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
            let sent = self.owner.call(Some(self.generation), deadline, move |r| {
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
        self.owner.call(
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
mod tests;
