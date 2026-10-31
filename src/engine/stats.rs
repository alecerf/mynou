//! Overview statistics, computed by a background worker so the overview never
//! waits for them: a pass over the jobs and followed series every minute, and
//! Plex's library and watchlist every ten minutes. Requests are counted once
//! per film or episode, by the most advanced of their jobs.
use super::{Engine, lock};
use crate::{Result, date, integrations, json::Value, store::Request};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

/// Plex is read at most this often.
const PLEX_EVERY: Duration = Duration::from_secs(600);
/// Pages of 200 jobs read in one pass; a larger store is reported as partial.
const MAX_PAGES: usize = 1_000;
const MAX_SERIES: usize = 1_000;
const MAX_WATCHLIST: usize = 5_000;
const WEEKS: i64 = 12;
const FAILURES: usize = 5;

/// What became of a film or episode, best first.
const CATEGORIES: [&str; 5] = ["fetched", "in_plex", "in_progress", "failed", "cancelled"];

#[derive(Default)]
pub(crate) struct Cache {
    report: Option<Value>,
    plex: Option<Plex>,
}

struct Plex {
    read_at: Instant,
    checked_at: u64,
    library: std::result::Result<[u64; 3], String>,
    watchlist: std::result::Result<Vec<Request>, String>,
}

struct Media {
    kind: &'static str,
    category: usize,
    updated_at: u64,
}

fn category(state: &str, imported: bool) -> usize {
    match state {
        "ready" if imported => 0,
        "ready" => 1,
        "failed" => 3,
        "cancelled" => 4,
        _ => 2,
    }
}

fn number(n: u64) -> Value {
    Value::Number(n as f64)
}

fn tally(counts: &[u64; 5], missing: Option<u64>) -> Value {
    let mut value = Value::object();
    for (name, n) in CATEGORIES.iter().zip(counts) {
        value.insert(*name, number(*n));
    }
    if let Some(missing) = missing {
        value.insert("missing", number(missing));
    }
    value.insert(
        "total",
        number(counts.iter().sum::<u64>() + missing.unwrap_or(0)),
    );
    value
}

impl Engine {
    /// The last statistics computed, or `null` before the first pass.
    pub fn stats(&self) -> Result<Value> {
        Ok(lock(&self.stats)?.report.clone().unwrap_or(Value::Null))
    }

    pub(super) fn refresh_stats(&self) {
        let report = self.compute_stats().unwrap_or_else(|error| {
            let mut value = Value::object();
            value.insert("error", error);
            value
        });
        if let Ok(mut cache) = lock(&self.stats) {
            cache.report = Some(report);
        }
    }

    fn read_plex(&self) -> Result<()> {
        let fresh = lock(&self.stats)?
            .plex
            .as_ref()
            .is_some_and(|plex| plex.read_at.elapsed() < PLEX_EVERY);
        if fresh || !self.config.plex.enabled {
            return Ok(());
        }
        let plex = Plex {
            read_at: Instant::now(),
            checked_at: crate::store::now(),
            library: integrations::plex_library_counts(&self.config),
            watchlist: integrations::plex_watchlist(&self.config, MAX_WATCHLIST),
        };
        lock(&self.stats)?.plex = Some(plex);
        Ok(())
    }

