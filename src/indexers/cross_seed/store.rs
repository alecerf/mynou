//! Private `cross-seed.idx`: seen feed entries, completed searches, provenance
//! of registered cross-seeds and bounded outcome counters. Created on the first
//! write; read-only opens create nothing.
use crate::{
    Result,
    crypto::{random_bytes, sha256},
    inventory::{Change, Index, Key, MAX_PAGE_ROWS, Query},
    json::{self, Value},
    store::{reject_symlinks, sync_directory},
};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
};

const INDEX: &str = "cross-seed.idx";
const STAGING: &str = "cross-seed.idx.staging";
const FORMAT: &[u8] = b"mynou-cross-seed/1";
const HEADER: Key = [0; 64];
const STATS: Key = single(1);
const RECENT: Key = single(2);
const T_SEEN: u8 = 0x10;
const T_SEARCHED: u8 = 0x11;
const T_ADDED: u8 = 0x12;
/// Feed entry identities remembered per source; feeds list at most 500.
pub(super) const MAX_SEEN: usize = 2_048;
pub(super) const MAX_RECENT: usize = 100;
/// Distinct outcome reasons kept in the counters.
const MAX_REASONS: usize = 64;

const fn single(table: u8) -> Key {
    let mut key = [0; 64];
    key[0] = table;
    key
}
fn key(table: u8, parts: &[&str]) -> Key {
    let mut text = Vec::new();
    for part in parts {
        text.extend_from_slice(&(part.len() as u64).to_be_bytes());
        text.extend_from_slice(part.as_bytes());
    }
    let mut key = single(table);
    key[1..33].copy_from_slice(&sha256(&text));
    key
}
fn exists(path: &Path) -> Result<bool> {
    reject_symlinks(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Cross-seed discovery storage: cannot inspect its path".into()),
    }
}
fn text(bytes: &[u8]) -> Result<Value> {
    json::parse(
        std::str::from_utf8(bytes).map_err(|_| "Cross-seed discovery storage: invalid UTF-8")?,
    )
}

/// Where a registered cross-seed came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub candidate_id: String,
    pub source_transfer_id: String,
    pub indexer: String,
    pub trigger: String,
    pub at: u64,
}
impl Provenance {
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("candidate_id", self.candidate_id.clone());
        v.insert("source_transfer_id", self.source_transfer_id.clone());
        v.insert("indexer", self.indexer.clone());
        v.insert("trigger", self.trigger.clone());
        v.insert("at", self.at.to_string());
        v
    }
    fn from_json(v: &Value) -> Result<Self> {
        let field = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "Cross-seed discovery storage: invalid provenance".to_owned())
        };
        Ok(Self {
            candidate_id: field("candidate_id")?,
            source_transfer_id: field("source_transfer_id")?,
            indexer: field("indexer")?,
            trigger: field("trigger")?,
            at: field("at")?
                .parse()
                .map_err(|_| "Cross-seed discovery storage: invalid provenance time")?,
        })
    }
}

/// One evaluated offer or failed request, without titles, URLs or credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub at: u64,
    pub indexer: String,
    pub trigger: String,
    /// `added`, `skipped` or `rejected`.
    pub result: String,
    pub reason: String,
    pub source_transfer_id: Option<String>,
    pub candidate_id: Option<String>,
}
impl Outcome {
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("at", self.at.to_string());
        v.insert("indexer", self.indexer.clone());
        v.insert("trigger", self.trigger.clone());
        v.insert("result", self.result.clone());
        v.insert("reason", self.reason.clone());
        let id = |id: &Option<String>| id.clone().map_or(Value::Null, Value::String);
        v.insert("source_transfer_id", id(&self.source_transfer_id));
        v.insert("candidate_id", id(&self.candidate_id));
        v
    }
    fn from_json(v: &Value) -> Result<Self> {
        let field = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "Cross-seed discovery storage: invalid outcome".to_owned())
        };
        let id = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        Ok(Self {
            at: field("at")?
                .parse()
                .map_err(|_| "Cross-seed discovery storage: invalid outcome time")?,
            indexer: field("indexer")?,
            trigger: field("trigger")?,
            result: field("result")?,
            reason: field("reason")?,
            source_transfer_id: id("source_transfer_id"),
            candidate_id: id("candidate_id"),
        })
    }
}

