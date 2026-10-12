//! Owned library versions and controlled upgrades. Network I/O never holds the journal lock.
mod groups;
use crate::{
    Result,
    engine::{Engine, lock, public_job},
    integrations,
    json::Value,
    media,
    store::{self, Job, RecordedRelease, Store, reject_symlinks},
};
pub use groups::GroupRequest;
use std::{
    fs,
    path::Path,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

pub(crate) fn import_exists(job: &Job) -> bool {
    !job.imports.is_empty()
        && job.imports.iter().all(|file| {
            let path = Path::new(file);
            reject_symlinks(path).is_ok()
                && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        })
}

/// Shared-group entries listed in one report; more set `limited`.
const MAX_SHARED_ENTRIES: usize = 200;

fn entry(job: &Job, action: &str) -> Value {
    let mut value = Value::object();
    value.insert("job_id", job.id.clone());
    value.insert("media_key", job.request.media_key());
    value.insert("action", action);
    value
}

/// Library entries with their upgrade state. A pending upgrade is found by
/// its media identity, never by scanning the other jobs.
pub fn describe(store: &mut Store, current: &[Job]) -> Result<Value> {
    let mut entries = Vec::with_capacity(current.len());
    for job in current {
        let mut value = public_job(job);
        value.insert("media_key", job.request.media_key());
        value.insert(
            "baseline_required",
            job.release.is_none() && job.shared_file.is_none(),
        );
        value.insert("group_upgrade_required", job.shared_file.is_some());
        value.insert(
            "group_baseline_required",
            job.shared_file.is_some() && job.release.is_none(),
        );
        value.insert("imports_present", import_exists(job));
        value.insert(
            "pending_upgrade_id",
            store
                .pending_for(&job.request.media_key())?
                .map_or(Value::Null, |child| child.id.into()),
        );
        entries.push(value);
    }
    Ok(Value::Array(entries))
}

/// One page of the library, in the shape of every other paged list.
pub fn describe_page(store: &mut Store, after: Option<&str>, limit: usize) -> Result<Value> {
    let page = store.library_page(after, limit)?;
    let mut value = Value::object();
    value.insert("library", describe(store, &page.jobs)?);
    value.insert("next", page.next.map_or(Value::Null, Value::String));
    value.insert("total", Value::Number(page.total as f64));
    value.insert("sequence", Value::String(page.sequence.to_string()));
    Ok(value)
}

fn report(apply: bool, entries: Vec<Value>, queued: u32, limited: bool, counts: [u32; 3]) -> Value {
    let mut skipped = Value::object();
    skipped.insert("unmonitored", counts[0]);
    skipped.insert("baseline_required", counts[1]);
    skipped.insert("unsupported_kind", counts[2]);
    let mut value = Value::object();
    value.insert("apply", apply);
    value.insert("checked", entries.len() as u32);
    value.insert("queued", queued);
    value.insert("entries", Value::Array(entries));
    value.insert("limited", limited);
    value.insert("skipped", skipped);
    value
}

/// A fresh installation has no owned imports and needs no preview storage.
pub fn empty_preview() -> Value {
    report(false, Vec::new(), 0, false, [0; 3])
}

pub fn validate_baseline(
    config: &crate::config::Config,
    job: &Job,
    title: &str,
) -> Result<RecordedRelease> {
    if job.shared_file.is_some() {
        return Err(
            "Shared-file owners require a group baseline; individual baselines are blocked".into(),
        );
    }
    if title.is_empty() || title.len() > 2_048 || title.chars().any(char::is_control) {
        return Err("Baseline: release title must contain 1 to 2048 bytes without controls".into());
    }
    if !matches!(job.request.kind.as_str(), "movie" | "episode")
        || !integrations::release_identity_matches(&job.request, title)
    {
        return Err("Baseline: release title does not match this movie or episode".into());
    }
    if !import_exists(job) {
        return Err("Baseline: imported files are missing or unsafe".into());
    }
    for file in &job.imports {
        if media::analyze(Path::new(file))?.video_streams.is_empty() {
            return Err("Baseline: movie or episode has no declared video stream".into());
        }
    }
    let (profile, _) = config.selection.profile(&job.request.kind)?;
    // A legacy baseline may describe a quality outside today's allowed profile.
    Ok(RecordedRelease {
        title: title.into(),
        profile: profile.into(),
    })
}

impl Engine {
    /// Returns a page of owned ready versions; an unfinished child cannot
    /// replace its parent.
    pub fn library(&self, after: Option<&str>, limit: usize) -> Result<Value> {
        describe_page(&mut *lock(&self.store)?, after, limit)
    }

    pub fn set_monitored(&self, id: &str, enabled: bool) -> Result<Job> {
        lock(&self.store)?.set_monitored(id, enabled)
    }

    /// Establishes an explicit release-name baseline for a legacy owned import.
    pub fn set_baseline(&self, id: &str, title: &str) -> Result<Job> {
        let job = lock(&self.store)?.get(id)?.ok_or("Unknown library entry")?;
        let release = validate_baseline(&self.configuration_for(&job), &job, title)?;
        lock(&self.store)?.set_baseline(id, release)
    }

    /// Manual checks ignore the polling interval. Preview never writes the journal.
    pub fn check_upgrades(&self, apply: bool) -> Result<Value> {
        self.scan_upgrades(apply, true)
    }

    pub(crate) fn check_due_upgrades(&self) -> Result<Value> {
        self.scan_upgrades(true, false)
    }

    fn scan_upgrades(&self, apply: bool, force: bool) -> Result<Value> {
        if apply && self.read_only {
            return Err("Read-only preview cannot apply upgrades".into());
        }
        let _guard = self
            .upgrade_lock
            .try_lock()
            .map_err(|_| "An upgrade check is already in progress")?;
        let at = store::now();
        let started = Instant::now();
        let max_checks = self.config.monitoring.max_checks;
        // Counters and the oldest-check index replace preparatory inventory scans.
        let (counts, window, shared) = {
            let mut store = lock(&self.store)?;
            let counts = store.monitoring_counts()?;
            let window = store.monitoring_due(
                at,
                self.config.monitoring.interval_secs,
                force,
                max_checks + 1,
            )?;
            let shared = store.monitored_shared(MAX_SHARED_ENTRIES)?;
            (counts, window, shared)
        };
        let [
            eligible_total,
            unmonitored,
            baseline_required,
            unsupported,
            shared_total,
        ] = counts;
        let oldest_checked = window.jobs.first().map(|job| job.monitor_checked_at);
        let window_limited = window.jobs.len() > max_checks || window.next.is_some();
        let mut limited = window_limited || shared_total > shared.len() as u64;
        let mut entries = shared
            .iter()
            .map(|job| entry(job, "shared_group_upgrade_required"))
            .collect::<Vec<_>>();
        let eligible = window.jobs;
        let mut queued = 0_u32;
        for job in eligible.into_iter().take(self.config.monitoring.max_checks) {
            if self.stopped.load(Ordering::Acquire) || started.elapsed() >= Duration::from_secs(90)
            {
                limited = true;
                break;
            }
            if apply {
                let mut store = lock(&self.store)?;
                if store.get(&job.id)?.as_ref() != Some(&job) || !store.is_library_tip(&job.id)? {
                    entries.push(entry(&job, "entry_changed"));
                    continue;
                }
                // Resource/durability failures must reach the caller, rather
                // than pretending the record was changed by another operator.
                store.record_monitor_check(&job.id, at)?;
            }
            let media_key = job.request.media_key();
            if let Some(pending) = lock(&self.store)?.pending_for(&media_key)? {
                let mut item = entry(&job, "upgrade_pending");
                item.insert("upgrade_job_id", pending.id.clone());
                entries.push(item);
                continue;
            }
            if !import_exists(&job) {
                entries.push(entry(&job, "imports_missing"));
                continue;
            }
            let config = self.configuration_for(&job);
            let (_, profile) = config.selection.profile(&job.request.kind)?;
            let recorded = job.release.as_ref().ok_or("Missing release baseline")?;
            let baseline = profile.assess(&recorded.title, &job.request.title);
            if profile.cutoff_reached(&baseline) {
                entries.push(entry(&job, "cutoff_reached"));
                continue;
            }
            let mut request = job.request.clone();
            request.source_path = None;
            request.source_url = None;
            let selected = match integrations::select_release_before(
                &config,
                &request,
                started + Duration::from_secs(90),
            ) {
                Ok(selected) => selected,
                Err(_) => {
                    entries.push(entry(&job, "search_unavailable"));
                    continue;
                }
            };
            let title = integrations::report_text(&selected.title, 2_048);
            let mut item = entry(&job, "no_improvement");
            item.insert("current_assessment", baseline.to_json());
            item.insert("candidate_id", selected.id);
            item.insert("candidate_title", title);
            item.insert("candidate_assessment", selected.assessment.to_json());
            if baseline.accepted && selected.assessment.rank <= baseline.rank {
                entries.push(item);
                continue;
            }
            if job.acquisition_url.as_deref() == Some(&selected.url)
                || job.request.source_url.as_deref() == Some(&selected.url)
            {
                item.insert("action", "same_acquisition");
                entries.push(item);
                continue;
            }
            if !apply {
                item.insert("action", "upgrade_available");
                entries.push(item);
                continue;
            }
            request.source_url = Some(selected.url);
            let release = RecordedRelease {
                title: selected.title,
                profile: selected.profile,
            };
            let mut store = lock(&self.store)?;
            let before = store.counts()?.jobs;
            match store.submit_upgrade(&job.id, request, release) {
                Ok(child) => {
                    let created = store.counts()?.jobs > before;
                    queued += u32::from(created);
                    item.insert(
                        "action",
                        if created {
                            "queued"
                        } else {
                            "candidate_already_recorded"
                        },
                    );
                    item.insert("upgrade_job_id", child.id);
                    item.insert("upgrade_state", child.state);
                }
                Err(error) if matches!(error.as_str(),
                    "upgrade source URL is already owned by an unrelated request"
                    | "an upgrade requires a monitored current library import with a release baseline"
                    | "an upgrade for this media is already pending"
                    | "an upgrade must use a different acquisition URL") => {
                    // A concurrent control, another source identity, or an obsolete
                    // parent must never cause a second upgrade or leak its URL.
                    item.insert("action", "entry_changed");
                }
                Err(error) => return Err(error),
            }
            entries.push(item);
        }
        let mut value = report(
            apply,
            entries,
            queued,
            limited,
            [unmonitored, baseline_required, unsupported].map(|n| n.min(u32::MAX as u64) as u32),
        );
        value.insert("eligible_total", eligible_total.to_string());
        value.insert("window_limited", window_limited);
        value.insert(
            "oldest_checked_at",
            oldest_checked.map_or(Value::Null, |at| Value::String(at.to_string())),
        );
        value.insert(
            "monitoring_interval_secs",
            self.config.monitoring.interval_secs.to_string(),
        );
        let mut totals = Value::object();
        for (name, count) in [
            "eligible",
            "unmonitored",
            "baseline_required",
            "unsupported_kind",
            "shared_group",
        ]
        .into_iter()
        .zip(counts)
        {
            totals.insert(name, count.to_string());
        }
        value.insert("inventory_counts", totals);
        Ok(value)
    }
}
