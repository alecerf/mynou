//! Durable orchestration. Locks never cover imports or Plex requests.
mod startup;
#[cfg(test)]
mod startup_tests;

use crate::{
    Result,
    config::Config,
    integrations::{self, SweepCursor},
    json::Value,
    media, organizer,
    store::{self, Job, RecordedRelease, Request, Store},
    torrent::{Client, FilePriority, SelectionUpdate, TransferPolicy},
};
use std::{
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Watchlist pages read by one single-account sync pass.
const WATCHLIST_PAGES: usize = 8;

/// Bounded native-transfer list query. Transfer cursors belong to the transfer
/// index, independently of job and request-reference cursors.
pub struct TransferPageQuery {
    pub after: Option<String>,
    pub limit: usize,
}

impl TransferPageQuery {
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() > 256 {
            return Err("Transfer page query is too large".into());
        }
        let mut query = Self {
            after: None,
            limit: 50,
        };
        let mut seen = std::collections::BTreeSet::new();
        for pair in text.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').ok_or("Invalid transfer page query")?;
            if !seen.insert(name) {
                return Err("Duplicate transfer page query field".into());
            }
            match name {
                "after"
                    if value.len() == 128 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
                {
                    query.after = Some(value.to_owned());
                }
                "limit" => {
                    query.limit = value
                        .parse()
                        .ok()
                        .filter(|limit| (1..=200).contains(limit) && !value.starts_with('0'))
                        .ok_or("Transfer page limit must be 1 to 200")?;
                }
                "after" => return Err("Invalid transfer page cursor".into()),
                _ => return Err("Unknown transfer page query field".into()),
            }
        }
        Ok(query)
    }
}

