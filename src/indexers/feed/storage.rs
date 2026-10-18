//! Private disk-resident feed records. Legacy maps exist only while a bounded
//! old snapshot is being inspected or migrated, never as the runtime inventory.
use super::{MAGIC, MAX_BYTES, MAX_ROWS, Record};
use crate::{
    Result,
    crypto::{random_bytes, sha256},
    inventory::{Change, Index, Key, Query},
    json,
    store::{private_options, reject_symlinks, sync_directory, sync_file},
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MARKER: &[u8; 8] = b"MYNOUF02";
const OLD_FORMAT: &[u8] = b"mynou-feeds/1";
const ROUTING_FORMAT: &[u8] = b"mynou-feeds/2";
const FORMAT: &[u8] = b"mynou-feeds/3";
const ROUTING: Key = single(4);
const SCHEDULE: Key = single(5);
mod routing;
const HEADER: Key = single(0);
const COUNTS: Key = single(1);
const SOURCE: Key = single(2);
const CURSOR: Key = single(3);
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
    upgrade: bool,
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

fn read(path: &Path) -> Result<Vec<u8>> {
    reject_symlinks(path)?;
    let meta = fs::symlink_metadata(path).map_err(|_| "Feed state: cannot inspect snapshot")?;
    if !meta.is_file() || meta.len() < 48 || meta.len() > (MAX_BYTES + 48) as u64 {
        return Err("Feed state: invalid snapshot type or size".into());
    }
    use std::os::unix::fs::MetadataExt;
    if meta.nlink() != 1 || meta.mode() & 0o077 != 0 {
        return Err("Feed state: snapshot must be private with one hard link".into());
    }
    let file = private_options()
        .read(true)
        .open(path)
        .map_err(|_| "Feed state: cannot open snapshot")?;
    let opened = file
        .metadata()
        .map_err(|_| "Feed state: cannot inspect opened snapshot")?;
    if opened.dev() != meta.dev()
        || opened.ino() != meta.ino()
        || opened.len() != meta.len()
        || opened.nlink() != 1
        || opened.mode() & 0o077 != 0
    {
        return Err("Feed state: snapshot changed during opening".into());
    }
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 49) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Feed state: cannot read snapshot")?;
    if bytes.len() < 48
        || bytes.len() > MAX_BYTES + 48
        || u64::from_le_bytes(
            bytes[8..16]
                .try_into()
                .map_err(|_| "Feed state: invalid length")?,
        ) != (bytes.len() - 48) as u64
        || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
    {
        return Err("Feed state: corrupt snapshot".into());
    }
    Ok(bytes)
}

