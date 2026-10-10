//! Durable orchestration. Locks never cover imports or Plex requests.
use crate::{
    Result,
    config::Config,
    integrations,
    json::Value,
    media, organizer,
    store::{self, Job, RecordedRelease, Request, Store},
    torrent::{Client, FilePriority, SelectionUpdate, TransferPolicy},
};
use std::{
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub struct Engine {
    pub config: Config,
    pub store: Mutex<Store>,
    pub(crate) series_store: Mutex<crate::series::SeriesStore>,
    pub(crate) series_refresh_lock: Mutex<()>,
    downloads: Option<Client>,
    pub(crate) sync_lock: Mutex<()>,
    pub(crate) requester_store: Mutex<crate::requesters::RequesterStore>,
    pub(crate) irc_store: Mutex<crate::irc::AnnouncementStore>,
    pub(crate) irc_runtime: Mutex<crate::irc::client::Runtime>,
    pub(crate) irc_route_runtime: Mutex<crate::irc::routing::Runtime>,
    pub(crate) irc_route_lock: Mutex<()>,
    pub(crate) indexer_store: Mutex<crate::indexers::policy::SourceStore>,
    pub(crate) upgrade_lock: Mutex<()>,
    pub(crate) read_only: bool,
    pub stopped: AtomicBool,
    pub last_sync_error: Mutex<Option<String>>,
    pub last_upgrade_error: Mutex<Option<String>>,
    pub last_series_error: Mutex<Option<String>>,
}

pub fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| "Internal lock poisoned".to_owned())
}

impl Engine {
    pub fn open(config: Config) -> Result<Arc<Self>> {
        Self::open_with_downloads(config, true)
    }

    /// Opens the journal for explicit management without starting native transfers.
    pub fn open_for_management(config: Config) -> Result<Arc<Self>> {
        Self::open_with_downloads(config, false)
    }

    /// Loads existing storage without creating, repairing, or changing its permissions.
    pub fn open_for_preview(config: Config) -> Result<Arc<Self>> {
        let store = Store::open_read_only(&config.store_dir)?;
        Self::from_store(config, store, false, true)
    }

    fn open_with_downloads(config: Config, start_downloads: bool) -> Result<Arc<Self>> {
        let store = Store::prepare(&config.store_dir)?;
        Self::from_store(config, store, start_downloads, false)
    }

    fn from_store(
        config: Config,
        mut store: Store,
        start_downloads: bool,
        read_only: bool,
    ) -> Result<Arc<Self>> {
        let mut indexer_store =
            crate::indexers::policy::SourceStore::open(&config.store_dir, &config, read_only)?;
        let series_store = crate::series::SeriesStore::open(&config.store_dir, read_only)?;
        let mut requester_store =
            crate::requesters::RequesterStore::open(&config.store_dir, &config, read_only)?;
        let mut irc_store =
            crate::irc::AnnouncementStore::open(&config.store_dir, &config.irc, read_only)?;
        crate::requesters::engine::validate_storage(&requester_store.state, &store)?;
        let irc_admitted =
            crate::irc::admission::recovered_state(&irc_store.state, &requester_store.state)?;
        let irc_recovered = crate::irc::routing::recovered_state(&irc_admitted, &store)?;
        let irc_recovery_changed = irc_recovered != irc_store.state;
        if !read_only {
            store.initialize()?;
        }
        indexer_store.initialize(&config)?;
        requester_store.initialize()?;
        irc_store.initialize()?;
        if !read_only && irc_recovery_changed {
            irc_store.save(irc_recovered)?;
        }
        if !read_only {
            crate::requesters::engine::cancel_unwanted(
                &requester_store.state,
                &mut store,
                &config,
            )?;
        }
        let downloads = if start_downloads && config.downloads_enabled {
            let client =
                Client::open_with_policy(config.downloads.clone(), config.download_policy.clone())?;
            let jobs = store.list();
            let active: std::collections::BTreeSet<_> = jobs
                .iter()
                .filter(|job| job.state != "cancelled")
                .filter_map(|job| {
                    job.download_id.as_deref().or_else(|| {
                        job.shared_file
                            .as_ref()
                            .map(|file| file.torrent_id.as_str())
                    })
                })
                .collect();
            let known: std::collections::BTreeSet<_> = client
                .statuses()?
                .into_iter()
                .map(|status| status.id)
                .collect();
            for id in jobs
                .iter()
                .filter(|job| job.state == "cancelled")
                .filter_map(|job| job.download_id.as_deref())
            {
                if !active.contains(id) && known.contains(id) {
                    client.cancel(id)?;
                }
            }
            Some(client)
        } else {
            None
        };
        let irc_runtime = crate::irc::client::Runtime::new(&config.irc.sources);
        Ok(Arc::new(Self {
            config,
            store: Mutex::new(store),
            series_store: Mutex::new(series_store),
            series_refresh_lock: Mutex::new(()),
            downloads,
            sync_lock: Mutex::new(()),
            requester_store: Mutex::new(requester_store),
            irc_store: Mutex::new(irc_store),
            irc_runtime: Mutex::new(irc_runtime),
            irc_route_runtime: Mutex::new(crate::irc::routing::Runtime::default()),
            irc_route_lock: Mutex::new(()),
            indexer_store: Mutex::new(indexer_store),
            upgrade_lock: Mutex::new(()),
            read_only,
            stopped: AtomicBool::new(false),
            last_sync_error: Mutex::new(None),
            last_upgrade_error: Mutex::new(None),
            last_series_error: Mutex::new(None),
        }))
    }

