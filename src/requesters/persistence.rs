//! Indexed demand storage (`requesters.idx`).
use super::{Demand, LedgerRead, PlexRegistry, Policy, Record, State, index};
use crate::{
    Result,
    config::Config,
    inventory::{Change, Index, Key},
    json,
    store::{reject_symlinks, sync_directory},
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
const INDEX: &str = "requesters.idx";
/// Where a new index is built before it takes its name.
const STAGING: &str = "requesters.idx.creating";

enum Backend {
    /// Nothing stored yet; a writable start creates the index.
    Absent,
    Ready(Box<Index>),
}
pub(crate) struct RequesterStore {
    /// Bounded account metadata only. Demands/operator interests live on disk.
    pub state: State,
    /// The owner and friends Plex lists; their switch is the account policy.
    pub plex: PlexRegistry,
    directory: PathBuf,
    backend: RefCell<Backend>,
    read_only: bool,
    dirty: bool,
    poisoned: bool,
    pub admission_cursor: Option<Key>,
    pub admission_state: u8,
    pub outcome_cursor: Option<Key>,
}
fn header(state: &State) -> State {
    let mut state = state.clone();
    state.demands.clear();
    state.operator_jobs.clear();
    state
}
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Requester: cannot inspect storage".into()),
    }
}
impl RequesterStore {
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
            Backend::Ready(index) => {
                crate::inventory::backlog::read(index, "requester_admission_scan", at)
            }
            Backend::Absent => Ok(crate::inventory::backlog::inactive(
                "disabled",
                "requester_admission_scan",
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
    pub(crate) fn open(directory: &Path, config: &Config, read_only: bool) -> Result<Self> {
        let path = directory.join(INDEX);
        reject_symlinks(&path)?;
        let (mut backend, mut state) = if exists(&path)? {
            let mut index = Index::open(&path, read_only)?;
            let value = index
                .get(&index::HEADER)?
                .ok_or("Requester: index metadata is missing")?;
            let state = State::from_json(&json::parse(
                std::str::from_utf8(&value).map_err(|_| "Requester: invalid metadata text")?,
            )?)?;
            if !state.demands.is_empty() || !state.operator_jobs.is_empty() {
                return Err("Requester: index metadata contains inventory records".into());
            }
            crate::inventory::backlog::current(&mut index)?;
            (Backend::Ready(Box::new(index)), state)
        } else {
            crate::store::refuse_older_format(directory, &["requesters.bin"], "Requester")?;
            (Backend::Absent, State::empty())
        };
        let plex = match &mut backend {
            Backend::Ready(index) => match index.get(&index::PLEX_ACCOUNTS)? {
                Some(bytes) => PlexRegistry::from_json(&json::parse(
                    std::str::from_utf8(&bytes)
                        .map_err(|_| "Requester: invalid Plex accounts text")?,
                )?)?,
                None => PlexRegistry::default(),
            },
            Backend::Absent => PlexRegistry::default(),
        };
        for record in state.accounts.values() {
            if let Some(user) = super::plex_user(&record.id)
                && record.binding != super::plex_binding(user)
            {
                return Err("Requester: a Plex account's binding changed".into());
            }
        }
        let original = state.clone();
        for account in &config.requesters.accounts {
            if let Some(record) = state.accounts.get(&account.id) {
                if record.binding != account.binding() {
                    return Err("Requester: an existing account binding changed; use a new stable account ID".into());
                }
            } else {
                state.accounts.insert(
                    account.id.clone(),
                    Record {
                        id: account.id.clone(),
                        binding: account.binding(),
                        policy: Policy::initial(config),
                        revision: 1,
                        cursor: 0,
                        snapshot: None,
                        polled_at: 0,
                        last_error: None,
                    },
                );
            }
        }
        State::from_json(&state.to_json())?;
        let dirty = state != original;
        let (admission_state, admission_cursor, outcome_cursor) = (0, None, None);
        let mut store = Self {
            state,
            plex,
            directory: directory.to_owned(),
            backend: RefCell::new(backend),
            read_only,
            dirty,
            poisoned: false,
            admission_cursor,
            admission_state,
            outcome_cursor,
        };
        if let Backend::Ready(index) = &mut *store.backend.borrow_mut() {
            if let Some(bytes) = index.get(&index::ADMISSION_CURSOR)? {
                if ![1, 65].contains(&bytes.len()) || bytes[0] > 2 {
                    return Err("Requester: invalid admission continuation".into());
                }
                store.admission_state = bytes[0];
                store.admission_cursor = if bytes.len() == 65 {
                    Some(
                        bytes[1..]
                            .try_into()
                            .map_err(|_| "Requester: invalid admission cursor")?,
                    )
                } else {
                    None
                };
            }
            if let Some(bytes) = index.get(&index::OUTCOME_CURSOR)? {
                store.outcome_cursor = if bytes.is_empty() {
                    None
                } else {
                    Some(
                        bytes
                            .as_slice()
                            .try_into()
                            .map_err(|_| "Requester: invalid outcome cursor")?,
                    )
                };
            }
        }
        Ok(store)
    }
    fn available(&self) -> Result<()> {
        if self.poisoned {
            return Err("Requester storage durability is uncertain; restart before reading or changing demand".into());
        }
        if let Backend::Ready(index) = &*self.backend.borrow() {
            index.sequence()?;
        }
        Ok(())
    }
    pub(crate) fn sequence(&self) -> Result<u64> {
        self.available()?;
        match &*self.backend.borrow() {
            Backend::Ready(index) => index.sequence(),
            Backend::Absent => Ok(self.state.revision),
        }
    }
    pub(crate) fn initialize(&mut self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        if matches!(&*self.backend.borrow(), Backend::Absent) {
            match self.create() {
                Ok(index) => *self.backend.borrow_mut() = Backend::Ready(Box::new(index)),
                Err(error) => {
                    self.poisoned = true;
                    return Err(error);
                }
            }
        }
        if let Backend::Ready(index) = &mut *self.backend.borrow_mut() {
            index::prepare_backlog(index)?;
        }
        if self.dirty {
            self.save_patch(self.state.clone())?;
        }
        Ok(())
    }
    /// The supplied State contains metadata and explicit changed records only;
    /// records omitted from this bounded patch are never removed.
    pub(crate) fn save_patch(&mut self, patch: State) -> Result<()> {
        self.save_poll_patch(patch, None)
    }
    pub(crate) fn save_poll_patch(
        &mut self,
        mut patch: State,
        stage: Option<index::Stage<'_>>,
    ) -> Result<()> {
        self.available()?;
        if self.read_only {
            return Err("Requester storage is read-only".into());
        }
        patch.revision = self
            .state
            .revision
            .checked_add(1)
            .ok_or("Requester: revision overflow")?;
        for (id, previous) in &self.state.accounts {
            let next = patch
                .accounts
                .get(id)
                .ok_or("Requester: a patch cannot discard an account")?;
            if next.binding != previous.binding
                || next.revision < previous.revision
                || next.cursor < previous.cursor
            {
                return Err("Requester: account binding or revision moved backwards".into());
            }
        }
        State::from_json(&patch.to_json())?;
        let demands: Vec<_> = patch.demands.values().cloned().collect();
        let operators: Vec<_> = patch.operator_jobs.iter().cloned().collect();
        let mut backend = self.backend.borrow_mut();
        let Backend::Ready(index) = &mut *backend else {
            return Err("Requester: storage is not initialized".into());
        };
        let metadata = header(&patch);
        let result = index::commit_poll(index, &metadata, &demands, &operators, stage);
        if result.is_err() && !index.is_usable() {
            self.poisoned = true;
        }
        result?;
        self.state = metadata;
        self.dirty = false;
        Ok(())
    }
    pub(crate) fn account_page(
        &self,
        account: &str,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Demand>, Option<Key>)> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => {
                let page = index::account_page(index, account, after, limit)?;
                let demands = page
                    .demands
                    .into_iter()
                    .map(|d| index::effective(index, d))
                    .collect::<Result<Vec<_>>>()?;
                Ok((demands, page.next))
            }
            Backend::Absent => Ok((Vec::new(), None)),
        }
    }
    pub(crate) fn save_admission_cursor(&mut self, state: u8, after: Option<Key>) -> Result<()> {
        let mut bytes = vec![state];
        if let Some(after) = after {
            bytes.extend_from_slice(&after);
        }
        self.persist_cursor(index::ADMISSION_CURSOR, bytes)?;
        self.admission_state = state;
        self.admission_cursor = after;
        Ok(())
    }
    /// Records the Plex accounts read with the owner's connection. A patch
    /// that adds the requester of one comes after it, so a stored Plex
    /// requester always has its account here.
    pub(crate) fn save_plex(&mut self, registry: PlexRegistry) -> Result<()> {
        if registry == self.plex {
            return Ok(());
        }
        let value = registry.to_json();
        PlexRegistry::from_json(&value)?;
        self.persist_cursor(index::PLEX_ACCOUNTS, json::stringify(&value).into_bytes())?;
        self.plex = registry;
        Ok(())
    }
    pub(crate) fn save_outcome_cursor(&mut self, after: Option<Key>) -> Result<()> {
        self.persist_cursor(
            index::OUTCOME_CURSOR,
            after.map_or_else(Vec::new, |key| key.to_vec()),
        )?;
        self.outcome_cursor = after;
        Ok(())
    }
    fn persist_cursor(&mut self, key: Key, bytes: Vec<u8>) -> Result<()> {
        self.available()?;
        if self.read_only {
            return Err("Requester storage is read-only".into());
        }
        let mut backend = self.backend.borrow_mut();
        let Backend::Ready(index) = &mut *backend else {
            return Err("Requester: storage is not initialized".into());
        };
        index.commit(vec![Change::Put(key, bytes)])?;
        Ok(())
    }
    pub(crate) fn queue_page(
        &self,
        state_name: &str,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Demand>, Option<Key>)> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => {
                let page = index::queue_page(index, state_name, after, limit)?;
                let demands = page
                    .demands
                    .into_iter()
                    .map(|d| index::effective(index, d))
                    .collect::<Result<Vec<_>>>()?;
                Ok((demands, page.next))
            }
            Backend::Absent => Ok((Vec::new(), None)),
        }
    }
    pub(crate) fn count(&self, account: &str, state_name: Option<&str>) -> Result<u64> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => index::count(
                index,
                account,
                state_name
                    .map(index::state_code)
                    .transpose()?
                    .unwrap_or(255),
            ),
            Backend::Absent => Ok(0),
        }
    }
    /// The unfinished watchlist sweep of an account, if any.
    pub(crate) fn sweep(&self, account: &str) -> Result<Option<index::Sweep>> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => index::sweep(index, account),
            Backend::Absent => Ok(None),
        }
    }
    pub(crate) fn poll_ready(&self, account: &str) -> Result<bool> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => index::poll_ready(index, account),
            Backend::Absent => Ok(true),
        }
    }
    /// One bounded durable retirement pass. A later poll cannot publish before
    /// every prior absence is recorded, so origins never resurrect raw work.
    pub(crate) fn reconcile_origins(&mut self, account: &str) -> Result<()> {
        if self.poll_ready(account)? {
            return Ok(());
        }
        let (page, mut progress, changed) = {
            let mut backend = self.backend.borrow_mut();
            let Backend::Ready(index) = &mut *backend else {
                return Err("Requester: missing live index".into());
            };
            let (page, progress) = index::cleanup_page(index, account)?;
            let mut changed = Vec::new();
            for demand in &page.demands {
                let effective = index::effective(index, demand.clone())?;
                if effective != *demand {
                    changed.push(effective);
                }
            }
            (page, progress, changed)
        };
        if !changed.is_empty() {
            let mut patch = self.state.clone();
            patch.demands = changed.into_iter().map(|d| (d.id.clone(), d)).collect();
            self.save_patch(patch)?;
        }
        progress.push(u8::from(page.next.is_none()));
        if let Some(next) = page.next {
            progress.extend_from_slice(&next);
        }
        let mut backend = self.backend.borrow_mut();
        let Backend::Ready(index) = &mut *backend else {
            return Err("Requester: missing live index".into());
        };
        index.commit(vec![Change::Put(index::clean_key(account), progress)])?;
        Ok(())
    }
    /// Builds the index with its metadata under a temporary name, then
    /// publishes it, so a published index always has its metadata.
    fn create(&self) -> Result<Index> {
        let staging = self.directory.join(STAGING);
        reject_symlinks(&staging)?;
        if exists(&staging)? {
            // Only an interrupted creation leaves it, and it held no demand.
            fs::remove_dir_all(&staging)
                .map_err(|_| "Requester: cannot remove an unfinished index")?;
        }
        let mut index = Index::create(&staging)?;
        index::prepare_backlog(&mut index)?;
        index::commit(&mut index, &header(&self.state), &[], &[])?;
        drop(index);
        let path = self.directory.join(INDEX);
        fs::rename(&staging, &path)
            .map_err(|error| crate::store::storage_io("Requester storage", "write", &error))?;
        sync_directory(&self.directory)
            .map_err(|error| crate::store::storage_sync("Requester storage", &error))?;
        Index::open(&path, false)
    }
}
impl LedgerRead for RequesterStore {
    fn accounts(&self) -> &BTreeMap<String, Record> {
        &self.state.accounts
    }
    fn plex(&self) -> &PlexRegistry {
        &self.plex
    }
    fn demand(&self, id: &str) -> Result<Option<Demand>> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => index::get(index, id)?
                .map(|d| index::effective(index, d))
                .transpose(),
            Backend::Absent => Ok(None),
        }
    }
    fn demands_page(&self, after: Option<Key>, limit: usize) -> Result<(Vec<Demand>, Option<Key>)> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => {
                let page = index::all_page(index, after, limit)?;
                let demands = page
                    .demands
                    .into_iter()
                    .map(|d| index::effective(index, d))
                    .collect::<Result<Vec<_>>>()?;
                Ok((demands, page.next))
            }
            Backend::Absent => Ok((Vec::new(), None)),
        }
    }
    fn irc_demands_page(
        &self,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Demand>, Option<Key>)> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => {
                let page = index::irc_page(index, after, limit)?;
                let demands = page
                    .demands
                    .into_iter()
                    .map(|d| index::effective(index, d))
                    .collect::<Result<Vec<_>>>()?;
                Ok((demands, page.next))
            }
            Backend::Absent => Ok((Vec::new(), None)),
        }
    }
    fn operator_interest(&self, id: &str) -> Result<bool> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => index::has_operator(index, id),
            Backend::Absent => Ok(false),
        }
    }
}
