//! Private disk-resident feed records (`feeds.idx`).
use super::Record;
use crate::{
    Result,
    crypto::sha256,
    inventory::{Change, Index, Key, Query},
    json,
    store::{reject_symlinks, sync_directory},
};
use std::{
    fs,
    path::{Path, PathBuf},
};

const FORMAT: &[u8] = b"mynou-feeds/3";
const INDEX: &str = "feeds.idx";
/// Where a new index is built before it takes its name.
const STAGING: &str = "feeds.idx.creating";
const ROUTING: Key = single(4);
const SCHEDULE: Key = single(5);
mod routing;
const HEADER: Key = single(0);
const COUNTS: Key = single(1);
const PRIMARY: u8 = 0x10;
const ACTIVE: u8 = 0x11;

const fn single(table: u8) -> Key {
    let mut key = [0; 64];
    key[0] = table;
    key
}
fn key(table: u8, id: &str) -> Key {
    let mut key = single(table);
    key[1..33].copy_from_slice(&sha256(id.as_bytes()));
    key
}
fn range(table: u8) -> (Key, Option<Key>) {
    (single(table), Some(single(table + 1)))
}

enum Backend {
    /// Nothing stored yet; a writable start creates the index.
    Absent,
    Ready(Box<Index>),
}
pub(super) struct Storage {
    directory: PathBuf,
    backend: Backend,
    read_only: bool,
    poisoned: bool,
}
pub(super) struct Page {
    pub rows: Vec<Record>,
    pub next: Option<Key>,
    pub total: u64,
    pub sequence: u64,
}
pub(super) fn cursor(key: &Key) -> String {
    key[1..33]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
pub(super) fn parse_cursor(text: &str) -> Result<Key> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err("Feed state: invalid page cursor".into());
    }
    let mut key = single(PRIMARY);
    for (slot, pair) in key[1..33]
        .iter_mut()
        .zip(text.as_bytes().as_chunks::<2>().0)
    {
        let nibble = |byte: u8| {
            if byte <= b'9' {
                byte - b'0'
            } else {
                byte - b'a' + 10
            }
        };
        *slot = nibble(pair[0]) << 4 | nibble(pair[1]);
    }
    Ok(key)
}

fn exists(path: &Path) -> Result<bool> {
    reject_symlinks(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Feed state: cannot inspect storage path".into()),
    }
}

fn counters(index: &mut Index) -> Result<(u64, u64)> {
    let bytes = index
        .get(&COUNTS)?
        .ok_or("Feed state: missing index counts")?;
    if bytes.len() != 16 {
        return Err("Feed state: invalid index counts".into());
    }
    let total = u64::from_le_bytes(
        bytes[..8]
            .try_into()
            .map_err(|_| "Feed state: invalid total")?,
    );
    let active = u64::from_le_bytes(
        bytes[8..]
            .try_into()
            .map_err(|_| "Feed state: invalid active count")?,
    );
    if active > total {
        return Err("Feed state: active count exceeds retained count".into());
    }
    Ok((total, active))
}
fn encode_counts(total: u64, active: u64) -> Vec<u8> {
    let mut bytes = total.to_le_bytes().to_vec();
    bytes.extend_from_slice(&active.to_le_bytes());
    bytes
}
fn check(index: &mut Index) -> Result<()> {
    let format = index
        .get(&HEADER)?
        .ok_or("Feed state: missing index format")?;
    if format != FORMAT {
        return Err("Feed state: unsupported index format".into());
    }
    let (total, active) = counters(index)?;
    let (generations, entries) = routing::counts(index)?;
    routing::schedule(index)?;
    // Format, counts, routing counts and schedule, plus the last pass once run.
    let metadata = 4 + u64::from(crate::inventory::backlog::last(index)?.is_some());
    let expected = metadata
        .checked_add(total)
        .and_then(|count| count.checked_add(active))
        .and_then(|count| count.checked_add(generations))
        .and_then(|count| count.checked_add(entries))
        .ok_or("Feed state: index cardinality overflow")?;
    if index.len()? != expected {
        return Err("Feed state: index cardinality disagrees with counts".into());
    }
    Ok(())
}
fn get(index: &mut Index, id: &str) -> Result<Option<Record>> {
    let Some(bytes) = index.get(&key(PRIMARY, id))? else {
        return Ok(None);
    };
    let record = Record::from_json(&json::parse(
        std::str::from_utf8(&bytes).map_err(|_| "Feed state: invalid record bytes")?,
    )?)?;
    if record.id != id {
        return Err("Feed state: primary identity collision or corruption".into());
    }
    Ok(Some(record))
}