    pub fn submit(&self, request: Request) -> Result<Vec<Job>> {
        if request.kind == "series" {
            let record = self.track_series(&request, false, false)?;
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .ok_or("Missing series identity")?;
            let series = lock(&self.series_store)?
                .get(id)
                .ok_or("Missing series record")?;
            let keys: std::collections::BTreeSet<_> = series
                .plan
                .episodes
                .iter()
                .map(|episode| series.episode_request(episode).media_key())
                .collect();
            let jobs = lock(&self.store)?
                .list()
                .into_iter()
                .filter(|job| keys.contains(&job.request.media_key()))
                .take(64)
                .collect::<Vec<_>>();
            self.requester_operator_interest(&jobs)?;
            return Ok(jobs);
        }
        let requests = integrations::expand(&self.config, &request)?;
        let jobs = {
            let mut store = lock(&self.store)?;
            requests
                .into_iter()
                .map(|r| store.submit(r))
                .collect::<Result<Vec<_>>>()?
        };
        self.requester_operator_interest(&jobs)?;
        Ok(jobs)
    }

    pub fn sync(&self) -> Result<usize> {
        if !self.config.requesters.accounts.is_empty() {
            let before = lock(&self.store)?.list().len();
            self.sync_requesters()?;
            return Ok(lock(&self.store)?.list().len().saturating_sub(before));
        }
        let _guard = lock(&self.sync_lock)?;
        let requests = integrations::watchlist_identities(&self.config)?;
        let before = lock(&self.store)?.list().len();
        for request in requests {
            if request.kind == "series" {
                self.track_series(&request, false, false)?;
            } else {
                self.submit(request)?;
            }
        }
        Ok(lock(&self.store)?.list().len().saturating_sub(before))
    }

    pub fn cancel(&self, id: &str) -> Result<Job> {
        let (job, shared) = {
            let mut store = lock(&self.store)?;
            let job = store.cancel(id)?;
            let shared = store.list().iter().any(|other| {
                other.id != id
                    && other.state != "cancelled"
                    && ((other.download_id.is_some() && other.download_id == job.download_id)
                        || job.shared_file.as_ref().is_some_and(|file| {
                            other
                                .shared_file
                                .as_ref()
                                .is_some_and(|known| known.torrent_id == file.torrent_id)
                        }))
            });
            (job, shared)
        };
        let transfer_id = job.download_id.as_deref().or_else(|| {
            job.shared_file
                .as_ref()
                .map(|file| file.torrent_id.as_str())
        });
        if !shared
            && let (Some(client), Some(id)) = (&self.downloads, transfer_id)
            && client.statuses()?.iter().any(|transfer| transfer.id == id)
        {
            client.cancel(id)?;
        }
        Ok(job)
    }

