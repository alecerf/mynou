//! Immutable provider settings. Only environment names are configured.
use crate::{Result, json::Value};
use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
pub(super) struct Health {
    pub epoch: String,
    pub attempts: u64,
    pub successes: u64,
    pub last_error: Option<&'static str>,
}
#[derive(Clone)]
pub struct Server {
    pub(super) id: String,
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) tls: bool,
    pub(super) username_env: Option<String>,
    pub(super) password_env: Option<String>,
    pub(super) timeout_ms: u64,
    pub(super) max_article_bytes: usize,
    pub(super) health: Arc<Mutex<Health>>,
}
impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsenetServer")
            .field("id", &self.id)
            .field("tls", &self.tls)
            .field("authenticated", &self.username_env.is_some())
            .finish_non_exhaustive()
    }
}
fn text(v: &Value, k: &str) -> Result<String> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("Usenet: invalid {k}"))
}
fn environment(v: &Value, k: &str) -> Result<Option<String>> {
    match v.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s))
            if !s.is_empty()
                && s.len() <= 128
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
        {
            Ok(Some(s.clone()))
        }
        _ => Err("Usenet: invalid credential environment name".into()),
    }
}
fn number(v: &Value, k: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    match v.get(k) {
        None => Ok(default),
        Some(v) => v
            .as_u64()
            .filter(|n| *n >= min && *n <= max)
            .ok_or_else(|| format!("Usenet: invalid {k}")),
    }
}
fn valid_host(s: &str) -> bool {
    if s.parse::<IpAddr>().is_ok() {
        return true;
    }
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|p| {
            !p.is_empty()
                && p.len() <= 63
                && !p.starts_with('-')
                && !p.ends_with('-')
                && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}
impl Server {
    pub fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(
            v,
            &[
                "id",
                "host",
                "port",
                "tls",
                "username_env",
                "password_env",
                "timeout_ms",
                "max_article_bytes",
            ],
        )?;
        let id = text(v, "id")?;
        let host = text(v, "host")?;
        if !crate::requesters::valid_id(&id) || !valid_host(&host) {
            return Err("Usenet: invalid server identity or host".into());
        }
        let tls = match v.get("tls") {
            None => true,
            Some(Value::Bool(b)) => *b,
            _ => return Err("Usenet: invalid TLS flag".into()),
        };
        if !tls && !host.parse::<IpAddr>().is_ok_and(|a| a.is_loopback()) {
            return Err("Usenet: remote servers require verified TLS".into());
        }
        let username_env = environment(v, "username_env")?;
        let password_env = environment(v, "password_env")?;
        if username_env.is_some() != password_env.is_some() {
            return Err(
                "Usenet: username and password environment names are required together".into(),
            );
        }
        Ok(Self {
            id,
            host,
            port: number(v, "port", if tls { 563 } else { 119 }, 1, 65535)? as u16,
            tls,
            username_env,
            password_env,
            timeout_ms: number(v, "timeout_ms", 10_000, 100, 30_000)?,
            max_article_bytes: number(
                v,
                "max_article_bytes",
                16 * 1024 * 1024,
                1,
                super::yenc::MAX_ARTICLE_BYTES as u64,
            )? as usize,
            health: Arc::new(Mutex::new(Health {
                epoch: crate::requesters::digest(&crate::crypto::random_bytes::<16>()?),
                ..Health::default()
            })),
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
    /// Stable provider identity including credential names, never credential values.
    pub fn binding(&self) -> String {
        let v = Value::Array(vec![
            self.id.clone().into(),
            self.host.clone().into(),
            u32::from(self.port).into(),
            self.tls.into(),
            self.username_env.clone().map_or(Value::Null, Value::from),
            self.password_env.clone().map_or(Value::Null, Value::from),
            self.timeout_ms.to_string().into(),
            self.max_article_bytes.to_string().into(),
        ]);
        crate::requesters::digest(crate::json::stringify(&v).as_bytes())
    }
    pub(super) fn guard(&self, h: &Health) -> String {
        crate::requesters::digest(
            crate::json::stringify(&Value::Array(vec![
                self.binding().into(),
                h.epoch.clone().into(),
                h.attempts.to_string().into(),
            ]))
            .as_bytes(),
        )
    }
    pub(super) fn preview_guard(&self) -> Result<String> {
        let h = self.health.try_lock().map_err(|_| "NNTP: server is busy")?;
        Ok(self.guard(&h))
    }
    pub(super) fn report(&self) -> Value {
        let mut v = Value::object();
        v.insert("id", self.id.clone());
        v.insert("tls", self.tls);
        v.insert("authenticated", self.username_env.is_some());
        if let Ok(h) = self.health.try_lock() {
            v.insert("busy", false);
            v.insert("attempts", h.attempts.to_string());
            v.insert("successes", h.successes.to_string());
            v.insert("last_error", h.last_error.map_or(Value::Null, Value::from));
        } else {
            v.insert("busy", true);
        }
        v
    }
}
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub servers: Vec<Server>,
    pub downloads: Option<Downloads>,
}
#[derive(Clone, Debug)]
pub struct Downloads {
    pub enabled: bool,
    pub state_dir: PathBuf,
    pub max_active: usize,
    pub max_attempts: u8,
    pub max_file_bytes: u64,
    /// Explicit opt-in. Limits are captured in canonical selection provenance.
    pub zip: Option<crate::archive::Limits>,
}
impl Downloads {
    fn parse(v: &Value, base: &Path) -> Result<Self> {
        crate::numbering::only(
            v,
            &[
                "enabled",
                "state_dir",
                "max_active",
                "max_attempts",
                "max_file_bytes",
                "zip",
            ],
        )?;
        let enabled = match v.get("enabled") {
            None => false,
            Some(Value::Bool(b)) => *b,
            _ => return Err("Usenet: invalid downloads enabled flag".into()),
        };
        let state_dir = match v.get("state_dir") {
            None => "state/usenet".to_owned(),
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            _ => return Err("Usenet: invalid downloads state directory".into()),
        };
        Ok(Self {
            enabled,
            state_dir: crate::config::path(base, state_dir)?,
            max_active: number(v, "max_active", 2, 1, 2)? as usize,
            max_attempts: number(v, "max_attempts", 3, 1, 10)? as u8,
            max_file_bytes: number(v, "max_file_bytes", 64 << 30, 1, 1 << 40)?,
            zip: match v.get("zip") {
                None | Some(Value::Null) => None,
                Some(v) => Some(crate::archive::Limits::from_json(v)?),
            },
        })
    }
}
impl Settings {
    pub fn from_json(v: Option<&Value>) -> Result<Self> {
        Self::from_json_at(v, Path::new("."))
    }
    pub(crate) fn from_json_at(v: Option<&Value>, base: &Path) -> Result<Self> {
        let Some(v) = v else {
            return Ok(Self::default());
        };
        crate::numbering::only(v, &["servers", "downloads"])?;
        let rows = v
            .get("servers")
            .and_then(Value::as_array)
            .filter(|a| a.len() <= 8)
            .ok_or("Usenet: expected at most eight servers")?;
        let mut servers = Vec::<Server>::new();
        for v in rows {
            let s = Server::from_json(v)?;
            if servers.iter().any(|old| old.id == s.id) {
                return Err("Usenet: duplicate server ID".into());
            }
            servers.push(s);
        }
        let downloads = v
            .get("downloads")
            .map(|v| Downloads::parse(v, base))
            .transpose()?;
        Ok(Self { servers, downloads })
    }
    pub fn report(&self) -> Value {
        let mut v = Value::object();
        v.insert(
            "servers",
            Value::Array(self.servers.iter().map(Server::report).collect()),
        );
        v.insert(
            "downloads_enabled",
            self.downloads.as_ref().is_some_and(|d| d.enabled),
        );
        v
    }
}
