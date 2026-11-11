//! Opt-in automatic cross-seed discovery. Feeds and completion searches only
//! nominate candidates: a candidate is registered through the manual
//! cross-seed path, which verifies every piece from existing bytes before any
//! announce. Discovery never downloads payload, creates imports or writes media.
pub(crate) mod store;
#[cfg(test)]
mod tests;

use crate::{
    Result,
    config::Source,
    engine::{Engine, lock},
    integrations::{self, Offer},
    json::Value,
    torrent::{CandidateLayout, CrossSeedRequest, ReuseSource},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, atomic::Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use store::{Batch, Outcome, Provenance};

/// Completed transfers compared in one pass.
const MAX_CATALOG: usize = 50_000;
/// Candidate metainfo downloads per source and pass.
const MAX_FETCHES: usize = 8;
/// Completion searches per source and pass.
const MAX_SEARCHES: usize = 1;
const PASS_INTERVAL: Duration = Duration::from_secs(60);
const REQUEST_BUDGET: Duration = Duration::from_secs(30);
const FAILURE_RETRY: Duration = Duration::from_secs(300);
const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "m4v", "ts", "m2ts", "wmv", "mov"];

/// Volatile schedule and last report; the durable state is in the store.
#[derive(Default)]
pub(crate) struct Runtime {
    next_feed: BTreeMap<String, Instant>,
    last_pass_at: Option<u64>,
    last_report: Option<Value>,
}

/// One requested pass.
#[derive(Clone, Debug, Default)]
pub struct Run {
    /// Evaluate and report without registering or remembering anything.
    pub dry_run: bool,
    /// Ignore the feed schedule and search cooldowns (rate limits still apply).
    pub force: bool,
    pub indexer: Option<String>,
    /// Search only for this completed transfer.
    pub transfer: Option<String>,
}
impl Run {
    pub fn from_json(value: &Value) -> Result<Self> {
        crate::numbering::only(value, &["dry_run", "force", "indexer", "transfer"])?;
        let flag = |key: &str| match value.get(key) {
            None => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            _ => Err(format!("{key} must be a boolean")),
        };
        let text = |key: &str| match value.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.is_empty() && s.len() <= 128 => Ok(Some(s.clone())),
            _ => Err(format!("{key} must be a non-empty string")),
        };
        let transfer = text("transfer")?.map(|id| id.to_ascii_lowercase());
        if transfer.as_ref().is_some_and(|id| {
            ![40, 64].contains(&id.len()) || !id.bytes().all(|b| b.is_ascii_hexdigit())
        }) {
            return Err("transfer must be a native transfer ID".into());
        }
        Ok(Self {
            dry_run: flag("dry_run")?,
            force: flag("force")?,
            indexer: text("indexer")?,
            transfer,
        })
    }
}

/// Lowercase alphanumeric words of a release or torrent name, without a
/// trailing video extension, so separators and case never decide a match.
pub(crate) fn normalize(name: &str) -> String {
    strip_video_extension(name.trim())
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The series words, season and, for a single episode, episode of a release
/// or torrent name; `None` when it names no season or several.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Numbered {
    pub(crate) series: String,
    pub(crate) season: u32,
    pub(crate) episode: Option<u32>,
}

