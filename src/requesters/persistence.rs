//! Indexed demand storage; legacy state is retained only during checked migration.
use super::{Demand, LedgerRead, Policy, Record, State, index};
use crate::{
    Result,
    config::Config,
    crypto::{random_bytes, sha256},
    inventory::{Change, Index, Key, MAX_PAGE_ROWS},
    json,
    store::{private_options, reject_symlinks, sync_directory},
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
const MAX_LEGACY_BYTES: usize = 16 * 1024 * 1024;
const MARKER: &[u8; 8] = b"MYNOUR03";
const INDEX: &str = "requesters.idx";
const STAGING: &str = "requesters.idx.staging";

enum Backend {
    Legacy {
        state: State,
        bytes: Option<Vec<u8>>,
    },
    Ready(Index),
}
pub(crate) struct RequesterStore {
    /// Bounded account metadata only. Demands/operator interests live on disk.
    pub state: State,
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
fn parse(bytes: &[u8]) -> Result<State> {
    if bytes.len() < 48
        || bytes.len() > MAX_LEGACY_BYTES + 48
        || ![b"MYNOUR01".as_slice(), b"MYNOUR02".as_slice()].contains(&&bytes[..8])
        || u64::from_le_bytes(
            bytes[8..16]
                .try_into()
                .map_err(|_| "Requester: invalid snapshot length")?,
        ) != bytes.len() as u64 - 48
        || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
    {
        return Err("Requester: corrupt or unsupported snapshot".into());
    }
    let state = State::from_json(&json::parse(
        std::str::from_utf8(&bytes[16..bytes.len() - 32])
            .map_err(|_| "Requester: snapshot is not UTF-8")?,
    )?)?;
    if &bytes[..8] == b"MYNOUR01"
        && state
            .demands
            .values()
            .any(|d| d.origins.iter().any(|o| o.starts_with("irc:")))
    {
        return Err("Requester: explicit IRC origins require snapshot format 2".into());
    }
    Ok(state)
}
fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    reject_symlinks(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Requester: cannot inspect snapshot".into()),
        Ok(m) => m,
    };
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > (MAX_LEGACY_BYTES + 48) as u64
    {
        return Err("Requester: invalid snapshot file, permissions or size".into());
    }
    let mut file = private_options()
        .read(true)
        .open(path)
        .map_err(|_| "Requester: cannot open snapshot")?;
    let opened = file
        .metadata()
        .map_err(|_| "Requester: cannot inspect open snapshot")?;
    if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
        return Err("Requester: snapshot changed during open".into());
    }
    let mut bytes = Vec::new();
    Read::take(&mut file, (MAX_LEGACY_BYTES + 49) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Requester: cannot read snapshot")?;
    if bytes.len() as u64 != metadata.len() {
        return Err("Requester: snapshot changed during read".into());
    }
    Ok(Some(bytes))
}
fn replace(directory: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    reject_symlinks(path)?;
    let temp = directory.join(format!(
        ".requesters-{}.tmp",
        super::digest(&random_bytes::<16>()?)
    ));
    let result = (|| {
        let mut file = private_options()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| "Requester: cannot create storage file")?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| "Requester: cannot persist storage file")?;
        fs::rename(&temp, path).map_err(|_| "Requester: cannot publish storage file")?;
        sync_directory(directory)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
impl RequesterStore {
    pub(crate) fn open(directory: &Path, config: &Config, read_only: bool) -> Result<Self> {
        let bytes = read(&directory.join("requesters.bin"))?;
        let (backend, mut state) = if let Some(bytes) = &bytes
            && bytes.starts_with(MARKER)
        {
            if bytes.len() != 72 || sha256(&bytes[..40]).as_slice() != &bytes[40..] {
                return Err("Requester: corrupt index marker".into());
            }
            let mut index = Index::open(&directory.join(INDEX), read_only)?;
            if index.identity().as_slice() != &bytes[8..40] {
                return Err("Requester: index identity differs from marker".into());
            }
            let value = index
                .get(&index::HEADER)?
                .ok_or("Requester: index metadata is missing")?;
            let state = State::from_json(&json::parse(
                std::str::from_utf8(&value).map_err(|_| "Requester: invalid metadata text")?,
            )?)?;
            if !state.demands.is_empty() || !state.operator_jobs.is_empty() {
                return Err("Requester: index metadata contains inventory records".into());
            }
            (Backend::Ready(index), state)
        } else {
            let state = bytes
                .as_deref()
                .map(parse)
                .transpose()?
                .unwrap_or_else(State::empty);
            let metadata = header(&state);
            (Backend::Legacy { state, bytes }, metadata)
        };
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
            Backend::Legacy { state, .. } => Ok(state.revision),
        }
    }
    pub(crate) fn initialize(&mut self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        let legacy = match &*self.backend.borrow() {
            Backend::Legacy { state, bytes } => Some((state.clone(), bytes.clone())),
            Backend::Ready(_) => None,
        };
        if let Some((state, bytes)) = legacy {
            let result = self.migrate(&state, bytes.as_deref());
            match result {
                Ok(index) => *self.backend.borrow_mut() = Backend::Ready(index),
                Err(error) => {
                    self.poisoned = true;
                    return Err(error);
                }
            }
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
        poll: Option<(&str, &BTreeSet<String>)>,
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
            return Err("Requester: checked migration must precede a patch".into());
        };
        let metadata = header(&patch);
        let result = index::commit_poll(index, &metadata, &demands, &operators, poll);
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
            Backend::Legacy { state, .. } => {
                let filtered = State {
                    revision: state.revision,
                    accounts: BTreeMap::new(),
                    demands: state
                        .demands
                        .iter()
                        .filter(|(_, d)| d.account_id == account)
                        .map(|(id, d)| (id.clone(), d.clone()))
                        .collect(),
                    operator_jobs: BTreeSet::new(),
                };
                index::legacy_page(&filtered, after, limit, false)
            }
        }
    }
    pub(crate) fn account_cursor(&self, account: &str, id: &str) -> Result<Key> {
        match &*self.backend.borrow() {
            Backend::Ready(_) => index::account_cursor(account, id),
            Backend::Legacy { .. } => index::primary_cursor(id),
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
            return Err("Requester: checked migration must precede cursor writes".into());
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
            Backend::Legacy { .. } => {
                Err("Requester: writable migration is required before scheduling".into())
            }
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
            Backend::Legacy { state, .. } => Ok(state
                .demands
                .values()
                .filter(|d| {
                    (account.is_empty() || d.account_id == account)
                        && state_name.is_none_or(|name| name == d.state)
                })
                .count() as u64),
        }
    }
    pub(crate) fn describe(&self) -> Result<json::Value> {
        let mut value = json::Value::object();
        value.insert("demands", self.count("", None)?.to_string());
        value.insert("pending", self.count("", Some("pending"))?.to_string());
        value.insert("reserved", self.count("", Some("reserved"))?.to_string());
        value.insert("revision", self.sequence()?.to_string());
        value.insert(
            "backend",
            if matches!(&*self.backend.borrow(), Backend::Ready(_)) {
                "disk-index"
            } else {
                "legacy-awaiting-migration"
            },
        );
        Ok(value)
    }
    pub(crate) fn poll_ready(&self, account: &str) -> Result<bool> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => index::poll_ready(index, account),
            Backend::Legacy { .. } => Ok(true),
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
    fn migrate(&self, state: &State, bytes: Option<&[u8]>) -> Result<Index> {
        let source = sha256(bytes.unwrap_or_default());
        if let Some(bytes) = bytes {
            let backup = self
                .directory
                .join(format!("requesters.legacy-{}.bin", super::digest(&source)));
            if let Some(known) = read(&backup)? {
                if known != bytes {
                    return Err(
                        "Requester: preserved legacy copy differs; refusing replacement".into(),
                    );
                }
            } else {
                replace(&self.directory, &backup, bytes)?;
            }
        }
        let published = self.directory.join(INDEX);
        reject_symlinks(&published)?;
        let staging = self.directory.join(STAGING);
        reject_symlinks(&staging)?;
        let mut index = if published
            .try_exists()
            .map_err(|_| "Requester: cannot inspect published index")?
        {
            let mut index = Index::open(&published, false)?;
            if index.get(&index::SOURCE)?.as_deref() != Some(source.as_slice()) {
                return Err(
                    "Requester: published index differs from validated legacy source".into(),
                );
            }
            // A promoted index may contain acknowledged changes if somebody
            // restored old bytes beside it. Never rewrite it before proving
            // this is exactly the pre-marker migration checkpoint.
            verify(&mut index, state, &self.state, &source)?;
            if index.get(&index::CURSOR)?.is_some() {
                return Err("Requester: promoted index retains a staging cursor".into());
            }
            let identity = index.identity();
            drop(index);
            let mut marker = MARKER.to_vec();
            marker.extend_from_slice(&identity);
            marker.extend_from_slice(&sha256(&marker));
            replace(&self.directory, &self.directory.join("requesters.bin"), &marker)?;
            return Index::open(&published, false);
        } else {
            let mut reusable = None;
            if staging
                .try_exists()
                .map_err(|_| "Requester: cannot inspect staging")?
            {
                if let Ok(mut inspect) = Index::open(&staging, true)
                    && inspect.get(&index::SOURCE)?.as_deref() == Some(source.as_slice())
                {
                    drop(inspect);
                    reusable = Some(Index::open(&staging, false)?);
                } else {
                    let metadata = fs::symlink_metadata(&staging)
                        .map_err(|_| "Requester: cannot inspect staging")?;
                    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
                        return Err("Requester: staging must be a private directory; move it aside explicitly".into());
                    }
                    let preserved = self.directory.join(format!(
                        "requesters.idx.abandoned-{}",
                        super::digest(&random_bytes::<16>()?)
                    ));
                    fs::rename(&staging, &preserved)
                        .map_err(|_| "Requester: cannot preserve interrupted staging")?;
                    sync_directory(&self.directory)?;
                }
            }
            if let Some(index) = reusable {
                index
            } else {
                let mut index = Index::create(&staging)?;
                index.commit(vec![Change::PutInline(index::SOURCE, source.to_vec())])?;
                index
            }
        };
        let cursor = index.get(&index::CURSOR)?;
        let mut batch = Vec::new();
        for demand in state.demands.values() {
            if cursor
                .as_deref()
                .is_some_and(|done| demand.id.as_bytes() <= done)
            {
                continue;
            }
            batch.push(demand.clone());
            if batch.len() == MAX_PAGE_ROWS {
                write_migration_batch(&mut index, &self.state, &batch)?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            write_migration_batch(&mut index, &self.state, &batch)?;
        }
        let operators: Vec<_> = state.operator_jobs.iter().cloned().collect();
        for batch in operators.chunks(MAX_PAGE_ROWS) {
            index::commit(&mut index, &self.state, &[], batch)?;
        }
        index::commit(&mut index, &self.state, &[], &[])?;
        verify(&mut index, state, &self.state, &source)?;
        if index.get(&index::CURSOR)?.is_some() {
            index.commit(vec![Change::Delete(index::CURSOR)])?;
        }
        let identity = index.identity();
        drop(index);
        if !published
            .try_exists()
            .map_err(|_| "Requester: cannot inspect promotion")?
        {
            fs::rename(&staging, &published)
                .map_err(|_| "Requester: cannot promote checked index")?;
            sync_directory(&self.directory)?;
        }
        let mut marker = MARKER.to_vec();
        marker.extend_from_slice(&identity);
        marker.extend_from_slice(&sha256(&marker));
        replace(
            &self.directory,
            &self.directory.join("requesters.bin"),
            &marker,
        )?;
        Index::open(&published, false)
    }
}
fn write_migration_batch(index: &mut Index, metadata: &State, batch: &[Demand]) -> Result<()> {
    index::commit(index, metadata, batch, &[])?;
    let last = batch.last().ok_or("Requester: empty migration batch")?;
    index.commit(vec![Change::Put(
        index::CURSOR,
        last.id.as_bytes().to_vec(),
    )])?;
    Ok(())
}
fn verify(index: &mut Index, legacy: &State, metadata: &State, source: &[u8; 32]) -> Result<()> {
    let mut expected = BTreeMap::from([
        (
            index::HEADER,
            json::stringify(&header(metadata).to_json()).into_bytes(),
        ),
        (index::SOURCE, source.to_vec()),
    ]);
    let mut counts = BTreeMap::<Key, u64>::new();
    for demand in legacy.demands.values() {
        expected.extend(index::entries(demand)?);
        for account in [&demand.account_id[..], ""] {
            for state in [index::state_code(&demand.state)?, 255] {
                *counts.entry(index::counter(account, state)).or_default() += 1;
            }
        }
    }
    for (key, count) in counts {
        expected.insert(key, count.to_le_bytes().to_vec());
    }
    for id in &legacy.operator_jobs {
        expected.insert(index::operator_key(id)?, Vec::new());
    }
    if !legacy.operator_jobs.is_empty() {
        expected.insert(
            index::OPERATOR_COUNT,
            (legacy.operator_jobs.len() as u64).to_le_bytes().to_vec(),
        );
    }
    if let Some(cursor) = index.get(&index::CURSOR)? {
        expected.insert(index::CURSOR, cursor);
    }
    for (key, value) in &expected {
        if index.get(key)?.as_ref() != Some(value) {
            return Err("Requester: migration primary, secondary or metadata proof differs".into());
        }
    }
    if index.len()? != expected.len() as u64 {
        return Err("Requester: migration contains unacknowledged extra keys".into());
    }
    Ok(())
}
impl LedgerRead for RequesterStore {
    fn accounts(&self) -> &BTreeMap<String, Record> {
        &self.state.accounts
    }
    fn demand(&self, id: &str) -> Result<Option<Demand>> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => index::get(index, id)?
                .map(|d| index::effective(index, d))
                .transpose(),
            Backend::Legacy { state, .. } => state.demand(id),
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
            Backend::Legacy { state, .. } => state.demands_page(after, limit),
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
            Backend::Legacy { state, .. } => state.irc_demands_page(after, limit),
        }
    }
    fn operator_interest(&self, id: &str) -> Result<bool> {
        self.available()?;
        match &mut *self.backend.borrow_mut() {
            Backend::Ready(index) => index::has_operator(index, id),
            Backend::Legacy { state, .. } => state.operator_interest(id),
        }
    }
}