fn put_with(index: &mut Index, records: &[Record], extra: Vec<Change>) -> Result<()> {
    if records.len() > 200 {
        return Err("Feed state: transaction batch exceeds 200 records".into());
    }
    let (mut total, mut active) = counters(index)?;
    let mut changes = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for record in records {
        if !seen.insert(&record.id) {
            return Err("Feed state: duplicate changed record".into());
        }
        let bytes = json::stringify(&record.to_json()).into_bytes();
        Record::from_json(&record.to_json())?;
        let previous = get(index, &record.id)?;
        if let Some(previous) = &previous {
            if previous.binding != record.binding {
                return Err("Feed state: source binding is immutable".into());
            }
        } else {
            total = total
                .checked_add(1)
                .ok_or("Feed state: retained count overflow")?;
        }
        let reference = index.get(&key(ACTIVE, &record.id))?;
        let expected = previous
            .as_ref()
            .filter(|row| row.baselined)
            .map(|row| row.id.as_bytes());
        if reference.as_deref() != expected {
            return Err("Feed state: previous active reference disagrees with record".into());
        }
        if previous.as_ref().is_some_and(|record| record.baselined) {
            active = active
                .checked_sub(1)
                .ok_or("Feed state: active count underflow")?;
        }
        if record.baselined {
            active = active
                .checked_add(1)
                .ok_or("Feed state: active count overflow")?;
            changes.push(Change::Put(
                key(ACTIVE, &record.id),
                record.id.as_bytes().to_vec(),
            ));
        } else {
            changes.push(Change::Delete(key(ACTIVE, &record.id)));
        }
        changes.push(Change::Put(key(PRIMARY, &record.id), bytes));
    }
    changes.push(Change::PutInline(COUNTS, encode_counts(total, active)));
    changes.extend(extra);
    index.commit(changes)?;
    Ok(())
}

impl Storage {
    /// Index size and state for diagnostics, from the durable root only.
    pub(super) fn report(&self) -> crate::json::Value {
        match &self.backend {
            Backend::Absent => crate::inventory::report("empty", None),
            Backend::Ready(index) => crate::inventory::report("index", Some(index)),
        }
    }
    pub(super) fn last_pass(&mut self) -> Result<Option<u64>> {
        self.available()?;
        match &mut self.backend {
            Backend::Ready(index) => crate::inventory::backlog::last(index),
            Backend::Absent => Ok(None),
        }
    }
    pub(super) fn finish_pass(&mut self, at: u64) -> Result<()> {
        let index = self.writable_index()?;
        crate::inventory::backlog::finish(index, at)
    }
    pub(super) fn open(directory: &Path, read_only: bool) -> Result<Self> {
        let path = directory.join(INDEX);
        let backend = if exists(&path)? {
            let mut index = Index::open(&path, read_only)?;
            check(&mut index)?;
            Backend::Ready(Box::new(index))
        } else {
            Backend::Absent
        };
        Ok(Self {
            directory: directory.to_owned(),
            backend,
            read_only,
            poisoned: false,
        })
    }

    fn available(&self) -> Result<()> {
        if self.poisoned {
            return Err("Feed state: durability is uncertain; writable restart required".into());
        }
        Ok(())
    }

