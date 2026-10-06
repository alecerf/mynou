//! Native private NZB queue. Network I/O never holds the persistent state lock.
mod model;
mod storage;
use super::{
    Downloads, ProbeRequest, Server, Settings, nntp,
    nzb::{self, Nzb},
    workspace::Workspace,
    yenc,
};
use crate::{
    Result,
    crypto::random_bytes,
    json::{self, Value},
    requesters::{digest, valid_digest},
    store::{self, private_options, sync_directory},
};
use model::{
    Data, MAX_RECORDS, MAX_SNAPSHOT, MAX_SOURCE_BYTES, MAX_SOURCES, Phase, Record, Reservation,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

struct Source {
    bytes: Arc<Vec<u8>>,
    nzb: Nzb,
}
struct Inner {
    settings: Downloads,
    servers: BTreeMap<String, Server>,
    sources: BTreeMap<String, Source>,
    data: Data,
    workspaces: BTreeMap<String, Workspace>,
    verified: BTreeMap<String, u32>,
    active: BTreeSet<String>,
    epoch: String,
    _owner: Option<File>,
    existing: bool,
    initialized: bool,
    read_only: bool,
    poisoned: bool,
}
impl Inner {
    fn writable(&self) -> Result<()> {
        if self.read_only || !self.initialized || !self.settings.enabled || self.poisoned {
            return Err(
                "Usenet queue: running writable service and recovered storage required".into(),
            );
        }
        Ok(())
    }
    fn persist(&mut self, next: Data) -> Result<()> {
        self.writable()?;
        let v = next.json();
        Data::parse(&v)?;
        let bytes = json::stringify(&v).into_bytes();
        if bytes.len() > MAX_SNAPSHOT {
            return Err("Usenet queue: snapshot limit exceeded".into());
        }
        if let Err(e) = storage::write(
            &self.settings.state_dir.join("queue.bin"),
            storage::QUEUE,
            &bytes,
        ) {
            self.poisoned = true;
            return Err(e);
        }
        self.data = next;
        Ok(())
    }
    fn guard(&self, scope: Value) -> String {
        digest(
            json::stringify(&Value::Array(vec![
                self.epoch.clone().into(),
                self.data.json(),
                scope,
            ]))
            .as_bytes(),
        )
    }
    fn workspace_path(&self, id: &str) -> PathBuf {
        self.settings.state_dir.join("files").join(id)
    }
    fn provider(&self, r: &Record) -> Option<Server> {
        self.servers.get(&r.server).cloned()
    }
    fn check_review(&self, q: &ProbeRequest, guard: &str) -> Result<()> {
        self.writable()?;
        if q.plan_id.as_deref() != Some(guard) {
            return Err("Usenet queue: review is stale; preview again".into());
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct Client {
    inner: Arc<Mutex<Inner>>,
    stopped: Arc<AtomicBool>,
}
impl Client {
    /// No worker starts here. Explicit callers may drive tick; the service owns its workers.
    pub fn open(settings: &Settings, read_only: bool) -> Result<Self> {
        let client = Self::prepare(settings, read_only)?;
        if !read_only && settings.downloads.as_ref().is_some_and(|d| d.enabled) {
            client.initialize()?;
        }
        Ok(client)
    }
    pub(crate) fn prepare(settings: &Settings, read_only: bool) -> Result<Self> {
        let downloads = settings
            .downloads
            .clone()
            .ok_or("Usenet queue: downloads are not configured")?;
        let root = &downloads.state_dir;
        let existing = storage::exists(root)?;
        let mut owner = None;
        let data = if existing {
            storage::directory(root)?;
            storage::directory(&root.join("sources"))?;
            storage::directory(&root.join("files"))?;
            let f = storage::file(&root.join(".owner"), 0)?;
            if !read_only {
                f.try_lock()
                    .map_err(|_| "Usenet queue: another client owns storage")?;
                owner = Some(f);
            }
            let bytes = storage::read(&root.join("queue.bin"), storage::QUEUE, MAX_SNAPSHOT)?;
            Data::parse(&json::parse(
                std::str::from_utf8(&bytes)
                    .map_err(|_| "Usenet queue: invalid snapshot encoding")?,
            )?)?
        } else {
            Data::default()
        };
        let servers = settings
            .servers
            .iter()
            .map(|s| (s.id().to_owned(), s.clone()))
            .collect::<BTreeMap<_, _>>();
        if servers.len() != settings.servers.len() {
            return Err("Usenet queue: duplicate provider identity".into());
        }
        let mut sources = BTreeMap::<String, Source>::new();
        let mut bytes_total = 0;
        let mut workspaces = BTreeMap::new();
        let mut verified = BTreeMap::new();
        // Every referenced source and workspace is validated without repair before initialization.
        for r in data.records.values() {
            if servers
                .get(&r.server)
                .is_some_and(|s| s.binding() != r.binding)
            {
                return Err("Usenet queue: provider binding changed; use a new server ID".into());
            }
            if !sources.contains_key(&r.source) {
                let bytes = storage::read(
                    &root.join("sources").join(format!("{}.bin", r.source)),
                    storage::SOURCE,
                    nzb::MAX_BYTES,
                )?;
                let nzb = Nzb::parse(&bytes)?;
                if nzb.id != r.source {
                    return Err("Usenet queue: source identity mismatch".into());
                }
                bytes_total += bytes.len();
                if bytes_total > MAX_SOURCE_BYTES {
                    return Err("Usenet queue: source bytes exceed retained limit".into());
                }
                sources.insert(
                    r.source.clone(),
                    Source {
                        bytes: Arc::new(bytes),
                        nzb,
                    },
                );
            }
            let source = &sources[&r.source];
            let file = source
                .nzb
                .files
                .get(r.file_index)
                .ok_or("Usenet queue: captured file index is absent")?;
            if file.segments.len() != r.attempts.len() {
                return Err("Usenet queue: captured inventory differs".into());
            }
            let path = root.join("files").join(&r.id);
            if storage::exists(&path)? {
                let w = Workspace::open(
                    &path,
                    &source.bytes,
                    r.file_index,
                    &r.binding,
                    r.max_file,
                    true,
                )?;
                if w.verified_numbers()
                    .any(|n| r.attempts[n as usize - 1] == 0)
                {
                    return Err("Usenet queue: receipt has no reserved attempt".into());
                }
                if r.phase == Phase::Preparing && w.first_missing() != Some(1) {
                    return Err(
                        "Usenet queue: preparing workspace already contains articles".into(),
                    );
                }
                if r.phase == Phase::Complete && w.available_file().is_none() {
                    return Err("Usenet queue: complete output lacks verified proof".into());
                }
                verified.insert(r.id.clone(), w.verified_numbers().count() as u32);
                workspaces.insert(r.id.clone(), w);
            } else if r.phase != Phase::Preparing {
                return Err("Usenet queue: referenced workspace is missing".into());
            } else {
                verified.insert(r.id.clone(), 0);
            }
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                settings: downloads,
                servers,
                sources,
                data,
                workspaces,
                verified,
                active: BTreeSet::new(),
                epoch: digest(&random_bytes::<16>()?),
                _owner: owner,
                existing,
                initialized: false,
                read_only,
                poisoned: false,
            })),
            stopped: Arc::new(AtomicBool::new(false)),
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| "Usenet queue: internal state requires recovery".into())
    }
    pub(crate) fn initialize(&self) -> Result<()> {
        let mut inner = self.lock()?;
        if inner.read_only || !inner.settings.enabled {
            return Ok(());
        }
        if inner.initialized {
            return Ok(());
        }
        if !inner.existing {
            let root = inner.settings.state_dir.clone();
            storage::create_directory(&root, true)?;
            let owner = private_options()
                .create_new(true)
                .read(true)
                .write(true)
                .open(root.join(".owner"))
                .map_err(|_| "Usenet queue: cannot create owner")?;
            owner
                .try_lock()
                .map_err(|_| "Usenet queue: cannot acquire owner")?;
            owner
                .sync_all()
                .map_err(|_| "Usenet queue: owner durability failed")?;
            storage::create_directory(&root.join("sources"), false)?;
            storage::create_directory(&root.join("files"), false)?;
            sync_directory(&root).map_err(|_| "Usenet queue: directory durability failed")?;
            sync_directory(root.parent().ok_or("Usenet queue: root has no parent")?)
                .map_err(|_| "Usenet queue: parent durability failed")?;
            inner._owner = Some(owner);
            inner.existing = true;
        }
        let records = inner.data.records.values().cloned().collect::<Vec<_>>();
        // All preflight handles were read-only. Upgrade only after complete validation.
        inner.workspaces.clear();
        for r in &records {
            let path = inner.workspace_path(&r.id);
            let source = &inner.sources[&r.source];
            let w = if storage::exists(&path)? {
                Workspace::open(
                    &path,
                    &source.bytes,
                    r.file_index,
                    &r.binding,
                    r.max_file,
                    false,
                )?
            } else {
                Workspace::create(&path, &source.bytes, r.file_index, &r.binding, r.max_file)?
            };
            inner
                .verified
                .insert(r.id.clone(), w.verified_numbers().count() as u32);
            inner.workspaces.insert(r.id.clone(), w);
        }
        inner.initialized = true;
        let mut next = inner.data.clone();
        let mut changed = !storage::exists(&inner.settings.state_dir.join("queue.bin"))?;
        for r in records {
            let recovery = matches!(
                r.phase,
                Phase::Preparing | Phase::Downloading | Phase::Verifying
            );
            let removed = inner.provider(&r).is_none()
                && matches!(
                    r.phase,
                    Phase::Preparing | Phase::Queued | Phase::Downloading | Phase::Verifying
                );
            if recovery || removed {
                let revision = next.next()?;
                let row = next
                    .records
                    .get_mut(&r.id)
                    .ok_or("Usenet queue: missing recovery record")?;
                row.phase = if removed {
                    Phase::Paused
                } else if inner.workspaces[&r.id].available_file().is_some() {
                    Phase::Complete
                } else {
                    Phase::Queued
                };
                row.reservation = None;
                row.next_attempt = 0;
                row.revision = revision;
                if removed {
                    row.error = Some("provider_removed".into());
                }
                changed = true;
            }
        }
        if changed {
            inner.persist(next)?;
        }
        Ok(())
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    pub fn max_active(&self) -> Result<usize> {
        Ok(self.lock()?.settings.max_active)
    }
    pub fn report(&self) -> Result<Value> {
        let inner = self.lock()?;
        let mut v = Value::object();
        v.insert("enabled", inner.settings.enabled);
        v.insert("recovery_required", inner.poisoned);
        v.insert("revision", inner.data.revision.to_string());
        v.insert("active", inner.active.len() as u32);
        v.insert("max_active", inner.settings.max_active as u32);
        let rows = inner
            .data
            .records
            .values()
            .map(|r| {
                let mut v = Value::object();
                v.insert("id", r.id.clone());
                v.insert("source_id", r.source.clone());
                v.insert("file_index", r.file_index as u32);
                v.insert("server_id", r.server.clone());
                v.insert(
                    "state",
                    if inner.poisoned {
                        "recovery_required"
                    } else {
                        r.phase.name()
                    },
                );
                v.insert("revision", r.revision.to_string());
                v.insert(
                    "verified_parts",
                    inner.verified.get(&r.id).copied().unwrap_or(0),
                );
                v.insert("total_parts", r.attempts.len() as u32);
                v.insert(
                    "attempts",
                    r.attempts.iter().map(|n| u32::from(*n)).sum::<u32>(),
                );
                v.insert("attempt_limit", u32::from(r.limit));
                v.insert("ready", r.phase == Phase::Complete && !inner.poisoned);
                v.insert("provider_available", inner.provider(r).is_some());
                v.insert(
                    "last_error",
                    r.error.clone().map_or(Value::Null, Value::from),
                );
                v
            })
            .collect();
        v.insert("records", Value::Array(rows));
        Ok(v)
    }
    pub fn enqueue(
        &self,
        bytes: &[u8],
        server_id: &str,
        file_index: usize,
        review: &ProbeRequest,
    ) -> Result<Value> {
        review.validate()?;
        let nzb = Nzb::parse(bytes)?;
        let file = nzb
            .files
            .get(file_index)
            .ok_or("Usenet queue: file index is absent")?;
        let mut inner = self.lock()?;
        let server = inner
            .servers
            .get(server_id)
            .ok_or("Usenet queue: provider is not configured")?;
        let binding = server.binding();
        let settings = &inner.settings;
        let id = Record::identity(
            &nzb.id,
            file_index,
            server_id,
            &binding,
            settings.max_file_bytes,
            settings.max_attempts,
        );
        let guard = inner.guard(Value::Array(vec![
            "enqueue".into(),
            id.clone().into(),
            settings.state_dir.to_string_lossy().into_owned().into(),
            settings.max_active.to_string().into(),
            settings.enabled.into(),
        ]));
        let mut report = Value::object();
        report.insert("id", id.clone());
        report.insert("source_id", nzb.id.clone());
        report.insert("file_index", file_index as u32);
        report.insert("server_id", server_id);
        report.insert("total_parts", file.segments.len() as u32);
        report.insert("plan_id", guard.clone());
        report.insert("applied", false);
        report.insert("already_retained", inner.data.records.contains_key(&id));
        report.insert("library_admitted", false);
        let mut next = inner.data.clone();
        let revision = next.next()?;
        let row = Record {
            id: id.clone(),
            source: nzb.id.clone(),
            file_index,
            server: server_id.into(),
            binding,
            max_file: settings.max_file_bytes,
            limit: settings.max_attempts,
            order: revision,
            revision,
            phase: Phase::Preparing,
            attempts: vec![0; file.segments.len()],
            reservation: None,
            next_attempt: 0,
            error: None,
        };
        if !inner.data.records.contains_key(&id) {
            if inner.data.records.len() >= MAX_RECORDS {
                return Err("Usenet queue: retained record limit reached".into());
            }
            let extra = if inner.sources.contains_key(&nzb.id) {
                0
            } else {
                bytes.len()
            };
            if inner.sources.len() + usize::from(extra != 0) > MAX_SOURCES
                || inner.sources.values().map(|s| s.bytes.len()).sum::<usize>() + extra
                    > MAX_SOURCE_BYTES
            {
                return Err("Usenet queue: retained source limit reached".into());
            }
            next.records.insert(id.clone(), row.clone());
            Data::parse(&next.json())?;
        }
        if !review.apply {
            return Ok(report);
        }
        if self.stopped.load(Ordering::Acquire) {
            return Err("Usenet queue: service is stopped".into());
        }
        inner.check_review(review, &guard)?;
        if inner.data.records.contains_key(&id) {
            return Err("Usenet queue: transfer is already retained; use its controls".into());
        }
        let source_path = inner
            .settings
            .state_dir
            .join("sources")
            .join(format!("{}.bin", nzb.id));
        if storage::exists(&source_path)? {
            if storage::read(&source_path, storage::SOURCE, nzb::MAX_BYTES)? != bytes {
                return Err("Usenet queue: immutable source conflict".into());
            }
        } else if let Err(e) = storage::write(&source_path, storage::SOURCE, bytes) {
            inner.poisoned = true;
            return Err(e);
        }
        inner
            .sources
            .entry(nzb.id.clone())
            .or_insert_with(|| Source {
                bytes: Arc::new(bytes.to_vec()),
                nzb,
            });
        // The intent and source are durable before the selected workspace is created.
        inner.persist(next.clone())?;
        let w = match Workspace::create(
            &inner.workspace_path(&id),
            bytes,
            file_index,
            &row.binding,
            row.max_file,
        ) {
            Ok(w) => w,
            Err(e) => {
                inner.poisoned = true;
                return Err(e);
            }
        };
        inner.workspaces.insert(id.clone(), w);
        inner.verified.insert(id.clone(), 0);
        let revision = next.next()?;
        let row = next
            .records
            .get_mut(&id)
            .ok_or("Usenet queue: missing preparation intent")?;
        row.phase = Phase::Queued;
        row.revision = revision;
        inner.persist(next)?;
        report.insert("applied", true);
        Ok(report)
    }
    pub fn control(&self, id: &str, action: &str, review: &ProbeRequest) -> Result<Value> {
        review.validate()?;
        if !valid_digest(id) || !matches!(action, "pause" | "resume" | "cancel" | "retry") {
            return Err("Usenet queue: invalid control".into());
        }
        let mut inner = self.lock()?;
        let r = inner
            .data
            .records
            .get(id)
            .ok_or("Usenet queue: transfer is absent")?
            .clone();
        let phase = match action {
            "pause"
                if matches!(
                    r.phase,
                    Phase::Queued | Phase::Downloading | Phase::Verifying
                ) =>
            {
                Phase::Paused
            }
            "resume" if r.phase == Phase::Paused => Phase::Queued,
            "cancel" if r.phase != Phase::Cancelled => Phase::Cancelled,
            "retry" if r.phase == Phase::Failed => Phase::Queued,
            _ => return Err("Usenet queue: control is not valid in this state".into()),
        };
        if phase == Phase::Queued && inner.provider(&r).is_none() {
            return Err("Usenet queue: captured provider is unavailable".into());
        }
        if action == "retry"
            && inner
                .workspaces
                .get(id)
                .and_then(Workspace::first_missing)
                .is_some_and(|n| r.attempts[n as usize - 1] >= r.limit)
        {
            return Err("Usenet queue: retained attempt budget is exhausted".into());
        }
        let guard = inner.guard(Value::Array(vec![
            "control".into(),
            id.into(),
            action.into(),
        ]));
        let mut v = Value::object();
        v.insert("id", id);
        v.insert("action", action);
        v.insert("state", phase.name());
        v.insert("plan_id", guard.clone());
        v.insert("applied", false);
        v.insert("library_admitted", false);
        if review.apply {
            if self.stopped.load(Ordering::Acquire) {
                return Err("Usenet queue: service is stopped".into());
            }
            inner.check_review(review, &guard)?;
            let mut next = inner.data.clone();
            let revision = next.next()?;
            let row = next
                .records
                .get_mut(id)
                .ok_or("Usenet queue: transfer is absent")?;
            row.phase = phase;
            row.reservation = None;
            row.revision = revision;
            row.next_attempt = 0;
            row.error = None;
            inner.persist(next)?;
            v.insert("applied", true);
        }
        Ok(v)
    }
    /// Returns a private path only after rechecking the complete workspace proof.
    /// This does not authorize an import, request admission, or library ownership.
    pub fn verified_file(&self, id: &str) -> Result<PathBuf> {
        let mut inner = self.lock()?;
        if inner.poisoned
            || inner
                .data
                .records
                .get(id)
                .is_none_or(|r| r.phase != Phase::Complete)
        {
            return Err("Usenet queue: verified file is unavailable".into());
        }
        let w = inner
            .workspaces
            .get_mut(id)
            .ok_or("Usenet queue: workspace is unavailable")?;
        let result = w.verify_ready();
        if result.is_err() {
            inner.poisoned = true;
        }
        result
    }
    pub fn tick(&self) -> Result<bool> {
        if self.stopped.load(Ordering::Acquire) {
            return Ok(false);
        }
        let (changed, work) = self.claim()?;
        let Some(mut work) = work else {
            return Ok(changed);
        };
        let _active = Active {
            inner: self.inner.clone(),
            id: work.id.clone(),
        };
        let result = if work.part == 0 {
            work.workspace.assemble().map(|_| None)
        } else {
            nntp::body(
                work.server
                    .as_ref()
                    .ok_or("Usenet queue: captured provider is absent")?,
                &work.article,
            )
            .and_then(|body| yenc::decode(&body))
            .and_then(|p| {
                if p.number() != work.part {
                    return Err("Usenet queue: article part differs from reservation".into());
                }
                Ok(Some(p))
            })
        };
        let mut inner = self.lock()?;
        let current = inner
            .data
            .records
            .get(&work.id)
            .ok_or("Usenet queue: active record is absent")?;
        let valid = !self.stopped.load(Ordering::Acquire)
            && !inner.poisoned
            && current
                .reservation
                .as_ref()
                .is_some_and(|r| r.token == work.token && r.part == work.part)
            && matches!(current.phase, Phase::Downloading | Phase::Verifying);
        if !valid {
            inner.workspaces.insert(work.id.clone(), work.workspace);
            return Ok(true);
        }
        let mut next = inner.data.clone();
        let revision = next.next()?;
        let row = next
            .records
            .get_mut(&work.id)
            .ok_or("Usenet queue: active record is absent")?;
        row.reservation = None;
        row.revision = revision;
        row.next_attempt = 0;
        match result {
            Ok(Some(p)) => {
                if let Err(e) = work.workspace.accept(&p, &work.article) {
                    inner.poisoned = true;
                    inner.workspaces.insert(work.id.clone(), work.workspace);
                    return Err(e);
                }
                row.phase = Phase::Queued;
                row.error = None;
            }
            Ok(None) => {
                row.phase = Phase::Complete;
                row.error = None;
            }
            Err(_) => {
                row.error = Some(
                    if work.part == 0 {
                        "verification_failed"
                    } else {
                        "article_failed"
                    }
                    .into(),
                );
                if work.part == 0 || row.attempts[work.part as usize - 1] >= row.limit {
                    row.phase = Phase::Failed;
                } else {
                    row.phase = Phase::Queued;
                    row.next_attempt = store::now()
                        .saturating_add(1_u64 << row.attempts[work.part as usize - 1].min(5));
                }
            }
        }
        let recovery = work.workspace.report().get("phase").and_then(Value::as_str)
            == Some("recovery_required");
        inner.verified.insert(
            work.id.clone(),
            work.workspace.verified_numbers().count() as u32,
        );
        inner.workspaces.insert(work.id.clone(), work.workspace);
        if recovery {
            inner.poisoned = true;
            return Err("Usenet queue: workspace requires recovery".into());
        }
        inner.persist(next)?;
        Ok(true)
    }
    fn claim(&self) -> Result<(bool, Option<Work>)> {
        let mut inner = self.lock()?;
        inner.writable()?;
        if inner.active.len() >= inner.settings.max_active {
            return Ok((false, None));
        }
        let mut candidates = inner
            .data
            .records
            .values()
            .filter(|r| {
                r.phase == Phase::Queued
                    && r.next_attempt <= store::now()
                    && !inner.active.contains(&r.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by_key(|r| r.order);
        for r in candidates {
            let w = inner
                .workspaces
                .get(&r.id)
                .ok_or("Usenet queue: queued workspace is absent")?;
            let part = w.first_missing().unwrap_or(0);
            let provider = inner.provider(&r);
            let failure = if provider.is_none() {
                Some((Phase::Paused, "provider_removed"))
            } else if part != 0 && r.attempts[part as usize - 1] >= r.limit {
                Some((Phase::Failed, "attempts_exhausted"))
            } else {
                None
            };
            if let Some((phase, error)) = failure {
                let mut next = inner.data.clone();
                let revision = next.next()?;
                let row = next
                    .records
                    .get_mut(&r.id)
                    .ok_or("Usenet queue: queued record is absent")?;
                row.phase = phase;
                row.error = Some(error.into());
                row.revision = revision;
                row.next_attempt = 0;
                inner.persist(next)?;
                return Ok((true, None));
            }
            if part != 0
                && provider
                    .as_ref()
                    .is_some_and(|s| s.health.try_lock().is_err())
            {
                continue;
            }
            let article = if part == 0 {
                String::new()
            } else {
                inner.sources[&r.source].nzb.files[r.file_index].segments[part as usize - 1]
                    .message_id
                    .clone()
            };
            let token = digest(&random_bytes::<16>()?);
            let mut next = inner.data.clone();
            let revision = next.next()?;
            let row = next
                .records
                .get_mut(&r.id)
                .ok_or("Usenet queue: queued record is absent")?;
            row.phase = if part == 0 {
                Phase::Verifying
            } else {
                Phase::Downloading
            };
            row.reservation = Some(Reservation {
                part,
                token: token.clone(),
            });
            row.revision = revision;
            row.next_attempt = 0;
            if part != 0 {
                row.attempts[part as usize - 1] += 1;
            }
            // An attempt is consumed durably before opening an NNTP connection.
            inner.persist(next)?;
            let workspace = inner
                .workspaces
                .remove(&r.id)
                .ok_or("Usenet queue: reserved workspace is absent")?;
            inner.active.insert(r.id.clone());
            return Ok((
                true,
                Some(Work {
                    id: r.id,
                    token,
                    part,
                    article,
                    server: provider,
                    workspace,
                }),
            ));
        }
        Ok((false, None))
    }
}
struct Work {
    id: String,
    token: String,
    part: u32,
    article: String,
    server: Option<Server>,
    workspace: Workspace,
}
struct Active {
    inner: Arc<Mutex<Inner>>,
    id: String,
}
impl Drop for Active {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.active.remove(&self.id);
        }
    }
}
