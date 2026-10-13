//! Private checked baseline identities and bounded captured routing work.
//! Candidate payloads remain private; public summaries expose only progress.
use super::Source;
use crate::{
    Result,
    config::Config,
    json::Value,
    requesters::{digest, valid_digest, valid_id},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
pub(super) mod generation;
mod storage;

const MAGIC: &[u8; 8] = b"MYNOUF01";
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_ROWS: usize = 1_000;
/// Deduplication window per source; the oldest identities leave first.
pub(crate) const MAX_SEEN: usize = 1_024;
pub(crate) const MAX_WATCHED: usize = 64;
/// A source that was not polled successfully for this long is baselined again
/// instead of treating everything published meanwhile as new.
pub(crate) const STALE_SECS: u64 = 7 * 24 * 60 * 60;

fn text(v: &Value, k: &str) -> Result<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Feed state: invalid {k}"))
}
fn num(v: &Value, k: &str) -> Result<u64> {
    v.get(k)
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .ok_or_else(|| format!("Feed state: invalid {k}"))
}
fn identity(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The durable cursor and bounded deduplication window of one watched source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) id: String,
    pub(crate) binding: String,
    pub(crate) baselined: bool,
    pub(crate) cursor: String,
    pub(crate) seen: Vec<String>,
    pub(crate) revision: u64,
    pub(crate) baselined_at: u64,
    pub(crate) last_success_at: u64,
    pub(crate) successes: u64,
    pub(crate) new_entries: u64,
}
impl Record {
    pub(crate) fn new(id: String, binding: String) -> Self {
        Self {
            id,
            binding,
            baselined: false,
            cursor: String::new(),
            seen: Vec::new(),
            revision: 0,
            baselined_at: 0,
            last_success_at: 0,
            successes: 0,
            new_entries: 0,
        }
    }
    fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("id", self.id.clone());
        v.insert("binding", self.binding.clone());
        v.insert("baselined", self.baselined);
        v.insert("cursor", self.cursor.clone());
        v.insert(
            "seen",
            Value::Array(self.seen.iter().cloned().map(Value::from).collect()),
        );
        v.insert("revision", self.revision.to_string());
        v.insert("baselined_at", self.baselined_at.to_string());
        v.insert("last_success_at", self.last_success_at.to_string());
        v.insert("successes", self.successes.to_string());
        v.insert("new_entries", self.new_entries.to_string());
        v
    }
    fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(
            v,
            &[
                "id",
                "binding",
                "baselined",
                "cursor",
                "seen",
                "revision",
                "baselined_at",
                "last_success_at",
                "successes",
                "new_entries",
            ],
        )?;
        let seen: Vec<String> = v
            .get("seen")
            .and_then(Value::as_array)
            .filter(|a| a.len() <= MAX_SEEN)
            .ok_or("Feed state: invalid deduplication window")?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "Feed state: invalid identity".to_owned())
            })
            .collect::<Result<_>>()?;
        let r = Self {
            id: text(v, "id")?,
            binding: text(v, "binding")?,
            baselined: v
                .get("baselined")
                .and_then(Value::as_bool)
                .ok_or("Feed state: invalid baseline flag")?,
            cursor: text(v, "cursor")?,
            seen,
            revision: num(v, "revision")?,
            baselined_at: num(v, "baselined_at")?,
            last_success_at: num(v, "last_success_at")?,
            successes: num(v, "successes")?,
            new_entries: num(v, "new_entries")?,
        };
        if !valid_id(&r.id)
            || !valid_digest(&r.binding)
            || (!r.cursor.is_empty() && !identity(&r.cursor))
            || r.seen.iter().any(|s| !identity(s))
            || r.seen.iter().collect::<BTreeSet<_>>().len() != r.seen.len()
            || (!r.baselined && (!r.seen.is_empty() || !r.cursor.is_empty()))
        {
            return Err("Feed state: invalid record".into());
        }
        Ok(r)
    }
}

/// The result of comparing one successful poll with the retained window.
#[derive(Debug)]
pub(crate) struct Observation {
    pub(crate) record: Record,
    /// The poll only establishes the baseline: nothing is considered new.
    pub(crate) baseline: bool,
    /// Indexes into the polled entries that were never observed before.
    pub(crate) fresh: Vec<usize>,
    pub(crate) duplicates: u32,
}

