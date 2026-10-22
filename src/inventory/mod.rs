//! Disk-resident ordered records for the inventory migration tracked in #106.
//!
//! This API owns a separate, newly created private directory. It never opens or
//! migrates the legacy request journal. See docs/inventory-storage.md for the
//! commit/recovery protocol and the remaining integration work.

pub(crate) mod backlog;
mod persistence;
mod tree;
#[cfg(test)]
pub(crate) use persistence::{Fault, damage_record};
#[cfg(test)]
pub(crate) use tests::legacy_range_index;

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

/// Exact cardinality and first key of a half-open range. Values are not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeSummary {
    pub count: u64,
    pub first: Option<Key>,
}

/// One owner, bounded tree paths and bounded response/transaction buffers.
/// Use `create` for a new directory and `open` for this format only.
pub struct Index {
    directory: PathBuf,
    /// English store name for operator errors, derived from the directory.
    name: &'static str,
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
            Err(format!(
                "{}: an earlier change failed, so reads and writes are paused; {RECOVER}",
                self.name
            ))
        } else {
            Ok(())
        }
    }

    /// Size and state from the durable root alone: no tree walk or scan.
    pub fn stats(&self) -> Stats {
        Stats {
            entries: self.header.count,
            index_bytes: tree::offset(self.header.next).unwrap_or(0),
            log_bytes: self.header.log_bytes,
            live_bytes: self.header.live_bytes,
            sequence: self.header.sequence,
            uncertain: self.poisoned,
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

    /// Exact [start, end) count and first key, in at most three tree paths.
    /// None means an unbounded end. Legacy branches require the explicit
    /// `prepare_range_counts` upgrade; this read never migrates or scans.
    pub fn range_summary(&mut self, start: Key, end: Option<Key>) -> Result<RangeSummary> {
        self.available()?;
        #[cfg(test)]
        {
            self.requests += 1;
        }
        let mut tree = Tree::new(&mut self.tree, self.header, &mut self.cache);
        let result = tree.summary(start, end);
        #[cfg(test)]
        {
            self.last_page_reads = tree.reads;
        }
        result
    }

    /// Explicit resumable upgrade of legacy branch counts. Returns true when
    /// complete. Each call visits at most (budget + height) * 32 children and
    /// commits at most budget + height pages through the ordinary WAL, with
    /// no record changes. Open/read-only paths never call this automatically.
    /// Keep a stopped-store backup for downgrade before starting the upgrade.
    pub fn prepare_range_counts(&mut self, budget: usize) -> Result<bool> {
        self.available()?;
        if self.read_only {
            return Err(format!(
                "{} is open read-only; range counts were not upgraded",
                self.name
            ));
        }
        if !(1..=256).contains(&budget) {
            return Err("Inventory range-count upgrade budget must be 1..=256".into());
        }
        let mut tree = Tree::new(&mut self.tree, self.header, &mut self.cache);
        let complete = tree.prepare_counts(budget)?;
        #[cfg(test)]
        {
            self.last_page_reads = tree.reads;
            self.last_staged_nodes = tree.dirty.len();
        }
        if tree.dirty.is_empty() {
            return Ok(complete);
        }
        tree.header.sequence = self
            .header
            .sequence
            .checked_add(1)
            .ok_or("Inventory sequence overflow")?;
        let transaction = persistence::Transaction {
            before: self.header,
            after: tree.header,
            pages: tree.dirty,
            actions: Vec::new(),
        };
        let bytes = transaction.encode()?;
        self.cache.clear();
        persistence::publish_and_apply(self, &transaction, &bytes)
            .map_err(|failure| failure.explain(self.name))?;
        self.header = transaction.after;
        Ok(complete)
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
    /// Every error names the store and says whether the change was saved.
    pub fn commit(&mut self, changes: Vec<Change>) -> Result<u64> {
        self.available()?;
        let name = self.name;
        if self.read_only {
            return Err(format!(
                "{name} is open read-only; the change was not saved"
            ));
        }
        if changes.is_empty() {
            return Err(format!("{name}: empty change; nothing was saved"));
        }
        if changes.len() > MAX_CHANGES {
            return Err(format!(
                "{name}: one change touches more than {MAX_CHANGES} entries, its operation bound \
                 (not an inventory limit); it was not saved and existing records are unchanged. {REPORT}"
            ));
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
                            format!(
                                "{name}: a single record is larger than 1 MiB; it was not saved \
                                 and existing records are unchanged. {REPORT}"
                            )
                        } else {
                            format!("{name}: inline value exceeds 32 bytes; nothing was saved")
                        });
                    }
                    bytes = bytes.saturating_add(value.len());
                    key
                }
                Change::Delete(key) => key,
            };
            if !keys.insert(*key) {
                return Err(format!(
                    "{name}: duplicate key in one change; nothing was saved"
                ));
            }
        }
        // Values alone may exceed the bound; with WAL pages and framing,
        // staging finds the rest. Both are the same operator error.
        let too_large = || {
            format!(
                "{name}: one change needs more than 128 MiB of write-ahead log, its operation \
                 bound (not an inventory limit); it was not saved and existing records are \
                 unchanged. {REPORT}"
            )
        };
        if bytes > persistence::MAX_WAL_BYTES {
            return Err(too_large());
        }
        let (transaction, bytes) = self.stage(changes).map_err(|error| {
            if error == persistence::WAL_BUDGET {
                too_large()
            } else {
                format!("{name}: the change was not saved: {error}")
            }
        })?;
        // Applying rewrites pages in place: nothing cached may outlive it.
        self.cache.clear();
        persistence::publish_and_apply(self, &transaction, &bytes)
            .map_err(|failure| failure.explain(name))?;
        self.header = transaction.after;
        Ok(self.header.sequence)
    }

    /// Prepares a validated change without touching any file.
    fn stage(&mut self, changes: Vec<Change>) -> Result<(persistence::Transaction, Vec<u8>)> {
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
        Ok((transaction, bytes))
    }
}

