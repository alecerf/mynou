//! Interoperability with an independent WireGuard implementation: the pinned
//! upstream wireguard-go peer of engineering/Dockerfile.wireguard-peer. Only
//! engineering/wireguard_interop.py runs it for real, inside the peer's network
//! namespace with every input set; anywhere else it returns at once. Mynou's
//! public `Tunnel` API is the only Mynou code under test. See
//! docs/wireguard-interop.md for the scenarios, the evidence and the limits.

use mynou::crypto::{blake2s, keyed_blake2s, x25519_public_key};
use mynou::netstack::IpConfig;
use mynou::tunnel::{Clock, Endpoint, Stats, Tunnel, TunnelConfig, TunnelState};
use mynou::wireguard::{Entropy, OsEntropy};
use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

const PEER_OUTER: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 51820);
const PEER_INNER: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);
const MYNOU_INNER: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 2);
/// Source of the junk handshake load; loopback, never the address of either side.
const FLOOD_SOURCE: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
/// Comfortably past the 120 s `REKEY_AFTER_TIME` of both implementations.
const PAST_REKEY: Duration = Duration::from_secs(121);
/// Longer than one 5 s `REKEY_TIMEOUT` retry, so a refused handshake is retried
/// at least once while it is watched.
const REFUSAL_WINDOW: Duration = Duration::from_secs(7);
const MAX_UAPI_REPLY: usize = 64 * 1024;
const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LOG_LINES: usize = 400_000;
const MAX_LOG_LINE: usize = 1024;
const MAX_OBSERVED: usize = 600;

/// Monotonic and wall time that a scenario can move forward together.
#[derive(Clone)]
struct Offset {
    start: Instant,
    skew: Rc<Cell<Duration>>,
}

impl Offset {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            skew: Rc::new(Cell::new(Duration::ZERO)),
        }
    }

    fn advance(&self, by: Duration) {
        self.skew.set(self.skew.get() + by);
    }
}

impl Clock for Offset {
    fn now(&self) -> Duration {
        self.start.elapsed() + self.skew.get()
    }

    fn wall(&self) -> SystemTime {
        SystemTime::now() + self.skew.get()
    }
}

type Mynou = Tunnel<OsEntropy, Offset>;

/// One synthetic static identity, fresh for every scenario and never printed.
struct Keys {
    secret: [u8; 32],
    public: [u8; 32],
}

fn keys() -> Keys {
    let secret = OsEntropy.bytes::<32>().expect("entropy");
    Keys {
        secret,
        public: x25519_public_key(secret),
    }
}

fn random_key() -> [u8; 32] {
    OsEntropy.bytes::<32>().expect("entropy")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |acc, (index, byte)| {
            acc | u32::from(*byte) << (16 - 8 * index)
        });
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    ALPHABET[(value >> (18 - 6 * index)) as usize & 63],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// How wireguard-go names a peer in its log: `peer(` + the first four and the
/// 40th to 43rd base64 characters of its public key + `)`.
fn peer_name(public: &[u8; 32]) -> String {
    let encoded = base64(public);
    format!("peer({}…{})", &encoded[..4], &encoded[39..43])
}

/// A MAC1-valid handshake initiation with junk content. It costs the receiver
/// a MAC check and, outside load, a failing Noise attempt; it can never
/// complete a handshake.
fn junk_initiation(responder: &[u8; 32]) -> Vec<u8> {
    let mut message = vec![0u8; 148];
    message[0] = 1;
    let body = OsEntropy.bytes::<112>().expect("entropy");
    message[4..116].copy_from_slice(&body);
    let mut label = b"mac1----".to_vec();
    label.extend_from_slice(responder);
    let key = blake2s(&label).expect("mac1 key");
    let mac1 = keyed_blake2s(&key, &message[..116], 16).expect("mac1");
    message[116..132].copy_from_slice(&mac1);
    message
}

/// Paced bursts of junk initiations from `FLOOD_SOURCE`, never a continuous
/// flood: a full handshake queue would silently drop the genuine initiation.
struct Flood {
    stop: Arc<AtomicBool>,
    sent: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
}

impl Flood {
    fn start(target: SocketAddr, responder: [u8; 32], burst: usize, every: Duration) -> Self {
        let socket = UdpSocket::bind((FLOOD_SOURCE, 0)).expect("flood socket");
        let pool: Vec<Vec<u8>> = (0..32).map(|_| junk_initiation(&responder)).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let (halt, count) = (stop.clone(), sent.clone());
        let worker = thread::spawn(move || {
            let mut index = 0usize;
            while !halt.load(Ordering::Relaxed) {
                for _ in 0..burst {
                    if socket.send_to(&pool[index % pool.len()], target).is_ok() {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                    index = index.wrapping_add(1);
                }
                thread::sleep(every);
            }
        });
        Self {
            stop,
            sent,
            worker: Some(worker),
        }
    }

    fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }
}

impl Drop for Flood {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // Let the peer leave its one-second under-load state before the next
        // scenario.
        thread::sleep(Duration::from_millis(1500));
    }
}

/// The peer's verbose log, followed by the driver. It is data: only bounded
/// substring matches are made, from a mark taken before each step.
struct PeerLog {
    path: PathBuf,
}

