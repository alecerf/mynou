//! Bounded DNS stub client over virtual UDP with virtual TCP fallback. It asks configured inner resolver
//! IPv4 addresses only, for A records, with one virtual socket per query. There
//! is no host resolver, search list or cache. TCP shares the caller's bounded flow table.

use super::ipv4::{check_mtu, unroutable};
use super::sockets::{Handle, SocketError, UdpSockets};
use super::{Fence, FlowHandle, Ipv4Config, Read, TcpError, TcpFailure, TcpStack};
use crate::wireguard::Entropy;
use std::net::Ipv4Addr;
use std::time::Duration;

pub const PORT: u16 = 53;
pub const MAX_NAME: usize = 253;
pub const MAX_RESOLVERS: usize = 3;
/// Addresses kept from one answer; further ones are ignored.
pub const MAX_ADDRESSES: usize = 8;
/// Response limit for either transport, before allocation from a TCP prefix.
pub const MAX_DNS_MESSAGE: usize = 4096;
const MAX_LABEL: usize = 63;
const MAX_WIRE_NAME: usize = 255;
const MAX_POINTERS: usize = 16;
const MAX_RECORDS: usize = 32;
const MAX_ALIASES: usize = 8;
const TYPE_A: u16 = 1;
const TYPE_CNAME: u16 = 5;
const CLASS_IN: u16 = 1;
const TXID_ATTEMPTS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplyError {
    Truncated,
    Id,
    Header,
    Question,
    /// A name longer than 255 wire bytes.
    Name,
    /// A reserved label type or a label overrunning the message.
    Label,
    /// A compression pointer that is not strictly backward, or too many.
    Pointer,
    Records,
    Rdata,
    /// Any answer record other than A and CNAME, so also AAAA.
    Type,
    Class,
    TooLarge,
}

/// What one validated reply says.
#[derive(PartialEq, Eq, Debug)]
pub enum Reply {
    Addresses {
        addresses: Vec<Ipv4Addr>,
        ttl: u32,
    },
    NameError,
    Failure(u8),
    /// The TC bit: retry over virtual TCP.
    Truncated,
    NoAddress,
}

