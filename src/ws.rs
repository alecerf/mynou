//! Bounded WebSocket (RFC 6455) transport for the web interface.
//!
//! One socket carries request/response calls to the existing `/api/*` routes
//! and server-pushed snapshots of subscribed topics. Client frames must be
//! masked, unfragmented and at most `MAX_CLIENT_FRAME` bytes; any violation
//! closes the connection.
use crate::{
    Result,
    crypto::sha1,
    engine::Engine,
    json::{self, Value},
};
use std::{
    collections::BTreeMap,
    io::Read,
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub const SUBPROTOCOL: &str = "mynou.v1";
pub const MAX_SOCKETS: usize = 8;
const MAX_CLIENT_FRAME: usize = 65_536;
const MAX_MESSAGE: usize = 2 * 1_048_576;
const MAX_TOPICS: usize = 16;
const TICK: Duration = Duration::from_millis(200);
const PING_EVERY: Duration = Duration::from_secs(20);
const IDLE_LIMIT: Duration = Duration::from_secs(60);
const WRITE_LIMIT: Duration = Duration::from_secs(10);
const FAST: Duration = Duration::from_secs(1);
const SLOW: Duration = Duration::from_secs(5);
/// Topic name, API path and refresh period.
const TOPICS: [(&str, &str, Duration); 9] = [
    ("status", "/api/status", FAST),
    ("jobs", "/api/jobs?limit=50", FAST),
    ("transfers", "/api/transfers", FAST),
    ("library", "/api/library?limit=50", SLOW),
    ("series", "/api/series?limit=50", SLOW),
    ("indexers", "/api/indexers", SLOW),
    ("feeds", "/api/feeds", SLOW),
    ("irc", "/api/irc", SLOW),
    ("requesters", "/api/requesters", SLOW),
];
static OPEN: AtomicUsize = AtomicUsize::new(0);

/// A fixed topic, or `transfer:ID` for the details of one native transfer.
fn topic(name: &str) -> Option<(String, String, Duration)> {
    if let Some((topic, path, every)) = TOPICS.iter().find(|(topic, _, _)| *topic == name) {
        return Some(((*topic).to_owned(), (*path).to_owned(), *every));
    }
    let id = name.strip_prefix("transfer:")?;
    ([40, 64].contains(&id.len()) && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then(|| (name.to_owned(), format!("/api/transfers/{id}"), FAST))
}

pub type Route = fn(&Arc<Engine>, &str, &str, &[u8]) -> Result<(u16, Value)>;

/// Holds one of the `MAX_SOCKETS` slots until dropped.
pub struct Slot;
impl Slot {
    pub fn take() -> Option<Self> {
        OPEN.try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_SOCKETS).then_some(n + 1)
        })
        .ok()
        .map(|_| Self)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `Sec-WebSocket-Accept` for a valid client key.
pub fn accept_key(key: &str) -> String {
    base64(&sha1(
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    ))
}

/// A client key is the base64 form of 16 bytes: 22 characters and `==`.
pub fn valid_key(key: &str) -> bool {
    key.len() == 24
        && key.ends_with("==")
        && key[..22]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// Whether the client offers the Mynou subprotocol.
pub fn offers_subprotocol(header: &str) -> bool {
    header.split(',').any(|item| item.trim() == SUBPROTOCOL)
}

/// The origin must name the same authority the request was sent to.
pub fn same_origin(origin: &str, host: &str) -> bool {
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

pub fn encode(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => frame.push(n as u8),
        n if n <= 65_535 => {
            frame.push(126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    frame
}

#[derive(Debug, PartialEq)]
pub enum Frame {
    Text(String),
    Ping(Vec<u8>),
    Pong,
    Close,
}

/// Removes one complete client frame from `buffer`; `Ok(None)` waits for more.
pub fn decode(buffer: &mut Vec<u8>) -> Result<Option<Frame>> {
    if buffer.len() < 2 {
        return Ok(None);
    }
    let (b0, b1) = (buffer[0], buffer[1]);
    if b0 & 0x70 != 0 {
        return Err("reserved bits are not supported".into());
    }
    if b0 & 0x80 == 0 {
        return Err("fragmented messages are not supported".into());
    }
    if b1 & 0x80 == 0 {
        return Err("client frames must be masked".into());
    }
    let opcode = b0 & 0x0f;
    let (length, header) = match b1 & 0x7f {
        126 if buffer.len() >= 4 => (usize::from(u16::from_be_bytes([buffer[2], buffer[3]])), 4),
        127 if buffer.len() >= 10 => {
            let n = u64::from_be_bytes(buffer[2..10].try_into().map_err(|_| "bad length")?);
            (usize::try_from(n).unwrap_or(usize::MAX), 10)
        }
        126 | 127 => return Ok(None),
        n => (usize::from(n), 2),
    };
    if length > MAX_CLIENT_FRAME || opcode >= 8 && length > 125 {
        return Err("frame is too large".into());
    }
    if buffer.len() < header + 4 + length {
        return Ok(None);
    }
    let mask = [
        buffer[header],
        buffer[header + 1],
        buffer[header + 2],
        buffer[header + 3],
    ];
    let payload: Vec<u8> = buffer[header + 4..header + 4 + length]
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ mask[i % 4])
        .collect();
    buffer.drain(..header + 4 + length);
    match opcode {
        1 => Ok(Some(Frame::Text(
            String::from_utf8(payload).map_err(|_| "text frames must be UTF-8")?,
        ))),
        8 => Ok(Some(Frame::Close)),
        9 => Ok(Some(Frame::Ping(payload))),
        10 => Ok(Some(Frame::Pong)),
        _ => Err("unsupported frame type".into()),
    }
}

fn send(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> Result<()> {
    crate::server::write_within(
        stream,
        &encode(opcode, payload),
        Instant::now() + WRITE_LIMIT,
    )
}

fn reply(id: &Value, status: u16, body: &str) -> String {
    format!(
        "{{\"id\":{},\"status\":{status},\"body\":{body}}}",
        json::stringify(id)
    )
}

fn failure(message: &str) -> String {
    let mut v = Value::object();
    v.insert("error", message);
    json::stringify(&v)
}

/// Runs one call and returns the response text.
fn call(engine: &Arc<Engine>, route: Route, message: &Value) -> String {
    let id = match message.get("id") {
        Some(id @ Value::Number(_)) => id.clone(),
        _ => return reply(&Value::Null, 400, &failure("id must be a number")),
    };
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let path = message.get("path").and_then(Value::as_str).unwrap_or("");
    let bare = path.split('?').next().unwrap_or("");
    if !matches!(method, "GET" | "POST")
        || !path.starts_with("/api/")
        || path.len() > 4096
        || path
            .bytes()
            .any(|b| b < 33 || b == 127 || b == b'#' || b == b'\\')
        || bare == "/api/shutdown"
    {
        return reply(
            &id,
            400,
            &failure("This call is not available over the socket"),
        );
    }
    let body = match message.get("body") {
        None | Some(Value::Null) => Vec::new(),
        Some(v @ Value::Object(_)) => json::stringify(v).into_bytes(),
        Some(_) => return reply(&id, 400, &failure("body must be an object")),
    };
    if method == "GET" && !body.is_empty() {
        return reply(&id, 400, &failure("GET calls have no body"));
    }
    let (status, value) = route(engine, method, path, &body)
        .unwrap_or_else(|e| (400, json::parse(&failure(&e)).unwrap_or(Value::Null)));
    reply(&id, status, &json::stringify(&value))
}

struct Watch {
    path: String,
    every: Duration,
    due: Instant,
    last: Option<String>,
}

/// Serves an upgraded socket until it closes, errors or the engine stops.
pub fn serve(stream: &mut TcpStream, engine: &Arc<Engine>, route: Route) -> Result<()> {
    stream
        .set_read_timeout(Some(TICK))
        .map_err(|e| e.to_string())?;
    let mut watches: BTreeMap<String, Watch> = BTreeMap::new();
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut heard = Instant::now();
    let mut pinged = Instant::now();
    loop {
        if engine.stopped.load(Ordering::Acquire) {
            return send(stream, 8, &1001u16.to_be_bytes());
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e.to_string()),
        }
        loop {
            let frame = match decode(&mut buffer) {
                Ok(Some(frame)) => {
                    // Only a complete frame counts as life: a trickled
                    // partial frame cannot hold a slot past the idle limit.
                    heard = Instant::now();
                    frame
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = send(stream, 8, &1002u16.to_be_bytes());
                    return Err(e);
                }
            };
            match frame {
                Frame::Close => return send(stream, 8, &1000u16.to_be_bytes()),
                Frame::Ping(data) => send(stream, 10, &data)?,
                Frame::Pong => {}
                Frame::Text(text) => {
                    let message = match json::parse(&text) {
                        Ok(v) => v,
                        Err(_) => {
                            send(
                                stream,
                                1,
                                reply(&Value::Null, 400, &failure("Invalid JSON")).as_bytes(),
                            )?;
                            continue;
                        }
                    };
                    if let Some(topics) = message.get("subscribe") {
                        let names = topics.as_array().filter(|a| a.len() <= MAX_TOPICS);
                        let names: Option<Vec<&str>> =
                            names.and_then(|a| a.iter().map(Value::as_str).collect());
                        let wanted: Option<Vec<_>> =
                            names.and_then(|names| names.iter().map(|n| topic(n)).collect());
                        match wanted {
                            Some(wanted) => {
                                watches.retain(|name, _| wanted.iter().any(|(t, _, _)| t == name));
                                for (name, path, every) in wanted {
                                    watches.entry(name).or_insert(Watch {
                                        path,
                                        every,
                                        due: Instant::now(),
                                        last: None,
                                    });
                                }
                            }
                            None => send(
                                stream,
                                1,
                                reply(&Value::Null, 400, &failure("Unknown topic")).as_bytes(),
                            )?,
                        }
                    } else {
                        let response = call(engine, route, &message);
                        if response.len() > MAX_MESSAGE {
                            let id = message.get("id").cloned().unwrap_or(Value::Null);
                            send(
                                stream,
                                1,
                                reply(&id, 413, &failure("Response is too large")).as_bytes(),
                            )?;
                        } else {
                            send(stream, 1, response.as_bytes())?;
                        }
                    }
                }
            }
        }
        let now = Instant::now();
        for (name, watch) in &mut watches {
            if now < watch.due {
                continue;
            }
            watch.due = now + watch.every;
            let (status, body) = match route(engine, "GET", &watch.path, &[]) {
                Ok((status, value)) => (status, json::stringify(&value)),
                Err(e) => (400, failure(&e)),
            };
            let text = format!("{{\"topic\":\"{name}\",\"status\":{status},\"data\":{body}}}");
            if watch.last.as_deref() != Some(text.as_str()) && text.len() <= MAX_MESSAGE {
                send(stream, 1, text.as_bytes())?;
                watch.last = Some(text);
            }
        }
        if now.duration_since(heard) > IDLE_LIMIT {
            return send(stream, 8, &1001u16.to_be_bytes());
        }
        if now.duration_since(pinged) > PING_EVERY {
            pinged = now;
            send(stream, 9, b"")?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masked(opcode: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
        let mask = [1u8, 2, 3, 4];
        let mut frame = vec![if fin { 0x80 } else { 0 } | opcode];
        match payload.len() {
            n if n < 126 => frame.push(0x80 | n as u8),
            n if n <= 65_535 => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        frame
    }

    #[test]
    fn rfc_6455_accept_key() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert!(valid_key("dGhlIHNhbXBsZSBub25jZQ=="));
        assert!(!valid_key("short"));
    }

    #[test]
    fn text_frames_of_every_length_class_decode() {
        for len in [0usize, 5, 125, 126, 300, 65_535] {
            let text = "x".repeat(len);
            let mut buffer = masked(1, true, text.as_bytes());
            assert_eq!(decode(&mut buffer).unwrap(), Some(Frame::Text(text)));
            assert!(buffer.is_empty());
        }
    }

    #[test]
    fn partial_frames_wait_and_pipelined_frames_split() {
        let whole = masked(1, true, b"hello");
        let mut buffer = whole[..whole.len() - 1].to_vec();
        assert_eq!(decode(&mut buffer).unwrap(), None);
        buffer.extend_from_slice(&whole[whole.len() - 1..]);
        buffer.extend_from_slice(&masked(9, true, b"p"));
        assert_eq!(
            decode(&mut buffer).unwrap(),
            Some(Frame::Text("hello".into()))
        );
        assert_eq!(
            decode(&mut buffer).unwrap(),
            Some(Frame::Ping(b"p".to_vec()))
        );
    }

    #[test]
    fn protocol_violations_are_errors() {
        let mut unmasked = vec![0x81, 0x01, b'x'];
        assert!(decode(&mut unmasked).is_err());
        assert!(decode(&mut masked(1, false, b"x")).is_err());
        assert!(decode(&mut masked(1, true, &vec![b'x'; 65_537])).is_err());
        assert!(decode(&mut masked(9, true, &[0; 126])).is_err());
        assert!(decode(&mut masked(2, true, b"x")).is_err());
        assert!(decode(&mut masked(1, true, &[0xff, 0xfe])).is_err());
        let mut reserved = masked(1, true, b"x");
        reserved[0] |= 0x40;
        assert!(decode(&mut reserved).is_err());
    }

    #[test]
    fn server_frames_use_the_shortest_length() {
        assert_eq!(encode(1, b"hi"), [0x81, 2, b'h', b'i']);
        assert_eq!(&encode(1, &[0; 126])[..4], &[0x81, 126, 0, 126]);
        assert_eq!(&encode(1, &vec![0; 70_000])[..2], &[0x81, 127]);
    }

    #[test]
    fn the_subprotocol_must_be_offered() {
        assert!(offers_subprotocol("mynou.v1"));
        assert!(offers_subprotocol("other, mynou.v1"));
        assert!(!offers_subprotocol(""));
        assert!(!offers_subprotocol("mynou.v2"));
    }

    #[test]
    fn origin_must_match_the_host() {
        assert!(same_origin("http://127.0.0.1:8787", "127.0.0.1:8787"));
        assert!(!same_origin("http://evil.example", "127.0.0.1:8787"));
        assert!(!same_origin("null", "127.0.0.1:8787"));
    }
}
