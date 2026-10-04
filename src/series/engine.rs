//! Catalog I/O stays outside storage locks; policy revisions fence stale plans.
use super::{Episode, Record};
use crate::{
    Result, date,
    engine::{Engine, lock},
    integrations,
    json::Value,
    store::{self, Job, Request},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{TryLockError, atomic::Ordering},
    time::{Duration, Instant},
};

const INTERVAL: u64 = 3600;
const RETRY: u64 = 300;
const MAX_CHECKS: usize = 4;
const MAX_SUBMISSIONS: usize = 64;

impl Engine {
    pub fn series(&self) -> Result<Value> {
        Ok(lock(&self.series_store)?.summaries())
    }
    pub fn series_record(&self, id: &str) -> Result<Value> {
        Ok(lock(&self.series_store)?
            .get(id)
            .ok_or("Unknown monitored series")?
            .public_json())
    }

    pub fn track_series(
        &self,
        request: &Request,
        include_specials: bool,
        future_only: bool,
    ) -> Result<Value> {
        if self.read_only {
            return Err("Series storage is read-only".into());
        }
        request.validate()?;
        if request.kind != "series" || request.source_path.is_some() || request.source_url.is_some()
        {
            return Err(
                "Series monitoring requires a series identity without an explicit source".into(),
            );
        }
        let records = lock(&self.series_store)?.list();
        let matches: Vec<_> = records
            .iter()
            .filter(|record| {
                let known = &record.plan.request;
                known.season == request.season
                    && known.episode == request.episode
                    && match request.tmdb_id {
                        Some(id) => known.tmdb_id == Some(id),
                        None => {
                            crate::selection::tokens(&known.title)
                                == crate::selection::tokens(&request.title)
                                && (request.year == 0 || request.year == known.year)
                        }
                    }
            })
            .collect();
        if matches.len() > 1 {
            return Err("Series identity is ambiguous; specify a TMDB ID".into());
        }
        let record = if let Some(record) = matches.first() {
            (*record).clone()
        } else {
            let plan = integrations::series_plan(&self.config, request, include_specials)?;
            lock(&self.series_store)?.subscribe(
                plan,
                include_specials,
                future_only,
                store::now(),
            )?
        };
        let (record, submitted) = self.queue_series(&record.id, record.revision)?;
        let mut value = record.public_json();
        value.insert("submitted", submitted.len() as u32);
        Ok(value)
    }

