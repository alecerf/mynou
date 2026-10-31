//! Pure previews and row-scoped audit reviews remain separate from routing.
use super::{Announcement, ControlRequest, Record, digest, evaluate, hash, valid_id};
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
                        v.insert("protection", s.protection.name());
                        v.insert(
                            "protection_ready",
                            s.protection.check(Some(&self.source_network)).is_ok(),
                        );
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
        v.insert("lookup", lock(&self.irc_lookup)?.to_json());
        Ok(v)
    }
    pub fn irc_announcements(&self, offset: usize, limit: usize) -> Result<Value> {
        if offset > super::MAX_OFFSET || limit == 0 || limit > 200 {
            return Err("IRC: invalid page bounds; use a cursor beyond offset 1000".into());
        }
        let ledger = lock(&self.irc_store)?;
        let mut after = None;
        let mut remaining = offset;
        while remaining > 0 {
            let (rows, next) = ledger.page(2, after, remaining.min(200))?;
            remaining -= rows.len();
            if next.is_none() {
                return self.irc_history_report(&ledger, Vec::new(), None, Some(offset), limit);
            }
            after = next;
        }
        let (rows, next) = ledger.page(2, after, limit)?;
        self.irc_history_report(&ledger, rows, next, Some(offset), limit)
    }
    pub fn irc_announcements_page(&self, after: Option<&str>, limit: usize) -> Result<Value> {
        let after = after.map(super::index::parse_cursor).transpose()?;
        let ledger = lock(&self.irc_store)?;
        let (rows, next) = ledger.page(2, after, limit)?;
        self.irc_history_report(&ledger, rows, next, None, limit)
    }
    fn irc_history_report(
        &self,
        ledger: &super::AnnouncementStore,
        rows: Vec<Record>,
        next: Option<crate::inventory::Key>,
        offset: Option<usize>,
        limit: usize,
    ) -> Result<Value> {
        let runtime = lock(&self.irc_route_runtime)?;
        let mut v = Value::object();
        v.insert("total", Value::Number(ledger.count()? as f64));
        if let Some(offset) = offset {
            v.insert("offset", Value::Number(offset as f64));
        }
        v.insert("limit", limit as u32);
        let next = next.map_or(Value::Null, |k| Value::from(super::index::cursor(&k)));
        // `next` is the field of every other cursor page; `next_cursor` stays for old callers.
        v.insert("next", next.clone());
        v.insert("next_cursor", next);
        v.insert(
            "announcements",
            Value::Array(rows.iter().map(|r| runtime.annotate(r)).collect()),
        );
        Ok(v)
    }
    pub fn irc_announcement(&self, id: &str) -> Result<Value> {
        if !hash(id) {
            return Err("IRC: invalid announcement ID".into());
        }
        let ledger = lock(&self.irc_store)?;
        let record = ledger.get(id)?.ok_or("IRC: unknown announcement")?;
        Ok(lock(&self.irc_route_runtime)?.annotate(&record))
    }
    pub fn irc_preview(&self, source_id: &str, value: &Value) -> Result<Value> {
        super::preview(&self.config, source_id, value)
    }
    pub fn irc_preview_text(&self, source_id: &str, text: &str) -> Result<Value> {
        super::preview_text(&self.config, source_id, text)
    }
    pub fn irc_receive_text(
        &self,
        source_id: &str,
        sender: &str,
        channel: &str,
        text: &str,
    ) -> Result<Option<Value>> {
        let source = self.config.irc.source(source_id)?;
        if self.read_only
            || !source.enabled
            || source.sender != sender
            || !source.channel.eq_ignore_ascii_case(channel)
        {
            return Err("IRC: announcement source or storage is not writable".into());
        }
        let Some(body) = source.wire_payload(text)? else {
            return Ok(None);
        };
        if let Some(candidate) = source.lookup_candidate(&body)? {
            return self.irc_queue_candidate(source, candidate).map(Some);
        }
        self.irc_receive(
            source_id,
            sender,
            channel,
            &source.decode_payload(&body)?.to_json(),
        )
        .map(Some)
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
        let _publication = self.source_publication(source.protection)?;
        let a = Announcement::from_json(value)?;
        let binding = source.binding();
        let id = a.id(&binding);
        let mut ledger = lock(&self.irc_store)?;
        if let Some(r) = ledger.get(&id)? {
            let mut v = r.public_json();
            v.insert("duplicate", true);
            v.insert("claim_changed", r.announcement != a);
            return Ok(v);
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
            admission: None,
        };
        ledger.save_rows(std::slice::from_ref(&record))?;
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
        if query.action == "request" {
            return self.irc_request_control(id, query);
        }
        let mut ledger = lock(&self.irc_store)?;
        let r = ledger.get(id)?.ok_or("IRC: unknown announcement")?;
        let source = self.config.irc.source(&r.source_id)?;
        if !valid_id(&source.id) || source.binding() != r.binding {
            return Err("IRC: source identity changed".into());
        }
        if r.decision != "pending" || r.admission.is_some() {
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
            let mut row = r.clone();
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
            ledger.save_rows(&[row])?;
            report.insert("announcement", result);
        }
        Ok(report)
    }
}
