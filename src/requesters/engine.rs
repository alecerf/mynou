//! Account I/O precedes admission. Snapshot reservations precede queued jobs.
use super::{
    Capture, ControlRequest, Demand, MAX_DEMANDS, MAX_POLL_ITEMS, Policy, Provenance, Record,
    State, digest, valid_id,
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
    fs,
    path::{Component, Path},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

struct Watchlist {
    origins: BTreeSet<String>,
    requests: Vec<(String, Request)>,
}

fn current_job(store: &Store, request: &Request) -> Option<Job> {
    let key = request.media_key();
    store
        .library_jobs()
        .into_iter()
        .find(|j| j.request.media_key() == key)
        .or_else(|| {
            store
                .list()
                .into_iter()
                .filter(|j| j.request.media_key() == key && j.upgrade_parent.is_none())
                .min_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)))
        })
}
fn compatible(config: &crate::config::Config, demand: &Demand, job: &Job) -> Result<bool> {
    if let Some(p) = &job.requester {
        return Ok(p.capture == demand.capture);
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
        && job.imports.iter().all(|file| {
            let path = Path::new(file);
            path.is_absolute()
                && path.starts_with(root)
                && !path.components().any(|c| c == Component::ParentDir)
                && store::reject_symlinks(path).is_ok()
                && fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
        })
        && job.release.as_ref().map_or_else(
            || capture.profile == crate::selection::Profile::default(),
            |r| {
                r.profile == capture.profile_name
                    && capture
                        .profile
                        .assess(&r.title, &job.request.title)
                        .accepted
            },
        ))
}
pub(crate) fn interest(state: &State, job: &Job, config: &crate::config::Config) -> bool {
    if job.requester.is_none() || state.operator_jobs.contains(&job.id) {
        return true;
    }
    let key = job.request.media_key();
    state.demands.values().any(|d| {
        d.request.media_key() == key
            && matches!(d.state.as_str(), "reserved" | "active" | "ready")
            && job
                .requester
                .as_ref()
                .is_some_and(|p| p.capture == d.capture)
            && config.requesters.accounts.iter().any(|a| {
                a.id == d.account_id && a.binding() == state.accounts[&d.account_id].binding
            })
    })
}
fn public_account(state: &State, record: &Record, config: &crate::config::Config) -> Value {
    let mut v = record.to_json();
    if let Value::Object(m) = &mut v {
        m.remove("binding");
    }
    v.insert(
        "configured",
        config.requesters.accounts.iter().any(|a| a.id == record.id),
    );
    v.insert(
        "pending",
        state
            .demands
            .values()
            .filter(|d| d.account_id == record.id && d.state == "pending")
            .count() as u32,
    );
    v
}

