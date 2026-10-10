//! Private checked feed cursors. Only opaque bounded identities are retained,
//! never URLs, titles, credentials or media names.
use super::Source;
use crate::{
    Result,
    config::Config,
    crypto::{random_bytes, sha256},
    json::{self, Value},
    requesters::{digest, valid_digest, valid_id},
    store::{private_options, reject_symlinks, sync_directory},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

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
    rows: BTreeMap<String, Record>,
    path: PathBuf,
    read_only: bool,
    dirty: bool,
    poisoned: bool,
}
impl FeedStore {
    pub(crate) fn open(dir: &Path, config: &Config, read_only: bool) -> Result<Self> {
        let path = dir.join("feeds.bin");
        reject_symlinks(&path).map_err(|_| "Feed state: invalid snapshot path")?;
        let mut rows = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(_) => return Err("Feed state: cannot inspect snapshot".into()),
            Ok(m) => Self::read(&path, &m)?,
        };
        let before = rows.clone();
        let mut watched = BTreeSet::new();
        for source in &config.sources {
            let id = source.options.identity(source);
            let binding = source.options.binding(source);
            if rows.get(&id).is_some_and(|r| r.binding != binding) {
                return Err("Feed state: source binding changed; use a new stable ID".into());
            }
            if source.options.watch.enabled {
                if !watched.insert(id.clone()) {
                    return Err("Feed state: duplicate watched source ID".into());
                }
                rows.entry(id.clone())
                    .or_insert_with(|| Record::new(id, binding));
            }
        }
        // A source that is not watched forgets its window: watching it again
        // starts with a fresh baseline instead of replaying what it published.
        for row in rows.values_mut() {
            if !watched.contains(&row.id) && row.baselined {
                row.baselined = false;
                row.cursor.clear();
                row.seen.clear();
                row.revision = row.revision.saturating_add(1);
            }
        }
        if rows.len() > MAX_ROWS {
            return Err("Feed state: retained source capacity reached".into());
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
    fn read(path: &Path, m: &fs::Metadata) -> Result<BTreeMap<String, Record>> {
        if !m.is_file() || m.len() < 48 || m.len() > MAX_BYTES as u64 + 48 {
            return Err("Feed state: invalid snapshot size".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if m.nlink() != 1 || m.mode() & 0o077 != 0 {
                return Err("Feed state: snapshot must be private with one hard link".into());
            }
        }
        let file = private_options()
            .read(true)
            .open(path)
            .map_err(|_| "Feed state: cannot read snapshot")?;
        let opened = file
            .metadata()
            .map_err(|_| "Feed state: cannot inspect snapshot")?;
        if !opened.is_file() || opened.len() != m.len() {
            return Err("Feed state: snapshot changed during opening".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if opened.nlink() != 1
                || opened.mode() & 0o077 != 0
                || opened.dev() != m.dev()
                || opened.ino() != m.ino()
            {
                return Err("Feed state: snapshot changed during opening".into());
            }
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 49)
            .read_to_end(&mut bytes)
            .map_err(|_| "Feed state: cannot read snapshot")?;
        if bytes.len() < 48
            || bytes.len() > MAX_BYTES + 48
            || &bytes[..8] != MAGIC
            || u64::from_le_bytes(
                bytes[8..16]
                    .try_into()
                    .map_err(|_| "Feed state: invalid length")?,
            ) != bytes.len() as u64 - 48
            || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
        {
            return Err("Feed state: corrupt or unsupported snapshot".into());
        }
        Self::parse(&json::parse(
            std::str::from_utf8(&bytes[16..bytes.len() - 32])
                .map_err(|_| "Feed state: invalid UTF-8")?,
        )?)
    }
    fn parse(v: &Value) -> Result<BTreeMap<String, Record>> {
        crate::numbering::only(v, &["records"])?;
        let mut rows = BTreeMap::new();
        for v in v
            .get("records")
            .and_then(Value::as_array)
            .filter(|a| a.len() <= MAX_ROWS)
            .ok_or("Feed state: invalid collection")?
        {
            let r = Record::from_json(v)?;
            if rows.insert(r.id.clone(), r).is_some() {
                return Err("Feed state: duplicate record".into());
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
    pub(crate) fn initialize(&mut self) -> Result<()> {
        if !self.read_only && self.dirty {
            self.write()?;
        }
        Ok(())
    }
    pub(crate) fn row(&self, id: &str) -> Option<&Record> {
        self.rows.get(id)
    }
    /// Replaces one record durably. Memory changes only after the synchronized
    /// snapshot replaced the previous one.
    pub(crate) fn commit(&mut self, record: Record) -> Result<()> {
        if self.read_only || self.poisoned {
            return Err("Feed state: writable recovery is required".into());
        }
        let id = record.id.clone();
        let previous = self.rows.insert(id.clone(), record);
        if self.rows.len() > MAX_ROWS {
            self.restore(&id, previous);
            return Err("Feed state: retained source capacity reached".into());
        }
        if let Err(error) = self.write() {
            self.restore(&id, previous);
            return Err(error);
        }
        Ok(())
    }
    fn restore(&mut self, id: &str, previous: Option<Record>) {
        match previous {
            Some(record) => {
                self.rows.insert(id.to_owned(), record);
            }
            None => {
                self.rows.remove(id);
            }
        }
    }
    /// Forgets the window of a source that stopped being watched. Returns
    /// whether anything changed.
    pub(crate) fn suspend(&mut self, id: &str) -> Result<bool> {
        let Some(row) = self.rows.get(id).filter(|row| row.baselined) else {
            return Ok(false);
        };
        let mut next = row.clone();
        next.baselined = false;
        next.cursor.clear();
        next.seen.clear();
        next.revision = next.revision.saturating_add(1);
        self.commit(next)?;
        Ok(true)
    }
    fn write(&mut self) -> Result<()> {
        if self.read_only || self.poisoned {
            return Err("Feed state: writable recovery is required".into());
        }
        let v = Self::json(&self.rows);
        let payload = json::stringify(&v).into_bytes();
        if payload.len() > MAX_BYTES {
            return Err("Feed state: snapshot too large".into());
        }
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&sha256(&bytes));
        reject_symlinks(&self.path).map_err(|_| "Feed state: invalid snapshot path")?;
        let temp = self
            .path
            .with_file_name(format!(".feeds-{}.tmp", digest(&random_bytes::<16>()?)));
        let result = (|| -> Result<()> {
            let mut f = private_options()
                .create_new(true)
                .write(true)
                .open(&temp)
                .map_err(|_| "Feed state: cannot create snapshot")?;
            f.write_all(&bytes)
                .and_then(|()| f.sync_all())
                .map_err(|_| "Feed state: cannot persist snapshot")?;
            fs::rename(&temp, &self.path).map_err(|_| "Feed state: cannot replace snapshot")?;
            if sync_directory(self.path.parent().ok_or("Feed state: missing directory")?).is_err() {
                self.poisoned = true;
                return Err("Feed state: durability uncertain; restart required".into());
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        self.dirty = false;
        Ok(())
    }
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
