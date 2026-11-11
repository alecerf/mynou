//! Wanted titles: films and episodes requested before an acceptable release
//! exists. A search that finds nothing to take (nothing found, or everything
//! refused by identity, seeds or profile), or that could not ask any indexer,
//! leaves the request `wanted` instead of `failed`, without spending one of its
//! attempts. It stays wanted until it is fulfilled, cancelled or removed from
//! the watchlist, across restarts, and feeds and IRC announcements fulfil it
//! whenever a release appears.
//!
//! A slow background pass searches wanted titles again: each at most once per
//! interval, at most `titles_per_pass` an hour, one at a time and evenly
//! spaced, the one due first first. The schedule is each request's
//! `next_attempt_at`, kept in the job index, so a restart searches nothing
//! early. Nothing is searched while downloads are off.
//!
//! A film is not searched before it can be found: its earliest digital,
//! physical or TV release on TMDB, else its first theatrical release plus a
//! delay, like Radarr's "Released". Until then it is wanted and "not yet
//! available": neither searched nor fulfilled by feeds or IRC. Without TMDB, or
//! without any date, nothing holds it back.
use crate::{
    Result,
    config::{self, Config},
    engine::{Engine, lock},
    integrations,
    json::Value,
    store::{self, Job},
};
use std::{
    sync::{Arc, atomic::Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// The wanted search runs in passes of an hour.
pub const PASS_SECS: u64 = 3_600;
/// TMDB is asked again this often about a film that is not out yet.
pub const AVAILABILITY_REFRESH_SECS: u64 = 7 * DAY;
/// A search that could not run, or TMDB that did not answer, is tried again
/// after at most this long.
const RETRY_SECS: u64 = 3_600;
/// How long the pass waits for the title it put back in the queue to be
/// searched before it moves on to the next one.
const IN_FLIGHT: Duration = Duration::from_secs(600);
/// How often the pass looks again when nothing was due.
const IDLE: Duration = Duration::from_secs(60);
const DAY: u64 = 86_400;

/// When a film can be found, as TMDB told it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Availability {
    /// When the film counts as released; 0 when nothing holds it back.
    pub at: u64,
    /// When TMDB was asked; 0 to ask again at the next search.
    pub checked_at: u64,
    /// Which date `at` comes from.
    pub basis: String,
}

/// Where an availability date comes from: the earliest digital, physical or
/// TV release, the first theatrical release plus the delay, the film's
/// release date plus the delay, or nothing known.
const BASES: [&str; 6] = [
    "digital",
    "physical",
    "tv",
    "theatrical",
    "release",
    "unknown",
];

impl Availability {
    pub fn to_json(&self) -> Value {
        let mut value = Value::object();
        // Timestamps as decimal strings, like every stored job time.
        value.insert("at", self.at.to_string());
        value.insert("checked_at", self.checked_at.to_string());
        value.insert("basis", self.basis.clone());
        value
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let time = |name: &str| -> Result<u64> {
            value
                .get(name)
                .and_then(Value::as_str)
                .and_then(|text| text.parse().ok())
                .ok_or_else(|| format!("invalid availability {name}"))
        };
        let basis = value
            .get("basis")
            .and_then(Value::as_str)
            .filter(|basis| BASES.contains(basis))
            .ok_or("invalid availability basis")?;
        Ok(Self {
            at: time("at")?,
            checked_at: time("checked_at")?,
            basis: basis.into(),
        })
    }
}

/// When a film can be found, from its TMDB details with `release_dates`
/// appended. Release types: 2 and 3 theatrical, 4 digital, 5 physical, 6 TV;
/// premieres (1) do not count. The earliest date of any country counts.
pub(crate) fn availability(details: &Value, delay_days: u32, checked_at: u64) -> Availability {
    let mut home: Option<(u64, &str)> = None;
    let mut theatrical: Option<u64> = None;
    let countries = details
        .get("release_dates")
        .and_then(|dates| dates.get("results"))
        .and_then(Value::as_array)
        .unwrap_or_default();
    for country in countries {
        let releases = country
            .get("release_dates")
            .and_then(Value::as_array)
            .unwrap_or_default();
        for release in releases {
            let Some(at) = release
                .get("release_date")
                .and_then(Value::as_str)
                .and_then(day_start)
            else {
                continue;
            };
            match release.get("type").and_then(Value::as_u64) {
                Some(2 | 3) => theatrical = Some(theatrical.map_or(at, |known| known.min(at))),
                Some(kind @ 4..=6) if home.is_none_or(|(known, _)| at < known) => {
                    home = Some((at, ["digital", "physical", "tv"][kind as usize - 4]));
                }
                _ => {}
            }
        }
    }
    let delay = u64::from(delay_days) * DAY;
    let theatrical = theatrical.map(|at| (at.saturating_add(delay), "theatrical"));
    let (at, basis) = match (home, theatrical) {
        (Some(home), Some(theatrical)) => {
            if home.0 <= theatrical.0 {
                home
            } else {
                theatrical
            }
        }
        (Some(found), None) | (None, Some(found)) => found,
        (None, None) => details
            .get("release_date")
            .and_then(Value::as_str)
            .and_then(day_start)
            .map_or((0, "unknown"), |at| (at.saturating_add(delay), "release")),
    };
    Availability {
        at,
        checked_at,
        basis: basis.into(),
    }
}

/// The start of the day of a TMDB date or date and time, such as
/// `2024-05-01` or `2024-05-01T00:00:00.000Z`, in Unix seconds.
fn day_start(text: &str) -> Option<u64> {
    let day = crate::date::day(text.get(..10)?).ok()?;
    Some(u64::try_from(day.max(0)).ok()? * DAY)
}

/// Why a search left a request wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unfulfilled {
    /// The indexers answered, and nothing they offered is acceptable.
    NoRelease,
    /// No indexer could be asked, or none answered in time.
    Unreachable,
}

