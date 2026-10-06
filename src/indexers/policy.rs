//! Private checked source policy. Cookies and credential values are never persisted.
use super::{Authentication, Options, Source};
use crate::{
    Result,
    config::Config,
    crypto::{random_bytes, sha256},
    engine::{Engine, lock},
    json::{self, Value},
    requesters::{digest, valid_digest, valid_id},
    store::{self, private_options, reject_symlinks, sync_directory},
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
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
    pub(crate) fn binding(&self, source: &Source) -> String {
        let mut fields = vec![
            source.name.clone().into(),
            source.kind.clone().into(),
            source.url.clone().into(),
            source.api_key_env.clone().into(),
            self.min_interval_ms.to_string().into(),
            self.auth_binding(),
        ];
        // Preserve the exact preceding binding for every existing torrent source.
        if let Some(options) = &self.newznab {
            fields.push(options.json());
        }
        digest(json::stringify(&Value::Array(fields)).as_bytes())
    }
    pub(crate) fn identity(&self, source: &Source) -> String {
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
    rows: BTreeMap<String, Record>,
    path: PathBuf,
    read_only: bool,
    dirty: bool,
    poisoned: bool,
}
impl SourceStore {
    pub(crate) fn open(dir: &Path, config: &Config, read_only: bool) -> Result<Self> {
        let path = dir.join("indexers.bin");
        reject_symlinks(&path).map_err(|_| "Indexer policy: invalid snapshot path")?;
        let mut rows = BTreeMap::new();
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("Indexer policy: cannot inspect snapshot".into()),
            Ok(m) => {
                if !m.is_file() || m.len() < 48 || m.len() > MAX_BYTES as u64 + 48 {
                    return Err("Indexer policy: invalid snapshot size".into());
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if m.nlink() != 1 || m.mode() & 0o077 != 0 {
                        return Err(
                            "Indexer policy: snapshot must be private with one hard link".into(),
                        );
                    }
                }
                let file = private_options()
                    .read(true)
                    .open(&path)
                    .map_err(|_| "Indexer policy: cannot read snapshot")?;
                let opened = file
                    .metadata()
                    .map_err(|_| "Indexer policy: cannot inspect snapshot")?;
                if !opened.is_file() || opened.len() != m.len() {
                    return Err("Indexer policy: snapshot changed during opening".into());
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if opened.nlink() != 1
                        || opened.mode() & 0o077 != 0
                        || opened.dev() != m.dev()
                        || opened.ino() != m.ino()
                    {
                        return Err("Indexer policy: snapshot changed during opening".into());
                    }
                }
                let mut bytes = Vec::new();
                file.take(MAX_BYTES as u64 + 49)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "Indexer policy: cannot read snapshot")?;
                if bytes.len() < 48
                    || bytes.len() > MAX_BYTES + 48
                    || &bytes[..8] != MAGIC
                    || u64::from_le_bytes(
                        bytes[8..16]
                            .try_into()
                            .map_err(|_| "Indexer policy: invalid length")?,
                    ) != bytes.len() as u64 - 48
                    || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
                {
                    return Err("Indexer policy: corrupt or unsupported snapshot".into());
                }
                rows = Self::parse(&json::parse(
                    std::str::from_utf8(&bytes[16..bytes.len() - 32])
                        .map_err(|_| "Indexer policy: invalid UTF-8")?,
                )?)?;
            }
        }
        let before = rows.clone();
        let mut configured = std::collections::BTreeSet::new();
        for source in &config.sources {
            let id = source.options.identity(source);
            if !configured.insert(id.clone()) {
                return Err("Indexer policy: duplicate configured source ID".into());
            }
            let binding = source.options.binding(source);
            if rows.get(&id).is_some_and(|r| r.binding != binding) {
                return Err("Indexer policy: source binding changed; use a new stable ID".into());
            }
            if !rows.contains_key(&id) {
                rows.insert(
                    id.clone(),
                    Record {
                        id,
                        binding,
                        enabled: source.options.enabled,
                        revision: 1,
                        action: "initial".into(),
                        at: store::now(),
                    },
                );
            }
        }
        if rows.len() > 1000 {
            return Err("Indexer policy: retained source capacity reached".into());
        }
        let dirty = rows != before;
        Ok(Self {
            rows,
            path,
            read_only,
            dirty,
            poisoned: false,
        })
    }
    fn parse(v: &Value) -> Result<BTreeMap<String, Record>> {
        crate::numbering::only(v, &["records"])?;
        let mut rows = BTreeMap::new();
        for v in v
            .get("records")
            .and_then(Value::as_array)
            .filter(|a| a.len() <= 1000)
            .ok_or("Indexer policy: invalid collection")?
        {
            let r = Record::from_json(v)?;
            if rows.insert(r.id.clone(), r).is_some() {
                return Err("Indexer policy: duplicate record".into());
            }
        }
        Ok(rows)
    }
    fn json(rows: &BTreeMap<String, Record>) -> Value {
        let mut v = Value::object();
        v.insert(
            "records",
            Value::Array(rows.values().map(Record::to_json).collect()),
        );
        v
    }
    pub(crate) fn initialize(&mut self, config: &Config) -> Result<()> {
        if !self.read_only {
            if self.dirty {
                self.save(self.rows.clone())?
            }
            for s in &config.sources {
                s.options.publish(&self.rows[&s.options.identity(s)]);
            }
        }
        Ok(())
    }
    fn save(&mut self, rows: BTreeMap<String, Record>) -> Result<()> {
        if self.read_only || self.poisoned {
            return Err("Indexer policy: writable recovery is required".into());
        }
        let v = Self::json(&rows);
        Self::parse(&v)?;
        let payload = json::stringify(&v).into_bytes();
        if payload.len() > MAX_BYTES {
            return Err("Indexer policy: snapshot too large".into());
        }
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&sha256(&bytes));
        reject_symlinks(&self.path).map_err(|_| "Indexer policy: invalid snapshot path")?;
        let temp = self
            .path
            .with_file_name(format!(".indexers-{}.tmp", digest(&random_bytes::<16>()?)));
        let result = (|| -> Result<()> {
            let mut f = private_options()
                .create_new(true)
                .write(true)
                .open(&temp)
                .map_err(|_| "Indexer policy: cannot create snapshot")?;
            f.write_all(&bytes)
                .and_then(|()| f.sync_all())
                .map_err(|_| "Indexer policy: cannot persist snapshot")?;
            fs::rename(&temp, &self.path).map_err(|_| "Indexer policy: cannot replace snapshot")?;
            if sync_directory(
                self.path
                    .parent()
                    .ok_or("Indexer policy: missing directory")?,
            )
            .is_err()
            {
                self.poisoned = true;
                return Err("Indexer policy: durability uncertain; restart required".into());
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        self.rows = rows;
        self.dirty = false;
        Ok(())
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
        let s = lock(&self.indexer_store)?;
        let mut report = super::report(&self.config.sources);
        if let Some(Value::Array(rows)) = report.get_mut("sources") {
            for v in rows {
                let id = text(v, "id")?;
                let r = &s.rows[&id];
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
        let r = store
            .rows
            .get(id)
            .ok_or("Indexer policy: source not found")?;
        if (q.action == "enable" && r.enabled)
            || (q.action == "pause" && !r.enabled)
            || (q.action == "probe" && !r.enabled)
        {
            return Err("Indexer policy: action does not apply to the current state".into());
        }
        let mut scope = SourceStore::json(&store.rows);
        scope.insert("id", id);
        scope.insert("action", q.action.clone());
        let guard = digest(json::stringify(&scope).as_bytes());
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
        let mut next = store.rows.clone();
        let row = next.get_mut(id).ok_or("Indexer policy: missing record")?;
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
        let row = row.clone();
        store.save(next)?;
        source.options.publish(&row);
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
                &self.config,
                source,
                &request,
                Instant::now() + Duration::from_secs(5),
            );
            report.insert("probe_success", ok);
        }
        Ok(report)
    }
}
