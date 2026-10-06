//! Original bounded, endpoint-bound notification events. Delivery is at-least-once.
pub(crate) mod delivery;
use crate::{
    Result,
    json::{self, Value},
    requesters::{digest, valid_digest, valid_id},
};
pub use delivery::ControlRequest;
use std::collections::BTreeMap;

pub const MAX_ROUTES: usize = 32;
pub const MAX_EVENTS: usize = 1024;
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub routes: Vec<Route>,
}
#[derive(Clone, Debug)]
pub struct Route {
    pub id: String,
    pub enabled: bool,
    pub kind: String,
    pub scope: String,
    pub url: String,
    pub token_env: Option<String>,
    pub max_attempts: u32,
}
fn text(v: &Value, k: &str) -> Result<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Notifications: invalid {k}"))
}
fn number(v: &Value, k: &str) -> Result<u64> {
    v.get(k)
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .ok_or_else(|| format!("Notifications: invalid {k}"))
}
fn only(v: &Value, keys: &[&str]) -> Result<()> {
    crate::numbering::only(v, keys)
}
fn boolean(v: &Value, k: &str, default: bool) -> Result<bool> {
    match v.get(k) {
        None => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        _ => Err(format!("Notifications: invalid {k}")),
    }
}
impl Route {
    pub(crate) fn binding(&self) -> String {
        digest(
            json::stringify(&Value::Array(vec![
                self.id.clone().into(),
                self.kind.clone().into(),
                self.scope.clone().into(),
                self.url.clone().into(),
                self.token_env.clone().map_or(Value::Null, Value::from),
                self.max_attempts.into(),
            ]))
            .as_bytes(),
        )
    }
}
impl Settings {
    pub(crate) fn from_json(
        v: Option<&Value>,
        requesters: &crate::requesters::Settings,
        irc: &crate::irc::Settings,
    ) -> Result<Self> {
        let Some(v) = v else {
            return Ok(Self::default());
        };
        only(v, &["routes"])?;
        let rows = v
            .get("routes")
            .and_then(Value::as_array)
            .filter(|v| v.len() <= MAX_ROUTES)
            .ok_or("Notifications: invalid route capacity")?;
        let mut routes = Vec::new();
        for v in rows {
            only(
                v,
                &[
                    "id",
                    "enabled",
                    "account_id",
                    "source_id",
                    "url",
                    "token_env",
                    "max_attempts",
                ],
            )?;
            let id = text(v, "id")?;
            if !valid_id(&id) || routes.iter().any(|r: &Route| r.id == id) {
                return Err("Notifications: invalid or duplicate route ID".into());
            }
            let (kind, scope) = match (v.get("account_id"), v.get("source_id")) {
                (Some(Value::String(a)), None)
                    if requesters.accounts.iter().any(|r| r.id == *a) =>
                {
                    ("requester", a.clone())
                }
                (None, Some(Value::String(s))) if irc.sources.iter().any(|r| r.id == *s) => {
                    ("irc", s.clone())
                }
                _ => {
                    return Err(
                        "Notifications: select exactly one configured account or IRC source".into(),
                    );
                }
            };
            let url = text(v, "url")?;
            let parsed =
                crate::net::parse_url(&url).map_err(|_| "Notifications: invalid endpoint")?;
            if url.contains(['?', '#'])
                || (parsed.scheme != "https"
                    && !parsed
                        .host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback()))
            {
                return Err("Notifications: endpoints require HTTPS or literal loopback, without queries or fragments".into());
            }
            let token_env = match v.get("token_env") {
                None | Some(Value::Null) => None,
                Some(Value::String(s))
                    if !s.is_empty()
                        && s.len() <= 128
                        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
                {
                    Some(s.clone())
                }
                _ => return Err("Notifications: invalid credential environment name".into()),
            };
            let max_attempts = match v.get("max_attempts") {
                None => 4,
                Some(_) => number(v, "max_attempts")?,
            };
            if !(1..=8).contains(&max_attempts) {
                return Err("Notifications: attempt limit must be between 1 and 8".into());
            }
            routes.push(Route {
                id,
                enabled: boolean(v, "enabled", false)?,
                kind: kind.into(),
                scope,
                url,
                token_env,
                max_attempts: max_attempts as u32,
            });
        }
        Ok(Self { routes })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    pub kind: String,
    pub scope: String,
    pub digest: String,
    pub enabled: bool,
    pub max_attempts: u32,
}
impl Binding {
    fn to_json(&self, id: &str) -> Value {
        let mut v = Value::object();
        v.insert("id", id);
        v.insert("kind", self.kind.clone());
        v.insert("scope", self.scope.clone());
        v.insert("binding", self.digest.clone());
        v.insert("enabled", self.enabled);
        v.insert("max_attempts", self.max_attempts);
        v
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Signal {
    pub kind: String,
    pub scope: String,
    pub subject: String,
    pub emission: u64,
    pub outcome: String,
    pub at: u64,
}
impl Signal {
    fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("schema", 1_u32);
        v.insert("kind", self.kind.clone());
        v.insert("scope_id", self.scope.clone());
        v.insert("subject_id", self.subject.clone());
        v.insert("emission", self.emission.to_string());
        v.insert("outcome", self.outcome.clone());
        v.insert("at", self.at.to_string());
        v
    }
    fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "schema",
                "kind",
                "scope_id",
                "subject_id",
                "emission",
                "outcome",
                "at",
            ],
        )?;
        let s = Self {
            kind: text(v, "kind")?,
            scope: text(v, "scope_id")?,
            subject: text(v, "subject_id")?,
            emission: number(v, "emission")?,
            outcome: text(v, "outcome")?,
            at: number(v, "at")?,
        };
        let allowed = match s.kind.as_str() {
            "requester" => &[
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
            ][..],
            "irc" => &[
                "pending",
                "acknowledged",
                "dismissed",
                "requested",
                "reserved",
                "routed",
                "aborted",
                "route_aborted",
            ][..],
            _ => return Err("Notifications: invalid event kind".into()),
        };
        if number(v, "schema")? != 1
            || !valid_id(&s.scope)
            || !valid_digest(&s.subject)
            || s.emission == 0
            || !allowed.contains(&s.outcome.as_str())
        {
            return Err("Notifications: invalid outcome event".into());
        }
        Ok(s)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Event {
    pub id: String,
    pub sequence: u64,
    pub route_id: String,
    pub signal: Signal,
    pub phase: String,
    pub attempts: u32,
    pub next_at: u64,
    pub last_error: Option<String>,
}
impl Event {
    pub(crate) fn payload(&self) -> Value {
        let mut v = self.signal.to_json();
        v.insert("id", self.id.clone());
        v.insert("route_id", self.route_id.clone());
        v
    }
    pub(crate) fn public_json(&self) -> Value {
        let mut v = self.payload();
        v.insert("phase", self.phase.clone());
        v.insert("attempts", self.attempts);
        v.insert("next_at", self.next_at.to_string());
        v.insert(
            "last_error",
            self.last_error.clone().map_or(Value::Null, Value::from),
        );
        v
    }
    fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("id", self.id.clone());
        v.insert("sequence", self.sequence.to_string());
        v.insert("route_id", self.route_id.clone());
        v.insert("signal", self.signal.to_json());
        v.insert("phase", self.phase.clone());
        v.insert("attempts", self.attempts);
        v.insert("next_at", self.next_at.to_string());
        v.insert(
            "last_error",
            self.last_error.clone().map_or(Value::Null, Value::from),
        );
        v
    }
    fn identity(route: &Binding, signal: &Signal) -> String {
        digest(format!("{}:{}", route.digest, json::stringify(&signal.to_json())).as_bytes())
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DeliveryState {
    pub routes: BTreeMap<String, Binding>,
    pub events: BTreeMap<String, Event>,
    pub sequence: u64,
}
impl DeliveryState {
    pub(crate) fn is_empty(&self) -> bool {
        self.routes.is_empty() && self.events.is_empty() && self.sequence == 0
    }
    pub(crate) fn configure(&mut self, settings: &Settings, kind: &str) -> Result<()> {
        for b in self.routes.values_mut() {
            b.enabled = false
        }
        for r in settings.routes.iter().filter(|r| r.kind == kind) {
            let binding = Binding {
                kind: r.kind.clone(),
                scope: r.scope.clone(),
                digest: r.binding(),
                enabled: r.enabled,
                max_attempts: r.max_attempts,
            };
            if let Some(old) = self.routes.get(&r.id) {
                if old.digest != binding.digest {
                    return Err(
                        "Notifications: retained route binding changed; use a new stable ID".into(),
                    );
                }
            }
            self.routes.insert(r.id.clone(), binding);
        }
        if self.routes.len() > MAX_ROUTES {
            return Err("Notifications: retained route capacity reached".into());
        }
        Ok(())
    }
    pub(crate) fn enqueue(&mut self, signal: Signal) -> Result<()> {
        Signal::from_json(&signal.to_json())?;
        let routes: Vec<_> = self
            .routes
            .iter()
            .filter(|(_, r)| r.enabled && r.kind == signal.kind && r.scope == signal.scope)
            .map(|(id, r)| (id.clone(), r.clone()))
            .collect();
        for (route_id, r) in routes {
            let id = Event::identity(&r, &signal);
            if self.events.contains_key(&id) {
                continue;
            }
            if self.events.len() == MAX_EVENTS {
                let oldest = self
                    .events
                    .values()
                    .filter(|e| matches!(e.phase.as_str(), "delivered" | "failed" | "discarded"))
                    .min_by_key(|e| e.sequence)
                    .map(|e| e.id.clone())
                    .ok_or("Notifications: live event capacity reached")?;
                self.events.remove(&oldest);
            }
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or("Notifications: sequence overflow")?;
            self.events.insert(
                id.clone(),
                Event {
                    id,
                    sequence: self.sequence,
                    route_id,
                    signal: signal.clone(),
                    phase: "pending".into(),
                    attempts: 0,
                    next_at: signal.at,
                    last_error: None,
                },
            );
        }
        Ok(())
    }
    pub(crate) fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("sequence", self.sequence.to_string());
        v.insert(
            "routes",
            Value::Array(self.routes.iter().map(|(id, r)| r.to_json(id)).collect()),
        );
        v.insert(
            "events",
            Value::Array(self.events.values().map(Event::to_json).collect()),
        );
        v
    }
    pub(crate) fn from_json(v: Option<&Value>) -> Result<Self> {
        let Some(v) = v else {
            return Ok(Self::default());
        };
        only(v, &["sequence", "routes", "events"])?;
        let mut state = Self {
            sequence: number(v, "sequence")?,
            ..Self::default()
        };
        let rows = |k| {
            v.get(k)
                .and_then(Value::as_array)
                .ok_or("Notifications: invalid collection")
        };
        for v in rows("routes")? {
            only(
                v,
                &["id", "kind", "scope", "binding", "enabled", "max_attempts"],
            )?;
            let id = text(v, "id")?;
            let max = number(v, "max_attempts")?;
            let b = Binding {
                kind: text(v, "kind")?,
                scope: text(v, "scope")?,
                digest: text(v, "binding")?,
                enabled: boolean(v, "enabled", false)?,
                max_attempts: max as u32,
            };
            if !valid_id(&id)
                || !valid_id(&b.scope)
                || !valid_digest(&b.digest)
                || !matches!(b.kind.as_str(), "requester" | "irc")
                || !(1..=8).contains(&max)
                || state.routes.insert(id, b).is_some()
            {
                return Err("Notifications: invalid retained route".into());
            }
        }
        let mut sequences = std::collections::BTreeSet::new();
        for v in rows("events")? {
            only(
                v,
                &[
                    "id",
                    "sequence",
                    "route_id",
                    "signal",
                    "phase",
                    "attempts",
                    "next_at",
                    "last_error",
                ],
            )?;
            let attempts = number(v, "attempts")?;
            let e = Event {
                id: text(v, "id")?,
                sequence: number(v, "sequence")?,
                route_id: text(v, "route_id")?,
                signal: Signal::from_json(v.get("signal").ok_or("Notifications: missing event")?)?,
                phase: text(v, "phase")?,
                attempts: attempts as u32,
                next_at: number(v, "next_at")?,
                last_error: match v.get("last_error") {
                    Some(Value::Null) => None,
                    Some(Value::String(s))
                        if s == "Delivery failed" || s == "Attempt limit reached" =>
                    {
                        Some(s.clone())
                    }
                    _ => return Err("Notifications: invalid delivery error".into()),
                },
            };
            let b = state
                .routes
                .get(&e.route_id)
                .ok_or("Notifications: orphaned event")?;
            if e.id != Event::identity(b, &e.signal)
                || e.signal.kind != b.kind
                || e.signal.scope != b.scope
                || e.sequence == 0
                || e.sequence > state.sequence
                || !sequences.insert(e.sequence)
                || attempts > u64::from(b.max_attempts)
                || !matches!(
                    e.phase.as_str(),
                    "pending" | "sending" | "delivered" | "failed" | "discarded"
                )
                || (e.phase == "sending" && attempts == 0)
                || (e.phase == "delivered" && (attempts == 0 || e.last_error.is_some()))
                || e.next_at < e.signal.at
                || state.events.insert(e.id.clone(), e).is_some()
            {
                return Err("Notifications: inconsistent event provenance".into());
            }
        }
        if state.routes.len() > MAX_ROUTES || state.events.len() > MAX_EVENTS {
            return Err("Notifications: retained capacity reached".into());
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> DeliveryState {
        let mut s = DeliveryState::default();
        let r = Route {
            id: "local".into(),
            enabled: true,
            kind: "irc".into(),
            scope: "announce".into(),
            url: "http://127.0.0.1:1/notify".into(),
            token_env: None,
            max_attempts: 2,
        };
        s.configure(&Settings { routes: vec![r] }, "irc").unwrap();
        s
    }
    fn signal(emission: u64) -> Signal {
        Signal {
            kind: "irc".into(),
            scope: "announce".into(),
            subject: "a".repeat(64),
            emission,
            outcome: "pending".into(),
            at: 1,
        }
    }
    #[test]
    fn events_deduplicate_exact_emissions_and_validate_checked_roundtrips() {
        let mut s = state();
        s.enqueue(signal(1)).unwrap();
        let original = s.clone();
        s.enqueue(signal(1)).unwrap();
        assert_eq!(s, original);
        s.enqueue(signal(2)).unwrap();
        assert_eq!(s.events.len(), 2);
        assert_eq!(DeliveryState::from_json(Some(&s.to_json())).unwrap(), s);
    }
    #[test]
    fn live_capacity_is_never_evicted_and_only_oldest_terminal_event_is_pruned() {
        let mut s = state();
        for n in 1..=MAX_EVENTS as u64 {
            s.enqueue(signal(n)).unwrap();
        }
        let before = s.clone();
        assert!(s.enqueue(signal(2000)).is_err());
        assert_eq!(s, before);
        let first = s.events.values_mut().find(|e| e.sequence == 1).unwrap();
        let id = first.id.clone();
        first.phase = "delivered".into();
        first.attempts = 1;
        s.enqueue(signal(2000)).unwrap();
        assert_eq!(s.events.len(), MAX_EVENTS);
        assert!(!s.events.contains_key(&id));
        assert_eq!(DeliveryState::from_json(Some(&s.to_json())).unwrap(), s);
    }
    #[test]
    fn route_rebinding_and_forged_outcome_payloads_fail_closed() {
        let mut s = state();
        s.enqueue(signal(1)).unwrap();
        let mut v = s.to_json();
        let Value::Array(es) = v.get_mut("events").unwrap() else {
            panic!()
        };
        es[0].get_mut("signal").unwrap().insert("outcome", "routed");
        assert!(DeliveryState::from_json(Some(&v)).is_err());
        let mut r = Route {
            id: "local".into(),
            enabled: true,
            kind: "irc".into(),
            scope: "announce".into(),
            url: "http://127.0.0.1:1/notify".into(),
            token_env: None,
            max_attempts: 2,
        };
        r.url.push_str("/new");
        assert!(s.configure(&Settings { routes: vec![r] }, "irc").is_err());
    }
}