/// Operator action after an uncertain commit or interrupted recovery.
pub(crate) const RECOVER: &str = "existing records and recovery files are kept; \
restart Mynou to finish recovery and do not delete files in the data directory";
/// Operator action after a change refused by an operation bound.
const REPORT: &str = "Report the item that produced it; Mynou cannot store it";

/// Durable size and state of one index, read from its root in constant time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    /// Index entries, including the store's own secondary keys.
    pub entries: u64,
    /// Bytes of the tree file.
    pub index_bytes: u64,
    /// Bytes of value-log segments, including garbage awaiting cleaning.
    pub log_bytes: u64,
    /// Value-log bytes still referenced by the tree.
    pub live_bytes: u64,
    pub sequence: u64,
    /// A commit failed with an uncertain outcome; recovery runs on reopen.
    pub uncertain: bool,
}

impl Stats {
    pub fn to_value(self) -> crate::json::Value {
        let mut value = crate::json::Value::object();
        let number = |n: u64| crate::json::Value::Number(n as f64);
        value.insert("entries", number(self.entries));
        value.insert("index_bytes", number(self.index_bytes));
        value.insert("log_bytes", number(self.log_bytes));
        value.insert("live_bytes", number(self.live_bytes));
        value.insert("sequence", number(self.sequence));
        value.insert("commit_uncertain", self.uncertain);
        value
    }
}

/// Storage report for status: the backend state and, when an index is open,
/// its root statistics. Never reads records, so it cannot fail.
pub(crate) fn report(state: &str, index: Option<&Index>) -> crate::json::Value {
    let mut value = index.map_or_else(crate::json::Value::object, |index| index.stats().to_value());
    value.insert("state", state);
    value
}

/// English name of the store that owns an index directory, for errors.
/// Directories are `<store>.idx`, optionally with a migration suffix.
pub(crate) fn store_name(directory: &std::path::Path) -> &'static str {
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match name.split_once(".idx").map_or(name, |(store, _)| store) {
        "jobs" => "Job storage",
        "series" => "Series storage",
        "requesters" => "Requester storage",
        "announcements" => "IRC announcement storage",
        "indexers" => "Source policy storage",
        "feeds" => "Feed storage",
        "transfers" => "Transfer storage",
        "profiles" => "Title profile storage",
        _ => "Inventory storage",
    }
}

/// Log garbage tolerated before the cleaner runs, beyond twice the live bytes.
const CLEAN_SLACK: u64 = 8 * 1024 * 1024;
/// Least segment bytes the cleaner reads in one commit; it reads at least as
/// much as the commit appends, so the log cannot outgrow it.
const CLEAN_BUDGET: u64 = 4 * 1024 * 1024;
/// Bounds segment metadata checks and removals in a commit and its replay.
const MAX_RETIRED_SEGMENTS: u64 = 256;
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
        while self.tail < self.before.head
            && self.tail - self.before.tail < MAX_RETIRED_SEGMENTS
            && read < budget
        {
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
