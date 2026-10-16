//! Checked disk-resident history; the bounded legacy snapshot is migration input only.
use super::{Record, Settings, State, digest, index};
use crate::{
    Result,
    crypto::{random_bytes, sha256},
    inventory::{Change, Index, Key, MAX_PAGE_ROWS},
    json,
    store::{
        before_private_open, private_options, reject_symlinks, sync_directory, sync_file,
        try_lock_file,
    },
};
use std::{
    cell::RefCell,
    fs,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MARKER: &[u8; 8] = b"MYNOUI04";
const INDEX: &str = "announcements.idx";
const STAGING: &str = "announcements.idx.staging";
enum Backend {
    Legacy {
        state: State,
        bytes: Option<Vec<u8>>,
    },
    Ready(Index),
}
pub(crate) struct AnnouncementStore {
    /// Only bounded binding metadata is retained after migration.
    pub state: State,
    directory: PathBuf,
    backend: RefCell<Backend>,
    read_only: bool,
    poisoned: bool,
    dirty: bool,
    #[cfg(test)]
    stop: Option<u8>,
}
fn header(state: &State) -> State {
    let mut s = state.clone();
    s.records.clear();
    s
}
fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    reject_symlinks(path)?;
    let m = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("IRC: cannot inspect history".into()),
        Ok(m) => m,
    };
    if !m.is_file() || m.nlink() != 1 || m.mode() & 0o077 != 0 || m.len() > (MAX_BYTES + 48) as u64
    {
        return Err("IRC: invalid history file, permissions or size".into());
    }
    before_private_open(path);
    let mut f = private_options()
        .read(true)
        .open(path)
        .map_err(|_| "IRC: cannot open history")?;
    let opened = f
        .metadata()
        .map_err(|_| "IRC: cannot inspect open history")?;
    if !opened.is_file() {
        return Err("IRC: invalid history file, permissions or size".into());
    }
    if m.dev() != opened.dev() || m.ino() != opened.ino() {
        return Err("IRC: history changed during open".into());
    }
    let mut bytes = Vec::new();
    Read::take(&mut f, (MAX_BYTES + 49) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "IRC: cannot read history")?;
    if bytes.len() as u64 != m.len() {
        return Err("IRC: history changed during read".into());
    }
    Ok(Some(bytes))
}
fn parse(bytes: &[u8]) -> Result<State> {
    if bytes.len() < 48
        || ![
            b"MYNOUI01".as_slice(),
            b"MYNOUI02".as_slice(),
            b"MYNOUI03".as_slice(),
        ]
        .contains(&&bytes[..8])
        || u64::from_le_bytes(
            bytes[8..16]
                .try_into()
                .map_err(|_| "IRC: invalid history length")?,
        ) != bytes.len() as u64 - 48
        || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
    {
        return Err("IRC: corrupt or unsupported history".into());
    }
    let state = State::from_json(&json::parse(
        std::str::from_utf8(&bytes[16..bytes.len() - 32])
            .map_err(|_| "IRC: history is not UTF-8")?,
    )?)?;
    if state.records.values().any(|r| r.route.is_some()) && &bytes[..8] == b"MYNOUI01" {
        return Err("IRC: acquisition reservations require history format 2".into());
    }
    if state.records.values().any(|r| r.admission.is_some()) && &bytes[..8] != b"MYNOUI03" {
        return Err("IRC: requester admissions require history format 3".into());
    }
    Ok(state)
}
fn replace(directory: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    // Inspect any existing destination as strictly as the source before replacement.
    read(path)?;
    let temp = directory.join(format!(
        ".announcements-{}.tmp",
        digest(&random_bytes::<16>()?)
    ));
    let result = (|| {
        let mut f = private_options()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| "IRC: cannot create storage file")?;
        f.write_all(bytes)
            .and_then(|()| sync_file(&f))
            .map_err(|_| "IRC: cannot persist history")?;
        fs::rename(&temp, path).map_err(|_| "IRC: cannot publish history")?;
        sync_directory(directory)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
impl AnnouncementStore {
    pub(crate) fn open(directory: &Path, settings: &Settings, read_only: bool) -> Result<Self> {
        let bytes = read(&directory.join("announcements.bin"))?;
        let (backend, mut state) = if let Some(bytes) = &bytes
            && bytes.starts_with(MARKER)
        {
            if bytes.len() != 72 || sha256(&bytes[..40]).as_slice() != &bytes[40..] {
                return Err("IRC: corrupt index marker".into());
            }
            let identity = bytes[8..40]
                .try_into()
                .map_err(|_| "IRC: invalid index identity")?;
            let mut index = Index::open_bound(&directory.join(INDEX), read_only, identity)?;
            let state = State::from_json(&json::parse(
                std::str::from_utf8(
                    &index
                        .get(&index::HEADER)?
                        .ok_or("IRC: missing index metadata")?,
                )
                .map_err(|_| "IRC: invalid metadata text")?,
            )?)?;
            if !state.records.is_empty() {
                return Err("IRC: index metadata contains history rows".into());
            }
            index::count(&mut index)?;
            (Backend::Ready(index), state)
        } else {
            // A missing marker cannot hide an already published history.
            if bytes.is_none() && fs::symlink_metadata(directory.join(INDEX)).is_ok() {
                return Err("IRC: published index has no history marker".into());
            }
            let state = bytes.as_deref().map(parse).transpose()?.unwrap_or_default();
            let metadata = header(&state);
            (Backend::Legacy { state, bytes }, metadata)
        };
        let old = state.clone();
        for source in &settings.sources {
            if let Some(binding) = state.bindings.get(&source.id) {
                if *binding != source.binding() {
                    return Err(
                        "IRC: an existing source binding changed; use a new stable source ID"
                            .into(),
                    );
                }
            } else {
                state.bindings.insert(source.id.clone(), source.binding());
            }
        }
        State::from_json(&state.to_json())?;
        let dirty = state != old;
        Ok(Self {
            state,
            directory: directory.into(),
            backend: RefCell::new(backend),
            read_only,
            poisoned: false,
            dirty,
            #[cfg(test)]
            stop: None,
        })
    }
    fn available(&self) -> Result<()> {
        if self.poisoned {
            return Err(
                "IRC: history durability is uncertain; restart before making decisions".into(),
            );
        }
        if let Backend::Ready(index) = &*self.backend.borrow() {
            index.sequence()?;
        }
        Ok(())
    }
    pub(crate) fn get(&self, id: &str) -> Result<Option<Record>> {
        self.available()?;
        let row = match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => index::get(index, id)?,
            Backend::Legacy { state, .. } => {
                index::primary(id)?;
                state.records.get(id).cloned()
            }
        };
        if row
            .as_ref()
            .is_some_and(|r| self.state.bindings.get(&r.source_id) != Some(&r.binding))
        {
            return Err("IRC: unbound stored announcement".into());
        }
        Ok(row)
    }
    pub(crate) fn count(&self) -> Result<u64> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => index::count(index),
            Backend::Legacy { state, .. } => Ok(state.records.len() as u64),
        }
    }
    pub(crate) fn page(
        &self,
        table: u8,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Record>, Option<Key>)> {
        self.available()?;
        if limit == 0 || limit > MAX_PAGE_ROWS {
            return Err("IRC: invalid page bounds".into());
        }
        let (rows, next) = match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => index::page(index, table, after, limit)?,
            Backend::Legacy { state, .. } => {
                let mut entries = Vec::new();
                for r in state.records.values() {
                    for (key, _) in index::entries(r)? {
                        if key[0] == table && after.is_none_or(|after| key > after) {
                            entries.push((key, r.clone()));
                        }
                    }
                }
                entries.sort_by_key(|(key, _)| *key);
                let next = if entries.len() > limit {
                    Some(entries[limit - 1].0)
                } else {
                    None
                };
                (
                    entries.into_iter().take(limit).map(|(_, r)| r).collect(),
                    next,
                )
            }
        };
        if rows
            .iter()
            .any(|r| self.state.bindings.get(&r.source_id) != Some(&r.binding))
        {
            return Err("IRC: unbound stored announcement".into());
        }
        Ok((rows, next))
    }
    pub(crate) fn initialize(&mut self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        if self.state.bindings.is_empty()
            && matches!(&*self.backend.borrow(), Backend::Legacy { bytes: None, .. })
        {
            return Ok(());
        }
        // Persist a bounded empty legacy input before creating a first index, so
        // an interruption after promotion still has an unambiguous restart source.
        if matches!(&*self.backend.borrow(), Backend::Legacy { bytes: None, .. }) {
            let state = self.state.clone();
            let payload = json::stringify(&state.to_json()).into_bytes();
            let mut bytes = b"MYNOUI03".to_vec();
            bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
            bytes.extend_from_slice(&payload);
            bytes.extend_from_slice(&sha256(&bytes));
            replace(
                &self.directory,
                &self.directory.join("announcements.bin"),
                &bytes,
            )?;
            *self.backend.borrow_mut() = Backend::Legacy {
                state,
                bytes: Some(bytes),
            };
        }
        let legacy = match &*self.backend.borrow() {
            Backend::Legacy { state, bytes } => Some((state.clone(), bytes.clone())),
            Backend::Ready(_) => None,
        };
        if let Some((state, bytes)) = legacy {
            match self.migrate(&state, bytes.as_deref()) {
                Ok(index) => *self.backend.borrow_mut() = Backend::Ready(index),
                Err(e) => {
                    self.poisoned = true;
                    return Err(e);
                }
            }
        }
        if self.dirty {
            self.save_rows(&[])?;
        }
        Ok(())
    }
    pub(crate) fn save_rows(&mut self, rows: &[Record]) -> Result<()> {
        self.available()?;
        if self.read_only {
            return Err("IRC: history is read-only".into());
        }
        if rows.len() > MAX_PAGE_ROWS {
            return Err("IRC: history patch exceeds operation bound".into());
        }
        let mut metadata = self.state.clone();
        metadata.revision = metadata
            .revision
            .checked_add(1)
            .ok_or("IRC: history revision overflow")?;
        let mut backend = self.backend.borrow_mut();
        let Backend::Ready(index) = &mut *backend else {
            return Err("IRC: checked migration must precede history writes".into());
        };
        let result = index::commit(index, &metadata, rows, None);
        if result.is_err() && !index.is_usable() {
            self.poisoned = true;
        }
        result?;
        self.state = metadata;
        self.dirty = false;
        Ok(())
    }
    fn checkpoint(&self, stage: u8) -> Result<()> {
        #[cfg(test)]
        if self.stop == Some(stage) {
            return Err("IRC: injected migration interruption".into());
        }
        let _ = stage;
        Ok(())
    }
    fn migrate(&self, legacy: &State, bytes: Option<&[u8]>) -> Result<Index> {
        let source = sha256(bytes.unwrap_or_default());
        if let Some(bytes) = bytes {
            let backup = self
                .directory
                .join(format!("announcements.legacy-{}.bin", digest(&source)));
            if let Some(known) = read(&backup)? {
                if known != bytes {
                    return Err("IRC: preserved legacy copy differs".into());
                }
            } else {
                replace(&self.directory, &backup, bytes)?;
            }
        }
        self.checkpoint(1)?;
        let published = self.directory.join(INDEX);
        let staging = self
            .directory
            .join(format!("{STAGING}-{}", digest(&source)));
        let proof_path = self
            .directory
            .join(format!("announcements.migration-{}.bin", digest(&source)));
        reject_symlinks(&published)?;
        reject_symlinks(&staging)?;
        let promoted = published
            .try_exists()
            .map_err(|_| "IRC: cannot inspect promotion")?;
        let proof = read(&proof_path)?;
        let identity = if let Some(proof) = proof {
            if proof.len() != 104
                || &proof[..8] != b"MYNOUIM1"
                || proof[8..40] != source
                || sha256(&proof[..72]).as_slice() != &proof[72..]
            {
                return Err("IRC: foreign or corrupt migration proof".into());
            }
            proof[40..72]
                .try_into()
                .map_err(|_| "IRC: invalid migration identity")?
        } else {
            if promoted
                || staging
                    .try_exists()
                    .map_err(|_| "IRC: cannot inspect staging")?
            {
                return Err(
                    "IRC: migration identity proof is missing; refusing foreign staging".into(),
                );
            }
            let identity = random_bytes::<32>()?;
            let mut proof = b"MYNOUIM1".to_vec();
            proof.extend_from_slice(&source);
            proof.extend_from_slice(&identity);
            proof.extend_from_slice(&sha256(&proof));
            replace(&self.directory, &proof_path, &proof)?;
            identity
        };
        self.checkpoint(9)?;
        #[cfg(test)]
        if self.stop.is_some_and(|n| n >= 10) {
            Index::inject_creation(self.stop.map(|n| n - 9));
        }
        let opened = if promoted {
            Index::open_bound(&published, false, identity)
        } else {
            open_staging(&self.directory, &staging, identity)
        };
        #[cfg(test)]
        Index::inject_creation(None);
        let mut index = opened?;
        self.checkpoint(2)?;
        match index.get(&index::SOURCE)? {
            Some(known) if known.as_slice() != source => {
                return Err("IRC: index differs from validated legacy source".into());
            }
            None if !promoted && index.len()? == 0 => {
                index.commit(vec![Change::PutInline(index::SOURCE, source.to_vec())])?;
            }
            None => return Err("IRC: missing migration source proof".into()),
            _ => {}
        }
        self.checkpoint(3)?;
        if !promoted {
            verify_checkpoint(&mut index, legacy, &source)?;
        }
        if !promoted {
            let cursor = index.get(&index::CURSOR)?;
            let cursor = cursor
                .map(|b| {
                    <Key>::try_from(b.as_slice())
                        .map_err(|_| "IRC: invalid migration cursor".to_owned())
                })
                .transpose()?;
            if cursor.is_some_and(|k| {
                !legacy
                    .records
                    .values()
                    .any(|r| index::primary(&r.id).ok() == Some(k))
            }) {
                return Err("IRC: migration cursor is not a legacy identity".into());
            }
            let mut batch = Vec::new();
            for r in legacy.records.values() {
                if cursor.is_some_and(|cursor| index::primary(&r.id).is_ok_and(|k| k <= cursor)) {
                    continue;
                }
                batch.push(r.clone());
                if batch.len() == MAX_PAGE_ROWS {
                    index::commit(
                        &mut index,
                        &header(legacy),
                        &batch,
                        batch.last().map(|r| r.id.as_str()),
                    )?;
                    self.checkpoint(4)?;
                    batch.clear();
                }
            }
            if !batch.is_empty() {
                index::commit(
                    &mut index,
                    &header(legacy),
                    &batch,
                    batch.last().map(|r| r.id.as_str()),
                )?;
            }
            index::commit(&mut index, &header(legacy), &[], None)?;
        }
        verify(&mut index, legacy, &source)?;
        self.checkpoint(5)?;
        if promoted && index.get(&index::CURSOR)?.is_some() {
            return Err("IRC: promoted index retains a staging cursor".into());
        }
        if index.get(&index::CURSOR)?.is_some() {
            index.commit(vec![Change::Delete(index::CURSOR)])?;
        }
        self.checkpoint(6)?;
        let identity = index.identity();
        drop(index);
        if !promoted {
            fs::rename(&staging, &published).map_err(|_| "IRC: cannot promote checked history")?;
            sync_directory(&self.directory)?;
        }
        self.checkpoint(7)?;
        let mut marker = MARKER.to_vec();
        marker.extend_from_slice(&identity);
        marker.extend_from_slice(&sha256(&marker));
        replace(
            &self.directory,
            &self.directory.join("announcements.bin"),
            &marker,
        )?;
        self.checkpoint(8)?;
        Index::open_bound(&published, false, identity)
    }
}
/// A prepublished identity proof permits recreating only an unpopulated initial
/// directory. Partial artifacts are preserved; any unknown or committed data is refused.
fn open_staging(directory: &Path, staging: &Path, identity: [u8; 32]) -> Result<Index> {
    if !staging
        .try_exists()
        .map_err(|_| "IRC: cannot inspect staging")?
    {
        return Index::create_bound(staging, identity);
    }
    let metadata = fs::symlink_metadata(staging).map_err(|_| "IRC: cannot inspect staging")?;
    if !metadata.is_dir() || metadata.mode() & 0o077 != 0 {
        return Err("IRC: staging must be a private directory".into());
    }
    let tree = staging.join("tree.bin");
    let incomplete = match fs::symlink_metadata(&tree) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => return Err("IRC: cannot inspect staged tree".into()),
        Ok(m) => {
            if !m.is_file() || m.nlink() != 1 || m.mode() & 0o077 != 0 {
                return Err("IRC: invalid staged tree path".into());
            }
            m.len() < 4096
        }
    };
    if !incomplete {
        return Index::open_bound(staging, false, identity);
    }
    check_creation_artifacts(staging, identity)?;
    // Exclude a creator that still owns the unpopulated stage before preserving it.
    let lock_path = staging.join(".lock");
    let lock = match fs::symlink_metadata(&lock_path) {
        Ok(metadata) => {
            before_private_open(&lock_path);
            let file = private_options()
                .read(true)
                .write(true)
                .open(&lock_path)
                .map_err(|_| "IRC: cannot open creation lock")?;
            let opened = file
                .metadata()
                .map_err(|_| "IRC: cannot inspect creation lock")?;
            if !opened.is_file() || opened.nlink() != 1 {
                return Err("IRC: creation lock must be a regular file".into());
            }
            if metadata.dev() != opened.dev() || metadata.ino() != opened.ino() {
                return Err("IRC: creation lock changed during open".into());
            }
            file
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => private_options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|_| "IRC: cannot create initial staging lock")?,
        Err(_) => return Err("IRC: cannot inspect creation lock".into()),
    };
    try_lock_file(&lock).map_err(|_| "IRC: initial staging is already owned")?;
    // A creator may have advanced since preflight. Recheck under ownership.
    check_creation_artifacts(staging, identity)?;
    let preserved = directory.join(format!(
        "{STAGING}.incomplete-{}",
        digest(&random_bytes::<16>()?)
    ));
    fs::rename(staging, &preserved)
        .map_err(|_| "IRC: cannot preserve incomplete initial staging")?;
    sync_directory(directory)?;
    let created = Index::create_bound(staging, identity);
    drop(lock);
    created
}