    pub(super) fn row(&mut self, id: &str) -> Result<Option<Record>> {
        self.available()?;
        match &mut self.backend {
            Backend::Ready(index) => get(index, id),
            Backend::Absent => Ok(None),
        }
    }
    pub(super) fn total(&mut self) -> Result<u64> {
        self.available()?;
        match &mut self.backend {
            Backend::Ready(index) => Ok(counters(index)?.0),
            Backend::Absent => Ok(0),
        }
    }
    pub(super) fn page(&mut self, after: Option<Key>, limit: usize) -> Result<Page> {
        self.available()?;
        if !(1..=200).contains(&limit) {
            return Err("Feed state: page limit must be 1 to 200".into());
        }
        if after.is_some_and(|key| key[0] != PRIMARY || key[33..].iter().any(|byte| *byte != 0)) {
            return Err("Feed state: invalid page cursor".into());
        }
        match &mut self.backend {
            Backend::Ready(index) => {
                let (start, end) = range(PRIMARY);
                let page = index.page(&Query {
                    start,
                    end,
                    after,
                    limit,
                    sequence: None,
                })?;
                let mut rows = Vec::new();
                for (found, bytes) in page.entries {
                    let record = Record::from_json(&json::parse(
                        std::str::from_utf8(&bytes)
                            .map_err(|_| "Feed state: invalid page record")?,
                    )?)?;
                    if found != key(PRIMARY, &record.id) {
                        return Err("Feed state: page identity differs from its key".into());
                    }
                    rows.push(record);
                }
                Ok(Page {
                    rows,
                    next: page.next,
                    total: counters(index)?.0,
                    sequence: index.sequence()?,
                })
            }
            Backend::Absent => Ok(Page {
                rows: Vec::new(),
                next: None,
                total: 0,
                sequence: 0,
            }),
        }
    }

    pub(super) fn initialize(&mut self) -> Result<()> {
        self.available()?;
        let result = self.initialize_inner();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn initialize_inner(&mut self) -> Result<()> {
        if self.read_only || matches!(self.backend, Backend::Ready(_)) {
            return Ok(());
        }
        let staging = self.directory.join(STAGING);
        if exists(&staging)? {
            // Only an interrupted creation leaves it, and it held no record.
            fs::remove_dir_all(&staging)
                .map_err(|_| "Feed state: cannot remove an unfinished index")?;
        }
        let mut index = Index::create(&staging)?;
        index.commit(vec![
            Change::PutInline(HEADER, FORMAT.to_vec()),
            Change::PutInline(COUNTS, encode_counts(0, 0)),
            Change::PutInline(ROUTING, encode_counts(0, 0)),
            Change::PutInline(SCHEDULE, Vec::new()),
        ])?;
        drop(index);
        let path = self.directory.join(INDEX);
        fs::rename(&staging, &path)
            .map_err(|error| crate::store::storage_io("Feed storage", "write", &error))?;
        sync_directory(&self.directory)
            .map_err(|error| crate::store::storage_sync("Feed storage", &error))?;
        self.backend = Backend::Ready(Box::new(Index::open(&path, false)?));
        Ok(())
    }

    pub(super) fn commit(&mut self, record: Record) -> Result<()> {
        self.available()?;
        if self.read_only {
            return Err("Feed state: storage is read-only".into());
        }
        let Backend::Ready(index) = &mut self.backend else {
            return Err("Feed state: storage is not initialized".into());
        };
        let extra = if record.baselined {
            if routing::header(index, &record.id)?.is_some() {
                return Err(
                    "Feed routing: pending generation must settle before another observation"
                        .into(),
                );
            }
            Vec::new()
        } else {
            routing::cancel(index, &record.id)?
        };
        put_with(index, &[record], extra)
    }
    pub(super) fn active(&mut self, after: Option<Key>) -> Result<(Vec<Record>, Option<Key>)> {
        self.available()?;
        let Backend::Ready(index) = &mut self.backend else {
            return Ok((Vec::new(), None));
        };
        let (start, end) = range(ACTIVE);
        let page = index.page(&Query {
            start,
            end,
            after,
            limit: 200,
            sequence: None,
        })?;
        let mut rows = Vec::new();
        let mut hydrated = 0;
        let mut last = None;
        for (found, bytes) in page.entries {
            let id =
                std::str::from_utf8(&bytes).map_err(|_| "Feed state: invalid active identity")?;
            let record =
                get(index, id)?.ok_or("Feed state: active reference is missing its record")?;
            if found != key(ACTIVE, id) || !record.baselined {
                return Err("Feed state: inconsistent active reference".into());
            }
            let length = json::stringify(&record.to_json()).len();
            if !rows.is_empty() && hydrated + length > crate::inventory::MAX_PAGE_BYTES {
                return Ok((rows, last));
            }
            hydrated += length;
            last = Some(found);
            rows.push(record);
        }
        Ok((rows, page.next))
    }
}

#[cfg(test)]
mod tests;
