//! Private disk-resident source policies. Legacy maps exist only while a bounded
//! old snapshot is being inspected or migrated, never as the runtime inventory.
use super::{MAGIC, MAX_BYTES, Record, digest};
use crate::{
    Result,
    crypto::{random_bytes, sha256},
    inventory::{Change, Index, Key},
    json,
    store::{private_options, reject_symlinks, sync_directory, sync_file},
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_ROWS: usize = 1000;
const MARKER: &[u8; 8] = b"MYNOUS02";
const FORMAT: &[u8] = b"mynou-indexers/1";
const HEADER: Key = single(0);
const COUNTS: Key = single(1);
const SOURCE: Key = single(2);
const CURSOR: Key = single(3);
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

struct Legacy {
    bytes: Vec<u8>,
    rows: BTreeMap<String, Record>,
}
enum Backend {
    Absent,
    Legacy(Legacy),
    Unpublished(Box<Index>, Option<Legacy>),
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

fn read(path: &Path) -> Result<Vec<u8>> {
    reject_symlinks(path)?;
    let meta = fs::symlink_metadata(path).map_err(|_| "Indexer policy: cannot inspect snapshot")?;
    if !meta.is_file() || meta.len() < 48 || meta.len() > (MAX_BYTES + 48) as u64 {
        return Err("Indexer policy: invalid snapshot type or size".into());
    }
    use std::os::unix::fs::MetadataExt;
    if meta.nlink() != 1 || meta.mode() & 0o077 != 0 {
        return Err("Indexer policy: snapshot must be private with one hard link".into());
    }
    let file = private_options()
        .read(true)
        .open(path)
        .map_err(|_| "Indexer policy: cannot open snapshot")?;
    let opened = file
        .metadata()
        .map_err(|_| "Indexer policy: cannot inspect opened snapshot")?;
    if opened.dev() != meta.dev()
        || opened.ino() != meta.ino()
        || opened.len() != meta.len()
        || opened.nlink() != 1
        || opened.mode() & 0o077 != 0
    {
        return Err("Indexer policy: snapshot changed during opening".into());
    }
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 49) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Indexer policy: cannot read snapshot")?;
    if bytes.len() < 48
        || bytes.len() > MAX_BYTES + 48
        || u64::from_le_bytes(
            bytes[8..16]
                .try_into()
                .map_err(|_| "Indexer policy: invalid length")?,
        ) != (bytes.len() - 48) as u64
        || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
    {
        return Err("Indexer policy: corrupt snapshot".into());
    }
    Ok(bytes)
}

fn legacy(bytes: Vec<u8>) -> Result<Legacy> {
    if &bytes[..8] != MAGIC {
        return Err("Indexer policy: unsupported snapshot".into());
    }
    let value = json::parse(
        std::str::from_utf8(&bytes[16..bytes.len() - 32])
            .map_err(|_| "Indexer policy: invalid UTF-8")?,
    )?;
    crate::numbering::only(&value, &["records"])?;
    let mut rows = BTreeMap::new();
    for value in value
        .get("records")
        .and_then(json::Value::as_array)
        .filter(|rows| rows.len() <= MAX_ROWS)
        .ok_or("Indexer policy: invalid legacy collection")?
    {
        let record = Record::from_json(value)?;
        if rows.insert(record.id.clone(), record).is_some() {
            return Err("Indexer policy: duplicate legacy record".into());
        }
    }
    Ok(Legacy { bytes, rows })
}

fn frame(magic: &[u8; 8], body: &[u8]) -> Vec<u8> {
    let mut bytes = magic.to_vec();
    bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(&sha256(&bytes));
    bytes
}

fn replace(directory: &Path, path: &Path, bytes: &[u8], preserved: bool) -> Result<()> {
    if exists(path)? && preserved {
        if read(path)? == bytes {
            return Ok(());
        }
        return Err("Indexer policy: a preserved copy has different bytes".into());
    }
    let temporary = directory.join(format!(".indexers-{}.tmp", digest(&random_bytes::<16>()?)));
    let mut file = private_options()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| crate::store::migration_io("Source policy storage", "write", &error))?;
    file.write_all(bytes)
        .and_then(|()| sync_file(&file))
        .map_err(|error| crate::store::migration_io("Source policy storage", "write", &error))?;
    reject_symlinks(path)?;
    fs::rename(&temporary, path)
        .map_err(|error| crate::store::migration_io("Source policy storage", "write", &error))?;
    sync_directory(directory)
        .map_err(|error| crate::store::migration_sync("Source policy storage", &error))
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
    let cursor = u64::from(index.get(&CURSOR)?.is_some());
    let expected = total(index)?
        .checked_add(3)
        .and_then(|n| n.checked_add(cursor))
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

fn put(index: &mut Index, records: &[Record], cursor: Option<&str>) -> Result<()> {
    if records.len() > 200 {
        return Err("Indexer policy: transaction batch exceeds 200 records".into());
    }
    let mut count = total(index)?;
    let mut changes = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for record in records {
        if !seen.insert(&record.id) {
            return Err("Indexer policy: duplicate changed record".into());
        }
        Record::from_json(&record.to_json())?;
        if let Some(previous) = get(index, &record.id)? {
            if previous.binding != record.binding {
                return Err("Indexer policy: source binding is immutable".into());
            }
        } else {
            count = count
                .checked_add(1)
                .ok_or("Indexer policy: retained count overflow")?;
        }
        changes.push(Change::Put(
            key(PRIMARY, &record.id),
            json::stringify(&record.to_json()).into_bytes(),
        ));
    }
    changes.push(Change::PutInline(COUNTS, count.to_le_bytes().to_vec()));
    if let Some(cursor) = cursor {
        changes.push(Change::Put(CURSOR, cursor.as_bytes().to_vec()));
    }
    index.commit(changes)?;
    Ok(())
}
fn verify(index: &mut Index, original: Option<&Legacy>) -> Result<()> {
    check(index)?;
    let source = sha256(original.map_or(&[][..], |image| image.bytes.as_slice()));
    if index.get(&SOURCE)?.as_deref() != Some(&source[..]) {
        return Err("Indexer policy: index belongs to different legacy bytes".into());
    }
    let rows = original.map(|image| &image.rows);
    let total = rows.map_or(0, |rows| rows.len() as u64);
    if self::total(index)? != total {
        return Err("Indexer policy: migration count disagrees".into());
    }
    if let Some(rows) = rows {
        for record in rows.values() {
            if get(index, &record.id)?.as_ref() != Some(record) {
                return Err("Indexer policy: migration record differs from legacy state".into());
            }
        }
    }
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
            Backend::Legacy(_) => crate::inventory::report("legacy", None),
            Backend::Unpublished(index, _) => crate::inventory::report("migrating", Some(index)),
            Backend::Ready(index) => crate::inventory::report("index", Some(index)),
        }
    }
    pub(super) fn open(directory: &Path, read_only: bool) -> Result<Self> {
        let path = directory.join("indexers.bin");
        let indexed = exists(&directory.join("indexers.idx"))?;
        let image = if exists(&path)? {
            Some(read(&path)?)
        } else {
            None
        };
        let backend = match image {
            Some(bytes) if &bytes[..8] == MARKER => {
                if bytes.len() != 80 || !indexed {
                    return Err("Indexer policy: missing or invalid indexed storage".into());
                }
                let identity = bytes[16..48]
                    .try_into()
                    .map_err(|_| "Indexer policy: invalid marker identity")?;
                let mut index =
                    Index::open_bound(&directory.join("indexers.idx"), read_only, identity)?;
                check(&mut index)?;
                if index.get(&CURSOR)?.is_some() {
                    return Err("Indexer policy: published index retains a migration cursor".into());
                }
                Backend::Ready(Box::new(index))
            }
            image => {
                let original = image.map(legacy).transpose()?;
                if indexed {
                    // Prove an unmarked promoted index before allowing WAL recovery.
                    let mut index = Index::open(&directory.join("indexers.idx"), true)?;
                    verify(&mut index, original.as_ref())?;
                    if !read_only {
                        drop(index);
                        index = Index::open(&directory.join("indexers.idx"), false)?;
                        verify(&mut index, original.as_ref())?;
                    }
                    Backend::Unpublished(Box::new(index), original)
                } else {
                    match original {
                        Some(image) => Backend::Legacy(image),
                        None => Backend::Absent,
                    }
                }
            }
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
                "Indexer policy: migration durability is uncertain; writable restart required"
                    .into(),
            );
        }
        Ok(())
    }

    pub(super) fn row(&mut self, id: &str) -> Result<Option<Record>> {
        self.available()?;
        match &mut self.backend {
            Backend::Ready(index) | Backend::Unpublished(index, _) => get(index, id),
            Backend::Legacy(image) => Ok(image.rows.get(id).cloned()),
            Backend::Absent => Ok(None),
        }
    }
    #[cfg(test)]
    fn total(&mut self) -> Result<u64> {
        self.available()?;
        match &mut self.backend {
            Backend::Ready(index) | Backend::Unpublished(index, _) => total(index),
            Backend::Legacy(image) => Ok(image.rows.len() as u64),
            Backend::Absent => Ok(0),
        }
    }
    #[cfg(test)]
    fn needs_migration(&self) -> bool {
        !matches!(self.backend, Backend::Ready(_))
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
        if self.read_only {
            return Ok(());
        }
        if matches!(self.backend, Backend::Ready(_)) {
            return Ok(());
        }
        if !matches!(self.backend, Backend::Unpublished(..)) {
            self.migrate()?;
        }
        let Backend::Unpublished(index, original) = &mut self.backend else {
            return Err("Indexer policy: incomplete migration".into());
        };
        let snapshot = self.directory.join("indexers.bin");
        match original {
            Some(original) if !exists(&snapshot)? || read(&snapshot)? != original.bytes => {
                return Err("Indexer policy: legacy snapshot changed before publication".into());
            }
            None if exists(&snapshot)? => {
                return Err("Indexer policy: snapshot appeared before publication".into());
            }
            _ => {}
        }
        if let Some(original) = original {
            replace(
                &self.directory,
                &self
                    .directory
                    .join(format!("indexers.legacy-{}.bin", digest(&original.bytes))),
                &original.bytes,
                true,
            )?;
        }
        replace(
            &self.directory,
            &self.directory.join("indexers.bin"),
            &frame(MARKER, &index.identity()),
            false,
        )?;
        let backend = std::mem::replace(&mut self.backend, Backend::Absent);
        let Backend::Unpublished(index, _) = backend else {
            return Err("Indexer policy: incomplete promotion".into());
        };
        self.backend = Backend::Ready(index);
        Ok(())
    }

    fn migrate(&mut self) -> Result<()> {
        let original = match &self.backend {
            Backend::Legacy(image) => Some(image),
            Backend::Absent => None,
            _ => return Err("Indexer policy: migration already published".into()),
        };
        let total = original.map_or(0, |image| image.rows.len() as u64);
        let progress = crate::progress::Tracker::start_if(
            original.is_some(),
            &self.directory,
            "indexers",
            Some(total),
        );
        progress.stage("preserving", 0);
        let source = sha256(original.map_or(&[][..], |image| image.bytes.as_slice()));
        if let Some(image) = original {
            replace(
                &self.directory,
                &self
                    .directory
                    .join(format!("indexers.legacy-{}.bin", digest(&image.bytes))),
                &image.bytes,
                true,
            )?;
        }
        let staging = self.directory.join("indexers.idx.migrating");
        let mut index = if exists(&staging)? {
            let reuse = Index::open(&staging, true).and_then(|mut index| {
                if index.interrupted() {
                    return Err("Indexer policy: interrupted migration stage".into());
                }
                check(&mut index)?;
                if index.get(&SOURCE)?.as_deref() != Some(&source[..]) {
                    return Err("Indexer policy: foreign migration stage".into());
                }
                drop(index);
                Index::open(&staging, false)
            });
            match reuse {
                Ok(index) => {
                    progress.resumed();
                    index
                }
                Err(_) => {
                    // Staging is derived and has never acknowledged a runtime control.
                    // Keep all unexpected contents, including interrupted writes.
                    let abandoned = self.directory.join(format!(
                        "indexers.idx.abandoned-{}",
                        digest(&random_bytes::<16>()?)
                    ));
                    fs::rename(&staging, abandoned).map_err(|error| {
                        crate::store::migration_io("Source policy storage", "write", &error)
                    })?;
                    sync_directory(&self.directory).map_err(|error| {
                        crate::store::migration_sync("Source policy storage", &error)
                    })?;
                    create(&staging, source)?
                }
            }
        } else {
            create(&staging, source)?
        };
        let cursor = index
            .get(&CURSOR)?
            .map(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|_| "Indexer policy: invalid migration cursor".to_owned())
            })
            .transpose()?;
        if let Some(cursor) = &cursor
            && original.is_none_or(|image| !image.rows.contains_key(cursor))
        {
            return Err("Indexer policy: cursor is absent from original snapshot".into());
        }
        if let Some(image) = original {
            let mut batch = Vec::new();
            let mut done = 0_u64;
            progress.stage("copying", 0);
            for (id, record) in &image.rows {
                done += 1;
                if cursor.as_ref().is_some_and(|cursor| id <= cursor) {
                    continue;
                }
                batch.push(record.clone());
                if batch.len() == 100 {
                    let last = batch
                        .last()
                        .ok_or("Indexer policy: empty migration batch")?
                        .id
                        .clone();
                    put(&mut index, &batch, Some(&last))?;
                    progress.stage("copying", done);
                    batch.clear();
                }
            }
            if let Some(last) = batch.last() {
                let last = last.id.clone();
                put(&mut index, &batch, Some(&last))?;
            }
        }
        progress.stage("verifying", total);
        verify(&mut index, original)?;
        if index.get(&CURSOR)?.is_some() {
            index.commit(vec![Change::Delete(CURSOR)])?;
        }
        drop(index);
        progress.stage("promoting", total);
        if exists(&self.directory.join("indexers.idx"))? {
            return Err("Indexer policy: index appeared before promotion".into());
        }
        fs::rename(&staging, self.directory.join("indexers.idx")).map_err(|error| {
            crate::store::migration_io("Source policy storage", "write", &error)
        })?;
        sync_directory(&self.directory)
            .map_err(|error| crate::store::migration_sync("Source policy storage", &error))?;
        let index = Index::open(&self.directory.join("indexers.idx"), false)?;
        let backend = std::mem::replace(&mut self.backend, Backend::Absent);
        let original = match backend {
            Backend::Legacy(image) => Some(image),
            Backend::Absent => None,
            _ => return Err("Indexer policy: unexpected migration backend".into()),
        };
        self.backend = Backend::Unpublished(Box::new(index), original);
        progress.finish();
        Ok(())
    }

    pub(super) fn commit(&mut self, record: Record) -> Result<()> {
        self.available()?;
        let result = self.commit_inner(record);
        if result.is_err() && !self.read_only {
            self.poisoned = true;
        }
        result
    }
    fn commit_inner(&mut self, record: Record) -> Result<()> {
        self.available()?;
        if self.read_only {
            return Err("Indexer policy: storage is read-only".into());
        }
        let Backend::Ready(index) = &mut self.backend else {
            return Err("Indexer policy: writable migration is required".into());
        };
        let bytes = read(&self.directory.join("indexers.bin"))?;
        if &bytes[..8] != MARKER || bytes.len() != 80 || bytes[16..48] != index.identity() {
            return Err("Indexer policy: index marker changed; restart required".into());
        }
        put(index, &[record], None)
    }
    pub(super) fn guard(&mut self) -> Result<([u8; 32], u64)> {
        self.available()?;
        match &mut self.backend {
            Backend::Ready(index) => Ok((index.identity(), index.sequence()?)),
            Backend::Legacy(image) => Ok((sha256(&image.bytes), 0)),
            Backend::Absent => Ok((sha256(&[]), 0)),
            Backend::Unpublished(index, _) => Ok((index.identity(), index.sequence()?)),
        }
    }
}

fn create(path: &Path, source: [u8; 32]) -> Result<Index> {
    let mut index = Index::create(path)?;
    index.commit(vec![
        Change::PutInline(HEADER, FORMAT.to_vec()),
        Change::PutInline(COUNTS, 0_u64.to_le_bytes().to_vec()),
        Change::PutInline(SOURCE, source.to_vec()),
    ])?;
    Ok(index)
}

#[cfg(test)]
mod tests;
