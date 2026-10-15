//! Disk-resident ordered records for the inventory migration tracked in #106.
//!
//! This API owns a separate, newly created private directory. It never opens or
//! migrates the legacy request journal. See docs/inventory-storage.md for the
//! commit/recovery protocol and the remaining integration work.

mod persistence;
mod tree;
#[cfg(test)]
pub(crate) use persistence::{Fault, damage_record};

use crate::Result;
use std::{collections::BTreeSet, fs::File, path::PathBuf};
use tree::{Header, Tree};

/// Ordered binary identity. Callers define their own table/range prefixes.
pub type Key = [u8; 64];
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;
pub const MAX_PAGE_ROWS: usize = 200;
pub const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
/// Operation bound; see docs/inventory-storage.md. It is not an inventory bound.
pub const MAX_CHANGES: usize = 16_384;
/// Longest value that can be stored inline in a tree leaf (no record file).
pub const MAX_INLINE_BYTES: usize = tree::MAX_INLINE_BYTES;

#[derive(Debug)]
pub enum Change {
    /// A record file addressed by the key (at most `MAX_VALUE_BYTES`).
    Put(Key, Vec<u8>),
    /// A tiny value (at most `MAX_INLINE_BYTES`) stored inside the tree leaf.
    PutInline(Key, Vec<u8>),
    Delete(Key),
}

/// A live ordered scan. Supply the first page's sequence to reject mutations
/// between pages. `after` is exclusive; `start` is inclusive; `end` exclusive.
#[derive(Clone, Debug)]
pub struct Query {
    pub start: Key,
    pub end: Option<Key>,
    pub after: Option<Key>,
    pub limit: usize,
    pub sequence: Option<u64>,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            start: [0; 64],
            end: None,
            after: None,
            limit: 50,
            sequence: None,
        }
    }
}

#[derive(Debug)]
pub struct Page {
    pub entries: Vec<(Key, Vec<u8>)>,
    /// Exclusive continuation key, present only when another row exists.
    pub next: Option<Key>,
    pub total: u64,
    pub sequence: u64,
}

/// One owner, bounded tree paths and bounded response/transaction buffers.
/// Use `create` for a new directory and `open` for this format only.
pub struct Index {
    directory: PathBuf,
    _lock: File,
    tree: File,
    header: Header,
    cache: tree::Cache,
    read_only: bool,
    poisoned: bool,
    #[cfg(test)]
    fault: Option<persistence::Fault>,
    #[cfg(test)]
    last_page_reads: usize,
    #[cfg(test)]
    requests: u64,
    #[cfg(test)]
    last_staged_nodes: usize,
    /// Writes puts as record files, as releases before the value log did.
    #[cfg(test)]
    legacy_records: bool,
}

impl Index {
    pub fn create(directory: &std::path::Path) -> Result<Self> {
        persistence::create(directory)
    }

    /// Create a new index with an identity already bound by the caller's durable migration proof.
    pub(crate) fn create_bound(directory: &std::path::Path, identity: [u8; 32]) -> Result<Self> {
        persistence::create_bound(directory, identity)
    }
    pub(crate) fn valid_creation_prefix(identity: [u8; 32], bytes: &[u8]) -> bool {
        tree::Header::empty(identity).encode().starts_with(bytes)
    }
    #[cfg(test)]
    pub(crate) fn inject_creation(stage: Option<u8>) {
        persistence::CREATE_STOP.with(|s| s.set(stage));
    }

    pub fn open(directory: &std::path::Path, read_only: bool) -> Result<Self> {
        persistence::open(directory, read_only, None)
    }

    /// Bind a published marker before any writable transaction recovery.
    pub(crate) fn open_bound(
        directory: &std::path::Path,
        read_only: bool,
        identity: [u8; 32],
    ) -> Result<Self> {
        persistence::open(directory, read_only, Some(identity))
    }

    /// The random identity chosen when the store was created.
    pub fn identity(&self) -> [u8; 32] {
        self.header.identity
    }

    /// False after an uncertain commit, until the store is reopened.
    pub fn is_usable(&self) -> bool {
        !self.poisoned
    }

    #[cfg(test)]
    pub(crate) fn last_page_reads(&self) -> usize {
        self.last_page_reads
    }

