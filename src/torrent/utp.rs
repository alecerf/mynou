//! uTP (BEP 29): BitTorrent over UDP, with delay-based congestion control
//! (LEDBAT) so transfers yield to other traffic. It shares the BitTorrent UDP
//! port with the DHT node: DHT messages are bencoded dictionaries, which start
//! with `d`, and uTP packets start with their type and version 1.
//!
//! One hub thread reads the port, answers the DHT, feeds connections and runs
//! their timers; streams send from their own threads. Outgoing peers are dialled
//! over TCP first, and over uTP too when TCP has not connected within 250 ms,
//! so a dead peer costs no more than before. Every connection is bounded by its
//! receive and congestion windows, and the hub by its connection count.
use super::{
    dht_node::{self, Node},
    transport::{Network, NetworkListener, Publication},
};
use crate::{
    Result,
    net::{HttpClient, NetworkDatagram, NetworkStream},
};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, ErrorKind, Read, Write},
    net::SocketAddr,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const VERSION: u8 = 1;
const ST_DATA: u8 = 0;
const ST_FIN: u8 = 1;
const ST_STATE: u8 = 2;
const ST_RESET: u8 = 3;
const ST_SYN: u8 = 4;
const HEADER: usize = 20;
/// Payload of one packet: 1,200-byte datagrams fit the tunnel MTU.
pub(crate) const PAYLOAD: usize = 1180;
/// Bytes received but not yet read, in order or not, per connection.
const RECV_WINDOW: usize = 256 * 1024;
const MAX_CWND: usize = 256 * 1024;
const INITIAL_CWND: usize = 4 * PAYLOAD;
/// LEDBAT: the queuing delay a connection aims for, and its growth per RTT.
const TARGET_DELAY: u32 = 100_000;
const MAX_CWND_INCREASE: f64 = 3000.0;
const INITIAL_RTO: Duration = Duration::from_secs(1);
const MIN_RTO: Duration = Duration::from_millis(500);
const MAX_RTO: Duration = Duration::from_secs(30);
/// Retransmission timeouts in a row before a connection is given up.
const MAX_TIMEOUTS: u32 = 6;
const DUPLICATE_ACKS: u32 = 3;
const KEEPALIVE: Duration = Duration::from_secs(29);
const DEAD_AFTER: Duration = Duration::from_secs(90);
/// How long a closed stream waits for its FIN to be acknowledged.
const CLOSE_LINGER: Duration = Duration::from_secs(20);
const MAX_CONNECTIONS: usize = 320;
const MAX_ACCEPT_QUEUE: usize = 8;
/// Out-of-order packets kept, counted from the next expected one.
const REORDER_SPAN: u16 = 1024;
const TICK: Duration = Duration::from_millis(20);
/// How long TCP has to connect before uTP is tried as well.
const UTP_AFTER: Duration = Duration::from_millis(250);

fn micros() -> u32 {
    static START: OnceLock<Instant> = OnceLock::new();
    // Wraps every 71 minutes, as the protocol expects.
    START.get_or_init(Instant::now).elapsed().as_micros() as u32
}

/// `a` comes before `b` in the wrapping sequence space.
fn before(a: u16, b: u16) -> bool {
    (b.wrapping_sub(a) as i16) > 0
}

/// The earlier of two wrapping microsecond values.
fn earliest(a: u32, b: u32) -> u32 {
    if (a.wrapping_sub(b) as i32) < 0 { a } else { b }
}