/// Reads a possibly compressed name into lowercase wire form (length-prefixed
/// labels and a final zero) and returns the offset after its first encoding.
/// Pointers must target strictly lower offsets than anything read so far, which
/// rules out loops without a visited set.
fn read_name(message: &[u8], start: usize) -> Result<(Vec<u8>, usize), ReplyError> {
    let mut name = Vec::new();
    let mut position = start;
    let mut resume = None;
    let mut bound = start;
    let mut pointers = 0;
    loop {
        let length = *message.get(position).ok_or(ReplyError::Truncated)?;
        match length & 0xc0 {
            0 => {
                let length = usize::from(length);
                if length == 0 {
                    name.push(0);
                    return Ok((name, resume.unwrap_or(position + 1)));
                }
                let label = message
                    .get(position + 1..position + 1 + length)
                    .ok_or(ReplyError::Label)?;
                if name.len() + length + 2 > MAX_WIRE_NAME {
                    return Err(ReplyError::Name);
                }
                name.push(length as u8);
                name.extend(label.iter().map(u8::to_ascii_lowercase));
                position += 1 + length;
            }
            0xc0 => {
                let low = *message.get(position + 1).ok_or(ReplyError::Truncated)?;
                let target = (usize::from(length & 0x3f) << 8) | usize::from(low);
                resume = resume.or(Some(position + 2));
                pointers += 1;
                if target >= bound || pointers > MAX_POINTERS {
                    return Err(ReplyError::Pointer);
                }
                bound = target;
                position = target;
            }
            _ => return Err(ReplyError::Label),
        }
    }
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

/// Validates one datagram against the pending query: transaction id, response
/// flags, exactly the asked question (`question` is its lowercase wire name), then
/// the answers. Only A records (and CNAMEs leading to them) owned by the asked
/// name or an alias are used; unrelated owners are ignored, any other type is an
/// error. Addresses that are never valid unicast peers are dropped.
pub fn parse_response(id: u16, question: &[u8], message: &[u8]) -> Result<Reply, ReplyError> {
    if message.len() > MAX_DNS_MESSAGE {
        return Err(ReplyError::TooLarge);
    }
    if message.len() < 12 {
        return Err(ReplyError::Truncated);
    }
    if be16(message) != id {
        return Err(ReplyError::Id);
    }
    let flags = be16(&message[2..]);
    if flags & 0x8000 == 0 || (flags >> 11) & 0xf != 0 {
        return Err(ReplyError::Header);
    }
    if be16(&message[4..]) != 1 {
        return Err(ReplyError::Question);
    }
    let answers = usize::from(be16(&message[6..]));
    let (asked, mut position) = read_name(message, 12)?;
    if asked != question {
        return Err(ReplyError::Question);
    }
    let tail = message
        .get(position..position + 4)
        .ok_or(ReplyError::Truncated)?;
    if tail != [0u8, 1, 0, 1] {
        return Err(ReplyError::Question);
    }
    position += 4;
    if flags & 0x0200 != 0 {
        return Ok(Reply::Truncated);
    }
    match flags & 0xf {
        0 => {}
        3 => return Ok(Reply::NameError),
        code => return Ok(Reply::Failure(code as u8)),
    }
    if answers > MAX_RECORDS {
        return Err(ReplyError::Records);
    }
    let mut owners = vec![question.to_vec()];
    let mut addresses = Vec::new();
    let mut ttl: Option<u32> = None;
    for _ in 0..answers {
        let (owner, next) = read_name(message, position)?;
        let header = message.get(next..next + 10).ok_or(ReplyError::Truncated)?;
        let start = next + 10;
        let end = start + usize::from(be16(&header[8..]));
        let data = message.get(start..end).ok_or(ReplyError::Truncated)?;
        position = end;
        if be16(&header[2..]) != CLASS_IN {
            return Err(ReplyError::Class);
        }
        // RFC 2181 section 8: a TTL with the top bit set is treated as zero.
        let record_ttl = match u32::from_be_bytes([header[4], header[5], header[6], header[7]]) {
            value if value > 0x7fff_ffff => 0,
            value => value,
        };
        match be16(header) {
            TYPE_A => {
                if data.len() != 4 {
                    return Err(ReplyError::Rdata);
                }
                let address = Ipv4Addr::new(data[0], data[1], data[2], data[3]);
                if owners.contains(&owner)
                    && !unroutable(address)
                    && !addresses.contains(&address)
                    && addresses.len() < MAX_ADDRESSES
                {
                    addresses.push(address);
                    ttl = Some(ttl.map_or(record_ttl, |ttl| ttl.min(record_ttl)));
                }
            }
            TYPE_CNAME => {
                let (target, after) = read_name(message, start)?;
                if after != end {
                    return Err(ReplyError::Rdata);
                }
                if owners.contains(&owner) {
                    if owners.len() >= MAX_ALIASES {
                        return Err(ReplyError::Records);
                    }
                    owners.push(target);
                    ttl = Some(ttl.map_or(record_ttl, |ttl| ttl.min(record_ttl)));
                }
            }
            _ => return Err(ReplyError::Type),
        }
    }
    match ttl {
        Some(ttl) if !addresses.is_empty() => Ok(Reply::Addresses { addresses, ttl }),
        _ => Ok(Reply::NoAddress),
    }
}

/// ASCII letters, digits, hyphen and underscore labels; a single trailing dot is
/// accepted. Case is preserved on the wire. No IDNA, no empty labels.
fn encode_name(name: &str) -> Result<Vec<u8>, DnsError> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(DnsError::BadName);
    }
    let mut wire = Vec::with_capacity(name.len() + 2);
    for label in name.split('.') {
        let bytes = label.as_bytes();
        let valid = bytes
            .iter()
            .all(|&byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        if bytes.is_empty() || bytes.len() > MAX_LABEL || !valid {
            return Err(DnsError::BadName);
        }
        wire.push(bytes.len() as u8);
        wire.extend_from_slice(bytes);
    }
    wire.push(0);
    Ok(wire)
}