    pub fn configure_series(
        &self,
        id: &str,
        monitored: Option<bool>,
        include_specials: Option<bool>,
        start_date: Option<Option<String>>,
    ) -> Result<Value> {
        Ok(lock(&self.series_store)?
            .configure(id, monitored, include_specials, start_date, store::now())?
            .public_json())
    }
    pub fn monitor_series_episode(
        &self,
        id: &str,
        season: u32,
        episode: u32,
        enabled: bool,
    ) -> Result<Value> {
        Ok(lock(&self.series_store)?
            .monitor_episode(id, season, episode, enabled, store::now())?
            .public_json())
    }
    pub fn refresh_series(&self, id: &str) -> Result<Value> {
        let _refresh = self
            .series_refresh_lock
            .try_lock()
            .map_err(|_| "A series refresh is already running")?;
        self.refresh_series_before(id, Instant::now() + Duration::from_secs(90))
    }
    pub(crate) fn refresh_series_due(&self) -> Result<Value> {
        let _refresh = match self.series_refresh_lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => {
                let mut value = Value::object();
                value.insert("busy", true);
                return Ok(value);
            }
            Err(TryLockError::Poisoned(_)) => {
                return Err("Series refresh coordination unavailable".into());
            }
        };
        let now = store::now();
        let mut records = lock(&self.series_store)?.list();
        records.retain(|record| record.monitored && record.next_check_at <= now);
        records.sort_by_key(|record| (record.next_check_at, record.id.clone()));
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut reports = Vec::new();
        for record in records.into_iter().take(MAX_CHECKS) {
            if self.stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
                break;
            }
            let mut report = Value::object();
            report.insert("id", record.id.clone());
            match self.refresh_series_before(&record.id, deadline) {
                Ok(value) => report.insert("result", value),
                Err(error) => report.insert("error", integrations::report_text(&error, 2048)),
            }
            reports.push(report);
        }
        let mut value = Value::object();
        value.insert("reports", Value::Array(reports));
        Ok(value)
    }

    fn refresh_series_before(&self, id: &str, deadline: Instant) -> Result<Value> {
        if self.read_only {
            return Err("Series storage is read-only".into());
        }
        let previous = lock(&self.series_store)?
            .get(id)
            .ok_or("Unknown monitored series")?;
        let fetched = integrations::series_plan_before(
            &self.config,
            &previous.plan.request,
            previous.include_specials,
            deadline,
        )
        .and_then(|plan| {
            validate_identity_changes(&previous.plan.episodes, &plan.episodes)?;
            Ok(plan)
        });
        {
            let mut series = lock(&self.series_store)?;
            let mut current = series.get(id).ok_or("Unknown monitored series")?;
            if current.revision != previous.revision || self.stopped.load(Ordering::Acquire) {
                return Err("Series settings changed during refresh; result discarded".into());
            }
            match fetched {
                Ok(plan) => {
                    current.plan = plan;
                    current.last_error = None;
                    current.checked_at = store::now();
                    current.next_check_at = current.checked_at;
                    series.save(current)?;
                }
                Err(error) => {
                    current.last_error = Some(integrations::report_text(&error, 2048));
                    current.checked_at = store::now();
                    current.next_check_at = current.checked_at.saturating_add(RETRY);
                    series.save(current)?;
                    return Err(error);
                }
            }
        }
        let (record, submitted) = self.queue_series(id, previous.revision)?;
        let mut value = record.public_json();
        value.insert("submitted", submitted.len() as u32);
        Ok(value)
    }

    fn queue_series(&self, id: &str, revision: u64) -> Result<(Record, Vec<Job>)> {
        // Lock order is series then requests. This bounded commit batch contains no network I/O.
        let mut series = lock(&self.series_store)?;
        let mut record = series.get(id).ok_or("Unknown monitored series")?;
        if record.revision != revision {
            return Err("Series settings changed; submission discarded".into());
        }
        let today = date::today();
        let mut jobs = lock(&self.store)?;
        let existing: BTreeSet<_> = jobs
            .list()
            .iter()
            .map(|job| job.request.media_key())
            .collect();
        let mut candidates: Vec<_> = record
            .plan
            .episodes
            .iter()
            .filter(|episode| {
                record.episode_monitored(episode)
                    && episode
                        .air_date
                        .as_deref()
                        .is_some_and(|date| date <= today.as_str())
            })
            .map(|episode| (episode.air_date.clone(), record.episode_request(episode)))
            .filter(|(_, request)| !existing.contains(&request.media_key()))
            .collect();
        candidates
            .sort_by(|a, b| (&a.0, a.1.season, a.1.episode).cmp(&(&b.0, b.1.season, b.1.episode)));
        let limited = candidates.len() > MAX_SUBMISSIONS;
        let mut submitted = Vec::new();
        for (_, request) in candidates.into_iter().take(MAX_SUBMISSIONS) {
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            submitted.push(jobs.submit(request)?);
        }
        drop(jobs);
        record.next_check_at = store::now().saturating_add(if limited { 60 } else { INTERVAL });
        record.updated_at = store::now();
        series.save(record.clone())?;
        Ok((record, submitted))
    }

    pub fn episode_calendar(&self, query: &CalendarQuery) -> Result<Value> {
        query.validate()?;
        let records = lock(&self.series_store)?.list();
        let jobs = lock(&self.store)?.list();
        let mut states = BTreeMap::<String, &Job>::new();
        for job in &jobs {
            let key = job.request.media_key();
            if states
                .get(&key)
                .is_none_or(|current| current.state != "ready")
            {
                states.insert(key, job);
            }
        }
        // Sort borrowed catalog rows, then construct public JSON only for the requested page.
        let mut entries = Vec::new();
        for record in &records {
            if query.series_id.as_ref().is_some_and(|id| id != &record.id) {
                continue;
            }
            for episode in &record.plan.episodes {
                if episode
                    .air_date
                    .as_ref()
                    .is_some_and(|date| date >= &query.from && date <= &query.to)
                {
                    entries.push((record, episode));
                }
            }
        }
        entries.sort_by(|(a, x), (b, y)| {
            (&x.air_date, &a.id, x.season, x.episode).cmp(&(
                &y.air_date,
                &b.id,
                y.season,
                y.episode,
            ))
        });
        let total = entries.len();
        let today = date::today();
        let page = entries
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .map(|(record, episode)| {
                let job = states.get(&record.episode_request(episode).media_key());
                let mut value = Value::object();
                value.insert("series_id", record.id.clone());
                value.insert(
                    "title",
                    integrations::report_text(&record.plan.request.title, 4096),
                );
                value.insert(
                    "episode_title",
                    integrations::report_text(&episode.title, 2048),
                );
                value.insert("season", episode.season);
                value.insert("episode", episode.episode);
                value.insert(
                    "air_date",
                    episode.air_date.clone().map_or(Value::Null, Value::String),
                );
                value.insert("monitored", record.episode_monitored(episode));
                value.insert(
                    "job_id",
                    job.map_or(Value::Null, |job| job.id.clone().into()),
                );
                let state = if !record.episode_monitored(episode) {
                    "unmonitored"
                } else if episode
                    .air_date
                    .as_deref()
                    .is_some_and(|date| date <= today.as_str())
                {
                    "missing"
                } else {
                    "scheduled"
                };
                value.insert("state", job.map_or(state, |job| job.state.as_str()));
                value
            })
            .collect();
        let mut value = Value::object();
        value.insert("from", query.from.clone());
        value.insert("to", query.to.clone());
        value.insert("total", total as u32);
        value.insert("offset", query.offset as u32);
        value.insert("limit", query.limit as u32);
        value.insert("episodes", Value::Array(page));
        Ok(value)
    }
}