fn check_creation_artifacts(staging: &Path, identity: [u8; 32]) -> Result<()> {
    let m = fs::symlink_metadata(staging).map_err(|_| "IRC: cannot inspect initial staging")?;
    if !m.is_dir() || m.mode() & 0o077 != 0 {
        return Err("IRC: initial staging must remain private".into());
    }
    let tree = staging.join("tree.bin");
    match fs::symlink_metadata(&tree) {
        Ok(m) if m.len() >= 4096 => {
            return Err("IRC: staging creation advanced; reopen storage".into());
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err("IRC: cannot inspect initial tree".into());
        }
        _ => {}
    }
    let id = read(&staging.join("identity.bin"))?;
    let mut expected_id = b"MYNOUID1".to_vec();
    expected_id.extend_from_slice(&identity);
    expected_id.extend_from_slice(&sha256(&expected_id));
    let complete_identity = id.as_ref().is_some_and(|id| id == &expected_id);
    if id.as_ref().is_some_and(|id| !expected_id.starts_with(id)) {
        return Err("IRC: foreign or corrupt initial staging identity".into());
    }
    if !complete_identity
        && (fs::symlink_metadata(&tree).is_ok()
            || fs::symlink_metadata(staging.join("objects")).is_ok())
    {
        return Err("IRC: incomplete staging has unbound artifacts".into());
    }
    if let Some(bytes) = read(&tree)?
        && !Index::valid_creation_prefix(identity, &bytes)
    {
        return Err("IRC: corrupt partial creation tree".into());
    }
    if let Some(lock) = read(&staging.join(".lock"))?
        && !lock.is_empty()
    {
        return Err("IRC: foreign staged lock".into());
    }
    for entry in fs::read_dir(staging).map_err(|_| "IRC: cannot inspect creation artifacts")? {
        let path = entry
            .map_err(|_| "IRC: cannot inspect creation artifact")?
            .path();
        match path.file_name().and_then(|s| s.to_str()) {
            Some(".lock" | "identity.bin" | "tree.bin") => {
                read(&path)?;
            }
            Some("objects") => {
                let m = fs::symlink_metadata(&path)
                    .map_err(|_| "IRC: cannot inspect staged objects")?;
                if !m.is_dir()
                    || m.mode() & 0o077 != 0
                    || fs::read_dir(&path)
                        .map_err(|_| "IRC: cannot inspect staged objects")?
                        .next()
                        .is_some()
                {
                    return Err("IRC: initial staging contains foreign or populated objects".into());
                }
            }
            _ => return Err("IRC: initial staging contains foreign artifacts".into()),
        }
    }
    Ok(())
}