fn season_word(word: &str) -> Option<u32> {
    let digits = word.strip_prefix('s')?;
    (!digits.is_empty() && digits.len() <= 4 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// A word that numbers an episode or a season.
fn numbering_word(word: &str) -> bool {
    crate::numbering::episode_marker(word).is_some()
        || season_word(word).is_some()
        || word
            .strip_prefix('e')
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        || (word.starts_with('s')
            && word.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
            && word.contains('e'))
}

/// Words of a name, in their original case, and where its numbering starts.
fn numbered_words(name: &str) -> Option<(Vec<&str>, usize, Numbered)> {
    let stem = strip_video_extension(name.trim());
    let original: Vec<&str> = stem
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let words = crate::selection::tokens(stem);
    if words.len() != original.len() {
        return None;
    }
    let at = words
        .iter()
        .position(|word| numbering_word(word) || matches!(word.as_str(), "season" | "saison"))?;
    let (season, episode, consumed) =
        if let Some(number) = crate::numbering::episode_marker(&words[at]) {
            (number.season, Some(number.episode), 1)
        } else if let Some(season) = season_word(&words[at]) {
            (season, None, 1)
        } else {
            (
                crate::numbering::decimal_token(words.get(at + 1)?)?,
                None,
                2,
            )
        };
    // Another number after it: several episodes or seasons.
    if words[at + consumed..]
        .iter()
        .any(|word| numbering_word(word) || matches!(word.as_str(), "season" | "saison"))
    {
        return None;
    }
    let mut series = &words[..at];
    // A year is often left out of one name and kept in another.
    if series.len() > 1
        && series
            .last()
            .and_then(|word| crate::numbering::decimal_token(word))
            .is_some_and(|year| (1888..=2200).contains(&year))
    {
        series = &series[..series.len() - 1];
    }
    if series.is_empty() {
        return None;
    }
    Some((
        original,
        at,
        Numbered {
            series: series.join(" "),
            season,
            episode,
        },
    ))
}

pub(crate) fn numbered(name: &str) -> Option<Numbered> {
    numbered_words(name).map(|(_, _, numbered)| numbered)
}

/// A text search for every release of a season pack's season, its single
/// episodes included: the pack's title words and `S01`.
pub(crate) fn season_query(name: &str) -> Option<String> {
    let (words, at, numbered) = numbered_words(name)?;
    if numbered.episode.is_some() {
        return None;
    }
    Some(format!("{} S{:02}", words[..at].join(" "), numbered.season))
}

fn strip_video_extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, extension))
            if VIDEO_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known)) =>
        {
            stem
        }
        _ => name,
    }
}

/// Original source indices for each candidate file, in candidate order, or
/// `None` for a missing small file to download. Equal lengths are required;
/// file names, then paths below the root, break ties. Anything still
/// ambiguous is refused rather than guessed. Video is never missing, at least
/// one file is reused and the missing files hold at most `limit` bytes.
pub(crate) fn map_files(
    source: &ReuseSource,
    candidate: &CandidateLayout,
    limit: u64,
) -> std::result::Result<Vec<Option<usize>>, &'static str> {
    let below_root = |path: &Path| path.components().skip(1).collect::<std::path::PathBuf>();
    let mut used = BTreeSet::new();
    let mut mapping = Vec::with_capacity(candidate.files.len());
    let mut missing = 0_u64;
    for (path, length) in &candidate.files {
        let sized: Vec<_> = source
            .files
            .iter()
            .filter(|file| file.2 == *length && !used.contains(&file.0))
            .collect();
        let pick = match sized.as_slice() {
            [] if crate::torrent::downloadable_extra(path) => {
                missing = missing.saturating_add(*length);
                mapping.push(None);
                continue;
            }
            [] => return Err("layout_mismatch"),
            [only] => *only,
            _ => {
                let named: Vec<_> = sized
                    .iter()
                    .filter(|file| file.1.file_name() == path.file_name())
                    .collect();
                match named.as_slice() {
                    [only] => **only,
                    _ => {
                        let placed: Vec<_> = sized
                            .iter()
                            .filter(|file| below_root(&file.1) == below_root(path))
                            .collect();
                        match placed.as_slice() {
                            [only] => **only,
                            _ => return Err("ambiguous_layout"),
                        }
                    }
                }
            }
        };
        used.insert(pick.0);
        mapping.push(Some(pick.0));
    }
    if !mapping.iter().any(Option::is_some) {
        return Err("layout_mismatch");
    }
    if missing > limit {
        return Err("too_much_missing");
    }
    Ok(mapping)
}

/// Completed transfers grouped by normalized name.
struct Catalog {
    by_name: BTreeMap<String, Vec<ReuseSource>>,
    /// Season packs by series words and season: their normalized names and IDs.
    packs: BTreeMap<(String, u32), Vec<(String, String)>>,
    /// Newest first, so recent completions are searched before older ones.
    order: Vec<(String, String)>,
    truncated: bool,
}

#[derive(Default)]
struct Tally {
    offers: u32,
    unmatched: u32,
    deferred: u32,
    fetched: u32,
    added: u32,
    skipped: u32,
    rejected: u32,
    would_add: u32,
    searches: u32,
}
impl Tally {
    fn to_json(&self) -> Value {
        let mut v = Value::object();
        for (k, n) in [
            ("offers", self.offers),
            ("unmatched", self.unmatched),
            ("deferred", self.deferred),
            ("metainfo_fetched", self.fetched),
            ("added", self.added),
            ("skipped", self.skipped),
            ("rejected", self.rejected),
            ("would_add", self.would_add),
            ("searches", self.searches),
        ] {
            v.insert(k, n);
        }
        v
    }
}

