//! Catalog I/O stays outside storage locks; policy revisions fence stale plans.
use super::Record;
use crate::{
    Result, date,
    engine::{Engine, lock},
    integrations,
    json::Value,
    store::{self, Job, Request},
};
use std::{
    collections::BTreeMap,
    sync::{TryLockError, atomic::Ordering},
    time::{Duration, Instant},
};

const INTERVAL: u64 = 3600;
const RETRY: u64 = 300;
const MAX_CHECKS: usize = 4;
const MAX_SUBMISSIONS: usize = 64;
const SCAN_PAGE: usize = 50;
const MAX_SCANNED: usize = 400;
const MAX_UNREADABLE: usize = 1024;
const UNREADABLE_RETRY: Duration = Duration::from_secs(3600);

impl Engine {
    /// One page of summaries ordered by identifier. `next` is the `after`
    /// cursor of the following page and is null after the last.
    pub fn series_page(&self, after: Option<&str>, limit: usize) -> Result<Value> {
        let page = lock(&self.series_store)?.page(after, limit)?;
        let mut value = Value::object();
        value.insert("series", Value::Array(page.series));
        value.insert("next", page.next.map_or(Value::Null, Value::String));
        value.insert("total", Value::Number(page.total as f64));
        Ok(value)
    }

    pub fn series_record(&self, id: &str) -> Result<Value> {
        Ok(lock(&self.series_store)?
            .get(id)?
            .ok_or("Unknown monitored series")?
            .public_json())
    }

    /// The request of one catalog episode of a tracked series, as monitoring
    /// would submit it.
    pub fn series_episode_request(&self, id: &str, season: u32, episode: u32) -> Result<Request> {
        let record = lock(&self.series_store)?
            .get(id)?
            .ok_or("Unknown monitored series")?;
        let known = record
            .plan
            .episodes
            .iter()
            .find(|e| e.season == season && e.episode == episode)
            .ok_or("Episode is absent from the series catalog")?;
        Ok(record.episode_request(known))
    }

    pub fn track_series(
        &self,
        request: &Request,
        include_specials: bool,
        future_only: bool,
    ) -> Result<Value> {
        self.track_series_with_policy(request, include_specials, future_only, true)
    }

    pub fn track_series_with_policy(
        &self,
        request: &Request,
        include_specials: bool,
        future_only: bool,
        monitored: bool,
    ) -> Result<Value> {
        if self.read_only {
            return Err("Series storage is read-only".into());
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        request.validate()?;
        if request.kind != "series" || request.source_path.is_some() || request.source_url.is_some()
        {
            return Err(
                "Series monitoring requires a series identity without an explicit source".into(),
            );
        }
        let matches = lock(&self.series_store)?.matching(request)?;
        if matches.len() > 1 {
            return Err("Series identity is ambiguous; specify a TMDB ID".into());
        }
        let (record, schedule) = if let Some(record) = matches.into_iter().next() {
            (record, false)
        } else {
            let plan = integrations::series_plan_before(
                &self.config,
                request,
                include_specials,
                deadline,
            )?;
            (
                lock(&self.series_store)?.subscribe(
                    plan,
                    include_specials,
                    future_only,
                    monitored,
                    store::now(),
                )?,
                true,
            )
        };
        let (record, submitted) =
            self.queue_series(&record.id, record.revision, schedule, deadline)?;
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
        self.refresh_series_due_before(Instant::now() + Duration::from_secs(90))
    }

    pub(super) fn refresh_series_due_before(&self, deadline: Instant) -> Result<Value> {
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
        // Every visited due row consumes the scan budget, including cached
        // unreadable scopes. The durable cursor continues past a bad prefix
        // on the next pass and after restart, then wraps at the end.
        let now = store::now();
        let mut reports = Vec::new();
        let mut first_error = None;
        let mut attempts = 0;
        let mut scanned = 0;
        let mut pages = 0;
        let mut skipped_unreadable = false;
        let mut cursor = lock(&self.series_store)?.due_cursor(now)?;
        'pass: loop {
            let page = lock(&self.series_store)?.due_after(now, cursor.as_ref(), SCAN_PAGE)?;
            pages += 1;
            for record in &page {
                if self.stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
                    break 'pass;
                }
                if scanned == MAX_SCANNED {
                    break 'pass;
                }
                cursor = Some(record.clone());
                scanned += 1;
                if self.series_is_unreadable(&record.id) {
                    skipped_unreadable = true;
                    continue;
                }
                let probe = lock(&self.series_store)?.get(&record.id);
                if !matches!(probe, Ok(Some(_))) {
                    let error = match probe {
                        Err(error) => error,
                        _ => "the due index names a record that does not exist".to_owned(),
                    };
                    self.series_mark_unreadable(&record.id);
                    let error = integrations::report_text(
                        &format!(
                            "Series {} is unreadable and was skipped: {error}",
                            record.id
                        ),
                        2048,
                    );
                    first_error.get_or_insert_with(|| error.clone());
                    let mut report = Value::object();
                    report.insert("id", record.id.clone());
                    report.insert("error", error);
                    reports.push(report);
                    continue;
                }
                let mut report = Value::object();
                report.insert("id", record.id.clone());
                match self.refresh_series_before(&record.id, deadline) {
                    Ok(value) => report.insert("result", value),
                    Err(error) => {
                        let error = integrations::report_text(&error, 2048);
                        first_error.get_or_insert_with(|| error.clone());
                        report.insert("error", error);
                    }
                }
                reports.push(report);
                attempts += 1;
                if attempts == MAX_CHECKS {
                    break 'pass;
                }
            }
            if page.len() < SCAN_PAGE {
                cursor = None;
                break;
            }
            if scanned >= MAX_SCANNED {
                break;
            }
        }
        {
            let mut series = lock(&self.series_store)?;
            series.due_pass = [pages, scanned, attempts];
            let completed = first_error.is_none()
                && !skipped_unreadable
                && !self.stopped.load(Ordering::Acquire)
                && Instant::now() < deadline;
            series.finish_due_pass(cursor.as_ref(), completed.then(store::now))?;
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        let mut value = Value::object();
        value.insert("reports", Value::Array(reports));
        Ok(value)
    }