fn random_u16() -> u16 {
    crate::crypto::random_bytes::<2>().map_or(0x5a5a, u16::from_be_bytes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Header {
    kind: u8,
    connection: u16,
    timestamp: u32,
    difference: u32,
    window: u32,
    seq: u16,
    ack: u16,
}

/// A header and its payload; extensions, such as selective acks, are skipped.
fn parse(packet: &[u8]) -> Option<(Header, &[u8])> {
    let fixed: &[u8; HEADER] = packet.get(..HEADER)?.try_into().ok()?;
    if fixed[0] & 0x0f != VERSION || fixed[0] >> 4 > ST_SYN {
        return None;
    }
    let (mut extension, mut at) = (fixed[1], HEADER);
    for _ in 0..8 {
        if extension == 0 {
            break;
        }
        let (next, length) = (*packet.get(at)?, usize::from(*packet.get(at + 1)?));
        at = at.checked_add(2 + length)?;
        if at > packet.len() {
            return None;
        }
        extension = next;
    }
    if extension != 0 {
        return None;
    }
    let u16_at = |i: usize| u16::from_be_bytes([fixed[i], fixed[i + 1]]);
    let u32_at =
        |i: usize| u32::from_be_bytes([fixed[i], fixed[i + 1], fixed[i + 2], fixed[i + 3]]);
    let header = Header {
        kind: fixed[0] >> 4,
        connection: u16_at(2),
        timestamp: u32_at(4),
        difference: u32_at(8),
        window: u32_at(12),
        seq: u16_at(16),
        ack: u16_at(18),
    };
    Some((header, &packet[at..]))
}

fn encode(header: &Header, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.push(header.kind << 4 | VERSION);
    out.push(0);
    out.extend_from_slice(&header.connection.to_be_bytes());
    out.extend_from_slice(&header.timestamp.to_be_bytes());
    out.extend_from_slice(&header.difference.to_be_bytes());
    out.extend_from_slice(&header.window.to_be_bytes());
    out.extend_from_slice(&header.seq.to_be_bytes());
    out.extend_from_slice(&header.ack.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    SynSent,
    Connected,
    /// `None` once both sides finished cleanly.
    Closed(Option<ErrorKind>),
}

struct Sent {
    kind: u8,
    seq: u16,
    payload: Vec<u8>,
    sent_at: Instant,
    transmissions: u32,
    /// Lost: sent again once the window allows.
    resend: bool,
}

struct State {
    phase: Phase,
    /// The next sequence number this side sends.
    seq_nr: u16,
    /// The last sequence number received in order.
    ack_nr: u16,
    in_flight: VecDeque<Sent>,
    cwnd: usize,
    slow_start: bool,
    peer_window: usize,
    srtt: Option<(Duration, Duration)>,
    rto: Duration,
    rto_deadline: Option<Instant>,
    timeouts: u32,
    /// In fast recovery until this sequence number is acknowledged.
    recovery: Option<u16>,
    last_ack: u16,
    duplicate_acks: u32,
    /// The lowest one-way delay seen this minute and the previous one.
    base_delay: (Option<u32>, Option<u32>),
    base_rotated: Instant,
    /// Echoed to the peer: how late its last packet arrived by this clock.
    reply_micro: u32,
    received: VecDeque<u8>,
    reorder: BTreeMap<u16, Vec<u8>>,
    reorder_bytes: usize,
    fin_received: Option<u16>,
    eof: bool,
    closing: Option<Instant>,
    ack_owed: u32,
    /// Data was sent: closing then needs a FIN, not a reset.
    sent_data: bool,
    last_sent: Instant,
    last_received: Instant,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

impl State {
    fn new(phase: Phase, seq_nr: u16, ack_nr: u16, now: Instant) -> Self {
        Self {
            phase,
            seq_nr,
            ack_nr,
            in_flight: VecDeque::new(),
            cwnd: INITIAL_CWND,
            slow_start: true,
            peer_window: INITIAL_CWND,
            srtt: None,
            rto: INITIAL_RTO,
            rto_deadline: None,
            timeouts: 0,
            recovery: None,
            last_ack: seq_nr.wrapping_sub(1),
            duplicate_acks: 0,
            base_delay: (None, None),
            base_rotated: now,
            reply_micro: 0,
            received: VecDeque::new(),
            reorder: BTreeMap::new(),
            reorder_bytes: 0,
            fin_received: None,
            eof: false,
            closing: None,
            ack_owed: 0,
            sent_data: false,
            last_sent: now,
            last_received: now,
            read_timeout: None,
            write_timeout: None,
        }
    }

    fn window(&self) -> usize {
        RECV_WINDOW.saturating_sub(self.received.len() + self.reorder_bytes)
    }

    fn send_window(&self) -> usize {
        self.cwnd.min(self.peer_window)
    }

    /// The timeout after `timeouts` expiries in a row, each doubling it until
    /// new data is acknowledged.
    fn backed_off(&self) -> Duration {
        self.rto
            .saturating_mul(1 << self.timeouts.min(8))
            .min(MAX_RTO)
    }

    fn flight(&self) -> usize {
        self.in_flight
            .iter()
            .filter(|sent| !sent.resend)
            .map(|sent| sent.payload.len())
            .sum()
    }

    fn close(&mut self, error: Option<ErrorKind>) {
        if !matches!(self.phase, Phase::Closed(_)) {
            self.phase = Phase::Closed(error);
        }
        self.in_flight.clear();
        self.rto_deadline = None;
    }

    fn rtt_sample(&mut self, sample: Duration) {
        let (srtt, var) = match self.srtt {
            None => (sample, sample / 2),
            Some((srtt, var)) => {
                let error = srtt.abs_diff(sample);
                (srtt * 7 / 8 + sample / 8, var * 3 / 4 + error / 4)
            }
        };
        self.srtt = Some((srtt, var));
        self.rto = (srtt + var * 4).clamp(MIN_RTO, MAX_RTO);
    }

    /// LEDBAT: grows the window while the queuing delay the peer measures
    /// stays under the target, and shrinks it above.
    fn congestion(&mut self, acked: usize, difference: u32, now: Instant) {
        let mut queuing = 0;
        if difference != 0 {
            if now.duration_since(self.base_rotated) >= Duration::from_secs(60) {
                self.base_delay = (None, self.base_delay.0);
                self.base_rotated = now;
            }
            let current = self
                .base_delay
                .0
                .map_or(difference, |b| earliest(b, difference));
            self.base_delay.0 = Some(current);
            let base = self.base_delay.1.map_or(current, |b| earliest(b, current));
            queuing = difference.wrapping_sub(base);
            if queuing > i32::MAX as u32 {
                queuing = 0;
            }
        }
        if self.slow_start && queuing < TARGET_DELAY / 2 {
            self.cwnd += acked;
        } else {
            self.slow_start = false;
            let off_target =
                (f64::from(TARGET_DELAY) - f64::from(queuing)) / f64::from(TARGET_DELAY);
            let factor = acked.min(self.cwnd) as f64 / self.cwnd.max(acked) as f64;
            let gain = MAX_CWND_INCREASE * off_target * factor;
            self.cwnd = (self.cwnd as f64 + gain).max(0.0) as usize;
        }
        self.cwnd = self.cwnd.clamp(PAYLOAD, MAX_CWND);
    }
}

pub(crate) struct Conn {
    remote: SocketAddr,
    send_id: u16,
    recv_id: u16,
    state: Mutex<State>,
    changed: Condvar,
    /// The stream is gone: once closed, the hub forgets the connection.
    dropped: AtomicBool,
}

type Out = Vec<Vec<u8>>;

impl Conn {
    fn uses_id(&self, address: SocketAddr, id: u16) -> bool {
        self.remote == address && (self.recv_id == id || self.send_id == id)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn header(&self, st: &State, kind: u8, seq: u16) -> Header {
        Header {
            kind,
            connection: if kind == ST_SYN {
                self.recv_id
            } else {
                self.send_id
            },
            timestamp: micros(),
            difference: st.reply_micro,
            window: st.window() as u32,
            seq,
            ack: st.ack_nr,
        }
    }

    fn ack(&self, st: &mut State, now: Instant) -> Vec<u8> {
        st.ack_owed = 0;
        st.last_sent = now;
        encode(&self.header(st, ST_STATE, st.seq_nr), &[])
    }

    /// Queues a packet that takes a sequence number and must be acknowledged.
    fn push(&self, st: &mut State, kind: u8, payload: Vec<u8>, now: Instant) -> Vec<u8> {
        let seq = st.seq_nr;
        st.seq_nr = seq.wrapping_add(1);
        let packet = encode(&self.header(st, kind, seq), &payload);
        st.in_flight.push_back(Sent {
            kind,
            seq,
            payload,
            sent_at: now,
            transmissions: 1,
            resend: false,
        });
        st.rto_deadline.get_or_insert(now + st.backed_off());
        st.ack_owed = 0;
        st.last_sent = now;
        st.sent_data |= kind == ST_DATA;
        packet
    }

    /// Sends lost packets again, as far as the window allows.
    fn flush(&self, st: &mut State, now: Instant, out: &mut Out) {
        for i in 0..st.in_flight.len() {
            if !st.in_flight[i].resend {
                continue;
            }
            // The oldest packet, which holds everything else back, always goes.
            let flight = st.flight();
            if i > 0 && flight > 0 && flight + st.in_flight[i].payload.len() > st.send_window() {
                break;
            }
            let header = self.header(st, st.in_flight[i].kind, st.in_flight[i].seq);
            let sent = &mut st.in_flight[i];
            sent.resend = false;
            sent.transmissions += 1;
            sent.sent_at = now;
            out.push(encode(&header, &sent.payload));
            st.ack_owed = 0;
            st.last_sent = now;
            st.rto_deadline.get_or_insert(now + st.backed_off());
        }
    }

    fn on_ack(&self, st: &mut State, header: &Header, pure: bool, now: Instant) {
        // Only acknowledgements of packets this side sent count.
        if before(st.seq_nr.wrapping_sub(1), header.ack) {
            return;
        }
        let (mut acked, mut bytes, mut sample, mut resent) = (0, 0, None, false);
        while st
            .in_flight
            .front()
            .is_some_and(|first| !before(header.ack, first.seq))
        {
            if let Some(sent) = st.in_flight.pop_front() {
                acked += 1;
                bytes += sent.payload.len();
                resent |= sent.transmissions > 1;
                sample = Some(now.saturating_duration_since(sent.sent_at));
            }
        }
        // Karn: no sample from a retransmission, nor from packets held at
        // the peer behind one.
        if resent || st.recovery.is_some() {
            sample = None;
        }
        if acked > 0 {
            st.duplicate_acks = 0;
            // New data acknowledged: the timeout is no longer backed off.
            st.timeouts = 0;
            if let Some(sample) = sample {
                st.rtt_sample(sample);
            }
            st.rto_deadline = (!st.in_flight.is_empty()).then(|| now + st.rto);
            if bytes > 0 {
                st.congestion(bytes, header.difference, now);
            }
            match st.recovery {
                // A partial acknowledgement in recovery: the next hole is
                // lost too (NewReno), so it is sent again at once.
                Some(end) if before(header.ack, end) => {
                    if let Some(first) = st.in_flight.front_mut()
                        && first.transmissions == 1
                    {
                        first.resend = true;
                    }
                }
                Some(_) => st.recovery = None,
                None => {}
            }
        } else if pure && header.ack == st.last_ack && !st.in_flight.is_empty() {
            st.duplicate_acks += 1;
            if st.duplicate_acks == DUPLICATE_ACKS && st.recovery.is_none() {
                // Fast retransmit of the oldest packet, once per window.
                st.cwnd = (st.cwnd / 2).max(PAYLOAD);
                st.slow_start = false;
                st.recovery = Some(st.seq_nr.wrapping_sub(1));
                if let Some(first) = st.in_flight.front_mut() {
                    first.resend = true;
                }
            } else if st.duplicate_acks > DUPLICATE_ACKS
                && st.duplicate_acks.is_multiple_of(DUPLICATE_ACKS)
                && let Some(first) = st.in_flight.front_mut()
                && now.duration_since(first.sent_at)
                    >= st.srtt.map_or(MIN_RTO, |(srtt, _)| srtt * 2)
            {
                // The retransmission was lost as well.
                first.resend = true;
            }
        }
        st.last_ack = header.ack;
    }

    fn on_data(&self, st: &mut State, seq: u16, payload: &[u8], now: Instant, out: &mut Out) {
        let expected = st.ack_nr.wrapping_add(1);
        if seq == expected {
            if st.received.len() + payload.len() > RECV_WINDOW {
                // Beyond the advertised window: dropped, the peer resends.
                out.push(self.ack(st, now));
                return;
            }
            st.received.extend(payload);
            st.ack_nr = seq;
            while let Some(next) = st.reorder.remove(&st.ack_nr.wrapping_add(1)) {
                st.reorder_bytes -= next.len();
                st.received.extend(next);
                st.ack_nr = st.ack_nr.wrapping_add(1);
            }
            st.ack_owed += 1;
            if st.ack_owed >= 2 || !st.reorder.is_empty() {
                out.push(self.ack(st, now));
            }
        } else {
            if before(expected, seq)
                && seq.wrapping_sub(expected) < REORDER_SPAN
                && payload.len() <= st.window()
                && !st.reorder.contains_key(&seq)
            {
                st.reorder_bytes += payload.len();
                st.reorder.insert(seq, payload.to_vec());
            }
            // A gap or a duplicate: acknowledge at once.
            out.push(self.ack(st, now));
        }
    }

    fn on_packet(&self, st: &mut State, header: Header, payload: &[u8], now: Instant) -> Out {
        let mut out = Vec::new();
        if header.kind == ST_RESET {
            st.close(Some(ErrorKind::ConnectionReset));
            return out;
        }
        if matches!(st.phase, Phase::Closed(_)) {
            return out;
        }
        st.last_received = now;
        st.reply_micro = micros().wrapping_sub(header.timestamp);
        st.peer_window = header.window as usize;
        if st.phase == Phase::SynSent {
            if header.kind != ST_STATE {
                return out;
            }
            st.ack_nr = header.seq.wrapping_sub(1);
            st.phase = Phase::Connected;
        }
        if header.kind == ST_SYN {
            // The answer to a SYN was lost.
            out.push(self.ack(st, now));
            return out;
        }
        self.on_ack(st, &header, payload.is_empty(), now);
        match header.kind {
            ST_DATA if !payload.is_empty() => self.on_data(st, header.seq, payload, now, &mut out),
            ST_FIN => {
                st.fin_received.get_or_insert(header.seq);
                if (st.eof && st.ack_nr == header.seq)
                    || before(st.ack_nr.wrapping_add(1), header.seq)
                {
                    out.push(self.ack(st, now));
                }
            }
            _ => {}
        }
        if let Some(fin) = st.fin_received
            && !st.eof
            && st.ack_nr == fin.wrapping_sub(1)
        {
            st.ack_nr = fin;
            st.eof = true;
            out.push(self.ack(st, now));
        }
        if st.closing.is_some() && st.in_flight.is_empty() {
            st.close(None);
            return out;
        }
        self.flush(st, now, &mut out);
        out
    }

    fn tick(&self, st: &mut State, now: Instant) -> Out {
        let mut out = Vec::new();
        if matches!(st.phase, Phase::Closed(_)) {
            return out;
        }
        if st.rto_deadline.is_some_and(|at| now >= at) && !st.in_flight.is_empty() {
            st.timeouts += 1;
            if st.timeouts > MAX_TIMEOUTS {
                st.close(Some(ErrorKind::TimedOut));
                return out;
            }
            st.cwnd = PAYLOAD;
            st.slow_start = false;
            st.recovery = None;
            // A peer whose window stays shut is probed with one packet.
            st.peer_window = st.peer_window.max(PAYLOAD);
            for sent in &mut st.in_flight {
                sent.resend = true;
            }
            st.rto_deadline = Some(now + st.backed_off());
            self.flush(st, now, &mut out);
        }
        if st.phase == Phase::Connected {
            if st.ack_owed > 0 || now.duration_since(st.last_sent) >= KEEPALIVE {
                out.push(self.ack(st, now));
            }
            if now.duration_since(st.last_received) >= DEAD_AFTER {
                st.close(Some(ErrorKind::TimedOut));
            }
        }
        if st
            .closing
            .is_some_and(|at| now.duration_since(at) >= CLOSE_LINGER)
        {
            st.close(None);
        }
        out
    }
}

struct Shared {
    socket: Box<dyn NetworkDatagram + Sync>,
    /// The network the socket belongs to: streams refuse I/O once it is revoked.
    network: Arc<dyn Network>,
    conns: Mutex<BTreeMap<(SocketAddr, u16), Arc<Conn>>>,
    accepted: Mutex<VecDeque<Arc<Conn>>>,
    stop: Arc<AtomicBool>,
    dead: AtomicBool,
    routers: Arc<Mutex<Vec<SocketAddr>>>,
}

impl Shared {
    fn send(&self, packets: Out, to: SocketAddr) {
        for packet in packets {
            let _ = self.socket.send_to(&packet, to);
        }
    }

    fn admitted(&self) -> io::Result<()> {
        if self.dead.load(Ordering::Acquire) {
            return Err(ErrorKind::ConnectionAborted.into());
        }
        self.network
            .generation()
            .map(|_| ())
            .map_err(|_| ErrorKind::PermissionDenied.into())
    }

    fn conns(&self) -> MutexGuard<'_, BTreeMap<(SocketAddr, u16), Arc<Conn>>> {
        self.conns.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn unused_id(
        conns: &BTreeMap<(SocketAddr, u16), Arc<Conn>>,
        address: SocketAddr,
        mut recv_id: u16,
    ) -> u16 {
        while conns.values().any(|conn| {
            conn.uses_id(address, recv_id) || conn.uses_id(address, recv_id.wrapping_add(1))
        }) {
            recv_id = recv_id.wrapping_add(2);
        }
        recv_id
    }

    fn accepted(&self) -> MutexGuard<'_, VecDeque<Arc<Conn>>> {
        self.accepted.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset(&self, connection: u16, ack: u16, to: SocketAddr) {
        let header = Header {
            kind: ST_RESET,
            connection,
            timestamp: micros(),
            difference: 0,
            window: 0,
            seq: random_u16(),
            ack,
        };
        let _ = self.socket.send_to(&encode(&header, &[]), to);
    }

    fn incoming(&self, packet: &[u8], from: SocketAddr, now: Instant) {
        let Some((header, payload)) = parse(packet) else {
            return;
        };
        if header.kind == ST_SYN {
            let key = (from, header.connection.wrapping_add(1));
            let mut conns = self.conns();
            let existing = conns
                .get(&key)
                .filter(|conn| conn.send_id == header.connection)
                .cloned();
            let conn = match existing {
                Some(conn) => conn,
                None => {
                    if conns.len() >= MAX_CONNECTIONS
                        || self.accepted().len() >= MAX_ACCEPT_QUEUE
                        || conns.values().any(|conn| {
                            conn.uses_id(from, header.connection) || conn.uses_id(from, key.1)
                        })
                    {
                        drop(conns);
                        self.reset(header.connection, header.seq, from);
                        return;
                    }
                    let mut state = State::new(Phase::Connected, random_u16(), header.seq, now);
                    state.peer_window = header.window as usize;
                    state.reply_micro = micros().wrapping_sub(header.timestamp);
                    let conn = Arc::new(Conn {
                        remote: from,
                        send_id: header.connection,
                        recv_id: header.connection.wrapping_add(1),
                        state: Mutex::new(state),
                        changed: Condvar::new(),
                        dropped: AtomicBool::new(false),
                    });
                    conns.insert(key, conn.clone());
                    self.accepted().push_back(conn.clone());
                    conn
                }
            };
            drop(conns);
            let packet = conn.ack(&mut conn.lock(), now);
            self.send(vec![packet], from);
            return;
        }
        let conn = {
            let conns = self.conns();
            conns.get(&(from, header.connection)).cloned().or_else(|| {
                // A reset may name either connection id.
                (header.kind == ST_RESET)
                    .then(|| {
                        conns
                            .values()
                            .find(|c| c.remote == from && c.send_id == header.connection)
                            .cloned()
                    })
                    .flatten()
            })
        };
        let Some(conn) = conn else {
            if header.kind != ST_RESET {
                self.reset(header.connection, header.seq, from);
            }
            return;
        };
        let out = conn.on_packet(&mut conn.lock(), header, payload, now);
        conn.changed.notify_all();
        self.send(out, from);
    }

    fn tick(&self, now: Instant) {
        let conns: Vec<_> = self.conns().values().cloned().collect();
        for conn in &conns {
            let (out, closed) = {
                let mut st = conn.lock();
                let out = conn.tick(&mut st, now);
                (out, matches!(st.phase, Phase::Closed(_)))
            };
            // Waiters wake for what a tick changed: lost packets sent again
            // free the window, and a connection may have ended.
            if !out.is_empty() || closed {
                conn.changed.notify_all();
            }
            self.send(out, conn.remote);
        }
        // A connection that ended before it was accepted has no stream.
        self.accepted().retain(|conn| {
            let open = !matches!(conn.lock().phase, Phase::Closed(_));
            if !open {
                conn.dropped.store(true, Ordering::Release);
            }
            open
        });
        self.conns().retain(|_, conn| {
            !(conn.dropped.load(Ordering::Acquire) && matches!(conn.lock().phase, Phase::Closed(_)))
        });
    }

    /// Acknowledgements held back for a second packet are sent once the
    /// socket goes quiet.
    fn flush_acks(&self, now: Instant) {
        let conns: Vec<_> = self.conns().values().cloned().collect();
        for conn in conns {
            let mut st = conn.lock();
            if st.ack_owed > 0 && st.phase == Phase::Connected {
                let packet = conn.ack(&mut st, now);
                drop(st);
                self.send(vec![packet], conn.remote);
            }
        }
    }

    fn end(&self) {
        self.dead.store(true, Ordering::Release);
        for conn in self.conns().values() {
            conn.lock().close(Some(ErrorKind::ConnectionAborted));
            conn.changed.notify_all();
        }
    }
}

/// The BitTorrent UDP port: uTP connections and the DHT node.
pub(crate) struct Hub {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Hub {
    pub(crate) fn start(
        network: Arc<dyn Network>,
        socket: Box<dyn NetworkDatagram + Sync>,
        dht: bool,
        stop: Arc<AtomicBool>,
    ) -> Result<Arc<Self>> {
        let node = if dht { Some(Node::new()?) } else { None };
        let routers = Arc::new(Mutex::new(Vec::new()));
        if dht {
            let (network, stop, routers) = (network.clone(), stop.clone(), routers.clone());
            // Detached: a system lookup cannot be interrupted, and the hub
            // must keep serving while it runs.
            thread::spawn(move || dht_node::resolve_routers(&*network, &stop, &routers));
        }
        let shared = Arc::new(Shared {
            socket,
            network,
            conns: Mutex::new(BTreeMap::new()),
            accepted: Mutex::new(VecDeque::new()),
            stop,
            dead: AtomicBool::new(false),
            routers,
        });
        let thread = {
            let shared = shared.clone();
            thread::spawn(move || run(&shared, node))
        };
        Ok(Arc::new(Self {
            shared,
            thread: Mutex::new(Some(thread)),
        }))
    }

    /// Waits for the hub thread, which ends with the client's stop flag.
    pub(crate) fn join(&self) {
        let thread = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }

    /// Sends a SYN; the stream is usable once `wait_connected` says so.
    fn connect(&self, address: SocketAddr) -> io::Result<UtpStream> {
        self.shared.admitted()?;
        let now = Instant::now();
        let mut conns = self.shared.conns();
        if conns.len() >= MAX_CONNECTIONS {
            return Err(ErrorKind::OutOfMemory.into());
        }
        let recv_id = Shared::unused_id(&conns, address, random_u16());
        let conn = Arc::new(Conn {
            remote: address,
            send_id: recv_id.wrapping_add(1),
            recv_id,
            state: Mutex::new(State::new(Phase::SynSent, 1, 0, now)),
            changed: Condvar::new(),
            dropped: AtomicBool::new(false),
        });
        conns.insert((address, recv_id), conn.clone());
        drop(conns);
        let packet = conn.push(&mut conn.lock(), ST_SYN, Vec::new(), now);
        self.shared.send(vec![packet], address);
        Ok(UtpStream {
            conn,
            shared: self.shared.clone(),
        })
    }

    fn accept(&self) -> Option<(UtpStream, SocketAddr)> {
        let conn = self.shared.accepted().pop_front()?;
        let address = conn.remote;
        Some((
            UtpStream {
                conn,
                shared: self.shared.clone(),
            },
            address,
        ))
    }
}

fn run(shared: &Shared, mut node: Option<Node>) {
    let started = Instant::now();
    let mut buffer = [0; 2048];
    let mut next_tick = started + TICK;
    let mut timeout = None;
    while !shared.stop.load(Ordering::Acquire) {
        // Quick turns while connections need acknowledgements and timers.
        let wanted = if shared.conns().is_empty() {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(5)
        };
        if timeout != Some(wanted) {
            if shared.socket.set_read_timeout(Some(wanted)).is_err() {
                break;
            }
            timeout = Some(wanted);
        }
        match shared.socket.recv_from(&mut buffer) {
            Ok((n, from)) => {
                let (packet, now) = (&buffer[..n], Instant::now());
                if packet.first() == Some(&b'd') {
                    if let Some(node) = &mut node
                        && let Some(reply) = node.handle(packet, from, now)
                    {
                        let _ = shared.socket.send_to(&reply, from);
                    }
                } else {
                    shared.incoming(packet, from, now);
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                shared.flush_acks(Instant::now());
            }
            // An ICMP error for a vanished peer is not the socket's end.
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionRefused
                ) => {}
            Err(_) => break,
        }
        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + TICK;
            shared.tick(now);
            if let Some(node) = &mut node
                && now.duration_since(started) >= dht_node::FIRST_REFRESH
                && node.due(now)
            {
                let routers = shared
                    .routers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                for (query, target) in node.refresh(&routers, now) {
                    let _ = shared.socket.send_to(&query, target);
                }
            }
        }
    }
    shared.end();
}

/// One uTP connection as a byte stream.
pub(crate) struct UtpStream {
    conn: Arc<Conn>,
    shared: Arc<Shared>,
}

fn deadline(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|timeout| Instant::now().checked_add(timeout))
}

impl UtpStream {
    /// Whether the peer answered the SYN by `deadline`.
    fn wait_connected(&self, until: Instant) -> io::Result<bool> {
        let mut st = self.conn.lock();
        loop {
            match st.phase {
                Phase::Connected => return Ok(true),
                Phase::Closed(error) => {
                    return Err(error.unwrap_or(ErrorKind::ConnectionAborted).into());
                }
                Phase::SynSent => {}
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            st = self
                .conn
                .changed
                .wait_timeout(st, left.min(Duration::from_millis(10)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn wait<'a>(
        &'a self,
        st: MutexGuard<'a, State>,
        until: Option<Instant>,
    ) -> io::Result<MutexGuard<'a, State>> {
        let left = match until {
            Some(until) => until.saturating_duration_since(Instant::now()),
            None => Duration::from_millis(100),
        };
        if left.is_zero() {
            return Err(ErrorKind::WouldBlock.into());
        }
        Ok(self
            .conn
            .changed
            .wait_timeout(st, left.min(Duration::from_millis(100)))
            .unwrap_or_else(|e| e.into_inner())
            .0)
    }
}

impl Read for UtpStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.shared.admitted()?;
        let mut st = self.conn.lock();
        let until = deadline(st.read_timeout);
        loop {
            if !st.received.is_empty() {
                let before = st.window();
                let n = buffer.len().min(st.received.len());
                for (out, byte) in buffer.iter_mut().zip(st.received.drain(..n)) {
                    *out = byte;
                }
                // A window that was nearly shut reopens with an update.
                let update = (before < 2 * PAYLOAD
                    && st.window() >= 2 * PAYLOAD
                    && st.phase == Phase::Connected)
                    .then(|| self.conn.ack(&mut st, Instant::now()));
                drop(st);
                if let Some(update) = update {
                    self.shared.send(vec![update], self.conn.remote);
                }
                return Ok(n);
            }
            if st.eof {
                return Ok(0);
            }
            if let Phase::Closed(error) = st.phase {
                return match error {
                    None => Ok(0),
                    Some(kind) => Err(kind.into()),
                };
            }
            st = self.wait(st, until)?;
            self.shared.admitted()?;
        }
    }
}

impl Write for UtpStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        self.shared.admitted()?;
        let mut st = self.conn.lock();
        let until = deadline(st.write_timeout);
        loop {
            match st.phase {
                Phase::Closed(error) => {
                    return Err(error.unwrap_or(ErrorKind::BrokenPipe).into());
                }
                Phase::SynSent => return Err(ErrorKind::NotConnected.into()),
                Phase::Connected => {}
            }
            let now = Instant::now();
            let mut packets = Vec::new();
            let mut taken = 0;
            while taken < data.len() {
                let length = PAYLOAD.min(data.len() - taken);
                let flight = st.flight();
                // An empty pipe always takes one packet, probing a shut window.
                if flight > 0 && flight + length > st.send_window() {
                    break;
                }
                packets.push(self.conn.push(
                    &mut st,
                    ST_DATA,
                    data[taken..taken + length].to_vec(),
                    now,
                ));
                taken += length;
            }
            if taken > 0 {
                drop(st);
                self.shared.send(packets, self.conn.remote);
                return Ok(taken);
            }
            st = self.wait(st, until)?;
            self.shared.admitted()?;
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl NetworkStream for UtpStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        if timeout == Some(Duration::ZERO) {
            return Err(ErrorKind::InvalidInput.into());
        }
        self.conn.lock().read_timeout = timeout;
        Ok(())
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        if timeout == Some(Duration::ZERO) {
            return Err(ErrorKind::InvalidInput.into());
        }
        self.conn.lock().write_timeout = timeout;
        Ok(())
    }
}

impl Drop for UtpStream {
    fn drop(&mut self) {
        let mut st = self.conn.lock();
        let now = Instant::now();
        let fin = match st.phase {
            // Nothing was ever sent: no data to deliver, so a reset ends it
            // at once, as when a handshake is refused.
            Phase::Connected if st.closing.is_none() && !st.sent_data => {
                st.close(None);
                let reset = Header {
                    kind: ST_RESET,
                    ..self.conn.header(&st, ST_RESET, st.seq_nr)
                };
                Some(encode(&reset, &[]))
            }
            Phase::Connected if st.closing.is_none() => {
                st.closing = Some(now);
                Some(self.conn.push(&mut st, ST_FIN, Vec::new(), now))
            }
            Phase::SynSent => {
                st.close(Some(ErrorKind::ConnectionAborted));
                None
            }
            _ => None,
        };
        drop(st);
        self.conn.dropped.store(true, Ordering::Release);
        if let Some(fin) = fin {
            self.shared.send(vec![fin], self.conn.remote);
        }
    }
}

/// Inbound peers over TCP, then over uTP.
pub(crate) struct Listener {
    pub(crate) tcp: Box<dyn NetworkListener>,
    pub(crate) hub: Arc<Hub>,
}

impl NetworkListener for Listener {
    fn accept(&self) -> io::Result<(Box<dyn NetworkStream>, SocketAddr)> {
        match self.tcp.accept() {
            Err(e) if e.kind() == ErrorKind::WouldBlock => match self.hub.accept() {
                Some((stream, address)) => Ok((Box::new(stream), address)),
                None => Err(e),
            },
            other => other,
        }
    }

    fn port(&self) -> u16 {
        self.tcp.port()
    }
}

/// A network whose peer connections may use uTP; everything else is `inner`.
pub(crate) struct WithUtp {
    pub(crate) inner: Arc<dyn Network>,
    pub(crate) hub: Arc<Hub>,
}

impl Network for WithUtp {
    fn generation(&self) -> Result<Option<String>> {
        self.inner.generation()
    }

    fn publication(&self) -> Result<Box<dyn Publication + '_>> {
        self.inner.publication()
    }

    /// TCP first; uTP as well once TCP is slow or refused. The first to
    /// connect wins and the other is closed.
    fn connect(&self, address: SocketAddr, timeout: Duration) -> Result<Box<dyn NetworkStream>> {
        if !address.is_ipv4() {
            return self.inner.connect(address, timeout);
        }
        let until = Instant::now() + timeout;
        let (sender, tcp) = mpsc::channel();
        {
            let inner = self.inner.clone();
            thread::spawn(move || {
                let _ = sender.send(inner.connect(address, timeout));
            });
        }
        let mut tcp_error = match tcp.recv_timeout(UTP_AFTER.min(timeout)) {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => Some(error),
            Err(_) => None,
        };
        let utp = match self.hub.connect(address) {
            Ok(utp) => Some(utp),
            Err(_) if tcp_error.is_none() => None,
            Err(_) => return Err(tcp_error.unwrap_or_else(|| "Could not connect to peer".into())),
        };
        loop {
            if tcp_error.is_none() {
                match tcp.recv_timeout(Duration::from_millis(5)) {
                    Ok(Ok(stream)) => return Ok(stream),
                    Ok(Err(error)) => tcp_error = Some(error),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        tcp_error = Some("Could not connect to peer".into());
                    }
                }
            }
            let left = until.saturating_duration_since(Instant::now());
            let state = utp.as_ref().map(|stream| {
                stream.wait_connected(Instant::now() + left.min(Duration::from_millis(5)))
            });
            match state {
                Some(Ok(true)) => {
                    if let Some(utp) = utp {
                        return Ok(Box::new(utp));
                    }
                    break;
                }
                Some(Ok(false)) => {}
                Some(Err(_)) | None if tcp_error.is_some() => break,
                Some(Err(_)) => {
                    // TCP alone is left.
                    return match tcp.recv_timeout(left) {
                        Ok(result) => result,
                        Err(_) => Err("Could not connect to peer".into()),
                    };
                }
                None => {}
            }
            if left.is_zero() {
                break;
            }
        }
        Err(tcp_error.unwrap_or_else(|| "Could not connect to peer".into()))
    }

    fn resolve(&self, authority: &str, deadline: Instant) -> Result<Vec<SocketAddr>> {
        self.inner.resolve(authority, deadline)
    }

    fn datagram(&self, ipv6: bool) -> Result<Box<dyn NetworkDatagram>> {
        self.inner.datagram(ipv6)
    }

    fn bind_datagram(&self, port: u16) -> Result<Box<dyn NetworkDatagram + Sync>> {
        self.inner.bind_datagram(port)
    }

    fn http(&self) -> HttpClient {
        self.inner.http()
    }

    fn listen(&self, address: SocketAddr) -> Result<Box<dyn NetworkListener>> {
        self.inner.listen(address)
    }
}

/// Feeds length-prefixed packets to one connection with data in flight, a
/// timer tick between each, for `examples/parser_fuzz.rs`.
#[doc(hidden)]
pub(crate) fn fuzz(data: &[u8]) {
    let mut now = Instant::now();
    let conn = Conn {
        remote: SocketAddr::from(([192, 0, 2, 1], 6881)),
        send_id: 7,
        recv_id: 8,
        state: Mutex::new(State::new(Phase::Connected, 100, 41, now)),
        changed: Condvar::new(),
        dropped: AtomicBool::new(false),
    };
    let mut st = conn.lock();
    for _ in 0..4 {
        conn.push(&mut st, ST_DATA, vec![0; 100], now);
    }
    let mut rest = data;
    while let Some((&length, tail)) = rest.split_first() {
        let (packet, next) = tail.split_at(usize::from(length).min(tail.len()));
        rest = next;
        if let Some((header, payload)) = parse(packet) {
            let _ = conn.on_packet(&mut st, header, payload, now);
        }
        now += Duration::from_millis(300);
        let _ = conn.tick(&mut st, now);
        let read = st.received.len().min(4096);
        st.received.drain(..read);
    }
}

#[cfg(test)]
#[path = "utp_tests.rs"]
mod tests;