impl PeerLog {
    fn mark(&self) -> u64 {
        std::fs::metadata(&self.path).map_or(0, |meta| meta.len())
    }

    fn lines(&self, mark: u64) -> Result<Vec<String>, String> {
        let mut file = File::open(&self.path).map_err(|error| format!("peer log: {error}"))?;
        let length = file.metadata().map_err(|error| error.to_string())?.len();
        if length.saturating_sub(mark) > MAX_LOG_BYTES {
            return Err("peer log grew past its bound".into());
        }
        file.seek(SeekFrom::Start(mark))
            .map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        file.take(MAX_LOG_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        // A line still being written is ignored until it is complete.
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        if bytes[..complete]
            .split(|byte| *byte == b'\n')
            .any(|line| line.len() > MAX_LOG_LINE)
        {
            return Err("peer log line exceeds its bound".into());
        }
        let lines: Vec<String> = bytes[..complete]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .take(MAX_LOG_LINES + 1)
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect();
        if lines.len() > MAX_LOG_LINES {
            return Err("peer log has too many lines".into());
        }
        Ok(lines)
    }

    /// The lines since `mark` once `done` holds for them, or the last lines
    /// read at `within`: the driver follows the log with a short delay.
    fn wait(
        &self,
        mark: u64,
        within: Duration,
        done: impl Fn(&[String]) -> bool,
    ) -> Result<Vec<String>, String> {
        let start = Instant::now();
        loop {
            let lines = self.lines(mark)?;
            if done(&lines) || start.elapsed() > within {
                return Ok(lines);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// How long the driver may take to copy a peer log line into the file.
const LOG_DELAY: Duration = Duration::from_secs(3);

/// The index of the first line at or after `from` containing every needle.
fn find(lines: &[String], from: usize, needles: &[&str]) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, line)| needles.iter().all(|needle| line.contains(needle)))
        .map(|(index, _)| index)
}

fn count(lines: &[String], needles: &[&str]) -> usize {
    lines
        .iter()
        .filter(|line| needles.iter().all(|needle| line.contains(needle)))
        .count()
}

/// The peer's configuration socket. Replies are filtered so that no private or
/// preshared key ever leaves this function.
struct Uapi {
    path: PathBuf,
}

impl Uapi {
    fn exchange(&self, request: &str) -> Result<Vec<(String, String)>, String> {
        let mut stream =
            UnixStream::connect(&self.path).map_err(|error| format!("uapi connect: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(5))))
            .map_err(|error| error.to_string())?;
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("uapi write: {error}"))?;
        let mut reply = Vec::new();
        let mut buffer = [0u8; 4096];
        while !reply.ends_with(b"\n\n") {
            let read = stream
                .read(&mut buffer)
                .map_err(|error| format!("uapi read: {error}"))?;
            if read == 0 || reply.len() + read > MAX_UAPI_REPLY {
                return Err("uapi reply truncated or too long".into());
            }
            reply.extend_from_slice(&buffer[..read]);
        }
        let text = String::from_utf8(reply).map_err(|_| "uapi reply is not UTF-8")?;
        let pairs: Vec<(String, String)> = text
            .lines()
            .filter(|line| !line.starts_with("private_key=") && !line.starts_with("preshared_key="))
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        match pairs.iter().find(|(key, _)| key == "errno") {
            Some((_, errno)) if errno == "0" => Ok(pairs),
            Some((_, errno)) => Err(format!("uapi errno={errno}")),
            None => Err("uapi reply without errno".into()),
        }
    }

    /// Replaces the peer's identity and its only peer: `mynou` with the inner
    /// address `MYNOU_INNER`, an optional PSK and an optional endpoint.
    fn configure(
        &self,
        peer: &Keys,
        mynou: [u8; 32],
        psk: Option<[u8; 32]>,
        endpoint: Option<SocketAddr>,
    ) -> Result<(), String> {
        let mut request = format!(
            "set=1\nprivate_key={}\nlisten_port={}\nreplace_peers=true\npublic_key={}\n",
            hex(&peer.secret),
            PEER_OUTER.port(),
            hex(&mynou)
        );
        if let Some(psk) = psk {
            request.push_str(&format!("preshared_key={}\n", hex(&psk)));
        }
        if let Some(endpoint) = endpoint {
            request.push_str(&format!("endpoint={endpoint}\n"));
        }
        request.push_str(&format!(
            "replace_allowed_ips=true\nallowed_ip={MYNOU_INNER}/32\n\n"
        ));
        self.exchange(&request).map(|_| ())
    }

    /// The peer's last completed handshake, as (seconds, nanoseconds).
    fn last_handshake(&self) -> Result<(u64, u64), String> {
        let pairs = self.exchange("get=1\n\n")?;
        let field = |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .and_then(|(_, value)| value.parse::<u64>().ok())
                .ok_or(format!("uapi reply without {name}"))
        };
        Ok((
            field("last_handshake_time_sec")?,
            field("last_handshake_time_nsec")?,
        ))
    }
}

/// What a passing or failing scenario saw, and where it saw it.
struct Evidence {
    source: &'static str,
    observed: String,
}

type Outcome = Result<Evidence, String>;

struct Harness {
    uapi: Uapi,
    log: PeerLog,
    result: PathBuf,
}

/// Escapes a string for the JSON result record.
fn json(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

fn bounded(text: &str) -> String {
    text.chars().take(MAX_OBSERVED).collect()
}

impl Harness {
    fn from_env() -> Self {
        let input = |name: &str| {
            let value = std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
            assert!(!value.is_empty(), "{name} is required");
            PathBuf::from(value)
        };
        Self {
            uapi: Uapi {
                path: input("MYNOU_WIREGUARD_UAPI"),
            },
            log: PeerLog {
                path: input("MYNOU_WIREGUARD_PEER_LOG"),
            },
            result: input("MYNOU_WIREGUARD_RESULT"),
        }
    }

    fn record(&self, scenario: &str, outcome: &Outcome, elapsed: Duration) {
        let (status, source, observed) = match outcome {
            Ok(evidence) => ("pass", evidence.source, evidence.observed.as_str()),
            Err(error) => ("fail", "assertion", error.as_str()),
        };
        let line = format!(
            "{{\"scenario\":{},\"outcome\":{},\"milliseconds\":{},\"source\":{},\"observed\":{}}}\n",
            json(scenario),
            json(status),
            elapsed.as_millis(),
            json(source),
            json(&bounded(observed))
        );
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.result)
            .expect("result record");
        file.write_all(line.as_bytes()).expect("result record");
    }
}

/// Settings shared by every Mynou side; only the scenario's choices vary.
struct Side<'a> {
    keys: &'a Keys,
    peer_public: [u8; 32],
    psk: Option<[u8; 32]>,
    initiate: bool,
    handshake_load: u32,
}

impl Side<'_> {
    fn new(keys: &Keys, peer_public: [u8; 32]) -> Side<'_> {
        Side {
            keys,
            peer_public,
            psk: None,
            initiate: true,
            handshake_load: 32,
        }
    }

    fn start(&self, clock: Offset) -> Result<Mynou, String> {
        let ip =
            IpConfig::new(MYNOU_INNER, PEER_INNER, 32).map_err(|error| format!("{error:?}"))?;
        let mut config = TunnelConfig::new(
            self.keys.secret,
            self.peer_public,
            Endpoint::Address(PEER_OUTER),
            ip,
        );
        config.bind = SocketAddr::from(([127, 0, 0, 1], 0));
        config.psk = self.psk;
        config.initiate = self.initiate;
        config.handshake_load = self.handshake_load;
        Tunnel::new(config, OsEntropy, clock).map_err(|error| format!("tunnel: {error:?}"))
    }
}

