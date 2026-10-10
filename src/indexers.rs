//! Native bounded source authentication. Sessions are ephemeral and origin-bound.
pub(crate) mod feed;
pub(crate) mod policy;
mod session;
pub(crate) mod watch;
use crate::{
    Result,
    config::Source,
    json::Value,
    net::{self, HttpClient, Response},
};
pub use policy::ControlRequest;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
#[derive(Clone, Debug, Default)]
pub enum Authentication {
    #[default]
    None,
    Bearer {
        token_env: String,
    },
    Basic {
        username_env: String,
        password_env: String,
    },
    Form(Form),
}
#[derive(Clone, Debug)]
pub struct Form {
    pub login_url: String,
    pub username_env: String,
    pub password_env: String,
    pub username_field: String,
    pub password_field: String,
    pub cookie_name: String,
    pub max_age_secs: u64,
}
impl Authentication {
    pub fn mode(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer { .. } => "bearer",
            Self::Basic { .. } => "basic",
            Self::Form(_) => "form",
        }
    }
}
/// Opt-in bounded polling of an RSS or Torznab feed for new releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watch {
    pub enabled: bool,
    pub interval_secs: u64,
}
impl Default for Watch {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 900,
        }
    }
}
#[derive(Clone)]
pub struct Options {
    pub id: Option<String>,
    policy_initialized: Arc<AtomicBool>,
    policy_enabled: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    reset_pending: Arc<AtomicBool>,
    pub enabled: bool,
    pub min_interval_ms: u64,
    pub authentication: Authentication,
    pub watch: Watch,
    runtime: Arc<Mutex<Runtime>>,
}
impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("enabled", &self.enabled)
            .field("min_interval_ms", &self.min_interval_ms)
            .field("authentication", &self.authentication.mode())
            .finish_non_exhaustive()
    }
}
impl Default for Options {
    fn default() -> Self {
        Self {
            id: None,
            policy_initialized: Arc::new(AtomicBool::new(false)),
            policy_enabled: Arc::new(AtomicBool::new(true)),
            generation: Arc::new(AtomicU64::new(0)),
            reset_pending: Arc::new(AtomicBool::new(false)),
            enabled: true,
            min_interval_ms: 0,
            authentication: Authentication::None,
            watch: Watch::default(),
            runtime: Arc::new(Mutex::new(Runtime::default())),
        }
    }
}
#[derive(Default)]
struct Runtime {
    session: Option<session::Session>,
    last_start: Option<Instant>,
    blocked_until: Option<Instant>,
    requests: u64,
    successes: u64,
    failures: u64,
    last_status: Option<u16>,
    last_error: Option<&'static str>,
    last_parse: Option<bool>,
}
fn text(v: &Value, k: &str) -> Result<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Indexer: invalid {k}"))
}
fn environment(v: &Value, k: &str) -> Result<String> {
    let s = text(v, k)?;
    if s.is_empty() || s.len() > 128 || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err("Indexer: invalid credential environment name".into());
    }
    Ok(s)
}
fn integer(v: &Value, k: &str, default: u64, max: u64) -> Result<u64> {
    match v.get(k) {
        None => Ok(default),
        Some(v) => v
            .as_u64()
            .filter(|n| *n <= max)
            .ok_or_else(|| format!("Indexer: invalid {k}")),
    }
}
fn token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
fn watch(v: &Value) -> Result<Watch> {
    let Some(section) = v.get("watch") else {
        return Ok(Watch::default());
    };
    crate::numbering::only(section, &["enabled", "interval_secs"])?;
    let enabled = match section.get("enabled") {
        None => false,
        Some(Value::Bool(b)) => *b,
        _ => return Err("Indexer: invalid watch flag".into()),
    };
    let watch = Watch {
        enabled,
        interval_secs: integer(section, "interval_secs", 900, 86_400)?,
    };
    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("json");
    if watch.interval_secs < 60 || (watch.enabled && !matches!(kind, "rss" | "torznab")) {
        return Err("Indexer: watching requires an rss or torznab source and an interval of 60 to 86400 seconds".into());
    }
    Ok(watch)
}
impl Options {
    /// A watched source is opted in and not paused.
    pub(crate) fn watched(&self) -> bool {
        self.watch.enabled && self.effective_enabled()
    }
    pub(crate) fn from_json(v: &Value, url: &str) -> Result<Self> {
        let enabled = match v.get("enabled") {
            None => true,
            Some(Value::Bool(b)) => *b,
            _ => return Err("Indexer: invalid enabled flag".into()),
        };
        let id = match v.get("id") {
            None => None,
            Some(Value::String(s)) if crate::requesters::valid_id(s) => Some(s.clone()),
            _ => return Err("Indexer: invalid stable source ID".into()),
        };
        let mut options = Self {
            id,
            enabled,
            min_interval_ms: integer(v, "min_interval_ms", 0, 60_000)?,
            watch: watch(v)?,
            ..Self::default()
        };
        if let Some(auth) = v.get("authentication") {
            let method = text(auth, "method")?;
            options.authentication = match method.as_str() {
                "none" => {
                    crate::numbering::only(auth, &["method"])?;
                    Authentication::None
                }
                "bearer" => {
                    crate::numbering::only(auth, &["method", "token_env"])?;
                    Authentication::Bearer {
                        token_env: environment(auth, "token_env")?,
                    }
                }
                "basic" => {
                    crate::numbering::only(auth, &["method", "username_env", "password_env"])?;
                    Authentication::Basic {
                        username_env: environment(auth, "username_env")?,
                        password_env: environment(auth, "password_env")?,
                    }
                }
                "form" => {
                    crate::numbering::only(
                        auth,
                        &[
                            "method",
                            "login_url",
                            "username_env",
                            "password_env",
                            "username_field",
                            "password_field",
                            "cookie_name",
                            "max_age_secs",
                        ],
                    )?;
                    let login_url = text(auth, "login_url")?;
                    let endpoint = net::parse_url(url)?;
                    let login = net::parse_url(&login_url)?;
                    if login.origin() != endpoint.origin() || login_url.contains(['?', '#']) {
                        return Err("Indexer: login endpoint must share the source origin without queries or fragments".into());
                    }
                    let form = Form {
                        login_url,
                        username_env: environment(auth, "username_env")?,
                        password_env: environment(auth, "password_env")?,
                        username_field: text(auth, "username_field")?,
                        password_field: text(auth, "password_field")?,
                        cookie_name: text(auth, "cookie_name")?,
                        max_age_secs: integer(auth, "max_age_secs", 1800, 3600)?,
                    };
                    if form.max_age_secs == 0
                        || !token(&form.cookie_name)
                        || !token(&form.username_field)
                        || !token(&form.password_field)
                        || form.username_field == form.password_field
                    {
                        return Err("Indexer: invalid form or cookie settings".into());
                    }
                    Authentication::Form(form)
                }
                _ => return Err("Indexer: unsupported authentication method".into()),
            };
        }
        if !matches!(options.authentication, Authentication::None) {
            let u = net::parse_url(url)?;
            if u.scheme != "https"
                && !u
                    .host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            {
                return Err("Indexer: authentication requires HTTPS or literal loopback".into());
            }
        }
        Ok(options)
    }
}
fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "Indexer: request budget expired".into())
}
fn secret(name: &str) -> Result<String> {
    let s = std::env::var(name).map_err(|_| "Indexer: authentication unavailable")?;
    if s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control) {
        return Err("Indexer: authentication unavailable".into());
    }
    Ok(s)
}
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for b in bytes.chunks(3) {
        let n = (u32::from(b[0]) << 16)
            | (u32::from(*b.get(1).unwrap_or(&0)) << 8)
            | u32::from(*b.get(2).unwrap_or(&0));
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if b.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if b.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
pub(crate) fn fetch(
    source: &Source,
    url: &str,
    accept: &str,
    deadline: Instant,
) -> Result<Response> {
    let runtime = &source.options.runtime;
    let mut state = runtime.try_lock().map_err(|_| "Indexer: source is busy")?;
    let generation = source.options.generation.load(Ordering::Acquire);
    if !source.options.effective_enabled() {
        state.last_error = Some("disabled");
        return Err("Indexer: source is disabled".into());
    }
    let now = Instant::now();
    if state.blocked_until.is_some_and(|at| at > now)
        || state.last_start.is_some_and(|at| {
            now.duration_since(at) < Duration::from_millis(source.options.min_interval_ms)
        })
    {
        state.last_error = Some("rate_limited");
        return Err("Indexer: source rate limit reached".into());
    }
    state.last_start = Some(now);
    state.requests = state.requests.saturating_add(1);
    if source.options.reset_pending.swap(false, Ordering::AcqRel) {
        state.session = None;
    }
    let mut result = if generation == source.options.generation.load(Ordering::Acquire) {
        fetch_before(source, url, accept, deadline, &mut state)
    } else {
        Err("Indexer: source policy changed during the request".into())
    };
    if generation != source.options.generation.load(Ordering::Acquire) {
        state.session = None;
        state.last_error = Some("policy_changed");
        result = Err("Indexer: source policy changed during the request".into());
    }
    match &result {
        Ok(_) => {
            state.successes = state.successes.saturating_add(1);
            state.last_error = None;
        }
        Err(_) => {
            state.failures = state.failures.saturating_add(1);
            if state.last_error.is_none() {
                state.last_error = Some("request_failed");
            }
        }
    }
    result
}
fn fetch_before(
    source: &Source,
    url: &str,
    accept: &str,
    deadline: Instant,
    state: &mut Runtime,
) -> Result<Response> {
    state.last_error = None;
    let origin = net::parse_url(&source.url)?.origin();
    if net::parse_url(url)?.origin() != origin {
        return Err("Indexer: request origin changed".into());
    }
    for attempt in 0..2 {
        let mut headers = vec![("Accept".into(), accept.into())];
        match &source.options.authentication {
            Authentication::None => {}
            Authentication::Bearer { token_env } => {
                let value = secret(token_env)?;
                let core = value.trim_end_matches('=');
                if core.is_empty()
                    || !core.bytes().all(|b| {
                        b.is_ascii_alphanumeric()
                            || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
                    })
                {
                    return Err("Indexer: authentication unavailable".into());
                }
                headers.push(("Authorization".into(), format!("Bearer {value}")));
            }
            Authentication::Basic {
                username_env,
                password_env,
            } => {
                let user = secret(username_env)?;
                if user.contains(':') {
                    return Err("Indexer: authentication unavailable".into());
                }
                headers.push((
                    "Authorization".into(),
                    format!(
                        "Basic {}",
                        base64(format!("{user}:{}", secret(password_env)?).as_bytes())
                    ),
                ));
            }
            Authentication::Form(form) => {
                if state
                    .session
                    .as_ref()
                    .is_none_or(|s| s.expires <= Instant::now())
                {
                    state.session = None;
                    state.session = Some(session::login(form, &source.url, deadline)?);
                }
                headers.push((
                    "Cookie".into(),
                    state
                        .session
                        .as_ref()
                        .ok_or("Indexer: missing session")?
                        .header(&source.url)?,
                ));
            }
        }
        let response = HttpClient::new()
            .with_timeout(remaining(deadline)?.min(Duration::from_secs(20)))
            .with_max_body(8 * 1024 * 1024)
            .without_redirects()
            .request("GET", url, &headers, &[])
            .map_err(|_| "Indexer: network request failed")?;
        remaining(deadline)?;
        state.last_status = Some(response.status);
        if response.status == 401
            && attempt == 0
            && matches!(source.options.authentication, Authentication::Form(_))
        {
            state.session = None;
            continue;
        }
        if response.status == 401 {
            state.session = None;
            state.last_error = Some("authentication_rejected");
            return Err("Indexer: authentication rejected".into());
        }
        if response.status == 429 {
            let seconds = response
                .headers
                .get("retry-after")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(60)
                .clamp(1, 3600);
            state.blocked_until = Some(Instant::now() + Duration::from_secs(seconds));
            state.last_error = Some("rate_limited");
            return Err("Indexer: source rate limit reached".into());
        }
        if !(200..300).contains(&response.status) {
            state.last_error = Some("response_rejected");
            return Err("Indexer: response rejected".into());
        }
        return Ok(response);
    }
    Err("Indexer: authentication rejected".into())
}
/// Remaining 429 cooldown of a source, if any. A busy source reports none.
pub(crate) fn cooldown(source: &Source) -> Option<Duration> {
    let state = source.options.runtime.try_lock().ok()?;
    state
        .blocked_until
        .and_then(|at| at.checked_duration_since(Instant::now()))
        .filter(|remaining| !remaining.is_zero())
}
pub(crate) fn parsed(source: &Source, ok: bool) {
    if let Ok(mut s) = source.options.runtime.try_lock() {
        s.last_parse = Some(ok);
        if !ok {
            s.last_error = Some("parse_rejected")
        }
    }
}
pub fn report(sources: &[Source]) -> Value {
    let mut v = Value::object();
    v.insert(
        "sources",
        Value::Array(
            sources
                .iter()
                .map(|source| {
                    let mut v = Value::object();
                    v.insert("name", crate::integrations::report_text(&source.name, 128));
                    v.insert("kind", source.kind.clone());
                    v.insert("enabled", source.options.effective_enabled());
                    v.insert("id", source.options.identity(source));
                    v.insert("authentication", source.options.authentication.mode());
                    v.insert("min_interval_ms", source.options.min_interval_ms as u32);
                    if let Ok(s) = source.options.runtime.try_lock() {
                        v.insert("busy", false);
                        v.insert("requests", s.requests.to_string());
                        v.insert("successes", s.successes.to_string());
                        v.insert("failures", s.failures.to_string());
                        v.insert(
                            "last_status",
                            s.last_status.map_or(Value::Null, |n| u32::from(n).into()),
                        );
                        v.insert("last_error", s.last_error.map_or(Value::Null, Value::from));
                        v.insert("last_parse", s.last_parse.map_or(Value::Null, Value::from));
                        v.insert(
                            "session_active",
                            !source.options.reset_pending.load(Ordering::Acquire)
                                && s.session
                                    .as_ref()
                                    .is_some_and(|s| s.expires > Instant::now()),
                        );
                    } else {
                        v.insert("busy", true);
                    }
                    v
                })
                .collect(),
        ),
    );
    v
}
