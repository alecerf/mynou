//! Durable per-account demand and explicit acquisition policies.
pub(crate) mod engine;
mod persistence;

use crate::{
    Result,
    config::Config,
    crypto::sha256,
    json::{self, Value},
    selection::Profile,
    store::Request,
};
pub(crate) use persistence::RequesterStore;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub const MAX_ACCOUNTS: usize = 32;
pub const MAX_DEMANDS: usize = 10_000;
pub const MAX_POLL_ITEMS: usize = 512;
pub const MAX_NOTIFICATIONS: usize = 1_000;

pub fn page(query: &str) -> Result<(usize, usize)> {
    let mut offset = 0;
    let mut limit = 100;
    let mut seen = BTreeSet::new();
    if !query.is_empty() {
        for pair in query.split('&') {
            let (key, value) = pair
                .split_once('=')
                .ok_or("Requester: invalid page query")?;
            if !["offset", "limit"].contains(&key)
                || !seen.insert(key)
                || value.is_empty()
                || !value.bytes().all(|b| b.is_ascii_digit())
            {
                return Err("Requester: unknown or duplicate page field".into());
            }
            let value = value
                .parse::<usize>()
                .map_err(|_| "Requester: invalid page bound")?;
            if key == "offset" {
                offset = value
            } else {
                limit = value
            }
        }
    }
    if offset > MAX_DEMANDS || limit == 0 || limit > 200 {
        return Err("Requester: page bounds exceeded".into());
    }
    Ok((offset, limit))
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    sha256(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}
pub(crate) fn valid_digest(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_profile(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
fn text(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Requester: invalid {key}"))
}
fn boolean(value: &Value, key: &str) -> Result<bool> {
    value
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("Requester: invalid {key}"))
}
fn integer(value: &Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .ok_or_else(|| format!("Requester: invalid {key}"))
}
fn optional(value: &Value, key: &str) -> Result<Option<String>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(format!("Requester: invalid {key}")),
    }
}
fn only(value: &Value, fields: &[&str]) -> Result<()> {
    crate::numbering::only(value, fields)
}