    fn compute_stats(&self) -> Result<Value> {
        // Every film and episode, by the most advanced of its jobs.
        let mut media: BTreeMap<String, Media> = BTreeMap::new();
        let mut failures = Vec::new();
        let mut after = None;
        let mut partial = false;
        for page in 0.. {
            if page == MAX_PAGES {
                partial = true;
                break;
            }
            let jobs = lock(&self.store)?.page(after.as_deref(), 200)?;
            for job in &jobs.jobs {
                let kind = match job.request.kind.as_str() {
                    "movie" => "movie",
                    "episode" => "episode",
                    _ => continue,
                };
                let found = Media {
                    kind,
                    category: category(&job.state, !job.imports.is_empty()),
                    updated_at: job.updated_at,
                };
                if job.state == "failed" {
                    failures.push(job.clone());
                    if failures.len() > 4 * FAILURES {
                        failures.sort_by_key(|job| std::cmp::Reverse(job.updated_at));
                        failures.truncate(FAILURES);
                    }
                }
                let key = job.request.media_key();
                match media.get_mut(&key) {
                    Some(known) if found.category < known.category => *known = found,
                    Some(_) => {}
                    None => {
                        media.insert(key, found);
                    }
                }
            }
            match jobs.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        let mut by_kind = BTreeMap::from([("movie", [0_u64; 5]), ("episode", [0_u64; 5])]);
        let today = date::day(&date::today())?;
        // Weeks start on Monday; 1970-01-01 was a Thursday.
        let this_week = today - (today + 3).rem_euclid(7);
        let first_week = this_week - 7 * (WEEKS - 1);
        let mut weeks = vec![[0_u64; 2]; WEEKS as usize];
        for found in media.values() {
            if let Some(counts) = by_kind.get_mut(found.kind) {
                counts[found.category] += 1;
            }
            let day = (found.updated_at / 86_400) as i64;
            if found.category == 0 && day >= first_week {
                let week = ((day - first_week) / 7) as usize;
                if let Some(slot) = weeks.get_mut(week) {
                    slot[usize::from(found.kind == "episode")] += 1;
                }
            }
        }
        let mut value = Value::object();
        value.insert("computed_at", number(crate::store::now()));
        value.insert("partial", partial);
        let mut requests = Value::object();
        for (kind, counts) in &by_kind {
            requests.insert(*kind, tally(counts, None));
        }
        value.insert("requests", requests);
        let mut history = Vec::new();
        for (i, [movies, episodes]) in weeks.iter().enumerate() {
            let mut week = Value::object();
            week.insert("week", date::from_day(first_week + 7 * i as i64)?);
            week.insert("movie", number(*movies));
            week.insert("episode", number(*episodes));
            history.push(week);
        }
        value.insert("history", Value::Array(history));

        let (series, followed) = self.series_stats(&media)?;
        value.insert("series", Value::Array(series));

        failures.sort_by_key(|job| std::cmp::Reverse(job.updated_at));
        let mut health = Value::object();
        health.insert(
            "failures",
            Value::Array(
                failures
                    .iter()
                    .take(FAILURES)
                    .map(|job| {
                        let mut v = Value::object();
                        v.insert("id", job.id.clone());
                        v.insert("request", job.request.to_json());
                        v.insert(
                            "error",
                            job.last_error.clone().map_or(Value::Null, Value::from),
                        );
                        v.insert("at", number(job.updated_at));
                        v
                    })
                    .collect(),
            ),
        );
        value.insert("health", health);

        if let Err(error) = self.read_plex() {
            value.insert("plex_error", error);
        }
        let cache = lock(&self.stats)?;
        if let Some(plex) = &cache.plex {
            let mut library = Value::object();
            library.insert("checked_at", number(plex.checked_at));
            match &plex.library {
                Ok([films, series, episodes]) => {
                    library.insert("films", number(*films));
                    library.insert("series", number(*series));
                    library.insert("episodes", number(*episodes));
                }
                Err(error) => library.insert("error", error.clone()),
            }
            value.insert("plex", library);
            let mut watchlist = Value::object();
            watchlist.insert("checked_at", number(plex.checked_at));
            match &plex.watchlist {
                Ok(items) => {
                    let mut films = [0_u64; 5];
                    let mut missing = 0;
                    let (mut series, mut tracked) = (0_u64, 0_u64);
                    for item in items {
                        if item.kind == "series" {
                            series += 1;
                            tracked +=
                                u64::from(item.tmdb_id.is_some_and(|id| followed.contains(&id)));
                        } else if let Some(found) = media.get(&item.media_key()) {
                            films[found.category] += 1;
                        } else {
                            missing += 1;
                        }
                    }
                    watchlist.insert("films", tally(&films, Some(missing)));
                    let mut shows = Value::object();
                    shows.insert("total", number(series));
                    shows.insert("followed", number(tracked));
                    watchlist.insert("series", shows);
                }
                Err(error) => watchlist.insert("error", error.clone()),
            }
            value.insert("watchlist", watchlist);
        }
        Ok(value)
    }

    /// Each followed series: its aired episodes by what became of them, and
    /// the next one to air. Also the catalog identities followed.
    fn series_stats(
        &self,
        media: &BTreeMap<String, Media>,
    ) -> Result<(Vec<Value>, std::collections::BTreeSet<u64>)> {
        let today = date::today();
        let mut rows = Vec::new();
        let mut followed = std::collections::BTreeSet::new();
        let mut after: Option<String> = None;
        while rows.len() < MAX_SERIES {
            let page = lock(&self.series_store)?.page(after.as_deref(), 200)?;
            for summary in &page.series {
                let Some(id) = summary.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(record) = lock(&self.series_store)?.get(id)? else {
                    continue;
                };
                if let Some(tmdb) = record.plan.request.tmdb_id {
                    followed.insert(tmdb);
                }
                let mut counts = [0_u64; 5];
                let mut missing = 0;
                let mut next: Option<&crate::series::Episode> = None;
                for episode in &record.plan.episodes {
                    if !record.episode_monitored(episode) {
                        continue;
                    }
                    match episode.air_date.as_deref() {
                        Some(day) if day <= today.as_str() => {
                            match media.get(&record.episode_request(episode).media_key()) {
                                Some(found) => counts[found.category] += 1,
                                None => missing += 1,
                            }
                        }
                        Some(day)
                            if next
                                .and_then(|n| n.air_date.as_deref())
                                .is_none_or(|known| day < known) =>
                        {
                            next = Some(episode);
                        }
                        _ => {}
                    }
                }
                let mut row = Value::object();
                row.insert("id", record.id.clone());
                row.insert("request", record.plan.request.to_json());
                row.insert("monitored", record.monitored);
                row.insert("aired", tally(&counts, Some(missing)));
                row.insert(
                    "next",
                    next.map_or(Value::Null, |episode| episode.to_json()),
                );
                row.insert(
                    "last_error",
                    record.last_error.clone().map_or(Value::Null, Value::from),
                );
                rows.push(row);
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        Ok((rows, followed))
    }
}
