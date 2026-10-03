//! API HTTP locale, authentifiée et bornée, sans framework.
use crate::{
    Result,
    crypto::constant_time_eq,
    engine::{Engine, lock, public_job},
    json::{self, Value},
    store::Request,
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Arc, atomic::Ordering},
    thread::{self, JoinHandle},
    time::Duration,
};

const MAX_BODY: usize = 1_048_576;
const MAX_HEADER: usize = 16_384;
pub struct Api {
    listener: TcpListener,
    token: String,
    engine: Arc<Engine>,
}
impl Api {
    pub fn bind(engine: Arc<Engine>, token: String) -> Result<Self> {
        if token.len() < 32 || token.len() > 4096 || token.chars().any(char::is_control) {
            return Err("Le jeton API doit contenir au moins 32 caractères sans contrôle".into());
        }
        let listener = TcpListener::bind(&engine.config.listen)
            .map_err(|e| format!("Écoute API impossible : {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(Self {
            listener,
            token,
            engine,
        })
    }
    pub fn address(&self) -> Result<SocketAddr> {
        self.listener.local_addr().map_err(|e| e.to_string())
    }
    pub fn run(self) -> Result<()> {
        let mut threads: Vec<JoinHandle<()>> = Vec::new();
        while !self.engine.stopped.load(Ordering::Acquire) {
            let mut i = 0;
            while i < threads.len() {
                if threads[i].is_finished() {
                    let h = threads.swap_remove(i);
                    let _ = h.join();
                } else {
                    i += 1;
                }
            }
            match self.listener.accept() {
                Ok((mut stream, _)) if threads.len() < 32 => {
                    let engine = self.engine.clone();
                    let token = self.token.clone();
                    threads.push(thread::spawn(move || {
                        let _ = connection(&mut stream, &engine, &token);
                    }));
                }
                Ok((mut stream, _)) => {
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                    let _ = respond(&mut stream, 503, error("API occupée"));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(e) => return Err(format!("Acceptation API impossible : {e}")),
            }
        }
        for h in threads {
            let _ = h.join();
        }
        Ok(())
    }
}

fn error(message: &str) -> Value {
    let mut v = Value::object();
    v.insert("error", message);
    v
}
struct HttpRequest {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
fn read_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut data = Vec::new();
    let mut byte = [0];
    while !data.ends_with(b"\r\n\r\n") {
        if data.len() >= MAX_HEADER || std::time::Instant::now() > deadline {
            return Err("En-têtes trop grands ou trop lents".into());
        }
        stream
            .read_exact(&mut byte)
            .map_err(|_| "En-têtes incomplets")?;
        data.push(byte[0]);
    }
    let text = std::str::from_utf8(&data).map_err(|_| "En-têtes non UTF-8")?;
    let mut lines = text[..text.len() - 4].split("\r\n");
    let first = lines.next().ok_or("Requête absente")?;
    let parts: Vec<_> = first.split(' ').collect();
    if parts.len() != 3
        || parts[2] != "HTTP/1.1"
        || !parts[1].starts_with('/')
        || parts[1].contains(['#', '\\'])
        || parts[1].len() > 4096
        || !parts[0].bytes().all(|b| b.is_ascii_uppercase())
    {
        return Err("Ligne HTTP invalide".into());
    }
    let mut headers = BTreeMap::new();
    for line in lines {
        let (key, value) = line.split_once(':').ok_or("En-tête invalide")?;
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b < 32 && b != 9 || b == 127)
        {
            return Err("En-tête invalide".into());
        }
        let key = key.to_ascii_lowercase();
        if headers.insert(key, value.trim().to_owned()).is_some() {
            return Err("En-tête dupliqué interdit".into());
        }
    }
    if !headers.contains_key("host")
        || headers.contains_key("transfer-encoding")
        || headers.contains_key("expect")
    {
        return Err("Host requis ; transfert segmenté et Expect non pris en charge".into());
    }
    let length = match headers.get("content-length") {
        Some(v) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => {
            v.parse::<usize>().map_err(|_| "Longueur invalide")?
        }
        Some(_) => return Err("Longueur invalide".into()),
        None => 0,
    };
    if length > MAX_BODY {
        return Err("Corps trop grand".into());
    }
    if parts[0] == "GET" && length != 0 {
        return Err("Corps GET interdit".into());
    }
    let mut body = vec![0; length];
    let mut offset = 0;
    while offset < length {
        if std::time::Instant::now() > deadline {
            return Err("Corps trop lent".into());
        }
        let n = stream
            .read(&mut body[offset..])
            .map_err(|_| "Corps incomplet")?;
        if n == 0 {
            return Err("Corps incomplet".into());
        }
        offset += n;
    }
    Ok(HttpRequest {
        method: parts[0].into(),
        target: parts[1].into(),
        headers,
        body,
    })
}
fn connection(stream: &mut TcpStream, engine: &Arc<Engine>, token: &str) -> Result<()> {
    let HttpRequest {
        method,
        target: path,
        headers,
        body,
    } = match read_request(stream) {
        Ok(v) => v,
        Err(e) => return respond(stream, 400, error(&e)),
    };
    if method == "GET" && (path == "/healthz" || path == "/readyz") {
        let mut v = Value::object();
        v.insert(
            "status",
            if engine.stopped.load(Ordering::Acquire) {
                "stopping"
            } else {
                "ok"
            },
        );
        v.insert("version", env!("CARGO_PKG_VERSION"));
        return respond(
            stream,
            if engine.stopped.load(Ordering::Acquire) {
                503
            } else {
                200
            },
            v,
        );
    }
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(bearer.as_bytes(), token.as_bytes()) {
        return respond(stream, 401, error("Authentification requise"));
    }
    if method == "POST"
        && !body.is_empty()
        && headers
            .get("content-type")
            .is_none_or(|v| v.split(';').next() != Some("application/json"))
    {
        return respond(stream, 415, error("Content-Type application/json requis"));
    }
    match route(engine, &method, &path, &body) {
        Ok((status, value)) => respond(stream, status, value),
        Err(e) => respond(stream, 400, error(&e)),
    }
}
fn route(engine: &Arc<Engine>, method: &str, path: &str, body: &[u8]) -> Result<(u16, Value)> {
    match (method, path) {
        ("GET", "/api/status") => Ok((200, engine.status()?)),
        ("GET", "/api/jobs") => Ok((
            200,
            Value::Array(lock(&engine.store)?.list().iter().map(public_job).collect()),
        )),
        ("POST", "/api/jobs") => {
            let v = json::parse(std::str::from_utf8(body).map_err(|_| "Corps non UTF-8")?)?;
            let request = Request::from_json(&v)?;
            Ok((
                201,
                Value::Array(engine.submit(request)?.iter().map(public_job).collect()),
            ))
        }
        ("POST", "/api/sync") => {
            let mut v = Value::object();
            v.insert("submitted", Value::Number(engine.sync()? as f64));
            Ok((200, v))
        }
        ("POST", "/api/shutdown") => {
            engine.stopped.store(true, Ordering::Release);
            let mut v = Value::object();
            v.insert("status", "stopping");
            Ok((200, v))
        }
        _ => {
            if let Some(tail) = path.strip_prefix("/api/jobs/") {
                let parts: Vec<_> = tail.split('/').collect();
                let id = parts[0];
                if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Ok((404, error("Demande inconnue")));
                }
                if parts.len() == 1 && method == "GET" {
                    return Ok(match lock(&engine.store)?.get(id) {
                        Some(job) => (200, public_job(&job)),
                        None => (404, error("Demande inconnue")),
                    });
                }
                if parts.len() == 2 {
                    match (method, parts[1]) {
                        ("GET", "events") => {
                            return Ok((
                                200,
                                Value::Array(
                                    lock(&engine.store)?
                                        .events(id)
                                        .iter()
                                        .map(|e| e.to_json())
                                        .collect(),
                                ),
                            ));
                        }
                        ("POST", "cancel") => return Ok((200, public_job(&engine.cancel(id)?))),
                        ("POST", "retry") => return Ok((200, public_job(&engine.retry(id)?))),
                        _ => {}
                    }
                }
            }
            Ok((404, error("Route inconnue")))
        }
    }
}
fn respond(stream: &mut TcpStream, status: u16, value: Value) -> Result<()> {
    let body = json::stringify(&value);
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        415 => "Unsupported Media Type",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body.as_bytes()))
        .map_err(|e| format!("Réponse impossible : {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(raw: &[u8]) -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let bytes = raw.to_vec();
        let writer = thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(&bytes).unwrap();
        });
        let (mut c, _) = listener.accept().unwrap();
        let result = read_request(&mut c).map(|_| ());
        writer.join().unwrap();
        result
    }
    #[test]
    fn reject_smuggling() {
        for raw in [
            b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nContent-Length: 1\r\n\r\n"
                .as_slice(),
            b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\n\r\nx",
        ] {
            assert!(parse(raw).is_err());
        }
        assert!(parse(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n").is_ok());
    }
}
