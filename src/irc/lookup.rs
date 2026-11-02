//! Bracketed tracker announcements carry only a source-local ID. They become
//! unverified candidates, and an authenticated Torznab lookup plus the TMDB
//! catalog supply the release identity before the ordinary history and gates.
use super::{Announcement, Source, format::formatting};
use crate::{
    Result,
    config::Config,
    engine::{Engine, lock},
    json::Value,
    numbering::{EpisodeNumber, SourceNumber},
    numbering::{conflicting_marker, episode_marker},
    selection::tokens,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, atomic::Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Unresolved candidates live only in memory: a restart drops them, never replays them.
pub(crate) const MAX_PENDING: usize = 64;
pub(crate) const MAX_ATTEMPTS: u32 = 3;
const RETRY: Duration = Duration::from_secs(30);
const BUDGET: Duration = Duration::from_secs(10);
const MAX_CATEGORIES: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lookup {
    pub indexer: String,
    pub details_prefix: String,
    pub categories: BTreeMap<String, String>,
}
impl Lookup {
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        super::only(
            v,
            &["type", "lookup_indexer", "details_prefix", "categories"],
        )?;
        let indexer = super::text(v, "lookup_indexer")?;
        let details_prefix = super::text(v, "details_prefix")?;
        let url = crate::net::parse_url(&details_prefix)?;
        let loopback = url
            .host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
        if !super::valid_id(&indexer)
            || details_prefix.len() > 256
            || !details_prefix.ends_with('/')
            || details_prefix.contains(['?', '#', '@', ' '])
            || !(details_prefix.starts_with("https://")
                || loopback && details_prefix.starts_with("http://"))
        {
            return Err("IRC: invalid announcement lookup".into());
        }
        let mut categories = BTreeMap::new();
        for (label, kind) in v
            .get("categories")
            .and_then(Value::as_object)
            .filter(|m| (1..=MAX_CATEGORIES).contains(&m.len()))
            .ok_or("IRC: announcement categories exceed bounds")?
        {
            let kind = kind
                .as_str()
                .filter(|k| matches!(*k, "movie" | "episode"))
                .ok_or("IRC: announcement category requires movie or episode")?;
            if !label_text(label) {
                return Err("IRC: invalid announcement category".into());
            }
            categories.insert(label.clone(), kind.to_owned());
        }
        Ok(Self {
            indexer,
            details_prefix,
            categories,
        })
    }
    pub(crate) fn configuration(&self) -> Value {
        let mut v = Value::object();
        v.insert("type", "bracketed");
        v.insert("lookup_indexer", self.indexer.clone());
        v.insert("details_prefix", self.details_prefix.clone());
        v.insert(
            "categories",
            Value::Object(
                self.categories
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(v.clone())))
                    .collect(),
            ),
        );
        v
    }
    /// Lines that do not open with a bracketed group are not announcements.
    pub(crate) fn payload(&self, line: &str) -> Result<Option<String>> {
        let clean = formatting(line)?;
        Ok(clean
            .trim_start()
            .starts_with('[')
            .then(|| clean.trim().to_owned()))
    }
    /// `[TAG] [category] [name] [source] [size] [N%]( [pre …])? par <uploader> -> <details>`
    pub(crate) fn candidate(&self, line: &str) -> Result<Candidate> {
        let mut rest = line.trim();
        let mut groups = Vec::new();
        while let Some(body) = rest.strip_prefix('[') {
            let (group, tail) = body
                .split_once(']')
                .ok_or("IRC: unterminated announcement field")?;
            if groups.len() == 7 || group.is_empty() || group.contains('[') {
                return Err("IRC: announcement fields do not match".into());
            }
            groups.push(group);
            rest = tail
                .strip_prefix(' ')
                .ok_or("IRC: announcement fields do not match")?;
        }
        let (uploader, details) = rest
            .strip_prefix("par ")
            .and_then(|r| r.split_once(" -> "))
            .ok_or("IRC: announcement fields do not match")?;
        if !(6..=7).contains(&groups.len())
            || (groups.len() == 7 && !groups[6].starts_with("pre "))
            || !groups[5].ends_with('%')
            || uploader.is_empty()
            || uploader.contains(" -> ")
        {
            return Err("IRC: announcement fields do not match".into());
        }
        let category = groups[1];
        let name = groups[2];
        let id = details
            .strip_prefix(&self.details_prefix)
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 19
                    && id.bytes().all(|b| b.is_ascii_digit())
                    && !id.starts_with('0')
            })
            .ok_or("IRC: announcement details page does not match")?;
        let lower = name.to_ascii_lowercase();
        if name.len() > 512
            || name.trim() != name
            || !label_text(category)
            || ["://", "magnet:", "passkey=", "token=", "apikey="]
                .iter()
                .any(|s| lower.contains(s))
        {
            return Err("IRC: invalid announced release".into());
        }
        let kind = self
            .categories
            .get(category)
            .ok_or("IRC: announcement category is not configured")?;
        let number = if kind == "episode" {
            Some(episode(name)?)
        } else {
            None
        };
        Ok(Candidate {
            title: name.into(),
            kind: kind.clone(),
            number,
            torrent_id: id.into(),
        })
    }
}
fn label_text(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.trim() == s
        && !s.chars().any(|c| c.is_control() || c == '[' || c == ']')
}
/// Exactly one season and episode marker; packs, multi-episode and bare `E12` labels fail.
fn episode(name: &str) -> Result<EpisodeNumber> {
    let t = tokens(name);
    let mut found = t.iter().filter_map(|s| episode_marker(s));
    let number = found
        .next()
        .filter(|_| found.next().is_none())
        .ok_or("IRC: announced episode numbering is missing or ambiguous")?;
    let expected = SourceNumber::SeasonEpisode(number);
    if number.episode == 0
        || (0..t.len())
            .any(|at| episode_marker(&t[at]).is_none() && conflicting_marker(expected, &t, at, 0))
    {
        return Err("IRC: announced episode numbering is missing or ambiguous".into());
    }
    Ok(number)
}
/// Torznab returns 40/64 hex digits, or (as some trackers do) the hex of those ASCII digits.
pub(crate) fn info_hash(text: &str) -> Option<String> {
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_hexdigit());
    let text = text.to_ascii_lowercase();
    match text.len() {
        40 | 64 if hex(&text) => Some(text),
        80 | 128 if hex(&text) => {
            let mut inner = String::with_capacity(text.len() / 2);
            for pair in text.as_bytes().chunks(2) {
                let byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
                if !(byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
                    return None;
                }
                inner.push(byte as char);
            }
            Some(inner)
        }
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub title: String,
    pub kind: String,
    pub number: Option<EpisodeNumber>,
    pub torrent_id: String,
}
impl Candidate {
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("title", self.title.clone());
        v.insert("kind", self.kind.clone());
        v.insert("torrent_id", self.torrent_id.clone());
        if let Some(n) = self.number {
            v.insert("season", n.season);
            v.insert("episode", n.episode);
        }
        v
    }
}