    pub fn retry(&self, id: &str) -> Result<Job> {
        self.requester_retry_allowed(id)?;
        let mut store = lock(&self.store)?;
        let previous = store.get(id).ok_or("Unknown job")?;
        let job = store.retry(id)?;
        if job.irc_origin.is_some() {
            drop(store);
            if let (Some(client), Some(id)) = (&self.downloads, &job.download_id)
                && client.statuses()?.iter().any(|s| &s.id == id)
            {
                client.resume_if_allowed(id)?;
            }
            return Ok(job);
        }
        if job.shared_upgrade.is_some() {
            let transfer_id = job
                .shared_file
                .as_ref()
                .ok_or("Missing shared replacement binding")?
                .torrent_id
                .clone();
            drop(store);
            if let Some(client) = &self.downloads
                && client
                    .statuses()?
                    .iter()
                    .any(|status| status.id == transfer_id)
            {
                client.resume_if_allowed(&transfer_id)?;
            }
            return Ok(job);
        }
        if job.imports.is_empty()
            && job.request.source_path.is_none()
            && job.request.source_url.is_none()
        {
            let old_id = previous.download_id;
            let shared = old_id.as_ref().is_some_and(|id| {
                store.list().iter().any(|other| {
                    other.id != job.id
                        && other.download_id.as_ref() == Some(id)
                        && other.state != "cancelled"
                })
            });
            drop(store);
            if !shared
                && let (Some(client), Some(old_id)) = (&self.downloads, old_id)
                && client.statuses()?.iter().any(|status| status.id == old_id)
            {
                client.cancel(&old_id)?;
            }
        }
        Ok(job)
    }

    fn native_client(&self) -> Result<&Client> {
        self.downloads
            .as_ref()
            .ok_or_else(|| "Native downloads are disabled".to_owned())
    }

    /// Native transfer controls affect every request sharing that torrent identity.
    pub fn transfer(&self, id: &str) -> Result<Value> {
        let mut value = self.native_client()?.transfer(id)?;
        let requests = lock(&self.store)?
            .list()
            .into_iter()
            .filter(|job| job.download_id.as_deref() == Some(id))
            .map(|job| Value::String(job.id))
            .collect();
        value.insert("request_ids", Value::Array(requests));
        Ok(value)
    }

    pub fn transfers(&self) -> Result<Value> {
        let snapshots = self.native_client()?.transfers()?;
        let jobs = lock(&self.store)?.list();
        let mut references: std::collections::BTreeMap<String, Vec<Value>> =
            std::collections::BTreeMap::new();
        for job in jobs {
            if let Some(id) = job.download_id {
                references
                    .entry(id)
                    .or_default()
                    .push(Value::String(job.id));
            }
        }
        let mut snapshots = snapshots
            .as_array()
            .ok_or("Invalid native transfer snapshot")?
            .to_vec();
        for value in &mut snapshots {
            let id = value
                .get("id")
                .and_then(Value::as_str)
                .ok_or("Missing native transfer identity")?;
            value.insert(
                "request_ids",
                Value::Array(references.remove(id).unwrap_or_default()),
            );
        }
        Ok(Value::Array(snapshots))
    }

    pub fn pause_transfer(&self, id: &str) -> Result<Value> {
        self.native_client()?.pause(id)?;
        self.transfer(id)
    }

    pub fn resume_transfer(&self, id: &str) -> Result<Value> {
        self.native_client()?.resume(id)?;
        self.transfer(id)
    }

    pub fn set_transfer_priority(&self, id: &str, priority: i32) -> Result<Value> {
        self.native_client()?.set_priority(id, priority)?;
        self.transfer(id)
    }

    pub fn set_file_priority(
        &self,
        id: &str,
        index: usize,
        priority: FilePriority,
    ) -> Result<Value> {
        self.native_client()?
            .set_file_priority(id, index, priority)?;
        self.transfer(id)
    }

    pub fn set_transfer_policy(&self, id: &str, policy: Option<TransferPolicy>) -> Result<Value> {
        self.native_client()?.set_policy(id, policy)?;
        self.transfer(id)
    }

    pub fn select_transfer_files(&self, id: &str, update: &SelectionUpdate) -> Result<Value> {
        self.native_client()?.select_files(id, update)?;
        self.transfer(id)
    }

