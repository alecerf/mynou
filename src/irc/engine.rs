//! Pure previews and row-scoped audit reviews remain separate from routing.
use super::{Announcement, ControlRequest, MAX_RECORDS, Record, digest, evaluate, hash, valid_id};
use crate::{
    Result,
    engine::{Engine, lock},
    json::{self, Value},
    store,
};
impl Engine {
    pub fn irc_sources(&self) -> Result<Value> {
        let runtime = lock(&self.irc_runtime)?;
        let mut v = Value::object();
        v.insert(
            "sources",
            Value::Array(
                self.config
                    .irc
                    .sources
                    .iter()
                    .map(|s| {
                        let mut v = s.public_json();
                        if let Some(h) = runtime.health.get(&s.id) {
                            v.insert("health", h.to_json());
                        }
                        v
                    })
                    .collect(),
            ),
        );
        v.insert(
            "rules",
            Value::Array(
                self.config
                    .irc
                    .rules
                    .iter()
                    .map(super::Rule::to_json)
                    .collect(),
            ),
        );
        v.insert(
            "automatic_acquisition",
            super::routing::enabled(&self.config),
        );
        v.insert("routing", self.irc_routing_json()?);
        Ok(v)
    }
    pub fn irc_announcements(&self, offset: usize, limit: usize) -> Result<Value> {
        if offset > MAX_RECORDS || limit == 0 || limit > 200 {
            return Err("IRC: invalid page bounds".into());
        }
        let ledger = lock(&self.irc_store)?;
        let runtime = lock(&self.irc_route_runtime)?;
        let mut rows: Vec<_> = ledger.state.records.values().collect();
        rows.sort_by(|a, b| (&b.first_seen, &b.id).cmp(&(&a.first_seen, &a.id)));
        let mut v = Value::object();
        v.insert("total", rows.len() as u32);
        v.insert("offset", offset as u32);
        v.insert("limit", limit as u32);
        v.insert(
            "announcements",
            Value::Array(
                rows.into_iter()
                    .skip(offset)
                    .take(limit)
                    .map(|r| runtime.annotate(r))
                    .collect(),
            ),
        );
        Ok(v)
    }
    pub fn irc_announcement(&self, id: &str) -> Result<Value> {
        if !hash(id) {
            return Err("IRC: invalid announcement ID".into());
        }
        let ledger = lock(&self.irc_store)?;
        let record = ledger
            .state
            .records
            .get(id)
            .ok_or("IRC: unknown announcement")?;
        Ok(lock(&self.irc_route_runtime)?.annotate(record))
    }
    pub fn irc_preview(&self, source_id: &str, value: &Value) -> Result<Value> {
        super::preview(&self.config, source_id, value)
    }
    /// A library caller must supply the configured sender/channel, like the live client.
    pub fn irc_receive(
        &self,
        source_id: &str,
        sender: &str,
        channel: &str,
        value: &Value,
    ) -> Result<Value> {
        if self.read_only {
            return Err("IRC: history is read-only".into());
        }
        let source = self.config.irc.source(source_id)?;
        if !source.enabled
            || source.sender != sender
            || !source.channel.eq_ignore_ascii_case(channel)
        {
            return Err("IRC: announcement source is not allowed".into());
        }
        let a = Announcement::from_json(value)?;
        let binding = source.binding();
        let id = a.id(&binding);
        let mut ledger = lock(&self.irc_store)?;
        if let Some(r) = ledger.state.records.get(&id) {
            let mut v = r.public_json();
            v.insert("duplicate", true);
            v.insert("claim_changed", r.announcement != a);
            return Ok(v);
        }
        if ledger.state.records.len() == MAX_RECORDS {
            return Err("IRC: announcement history is full; no identity was discarded".into());
        }
        let now = store::now();
        let evaluations = evaluate(&self.config.irc, &self.config.selection, source_id, &a)?;
        let record = Record {
            id: id.clone(),
            source_id: source_id.into(),
            binding,
            announcement: a,
            evaluations,
            fingerprint: self.config.irc.fingerprint(&self.config.selection),
            decision: "pending".into(),
            revision: 1,
            first_seen: now,
            decided_at: None,
            route: None,
        };
        let mut next = ledger.state.clone();
        next.records.insert(id, record.clone());
        ledger.save(next)?;
        let mut v = record.public_json();
        v.insert("duplicate", false);
        v.insert("claim_changed", false);
        Ok(v)
    }
    pub fn irc_control(&self, id: &str, query: &ControlRequest) -> Result<Value> {
        query.validate()?;
        if !hash(id) {
            return Err("IRC: invalid announcement ID".into());
        }
        let mut ledger = lock(&self.irc_store)?;
        let r = ledger
            .state
            .records
            .get(id)
            .ok_or("IRC: unknown announcement")?
            .clone();
        let source = self.config.irc.source(&r.source_id)?;
        if !valid_id(&source.id) || source.binding() != r.binding {
            return Err("IRC: source identity changed".into());
        }
        if r.decision != "pending" {
            return Err("IRC: announcement was already reviewed".into());
        }
        let mut scope = r.to_json();
        scope.insert(
            "current_configuration",
            self.config.irc.fingerprint(&self.config.selection),
        );
        scope.insert("action", query.action.clone());
        let plan = digest(json::stringify(&scope).as_bytes());
        let mut report = Value::object();
        report.insert("action", query.action.clone());
        report.insert("plan_id", plan.clone());
        report.insert("announcement", r.public_json());
        report.insert("apply", query.apply);
        report.insert("acquisition_started", false);
        if query.apply {
            if query.plan_id.as_deref() != Some(&plan) {
                return Err("IRC: review is stale; preview again".into());
            }
            let mut next = ledger.state.clone();
            let row = next
                .records
                .get_mut(id)
                .ok_or("IRC: missing announcement")?;
            row.decision = if query.action == "acknowledge" {
                "acknowledged"
            } else {
                "dismissed"
            }
            .into();
            row.revision = row
                .revision
                .checked_add(1)
                .ok_or("IRC: decision revision overflow")?;
            row.decided_at = Some(store::now());
            let result = row.public_json();
            ledger.save(next)?;
            report.insert("announcement", result);
        }
        Ok(report)
    }
}