#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub expected_user_id: String,
    pub token_env: String,
    pub identity_url: String,
    pub watchlist_url: String,
}
impl Account {
    pub(crate) fn binding(&self) -> String {
        let values = [
            &self.id,
            &self.expected_user_id,
            &self.token_env,
            &self.identity_url,
            &self.watchlist_url,
        ];
        digest(
            json::stringify(&Value::Array(
                values
                    .into_iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ))
            .as_bytes(),
        )
    }
}
#[derive(Clone, Debug)]
pub struct Destination {
    pub id: String,
    pub movies_root: PathBuf,
    pub series_root: PathBuf,
}
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub accounts: Vec<Account>,
    pub destinations: Vec<Destination>,
}
impl Settings {
    pub(crate) fn from_json(value: Option<&Value>, base: &Path) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        only(value, &["accounts", "destinations"])?;
        let list = |key| -> Result<&[Value]> {
            match value.get(key) {
                None => Ok(&[]),
                Some(v) => v
                    .as_array()
                    .ok_or_else(|| format!("Requester configuration: {key} must be an array")),
            }
        };
        let mut accounts = Vec::new();
        let mut ids = BTreeSet::new();
        let mut users = BTreeSet::new();
        for v in list("accounts")? {
            only(
                v,
                &[
                    "id",
                    "expected_user_id",
                    "token_env",
                    "identity_url",
                    "watchlist_url",
                ],
            )?;
            let account = Account {
                id: text(v, "id")?,
                expected_user_id: text(v, "expected_user_id")?,
                token_env: text(v, "token_env")?,
                identity_url: v.get("identity_url").map_or_else(
                    || Ok("https://plex.tv/api/v2/user".into()),
                    |_| text(v, "identity_url"),
                )?,
                watchlist_url: v.get("watchlist_url").map_or_else(
                    || {
                        Ok(
                            "https://discover.provider.plex.tv/library/sections/watchlist/all"
                                .into(),
                        )
                    },
                    |_| text(v, "watchlist_url"),
                )?,
            };
            if accounts.len() >= MAX_ACCOUNTS
                || !valid_id(&account.id)
                || !ids.insert(account.id.clone())
                || account
                    .expected_user_id
                    .parse::<u64>()
                    .ok()
                    .is_none_or(|n| n == 0 || n.to_string() != account.expected_user_id)
                || account.expected_user_id.is_empty()
                || account.expected_user_id.len() > 32
                || !account.expected_user_id.bytes().all(|b| b.is_ascii_digit())
                || !users.insert(account.expected_user_id.clone())
                || account.token_env.is_empty()
                || account.token_env.len() > 128
                || !account
                    .token_env
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                return Err("Requester configuration: invalid or duplicate account binding".into());
            }
            for url in [&account.identity_url, &account.watchlist_url] {
                crate::net::parse_url(url)
                    .map_err(|_| "Requester configuration: invalid service URL")?;
            }
            accounts.push(account);
        }
        let mut destinations = Vec::new();
        ids.clear();
        for v in list("destinations")? {
            only(v, &["id", "movies_root", "series_root"])?;
            let id = text(v, "id")?;
            if destinations.len() >= MAX_ACCOUNTS
                || id == "default"
                || !valid_id(&id)
                || !ids.insert(id.clone())
            {
                return Err("Requester configuration: invalid or duplicate destination".into());
            }
            destinations.push(Destination {
                id,
                movies_root: crate::config::path(base, text(v, "movies_root")?)?,
                series_root: crate::config::path(base, text(v, "series_root")?)?,
            });
        }
        Ok(Self {
            accounts,
            destinations,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub enabled: bool,
    pub approval_required: bool,
    pub max_active: u32,
    pub max_daily: u32,
    pub movie_profile: String,
    pub episode_profile: String,
    pub destination: String,
    pub notifications: String,
}
impl Policy {
    pub(crate) fn initial(config: &Config) -> Self {
        Self {
            enabled: false,
            approval_required: true,
            max_active: 8,
            max_daily: 32,
            movie_profile: config.selection.movie_profile.clone(),
            episode_profile: config.selection.episode_profile.clone(),
            destination: "default".into(),
            notifications: "decisions".into(),
        }
    }
    pub fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "enabled",
                "approval_required",
                "max_active",
                "max_daily",
                "movie_profile",
                "episode_profile",
                "destination",
                "notifications",
            ],
        )?;
        let policy = Self {
            enabled: boolean(v, "enabled")?,
            approval_required: boolean(v, "approval_required")?,
            max_active: u32::try_from(integer(v, "max_active")?)
                .map_err(|_| "Requester: invalid max_active")?,
            max_daily: u32::try_from(integer(v, "max_daily")?)
                .map_err(|_| "Requester: invalid max_daily")?,
            movie_profile: text(v, "movie_profile")?,
            episode_profile: text(v, "episode_profile")?,
            destination: text(v, "destination")?,
            notifications: text(v, "notifications")?,
        };
        if policy.max_active > 64
            || policy.max_active == 0
            || policy.max_daily > 1024
            || policy.max_daily == 0
            || !valid_id(&policy.destination)
            || !valid_profile(&policy.movie_profile)
            || !valid_profile(&policy.episode_profile)
            || !["none", "decisions", "all"].contains(&policy.notifications.as_str())
        {
            return Err("Requester: invalid policy bounds or preferences".into());
        }
        Ok(policy)
    }
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("enabled", self.enabled);
        v.insert("approval_required", self.approval_required);
        v.insert("max_active", self.max_active);
        v.insert("max_daily", self.max_daily);
        v.insert("movie_profile", self.movie_profile.clone());
        v.insert("episode_profile", self.episode_profile.clone());
        v.insert("destination", self.destination.clone());
        v.insert("notifications", self.notifications.clone());
        v
    }
    pub(crate) fn validate_config(&self, config: &Config) -> Result<()> {
        Self::from_json(&self.to_json())?;
        for kind in ["movie", "episode"] {
            Capture::new(config, self, kind)?;
        }
        Ok(())
    }
}

