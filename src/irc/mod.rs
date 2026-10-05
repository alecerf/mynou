//! Opt-in IRC claims, audit reviews, and routing to existing approved work.
pub(crate) mod client;
mod engine;
mod persistence;
pub mod protocol;
pub(crate) mod routing;
pub(crate) mod sasl;
use crate::{Result, crypto::sha256, json::Value, selection::SelectionConfig, store::Request};
pub(crate) use persistence::AnnouncementStore;
pub use routing::Origin;
pub use sasl::Settings as Sasl;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_RECORDS: usize = 1000;
pub const MAX_SOURCES: usize = 8;
pub const MAX_RULES: usize = 64;
pub fn valid_id(s: &str) -> bool {
    crate::requesters::valid_id(s)
}
pub fn digest(bytes: &[u8]) -> String {
    sha256(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
fn hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn valid_announcement_id(s: &str) -> bool {
    hash(s)
}

/// Evaluate an explicit claim without storage, IRC traffic or media acquisition.
pub fn preview(config: &crate::config::Config, source_id: &str, value: &Value) -> Result<Value> {
    let source = config.irc.source(source_id)?;
    let a = Announcement::from_json(value)?;
    let mut v = Value::object();
    v.insert("source_id", source.id.clone());
    v.insert("announcement", a.to_json());
    v.insert(
        "evaluations",
        Value::Array(
            evaluate(&config.irc, &config.selection, source_id, &a)?
                .iter()
                .map(Evaluation::to_json)
                .collect(),
        ),
    );
    v.insert("identity_verified", false);
    v.insert("acquisition_started", false);
    v.insert("persisted", false);
    Ok(v)
}
fn only(v: &Value, fields: &[&str]) -> Result<()> {
    if v.as_object()
        .is_none_or(|m| m.keys().any(|k| !fields.contains(&k.as_str())))
    {
        return Err("IRC: unknown field or invalid object".into());
    }
    Ok(())
}
fn text(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("IRC: missing string {key}"))
}
fn boolean(v: &Value, key: &str, default: bool) -> Result<bool> {
    match v.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| format!("IRC: invalid boolean {key}")),
    }
}
fn count(v: &Value, key: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let n = match v.get(key) {
        None => default,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| format!("IRC: invalid integer {key}"))?,
    };
    if !(min..=max).contains(&n) {
        return Err(format!("IRC: {key} exceeds its bounds"));
    }
    Ok(n)
}
fn integer(v: &Value, key: &str) -> Result<u64> {
    let s = text(v, key)?;
    let n = s
        .parse::<u64>()
        .map_err(|_| format!("IRC: invalid decimal {key}"))?;
    if n.to_string() != s {
        return Err(format!("IRC: invalid decimal {key}"));
    }
    Ok(n)
}
fn strings(v: &Value, key: &str) -> Result<Vec<String>> {
    match v.get(key) {
        None => Ok(Vec::new()),
        Some(v) => {
            let a = v
                .as_array()
                .filter(|a| a.len() <= 16)
                .ok_or("IRC: filter terms exceed bounds")?;
            let mut result = Vec::new();
            for item in a {
                let s = item
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
                    .ok_or("IRC: invalid filter term")?
                    .to_ascii_lowercase();
                if result.contains(&s) {
                    return Err("IRC: duplicate filter term".into());
                }
                result.push(s);
            }
            Ok(result)
        }
    }
}
fn profile_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn nickname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.as_bytes()[0].is_ascii_alphabetic()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn channel(s: &str) -> bool {
    s.starts_with('#')
        && (2..=64).contains(&s.len())
        && s[1..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn variable(v: &Value, key: &str) -> Result<Option<String>> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let s = v
                .as_str()
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 128
                        && s.bytes()
                            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                })
                .ok_or("IRC: invalid credential variable name")?;
            Ok(Some(s.into()))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub url: String,
    pub enabled: bool,
    pub nickname: String,
    pub channel: String,
    pub sender: String,
    pub password_env: Option<String>,
    pub join_key_env: Option<String>,
    pub sasl: Option<Sasl>,
    pub magnet_template: Option<String>,
    pub idle_timeout_secs: u64,
    pub reconnect_min_secs: u64,
    pub reconnect_max_secs: u64,
}
impl Source {
    pub(crate) fn endpoint(&self) -> Result<(crate::net::Url, bool)> {
        let (rest, tls) = if let Some(s) = self.url.strip_prefix("ircs://") {
            (s, true)
        } else if let Some(s) = self.url.strip_prefix("irc://") {
            (s, false)
        } else {
            return Err("IRC: endpoint requires ircs:// or loopback irc://".into());
        };
        let url = crate::net::parse_url(&format!("https://{rest}"))?;
        if url.path != "/"
            || !rest
                .trim_end_matches('/')
                .ends_with(&format!(":{}", url.port))
        {
            return Err("IRC: endpoint requires an explicit port and no path".into());
        }
        if !tls
            && !url
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        {
            return Err("IRC: plaintext is limited to explicit loopback IP addresses".into());
        }
        Ok((url, tls))
    }
    fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "id",
                "url",
                "enabled",
                "nickname",
                "channel",
                "sender",
                "password_env",
                "join_key_env",
                "sasl",
                "magnet_template",
                "idle_timeout_secs",
                "reconnect_min_secs",
                "reconnect_max_secs",
            ],
        )?;
        let s = Self {
            id: text(v, "id")?,
            url: text(v, "url")?,
            enabled: boolean(v, "enabled", false)?,
            nickname: v
                .get("nickname")
                .map_or(Ok("mynou".into()), |_| text(v, "nickname"))?,
            channel: text(v, "channel")?,
            sender: text(v, "sender")?,
            password_env: variable(v, "password_env")?,
            join_key_env: variable(v, "join_key_env")?,
            sasl: match v.get("sasl") {
                None | Some(Value::Null) => None,
                Some(v) => Some(Sasl::from_json(v)?),
            },
            magnet_template: match v.get("magnet_template") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_str()
                        .ok_or("IRC: magnet_template must be a string")?
                        .to_owned(),
                ),
            },
            idle_timeout_secs: count(v, "idle_timeout_secs", 180, 30, 600)?,
            reconnect_min_secs: count(v, "reconnect_min_secs", 1, 1, 30)?,
            reconnect_max_secs: count(v, "reconnect_max_secs", 60, 1, 300)?,
        };
        let prefix = s
            .sender
            .split_once('!')
            .and_then(|(nick, rest)| rest.split_once('@').map(|(user, host)| (nick, user, host)));
        if !valid_id(&s.id)
            || !nickname(&s.nickname)
            || !channel(&s.channel)
            || s.sender.len() > 128
            || prefix.is_none_or(|(n, u, h)| {
                !nickname(n)
                    || u.is_empty()
                    || h.is_empty()
                    || !u
                        .bytes()
                        .chain(h.bytes())
                        .all(|b| b.is_ascii_alphanumeric() || b"._-~:/".contains(&b))
            })
            || s.reconnect_max_secs < s.reconnect_min_secs
        {
            return Err("IRC: invalid source identity or connection bounds".into());
        }
        s.endpoint()?;
        if let Some(template) = &s.magnet_template {
            routing::validate_template(template)?;
        }
        Ok(s)
    }
    pub fn binding(&self) -> String {
        let mut v = Value::object();
        for (k, s) in [
            ("id", &self.id),
            ("url", &self.url),
            ("nickname", &self.nickname),
            ("channel", &self.channel),
            ("sender", &self.sender),
        ] {
            v.insert(k, s.clone());
        }
        v.insert(
            "password_env",
            self.password_env.clone().map_or(Value::Null, Value::from),
        );
        v.insert(
            "join_key_env",
            self.join_key_env.clone().map_or(Value::Null, Value::from),
        );
        if let Some(sasl) = &self.sasl {
            v.insert("sasl", sasl.configuration());
        }
        digest(crate::json::stringify(&v).as_bytes())
    }
    pub fn public_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("id", self.id.clone());
        v.insert("enabled", self.enabled);
        v.insert("channel", self.channel.clone());
        v.insert("tls_required", self.url.starts_with("ircs://"));
        v.insert("magnet_configured", self.magnet_template.is_some());
        v.insert(
            "authentication",
            if self.sasl.is_some() {
                "sasl_plain"
            } else {
                "none"
            },
        );
        v
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub id: String,
    pub source: String,
    pub enabled: bool,
    pub kind: String,
    pub profile: String,
    pub action: String,
    pub required_terms: Vec<String>,
    pub blocked_terms: Vec<String>,
}
impl Rule {
    fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "id",
                "source",
                "enabled",
                "kind",
                "profile",
                "required_terms",
                "blocked_terms",
                "action",
            ],
        )?;
        let r = Self {
            id: text(v, "id")?,
            source: text(v, "source")?,
            enabled: boolean(v, "enabled", false)?,
            kind: text(v, "kind")?,
            profile: text(v, "profile")?,
            action: v
                .get("action")
                .map_or(Ok("review".into()), |_| text(v, "action"))?,
            required_terms: strings(v, "required_terms")?,
            blocked_terms: strings(v, "blocked_terms")?,
        };
        if !valid_id(&r.id)
            || !valid_id(&r.source)
            || !matches!(r.kind.as_str(), "movie" | "episode")
            || !profile_name(&r.profile)
            || !matches!(r.action.as_str(), "review" | "grab")
        {
            return Err("IRC: invalid rule identity or profile".into());
        }
        Ok(r)
    }
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        for (k, s) in [
            ("id", &self.id),
            ("source", &self.source),
            ("kind", &self.kind),
            ("profile", &self.profile),
        ] {
            v.insert(k, s.clone());
        }
        v.insert("enabled", self.enabled);
        v.insert("action", self.action.clone());
        for (k, list) in [
            ("required_terms", &self.required_terms),
            ("blocked_terms", &self.blocked_terms),
        ] {
            v.insert(
                k,
                Value::Array(list.iter().cloned().map(Value::from).collect()),
            );
        }
        v
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub sources: Vec<Source>,
    pub rules: Vec<Rule>,
}
impl Settings {
    pub fn from_json(v: &Value, profiles: &SelectionConfig) -> Result<Self> {
        only(v, &["sources", "rules"])?;
        let mut settings = Self::default();
        if let Some(a) = v.get("sources") {
            for v in a
                .as_array()
                .filter(|a| a.len() <= MAX_SOURCES)
                .ok_or("IRC: too many sources")?
            {
                let s = Source::from_json(v)?;
                if settings.sources.iter().any(|p| p.id == s.id) {
                    return Err("IRC: duplicate source ID".into());
                }
                settings.sources.push(s);
            }
        }
        if let Some(a) = v.get("rules") {
            for v in a
                .as_array()
                .filter(|a| a.len() <= MAX_RULES)
                .ok_or("IRC: too many rules")?
            {
                let r = Rule::from_json(v)?;
                if settings.rules.iter().any(|p| p.id == r.id)
                    || !settings.sources.iter().any(|s| s.id == r.source)
                    || !profiles.profiles.contains_key(&r.profile)
                {
                    return Err("IRC: duplicate rule or unknown source/profile".into());
                }
                if r.action == "grab" && settings.source(&r.source)?.magnet_template.is_none() {
                    return Err("IRC: grab rules require a configured magnet template".into());
                }
                settings.rules.push(r);
            }
        }
        settings.sources.sort_by(|a, b| a.id.cmp(&b.id));
        settings.rules.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(settings)
    }
    pub(crate) fn fingerprint(&self, profiles: &SelectionConfig) -> String {
        let mut v = Value::object();
        v.insert(
            "sources",
            Value::Array(
                self.sources
                    .iter()
                    .map(|s| {
                        let mut v = s.public_json();
                        v.insert("binding", s.binding());
                        v.insert(
                            "magnet_template_digest",
                            s.magnet_template
                                .as_ref()
                                .map_or(Value::Null, |t| Value::from(digest(t.as_bytes()))),
                        );
                        v.insert("idle_timeout_secs", s.idle_timeout_secs.to_string());
                        v.insert("reconnect_min_secs", s.reconnect_min_secs.to_string());
                        v.insert("reconnect_max_secs", s.reconnect_max_secs.to_string());
                        v
                    })
                    .collect(),
            ),
        );
        v.insert(
            "rules",
            Value::Array(self.rules.iter().map(Rule::to_json).collect()),
        );
        v.insert("profiles", profiles.to_json());
        digest(crate::json::stringify(&v).as_bytes())
    }
    pub(crate) fn source(&self, id: &str) -> Result<&Source> {
        self.sources
            .iter()
            .find(|s| s.id == id)
            .ok_or("IRC: unknown configured source".into())
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Announcement {
    pub title: String,
    pub request: Request,
    pub info_hash: String,
}
impl Announcement {
    pub fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "title",
                "kind",
                "media_title",
                "year",
                "season",
                "episode",
                "tmdb_id",
                "info_hash",
            ],
        )?;
        let a = Self {
            title: text(v, "title")?,
            info_hash: text(v, "info_hash")?.to_ascii_lowercase(),
            request: Request {
                kind: text(v, "kind")?,
                title: text(v, "media_title")?,
                year: count(v, "year", 0, 0, 9999)? as u32,
                season: count(v, "season", 0, 0, 9999)? as u32,
                episode: count(v, "episode", 0, 0, 9999)? as u32,
                tmdb_id: Some(integer(v, "tmdb_id")?),
                source_path: None,
                source_url: None,
                source_numbering: None,
            },
        };
        a.request.validate()?;
        if !matches!(a.request.kind.as_str(), "movie" | "episode")
            || a.request.tmdb_id == Some(0)
            || a.request.title.len() > 512
            || a.request.title.chars().any(char::is_control)
            || (a.request.kind == "movie" && (a.request.season != 0 || a.request.episode != 0))
            || (a.request.kind == "episode" && a.request.episode == 0)
            || a.title.is_empty()
            || a.title.len() > 2048
            || a.title.chars().any(char::is_control)
            || ![40, 64].contains(&a.info_hash.len())
            || !a.info_hash.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("IRC: invalid release identity claim".into());
        }
        Ok(a)
    }
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        for (k, s) in [
            ("title", &self.title),
            ("kind", &self.request.kind),
            ("media_title", &self.request.title),
            ("info_hash", &self.info_hash),
        ] {
            v.insert(k, s.clone());
        }
        v.insert("tmdb_id", self.request.tmdb_id.unwrap_or(0).to_string());
        v.insert("year", self.request.year);
        v.insert("season", self.request.season);
        v.insert("episode", self.request.episode);
        v
    }
    fn id(&self, binding: &str) -> String {
        digest(
            format!(
                "{binding}\n{}\n{}",
                self.request.media_key(),
                self.info_hash
            )
            .as_bytes(),
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluation {
    pub rule_id: String,
    pub profile: String,
    pub outcome: String,
}
impl Evaluation {
    fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("rule_id", self.rule_id.clone());
        v.insert("profile", self.profile.clone());
        v.insert("outcome", self.outcome.clone());
        v
    }
    fn from_json(v: &Value) -> Result<Self> {
        only(v, &["rule_id", "profile", "outcome"])?;
        let e = Self {
            rule_id: text(v, "rule_id")?,
            profile: text(v, "profile")?,
            outcome: text(v, "outcome")?,
        };
        if !valid_id(&e.rule_id)
            || !profile_name(&e.profile)
            || !matches!(
                e.outcome.as_str(),
                "matched"
                    | "disabled"
                    | "kind_mismatch"
                    | "blocked"
                    | "required_missing"
                    | "profile_rejected"
            )
        {
            return Err("IRC: invalid rule outcome".into());
        }
        Ok(e)
    }
}
pub(crate) fn evaluate(
    settings: &Settings,
    profiles: &SelectionConfig,
    source: &str,
    a: &Announcement,
) -> Result<Vec<Evaluation>> {
    let title = a.title.to_ascii_lowercase();
    settings
        .rules
        .iter()
        .filter(|r| r.source == source)
        .map(|r| {
            let profile = profiles
                .profiles
                .get(&r.profile)
                .ok_or("IRC: missing selection profile")?;
            let outcome = if !r.enabled {
                "disabled"
            } else if r.kind != a.request.kind {
                "kind_mismatch"
            } else if r.blocked_terms.iter().any(|t| title.contains(t)) {
                "blocked"
            } else if r.required_terms.iter().any(|t| !title.contains(t)) {
                "required_missing"
            } else if !profile.assess(&a.title, &a.request.title).accepted {
                "profile_rejected"
            } else {
                "matched"
            };
            Ok(Evaluation {
                rule_id: r.id.clone(),
                profile: r.profile.clone(),
                outcome: outcome.into(),
            })
        })
        .collect()
}
fn outcome(evaluations: &[Evaluation]) -> &'static str {
    match evaluations
        .iter()
        .filter(|e| e.outcome == "matched")
        .count()
    {
        0 => "unmatched",
        1 => "matched",
        _ => "conflict",
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Record {
    pub id: String,
    pub source_id: String,
    pub binding: String,
    pub announcement: Announcement,
    pub evaluations: Vec<Evaluation>,
    pub fingerprint: String,
    pub decision: String,
    pub revision: u64,
    pub first_seen: u64,
    pub decided_at: Option<u64>,
    pub route: Option<routing::Route>,
}
impl Record {
    pub(crate) fn public_json(&self) -> Value {
        let mut v = self.to_json();
        if let Value::Object(m) = &mut v {
            m.remove("binding");
            m.remove("fingerprint");
        }
        v.insert(
            "route",
            self.route
                .as_ref()
                .map_or(Value::Null, routing::Route::public_json),
        );
        v.insert("outcome", outcome(&self.evaluations));
        v.insert("identity_verified", false);
        v.insert(
            "candidate_routed",
            self.route.as_ref().is_some_and(|r| r.phase == "routed"),
        );
        v
    }
    fn to_json(&self) -> Value {
        let mut v = Value::object();
        for (k, s) in [
            ("id", &self.id),
            ("source_id", &self.source_id),
            ("binding", &self.binding),
            ("fingerprint", &self.fingerprint),
            ("decision", &self.decision),
        ] {
            v.insert(k, s.clone());
        }
        v.insert("announcement", self.announcement.to_json());
        v.insert(
            "route",
            self.route
                .as_ref()
                .map_or(Value::Null, routing::Route::to_json),
        );
        v.insert(
            "evaluations",
            Value::Array(self.evaluations.iter().map(Evaluation::to_json).collect()),
        );
        for (k, n) in [("revision", self.revision), ("first_seen", self.first_seen)] {
            v.insert(k, n.to_string());
        }
        v.insert(
            "decided_at",
            self.decided_at
                .map_or(Value::Null, |n| Value::from(n.to_string())),
        );
        v
    }
    fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "id",
                "source_id",
                "binding",
                "fingerprint",
                "decision",
                "announcement",
                "evaluations",
                "revision",
                "first_seen",
                "decided_at",
                "route",
            ],
        )?;
        let r = Self {
            id: text(v, "id")?,
            source_id: text(v, "source_id")?,
            binding: text(v, "binding")?,
            fingerprint: text(v, "fingerprint")?,
            decision: text(v, "decision")?,
            announcement: Announcement::from_json(
                v.get("announcement").ok_or("IRC: missing announcement")?,
            )?,
            evaluations: v
                .get("evaluations")
                .and_then(Value::as_array)
                .filter(|a| a.len() <= MAX_RULES)
                .ok_or("IRC: invalid outcomes")?
                .iter()
                .map(Evaluation::from_json)
                .collect::<Result<_>>()?,
            revision: integer(v, "revision")?,
            first_seen: integer(v, "first_seen")?,
            decided_at: match v.get("decided_at") {
                Some(Value::Null) => None,
                Some(_) => Some(integer(v, "decided_at")?),
                None => return Err("IRC: missing decision timestamp".into()),
            },
            route: match v.get("route") {
                None | Some(Value::Null) => None,
                Some(v) => Some(routing::Route::from_json(v)?),
            },
        };
        let unique: BTreeSet<_> = r.evaluations.iter().map(|e| &e.rule_id).collect();
        if !hash(&r.binding)
            || !hash(&r.fingerprint)
            || r.id != r.announcement.id(&r.binding)
            || !valid_id(&r.source_id)
            || r.revision == 0
            || unique.len() != r.evaluations.len()
            || !matches!(
                r.decision.as_str(),
                "pending" | "acknowledged" | "dismissed"
            )
            || (r.decision == "pending") != r.decided_at.is_none()
            || r.decided_at.is_some_and(|t| t < r.first_seen)
        {
            return Err("IRC: inconsistent announcement provenance".into());
        }
        if let Some(route) = &r.route {
            route.origin.validate_record(&r)?;
            if route.reserved_at < r.first_seen
                || r.revision < 2
                || (r.decision != "pending" && r.revision < 3)
            {
                return Err("IRC: inconsistent reservation revision or timestamp".into());
            }
        }
        Ok(r)
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct State {
    pub revision: u64,
    pub bindings: BTreeMap<String, String>,
    pub records: BTreeMap<String, Record>,
}
impl State {
    pub(crate) fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("revision", self.revision.to_string());
        v.insert(
            "bindings",
            Value::Object(
                self.bindings
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(v.clone())))
                    .collect(),
            ),
        );
        v.insert(
            "records",
            Value::Array(self.records.values().map(Record::to_json).collect()),
        );
        v
    }
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        only(v, &["revision", "bindings", "records"])?;
        let mut s = Self {
            revision: integer(v, "revision")?,
            ..Self::default()
        };
        for (k, v) in v
            .get("bindings")
            .and_then(Value::as_object)
            .filter(|a| a.len() <= 32)
            .ok_or("IRC: invalid retained sources")?
        {
            let b = v
                .as_str()
                .filter(|v| hash(v))
                .ok_or("IRC: invalid source binding")?;
            if !valid_id(k) {
                return Err("IRC: invalid retained source ID".into());
            }
            s.bindings.insert(k.clone(), b.into());
        }
        for v in v
            .get("records")
            .and_then(Value::as_array)
            .filter(|a| a.len() <= MAX_RECORDS)
            .ok_or("IRC: announcement history is full")?
        {
            let r = Record::from_json(v)?;
            if s.bindings.get(&r.source_id) != Some(&r.binding)
                || s.records.insert(r.id.clone(), r).is_some()
            {
                return Err("IRC: duplicate or unbound announcement".into());
            }
        }
        Ok(s)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlRequest {
    pub action: String,
    pub apply: bool,
    pub plan_id: Option<String>,
}
impl ControlRequest {
    pub fn from_json(v: &Value) -> Result<Self> {
        only(v, &["action", "apply", "plan_id"])?;
        let q = Self {
            action: text(v, "action")?,
            apply: boolean(v, "apply", false)?,
            plan_id: match v.get("plan_id") {
                None | Some(Value::Null) => None,
                Some(_) => Some(text(v, "plan_id")?),
            },
        };
        q.validate()?;
        Ok(q)
    }
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.action.as_str(), "acknowledge" | "dismiss")
            || self.apply != self.plan_id.is_some()
            || self.plan_id.as_ref().is_some_and(|s| !hash(s))
        {
            return Err("IRC: invalid review action or guard".into());
        }
        Ok(())
    }
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("action", self.action.clone());
        v.insert("apply", self.apply);
        v.insert(
            "plan_id",
            self.plan_id.clone().map_or(Value::Null, Value::from),
        );
        v
    }
}
pub fn page(query: &str) -> Result<(usize, usize)> {
    let p = crate::requesters::page(query)?;
    if p.0 > MAX_RECORDS {
        return Err("IRC: page offset exceeds history bounds".into());
    }
    Ok(p)
}