    #[cfg(test)]
    pub(crate) fn requests(&self) -> u64 {
        self.requests
    }

    #[cfg(test)]
    pub(crate) fn inject(&mut self, fault: Option<Fault>) {
        self.fault = fault;
    }

    fn available(&self) -> Result<()> {
        if self.poisoned {
            Err(
                "Inventory commit outcome is uncertain; reopen storage before reading or writing"
                    .into(),
            )
        } else {
            Ok(())
        }
    }

    pub fn len(&self) -> Result<u64> {
        self.available()?;
        Ok(self.header.count)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    pub fn sequence(&self) -> Result<u64> {
        self.available()?;
        Ok(self.header.sequence)
    }

    pub fn get(&mut self, key: &Key) -> Result<Option<Vec<u8>>> {
        self.available()?;
        #[cfg(test)]
        {
            self.requests += 1;
        }
        let value = Tree::new(&mut self.tree, self.header, &mut self.cache).find(key)?;
        value
            .map(|value| persistence::read_value(&self.directory, &self.header, key, value))
            .transpose()
    }

    pub fn page(&mut self, query: &Query) -> Result<Page> {
        self.available()?;
        #[cfg(test)]
        {
            self.requests += 1;
        }
        if query.limit == 0
            || query.limit > MAX_PAGE_ROWS
            || query.end.is_some_and(|end| end <= query.start)
            || query
                .after
                .is_some_and(|key| key < query.start || query.end.is_some_and(|end| key >= end))
        {
            return Err("Invalid inventory page range, cursor or row limit".into());
        }
        if query
            .sequence
            .is_some_and(|sequence| sequence != self.header.sequence)
        {
            return Err("Inventory changed during the scan; restart from the first page".into());
        }
        let mut tree = Tree::new(&mut self.tree, self.header, &mut self.cache);
        let refs = tree.range(query, query.limit + 1)?;
        #[cfg(test)]
        {
            self.last_page_reads = tree.reads;
        }
        let mut entries = Vec::new();
        let mut bytes = 0usize;
        let mut more = false;
        for (key, value) in refs {
            if entries.len() == query.limit || bytes + value.length as usize > MAX_PAGE_BYTES {
                more = true;
                break;
            }
            let data = persistence::read_value(&self.directory, &self.header, &key, value)?;
            bytes += data.len();
            entries.push((key, data));
        }
        let next = if more {
            entries.last().map(|(key, _)| *key)
        } else {
            None
        };
        Ok(Page {
            entries,
            next,
            total: self.header.count,
            sequence: self.header.sequence,
        })
    }

    /// Puts and deletes are one recoverable commit. A failed commit may have
    /// taken effect; reopen and inspect state, rather than blindly retrying.
    pub fn commit(&mut self, changes: Vec<Change>) -> Result<u64> {
        self.available()?;
        if self.read_only {
            return Err("Inventory storage is read-only".into());
        }
        if changes.is_empty() || changes.len() > MAX_CHANGES {
            return Err("Invalid inventory transaction size".into());
        }
        let mut keys = BTreeSet::new();
        let mut bytes = 0usize;
        for change in &changes {
            let key = match change {
                Change::Put(key, value) | Change::PutInline(key, value) => {
                    let limit = if matches!(change, Change::Put(..)) {
                        MAX_VALUE_BYTES
                    } else {
                        MAX_INLINE_BYTES
                    };
                    if value.len() > limit {
                        return Err(if limit == MAX_VALUE_BYTES {
                            "Inventory value exceeds 1 MiB"
                        } else {
                            "Inline inventory value exceeds 32 bytes"
                        }
                        .into());
                    }
                    bytes = bytes
                        .checked_add(value.len())
                        .ok_or("Inventory transaction size overflow")?;
                    key
                }
                Change::Delete(key) => key,
            };
            if !keys.insert(*key) {
                return Err("Duplicate inventory transaction key".into());
            }
        }
        if bytes > persistence::MAX_WAL_BYTES {
            return Err("Inventory transaction exceeds its byte budget".into());
        }
        let before = self.header;
        let mut log = Log::new(before);
        // Existing values are verified before staging; replacement is not a
        // silent repair of corrupt storage, and preparation changes no files.
        let mut actions = Vec::with_capacity(changes.len());
        for change in changes {
            let (key, data, inline) = match change {
                Change::Put(key, data) => (key, Some(data), false),
                Change::PutInline(key, data) => (key, Some(data), true),
                Change::Delete(key) => (key, None, false),
            };
            let existed = self.get(&key)?.is_some();
            if let Some(old) = Tree::new(&mut self.tree, before, &mut self.cache).find(&key)? {
                log.release(old)?;
            }
            persistence::check_value_path(&self.directory, &key)?;
            #[cfg(test)]
            let legacy = self.legacy_records;
            #[cfg(not(test))]
            let legacy = false;
            let segment = match &data {
                Some(data) if !inline && !legacy => Some(log.append(data)?),
                _ => None,
            };
            actions.push(persistence::Action {
                key,
                data,
                inline,
                existed,
                segment,
            });
        }
        let mut staged = Tree::new(&mut self.tree, before, &mut self.cache);
        for action in &actions {
            staged.change(&action.key, action.reference()?, action.existed)?;
        }
        log.clean(&self.directory, &mut staged, &mut actions)?;
        staged.header.sequence = before
            .sequence
            .checked_add(1)
            .ok_or("Inventory sequence overflow")?;
        log.finish(&mut staged.header)?;
        #[cfg(test)]
        {
            self.last_staged_nodes = staged.dirty.len();
        }
        let transaction = persistence::Transaction {
            before,
            after: staged.header,
            pages: staged.dirty,
            actions,
        };
        let bytes = transaction.encode()?;
        // Applying rewrites pages in place: nothing cached may outlive it.
        self.cache.clear();
        persistence::publish_and_apply(self, &transaction, &bytes)?;
        self.header = transaction.after;
        Ok(self.header.sequence)
    }
}

/// Log garbage tolerated before the cleaner runs, beyond twice the live bytes.
const CLEAN_SLACK: u64 = 8 * 1024 * 1024;
/// Least segment bytes the cleaner reads in one commit; it reads at least as
/// much as the commit appends, so the log cannot outgrow it.
const CLEAN_BUDGET: u64 = 4 * 1024 * 1024;
/// Room kept for one more tree path when deciding whether to relocate a value.
const CLEAN_PAGE_MARGIN: usize = 128;

#[cfg(test)]
thread_local! { pub(crate) static CLEAN_SLACK_OVERRIDE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) }; }

