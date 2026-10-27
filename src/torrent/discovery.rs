use super::transport::Network;
use crate::{Result, bencode::Value, net::NetworkDatagram};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub fn decode_peers(data: &[u8], ipv6: bool) -> Result<Vec<SocketAddr>> {
    let width = if ipv6 { 18 } else { 6 };
    if !data.len().is_multiple_of(width) || data.len() / width > 10_000 {
        return Err("Invalid compact peer list".into());
    }
    let mut out = Vec::new();
    for p in data.chunks_exact(width) {
        let ip = if ipv6 {
            let mut a = [0; 16];
            a.copy_from_slice(&p[..16]);
            IpAddr::V6(Ipv6Addr::from(a))
        } else {
            IpAddr::V4(Ipv4Addr::new(p[0], p[1], p[2], p[3]))
        };
        let port = u16::from_be_bytes([p[width - 2], p[width - 1]]);
        if port != 0 && !ip.is_unspecified() && !ip.is_multicast() {
            out.push(SocketAddr::new(ip, port));
        }
    }
    Ok(out)
}
fn dict(items: &[(&[u8], Value)]) -> Value {
    Value::Dict(items.iter().map(|(k, v)| (k.to_vec(), v.clone())).collect())
}
fn value_bytes(v: Option<&Value>) -> Option<&[u8]> {
    match v {
        Some(Value::Bytes(b)) => Some(b),
        _ => None,
    }
}
fn field<'a>(v: &'a Value, k: &[u8]) -> Option<&'a Value> {
    match v {
        Value::Dict(d) => d.get(k),
        _ => None,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackerEvent {
    None,
    Started,
    Completed,
    Stopped,
}

impl TrackerEvent {
    fn query_name(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Started => Some("started"),
            Self::Completed => Some("completed"),
            Self::Stopped => Some("stopped"),
        }
    }
    fn udp_code(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Completed => 1,
            Self::Started => 2,
            Self::Stopped => 3,
        }
    }
}

pub struct TrackerRequest<'a> {
    pub hash: &'a [u8; 20],
    pub peer_id: &'a [u8; 20],
    pub port: u16,
    pub left: u64,
    pub uploaded: u64,
    pub downloaded: u64,
    pub event: TrackerEvent,
}

#[derive(Clone, Debug)]
pub struct TrackerReply {
    pub peers: Vec<SocketAddr>,
    pub interval: Duration,
    /// Swarm counts the tracker reported, when it did.
    pub seeders: Option<u64>,
    pub leechers: Option<u64>,
}

pub const DEFAULT_TRACKER_INTERVAL: Duration = Duration::from_secs(1800);

fn bounded_interval(seconds: u64) -> Duration {
    Duration::from_secs(seconds.clamp(30, 86_400))
}

fn session_key(request: &TrackerRequest<'_>) -> u32 {
    // The random peer ID is stable during one Client lifetime; combine it with
    // the swarm identity so HTTP and UDP announcements retain the same key.
    let mut material = [0u8; 40];
    material[..20].copy_from_slice(request.peer_id);
    material[20..].copy_from_slice(request.hash);
    let digest = crate::crypto::sha256(&material);
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
}

pub fn query_url(url: &str, request: &TrackerRequest<'_>) -> String {
    fn escape(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 3);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
        out
    }
    let url = url.split('#').next().unwrap_or(url);
    let separator = if url.ends_with(['?', '&']) {
        ""
    } else if url.contains('?') {
        "&"
    } else {
        "?"
    };
    let mut query = format!(
        "{url}{separator}info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant=100",
        escape(request.hash),
        escape(request.peer_id),
        request.port,
        request.uploaded,
        request.downloaded,
        request.left,
    );
    use std::fmt::Write;
    let _ = write!(query, "&key={:08X}", session_key(request));
    if let Some(event) = request.event.query_name() {
        query.push_str("&event=");
        query.push_str(event);
    }
    query
}

pub fn tracker(
    network: &dyn Network,
    url: &str,
    request: &TrackerRequest<'_>,
) -> Result<TrackerReply> {
    tracker_before(
        network,
        url,
        request,
        Instant::now() + Duration::from_secs(8),
    )
}

pub fn tracker_before(
    network: &dyn Network,
    url: &str,
    request: &TrackerRequest<'_>,
    deadline: Instant,
) -> Result<TrackerReply> {
    if url.len() > 8192 || url.chars().any(char::is_control) {
        return Err("Tracker URL is too long or contains control characters".into());
    }
    if request.port == 0 {
        return Err("Tracker announcement cannot use port zero".into());
    }
    if url.starts_with("udp://") {
        return udp_tracker(network, url, request, deadline);
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("Unsupported tracker protocol".into());
    }
    let response = network
        .http()
        .with_timeout(
            deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or("Tracker deadline exceeded")?
                .min(Duration::from_secs(5)),
        )
        .with_max_body(1024 * 1024)
        .get(&query_url(url, request))?;
    if response.status != 200 {
        return Err("Tracker rejected request".into());
    }
    let value = crate::bencode::parse(&response.body)?;
    parse_tracker_reply(&value)
}