/// Exactly one item whose details page is the announced one, with one TMDB ID and
/// one decodable hash, then the catalog's canonical title and year.
pub(crate) fn resolve(
    config: &Config,
    lookup: &Lookup,
    c: &Candidate,
    deadline: Instant,
) -> std::result::Result<Announcement, &'static str> {
    let source = config
        .sources
        .iter()
        .find(|s| s.options.id.as_deref() == Some(&lookup.indexer))
        .ok_or("lookup_failed")?;
    let text = c.title.replace(['.', '_'], " ");
    let entries = crate::integrations::details_before(source, &text, deadline)
        .map_err(|_| "lookup_failed")?;
    let details = format!("{}{}", lookup.details_prefix, c.torrent_id);
    let mut found = entries.iter().filter(|e| e.details == details);
    let entry = match (found.next(), found.next()) {
        (Some(e), None) => e,
        (None, _) => return Err("not_found"),
        _ => return Err("ambiguous"),
    };
    let ([tmdb], [hash]) = (entry.tmdb_ids.as_slice(), entry.info_hashes.as_slice()) else {
        return Err("identity_rejected");
    };
    let tmdb_id = tmdb
        .parse::<u64>()
        .ok()
        .filter(|n| *n != 0 && n.to_string() == *tmdb)
        .ok_or("identity_rejected")?;
    let info_hash = info_hash(hash).ok_or("identity_rejected")?;
    let (media_title, year) =
        crate::integrations::catalog_canonical_before(config, &c.kind, tmdb_id, deadline)
            .map_err(|_| "catalog_unavailable")?;
    let mut v = Value::object();
    v.insert("title", c.title.clone());
    v.insert("kind", c.kind.clone());
    v.insert("media_title", media_title);
    v.insert("year", year);
    v.insert("tmdb_id", tmdb_id.to_string());
    v.insert("info_hash", info_hash);
    if let Some(n) = c.number {
        v.insert("season", n.season);
        v.insert("episode", n.episode);
    }
    Announcement::from_json(&v).map_err(|_| "identity_rejected")
}

