//! Announcement history in the disk-resident index (`announcements.idx`).
use super::{Record, Settings, State, index};
use crate::{
    Result,
    inventory::{Index, Key, MAX_PAGE_ROWS},
    json,
    store::{reject_symlinks, sync_directory},
};
use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
};
const INDEX: &str = "announcements.idx";
/// Where a new index is built before it takes its name.
const STAGING: &str = "announcements.idx.creating";
enum Backend {
    /// Nothing stored yet; the index is created once a source is bound.
    Absent,
    Ready(Box<Index>),
}
pub(crate) struct AnnouncementStore {
    /// Bounded binding metadata; rows live in the index.
    pub state: State,
    directory: PathBuf,
    backend: RefCell<Backend>,
    read_only: bool,
    poisoned: bool,
    dirty: bool,
}
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("IRC: cannot inspect history".into()),
    }
}
impl AnnouncementStore {
    /// Index size and state for diagnostics, from the durable root only.
    pub(crate) fn storage(&self) -> crate::json::Value {
        match &*self.backend.borrow() {
            Backend::Absent => crate::inventory::report("empty", None),
            Backend::Ready(index) => crate::inventory::report("index", Some(index)),
        }
    }
    pub(crate) fn backlog(&self, at: u64) -> Result<crate::json::Value> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => crate::inventory::backlog::read(index, "irc_routing_scan", at),
            Backend::Absent => Ok(crate::inventory::backlog::inactive(
                "disabled",
                "irc_routing_scan",
                None,
            )),
        }
    }
    pub(crate) fn finish_backlog_pass(&mut self, at: u64) -> Result<()> {
        self.available()?;
        if self.read_only {
            return Err("Backlog is read-only".into());
        }
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => crate::inventory::backlog::finish(index, at),
            Backend::Absent => Ok(()),
        }
    }
    pub(crate) fn open(directory: &Path, settings: &Settings, read_only: bool) -> Result<Self> {
        let path = directory.join(INDEX);
        reject_symlinks(&path)?;
        let (backend, mut state) = if exists(&path)? {
            let mut index = Index::open(&path, read_only)?;
            let state = State::from_json(&json::parse(
                std::str::from_utf8(
                    &index
                        .get(&index::HEADER)?
                        .ok_or("IRC: missing index metadata")?,
                )
                .map_err(|_| "IRC: invalid metadata text")?,
            )?)?;
            index::count(&mut index)?;
            (Backend::Ready(Box::new(index)), state)
        } else {
            (Backend::Absent, State::default())
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
            Backend::Absent => {
                index::primary(id)?;
                None
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
            Backend::Absent => Ok(0),
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
            Backend::Absent => (Vec::new(), None),
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
        if matches!(&*self.backend.borrow(), Backend::Absent) {
            if self.state.bindings.is_empty() {
                return Ok(());
            }
            match self.create() {
                Ok(index) => *self.backend.borrow_mut() = Backend::Ready(Box::new(index)),
                Err(e) => {
                    self.poisoned = true;
                    return Err(e);
                }
            }
            self.dirty = false;
        }
        if let Backend::Ready(index) = &mut *self.backend.borrow_mut() {
            index::prepare_backlog(index)?;
        }
        if self.dirty {
            self.save_rows(&[])?;
        }
        Ok(())
    }
    /// Builds the index with its metadata under a temporary name, then
    /// publishes it, so a published index always has its metadata.
    fn create(&self) -> Result<Index> {
        let staging = self.directory.join(STAGING);
        reject_symlinks(&staging)?;
        if exists(&staging)? {
            // Only an interrupted creation leaves it, and it held no row.
            fs::remove_dir_all(&staging).map_err(|_| "IRC: cannot remove an unfinished index")?;
        }
        let mut index = Index::create(&staging)?;
        index::prepare_backlog(&mut index)?;
        index::commit(&mut index, &self.state, &[])?;
        drop(index);
        let path = self.directory.join(INDEX);
        fs::rename(&staging, &path).map_err(|error| {
            crate::store::storage_io("IRC announcement storage", "write", &error)
        })?;
        sync_directory(&self.directory)
            .map_err(|error| crate::store::storage_sync("IRC announcement storage", &error))?;
        Index::open(&path, false)
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
            return Err("IRC: history is not initialized".into());
        };
        let result = index::commit(index, &metadata, rows);
        if result.is_err() && !index.is_usable() {
            self.poisoned = true;
        }
        result?;
        self.state = metadata;
        self.dirty = false;
        Ok(())
    }
}
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