pub struct Engine {
    pub config: Config,
    pub store: Arc<Mutex<Store>>,
    pub(crate) series_store: Mutex<crate::series::SeriesStore>,
    pub(crate) series_refresh_lock: Mutex<()>,
    /// Scopes whose records could not be read, skipped until the instant.
    pub(crate) series_unreadable: Mutex<std::collections::BTreeMap<String, std::time::Instant>>,
    downloads: OnceLock<Client>,
    start_downloads: bool,
    startup: Mutex<startup::Startup>,
    startup_runner: Mutex<()>,
    /// Startup validation has not completed: mutations stay refused.
    pub(crate) starting: AtomicBool,
    transfer_sweep: Mutex<Option<String>>,
    pub(crate) sync_lock: Mutex<()>,
    pub(crate) requester_store: Mutex<crate::requesters::RequesterStore>,
    pub(crate) irc_store: Mutex<crate::irc::AnnouncementStore>,
    pub(crate) irc_runtime: Mutex<crate::irc::client::Runtime>,
    pub(crate) irc_route_runtime: Mutex<crate::irc::routing::Runtime>,
    pub(crate) irc_route_lock: Mutex<()>,
    pub(crate) indexer_store: Mutex<crate::indexers::policy::SourceStore>,
    pub(crate) feed_store: Mutex<crate::indexers::feed::FeedStore>,
    pub(crate) feed_runtime: Mutex<crate::indexers::watch::Runtime>,
    pub(crate) feed_lock: Mutex<()>,
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

/// `None` while another thread holds the lock, for reports that must not wait.
fn try_lock<T>(mutex: &Mutex<T>) -> Result<Option<MutexGuard<'_, T>>> {
    match mutex.try_lock() {
        Ok(guard) => Ok(Some(guard)),
        Err(std::sync::TryLockError::WouldBlock) => Ok(None),
        Err(std::sync::TryLockError::Poisoned(_)) => Err("Internal lock poisoned".to_owned()),
    }
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
        Self::from_store(config, store, false, true, false)
    }

    fn open_with_downloads(config: Config, start_downloads: bool) -> Result<Arc<Self>> {
        let store = Store::prepare(&config.store_dir, &config)?;
        Self::from_store(config, store, start_downloads, false, false)
    }

    /// Opens the service journal without waiting for startup validation:
    /// the API answers reads at once, while mutations, workers and native
    /// transfers wait until `start` has completed every bounded startup stage.
    pub fn open_serving(config: Config) -> Result<Arc<Self>> {
        let store = Store::prepare(&config.store_dir, &config)?;
        Self::from_store(config, store, true, false, true)
    }

    fn from_store(
        config: Config,
        store: Store,
        start_downloads: bool,
        read_only: bool,
        defer: bool,
    ) -> Result<Arc<Self>> {
        let indexer_store =
            crate::indexers::policy::SourceStore::open(&config.store_dir, &config, read_only)?;
        let feed_store =
            crate::indexers::feed::FeedStore::open(&config.store_dir, &config, read_only)?;
        let series_store = crate::series::SeriesStore::open(&config.store_dir, read_only)?;
        let requester_store =
            crate::requesters::RequesterStore::open(&config.store_dir, &config, read_only)?;
        let irc_store =
            crate::irc::AnnouncementStore::open(&config.store_dir, &config.irc, read_only)?;
        let store = Arc::new(Mutex::new(store));
        let irc_runtime = crate::irc::client::Runtime::new(&config.irc.sources);
        let engine = Arc::new(Self {
            config,
            store,
            series_store: Mutex::new(series_store),
            series_refresh_lock: Mutex::new(()),
            series_unreadable: Mutex::new(std::collections::BTreeMap::new()),
            downloads: OnceLock::new(),
            start_downloads,
            startup: Mutex::new(startup::Startup::default()),
            startup_runner: Mutex::new(()),
            starting: AtomicBool::new(true),
            transfer_sweep: Mutex::new(None),
            sync_lock: Mutex::new(()),
            requester_store: Mutex::new(requester_store),
            irc_store: Mutex::new(irc_store),
            irc_runtime: Mutex::new(irc_runtime),
            irc_route_runtime: Mutex::new(crate::irc::routing::Runtime::default()),
            irc_route_lock: Mutex::new(()),
            indexer_store: Mutex::new(indexer_store),
            feed_store: Mutex::new(feed_store),
            feed_runtime: Mutex::new(crate::indexers::watch::Runtime::default()),
            feed_lock: Mutex::new(()),
            upgrade_lock: Mutex::new(()),
            read_only,
            stopped: AtomicBool::new(false),
            last_sync_error: Mutex::new(None),
            last_upgrade_error: Mutex::new(None),
            last_series_error: Mutex::new(None),
        });
        if !defer {
            engine.finish_startup()?;
        }
        Ok(engine)
    }

    pub fn submit(&self, request: Request) -> Result<Vec<Job>> {
        if request.kind == "series" {
            let record = self.track_series(&request, false, false)?;
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .ok_or("Missing series identity")?;
            let series = lock(&self.series_store)?
                .get(id)?
                .ok_or("Missing series record")?;
            let keys: std::collections::BTreeSet<_> = series
                .plan
                .episodes
                .iter()
                .map(|episode| series.episode_request(episode).media_key())
                .collect();
            let jobs = {
                let mut store = lock(&self.store)?;
                let mut found = Vec::new();
                for key in &keys {
                    for job in store.media_page(key, None, 64 - found.len())?.0 {
                        if found.len() == 64 {
                            break;
                        }
                        found.push(job);
                    }
                    if found.len() == 64 {
                        break;
                    }
                }
                found
            };
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
            let before = lock(&self.store)?.counts()?.jobs;
            self.sync_requesters()?;
            return Ok(lock(&self.store)?.counts()?.jobs.saturating_sub(before) as usize);
        }
        let _guard = lock(&self.sync_lock)?;
        let deadline = Instant::now() + Duration::from_secs(90);
        let before = lock(&self.store)?.counts()?.jobs;
        let mut cursor = lock(&self.store)?
            .watchlist_cursor()?
            .unwrap_or_else(SweepCursor::start);
        // A bounded number of pages per pass; the durable cursor resumes the
        // sweep in the next pass or after a restart.
        for _ in 0..WATCHLIST_PAGES {
            if self.stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
                break;
            }
            let page = integrations::watchlist_page_before(&self.config, &cursor, deadline)?;
            // Submissions are idempotent, so an interrupted page is read again.
            for (_, request) in page.entries {
                if request.kind == "series" {
                    self.track_series(&request, false, false)?;
                } else {
                    self.submit(request)?;
                }
            }
            // Nothing is removed without requester accounts, so a changed
            // listing only means the next sweep reads it again from the top.
            let finished = page.complete;
            cursor = if finished {
                SweepCursor::start()
            } else {
                page.next
            };
            lock(&self.store)?.save_watchlist_cursor(&cursor)?;
            if finished {
                break;
            }
        }
        Ok(lock(&self.store)?.counts()?.jobs.saturating_sub(before) as usize)
    }

    /// A page of jobs, newest first, with the cursor of the next page.
    pub fn jobs_page(&self, after: Option<&str>, limit: usize) -> Result<Value> {
        let page = lock(&self.store)?.page(after, limit)?;
        Ok(jobs_value(page))
    }

    /// A page of jobs, filtered on the server by `q` and `state`.
    pub fn jobs_query(&self, query: &crate::store::JobQuery) -> Result<Value> {
        jobs_query_value(&mut *lock(&self.store)?, query)
    }

    pub fn cancel(&self, id: &str) -> Result<Job> {
        let (job, shared) = {
            let mut store = lock(&self.store)?;
            let job = store.cancel(id)?;
            let shared = store.has_co_owner(&job)?;
            (job, shared)
        };
        let transfer_id = job.download_id.as_deref().or_else(|| {
            job.shared_file
                .as_ref()
                .map(|file| file.torrent_id.as_str())
        });
        if !shared
            && let (Some(client), Some(id)) = (self.downloads.get(), transfer_id)
            && client.contains(id)?
        {
            client.cancel(id)?;
        }
        Ok(job)
    }

    pub fn retry(&self, id: &str) -> Result<Job> {
        self.requester_retry_allowed(id)?;
        let mut store = lock(&self.store)?;
        store.get(id)?.ok_or("Unknown job")?;
        let job = store.retry(id)?;
        if job.irc_origin.is_some() {
            drop(store);
            if let (Some(client), Some(id)) = (self.downloads.get(), &job.download_id)
                && client.contains(id)?
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
            if let Some(client) = self.downloads.get()
                && client.contains(&transfer_id)?
            {
                client.resume_if_allowed(&transfer_id)?;
            }
            return Ok(job);
        }
        // The retry above already recorded any detached transfer durably.
        self.retire_previous_transfers(&mut store)?;
        Ok(job)
    }

    fn native_client(&self) -> Result<&Client> {
        self.downloads
            .get()
            .ok_or_else(|| "Native downloads are disabled".to_owned())
    }

    /// Native transfer controls affect every request sharing that torrent identity.
    pub fn transfer(&self, id: &str) -> Result<Value> {
        self.transfer_page(id, None, 200)
    }

    pub fn transfer_page(&self, id: &str, after: Option<&str>, limit: usize) -> Result<Value> {
        let mut value = self.native_client()?.transfer(id)?;
        let (jobs, next) = lock(&self.store)?.transfer_jobs_page(id, after, limit)?;
        let requests = jobs
            .into_iter()
            .filter(|job| job.download_id.as_deref() == Some(id))
            .map(|job| Value::String(job.id))
            .collect();
        value.insert("request_ids_next", next.map_or(Value::Null, Value::String));
        value.insert("request_ids", Value::Array(requests));
        Ok(value)
    }

    /// Compatibility array for the first 50 transfers. Use `transfers_page`
    /// to enumerate retained transfers and obtain a continuation cursor.
    pub fn transfers(&self) -> Result<Value> {
        self.transfers_page(None, 50)?
            .get("transfers")
            .cloned()
            .ok_or_else(|| "Invalid native transfer page".into())
    }

    /// One priority/FIFO-ordered page, with point-indexed request references.
    pub fn transfers_page(&self, after: Option<&str>, limit: usize) -> Result<Value> {
        let mut page = self.native_client()?.transfers_page(limit, after)?;
        let Value::Array(snapshots) = page
            .get_mut("transfers")
            .ok_or("Invalid native transfer page")?
        else {
            return Err("Invalid native transfer page".into());
        };
        for value in snapshots {
            let id = value
                .get("id")
                .and_then(Value::as_str)
                .ok_or("Missing native transfer identity")?;
            let (jobs, next) = lock(&self.store)?.transfer_jobs_page(id, None, 200)?;
            let references = jobs
                .into_iter()
                .filter(|job| job.download_id.as_deref() == Some(id))
                .map(|job| Value::String(job.id))
                .collect();
            value.insert("request_ids_next", next.map_or(Value::Null, Value::String));
            value.insert("request_ids", Value::Array(references));
        }
        Ok(page)
    }

    pub fn pause_transfer(&self, id: &str) -> Result<Value> {
        self.native_client()?.pause(id)?;
        self.transfer(id)
    }

    pub fn cross_seed(
        &self,
        source_id: &str,
        request: &crate::torrent::CrossSeedRequest,
    ) -> Result<Value> {
        let status = self.native_client()?.cross_seed(source_id, request)?;
        self.transfer(&status.id)
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

    /// Service status. Readings of a store that a long operation holds, or
    /// whose index failed, are null instead of waiting or failing the whole
    /// report; `storage` says why and `storage_errors` lists the failures.
    pub fn status(&self) -> Result<Value> {
        let mut errors = Vec::new();
        let (counts, maintenance_error) = match try_lock(&self.store)? {
            Some(mut store) => (
                store.counts().map_err(|error| errors.push(error)).ok(),
                store.maintenance_error().map(str::to_owned),
            ),
            None => (None, None),
        };
        let count = |read: fn(&store::Counts) -> u64| {
            counts
                .as_ref()
                .map_or(Value::Null, |counts| Value::Number(read(counts) as f64))
        };
        let mut v = Value::object();
        v.insert("version", env!("CARGO_PKG_VERSION"));
        v.insert(
            "migrations",
            crate::progress::report(&[
                self.config.store_dir.as_path(),
                self.config.downloads.state_dir.as_path(),
            ]),
        );
        v.insert("jobs", count(|counts| counts.jobs));
        v.insert("active", count(|counts| counts.active));
        v.insert("ready", count(|counts| counts.ready));
        v.insert("stopped", self.stopped.load(Ordering::Acquire));
        v.insert("startup", self.startup_status()?);
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
        let (monitored, series_storage) = match try_lock(&self.series_store)? {
            Some(mut series) => (
                series
                    .counts()
                    .map_err(|error| errors.push(error))
                    .map_or(Value::Null, |counts| Value::Number(counts.monitored as f64)),
                series.describe(),
            ),
            None => (Value::Null, crate::inventory::report("busy", None)),
        };
        v.insert("monitored_series", monitored);
        v.insert("series_storage", series_storage);
        v.insert(
            "series_unreadable",
            Value::Number(self.series_unreadable_count() as f64),
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
        v.insert("storage", self.storage()?);
        if let Some(client) = self.downloads.get() {
            v.insert("peer_port", client.listen_port() as u32);
            let stats = client
                .try_aggregate_stats()
                .map_err(|error| errors.push(error))
                .ok()
                .flatten();
            let total = stats.map_or(Value::Null, |(total, _, _)| Value::Number(total as f64));
            v.insert("torrents", total);
            for (name, bytes) in [
                ("downloaded_bytes", stats.map(|(_, down, _)| down)),
                ("uploaded_bytes", stats.map(|(_, _, up)| up)),
            ] {
                v.insert(
                    name,
                    bytes.map_or(Value::Null, |bytes| bytes.to_string().into()),
                );
            }
        }
        v.insert(
            "storage_errors",
            Value::Array(errors.into_iter().map(Value::String).collect()),
        );
        Ok(v)
    }

    /// Per-store index sizes and states, read from each durable root in
    /// constant time; no record is read, so a large inventory costs nothing.
    /// A store busy with a long operation reports `busy` instead of blocking.
    pub fn storage(&self) -> Result<Value> {
        fn read<T>(mutex: &Mutex<T>, report: impl FnOnce(&T) -> Value) -> Result<Value> {
            Ok(try_lock(mutex)?.map_or_else(
                || crate::inventory::report("busy", None),
                |store| report(&store),
            ))
        }
        let mut v = Value::object();
        v.insert("jobs", read(&self.store, Store::storage)?);
        v.insert("series", read(&self.series_store, |s| s.storage())?);
        v.insert("requesters", read(&self.requester_store, |s| s.storage())?);
        v.insert("announcements", read(&self.irc_store, |s| s.storage())?);
        v.insert("sources", read(&self.indexer_store, |s| s.storage())?);
        v.insert("feeds", read(&self.feed_store, |s| s.storage())?);
        v.insert(
            "transfers",
            match self.downloads.get() {
                Some(client) => client.storage()?,
                None => crate::inventory::report("not_open", None),
            },
        );
        Ok(v)
    }

    /// Completes any remaining startup stages on a launcher thread, then
    /// starts the workers there. A startup failure keeps every worker,
    /// mutation and native transfer stopped and is reported by status.
    pub fn start(self: &Arc<Self>) -> Workers {
        let engine = self.clone();
        let launcher = thread::spawn(move || {
            match engine.finish_startup() {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    eprintln!("mynou: startup validation failed: {error}");
                    return;
                }
            }
            let mut handles = Vec::new();
            engine.spawn_workers(&mut handles);
            for handle in handles {
                let _ = handle.join();
            }
        });
        Workers {
            engine: self.clone(),
            handles: vec![launcher],
        }
    }

    fn spawn_workers(self: &Arc<Self>, handles: &mut Vec<JoinHandle<()>>) {
        crate::irc::client::start(self, handles);
        crate::irc::routing::start(self, handles);
        crate::indexers::watch::start(self, handles);
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
        let Some(current) = store.get(&job.id)? else {
            return Ok(true);
        };
        if current.lease_id.as_deref() != Some(&lease) || current.lease_until <= store::now() {
            return Ok(true);
        }
        let result = result.and_then(|()| {
            if job.state == "ready" {
                organizer::check_retained_imports(&job, &self.config)?;
            }
            store.check_ready_promotion(&job)
        });
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

    /// Pauses, without deleting files, each transfer an automatic retry detached
    /// from its request, unless a live request still needs it. The request keeps
    /// the record until this finishes, so an interruption is repaired by the next
    /// call: after any retry, requester reconciliation and startup.
    pub(crate) fn retire_previous_transfers(&self, store: &mut Store) -> Result<()> {
        retire_transfers(self.downloads.get(), store)
    }

    pub(crate) fn pause_unwanted_transfers(&self, store: &mut Store) -> Result<()> {
        let Some(client) = self.downloads.get() else {
            return Ok(());
        };
        cancel_orphaned_transfers(client, store, &mut *lock(&self.transfer_sweep)?)
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
            if job.download_id.is_some() {
                organizer::import_native_versioned_file_cancellable(
                    source,
                    root,
                    &job.request,
                    &job.id,
                    active,
                )
            } else {
                organizer::import_versioned_file_cancellable(
                    source,
                    root,
                    &job.request,
                    &job.id,
                    active,
                )
            }
        } else if job.download_id.is_some() {
            organizer::import_native_file_cancellable(source, root, &job.request, active)
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
                .get()
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
            &*lock(&self.requester_store)?,
        )? {
            job.state = "queued".into();
            job.next_attempt_at = store::now().saturating_add(60);
            return Ok(());
        }
        // Resume a confirmed import after interruption without copying it again.
        if !job.imports.is_empty() {
            organizer::check_retained_imports(job, &config)?;
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
            let client = self.downloads.get().ok_or("Downloads are disabled")?;
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
                let client = self.downloads.get().ok_or("Downloads are disabled")?;
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
                    let current = store.get(&job.id)?.ok_or("Missing job")?;
                    if current.lease_id != job.lease_id || current.lease_until <= store::now() {
                        if current.state == "cancelled"
                            && current.acquisition_url == job.acquisition_url
                            && current.download_id.is_none()
                        {
                            let mut cancelled = current;
                            cancelled.download_id = Some(status.id.clone());
                            store.update(cancelled)?;
                        }
                        let shared =
                            store.transfer_claimed(&status.id, job.acquisition_url.as_deref())?;
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
                    let j = s.get(&id)?.ok_or("Missing job")?;
                    if j.lease_id.as_deref() != Some(&lease)
                        || j.lease_until <= store::now()
                        || !crate::requesters::engine::interest(&*ledger, &j, &engine.config)?
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

/// Settles one bounded batch of recorded retirements: pause an unowned transfer, then clear the
/// record. Without a native client the records stay for a later run. User pause
/// state is never touched, and no payload or metadata is removed. Each round
/// reads a bounded batch, so memory does not grow with the backlog.
fn retire_transfers(client: Option<&Client>, store: &mut Store) -> Result<()> {
    let Some(client) = client else {
        return Ok(());
    };
    let pending = store.pending_retirements(RETIREMENT_BATCH)?;
    if pending.is_empty() {
        return Ok(());
    }
    for (job_id, id) in pending {
        // A live request, including this one if it found the same release
        // again, keeps the transfer. Cancelled requests do not; finished
        // ones retain it.
        if client.contains(&id)? && !store.transfer_has_owner(&id)? {
            client.cancel(&id)?;
        }
        store.complete_retirement(&job_id, &id)?;
    }
    Ok(())
}

/// Requests examined per retirement round.
const RETIREMENT_BATCH: usize = 64;

/// Cancels a native transfer that only cancelled requests still name. The
/// native client reads one identity page; successive reconciliation rounds
/// continue the sweep. Each identity uses the request store's transfer index.
fn cancel_orphaned_transfers(
    client: &Client,
    store: &mut Store,
    after: &mut Option<String>,
) -> Result<()> {
    let (statuses, next) = client.statuses_page(after.as_deref(), RETIREMENT_BATCH)?;
    for status in statuses {
        if !store.transfer_has_owner(&status.id)? && store.transfer_cancelled(&status.id)? {
            client.cancel(&status.id)?;
        }
    }
    *after = next;
    Ok(())
}

/// The JSON of one page of jobs: `jobs`, the `next` cursor and the `total`.
pub fn jobs_value(page: crate::store::JobPage) -> Value {
    let mut value = Value::object();
    value.insert(
        "jobs",
        Value::Array(page.jobs.iter().map(public_job).collect()),
    );
    value.insert("next", page.next.map_or(Value::Null, Value::String));
    value.insert("total", Value::Number(page.total as f64));
    value.insert("sequence", Value::String(page.sequence.to_string()));
    value
}

/// One page of jobs for `query`. A filtered page also reports `scanned`, the
/// rows it examined: it may hold fewer matches than its limit while `next`
/// continues the search.
pub fn jobs_query_value(
    store: &mut crate::store::Store,
    query: &crate::store::JobQuery,
) -> Result<Value> {
    if query.filter.is_empty() {
        return Ok(jobs_value(store.page(query.after.as_deref(), query.limit)?));
    }
    let (page, scanned) =
        store.search(&query.filter, false, query.after.as_deref(), query.limit)?;
    let mut value = jobs_value(page);
    value.insert("scanned", Value::Number(scanned as f64));
    Ok(value)
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