    fn series_is_unreadable(&self, id: &str) -> bool {
        self.series_unreadable.lock().is_ok_and(|mut skipped| {
            let now = Instant::now();
            skipped.retain(|_, until| *until > now);
            skipped.contains_key(id)
        })
    }

    fn series_mark_unreadable(&self, id: &str) {
        if let Ok(mut skipped) = self.series_unreadable.lock() {
            let now = Instant::now();
            skipped.retain(|_, until| *until > now);
            if skipped.len() < MAX_UNREADABLE {
                skipped.insert(id.to_owned(), now + UNREADABLE_RETRY);
            }
        }
    }

    pub fn series_unreadable_count(&self) -> usize {
        self.series_unreadable.lock().map_or(0, |mut skipped| {
            let now = Instant::now();
            skipped.retain(|_, until| *until > now);
            skipped.len()
        })
    }

    fn refresh_series_before(&self, id: &str, deadline: Instant) -> Result<Value> {
        if self.read_only {
            return Err("Series storage is read-only".into());
        }
        let previous = lock(&self.series_store)?
            .get(id)?
            .ok_or("Unknown monitored series")?;
        let fetched = previous
            .fetch_catalog(self, false, deadline)
            .and_then(|plan| previous.normalize_catalog(plan))
            .and_then(|plan| {
                let mut checked = previous.clone();
                checked.accept_catalog(plan)?;
                Ok(checked.plan)
            });
        {
            let mut series = lock(&self.series_store)?;
            let mut current = series.get(id)?.ok_or("Unknown monitored series")?;
            if current.revision != previous.revision || self.stopped.load(Ordering::Acquire) {
                return Err("Series settings changed during refresh; result discarded".into());
            }
            match fetched {
                Ok(plan) => {
                    current.accept_catalog(plan)?;
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
        let (record, submitted) = self.queue_series(id, previous.revision, true, deadline)?;
        let mut value = record.public_json();
        value.insert("submitted", submitted.len() as u32);
        Ok(value)
    }

    fn queue_series(
        &self,
        id: &str,
        revision: u64,
        schedule: bool,
        deadline: Instant,
    ) -> Result<(Record, Vec<Job>)> {
        let mut submitted = Vec::new();
        let pack_plan = if self.config.series_packs_enabled {
            let mut series = lock(&self.series_store)?;
            let record = series.get(id)?.ok_or("Unknown monitored series")?;
            if record.revision != revision {
                return Err("Series settings changed; submission discarded".into());
            }
            Some(record)
        } else {
            None
        };
        if let Some(record) = &pack_plan
            && record.monitored
        {
            let today = date::today();
            let mut seasons = BTreeMap::<u32, usize>::new();
            for episode in &record.plan.episodes {
                if record.episode_monitored(episode)
                    && episode.catalog_id.is_some()
                    && episode
                        .air_date
                        .as_deref()
                        .is_some_and(|day| day <= today.as_str())
                    && !lock(&self.store)?
                        .media_exists(&record.episode_request(episode).media_key())?
                {
                    *seasons.entry(episode.season).or_default() += 1;
                }
            }
            for (season, count) in seasons.into_iter().take(4) {
                if self.stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
                    break;
                }
                if count > MAX_SUBMISSIONS.saturating_sub(submitted.len()) {
                    continue;
                }
                let query = crate::pack::AutoPackRequest {
                    season,
                    apply: true,
                    scope_id: None,
                    candidate_id: None,
                };
                if let Ok(report) = self.search_packs_before(id, &query, deadline) {
                    let mut jobs = lock(&self.store)?;
                    if let Some(rows) = report
                        .get("submission")
                        .and_then(|submission| submission.get("jobs"))
                        .and_then(Value::as_array)
                    {
                        for row in rows {
                            if let Some(job) = row
                                .get("id")
                                .and_then(Value::as_str)
                                .map(|id| jobs.get(id))
                                .transpose()?
                                .flatten()
                            {
                                submitted.push(job);
                            }
                        }
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return Err("Series acquisition exceeded its shared deadline".into());
        }
        // Lock order is series then requests. This bounded commit batch contains no network I/O.
        let mut series = lock(&self.series_store)?;
        series.check_writable()?;
        let mut record = series.get(id)?.ok_or("Unknown monitored series")?;
        if record.revision != revision {
            return Err("Series settings changed; submission discarded".into());
        }
        if pack_plan
            .as_ref()
            .is_some_and(|previous| previous.plan != record.plan)
        {
            return Err("Series catalog changed; submission discarded".into());
        }
        let today = date::today();
        let mut jobs = lock(&self.store)?;
        let mut candidates = Vec::new();
        for episode in &record.plan.episodes {
            if record.episode_monitored(episode)
                && episode.catalog_id.is_some()
                && episode
                    .air_date
                    .as_deref()
                    .is_some_and(|date| date <= today.as_str())
            {
                let request = record.episode_request(episode);
                if !jobs.media_exists(&request.media_key())? {
                    candidates.push((episode.air_date.clone(), request));
                }
            }
        }
        candidates
            .sort_by(|a, b| (&a.0, a.1.season, a.1.episode).cmp(&(&b.0, b.1.season, b.1.episode)));
        let remaining = MAX_SUBMISSIONS.saturating_sub(submitted.len());
        let limited = candidates.len() > remaining;
        for (_, request) in candidates.into_iter().take(remaining) {
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            submitted.push(jobs.submit(request)?);
        }
        drop(jobs);
        if submitted.is_empty() && !schedule {
            return Ok((record, submitted));
        }
        let next = store::now().saturating_add(if limited { 60 } else { INTERVAL });
        if schedule {
            record.next_check_at = next;
        } else if limited {
            record.next_check_at = record.next_check_at.min(next);
        }
        record.updated_at = store::now();
        series.save(record.clone())?;
        Ok((record, submitted))
    }

    /// One calendar page from the date index. `offset` pages are limited to
    /// 20,000 rows; `after` (the `next` cursor of a previous page) reaches
    /// every later row of any window.
    pub fn episode_calendar(&self, query: &CalendarQuery) -> Result<Value> {
        query.validate()?;
        let page = lock(&self.series_store)?.calendar(query)?;
        let mut wanted = std::collections::BTreeSet::new();
        for row in &page.rows {
            wanted.insert(row.series.episode_request(&row.episode).media_key());
        }
        let states = lock(&self.store)?.states_for(&wanted)?;
        let today = date::today();
        let rows = page
            .rows
            .iter()
            .map(|row| {
                let (record, episode) = (&row.series, &row.episode);
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
                let state = if episode.catalog_id.is_none() {
                    "mapping_required"
                } else if !record.episode_monitored(episode) {
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
        value.insert(
            "total",
            page.total
                .map_or(Value::Null, |total| Value::Number(total as f64)),
        );
        value.insert("offset", query.offset as u32);
        value.insert("limit", query.limit as u32);
        value.insert("next", page.next.map_or(Value::Null, Value::String));
        value.insert("episodes", Value::Array(rows));
        Ok(value)
    }
}

#[derive(Clone, Debug)]
pub struct CalendarQuery {
    pub from: String,
    pub to: String,
    pub series_id: Option<String>,
    pub offset: usize,
    pub limit: usize,
    /// Exclusive cursor from a previous page's `next`; excludes `offset`.
    pub after: Option<String>,
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
                if !["from", "to", "series_id", "offset", "limit", "after"].contains(&name)
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
        query.after = fields
            .get("after")
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
            after: None,
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
            || (self.after.is_some() && self.offset != 0)
            || self.after.as_ref().is_some_and(|after| {
                after.len() > 128
                    || !after.bytes().all(|b| {
                        b.is_ascii_digit() || b.is_ascii_lowercase() || matches!(b, b'-' | b'.')
                    })
            })
            || self.series_id.as_ref().is_some_and(|id| {
                id.len() != 32
                    || !id
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            })
        {
            return Err("Calendar requires an ordered range of at most 367 days, a limit of 1 to 200, either an offset of at most 20,000 or an after cursor, and a valid series identifier".into());
        }
        Ok(())
    }
}

/// Query of `GET /api/series`: a cursor page of summaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesPageQuery {
    pub after: Option<String>,
    pub limit: usize,
}

impl SeriesPageQuery {
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() > 256 {
            return Err("Series query is too large".into());
        }
        let mut query = Self {
            after: None,
            limit: 50,
        };
        let mut seen = std::collections::BTreeSet::new();
        for pair in text.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').ok_or("Invalid series query")?;
            if !seen.insert(name) {
                return Err("Duplicate series query field".into());
            }
            match name {
                "after" => {
                    if value.len() != 32
                        || !value
                            .bytes()
                            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                    {
                        return Err("Invalid series cursor".into());
                    }
                    query.after = Some(value.to_owned());
                }
                "limit" => {
                    query.limit = value
                        .parse()
                        .ok()
                        .filter(|limit| (1..=200).contains(limit) && !value.starts_with('0'))
                        .ok_or("Series limit must be 1 to 200")?;
                }
                _ => return Err("Unknown series query field".into()),
            }
        }
        Ok(query)
    }
}
