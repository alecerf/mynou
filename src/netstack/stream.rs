//! Offline virtual TCP: a bounded flow table with client connect and listeners
//! over the strict inner IPv4/TCP codecs. Every call carries the caller's captured
//! generation (and, when time matters, a monotonic elapsed time); a different
//! generation or a clock that moves backwards irreversibly revokes the stack and
//! drops every flow, listener and buffered byte. There is no I/O, thread or clock
//! read here: the caller moves inner datagrams in with [`TcpStack::ingest`] and
//! out with [`TcpStack::poll`].

use super::icmp::{self, Effect as IcmpEffect, Ingest as IcmpIngest};
use super::ipv4::{self, BuildError, Ipv4Config, PROTOCOL_TCP, PacketError, check_mtu, unroutable};
use super::tcp::{self, ACK, DEFAULT_PEER_MSS, FIN, Header, PSH, RST, SYN, Segment, SegmentError};
use super::{EPHEMERAL_START, Fence};
use crate::wireguard::Entropy;
use std::collections::{BTreeMap, VecDeque};
use std::net::Ipv4Addr;
use std::time::Duration;

const EPHEMERAL_COUNT: u16 = 16384;
const PORT_ATTEMPTS: usize = 32;
const MAX_WINDOW: usize = 65535;
/// IPv4 and TCP headers without options.
const HEADERS: usize = 40;

/// Explicit bounds. Every queue, table and timer here is limited by one of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TcpConfig {
    pub ip: Ipv4Config,
    /// The inner datagram budget; the MSS is `mtu - 40`.
    pub mtu: usize,
    pub flows: usize,
    pub listeners: usize,
    /// Half-open plus not yet accepted flows per listener.
    pub backlog: usize,
    /// Unacknowledged plus unsent bytes per flow.
    pub send_buffer: usize,
    /// Unread bytes per flow; also the largest advertised window (65535).
    pub receive_buffer: usize,
    /// Send, receive and out-of-order bytes over all flows.
    pub total_buffer: usize,
    /// Out-of-order segments kept per flow.
    pub reorder_segments: usize,
    /// Segments one flow may emit per poll.
    pub burst: usize,
    /// Queued control packets (RST, challenge answers) between polls.
    pub outbox: usize,
    pub initial_rto: Duration,
    pub max_rto: Duration,
    /// Retransmissions of unacknowledged data before the flow fails.
    pub retries: u32,
    /// SYN or SYN-ACK retransmissions before the handshake fails.
    pub syn_retries: u32,
    pub time_wait: Duration,
    /// How long a closed flow stays readable before it is released.
    pub linger: Duration,
    /// How long a zero window may stall queued data.
    pub persist_limit: Duration,
}

impl TcpConfig {
    pub fn new(ip: Ipv4Config, mtu: usize) -> Self {
        Self {
            ip,
            mtu,
            flows: 64,
            listeners: 8,
            backlog: 16,
            send_buffer: 65536,
            receive_buffer: 65535,
            total_buffer: 4 * 1024 * 1024,
            reorder_segments: 16,
            burst: 16,
            outbox: 64,
            initial_rto: Duration::from_secs(1),
            max_rto: Duration::from_secs(60),
            retries: 8,
            syn_retries: 5,
            time_wait: Duration::from_secs(30),
            linger: Duration::from_secs(30),
            persist_limit: Duration::from_secs(120),
        }
    }

    fn mss(&self) -> usize {
        self.mtu - HEADERS
    }

    fn validate(&self) -> Result<(), TcpError> {
        let hour = Duration::from_secs(3600);
        let timer = |value: Duration| !value.is_zero() && value <= hour;
        let valid = check_mtu(self.mtu).is_ok()
            && (1..=1024).contains(&self.flows)
            && (1..=64).contains(&self.listeners)
            && (1..=1024).contains(&self.backlog)
            && (1024..=1 << 20).contains(&self.send_buffer)
            && (1024..=MAX_WINDOW).contains(&self.receive_buffer)
            && (self.send_buffer + self.receive_buffer..=1 << 30).contains(&self.total_buffer)
            && self.reorder_segments <= 64
            && (1..=64).contains(&self.burst)
            && (1..=1024).contains(&self.outbox)
            && timer(self.initial_rto)
            && self.initial_rto >= Duration::from_secs(1)
            && timer(self.max_rto)
            && self.max_rto >= Duration::from_secs(60)
            && self.initial_rto <= self.max_rto
            && (1..=16).contains(&self.retries)
            && (1..=8).contains(&self.syn_retries)
            && timer(self.time_wait)
            && timer(self.linger)
            && timer(self.persist_limit);
        if valid { Ok(()) } else { Err(TcpError::Config) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
    /// Finished or failed. Buffered received bytes stay readable until released.
    Closed,
}

/// Why a flow ended abnormally.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Failure {
    /// The peer sent an acceptable RST.
    Reset,
    /// The peer answered the SYN with an RST.
    Refused,
    /// Retransmission, handshake or zero-window limits were exhausted.
    Timeout,
    /// Validated ICMP during an outstanding active open.
    Unreachable(u8),
}

#[derive(PartialEq, Eq, Debug)]
pub enum TcpError {
    Config,
    Revoked,
    /// A zero port, or a remote that is unroutable, this host or outside the
    /// allowed network.
    Endpoint,
    FlowsFull,
    ListenersFull,
    PortInUse,
    PortsExhausted,
    /// The handle is released, unknown or from a recycled flow.
    Closed,
    /// The handshake has not completed.
    NotConnected,
    /// The local side already shut down sending.
    Shutdown,
    Failed(Failure),
    Entropy(String),
}

/// A flow token. Releasing a flow invalidates every copy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FlowHandle {
    key: Key,
    serial: u64,
}

impl FlowHandle {
    pub fn local_port(&self) -> u16 {
        self.key.0
    }