    pub fn status(&self) -> Result<Value> {
        let (jobs, maintenance_error) = {
            let store = lock(&self.store)?;
            (store.list(), store.maintenance_error().map(str::to_owned))
        };
        let mut v = Value::object();
        v.insert("version", env!("CARGO_PKG_VERSION"));
        v.insert("jobs", Value::Number(jobs.len() as f64));
        v.insert(
            "active",
            Value::Number(
                jobs.iter()
                    .filter(|j| !matches!(j.state.as_str(), "ready" | "failed" | "cancelled"))
                    .count() as f64,
            ),
        );
        v.insert(
            "ready",
            Value::Number(jobs.iter().filter(|j| j.state == "ready").count() as f64),
        );
        v.insert("stopped", self.stopped.load(Ordering::Acquire));
        v.insert(
            "maintenance_error",
            maintenance_error.map_or(Value::Null, Value::String),
        );
        v.insert(
            "last_sync_error",
            lock(&self.last_sync_error)?
                .clone()
                .map_or(Value::Null, Value::String),
        );
        v.insert("monitoring_enabled", self.config.monitoring.enabled);
        v.insert(
            "monitored_series",
            lock(&self.series_store)?
                .list()
                .iter()
                .filter(|record| record.monitored)
                .count() as u32,
        );
        v.insert(
            "last_series_error",
            lock(&self.last_series_error)?
                .clone()
                .map_or(Value::Null, Value::String),
        );
        v.insert(
            "last_upgrade_error",
            lock(&self.last_upgrade_error)?
                .clone()
                .map_or(Value::Null, Value::String),
        );
        if let Some(client) = &self.downloads {
            v.insert("peer_port", client.listen_port() as u32);
            let statuses = client.statuses()?;
            v.insert("torrents", Value::Number(statuses.len() as f64));
            let mut downloaded = 0_u64;
            let mut uploaded = 0_u64;
            for status in statuses {
                let (d, u) = client.transfer_stats(&status.id)?;
                downloaded = downloaded.saturating_add(d);
                uploaded = uploaded.saturating_add(u);
            }
            v.insert("downloaded_bytes", downloaded.to_string());
            v.insert("uploaded_bytes", uploaded.to_string());
        }
        Ok(v)
    }

    pub fn start(self: &Arc<Self>) -> Workers {
        let mut handles = Vec::new();
        crate::irc::client::start(self, &mut handles);
        crate::irc::routing::start(self, &mut handles);
        if self.config.catalog.enabled {
            let engine = self.clone();
            handles.push(thread::spawn(move || {
                while !engine.stopped.load(Ordering::Acquire) {
                    let error = engine.refresh_series_due().err();
                    if let Ok(mut current) = engine.last_series_error.lock() {
                        *current = error;
                    }
                    engine.wait(60_000);
                }
            }));
        }
        for _ in 0..self.config.workers {
            let engine = self.clone();
            handles.push(thread::spawn(move || {
                while !engine.stopped.load(Ordering::Acquire) {
                    match engine.tick() {
                        Ok(true) => {}
                        Ok(false) => engine.wait(engine.config.poll_interval_ms),
                        Err(error) => {
                            eprintln!("mynou: {error}");
                            engine.wait(1000);
                        }
                    }
                }
            }));
        }
        if self.config.plex.enabled || !self.config.requesters.accounts.is_empty() {
            let engine = self.clone();
            handles.push(thread::spawn(move || {
                while !engine.stopped.load(Ordering::Acquire) {
                    let error = engine.sync().err();
                    if let Ok(mut current) = engine.last_sync_error.lock() {
                        *current = error;
                    }
                    engine.wait(60_000);
                }
            }));
        }
        if self.config.monitoring.enabled {
            let engine = self.clone();
            handles.push(thread::spawn(move || {
                while !engine.stopped.load(Ordering::Acquire) {
                    let error = engine.check_due_upgrades().err();
                    if let Ok(mut current) = engine.last_upgrade_error.lock() {
                        *current = error;
                    }
                    engine.wait(60_000);
                }
            }));
        }
        Workers {
            engine: self.clone(),
            handles,
        }
    }

