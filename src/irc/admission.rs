//! Reviewed catalog demand uses checked intent, requester origin and ordinary admission.
use super::{ControlRequest, Record, Rule, State, hash, integer, only, text, valid_id};
use crate::{
    Result,
    config::Config,
    engine::{Engine, lock},
    integrations,
    json::{self, Value},
    requesters::{self, Capture, Demand},
    selection::tokens,
    store::{self, Request},
};
use std::{
    collections::BTreeSet,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

pub(crate) fn split_origin(value: &str) -> Option<(&str, &str)> {
    let (source, id) = value.strip_prefix("irc:")?.split_once(':')?;
    (valid_id(source) && hash(id)).then_some((source, id))
}
pub(crate) fn valid_origin(value: &str) -> bool {
    split_origin(value).is_some()
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Admission {
    pub phase: String,
    pub rule_id: String,
    pub account_id: String,
    pub account_binding: String,
    pub demand_id: String,
    pub policy_revision: u64,
    pub request: Request,
    pub prepared_at: u64,
    pub committed_at: Option<u64>,
}
impl Admission {
    pub(crate) fn to_json(&self) -> Value {
        let mut v = self.public_json();
        v.insert("account_binding", self.account_binding.clone());
        v.insert("request", self.request.to_json());
        v.insert("policy_revision", self.policy_revision.to_string());
        v.insert("prepared_at", self.prepared_at.to_string());
        v.insert(
            "committed_at",
            self.committed_at
                .map_or(Value::Null, |n| n.to_string().into()),
        );
        v
    }
    pub(crate) fn public_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("phase", self.phase.clone());
        v.insert("rule_id", self.rule_id.clone());
        v.insert("account_id", self.account_id.clone());
        v.insert("demand_id", self.demand_id.clone());
        v
    }
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "phase",
                "rule_id",
                "account_id",
                "account_binding",
                "demand_id",
                "policy_revision",
                "request",
                "prepared_at",
                "committed_at",
            ],
        )?;
        let a = Self {
            phase: text(v, "phase")?,
            rule_id: text(v, "rule_id")?,
            account_id: text(v, "account_id")?,
            account_binding: text(v, "account_binding")?,
            demand_id: text(v, "demand_id")?,
            policy_revision: integer(v, "policy_revision")?,
            request: Request::from_json(v.get("request").ok_or("IRC: missing canonical demand")?)?,
            prepared_at: integer(v, "prepared_at")?,
            committed_at: match v.get("committed_at") {
                Some(Value::Null) => None,
                Some(_) => Some(integer(v, "committed_at")?),
                None => return Err("IRC: missing admission timestamp".into()),
            },
        };
        if !matches!(a.phase.as_str(), "prepared" | "committed" | "aborted")
            || !valid_id(&a.rule_id)
            || !valid_id(&a.account_id)
            || !hash(&a.account_binding)
            || a.demand_id != Demand::identity(&a.account_id, &a.request)
            || a.policy_revision == 0
            || !matches!(a.request.kind.as_str(), "movie" | "episode")
            || a.request.tmdb_id.is_none_or(|id| id == 0)
            || a.request.year == 0
            || a.request.source_path.is_some()
            || a.request.source_url.is_some()
            || (a.phase == "committed") != a.committed_at.is_some()
            || a.committed_at.is_some_and(|at| at < a.prepared_at)
        {
            return Err("IRC: invalid canonical admission".into());
        }
        Ok(a)
    }
    pub(crate) fn validate_record(&self, r: &Record) -> Result<()> {
        let expected = match self.phase.as_str() {
            "prepared" => "pending",
            "committed" => "requested",
            "aborted" => "dismissed",
            _ => return Err("IRC: invalid admission phase".into()),
        };
        if r.decision != expected
            || r.revision < 2
            || self.prepared_at < r.first_seen
            || self.request.media_key() != r.announcement.request.media_key()
            || self.request.year != r.announcement.request.year
            || tokens(&self.request.title) != tokens(&r.announcement.request.title)
            || !r
                .evaluations
                .iter()
                .any(|e| e.rule_id == self.rule_id && e.outcome == "matched")
            || (self.phase != "committed" && r.route.is_some())
            || (self.phase == "committed" && r.decided_at != self.committed_at)
        {
            return Err("IRC: admission differs from its original announcement".into());
        }
        Ok(())
    }
}

