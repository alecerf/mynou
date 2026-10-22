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
    let name = name.trim();
    let stem = match name.rsplit_once('.') {
        Some((stem, extension))
            if VIDEO_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known)) =>
        {
            stem
        }
        _ => name,
    };
    stem.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Original source indices for each candidate file, in candidate order. Equal
/// lengths are required; file names, then paths below the root, break ties.
/// Anything still ambiguous is refused rather than guessed.
pub(crate) fn map_files(
    source: &ReuseSource,
    candidate: &CandidateLayout,
) -> std::result::Result<Vec<usize>, &'static str> {
    let below_root = |path: &Path| path.components().skip(1).collect::<std::path::PathBuf>();
    let mut used = BTreeSet::new();
    let mut mapping = Vec::with_capacity(candidate.files.len());
    for (path, length) in &candidate.files {
        let sized: Vec<_> = source
            .files
            .iter()
            .filter(|file| file.2 == *length && !used.contains(&file.0))
            .collect();
        let pick = match sized.as_slice() {
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
        mapping.push(pick.0);
    }
    if mapping.is_empty() {
        return Err("layout_mismatch");
    }
    Ok(mapping)
}

/// Completed transfers grouped by normalized name.
struct Catalog {
    by_name: BTreeMap<String, Vec<ReuseSource>>,
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

    /// Searches by name for completed transfers this source has not searched.
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
            if !cx.run.force {
                let last = match lock(&self.cross_seed_store)
                    .and_then(|mut store| store.searched(&cx.indexer, transfer))
                {
                    Ok(last) => last,
                    Err(_) => return "storage_failed",
                };
                if last.is_some_and(|at| interval.is_none_or(|secs| now < at.saturating_add(secs)))
                {
                    continue;
                }
            }
            if searches == MAX_SEARCHES {
                return "pending";
            }
            searches += 1;
            tally.searches += 1;
            if !self.space(cx) {
                return "pending";
            }
            let query = target
                .name
                .rsplit_once('.')
                .filter(|(_, ext)| VIDEO_EXTENSIONS.iter().any(|k| ext.eq_ignore_ascii_case(k)))
                .map_or(target.name.as_str(), |(stem, _)| stem)
                .replace(['.', '_'], " ");
            cx.last_request = Some(Instant::now());
            let offers = match integrations::offers_before(
                cx.source,
                Some(&query),
                Instant::now() + REQUEST_BUDGET,
            ) {
                Ok(offers) => offers,
                Err(code) => return code,
            };
            let only = BTreeSet::from([transfer.as_str()]);
            for offer in &offers {
                let verdict = self.cross_seed_offer(cx, catalog, offer, Some(&only));
                tally.offers += 1;
                if self.cross_seed_record(cx, verdict, batch, tally).is_none() {
                    return "stopped";
                }
            }
            if !cx.run.dry_run {
                batch
                    .searched
                    .push((cx.indexer.clone(), transfer.clone(), now));
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
        let Some(group) = catalog.by_name.get(&normalize(&offer.title)) else {
            return Verdict::Unmatched;
        };
        let matches: Vec<&ReuseSource> = group
            .iter()
            .filter(|s| only.is_none_or(|only| only.contains(s.id.as_str())))
            .collect();
        let Some(first) = matches.first() else {
            return Verdict::Unmatched;
        };
        let Ok(client) = self.native_client() else {
            return Verdict::SourceFailed;
        };
        if let Some(hash) = &offer.info_hash
            && (cx.nominated.contains(hash) || client.contains(hash).unwrap_or(false))
        {
            return done("skipped", "already_present", Some(&first.id), Some(hash));
        }
        let largest = matches.iter().map(|s| s.total).max().unwrap_or(0);
        if offer.size.is_some_and(|size| size > largest) {
            return done("skipped", "size_mismatch", Some(&first.id), None);
        }
        let usable: Vec<&&ReuseSource> = matches.iter().filter(|s| s.ready && !s.paused).collect();
        if usable.is_empty() {
            if matches.iter().all(|s| s.paused) {
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
            let mapping = match map_files(source, &layout) {
                Ok(mapping) => mapping,
                Err(why) => {
                    if why == "ambiguous_layout" {
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