/// Value-log bookkeeping for one commit: where new envelopes go in this
/// commit's segment, the live byte total and the cleaner's progress.
struct Log {
    before: Header,
    number: u64,
    end: u64,
    appended: u64,
    live_bytes: u64,
    tail: u64,
    tail_offset: u64,
    log_bytes: u64,
}

impl Log {
    fn new(before: Header) -> Self {
        Self {
            before,
            number: before.head,
            end: persistence::SEGMENT_HEADER_BYTES,
            appended: 0,
            live_bytes: before.live_bytes,
            tail: before.tail,
            tail_offset: before.tail_offset,
            log_bytes: before.log_bytes,
        }
    }

    fn envelope_bytes(length: usize) -> u64 {
        persistence::ENVELOPE_BYTES + length as u64
    }

    /// Accounts for a value this commit replaces or deletes.
    fn release(&mut self, old: tree::ValueRef) -> Result<()> {
        if old.segment {
            self.live_bytes = self
                .live_bytes
                .checked_sub(Self::envelope_bytes(old.length as usize))
                .ok_or("Inventory live byte count underflow")?;
        }
        Ok(())
    }

    /// Places a value in this commit's segment.
    fn append(&mut self, data: &[u8]) -> Result<(u64, u32)> {
        let offset = u32::try_from(self.end).map_err(|_| "Inventory segment too large")?;
        let size = Self::envelope_bytes(data.len());
        self.end = self
            .end
            .checked_add(size)
            .filter(|end| *end <= u64::from(u32::MAX))
            .ok_or("Inventory segment too large")?;
        self.appended += size;
        self.live_bytes = self
            .live_bytes
            .checked_add(size)
            .ok_or("Inventory live byte count overflow")?;
        Ok((self.number, offset))
    }