fn parse_tracker_reply(value: &Value) -> Result<TrackerReply> {
    if !matches!(value, Value::Dict(_)) {
        return Err("Tracker response must be a dictionary".into());
    }
    if field(value, b"failure reason").is_some() {
        return Err("Tracker rejected announcement".into());
    }
    let seconds = |key: &[u8]| -> Result<Option<u64>> {
        match field(value, key) {
            Some(Value::Int(n)) => u64::try_from(*n)
                .map(Some)
                .map_err(|_| "Negative tracker interval".into()),
            Some(_) => Err("Invalid tracker interval".into()),
            None => Ok(None),
        }
    };
    let interval = seconds(b"interval")?.unwrap_or(DEFAULT_TRACKER_INTERVAL.as_secs());
    let interval = bounded_interval(interval.max(seconds(b"min interval")?.unwrap_or(0)));
    let mut peers = Vec::new();
    if let Some(p) = value_bytes(field(value, b"peers")) {
        peers.extend(decode_peers(p, false)?);
    } else if let Some(Value::List(list)) = field(value, b"peers") {
        if list.len() > 10_000 {
            return Err("Too many tracker peers".into());
        }
        for p in list {
            let ip = value_bytes(field(p, b"ip"))
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|v| v.parse::<IpAddr>().ok());
            let port = match field(p, b"port") {
                Some(Value::Int(n)) => u16::try_from(*n).ok(),
                _ => None,
            };
            if let (Some(ip), Some(port)) = (ip, port)
                && port != 0
                && !ip.is_unspecified()
                && !ip.is_multicast()
            {
                peers.push(SocketAddr::new(ip, port));
            }
        }
    } else if field(value, b"peers").is_some() {
        return Err("Invalid tracker peer list".into());
    }
    if let Some(p) = value_bytes(field(value, b"peers6")) {
        peers.extend(decode_peers(p, true)?);
    } else if field(value, b"peers6").is_some() {
        return Err("Invalid tracker IPv6 peer list".into());
    }
    peers.sort_unstable();
    peers.dedup();
    // Counts are informative only: a malformed one is ignored, not an error.
    let count = |key: &[u8]| match field(value, key) {
        Some(Value::Int(n)) => u64::try_from(*n).ok(),
        _ => None,
    };
    Ok(TrackerReply {
        peers,
        interval,
        seeders: count(b"complete"),
        leechers: count(b"incomplete"),
    })
}