    pub(crate) fn wait(&self, millis: u64) {
        for _ in 0..millis.div_ceil(100) {
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(millis.min(100)));
        }
    }

    /// Run one step and release the lease before waiting for the next poll.
    pub fn tick(self: &Arc<Self>) -> Result<bool> {
        let Some(mut job) = self.claim_requester_job()? else {
            return Ok(false);
        };
        let lease = job.lease_id.clone().ok_or("Missing lease")?;
        let heartbeat = Heartbeat::start(self.clone(), job.id.clone(), lease.clone());
        let result = self.advance(&mut job, &heartbeat.active);
        heartbeat.finish();
        // Cancellation or a new lease owner makes this result stale.
        let mut store = lock(&self.store)?;
        let Some(current) = store.get(&job.id) else {
            return Ok(true);
        };
        if current.lease_id.as_deref() != Some(&lease) || current.lease_until <= store::now() {
            return Ok(true);
        }
        let result = result.and_then(|()| store.check_ready_promotion(&job));
        if let Err(error) = result {
            job.attempts = job.attempts.saturating_add(1);
            job.last_error = Some(error);
            job.state = "failed".into();
            job.next_attempt_at = if job.attempts < self.config.max_attempts {
                store::now().saturating_add((1_u64 << job.attempts.min(10)).min(900))
            } else {
                0
            };
        }
        store.update(job.clone())?;
        if !matches!(job.state.as_str(), "ready" | "failed" | "cancelled") {
            store.release_lease(&job.id, &lease)?;
        }
        drop(store);
        self.requester_reconcile()?;
        Ok(true)
    }

    pub(crate) fn pause_unwanted_transfers(&self, store: &Store) -> Result<()> {
        let Some(client) = &self.downloads else {
            return Ok(());
        };
        let jobs = store.list();
        let active: std::collections::BTreeSet<_> = jobs
            .iter()
            .filter(|j| j.state != "cancelled")
            .filter_map(|j| {
                j.download_id
                    .as_deref()
                    .or_else(|| j.shared_file.as_ref().map(|f| f.torrent_id.as_str()))
            })
            .collect();
        let known: std::collections::BTreeSet<_> =
            client.statuses()?.into_iter().map(|s| s.id).collect();
        for id in jobs
            .iter()
            .filter(|j| j.state == "cancelled")
            .filter_map(|j| {
                j.download_id
                    .as_deref()
                    .or_else(|| j.shared_file.as_ref().map(|f| f.torrent_id.as_str()))
            })
        {
            if !active.contains(id) && known.contains(id) {
                client.cancel(id)?;
            }
        }
        Ok(())
    }

    fn import_version(
        &self,
        source: &Path,
        root: &Path,
        job: &Job,
        active: &AtomicBool,
    ) -> Result<std::path::PathBuf> {
        if let Some(file) = &job.shared_file {
            organizer::import_shared_file_cancellable(source, root, file, active)
        } else if job.upgrade_parent.is_some() {
            organizer::import_versioned_file_cancellable(
                source,
                root,
                &job.request,
                &job.id,
                active,
            )
        } else {
            organizer::import_file_cancellable(source, root, &job.request, active)
        }
    }

    fn advance(&self, job: &mut Job, active: &AtomicBool) -> Result<()> {
        let config = self.configuration_for(job);
        if !active.load(Ordering::Acquire) {
            return Err("Processing interrupted".into());
        }
        job.last_error = None;
        if job.state == "processing"
            && job.imports.is_empty()
            && let Some(id) = &job.download_id
        {
            self.downloads
                .as_ref()
                .ok_or("Downloads are disabled")?
                .resume_if_allowed(id)?;
        }
        if config.plex.enabled
            && job.imports.is_empty()
            && job.files.is_empty()
            && job.acquisition_url.is_none()
            && job.request.source_path.is_none()
            && job.request.source_url.is_none()
            && if job.requester.is_some() {
                integrations::available_destination(&config, &job.request)?
            } else {
                integrations::available(&config, &job.request)?
            }
        {
            job.state = "ready".into();
            job.progress = 1.0;
            job.next_attempt_at = 0;
            return Ok(());
        }
        if crate::irc::routing::selection_from_irc(
            &self.config,
            job,
            &lock(&self.requester_store)?.state,
        ) {
            job.state = "queued".into();
            job.next_attempt_at = store::now().saturating_add(60);
            return Ok(());
        }
        // Resume a confirmed import after interruption without copying it again.
        if !job.imports.is_empty() {
            if job.shared_file.is_some() {
                for path in &job.imports {
                    store::reject_symlinks(Path::new(path))?;
                    if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
                        return Err(
                            "Recorded shared import is missing or is not a regular file".into()
                        );
                    }
                }
            }
            if config.plex.enabled {
                if job.state != "scanning" {
                    integrations::refresh(&config, &job.request)?;
                }
                job.state = "scanning".into();
                let available = if job.upgrade_parent.is_some()
                    || job.shared_file.is_some()
                    || job.requester.is_some()
                {
                    integrations::available_import(&config, &job.request, &job.imports)?
                } else {
                    integrations::available(&config, &job.request)?
                };
                if !available {
                    job.next_attempt_at = store::now().saturating_add(5);
                    return Ok(());
                }
            }
            job.state = "ready".into();
            job.progress = 1.0;
            job.next_attempt_at = 0;
            return Ok(());
        }
        if job.imports.is_empty() && !job.files.is_empty() && job.download_id.is_some() {
            let client = self.downloads.as_ref().ok_or("Downloads are disabled")?;
            let id = job.download_id.as_deref().ok_or("Missing download")?;
            let status = if let Some(origin) = &job.irc_origin {
                client.require_bound_files(
                    id,
                    std::slice::from_ref(&origin.file),
                    &origin.torrent_id,
                )?
            } else if let Some(path) = &job.pack_file {
                if let Some(file) = &job.shared_file {
                    client.require_bound_files(id, std::slice::from_ref(path), &file.torrent_id)?
                } else if let Some(origin) = &job.pack_origin {
                    client.require_bound_files(
                        id,
                        std::slice::from_ref(path),
                        &origin.torrent_id,
                    )?
                } else {
                    client.require_files(id, std::slice::from_ref(path))?
                }
            } else {
                client.require_all(id)?
            };
            if !self.source_files_available(job, &status) {
                job.state = "downloading".into();
                job.progress = status.progress;
                job.next_attempt_at = store::now().saturating_add(1);
                return Ok(());
            }
        }
        if job.files.is_empty() {
            if let Some(path) = &job.request.source_path {
                job.files.push(path.clone());
            } else {
                let client = self.downloads.as_ref().ok_or("Downloads are disabled")?;
                if job.acquisition_url.is_none() {
                    job.acquisition_url = Some(match &job.request.source_url {
                        Some(url) => url.clone(),
                        None => {
                            let selected = integrations::select_release(&config, &job.request)?;
                            job.release = Some(RecordedRelease {
                                title: selected.title,
                                profile: selected.profile,
                            });
                            selected.url
                        }
                    });
                    // Persist the selection before performing network operations.
                    lock(&self.store)?.update(job.clone())?;
                }
                let status = if let Some(id) = &job.download_id {
                    if let Some(origin) = &job.irc_origin {
                        client.require_bound_files(
                            id,
                            std::slice::from_ref(&origin.file),
                            &origin.torrent_id,
                        )?
                    } else if let Some(path) = &job.pack_file {
                        if let Some(file) = &job.shared_file {
                            client.require_bound_files(
                                id,
                                std::slice::from_ref(path),
                                &file.torrent_id,
                            )?
                        } else if let Some(origin) = &job.pack_origin {
                            client.require_bound_files(
                                id,
                                std::slice::from_ref(path),
                                &origin.torrent_id,
                            )?
                        } else {
                            client.require_files(id, std::slice::from_ref(path))?
                        }
                    } else {
                        client.require_all(id)?
                    }
                } else {
                    // Fetch metadata before acquiring the client internal lock.
                    let source = job.acquisition_url.as_deref().ok_or("Missing source")?;
                    if let Some(origin) = &job.irc_origin {
                        client.ensure_bound_files(
                            source,
                            std::slice::from_ref(&origin.file),
                            &origin.torrent_id,
                        )?
                    } else if let Some(path) = &job.pack_file {
                        if let Some(file) = &job.shared_file {
                            client.ensure_bound_files(
                                source,
                                std::slice::from_ref(path),
                                &file.torrent_id,
                            )?
                        } else if let Some(origin) = &job.pack_origin {
                            client.ensure_bound_files(
                                source,
                                std::slice::from_ref(path),
                                &origin.torrent_id,
                            )?
                        } else {
                            client.ensure_files(source, std::slice::from_ref(path))?
                        }
                    } else {
                        client.ensure(source)?
                    }
                };
                if job
                    .shared_file
                    .as_ref()
                    .is_some_and(|file| file.torrent_id != status.id)
                {
                    return Err("Shared transfer uses another authenticated alias; ownership cannot be rebound".into());
                }
                job.download_id = Some(status.id.clone());
                if let Some(origin) = &job.irc_origin
                    && !origin.torrent_aliases.contains(&status.id)
                {
                    return Err("IRC: native transfer has another authenticated identity".into());
                }
                job.progress = status.progress;
                job.state = "downloading".into();
                // ensure may fetch a URL before creating the transfer. Cancellation
                // during that fetch must also pause the resulting transfer,
                // without waiting for the next heartbeat.
                {
                    let mut store = lock(&self.store)?;
                    let current = store.get(&job.id).ok_or("Missing job")?;
                    if current.lease_id != job.lease_id || current.lease_until <= store::now() {
                        if current.state == "cancelled"
                            && current.acquisition_url == job.acquisition_url
                            && current.download_id.is_none()
                        {
                            let mut cancelled = current;
                            cancelled.download_id = Some(status.id.clone());
                            store.update(cancelled)?;
                        }
                        let shared = store.list().iter().any(|other| {
                            other.state != "cancelled"
                                && (other.download_id.as_deref() == Some(&status.id)
                                    || other
                                        .shared_file
                                        .as_ref()
                                        .is_some_and(|file| file.torrent_id == status.id)
                                    || (other.acquisition_url.is_some()
                                        && other.acquisition_url == job.acquisition_url))
                        });
                        if !shared {
                            // Only local writes: no network requests while
                            // this decision is protected by the lock.
                            client.cancel(&status.id)?;
                        }
                        return Err("Processing lease expired".into());
                    }
                    // Publish the identity before continuing so cancel
                    // can always find the transfer associated with this job.
                    store.update(job.clone())?;
                }
                if !self.source_files_available(job, &status) {
                    job.next_attempt_at = store::now().saturating_add(1);
                    return Ok(());
                }
                let mapped = job
                    .pack_file
                    .as_ref()
                    .or_else(|| job.irc_origin.as_ref().map(|o| &o.file))
                    .map(|path| config.downloads.data_dir.join(&status.id).join(path));
                job.files = status
                    .available_files
                    .iter()
                    .filter(|path| mapped.as_ref().is_none_or(|expected| *path == expected))
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                if mapped.is_some() && job.files.len() != 1 {
                    return Err(
                        "Mapped torrent file is absent or ambiguous in the verified payload".into(),
                    );
                }
            }
        }
        if let Some(path) = job
            .pack_file
            .as_ref()
            .or_else(|| job.irc_origin.as_ref().map(|o| &o.file))
        {
            let id = job
                .download_id
                .as_ref()
                .ok_or("Verified file mapping requires a native transfer")?;
            let expected = config.downloads.data_dir.join(id).join(path);
            if job.files.len() != 1 || Path::new(&job.files[0]) != expected {
                return Err("Mapped torrent file differs from the retained verified path".into());
            }
        }
        // Group ownership serializes claims. Organizer compares existing bytes and
        // publishes one destination, including recovery after a pre-journal crash.
        job.state = "importing".into();
        lock(&self.store)?.update(job.clone())?;
        let mut candidates = Vec::new();
        for file in &job.files {
            let p = Path::new(file);
            if !active.load(Ordering::Acquire) {
                return Err("Processing interrupted".into());
            }
            // Ignore auxiliary torrent files and analyze each supported media file.
            let ext = p
                .extension()
                .and_then(|v| v.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if [
                "mp4", "m4v", "mov", "mkv", "webm", "avi", "wav", "flac", "mp3",
            ]
            .contains(&ext.as_str())
            {
                let analysis = media::analyze(p)?;
                if matches!(job.request.kind.as_str(), "movie" | "episode")
                    && analysis.video_streams.is_empty()
                {
                    continue;
                }
                let score = (!analysis.video_streams.is_empty(), analysis.size_bytes);
                candidates.push((score, p));
            }
        }
        // Each request represents one movie or episode. Prefer the largest media
        // file to avoid importing samples or mixing episodes from a pack.
        candidates.sort_by_key(|c| c.0);
        let source = candidates
            .last()
            .ok_or("No supported media found for this request")?
            .1;
        if job.request.kind == "episode" && job.pack_file.is_none() && job.files.len() > 1 {
            let numbering = job.request.source_numbering.unwrap_or(
                crate::numbering::SourceNumber::SeasonEpisode(crate::numbering::EpisodeNumber {
                    season: job.request.season,
                    episode: job.request.episode,
                }),
            );
            let matching: Vec<_> = candidates
                .iter()
                .filter(|(_, path)| {
                    numbering.matches_file(path, job.request.year, &job.request.title)
                })
                .collect();
            if matching.len() != 1 {
                return Err(
                    "Ambiguous pack: exactly one file matching the episode is required".into(),
                );
            }
            let path = matching[0].1;
            let imported = self.import_version(path, &config.series_root, job, active)?;
            job.imports.push(imported.to_string_lossy().into_owned());
        } else {
            let root = if job.request.kind == "episode" {
                &config.series_root
            } else {
                &config.movies_root
            };
            let imported = self.import_version(source, root, job, active)?;
            job.imports.push(imported.to_string_lossy().into_owned());
        }
        job.progress = 1.0;
        job.state = "imported".into();
        job.next_attempt_at = 0;
        // Persist the import before requesting a scan, which resumes on the next poll.
        Ok(())
    }

    fn source_files_available(&self, job: &Job, status: &crate::torrent::DownloadStatus) -> bool {
        job.pack_file
            .as_ref()
            .or_else(|| job.irc_origin.as_ref().map(|o| &o.file))
            .map_or(status.ready, |path| {
                let expected = self.config.downloads.data_dir.join(&status.id).join(path);
                status.available_files.contains(&expected)
            })
    }
}