/// What happened to one offer.
enum Verdict {
    /// Not a candidate of any completed transfer; remembered silently.
    Unmatched,
    /// Retry in a later pass: nothing is remembered.
    Deferred,
    /// Stop this source for the pass; the offer is retried later.
    SourceFailed,
    Done {
        result: &'static str,
        reason: &'static str,
        source: Option<String>,
        candidate: Option<String>,
    },
}

struct Context<'a> {
    source: &'a Source,
    indexer: String,
    trigger: &'static str,
    run: &'a Run,
    fetches: usize,
    last_request: Option<Instant>,
    /// Candidates added or proposed in this pass, so repeats cost nothing.
    nominated: BTreeSet<String>,
}

/// Keep deferred offers and the unexamined tail after a source failure.
fn unfinished_offers(
    offers: Vec<Offer>,
    mut evaluate: impl FnMut(&Offer) -> Option<bool>,
) -> (Vec<Offer>, bool) {
    let mut pending = Vec::new();
    let mut offers = offers.into_iter();
    while let Some(offer) = offers.next() {
        match evaluate(&offer) {
            Some(true) => {}
            Some(false) => pending.push(offer),
            None => {
                pending.push(offer);
                pending.extend(offers);
                return (pending, true);
            }
        }
    }
    (pending, false)
}

impl Engine {
    fn cross_seed_sources(&self, only: Option<&str>) -> Vec<(String, &Source)> {
        let mut sources: Vec<_> = self
            .config
            .sources
            .iter()
            .filter(|source| source.options.cross_seed.enabled)
            .map(|source| (source.options.identity(source), source))
            .filter(|(id, _)| only.is_none_or(|only| only == id))
            .collect();
        sources.sort_by(|a, b| a.0.cmp(&b.0));
        sources
    }

    fn cross_seed_catalog(&self, transfer: Option<&str>) -> Result<Catalog> {
        let client = self.native_client()?;
        let mut catalog = Catalog {
            by_name: BTreeMap::new(),
            packs: BTreeMap::new(),
            order: Vec::new(),
            truncated: false,
        };
        let mut after = None;
        let mut count = 0_usize;
        loop {
            let (page, next) = client.reuse_sources(after.as_deref(), 200)?;
            for source in page {
                if transfer.is_some_and(|id| id != source.id) {
                    continue;
                }
                if count == MAX_CATALOG {
                    catalog.truncated = true;
                    break;
                }
                count += 1;
                let name = normalize(&source.name);
                if name.is_empty() {
                    continue;
                }
                catalog.order.push((source.id.clone(), name.clone()));
                if let Some(pack) = numbered(&source.name).filter(|n| n.episode.is_none()) {
                    catalog
                        .packs
                        .entry((pack.series, pack.season))
                        .or_default()
                        .push((name.clone(), source.id.clone()));
                }
                catalog.by_name.entry(name).or_default().push(source);
            }
            match next {
                Some(next) if !catalog.truncated => after = Some(next),
                _ => break,
            }
        }
        catalog.order.reverse();
        Ok(catalog)
    }

    /// One bounded pass: due feeds, then pending completion searches.
    pub fn cross_seed_discover(&self, run: &Run) -> Result<Value> {
        if self.read_only || self.stopped.load(Ordering::Acquire) {
            return Err("Cross-seed discovery requires the running service".into());
        }
        if !self.config.downloads_enabled || !self.config.downloads.seed {
            return Err(
                "Cross-seed discovery requires enabled downloads and downloads.seed".into(),
            );
        }
        let sources = self.cross_seed_sources(run.indexer.as_deref());
        if run.indexer.is_some() && sources.is_empty() {
            return Err("Cross-seed discovery is not enabled for this indexer".into());
        }
        let _pass = self
            .cross_seed_lock
            .try_lock()
            .map_err(|_| "Cross-seed discovery is already running")?;
        let catalog = self.cross_seed_catalog(run.transfer.as_deref())?;
        if run.transfer.is_some() && catalog.order.is_empty() {
            return Err("Cross-seed discovery needs a completed ordinary transfer".into());
        }
        let mut reports = Vec::new();
        for (indexer, source) in sources {
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            reports.push(self.cross_seed_source(source, indexer, &catalog, run));
        }
        let mut report = Value::object();
        report.insert("dry_run", run.dry_run);
        report.insert("completed_transfers", catalog.order.len() as u32);
        report.insert("catalog_truncated", catalog.truncated);
        report.insert("indexers", Value::Array(reports));
        if !run.dry_run
            && let Ok(mut runtime) = lock(&self.cross_seed_runtime)
        {
            runtime.last_pass_at = Some(crate::store::now());
            runtime.last_report = Some(report.clone());
        }
        Ok(report)
    }