/// Whether a job error says that the search ran without a release to take,
/// or could not run; any other error keeps the usual retry rules.
pub(crate) fn unfulfilled(error: &str) -> Option<Unfulfilled> {
    if error == integrations::NO_RELEASE {
        Some(Unfulfilled::NoRelease)
    } else if [
        integrations::NO_INDEXER,
        integrations::NO_INDEXER_ANSWER,
        integrations::SEARCH_EXPIRED,
    ]
    .contains(&error)
    {
        Some(Unfulfilled::Unreachable)
    } else {
        None
    }
}

/// A film or an episode Mynou searches for itself, with nothing chosen,
/// downloaded or bound to another source yet: the requests that can be
/// wanted.
pub(crate) fn searchable(job: &Job) -> bool {
    matches!(job.request.kind.as_str(), "movie" | "episode")
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

/// Whether nothing holds a request back: a film that is out, or any episode.
pub fn available(job: &Job, now: u64) -> bool {
    job.availability.as_ref().is_none_or(|a| a.at <= now)
}

/// A wanted film that is not out yet.
pub fn not_yet_available(job: &Job, now: u64) -> bool {
    job.state == "wanted" && !available(job, now)
}

/// A wanted request that a feed entry or an IRC announcement may fulfil.
pub(crate) fn routable(job: &Job, now: u64) -> bool {
    job.state == "wanted" && job.lease_id.is_none() && available(job, now)
}

/// Leaves a request wanted after a search that `reason` explains, without
/// spending an attempt: the next search comes after the interval, or within
/// the hour for a search that could not run.
pub(crate) fn settle(job: &mut Job, reason: Unfulfilled, wanted: &config::Wanted, now: u64) {
    let wait = match reason {
        Unfulfilled::NoRelease => wanted.interval_secs,
        Unfulfilled::Unreachable => wanted.interval_secs.min(RETRY_SECS),
    };
    job.state = "wanted".into();
    job.progress = 0.0;
    job.searched_at = now;
    job.next_attempt_at = now.saturating_add(wait);
}

/// The time between two titles of the pass: the hour shared evenly.
pub(crate) fn spacing(wanted: &config::Wanted) -> Duration {
    Duration::from_millis((PASS_SECS * 1_000).div_ceil(u64::from(wanted.titles_per_pass.max(1))))
}

impl Engine {
    /// Whether a film must wait for its release before any search. TMDB is
    /// asked when its dates are unknown, or stale while it is not out. A film
    /// that waits is left wanted until its date, or until TMDB is asked again.
    pub(crate) fn film_waits(&self, config: &Config, job: &mut Job) -> bool {
        if job.request.kind != "movie" || !searchable(job) {
            return false;
        }
        let now = store::now();
        let stale = job.availability.as_ref().is_none_or(|known| {
            known.at > now && known.checked_at.saturating_add(AVAILABILITY_REFRESH_SECS) <= now
        });
        let mut ask_again = None;
        if stale
            && config.catalog.enabled
            && let Some(id) = job.request.tmdb_id
        {
            match integrations::movie_release_dates_before(
                config,
                id,
                Instant::now() + Duration::from_secs(20),
            ) {
                Ok(details) => {
                    job.availability = Some(availability(
                        &details,
                        config.wanted.theatrical_delay_days,
                        now,
                    ));
                }
                // What was known still holds; without it, nothing does.
                Err(_) => ask_again = Some(now.saturating_add(RETRY_SECS)),
            }
        }
        let Some(known) = job.availability.as_ref().filter(|known| known.at > now) else {
            return false;
        };
        let refresh =
            ask_again.unwrap_or_else(|| known.checked_at.saturating_add(AVAILABILITY_REFRESH_SECS));
        job.state = "wanted".into();
        job.progress = 0.0;
        job.last_error = None;
        job.next_attempt_at = known.at.min(refresh).max(now.saturating_add(60));
        true
    }

    /// Puts back in the queue the wanted title whose next search comes first,
    /// once it is due, for a worker to search it now. `None` when no title is
    /// due, or while downloads are off.
    pub fn search_wanted(&self) -> Result<Option<Job>> {
        if self.read_only {
            return Err("Read-only preview cannot search wanted titles".into());
        }
        if !self.config.downloads_enabled {
            return Ok(None);
        }
        lock(&self.store)?.requeue_wanted(store::now())
    }

    /// Requests that failed for good, before wanted titles existed, because a
    /// search found nothing or reached no indexer, become wanted. Runs once
    /// per store, in bounded pages; returns how many changed.
    pub fn adopt_wanted(&self) -> Result<usize> {
        if self.read_only || lock(&self.store)?.wanted_adopted()? {
            return Ok(0);
        }
        let mut adopted = 0;
        let mut after: Option<String> = None;
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return Ok(adopted);
            }
            let page = lock(&self.store)?.page(after.as_deref(), 200)?;
            for job in page.jobs.iter().filter(|job| store::adoptable(job)) {
                if lock(&self.store)?.adopt_wanted(&job.id, self.config.wanted.interval_secs)? {
                    adopted += 1;
                }
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        lock(&self.store)?.finish_wanted_adoption()?;
        Ok(adopted)
    }

    /// One turn of the pass: true when it put a title back in the queue. It
    /// waits while the network is down and while the title it put back last
    /// is still to be searched, so searches never pile up.
    fn wanted_turn(&self, in_flight: &mut Option<(String, Instant)>) -> Result<bool> {
        if self.network_waiting() {
            return Ok(false);
        }
        if let Some((id, since)) = in_flight
            && since.elapsed() < IN_FLIGHT
            && lock(&self.store)?
                .get(id)?
                .is_some_and(|job| matches!(job.state.as_str(), "queued" | "processing"))
        {
            return Ok(false);
        }
        *in_flight = None;
        Ok(match self.search_wanted()? {
            Some(job) => {
                *in_flight = Some((job.id, Instant::now()));
                true
            }
            None => false,
        })
    }
}

/// Adopts the requests that failed for good without a release, then runs the
/// wanted search while downloads are on.
pub(crate) fn start(engine: &Arc<Engine>, handles: &mut Vec<JoinHandle<()>>) {
    if engine.read_only {
        return;
    }
    let engine = engine.clone();
    handles.push(thread::spawn(move || {
        if let Err(error) = engine.adopt_wanted() {
            crate::error!("Wanted titles: {error}");
        }
        if !engine.config.downloads_enabled {
            return;
        }
        let spacing = spacing(&engine.config.wanted);
        let mut in_flight = None;
        // A restart starts with a pause: it never searches at once.
        let mut delay = spacing.min(IDLE);
        while !engine.stopped.load(Ordering::Acquire) {
            engine.wait(delay.as_millis() as u64);
            if engine.stopped.load(Ordering::Acquire) {
                break;
            }
            delay = match engine.wanted_turn(&mut in_flight) {
                Ok(true) => spacing,
                Ok(false) => IDLE.min(spacing),
                Err(error) => {
                    crate::error!("Wanted titles: {error}");
                    IDLE
                }
            };
        }
    }));
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::json;

    fn at(date: &str) -> u64 {
        day_start(date).unwrap()
    }

    pub(crate) fn queued_film() -> Job {
        let request = crate::store::Request {
            kind: "movie".into(),
            title: "Fixture Movie".into(),
            year: 2026,
            season: 0,
            episode: 0,
            source_path: None,
            source_url: None,
            tmdb_id: Some(7),
            runtime_minutes: None,
            source_numbering: None,
            original_language: None,
        };
        let mut value = Value::object();
        value.insert("id", "7".repeat(32));
        value.insert("key", request.canonical_key());
        value.insert("request", request.to_json());
        value.insert("state", "processing");
        value.insert("progress", Value::Number(0.0));
        value.insert("files", Value::Array(Vec::new()));
        value.insert("imports", Value::Array(Vec::new()));
        for name in [
            "attempts",
            "created_at",
            "updated_at",
            "next_attempt_at",
            "lease_until",
        ] {
            value.insert(name, "0");
        }
        Job::from_json(&value).unwrap()
    }

    #[test]
    fn jobs_written_before_wanted_titles_still_read() {
        let job = queued_film();
        assert_eq!((job.searched_at, job.availability.as_ref()), (0, None));
        // Absent fields stay absent, so an earlier record keeps its bytes.
        let value = job.to_json();
        assert!(value.get("searched_at").is_none() && value.get("availability").is_none());
        let mut wanted = job.clone();
        wanted.searched_at = 9;
        wanted.availability = Some(Availability {
            at: 10,
            checked_at: 9,
            basis: "theatrical".into(),
        });
        assert_eq!(Job::from_json(&wanted.to_json()).unwrap(), wanted);
        assert!(available(&wanted, 10) && !available(&wanted, 9));
    }

    fn details(release_date: Option<&str>, dates: &[(&str, u64, &str)]) -> Value {
        let mut countries = Vec::new();
        for (country, kind, date) in dates {
            countries.push(format!(
                r#"{{"iso_3166_1":"{country}","release_dates":[{{"type":{kind},"release_date":"{date}T00:00:00.000Z","certification":""}}]}}"#
            ));
        }
        let release = release_date.map_or("null".into(), |date| format!("\"{date}\""));
        json::parse(&format!(
            r#"{{"id":7,"release_date":{release},"release_dates":{{"results":[{}]}}}}"#,
            countries.join(",")
        ))
        .unwrap()
    }

    #[test]
    fn availability_takes_the_earliest_home_release_or_theatrical_plus_the_delay() {
        // A digital release in one country beats a later physical one elsewhere.
        let film = details(
            Some("2026-01-10"),
            &[
                ("US", 3, "2026-01-10"),
                ("US", 4, "2026-03-01"),
                ("FR", 5, "2026-02-20"),
                ("FR", 1, "2025-09-01"),
            ],
        );
        let found = availability(&film, 90, 5);
        assert_eq!(
            (found.at, found.basis.as_str(), found.checked_at),
            (at("2026-02-20"), "physical", 5)
        );
        // A home release later than theatrical plus the delay waits no longer.
        let found = availability(&film, 30, 5);
        assert_eq!(
            (found.at, found.basis.as_str()),
            (at("2026-02-09"), "theatrical")
        );
        // Only theatrical dates: the earliest of them plus the delay.
        let cinema = details(None, &[("FR", 2, "2026-05-01"), ("US", 3, "2026-04-01")]);
        let found = availability(&cinema, 90, 0);
        assert_eq!(
            (found.at, found.basis.as_str()),
            (at("2026-06-30"), "theatrical")
        );
        // A TV film is out on its broadcast.
        let tv = details(None, &[("GB", 6, "2025-12-24")]);
        assert_eq!(availability(&tv, 90, 0).basis, "tv");
        // Premieres alone fall back to the release date plus the delay.
        let festival = details(Some("2026-01-01"), &[("FR", 1, "2025-05-20")]);
        let found = availability(&festival, 10, 0);
        assert_eq!(
            (found.at, found.basis.as_str()),
            (at("2026-01-11"), "release")
        );
        // Nothing known holds nothing back.
        let found = availability(&details(None, &[]), 90, 0);
        assert_eq!((found.at, found.basis.as_str()), (0, "unknown"));
        let found = availability(&json::parse(r#"{"id":7}"#).unwrap(), 90, 0);
        assert_eq!(found.at, 0);
    }

    #[test]
    fn availability_round_trips_and_refuses_unknown_bases() {
        let known = Availability {
            at: 1_800_000_000,
            checked_at: 1_700_000_000,
            basis: "digital".into(),
        };
        assert_eq!(Availability::from_json(&known.to_json()).unwrap(), known);
        let mut value = known.to_json();
        value.insert("basis", "rumour");
        assert!(Availability::from_json(&value).is_err());
        let mut value = known.to_json();
        value.insert("at", Value::Number(-1.0));
        assert!(Availability::from_json(&value).is_err());
    }

    #[test]
    fn only_searches_without_a_release_or_an_indexer_leave_a_title_wanted() {
        assert_eq!(
            unfulfilled(integrations::NO_RELEASE),
            Some(Unfulfilled::NoRelease)
        );
        for error in [
            integrations::NO_INDEXER,
            integrations::NO_INDEXER_ANSWER,
            integrations::SEARCH_EXPIRED,
        ] {
            assert_eq!(unfulfilled(error), Some(Unfulfilled::Unreachable));
        }
        for error in [
            "runtime_mismatch: 22 min for a 120 min title",
            "No supported media found for this request",
            "Plex: network request failed",
            "Search: unsupported search identity",
        ] {
            assert_eq!(unfulfilled(error), None, "{error}");
        }
        let wanted = config::Wanted::default();
        let mut job = queued_film();
        assert!(searchable(&job));
        settle(&mut job, Unfulfilled::NoRelease, &wanted, 1_000);
        assert_eq!(
            (job.state.as_str(), job.attempts, job.searched_at),
            ("wanted", 0, 1_000)
        );
        assert_eq!(job.next_attempt_at, 1_000 + wanted.interval_secs);
        settle(&mut job, Unfulfilled::Unreachable, &wanted, 2_000);
        assert_eq!(job.next_attempt_at, 2_000 + RETRY_SECS);
    }

    #[test]
    fn a_pass_spreads_its_titles_evenly_over_the_hour() {
        let mut wanted = config::Wanted::default();
        assert_eq!(spacing(&wanted), Duration::from_secs(180));
        wanted.titles_per_pass = 1;
        assert_eq!(spacing(&wanted), Duration::from_secs(PASS_SECS));
        wanted.titles_per_pass = 120;
        assert_eq!(spacing(&wanted), Duration::from_secs(30));
        // However many titles are due, a pass of an hour holds at most its bound.
        for titles in [1, 7, 20, 120] {
            wanted.titles_per_pass = titles;
            let gap = spacing(&wanted);
            let searched = (0..1_000)
                .map(|n| gap * n)
                .take_while(|elapsed| *elapsed < Duration::from_secs(PASS_SECS))
                .count();
            assert_eq!(searched, titles as usize);
        }
    }
}
