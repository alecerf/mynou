//! Opt-in feed watching. Source I/O holds no storage lock, every poll commits
//! observations and captured candidates before routing, and a release only reaches
//! demand that already exists: a feed never creates a request.
use super::feed;
use crate::{
    Result,
    config::{Config, Source},
    engine::{Engine, lock},
    integrations::{self, FeedEntry, WatchCandidate},
    json::Value,
    requesters::engine::interest,
    store::{self, Job, RecordedRelease},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Unseen entries considered per poll; the rest stay new for the next poll.
const MAX_NEW_PER_POLL: usize = 200;
const MAX_POLLS_PER_PASS: usize = 4;
const MAX_ROUTES_PER_POLL: usize = 32;
const MAX_ROUTE_ROWS: usize = 200;
const ROUTE_BUDGET: Duration = Duration::from_secs(10);
const POLL_BUDGET: Duration = Duration::from_secs(30);
const BUSY_RETRY: Duration = Duration::from_secs(10);
const MAX_RETRY: Duration = Duration::from_secs(3_600);

/// Volatile health: it resets when the service restarts, unlike the cursor.
#[derive(Default)]
pub(crate) struct Runtime {
    sources: BTreeMap<String, State>,
}
#[derive(Default)]
struct State {
    next_due: Option<Instant>,
    outcome: &'static str,
    last_attempt_at: u64,
    polls: u64,
    failures: u64,
    consecutive_failures: u32,
    last: Tally,
    routed: u64,
    upgrades: u64,
}
#[derive(Clone, Copy, Default)]
struct Tally {
    baseline: bool,
    observed: u32,
    new_entries: u32,
    duplicates: u32,
    skipped: u32,
    truncated: bool,
    routed: u32,
    upgrades: u32,
    available: u32,
    unverified: u32,
    changed: u32,
    failed: u32,
    resumed: bool,
    pending: bool,
    examined: u32,
}
impl Tally {
    fn insert_into(&self, v: &mut Value) {
        v.insert("baseline", self.baseline);
        v.insert("observed", self.observed);
        v.insert("new", self.new_entries);
        v.insert("duplicates", self.duplicates);
        v.insert("skipped", self.skipped);
        v.insert("truncated", self.truncated);
        v.insert("routed", self.routed);
        v.insert("upgrades_queued", self.upgrades);
        v.insert("already_available", self.available);
        v.insert("unverified", self.unverified);
        v.insert("changed", self.changed);
        v.insert("routing_failed", self.failed);
        v.insert("routing_resumed", self.resumed);
        v.insert("routing_pending", self.pending);
        v.insert("routing_examined", self.examined);
    }
}

#[derive(Clone, Copy)]
enum Attach {
    Routed,
    Available,
    Unverified,
    Changed,
    Upgraded,
    Revoked,
    Retry,
}

/// Work that no one has started and that no other path has selected yet. Such
/// a job would otherwise search the indexers on its own.
fn routable(job: &Job) -> bool {
    (job.state == "queued" || (job.state == "failed" && job.next_attempt_at != 0))
        && job.lease_id.is_none()
        && job.lease_until == 0
        && matches!(job.request.kind.as_str(), "movie" | "episode")
        && job.request.source_path.is_none()
        && job.request.source_url.is_none()
        && job.acquisition_url.is_none()
        && job.download_id.is_none()
        && job.release.is_none()
        && job.files.is_empty()
        && job.imports.is_empty()
        && job.upgrade_parent.is_none()
        && job.pack_file.is_none()
        && job.pack_origin.is_none()
        && job.shared_file.is_none()
        && job.shared_upgrade.is_none()
        && job.irc_origin.is_none()
}

fn best(config: &Config, request: &store::Request, fresh: &[&FeedEntry]) -> Option<WatchCandidate> {
    fresh
        .iter()
        .filter_map(|entry| integrations::assess_feed_entry(config, request, entry))
        .reduce(|best, next| if next.beats(&best) { next } else { best })
}

/// When the source may be polled again. A 429 cooldown and the source interval
/// bound the delay from below.
fn delay(source: &Source, outcome: &str, failures: u32) -> Duration {
    let interval = Duration::from_secs(source.options.watch.interval_secs);
    match outcome {
        "ok" => interval,
        "busy" => BUSY_RETRY.min(interval),
        "rate_limited" => {
            let wait = crate::indexers::cooldown(source)
                .unwrap_or_else(|| Duration::from_millis(source.options.min_interval_ms));
            (wait + Duration::from_secs(1)).max(Duration::from_secs(1))
        }
        _ => Duration::from_secs(30)
            .saturating_mul(1_u32 << failures.saturating_sub(1).min(7))
            .min(interval)
            .min(MAX_RETRY),
    }
}

impl Engine {
    pub fn feeds_retained(&self, query: &str) -> Result<Value> {
        let (after, limit) = feed::retained_query(query)?;
        lock(&self.feed_store)?.retained(after.as_deref(), limit)
    }
    /// One bounded pass over the sources that are due. `force` ignores only the
    /// watcher's own schedule: intervals, busy state and cooldowns still apply.
    pub fn feeds_poll(&self, only: Option<&str>, force: bool) -> Result<Value> {
        if self.read_only || self.stopped.load(Ordering::Acquire) {
            return Err("Feeds: polling requires the running service".into());
        }
        if !self.config.downloads_enabled {
            return Err("Feeds: routing requires enabled downloads".into());
        }
        let _pass = self
            .feed_lock
            .try_lock()
            .map_err(|_| "Feeds: a poll is already in progress")?;
        let mut sources: Vec<(String, &Source)> = self
            .config
            .sources
            .iter()
            .filter(|source| source.options.watch.enabled)
            .map(|source| (feed::source_id(source), source))
            .collect();
        sources.sort_by(|a, b| a.0.cmp(&b.0));
        let scheduled = lock(&self.feed_store)?.schedule_cursor()?;
        let pivot = sources.partition_point(|(id, _)| id <= &scheduled);
        sources.rotate_left(pivot);
        if only.is_some_and(|id| !sources.iter().any(|(known, _)| known == id)) {
            return Err("Feeds: source is not watched".into());
        }
        let mut reports = Vec::new();
        let mut not_due = 0_u32;
        for (id, source) in sources {
            if only.is_some_and(|only| only != id) {
                continue;
            }
            if !source.options.watched() {
                // Resuming a paused source baselines it again.
                let outcome = match lock(&self.feed_store)?.suspend(&id) {
                    Ok(_) => "disabled",
                    Err(_) => "storage_failed",
                };
                let mut state = lock(&self.feed_runtime)?;
                let state = state.sources.entry(id.clone()).or_default();
                state.outcome = outcome;
                state.next_due = None;
                continue;
            }
            let pending = lock(&self.feed_store)?.pending_header(&id)?.is_some();
            let due = pending
                || force
                || lock(&self.feed_runtime)?
                    .sources
                    .get(&id)
                    .and_then(|state| state.next_due)
                    .is_none_or(|at| at <= Instant::now());
            if !due {
                not_due += 1;
                continue;
            }
            if reports.len() == MAX_POLLS_PER_PASS || self.stopped.load(Ordering::Acquire) {
                break;
            }
            if lock(&self.feed_store)?.scheduled(&id).is_err() {
                let mut failure = Value::object();
                failure.insert("id", id);
                failure.insert("outcome", "storage_failed");
                Tally::default().insert_into(&mut failure);
                reports.push(failure);
                break;
            }
            reports.push(self.poll_source(source, &id));
        }
        let mut report = Value::object();
        report.insert("polled", reports.len() as u32);
        report.insert("not_due", not_due);
        report.insert("sources", Value::Array(reports));
        Ok(report)
    }

    fn poll_source(&self, source: &Source, id: &str) -> Value {
        let at = store::now();
        let mut tally = Tally::default();
        let outcome = match self.run_poll(source, id, at, &mut tally) {
            Ok(()) => "ok",
            Err(code) => code,
        };
        if let Ok(mut runtime) = lock(&self.feed_runtime) {
            let state = runtime.sources.entry(id.to_owned()).or_default();
            state.polls = state.polls.saturating_add(1);
            state.last_attempt_at = at;
            state.outcome = outcome;
            if outcome == "ok" {
                state.consecutive_failures = 0;
                state.last = tally;
            } else {
                state.failures = state.failures.saturating_add(1);
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            }
            state.routed = state.routed.saturating_add(u64::from(tally.routed));
            state.upgrades = state.upgrades.saturating_add(u64::from(tally.upgrades));
            state.next_due =
                Some(Instant::now() + delay(source, outcome, state.consecutive_failures.max(1)));
        }
        let mut report = Value::object();
        report.insert("id", id);
        report.insert("outcome", outcome);
        tally.insert_into(&mut report);
        report
    }

    fn run_poll(
        &self,
        source: &Source,
        id: &str,
        at: u64,
        tally: &mut Tally,
    ) -> std::result::Result<(), &'static str> {
        let policy_revision = {
            let policy = lock(&self.indexer_store).map_err(|_| "storage_failed")?;
            let (enabled, revision) = policy.authorization(id).map_err(|_| "storage_failed")?;
            if !enabled || !source.options.watched() {
                return Err("disabled");
            }
            revision
        };
        let pending = lock(&self.feed_store)
            .map_err(|_| "storage_failed")?
            .pending(id)
            .map_err(|_| "storage_failed")?;
        if let Some(generation) = pending {
            tally.resumed = true;
            if generation.header.policy_revision != policy_revision {
                lock(&self.feed_store)
                    .map_err(|_| "storage_failed")?
                    .suspend(id)
                    .map_err(|_| "storage_failed")?;
                return Ok(());
            }
            return self
                .route_generation(source, &generation, tally)
                .map_err(|_| "storage_failed");
        }
        let poll = integrations::poll_feed(source, Instant::now() + POLL_BUDGET)?;
        let ids: Vec<&str> = poll.entries.iter().map(|entry| entry.id.as_str()).collect();
        // The authoritative source policy fences a pause/re-enable during I/O.
        let generation = {
            let policy = lock(&self.indexer_store).map_err(|_| "storage_failed")?;
            let (enabled, revision) = policy.authorization(id).map_err(|_| "storage_failed")?;
            if !enabled || revision != policy_revision || !source.options.watched() {
                return Err("disabled");
            }
            let mut feeds = lock(&self.feed_store).map_err(|_| "storage_failed")?;
            let current = feeds
                .row(id)
                .map_err(|_| "storage_failed")?
                .ok_or("storage_failed")?;
            let observation = feed::observe(&current, &ids, at, MAX_NEW_PER_POLL);
            tally.baseline = observation.baseline;
            tally.observed = poll.entries.len() as u32;
            tally.new_entries = observation.fresh.len() as u32;
            tally.duplicates = observation.duplicates;
            tally.skipped = poll.skipped;
            tally.truncated = poll.truncated;
            if observation.baseline || observation.fresh.is_empty() {
                feeds
                    .commit(observation.record)
                    .map_err(|_| "storage_failed")?;
                None
            } else {
                let entries = observation
                    .fresh
                    .iter()
                    .map(|index| poll.entries[*index].clone())
                    .collect();
                let generation =
                    feed::generation::Generation::new(&observation.record, revision, entries, at)
                        .map_err(|_| "capture_rejected")?;
                feeds
                    .capture(observation.record, generation.clone())
                    .map_err(|_| "storage_failed")?;
                Some(generation)
            }
        };
        if let Some(generation) = generation {
            self.route_generation(source, &generation, tally)
                .map_err(|_| "storage_failed")?;
        }
        Ok(())
    }

    fn route_generation(
        &self,
        source: &Source,
        generation: &feed::generation::Generation,
        tally: &mut Tally,
    ) -> Result<()> {
        self.route_window(
            source,
            &generation.header,
            &generation.entries,
            tally,
            Instant::now() + ROUTE_BUDGET,
        )?;
        if tally.routed > 0 {
            self.requester_reconcile()?;
        }
        Ok(())
    }
    fn route_window(
        &self,
        source: &Source,
        header: &feed::generation::Header,
        entries: &[FeedEntry],
        tally: &mut Tally,
        deadline: Instant,
    ) -> Result<()> {
        tally.pending = true;
        let fresh: Vec<_> = entries.iter().collect();
        let mut durable = header.clone();
        let mut progress = durable.clone();
        let _upgrades = if progress.library {
            if !self.config.monitoring.enabled {
                lock(&self.feed_store)?.checkpoint(&durable, None)?;
                tally.pending = false;
                return Ok(());
            }
            match self.upgrade_lock.try_lock() {
                Ok(guard) => Some(guard),
                Err(_) => return Ok(()),
            }
        } else {
            None
        };
        let page = if progress.library {
            lock(&self.store)?.library_page(
                progress.after.as_deref(),
                MAX_ROUTE_ROWS - tally.examined as usize,
            )?
        } else {
            lock(&self.store)?.page(
                progress.after.as_deref(),
                MAX_ROUTE_ROWS - tally.examined as usize,
            )?
        };
        let mut settled = 0;
        for job in &page.jobs {
            if tally.routed as usize + tally.upgrades as usize >= MAX_ROUTES_PER_POLL
                || Instant::now() >= deadline
                || self.stopped.load(Ordering::Acquire)
            {
                break;
            }
            let eligible = if progress.library || !routable(job) {
                false
            } else {
                let requesters = lock(&self.requester_store)?;
                interest(&*requesters, job, &self.config)?
            };
            let action = if progress.library {
                self.attach_upgrade(source, header.policy_revision, job, &fresh)?
            } else if eligible {
                let config = self.configuration_for(job);
                match best(&config, &job.request, &fresh) {
                    Some(candidate) => self.attach_release(
                        source,
                        header.policy_revision,
                        job,
                        &config,
                        &candidate,
                    )?,
                    None => Attach::Changed,
                }
            } else {
                Attach::Changed
            };
            tally.examined += 1;
            match action {
                Attach::Revoked => {
                    lock(&self.feed_store)?.suspend(&header.source)?;
                    tally.pending = false;
                    return Ok(());
                }
                Attach::Retry | Attach::Unverified => {
                    tally.unverified += 1;
                    break;
                }
                Attach::Routed => tally.routed += 1,
                Attach::Upgraded => tally.upgrades += 1,
                Attach::Available => tally.available += 1,
                Attach::Changed => tally.changed += 1,
            }
            progress.after = Some(format!("{:016x}{}", job.created_at, job.id));
            settled += 1;
            if matches!(action, Attach::Routed | Attach::Upgraded) {
                if !lock(&self.feed_store)?.checkpoint(&durable, Some(&progress))? {
                    tally.pending = false;
                    return Ok(());
                }
                durable.clone_from(&progress);
            }
        }
        if settled == page.jobs.len() && page.next.is_none() {
            if progress.library || !self.config.monitoring.enabled {
                lock(&self.feed_store)?.checkpoint(&durable, None)?;
                tally.pending = false;
            } else {
                progress.library = true;
                progress.after = None;
                tally.pending = lock(&self.feed_store)?.checkpoint(&durable, Some(&progress))?;
                if tally.pending
                    && tally.examined < MAX_ROUTE_ROWS as u32
                    && tally.routed + tally.upgrades < MAX_ROUTES_PER_POLL as u32
                    && Instant::now() < deadline
                {
                    self.route_window(source, &progress, entries, tally, deadline)?;
                }
            }
        } else if progress != durable {
            tally.pending = lock(&self.feed_store)?.checkpoint(&durable, Some(&progress))?;
        }
        Ok(())
    }

    fn attach_release(
        &self,
        source: &Source,
        policy_revision: u64,
        job: &Job,
        config: &Config,
        candidate: &WatchCandidate,
    ) -> Result<Attach> {
        // Existing media is verified outside storage locks, as the search path does.
        if config.plex.enabled {
            match integrations::available_before(
                config,
                &job.request,
                job.requester.is_some(),
                Instant::now() + Duration::from_secs(10),
            ) {
                Ok(false) => {}
                Ok(true) => return Ok(Attach::Available),
                Err(_) => return Ok(Attach::Unverified),
            }
        }
        // Policy/control and requester locks fence the final job mutation.
        let policy = lock(&self.indexer_store)?;
        let (enabled, revision) = policy.authorization(&feed::source_id(source))?;
        if !enabled || revision != policy_revision || !source.options.watched() {
            return Ok(Attach::Revoked);
        }
        let requesters = lock(&self.requester_store)?;
        let mut jobs = lock(&self.store)?;
        let Some(current) = jobs.get(&job.id)? else {
            return Ok(Attach::Changed);
        };
        if self.stopped.load(Ordering::Acquire) {
            return Ok(Attach::Retry);
        }
        if !self.config.downloads_enabled
            || !source.options.watched()
            || &current != job
            || !routable(&current)
            || !interest(&*requesters, &current, &self.config)?
        {
            return Ok(Attach::Changed);
        }
        let mut next = current;
        next.state = "queued".into();
        next.last_error = None;
        next.next_attempt_at = 0;
        next.acquisition_url = Some(candidate.url.clone());
        next.release = Some(RecordedRelease {
            title: candidate.title.clone(),
            profile: candidate.profile.clone(),
        });
        jobs.update(next)?;
        Ok(Attach::Routed)
    }

    fn attach_upgrade(
        &self,
        source: &Source,
        policy_revision: u64,
        parent: &Job,
        fresh: &[&FeedEntry],
    ) -> Result<Attach> {
        let policy = lock(&self.indexer_store)?;
        let (enabled, revision) = policy.authorization(&feed::source_id(source))?;
        if !enabled || revision != policy_revision || !source.options.watched() {
            return Ok(Attach::Revoked);
        }
        if self.stopped.load(Ordering::Acquire) {
            return Ok(Attach::Retry);
        }
        let requesters = lock(&self.requester_store)?;
        let mut jobs = lock(&self.store)?;
        let Some(current) = jobs.get(&parent.id)? else {
            return Ok(Attach::Changed);
        };
        if &current != parent
            || !current.monitored
            || current.shared_file.is_some()
            || !matches!(current.request.kind.as_str(), "movie" | "episode")
            || !jobs.is_library_tip(&current.id)?
            || !crate::library::import_exists(&current)
            || !interest(&*requesters, &current, &self.config)?
        {
            return Ok(Attach::Changed);
        }
        let Some(recorded) = &current.release else {
            return Ok(Attach::Changed);
        };
        let config = self.configuration_for(&current);
        let Ok((_, profile)) = config.selection.profile(&current.request.kind) else {
            return Ok(Attach::Changed);
        };
        let baseline = profile.assess(&recorded.title, &current.request.title);
        if profile.cutoff_reached(&baseline) {
            return Ok(Attach::Changed);
        }
        let mut request = current.request.clone();
        request.source_path = None;
        request.source_url = None;
        let Some(candidate) = best(&config, &request, fresh) else {
            return Ok(Attach::Changed);
        };
        if (baseline.accepted && candidate.assessment.rank <= baseline.rank)
            || current.acquisition_url.as_deref() == Some(candidate.url.as_str())
            || current.request.source_url.as_deref() == Some(candidate.url.as_str())
        {
            return Ok(Attach::Changed);
        }
        request.source_url = Some(candidate.url.clone());
        let release = RecordedRelease {
            title: candidate.title,
            profile: candidate.profile,
        };
        let before = jobs.sequence()?;
        match jobs.submit_upgrade(&current.id, request, release) {
            Ok(_) => Ok(if jobs.sequence()? != before {
                Attach::Upgraded
            } else {
                Attach::Changed
            }),
            Err(error)
                if matches!(
                    error.as_str(),
                    "an upgrade for this media is already pending"
                        | "upgrade source URL is already owned by an unrelated request"
                ) =>
            {
                Ok(Attach::Changed)
            }
            Err(error) => Err(error),
        }
    }

    pub fn feeds(&self) -> Result<Value> {
        let mut feeds = lock(&self.feed_store)?;
        let runtime = lock(&self.feed_runtime)?;
        let mut rows = Vec::new();
        for source in self
            .config
            .sources
            .iter()
            .filter(|source| matches!(source.kind.as_str(), "rss" | "torznab"))
        {
            let id = feed::source_id(source);
            let mut v = Value::object();
            v.insert("id", id.clone());
            v.insert("name", integrations::report_text(&source.name, 128));
            v.insert("kind", source.kind.clone());
            v.insert("watch_enabled", source.options.watch.enabled);
            v.insert("active", source.options.watched());
            v.insert(
                "interval_secs",
                source.options.watch.interval_secs.to_string(),
            );
            let pending = feeds.pending_header(&id)?;
            v.insert("routing_pending", pending.is_some());
            v.insert(
                "captured_entries",
                pending.as_ref().map_or(0, |header| header.count as u32),
            );
            v.insert(
                "routing_captured_at",
                pending
                    .as_ref()
                    .map_or(0, |header| header.captured_at)
                    .to_string(),
            );
            v.insert(
                "routing_phase",
                pending.as_ref().map_or(Value::Null, |header| {
                    Value::from(if header.library { "library" } else { "jobs" })
                }),
            );
            let owned_row = feeds.row(&id)?;
            let row = owned_row.as_ref();
            v.insert("baselined", row.is_some_and(|row| row.baselined));
            v.insert(
                "baselined_at",
                row.map_or(0, |row| row.baselined_at).to_string(),
            );
            v.insert(
                "last_success_at",
                row.map_or(0, |row| row.last_success_at).to_string(),
            );
            v.insert("successes", row.map_or(0, |row| row.successes).to_string());
            v.insert(
                "new_entries",
                row.map_or(0, |row| row.new_entries).to_string(),
            );
            v.insert(
                "retained_identities",
                row.map_or(0, |row| row.seen.len()) as u32,
            );
            v.insert("cursor_set", row.is_some_and(|row| !row.cursor.is_empty()));
            let state = runtime.sources.get(&id);
            let outcome = state.map_or("", |state| state.outcome);
            v.insert(
                "last_outcome",
                if outcome.is_empty() {
                    Value::Null
                } else {
                    outcome.into()
                },
            );
            v.insert(
                "last_attempt_at",
                state.map_or(0, |state| state.last_attempt_at).to_string(),
            );
            v.insert("polls", state.map_or(0, |state| state.polls).to_string());
            v.insert(
                "failures",
                state.map_or(0, |state| state.failures).to_string(),
            );
            v.insert(
                "consecutive_failures",
                state.map_or(0, |state| state.consecutive_failures),
            );
            v.insert(
                "next_poll_in_secs",
                state
                    .and_then(|state| state.next_due)
                    .map_or(Value::Null, |at| {
                        (at.saturating_duration_since(Instant::now()).as_secs() as u32).into()
                    }),
            );
            v.insert(
                "cooldown_secs",
                crate::indexers::cooldown(source).map_or(Value::Null, |wait| {
                    (wait.as_secs().min(u64::from(u32::MAX)) as u32).into()
                }),
            );
            let last = state.map(|state| state.last).unwrap_or_default();
            v.insert("last_observed", last.observed);
            v.insert("last_new", last.new_entries);
            v.insert("last_skipped", last.skipped);
            v.insert("routed", state.map_or(0, |state| state.routed).to_string());
            v.insert(
                "upgrades_queued",
                state.map_or(0, |state| state.upgrades).to_string(),
            );
            rows.push(v);
        }
        let mut report = Value::object();
        report.insert("routing_enabled", self.config.downloads_enabled);
        let (generations, entries) = feeds.routing_counts()?;
        report.insert("pending_generations", generations.to_string());
        report.insert("captured_entries", entries.to_string());
        report.insert("retained_sources", feeds.total()?.to_string());
        report.insert("needs_migration", feeds.needs_migration());
        report.insert("sources", Value::Array(rows));
        Ok(report)
    }
}

pub(crate) fn start(engine: &Arc<Engine>, handles: &mut Vec<JoinHandle<()>>) {
    if !engine.config.downloads_enabled
        || !engine
            .config
            .sources
            .iter()
            .any(|source| source.options.watch.enabled)
    {
        return;
    }
    let engine = engine.clone();
    handles.push(thread::spawn(move || {
        while !engine.stopped.load(Ordering::Acquire) {
            let _ = engine.feeds_poll(None, false);
            engine.wait(1000);
        }
    }));
}