    fn cross_seed_source(
        &self,
        source: &Source,
        indexer: String,
        catalog: &Catalog,
        run: &Run,
    ) -> Value {
        let settings = source.options.cross_seed;
        let mut tally = Tally::default();
        let mut batch = Batch::default();
        let mut cx = Context {
            source,
            indexer: indexer.clone(),
            trigger: "feed",
            run,
            fetches: 0,
            last_request: None,
            nominated: BTreeSet::new(),
        };
        let now = Instant::now();
        let feed_due = settings.feed
            && run.transfer.is_none()
            && (run.force
                || lock(&self.cross_seed_runtime)
                    .ok()
                    .and_then(|runtime| runtime.next_feed.get(&indexer).copied())
                    .is_none_or(|at| at <= now));
        let mut feed = if feed_due {
            self.cross_seed_feed(&mut cx, catalog, &mut batch, &mut tally)
        } else {
            "not_due"
        };
        if feed_due && !run.dry_run {
            let wait = if feed == "ok" {
                Duration::from_secs(settings.interval_secs)
            } else {
                FAILURE_RETRY.min(Duration::from_secs(settings.interval_secs))
            };
            if let Ok(mut runtime) = lock(&self.cross_seed_runtime) {
                runtime.next_feed.insert(indexer.clone(), now + wait);
            }
        }
        let search = if settings.search && feed != "stopped" {
            cx.trigger = "completion";
            self.cross_seed_searches(&mut cx, catalog, &mut batch, &mut tally)
        } else {
            "disabled"
        };
        tally.fetched = cx.fetches as u32;
        let recorded: Vec<Value> = batch.outcomes.iter().map(Outcome::to_json).collect();
        if !run.dry_run
            && lock(&self.cross_seed_store)
                .and_then(|mut store| store.commit(batch))
                .is_err()
        {
            feed = "storage_failed";
        }
        let mut report = Value::object();
        report.insert("id", indexer);
        report.insert("feed", feed);
        report.insert("search", search);
        for (k, v) in tally.to_json().as_object().into_iter().flatten() {
            report.insert(k.as_str(), v.clone());
        }
        report.insert("outcomes", Value::Array(recorded));
        report
    }