/// Pure and order-independent: entries are compared by identity, so reordering
/// and duplicates never change the outcome. Only the first `max_new` unseen
/// entries are considered (and retained); later ones stay new for the next poll.
pub(crate) fn observe(current: &Record, ids: &[&str], at: u64, max_new: usize) -> Observation {
    let baseline = !current.baselined || at.saturating_sub(current.last_success_at) > STALE_SECS;
    let known: BTreeSet<&str> = if baseline {
        BTreeSet::new()
    } else {
        current.seen.iter().map(String::as_str).collect()
    };
    let mut listed = BTreeSet::new();
    let mut unique = Vec::new();
    let mut fresh = Vec::new();
    let mut duplicates = 0_u32;
    for (index, id) in ids.iter().enumerate() {
        if !listed.insert(*id) {
            duplicates = duplicates.saturating_add(1);
            continue;
        }
        unique.push(index);
        if !baseline && !known.contains(id) && fresh.len() < max_new {
            fresh.push(index);
        }
    }
    let mut seen = if baseline {
        Vec::new()
    } else {
        current.seen.clone()
    };
    let retained = if baseline { &unique } else { &fresh };
    // Oldest first: a feed lists its newest entries first.
    for index in retained.iter().rev() {
        seen.push(ids[*index].to_owned());
    }
    if seen.len() > MAX_SEEN {
        let excess = seen.len() - MAX_SEEN;
        seen.drain(..excess);
    }
    let mut record = current.clone();
    record.baselined = true;
    if baseline {
        record.baselined_at = at;
    }
    if let Some(head) = ids.first() {
        record.cursor = (*head).to_owned();
    }
    record.seen = seen;
    record.revision = current.revision.saturating_add(1);
    record.last_success_at = at;
    record.successes = current.successes.saturating_add(1);
    record.new_entries = current.new_entries.saturating_add(fresh.len() as u64);
    Observation {
        record,
        baseline,
        fresh,
        duplicates,
    }
}

