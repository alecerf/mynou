//! One receiver per opt-in source; one shared shutdown monitor interrupts sockets.
use super::{
    Source,
    protocol::{Decoder, Event, Protocol},
};
use crate::{
    Result,
    engine::{Engine, lock},
    json::Value,
    net::DeadlineStream,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::{Shutdown, TcpStream, ToSocketAddrs},
    sync::{Arc, atomic::Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) struct Health {
    phase: String,
    attempts: u64,
    received: u64,
    duplicates: u64,
    last_received: u64,
    last_error: Option<String>,
    retry_in_secs: u64,
    sasl_authenticated: bool,
}
impl Health {
    pub(crate) fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("phase", self.phase.clone());
        v.insert("sasl_authenticated", self.sasl_authenticated);
        for (k, n) in [
            ("attempts", self.attempts),
            ("received", self.received),
            ("duplicates", self.duplicates),
            ("last_received", self.last_received),
            ("retry_in_secs", self.retry_in_secs),
        ] {
            v.insert(k, n.to_string());
        }
        v.insert(
            "last_error",
            self.last_error.clone().map_or(Value::Null, Value::from),
        );
        v
    }
}
pub(crate) struct Runtime {
    pub health: BTreeMap<String, Health>,
    sockets: BTreeMap<String, TcpStream>,
    started: BTreeSet<String>,
    monitor_started: bool,
}
impl Runtime {
    pub(crate) fn new(sources: &[Source]) -> Self {
        Self {
            health: sources
                .iter()
                .map(|s| {
                    (
                        s.id.clone(),
                        Health {
                            phase: if s.enabled {
                                "disconnected"
                            } else {
                                "disabled"
                            }
                            .into(),
                            attempts: 0,
                            received: 0,
                            duplicates: 0,
                            last_received: 0,
                            last_error: None,
                            retry_in_secs: 0,
                            sasl_authenticated: false,
                        },
                    )
                })
                .collect(),
            sockets: BTreeMap::new(),
            started: BTreeSet::new(),
            monitor_started: false,
        }
    }
}
fn health(engine: &Engine, id: &str, phase: &str, error: Option<&str>, delay: u64) {
    if let Ok(mut runtime) = engine.irc_runtime.lock()
        && let Some(h) = runtime.health.get_mut(id)
    {
        h.phase = phase.into();
        h.last_error = error.map(str::to_owned);
        h.retry_in_secs = delay;
        if matches!(phase, "connecting" | "backoff" | "stopped") {
            h.sasl_authenticated = false;
        }
    }
}
pub(crate) fn start(engine: &Arc<Engine>, handles: &mut Vec<JoinHandle<()>>) {
    let sources = if let Ok(mut runtime) = engine.irc_runtime.lock() {
        engine
            .config
            .irc
            .sources
            .iter()
            .filter(|s| s.enabled && runtime.started.insert(s.id.clone()))
            .cloned()
            .collect::<Vec<_>>()
    } else {
        return;
    };
    if sources.is_empty() {
        return;
    }
    let monitor = if let Ok(mut r) = engine.irc_runtime.lock() {
        let start = !r.monitor_started;
        r.monitor_started = true;
        start
    } else {
        false
    };
    if monitor {
        let e = engine.clone();
        handles.push(thread::spawn(move || {
            while !e.stopped.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(50));
            }
            if let Ok(r) = e.irc_runtime.lock() {
                for socket in r.sockets.values() {
                    let _ = socket.shutdown(Shutdown::Both);
                }
            }
        }));
    }
    for source in sources {
        let engine = engine.clone();
        handles.push(thread::spawn(move || {
            let mut delay = source.reconnect_min_secs;
            while !engine.stopped.load(Ordering::Acquire) {
                health(&engine, &source.id, "connecting", None, 0);
                if let Ok(mut r) = engine.irc_runtime.lock()
                    && let Some(h) = r.health.get_mut(&source.id)
                {
                    h.attempts = h.attempts.saturating_add(1);
                }
                let started = Instant::now();
                let result = connected(&engine, &source);
                if let Ok(mut r) = engine.irc_runtime.lock() {
                    r.sockets.remove(&source.id);
                }
                if engine.stopped.load(Ordering::Acquire) {
                    break;
                }
                if started.elapsed() >= Duration::from_secs(30) {
                    delay = source.reconnect_min_secs;
                }
                health(
                    &engine,
                    &source.id,
                    "backoff",
                    Some(if result.is_err() {
                        "connection_or_protocol_failed"
                    } else {
                        "connection_closed"
                    }),
                    delay,
                );
                engine.wait(delay * 1000);
                delay = delay.saturating_mul(2).min(source.reconnect_max_secs);
            }
            health(&engine, &source.id, "stopped", None, 0);
        }));
    }
}
enum Stream {
    Tcp(DeadlineStream),
    Tls(Box<crate::tls::TlsStream>),
}
impl Stream {
    fn deadline(&mut self, deadline: Instant) {
        match self {
            Self::Tcp(s) => s.set_deadline(deadline),
            Self::Tls(s) => s.set_deadline(deadline),
        }
    }
}
impl Read for Stream {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(b),
            Self::Tls(s) => s.read(b),
        }
    }
}
impl Write for Stream {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(b),
            Self::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}