/// Immutable behavior and destination, independent of later configuration edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    pub profile_name: String,
    pub profile: Profile,
    pub destination: String,
    pub movies_root: String,
    pub series_root: String,
}
impl Capture {
    pub(crate) fn new(config: &Config, policy: &Policy, kind: &str) -> Result<Self> {
        let name = match kind {
            "movie" => &policy.movie_profile,
            "episode" => &policy.episode_profile,
            _ => return Err("Requester: unsupported acquisition kind".into()),
        };
        let profile = config
            .selection
            .profiles
            .get(name)
            .ok_or("Requester: selected profile is unavailable")?
            .clone();
        let (movies, series) = if policy.destination == "default" {
            (&config.movies_root, &config.series_root)
        } else {
            let route = config
                .requesters
                .destinations
                .iter()
                .find(|r| r.id == policy.destination)
                .ok_or("Requester: destination is unavailable")?;
            (&route.movies_root, &route.series_root)
        };
        let capture = Self {
            profile_name: name.clone(),
            profile,
            destination: policy.destination.clone(),
            movies_root: movies
                .to_str()
                .ok_or("Requester: destination is not UTF-8")?
                .into(),
            series_root: series
                .to_str()
                .ok_or("Requester: destination is not UTF-8")?
                .into(),
        };
        Self::from_json(&capture.to_json())?;
        Ok(capture)
    }
    pub fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "profile_name",
                "profile",
                "destination",
                "movies_root",
                "series_root",
            ],
        )?;
        let result = Self {
            profile_name: text(v, "profile_name")?,
            profile: Profile::from_json(
                v.get("profile")
                    .ok_or("Requester: missing captured profile")?,
            )?,
            destination: text(v, "destination")?,
            movies_root: text(v, "movies_root")?,
            series_root: text(v, "series_root")?,
        };
        if !valid_profile(&result.profile_name) || !valid_id(&result.destination) {
            return Err("Requester: invalid captured identity".into());
        }
        for root in [&result.movies_root, &result.series_root] {
            if root.len() > 4096
                || root.chars().any(char::is_control)
                || !Path::new(root).is_absolute()
                || Path::new(root).components().any(|p| {
                    matches!(
                        p,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
            {
                return Err("Requester: invalid captured destination".into());
            }
        }
        Ok(result)
    }
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("profile_name", self.profile_name.clone());
        v.insert("profile", self.profile.to_json());
        v.insert("destination", self.destination.clone());
        v.insert("movies_root", self.movies_root.clone());
        v.insert("series_root", self.series_root.clone());
        v
    }
    pub(crate) fn configuration(&self, config: &Config, kind: &str) -> Config {
        let mut config = config.clone();
        config.movies_root = PathBuf::from(&self.movies_root);
        config.series_root = PathBuf::from(&self.series_root);
        config
            .selection
            .profiles
            .insert(self.profile_name.clone(), self.profile.clone());
        if kind == "movie" {
            config.selection.movie_profile = self.profile_name.clone()
        } else {
            config.selection.episode_profile = self.profile_name.clone()
        }
        config
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub account_id: String,
    pub demand_id: String,
    pub policy_revision: u64,
    pub capture: Capture,
}
impl Provenance {
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("account_id", self.account_id.clone());
        v.insert("demand_id", self.demand_id.clone());
        v.insert("policy_revision", self.policy_revision.to_string());
        v.insert("capture", self.capture.to_json());
        v
    }
    pub fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &["account_id", "demand_id", "policy_revision", "capture"],
        )?;
        let p = Self {
            account_id: text(v, "account_id")?,
            demand_id: text(v, "demand_id")?,
            policy_revision: integer(v, "policy_revision")?,
            capture: Capture::from_json(
                v.get("capture")
                    .ok_or("Requester: missing acquisition policy")?,
            )?,
        };
        if !valid_id(&p.account_id) || !valid_digest(&p.demand_id) || p.policy_revision == 0 {
            return Err("Requester: invalid acquisition provenance".into());
        }
        Ok(p)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Demand {
    pub id: String,
    pub account_id: String,
    pub origins: BTreeSet<String>,
    pub request: Request,
    pub revision: u64,
    pub capture: Capture,
    pub approved: bool,
    pub state: String,
    pub job_id: Option<String>,
    pub charged_at: Option<u64>,
    pub outcome: String,
}
impl Demand {
    pub(crate) fn identity(account: &str, request: &Request) -> String {
        digest(
            json::stringify(&Value::Array(vec![
                account.into(),
                request.media_key().into(),
            ]))
            .as_bytes(),
        )
    }
    pub(crate) fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("id", self.id.clone());
        v.insert("account_id", self.account_id.clone());
        v.insert(
            "origins",
            Value::Array(self.origins.iter().cloned().map(Value::String).collect()),
        );
        v.insert("request", self.request.to_json());
        v.insert("revision", self.revision.to_string());
        v.insert("capture", self.capture.to_json());
        v.insert("approved", self.approved);
        v.insert("state", self.state.clone());
        v.insert(
            "job_id",
            self.job_id.clone().map_or(Value::Null, Value::String),
        );
        v.insert(
            "charged_at",
            self.charged_at
                .map_or(Value::Null, |n| Value::String(n.to_string())),
        );
        v.insert("outcome", self.outcome.clone());
        v
    }
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "id",
                "account_id",
                "origins",
                "request",
                "revision",
                "capture",
                "approved",
                "state",
                "job_id",
                "charged_at",
                "outcome",
            ],
        )?;
        let d = Self {
            id: text(v, "id")?,
            account_id: text(v, "account_id")?,
            origins: v
                .get("origins")
                .and_then(Value::as_array)
                .ok_or("Requester: missing watchlist origins")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or("Requester: invalid watchlist origin".to_owned())
                })
                .collect::<Result<BTreeSet<_>>>()?,
            request: Request::from_json(
                v.get("request")
                    .ok_or("Requester: missing demand request")?,
            )?,
            revision: integer(v, "revision")?,
            capture: Capture::from_json(
                v.get("capture").ok_or("Requester: missing demand policy")?,
            )?,
            approved: boolean(v, "approved")?,
            state: text(v, "state")?,
            job_id: optional(v, "job_id")?,
            charged_at: match v.get("charged_at") {
                Some(Value::Null) => None,
                Some(_) => Some(integer(v, "charged_at")?),
                None => return Err("Requester: missing quota charge".into()),
            },
            outcome: text(v, "outcome")?,
        };
        if !valid_id(&d.account_id)
            || (d.origins.is_empty() && !matches!(d.state.as_str(), "removed" | "rejected"))
            || d.origins.len() > MAX_POLL_ITEMS
            || d.origins.iter().any(|origin| {
                origin.is_empty()
                    || origin.len() > 4096
                    || origin.chars().any(char::is_control)
                    || (origin.starts_with("irc:") && !crate::irc::admission::valid_origin(origin))
            })
            || d.id != Self::identity(&d.account_id, &d.request)
            || d.revision == 0
            || !matches!(d.request.kind.as_str(), "movie" | "episode")
            || d.request.source_path.is_some()
            || d.request.source_url.is_some()
            || ![
                "pending", "reserved", "active", "ready", "quota", "conflict", "rejected",
                "removed",
            ]
            .contains(&d.state.as_str())
            || ![
                "",
                "pending",
                "reserved",
                "active",
                "ready",
                "failed",
                "cancelled",
                "quota",
                "conflict",
                "rejected",
                "removed",
            ]
            .contains(&d.outcome.as_str())
            || d.job_id
                .as_ref()
                .is_some_and(|id| id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()))
            || (matches!(d.state.as_str(), "reserved" | "active" | "ready")
                && (!d.approved || d.charged_at.is_none()))
            || (matches!(d.state.as_str(), "active" | "ready") && d.job_id.is_none())
        {
            return Err("Requester: inconsistent persistent demand".into());
        }
        Ok(d)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub id: String,
    pub binding: String,
    pub policy: Policy,
    pub revision: u64,
    pub cursor: u64,
    pub snapshot: Option<String>,
    pub polled_at: u64,
    pub last_error: Option<String>,
}
impl Record {
    pub(crate) fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("id", self.id.clone());
        v.insert("binding", self.binding.clone());
        v.insert("policy", self.policy.to_json());
        v.insert("revision", self.revision.to_string());
        v.insert("cursor", self.cursor.to_string());
        v.insert(
            "snapshot",
            self.snapshot.clone().map_or(Value::Null, Value::String),
        );
        v.insert("polled_at", self.polled_at.to_string());
        v.insert(
            "last_error",
            self.last_error.clone().map_or(Value::Null, Value::String),
        );
        v
    }
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "id",
                "binding",
                "policy",
                "revision",
                "cursor",
                "snapshot",
                "polled_at",
                "last_error",
            ],
        )?;
        let r = Self {
            id: text(v, "id")?,
            binding: text(v, "binding")?,
            policy: Policy::from_json(v.get("policy").ok_or("Requester: missing policy")?)?,
            revision: integer(v, "revision")?,
            cursor: integer(v, "cursor")?,
            snapshot: optional(v, "snapshot")?,
            polled_at: integer(v, "polled_at")?,
            last_error: optional(v, "last_error")?,
        };
        if !valid_id(&r.id)
            || !valid_digest(&r.binding)
            || r.revision == 0
            || r.snapshot.as_ref().is_some_and(|s| !valid_digest(s))
            || r.last_error.as_ref().is_some_and(|s| {
                s != "Plex account poll failed"
                    && s != "Plex account identity changed"
                    && s != "Plex account policy changed during poll"
                    && s != "Plex account poll deadline reached"
            })
        {
            return Err("Requester: invalid persistent account".into());
        }
        Ok(r)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct State {
    pub revision: u64,
    pub accounts: BTreeMap<String, Record>,
    pub demands: BTreeMap<String, Demand>,
    pub operator_jobs: BTreeSet<String>,
    pub notifications: Vec<Value>,
    pub notification_sequence: u64,
    pub delivery: crate::notifications::DeliveryState,
}
impl State {
    pub(crate) fn empty() -> Self {
        Self {
            revision: 0,
            accounts: BTreeMap::new(),
            demands: BTreeMap::new(),
            operator_jobs: BTreeSet::new(),
            notifications: Vec::new(),
            notification_sequence: 0,
            delivery: crate::notifications::DeliveryState::default(),
        }
    }
    pub(crate) fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("revision", self.revision.to_string());
        v.insert(
            "accounts",
            Value::Array(self.accounts.values().map(Record::to_json).collect()),
        );
        v.insert(
            "demands",
            Value::Array(self.demands.values().map(Demand::to_json).collect()),
        );
        v.insert(
            "operator_jobs",
            Value::Array(
                self.operator_jobs
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        v.insert("notifications", Value::Array(self.notifications.clone()));
        if !self.delivery.is_empty() {
            v.insert("delivery", self.delivery.to_json());
        }
        v.insert(
            "notification_sequence",
            self.notification_sequence.to_string(),
        );
        v
    }
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "revision",
                "accounts",
                "demands",
                "operator_jobs",
                "notifications",
                "notification_sequence",
                "delivery",
            ],
        )?;
        let mut s = Self::empty();
        s.revision = integer(v, "revision")?;
        s.notification_sequence = integer(v, "notification_sequence")?;
        s.delivery = crate::notifications::DeliveryState::from_json(v.get("delivery"))?;
        let list = |key| {
            v.get(key)
                .and_then(Value::as_array)
                .ok_or("Requester: missing snapshot collection")
        };
        for v in list("accounts")? {
            let r = Record::from_json(v)?;
            if s.accounts.insert(r.id.clone(), r).is_some() {
                return Err("Requester: duplicate account".into());
            }
        }
        if s.accounts.len() > MAX_ACCOUNTS {
            return Err("Requester: account capacity reached".into());
        }
        for v in list("demands")? {
            let d = Demand::from_json(v)?;
            if !s.accounts.contains_key(&d.account_id)
                || s.accounts[&d.account_id].revision < d.revision
                || s.demands.insert(d.id.clone(), d).is_some()
            {
                return Err("Requester: duplicate or orphaned demand".into());
            }
        }
        if s.demands.len() > MAX_DEMANDS {
            return Err("Requester: demand capacity reached".into());
        }
        for v in list("operator_jobs")? {
            let id = v.as_str().ok_or("Requester: invalid operator interest")?;
            if id.len() != 32
                || !id.bytes().all(|b| b.is_ascii_hexdigit())
                || !s.operator_jobs.insert(id.into())
            {
                return Err("Requester: invalid operator interest".into());
            }
        }
        if s.operator_jobs.len() > MAX_DEMANDS {
            return Err("Requester: operator interest capacity reached".into());
        }
        let mut previous = 0;
        for v in list("notifications")? {
            only(v, &["id", "account_id", "demand_id", "outcome", "at"])?;
            let id = integer(v, "id")?;
            let account = text(v, "account_id")?;
            let demand = text(v, "demand_id")?;
            if id <= previous
                || id > s.notification_sequence
                || !s
                    .demands
                    .get(&demand)
                    .is_some_and(|d| d.account_id == account)
                || !s.accounts.contains_key(&account)
                || ![
                    "pending",
                    "reserved",
                    "active",
                    "ready",
                    "failed",
                    "cancelled",
                    "quota",
                    "conflict",
                    "rejected",
                    "removed",
                ]
                .contains(&text(v, "outcome")?.as_str())
            {
                return Err("Requester: invalid notification outcome".into());
            }
            integer(v, "at")?;
            previous = id;
            s.notifications.push(v.clone());
        }
        if s.notifications.len() > MAX_NOTIFICATIONS {
            return Err("Requester: notification capacity reached".into());
        }
        for r in s.delivery.routes.values() {
            if r.kind != "requester" || !s.accounts.contains_key(&r.scope) {
                return Err("Requester: unbound notification route".into());
            }
        }
        for e in s.delivery.events.values() {
            if e.signal.emission > s.notification_sequence
                || !s
                    .demands
                    .get(&e.signal.subject)
                    .is_some_and(|d| d.account_id == e.signal.scope)
            {
                return Err("Requester: unbound notification event".into());
            }
        }
        Ok(s)
    }
    pub(crate) fn notify(&mut self, id: &str, outcome: &str, now: u64) -> Result<()> {
        let d = self
            .demands
            .get_mut(id)
            .ok_or("Requester: unknown demand")?;
        if d.outcome == outcome {
            return Ok(());
        }
        d.outcome = outcome.into();
        let preference = &self.accounts[&d.account_id].policy.notifications;
        if preference == "none"
            || (preference == "decisions" && matches!(outcome, "reserved" | "active"))
        {
            return Ok(());
        }
        self.notification_sequence = self
            .notification_sequence
            .checked_add(1)
            .ok_or("Requester: notification sequence overflow")?;
        let mut v = Value::object();
        v.insert("id", self.notification_sequence.to_string());
        v.insert("account_id", d.account_id.clone());
        v.insert("demand_id", d.id.clone());
        v.insert("outcome", outcome);
        v.insert("at", now.to_string());
        self.delivery.enqueue(crate::notifications::Signal {
            kind: "requester".into(),
            scope: d.account_id.clone(),
            subject: d.id.clone(),
            emission: self.notification_sequence,
            outcome: outcome.into(),
            at: now,
        })?;
        self.notifications.push(v);
        if self.notifications.len() > MAX_NOTIFICATIONS {
            self.notifications.remove(0);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlRequest {
    pub action: String,
    pub policy: Option<Policy>,
    pub demand_id: Option<String>,
    pub apply: bool,
    pub plan_id: Option<String>,
}
impl ControlRequest {
    pub fn from_json(v: &Value) -> Result<Self> {
        only(v, &["action", "policy", "demand_id", "apply", "plan_id"])?;
        let q = Self {
            action: text(v, "action")?,
            policy: match v.get("policy") {
                None | Some(Value::Null) => None,
                Some(v) => Some(Policy::from_json(v)?),
            },
            demand_id: optional(v, "demand_id")?,
            apply: match v.get("apply") {
                None => false,
                Some(v) => v.as_bool().ok_or("Requester: apply must be a boolean")?,
            },
            plan_id: optional(v, "plan_id")?,
        };
        q.validate()?;
        Ok(q)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if !["policy", "approve", "reject", "remove", "retry"].contains(&self.action.as_str())
            || (self.action == "policy") != self.policy.is_some()
            || (self.action == "policy") != self.demand_id.is_none()
            || self.demand_id.as_ref().is_some_and(|id| !valid_digest(id))
            || self.plan_id.as_ref().is_some_and(|id| !valid_digest(id))
            || self.apply != self.plan_id.is_some()
        {
            return Err("Requester: invalid control scope or guard".into());
        }
        Ok(())
    }
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("action", self.action.clone());
        if let Some(p) = &self.policy {
            v.insert("policy", p.to_json())
        }
        if let Some(id) = &self.demand_id {
            v.insert("demand_id", id.clone())
        }
        v.insert("apply", self.apply);
        if let Some(id) = &self.plan_id {
            v.insert("plan_id", id.clone())
        }
        v
    }
}