pub struct Workers {
    engine: Arc<Engine>,
    handles: Vec<JoinHandle<()>>,
}
impl Drop for Workers {
    fn drop(&mut self) {
        self.engine.stopped.store(true, Ordering::Release);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

struct Heartbeat {
    active: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}
impl Heartbeat {
    fn start(engine: Arc<Engine>, id: String, lease: String) -> Self {
        let active = Arc::new(AtomicBool::new(true));
        let done = Arc::new(AtomicBool::new(false));
        let a = active.clone();
        let d = done.clone();
        let handle = thread::spawn(move || {
            let period = Duration::from_secs((engine.config.lease_duration_secs / 3).max(1));
            let mut last = std::time::Instant::now();
            while !d.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(100));
                if engine.stopped.load(Ordering::Acquire) {
                    a.store(false, Ordering::Release);
                    break;
                }
                let valid = (|| -> Result<()> {
                    let ledger = lock(&engine.requester_store)?;
                    let mut s = lock(&engine.store)?;
                    let j = s.get(&id).ok_or("Missing job")?;
                    if j.lease_id.as_deref() != Some(&lease)
                        || j.lease_until <= store::now()
                        || !crate::requesters::engine::interest(&ledger.state, &j, &engine.config)
                    {
                        return Err("Lease or admitted demand lost".into());
                    }
                    if last.elapsed() >= period {
                        s.renew(&id, &lease, store::now(), engine.config.lease_duration_secs)?;
                        last = std::time::Instant::now();
                    }
                    Ok(())
                })();
                if valid.is_err() {
                    a.store(false, Ordering::Release);
                    break;
                }
            }
        });
        Self {
            active,
            done,
            handle: Some(handle),
        }
    }
    fn finish(mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub fn public_job(job: &Job) -> Value {
    let mut value = job.to_json();
    if let Value::Object(map) = &mut value {
        map.remove("lease_id");
        map.remove("lease_until");
        map.remove("acquisition_url");
        if let Some(origin) = &job.irc_origin {
            map.insert("irc_origin".into(), origin.public_json());
        }
        if let Some(Value::String(path)) = map.get_mut("pack_file") {
            *path = integrations::report_text(path, 4096);
        }
        if let Some(Value::Object(origin)) = map.get_mut("pack_origin")
            && let Some(Value::Object(release)) = origin.get_mut("release")
            && let Some(Value::String(title)) = release.get_mut("title")
        {
            *title = integrations::report_text(title, 2_048);
        }
        if let Some(Value::Object(release)) = map.get_mut("release")
            && let Some(Value::String(title)) = release.get_mut("title")
        {
            *title = integrations::report_text(title, 2_048);
        }
        if let Some(Value::Object(request)) = map.get_mut("request")
            && request
                .get("source_url")
                .is_some_and(|v| matches!(v, Value::String(_)))
        {
            request.insert(
                "source_url".into(),
                Value::String("[configured source]".into()),
            );
        }
    }
    value
}