fn credential(variable: &Option<String>) -> Result<Option<String>> {
    variable
        .as_ref()
        .map(|name| {
            let v = std::env::var(name).map_err(|_| "IRC: configured credential is unavailable")?;
            if v.is_empty() || v.len() > 256 || v.bytes().any(|b| b <= 32 || b == 127) {
                return Err("IRC: configured credential is invalid".into());
            }
            Ok(v)
        })
        .transpose()
}
fn send(stream: &mut Stream, line: &str) -> Result<()> {
    if line.len() > 510 || line.chars().any(char::is_control) {
        return Err("IRC: outgoing command exceeds bounds".into());
    }
    stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.write_all(b"\r\n"))
        .map_err(|_| "IRC: cannot send command".into())
}
fn connected(engine: &Engine, source: &Source) -> Result<()> {
    let password = credential(&source.password_env)?;
    let key = credential(&source.join_key_env)?;
    let mut authentication = source
        .sasl
        .as_ref()
        .map(super::sasl::Settings::commands)
        .transpose()?;
    let (url, tls) = source.endpoint()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut socket = None;
    for address in (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|_| "IRC: hostname lookup failed")?
        .take(8)
    {
        if engine.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("IRC: connection deadline exceeded")?;
        if let Ok(stream) = TcpStream::connect_timeout(&address, remaining) {
            socket = Some(stream);
            break;
        }
    }
    let socket = socket.ok_or("IRC: connection failed")?;
    socket
        .set_nodelay(true)
        .map_err(|_| "IRC: socket configuration failed")?;
    lock(&engine.irc_runtime)?.sockets.insert(
        source.id.clone(),
        socket
            .try_clone()
            .map_err(|_| "IRC: shutdown handle unavailable")?,
    );
    if engine.stopped.load(Ordering::Acquire) {
        let _ = socket.shutdown(Shutdown::Both);
        return Ok(());
    }
    let mut stream = if tls {
        Stream::Tls(Box::new(crate::tls::TlsStream::connect_stream(
            DeadlineStream::new(socket, deadline),
            &url.host,
        )?))
    } else {
        Stream::Tcp(DeadlineStream::new(socket, deadline))
    };
    health(engine, &source.id, "registering", None, 0);
    if let Some(password) = password {
        send(&mut stream, &format!("PASS {password}"))?;
    }
    if source.sasl.is_some() {
        send(&mut stream, "CAP LS 302")?;
    }
    send(&mut stream, &format!("NICK {}", source.nickname))?;
    send(&mut stream, &format!("USER {} 0 * :Mynou", source.nickname))?;
    let mut protocol = Protocol::new(source.clone());
    let mut decoder = Decoder::default();
    let mut buffer = [0; 4096];
    let mut partial_started = None;
    let mut last_complete = Instant::now();
    let mut ready = false;
    let mut window = Instant::now();
    let mut messages = 0_u32;
    while !engine.stopped.load(Ordering::Acquire) {
        let deadline = if !ready {
            deadline
        } else if let Some(started) = partial_started {
            started + Duration::from_secs(10)
        } else {
            last_complete + Duration::from_secs(source.idle_timeout_secs)
        };
        stream.deadline(deadline);
        let n = stream
            .read(&mut buffer)
            .map_err(|_| "IRC: read failed or timed out")?;
        if n == 0 {
            return Ok(());
        }
        let was_partial = decoder.has_partial();
        let decoded = decoder.feed(&buffer[..n])?;
        if !decoded.is_empty() {
            last_complete = Instant::now();
        }
        if decoder.has_partial() {
            if !was_partial || !decoded.is_empty() {
                partial_started = Some(Instant::now());
            }
        } else {
            partial_started = None;
        }
        for message in decoded {
            if window.elapsed() >= Duration::from_secs(1) {
                window = Instant::now();
                messages = 0;
            }
            messages += 1;
            if messages > 256 {
                return Err("IRC: message rate exceeds bounds".into());
            }
            match protocol.receive(&message)? {
                Event::Ignore => {}
                Event::Reply(reply) => {
                    send(&mut stream, &reply)?;
                    if reply == "AUTHENTICATE PLAIN" {
                        health(engine, &source.id, "authenticating", None, 0);
                    } else if reply == "CAP END" {
                        if let Some(h) = lock(&engine.irc_runtime)?.health.get_mut(&source.id) {
                            h.sasl_authenticated = true;
                        }
                        health(engine, &source.id, "registering", None, 0);
                    }
                }
                Event::Authenticate => {
                    for command in authentication
                        .take()
                        .ok_or("IRC: SASL response already sent")?
                    {
                        send(&mut stream, &command)?;
                    }
                }
                Event::Join => {
                    health(engine, &source.id, "joining", None, 0);
                    send(
                        &mut stream,
                        &format!(
                            "JOIN {}{}",
                            source.channel,
                            key.as_ref().map_or(String::new(), |s| format!(" {s}"))
                        ),
                    )?;
                }
                Event::Joined => {
                    ready = true;
                    health(engine, &source.id, "connected", None, 0);
                }
                Event::Announcement(body) => {
                    let value = source.decode_payload(&body)?.to_json();
                    let report =
                        engine.irc_receive(&source.id, &source.sender, &source.channel, &value)?;
                    let mut runtime = lock(&engine.irc_runtime)?;
                    if let Some(h) = runtime.health.get_mut(&source.id) {
                        h.received = h.received.saturating_add(1);
                        h.last_received = crate::store::now();
                        if report.get("duplicate").and_then(Value::as_bool) == Some(true) {
                            h.duplicates = h.duplicates.saturating_add(1);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