fn query_message(txid: u16, wire: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(12 + wire.len() + 4);
    message.extend_from_slice(&txid.to_be_bytes());
    message.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    message.extend_from_slice(wire);
    message.extend_from_slice(&[0, 1, 0, 1]);
    message
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DnsConfig {
    pub ip: Ipv4Config,
    pub resolvers: Vec<Ipv4Addr>,
    pub mtu: usize,
    /// Spacing between attempts; resolvers are tried in turn.
    pub retry_interval: Duration,
    pub attempts: u32,
    pub max_pending: usize,
    /// Answers are never trusted for longer than this.
    pub max_ttl: Duration,
}

impl DnsConfig {
    pub fn new(ip: Ipv4Config, resolvers: Vec<Ipv4Addr>, mtu: usize) -> Self {
        Self {
            ip,
            resolvers,
            mtu,
            retry_interval: Duration::from_secs(2),
            attempts: 3,
            max_pending: 16,
            max_ttl: Duration::from_secs(3600),
        }
    }

    fn validate(&self) -> Result<(), DnsError> {
        let resolvers = &self.resolvers;
        let distinct = resolvers
            .iter()
            .enumerate()
            .all(|(index, resolver)| !resolvers[..index].contains(resolver));
        let valid = (1..=MAX_RESOLVERS).contains(&resolvers.len())
            && distinct
            && !resolvers.iter().any(|&resolver| unroutable(resolver))
            && check_mtu(self.mtu).is_ok()
            && (1..=8).contains(&self.attempts)
            && (1..=64).contains(&self.max_pending)
            && !self.retry_interval.is_zero()
            && self.retry_interval <= Duration::from_secs(60);
        if valid {
            Ok(())
        } else {
            Err(DnsError::BadConfig)
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum DnsError {
    Revoked,
    BadConfig,
    BadName,
    /// Too many pending queries, or no free transaction id.
    Busy,
    Entropy(String),
    Socket(SocketError),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct QueryId(u64);

#[derive(PartialEq, Eq, Debug)]
pub struct Answer {
    pub addresses: Vec<Ipv4Addr>,
    pub ttl: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Failure {
    Timeout,
    NameError,
    ServerFailure(u8),
    /// The resolver still set TC on its TCP response.
    Truncated,
    NoAddress,
    /// A local virtual-socket fault (a full flow table, entropy) or a TCP
    /// response cut short by EOF.
    Aborted,
    /// Validated ICMP destination-unreachable (this code) answered the last
    /// UDP attempt, where earlier unreachable attempts moved on to the next
    /// resolver, or the TCP fallback connection.
    Unreachable(u8),
    /// The resolver answered the TCP fallback with an RST, to the SYN or later.
    Refused,
    /// A TCP response length exceeded the fixed message budget.
    TooLarge,
}

#[derive(PartialEq, Eq, Debug)]
pub struct Finished {
    pub id: QueryId,
    pub result: Result<Answer, Failure>,
}

/// One drive step: inner packets to send, finished queries and the next instant
/// at which polling again can change something.
#[derive(PartialEq, Eq, Debug, Default)]
pub struct Poll {
    pub send: Vec<Vec<u8>>,
    pub finished: Vec<Finished>,
    pub next: Option<Duration>,
}

struct Pending {
    id: QueryId,
    txid: u16,
    wire: Vec<u8>,
    message: Vec<u8>,
    socket: Handle,
    sent: u32,
    next: Duration,
    tcp: Option<TcpQuery>,
}

struct TcpQuery {
    flow: FlowHandle,
    deadline: Duration,
    sent: usize,
    received: Vec<u8>,
    expected: Option<usize>,
}

enum Step {
    Pending,
    Done(Result<Answer, Failure>),
    Revoked,
}

pub struct DnsClient<E: Entropy> {
    config: DnsConfig,
    entropy: E,
    fence: Fence,
    pending: Vec<Pending>,
    queries: u64,
    rejected: u64,
}

impl<E: Entropy> DnsClient<E> {
    pub fn new(
        config: DnsConfig,
        entropy: E,
        generation: u64,
        now: Duration,
    ) -> Result<Self, DnsError> {
        config.validate()?;
        Ok(Self {
            config,
            entropy,
            fence: Fence::new(generation, now),
            pending: Vec::new(),
            queries: 0,
            rejected: 0,
        })
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Responses dropped for wrong source, id, question or malformed content.
    pub fn rejected(&self) -> u64 {
        self.rejected
    }

    fn admit(&mut self, generation: u64, now: Duration) -> Result<(), DnsError> {
        if self.fence.admit(generation, now) {
            Ok(())
        } else {
            self.pending.clear();
            Err(DnsError::Revoked)
        }
    }

    /// Irreversibly forgets every query. Revoke both the UDP socket table and
    /// TCP stack as well; the caller owns those transports.
    pub fn revoke(&mut self) {
        self.fence.revoke();
        self.pending.clear();
    }

    fn lift(&mut self, error: SocketError) -> DnsError {
        if error == SocketError::Revoked {
            self.revoke();
            DnsError::Revoked
        } else {
            DnsError::Socket(error)
        }
    }

    fn transaction(&mut self) -> Result<u16, DnsError> {
        for _ in 0..TXID_ATTEMPTS {
            let raw = self.entropy.bytes::<2>().map_err(DnsError::Entropy)?;
            let txid = u16::from_be_bytes(raw);
            if self.pending.iter().all(|query| query.txid != txid) {
                return Ok(txid);
            }
        }
        Err(DnsError::Busy)
    }

    fn transmit(
        &self,
        sockets: &mut UdpSockets,
        generation: u64,
        socket: Handle,
        resolver: Ipv4Addr,
        message: &[u8],
    ) -> Result<Vec<u8>, SocketError> {
        let remote = (resolver, PORT);
        sockets.connect(generation, socket, remote)?;
        sockets.send(
            generation,
            socket,
            self.config.ip.local(),
            remote,
            message,
            self.config.mtu,
        )
    }

    /// Starts a query: validates the name, takes a fresh random transaction id and
    /// source port, and returns the first inner packet to send.
    pub fn start(
        &mut self,
        sockets: &mut UdpSockets,
        generation: u64,
        name: &str,
        now: Duration,
    ) -> Result<(QueryId, Vec<u8>), DnsError> {
        self.admit(generation, now)?;
        if self.pending.len() >= self.config.max_pending {
            return Err(DnsError::Busy);
        }
        let wire = encode_name(name)?;
        let txid = self.transaction()?;
        let bound = sockets.bind(generation, 0, &mut self.entropy);
        let socket = bound.map_err(|error| self.lift(error))?;
        let message = query_message(txid, &wire);
        let resolver = self.config.resolvers[0];
        let sent = self.transmit(sockets, generation, socket, resolver, &message);
        let packet = match sent {
            Ok(packet) => packet,
            Err(error) => {
                let _ = sockets.close(generation, socket);
                return Err(self.lift(error));
            }
        };
        self.queries = self.queries.saturating_add(1);
        let id = QueryId(self.queries);
        self.pending.push(Pending {
            id,
            txid,
            wire: wire.to_ascii_lowercase(),
            message,
            socket,
            sent: 1,
            next: now.saturating_add(self.config.retry_interval),
            tcp: None,
        });
        Ok((id, packet))
    }

    /// Abandons a query and frees its UDP socket and any TCP flow. `false` if it
    /// already finished.
    pub fn cancel(
        &mut self,
        sockets: &mut UdpSockets,
        tcp: &mut TcpStack<impl Entropy>,
        generation: u64,
        id: QueryId,
    ) -> Result<bool, DnsError> {
        if !self.fence.admit_generation(generation) {
            self.pending.clear();
            return Err(DnsError::Revoked);
        }
        let Some(index) = self.pending.iter().position(|query| query.id == id) else {
            return Ok(false);
        };
        let query = self.pending.remove(index);
        if let Some(stream) = query.tcp {
            let _ = tcp.abort(generation, stream.flow);
        }
        let _ = sockets.close(generation, query.socket);
        Ok(true)
    }

    /// Reads delivered replies, finishes answered or timed-out queries and
    /// retransmits due UDP attempts to the next resolver. A validated TC reply
    /// starts one TCP attempt; poll the shared TCP stack after this call.
    pub fn poll(
        &mut self,
        sockets: &mut UdpSockets,
        tcp: &mut TcpStack<impl Entropy>,
        generation: u64,
        now: Duration,
    ) -> Result<Poll, DnsError> {
        self.admit(generation, now)?;
        let mut poll = Poll::default();
        let mut keep = Vec::new();
        for mut query in std::mem::take(&mut self.pending) {
            match self.advance(sockets, tcp, generation, &mut query, now, &mut poll.send) {
                Step::Pending => keep.push(query),
                Step::Done(result) => {
                    if let Some(stream) = query.tcp {
                        let _ = tcp.abort(generation, stream.flow);
                    }
                    let _ = sockets.close(generation, query.socket);
                    poll.finished.push(Finished {
                        id: query.id,
                        result,
                    });
                }
                Step::Revoked => {
                    self.revoke();
                    return Err(DnsError::Revoked);
                }
            }
        }
        self.pending = keep;
        poll.next = self.pending.iter().map(|query| query.next).min();
        Ok(poll)
    }

    fn advance(
        &mut self,
        sockets: &mut UdpSockets,
        tcp: &mut TcpStack<impl Entropy>,
        generation: u64,
        query: &mut Pending,
        now: Duration,
        send: &mut Vec<Vec<u8>>,
    ) -> Step {
        if query.tcp.is_some() {
            return self.advance_tcp(tcp, generation, query, now);
        }
        let mut unreachable = None;
        loop {
            match sockets.recv(generation, query.socket) {
                Ok(Some(datagram)) => {
                    let known = self.config.resolvers.contains(&datagram.source);
                    if !known || datagram.source_port != PORT {
                        self.rejected = self.rejected.saturating_add(1);
                        continue;
                    }
                    match parse_response(query.txid, &query.wire, &datagram.payload) {
                        Ok(Reply::Truncated) => {
                            return self.start_tcp(tcp, generation, query, datagram.source, now);
                        }
                        Ok(reply) => return Step::Done(self.finish(reply)),
                        Err(_) => self.rejected = self.rejected.saturating_add(1),
                    }
                }
                Ok(None) => break,
                Err(SocketError::Revoked) => return Step::Revoked,
                // ICMP ends only the current attempt: the next one is due now,
                // within the same attempt budget.
                Err(SocketError::Unreachable(code)) => {
                    unreachable = Some(code);
                    query.next = now;
                }
                Err(_) => return Step::Done(Err(Failure::Aborted)),
            }
        }
        if now < query.next {
            return Step::Pending;
        }
        if query.sent >= self.config.attempts {
            let failure = unreachable.map_or(Failure::Timeout, Failure::Unreachable);
            return Step::Done(Err(failure));
        }
        let resolver = self.config.resolvers[query.sent as usize % self.config.resolvers.len()];
        match self.transmit(sockets, generation, query.socket, resolver, &query.message) {
            Ok(packet) => {
                send.push(packet);
                query.sent = query.sent.saturating_add(1);
                query.next = now.saturating_add(self.config.retry_interval);
                Step::Pending
            }
            Err(SocketError::Revoked) => Step::Revoked,
            Err(_) => Step::Done(Err(Failure::Aborted)),
        }
    }

    fn start_tcp(
        &mut self,
        tcp: &mut TcpStack<impl Entropy>,
        generation: u64,
        query: &mut Pending,
        resolver: Ipv4Addr,
        now: Duration,
    ) -> Step {
        let flow = match tcp.connect(generation, now, resolver, PORT) {
            Ok(flow) => flow,
            Err(TcpError::Revoked) => return Step::Revoked,
            Err(_) => return Step::Done(Err(Failure::Aborted)),
        };
        // One TCP attempt to the resolver that set TC, under a fixed total
        // deadline. Neither incoming bytes nor invalid frames extend it.
        let deadline = now.saturating_add(
            self.config
                .retry_interval
                .saturating_mul(self.config.attempts),
        );
        query.next = deadline;
        query.tcp = Some(TcpQuery {
            flow,
            deadline,
            sent: 0,
            received: Vec::with_capacity(MAX_DNS_MESSAGE + 2),
            expected: None,
        });
        Step::Pending
    }

    fn advance_tcp(
        &mut self,
        tcp: &mut TcpStack<impl Entropy>,
        generation: u64,
        query: &mut Pending,
        now: Duration,
    ) -> Step {
        let stream = query.tcp.as_mut().expect("TCP query");
        if now >= stream.deadline {
            return Step::Done(Err(Failure::Timeout));
        }
        query.next = stream.deadline;
        let mut request = (query.message.len() as u16).to_be_bytes().to_vec();
        request.extend_from_slice(&query.message);
        if stream.sent < request.len() {
            match tcp.send(generation, stream.flow, &request[stream.sent..]) {
                Ok(count) => stream.sent += count,
                Err(TcpError::NotConnected) => return Step::Pending,
                Err(error) => return tcp_step(error),
            }
        }
        // At most one framed response per poll, and two bounded reads: prefix
        // then body. A forged ID/question or malformed response is discarded,
        // never a reason to accept data or fail the pending query.
        for _ in 0..2 {
            let target = stream.expected.map_or(2, |length| length + 2);
            let mut bytes = [0; MAX_DNS_MESSAGE + 2];
            match tcp.recv(
                generation,
                stream.flow,
                &mut bytes[..target - stream.received.len()],
            ) {
                Ok(Read::Data(count)) => stream.received.extend_from_slice(&bytes[..count]),
                Ok(Read::WouldBlock) => return Step::Pending,
                Ok(Read::Eof) => return Step::Done(Err(Failure::Aborted)),
                Err(error) => return tcp_step(error),
            }
            if stream.received.len() != target {
                return Step::Pending;
            }
            if stream.expected.is_none() {
                let length = usize::from(be16(&stream.received));
                if length > MAX_DNS_MESSAGE {
                    return Step::Done(Err(Failure::TooLarge));
                }
                if length == 0 {
                    self.rejected = self.rejected.saturating_add(1);
                    stream.received.clear();
                    query.next = now;
                    return Step::Pending;
                }
                stream.expected = Some(length);
            } else {
                match parse_response(query.txid, &query.wire, &stream.received[2..]) {
                    Ok(reply) => return Step::Done(self.finish(reply)),
                    Err(_) => {
                        self.rejected = self.rejected.saturating_add(1);
                        stream.received.clear();
                        stream.expected = None;
                        query.next = now;
                        return Step::Pending;
                    }
                }
            }
        }
        Step::Pending
    }

    fn finish(&self, reply: Reply) -> Result<Answer, Failure> {
        match reply {
            Reply::Addresses { addresses, ttl } => Ok(Answer {
                addresses,
                ttl: Duration::from_secs(u64::from(ttl)).min(self.config.max_ttl),
            }),
            Reply::NameError => Err(Failure::NameError),
            Reply::Failure(code) => Err(Failure::ServerFailure(code)),
            Reply::Truncated => Err(Failure::Truncated),
            Reply::NoAddress => Err(Failure::NoAddress),
        }
    }
}

/// Reports why the TCP fallback flow ended. The resolver's own refusal, ICMP
/// advice and TCP retransmission limits keep their meaning; anything else is a
/// local fault.
fn tcp_step(error: TcpError) -> Step {
    let failure = match error {
        TcpError::Revoked => return Step::Revoked,
        TcpError::Failed(TcpFailure::Unreachable(code)) => Failure::Unreachable(code),
        TcpError::Failed(TcpFailure::Refused | TcpFailure::Reset) => Failure::Refused,
        TcpError::Failed(TcpFailure::Timeout) => Failure::Timeout,
        _ => Failure::Aborted,
    };
    Step::Done(Err(failure))
}

#[cfg(test)]
#[path = "dns_tests.rs"]
mod tests;