/// Everything one pass commits together, so a restart never repeats a
/// registered candidate nor forgets an entry it already evaluated.
#[derive(Default)]
pub struct Batch {
    pub seen: BTreeMap<String, Vec<String>>,
    pub searched: Vec<(String, String, u64)>,
    pub added: Vec<Provenance>,
    pub outcomes: Vec<Outcome>,
}
impl Batch {
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
            && self.searched.is_empty()
            && self.added.is_empty()
            && self.outcomes.is_empty()
    }
}

pub struct DiscoveryStore {
    directory: PathBuf,
    index: Option<Index>,
    read_only: bool,
}

impl DiscoveryStore {
    pub fn open(directory: &Path, read_only: bool) -> Result<Self> {
        let index = if exists(&directory.join(INDEX))? {
            let mut index = Index::open(&directory.join(INDEX), read_only)?;
            if index.get(&HEADER)?.as_deref() != Some(FORMAT) {
                return Err("Cross-seed discovery storage: unsupported format".into());
            }
            Some(index)
        } else {
            None
        };
        Ok(Self {
            directory: directory.to_owned(),
            index,
            read_only,
        })
    }

    fn get(&mut self, key: &Key) -> Result<Option<Vec<u8>>> {
        match &mut self.index {
            Some(index) => index.get(key),
            None => Ok(None),
        }
    }

    /// Entry identities already evaluated for this source, oldest first.
    pub fn seen(&mut self, indexer: &str) -> Result<VecDeque<String>> {
        let Some(bytes) = self.get(&key(T_SEEN, &[indexer]))? else {
            return Ok(VecDeque::new());
        };
        let text = String::from_utf8(bytes)
            .map_err(|_| "Cross-seed discovery storage: invalid seen entries")?;
        Ok(text.lines().map(str::to_owned).collect())
    }

    /// When this source last searched for the transfer's candidates.
    pub fn searched(&mut self, indexer: &str, transfer: &str) -> Result<Option<u64>> {
        self.get(&key(T_SEARCHED, &[indexer, transfer]))?
            .map(|bytes| {
                Ok(u64::from_le_bytes(bytes.try_into().map_err(
                    |_| "Cross-seed discovery storage: invalid search time",
                )?))
            })
            .transpose()
    }

    /// Registered cross-seeds, newest pages first by identity digest.
    pub fn added_page(&mut self, limit: usize) -> Result<(Vec<Provenance>, u64)> {
        let Some(index) = &mut self.index else {
            return Ok((Vec::new(), 0));
        };
        let page = index.page(&Query {
            start: single(T_ADDED),
            end: Some(single(T_ADDED + 1)),
            after: None,
            limit: limit.clamp(1, MAX_PAGE_ROWS),
            sequence: None,
        })?;
        let total = index.range_summary(single(T_ADDED), Some(single(T_ADDED + 1)))?;
        Ok((
            page.entries
                .iter()
                .map(|(_, bytes)| Provenance::from_json(&text(bytes)?))
                .collect::<Result<_>>()?,
            total.count,
        ))
    }

    pub fn stats(&mut self) -> Result<Value> {
        Ok(match self.get(&STATS)? {
            Some(bytes) => text(&bytes)?,
            None => Value::object(),
        })
    }

    pub fn recent(&mut self) -> Result<Vec<Outcome>> {
        let Some(bytes) = self.get(&RECENT)? else {
            return Ok(Vec::new());
        };
        text(&bytes)?
            .as_array()
            .ok_or("Cross-seed discovery storage: invalid recent outcomes")?
            .iter()
            .map(Outcome::from_json)
            .collect()
    }