pub(crate) struct Pending {
    source_id: String,
    candidate: Candidate,
    attempts: u32,
    due: Instant,
}
#[derive(Default)]
pub(crate) struct Queue {
    pending: VecDeque<Pending>,
    pub(crate) dropped: u64,
    pub(crate) failed: u64,
    pub(crate) resolved: u64,
}
impl Queue {
    /// Returns false when the candidate is already waiting or the queue is full.
    pub(crate) fn push(&mut self, source_id: &str, candidate: Candidate) -> bool {
        if self
            .pending
            .iter()
            .any(|p| p.source_id == source_id && p.candidate.torrent_id == candidate.torrent_id)
        {
            return false;
        }
        if self.pending.len() == MAX_PENDING {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.pending.push_back(Pending {
            source_id: source_id.into(),
            candidate,
            attempts: 0,
            due: Instant::now(),
        });
        true
    }
    fn take(&mut self, now: Instant) -> Option<Pending> {
        let i = self.pending.iter().position(|p| p.due <= now)?;
        self.pending.remove(i)
    }
    pub(crate) fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("pending", self.pending.len() as u32);
        v.insert("limit", MAX_PENDING as u32);
        for (k, n) in [
            ("resolved", self.resolved),
            ("failed", self.failed),
            ("dropped", self.dropped),
        ] {
            v.insert(k, n.to_string());
        }
        v
    }
}

impl Source {
    pub(crate) fn lookup_candidate(&self, line: &str) -> Result<Option<Candidate>> {
        let Some(lookup) = &self.announcement_lookup else {
            return Ok(None);
        };
        lookup.candidate(line).map(Some)
    }
}

impl Engine {
    /// Queue an unverified candidate from the configured sender and channel.
    pub(crate) fn irc_queue_candidate(
        &self,
        source: &Source,
        candidate: Candidate,
    ) -> Result<Value> {
        let _publication = self.source_publication(source.protection)?;
        let queued = lock(&self.irc_lookup)?.push(&source.id, candidate.clone());
        let mut v = Value::object();
        v.insert("source_id", source.id.clone());
        v.insert("candidate", candidate.to_json());
        v.insert("queued", queued);
        v.insert("identity_verified", false);
        v.insert("acquisition_started", false);
        Ok(v)
    }
    /// Resolve one due candidate outside every store lock. A failure retries a
    /// bounded number of times; the report carries only fixed outcome codes.
    pub fn irc_resolve_pending(&self) -> Result<Value> {
        let mut report = Value::object();
        let Some(mut p) = lock(&self.irc_lookup)?.take(Instant::now()) else {
            report.insert("outcome", "idle");
            return Ok(report);
        };
        let source = self.config.irc.source(&p.source_id)?;
        let lookup = source
            .announcement_lookup
            .as_ref()
            .ok_or("IRC: source has no announcement lookup")?;
        report.insert("candidate", p.candidate.to_json());
        let resolved = resolve(&self.config, lookup, &p.candidate, Instant::now() + BUDGET);
        let mut queue = lock(&self.irc_lookup)?;
        match resolved {
            Ok(a) => {
                queue.resolved = queue.resolved.saturating_add(1);
                drop(queue);
                let record =
                    self.irc_receive(&source.id, &source.sender, &source.channel, &a.to_json())?;
                let duplicate = record.get("duplicate").and_then(Value::as_bool) == Some(true);
                report.insert("outcome", if duplicate { "duplicate" } else { "recorded" });
                report.insert(
                    "announcement_id",
                    record.get("id").cloned().unwrap_or(Value::Null),
                );
            }
            Err(outcome) => {
                p.attempts += 1;
                // A release can be announced before the indexer lists it.
                let retry = matches!(
                    outcome,
                    "lookup_failed" | "not_found" | "catalog_unavailable"
                ) && p.attempts < MAX_ATTEMPTS
                    && !self.stopped.load(Ordering::Acquire);
                if retry {
                    p.due = Instant::now() + RETRY;
                    queue.pending.push_back(p);
                } else {
                    queue.failed = queue.failed.saturating_add(1);
                }
                report.insert("outcome", outcome);
                report.insert("retrying", retry);
            }
        }
        Ok(report)
    }
}

/// Private `.torrent` files fetched for routed candidates; never a public path.
pub(crate) const METAINFO_DIR: &str = "irc-metainfo";