/// Validate the acknowledged prefix before resuming writes into a staged index.
fn verify_checkpoint(index: &mut Index, legacy: &State, source: &[u8; 32]) -> Result<()> {
    if index.get(&index::HEADER)?.is_none() {
        if index.len()? != 1 || index.get(&index::SOURCE)?.as_deref() != Some(source.as_slice()) {
            return Err("IRC: foreign or corrupt initial staging".into());
        }
        return Ok(());
    }
    if let Some(cursor) = index.get(&index::CURSOR)? {
        let cursor: Key = cursor
            .as_slice()
            .try_into()
            .map_err(|_| "IRC: invalid migration cursor")?;
        if !legacy
            .records
            .values()
            .any(|r| index::primary(&r.id).ok() == Some(cursor))
        {
            return Err("IRC: migration cursor is not a legacy identity".into());
        }
        let mut prefix = header(legacy);
        for r in legacy.records.values() {
            if index::primary(&r.id)? <= cursor {
                prefix.records.insert(r.id.clone(), r.clone());
            }
        }
        verify(index, &prefix, source)
    } else {
        verify(index, legacy, source)
    }
}

fn verify(index: &mut Index, legacy: &State, source: &[u8; 32]) -> Result<()> {
    let mut expected = std::collections::BTreeMap::from([
        (
            index::HEADER,
            json::stringify(&header(legacy).to_json()).into_bytes(),
        ),
        (index::SOURCE, source.to_vec()),
        (
            index::COUNT,
            (legacy.records.len() as u64).to_le_bytes().to_vec(),
        ),
    ]);
    for r in legacy.records.values() {
        expected.extend(index::entries(r)?);
    }
    if let Some(cursor) = index.get(&index::CURSOR)? {
        expected.insert(index::CURSOR, cursor);
    }
    for (key, value) in &expected {
        if index.get(key)?.as_ref() != Some(value) {
            return Err("IRC: migration record, metadata or secondary proof differs".into());
        }
    }
    if index.len()? != expected.len() as u64 {
        return Err("IRC: migration contains unacknowledged extra keys".into());
    }
    Ok(())
}
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