    /// Reads the feed once and evaluates entries not yet remembered.
    fn cross_seed_feed(
        &self,
        cx: &mut Context,
        catalog: &Catalog,
        batch: &mut Batch,
        tally: &mut Tally,
    ) -> &'static str {
        let seen = match lock(&self.cross_seed_store).and_then(|mut store| store.seen(&cx.indexer))
        {
            Ok(seen) => seen,
            Err(_) => return "storage_failed",
        };
        cx.last_request = Some(Instant::now());
        let offers =
            match integrations::offers_before(cx.source, None, Instant::now() + REQUEST_BUDGET) {
                Ok(offers) => offers,
                Err(code) => return code,
            };
        let mut fresh = Vec::new();
        for offer in offers {
            if seen.contains(&offer.id) || fresh.iter().any(|known: &Offer| known.id == offer.id) {
                continue;
            }
            fresh.push(offer);
        }
        let mut remembered = Vec::new();
        for offer in &fresh {
            tally.offers += 1;
            let verdict = self.cross_seed_offer(cx, catalog, offer, None);
            match self.cross_seed_record(cx, verdict, batch, tally) {
                Some(true) => remembered.push(offer.id.clone()),
                Some(false) => {}
                None => {
                    if !cx.run.dry_run && !remembered.is_empty() {
                        batch.seen.insert(cx.indexer.clone(), remembered);
                    }
                    return "stopped";
                }
            }
        }
        if !cx.run.dry_run && !remembered.is_empty() {
            batch.seen.insert(cx.indexer.clone(), remembered);
        }
        "ok"
    }

    /// Searches by name for completed transfers this source has not searched,
    /// and a season pack's season for its single episodes.
    fn cross_seed_searches(
        &self,
        cx: &mut Context,
        catalog: &Catalog,
        batch: &mut Batch,
        tally: &mut Tally,
    ) -> &'static str {
        let interval = cx.source.options.cross_seed.search_interval_secs;
        let now = crate::store::now();
        let mut searches = 0;
        for (transfer, name) in &catalog.order {
            let Some(target) = catalog
                .by_name
                .get(name)
                .and_then(|group| group.iter().find(|s| &s.id == transfer))
            else {
                continue;
            };
            if !target.ready || target.paused {
                continue;
            }
            let mut queries = vec![(
                transfer.clone(),
                strip_video_extension(&target.name).replace(['.', '_'], " "),
            )];
            if let Some(query) = season_query(&target.name) {
                queries.push((format!("{transfer}#season"), query));
            }
            for (key, query) in queries {
                let pending = match lock(&self.cross_seed_store)
                    .and_then(|mut store| store.pending(&cx.indexer, &key))
                {
                    Ok(pending) => pending,
                    Err(_) => return "storage_failed",
                };
                if pending.is_none() && !cx.run.force {
                    let last = match lock(&self.cross_seed_store)
                        .and_then(|mut store| store.searched(&cx.indexer, &key))
                    {
                        Ok(last) => last,
                        Err(_) => return "storage_failed",
                    };
                    if last
                        .is_some_and(|at| interval.is_none_or(|secs| now < at.saturating_add(secs)))
                    {
                        continue;
                    }
                }
                if searches == MAX_SEARCHES {
                    return "pending";
                }
                searches += 1;
                let offers = if let Some(offers) = pending {
                    offers
                } else {
                    if !self.space(cx) {
                        return "pending";
                    }
                    cx.last_request = Some(Instant::now());
                    tally.searches += 1;
                    match integrations::offers_before(
                        cx.source,
                        Some(&query),
                        Instant::now() + REQUEST_BUDGET,
                    ) {
                        Ok(offers) => offers,
                        Err(code) => return code,
                    }
                };
                let only = BTreeSet::from([transfer.as_str()]);
                let (pending, stopped) = unfinished_offers(offers, |offer| {
                    let verdict = self.cross_seed_offer(cx, catalog, offer, Some(&only));
                    tally.offers += 1;
                    self.cross_seed_record(cx, verdict, batch, tally)
                });
                let unfinished = !pending.is_empty();
                if !cx.run.dry_run {
                    batch.finish_search(&cx.indexer, &key, pending, now);
                }
                if stopped {
                    return "stopped";
                }
                if unfinished {
                    return "pending";
                }
            }
        }
        "ok"
    }

    /// Waits for the source's minimum spacing; false if the budget is too short.
    fn space(&self, cx: &Context) -> bool {
        let spacing = Duration::from_millis(cx.source.options.min_interval_ms);
        if let Some(last) = cx.last_request {
            let ready = last + spacing;
            let now = Instant::now();
            if ready > now {
                if ready - now > REQUEST_BUDGET || self.stopped.load(Ordering::Acquire) {
                    return false;
                }
                thread::sleep(ready - now);
            }
        }
        true
    }

    /// Adds the verdict to the batch. `Some(true)` remembers the offer,
    /// `Some(false)` retries it later and `None` stops the source.
    fn cross_seed_record(
        &self,
        cx: &Context,
        verdict: Verdict,
        batch: &mut Batch,
        tally: &mut Tally,
    ) -> Option<bool> {
        match verdict {
            Verdict::Unmatched => {
                tally.unmatched += 1;
                Some(true)
            }
            Verdict::Deferred => {
                tally.deferred += 1;
                Some(false)
            }
            Verdict::SourceFailed => {
                tally.deferred += 1;
                None
            }
            Verdict::Done {
                result,
                reason,
                source,
                candidate,
            } => {
                match result {
                    "added" => tally.added += 1,
                    "would_add" => tally.would_add += 1,
                    "skipped" => tally.skipped += 1,
                    _ => tally.rejected += 1,
                }
                let at = crate::store::now();
                if result == "added"
                    && let (Some(source), Some(candidate)) = (&source, &candidate)
                {
                    batch.added.push(Provenance {
                        candidate_id: candidate.clone(),
                        source_transfer_id: source.clone(),
                        indexer: cx.indexer.clone(),
                        trigger: cx.trigger.into(),
                        at,
                    });
                }
                batch.outcomes.push(Outcome {
                    at,
                    indexer: cx.indexer.clone(),
                    trigger: cx.trigger.into(),
                    result: result.into(),
                    reason: reason.into(),
                    source_transfer_id: source,
                    candidate_id: candidate,
                });
                Some(true)
            }
        }
    }

    fn cross_seed_offer(
        &self,
        cx: &mut Context,
        catalog: &Catalog,
        offer: &Offer,
        only: Option<&BTreeSet<&str>>,
    ) -> Verdict {
        let done = |result, reason, source: Option<&str>, candidate: Option<&str>| Verdict::Done {
            result,
            reason,
            source: source.map(str::to_owned),
            candidate: candidate.map(str::to_owned),
        };
        // The same release by name, and a single episode by its local
        // season pack, whose matching file it may reuse.
        let named = catalog.by_name.get(&normalize(&offer.title));
        let packs = numbered(&offer.title)
            .filter(|n| n.episode.is_some())
            .and_then(|n| catalog.packs.get(&(n.series, n.season)));
        let mut matches: Vec<(&ReuseSource, bool)> =
            named.into_iter().flatten().map(|s| (s, false)).collect();
        for (name, id) in packs.into_iter().flatten() {
            if let Some(pack) = catalog
                .by_name
                .get(name)
                .and_then(|group| group.iter().find(|s| &s.id == id))
                && !matches.iter().any(|(s, _)| s.id == pack.id)
            {
                matches.push((pack, true));
            }
        }
        matches.retain(|(s, _)| only.is_none_or(|only| only.contains(s.id.as_str())));
        let Some((first, _)) = matches.first() else {
            return Verdict::Unmatched;
        };
        let limit = self.config.cross_seed_missing_bytes;
        let Ok(client) = self.native_client() else {
            return Verdict::SourceFailed;
        };
        if let Some(hash) = &offer.info_hash
            && (cx.nominated.contains(hash) || client.contains(hash).unwrap_or(false))
        {
            return done("skipped", "already_present", Some(&first.id), Some(hash));
        }
        // An episode is one file of its pack, plus small extras.
        let largest = matches
            .iter()
            .map(|(s, episode)| {
                if *episode {
                    s.files.iter().map(|file| file.2).max().unwrap_or(0)
                } else {
                    s.total
                }
            })
            .max()
            .unwrap_or(0);
        if offer
            .size
            .is_some_and(|size| size > largest.saturating_add(limit))
        {
            return done("skipped", "size_mismatch", Some(&first.id), None);
        }
        let usable: Vec<&ReuseSource> = matches
            .iter()
            .map(|(s, _)| *s)
            .filter(|s| s.ready && !s.paused)
            .collect();
        if usable.is_empty() {
            if matches.iter().all(|(s, _)| s.paused) {
                return done("skipped", "source_paused", Some(&first.id), None);
            }
            // Retained seeds verify again when they rotate into the active window.
            return Verdict::Deferred;
        }
        if cx.fetches == MAX_FETCHES || !self.space(cx) {
            return Verdict::Deferred;
        }
        cx.fetches += 1;
        cx.last_request = Some(Instant::now());
        let metadata = match integrations::indexer_metainfo_before(
            cx.source,
            &offer.url,
            Instant::now() + REQUEST_BUDGET,
        ) {
            Ok(metadata) => metadata,
            Err(error) => {
                return match error.as_str() {
                    "Indexer: invalid download" | "Indexer: response rejected" => {
                        done("rejected", "metainfo_unavailable", Some(&first.id), None)
                    }
                    _ => Verdict::SourceFailed,
                };
            }
        };
        let layout = match crate::torrent::candidate_layout(&metadata) {
            Ok(layout) => layout,
            Err(_) => return done("rejected", "unsupported_metadata", Some(&first.id), None),
        };
        if cx.nominated.contains(&layout.id) || client.contains(&layout.id).unwrap_or(false) {
            return done(
                "skipped",
                "already_present",
                Some(&first.id),
                Some(&layout.id),
            );
        }
        let mut reason = "layout_mismatch";
        for source in usable {
            let mapping = match map_files(source, &layout, limit) {
                Ok(mapping) => mapping,
                Err(why) => {
                    if why != "layout_mismatch" {
                        reason = why;
                    }
                    continue;
                }
            };
            cx.nominated.insert(layout.id.clone());
            if cx.run.dry_run {
                return done(
                    "would_add",
                    "verified_layout",
                    Some(&source.id),
                    Some(&layout.id),
                );
            }
            let request = CrossSeedRequest {
                metadata,
                mapping: Some(mapping),
                missing_limit: limit,
            };
            return match client.cross_seed(&source.id, &request) {
                Ok(_) => done(
                    "added",
                    "verification_queued",
                    Some(&source.id),
                    Some(&layout.id),
                ),
                Err(error) if error.contains("already exists") => done(
                    "skipped",
                    "already_present",
                    Some(&source.id),
                    Some(&layout.id),
                ),
                Err(error) if error.contains("fully verified") || error.contains("busy") => {
                    Verdict::Deferred
                }
                Err(_) => done(
                    "rejected",
                    "registration_refused",
                    Some(&source.id),
                    Some(&layout.id),
                ),
            };
        }
        done("rejected", reason, Some(&first.id), Some(&layout.id))
    }

    /// Configuration, schedule, counters and recent outcomes; no titles or URLs.
    pub fn cross_seed_status(&self) -> Result<Value> {
        let mut store = lock(&self.cross_seed_store)?;
        let runtime = lock(&self.cross_seed_runtime)?;
        let mut indexers = Vec::new();
        for (id, source) in self.cross_seed_sources(None) {
            let settings = source.options.cross_seed;
            let mut v = Value::object();
            v.insert("id", id.clone());
            v.insert("active", source.options.effective_enabled());
            v.insert("feed", settings.feed);
            v.insert(
                "interval_secs",
                Value::Number(settings.interval_secs as f64),
            );
            v.insert("search", settings.search);
            v.insert(
                "search_interval_secs",
                settings
                    .search_interval_secs
                    .map_or(Value::Null, |secs| Value::String(secs.to_string())),
            );
            v.insert("remembered_entries", store.seen(&id)?.len() as u32);
            v.insert(
                "next_feed_in_secs",
                runtime.next_feed.get(&id).map_or(Value::Null, |at| {
                    Value::String(
                        at.saturating_duration_since(Instant::now())
                            .as_secs()
                            .to_string(),
                    )
                }),
            );
            indexers.push(v);
        }
        let (added, total) = store.added_page(50)?;
        let mut report = Value::object();
        report.insert(
            "enabled",
            !indexers.is_empty() && self.config.downloads_enabled && self.config.downloads.seed,
        );
        report.insert("indexers", Value::Array(indexers));
        report.insert("counts", store.stats()?);
        report.insert("added_total", total.to_string());
        report.insert(
            "added",
            Value::Array(added.iter().map(Provenance::to_json).collect()),
        );
        report.insert(
            "recent",
            Value::Array(store.recent()?.iter().rev().map(Outcome::to_json).collect()),
        );
        report.insert(
            "last_pass_at",
            runtime
                .last_pass_at
                .map_or(Value::Null, |at| Value::String(at.to_string())),
        );
        report.insert(
            "last_pass",
            runtime.last_report.clone().unwrap_or(Value::Null),
        );
        Ok(report)
    }
}

pub(crate) fn start(engine: &Arc<Engine>, handles: &mut Vec<JoinHandle<()>>) {
    if engine.read_only
        || !engine.config.downloads_enabled
        || !engine.config.downloads.seed
        || engine.cross_seed_sources(None).is_empty()
    {
        return;
    }
    let engine = engine.clone();
    handles.push(thread::spawn(move || {
        let mut next = Instant::now();
        while !engine.stopped.load(Ordering::Acquire) {
            if Instant::now() >= next && !engine.starting.load(Ordering::Acquire) {
                let _ = engine.cross_seed_discover(&Run::default());
                next = Instant::now() + PASS_INTERVAL;
            }
            engine.wait(1000);
        }
    }));
}