fn pump(tunnel: &mut Mynou) -> Result<(), String> {
    tunnel
        .pump(Duration::from_millis(10))
        .map(|_| ())
        .map_err(|error| format!("pump: {error:?}"))
}

/// Pumps until `done` holds, or fails at `within`.
fn pump_until(
    tunnel: &mut Mynou,
    within: Duration,
    what: &str,
    mut done: impl FnMut(&mut Mynou) -> Result<bool, String>,
) -> Result<Duration, String> {
    let start = Instant::now();
    loop {
        pump(tunnel)?;
        if done(tunnel)? {
            return Ok(start.elapsed());
        }
        if start.elapsed() > within {
            let stats = tunnel.stats();
            return Err(format!(
                "timed out after {within:?} waiting for {what}; state {:?}, {}",
                tunnel.status().state,
                counters(&stats)
            ));
        }
    }
}

fn counters(stats: &Stats) -> String {
    format!(
        "received {} sent {} rejected {} handshake_invalid {} high_load {}",
        stats.received, stats.sent, stats.rejected, stats.handshake_invalid, stats.high_load
    )
}

fn protected(tunnel: &mut Mynou, within: Duration) -> Result<Duration, String> {
    pump_until(tunnel, within, "Protected", |tunnel| {
        Ok(tunnel.status().state == TunnelState::Protected)
    })
}

/// Pumps for `window` and fails if a session ever appears.
fn refused(tunnel: &mut Mynou, window: Duration) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < window {
        pump(tunnel)?;
        if tunnel.status().state == TunnelState::Protected {
            return Err("a session was established".into());
        }
    }
    Ok(())
}