fn origin(r: &Record) -> String {
    format!("irc:{}:{}", r.source_id, r.id)
}
fn demand_matches(a: &Admission, d: &Demand, state: &requesters::State) -> bool {
    a.demand_id == d.id
        && a.account_id == d.account_id
        && a.request == d.request
        && state
            .accounts
            .get(&a.account_id)
            .is_some_and(|r| r.binding == a.account_binding && r.revision >= a.policy_revision)
}

/// Only a durable requester origin commits an intent. Absence aborts without replay.
pub(crate) fn recovered_state(state: &State, requesters: &requesters::State) -> Result<State> {
    let mut next = state.clone();
    for r in next.records.values_mut() {
        let o = origin(r);
        let Some(a) = &mut r.admission else { continue };
        let retained = requesters
            .demands
            .get(&a.demand_id)
            .filter(|d| d.origins.contains(&o));
        if retained.is_some_and(|d| !demand_matches(a, d, requesters))
            || (a.phase == "committed" && retained.is_none())
            || (a.phase == "aborted" && retained.is_some())
        {
            return Err("IRC: durable demand differs from its admission intent".into());
        }
        if a.phase == "prepared" {
            let at = store::now().max(a.prepared_at);
            a.phase = if retained.is_some() {
                "committed"
            } else {
                "aborted"
            }
            .into();
            a.committed_at = retained.map(|_| at);
            r.decision = if retained.is_some() {
                "requested"
            } else {
                "dismissed"
            }
            .into();
            r.decided_at = Some(at);
            r.revision = r
                .revision
                .checked_add(1)
                .ok_or("IRC: admission revision overflow")?;
        }
    }
    for d in requesters.demands.values() {
        for o in &d.origins {
            if let Some((source, id)) = split_origin(o) {
                let r = next
                    .records
                    .get(id)
                    .filter(|r| r.source_id == source)
                    .ok_or("IRC: explicit demand has no durable announcement")?;
                let a = r
                    .admission
                    .as_ref()
                    .filter(|a| a.phase == "committed")
                    .ok_or("IRC: explicit demand has no committed admission")?;
                if !demand_matches(a, d, requesters) {
                    return Err("IRC: explicit demand differs from committed admission".into());
                }
            }
        }
    }
    State::from_json(&next.to_json())?;
    Ok(next)
}

fn request_rule<'a>(config: &'a Config, r: &Record) -> Result<&'a Rule> {
    if r.decision != "pending"
        || r.route.is_some()
        || r.admission.is_some()
        || r.fingerprint != config.irc.fingerprint(&config.selection)
        || super::outcome(&r.evaluations) != "matched"
    {
        return Err("IRC: new demand requires an unreviewed current matched claim".into());
    }
    let source = config.irc.source(&r.source_id)?;
    if !source.enabled || source.binding() != r.binding || source.magnet_template.is_none() {
        return Err("IRC: request source is disabled or changed".into());
    }
    config
        .irc
        .rules
        .iter()
        .find(|rule| {
            rule.enabled
                && rule.source == r.source_id
                && rule.action == "request"
                && rule.requester.is_some()
                && r.evaluations
                    .iter()
                    .any(|e| e.rule_id == rule.id && e.outcome == "matched")
        })
        .ok_or("IRC: no matched rule authorizes requester demand".into())
}

fn public_demand(d: &Demand) -> Value {
    let mut v = Value::object();
    v.insert("id", d.id.clone());
    v.insert("account_id", d.account_id.clone());
    v.insert("state", d.state.clone());
    v.insert("job_id", d.job_id.clone().map_or(Value::Null, Value::from));
    v
}

fn public_request(request: &Request) -> Value {
    let mut v = request.to_json();
    if let Value::Object(fields) = &mut v {
        fields.remove("source_url");
        fields.remove("source_path");
    }
    v
}

