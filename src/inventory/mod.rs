//! Disk-resident ordered records for the inventory migration tracked in #106.
//!
//! This API owns a separate, newly created private directory. It never opens or
//! migrates the legacy request journal. See docs/inventory-storage.md for the
//! commit/recovery protocol and the remaining integration work.

mod persistence;
mod tree;
#[cfg(test)]
pub(crate) use persistence::Fault;

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
    read_only: bool,
    poisoned: bool,
    #[cfg(test)]
    fault: Option<persistence::Fault>,
    #[cfg(test)]
    last_page_reads: usize,
    #[cfg(test)]
    last_staged_nodes: usize,
}

impl Index {
    pub fn create(directory: &std::path::Path) -> Result<Self> {
        persistence::create(directory)
    }

    pub fn open(directory: &std::path::Path, read_only: bool) -> Result<Self> {
        persistence::open(directory, read_only)
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
        let value = Tree::new(&mut self.tree, self.header).find(key)?;
        value
            .map(|value| persistence::read_value(&self.directory, key, value))
            .transpose()
    }

    pub fn page(&mut self, query: &Query) -> Result<Page> {
        self.available()?;
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
        let mut tree = Tree::new(&mut self.tree, self.header);
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
            let data = persistence::read_value(&self.directory, &key, value)?;
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
            persistence::check_value_path(&self.directory, &key)?;
            actions.push(persistence::Action {
                key,
                data,
                inline,
                existed,
            });
        }
        let mut staged = Tree::new(&mut self.tree, before);
        for action in &actions {
            staged.change(&action.key, action.reference()?, action.existed)?;
        }
        staged.header.sequence = before
            .sequence
            .checked_add(1)
            .ok_or("Inventory sequence overflow")?;
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
        persistence::publish_and_apply(self, &transaction, &bytes)?;
        self.header = transaction.after;
        Ok(self.header.sequence)
    }
}

#[cfg(test)]
mod tests;