impl Engine {
    pub(crate) fn configuration_for(&self, job: &Job) -> crate::config::Config {
        job.requester.as_ref().map_or_else(
            || self.config.clone(),
            |p| p.capture.configuration(&self.config, &job.request.kind),
        )
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
                    .map(|a| public_account(&ledger.state, a, &self.config))
                    .collect(),
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
        v.insert(
            "legacy_single_account",
            self.config.requesters.accounts.is_empty(),
        );
        Ok(v)
    }
    /// Bounded pages avoid unbounded API/browser response bodies.
    pub fn requester(&self, id: &str, offset: usize, limit: usize) -> Result<Value> {
        if !valid_id(id) || limit == 0 || limit > 200 || offset > MAX_DEMANDS {
            return Err("Requester: invalid account or page bounds".into());
        }
        let ledger = lock(&self.requester_store)?;
        let record = ledger
            .state
            .accounts
            .get(id)
            .ok_or("Requester: unknown account")?;
        let mut v = public_account(&ledger.state, record, &self.config);
        let demands: Vec<_> = ledger
            .state
            .demands
            .values()
            .filter(|d| d.account_id == id)
            .collect();
        v.insert("total", demands.len() as u32);
        v.insert("offset", offset as u32);
        v.insert("limit", limit as u32);
        v.insert(
            "demands",
            Value::Array(
                demands
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .map(Demand::to_json)
                    .collect(),
            ),
        );
        Ok(v)
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
            let (policy, revision) = (account.policy.clone(), account.revision);
            // Unadmitted demand follows the reviewed policy; removed records do not.
            for d in proposed.demands.values_mut().filter(|d| {
                d.account_id == id
                    && d.job_id.is_none()
                    && d.admitted_at.is_none()
                    && matches!(d.state.as_str(), "pending" | "conflict")
            }) {
                d.capture = Capture::new(&self.config, &policy, &d.request.kind)?;
                d.revision = revision;
                d.state = "pending".into();
            }
        } else {
            let demand_id = query
                .demand_id
                .as_deref()
                .ok_or("Requester: missing demand ID")?;
            let d = proposed
                .demands
                .get_mut(demand_id)
                .filter(|d| d.account_id == id)
                .ok_or("Requester: demand belongs to another account or does not exist")?;
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
                        .get(d.job_id.as_deref().ok_or("Requester: missing job")?)
                        .ok_or("Requester: acquisition is missing")?;
                    if !matches!(job.state.as_str(), "failed" | "cancelled") {
                        return Err("Requester: acquisition is not retryable".into());
                    }
                    d.state = "reserved".into();
                }
                _ => return Err("Requester: invalid action".into()),
            }
            let outcome = proposed.demands[demand_id].state.clone();
            proposed.record_outcome(demand_id, &outcome)?;
        }
        let mut normalized = query.clone();
        normalized.apply = false;
        normalized.plan_id = None;
        let mut guard = Value::object();
        guard.insert("account", record.to_json());
        guard.insert("ledger", ledger.state.to_json());
        guard.insert("query", normalized.to_json());
        guard.insert(
            "jobs",
            Value::Array(jobs.list().iter().map(Job::to_json).collect()),
        );
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
            ledger.save(proposed)?;
            if query.action == "retry" {
                let demand_id = query
                    .demand_id
                    .as_deref()
                    .ok_or("Requester: missing demand")?;
                let job_id = ledger.state.demands[demand_id]
                    .job_id
                    .clone()
                    .ok_or("Requester: missing acquisition")?;
                jobs.retry(&job_id)?;
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
                    .state
                    .demands
                    .get(demand)
                    .map_or(Value::Null, Demand::to_json),
            );
        }
        Ok(result)
    }

    /// Mutex order everywhere is requester ledger, then request journal.
    pub(crate) fn admit_requester_demand(
        &self,
        ledger: &mut super::RequesterStore,
        jobs: &mut Store,
    ) -> Result<()> {
        let now = store::now();
        let mut next = ledger.state.clone();
        let mut current = BTreeMap::<String, Job>::new();
        for job in jobs
            .list()
            .into_iter()
            .filter(|j| j.upgrade_parent.is_none())
        {
            current.entry(job.request.media_key()).or_insert(job);
        }
        for job in jobs.library_jobs() {
            current.insert(job.request.media_key(), job);
        }
        let mut planned: BTreeMap<_, _> = next
            .demands
            .values()
            .filter(|d| d.state == "reserved")
            .map(|d| (d.request.media_key(), d.capture.clone()))
            .collect();
        let mut ids: Vec<_> = next
            .demands
            .values()
            .filter(|d| matches!(d.state.as_str(), "pending" | "conflict" | "reserved"))
            .map(|d| d.id.clone())
            .collect();
        ids.sort_by_key(|id| {
            (
                next.demands[id].state != "reserved",
                next.demands[id].request.season,
                next.demands[id].request.episode,
                id.clone(),
            )
        });
        let mut admitted = Vec::new();
        for id in ids {
            if admitted.len() >= 64 {
                break;
            }
            let d = next.demands[&id].clone();
            let record = &next.accounts[&d.account_id];
            if !record.policy.enabled
                || !self
                    .config
                    .requesters
                    .accounts
                    .iter()
                    .any(|a| a.id == d.account_id)
            {
                continue;
            }
            if d.revision != record.revision && d.admitted_at.is_none() {
                // Unadmitted demand waits for capture under the current policy.
                next.demands
                    .get_mut(&id)
                    .ok_or("Requester: missing demand")?
                    .state = "pending".into();
                continue;
            }
            let existing = current.get(&d.request.media_key()).cloned();
            if planned
                .get(&d.request.media_key())
                .is_some_and(|p| p != &d.capture)
                || existing
                    .as_ref()
                    .is_some_and(|j| !compatible(&self.config, &d, j).unwrap_or(false))
            {
                next.demands
                    .get_mut(&id)
                    .ok_or("Requester: missing demand")?
                    .state = "conflict".into();
                next.record_outcome(&id, "conflict")?;
                continue;
            }
            let demand = next
                .demands
                .get_mut(&id)
                .ok_or("Requester: missing demand")?;
            demand.admitted_at.get_or_insert(now);
            demand.state = "reserved".into();
            planned.insert(d.request.media_key(), d.capture.clone());
            next.record_outcome(&id, "reserved")?;
            admitted.push((id, existing));
        }
        if next != ledger.state {
            ledger.save(next)?;
        }
        // All provenance and reservations are durable before any new job can run.
        let mut next = ledger.state.clone();
        for (id, current) in admitted {
            let demand = next.demands[&id].clone();
            let job = if let Some(job) = current.or_else(|| current_job(jobs, &demand.request)) {
                if !compatible(&self.config, &demand, &job)? {
                    return Err("Requester: reservation conflicts with queued acquisition".into());
                }
                job
            } else {
                jobs.submit_requester(
                    demand.request.clone(),
                    Provenance {
                        account_id: demand.account_id.clone(),
                        demand_id: id.clone(),
                        policy_revision: demand.revision,
                        capture: demand.capture.clone(),
                    },
                )?
            };
            let d = next
                .demands
                .get_mut(&id)
                .ok_or("Requester: missing demand")?;
            d.job_id = Some(job.id.clone());
            d.state = if job.state == "ready" {
                "ready"
            } else {
                "active"
            }
            .into();
            next.record_outcome(
                &id,
                if matches!(job.state.as_str(), "ready" | "failed" | "cancelled") {
                    &job.state
                } else {
                    "active"
                },
            )?;
        }
        for d in next.demands.values_mut() {
            if let Some(id) = &d.job_id
                && let Some(job) = jobs.get(id)
                && d.state != "removed"
                && job.state == "ready"
            {
                d.state = "ready".into();
            }
        }
        let outcomes: Vec<_> = next
            .demands
            .values()
            .filter(|d| d.state != "removed")
            .filter_map(|d| {
                d.job_id
                    .as_ref()
                    .and_then(|id| jobs.get(id))
                    .map(|j| (d.id.clone(), j.state))
            })
            .collect();
        for (id, outcome) in outcomes {
            if matches!(outcome.as_str(), "ready" | "failed" | "cancelled") {
                next.record_outcome(&id, &outcome)?;
            }
        }
        if next != ledger.state {
            ledger.save(next)?;
        }
        cancel_unwanted(&ledger.state, jobs, &self.config)?;
        self.pause_unwanted_transfers(jobs)?;
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
            interest(&ledger.state, job, &self.config)
                && !crate::irc::routing::waits_for_candidate(&self.config, job, &ledger.state)
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
            ledger.save(next)?;
        }
        Ok(())
    }
    pub(crate) fn requester_retry_allowed(&self, id: &str) -> Result<()> {
        let ledger = lock(&self.requester_store)?;
        let jobs = lock(&self.store)?;
        let job = jobs.get(id).ok_or("Unknown job")?;
        if job.requester.is_none() || ledger.state.operator_jobs.contains(id) {
            return Ok(());
        }
        if !interest(&ledger.state, &job, &self.config) {
            return Err("Requester: acquisition has no admitted demand".into());
        }
        Ok(())
    }
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
            let previous = lock(&self.requester_store)?
                .state
                .accounts
                .get(&account.id)
                .ok_or("Requester: missing account")?
                .clone();
            let account_deadline = deadline.min(Instant::now() + Duration::from_secs(10));
            let result = if self.stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
                Err("Plex account poll deadline reached".into())
            } else {
                integrations::requester_watchlist(&self.config, account, account_deadline)
                    .and_then(|items| self.expand_requester_items(&items, account_deadline))
                    .and_then(|items| {
                        previous.policy.validate_config(&self.config)?;
                        Ok(items)
                    })
            };
            let mut report = Value::object();
            report.insert("account_id", account.id.clone());
            let mut ledger = lock(&self.requester_store)?;
            let mut next = ledger.state.clone();
            let current = next
                .accounts
                .get(&account.id)
                .ok_or("Requester: missing account")?;
            let result =
                if current.revision != previous.revision || current.binding != previous.binding {
                    Err("Plex account policy changed during poll".into())
                } else {
                    result
                };
            match result {
                Ok(Watchlist { origins, requests }) => {
                    let policy = &previous.policy;
                    for (origin, request) in requests {
                        let id = Demand::identity(&account.id, &request);
                        if !next.demands.contains_key(&id) {
                            if next.demands.len() >= MAX_DEMANDS {
                                return Err("Requester: demand capacity reached".into());
                            }
                            next.demands.insert(
                                id.clone(),
                                Demand {
                                    id: id.clone(),
                                    account_id: account.id.clone(),
                                    origins: BTreeSet::from([origin.clone()]),
                                    request: request.clone(),
                                    revision: previous.revision,
                                    capture: Capture::new(&self.config, policy, &request.kind)?,
                                    state: "pending".into(),
                                    job_id: None,
                                    admitted_at: None,
                                    outcome: String::new(),
                                },
                            );
                            next.record_outcome(&id, "pending")?;
                        } else if let Some(d) = next.demands.get_mut(&id) {
                            d.origins.insert(origin);
                            if d.job_id.is_none()
                                && d.admitted_at.is_none()
                                && d.revision != previous.revision
                                && d.state != "removed"
                            {
                                d.capture = Capture::new(&self.config, policy, &d.request.kind)?;
                                d.revision = previous.revision;
                                d.state = "pending".into();
                            }
                        }
                    }
                    for d in next
                        .demands
                        .values_mut()
                        .filter(|d| d.account_id == account.id)
                    {
                        d.origins.retain(|origin| {
                            origin.starts_with("irc:") || origins.contains(origin)
                        });
                    }
                    let removed: Vec<_> = next
                        .demands
                        .values()
                        .filter(|d| {
                            d.account_id == account.id
                                && d.origins.is_empty()
                                && d.state != "removed"
                        })
                        .map(|d| d.id.clone())
                        .collect();
                    for id in removed {
                        let d = next
                            .demands
                            .get_mut(&id)
                            .ok_or("Requester: missing demand")?;
                        d.state = "removed".into();
                        next.record_outcome(&id, "removed")?;
                    }
                    let current = next
                        .accounts
                        .get_mut(&account.id)
                        .ok_or("Requester: missing account")?;
                    current.cursor = current
                        .cursor
                        .checked_add(1)
                        .ok_or("Requester: cursor overflow")?;
                    current.polled_at = store::now();
                    current.last_error = None;
                    current.snapshot = Some(digest(
                        json::stringify(&Value::Array(
                            origins.iter().cloned().map(Value::String).collect(),
                        ))
                        .as_bytes(),
                    ));
                    report.insert("cursor", current.cursor.to_string());
                    report.insert("success", true);
                }
                Err(error) => {
                    let error = if [
                        "Plex account identity changed",
                        "Plex account policy changed during poll",
                        "Plex account poll deadline reached",
                    ]
                    .contains(&error.as_str())
                    {
                        error
                    } else {
                        "Plex account poll failed".into()
                    };
                    let current = next
                        .accounts
                        .get_mut(&account.id)
                        .ok_or("Requester: missing account")?;
                    if error != "Plex account poll deadline reached" {
                        current.polled_at = store::now();
                    }
                    current.last_error = Some(error.clone());
                    report.insert("error", error);
                    report.insert("success", false);
                }
            }
            ledger.save(next)?;
            let mut jobs = lock(&self.store)?;
            self.admit_requester_demand(&mut ledger, &mut jobs)?;
            reports.push(report);
        }
        let mut v = Value::object();
        v.insert("accounts", Value::Array(reports));
        Ok(v)
    }
    fn expand_requester_items(&self, items: &[Request], deadline: Instant) -> Result<Watchlist> {
        let mut origins = BTreeSet::new();
        let mut requests = Vec::new();
        for item in items {
            if Instant::now() >= deadline {
                return Err("Plex account poll deadline reached".into());
            }
            let origin = item.media_key();
            origins.insert(origin.clone());
            if item.kind == "movie" {
                requests.push((
                    origin,
                    integrations::requester_movie_before(&self.config, item, deadline)?,
                ));
            } else {
                let mut identity = item.clone();
                identity.kind = "series".into();
                identity.validate()?;
                let records = lock(&self.series_store)?.list();
                let matches: Vec<_> = records
                    .into_iter()
                    .filter(|r| {
                        let known = &r.plan.request;
                        known.season == identity.season
                            && known.episode == identity.episode
                            && match identity.tmdb_id {
                                Some(id) => known.tmdb_id == Some(id),
                                None => {
                                    crate::selection::tokens(&known.title)
                                        == crate::selection::tokens(&identity.title)
                                        && (identity.year == 0 || known.year == identity.year)
                                }
                            }
                    })
                    .collect();
                if matches.len() > 1 {
                    return Err("Requester: ambiguous retained catalog identity".into());
                }
                let previous = matches.into_iter().next();
                let fetched_identity = previous.as_ref().map_or(&identity, |r| &r.plan.request);
                let plan = integrations::series_plan_before(
                    &self.config,
                    fetched_identity,
                    false,
                    deadline,
                )?;
                let record = if let Some(previous) = previous {
                    let normalized = previous.normalize_catalog(plan)?;
                    let mut checked = previous.clone();
                    checked.accept_catalog(normalized)?;
                    let mut series = lock(&self.series_store)?;
                    if series.get(&previous.id).as_ref() != Some(&previous) {
                        return Err("Requester: captured series scope changed during poll".into());
                    }
                    series.save(checked.clone())?;
                    checked
                } else {
                    lock(&self.series_store)?.subscribe(plan, false, false, false, store::now())?
                };
                let today = date::today();
                for e in &record.plan.episodes {
                    if e.catalog_id.is_some()
                        && e.season != 0
                        && e.air_date
                            .as_deref()
                            .is_some_and(|date| date <= today.as_str())
                    {
                        requests.push((origin.clone(), record.episode_request(e)));
                    }
                }
            }
            if requests.len() > MAX_POLL_ITEMS {
                return Err("Requester: expanded account scope exceeds 512 requests".into());
            }
        }
        Ok(Watchlist { origins, requests })
    }
}

pub(crate) fn validate_storage(state: &State, jobs: &Store) -> Result<()> {
    for d in state.demands.values() {
        if let Some(id) = &d.job_id {
            let job = jobs
                .get(id)
                .ok_or("Requester: recorded acquisition is missing")?;
            if job.request.media_key() != d.request.media_key()
                || job
                    .requester
                    .as_ref()
                    .is_some_and(|p| p.capture != d.capture)
            {
                return Err("Requester: acquisition differs from retained demand".into());
            }
        }
    }
    for job in jobs.list() {
        if let Some(p) = &job.requester {
            let d = state
                .demands
                .get(&p.demand_id)
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
    }
    Ok(())
}
pub(crate) fn cancel_unwanted(
    state: &State,
    jobs: &mut Store,
    config: &crate::config::Config,
) -> Result<()> {
    for job in jobs.list() {
        if job.requester.is_some()
            && job.state != "ready"
            && job.state != "cancelled"
            && !interest(state, &job, config)
        {
            if job.shared_file.is_some() {
                let owners = jobs.shared_group(&job.id)?;
                if owners.iter().any(|j| interest(state, j, config)) {
                    continue;
                }
            }
            jobs.cancel(&job.id)?;
        }
    }
    Ok(())
}