/// The peer's kernel side of the tunnel: a std socket on its wg0 address.
fn inner_socket() -> Result<UdpSocket, String> {
    let socket =
        UdpSocket::bind((PEER_INNER, 0)).map_err(|error| format!("inner bind: {error}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    Ok(socket)
}

fn inner_port(socket: &UdpSocket) -> Result<u16, String> {
    socket
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| error.to_string())
}

fn nonce() -> String {
    hex(&OsEntropy.bytes::<8>().expect("entropy"))
}

/// UDP data both ways through the peer's kernel stack, Mynou first unless the
/// peer must speak first to keep its initiator role.
fn exchange(tunnel: &mut Mynou, peer_first: bool) -> Result<String, String> {
    let inner = inner_socket()?;
    let port = inner_port(&inner)?;
    let virtual_socket = tunnel
        .udp_bind(0)
        .map_err(|error| format!("udp_bind: {error:?}"))?;
    let to_peer = format!("to-peer {}", nonce());
    let to_mynou = format!("to-mynou {}", nonce());
    let send_to_peer = |tunnel: &mut Mynou| -> Result<Duration, String> {
        tunnel
            .udp_send_to(
                virtual_socket,
                (PEER_INNER.into(), port),
                to_peer.as_bytes(),
            )
            .map_err(|error| format!("udp_send_to: {error:?}"))?;
        let mut buffer = [0u8; 256];
        pump_until(
            tunnel,
            Duration::from_secs(10),
            "data to the peer",
            |_| match inner.recv_from(&mut buffer) {
                Ok((length, source)) => Ok(&buffer[..length] == to_peer.as_bytes()
                    && source == SocketAddr::from((MYNOU_INNER, virtual_socket.port()))),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
                Err(error) => Err(format!("inner recv: {error}")),
            },
        )
    };
    let send_to_mynou = |tunnel: &mut Mynou| -> Result<Duration, String> {
        inner
            .send_to(to_mynou.as_bytes(), (MYNOU_INNER, virtual_socket.port()))
            .map_err(|error| format!("inner send: {error}"))?;
        pump_until(
            tunnel,
            Duration::from_secs(10),
            "data to Mynou",
            |tunnel| match tunnel.udp_recv(virtual_socket) {
                Ok(Some(received)) => Ok(received.payload == to_mynou.as_bytes()
                    && received.source == PEER_INNER
                    && received.source_port == port),
                Ok(None) => Ok(false),
                Err(error) => Err(format!("udp_recv: {error:?}")),
            },
        )
    };
    let (first, second) = if peer_first {
        let first = send_to_mynou(tunnel)?;
        (first, send_to_peer(tunnel)?)
    } else {
        let first = send_to_peer(tunnel)?;
        (first, send_to_mynou(tunnel)?)
    };
    let _ = tunnel.udp_close(virtual_socket);
    let order = if peer_first {
        "peer->mynou then mynou->peer"
    } else {
        "mynou->peer then peer->mynou"
    };
    Ok(format!("udp {order} in {first:?} and {second:?}"))
}

/// The common positive journey: a session from the chosen initiator, then data
/// both ways, with the peer log confirming who initiated.
fn established(harness: &Harness, peer_initiates: bool, psk: Option<[u8; 32]>) -> Outcome {
    let (mynou, peer) = (keys(), keys());
    let mut side = Side::new(&mynou, peer.public);
    side.psk = psk;
    side.initiate = !peer_initiates;
    let mark = harness.log.mark();
    let mut tunnel;
    let took = if peer_initiates {
        tunnel = side.start(Offset::new())?;
        let endpoint = tunnel.local_addr().map_err(|error| format!("{error:?}"))?;
        harness
            .uapi
            .configure(&peer, mynou.public, psk, Some(endpoint))?;
        // The peer's first inner datagram makes it initiate.
        let data = exchange(&mut tunnel, true)?;
        let took = protected(&mut tunnel, Duration::from_secs(1))?;
        format!("{data}; protected after {took:?}")
    } else {
        harness.uapi.configure(&peer, mynou.public, psk, None)?;
        tunnel = side.start(Offset::new())?;
        let took = protected(&mut tunnel, Duration::from_secs(10))?;
        let data = exchange(&mut tunnel, false)?;
        format!("protected after {took:?}; {data}")
    };
    let name = peer_name(&mynou.public);
    let roles = |lines: &[String]| {
        (
            count(lines, &[&name, "Received handshake initiation"]),
            count(lines, &[&name, "Sending handshake initiation"]),
            count(lines, &[&name, "Received handshake response"]),
        )
    };
    let hold = |(initiations, sent, responses): (usize, usize, usize)| {
        if peer_initiates {
            initiations == 0 && sent >= 1 && responses >= 1
        } else {
            initiations >= 1 && sent == 0
        }
    };
    let lines = harness
        .log
        .wait(mark, LOG_DELAY, |lines| hold(roles(lines)))?;
    let (initiations, sent, responses) = roles(&lines);
    if !hold((initiations, sent, responses)) {
        return Err(format!(
            "peer log does not show the expected roles: {initiations} initiations received, {sent} sent, {responses} responses received"
        ));
    }
    tunnel.shutdown();
    Ok(Evidence {
        source: "Mynou Tunnel status and virtual UDP; peer inner socket; peer log",
        observed: format!(
            "{took}; peer log: {initiations} initiations received, {sent} sent, {responses} responses received"
        ),
    })
}

fn mynou_initiator(harness: &mut Harness) -> Outcome {
    established(harness, false, None)
}

fn peer_initiator(harness: &mut Harness) -> Outcome {
    established(harness, true, None)
}

fn psk_mynou_initiator(harness: &mut Harness) -> Outcome {
    established(harness, false, Some(random_key()))
}

fn psk_peer_initiator(harness: &mut Harness) -> Outcome {
    established(harness, true, Some(random_key()))
}

/// Mynou initiates with one PSK, the peer holds another: the peer answers, and
/// Mynou must refuse the response.
fn psk_mismatch_mynou_initiator(harness: &mut Harness) -> Outcome {
    let (mynou, peer) = (keys(), keys());
    harness
        .uapi
        .configure(&peer, mynou.public, Some(random_key()), None)?;
    let mark = harness.log.mark();
    let mut side = Side::new(&mynou, peer.public);
    side.psk = Some(random_key());
    let mut tunnel = side.start(Offset::new())?;
    let inner = inner_socket()?;
    let virtual_socket = tunnel.udp_bind(0).map_err(|error| format!("{error:?}"))?;
    // Queued without a session; it must never come out of the tunnel.
    let _ = tunnel.udp_send_to(
        virtual_socket,
        (PEER_INNER.into(), inner_port(&inner)?),
        b"must-not-arrive",
    );
    refused(&mut tunnel, REFUSAL_WINDOW)?;
    let mut buffer = [0u8; 64];
    if inner.recv_from(&mut buffer).is_ok() {
        return Err("inner data was delivered without a session".into());
    }
    let stats = tunnel.stats();
    let lines = harness.log.lines(mark)?;
    let name = peer_name(&mynou.public);
    let answered = count(&lines, &[&name, "Sending handshake response"]);
    if answered == 0 || stats.rejected == 0 {
        return Err(format!(
            "expected the peer to answer and Mynou to refuse: {answered} responses sent; {}",
            counters(&stats)
        ));
    }
    tunnel.shutdown();
    Ok(Evidence {
        source: "Mynou Tunnel status and stats; peer inner socket; peer log",
        observed: format!(
            "never Protected in {REFUSAL_WINDOW:?}; peer sent {answered} responses; Mynou {}",
            counters(&stats)
        ),
    })
}

/// The peer initiates with a PSK that Mynou does not hold: Mynou answers, and
/// the peer must refuse the response.
fn psk_mismatch_peer_initiator(harness: &mut Harness) -> Outcome {
    let (mynou, peer) = (keys(), keys());
    let mut side = Side::new(&mynou, peer.public);
    side.initiate = false;
    let mut tunnel = side.start(Offset::new())?;
    let endpoint = tunnel.local_addr().map_err(|error| format!("{error:?}"))?;
    harness
        .uapi
        .configure(&peer, mynou.public, Some(random_key()), Some(endpoint))?;
    let mark = harness.log.mark();
    let virtual_socket = tunnel.udp_bind(0).map_err(|error| format!("{error:?}"))?;
    let inner = inner_socket()?;
    inner
        .send_to(b"must-not-arrive", (MYNOU_INNER, virtual_socket.port()))
        .map_err(|error| error.to_string())?;
    let start = Instant::now();
    while start.elapsed() < REFUSAL_WINDOW {
        pump(&mut tunnel)?;
        if tunnel.status().state == TunnelState::Protected {
            return Err("a session was established".into());
        }
        if !matches!(tunnel.udp_recv(virtual_socket), Ok(None)) {
            return Err("inner data was delivered without a session".into());
        }
    }
    let lines = harness.log.lines(mark)?;
    let invalid = count(
        &lines,
        &[&format!(
            "Received invalid response message from {endpoint}"
        )],
    );
    if invalid == 0 {
        return Err("the peer did not refuse Mynou's response".into());
    }
    let stats = tunnel.stats();
    tunnel.shutdown();
    Ok(Evidence {
        source: "Mynou Tunnel status and virtual UDP; peer log",
        observed: format!(
            "never Protected in {REFUSAL_WINDOW:?}; peer refused {invalid} responses; Mynou {}",
            counters(&stats)
        ),
    })
}

/// Mynou pins a public key that is not the peer's.
fn wrong_key_mynou_pins(harness: &mut Harness) -> Outcome {
    let (mynou, peer) = (keys(), keys());
    harness.uapi.configure(&peer, mynou.public, None, None)?;
    let mark = harness.log.mark();
    let mut tunnel = Side::new(&mynou, keys().public).start(Offset::new())?;
    refused(&mut tunnel, REFUSAL_WINDOW)?;
    let lines = harness.log.lines(mark)?;
    let invalid = count(&lines, &["Received packet with invalid mac1"]);
    let initiations = count(
        &lines,
        &[&peer_name(&mynou.public), "Received handshake initiation"],
    );
    if invalid == 0 || initiations != 0 {
        return Err(format!(
            "expected only MAC1 refusals: {invalid} invalid MAC1, {initiations} initiations accepted"
        ));
    }
    let stats = tunnel.stats();
    tunnel.shutdown();
    Ok(Evidence {
        source: "Mynou Tunnel status; peer log",
        observed: format!(
            "never Protected in {REFUSAL_WINDOW:?}; peer refused {invalid} initiations on MAC1; Mynou {}",
            counters(&stats)
        ),
    })
}

/// The peer pins a public key that is not Mynou's.
fn wrong_key_peer_pins(harness: &mut Harness) -> Outcome {
    let (mynou, peer) = (keys(), keys());
    harness.uapi.configure(&peer, keys().public, None, None)?;
    let mark = harness.log.mark();
    let mut tunnel = Side::new(&mynou, peer.public).start(Offset::new())?;
    let endpoint = tunnel.local_addr().map_err(|error| format!("{error:?}"))?;
    refused(&mut tunnel, REFUSAL_WINDOW)?;
    let lines = harness.log.lines(mark)?;
    let invalid = count(
        &lines,
        &[&format!(
            "Received invalid initiation message from {endpoint}"
        )],
    );
    if invalid == 0 {
        return Err("the peer did not refuse the unknown initiator".into());
    }
    let stats = tunnel.stats();
    tunnel.shutdown();
    Ok(Evidence {
        source: "Mynou Tunnel status; peer log",
        observed: format!(
            "never Protected in {REFUSAL_WINDOW:?}; peer refused {invalid} initiations from an unknown key; Mynou {}",
            counters(&stats)
        ),
    })
}

/// Mynou answers only its pinned peer: an initiation from another static key
/// is refused without a response.
fn unknown_static_mynou_responder(harness: &mut Harness) -> Outcome {
    let (mynou, peer) = (keys(), keys());
    let mut side = Side::new(&mynou, keys().public);
    side.initiate = false;
    let mut tunnel = side.start(Offset::new())?;
    let endpoint = tunnel.local_addr().map_err(|error| format!("{error:?}"))?;
    harness
        .uapi
        .configure(&peer, mynou.public, None, Some(endpoint))?;
    let mark = harness.log.mark();
    let inner = inner_socket()?;
    inner
        .send_to(b"must-not-arrive", (MYNOU_INNER, 9))
        .map_err(|error| error.to_string())?;
    refused(&mut tunnel, REFUSAL_WINDOW)?;
    let stats = tunnel.stats();
    let lines = harness.log.lines(mark)?;
    let name = peer_name(&mynou.public);
    let sent = count(&lines, &[&name, "Sending handshake initiation"]);
    let answered = count(&lines, &[&name, "Received handshake response"]);
    if sent == 0 || answered != 0 || stats.rejected == 0 {
        return Err(format!(
            "expected refused initiations: {sent} sent, {answered} answered; Mynou {}",
            counters(&stats)
        ));
    }
    tunnel.shutdown();
    Ok(Evidence {
        source: "Mynou Tunnel status and stats; peer log",
        observed: format!(
            "never Protected in {REFUSAL_WINDOW:?}; peer sent {sent} initiations, none answered; Mynou {}",
            counters(&stats)
        ),
    })
}

/// Pumps until the peer logs `line` since `mark` and reports a handshake newer
/// than `first`.
fn fresh_handshake(
    harness: &Harness,
    tunnel: &mut Mynou,
    mark: u64,
    line: &[&str],
    first: (u64, u64),
) -> Result<Duration, String> {
    pump_until(tunnel, Duration::from_secs(15), "a fresh handshake", |_| {
        let lines = harness.log.lines(mark)?;
        Ok(count(&lines, line) >= 1 && harness.uapi.last_handshake()? != first)
    })
    .map_err(|error| {
        let logged = harness.log.lines(mark).map(|lines| count(&lines, line));
        let latest = harness.uapi.last_handshake();
        format!("{error}; peer log since the mark: {logged:?} of {line:?}; first handshake {first:?}, latest {latest:?}")
    })
}

/// Mynou's clock moves past `REKEY_AFTER_TIME`: its next send must start a
/// fresh handshake that the peer accepts, in the same generation.
fn rekey_mynou(harness: &mut Harness) -> Outcome {
    let (mynou, peer) = (keys(), keys());
    harness.uapi.configure(&peer, mynou.public, None, None)?;
    let clock = Offset::new();
    let mut tunnel = Side::new(&mynou, peer.public).start(clock.clone())?;
    protected(&mut tunnel, Duration::from_secs(10))?;
    exchange(&mut tunnel, false)?;
    let generation = tunnel.status().generation;
    let first = harness.uapi.last_handshake()?;
    let name = peer_name(&mynou.public);
    let mark = harness.log.mark();
    clock.advance(PAST_REKEY);
    // The send after the jump is the one that must start the rekey.
    let before = exchange(&mut tunnel, false)?;
    let took = fresh_handshake(
        harness,
        &mut tunnel,
        mark,
        &[&name, "Received handshake initiation"],
        first,
    )?;
    let after = exchange(&mut tunnel, false)?;
    let status = tunnel.status();
    if status.generation != generation || status.state != TunnelState::Protected {
        return Err(format!(
            "the rekey changed the generation or state: {status:?}"
        ));
    }
    tunnel.shutdown();
    Ok(Evidence {
        source: "offset Clock; peer log and UAPI last_handshake_time; Mynou Tunnel status and virtual UDP",
        observed: format!(
            "clock +{PAST_REKEY:?}; {before}; fresh handshake accepted by the peer within {took:?}; same generation; {after}"
        ),
    })
}

/// The peer's own 120 s timer, in real time: as initiator, its next send must
/// start a fresh handshake that Mynou answers, in the same generation.
fn rekey_peer(harness: &mut Harness) -> Outcome {
    let (mynou, peer) = (keys(), keys());
    let mut side = Side::new(&mynou, peer.public);
    side.initiate = false;
    let mut tunnel = side.start(Offset::new())?;
    let endpoint = tunnel.local_addr().map_err(|error| format!("{error:?}"))?;
    harness
        .uapi
        .configure(&peer, mynou.public, None, Some(endpoint))?;
    let mark = harness.log.mark();
    exchange(&mut tunnel, true)?;
    protected(&mut tunnel, Duration::from_secs(1))?;
    let generation = tunnel.status().generation;
    let session = Instant::now();
    let first = harness.uapi.last_handshake()?;
    // Keepalives can trigger the peer's rekey during the wait itself.
    let rekey_mark = harness.log.mark();
    // Traffic keeps both sides active while the peer's timer runs, always
    // from the peer first so that only its timer can trigger the rekey.
    while session.elapsed() < PAST_REKEY {
        exchange(&mut tunnel, true)?;
        let pause = Instant::now();
        while pause.elapsed() < Duration::from_secs(10) && session.elapsed() < PAST_REKEY {
            pump(&mut tunnel)?;
        }
    }
    let name = peer_name(&mynou.public);
    let before = exchange(&mut tunnel, true)?;
    let took = fresh_handshake(
        harness,
        &mut tunnel,
        rekey_mark,
        &[&name, "Received handshake response"],
        first,
    )?;
    let after = exchange(&mut tunnel, true)?;
    let status = tunnel.status();
    if status.generation != generation || status.state != TunnelState::Protected {
        return Err(format!(
            "the rekey changed the generation or state: {status:?}"
        ));
    }
    let lines = harness.log.lines(mark)?;
    let mynou_initiations = count(&lines, &[&name, "Received handshake initiation"]);
    if mynou_initiations != 0 {
        return Err(format!("Mynou initiated {mynou_initiations} handshakes"));
    }
    tunnel.shutdown();
    Ok(Evidence {
        source: "real time; peer log and UAPI last_handshake_time; Mynou Tunnel status and virtual UDP",
        observed: format!(
            "session older than {PAST_REKEY:?}; {before}; fresh peer-initiated handshake within {took:?}; same generation; {after}"
        ),
    })
}

/// The peer is held under load by junk from `FLOOD_SOURCE`: it must send Mynou
/// a cookie reply, then accept Mynou's MAC2-carrying retry while still under
/// load, and data must flow.
fn cookie_peer_under_load(harness: &mut Harness) -> Outcome {
    let (mynou, peer) = (keys(), keys());
    harness.uapi.configure(&peer, mynou.public, None, None)?;
    let warmup = harness.log.mark();
    let flood = Flood::start(PEER_OUTER, peer.public, 400, Duration::from_millis(200));
    let junk_cookie =
        format!("Sending cookie response for denied handshake message for {FLOOD_SOURCE}:");
    let loaded = Instant::now();
    loop {
        if find(&harness.log.lines(warmup)?, 0, &[&junk_cookie]).is_some() {
            break;
        }
        if loaded.elapsed() > Duration::from_secs(10) {
            return Err(format!(
                "the flood did not load the peer ({} junk sent)",
                flood.sent()
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
    let mark = harness.log.mark();
    let mut tunnel = Side::new(&mynou, peer.public).start(Offset::new())?;
    let endpoint = tunnel.local_addr().map_err(|error| format!("{error:?}"))?;
    let took = protected(&mut tunnel, Duration::from_secs(40))?;
    let data = exchange(&mut tunnel, false)?;
    // One more second of load, so a junk cookie reply follows the acceptance.
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(1) {
        pump(&mut tunnel)?;
    }
    let sent = flood.sent();
    let mynou_cookie =
        format!("Sending cookie response for denied handshake message for {endpoint}");
    let name = peer_name(&mynou.public);
    let lines = harness.log.wait(mark, LOG_DELAY, |lines| {
        find(lines, 0, &[&mynou_cookie])
            .and_then(|cookie| find(lines, cookie + 1, &[&name, "Received handshake initiation"]))
            .and_then(|accepted| find(lines, accepted + 1, &[&junk_cookie]))
            .is_some()
    })?;
    let cookie = find(&lines, 0, &[&mynou_cookie]).ok_or("the peer sent Mynou no cookie reply")?;
    let accepted = find(
        &lines,
        cookie + 1,
        &[&name, "Received handshake initiation"],
    )
    .ok_or("the peer accepted no initiation after its cookie reply")?;
    find(&lines, accepted + 1, &[&junk_cookie])
        .ok_or("the peer was no longer under load after accepting the initiation")?;
    // The pinned peer keeps its load state for one second. Require a prior
    // junk cookie in the same logged second as the acceptance, rather than
    // inferring load from a cookie arbitrarily far in the past or future.
    let second = |line: &str| {
        line.strip_prefix("DEBUG: (wg0) ")
            .and_then(|rest| rest.get(..19))
            .map(str::to_string)
    };
    let accepted_second = second(&lines[accepted]).ok_or("missing peer log timestamp")?;
    if !lines[..accepted]
        .iter()
        .any(|line| line.contains(&junk_cookie) && second(line).as_ref() == Some(&accepted_second))
    {
        return Err("no same-second load evidence before the accepted retry".into());
    }
    drop(flood);
    tunnel.shutdown();
    Ok(Evidence {
        source: "peer log order and same-second load evidence; Mynou Tunnel status and virtual UDP",
        observed: format!(
            "{sent} junk initiations in bursts of 400 per 200 ms; cookie reply to Mynou at log line {cookie}, initiation accepted at {accepted} under load; protected after {took:?}; {data}"
        ),
    })
}

/// Mynou is held above its handshake load by junk from `FLOOD_SOURCE`: it must
/// answer the peer's initiation with a cookie reply, then admit the peer's
/// MAC2-carrying retry, and data must flow.
fn cookie_mynou_under_load(harness: &mut Harness) -> Outcome {
    let (mynou, peer) = (keys(), keys());
    let mut side = Side::new(&mynou, peer.public);
    side.initiate = false;
    side.handshake_load = 1;
    let mut tunnel = side.start(Offset::new())?;
    let endpoint = tunnel.local_addr().map_err(|error| format!("{error:?}"))?;
    let flood = Flood::start(endpoint, mynou.public, 8, Duration::from_millis(20));
    let loaded = Instant::now();
    pump_until(&mut tunnel, Duration::from_secs(5), "high load", |tunnel| {
        Ok(tunnel.stats().high_load > 0)
    })?;
    harness
        .uapi
        .configure(&peer, mynou.public, None, Some(endpoint))?;
    let mark = harness.log.mark();
    let data = exchange(&mut tunnel, true)?;
    let took = protected(&mut tunnel, Duration::from_secs(1))?;
    let name = peer_name(&mynou.public);
    let lines = harness.log.wait(mark, LOG_DELAY, |lines| {
        count(lines, &[&name, "Received handshake response"]) >= 1
    })?;
    let cookie = find(
        &lines,
        0,
        &[&format!("Receiving cookie response from {endpoint}")],
    )
    .ok_or("the peer received no cookie reply from Mynou")?;
    let initiations: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(&name) && line.contains("Sending handshake initiation"))
        .map(|(index, _)| index)
        .collect();
    let retry = initiations
        .iter()
        .copied()
        .find(|index| *index > cookie)
        .ok_or("the peer sent no retry after the cookie reply")?;
    let answered = find(&lines, retry + 1, &[&name, "Received handshake response"])
        .ok_or("Mynou did not answer the retry")?;
    if find(&lines, 0, &[&name, "Received handshake response"]) != Some(answered) {
        return Err("Mynou answered an initiation before the cookie exchange".into());
    }
    let high_before = tunnel.stats().high_load;
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(1) {
        pump(&mut tunnel)?;
    }
    let stats = tunnel.stats();
    if stats.high_load <= high_before {
        return Err("Mynou left high load before the exchange ended".into());
    }
    let sent = flood.sent();
    drop(flood);
    tunnel.shutdown();
    Ok(Evidence {
        source: "Mynou Tunnel stats.high_load and status; peer log order (cookie received, retry, response); virtual UDP",
        observed: format!(
            "{sent} junk initiations over {:?} in bursts of 8 per 20 ms, handshake_load 1; {} initiations, cookie at log line {cookie}, retry at {retry} answered at {answered}; protected {took:?} after data; {data}; Mynou {}",
            loaded.elapsed(),
            initiations.len(),
            counters(&stats)
        ),
    })
}

type Scenario = fn(&mut Harness) -> Outcome;

/// Every scenario the gate requires, in order. None is skipped or retried.
const SCENARIOS: [(&str, Scenario); 13] = [
    ("mynou-initiator", mynou_initiator),
    ("peer-initiator", peer_initiator),
    ("psk-mynou-initiator", psk_mynou_initiator),
    ("psk-peer-initiator", psk_peer_initiator),
    ("psk-mismatch-mynou-initiator", psk_mismatch_mynou_initiator),
    ("psk-mismatch-peer-initiator", psk_mismatch_peer_initiator),
    ("wrong-key-mynou-pins", wrong_key_mynou_pins),
    ("wrong-key-peer-pins", wrong_key_peer_pins),
    (
        "unknown-static-mynou-responder",
        unknown_static_mynou_responder,
    ),
    ("rekey-mynou", rekey_mynou),
    ("rekey-peer", rekey_peer),
    ("cookie-peer-under-load", cookie_peer_under_load),
    ("cookie-mynou-under-load", cookie_mynou_under_load),
];

#[test]
fn wireguard_go_interoperability() {
    if std::env::var("MYNOU_WIREGUARD_INTEROP").as_deref() != Ok("1") {
        return;
    }
    let mut harness = Harness::from_env();
    let only = std::env::var("MYNOU_WIREGUARD_SCENARIO").ok();
    let mut failed = Vec::new();
    for (name, scenario) in SCENARIOS {
        if only.as_deref().is_some_and(|only| only != name) {
            continue;
        }
        let start = Instant::now();
        let outcome = scenario(&mut harness);
        harness.record(name, &outcome, start.elapsed());
        if let Err(error) = &outcome {
            eprintln!("{name}: {error}");
            failed.push(name);
        }
    }
    assert!(failed.is_empty(), "failed scenarios: {failed:?}");
}