/// A routed metainfo is `<store>/irc-metainfo/<hash>.torrent`; it holds no link.
pub(crate) fn metainfo_path(s: &str, hash: &str) -> bool {
    let path = std::path::Path::new(s);
    path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        && path.file_name().and_then(|n| n.to_str()) == Some(&format!("{hash}.torrent"))
        && path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some(METAINFO_DIR)
}

fn private_directory(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() && m.permissions().mode() & 0o077 == 0 => Ok(()),
        Ok(_) => Err("IRC: metainfo directory is not private".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .map_err(|_| "IRC: cannot create the metainfo directory")?;
            match dir.parent() {
                Some(parent) => crate::store::sync_directory(parent),
                None => Ok(()),
            }
        }
        Err(_) => Err("IRC: metainfo directory is inaccessible".into()),
    }
}

impl Engine {
    /// The verified `.torrent` of a resolved release, downloaded once through the
    /// authenticated indexer. Its announce list stays in private transfer state;
    /// the download link is never kept.
    /// The routing-pass lock must remain held through reuse, fetch and route
    /// publication; cleanup takes the same lock before inspecting these files.
    pub(crate) fn irc_metainfo(
        &self,
        lookup: &Lookup,
        a: &Announcement,
        deadline: Instant,
    ) -> Result<String> {
        let unavailable = || "IRC: metadata unavailable or unauthenticated".to_owned();
        let dir = self.config.store_dir.join(METAINFO_DIR);
        private_directory(&dir)?;
        let path = dir.join(format!("{}.torrent", a.info_hash));
        let text = path.to_str().ok_or_else(unavailable)?.to_owned();
        if refreshed(&path).map_err(|_| unavailable())? {
            return Ok(text);
        }
        let source = self
            .config
            .sources
            .iter()
            .find(|s| s.options.id.as_deref() == Some(&lookup.indexer))
            .ok_or_else(unavailable)?;
        let entries = crate::integrations::details_before(
            source,
            &a.title.replace(['.', '_'], " "),
            deadline,
        )
        .map_err(|_| unavailable())?;
        let mut found = entries.iter().filter(|e| {
            e.details.starts_with(&lookup.details_prefix)
                && matches!(e.info_hashes.as_slice(), [h] if info_hash(h).as_deref() == Some(&a.info_hash))
        });
        let url = match (found.next(), found.next()) {
            (Some(e), None) => e.enclosure.clone().ok_or_else(unavailable)?,
            _ => return Err(unavailable()),
        };
        // The search and the download share the source's minimum spacing.
        let spacing = Duration::from_millis(source.options.min_interval_ms);
        if Instant::now() + spacing >= deadline {
            return Err(unavailable());
        }
        thread::sleep(spacing);
        let body = crate::integrations::indexer_metainfo_before(source, &url, deadline)
            .map_err(|_| unavailable())?;
        let part = dir.join(format!("{}.part", a.info_hash));
        let _ = std::fs::remove_file(&part);
        let written = (|| {
            use std::io::Write;
            let mut file = crate::store::private_options()
                .write(true)
                .create_new(true)
                .open(&part)
                .map_err(|_| unavailable())?;
            file.write_all(&body).map_err(|_| unavailable())?;
            crate::store::sync_file(&file).map_err(|_| unavailable())?;
            let metadata = crate::torrent::inspect_metadata(
                &*self.torrent_network,
                part.to_str().ok_or_else(unavailable)?,
                deadline,
            )
            .map_err(|_| unavailable())?;
            if metadata.id != a.info_hash {
                return Err("IRC: metadata identity differs".to_owned());
            }
            std::fs::rename(&part, &path).map_err(|_| unavailable())?;
            // The route recorded next names this file: its name must survive
            // a power loss first.
            crate::store::sync_directory(&dir)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        written.map(|()| text)
    }
}

pub(crate) fn start(engine: &Arc<Engine>, handles: &mut Vec<JoinHandle<()>>) {
    if engine.read_only
        || !engine
            .config
            .irc
            .sources
            .iter()
            .any(|s| s.enabled && s.announcement_lookup.is_some())
    {
        return;
    }
    let engine = engine.clone();
    handles.push(thread::spawn(move || {
        let mut pruned: Option<Instant> = None;
        while !engine.stopped.load(Ordering::Acquire) {
            if pruned.is_none_or(|at| at.elapsed() >= PRUNE_EVERY) {
                pruned = Some(Instant::now());
                if let Err(error) = engine.prune_metainfo(std::time::SystemTime::now()) {
                    eprintln!("mynou: IRC metainfo cleanup stopped: {error}");
                }
            }
            let busy = engine
                .irc_resolve_pending()
                .is_ok_and(|r| r.get("outcome").and_then(Value::as_str) != Some("idle"));
            if !busy {
                engine.wait(1000);
            }
        }
    }));
}

/// Whether a fetched file is there to reuse, after making it new again: the
/// cleanup leaves files younger than `KEEP_AT_LEAST`, so a file still there
/// after its refresh stays until the route that reuses it is recorded.
fn refreshed(path: &std::path::Path) -> std::io::Result<bool> {
    let present = |path| match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Ok(true),
        Ok(_) => Err(std::io::Error::other("not a file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    };
    if !present(path)? {
        return Ok(false);
    }
    match std::fs::File::open(path) {
        Ok(file) => file.set_modified(std::time::SystemTime::now())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    }
    // Removed between the open and the refresh: fetch it again.
    present(path)
}

/// How often fetched metainfo no job needs any more is removed.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
/// A fetched or reused file is kept at least this long, so a route being
/// recorded for it is never raced.
const KEEP_AT_LEAST: Duration = Duration::from_secs(24 * 3600);

impl Engine {
    /// Removes fetched `.torrent` files untouched for a day that no job may
    /// still acquire from, and abandoned partial downloads. Returns how many
    /// files were removed.
    pub(crate) fn prune_metainfo(&self, now: std::time::SystemTime) -> Result<usize> {
        // A route may refresh an old file or be writing its partial download.
        // Serialize the complete use/publication with cleanup, not just the
        // final job-store check: an mtime check alone races a later refresh.
        let _route = lock(&self.irc_route_lock)?;
        let dir = self.config.store_dir.join(METAINFO_DIR);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(_) => return Err("IRC: metainfo directory is inaccessible".into()),
        };
        private_directory(&dir)?;
        let mut removed = 0;
        for entry in entries {
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            let entry = entry.map_err(|_| "IRC: cannot list the metainfo directory")?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some((hash, extension)) = name.rsplit_once('.') else {
                continue;
            };
            if !matches!(extension, "torrent" | "part") || info_hash(hash).as_deref() != Some(hash)
            {
                continue;
            }
            let Some(url) = path.to_str() else {
                continue;
            };
            let old = |path: &std::path::Path| {
                std::fs::symlink_metadata(path).is_ok_and(|metadata| {
                    metadata.is_file()
                        && metadata
                            .modified()
                            .ok()
                            .and_then(|at| now.duration_since(at).ok())
                            .is_some_and(|age| age >= KEEP_AT_LEAST)
                })
            };
            // Recent files stay without taking the store lock.
            if !old(&path) {
                continue;
            }
            // The age, the owners and the removal are then decided under the
            // store lock, which recording a route also takes: a file reused
            // before this check is new again, and one reused after it is
            // fetched again.
            let mut store = lock(&self.store)?;
            if !old(&path) || extension == "torrent" && store.acquisition_needed(url)? {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
            drop(store);
        }
        if removed > 0 {
            crate::store::sync_directory(&dir)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::{episode, info_hash, refreshed};

    #[test]
    fn metainfo_prune_waits_for_routing_refresh_and_publication() {
        use crate::{
            config,
            engine::{Engine, lock},
            pack::tunnel_tests::Directory,
        };
        use std::{
            sync::mpsc,
            thread,
            time::{Duration, SystemTime},
        };
        let directory = Directory::new();
        let cfg = config::from_json(&config::default_json(), &directory.0).unwrap();
        let engine = Engine::open_for_management(cfg).unwrap();
        let dir = engine.config.store_dir.join(super::METAINFO_DIR);
        super::private_directory(&dir).unwrap();
        let path = dir.join(format!("{}.torrent", "1".repeat(40)));
        let part = dir.join(format!("{}.part", "2".repeat(40)));
        let obsolete = dir.join(format!("{}.torrent", "3".repeat(40)));
        let now = SystemTime::now();
        for path in [&path, &part, &obsolete] {
            std::fs::File::create(path)
                .unwrap()
                .set_modified(now - Duration::from_secs(3 * 86_400))
                .unwrap();
        }
        // Routing owns this lock from the cache refresh through the durable
        // route publication, including work on its partial metainfo file.
        let route = lock(&engine.irc_route_lock).unwrap();
        let (started, starting) = mpsc::channel();
        let (finished, pending) = mpsc::channel();
        let pruning = engine.clone();
        let worker = thread::spawn(move || {
            started.send(()).unwrap();
            finished.send(pruning.prune_metainfo(now)).unwrap();
        });
        starting.recv().unwrap();
        let while_routing = pending.recv_timeout(Duration::from_millis(100));
        let reused = refreshed(&path).unwrap();
        std::fs::write(&part, b"refreshed partial metainfo").unwrap();
        drop(route);
        worker.join().unwrap();
        assert!(
            matches!(while_routing, Err(mpsc::RecvTimeoutError::Timeout)),
            "pruning must wait until the route finishes using its files"
        );
        assert!(reused);
        assert_eq!(pending.recv().unwrap().unwrap(), 1);
        assert!(path.is_file() && part.is_file());
        assert!(!obsolete.exists());
    }

    #[test]
    fn a_reused_metainfo_file_is_new_again_and_a_removed_one_is_fetched() {
        let dir = std::env::temp_dir().join(format!(
            "mynou-refreshed-{}",
            crate::crypto::random_bytes::<8>()
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("old.torrent");
        let file = std::fs::File::create(&path).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 86_400);
        file.set_modified(old).unwrap();
        drop(file);
        assert!(refreshed(&path).unwrap());
        let age = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap();
        assert!(age < std::time::Duration::from_secs(3600));
        std::fs::remove_file(&path).unwrap();
        assert!(!refreshed(&path).unwrap());
        std::fs::create_dir(&path).unwrap();
        assert!(refreshed(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    use crate::store::Request;

    #[test]
    fn hashes_decode_once_and_strictly() {
        let hash = "a0b8304fc89c2a924f18d6fb9b1be593bec3df9e";
        let doubled: String = hash.bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(info_hash(hash).as_deref(), Some(hash));
        assert_eq!(info_hash(&hash.to_uppercase()).as_deref(), Some(hash));
        assert_eq!(info_hash(&doubled).as_deref(), Some(hash));
        assert_eq!(info_hash(&doubled.to_uppercase()).as_deref(), Some(hash));
        let nested: String = doubled.bytes().map(|b| format!("{b:02x}")).collect();
        for bad in [
            "",
            &hash[1..],
            &"g".repeat(40),
            &"47".repeat(40),
            &"2f".repeat(40),
            &nested,
        ] {
            assert_eq!(info_hash(bad), None, "{bad}");
        }
    }

    #[test]
    fn episode_labels_need_one_number_and_may_omit_the_series_year() {
        assert_eq!(episode("Show.S01E05.1080p").unwrap().episode, 5);
        for bad in [
            "Show.E12.1080p",
            "Show.S06.1080p",
            "Show.S01E01E02",
            "Show.S01E00",
        ] {
            assert!(episode(bad).is_err(), "{bad}");
        }
        let request = Request {
            kind: "episode".into(),
            title: "Fixture Show".into(),
            year: 2022,
            season: 6,
            episode: 6,
            tmdb_id: Some(95480),
            source_path: None,
            source_url: None,
            source_numbering: None,
        };
        let matches = |t| super::super::routing::title_matches(&request, t);
        assert!(matches(
            "Fixture.Show.S06E06.FiNAL.MULTi.1080p.WEB.H264-GROUP"
        ));
        assert!(matches("Fixture.Show.2022.S06E06.1080p"));
        assert!(!matches("Fixture.Show.2005.S06E06.1080p"));
        assert!(!matches("Fixture.Show.S06E07.1080p"));
        let absolute = Request {
            source_numbering: Some(crate::numbering::SourceNumber::Absolute(13)),
            ..request.clone()
        };
        for title in [
            "Fixture.Show.013.1080p.WEB.H.264.AAC2.0-GRP",
            "Fixture.Show.2022.013.DDP5.1.H.265",
            "Fixture.Show.013.DD+7.1.AAC.2.0",
        ] {
            assert!(
                super::super::routing::title_matches(&absolute, title),
                "{title}"
            );
        }
        for title in [
            "Fixture.Show.013-014.H.264",
            "Fixture.Show.013.013.AAC2.0",
            "Fixture.Show.0013-2200.H.265",
        ] {
            assert!(
                !super::super::routing::title_matches(&absolute, title),
                "{title}"
            );
        }
        let movie = Request {
            kind: "movie".into(),
            season: 0,
            episode: 0,
            ..request
        };
        assert!(!super::super::routing::title_matches(
            &movie,
            "Fixture.Show.1080p"
        ));
    }
}
