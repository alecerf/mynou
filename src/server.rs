//! Local authenticated HTTP API with bounded requests and no framework.
use crate::{
    Result,
    crypto::constant_time_eq,
    engine::{Engine, lock, public_job},
    integrations,
    json::{self, Value},
    store::Request,
    web::{BrowserRequest, Web},
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
    web: Arc<Web>,
}
impl Api {
    pub fn bind(engine: Arc<Engine>, token: String) -> Result<Self> {
        if token.len() < 32 || token.len() > 4096 || token.chars().any(char::is_control) {
            return Err(
                "The API token must contain 32 to 4096 bytes and no control characters".into(),
            );
        }
        let listener = TcpListener::bind(&engine.config.listen)
            .map_err(|e| format!("Cannot bind the API listener: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(Self {
            listener,
            token,
            engine,
            web: Arc::new(Web::new()),
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
                    let web = self.web.clone();
                    threads.push(thread::spawn(move || {
                        let _ = connection(&mut stream, &engine, &token, &web);
                    }));
                }
                Ok((mut stream, _)) => {
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                    let _ = respond(&mut stream, 503, error("API busy"));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(e) => return Err(format!("Cannot accept an API connection: {e}")),
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
            return Err("Headers are too large or timed out".into());
        }
        stream
            .read_exact(&mut byte)
            .map_err(|_| "Incomplete headers")?;
        data.push(byte[0]);
    }
    let text = std::str::from_utf8(&data).map_err(|_| "Headers are not valid UTF-8")?;
    let mut lines = text[..text.len() - 4].split("\r\n");
    let first = lines.next().ok_or("Missing request")?;
    let parts: Vec<_> = first.split(' ').collect();
    if parts.len() != 3
        || parts[2] != "HTTP/1.1"
        || !parts[1].starts_with('/')
        || parts[1].contains(['#', '\\'])
        || parts[1].len() > 4096
        || !parts[0].bytes().all(|b| b.is_ascii_uppercase())
    {
        return Err("Invalid HTTP request line".into());
    }
    let mut headers = BTreeMap::new();
    for line in lines {
        let (key, value) = line.split_once(':').ok_or("Invalid header")?;
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b < 32 && b != 9 || b == 127)
        {
            return Err("Invalid header".into());
        }
        let key = key.to_ascii_lowercase();
        if headers.insert(key, value.trim().to_owned()).is_some() {
            return Err("Duplicate headers are not allowed".into());
        }
    }
    if !headers.contains_key("host")
        || headers.contains_key("transfer-encoding")
        || headers.contains_key("expect")
    {
        return Err("Host is required; Transfer-Encoding and Expect are not supported".into());
    }
    let length = match headers.get("content-length") {
        Some(v) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => {
            v.parse::<usize>().map_err(|_| "Invalid content length")?
        }
        Some(_) => return Err("Invalid content length".into()),
        None => 0,
    };
    if length > MAX_BODY {
        return Err("Request body is too large".into());
    }
    if parts[0] == "GET" && length != 0 {
        return Err("GET requests must not have a body".into());
    }
    let mut body = vec![0; length];
    let mut offset = 0;
    while offset < length {
        if std::time::Instant::now() > deadline {
            return Err("Request body timed out".into());
        }
        let n = stream
            .read(&mut body[offset..])
            .map_err(|_| "Incomplete request body")?;
        if n == 0 {
            return Err("Incomplete request body".into());
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
fn connection(stream: &mut TcpStream, engine: &Arc<Engine>, token: &str, web: &Web) -> Result<()> {
    let HttpRequest {
        method,
        target: path,
        headers,
        body,
    } = match read_request(stream) {
        Ok(v) => v,
        Err(e) => return respond(stream, 400, error(&e)),
    };
    let browser_path = path.split('?').next().unwrap_or(&path);
    if browser_path == "/" || browser_path == "/ui" || browser_path.starts_with("/ui/") {
        return web
            .handle(
                engine,
                token,
                BrowserRequest {
                    method: &method,
                    target: &path,
                    headers: &headers,
                    body: &body,
                },
            )
            .write(stream);
    }
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
        return respond(stream, 401, error("Authentication required"));
    }
    if method == "POST"
        && !body.is_empty()
        && headers
            .get("content-type")
            .is_none_or(|v| v.split(';').next() != Some("application/json"))
    {
        return respond(
            stream,
            415,
            error("Content-Type application/json is required"),
        );
    }
    match route(engine, &method, &path, &body) {
        Ok((status, value)) => respond(stream, status, value),
        Err(e) => respond(stream, 400, error(&e)),
    }
}
fn route(engine: &Arc<Engine>, method: &str, path: &str, body: &[u8]) -> Result<(u16, Value)> {
    match (method, path) {
        ("GET", "/api/status") => Ok((200, engine.status()?)),
        ("GET", "/api/transfers") => Ok((200, engine.transfers()?)),
        ("GET", "/api/library") => Ok((200, engine.library()?)),
        ("GET", "/api/series") => Ok((200, engine.series()?)),
        ("GET", "/api/requesters") => Ok((200, engine.requesters()?)),
        ("GET", "/api/irc") => Ok((200, engine.irc_sources()?)),
        ("GET", "/api/notifications") => Ok((200, engine.notifications(0, 100)?)),
        ("POST", "/api/notifications/dispatch") => {
            control_body(body, &[], true)?;
            Ok((200, engine.dispatch_notifications()?))
        }
        ("POST", "/api/notifications/control") => {
            let v = control_body(
                body,
                &["kind", "event_id", "action", "apply", "plan_id"],
                false,
            )?;
            Ok((
                200,
                engine
                    .notification_control(&crate::notifications::ControlRequest::from_json(&v)?)?,
            ))
        }
        ("POST", "/api/requesters/sync") => {
            control_body(body, &[], true)?;
            Ok((200, engine.sync_requesters()?))
        }
        ("POST", "/api/series") => {
            let value = control_body(
                body,
                &["request", "include_specials", "future_only", "enabled"],
                false,
            )?;
            let request = Request::from_json(value.get("request").ok_or("request is required")?)?;
            let include_specials = optional_boolean(&value, "include_specials")?.unwrap_or(false);
            let future_only = optional_boolean(&value, "future_only")?.unwrap_or(false);
            let enabled = optional_boolean(&value, "enabled")?.unwrap_or(true);
            Ok((
                201,
                engine.track_series_with_policy(
                    &request,
                    include_specials,
                    future_only,
                    enabled,
                )?,
            ))
        }
        ("POST", "/api/upgrades") => {
            let value = control_body(body, &["apply"], true)?;
            let apply = match value.get("apply") {
                None => false,
                Some(value) => value.as_bool().ok_or("apply must be a boolean")?,
            };
            Ok((200, engine.check_upgrades(apply)?))
        }
        ("GET", "/api/jobs") => Ok((
            200,
            Value::Array(lock(&engine.store)?.list().iter().map(public_job).collect()),
        )),
        ("POST", "/api/search") => {
            let value = json::parse(
                std::str::from_utf8(body).map_err(|_| "Request body is not valid UTF-8")?,
            )?;
            let request = Request::from_json(&value)?;
            Ok((200, integrations::search_report(&engine.config, &request)?))
        }
        ("GET", "/api/profiles") => Ok((200, engine.config.selection.to_json())),
        ("POST", "/api/jobs") => {
            let v = json::parse(
                std::str::from_utf8(body).map_err(|_| "Request body is not valid UTF-8")?,
            )?;
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
            if let Some(query) = path.strip_prefix("/api/notifications?") {
                if method != "GET" {
                    return Err("Notifications: unsupported request".into());
                }
                let (offset, limit) = crate::requesters::page(query)?;
                return Ok((200, engine.notifications(offset, limit)?));
            }
            if let Some(tail) = path.strip_prefix("/api/irc/") {
                let (tail, query) = tail.split_once('?').unwrap_or((tail, ""));
                if method == "GET" && tail == "announcements" {
                    let (offset, limit) = crate::irc::page(query)?;
                    return Ok((200, engine.irc_announcements(offset, limit)?));
                }
                if method == "POST" && tail == "preview" && query.is_empty() {
                    if body.len() > 8192 {
                        return Err("IRC: preview body exceeds 8 KiB".into());
                    }
                    let value = control_body(body, &["source_id", "announcement", "text"], false)?;
                    let source = value
                        .get("source_id")
                        .and_then(Value::as_str)
                        .ok_or("IRC: source ID is required")?;
                    let report = match (value.get("announcement"), value.get("text")) {
                        (Some(a), None) => engine.irc_preview(source, a)?,
                        (None, Some(t)) => engine.irc_preview_text(
                            source,
                            t.as_str().ok_or("IRC: text must be a string")?,
                        )?,
                        _ => return Err("IRC: specify exactly one announcement or text".into()),
                    };
                    return Ok((200, report));
                }
                if let Some(tail) = tail.strip_prefix("announcements/") {
                    let parts: Vec<_> = tail.split('/').collect();
                    if !crate::irc::valid_announcement_id(parts[0]) {
                        return Ok((404, error("Unknown IRC announcement")));
                    }
                    if method == "GET" && parts.len() == 1 && query.is_empty() {
                        return Ok((200, engine.irc_announcement(parts[0])?));
                    }
                    if method == "POST"
                        && parts.len() == 2
                        && parts[1] == "control"
                        && query.is_empty()
                    {
                        let value = control_body(body, &["action", "apply", "plan_id"], false)?;
                        return Ok((
                            200,
                            engine.irc_control(
                                parts[0],
                                &crate::irc::ControlRequest::from_json(&value)?,
                            )?,
                        ));
                    }
                }
                return Ok((404, error("Unknown IRC route")));
            }
            if let Some(tail) = path.strip_prefix("/api/requesters/") {
                let (tail, query) = tail.split_once('?').unwrap_or((tail, ""));
                let parts: Vec<_> = tail.split('/').collect();
                if !crate::requesters::valid_id(parts[0]) {
                    return Ok((404, error("Unknown requester account")));
                }
                if method == "GET" && parts.len() == 1 {
                    let (offset, limit) = crate::requesters::page(query)?;
                    return Ok((200, engine.requester(parts[0], offset, limit)?));
                }
                if method == "POST" && parts.len() == 2 && parts[1] == "control" && query.is_empty()
                {
                    let v = control_body(
                        body,
                        &["action", "policy", "demand_id", "apply", "plan_id"],
                        false,
                    )?;
                    return Ok((
                        200,
                        engine.requester_control(
                            parts[0],
                            &crate::requesters::ControlRequest::from_json(&v)?,
                        )?,
                    ));
                }
                return Ok((404, error("Unknown requester route")));
            }
            if method == "GET" && (path == "/api/calendar" || path.starts_with("/api/calendar?")) {
                let query = crate::series::CalendarQuery::parse(
                    path.split_once('?').map_or("", |(_, query)| query),
                )?;
                return Ok((200, engine.episode_calendar(&query)?));
            }
            if let Some(tail) = path.strip_prefix("/api/series/") {
                let parts: Vec<_> = tail.split('/').collect();
                if parts[0].len() != 32 || !parts[0].bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Ok((404, error("Unknown monitored series")));
                }
                let id = parts[0].to_ascii_lowercase();
                if parts.len() == 1 && method == "GET" {
                    return Ok((200, engine.series_record(&id)?));
                }
                if parts.len() != 2 || method != "POST" {
                    return Ok((404, error("Unknown series route")));
                }
                let result = match parts[1] {
                    "numbering" => {
                        let value = control_body(body, &["changes", "apply", "plan_id"], false)?;
                        let query = crate::series::NumberingRequest::from_json(&value)?;
                        engine.series_numbering(&id, &query)?
                    }
                    "pack-search" => {
                        let value = control_body(
                            body,
                            &["season", "apply", "scope_id", "candidate_id"],
                            false,
                        )?;
                        let query = crate::pack::AutoPackRequest::from_json(&value)?;
                        engine.search_packs(&id, &query)?
                    }
                    "packs" => {
                        let value = control_body(body, &["source_url", "episodes"], false)?;
                        let pack = crate::pack::PackSubmission::from_json(&value)?;
                        engine.submit_pack(&id, &pack)?
                    }
                    "shared-file" => {
                        let value = control_body(
                            body,
                            &[
                                "source_url",
                                "file_path",
                                "season",
                                "episodes",
                                "apply",
                                "plan_id",
                            ],
                            false,
                        )?;
                        let query = crate::pack::SharedFileRequest::from_json(&value)?;
                        engine.shared_file(&id, &query)?
                    }
                    "refresh" => {
                        control_body(body, &[], true)?;
                        engine.refresh_series(&id)?
                    }
                    "monitor" => {
                        let value = control_body(
                            body,
                            &["enabled", "include_specials", "start_date"],
                            false,
                        )?;
                        if value.as_object().is_none_or(|fields| fields.is_empty()) {
                            return Err("Specify at least one series setting".into());
                        }
                        let monitored = optional_boolean(&value, "enabled")?;
                        let specials = optional_boolean(&value, "include_specials")?;
                        let start = match value.get("start_date") {
                            None => None,
                            Some(Value::Null) => Some(None),
                            Some(Value::String(text)) => Some(Some(text.clone())),
                            _ => return Err("start_date must be a date or null".into()),
                        };
                        engine.configure_series(&id, monitored, specials, start)?
                    }
                    "episodes" => {
                        let value = control_body(body, &["season", "episode", "enabled"], false)?;
                        let number = |key| {
                            value
                                .get(key)
                                .and_then(Value::as_u64)
                                .and_then(|number| u32::try_from(number).ok())
                                .ok_or_else(|| format!("{key} must be a nonnegative integer"))
                        };
                        let enabled =
                            optional_boolean(&value, "enabled")?.ok_or("enabled is required")?;
                        engine.monitor_series_episode(
                            &id,
                            number("season")?,
                            number("episode")?,
                            enabled,
                        )?
                    }
                    _ => return Ok((404, error("Unknown series operation"))),
                };
                return Ok((200, result));
            }
            if let Some(tail) = path.strip_prefix("/api/transfers/") {
                let parts: Vec<_> = tail.split('/').collect();
                let id = parts[0];
                if ![40, 64].contains(&id.len()) || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Ok((404, error("Unknown native transfer")));
                }
                let id = id.to_ascii_lowercase();
                if parts.len() == 1 && method == "GET" {
                    return Ok((200, engine.transfer(&id)?));
                }
                if parts.len() != 2 || method != "POST" {
                    return Ok((404, error("Unknown transfer route")));
                }
                let snapshot = match parts[1] {
                    "pause" | "resume" => {
                        control_body(body, &[], true)?;
                        if parts[1] == "pause" {
                            engine.pause_transfer(&id)?
                        } else {
                            engine.resume_transfer(&id)?
                        }
                    }
                    "priority" => {
                        let value = control_body(body, &["priority"], false)?;
                        let priority = value
                            .get("priority")
                            .and_then(Value::as_i64)
                            .and_then(|number| i32::try_from(number).ok())
                            .ok_or("priority must be an integer between -1000 and 1000")?;
                        engine.set_transfer_priority(&id, priority)?
                    }
                    "files" => {
                        let value = control_body(body, &["index", "priority"], false)?;
                        let index = value
                            .get("index")
                            .and_then(Value::as_u64)
                            .and_then(|number| usize::try_from(number).ok())
                            .ok_or("index must be a nonnegative file index")?;
                        let priority = match value.get("priority").and_then(Value::as_str) {
                            Some("low") => crate::torrent::FilePriority::Low,
                            Some("normal") => crate::torrent::FilePriority::Normal,
                            Some("high") => crate::torrent::FilePriority::High,
                            _ => return Err("File priority must be low, normal or high".into()),
                        };
                        engine.set_file_priority(&id, index, priority)?
                    }
                    "selection" => {
                        let value = control_body(body, &["all", "indices"], false)?;
                        let update = crate::torrent::SelectionUpdate::from_json(&value)?;
                        engine.select_transfer_files(&id, &update)?
                    }
                    "policy" => {
                        let value = control_body(body, &["policy"], false)?;
                        let policy = match value.get("policy").ok_or("policy is required")? {
                            Value::Null => None,
                            policy => Some(crate::torrent::TransferPolicy::from_json(policy)?),
                        };
                        engine.set_transfer_policy(&id, policy)?
                    }
                    _ => return Ok((404, error("Unknown transfer route"))),
                };
                return Ok((200, snapshot));
            }
            if let Some(tail) = path.strip_prefix("/api/library/") {
                let parts: Vec<_> = tail.split('/').collect();
                if parts.len() != 2
                    || method != "POST"
                    || parts[0].len() != 32
                    || !parts[0].bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Ok((404, error("Unknown library route")));
                }
                if parts[1] == "group" {
                    let value = control_body(
                        body,
                        &[
                            "action",
                            "release_title",
                            "source_url",
                            "file_path",
                            "apply",
                            "plan_id",
                        ],
                        false,
                    )?;
                    let query = crate::library::GroupRequest::from_json(&value)?;
                    return Ok((200, engine.library_group(parts[0], &query)?));
                }
                let job = match parts[1] {
                    "monitor" => {
                        let value = control_body(body, &["enabled"], false)?;
                        let enabled = value
                            .get("enabled")
                            .and_then(Value::as_bool)
                            .ok_or("enabled must be a boolean")?;
                        engine.set_monitored(parts[0], enabled)?
                    }
                    "baseline" => {
                        let value = control_body(body, &["release_title"], false)?;
                        let title = value
                            .get("release_title")
                            .and_then(Value::as_str)
                            .ok_or("release_title must be a string")?;
                        engine.set_baseline(parts[0], title)?
                    }
                    _ => return Ok((404, error("Unknown library route"))),
                };
                return Ok((200, public_job(&job)));
            }
            if let Some(tail) = path.strip_prefix("/api/jobs/") {
                let parts: Vec<_> = tail.split('/').collect();
                let id = parts[0];
                if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Ok((404, error("Unknown job")));
                }
                if parts.len() == 1 && method == "GET" {
                    return Ok(match lock(&engine.store)?.get(id) {
                        Some(job) => (200, public_job(&job)),
                        None => (404, error("Unknown job")),
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
                        ("POST", "pack-mapping") => {
                            let value = control_body(body, &["file_path"], false)?;
                            let path = value
                                .get("file_path")
                                .and_then(Value::as_str)
                                .ok_or("file_path must be a string")?;
                            return Ok((200, engine.remap_pack(id, path.into())?));
                        }
                        ("POST", "cancel") => return Ok((200, public_job(&engine.cancel(id)?))),
                        ("POST", "retry") => return Ok((200, public_job(&engine.retry(id)?))),
                        _ => {}
                    }
                }
            }
            Ok((404, error("Unknown route")))
        }
    }
}

fn optional_boolean(value: &Value, key: &str) -> Result<Option<bool>> {
    value
        .get(key)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| format!("{key} must be a boolean"))
        })
        .transpose()
}

fn control_body(body: &[u8], allowed: &[&str], allow_empty: bool) -> Result<Value> {
    let value = if allow_empty && body.is_empty() {
        Value::object()
    } else {
        json::parse(std::str::from_utf8(body).map_err(|_| "Request body is not valid UTF-8")?)?
    };
    let fields = value
        .as_object()
        .ok_or("Request body must be a JSON object")?;
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("Unexpected request field".into());
    }
    Ok(value)
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
        .map_err(|e| format!("Cannot write the response: {e}"))
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