    pub fn commit(&mut self, batch: Batch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut changes = Vec::new();
        for (indexer, ids) in &batch.seen {
            let mut seen = self.seen(indexer)?;
            for id in ids {
                if !seen.contains(id) {
                    seen.push_back(id.clone());
                }
            }
            while seen.len() > MAX_SEEN {
                seen.pop_front();
            }
            let joined: Vec<_> = seen.into_iter().collect();
            changes.push(Change::Put(
                key(T_SEEN, &[indexer]),
                joined.join("\n").into_bytes(),
            ));
        }
        for (indexer, transfer, at) in &batch.searched {
            changes.push(Change::Put(
                key(T_SEARCHED, &[indexer, transfer]),
                at.to_le_bytes().to_vec(),
            ));
        }
        for provenance in &batch.added {
            changes.push(Change::Put(
                key(T_ADDED, &[&provenance.candidate_id]),
                json::stringify(&provenance.to_json()).into_bytes(),
            ));
        }
        if !batch.outcomes.is_empty() {
            let mut stats = self.stats()?;
            for outcome in &batch.outcomes {
                count(&mut stats, outcome);
            }
            changes.push(Change::Put(STATS, json::stringify(&stats).into_bytes()));
            let mut recent = self.recent()?;
            recent.extend(batch.outcomes.iter().cloned());
            let excess = recent.len().saturating_sub(MAX_RECENT);
            recent.drain(..excess);
            changes.push(Change::Put(
                RECENT,
                json::stringify(&Value::Array(recent.iter().map(Outcome::to_json).collect()))
                    .into_bytes(),
            ));
        }
        if changes.is_empty() {
            return Ok(());
        }
        self.writable()?.commit(changes)?;
        Ok(())
    }

    fn writable(&mut self) -> Result<&mut Index> {
        if self.read_only {
            return Err("Cross-seed discovery storage is open read-only".into());
        }
        if self.index.is_none() {
            self.index = Some(self.create()?);
        }
        self.index
            .as_mut()
            .ok_or_else(|| "Cross-seed discovery storage is unavailable".into())
    }

    /// Publishes a complete new index atomically; unfinished attempts are kept.
    fn create(&self) -> Result<Index> {
        let staging = self.directory.join(STAGING);
        if exists(&staging)? {
            let preserved = self.directory.join(format!(
                "cross-seed.idx.abandoned-{}",
                crate::requesters::digest(&random_bytes::<16>()?)
            ));
            fs::rename(&staging, &preserved).map_err(
                |_| "Cross-seed discovery storage: cannot preserve unfinished staging data",
            )?;
        }
        let mut index = Index::create(&staging)?;
        index.commit(vec![Change::Put(HEADER, FORMAT.to_vec())])?;
        drop(index);
        if exists(&self.directory.join(INDEX))? {
            return Err("Cross-seed discovery storage: index appeared before promotion".into());
        }
        fs::rename(&staging, self.directory.join(INDEX)).map_err(|error| {
            crate::store::migration_io("Cross-seed discovery storage", "write", &error)
        })?;
        sync_directory(&self.directory).map_err(|error| {
            crate::store::migration_sync("Cross-seed discovery storage", &error)
        })?;
        Index::open(&self.directory.join(INDEX), false)
    }
}

/// Totals per result and reason; unknown reasons beyond the bound share `other`.
fn count(stats: &mut Value, outcome: &Outcome) {
    if !matches!(stats, Value::Object(_)) {
        *stats = Value::object();
    }
    let Value::Object(results) = stats else {
        return;
    };
    let reasons = results
        .entry(outcome.result.clone())
        .or_insert_with(Value::object);
    let Value::Object(reasons) = reasons else {
        return;
    };
    let reason = if reasons.contains_key(&outcome.reason) || reasons.len() < MAX_REASONS {
        outcome.reason.clone()
    } else {
        "other".to_owned()
    };
    let total = reasons
        .get(&reason)
        .and_then(Value::as_str)
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    reasons.insert(reason, Value::String(total.to_string()));
}