fn legacy(bytes: Vec<u8>) -> Result<Legacy> {
    if &bytes[..8] != MAGIC {
        return Err("Feed state: unsupported snapshot".into());
    }
    let value = json::parse(
        std::str::from_utf8(&bytes[16..bytes.len() - 32])
            .map_err(|_| "Feed state: invalid UTF-8")?,
    )?;
    crate::numbering::only(&value, &["records"])?;
    let mut rows = BTreeMap::new();
    for value in value
        .get("records")
        .and_then(json::Value::as_array)
        .filter(|rows| rows.len() <= MAX_ROWS)
        .ok_or("Feed state: invalid legacy collection")?
    {
        let record = Record::from_json(value)?;
        if rows.insert(record.id.clone(), record).is_some() {
            return Err("Feed state: duplicate legacy record".into());
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
        return Err("Feed state: a preserved copy has different bytes".into());
    }
    let temporary = directory.join(format!(
        ".feeds-{}.tmp",
        super::digest(&random_bytes::<16>()?)
    ));
    let mut file = private_options()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| crate::store::migration_io("Feed storage", "write", &error))?;
    file.write_all(bytes)
        .and_then(|()| sync_file(&file))
        .map_err(|error| crate::store::migration_io("Feed storage", "write", &error))?;
    reject_symlinks(path)?;
    fs::rename(&temporary, path)
        .map_err(|error| crate::store::migration_io("Feed storage", "write", &error))?;
    sync_directory(directory).map_err(|error| crate::store::migration_sync("Feed storage", &error))
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
    if format != FORMAT && format != ROUTING_FORMAT && format != OLD_FORMAT {
        return Err("Feed state: unsupported index format".into());
    }
    let (total, active) = counters(index)?;
    let (metadata, generations, entries) = if format == FORMAT || format == ROUTING_FORMAT {
        let (generations, entries) = routing::counts(index)?;
        routing::schedule(index)?;
        (5, generations, entries)
    } else {
        (3, 0, 0)
    };
    let metadata = metadata
        + u64::from(index.get(&CURSOR)?.is_some())
        + if format == FORMAT {
            u64::from(crate::inventory::backlog::last(index)?.is_some())
        } else {
            0
        };
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

fn put(index: &mut Index, records: &[Record], cursor: Option<&str>) -> Result<()> {
    put_with(index, records, cursor, Vec::new())
}
fn put_with(
    index: &mut Index,
    records: &[Record],
    cursor: Option<&str>,
    extra: Vec<Change>,
) -> Result<()> {
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
    if let Some(cursor) = cursor {
        changes.push(Change::Put(CURSOR, cursor.as_bytes().to_vec()));
    }
    changes.extend(extra);
    index.commit(changes)?;
    Ok(())
}

fn verify(index: &mut Index, original: Option<&Legacy>) -> Result<()> {
    check(index)?;
    let source = sha256(original.map_or(&[][..], |image| image.bytes.as_slice()));
    if index.get(&SOURCE)?.as_deref() != Some(&source[..]) {
        return Err("Feed state: index belongs to different legacy bytes".into());
    }
    let rows = original.map(|image| &image.rows);
    let total = rows.map_or(0, |rows| rows.len() as u64);
    let active = rows.map_or(0, |rows| {
        rows.values().filter(|row| row.baselined).count() as u64
    });
    if counters(index)? != (total, active) {
        return Err("Feed state: migration counts disagree".into());
    }
    if let Some(rows) = rows {
        for record in rows.values() {
            if get(index, &record.id)?.as_ref() != Some(record) {
                return Err("Feed state: migration record differs from legacy state".into());
            }
            let reference = index.get(&key(ACTIVE, &record.id))?;
            if reference.as_deref() != record.baselined.then_some(record.id.as_bytes()) {
                return Err("Feed state: migration active reference differs".into());
            }
        }
    }
    let metadata = if matches!(
        index.get(&HEADER)?.as_deref(),
        Some(FORMAT) | Some(ROUTING_FORMAT)
    ) {
        if routing::counts(index)? != (0, 0) || !routing::schedule(index)?.is_empty() {
            return Err("Feed state: migration contains runtime routing state".into());
        }
        5
    } else {
        3
    };
    let metadata = metadata + u64::from(index.get(&CURSOR)?.is_some());
    if index.len()? != metadata + total + active {
        return Err("Feed state: migration has unexpected entries".into());
    }
    Ok(())
}

impl Storage {
    /// Index size and state for diagnostics, from the durable root only.
    pub(super) fn report(&self) -> crate::json::Value {
        match &self.backend {
            Backend::Absent => crate::inventory::report("empty", None),
            Backend::Legacy(_) => crate::inventory::report("legacy", None),
            Backend::Unpublished(index, _) => crate::inventory::report("migrating", Some(index)),
            Backend::Ready(index) => crate::inventory::report("index", Some(index)),
        }
    }
    pub(super) fn last_pass(&mut self) -> Result<Option<u64>> {
        self.available()?;
        match &mut self.backend {
            Backend::Ready(index) if !self.upgrade => crate::inventory::backlog::last(index),
            _ => Err("Feed backlog requires writable preparation".into()),
        }
    }
    pub(super) fn finish_pass(&mut self, at: u64) -> Result<()> {
        let index = self.writable_index()?;
        crate::inventory::backlog::finish(index, at)
    }
    pub(super) fn open(directory: &Path, read_only: bool) -> Result<Self> {
        let path = directory.join("feeds.bin");
        let indexed = exists(&directory.join("feeds.idx"))?;
        let image = if exists(&path)? {
            Some(read(&path)?)
        } else {
            None
        };
        let mut upgrade = false;
        let backend = match image {
            Some(bytes) if &bytes[..8] == MARKER => {
                if bytes.len() != 80 || !indexed {
                    return Err("Feed state: missing or invalid indexed storage".into());
                }
                let identity = bytes[16..48]
                    .try_into()
                    .map_err(|_| "Feed state: invalid marker identity")?;
                let mut index =
                    Index::open_bound(&directory.join("feeds.idx"), read_only, identity)?;
                check(&mut index)?;
                upgrade = index.get(&HEADER)?.as_deref() != Some(FORMAT);
                if read_only && upgrade {
                    return Err("Feed state: writable routing format upgrade required".into());
                }
                if index.get(&CURSOR)?.is_some() {
                    return Err("Feed state: published index retains a migration cursor".into());
                }
                Backend::Ready(Box::new(index))
            }
            image => {
                let original = image.map(legacy).transpose()?;
                if indexed {
                    // Prove an unmarked promoted index before allowing WAL recovery.
                    let mut index = Index::open(&directory.join("feeds.idx"), true)?;
                    verify(&mut index, original.as_ref())?;
                    if !read_only {
                        drop(index);
                        index = Index::open(&directory.join("feeds.idx"), false)?;
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
            upgrade,
        })
    }

    fn available(&self) -> Result<()> {
        if self.poisoned {
            return Err(
                "Feed state: migration durability is uncertain; writable restart required".into(),
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
    pub(super) fn total(&mut self) -> Result<u64> {
        self.available()?;
        match &mut self.backend {
            Backend::Ready(index) | Backend::Unpublished(index, _) => Ok(counters(index)?.0),
            Backend::Legacy(image) => Ok(image.rows.len() as u64),
            Backend::Absent => Ok(0),
        }
    }
    pub(super) fn needs_migration(&self) -> bool {
        self.upgrade || !matches!(self.backend, Backend::Ready(_))
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
            Backend::Ready(index) | Backend::Unpublished(index, _) => {
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
            Backend::Legacy(image) => {
                let mut ordered: Vec<_> = image
                    .rows
                    .values()
                    .map(|record| (key(PRIMARY, &record.id), record))
                    .collect();
                ordered.sort_by_key(|(key, _)| *key);
                let mut rows = Vec::new();
                let mut bytes = 0;
                let mut last = None;
                let mut more = false;
                for (key, record) in ordered {
                    if after.is_some_and(|after| key <= after) {
                        continue;
                    }
                    let length = json::stringify(&record.to_json()).len();
                    if rows.len() == limit
                        || (!rows.is_empty() && bytes + length > crate::inventory::MAX_PAGE_BYTES)
                    {
                        more = true;
                        break;
                    }
                    rows.push(record.clone());
                    bytes += length;
                    last = Some(key);
                }
                Ok(Page {
                    rows,
                    next: if more { last } else { None },
                    total: image.rows.len() as u64,
                    sequence: 0,
                })
            }
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
        if self.read_only {
            return Ok(());
        }
        if let Backend::Ready(index) = &mut self.backend {
            routing::upgrade(index)?;
            self.upgrade = false;
            return Ok(());
        }
        if !matches!(self.backend, Backend::Unpublished(..)) {
            self.migrate()?;
        }
        let Backend::Unpublished(index, original) = &mut self.backend else {
            return Err("Feed state: incomplete migration".into());
        };
        routing::upgrade(index)?;
        let snapshot = self.directory.join("feeds.bin");
        match original {
            Some(original) if !exists(&snapshot)? || read(&snapshot)? != original.bytes => {
                return Err("Feed state: legacy snapshot changed before publication".into());
            }
            None if exists(&snapshot)? => {
                return Err("Feed state: snapshot appeared before publication".into());
            }
            _ => {}
        }
        if let Some(original) = original {
            replace(
                &self.directory,
                &self.directory.join(format!(
                    "feeds.legacy-{}.bin",
                    super::digest(&original.bytes)
                )),
                &original.bytes,
                true,
            )?;
        }
        replace(
            &self.directory,
            &self.directory.join("feeds.bin"),
            &frame(MARKER, &index.identity()),
            false,
        )?;
        let backend = std::mem::replace(&mut self.backend, Backend::Absent);
        let Backend::Unpublished(index, _) = backend else {
            return Err("Feed state: incomplete promotion".into());
        };
        self.backend = Backend::Ready(index);
        Ok(())
    }

    fn migrate(&mut self) -> Result<()> {
        let original = match &self.backend {
            Backend::Legacy(image) => Some(image),
            Backend::Absent => None,
            _ => return Err("Feed state: migration already published".into()),
        };
        let total = original.map_or(0, |image| image.rows.len() as u64);
        let progress = crate::progress::Tracker::start_if(
            original.is_some(),
            &self.directory,
            "feeds",
            Some(total),
        );
        progress.stage("preserving", 0);
        let source = sha256(original.map_or(&[][..], |image| image.bytes.as_slice()));
        if let Some(image) = original {
            replace(
                &self.directory,
                &self
                    .directory
                    .join(format!("feeds.legacy-{}.bin", super::digest(&image.bytes))),
                &image.bytes,
                true,
            )?;
        }
        let staging = self.directory.join("feeds.idx.migrating");
        let mut index = if exists(&staging)? {
            let reuse = Index::open(&staging, true).and_then(|mut index| {
                check(&mut index)?;
                if index.get(&SOURCE)?.as_deref() != Some(&source[..]) {
                    return Err("Feed state: foreign migration stage".into());
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
                    // Staging is derived and has never acknowledged a runtime poll.
                    // Keep all unexpected contents, including interrupted writes.
                    let abandoned = self.directory.join(format!(
                        "feeds.idx.abandoned-{}",
                        super::digest(&random_bytes::<16>()?)
                    ));
                    fs::rename(&staging, abandoned).map_err(|error| {
                        crate::store::migration_io("Feed storage", "write", &error)
                    })?;
                    sync_directory(&self.directory)
                        .map_err(|error| crate::store::migration_sync("Feed storage", &error))?;
                    create(&staging, source)?
                }
            }
        } else {
            create(&staging, source)?
        };
        routing::upgrade(&mut index)?;
        let cursor = index
            .get(&CURSOR)?
            .map(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|_| "Feed state: invalid migration cursor".to_owned())
            })
            .transpose()?;
        if let Some(cursor) = &cursor
            && original.is_none_or(|image| !image.rows.contains_key(cursor))
        {
            return Err("Feed state: cursor is absent from original snapshot".into());
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
                        .ok_or("Feed state: empty migration batch")?
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
        if exists(&self.directory.join("feeds.idx"))? {
            return Err("Feed state: index appeared before promotion".into());
        }
        fs::rename(&staging, self.directory.join("feeds.idx"))
            .map_err(|error| crate::store::migration_io("Feed storage", "write", &error))?;
        sync_directory(&self.directory)
            .map_err(|error| crate::store::migration_sync("Feed storage", &error))?;
        let index = Index::open(&self.directory.join("feeds.idx"), false)?;
        let backend = std::mem::replace(&mut self.backend, Backend::Absent);
        let original = match backend {
            Backend::Legacy(image) => Some(image),
            Backend::Absent => None,
            _ => return Err("Feed state: unexpected migration backend".into()),
        };
        self.backend = Backend::Unpublished(Box::new(index), original);
        progress.finish();
        Ok(())
    }

    pub(super) fn commit(&mut self, record: Record) -> Result<()> {
        self.available()?;
        if self.read_only {
            return Err("Feed state: storage is read-only".into());
        }
        let Backend::Ready(index) = &mut self.backend else {
            return Err("Feed state: writable migration is required".into());
        };
        let bytes = read(&self.directory.join("feeds.bin"))?;
        if &bytes[..8] != MARKER || bytes.len() != 80 || bytes[16..48] != index.identity() {
            return Err("Feed state: index marker changed; restart required".into());
        }
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
        put_with(index, &[record], None, extra)
    }
    pub(super) fn active(&mut self, after: Option<Key>) -> Result<(Vec<Record>, Option<Key>)> {
        self.available()?;
        let Backend::Ready(index) = &mut self.backend else {
            return Err("Feed state: indexed active reads require migration".into());
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

fn create(path: &Path, source: [u8; 32]) -> Result<Index> {
    let mut index = Index::create(path)?;
    index.commit(vec![
        Change::PutInline(HEADER, FORMAT.to_vec()),
        Change::PutInline(COUNTS, encode_counts(0, 0)),
        Change::PutInline(SOURCE, source.to_vec()),
        Change::PutInline(ROUTING, encode_counts(0, 0)),
        Change::PutInline(SCHEDULE, Vec::new()),
    ])?;
    Ok(index)
}

#[cfg(test)]
mod tests;