fn nonce() -> Result<u32> {
    Ok(u32::from_be_bytes(crate::crypto::random_bytes::<4>()?))
}
fn udp_exchange(
    socket: &dyn NetworkDatagram,
    address: SocketAddr,
    packet: &[u8],
    action: u32,
    tx: u32,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut buf = [0; 65_507];
    for _ in 0..2 {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or("Tracker deadline exceeded")?;
        socket
            .set_read_timeout(Some(remaining.min(Duration::from_secs(2))))
            .map_err(|_| "Could not configure UDP timeout")?;
        socket
            .set_write_timeout(Some(remaining.min(Duration::from_secs(2))))
            .map_err(|_| "Could not configure UDP timeout")?;
        socket
            .send_to(packet, address)
            .map_err(|_| "Could not send UDP tracker request")?;
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == address && n >= 8 && buf[4..8] == tx.to_be_bytes() => {
                if u32::from_be_bytes(buf[..4].try_into().map_err(|_| "Invalid UDP response")?) == 3
                {
                    return Err("UDP tracker rejected announcement".into());
                }
                if buf[..4] != action.to_be_bytes() {
                    return Err("Unexpected UDP tracker response".into());
                }
                return Ok(buf[..n].to_vec());
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    Err("UDP tracker did not respond".into())
}
fn udp_tracker(
    network: &dyn Network,
    url: &str,
    request: &TrackerRequest<'_>,
    deadline: Instant,
) -> Result<TrackerReply> {
    let rest = url.strip_prefix("udp://").ok_or("Invalid UDP tracker")?;
    let authority = rest.split('/').next().ok_or("Invalid UDP tracker")?;
    if authority.contains('@') {
        return Err("Invalid UDP tracker address".into());
    }
    let address = network
        .resolve(authority, deadline)
        .map_err(|_| "Could not resolve tracker address")?
        .into_iter()
        .next()
        .ok_or("Tracker has no address")?;
    let socket = network.datagram(address.is_ipv6())?;
    udp_announce(&*socket, address, request, deadline)
}

fn udp_announce(
    socket: &dyn NetworkDatagram,
    address: SocketAddr,
    request: &TrackerRequest<'_>,
    deadline: Instant,
) -> Result<TrackerReply> {
    let tx = nonce()?;
    let mut connect = Vec::with_capacity(16);
    connect.extend_from_slice(&0x41727101980u64.to_be_bytes());
    connect.extend_from_slice(&0u32.to_be_bytes());
    connect.extend_from_slice(&tx.to_be_bytes());
    let response = udp_exchange(socket, address, &connect, 0, tx, deadline)?;
    if response.len() != 16 {
        return Err("Invalid UDP connection response".into());
    }
    let tx = nonce()?;
    let mut announce = Vec::with_capacity(98);
    announce.extend_from_slice(&response[8..16]);
    announce.extend_from_slice(&1u32.to_be_bytes());
    announce.extend_from_slice(&tx.to_be_bytes());
    announce.extend_from_slice(request.hash);
    announce.extend_from_slice(request.peer_id);
    announce.extend_from_slice(&request.downloaded.to_be_bytes());
    announce.extend_from_slice(&request.left.to_be_bytes());
    announce.extend_from_slice(&request.uploaded.to_be_bytes());
    announce.extend_from_slice(&request.event.udp_code().to_be_bytes());
    announce.extend_from_slice(&0u32.to_be_bytes());
    announce.extend_from_slice(&session_key(request).to_be_bytes());
    announce.extend_from_slice(&100i32.to_be_bytes());
    announce.extend_from_slice(&request.port.to_be_bytes());
    let response = udp_exchange(socket, address, &announce, 1, tx, deadline)?;
    if response.len() < 20 {
        return Err("Truncated UDP announcement response".into());
    }
    let interval = u32::from_be_bytes(
        response[8..12]
            .try_into()
            .map_err(|_| "Truncated UDP interval")?,
    );
    let count = |at: usize| {
        response[at..at + 4]
            .try_into()
            .ok()
            .map(|bytes| u64::from(u32::from_be_bytes(bytes)))
    };
    let peers = decode_peers(&response[20..], address.is_ipv6())?;
    Ok(TrackerReply {
        peers,
        interval: bounded_interval(u64::from(interval)),
        leechers: count(12),
        seeders: count(16),
    })
}

// A bounded iterative Kademlia lookup. Unknown metadata only performs get_peers;
// announcing is allowed after the caller established that the torrent is public.
pub fn dht_lookup(
    network: &dyn Network,
    hash: &[u8; 20],
    port: u16,
    announce: bool,
    stop: &AtomicBool,
    seeds: &[SocketAddr],
) -> Result<Vec<SocketAddr>> {
    let socket = network
        .datagram(false)
        .map_err(|_| "Could not create DHT socket")?;
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut nodes: Vec<SocketAddr> = seeds.iter().copied().filter(SocketAddr::is_ipv4).collect();
    if seeds.is_empty() {
        for host in [
            "router.bittorrent.com:6881",
            "router.utorrent.com:6881",
            "dht.transmissionbt.com:6881",
        ] {
            if let Ok(addresses) = network.resolve(host, deadline) {
                nodes.extend(addresses.into_iter().filter(SocketAddr::is_ipv4).take(2));
            }
        }
    }
    dht_search(&*socket, hash, port, announce, stop, nodes, deadline)
}

/// Queries in flight at once, and the closest responders a lookup settles on (BEP 5).
const DHT_PARALLEL: usize = 8;
const DHT_CLOSEST: usize = 16;
/// Bounds of one lookup: queries sent, candidate nodes kept, peers returned.
const DHT_QUERIES: usize = 256;
const DHT_CANDIDATES: usize = 512;
const DHT_PEERS: usize = 1000;
/// How soon a lookup that found no peer is tried again, instead of the
/// usual minute: one lost bootstrap reply should not stall a new download.
pub const DHT_RETRY: Duration = Duration::from_secs(10);
/// A node that has not answered by then is skipped.
const DHT_QUERY_TIMEOUT: Duration = Duration::from_secs(1);
/// Distance given to nodes whose id is unknown, such as bootstrap routers.
const UNKNOWN: [u8; 20] = [0xff; 20];

fn distance(id: &[u8], hash: &[u8; 20]) -> [u8; 20] {
    let mut out = [0; 20];
    for ((out, a), b) in out.iter_mut().zip(id).zip(hash) {
        *out = a ^ b;
    }
    out
}

/// An iterative Kademlia lookup: the closest unqueried nodes are asked, a few
/// at a time, until the closest responders have all answered. Peers come from
/// every answer; the announcement goes to the closest nodes that gave a token.
fn dht_search(
    socket: &dyn NetworkDatagram,
    hash: &[u8; 20],
    port: u16,
    announce: bool,
    stop: &AtomicBool,
    nodes: Vec<SocketAddr>,
    deadline: Instant,
) -> Result<Vec<SocketAddr>> {
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|_| "Could not configure DHT timeout")?;
    let id = crate::crypto::random_bytes::<20>()?;
    let mut candidates: BTreeSet<([u8; 20], SocketAddr)> =
        nodes.into_iter().map(|node| (UNKNOWN, node)).collect();
    let mut queried = BTreeSet::new();
    let mut responders: BTreeMap<([u8; 20], SocketAddr), Option<Vec<u8>>> = BTreeMap::new();
    let mut pending: BTreeMap<[u8; 4], (SocketAddr, Instant)> = BTreeMap::new();
    let mut peers = BTreeSet::new();
    let mut tx = nonce()?;
    let mut sent = 0;
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline && peers.len() < DHT_PEERS {
        // Settled once every candidate left is farther than the closest responders.
        let bound = responders.keys().nth(DHT_CLOSEST - 1).map(|(d, _)| *d);
        while pending.len() < DHT_PARALLEL && sent < DHT_QUERIES {
            let Some(&(near, target)) = candidates
                .iter()
                .find(|(_, address)| !queried.contains(address))
            else {
                break;
            };
            if bound.is_some_and(|bound| near > bound) {
                break;
            }
            queried.insert(target);
            tx = tx.wrapping_add(1);
            let transaction = tx.to_be_bytes();
            let query = dict(&[
                (b"t", Value::Bytes(transaction.to_vec())),
                (b"y", Value::Bytes(b"q".to_vec())),
                (b"q", Value::Bytes(b"get_peers".to_vec())),
                (
                    b"a",
                    dict(&[
                        (b"id", Value::Bytes(id.to_vec())),
                        (b"info_hash", Value::Bytes(hash.to_vec())),
                    ]),
                ),
            ]);
            sent += 1;
            if socket
                .send_to(&crate::bencode::encode(&query), target)
                .is_ok()
            {
                pending.insert(transaction, (target, Instant::now()));
            }
        }
        if pending.is_empty() {
            break;
        }
        let mut buf = [0; 4096];
        if let Ok((n, from)) = socket.recv_from(&mut buf)
            && let Ok(response) = crate::bencode::parse(&buf[..n])
            && let Some(transaction) = value_bytes(field(&response, b"t"))
                .and_then(|t| <[u8; 4]>::try_from(t).ok())
            // An answer counts only from the node it was asked of.
            && pending.get(&transaction).is_some_and(|(target, _)| *target == from)
        {
            pending.remove(&transaction);
            if value_bytes(field(&response, b"y")) == Some(b"r".as_slice())
                && let Some(r) = field(&response, b"r")
            {
                if let Some(Value::List(values)) = field(r, b"values") {
                    for value in values.iter().take(200) {
                        if let Ok(raw) = super::metainfo::bytes(value)
                            && let Ok(found) = decode_peers(raw, false)
                        {
                            peers.extend(found);
                        }
                    }
                }
                if let Some(raw) = value_bytes(field(r, b"nodes"))
                    && raw.len() % 26 == 0
                {
                    for node in raw.as_chunks::<26>().0.iter().take(32) {
                        if let Ok(found) = decode_peers(&node[20..], false) {
                            for address in found {
                                if !queried.contains(&address) {
                                    candidates.insert((distance(&node[..20], hash), address));
                                }
                            }
                        }
                    }
                    while candidates.len() > DHT_CANDIDATES {
                        candidates.pop_last();
                    }
                }
                let near = value_bytes(field(r, b"id"))
                    .filter(|id| id.len() == 20)
                    .map_or(UNKNOWN, |id| distance(id, hash));
                let token = value_bytes(field(r, b"token"))
                    .filter(|token| announce && token.len() <= 256)
                    .map(<[u8]>::to_vec);
                responders.insert((near, from), token);
            }
        }
        pending.retain(|_, (_, at)| at.elapsed() < DHT_QUERY_TIMEOUT);
    }
    if announce {
        for ((_, target), token) in responders
            .iter()
            .filter_map(|(key, token)| Some((key, token.as_ref()?)))
            .take(DHT_PARALLEL)
        {
            tx = tx.wrapping_add(1);
            let query = dict(&[
                (b"t", Value::Bytes(tx.to_be_bytes().to_vec())),
                (b"y", Value::Bytes(b"q".to_vec())),
                (b"q", Value::Bytes(b"announce_peer".to_vec())),
                (
                    b"a",
                    dict(&[
                        (b"id", Value::Bytes(id.to_vec())),
                        (b"info_hash", Value::Bytes(hash.to_vec())),
                        (b"port", Value::Int(i64::from(port))),
                        (b"token", Value::Bytes(token.clone())),
                    ]),
                ),
            ]);
            let _ = socket.send_to(&crate::bencode::encode(&query), *target);
        }
    }
    Ok(peers.into_iter().take(DHT_PEERS).collect())
}

pub fn pex_peers(payload: &[u8]) -> Result<Vec<SocketAddr>> {
    let value = crate::bencode::parse(payload)?;
    let mut peers = Vec::new();
    if let Some(raw) = value_bytes(field(&value, b"added")) {
        peers.extend(decode_peers(raw, false)?);
    }
    if let Some(raw) = value_bytes(field(&value, b"added6")) {
        peers.extend(decode_peers(raw, true)?);
    }
    Ok(peers)
}

pub fn value_dict(items: &[(&[u8], Value)]) -> Value {
    dict(items)
}
pub fn value_field<'a>(v: &'a Value, key: &[u8]) -> Option<&'a Value> {
    field(v, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::transport::Direct;
    use std::{
        collections::VecDeque,
        io::{Error, ErrorKind},
        net::UdpSocket,
        sync::{Arc, Mutex},
    };

    /// Host, port and request bytes of every HTTP connection.
    type HttpLog = Arc<Mutex<Vec<(String, u16, Vec<u8>)>>>;
    type Responder = Box<dyn FnMut(&[u8], SocketAddr) -> Vec<(Vec<u8>, SocketAddr)> + Send>;

    /// A non-socket datagram transport: every sent packet is recorded and the
    /// responder queues the replies that later receives return.
    struct Memory {
        sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
        inbox: Mutex<VecDeque<(Vec<u8>, SocketAddr)>>,
        responder: Mutex<Responder>,
    }

    impl Memory {
        fn new(responder: Responder) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                inbox: Mutex::new(VecDeque::new()),
                responder: Mutex::new(responder),
            }
        }

        fn sent(&self) -> Vec<(Vec<u8>, SocketAddr)> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl NetworkDatagram for Memory {
        fn send_to(&self, packet: &[u8], target: SocketAddr) -> std::io::Result<usize> {
            self.sent.lock().unwrap().push((packet.to_vec(), target));
            let replies = (self.responder.lock().unwrap())(packet, target);
            self.inbox.lock().unwrap().extend(replies);
            Ok(packet.len())
        }

        fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
            let (packet, from) = self
                .inbox
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::from(ErrorKind::TimedOut))?;
            buf[..packet.len()].copy_from_slice(&packet);
            Ok((packet.len(), from))
        }

        fn set_read_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }

        fn set_write_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl NetworkDatagram for Arc<Memory> {
        fn send_to(&self, packet: &[u8], target: SocketAddr) -> std::io::Result<usize> {
            (**self).send_to(packet, target)
        }

        fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
            (**self).recv_from(buf)
        }

        fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
            (**self).set_read_timeout(timeout)
        }

        fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
            (**self).set_write_timeout(timeout)
        }
    }

    /// A non-socket torrent network: names resolve from a fixed table, the one
    /// datagram socket is a `Memory`, and HTTP answers with a canned response.
    /// It records every request, so a test sees each network operation.
    struct Fake {
        names: Vec<(&'static str, SocketAddr)>,
        socket: Arc<Memory>,
        http_reply: Vec<u8>,
        resolved: Mutex<Vec<String>>,
        datagrams: Mutex<Vec<bool>>,
        http: HttpLog,
    }

    impl Fake {
        fn new(names: Vec<(&'static str, SocketAddr)>, responder: Responder) -> Self {
            Self {
                names,
                socket: Arc::new(Memory::new(responder)),
                http_reply: Vec::new(),
                resolved: Mutex::new(Vec::new()),
                datagrams: Mutex::new(Vec::new()),
                http: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    struct Canned {
        reply: std::io::Cursor<Vec<u8>>,
        log: HttpLog,
    }

    impl std::io::Read for Canned {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reply.read(buf)
        }
    }

    impl std::io::Write for Canned {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if let Some(last) = self.log.lock().unwrap().last_mut() {
                last.2.extend_from_slice(data);
            }
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl crate::net::NetworkStream for Canned {
        fn set_read_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }

        fn set_write_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct CannedConnector {
        reply: Vec<u8>,
        log: HttpLog,
    }

    impl crate::net::HttpConnector for CannedConnector {
        fn connect(
            &self,
            host: &str,
            port: u16,
            _: Instant,
        ) -> Result<Box<dyn crate::net::NetworkStream>> {
            self.log
                .lock()
                .unwrap()
                .push((host.to_owned(), port, Vec::new()));
            Ok(Box::new(Canned {
                reply: std::io::Cursor::new(self.reply.clone()),
                log: self.log.clone(),
            }))
        }
    }

    impl Network for Fake {
        fn connect(
            &self,
            _: SocketAddr,
            _: Duration,
        ) -> Result<Box<dyn crate::net::NetworkStream>> {
            Err("No peer connection in this fixture".into())
        }

        fn resolve(&self, authority: &str, _: Instant) -> Result<Vec<SocketAddr>> {
            self.resolved.lock().unwrap().push(authority.to_owned());
            let found: Vec<_> = self
                .names
                .iter()
                .filter(|(name, _)| *name == authority)
                .map(|(_, address)| *address)
                .collect();
            if found.is_empty() {
                return Err("Unknown name".into());
            }
            Ok(found)
        }

        fn datagram(&self, ipv6: bool) -> Result<Box<dyn NetworkDatagram>> {
            self.datagrams.lock().unwrap().push(ipv6);
            Ok(Box::new(self.socket.clone()))
        }

        fn http(&self) -> crate::net::HttpClient {
            crate::net::HttpClient::new().with_connector(Arc::new(CannedConnector {
                reply: self.http_reply.clone(),
                log: self.http.clone(),
            }))
        }

        fn listen(
            &self,
            _: SocketAddr,
        ) -> Result<Box<dyn super::super::transport::NetworkListener>> {
            Err("No listener in this fixture".into())
        }
    }

    fn request() -> TrackerRequest<'static> {
        TrackerRequest {
            hash: &[7; 20],
            peer_id: &[9; 20],
            port: 6882,
            left: 23,
            downloaded: 17,
            uploaded: 19,
            event: TrackerEvent::Started,
        }
    }

    #[test]
    fn udp_tracker_runs_over_a_non_socket_datagram_and_ignores_foreign_sources() {
        let tracker: SocketAddr = "192.0.2.1:6969".parse().unwrap();
        let foreign: SocketAddr = "192.0.2.2:6969".parse().unwrap();
        let mut forged = false;
        let socket = Memory::new(Box::new(move |packet, target| {
            assert_eq!(target, tracker);
            let mut reply = Vec::new();
            if packet.len() == 16 {
                reply.extend_from_slice(&0u32.to_be_bytes());
                reply.extend_from_slice(&packet[12..16]);
                reply.extend_from_slice(&17u64.to_be_bytes());
            } else {
                assert_eq!(packet.len(), 98);
                assert_eq!(&packet[..8], &17u64.to_be_bytes());
                reply.extend_from_slice(&1u32.to_be_bytes());
                reply.extend_from_slice(&packet[12..16]);
                reply.extend_from_slice(&30u32.to_be_bytes());
                reply.extend_from_slice(&[0; 8]);
                reply.extend_from_slice(&[198, 51, 100, 7, 0x1a, 0xe1]);
            }
            // The first connect answer comes from another host with the right
            // transaction; it must be dropped and the request sent again.
            let from = if forged { tracker } else { foreign };
            forged = true;
            vec![(reply, from)]
        }));
        let reply = udp_announce(
            &socket,
            tracker,
            &request(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(reply.peers, vec!["198.51.100.7:6881".parse().unwrap()]);
        assert_eq!(reply.interval, Duration::from_secs(30));
        let sent = socket.sent();
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].0, sent[1].0);
        assert!(sent.iter().all(|(_, target)| *target == tracker));
    }

    #[test]
    fn udp_tracker_over_a_datagram_honours_its_deadline_and_silence() {
        let tracker: SocketAddr = "192.0.2.1:6969".parse().unwrap();
        let silent = Memory::new(Box::new(|_, _| Vec::new()));
        let expired = udp_announce(&silent, tracker, &request(), Instant::now()).unwrap_err();
        assert!(expired.to_string().contains("deadline exceeded"));
        assert!(silent.sent().is_empty());
        let unanswered = udp_announce(
            &silent,
            tracker,
            &request(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(unanswered.to_string().contains("did not respond"));
        assert_eq!(silent.sent().len(), 2);
    }

    #[test]
    fn dht_search_runs_over_a_non_socket_datagram() {
        let first: SocketAddr = "192.0.2.10:6881".parse().unwrap();
        let second: SocketAddr = "192.0.2.11:6881".parse().unwrap();
        let socket = Memory::new(Box::new(move |packet, target| {
            let query = crate::bencode::parse(packet).unwrap();
            if value_bytes(field(&query, b"q")) != Some(b"get_peers".as_slice()) {
                return Vec::new();
            }
            let t = value_bytes(field(&query, b"t")).unwrap().to_vec();
            if target == second {
                // A reply for the second node delivered from the first must
                // not count as the second node's answer.
                let reply = dict(&[
                    (b"t", Value::Bytes(t)),
                    (b"y", Value::Bytes(b"r".to_vec())),
                    (
                        b"r",
                        dict(&[(
                            b"values",
                            Value::List(vec![Value::Bytes(vec![203, 0, 113, 9, 0, 9])]),
                        )]),
                    ),
                ]);
                return vec![(crate::bencode::encode(&reply), first)];
            }
            let mut node = vec![1; 20];
            node.extend_from_slice(&[192, 0, 2, 11, 0x1a, 0xe1]);
            let reply = dict(&[
                (b"t", Value::Bytes(t)),
                (b"y", Value::Bytes(b"r".to_vec())),
                (
                    b"r",
                    dict(&[
                        (
                            b"values",
                            Value::List(vec![Value::Bytes(vec![198, 51, 100, 7, 0x1a, 0xe1])]),
                        ),
                        (b"nodes", Value::Bytes(node)),
                        (b"token", Value::Bytes(b"tok".to_vec())),
                    ]),
                ),
            ]);
            vec![(crate::bencode::encode(&reply), target)]
        }));
        let stop = AtomicBool::new(false);
        let peers = dht_search(
            &socket,
            &[7; 20],
            6882,
            true,
            &stop,
            vec![first],
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(peers, vec!["198.51.100.7:6881".parse().unwrap()]);
        let sent = socket.sent();
        let targets: Vec<_> = sent.iter().map(|(_, target)| *target).collect();
        // Both nodes are asked; only the one that answered itself, with a
        // token, receives the announcement.
        assert_eq!(targets, vec![first, second, first]);
        let announce = crate::bencode::parse(&sent[2].0).unwrap();
        assert_eq!(
            value_bytes(field(&announce, b"q")),
            Some(b"announce_peer".as_slice())
        );
        assert_eq!(
            value_bytes(field(field(&announce, b"a").unwrap(), b"token")),
            Some(b"tok".as_slice())
        );
    }

    #[test]
    fn udp_tracker_resolves_and_opens_its_socket_only_through_the_network() {
        let tracker: SocketAddr = "192.0.2.1:6969".parse().unwrap();
        let network = Fake::new(
            vec![("tracker.invalid:6969", tracker)],
            Box::new(move |packet, target| {
                assert_eq!(target, tracker);
                let mut reply = Vec::new();
                if packet.len() == 16 {
                    reply.extend_from_slice(&0u32.to_be_bytes());
                    reply.extend_from_slice(&packet[12..16]);
                    reply.extend_from_slice(&17u64.to_be_bytes());
                } else {
                    reply.extend_from_slice(&1u32.to_be_bytes());
                    reply.extend_from_slice(&packet[12..16]);
                    reply.extend_from_slice(&30u32.to_be_bytes());
                    reply.extend_from_slice(&[0; 8]);
                    reply.extend_from_slice(&[198, 51, 100, 7, 0x1a, 0xe1]);
                }
                vec![(reply, tracker)]
            }),
        );
        let reply = tracker_before(
            &network,
            "udp://tracker.invalid:6969/announce",
            &request(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(reply.peers, vec!["198.51.100.7:6881".parse().unwrap()]);
        assert_eq!(*network.resolved.lock().unwrap(), ["tracker.invalid:6969"]);
        assert_eq!(*network.datagrams.lock().unwrap(), [false]);
        assert_eq!(network.socket.sent().len(), 2);

        // A name the network cannot resolve fails without any socket.
        let unknown = Fake::new(Vec::new(), Box::new(|_, _| Vec::new()));
        let error = tracker_before(
            &unknown,
            "udp://other.invalid:6969/announce",
            &request(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Could not resolve tracker address")
        );
        assert!(unknown.datagrams.lock().unwrap().is_empty());
    }

    #[test]
    fn http_tracker_uses_the_client_of_its_network() {
        let body = crate::bencode::encode(&dict(&[
            (b"interval", Value::Int(45)),
            (b"peers", Value::Bytes(vec![198, 51, 100, 8, 0x1a, 0xe1])),
        ]));
        let mut network = Fake::new(Vec::new(), Box::new(|_, _| Vec::new()));
        network.http_reply =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        network.http_reply.extend_from_slice(&body);
        let reply = tracker_before(
            &network,
            "http://tracker.invalid:8080/announce?passkey=fixture",
            &request(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(reply.peers, vec!["198.51.100.8:6881".parse().unwrap()]);
        assert_eq!(reply.interval, Duration::from_secs(45));
        let http = network.http.lock().unwrap();
        assert_eq!(http.len(), 1);
        assert_eq!((http[0].0.as_str(), http[0].1), ("tracker.invalid", 8080));
        let sent = String::from_utf8_lossy(&http[0].2);
        assert!(sent.starts_with("GET /announce?passkey=fixture&info_hash="));
        assert!(network.resolved.lock().unwrap().is_empty());
        assert!(network.datagrams.lock().unwrap().is_empty());
    }

    #[test]
    fn dht_bootstrap_names_and_socket_come_from_the_network() {
        let router: SocketAddr = "192.0.2.10:6881".parse().unwrap();
        let network = Fake::new(
            vec![
                ("router.bittorrent.com:6881", router),
                (
                    "router.bittorrent.com:6881",
                    "[2001:db8::1]:6881".parse().unwrap(),
                ),
            ],
            Box::new(move |packet, target| {
                let query = crate::bencode::parse(packet).unwrap();
                let t = value_bytes(field(&query, b"t")).unwrap().to_vec();
                let reply = dict(&[
                    (b"t", Value::Bytes(t)),
                    (b"y", Value::Bytes(b"r".to_vec())),
                    (
                        b"r",
                        dict(&[(
                            b"values",
                            Value::List(vec![Value::Bytes(vec![203, 0, 113, 9, 0x1a, 0xe1])]),
                        )]),
                    ),
                ]);
                vec![(crate::bencode::encode(&reply), target)]
            }),
        );
        let peers = dht_lookup(
            &network,
            &[7; 20],
            6882,
            false,
            &AtomicBool::new(false),
            &[],
        )
        .unwrap();
        assert_eq!(peers, vec!["203.0.113.9:6881".parse().unwrap()]);
        assert_eq!(
            *network.resolved.lock().unwrap(),
            [
                "router.bittorrent.com:6881",
                "router.utorrent.com:6881",
                "dht.transmissionbt.com:6881"
            ]
        );
        assert_eq!(*network.datagrams.lock().unwrap(), [false]);
        let targets: Vec<_> = network.socket.sent().iter().map(|(_, to)| *to).collect();
        assert_eq!(targets, vec![router]);

        // Explicit seeds need no name resolution.
        let seeded = Fake::new(Vec::new(), Box::new(|_, _| Vec::new()));
        let _ = dht_lookup(
            &seeded,
            &[7; 20],
            6882,
            false,
            &AtomicBool::new(true),
            &[router],
        );
        assert!(seeded.resolved.lock().unwrap().is_empty());
    }

    #[test]
    fn dht_search_over_a_datagram_stops_on_cancellation_and_deadline() {
        let node: SocketAddr = "192.0.2.10:6881".parse().unwrap();
        let socket = Memory::new(Box::new(|_, _| Vec::new()));
        let stopped = AtomicBool::new(true);
        let later = Instant::now() + Duration::from_secs(5);
        assert!(
            dht_search(&socket, &[7; 20], 6882, true, &stopped, vec![node], later)
                .unwrap()
                .is_empty()
        );
        let running = AtomicBool::new(false);
        assert!(
            dht_search(
                &socket,
                &[7; 20],
                6882,
                true,
                &running,
                vec![node],
                Instant::now()
            )
            .unwrap()
            .is_empty()
        );
        assert!(socket.sent().is_empty());
    }
    #[test]
    fn compact_peers_are_bounded() {
        assert_eq!(
            decode_peers(&[127, 0, 0, 1, 0x1a, 0xe1], false).expect("peer")[0].port(),
            6881
        );
        assert!(decode_peers(&[1], false).is_err());
        assert!(
            decode_peers(&[224, 0, 0, 1, 0, 1], false)
                .expect("multicast")
                .is_empty()
        );
    }

    #[test]
    fn real_udp_dht_lookup_and_authenticated_announce() {
        for announce in [false, true] {
            let server = UdpSocket::bind("127.0.0.1:0").expect("local DHT");
            server
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("timeout");
            let address = server.local_addr().expect("address");
            let hash = [19; 20];
            let worker = std::thread::spawn(move || {
                let mut packet = [0; 4096];
                let (n, peer) = server.recv_from(&mut packet).expect("get_peers");
                let query = crate::bencode::parse(&packet[..n]).expect("request");
                assert_eq!(
                    value_bytes(field(&query, b"q")),
                    Some(b"get_peers".as_slice())
                );
                assert_eq!(
                    value_bytes(field(field(&query, b"a").expect("args"), b"info_hash")),
                    Some(hash.as_slice())
                );
                let tx = field(&query, b"t").expect("transaction").clone();
                let response = dict(&[
                    (b"t", tx),
                    (b"y", Value::Bytes(b"r".to_vec())),
                    (
                        b"r",
                        dict(&[
                            (b"id", Value::Bytes([3; 20].to_vec())),
                            (b"token", Value::Bytes(b"proof".to_vec())),
                            (
                                b"values",
                                Value::List(vec![Value::Bytes(vec![127, 0, 0, 1, 0x1a, 0xe1])]),
                            ),
                        ]),
                    ),
                ]);
                server
                    .send_to(&crate::bencode::encode(&response), peer)
                    .expect("response");
                if announce {
                    let (n, _) = server.recv_from(&mut packet).expect("announce_peer");
                    let query = crate::bencode::parse(&packet[..n]).expect("announcement");
                    assert_eq!(
                        value_bytes(field(&query, b"q")),
                        Some(b"announce_peer".as_slice())
                    );
                    assert_eq!(
                        value_bytes(field(field(&query, b"a").expect("args"), b"token")),
                        Some(b"proof".as_slice())
                    );
                }
            });
            let peers = dht_lookup(
                &Direct,
                &hash,
                6881,
                announce,
                &AtomicBool::new(false),
                &[address],
            )
            .expect("discovery");
            assert_eq!(peers, vec!["127.0.0.1:6881".parse().expect("peer")]);
            worker.join().expect("server");
        }
    }

    #[test]
    fn tracker_intervals_are_bounded_and_minimum_is_respected() {
        let reply = |fields: &[(&[u8], Value)]| parse_tracker_reply(&dict(fields));
        assert_eq!(reply(&[]).unwrap().interval, DEFAULT_TRACKER_INTERVAL);
        assert_eq!(
            reply(&[(b"interval", Value::Int(0))]).unwrap().interval,
            Duration::from_secs(30)
        );
        assert_eq!(
            reply(&[(b"interval", Value::Int(i64::MAX))])
                .unwrap()
                .interval,
            Duration::from_secs(86_400)
        );
        assert_eq!(
            reply(&[
                (b"interval", Value::Int(30)),
                (b"min interval", Value::Int(90))
            ])
            .unwrap()
            .interval,
            Duration::from_secs(90)
        );
        assert!(reply(&[(b"interval", Value::Int(-1))]).is_err());
        assert!(reply(&[(b"interval", Value::Bytes(b"30".to_vec()))]).is_err());
        assert!(reply(&[(b"min interval", Value::Int(-1))]).is_err());
        assert!(reply(&[(b"failure reason", Value::Bytes(b"private refusal".to_vec()))]).is_err());
        assert!(parse_tracker_reply(&Value::Int(1)).is_err());
        assert!(reply(&[(b"peers", Value::Bytes(vec![0; 6 * 10_001]))]).is_err());
    }

    #[test]
    fn real_http_tracker_events_counters_and_interval() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        server.set_nonblocking(true).unwrap();
        let address = server.local_addr().unwrap();
        let events = [
            (TrackerEvent::None, None),
            (TrackerEvent::Started, Some("started")),
            (TrackerEvent::Completed, Some("completed")),
            (TrackerEvent::Stopped, Some("stopped")),
        ];
        let worker = std::thread::spawn(move || {
            let mut first_key = None;
            for (_, expected) in events {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match server.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(1))
                        }
                        Err(error) => panic!("Local tracker unavailable: {error}"),
                    }
                };
                crate::net::blocking(&stream).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let query = line.split_whitespace().nth(1).unwrap();
                assert!(query.starts_with("/announce?passkey=fixture&info_hash="));
                assert!(query.contains("&downloaded=12345&left=54321"));
                assert!(query.contains("&uploaded=9876&"));
                assert!(query.contains("&port=6882&"));
                match expected {
                    Some(event) => assert!(query.ends_with(&format!("&event={event}"))),
                    None => assert!(!query.contains("event=")),
                }
                assert!(!query.contains("#ignored"));
                let key = query
                    .split('&')
                    .find_map(|part| part.strip_prefix("key="))
                    .unwrap()
                    .to_owned();
                assert_eq!(key.len(), 8);
                if let Some(previous) = &first_key {
                    assert_eq!(previous, &key);
                }
                first_key = Some(key);
                for _ in 0..32 {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                let body = crate::bencode::encode(&dict(&[
                    (b"interval", Value::Int(5)),
                    (b"min interval", Value::Int(90)),
                    (b"peers", Value::Bytes(vec![127, 0, 0, 1, 0x1a, 0xe1])),
                ]));
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                reader.get_mut().write_all(&body).unwrap();
            }
        });
        for (event, _) in events {
            let reply = tracker(
                &Direct,
                &format!("http://{address}/announce?passkey=fixture#ignored"),
                &TrackerRequest {
                    hash: &[7; 20],
                    peer_id: &[9; 20],
                    port: 6882,
                    left: 54321,
                    uploaded: 9876,
                    downloaded: 12345,
                    event,
                },
            )
            .unwrap();
            assert_eq!(reply.interval, Duration::from_secs(90));
            assert_eq!(reply.peers, vec!["127.0.0.1:6881".parse().unwrap()]);
        }
        worker.join().unwrap();
    }

    #[test]
    fn real_udp_tracker_all_event_codes_and_counters() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let address = server.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let mut first_key = None;
            for code in [0u32, 1, 2, 3] {
                let mut buf = [0u8; 1024];
                let (n, peer) = server.recv_from(&mut buf).unwrap();
                assert_eq!(n, 16);
                let mut reply = vec![0; 4];
                reply.extend_from_slice(&buf[12..16]);
                reply.extend_from_slice(&99u64.to_be_bytes());
                server.send_to(&reply, peer).unwrap();
                let (n, peer) = server.recv_from(&mut buf).unwrap();
                assert_eq!(n, 98);
                assert_eq!(&buf[56..64], &12345u64.to_be_bytes());
                assert_eq!(&buf[64..72], &54321u64.to_be_bytes());
                assert_eq!(&buf[72..80], &9876u64.to_be_bytes());
                assert_eq!(&buf[80..84], &code.to_be_bytes());
                let key: [u8; 4] = buf[88..92].try_into().unwrap();
                if let Some(previous) = first_key {
                    assert_eq!(previous, key);
                }
                first_key = Some(key);
                let mut reply = 1u32.to_be_bytes().to_vec();
                reply.extend_from_slice(&buf[12..16]);
                reply.extend_from_slice(&5u32.to_be_bytes());
                reply.extend_from_slice(&[0; 8]);
                server.send_to(&reply, peer).unwrap();
            }
        });
        for event in [
            TrackerEvent::None,
            TrackerEvent::Completed,
            TrackerEvent::Started,
            TrackerEvent::Stopped,
        ] {
            let reply = tracker(
                &Direct,
                &format!("udp://{address}/announce"),
                &TrackerRequest {
                    hash: &[7; 20],
                    peer_id: &[9; 20],
                    port: 6882,
                    left: 54321,
                    uploaded: 9876,
                    downloaded: 12345,
                    event,
                },
            )
            .unwrap();
            assert_eq!(reply.interval, Duration::from_secs(30));
            assert!(reply.peers.is_empty());
        }
        worker.join().unwrap();
    }

    #[test]
    fn real_udp_tracker_connect_and_announce() {
        let server = UdpSocket::bind("127.0.0.1:0").expect("tracker");
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let address = server.local_addr().expect("address");
        let hash = [7; 20];
        let id = [9; 20];
        let worker = std::thread::spawn(move || {
            let mut buf = [0; 1024];
            let (n, peer) = server.recv_from(&mut buf).expect("connect");
            assert_eq!(n, 16);
            let mut reply = 0u32.to_be_bytes().to_vec();
            reply.extend_from_slice(&buf[12..16]);
            reply.extend_from_slice(&17u64.to_be_bytes());
            server.send_to(&reply, peer).expect("reply");
            let (n, peer) = server.recv_from(&mut buf).expect("announce");
            assert_eq!(n, 98);
            assert_eq!(&buf[16..36], &hash);
            assert_eq!(&buf[36..56], &id);
            assert_eq!(&buf[56..64], &17u64.to_be_bytes());
            assert_eq!(&buf[64..72], &23u64.to_be_bytes());
            assert_eq!(&buf[72..80], &19u64.to_be_bytes());
            assert_eq!(&buf[80..84], &2u32.to_be_bytes());
            let mut reply = 1u32.to_be_bytes().to_vec();
            reply.extend_from_slice(&buf[12..16]);
            reply.extend_from_slice(&30u32.to_be_bytes());
            reply.extend_from_slice(&1u32.to_be_bytes());
            reply.extend_from_slice(&1u32.to_be_bytes());
            reply.extend_from_slice(&[127, 0, 0, 1, 0x1a, 0xe1]);
            server.send_to(&reply, peer).expect("reply");
        });
        assert_eq!(
            tracker(
                &Direct,
                &format!("udp://{address}/announce"),
                &TrackerRequest {
                    hash: &hash,
                    peer_id: &id,
                    port: 6882,
                    left: 23,
                    downloaded: 17,
                    uploaded: 19,
                    event: TrackerEvent::Started
                }
            )
            .expect("peers")
            .peers,
            vec!["127.0.0.1:6881".parse().expect("peer")]
        );
        worker.join().expect("worker");
    }
}
