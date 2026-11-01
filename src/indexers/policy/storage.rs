//! Private disk-resident source policies (`indexers.idx`).
use super::Record;
use crate::{
    Result,
    crypto::sha256,
    inventory::{Change, Index, Key},
    json,
    store::{reject_symlinks, sync_directory},
};
use std::{
    fs,
    path::{Path, PathBuf},
};

const FORMAT: &[u8] = b"mynou-indexers/1";
const INDEX: &str = "indexers.idx";
/// Where a new index is built before it takes its name.
const STAGING: &str = "indexers.idx.creating";
const HEADER: Key = single(0);
const COUNTS: Key = single(1);
const PRIMARY: u8 = 0x10;

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
fn exists(path: &Path) -> Result<bool> {
    reject_symlinks(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Indexer policy: cannot inspect storage path".into()),
    }
}

fn total(index: &mut Index) -> Result<u64> {
    let bytes = index
        .get(&COUNTS)?
        .ok_or("Indexer policy: missing index count")?;
    Ok(u64::from_le_bytes(
        bytes
            .try_into()
            .map_err(|_| "Indexer policy: invalid count")?,
    ))
}
fn check(index: &mut Index) -> Result<()> {
    if index.get(&HEADER)?.as_deref() != Some(FORMAT) {
        return Err("Indexer policy: unsupported index format".into());
    }
    let expected = total(index)?
        .checked_add(2)
        .ok_or("Indexer policy: invalid index cardinality")?;
    if index.len()? != expected {
        return Err("Indexer policy: index cardinality disagrees with count".into());
    }
    Ok(())
}
fn get(index: &mut Index, id: &str) -> Result<Option<Record>> {
    let Some(bytes) = index.get(&key(PRIMARY, id))? else {
        return Ok(None);
    };
    let record = Record::from_json(&json::parse(
        std::str::from_utf8(&bytes).map_err(|_| "Indexer policy: invalid record bytes")?,
    )?)?;
    if record.id != id {
        return Err("Indexer policy: primary identity collision or corruption".into());
    }
    Ok(Some(record))
}

fn put(index: &mut Index, record: &Record) -> Result<()> {
    Record::from_json(&record.to_json())?;
    let mut count = total(index)?;
    if let Some(previous) = get(index, &record.id)? {
        if previous.binding != record.binding {
            return Err("Indexer policy: source binding is immutable".into());
        }
    } else {
        count = count
            .checked_add(1)
            .ok_or("Indexer policy: retained count overflow")?;
    }
    index.commit(vec![
        Change::Put(
            key(PRIMARY, &record.id),
            json::stringify(&record.to_json()).into_bytes(),
        ),
        Change::PutInline(COUNTS, count.to_le_bytes().to_vec()),
    ])?;
    Ok(())
}

impl Storage {
    /// Index size and state for diagnostics, from the durable root only.
    pub(super) fn backlog(&self) -> crate::json::Value {
        if self.available().is_err()
            || matches!(&self.backend, Backend::Ready(index) if index.sequence().is_err())
        {
            return crate::inventory::backlog::unavailable();
        }
        let mut value =
            crate::inventory::backlog::inactive("not_applicable", "source_policy_control", None);
        value.insert("due", crate::json::Value::Null);
        value.insert(
            "reason",
            "Source policies are operator controls with no periodic worker",
        );
        value
    }
    pub(super) fn report(&self) -> crate::json::Value {
        match &self.backend {
            Backend::Absent => crate::inventory::report("empty", None),
            Backend::Ready(index) => crate::inventory::report("index", Some(index)),
        }
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
            return Err(
                "Indexer policy: durability is uncertain; writable restart required".into(),
            );
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
    #[cfg(test)]
    fn total(&mut self) -> Result<u64> {
        self.available()?;
        match &mut self.backend {
            Backend::Ready(index) => total(index),
            Backend::Absent => Ok(0),
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
            // Only an interrupted creation leaves it, and it held no policy.
            fs::remove_dir_all(&staging)
                .map_err(|_| "Indexer policy: cannot remove an unfinished index")?;
        }
        let mut index = Index::create(&staging)?;
        index.commit(vec![
            Change::PutInline(HEADER, FORMAT.to_vec()),
            Change::PutInline(COUNTS, 0_u64.to_le_bytes().to_vec()),
        ])?;
        drop(index);
        let path = self.directory.join(INDEX);
        fs::rename(&staging, &path)
            .map_err(|error| crate::store::storage_io("Source policy storage", "write", &error))?;
        sync_directory(&self.directory)
            .map_err(|error| crate::store::storage_sync("Source policy storage", &error))?;
        self.backend = Backend::Ready(Box::new(Index::open(&path, false)?));
        Ok(())
    }

    pub(super) fn commit(&mut self, record: Record) -> Result<()> {
        self.available()?;
        if self.read_only {
            return Err("Indexer policy: storage is read-only".into());
        }
        let Backend::Ready(index) = &mut self.backend else {
            return Err("Indexer policy: storage is not initialized".into());
        };
        let result = put(index, &record);
        if result.is_err() && !index.is_usable() {
            self.poisoned = true;
        }
        result
    }
    pub(super) fn guard(&mut self) -> Result<([u8; 32], u64)> {
        self.available()?;
        match &mut self.backend {
            Backend::Ready(index) => Ok((index.identity(), index.sequence()?)),
            Backend::Absent => Ok(([0; 32], 0)),
        }
    }
}

#[cfg(test)]
mod tests;