    pub fn remote(&self) -> (Ipv4Addr, u16) {
        (self.key.1, self.key.2)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ListenerHandle {
    port: u16,
    serial: u64,
}

impl ListenerHandle {
    pub fn port(&self) -> u16 {
        self.port
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum Read {
    Data(usize),
    WouldBlock,
    /// The peer finished sending and everything was read.
    Eof,
}

/// Why a valid segment changed nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Discard {
    Backlog,
    TableFull,
    Entropy,
    /// Outside the receive window; acknowledged unless it was an RST.
    Window,
    /// An unacceptable acknowledgment.
    Ack,
    /// A segment the current state does not use.
    State,
    /// An RST that was not exactly at the next expected sequence number.
    Reset,
}

#[derive(PartialEq, Eq, Debug)]
pub enum Ingest {
    /// The segment reached a flow.
    Delivered,
    /// A SYN created a half-open flow on a listener.
    Handshake,
    /// The flow was reset by an acceptable RST.
    Reset,
    /// A segment for no flow was answered with an RST.
    Answered,
    Dropped(Discard),
    Ip(PacketError),
    Segment(SegmentError),
    Revoked,
}

/// Packets to send and the time at which polling again can change something.
#[derive(PartialEq, Eq, Debug, Default)]
pub struct Poll {
    pub send: Vec<Vec<u8>>,
    pub next: Option<Duration>,
}

/// (local port, remote address, remote port)
type Key = (u16, Ipv4Addr, u16);

struct Listener {
    serial: u64,
    ready: VecDeque<(Key, u64)>,
}

struct Flow {
    serial: u64,
    /// The listening port that created the flow, until it is accepted.
    listener: Option<u16>,
    accepted: bool,
    state: State,
    failure: Option<Failure>,
    /// The effective send MSS: the peer's, capped by ours.
    mss: usize,
    path_mtu: usize,
    network_error: Option<u8>,
    iss: u32,
    irs: u32,
    snd_una: u32,
    snd_nxt: u32,
    /// The highest sequence number ever sent; `snd_nxt` rewinds on retransmission.
    snd_max: u32,
    snd_wnd: u32,
    wl1: u32,
    wl2: u32,
    rcv_nxt: u32,
    /// Bytes from `snd_una`: unacknowledged first, then unsent.
    send: VecDeque<u8>,
    recv: VecDeque<u8>,
    reorder: Vec<(u32, Vec<u8>)>,
    close_requested: bool,
    /// The sequence number of our FIN, fixed when sending is shut down.
    fin_seq: Option<u32>,
    fin_received: bool,
    ack_due: bool,
    syn_pending: bool,
    probe: bool,
    cwnd: usize,
    ssthresh: usize,
    dup_acks: u32,
    rto: Duration,
    srtt: Option<Duration>,
    rttvar: Duration,
    sample: Option<(u32, Duration)>,
    /// Karn: no measurement until all possibly retransmitted bytes settle.
    ambiguous_until: Option<u32>,
    deadline: Option<Duration>,
    retries: u32,
    stalled: Option<Duration>,
    expires: Option<Duration>,
}

impl Flow {
    /// A half-open inbound flow that nobody can hold yet.
    fn removable(&self) -> bool {
        self.state == State::Closed && self.listener.is_some() && !self.accepted
    }

    fn bytes(&self) -> usize {
        let reordered: usize = self.reorder.iter().map(|(_, data)| data.len()).sum();
        self.send.len() + self.recv.len() + reordered
    }
}

enum Step {
    Gone,
    Alive(Option<Duration>),
}

/// Serial-number comparison (RFC 1982) of 32-bit sequence numbers.
fn lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn le(a: u32, b: u32) -> bool {
    a == b || lt(a, b)
}

fn in_window(sequence: u32, start: u32, length: u32) -> bool {
    sequence.wrapping_sub(start) < length
}

pub struct TcpStack<E: Entropy> {
    config: TcpConfig,
    entropy: E,
    fence: Fence,
    flows: BTreeMap<Key, Flow>,
    listeners: BTreeMap<u16, Listener>,
    outbox: VecDeque<Vec<u8>>,
    serial: u64,
    rejected: u64,
    dropped: u64,
}

impl<E: Entropy> TcpStack<E> {
    pub fn new(
        config: TcpConfig,
        entropy: E,
        generation: u64,
        now: Duration,
    ) -> Result<Self, TcpError> {
        config.validate()?;
        Ok(Self {
            config,
            entropy,
            fence: Fence::new(generation, now),
            flows: BTreeMap::new(),
            listeners: BTreeMap::new(),
            outbox: VecDeque::new(),
            serial: 0,
            rejected: 0,
            dropped: 0,
        })
    }

    pub fn flows(&self) -> usize {
        self.flows.len()
    }

    pub fn listeners(&self) -> usize {
        self.listeners.len()
    }

    /// Send, receive and out-of-order bytes held over all flows.
    pub fn buffered(&self) -> usize {
        self.flows.values().map(Flow::bytes).sum()
    }

    /// Datagrams that failed IPv4 or TCP validation.
    pub fn rejected(&self) -> u64 {
        self.rejected
    }

    /// Valid segments that changed nothing, and control packets lost to a full outbox.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn clear(&mut self) {
        self.flows.clear();
        self.listeners.clear();
        self.outbox.clear();
    }

    /// Irreversibly revokes the stack and drops everything it held.
    pub fn revoke(&mut self) {
        self.fence.revoke();
        self.clear();
    }

    fn admit(&mut self, generation: u64, now: Duration) -> Result<(), TcpError> {
        if self.fence.admit(generation, now) {
            Ok(())
        } else {
            self.clear();
            Err(TcpError::Revoked)
        }
    }

    fn admit_generation(&mut self, generation: u64) -> Result<(), TcpError> {
        if self.fence.admit_generation(generation) {
            Ok(())
        } else {
            self.clear();
            Err(TcpError::Revoked)
        }
    }

    fn next_serial(&mut self) -> u64 {
        self.serial = self.serial.saturating_add(1);
        self.serial
    }

    fn draw(&mut self) -> Result<u32, TcpError> {
        let bytes = self.entropy.bytes::<4>().map_err(TcpError::Entropy)?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn ephemeral(&mut self, remote: (Ipv4Addr, u16)) -> Result<u16, TcpError> {
        for _ in 0..PORT_ATTEMPTS {
            let raw = self.entropy.bytes::<2>().map_err(TcpError::Entropy)?;
            let port = EPHEMERAL_START + u16::from_be_bytes(raw) % EPHEMERAL_COUNT;
            if !self.listeners.contains_key(&port)
                && !self.flows.contains_key(&(port, remote.0, remote.1))
            {
                return Ok(port);
            }
        }
        Err(TcpError::PortsExhausted)
    }

    fn flow(&mut self, handle: FlowHandle) -> Result<&mut Flow, TcpError> {
        match self.flows.get_mut(&handle.key) {
            Some(flow) if flow.serial == handle.serial => Ok(flow),
            _ => Err(TcpError::Closed),
        }
    }

    fn forget_ready(&mut self, key: Key, flow: &Flow) {
        if let Some(port) = flow.listener
            && let Some(listener) = self.listeners.get_mut(&port)
        {
            listener.ready.retain(|entry| *entry != (key, flow.serial));
        }
    }

    fn new_flow(&mut self, state: State, iss: u32, peer_mss: Option<u16>, window: u16) -> Flow {
        let mss = usize::from(peer_mss.unwrap_or(DEFAULT_PEER_MSS)).min(self.config.mss());
        let serial = self.next_serial();
        Flow {
            serial,
            listener: None,
            accepted: true,
            state,
            failure: None,
            mss,
            path_mtu: self.config.mtu,
            network_error: None,
            iss,
            irs: 0,
            snd_una: iss,
            snd_nxt: iss.wrapping_add(1),
            snd_max: iss.wrapping_add(1),
            snd_wnd: u32::from(window),
            wl1: 0,
            wl2: iss,
            rcv_nxt: 0,
            send: VecDeque::new(),
            recv: VecDeque::new(),
            reorder: Vec::new(),
            close_requested: false,
            fin_seq: None,
            fin_received: false,
            ack_due: false,
            syn_pending: true,
            probe: false,
            cwnd: (4 * mss).min(self.config.send_buffer),
            ssthresh: usize::MAX,
            dup_acks: 0,
            rto: self.config.initial_rto,
            srtt: None,
            rttvar: Duration::ZERO,
            sample: None,
            ambiguous_until: None,
            deadline: None,
            retries: 0,
            stalled: None,
            expires: None,
        }
    }

    /// Starts listening on `port`. Only SYNs to listening ports open flows.
    pub fn listen(&mut self, generation: u64, port: u16) -> Result<ListenerHandle, TcpError> {
        self.admit_generation(generation)?;
        if port == 0 {
            return Err(TcpError::Endpoint);
        }
        if self.listeners.contains_key(&port) {
            return Err(TcpError::PortInUse);
        }
        if self.listeners.len() >= self.config.listeners {
            return Err(TcpError::ListenersFull);
        }
        let serial = self.next_serial();
        self.listeners.insert(
            port,
            Listener {
                serial,
                ready: VecDeque::new(),
            },
        );
        Ok(ListenerHandle { port, serial })
    }

    /// Stops listening and resets every flow that was not accepted yet.
    pub fn unlisten(&mut self, generation: u64, handle: ListenerHandle) -> Result<(), TcpError> {
        self.admit_generation(generation)?;
        match self.listeners.get(&handle.port) {
            Some(listener) if listener.serial == handle.serial => {}
            _ => return Err(TcpError::Closed),
        }
        self.listeners.remove(&handle.port);
        let orphans: Vec<Key> = self
            .flows
            .iter()
            .filter(|(_, flow)| flow.listener == Some(handle.port) && !flow.accepted)
            .map(|(key, _)| *key)
            .collect();
        for key in orphans {
            if let Some(flow) = self.flows.remove(&key) {
                self.reset_peer(key, &flow);
            }
        }
        Ok(())
    }

    /// Takes the next established flow from the listener.
    pub fn accept(
        &mut self,
        generation: u64,
        handle: ListenerHandle,
    ) -> Result<Option<FlowHandle>, TcpError> {
        self.admit_generation(generation)?;
        let listener = match self.listeners.get_mut(&handle.port) {
            Some(listener) if listener.serial == handle.serial => listener,
            _ => return Err(TcpError::Closed),
        };
        while let Some((key, serial)) = listener.ready.pop_front() {
            if let Some(flow) = self.flows.get_mut(&key)
                && flow.serial == serial
                && !flow.accepted
            {
                flow.accepted = true;
                return Ok(Some(FlowHandle { key, serial }));
            }
        }
        Ok(None)
    }

    /// Starts a connection; the SYN leaves with the next [`poll`](Self::poll).
    pub fn connect(
        &mut self,
        generation: u64,
        now: Duration,
        remote: Ipv4Addr,
        remote_port: u16,
    ) -> Result<FlowHandle, TcpError> {
        self.admit(generation, now)?;
        let ip = self.config.ip;
        if remote_port == 0 || unroutable(remote) || remote == ip.local() || !ip.allows(remote) {
            return Err(TcpError::Endpoint);
        }
        if self.flows.len() >= self.config.flows {
            return Err(TcpError::FlowsFull);
        }
        let port = self.ephemeral((remote, remote_port))?;
        let iss = self.draw()?;
        let flow = self.new_flow(State::SynSent, iss, None, 0);
        let handle = FlowHandle {
            key: (port, remote, remote_port),
            serial: flow.serial,
        };
        self.flows.insert(handle.key, flow);
        Ok(handle)
    }

    pub fn state(&mut self, generation: u64, handle: FlowHandle) -> Result<State, TcpError> {
        self.admit_generation(generation)?;
        Ok(self.flow(handle)?.state)
    }

    pub fn failure(
        &mut self,
        generation: u64,
        handle: FlowHandle,
    ) -> Result<Option<Failure>, TcpError> {
        self.admit_generation(generation)?;
        Ok(self.flow(handle)?.failure)
    }

    /// Queues bytes for sending and returns how many fit; zero means the send
    /// buffer (or the global budget) is full.
    pub fn send(
        &mut self,
        generation: u64,
        handle: FlowHandle,
        data: &[u8],
    ) -> Result<usize, TcpError> {
        self.admit_generation(generation)?;
        let used = self.buffered();
        let total = self.config.total_buffer;
        let capacity = self.config.send_buffer;
        let flow = self.flow(handle)?;
        if let Some(failure) = flow.failure {
            return Err(TcpError::Failed(failure));
        }
        match flow.state {
            State::Established | State::CloseWait if !flow.close_requested => {}
            State::SynSent | State::SynReceived => return Err(TcpError::NotConnected),
            _ => return Err(TcpError::Shutdown),
        }
        let room = capacity
            .saturating_sub(flow.send.len())
            .min(total.saturating_sub(used));
        let taken = data.len().min(room);
        flow.send.extend(&data[..taken]);
        Ok(taken)
    }

    /// Reads received bytes in order.
    pub fn recv(
        &mut self,
        generation: u64,
        handle: FlowHandle,
        buffer: &mut [u8],
    ) -> Result<Read, TcpError> {
        self.admit_generation(generation)?;
        let capacity = self.config.receive_buffer;
        let flow = self.flow(handle)?;
        if !flow.recv.is_empty() && !buffer.is_empty() {
            let before = capacity.saturating_sub(flow.recv.len());
            let count = buffer.len().min(flow.recv.len());
            for (slot, byte) in buffer.iter_mut().zip(flow.recv.drain(..count)) {
                *slot = byte;
            }
            // Announce a window that opened by a worthwhile amount.
            let opened = capacity
                .saturating_sub(flow.recv.len())
                .saturating_sub(before);
            let worthwhile = flow.mss.min(capacity / 2);
            if flow.state != State::Closed && opened >= worthwhile {
                flow.ack_due = true;
            }
            return Ok(Read::Data(count));
        }
        if !flow.recv.is_empty() {
            return Ok(Read::Data(0));
        }
        if let Some(failure) = flow.failure {
            return Err(TcpError::Failed(failure));
        }
        if flow.fin_received || flow.state == State::Closed {
            return Ok(Read::Eof);
        }
        Ok(Read::WouldBlock)
    }

    /// Shuts down sending: the FIN follows the queued bytes. On a flow that is
    /// already closed this releases it. Reading stays possible until the peer
    /// finishes too.
    pub fn close(
        &mut self,
        generation: u64,
        now: Duration,
        handle: FlowHandle,
    ) -> Result<(), TcpError> {
        self.admit(generation, now)?;
        let flow = self.flow(handle)?;
        if matches!(flow.state, State::SynSent | State::Closed) {
            self.flows.remove(&handle.key);
        } else if matches!(flow.state, State::Established | State::CloseWait)
            && !flow.close_requested
        {
            flow.close_requested = true;
            flow.fin_seq = Some(flow.snd_una.wrapping_add(flow.send.len() as u32));
        }
        Ok(())
    }

    /// Resets the peer and releases the flow at once.
    pub fn abort(&mut self, generation: u64, handle: FlowHandle) -> Result<(), TcpError> {
        self.admit_generation(generation)?;
        self.flow(handle)?;
        if let Some(flow) = self.flows.remove(&handle.key) {
            self.reset_peer(handle.key, &flow);
        }
        Ok(())
    }

    fn reset_peer(&mut self, key: Key, flow: &Flow) {
        if matches!(flow.state, State::SynSent | State::Closed | State::TimeWait) {
            return;
        }
        self.control(key, flow.snd_nxt, flow.rcv_nxt, RST | ACK);
    }

    fn advertised(&self, flow: &Flow) -> u16 {
        let free = self.config.receive_buffer.saturating_sub(flow.recv.len());
        free.min(MAX_WINDOW) as u16
    }

    fn packet(&self, key: Key, header: Header, payload: &[u8]) -> Result<Vec<u8>, BuildError> {
        tcp::build(
            self.config.ip.local(),
            key.1,
            &header,
            payload,
            self.config.mtu,
        )
    }

    /// Queues a payload-free control segment, or counts it when the outbox is full.
    fn control(&mut self, key: Key, sequence: u32, acknowledgment: u32, flags: u8) {
        if self.outbox.len() >= self.config.outbox {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let header = Header {
            source_port: key.0,
            destination_port: key.2,
            sequence,
            acknowledgment,
            flags,
            window: 0,
            mss: None,
        };
        if let Ok(packet) = self.packet(key, header, &[]) {
            self.outbox.push_back(packet);
        }
    }

    /// The RFC 9293 answer to a segment that belongs to no connection.
    fn refuse(&mut self, key: Key, segment: &Segment<'_>) {
        if segment.flags & ACK != 0 {
            self.control(key, segment.acknowledgment, 0, RST);
        } else {
            let acknowledgment = segment.sequence.wrapping_add(segment.space());
            self.control(key, 0, acknowledgment, RST | ACK);
        }
    }

    /// The most recent validated unreachable advice. Established streams retain
    /// their retransmission policy: ICMP cannot reset an established connection.
    pub fn network_error(
        &mut self,
        generation: u64,
        handle: FlowHandle,
    ) -> Result<Option<u8>, TcpError> {
        self.admit_generation(generation)?;
        Ok(self.flow(handle)?.network_error)
    }

    pub fn path_mtu(&mut self, generation: u64, handle: FlowHandle) -> Result<usize, TcpError> {
        self.admit_generation(generation)?;
        Ok(self.flow(handle)?.path_mtu)
    }

    /// Matches the quoted tuple and a sequence within actually transmitted,
    /// unacknowledged sequence space before any effect (RFC 5927). No new table.
    pub fn ingest_icmp(&mut self, generation: u64, now: Duration, bytes: &[u8]) -> IcmpIngest {
        if self.admit(generation, now).is_err() {
            return IcmpIngest::Revoked;
        }
        let advice = match icmp::parse(&self.config.ip, bytes) {
            Ok(advice) => advice,
            Err(error) => return IcmpIngest::Invalid(error),
        };
        if advice.protocol != PROTOCOL_TCP {
            return IcmpIngest::Dropped;
        }
        let (local_port, remote_port) = advice.ports();
        let key = (local_port, advice.remote, remote_port);
        let Some(mut flow) = self.flows.remove(&key) else {
            return IcmpIngest::Dropped;
        };
        let sequence = u32::from_be_bytes(advice.transport[4..8].try_into().expect("sequence"));
        let outstanding = in_window(
            sequence,
            flow.snd_una,
            flow.snd_max.wrapping_sub(flow.snd_una),
        );
        let sent = !matches!(flow.state, State::Closed | State::TimeWait)
            && !(matches!(flow.state, State::SynSent | State::SynReceived)
                && flow.syn_pending
                && flow.deadline.is_none());
        let outcome = if !sent || !outstanding {
            IcmpIngest::Dropped
        } else if advice.code == 4 {
            // The sender controls the quoted length: trust it only up to a
            // segment this flow could have sent. No SYN or SYN-ACK exceeds the
            // 576-byte floor, so handshakes are never resegmented; rewinding
            // one would leave its sequence space outstanding with no timer.
            let handshake = matches!(flow.state, State::SynSent | State::SynReceived);
            let plausible = !handshake && advice.total <= HEADERS + flow.mss;
            match advice.reduction(flow.path_mtu).filter(|_| plausible) {
                Some(mtu) => {
                    flow.path_mtu = mtu;
                    flow.mss = flow.mss.min(mtu - HEADERS);
                    // Re-segment queued bytes at the new MSS, preserving stream
                    // offsets, congestion/window bounds and retransmission caps.
                    self.collapse(&mut flow);
                    IcmpIngest::Applied(IcmpEffect::PathMtu(mtu))
                }
                None => IcmpIngest::Dropped,
            }
        } else {
            flow.network_error = Some(advice.code);
            if flow.state == State::SynSent {
                self.fail(&mut flow, Failure::Unreachable(advice.code), now);
            }
            IcmpIngest::Applied(IcmpEffect::Unreachable(advice.code))
        };
        self.flows.insert(key, flow);
        outcome
    }

    /// Parses one inbound inner IPv4 packet and applies its TCP segment.
    pub fn ingest(&mut self, generation: u64, now: Duration, bytes: &[u8]) -> Ingest {
        if self.admit(generation, now).is_err() {
            return Ingest::Revoked;
        }
        let packet = match ipv4::parse_protocol(&self.config.ip, bytes, PROTOCOL_TCP) {
            Ok(packet) => packet,
            Err(error) => {
                self.rejected = self.rejected.saturating_add(1);
                return Ingest::Ip(error);
            }
        };
        let segment = match tcp::parse(packet.source, packet.destination, packet.payload) {
            Ok(segment) => segment,
            Err(error) => {
                self.rejected = self.rejected.saturating_add(1);
                return Ingest::Segment(error);
            }
        };
        let key = (segment.destination_port, packet.source, segment.source_port);
        let outcome = match self.flows.remove(&key) {
            Some(mut flow) => {
                let outcome = self.arrive(key, &mut flow, &segment, now);
                if !flow.removable() {
                    self.flows.insert(key, flow);
                } else {
                    self.forget_ready(key, &flow);
                }
                outcome
            }
            None => self.unknown(key, &segment),
        };
        if matches!(outcome, Ingest::Dropped(_)) {
            self.dropped = self.dropped.saturating_add(1);
        }
        outcome
    }

    /// A segment for no flow: a SYN to a listener opens one, anything else is
    /// answered with an RST (never answering an RST).
    fn unknown(&mut self, key: Key, segment: &Segment<'_>) -> Ingest {
        if segment.flags & RST != 0 {
            return Ingest::Dropped(Discard::Reset);
        }
        let opens = segment.flags & (SYN | ACK) == SYN;
        if !opens || !self.listeners.contains_key(&key.0) {
            self.refuse(key, segment);
            return Ingest::Answered;
        }
        let waiting = self
            .flows
            .values()
            .filter(|flow| flow.listener == Some(key.0) && !flow.accepted)
            .count();
        if waiting >= self.config.backlog {
            return Ingest::Dropped(Discard::Backlog);
        }
        if self.flows.len() >= self.config.flows {
            return Ingest::Dropped(Discard::TableFull);
        }
        let Ok(iss) = self.draw() else {
            return Ingest::Dropped(Discard::Entropy);
        };
        let mut flow = self.new_flow(State::SynReceived, iss, segment.mss, segment.window);
        flow.listener = Some(key.0);
        flow.accepted = false;
        flow.irs = segment.sequence;
        flow.rcv_nxt = segment.sequence.wrapping_add(1);
        flow.wl1 = segment.sequence;
        self.flows.insert(key, flow);
        Ingest::Handshake
    }

    fn arrive(
        &mut self,
        key: Key,
        flow: &mut Flow,
        segment: &Segment<'_>,
        now: Duration,
    ) -> Ingest {
        match flow.state {
            State::SynSent => return self.arrive_syn_sent(key, flow, segment, now),
            State::Closed => {
                if segment.flags & RST != 0 {
                    return Ingest::Dropped(Discard::State);
                }
                self.refuse(key, segment);
                return Ingest::Answered;
            }
            _ => {}
        }
        if flow.state == State::SynReceived
            && segment.flags & (SYN | ACK) == SYN
            && segment.sequence == flow.irs
        {
            // The peer repeats its SYN: answer with another SYN-ACK.
            flow.syn_pending = true;
            return Ingest::Dropped(Discard::State);
        }
        let space = segment.space();
        let window = u32::from(self.advertised(flow));
        let acceptable = if space == 0 {
            if window == 0 {
                segment.sequence == flow.rcv_nxt
            } else {
                in_window(segment.sequence, flow.rcv_nxt, window)
            }
        } else {
            window != 0
                && (in_window(segment.sequence, flow.rcv_nxt, window)
                    || in_window(
                        segment.sequence.wrapping_add(space - 1),
                        flow.rcv_nxt,
                        window,
                    ))
        };
        if !acceptable {
            if segment.flags & RST == 0 && flow.state != State::SynReceived {
                flow.ack_due = true;
                if flow.state == State::TimeWait && segment.flags & FIN != 0 {
                    flow.expires = Some(now.saturating_add(self.config.time_wait));
                }
            }
            return Ingest::Dropped(Discard::Window);
        }
        if segment.flags & RST != 0 {
            if segment.sequence == flow.rcv_nxt {
                self.fail(flow, Failure::Reset, now);
                return Ingest::Reset;
            }
            // RFC 5961: an in-window RST that is not exact only draws a challenge.
            flow.ack_due = true;
            return Ingest::Dropped(Discard::Reset);
        }
        if segment.flags & SYN != 0 {
            flow.ack_due = flow.state != State::SynReceived;
            return Ingest::Dropped(Discard::State);
        }
        if segment.flags & ACK == 0 {
            return Ingest::Dropped(Discard::Ack);
        }
        let acknowledgment = segment.acknowledgment;
        if flow.state == State::SynReceived {
            if !(lt(flow.snd_una, acknowledgment) && le(acknowledgment, flow.snd_nxt)) {
                self.control(key, acknowledgment, 0, RST);
                return Ingest::Dropped(Discard::Ack);
            }
            flow.snd_una = acknowledgment;
            flow.snd_wnd = u32::from(segment.window);
            flow.wl1 = segment.sequence;
            flow.wl2 = acknowledgment;
            flow.state = State::Established;
            self.sample_rtt(flow, acknowledgment, now);
            if flow.retries > 0 {
                flow.rto = flow.rto.max(Duration::from_secs(3));
            }
            flow.retries = 0;
            flow.deadline = None;
            flow.syn_pending = false;
            let port = flow.listener.unwrap_or(0);
            match self.listeners.get_mut(&port) {
                Some(listener) => listener.ready.push_back((key, flow.serial)),
                None => {
                    flow.state = State::Closed;
                    return Ingest::Dropped(Discard::State);
                }
            }
        } else {
            if lt(flow.snd_max, acknowledgment) {
                flow.ack_due = true;
                return Ingest::Dropped(Discard::Ack);
            }
            self.acknowledge(flow, segment, now);
        }
        self.receive(flow, segment, now);
        Ingest::Delivered
    }

    fn arrive_syn_sent(
        &mut self,
        key: Key,
        flow: &mut Flow,
        segment: &Segment<'_>,
        now: Duration,
    ) -> Ingest {
        let acknowledged = segment.flags & ACK != 0;
        if acknowledged
            && !(lt(flow.iss, segment.acknowledgment) && le(segment.acknowledgment, flow.snd_nxt))
        {
            if segment.flags & RST == 0 {
                self.control(key, segment.acknowledgment, 0, RST);
            }
            return Ingest::Dropped(Discard::Ack);
        }
        if segment.flags & RST != 0 {
            if acknowledged {
                self.fail(flow, Failure::Refused, now);
                return Ingest::Reset;
            }
            return Ingest::Dropped(Discard::Reset);
        }
        // Simultaneous open (a SYN without ACK) is not supported.
        if segment.flags & SYN == 0 || !acknowledged {
            return Ingest::Dropped(Discard::State);
        }
        flow.irs = segment.sequence;
        flow.rcv_nxt = segment.sequence.wrapping_add(1);
        flow.snd_una = segment.acknowledgment;
        flow.snd_wnd = u32::from(segment.window);
        flow.wl1 = segment.sequence;
        flow.wl2 = segment.acknowledgment;
        flow.mss = usize::from(segment.mss.unwrap_or(DEFAULT_PEER_MSS)).min(self.config.mss());
        flow.cwnd = (4 * flow.mss).min(self.config.send_buffer);
        flow.state = State::Established;
        flow.syn_pending = false;
        self.sample_rtt(flow, segment.acknowledgment, now);
        if flow.retries > 0 {
            flow.rto = flow.rto.max(Duration::from_secs(3));
        }
        flow.retries = 0;
        flow.deadline = None;
        flow.ack_due = true;
        Ingest::Delivered
    }

    /// Ends the flow abnormally: buffers are released and the entry lingers so
    /// the caller sees the failure.
    fn fail(&self, flow: &mut Flow, failure: Failure, now: Duration) {
        flow.state = State::Closed;
        flow.failure = Some(failure);
        flow.send = VecDeque::new();
        flow.recv = VecDeque::new();
        flow.reorder = Vec::new();
        flow.deadline = None;
        flow.ack_due = false;
        flow.syn_pending = false;
        flow.probe = false;
        flow.stalled = None;
        flow.expires = Some(if flow.removable() {
            now
        } else {
            now.saturating_add(self.config.linger)
        });
    }

    /// Ends the flow normally; received bytes stay readable.
    fn finish(&self, flow: &mut Flow, now: Duration) {
        flow.state = State::Closed;
        flow.send = VecDeque::new();
        flow.reorder = Vec::new();
        flow.deadline = None;
        flow.ack_due = false;
        flow.expires = Some(now.saturating_add(self.config.linger));
    }

    /// One outstanding timing sample per flow; never time retransmitted bytes,
    /// including fast retransmit, path-MTU resegmentation and zero-window probes.
    fn time_segment(&self, flow: &mut Flow, sequence: u32, end: u32, now: Duration) {
        if lt(sequence, flow.snd_max) {
            flow.sample = None;
            flow.ambiguous_until = Some(flow.snd_max);
        } else if flow.sample.is_none() && flow.ambiguous_until.is_none() {
            flow.sample = Some((end, now));
        }
    }

    fn sample_rtt(&self, flow: &mut Flow, acknowledgment: u32, now: Duration) {
        if let Some(end) = flow.ambiguous_until {
            if le(end, acknowledgment) {
                flow.ambiguous_until = None;
            }
            return;
        }
        let Some((end, sent)) = flow.sample else {
            return;
        };
        if !le(end, acknowledgment) {
            return;
        }
        flow.sample = None;
        let sample = now.saturating_sub(sent);
        match flow.srtt {
            None => {
                flow.srtt = Some(sample);
                flow.rttvar = sample / 2;
            }
            Some(previous) => {
                flow.rttvar = flow
                    .rttvar
                    .saturating_mul(3)
                    .saturating_add(previous.abs_diff(sample))
                    / 4;
                flow.srtt = Some(previous.saturating_mul(7).saturating_add(sample) / 8);
            }
        }
        // RFC 6298 coefficients, 1 ms clock granularity, one-second floor.
        flow.rto = flow
            .srtt
            .unwrap_or_default()
            .saturating_add(flow.rttvar.saturating_mul(4).max(Duration::from_millis(1)))
            .max(Duration::from_secs(1))
            .min(self.config.max_rto);
    }

    fn acknowledge(&mut self, flow: &mut Flow, segment: &Segment<'_>, now: Duration) {
        let acknowledgment = segment.acknowledgment;
        if lt(flow.snd_una, acknowledgment) {
            let acked = acknowledgment.wrapping_sub(flow.snd_una) as usize;
            let data = acked.min(flow.send.len());
            flow.send.drain(..data);
            if lt(flow.snd_nxt, acknowledgment) {
                flow.snd_nxt = acknowledgment;
            }
            flow.snd_una = acknowledgment;
            flow.retries = 0;
            self.sample_rtt(flow, acknowledgment, now);
            flow.dup_acks = 0;
            flow.probe = false;
            let mss = flow.mss;
            let grown = if flow.cwnd < flow.ssthresh {
                data.min(mss)
            } else {
                (mss * mss / flow.cwnd.max(1)).max(1)
            };
            flow.cwnd = flow.cwnd.saturating_add(grown).min(self.config.send_buffer);
            flow.deadline = if flow.snd_una != flow.snd_max {
                Some(now.saturating_add(flow.rto))
            } else {
                None
            };
            if flow.fin_seq.is_some_and(|fin| lt(fin, acknowledgment)) {
                self.fin_acknowledged(flow, now);
            }
        } else if acknowledgment == flow.snd_una
            && segment.payload.is_empty()
            && segment.flags & FIN == 0
            && flow.snd_una != flow.snd_max
            && flow.snd_wnd != 0
            && u32::from(segment.window) == flow.snd_wnd
        {
            flow.dup_acks += 1;
            if flow.dup_acks == 3 {
                // Fast retransmit, Tahoe style: back to slow start from snd_una.
                self.collapse(flow);
            }
        }
        let fresh = le(flow.snd_una, acknowledgment)
            && (lt(flow.wl1, segment.sequence)
                || (flow.wl1 == segment.sequence && le(flow.wl2, acknowledgment)));
        if fresh {
            flow.snd_wnd = u32::from(segment.window);
            flow.wl1 = segment.sequence;
            flow.wl2 = acknowledgment;
            if flow.snd_wnd > 0 {
                flow.stalled = None;
                flow.probe = false;
                if flow.snd_una == flow.snd_max {
                    flow.deadline = None;
                }
            }
        }
    }

    /// Loss response: halve the threshold, restart slow start and resend from
    /// the first unacknowledged byte.
    fn collapse(&self, flow: &mut Flow) {
        flow.sample = None;
        flow.ambiguous_until = Some(flow.snd_max);
        let flight = flow.snd_max.wrapping_sub(flow.snd_una) as usize;
        flow.ssthresh = (flight / 2).max(2 * flow.mss);
        flow.cwnd = flow.mss;
        flow.dup_acks = 0;
        flow.snd_nxt = flow.snd_una;
        flow.deadline = None;
    }

    fn fin_acknowledged(&self, flow: &mut Flow, now: Duration) {
        match flow.state {
            State::FinWait1 => flow.state = State::FinWait2,
            State::Closing => self.time_wait(flow, now),
            State::LastAck => self.finish(flow, now),
            _ => {}
        }
    }

    fn time_wait(&self, flow: &mut Flow, now: Duration) {
        flow.state = State::TimeWait;
        flow.send = VecDeque::new();
        flow.reorder = Vec::new();
        flow.deadline = None;
        flow.expires = Some(now.saturating_add(self.config.time_wait));
    }

    /// Accepts payload and FIN that the sequence check let through.
    fn receive(&mut self, flow: &mut Flow, segment: &Segment<'_>, now: Duration) {
        let accepts = matches!(
            flow.state,
            State::Established | State::FinWait1 | State::FinWait2
        );
        let fin_at = segment.sequence.wrapping_add(segment.payload.len() as u32);
        let mut sequence = segment.sequence;
        let mut data = segment.payload;
        if lt(sequence, flow.rcv_nxt) {
            let skip = flow.rcv_nxt.wrapping_sub(sequence) as usize;
            data = data.get(skip..).unwrap_or(&[]);
            sequence = flow.rcv_nxt;
        }
        let offset = sequence.wrapping_sub(flow.rcv_nxt) as usize;
        let room = usize::from(self.advertised(flow)).saturating_sub(offset);
        let mut fin = segment.flags & FIN != 0;
        if data.len() > room {
            data = &data[..room];
            fin = false;
        }
        if !data.is_empty() {
            flow.ack_due = true;
            let fits = self.buffered() + flow.bytes() + data.len() <= self.config.total_buffer;
            if accepts && fits {
                if offset == 0 {
                    flow.recv.extend(data);
                    flow.rcv_nxt = flow.rcv_nxt.wrapping_add(data.len() as u32);
                    self.merge(flow);
                } else {
                    let queued: usize = flow.reorder.iter().map(|(_, held)| held.len()).sum();
                    if flow.reorder.len() < self.config.reorder_segments
                        && queued + data.len() <= self.config.receive_buffer
                    {
                        flow.reorder.push((sequence, data.to_vec()));
                    }
                }
            }
        }
        if fin && fin_at == flow.rcv_nxt && accepts {
            flow.rcv_nxt = flow.rcv_nxt.wrapping_add(1);
            flow.fin_received = true;
            flow.ack_due = true;
            match flow.state {
                State::Established => flow.state = State::CloseWait,
                State::FinWait1 => flow.state = State::Closing,
                State::FinWait2 => self.time_wait(flow, now),
                _ => {}
            }
        }
    }

    /// Moves out-of-order segments that became contiguous into the stream.
    fn merge(&self, flow: &mut Flow) {
        loop {
            let next = flow.reorder.iter().position(|(sequence, data)| {
                le(*sequence, flow.rcv_nxt)
                    && lt(flow.rcv_nxt, sequence.wrapping_add(data.len() as u32))
            });
            let Some(index) = next else { break };
            let (sequence, data) = flow.reorder.swap_remove(index);
            let skip = flow.rcv_nxt.wrapping_sub(sequence) as usize;
            let room = self.config.receive_buffer.saturating_sub(flow.recv.len());
            let fresh = &data[skip..];
            let taken = fresh.len().min(room);
            flow.recv.extend(&fresh[..taken]);
            flow.rcv_nxt = flow.rcv_nxt.wrapping_add(taken as u32);
        }
        let rcv_nxt = flow.rcv_nxt;
        flow.reorder
            .retain(|(sequence, data)| lt(rcv_nxt, sequence.wrapping_add(data.len() as u32)));
    }

    /// Emits due segments and runs timers. `next` is when to poll again.
    pub fn poll(&mut self, generation: u64, now: Duration) -> Result<Poll, TcpError> {
        self.admit(generation, now)?;
        let mut poll = Poll::default();
        poll.send.extend(self.outbox.drain(..));
        let mut next: Option<Duration> = None;
        let keys: Vec<Key> = self.flows.keys().copied().collect();
        for key in keys {
            let Some(mut flow) = self.flows.remove(&key) else {
                continue;
            };
            match self.advance(key, &mut flow, now, &mut poll.send) {
                Step::Gone => self.forget_ready(key, &flow),
                Step::Alive(wake) => {
                    next = [next, wake].into_iter().flatten().min();
                    self.flows.insert(key, flow);
                }
            }
        }
        // Timers of control packets queued while advancing leave next time.
        poll.send.extend(self.outbox.drain(..));
        poll.next = next;
        Ok(poll)
    }

    fn advance(
        &mut self,
        key: Key,
        flow: &mut Flow,
        now: Duration,
        out: &mut Vec<Vec<u8>>,
    ) -> Step {
        if flow.expires.is_some_and(|expires| now >= expires) {
            return Step::Gone;
        }
        if flow.deadline.is_some_and(|deadline| now >= deadline) {
            flow.deadline = None;
            self.timeout(key, flow, now);
            if flow.expires.is_some_and(|expires| now >= expires) {
                return Step::Gone;
            }
        }
        let before = out.len();
        let mut more = false;
        match flow.state {
            State::SynSent | State::SynReceived => self.handshake(key, flow, now, out),
            State::Established
            | State::CloseWait
            | State::FinWait1
            | State::Closing
            | State::LastAck => more = self.transmit(key, flow, now, out),
            State::FinWait2 | State::TimeWait | State::Closed => {}
        }
        if flow.ack_due
            && out.len() == before
            && !matches!(
                flow.state,
                State::SynSent | State::SynReceived | State::Closed
            )
        {
            let header = Header {
                source_port: key.0,
                destination_port: key.2,
                sequence: flow.snd_max,
                acknowledgment: flow.rcv_nxt,
                flags: ACK,
                window: self.advertised(flow),
                mss: None,
            };
            if let Ok(packet) = self.packet(key, header, &[]) {
                out.push(packet);
            }
        }
        if flow.ack_due && !matches!(flow.state, State::SynSent | State::SynReceived) {
            flow.ack_due = false;
        }
        let wake = [flow.deadline, flow.expires, more.then_some(now)]
            .into_iter()
            .flatten()
            .min();
        Step::Alive(wake)
    }

    fn backoff(&self, flow: &mut Flow) {
        flow.rto = flow.rto.saturating_mul(2).min(self.config.max_rto);
    }

    fn timeout(&mut self, key: Key, flow: &mut Flow, now: Duration) {
        match flow.state {
            State::SynSent | State::SynReceived => {
                if flow.retries >= self.config.syn_retries {
                    self.fail(flow, Failure::Timeout, now);
                    return;
                }
                flow.retries += 1;
                self.backoff(flow);
                flow.syn_pending = true;
            }
            State::TimeWait | State::Closed | State::FinWait2 => {}
            _ => {
                let stalled = flow.snd_wnd == 0 && !flow.send.is_empty();
                if stalled {
                    let since = *flow.stalled.get_or_insert(now);
                    if now.saturating_sub(since) >= self.config.persist_limit {
                        self.give_up(key, flow, now);
                        return;
                    }
                    self.backoff(flow);
                    flow.snd_nxt = flow.snd_una;
                    flow.probe = true;
                } else if flow.snd_una != flow.snd_max {
                    if flow.retries >= self.config.retries {
                        self.give_up(key, flow, now);
                        return;
                    }
                    flow.retries += 1;
                    self.backoff(flow);
                    self.collapse(flow);
                }
            }
        }
    }

    fn give_up(&mut self, key: Key, flow: &mut Flow, now: Duration) {
        self.reset_peer(key, flow);
        self.fail(flow, Failure::Timeout, now);
    }

    fn handshake(&mut self, key: Key, flow: &mut Flow, now: Duration, out: &mut Vec<Vec<u8>>) {
        if !flow.syn_pending {
            return;
        }
        let synchronized = flow.state == State::SynReceived;
        let header = Header {
            source_port: key.0,
            destination_port: key.2,
            sequence: flow.iss,
            acknowledgment: if synchronized { flow.rcv_nxt } else { 0 },
            flags: if synchronized { SYN | ACK } else { SYN },
            window: self.advertised(flow),
            mss: Some((flow.path_mtu - HEADERS) as u16),
        };
        if let Ok(packet) = self.packet(key, header, &[]) {
            if flow.retries == 0 && flow.deadline.is_none() {
                flow.sample = Some((flow.iss.wrapping_add(1), now));
            } else {
                flow.sample = None;
                flow.ambiguous_until = Some(flow.snd_max);
            }
            out.push(packet);
        }
        flow.syn_pending = false;
        flow.deadline = Some(now.saturating_add(flow.rto));
    }

    /// Sends data and the FIN within the congestion and peer windows. Returns
    /// whether the burst limit left more to do.
    fn transmit(
        &mut self,
        key: Key,
        flow: &mut Flow,
        now: Duration,
        out: &mut Vec<Vec<u8>>,
    ) -> bool {
        let window = self.advertised(flow);
        let mut budget = self.config.burst;
        let mut more = false;
        loop {
            let flight = flow.snd_nxt.wrapping_sub(flow.snd_una) as usize;
            let sent = flight.min(flow.send.len());
            let unsent = flow.send.len() - sent;
            let (flags, payload) = if unsent > 0 {
                let usable = flow.cwnd.min(flow.snd_wnd as usize).saturating_sub(flight);
                let mut count = unsent.min(usable).min(flow.mss);
                if count == 0 && flow.probe && flight == 0 {
                    count = 1;
                    flow.probe = false;
                }
                if count == 0 {
                    break;
                }
                let payload: Vec<u8> = flow.send.range(sent..sent + count).copied().collect();
                let push = if count == unsent { PSH } else { 0 };
                (ACK | push, payload)
            } else if flow.fin_seq == Some(flow.snd_nxt) {
                (FIN | ACK, Vec::new())
            } else {
                break;
            };
            if budget == 0 {
                more = true;
                break;
            }
            let header = Header {
                source_port: key.0,
                destination_port: key.2,
                sequence: flow.snd_nxt,
                acknowledgment: flow.rcv_nxt,
                flags,
                window,
                mss: None,
            };
            let Ok(packet) = self.packet(key, header, &payload) else {
                break;
            };
            self.time_segment(
                flow,
                flow.snd_nxt,
                flow.snd_nxt
                    .wrapping_add(payload.len() as u32 + u32::from(flags & FIN != 0)),
                now,
            );
            out.push(packet);
            budget -= 1;
            flow.ack_due = false;
            flow.snd_nxt = flow
                .snd_nxt
                .wrapping_add(payload.len() as u32 + u32::from(flags & FIN != 0));
            if lt(flow.snd_max, flow.snd_nxt) {
                flow.snd_max = flow.snd_nxt;
            }
            if flow.deadline.is_none() {
                flow.deadline = Some(now.saturating_add(flow.rto));
            }
            if flags & FIN != 0 {
                match flow.state {
                    State::Established => flow.state = State::FinWait1,
                    State::CloseWait => flow.state = State::LastAck,
                    _ => {}
                }
                break;
            }
        }
        // A zero window keeps the timer running to probe it, up to the stall limit.
        let stalled = flow.snd_wnd == 0 && !flow.send.is_empty();
        if stalled {
            flow.stalled.get_or_insert(now);
        } else {
            flow.stalled = None;
        }
        if flow.deadline.is_none() && (stalled || flow.snd_una != flow.snd_max) {
            flow.deadline = Some(now.saturating_add(flow.rto));
        }
        more
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
