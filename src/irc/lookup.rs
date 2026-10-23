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
        || t.iter()
            .any(|s| episode_marker(s).is_none() && conflicting_marker(expected, s, 0))
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .map_err(|_| "IRC: cannot create the metainfo directory".into()),
        Err(_) => Err("IRC: metainfo directory is inaccessible".into()),
    }
}

impl Engine {
    /// The verified `.torrent` of a resolved release, downloaded once through the
    /// authenticated indexer. Its announce list stays in private transfer state;
    /// the download link is never kept.
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
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => return Ok(text),
            Ok(_) => return Err(unavailable()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(unavailable()),
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
            file.sync_all().map_err(|_| unavailable())?;
            let metadata = crate::torrent::inspect_metadata(
                &*self.torrent_network,
                part.to_str().ok_or_else(unavailable)?,
                deadline,
            )
            .map_err(|_| unavailable())?;
            if metadata.id != a.info_hash {
                return Err("IRC: metadata identity differs".to_owned());
            }
            std::fs::rename(&part, &path).map_err(|_| unavailable())
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
        while !engine.stopped.load(Ordering::Acquire) {
            let busy = engine
                .irc_resolve_pending()
                .is_ok_and(|r| r.get("outcome").and_then(Value::as_str) != Some("idle"));
            if !busy {
                engine.wait(1000);
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::{episode, info_hash};
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