    /// Relocates live values out of the oldest segments once garbage exceeds
    /// the live bytes plus slack, reading a bounded slice per commit. A segment
    /// fully read leaves the log once this commit's root is durable.
    fn clean(
        &mut self,
        directory: &std::path::Path,
        staged: &mut Tree,
        actions: &mut Vec<persistence::Action>,
    ) -> Result<()> {
        #[cfg(test)]
        let slack = CLEAN_SLACK_OVERRIDE
            .with(|value| value.get())
            .unwrap_or(CLEAN_SLACK);
        #[cfg(not(test))]
        let slack = CLEAN_SLACK;
        let total = self.log_bytes + self.segment_size();
        if total <= self.live_bytes.saturating_mul(2).saturating_add(slack) {
            return Ok(());
        }
        let budget = CLEAN_BUDGET.max(self.appended);
        let mut read = 0;
        // Same per-item sizes as `Transaction::encode`, kept clear of its limit.
        let page_bytes = (tree::PAGE_SIZE + 8) as u64;
        let mut wal_bytes = actions.iter().try_fold(3 * page_bytes, |total, action| {
            total.checked_add(82 + action.data.as_ref().map_or(0, Vec::len) as u64)
        });
        while self.tail < self.before.head && read < budget {
            let mut segment =
                persistence::Segment::open(directory, self.before.identity, self.tail)?;
            let mut offset = if self.tail_offset == 0 {
                persistence::SEGMENT_HEADER_BYTES
            } else {
                self.tail_offset
            };
            loop {
                let pages = (staged.dirty.len() + CLEAN_PAGE_MARGIN) as u64;
                if actions.len() >= MAX_CHANGES
                    || pages > tree::MAX_DIRTY_PAGES as u64
                    || wal_bytes
                        .and_then(|bytes| bytes.checked_add(pages * page_bytes))
                        .is_none_or(|bytes| {
                            bytes + 82 + MAX_VALUE_BYTES as u64 > persistence::MAX_WAL_BYTES as u64
                        })
                    || read >= budget
                {
                    self.tail_offset = offset;
                    return Ok(());
                }
                let Some((key, data)) = segment.envelope(offset)? else {
                    break;
                };
                let size = Self::envelope_bytes(data.len());
                let current = tree::ValueRef::segment(self.tail, offset as u32, &data);
                if staged.find(&key)? == Some(current) {
                    self.release(current)?;
                    let location = self.append(&data)?;
                    let action = persistence::Action {
                        key,
                        data: Some(data),
                        inline: false,
                        existed: true,
                        segment: Some(location),
                    };
                    staged.change(&key, action.reference()?, true)?;
                    wal_bytes = wal_bytes.and_then(|bytes| bytes.checked_add(82 + size));
                    actions.push(action);
                }
                offset += size;
                read += size;
            }
            self.log_bytes = self
                .log_bytes
                .checked_sub(segment.length)
                .ok_or("Inventory log byte count underflow")?;
            read += persistence::SEGMENT_HEADER_BYTES;
            self.tail += 1;
            self.tail_offset = 0;
        }
        Ok(())
    }

    fn segment_size(&self) -> u64 {
        if self.end > persistence::SEGMENT_HEADER_BYTES {
            self.end
        } else {
            0
        }
    }

    fn finish(&self, header: &mut Header) -> Result<()> {
        let size = self.segment_size();
        header.head = if size > 0 {
            self.number
                .checked_add(1)
                .ok_or("Inventory segment number overflow")?
        } else {
            self.number
        };
        header.tail = self.tail;
        header.tail_offset = if self.tail == header.head {
            0
        } else {
            self.tail_offset
        };
        header.log_bytes = self
            .log_bytes
            .checked_add(size)
            .ok_or("Inventory log byte count overflow")?;
        header.live_bytes = self.live_bytes;
        Ok(())
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        // A concurrently spawned child can briefly inherit the locked open
        // description before exec closes it. Release our ownership explicitly
        // so closing this index never waits for that child's descriptor.
        let _ = self._lock.unlock();
    }
}

#[cfg(test)]
mod flush_tests;
#[cfg(test)]
mod log_tests;
#[cfg(test)]
mod tests;
