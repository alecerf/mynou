//! Checked leases precede endpoint I/O. Acknowledgment does not affect acquisition.
use super::{DeliveryState, Event, boolean, digest, number, only, text};
use crate::{
    Result,
    engine::{Engine, lock},
    json::{self, Value},
    net::HttpClient,
    store,
};
use std::{
    sync::{Arc, atomic::Ordering},
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct ControlRequest {
    pub kind: String,
    pub event_id: String,
    pub action: String,
    pub apply: bool,
    pub plan_id: Option<String>,
}
impl ControlRequest {
    pub fn from_json(v: &Value) -> Result<Self> {
        only(v, &["kind", "event_id", "action", "apply", "plan_id"])?;
        let q = Self {
            kind: text(v, "kind")?,
            event_id: text(v, "event_id")?,
            action: text(v, "action")?,
            apply: boolean(v, "apply", false)?,
            plan_id: match v.get("plan_id") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                _ => return Err("Notifications: invalid review guard".into()),
            },
        };
        q.validate()?;
        Ok(q)
    }
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.kind.as_str(), "requester" | "irc")
            || !crate::requesters::valid_digest(&self.event_id)
            || !matches!(self.action.as_str(), "retry" | "discard")
            || self
                .plan_id
                .as_ref()
                .is_some_and(|s| !crate::requesters::valid_digest(s))
            || (self.apply && self.plan_id.is_none())
        {
            return Err("Notifications: invalid guarded control".into());
        }
        Ok(())
    }
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("kind", self.kind.clone());
        v.insert("event_id", self.event_id.clone());
        v.insert("action", self.action.clone());
        v.insert("apply", self.apply);
        v.insert(
            "plan_id",
            self.plan_id.clone().map_or(Value::Null, Value::from),
        );
        v
    }
}
impl Engine {
    fn delivery_change<T>(
        &self,
        kind: &str,
        change: impl FnOnce(&mut DeliveryState) -> Result<T>,
    ) -> Result<T> {
        if self.read_only || self.stopped.load(Ordering::Acquire) {
            return Err(
                "Notifications: delivery changes require a running writable service".into(),
            );
        }
        if kind == "irc" {
            let mut store = lock(&self.irc_store)?;
            let mut next = store.state.clone();
            let r = change(&mut next.delivery)?;
            if next != store.state {
                store.save(next)?
            }
            Ok(r)
        } else {
            let mut store = lock(&self.requester_store)?;
            let mut next = store.state.clone();
            let r = change(&mut next.delivery)?;
            if next != store.state {
                store.save(next)?
            }
            Ok(r)
        }
    }
    fn delivery_snapshot(&self, kind: &str) -> Result<DeliveryState> {
        if kind == "irc" {
            Ok(lock(&self.irc_store)?.state.delivery.clone())
        } else {
            Ok(lock(&self.requester_store)?.state.delivery.clone())
        }
    }
    pub fn notifications(&self, offset: usize, limit: usize) -> Result<Value> {
        if offset > 2 * super::MAX_EVENTS || limit == 0 || limit > 200 {
            return Err("Notifications: page bounds exceeded".into());
        }
        let mut events = Vec::new();
        let mut routes = Vec::new();
        for kind in ["requester", "irc"] {
            let state = self.delivery_snapshot(kind)?;
            for (id, b) in state.routes {
                let mut v = Value::object();
                v.insert("id", id);
                v.insert("kind", b.kind);
                v.insert("scope_id", b.scope);
                v.insert("enabled", b.enabled);
                v.insert("max_attempts", b.max_attempts);
                routes.push(v);
            }
            for e in state.events.into_values() {
                events.push(e.public_json());
            }
        }
        events.sort_by(|a, b| {
            number(b, "at")
                .unwrap_or(0)
                .cmp(&number(a, "at").unwrap_or(0))
                .then_with(|| {
                    text(a, "id")
                        .unwrap_or_default()
                        .cmp(&text(b, "id").unwrap_or_default())
                })
        });
        let mut v = Value::object();
        v.insert("routes", Value::Array(routes));
        v.insert("count", events.len() as u32);
        v.insert("offset", offset as u32);
        v.insert("limit", limit as u32);
        v.insert(
            "events",
            Value::Array(events.into_iter().skip(offset).take(limit).collect()),
        );
        v.insert("delivery_semantics", "at_least_once");
        Ok(v)
    }
    pub fn notification_control(&self, q: &ControlRequest) -> Result<Value> {
        q.validate()?;
        let _dispatch = lock(&self.notification_lock)?;
        let state = self.delivery_snapshot(&q.kind)?;
        let e = state
            .events
            .get(&q.event_id)
            .ok_or("Notifications: unknown event")?;
        let route = &state.routes[&e.route_id];
        if matches!(e.phase.as_str(), "delivered" | "discarded" | "sending")
            || (q.action == "retry" && e.attempts >= route.max_attempts)
        {
            return Err("Notifications: terminal or in-flight event cannot be changed".into());
        }
        let mut scope = e.to_json();
        scope.insert("route", route.to_json(&e.route_id));
        scope.insert("action", q.action.clone());
        scope.insert("kind", q.kind.clone());
        let plan = digest(json::stringify(&scope).as_bytes());
        let mut report = Value::object();
        report.insert("event", e.public_json());
        report.insert("action", q.action.clone());
        report.insert("kind", q.kind.clone());
        report.insert("plan_id", plan.clone());
        report.insert("applied", false);
        if q.apply {
            if q.plan_id.as_deref() != Some(&plan) {
                return Err("Notifications: review is stale; preview again".into());
            }
            self.delivery_change(&q.kind, |s| {
                let e = s
                    .events
                    .get_mut(&q.event_id)
                    .ok_or("Notifications: unknown event")?;
                e.phase = if q.action == "retry" {
                    "pending"
                } else {
                    "discarded"
                }
                .into();
                e.next_at = store::now().max(e.signal.at);
                e.last_error = None;
                Ok(())
            })?;
            report.insert("applied", true);
        }
        Ok(report)
    }
    pub fn dispatch_notifications(&self) -> Result<Value> {
        let _dispatch = lock(&self.notification_lock)?;
        if self.read_only || self.stopped.load(Ordering::Acquire) {
            return Err("Notifications: dispatch requires a running writable service".into());
        }
        let mut sent = 0_u32;
        let mut failed = 0_u32;
        for pass in 0..8 {
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            let mut selected = None;
            for kind in if pass % 2 == 0 {
                ["requester", "irc"]
            } else {
                ["irc", "requester"]
            } {
                let state = self.delivery_snapshot(kind)?;
                selected = state
                    .events
                    .values()
                    .filter(|e| {
                        matches!(e.phase.as_str(), "pending" | "sending")
                            && e.next_at <= store::now()
                    })
                    .filter_map(|e| {
                        self.config
                            .notifications
                            .routes
                            .iter()
                            .find(|r| {
                                r.enabled
                                    && r.id == e.route_id
                                    && r.kind == kind
                                    && r.binding() == state.routes[&e.route_id].digest
                            })
                            .map(|r| (kind, e.clone(), r.clone()))
                    })
                    .min_by_key(|(_, e, _)| e.sequence);
                if selected.is_some() {
                    break;
                }
            }
            let Some((kind, event, route)) = selected else {
                break;
            };
            let lease = self.delivery_change(kind, |s| {
                let e = s
                    .events
                    .get_mut(&event.id)
                    .ok_or("Notifications: unknown event")?;
                if e.attempts >= route.max_attempts {
                    e.phase = "failed".into();
                    e.last_error = Some("Attempt limit reached".into());
                    return Ok(None);
                }
                e.attempts += 1;
                e.phase = "sending".into();
                e.next_at = store::now().max(e.signal.at).saturating_add(15);
                Ok(Some(e.clone()))
            })?;
            let Some(lease) = lease else {
                failed += 1;
                continue;
            };
            let success = send(&route, &lease).is_ok();
            // Finish an acquired lease even if shutdown was requested during I/O.
            let finish = |s: &mut DeliveryState| -> Result<()> {
                let e = s
                    .events
                    .get_mut(&lease.id)
                    .ok_or("Notifications: missing leased event")?;
                if e.phase != "sending" || e.attempts != lease.attempts {
                    return Err("Notifications: delivery lease changed".into());
                }
                e.phase = if success {
                    "delivered"
                } else if e.attempts >= route.max_attempts {
                    "failed"
                } else {
                    "pending"
                }
                .into();
                e.last_error = (!success).then(|| "Delivery failed".into());
                e.next_at = store::now().max(e.signal.at).saturating_add(if success {
                    0
                } else {
                    1_u64 << (e.attempts - 1)
                });
                Ok(())
            };
            // Saving the acknowledgment is allowed after stop; no new I/O follows it.
            if kind == "irc" {
                let mut s = lock(&self.irc_store)?;
                let mut n = s.state.clone();
                finish(&mut n.delivery)?;
                s.save(n)?
            } else {
                let mut s = lock(&self.requester_store)?;
                let mut n = s.state.clone();
                finish(&mut n.delivery)?;
                s.save(n)?
            }
            if success { sent += 1 } else { failed += 1 }
        }
        let mut report = Value::object();
        report.insert("delivered", sent);
        report.insert("failed_attempts", failed);
        Ok(report)
    }
}
fn send(route: &super::Route, e: &Event) -> Result<()> {
    let mut headers = vec![
        ("Content-Type".into(), "application/json".into()),
        ("Idempotency-Key".into(), e.id.clone()),
        ("X-Mynou-Event-ID".into(), e.id.clone()),
    ];
    if let Some(name) = &route.token_env {
        let secret = crate::config::secret(name).map_err(|_| "Delivery failed")?;
        if secret.len() > 4096 || secret.chars().any(char::is_control) {
            return Err("Delivery failed".into());
        }
        headers.push(("Authorization".into(), format!("Bearer {secret}")));
    }
    let r = HttpClient::new()
        .with_timeout(Duration::from_secs(5))
        .with_max_body(4096)
        .without_redirects()
        .request(
            "POST",
            &route.url,
            &headers,
            json::stringify(&e.payload()).as_bytes(),
        )
        .map_err(|_| "Delivery failed")?;
    if !(200..300).contains(&r.status) {
        return Err("Delivery failed".into());
    }
    Ok(())
}
pub(crate) fn start(engine: &Arc<Engine>, handles: &mut Vec<JoinHandle<()>>) {
    if !engine.config.notifications.routes.iter().any(|r| r.enabled) {
        return;
    }
    let e = engine.clone();
    handles.push(thread::spawn(move || {
        while !e.stopped.load(Ordering::Acquire) {
            let _ = e.dispatch_notifications();
            e.wait(1000)
        }
    }));
}