pub(crate) struct FeedStore {
    storage: storage::Storage,
    // Configuration, bounded by MAX_WATCHED; historical records remain on disk.
    watched: BTreeMap<String, String>,
    read_only: bool,
}
impl FeedStore {
    pub(crate) fn open(dir: &Path, config: &Config, read_only: bool) -> Result<Self> {
        let mut storage = storage::Storage::open(dir, read_only)?;
        let mut watched = BTreeMap::new();
        // Validate every configured binding before publishing any migration.
        for source in &config.sources {
            let id = source.options.identity(source);
            let binding = source.options.binding(source);
            if storage.row(&id)?.is_some_and(|row| row.binding != binding) {
                return Err("Feed state: source binding changed; use a new stable ID".into());
            }
            if source.options.watch.enabled && watched.insert(id, binding).is_some() {
                return Err("Feed state: duplicate watched source ID".into());
            }
        }
        if watched.len() > MAX_WATCHED {
            return Err("Feed state: too many configured watched sources".into());
        }
        Ok(Self {
            storage,
            watched,
            read_only,
        })
    }
    pub(crate) fn initialize(&mut self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        self.storage.initialize()?;
        // Only baselined sources need reconciliation, not retained history.
        let mut after = None;
        loop {
            let (active, next) = self.storage.active(after)?;
            for record in active {
                if !self.watched.contains_key(&record.id) {
                    self.suspend(&record.id)?;
                }
            }
            after = next;
            if after.is_none() {
                break;
            }
        }
        let configured: Vec<_> = self
            .watched
            .iter()
            .map(|(id, binding)| (id.clone(), binding.clone()))
            .collect();
        for (id, binding) in configured {
            if self.storage.row(&id)?.is_none() {
                self.storage.commit(Record::new(id, binding))?;
            }
        }
        Ok(())
    }
    pub(crate) fn row(&mut self, id: &str) -> Result<Option<Record>> {
        let mut record = self.storage.row(id)?;
        if let Some(row) = &mut record {
            if !self.watched.contains_key(id) && row.baselined {
                row.baselined = false;
                row.cursor.clear();
                row.seen.clear();
                row.revision = row.revision.saturating_add(1);
            }
        } else if let Some(binding) = self.watched.get(id) {
            record = Some(Record::new(id.into(), binding.clone()));
        }
        Ok(record)
    }
    pub(crate) fn commit(&mut self, record: Record) -> Result<()> {
        if self.read_only {
            return Err("Feed state: storage is read-only".into());
        }
        if record.baselined && !self.watched.contains_key(&record.id) {
            return Err(
                "Feed state: only configured watched sources may establish a baseline".into(),
            );
        }
        self.storage.commit(record)
    }
    pub(crate) fn suspend(&mut self, id: &str) -> Result<bool> {
        let Some(mut row) = self.storage.row(id)?.filter(|row| row.baselined) else {
            return Ok(false);
        };
        row.baselined = false;
        row.cursor.clear();
        row.seen.clear();
        row.revision = row.revision.saturating_add(1);
        self.commit(row)?;
        Ok(true)
    }
    pub(crate) fn pending(&mut self, id: &str) -> Result<Option<generation::Generation>> {
        self.storage.pending(id)
    }
    pub(crate) fn pending_header(&mut self, id: &str) -> Result<Option<generation::Header>> {
        self.storage.pending_header(id)
    }
    pub(crate) fn capture(
        &mut self,
        record: Record,
        generation: generation::Generation,
    ) -> Result<()> {
        if !self.watched.contains_key(&record.id) {
            return Err("Feed routing: source is not configured for watching".into());
        }
        self.storage.capture(record, generation)
    }
    pub(crate) fn checkpoint(
        &mut self,
        expected: &generation::Header,
        next: Option<&generation::Header>,
    ) -> Result<bool> {
        self.storage.checkpoint(expected, next)
    }
    pub(crate) fn routing_counts(&mut self) -> Result<(u64, u64)> {
        self.storage.routing_counts()
    }
    pub(crate) fn schedule_cursor(&mut self) -> Result<String> {
        self.storage.schedule_cursor()
    }
    pub(crate) fn scheduled(&mut self, id: &str) -> Result<()> {
        self.storage.scheduled(id)
    }
    pub(crate) fn total(&mut self) -> Result<u64> {
        self.storage.total()
    }
    pub(crate) fn needs_migration(&self) -> bool {
        self.storage.needs_migration()
    }
    pub(crate) fn retained(&mut self, after: Option<&str>, limit: usize) -> Result<Value> {
        let page = self
            .storage
            .page(after.map(storage::parse_cursor).transpose()?, limit)?;
        let mut rows = Vec::new();
        for row in page.rows {
            let mut value = Value::object();
            value.insert("id", row.id.clone());
            value.insert("configured_watched", self.watched.contains_key(&row.id));
            value.insert("baselined", row.baselined);
            value.insert("last_success_at", row.last_success_at.to_string());
            value.insert("successes", row.successes.to_string());
            value.insert("retained_identities", row.seen.len() as u32);
            value.insert("cursor_set", !row.cursor.is_empty());
            rows.push(value);
        }
        let mut value = Value::object();
        value.insert("sources", Value::Array(rows));
        value.insert(
            "next",
            page.next
                .map_or(Value::Null, |key| Value::String(storage::cursor(&key))),
        );
        value.insert("total", page.total.to_string());
        value.insert("sequence", page.sequence.to_string());
        value.insert("needs_migration", self.needs_migration());
        Ok(value)
    }
}

pub(crate) fn retained_query(text: &str) -> Result<(Option<String>, usize)> {
    if text.len() > 256 {
        return Err("Feed state: page query exceeds bounds".into());
    }
    let mut after = None;
    let mut limit = 50;
    let mut fields = BTreeSet::new();
    for pair in text.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair
            .split_once('=')
            .ok_or("Feed state: invalid query field")?;
        if !fields.insert(name) {
            return Err("Feed state: duplicate query field".into());
        }
        match name {
            "after" => {
                storage::parse_cursor(value)?;
                after = Some(value.to_owned());
            }
            "limit" => {
                limit = value
                    .parse::<usize>()
                    .map_err(|_| "Feed state: invalid page limit")?;
                if !(1..=200).contains(&limit) || limit.to_string() != value {
                    return Err("Feed state: page limit must be 1 to 200".into());
                }
            }
            _ => return Err("Feed state: unknown query field".into()),
        }
    }
    Ok((after, limit))
}

