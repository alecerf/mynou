//! Private checked source policy. Cookies and credential values are never persisted.
use super::{Authentication, Options, Source};
use crate::{
    Result,
    config::Config,
    engine::{Engine, lock},
    json::{self, Value},
    requesters::{digest, valid_digest, valid_id},
    store,
};
use std::{path::Path, sync::atomic::Ordering, time::{Duration, Instant}};
mod storage;
const MAGIC: &[u8; 8] = b"MYNOUS01";
const MAX_BYTES: usize = 2 * 1024 * 1024;
fn text(v: &Value, k: &str) -> Result<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Indexer policy: invalid {k}"))
}
fn num(v: &Value, k: &str) -> Result<u64> {
    v.get(k)
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .ok_or_else(|| format!("Indexer policy: invalid {k}"))
}
impl Options {
    fn auth_binding(&self) -> Value {
        match &self.authentication {
            Authentication::None => Value::Null,
            Authentication::Basic {
                username_env,
                password_env,
            } => Value::Array(vec![
                "basic".into(),
                username_env.clone().into(),
                password_env.clone().into(),
            ]),
            Authentication::Bearer { token_env } => {
                Value::Array(vec!["bearer".into(), token_env.clone().into()])
            }
            Authentication::Form(f) => Value::Array(vec![
                "form".into(),
                f.login_url.clone().into(),
                f.username_env.clone().into(),
                f.password_env.clone().into(),
                f.username_field.clone().into(),
                f.password_field.clone().into(),
                f.cookie_name.clone().into(),
                f.max_age_secs.to_string().into(),
            ]),
        }
    }
    pub(super) fn binding(&self, source: &Source) -> String {
        let mut parts = vec![
            source.name.clone().into(),
            source.kind.clone().into(),
            source.url.clone().into(),
            source.api_key_env.clone().into(),
            self.min_interval_ms.to_string().into(),
            self.auth_binding(),
        ];
        // Only a mapped source binds its mapping, so earlier bindings stay valid.
        if !self.mapping.canonical().is_empty() {
            parts.push(self.mapping.canonical().into());
        }
        digest(json::stringify(&Value::Array(parts)).as_bytes())
    }
    pub(super) fn identity(&self, source: &Source) -> String {
        self.id.clone().unwrap_or_else(|| self.binding(source))
    }
    pub(super) fn effective_enabled(&self) -> bool {
        if self.policy_initialized.load(Ordering::Acquire) {
            self.policy_enabled.load(Ordering::Acquire)
        } else {
            self.enabled
        }
    }
    fn publish(&self, r: &Record) {
        if r.action == "reset_session" {
            self.reset_pending.store(true, Ordering::Release);
        }
        self.policy_enabled.store(r.enabled, Ordering::Release);
        self.generation.store(r.revision, Ordering::Release);
        self.policy_initialized.store(true, Ordering::Release);
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    id: String,
    binding: String,
    enabled: bool,
    revision: u64,
    action: String,
    at: u64,
}
impl Record {
    fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("id", self.id.clone());
        v.insert("binding", self.binding.clone());
        v.insert("enabled", self.enabled);
        v.insert("revision", self.revision.to_string());
        v.insert("action", self.action.clone());
        v.insert("at", self.at.to_string());
        v
    }
    fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(v, &["id", "binding", "enabled", "revision", "action", "at"])?;
        let r = Self {
            id: text(v, "id")?,
            binding: text(v, "binding")?,
            enabled: v
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or("Indexer policy: invalid enabled")?,
            revision: num(v, "revision")?,
            action: text(v, "action")?,
            at: num(v, "at")?,
        };
        if !valid_id(&r.id)
            || !valid_digest(&r.binding)
            || r.revision == 0
            || !matches!(
                r.action.as_str(),
                "initial" | "enable" | "pause" | "reset_session" | "probe"
            )
        {
            return Err("Indexer policy: invalid record".into());
        }
        Ok(r)
    }
}
pub(crate) struct SourceStore {
    storage: storage::Storage,
    read_only: bool,
}
impl SourceStore {
    fn row(&mut self, id: &str) -> Result<Record> {
        self.storage.row(id)?.ok_or_else(|| "Indexer policy: source not configured".into())
    }
    pub(crate) fn authorization(&mut self, id: &str) -> Result<(bool, u64)> {
        let row = self.row(id)?;
        Ok((row.enabled, row.revision))
    }
    pub(crate) fn open(dir: &Path, config: &Config, read_only: bool) -> Result<Self> {
        let mut store = Self { storage: storage::Storage::open(dir, read_only)?, read_only };
        let mut configured = std::collections::BTreeSet::new();
        for source in &config.sources {
            let id = source.options.identity(source);
            if !configured.insert(id.clone()) {
                return Err("Indexer policy: duplicate configured source ID".into());
            }
            if store.storage.row(&id)?.is_some_and(|r| r.binding != source.options.binding(source)) {
                return Err("Indexer policy: source binding changed; use a new stable ID".into());
            }
        }
        Ok(store)
    }
    fn configured(&mut self, source: &Source) -> Result<Record> {
        let id = source.options.identity(source);
        if !self.read_only { return self.row(&id); }
        Ok(self.storage.row(&id)?.unwrap_or_else(|| Record {
            id,
            binding: source.options.binding(source),
            enabled: source.options.enabled,
            revision: 1,
            action: "initial".into(),
            at: 0,
        }))
    }
    pub(crate) fn initialize(&mut self, config: &Config) -> Result<()> {
        self.storage.initialize()?;
        if self.read_only { return Ok(()); }
        for source in &config.sources {
            let id = source.options.identity(source);
            if self.storage.row(&id)?.is_none() {
                let mut row = Record {
                    id: id.clone(),
                    binding: source.options.binding(source),
                    enabled: source.options.enabled,
                    revision: 1,
                    action: "initial".into(),
                    at: 0,
                };
                row.at = store::now();
                self.storage.commit(row)?;
            }
            source.options.publish(&self.row(&id)?);
        }
        Ok(())
    }
    fn guard(&mut self, row: &Record, action: &str) -> Result<String> {
        let (identity, sequence) = self.storage.guard()?;
        let mut scope = row.to_json();
        scope.insert("action", action);
        scope.insert("store", digest(&identity));
        scope.insert("sequence", sequence.to_string());
        Ok(digest(json::stringify(&scope).as_bytes()))
    }
}
#[derive(Clone, Debug)]
pub struct ControlRequest {
    pub action: String,
    pub apply: bool,
    pub plan_id: Option<String>,
}
impl ControlRequest {
    pub fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(v, &["action", "apply", "plan_id"])?;
        let q = Self {
            action: text(v, "action")?,
            apply: match v.get("apply") {
                None => false,
                Some(Value::Bool(b)) => *b,
                _ => return Err("Indexer policy: invalid apply flag".into()),
            },
            plan_id: match v.get("plan_id") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                _ => return Err("Indexer policy: invalid guard".into()),
            },
        };
        q.validate()?;
        Ok(q)
    }
    pub fn validate(&self) -> Result<()> {
        if !matches!(
            self.action.as_str(),
            "enable" | "pause" | "reset_session" | "probe"
        ) || self.plan_id.as_ref().is_some_and(|s| !valid_digest(s))
            || (self.apply && self.plan_id.is_none())
        {
            return Err("Indexer policy: invalid guarded control".into());
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
impl Engine {
    pub fn indexers(&self) -> Result<Value> {
        let mut s = lock(&self.indexer_store)?;
        let mut report = super::report(&self.config.sources);
        if let Some(Value::Array(rows)) = report.get_mut("sources") {
            for v in rows {
                let id = text(v, "id")?;
                let source = self.config.sources.iter().find(|source| source.options.identity(source) == id)
                    .ok_or("Indexer policy: source not configured")?;
                let r = s.configured(source)?;
                v.insert("enabled", r.enabled);
                v.insert("policy_revision", r.revision.to_string());
                v.insert("last_action", r.action.clone());
            }
        }
        Ok(report)
    }
    pub fn indexer_control(&self, id: &str, q: &ControlRequest) -> Result<Value> {
        q.validate()?;
        if !valid_id(id) {
            return Err("Indexer policy: invalid source ID".into());
        }
        let source = self
            .config
            .sources
            .iter()
            .find(|s| s.options.identity(s) == id)
            .ok_or("Indexer policy: source not configured")?;
        let mut store = lock(&self.indexer_store)?;
        let r = store.configured(source)?;
        if (q.action == "enable" && r.enabled)
            || (q.action == "pause" && !r.enabled)
            || (q.action == "probe" && !r.enabled)
        {
            return Err("Indexer policy: action does not apply to the current state".into());
        }
        let guard = store.guard(&r, &q.action)?;
        let mut report = Value::object();
        report.insert("id", id);
        report.insert("name", crate::integrations::report_text(&source.name, 128));
        report.insert("action", q.action.clone());
        report.insert("plan_id", guard.clone());
        report.insert("applied", false);
        report.insert("enabled", r.enabled);
        if !q.apply {
            return Ok(report);
        }
        if self.read_only || self.stopped.load(Ordering::Acquire) {
            return Err("Indexer policy: application requires the running service".into());
        }
        if q.plan_id.as_deref() != Some(&guard) {
            return Err("Indexer policy: review is stale; preview again".into());
        }
        let mut row = r;
        row.revision = row
            .revision
            .checked_add(1)
            .ok_or("Indexer policy: revision overflow")?;
        row.action = q.action.clone();
        row.at = crate::store::now();
        if q.action == "enable" {
            row.enabled = true
        }
        if q.action == "pause" {
            row.enabled = false
        }
        if let Err(error) = store.storage.commit(row.clone()) {
            // A failed persistence response can mean the control committed. Fence
            // configured runtime callers too; only reopen may restore policy.
            for configured in &self.config.sources {
                configured.options.policy_enabled.store(false, Ordering::Release);
                configured.options.generation.fetch_xor(u64::MAX, Ordering::AcqRel);
                configured.options.policy_initialized.store(true, Ordering::Release);
            }
            return Err(error);
        }
        source.options.publish(&row);
        if q.action == "pause" {
            lock(&self.feed_store)?.suspend(id)?;
        }
        drop(store);
        report.insert("applied", true);
        report.insert("enabled", row.enabled);
        if q.action == "probe" {
            let request = crate::store::Request {
                kind: "movie".into(),
                title: "Mynou source health probe".into(),
                year: 0,
                season: 0,
                episode: 0,
                tmdb_id: None,
                source_numbering: None,
                source_path: None,
                source_url: None,
            };
            let ok = crate::integrations::probe_source(
                source,
                &request,
                Instant::now() + Duration::from_secs(5),
            );
            report.insert("probe_success", ok);
        }
        Ok(report)
    }
}