impl Engine {
    pub(crate) fn irc_request_control(&self, id: &str, query: &ControlRequest) -> Result<Value> {
        if self.stopped.load(Ordering::Acquire) || (query.apply && self.read_only) {
            return Err("IRC: request application requires a running writable service".into());
        }
        let record = lock(&self.irc_store)?
            .state
            .records
            .get(id)
            .ok_or("IRC: unknown announcement")?
            .clone();
        let rule = request_rule(&self.config, &record)?.clone();
        let episode = record.announcement.request.kind == "episode";
        let series_snapshot = if episode {
            lock(&self.series_store)?.list()
        } else {
            Vec::new()
        };
        let retained: Vec<_> = series_snapshot
            .iter()
            .filter(|r| r.plan.request.tmdb_id == record.announcement.request.tmdb_id)
            .collect();
        if retained.len() > 1 {
            return Err("IRC: retained series identity is ambiguous".into());
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let configured_account = self
            .config
            .requesters
            .accounts
            .iter()
            .find(|a| rule.requester.as_deref() == Some(a.id.as_str()))
            .ok_or("IRC: requester account is unavailable")?;
        integrations::requester_identity_before(&self.config, configured_account, deadline)
            .map_err(|_| "IRC: requester identity could not be verified".to_owned())?;
        let canonical = integrations::irc_catalog_request_before(
            &self.config,
            &record.announcement.request,
            retained.first().copied(),
            deadline,
        )
        .map_err(|_| "IRC: catalog identity could not be verified".to_owned())?;

        // Series precedes IRC, then requester ledger and jobs. No network I/O is held.
        let series = if episode {
            Some(lock(&self.series_store)?)
        } else {
            None
        };
        if series.as_ref().is_some_and(|s| s.list() != series_snapshot) {
            return Err("IRC: source numbering changed during catalog verification".into());
        }
        let mut irc = lock(&self.irc_store)?;
        if irc.state.records.get(id) != Some(&record)
            || Instant::now() >= deadline
            || self.stopped.load(Ordering::Acquire)
        {
            return Err("IRC: request review changed during catalog verification".into());
        }
        let mut ledger = lock(&self.requester_store)?;
        let mut jobs = lock(&self.store)?;
        let account_id = rule
            .requester
            .as_deref()
            .ok_or("IRC: missing requester selector")?;
        let account = ledger
            .state
            .accounts
            .get(account_id)
            .ok_or("IRC: requester account is unavailable")?
            .clone();
        if !self
            .config
            .requesters
            .accounts
            .iter()
            .any(|a| a.id == account_id && a.binding() == account.binding)
        {
            return Err("IRC: requester binding changed".into());
        }
        let demand_id = Demand::identity(account_id, &canonical);
        let existing = ledger.state.demands.get(&demand_id);
        if existing.is_some_and(|d| d.request != canonical || d.state == "removed") {
            return Err(
                "IRC: retained demand differs from the claim or is a removal tombstone".into(),
            );
        }
        if existing.is_none() && ledger.state.demands.len() >= requesters::MAX_DEMANDS {
            return Err("IRC: requester demand capacity reached".into());
        }
        if existing.is_some_and(|d| d.origins.len() >= requesters::MAX_POLL_ITEMS) {
            return Err("IRC: requester origin capacity reached".into());
        }
        let capture = match existing {
            Some(d) => d.capture.clone(),
            None => Capture::new(&self.config, &account.policy, &canonical.kind)?,
        };
        if capture.profile_name != rule.profile
            || self.config.selection.profiles.get(&rule.profile) != Some(&capture.profile)
            || !capture
                .profile
                .assess(&record.announcement.title, &canonical.title)
                .accepted
            || !super::routing::title_matches(&canonical, &record.announcement.title)
        {
            return Err("IRC: request differs from the captured profile or source labels".into());
        }
        let now = store::now();
        let mut guard = Value::object();
        guard.insert("announcement", record.to_json());
        guard.insert(
            "configuration",
            self.config.irc.fingerprint(&self.config.selection),
        );
        guard.insert("canonical_request", canonical.to_json());
        guard.insert("account", account.to_json());
        guard.insert("capture", capture.to_json());
        guard.insert("ledger", ledger.state.to_json());
        guard.insert(
            "jobs",
            Value::Array(jobs.list().iter().map(crate::store::Job::to_json).collect()),
        );
        guard.insert(
            "series",
            Value::Array(
                series_snapshot
                    .iter()
                    .map(crate::series::Record::to_json)
                    .collect(),
            ),
        );
        let plan_id = super::digest(json::stringify(&guard).as_bytes());
        let mut report = Value::object();
        report.insert("action", "request");
        report.insert("apply", query.apply);
        report.insert("applied", false);
        report.insert("plan_id", plan_id.clone());
        report.insert("announcement", record.public_json());
        report.insert("canonical_request", public_request(&canonical));
        report.insert("account_id", account_id);
        report.insert("demand_id", demand_id.clone());
        report.insert("profile", capture.profile_name.clone());
        report.insert("destination", capture.destination.clone());
        report.insert("new_demand", existing.is_none());
        report.insert("acquisition_started", false);
        if Instant::now() >= deadline || self.stopped.load(Ordering::Acquire) {
            return Err("IRC: request review expired or stopped".into());
        }
        if !query.apply {
            return Ok(report);
        }
        if query.plan_id.as_deref() != Some(&plan_id) {
            return Err("IRC: request review is stale; preview again".into());
        }

        let at = now.max(record.first_seen);
        let admission = Admission {
            phase: "prepared".into(),
            rule_id: rule.id,
            account_id: account_id.into(),
            account_binding: account.binding,
            demand_id: demand_id.clone(),
            policy_revision: account.revision,
            request: canonical.clone(),
            prepared_at: at,
            committed_at: None,
        };
        let mut next_irc = irc.state.clone();
        let row = next_irc
            .records
            .get_mut(id)
            .ok_or("IRC: missing announcement")?;
        row.admission = Some(admission);
        row.revision = row
            .revision
            .checked_add(1)
            .ok_or("IRC: admission revision overflow")?;
        irc.save(next_irc)?;

        let mut next = ledger.state.clone();
        let o = origin(&record);
        if let Some(d) = next.demands.get_mut(&demand_id) {
            d.origins.insert(o);
        } else {
            next.demands.insert(
                demand_id.clone(),
                Demand {
                    id: demand_id.clone(),
                    account_id: account_id.into(),
                    origins: BTreeSet::from([o]),
                    request: canonical,
                    revision: account.revision,
                    capture,
                    state: "pending".into(),
                    job_id: None,
                    admitted_at: None,
                    outcome: String::new(),
                },
            );
            next.record_outcome(&demand_id, "pending")?;
        }
        // The canonical origin is durable before ordinary admission can reserve a job.
        ledger.save(next)?;
        let mut committed = irc.state.clone();
        let row = committed
            .records
            .get_mut(id)
            .ok_or("IRC: missing announcement")?;
        let a = row
            .admission
            .as_mut()
            .ok_or("IRC: missing admission intent")?;
        let at = store::now().max(at);
        a.phase = "committed".into();
        a.committed_at = Some(at);
        row.decision = "requested".into();
        row.decided_at = Some(at);
        row.revision = row
            .revision
            .checked_add(1)
            .ok_or("IRC: admission revision overflow")?;
        irc.save(committed)?;
        if !self.stopped.load(Ordering::Acquire) {
            self.admit_requester_demand(&mut ledger, &mut jobs)?;
        }
        report.insert("applied", true);
        report.insert("announcement", irc.state.records[id].public_json());
        report.insert("demand", public_demand(&ledger.state.demands[&demand_id]));
        report.insert(
            "acquisition_started",
            ledger.state.demands[&demand_id]
                .job_id
                .as_deref()
                .and_then(|id| jobs.get(id))
                .is_some_and(|j| j.download_id.is_some()),
        );
        Ok(report)
    }
}