/// The stable public identity of a configured source.
pub(crate) fn source_id(source: &Source) -> String {
    source.options.identity(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: usize) -> String {
        format!("{n:032x}")
    }
    fn record() -> Record {
        Record::new("feed".into(), "0".repeat(64))
    }
    fn refs(ids: &[String]) -> Vec<&str> {
        ids.iter().map(String::as_str).collect()
    }

    #[test]
    fn first_poll_baselines_without_considering_anything_new() {
        let ids: Vec<_> = (1..=3).map(id).collect();
        let first = observe(&record(), &refs(&ids), 100, 10);
        assert!(first.baseline);
        assert!(first.fresh.is_empty());
        assert!(first.record.baselined);
        assert_eq!(first.record.baselined_at, 100);
        assert_eq!(first.record.cursor, ids[0]);
        assert_eq!(first.record.seen.len(), 3);
        assert_eq!(first.record.new_entries, 0);
        let again = observe(&first.record, &refs(&ids), 200, 10);
        assert!(!again.baseline);
        assert!(again.fresh.is_empty());
        assert_eq!(again.record.baselined_at, 100);
        assert_eq!(again.record.successes, 2);
    }

    #[test]
    fn reordering_and_duplicates_never_create_new_entries() {
        let ids: Vec<_> = (1..=4).map(id).collect();
        let base = observe(&record(), &refs(&ids), 100, 10).record;
        let reordered = [
            ids[2].clone(),
            ids[0].clone(),
            ids[3].clone(),
            ids[1].clone(),
        ];
        let poll = observe(&base, &refs(&reordered), 200, 10);
        assert!(poll.fresh.is_empty());
        let mut with_new = vec![id(9), id(9)];
        with_new.extend(ids.clone());
        let poll = observe(&base, &refs(&with_new), 200, 10);
        assert_eq!(poll.fresh, vec![0]);
        assert_eq!(poll.duplicates, 1);
        assert_eq!(poll.record.cursor, id(9));
        assert_eq!(poll.record.new_entries, 1);
        let next = observe(&poll.record, &refs(&with_new), 300, 10);
        assert!(next.fresh.is_empty());
    }

    #[test]
    fn window_is_bounded_and_unconsidered_entries_stay_new() {
        let ids: Vec<_> = (0..MAX_SEEN + 8).map(id).collect();
        let mut base = observe(&record(), &[], 100, 10).record;
        base.seen = ids[..MAX_SEEN].to_vec();
        let poll = observe(&base, &refs(&ids[MAX_SEEN..]), 200, 3);
        assert_eq!(poll.fresh.len(), 3);
        assert_eq!(poll.record.seen.len(), MAX_SEEN);
        assert_eq!(poll.record.seen.last().unwrap(), &ids[MAX_SEEN]);
        assert!(!poll.record.seen.contains(&ids[0]));
        let rest = observe(&poll.record, &refs(&ids[MAX_SEEN..]), 300, 10);
        assert_eq!(rest.fresh.len(), 5);
    }

    #[test]
    fn a_long_gap_baselines_again_instead_of_replaying_history() {
        let ids: Vec<_> = (1..=2).map(id).collect();
        let base = observe(&record(), &refs(&ids), 100, 10).record;
        let later = [id(7), id(1)];
        let poll = observe(&base, &refs(&later), 100 + STALE_SECS + 1, 10);
        assert!(poll.baseline);
        assert!(poll.fresh.is_empty());
        assert_eq!(poll.record.seen.len(), 2);
        let poll = observe(&base, &refs(&later), 100 + STALE_SECS, 10);
        assert!(!poll.baseline);
        assert_eq!(poll.fresh, vec![0]);
    }

    #[test]
    fn records_round_trip_and_reject_inconsistent_state() {
        let ids: Vec<_> = (1..=3).map(id).collect();
        let row = observe(&record(), &refs(&ids), 100, 10).record;
        assert_eq!(Record::from_json(&row.to_json()).unwrap(), row);
        let mut unbaselined = row.clone();
        unbaselined.baselined = false;
        assert!(Record::from_json(&unbaselined.to_json()).is_err());
        let mut duplicate = row.clone();
        duplicate.seen.push(ids[0].clone());
        assert!(Record::from_json(&duplicate.to_json()).is_err());
        let mut invalid = row;
        invalid.cursor = "not-an-identity".into();
        assert!(Record::from_json(&invalid.to_json()).is_err());
    }
}
