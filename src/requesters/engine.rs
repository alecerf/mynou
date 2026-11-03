//! Account I/O precedes admission. Snapshot reservations precede queued jobs.
use super::{
    Capture, ControlRequest, Demand, LedgerRead, MAX_BATCH_ITEMS, Policy, Provenance, Record,
    digest,
    index::{Progress, Stage, Sweep},
    valid_id,
};
use crate::{
    Result, date,
    engine::{Engine, lock},
    integrations,
    json::{self, Value},
    store::{self, Job, Request, Store},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

/// The job that currently represents a request's media: the library's current
/// version, else the oldest request that is not an upgrade.
fn current_job(store: &mut Store, request: &Request) -> Result<Option<Job>> {
    store.current_media_job(&request.media_key())
}
fn compatible(config: &crate::config::Config, demand: &Demand, job: &Job) -> Result<bool> {
    if let Some(p) = &job.requester {
        return Ok(p.capture == demand.capture
            && (job.state != "ready"
                || crate::organizer::check_retained_imports(job, config).is_ok()));
    }
    // Uncaptured work cannot promise a frozen profile or destination after restart.
    if job.state != "ready" || job.imports.is_empty() {
        return Ok(false);
    }
    let capture = Capture::new(
        config,
        &Policy {
            enabled: true,
            ..Policy::initial(config)
        },
        &job.request.kind,
    )?;
    let root = Path::new(if job.request.kind == "movie" {
        &capture.movies_root
    } else {
        &capture.series_root
    });
    Ok(capture == demand.capture
        && crate::organizer::check_retained_imports(job, config).is_ok()
        // Even a shared operator job must satisfy the new demand's destination.
        && job.imports.iter().all(|file| {
            let path = Path::new(file);
            path != root && path.starts_with(root)
        })
        && job.release.as_ref().map_or_else(
            || capture.profile == crate::selection::Profile::default(),
            |r| {
                r.profile == capture.profile_name
                    && capture
                        .profile
                        .assess_with_runtime(&r.title, &job.request.title, r.size_bytes, Some(1), job.request.runtime_minutes)
                        .accepted
            },
        ))
}
pub(crate) fn interest(
    ledger: &impl LedgerRead,
    job: &Job,
    config: &crate::config::Config,
) -> Result<bool> {
    if job.requester.is_none() || ledger.operator_interest(&job.id)? {
        return Ok(true);
    }
    let capture = &job
        .requester
        .as_ref()
        .ok_or("Requester: missing provenance")?
        .capture;
    for account in &config.requesters.accounts {
        let Some(record) = ledger.accounts().get(&account.id) else {
            continue;
        };
        if account.binding() != record.binding {
            continue;
        }
        let id = Demand::identity(&account.id, &job.request);
        if let Some(demand) = ledger.demand(&id)?
            && matches!(demand.state.as_str(), "reserved" | "active" | "ready")
            && &demand.capture == capture
        {
            return Ok(true);
        }
    }
    Ok(false)
}
fn public_account(
    ledger: &super::RequesterStore,
    record: &Record,
    config: &crate::config::Config,
) -> Result<Value> {
    let mut value = record.to_json();
    if let Value::Object(fields) = &mut value {
        fields.remove("binding");
    }
    value.insert(
        "configured",
        config
            .requesters
            .accounts
            .iter()
            .any(|account| account.id == record.id),
    );
    value.insert(
        "pending",
        Value::Number(ledger.count(&record.id, Some("pending"))? as f64),
    );
    value.insert("origin_cleanup_pending", !ledger.poll_ready(&record.id)?);
    value.insert("sweep", sweep_value(ledger.sweep(&record.id)?.as_ref()));
    Ok(value)
}

fn requester_value(
    ledger: &super::RequesterStore,
    id: &str,
    demands: Vec<Demand>,
    next: Option<crate::inventory::Key>,
    limit: usize,
    config: &crate::config::Config,
) -> Result<Value> {
    if !valid_id(id) || limit == 0 || limit > 200 {
        return Err("Requester: invalid account or page bounds".into());
    }
    let record = ledger
        .accounts()
        .get(id)
        .ok_or("Requester: unknown account")?;
    let mut value = public_account(ledger, record, config)?;
    value.insert("total", Value::Number(ledger.count(id, None)? as f64));
    value.insert("limit", Value::Number(limit as f64));
    value.insert(
        "next",
        next.map_or(Value::Null, |key| super::encode_cursor(&key).into()),
    );
    value.insert("sequence", ledger.sequence()?.to_string());
    value.insert(
        "demands",
        Value::Array(demands.iter().map(Demand::to_json).collect()),
    );
    Ok(value)
}

impl Engine {
    /// Precedence: title binding, then the requester capture, then the global profile.
    pub(crate) fn configuration_for(&self, job: &Job) -> Result<crate::config::Config> {
        let mut config = job.requester.as_ref().map_or_else(
            || self.config.clone(),
            |p| p.capture.configuration(&self.config, &job.request.kind),
        );
        if let Some(name) = self.title_binding(&job.request)? {
            // The title policy overrides even a capture with the same name.
            // Keep the captured destination, but resolve the profile from the
            // current configuration; a removed name must remain unresolved.
            if let Some(profile) = self.config.selection.profiles.get(&name) {
                config
                    .selection
                    .profiles
                    .insert(name.clone(), profile.clone());
            } else {
                config.selection.profiles.remove(&name);
            }
            crate::profiles::bind(&mut config, &job.request.kind, &name);
        }
        Ok(config)
    }
    pub fn requesters(&self) -> Result<Value> {
        let ledger = lock(&self.requester_store)?;
        let mut v = Value::object();
        v.insert(
            "accounts",
            Value::Array(
                ledger
                    .state
                    .accounts
                    .values()
                    .map(|a| public_account(&ledger, a, &self.config))
                    .collect::<Result<Vec<_>>>()?,
            ),
        );
        v.insert(
            "destinations",
            Value::Array(
                std::iter::once("default")
                    .chain(
                        self.config
                            .requesters
                            .destinations
                            .iter()
                            .map(|d| d.id.as_str()),
                    )
                    .map(Value::from)
                    .collect(),
            ),
        );
        Ok(v)
    }
    pub fn requester(&self, id: &str, after: Option<&str>, limit: usize) -> Result<Value> {
        if !valid_id(id) || limit == 0 || limit > 200 {
            return Err("Requester: invalid account or page bounds".into());
        }
        let ledger = lock(&self.requester_store)?;
        let after = after.map(super::decode_cursor).transpose()?;
        let (page, next) = ledger.account_page(id, after, limit)?;
        requester_value(&ledger, id, page, next, limit, &self.config)
    }
    pub fn requester_control(&self, id: &str, query: &ControlRequest) -> Result<Value> {
        query.validate()?;
        if !valid_id(id) {
            return Err("Requester: invalid account ID".into());
        }
        let configured = self
            .config
            .requesters
            .accounts
            .iter()
            .find(|a| a.id == id)
            .ok_or("Requester: account is not configured")?;
        let mut ledger = lock(&self.requester_store)?;
        let mut jobs = lock(&self.store)?;
        let record = ledger
            .state
            .accounts
            .get(id)
            .ok_or("Requester: unknown account")?
            .clone();
        if configured.binding() != record.binding {
            return Err("Requester: account identity changed".into());
        }
        if let Some(policy) = &query.policy {
            policy.validate_config(&self.config)?;
        }
        let mut proposed = ledger.state.clone();
        if query.action == "policy" {
            let account = proposed
                .accounts
                .get_mut(id)
                .ok_or("Requester: unknown account")?;
            account.policy = query.policy.clone().ok_or("Requester: missing policy")?;
            account.revision = account
                .revision
                .checked_add(1)
                .ok_or("Requester: policy revision overflow")?;
            // Unadmitted records adopt this revision in bounded admission.
        } else {
            let demand_id = query
                .demand_id
                .as_deref()
                .ok_or("Requester: missing demand ID")?;
            let mut demand = ledger
                .demand(demand_id)?
                .filter(|d| d.account_id == id)
                .ok_or("Requester: demand belongs to another account or does not exist")?;
            let d = &mut demand;
            match query.action.as_str() {
                "remove" => {
                    d.state = "removed".into();
                }
                "retry" => {
                    if !record.policy.enabled || d.job_id.is_none() || d.state == "removed" {
                        return Err(
                            "Requester: retry requires retained admitted acquisition".into()
                        );
                    }
                    let job = jobs
                        .get(d.job_id.as_deref().ok_or("Requester: missing job")?)?
                        .ok_or("Requester: acquisition is missing")?;
                    if !matches!(job.state.as_str(), "failed" | "cancelled") {
                        return Err("Requester: acquisition is not retryable".into());
                    }
                    d.state = "reserved".into();
                }
                _ => return Err("Requester: invalid action".into()),
            }
            demand.outcome = demand.state.clone();
            proposed.demands.insert(demand_id.to_owned(), demand);
        }
        let mut normalized = query.clone();
        normalized.apply = false;
        normalized.plan_id = None;
        let mut guard = Value::object();
        guard.insert("account", record.to_json());
        guard.insert("ledger_sequence", ledger.sequence()?.to_string());
        guard.insert("query", normalized.to_json());
        if let Some(id) = &query.demand_id {
            let demand = ledger.demand(id)?;
            guard.insert(
                "demand",
                demand.as_ref().map_or(Value::Null, Demand::to_json),
            );
            if let Some(job) = demand.and_then(|d| d.job_id) {
                guard.insert(
                    "job",
                    jobs.get(&job)?.map_or(Value::Null, |job| job.to_json()),
                );
            }
        }
        guard.insert("jobs_sequence", Value::String(jobs.sequence()?.to_string()));
        guard.insert("profiles", self.config.selection.to_json());
        guard.insert(
            "destinations",
            Value::Array(
                std::iter::once((&self.config.movies_root, &self.config.series_root))
                    .chain(
                        self.config
                            .requesters
                            .destinations
                            .iter()
                            .map(|r| (&r.movies_root, &r.series_root)),
                    )
                    .map(|(a, b)| {
                        Value::Array(vec![
                            a.to_string_lossy().into_owned().into(),
                            b.to_string_lossy().into_owned().into(),
                        ])
                    })
                    .collect(),
            ),
        );
        let plan_id = digest(json::stringify(&guard).as_bytes());
        if query.apply {
            if self.read_only {
                return Err("Requester application requires a running writable service".into());
            }
            if query.plan_id.as_deref() != Some(&plan_id) {
                return Err("Requester review changed or expired; preview again".into());
            }
            ledger.save_patch(proposed)?;
            if query.action == "retry" {
                // Admission also requeues a reserved demand's failed job, so a
                // crash between these two writes does not lose the retry.
                let demand_id = query
                    .demand_id
                    .as_deref()
                    .ok_or("Requester: missing demand")?;
                let job_id = ledger
                    .demand(demand_id)?
                    .ok_or("Requester: missing demand")?
                    .job_id
                    .ok_or("Requester: missing acquisition")?;
                jobs.retry(&job_id)?;
                // Same previous-transfer handling as `Engine::retry`.
                self.retire_previous_transfers(&mut jobs)?;
            }
            self.admit_requester_demand(&mut ledger, &mut jobs)?;
        }
        let mut result = Value::object();
        result.insert("account_id", id);
        result.insert("action", query.action.clone());
        result.insert("plan_id", plan_id);
        result.insert("applied", query.apply);
        result.insert(
            "policy",
            query
                .policy
                .as_ref()
                .map_or_else(|| record.policy.to_json(), Policy::to_json),
        );
        if let Some(demand) = &query.demand_id {
            result.insert(
                "demand",
                ledger
                    .demand(demand)?
                    .as_ref()
                    .map_or(Value::Null, Demand::to_json),
            );
        }
        Ok(result)
    }

    /// Mutex order is requester ledger, then jobs. Reservation patches are
    /// acknowledged before job creation; every pass examines at most200 rows.
    pub(crate) fn admit_requester_demand(
        &self,
        ledger: &mut super::RequesterStore,
        jobs: &mut Store,
    ) -> Result<()> {
        let now = store::now();
        let mut patch = ledger.state.clone();
        let mut planned = BTreeMap::new();
        let mut admitted = Vec::new();
        let names = ["reserved", "pending", "conflict"];
        let mut state = ledger.admission_state;
        let mut after = ledger.admission_cursor;
        let mut examined = 0;
        let mut phases = 0;
        while examined < 200 && admitted.len() < 64 && phases < 3 {
            let name = names[state as usize];
            let (page, next) = ledger.queue_page(name, after, 200 - examined)?;
            let mut exhausted = true;
            for original in page {
                examined += 1;
                after = Some(super::index::queue_cursor(name, &original)?);
                let mut demand = original.clone();
                if demand.state == "removed" {
                    patch.demands.insert(demand.id.clone(), demand);
                    continue;
                }
                let record = ledger
                    .state
                    .accounts
                    .get(&demand.account_id)
                    .ok_or("Requester: missing demand account")?;
                if !record.policy.enabled
                    || !self
                        .config
                        .requesters
                        .accounts
                        .iter()
                        .any(|a| a.id == demand.account_id && a.binding() == record.binding)
                {
                    continue;
                }
                if demand.revision != record.revision && demand.admitted_at.is_none() {
                    demand.capture =
                        Capture::new(&self.config, &record.policy, &demand.request.kind)?;
                    demand.revision = record.revision;
                    demand.state = "pending".into();
                }
                let media = demand.request.media_key();
                let mut conflict = planned
                    .get(&media)
                    .is_some_and(|capture| capture != &demand.capture);
                for account in ledger.accounts().keys() {
                    if let Some(reserved) =
                        ledger.demand(&Demand::identity(account, &demand.request))?
                        && reserved.state == "reserved"
                        && reserved.capture != demand.capture
                    {
                        conflict = true;
                    }
                }
                let existing = current_job(jobs, &demand.request)?;
                if conflict
                    || existing
                        .as_ref()
                        .is_some_and(|job| !compatible(&self.config, &demand, job).unwrap_or(false))
                {
                    demand.state = "conflict".into();
                    demand.outcome = "conflict".into();
                } else {
                    demand.admitted_at.get_or_insert(now);
                    demand.state = "reserved".into();
                    demand.outcome = "reserved".into();
                    planned.insert(media, demand.capture.clone());
                    admitted.push((demand.clone(), existing));
                }
                if demand != original {
                    patch.demands.insert(demand.id.clone(), demand);
                }
                if admitted.len() == 64 || examined == 200 {
                    exhausted = false;
                    break;
                }
            }
            if exhausted && next.is_none() {
                state = (state + 1) % 3;
                after = None;
                phases += 1;
            } else {
                break;
            }
        }
        if !patch.demands.is_empty() {
            ledger.save_patch(patch)?;
        }
        let mut linked = ledger.state.clone();
        for (mut demand, existing) in admitted {
            let job = if let Some(job) = existing {
                if !compatible(&self.config, &demand, &job)? {
                    return Err("Requester: reservation conflicts with queued acquisition".into());
                }
                // Only a retry reserves a demand already linked to its failed
                // acquisition. Requeuing it here resumes a retry interrupted
                // between its two writes. If requeuing fails, the demand stays
                // reserved so that a later pass tries again.
                if demand.job_id.as_deref() == Some(job.id.as_str())
                    && matches!(job.state.as_str(), "failed" | "cancelled")
                {
                    match jobs.retry(&job.id) {
                        Ok(job) => job,
                        Err(error) => {
                            eprintln!("mynou: requester retry of {} waits: {error}", job.id);
                            continue;
                        }
                    }
                } else {
                    job
                }
            } else {
                jobs.submit_requester(
                    demand.request.clone(),
                    Provenance {
                        account_id: demand.account_id.clone(),
                        demand_id: demand.id.clone(),
                        policy_revision: demand.revision,
                        capture: demand.capture.clone(),
                    },
                )?
            };
            demand.job_id = Some(job.id.clone());
            demand.state = if job.state == "ready" {
                "ready"
            } else {
                "active"
            }
            .into();
            demand.outcome = if matches!(job.state.as_str(), "ready" | "failed" | "cancelled") {
                job.state.clone()
            } else {
                "active".into()
            };
            linked.demands.insert(demand.id.clone(), demand);
        }
        if !linked.demands.is_empty() {
            ledger.save_patch(linked)?;
        }

        ledger.save_admission_cursor(state, after)?;
        let (page, next) = ledger.demands_page(ledger.outcome_cursor, 200)?;
        let mut outcomes = ledger.state.clone();
        for original in page {
            let mut demand = original.clone();
            if let Some(id) = &demand.job_id
                && demand.state != "removed"
            {
                let job = jobs
                    .get(id)?
                    .ok_or("Requester: recorded acquisition is missing")?;
                let outcome = if job.state == "ready" {
                    if compatible(&self.config, &demand, &job)? {
                        "ready"
                    } else {
                        "conflict"
                    }
                } else {
                    &job.state
                };
                if matches!(outcome, "ready" | "conflict") {
                    demand.state = outcome.into();
                }
                if matches!(outcome, "ready" | "conflict" | "failed" | "cancelled") {
                    demand.outcome = outcome.into();
                }
            }
            if demand != original {
                outcomes.demands.insert(demand.id.clone(), demand);
            }
        }
        if !outcomes.demands.is_empty() {
            ledger.save_patch(outcomes)?;
        }
        ledger.save_outcome_cursor(next)?;
        let accounts: Vec<_> = ledger.accounts().keys().cloned().collect();
        let mut cleaned = 0;
        for account in accounts {
            if !ledger.poll_ready(&account)? && cleaned < 4 {
                ledger.reconcile_origins(&account)?;
                cleaned += 1;
            }
        }
        cancel_unwanted(ledger, jobs, &self.config, false)?;
        self.pause_unwanted_transfers(jobs)?;
        self.retire_previous_transfers(jobs)?;
        if !self.stopped.load(Ordering::Acquire) {
            ledger.finish_backlog_pass(store::now())?;
        }
        Ok(())
    }
    pub(crate) fn requester_reconcile(&self) -> Result<()> {
        if self.config.requesters.accounts.is_empty()
            && lock(&self.requester_store)?.state.accounts.is_empty()
        {
            return Ok(());
        }
        if self.read_only {
            return Ok(());
        }
        let mut ledger = lock(&self.requester_store)?;
        let mut jobs = lock(&self.store)?;
        self.admit_requester_demand(&mut ledger, &mut jobs)
    }
    pub(crate) fn claim_requester_job(&self) -> Result<Option<Job>> {
        let ledger = lock(&self.requester_store)?;
        lock(&self.store)?.claim_filtered(store::now(), self.config.lease_duration_secs, |job| {
            Ok(interest(&*ledger, job, &self.config)?
                && !crate::irc::routing::waits_for_candidate(
                    &self.config,
                    job,
                    &*ledger,
                    self.title_binding(&job.request)?.as_deref(),
                )?)
        })
    }
    pub(crate) fn requester_operator_interest(&self, jobs: &[Job]) -> Result<()> {
        if jobs.iter().all(|j| j.requester.is_none()) {
            return Ok(());
        }
        let mut ledger = lock(&self.requester_store)?;
        let mut next = ledger.state.clone();
        for j in jobs {
            if j.requester.is_some() {
                next.operator_jobs.insert(j.id.clone());
            }
        }
        if next != ledger.state {
            ledger.save_patch(next)?;
        }
        Ok(())
    }
    pub(crate) fn requester_retry_allowed(&self, id: &str) -> Result<()> {
        let ledger = lock(&self.requester_store)?;
        let mut jobs = lock(&self.store)?;
        let job = jobs.get(id)?.ok_or("Unknown job")?;
        if job.requester.is_none() || ledger.operator_interest(id)? {
            return Ok(());
        }
        if !interest(&*ledger, &job, &self.config)? {
            return Err("Requester: acquisition has no admitted demand".into());
        }
        Ok(())
    }
    /// Advances every account's watchlist sweep by a bounded number of pages.
    /// Accounts polled least recently go first, so one large watchlist resumes
    /// in later passes instead of starving the others.
    pub fn sync_requesters(&self) -> Result<Value> {
        if self.read_only {
            return Err("Requester polling requires a running writable service".into());
        }
        let _sync = lock(&self.sync_lock)?;
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut reports = Vec::new();
        let attempted = lock(&self.requester_store)?.state.accounts.clone();
        let mut accounts: Vec<_> = self.config.requesters.accounts.iter().collect();
        accounts.sort_by_key(|a| (attempted[&a.id].polled_at, a.id.clone()));
        for account in accounts {
            let account_deadline = deadline.min(Instant::now() + Duration::from_secs(10));
            {
                // Retiring the origins a published sweep did not see shares the
                // account's time budget, one bounded page per lock.
                for _ in 0..CLEANUP_PAGES {
                    let mut ledger = lock(&self.requester_store)?;
                    if ledger.poll_ready(&account.id)? || Instant::now() >= account_deadline {
                        break;
                    }
                    ledger.reconcile_origins(&account.id)?;
                }
                let ledger = lock(&self.requester_store)?;
                if !ledger.poll_ready(&account.id)? {
                    let mut report = Value::object();
                    report.insert("account_id", account.id.clone());
                    report.insert("success", false);
                    report.insert("error", "Previous complete poll origin cleanup is pending");
                    reports.push(report);
                    continue;
                }
            }
            let mut progress = SweepReport::default();
            let result = loop {
                if progress.pages == SWEEP_PAGES {
                    break Ok(());
                }
                if self.stopped.load(Ordering::Acquire) || Instant::now() >= account_deadline {
                    break if progress.pages == 0 {
                        Err(integrations::PLEX_POLL_DEADLINE.to_owned())
                    } else {
                        Ok(())
                    };
                }
                match self.sweep_requester_page(account, account_deadline, &mut progress) {
                    Ok(true) => break Ok(()),
                    Ok(false) => {}
                    Err(error) => break Err(error),
                }
            };
            let mut report = Value::object();
            report.insert("account_id", account.id.clone());
            let mut ledger = lock(&self.requester_store)?;
            match result {
                Ok(()) => {
                    let record = ledger
                        .state
                        .accounts
                        .get(&account.id)
                        .ok_or("Requester: missing account")?;
                    report.insert("cursor", record.cursor.to_string());
                    report.insert("success", true);
                    // Published: this poll read the whole watchlist.
                    report.insert("complete", record.cursor != attempted[&account.id].cursor);
                }
                Err(error) => {
                    let error = if [
                        "Plex account identity changed",
                        "Plex account policy changed during poll",
                        integrations::PLEX_POLL_DEADLINE,
                    ]
                    .contains(&error.as_str())
                    {
                        error
                    } else {
                        "Plex account poll failed".into()
                    };
                    let mut next = ledger.state.clone();
                    let current = next
                        .accounts
                        .get_mut(&account.id)
                        .ok_or("Requester: missing account")?;
                    if error != integrations::PLEX_POLL_DEADLINE {
                        current.polled_at = store::now();
                    }
                    current.last_error = Some(error.clone());
                    // Committed batches stay; the sweep resumes from its cursor.
                    ledger.save_patch(next)?;
                    report.insert("error", error);
                    report.insert("success", false);
                    report.insert("complete", false);
                }
            }
            report.insert(
                "sweep",
                progress.to_json(ledger.sweep(&account.id)?.as_ref()),
            );
            let mut jobs = lock(&self.store)?;
            self.admit_requester_demand(&mut ledger, &mut jobs)?;
            reports.push(report);
        }
        let mut v = Value::object();
        v.insert("accounts", Value::Array(reports));
        Ok(v)
    }
    /// Reads, expands and commits one watchlist page of an account's sweep.
    /// Returns true when the sweep ended: published, or restarted because the
    /// listing changed while it was read.
    fn sweep_requester_page(
        &self,
        account: &crate::requesters::Account,
        deadline: Instant,
        progress: &mut SweepReport,
    ) -> Result<bool> {
        let (previous, mut sweep) = {
            let ledger = lock(&self.requester_store)?;
            let previous = ledger
                .state
                .accounts
                .get(&account.id)
                .ok_or("Requester: missing account")?
                .clone();
            // A sweep at or below the published generation is left over from
            // a binary that polled without sweeps; it cannot be resumed.
            let sweep = match ledger.sweep(&account.id)? {
                Some(sweep) if sweep.epoch > previous.cursor => sweep,
                _ => Sweep::new(
                    previous
                        .cursor
                        .checked_add(1)
                        .ok_or("Requester: cursor overflow")?,
                    store::now(),
                ),
            };
            (previous, sweep)
        };
        if progress.pages == 0 && sweep.cursor.position > 0 {
            sweep.multipass = true;
        }
        let page =
            integrations::requester_watchlist_page(&self.config, account, &sweep.cursor, deadline)?;
        progress.pages += 1;
        progress.requests += u64::from(page.requests);
        previous.policy.validate_config(&self.config)?;
        let mut batch = Vec::new();
        for (after, item) in page.entries {
            let origin = item.media_key();
            let expanded = if Instant::now() >= deadline {
                Err(integrations::PLEX_POLL_DEADLINE.to_owned())
            } else {
                self.expand_requester_item(&origin, &item, deadline)
            };
            let expanded = match expanded {
                Ok(expanded) => expanded,
                Err(error) => {
                    // Keep the items already resolved, so that a slow or
                    // failing entry cannot make the sweep repeat its page.
                    self.commit_sweep_batch(
                        account,
                        &previous,
                        batch,
                        Progress::Continue(sweep),
                        progress,
                    )?;
                    return Err(error);
                }
            };
            if !batch.is_empty() && batch.len() + expanded.len() > MAX_BATCH_ITEMS {
                let full = std::mem::take(&mut batch);
                self.commit_sweep_batch(
                    account,
                    &previous,
                    full,
                    Progress::Continue(sweep.clone()),
                    progress,
                )?;
            }
            // One item may expand beyond a batch; its earlier chunks keep the
            // cursor before the item, so an interruption repeats the item.
            let mut expanded = expanded;
            while expanded.len() > MAX_BATCH_ITEMS {
                let rest = expanded.split_off(MAX_BATCH_ITEMS);
                let chunk = std::mem::replace(&mut expanded, rest);
                self.commit_sweep_batch(
                    account,
                    &previous,
                    chunk,
                    Progress::Continue(sweep.clone()),
                    progress,
                )?;
            }
            batch.extend(expanded);
            sweep.cursor = after;
            sweep.observe(&origin);
        }
        let mut finished = false;
        let next = if page.complete && page.next.consistent {
            finished = true;
            sweep.cursor = page.next;
            if (sweep.multipass || sweep.verification.is_some())
                && sweep.verification != Some(sweep.digest)
            {
                // Anchors and totals cannot detect every same-total reorder.
                // Confirm the entire traversal before allowing any absence to
                // become a durable tombstone. Each pass keeps its usual bounds.
                let mut confirmation = Sweep::new(
                    sweep
                        .epoch
                        .checked_add(1)
                        .ok_or("Requester: cursor overflow")?,
                    store::now(),
                );
                confirmation.verification = Some(sweep.digest);
                Progress::Continue(confirmation)
            } else {
                progress.published = true;
                Progress::Publish(sweep)
            }
        } else if page.complete {
            // The listing moved under the sweep: nothing may be inferred from
            // it, so a new sweep with a new generation starts from the top.
            finished = true;
            progress.restarted = true;
            Progress::Continue(Sweep::new(
                sweep
                    .epoch
                    .checked_add(1)
                    .ok_or("Requester: cursor overflow")?,
                store::now(),
            ))
        } else {
            sweep.cursor = page.next;
            Progress::Continue(sweep)
        };
        self.commit_sweep_batch(account, &previous, batch, next, progress)?;
        Ok(finished)
    }
    /// Commits one bounded batch of demands together with the sweep progress.
    fn commit_sweep_batch(
        &self,
        account: &crate::requesters::Account,
        previous: &Record,
        batch: Vec<(String, Request)>,
        stage: Progress,
        progress: &mut SweepReport,
    ) -> Result<()> {
        let mut ledger = lock(&self.requester_store)?;
        let mut next = ledger.state.clone();
        let current = next
            .accounts
            .get(&account.id)
            .ok_or("Requester: missing account")?;
        if current.revision != previous.revision || current.binding != previous.binding {
            return Err("Plex account policy changed during poll".into());
        }
        let policy = &previous.policy;
        let mut origins = BTreeSet::new();
        progress.rows += batch.len() as u64;
        for (origin, request) in batch {
            let id = Demand::identity(&account.id, &request);
            let mut demand = if let Some(known) = next.demands.remove(&id) {
                known
            } else if let Some(known) = ledger.demand(&id)? {
                known
            } else {
                Demand {
                    id: id.clone(),
                    account_id: account.id.clone(),
                    origins: BTreeSet::new(),
                    request: request.clone(),
                    revision: previous.revision,
                    capture: Capture::new(&self.config, policy, &request.kind)?,
                    state: "pending".into(),
                    job_id: None,
                    admitted_at: None,
                    outcome: "pending".into(),
                }
            };
            // Origins the published generation no longer holds were already
            // dropped on read; the sweep only adds what it sees.
            demand.origins.insert(origin.clone());
            if demand.job_id.is_none()
                && demand.admitted_at.is_none()
                && demand.revision != previous.revision
                && demand.state != "removed"
            {
                demand.capture = Capture::new(&self.config, policy, &request.kind)?;
                demand.revision = previous.revision;
                demand.state = "pending".into();
            }
            next.demands.insert(id, demand);
            origins.insert(origin);
        }
        let current = next
            .accounts
            .get_mut(&account.id)
            .ok_or("Requester: missing account")?;
        current.polled_at = store::now();
        current.last_error = None;
        if let Progress::Publish(sweep) = &stage {
            current.cursor = sweep.epoch;
            current.snapshot = Some(
                sweep
                    .digest
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            );
        }
        ledger.save_poll_patch(
            next,
            Some(Stage {
                account: &account.id,
                origins: &origins,
                progress: stage,
            }),
        )?;
        progress.commits += 1;
        let mut jobs = lock(&self.store)?;
        self.admit_requester_demand(&mut ledger, &mut jobs)
    }
    /// Requests one watchlist item stands for: a movie, or the aired episodes
    /// of a series, all under the item's origin.
    fn expand_requester_item(
        &self,
        origin: &str,
        item: &Request,
        deadline: Instant,
    ) -> Result<Vec<(String, Request)>> {
        if item.kind == "movie" {
            return Ok(vec![(
                origin.to_owned(),
                integrations::requester_movie_before(&self.config, item, deadline)?,
            )]);
        }
        let mut identity = item.clone();
        identity.kind = "series".into();
        identity.validate()?;
        let matches = lock(&self.series_store)?.matching(&identity)?;
        if matches.len() > 1 {
            return Err("Requester: ambiguous retained catalog identity".into());
        }
        let previous = matches.into_iter().next();
        // A series already monitored is fetched the way its own refresh
        // fetches it, so its specials and numbering survive; only regular
        // episodes become demands below.
        let plan = match &previous {
            Some(previous) => previous.fetch_catalog(self, false, deadline)?,
            None => integrations::series_plan_before(&self.config, &identity, false, deadline)?,
        };
        let record = if let Some(previous) = previous {
            let normalized = previous.normalize_catalog(plan)?;
            let mut checked = previous.clone();
            checked.accept_catalog(normalized)?;
            let mut series = lock(&self.series_store)?;
            if series.get(&previous.id)?.as_ref() != Some(&previous) {
                return Err("Requester: captured series scope changed during poll".into());
            }
            series.save(checked.clone())?;
            checked
        } else {
            lock(&self.series_store)?.subscribe(plan, false, false, false, store::now())?
        };
        let today = date::today();
        Ok(record
            .plan
            .episodes
            .iter()
            .filter(|e| {
                e.catalog_id.is_some()
                    && e.season != 0
                    && e.air_date
                        .as_deref()
                        .is_some_and(|date| date <= today.as_str())
            })
            .map(|e| (origin.to_owned(), record.episode_request(e)))
            .collect())
    }
}

/// Watchlist pages read per account in one pass; the account deadline may
/// stop it earlier.
const SWEEP_PAGES: usize = 8;

/// Retained-demand pages whose unseen origins one pass may retire.
const CLEANUP_PAGES: usize = 64;

/// What one pass did for an account: work counters bound the pass.
#[derive(Default)]
struct SweepReport {
    pages: usize,
    requests: u64,
    rows: u64,
    commits: u64,
    published: bool,
    restarted: bool,
}
impl SweepReport {
    fn to_json(&self, sweep: Option<&Sweep>) -> Value {
        let mut v = Value::object();
        v.insert("pages", Value::Number(self.pages as f64));
        v.insert("requests", Value::Number(self.requests as f64));
        v.insert("rows", Value::Number(self.rows as f64));
        v.insert("commits", Value::Number(self.commits as f64));
        v.insert("published", self.published);
        v.insert("restarted", self.restarted);
        v.insert("progress", sweep_value(sweep));
        v
    }
}
/// The unfinished sweep of an account: how far it got in the listing.
fn sweep_value(sweep: Option<&Sweep>) -> Value {
    let Some(sweep) = sweep else {
        return Value::Null;
    };
    let mut v = Value::object();
    v.insert("generation", sweep.epoch.to_string());
    v.insert("position", sweep.cursor.position.to_string());
    v.insert(
        "total",
        sweep
            .cursor
            .total
            .map_or(Value::Null, |total| total.to_string().into()),
    );
    v.insert("consistent", sweep.cursor.consistent);
    v.insert("verifying", sweep.verification.is_some());
    v.insert("started_at", sweep.started_at.to_string());
    v
}

/// One page of requester provenance validation: the rows read and the
/// cursor of the next page in this stage.
pub(crate) fn validate_storage_page(
    ledger: &impl LedgerRead,
    jobs: &mut impl crate::store::RecoveryJobs,
    stage: u8,
    after: Option<crate::inventory::Key>,
) -> Result<(usize, Option<crate::inventory::Key>)> {
    if stage == 0 {
        let (page, next) = ledger.demands_page(after, 200)?;
        let rows = page.len();
        for demand in page {
            if let Some(id) = &demand.job_id {
                let job = jobs
                    .get(id)?
                    .ok_or("Requester: recorded acquisition is missing")?;
                if job.request.media_key() != demand.request.media_key()
                    || job
                        .requester
                        .as_ref()
                        .is_some_and(|p| p.capture != demand.capture)
                {
                    return Err("Requester: acquisition differs from retained demand".into());
                }
            }
        }
        return Ok((rows, next));
    }
    if stage != 1 {
        return Err("Requester: unknown validation stage".into());
    }
    // Every request that carries requester provenance, one page at a time.
    let (page, next) = jobs.requester_jobs_page(after, 200)?;
    let rows = page.len();
    for job in page {
        let Some(p) = &job.requester else {
            continue;
        };
        let d = ledger
            .demand(&p.demand_id)?
            .ok_or("Requester: queued provenance has no durable demand")?;
        if d.account_id != p.account_id
            || d.capture != p.capture
            || d.revision != p.policy_revision
            || d.request.media_key() != job.request.media_key()
            || d.admitted_at.is_none()
        {
            return Err("Requester: queued provenance differs from durable admission".into());
        }
    }
    Ok((rows, next))
}

/// Requests examined by one sweep for work no demand wants any more.
const SWEEP_JOBS: usize = 200;

/// Cancels unfinished requester work that no admitted demand needs. One call
/// examines at most `SWEEP_JOBS` requests during routine reconciliation.
/// The durable cursor advances only after the page settles. A full sweep
/// restarts from the beginning and repeats bounded pages until the end.
pub(crate) fn cancel_unwanted(
    ledger: &impl LedgerRead,
    jobs: &mut Store,
    config: &crate::config::Config,
    full: bool,
) -> Result<()> {
    if full {
        jobs.finish_requester_sweep(None)?;
    }
    loop {
        let (_, done) = cancel_unwanted_page(ledger, jobs, config)?;
        if !full || done {
            return Ok(());
        }
    }
}

/// One settled page of the requester cancellation sweep: the requests
/// examined and whether the sweep reached the end of the inventory.
pub(crate) fn cancel_unwanted_page(
    ledger: &impl LedgerRead,
    jobs: &mut Store,
    config: &crate::config::Config,
) -> Result<(usize, bool)> {
    let (page, next) = jobs.sweep_requester_jobs(SWEEP_JOBS)?;
    let rows = page.len();
    for job in page {
        if job.requester.is_some()
            && job.state != "ready"
            && job.state != "cancelled"
            && !interest(ledger, &job, config)?
        {
            if job.shared_file.is_some() {
                let owners = jobs.shared_group(&job.id)?;
                let mut wanted = false;
                for owner in &owners {
                    wanted |= interest(ledger, owner, config)?;
                }
                if wanted {
                    continue;
                }
            }
            jobs.cancel(&job.id)?;
        }
    }
    jobs.finish_requester_sweep(next)?;
    Ok((rows, next.is_none()))
}

#[cfg(test)]
pub(crate) mod regression_tests {
    use super::*;
    use crate::net::{HttpClient, HttpConnector, NetworkDatagram, NetworkStream};
    use crate::torrent::transport::{Network, NetworkListener};
    use std::{
        io::{self, Cursor, Read, Write},
        net::SocketAddr,
        sync::Arc,
    };

    type Reply = fn(&str) -> (u16, String);
    pub(crate) struct HttpNetwork(pub(crate) Reply);
    struct Stream {
        reply: Reply,
        request: Vec<u8>,
        response: Option<Cursor<Vec<u8>>>,
    }
    impl Read for Stream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let response = self.response.get_or_insert_with(|| {
                let (status, body) = (self.reply)(std::str::from_utf8(&self.request).unwrap());
                Cursor::new(format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes())
            });
            response.read(buffer)
        }
    }
    impl Write for Stream {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.request.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl NetworkStream for Stream {
        fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
        fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }
    impl HttpConnector for HttpNetwork {
        fn connect(&self, _: &str, _: u16, _: Instant) -> Result<Box<dyn NetworkStream>> {
            Ok(Box::new(Stream {
                reply: self.0,
                request: Vec::new(),
                response: None,
            }))
        }
    }
    impl Network for HttpNetwork {
        fn generation(&self) -> Result<Option<String>> {
            Ok(Some("fixture".into()))
        }
        fn connect(&self, _: SocketAddr, _: Duration) -> Result<Box<dyn NetworkStream>> {
            Err("No peer sockets in fixture".into())
        }
        fn resolve(&self, _: &str, _: Instant) -> Result<Vec<SocketAddr>> {
            Err("No DNS in fixture".into())
        }
        fn datagram(&self, _: bool) -> Result<Box<dyn NetworkDatagram>> {
            Err("No datagrams in fixture".into())
        }
        fn http(&self) -> HttpClient {
            HttpClient::new()
                .without_proxy()
                .with_connector(Arc::new(Self(self.0)))
        }
        fn listen(&self, _: SocketAddr) -> Result<Box<dyn NetworkListener>> {
            Err("No listeners in fixture".into())
        }
    }

    #[test]
    fn watchlist_expansion_preserves_monitored_specials() {
        let network = HttpNetwork(|request| {
            let body = if request.contains("/season/0") {
                r#"{"season_number":0,"episodes":[{"id":10,"episode_number":1,"name":"Special","air_date":"2020-01-01"}]}"#
            } else if request.contains("/season/1") {
                r#"{"season_number":1,"episodes":[{"id":11,"episode_number":1,"name":"Regular","air_date":"2020-01-01"}]}"#
            } else {
                r#"{"id":42,"seasons":[{"season_number":0},{"season_number":1}]}"#
            };
            (200, body.into())
        });
        let root = std::env::temp_dir().join(format!(
            "mynou-specials-{}",
            digest(&crate::crypto::random_bytes::<16>().unwrap())
        ));
        let mut config = crate::config::from_json(&crate::config::default_json(), &root).unwrap();
        config.downloads_enabled = false;
        config.catalog.enabled = true;
        config.catalog.url = "http://catalog.fixture".into();
        config.vpn.tmdb = true;
        config.catalog.token_env = "MYNOU_TEST_SECRET_A".into();
        let engine = Engine::open_with_network(config, Arc::new(network), false).unwrap();
        let request = Request {
            kind: "series".into(),
            title: "Synthetic show".into(),
            year: 2020,
            season: 0,
            episode: 0,
            tmdb_id: Some(42),
            source_path: None,
            source_url: None,
            runtime_minutes: None,
            source_numbering: None,
        };
        let episodes = [0, 1]
            .into_iter()
            .map(|season| crate::series::Episode {
                runtime_minutes: None,
                catalog_id: Some(10 + u64::from(season)),
                season,
                episode: 1,
                title: "Episode".into(),
                air_date: Some("2020-01-01".into()),
            })
            .collect();
        let retained = lock(&engine.series_store)
            .unwrap()
            .subscribe(
                crate::series::Plan {
                    request: request.clone(),
                    episodes,
                },
                true,
                false,
                true,
                store::now(),
            )
            .unwrap();
        let demands = engine
            .expand_requester_item(
                "watchlist:42",
                &request,
                Instant::now() + Duration::from_secs(10),
            )
            .unwrap();
        let updated = lock(&engine.series_store)
            .unwrap()
            .get(&retained.id)
            .unwrap()
            .unwrap();
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
        assert_eq!(demands.len(), 1);
        assert_eq!(demands[0].1.season, 1);
        assert!(
            updated
                .plan
                .episodes
                .iter()
                .any(|episode| episode.season == 0)
        );
    }
}
