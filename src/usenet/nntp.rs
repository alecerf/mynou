//! Original bounded read-only NNTP. No posting, STARTTLS downgrade or raw error leaks.
use super::{nzb::valid_message_id, settings::Server};
use crate::{Result, net::DeadlineStream, tls::TlsStream};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Instant,
};
const MAX_STATUS: usize = 512;
const MAX_LINE: usize = 65_536;
enum Wire {
    Plain(DeadlineStream),
    Tls(Box<TlsStream>),
}
impl Read for Wire {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(b),
            Self::Tls(s) => s.read(b),
        }
    }
}
impl Write for Wire {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(b),
            Self::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}
struct Connection {
    input: BufReader<Wire>,
    deadline: Instant,
}
fn remaining(deadline: Instant) -> Result<std::time::Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "NNTP: deadline exceeded".into())
}
fn credential(name: &str, username: bool) -> Result<String> {
    let s = std::env::var(name).map_err(|_| "NNTP: authentication unavailable")?;
    if s.is_empty()
        || s.len() > 4096
        || !s
            .bytes()
            .all(|b| b.is_ascii_graphic() || (!username && b == b' '))
    {
        return Err("NNTP: authentication unavailable".into());
    }
    Ok(s)
}
impl Connection {
    fn open(s: &Server, deadline: Instant) -> Result<Self> {
        // Resolve secrets before opening a socket; failures never cause fallback.
        let auth = match (&s.username_env, &s.password_env) {
            (Some(u), Some(p)) => Some((credential(u, true)?, credential(p, false)?)),
            (None, None) => None,
            _ => return Err("NNTP: authentication unavailable".into()),
        };
        remaining(deadline)?;
        let addresses = (s.host.as_str(), s.port)
            .to_socket_addrs()
            .map_err(|_| "NNTP: resolution failed")?;
        remaining(deadline)?;
        let mut connected = None;
        for address in addresses.take(16) {
            if let Ok(tcp) = TcpStream::connect_timeout(&address, remaining(deadline)?) {
                connected = Some(tcp);
                break;
            }
        }
        let tcp = connected.ok_or("NNTP: connection failed")?;
        let _ = tcp.set_nodelay(true);
        let tcp = DeadlineStream::new(tcp, deadline);
        let wire = if s.tls {
            Wire::Tls(Box::new(
                TlsStream::connect_stream(tcp, &s.host).map_err(|_| "NNTP: verified TLS failed")?,
            ))
        } else {
            Wire::Plain(tcp)
        };
        Self::authenticate(wire, deadline, auth)
    }
    fn authenticate(wire: Wire, deadline: Instant, auth: Option<(String, String)>) -> Result<Self> {
        let mut conn = Self {
            input: BufReader::with_capacity(8192, wire),
            deadline,
        };
        let (code, _) = conn.status()?;
        if !matches!(code, 200 | 201) {
            return Err("NNTP: greeting rejected".into());
        }
        if let Some((user, pass)) = auth {
            conn.command(&format!("AUTHINFO USER {user}"))?;
            let (code, _) = conn.status()?;
            match code {
                281 => {}
                381 => {
                    conn.command(&format!("AUTHINFO PASS {pass}"))?;
                    if conn.status()?.0 != 281 {
                        return Err("NNTP: authentication rejected".into());
                    }
                }
                _ => return Err("NNTP: authentication rejected".into()),
            }
        }
        remaining(deadline)?;
        Ok(conn)
    }
    fn command(&mut self, command: &str) -> Result<()> {
        remaining(self.deadline)?;
        if command.len() > 8192 || command.bytes().any(|b| b.is_ascii_control()) {
            return Err("NNTP: invalid command".into());
        }
        self.input
            .get_mut()
            .write_all(command.as_bytes())
            .and_then(|()| self.input.get_mut().write_all(b"\r\n"))
            .and_then(|()| self.input.get_mut().flush())
            .map_err(|_| "NNTP: command failed".into())
    }
    fn line(&mut self, limit: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            remaining(self.deadline)?;
            let n = {
                let buf = self.input.fill_buf().map_err(|_| "NNTP: response failed")?;
                if buf.is_empty() {
                    return Err("NNTP: truncated response".into());
                }
                let n = buf
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(buf.len(), |i| i + 1);
                if out.len() + n > limit + 2 {
                    return Err("NNTP: response line exceeds the limit".into());
                }
                out.extend_from_slice(&buf[..n]);
                n
            };
            self.input.consume(n);
            if out.last() == Some(&b'\n') {
                if out.len() < 2
                    || out[out.len() - 2] != b'\r'
                    || out[..out.len() - 2].contains(&b'\r')
                {
                    return Err("NNTP: invalid line ending".into());
                }
                out.truncate(out.len() - 2);
                return Ok(out);
            }
        }
    }
    fn status(&mut self) -> Result<(u16, String)> {
        let line = self.line(MAX_STATUS)?;
        if line.len() < 4
            || !line[..3].iter().all(u8::is_ascii_digit)
            || line[3] != b' '
            || !line.iter().all(|b| b.is_ascii_graphic() || *b == b' ')
        {
            return Err("NNTP: malformed status".into());
        }
        let code = u16::from(line[0] - b'0') * 100
            + u16::from(line[1] - b'0') * 10
            + u16::from(line[2] - b'0');
        let message =
            String::from_utf8(line[4..].to_vec()).map_err(|_| "NNTP: malformed status")?;
        Ok((code, message))
    }
}
fn finish_probe(mut conn: Connection) -> Result<()> {
    conn.command("QUIT")?;
    if conn.status()?.0 != 205 {
        return Err("NNTP: quit rejected".into());
    }
    remaining(conn.deadline)?;
    Ok(())
}
#[cfg(test)]
pub(crate) fn test_verified_tls_probe(mut stream: TlsStream) -> Result<()> {
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    stream.set_deadline(deadline);
    let auth = Some((credential("PWD", true)?, credential("PATH", false)?));
    finish_probe(Connection::authenticate(
        Wire::Tls(Box::new(stream)),
        deadline,
        auth,
    )?)
}
/// A connection/authentication/QUIT probe, with no article request or persistent work.
pub fn probe(s: &Server) -> Result<()> {
    probe_guarded(s, None)
}
pub(super) fn probe_guarded(s: &Server, guard: Option<&str>) -> Result<()> {
    let mut h = s.health.try_lock().map_err(|_| "NNTP: server is busy")?;
    if guard.is_some_and(|g| g != s.guard(&h)) {
        return Err("NNTP: probe review is stale; preview again".into());
    }
    h.attempts = h
        .attempts
        .checked_add(1)
        .ok_or("NNTP: attempt counter exhausted")?;
    let result = Connection::open(s, Instant::now() + s.timeout()).and_then(finish_probe);
    if result.is_ok() {
        h.successes = h.successes.saturating_add(1);
        h.last_error = None;
    } else {
        h.last_error = Some("probe_failed");
    }
    result
}
/// Fetch one explicitly addressed article body. No automatic fallback or posting.
/// The caller must still validate yEnc content and its acquisition policy.
pub fn body(s: &Server, id: &str) -> Result<Vec<u8>> {
    if !valid_message_id(id) {
        return Err("NNTP: invalid article identity".into());
    }
    let mut h = s.health.try_lock().map_err(|_| "NNTP: server is busy")?;
    h.attempts = h
        .attempts
        .checked_add(1)
        .ok_or("NNTP: attempt counter exhausted")?;
    let result = (|| -> Result<Vec<u8>> {
        let mut conn = Connection::open(s, Instant::now() + s.timeout())?;
        conn.command(&format!("BODY <{id}>"))?;
        let (code, message) = conn.status()?;
        if code != 222 {
            return Err("NNTP: article unavailable".into());
        }
        let mut fields = message.split(' ');
        let number = fields.next().ok_or("NNTP: invalid body identity")?;
        if number.is_empty()
            || !number.bytes().all(|b| b.is_ascii_digit())
            || number.parse::<u64>().is_err()
            || fields.next() != Some(format!("<{id}>").as_str())
        {
            return Err("NNTP: invalid body identity".into());
        }
        let mut out = Vec::new();
        loop {
            let line = conn.line(MAX_LINE)?;
            if line == b"." {
                remaining(conn.deadline)?;
                return Ok(out);
            }
            let line = if line.starts_with(b"..") {
                &line[1..]
            } else {
                if line.starts_with(b".") {
                    return Err("NNTP: unescaped leading dot".into());
                }
                &line[..]
            };
            if line.contains(&0) || out.len() + line.len() + 2 > s.max_article_bytes {
                return Err("NNTP: article body exceeds bounds".into());
            }
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
    })();
    if result.is_ok() {
        h.successes = h.successes.saturating_add(1);
        h.last_error = None;
    } else {
        h.last_error = Some("body_failed");
    }
    result
}