fn validate_identity_changes(old: &[Episode], new: &[Episode]) -> Result<()> {
    let numbers: BTreeMap<_, _> = old
        .iter()
        .map(|episode| ((episode.season, episode.episode), episode.catalog_id))
        .collect();
    let identities: BTreeMap<_, _> = old
        .iter()
        .filter_map(|episode| {
            episode
                .catalog_id
                .map(|id| (id, (episode.season, episode.episode)))
        })
        .collect();
    for episode in new {
        if numbers
            .get(&(episode.season, episode.episode))
            .copied()
            .flatten()
            .is_some_and(|id| Some(id) != episode.catalog_id)
            || episode
                .catalog_id
                .and_then(|id| identities.get(&id))
                .is_some_and(|number| *number != (episode.season, episode.episode))
        {
            return Err("Catalog episode identity or numbering changed; an explicit mapping decision is required".into());
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct CalendarQuery {
    pub from: String,
    pub to: String,
    pub series_id: Option<String>,
    pub offset: usize,
    pub limit: usize,
}

impl CalendarQuery {
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() > 1024 {
            return Err("Calendar query is too large".into());
        }
        let mut fields = BTreeMap::new();
        if !text.is_empty() {
            for pair in text.split('&') {
                let (name, value) = pair.split_once('=').ok_or("Invalid calendar query")?;
                if !["from", "to", "series_id", "offset", "limit"].contains(&name)
                    || fields.insert(name, value).is_some()
                {
                    return Err("Unknown or duplicate calendar field".into());
                }
            }
        }
        let mut query = Self::new(fields.get("from").copied(), fields.get("to").copied())?;
        query.series_id = fields
            .get("series_id")
            .filter(|text| !text.is_empty())
            .map(|text| (*text).to_owned());
        for (key, target) in [("offset", &mut query.offset), ("limit", &mut query.limit)] {
            if let Some(text) = fields.get(key) {
                if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err("Invalid calendar pagination".into());
                }
                *target = text.parse().map_err(|_| "Invalid calendar pagination")?;
            }
        }
        query.validate()?;
        Ok(query)
    }
    pub fn new(from: Option<&str>, to: Option<&str>) -> Result<Self> {
        let from = from
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(date::today);
        let to = to
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .map_or_else(|| date::add(&from, 30), Ok)?;
        let query = Self {
            from,
            to,
            series_id: None,
            offset: 0,
            limit: 50,
        };
        query.validate()?;
        Ok(query)
    }
    pub fn validate(&self) -> Result<()> {
        let length = date::day(&self.to)? - date::day(&self.from)?;
        if !(0..=366).contains(&length)
            || self.limit == 0
            || self.limit > 200
            || self.offset > 20_000
            || self.series_id.as_ref().is_some_and(|id| {
                id.len() != 32
                    || !id
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            })
        {
            return Err("Calendar requires an ordered range of at most 367 days, a limit of 1 to 200 and a valid series identifier".into());
        }
        Ok(())
    }
}
