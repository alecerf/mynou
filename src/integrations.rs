//! Plex, TMDB and indexer integrations without external dependencies.
//! Responses and XML are bounded; errors never contain a download URL
//! or an API key.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::Result;
use crate::config::{self, Config, Source};
use crate::crypto::Sha256;
use crate::indexers::mapping::Mapping;
use crate::json::{self, Value};
use crate::net::{self, HttpClient};
use crate::selection::{Assessment, Profile, tokens};
use crate::store::{RecordedRelease, Request};
use crate::xml::{MAX_XML, parse_xml};

/// Items of one parsed response or one catalog plan; never an inventory total.
const MAX_ITEMS: usize = 100_000;
/// Items requested per Plex page. Listings are read page by page under a
/// deadline, so no total item or page count applies.
const PAGE_SIZE: usize = 200;
const SEARCH_BUDGET: Duration = Duration::from_secs(90);
const SEARCH_EXPIRED: &str = "Search: indexer search exceeded its time budget";
/// The outcome of a requester account poll that ran out of time, before or
/// during a Plex request.
pub(crate) const PLEX_POLL_DEADLINE: &str = "Plex account poll deadline reached";
const CATALOG_CACHE_TTL: Duration = Duration::from_secs(3_600);
const CATALOG_CACHE_BYTES: usize = 32 * 1024 * 1024;
const CATALOG_CACHE_ENTRIES: usize = 256;

struct CatalogEntry {
    value: Arc<Value>,
    inserted: Instant,
    payload_bytes: usize,
}

/// Only TMDB JSON data is retained. Keys are SHA-256 hashes of length-prefixed
/// URLs and headers; no URL or API key is stored here.
#[derive(Default)]
struct CatalogCache {
    entries: BTreeMap<[u8; 32], CatalogEntry>,
    insertion_order: VecDeque<[u8; 32]>,
    payload_bytes: usize,
}

impl CatalogCache {
    fn remove(&mut self, key: &[u8; 32]) {
        if let Some(entry) = self.entries.remove(key) {
            self.payload_bytes -= entry.payload_bytes;
        }
        self.insertion_order.retain(|existing| existing != key);
    }

    fn expire(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(key, entry)| {
                (now.saturating_duration_since(entry.inserted) >= CATALOG_CACHE_TTL).then_some(*key)
            })
            .collect();
        for key in expired {
            self.remove(&key);
        }
    }

    fn lookup(&mut self, key: &[u8; 32], now: Instant) -> Option<Arc<Value>> {
        self.expire(now);
        self.entries.get(key).map(|entry| Arc::clone(&entry.value))
    }

    fn insert(&mut self, key: [u8; 32], value: Arc<Value>, payload_bytes: usize, now: Instant) {
        self.expire(now);
        self.remove(&key);
        if payload_bytes > CATALOG_CACHE_BYTES {
            return;
        }
        while self.payload_bytes + payload_bytes > CATALOG_CACHE_BYTES
            || self.entries.len() >= CATALOG_CACHE_ENTRIES
        {
            let Some(oldest) = self.insertion_order.front().copied() else {
                return;
            };
            self.remove(&oldest);
        }
        self.payload_bytes += payload_bytes;
        self.entries.insert(
            key,
            CatalogEntry {
                value,
                inserted: now,
                payload_bytes,
            },
        );
        self.insertion_order.push_back(key);
    }
}

fn catalog_cache() -> &'static Mutex<CatalogCache> {
    static CACHE: OnceLock<Mutex<CatalogCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(CatalogCache::default()))
}

fn catalog_cache_key(url: &str, headers: &[(String, String)]) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in std::iter::once(url).chain(
        headers
            .iter()
            .flat_map(|(name, value)| [name.as_str(), value.as_str()]),
    ) {
        hash.update(&(part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize()
}

fn client() -> HttpClient {
    HttpClient::new()
        .with_timeout(Duration::from_secs(20))
        .with_max_body(MAX_XML)
}

fn percent(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 15) as usize] as char);
        }
    }
    out
}

fn query(url: &str, pairs: &[(&str, String)]) -> Result<String> {
    let mut parsed = net::parse_url(url)?;
    for (key, value) in pairs {
        parsed
            .path
            .push(if parsed.path.contains('?') { '&' } else { '?' });
        parsed.path.push_str(&percent(key));
        parsed.path.push('=');
        parsed.path.push_str(&percent(value));
    }
    let out = parsed.as_string();
    net::parse_url(&out)?;
    Ok(out)
}

fn endpoint(base: &str, path: &str) -> Result<String> {
    let mut parsed = net::parse_url(base)?;
    if parsed.path.contains('?') {
        return Err("Service URL: query parameters are forbidden in the base URL".into());
    }
    parsed.path = format!(
        "{}/{}",
        parsed.path.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    Ok(parsed.as_string())
}

fn fetch_json_sized(
    url: &str,
    headers: &[(String, String)],
    service: &str,
) -> Result<(Value, usize)> {
    let response = client()
        .request("GET", url, headers, &[])
        .map_err(|_| format!("{service}: network request failed"))?;
    if !(200..300).contains(&response.status) {
        return Err(format!("{service}: HTTP response {}", response.status));
    }
    let text =
        std::str::from_utf8(&response.body).map_err(|_| format!("{service}: invalid UTF-8"))?;
    let value = json::parse(text).map_err(|_| format!("{service}: invalid JSON response"))?;
    Ok((value, response.body.len()))
}

fn string<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

fn integer(v: &Value, key: &str) -> Option<u64> {
    let value = v.get(key)?;
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn plex_headers(config: &Config) -> Result<Vec<(String, String)>> {
    Ok(vec![
        ("Accept".into(), "application/json".into()),
        (
            "X-Plex-Token".into(),
            config
                .plex
                .token_override
                .clone()
                .map_or_else(|| config::secret(&config.plex.token_env), Ok)?,
        ),
        ("X-Plex-Client-Identifier".into(), "mynou-rust-std".into()),
    ])
}

fn metadata_page(value: &Value) -> Result<(&[Value], Option<u64>)> {
    let container = value
        .get("MediaContainer")
        .ok_or("Plex: missing MediaContainer")?;
    if container.as_object().is_none() {
        return Err("Plex: invalid MediaContainer".into());
    }
    let items = match container.get("Metadata") {
        Some(v) => v.as_array().ok_or("Plex: invalid Metadata")?,
        None if integer(container, "size") == Some(0) => &[],
        None => return Err("Plex: missing Metadata".into()),
    };
    Ok((items, integer(container, "totalSize")))
}

/// One Plex listing page read from an absolute item offset.
fn plex_page_before(
    url: &str,
    headers: &[(String, String)],
    start: u64,
    deadline: Instant,
    expired: &str,
) -> Result<(Vec<Value>, Option<u64>)> {
    let url = query(
        url,
        &[
            ("X-Plex-Container-Start", start.to_string()),
            ("X-Plex-Container-Size", PAGE_SIZE.to_string()),
            ("includeGuids", "1".into()),
        ],
    )?;
    let value = fetch_json_before(&url, headers, deadline, expired)?;
    let (items, total) = metadata_page(&value)?;
    Ok((items.to_vec(), total))
}

/// Whether a page ends the listing: the reported total is reached, or a short
/// page ends a listing without a total.
fn last_page(end: u64, total: Option<u64>, returned: usize) -> bool {
    total.map_or(returned < PAGE_SIZE, |total| end >= total)
}

/// Visits a Plex listing one page at a time until `visit` returns true. Only
/// one page is held; the deadline, not an item or page count, ends a listing
/// that keeps growing.
fn plex_find_before(
    url: &str,
    headers: &[(String, String)],
    deadline: Instant,
    mut visit: impl FnMut(&Value) -> Result<bool>,
) -> Result<bool> {
    let mut start = 0_u64;
    let mut previous: Option<Value> = None;
    loop {
        let (items, total) = plex_page_before(url, headers, start, deadline, SEARCH_EXPIRED)?;
        if items.is_empty() {
            if total.is_some_and(|n| n > start) {
                return Err("Plex: incomplete pagination".into());
            }
            return Ok(false);
        }
        if previous.is_some() && previous.as_ref() == items.last() {
            return Err("Plex: pagination ignored by the server".into());
        }
        for item in &items {
            if visit(item)? {
                return Ok(true);
            }
        }
        start = start
            .checked_add(items.len() as u64)
            .ok_or("Plex: item offset overflow")?;
        if last_page(start, total, items.len()) {
            return Ok(false);
        }
        previous = items.into_iter().last();
    }
}

fn tmdb_guid(item: &Value) -> Option<u64> {
    fn decode(text: &str) -> Option<u64> {
        let id = text.strip_prefix("tmdb://")?.split(['?', '/']).next()?;
        id.parse().ok().filter(|n| *n != 0)
    }
    string(item, "guid").and_then(decode).or_else(|| {
        item.get("Guid")?
            .as_array()?
            .iter()
            .find_map(|v| string(v, "id").and_then(decode))
    })
}

/// Durable position of a watchlist sweep. `anchor` identifies the item just
/// before `position`; the next page re-reads it to detect a shifted listing.
/// A sweep is `consistent` while no shift or total change was observed, and
/// only a consistent complete sweep may prove that an item left the list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepCursor {
    pub position: u64,
    pub total: Option<u64>,
    pub anchor: Option<[u8; 32]>,
    pub consistent: bool,
}

impl SweepCursor {
    pub const ENCODED_LEN: usize = 51;

    /// The start of a new sweep.
    pub fn start() -> Self {
        Self {
            consistent: true,
            ..Self::default()
        }
    }

    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut bytes = [0; Self::ENCODED_LEN];
        bytes[..8].copy_from_slice(&self.position.to_be_bytes());
        if let Some(total) = self.total {
            bytes[8] = 1;
            bytes[9..17].copy_from_slice(&total.to_be_bytes());
        }
        if let Some(anchor) = self.anchor {
            bytes[17] = 1;
            bytes[18..50].copy_from_slice(&anchor);
        }
        bytes[50] = u8::from(self.consistent);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let invalid = || "Watchlist: invalid sweep cursor".to_owned();
        if bytes.len() != Self::ENCODED_LEN
            || bytes[8] > 1
            || bytes[17] > 1
            || bytes[50] > 1
            || (bytes[8] == 0 && bytes[9..17].iter().any(|b| *b != 0))
            || (bytes[17] == 0 && bytes[18..50].iter().any(|b| *b != 0))
        {
            return Err(invalid());
        }
        let cursor = Self {
            position: u64::from_be_bytes(bytes[..8].try_into().map_err(|_| invalid())?),
            total: (bytes[8] == 1)
                .then(|| bytes[9..17].try_into().map(u64::from_be_bytes))
                .transpose()
                .map_err(|_| invalid())?,
            anchor: (bytes[17] == 1)
                .then(|| bytes[18..50].try_into())
                .transpose()
                .map_err(|_| invalid())?,
            consistent: bytes[50] == 1,
        };
        // An anchor names the item before a nonzero position.
        if cursor.anchor.is_some() != (cursor.position > 0) {
            return Err(invalid());
        }
        Ok(cursor)
    }
}

/// One watchlist page. Each supported item carries the cursor that resumes
/// right after it, so a caller may commit part of a page and resume later.
pub struct WatchlistPage {
    pub entries: Vec<(SweepCursor, Request)>,
    pub next: SweepCursor,
    /// The page reached the end of the listing; `next.consistent` says whether
    /// the whole sweep saw one unchanged listing.
    pub complete: bool,
    /// Plex requests made for this page.
    pub requests: u32,
}

/// Stable identity of a listing item; volatile fields such as view counts are
/// excluded so that an unchanged list keeps its anchors.
fn item_anchor(item: &Value) -> [u8; 32] {
    let mut hash = Sha256::new();
    for field in ["type", "ratingKey", "guid", "key", "title"] {
        let text = string(item, field).unwrap_or_default();
        hash.update(&(text.len() as u64).to_be_bytes());
        hash.update(text.as_bytes());
    }
    hash.finalize()
}

/// A watchlist item as a request identity; other item types are skipped.
fn watchlist_request(item: &Value) -> Result<Option<Request>> {
    let kind = match string(item, "type") {
        Some("movie") => "movie",
        Some("show") => "series",
        Some("episode") => "episode",
        _ => return Ok(None),
    };
    let title = if kind == "episode" {
        string(item, "grandparentTitle").or_else(|| string(item, "title"))
    } else {
        string(item, "title")
    }
    .ok_or("Plex: missing title")?;
    let request = Request {
        kind: kind.into(),
        title: title.into(),
        year: u32::try_from(
            integer(
                item,
                if kind == "episode" {
                    "grandparentYear"
                } else {
                    "year"
                },
            )
            .unwrap_or(0),
        )
        .map_err(|_| "Plex: invalid year")?,
        season: u32::try_from(integer(item, "parentIndex").unwrap_or(0))
            .map_err(|_| "Plex: invalid season")?,
        episode: u32::try_from(integer(item, "index").unwrap_or(0))
            .map_err(|_| "Plex: invalid episode")?,
        source_path: None,
        source_url: None,
        source_numbering: None,
        tmdb_id: if kind == "episode" {
            None
        } else {
            tmdb_guid(item)
        },
    };
    request.validate()?;
    Ok(Some(request))
}

/// Reads the next watchlist page of a sweep. Movies, series and episodes keep
/// their watchlist identity; series are expanded by the caller.
pub fn watchlist_page(config: &Config, cursor: &SweepCursor) -> Result<WatchlistPage> {
    watchlist_page_before(config, cursor, Instant::now() + Duration::from_secs(90))
}

pub(crate) fn watchlist_page_before(
    config: &Config,
    cursor: &SweepCursor,
    deadline: Instant,
) -> Result<WatchlistPage> {
    if !config.plex.enabled {
        return Ok(WatchlistPage {
            entries: Vec::new(),
            next: SweepCursor::start(),
            complete: true,
            requests: 0,
        });
    }
    let headers = plex_headers(config)?;
    let url = &config.plex.watchlist_url;
    let mut consistent = cursor.consistent;
    let mut requests = 1;
    // Re-read the anchor so that a listing that moved since the previous page
    // is noticed instead of silently skipping items.
    let (mut start, (mut items, mut total)) = match cursor.anchor {
        Some(_) => {
            let start = cursor.position - 1;
            let page = plex_page_before(url, &headers, start, deadline, PLEX_POLL_DEADLINE)?;
            (start, page)
        }
        None => (
            cursor.position,
            plex_page_before(url, &headers, cursor.position, deadline, PLEX_POLL_DEADLINE)?,
        ),
    };
    let mut skip = 0;
    if let Some(anchor) = cursor.anchor {
        if items.first().map(item_anchor) == Some(anchor) {
            skip = 1;
            // A server that returns fewer items than requested may answer
            // with the anchor alone; read on from the position itself.
            if items.len() == 1 && !last_page(cursor.position, total, PAGE_SIZE) {
                requests += 1;
                start = cursor.position;
                (items, total) =
                    plex_page_before(url, &headers, start, deadline, PLEX_POLL_DEADLINE)?;
                skip = 0;
            }
        } else {
            consistent = false;
        }
        // The page that ended at the anchor came back again.
        if items.len() > skip && items.last().map(item_anchor) == Some(anchor) {
            return Err("Plex: pagination ignored by the server".into());
        }
    }
    let sweep_total = if cursor.position == 0 {
        total
    } else {
        cursor.total
    };
    if total != sweep_total {
        consistent = false;
    }
    let end = start
        .checked_add(items.len() as u64)
        .ok_or("Plex: item offset overflow")?;
    if items.len() <= skip && total.is_some_and(|n| n > start.saturating_add(skip as u64)) {
        // The listing shrank below the cursor or the server stopped early.
        consistent = false;
    }
    let complete = items.len() <= skip || last_page(end, total, items.len());
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    let mut after = SweepCursor {
        position: cursor.position,
        total: sweep_total,
        anchor: cursor.anchor,
        consistent,
    };
    for (index, item) in items.iter().enumerate().skip(skip) {
        after = SweepCursor {
            position: start + index as u64 + 1,
            total: sweep_total,
            anchor: Some(item_anchor(item)),
            consistent,
        };
        if let Some(request) = watchlist_request(item)?
            && seen.insert(request.canonical_key())
        {
            entries.push((after.clone(), request));
        }
    }
    Ok(WatchlistPage {
        entries,
        next: after,
        complete,
        requests,
    })
}

/// One Plex JSON request bounded by `deadline`. A request that cannot start,
/// or fails once the deadline has passed (its timeout is the time left), ends
/// with `expired` rather than as a network failure.
fn fetch_json_before(
    url: &str,
    headers: &[(String, String)],
    deadline: Instant,
    expired: &str,
) -> Result<Value> {
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(expired)?;
    let response = client()
        .with_timeout(timeout.min(Duration::from_secs(10)))
        .request("GET", url, headers, &[])
        .map_err(|_| {
            if Instant::now() >= deadline {
                expired
            } else {
                "Plex: network request failed"
            }
        })?;
    if !(200..300).contains(&response.status) {
        return Err(format!("Plex: HTTP response {}", response.status));
    }
    json::parse(std::str::from_utf8(&response.body).map_err(|_| "Plex: invalid UTF-8")?)
        .map_err(|_| "Plex: invalid JSON response".into())
}
/// The next page of a requester account's watchlist sweep, after confirming
/// that the account token still belongs to the expected Plex user.
pub(crate) fn requester_watchlist_page(
    config: &Config,
    account: &crate::requesters::Account,
    cursor: &SweepCursor,
    deadline: Instant,
) -> Result<WatchlistPage> {
    let cfg = verified_requester_config_before(config, account, deadline)?;
    let mut page = watchlist_page_before(&cfg, cursor, deadline)?;
    page.requests += 1;
    Ok(page)
}

pub(crate) fn requester_identity_before(
    config: &Config,
    account: &crate::requesters::Account,
    deadline: Instant,
) -> Result<()> {
    verified_requester_config_before(config, account, deadline).map(|_| ())
}

fn verified_requester_config_before(
    config: &Config,
    account: &crate::requesters::Account,
    deadline: Instant,
) -> Result<Config> {
    let token =
        config::secret(&account.token_env).map_err(|_| "Plex: account token unavailable")?;
    let mut cfg = config.clone();
    cfg.plex.token_override = Some(token);
    cfg.plex.enabled = true;
    cfg.plex.watchlist_url = account.watchlist_url.clone();
    let headers = plex_headers(&cfg)?;
    let identity = fetch_json_before(
        &account.identity_url,
        &headers,
        deadline,
        PLEX_POLL_DEADLINE,
    )?;
    if integer(&identity, "id").map(|n| n.to_string()).as_deref() != Some(&account.expected_user_id)
    {
        return Err("Plex account identity changed".into());
    }
    Ok(cfg)
}
pub(crate) fn requester_movie_before(
    config: &Config,
    request: &Request,
    deadline: Instant,
) -> Result<Request> {
    let mut request = request.clone();
    if request.tmdb_id.is_none() {
        let mut pairs = vec![("query", request.title.clone())];
        if request.year != 0 {
            pairs.push(("year", request.year.to_string()));
        }
        request.tmdb_id = Some(catalog_identity(
            &request,
            &fresh_catalog(config, "search/movie", &pairs, deadline)?,
        )?);
    }
    request.validate()?;
    Ok(request)
}

fn catalog_json(config: &Config, path: &str, pairs: &[(&str, String)]) -> Result<Value> {
    if !config.catalog.enabled {
        return Err("TMDB catalog is disabled".into());
    }
    let mut headers = vec![("Accept".into(), "application/json".into())];
    let Some(token) = config::optional_secret(&config.catalog.token_env)? else {
        return Err("TMDB catalog: missing read access token".into());
    };
    headers.push(("Authorization".into(), format!("Bearer {token}")));
    let url = query(&endpoint(&config.catalog.url, path)?, pairs)?;
    let key = catalog_cache_key(&url, &headers);
    let cached = catalog_cache()
        .lock()
        .map_err(|_| "TMDB catalog: cache unavailable")?
        .lookup(&key, Instant::now());
    if let Some(value) = cached {
        // Copy the JSON tree outside the shared lock.
        return Ok(value.as_ref().clone());
    }
    let (value, payload_bytes) = fetch_json_sized(&url, &headers, "TMDB catalog")?;
    let cached = Arc::new(value.clone());
    catalog_cache()
        .lock()
        .map_err(|_| "TMDB catalog: cache unavailable")?
        .insert(key, cached, payload_bytes, Instant::now());
    Ok(value)
}

fn resolve_catalog(config: &Config, request: &Request) -> Result<u64> {
    if let Some(id) = request.tmdb_id {
        return Ok(id);
    }
    let tv = matches!(request.kind.as_str(), "series" | "episode");
    let mut pairs = vec![("query", request.title.clone())];
    if request.year != 0 {
        pairs.push((
            if tv { "first_air_date_year" } else { "year" },
            request.year.to_string(),
        ));
    }
    let value = catalog_json(
        config,
        if tv { "search/tv" } else { "search/movie" },
        &pairs,
    )?;
    catalog_identity(request, &value)
}

fn catalog_identity(request: &Request, value: &Value) -> Result<u64> {
    let tv = matches!(request.kind.as_str(), "series" | "episode");
    let items = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or("TMDB catalog: missing results")?;
    let mut found = BTreeSet::new();
    for item in items {
        let title = string(item, if tv { "name" } else { "title" });
        let original = string(
            item,
            if tv {
                "original_name"
            } else {
                "original_title"
            },
        );
        let matches = [title, original]
            .into_iter()
            .flatten()
            .any(|t| tokens(t) == tokens(&request.title));
        let date = string(item, if tv { "first_air_date" } else { "release_date" });
        let year = date
            .and_then(|d| d.get(..4))
            .and_then(|s| s.parse::<u32>().ok());
        if matches
            && (request.year == 0 || year == Some(request.year))
            && let Some(id) = integer(item, "id").filter(|n| *n != 0)
        {
            found.insert(id);
        }
    }
    if found.len() != 1 {
        return Err("TMDB catalog: missing or ambiguous identification; specify tmdb_id".into());
    }
    Ok(*found
        .first()
        .ok_or("TMDB catalog: missing identification")?)
}

/// Titles the TMDB catalog finds for a query: what an operator picks from
/// before Mynou searches the indexers. `kind` is `movie` or `series`.
pub fn catalog_search(config: &Config, kind: &str, text: &str, year: u32) -> Result<Value> {
    let tv = match kind {
        "movie" => false,
        "series" => true,
        _ => return Err("Catalog: kind must be movie or series".into()),
    };
    let text = text.trim();
    if text.is_empty() || text.chars().count() > 128 || text.chars().any(char::is_control) {
        return Err("Catalog: the search text must have 1 to 128 characters".into());
    }
    let mut pairs = vec![
        ("query", text.to_owned()),
        ("include_adult", "false".to_owned()),
    ];
    if year != 0 {
        pairs.push((
            if tv { "first_air_date_year" } else { "year" },
            year.to_string(),
        ));
    }
    let value = catalog_json(
        config,
        if tv { "search/tv" } else { "search/movie" },
        &pairs,
    )?;
    let items = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or("TMDB catalog: missing results")?;
    let mut results = Vec::new();
    for item in items.iter().take(20) {
        let Some(id) = integer(item, "id").filter(|id| *id > 0 && *id <= 9_007_199_254_740_991)
        else {
            continue;
        };
        let Some(title) = string(item, if tv { "name" } else { "title" })
            .map(str::trim)
            .filter(|t| !t.is_empty() && t.len() <= 512 && !t.chars().any(char::is_control))
        else {
            continue;
        };
        let mut entry = Value::object();
        entry.insert("tmdb_id", Value::Number(id as f64));
        entry.insert("kind", kind);
        entry.insert("title", title);
        if let Some(original) = string(
            item,
            if tv {
                "original_name"
            } else {
                "original_title"
            },
        )
        .map(str::trim)
        .filter(|o| {
            !o.is_empty() && *o != title && o.len() <= 512 && !o.chars().any(char::is_control)
        }) {
            entry.insert("original_title", original);
        }
        let year = string(item, if tv { "first_air_date" } else { "release_date" })
            .filter(|text| crate::date::day(text).is_ok())
            .and_then(|text| text[..4].parse::<u32>().ok());
        entry.insert(
            "year",
            year.map_or(Value::Null, |y| Value::Number(f64::from(y))),
        );
        if let Some(overview) = string(item, "overview")
            .map(str::trim)
            .filter(|o| !o.is_empty())
        {
            entry.insert("overview", report_text(overview, 1_000));
        }
        if let Some(poster) = string(item, "poster_path").filter(|p| poster_path(p)) {
            entry.insert("poster", poster);
        }
        results.push(entry);
    }
    let mut report = Value::object();
    report.insert("results", Value::Array(results));
    Ok(report)
}

/// TMDB serves poster images here, at a width that suits a cover.
const TMDB_POSTERS: &str = "https://image.tmdb.org/t/p/w342";
const MAX_POSTER_BYTES: usize = 2 * 1024 * 1024;
/// Recent posters kept in memory, as data URLs.
const POSTER_CACHE_BYTES: usize = 16 * 1024 * 1024;

/// A poster path as TMDB names it, such as `/d5NXSklXo0qyIYkgV94XAgMIckC.jpg`.
fn poster_path(path: &str) -> bool {
    path.strip_prefix('/')
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(stem, extension)| {
            (1..=64).contains(&stem.len())
                && stem.bytes().all(|b| b.is_ascii_alphanumeric())
                && matches!(extension, "jpg" | "png")
        })
}

#[derive(Default)]
struct PosterCache {
    urls: BTreeMap<String, Arc<str>>,
    order: VecDeque<String>,
    bytes: usize,
}

fn poster_cache() -> &'static Mutex<PosterCache> {
    static CACHE: OnceLock<Mutex<PosterCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(PosterCache::default()))
}

/// One catalog poster as a `data:` URL, so the page shows it without loading
/// anything from another origin. Only JPEG and PNG images are passed on.
pub fn catalog_poster(config: &Config, path: &str) -> Result<Value> {
    catalog_poster_from(config, TMDB_POSTERS, path)
}

pub fn catalog_poster_from(config: &Config, base: &str, path: &str) -> Result<Value> {
    if !config.catalog.enabled {
        return Err("TMDB catalog is disabled".into());
    }
    if !poster_path(path) {
        return Err("Catalog: invalid poster path".into());
    }
    let key = format!("{base}{path}");
    let cached = poster_cache()
        .lock()
        .map_err(|_| "Catalog: poster cache unavailable")?
        .urls
        .get(&key)
        .cloned();
    let url = match cached {
        Some(url) => url,
        None => {
            let response = client()
                .with_max_body(MAX_POSTER_BYTES)
                .request("GET", &key, &[], &[])
                .map_err(|_| "Catalog: poster request failed")?;
            if !(200..300).contains(&response.status) {
                return Err(format!("Catalog: poster HTTP response {}", response.status));
            }
            let kind = match response.body.as_slice() {
                [0xff, 0xd8, 0xff, ..] => "jpeg",
                [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => "png",
                _ => return Err("Catalog: the poster is not a JPEG or PNG image".into()),
            };
            let url: Arc<str> = format!(
                "data:image/{kind};base64,{}",
                crate::ws::base64(&response.body)
            )
            .into();
            let mut cache = poster_cache()
                .lock()
                .map_err(|_| "Catalog: poster cache unavailable")?;
            if !cache.urls.contains_key(&key) {
                cache.bytes += url.len();
                cache.urls.insert(key.clone(), url.clone());
                cache.order.push_back(key);
                while cache.bytes > POSTER_CACHE_BYTES {
                    let Some(oldest) = cache.order.pop_front() else {
                        break;
                    };
                    if let Some(gone) = cache.urls.remove(&oldest) {
                        cache.bytes -= gone.len();
                    }
                }
            }
            url
        }
    };
    let mut value = Value::object();
    value.insert("data_url", url.as_ref());
    Ok(value)
}

pub(crate) fn series_plan_before(
    config: &Config,
    request: &Request,
    include_specials: bool,
    deadline: Instant,
) -> Result<crate::series::Plan> {
    request.validate()?;
    if request.kind != "series" || request.source_path.is_some() || request.source_url.is_some() {
        return Err(
            "Series monitoring requires a series identity without an explicit source".into(),
        );
    }
    if request
        .tmdb_id
        .is_some_and(|id| id == 0 || id > 9_007_199_254_740_991)
    {
        return Err("Invalid series TMDB identity".into());
    }
    let deadline = deadline.min(Instant::now() + SEARCH_BUDGET);
    remaining_search_time(deadline)?;
    let id = match request.tmdb_id {
        Some(id) => id,
        None => {
            let mut pairs = vec![("query", request.title.clone())];
            if request.year != 0 {
                pairs.push(("first_air_date_year", request.year.to_string()));
            }
            catalog_identity(
                request,
                &fresh_catalog(config, "search/tv", &pairs, deadline)?,
            )?
        }
    };
    let details = fresh_catalog(config, &format!("tv/{id}"), &[], deadline)?;
    series_plan_details_before(config, request, include_specials, id, &details, deadline)
}

fn series_plan_details_before(
    config: &Config,
    request: &Request,
    include_specials: bool,
    id: u64,
    details: &Value,
    deadline: Instant,
) -> Result<crate::series::Plan> {
    if integer(details, "id").is_some_and(|actual| actual != id) {
        return Err("TMDB catalog: series identity mismatch".into());
    }
    let seasons = details
        .get("seasons")
        .and_then(Value::as_array)
        .ok_or("TMDB catalog: missing seasons")?;
    if seasons.len() > 1000 {
        return Err("TMDB catalog: too many seasons".into());
    }
    let mut identity = request.clone();
    identity.tmdb_id = Some(id);
    if identity.year == 0 {
        identity.year = string(details, "first_air_date")
            .filter(|text| crate::date::day(text).is_ok())
            .and_then(|text| text[..4].parse().ok())
            .unwrap_or(0);
    }
    let mut chosen = BTreeSet::new();
    for season in seasons {
        let number = integer(season, "season_number")
            .and_then(|number| u32::try_from(number).ok())
            .filter(|number| *number <= 9999)
            .ok_or("TMDB catalog: invalid season number")?;
        if (number == 0 && !include_specials) || (request.season != 0 && request.season != number) {
            continue;
        }
        if !chosen.insert(number) {
            return Err("TMDB catalog: ambiguous duplicate season".into());
        }
        if chosen.len() > 100 {
            return Err("TMDB catalog: a series plan is limited to 100 seasons".into());
        }
    }
    let mut episodes = Vec::new();
    for season in chosen {
        remaining_search_time(deadline)?;
        let value = fresh_catalog(config, &format!("tv/{id}/season/{season}"), &[], deadline)?;
        if integer(&value, "season_number").is_some_and(|actual| actual != u64::from(season)) {
            return Err("TMDB catalog: season identity mismatch".into());
        }
        let items = value
            .get("episodes")
            .and_then(Value::as_array)
            .ok_or("TMDB catalog: missing episodes")?;
        if items.len() > 1000 {
            return Err("TMDB catalog: too many episodes in a season".into());
        }
        for item in items {
            if integer(item, "season_number").is_some_and(|actual| actual != u64::from(season)) {
                return Err("TMDB catalog: episode season mismatch".into());
            }
            let number = integer(item, "episode_number")
                .and_then(|number| u32::try_from(number).ok())
                .filter(|number| (1..=99999).contains(number))
                .ok_or("TMDB catalog: invalid episode number")?;
            if request.episode != 0 && request.episode != number {
                continue;
            }
            let episode = crate::series::Episode {
                catalog_id: match item.get("id") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(
                        value
                            .as_u64()
                            .filter(|id| *id > 0 && *id <= 9_007_199_254_740_991)
                            .ok_or("TMDB catalog: invalid episode identity")?,
                    ),
                },
                season,
                episode: number,
                title: string(item, "name")
                    .map_or_else(|| format!("Episode {number}"), str::to_owned),
                air_date: string(item, "air_date")
                    .filter(|text| crate::date::day(text).is_ok())
                    .map(str::to_owned),
            };
            episode.validate()?;
            episodes.push(episode);
            if episodes.len() > crate::series::MAX_EPISODES {
                return Err("TMDB catalog: a plan is limited to 2000 episodes".into());
            }
        }
    }
    episodes.sort_by_key(|episode| (episode.season, episode.episode));
    let plan = crate::series::Plan {
        request: identity,
        episodes,
    };
    plan.validate()?;
    remaining_search_time(deadline)?;
    Ok(plan)
}

/// A new IRC demand requires fresh catalog facts, without changing a series plan.
pub(crate) fn irc_catalog_request_before(
    config: &Config,
    claim: &Request,
    retained: Option<&crate::series::Record>,
    deadline: Instant,
) -> Result<Request> {
    claim.validate()?;
    if !matches!(claim.kind.as_str(), "movie" | "episode") || claim.year == 0 {
        return Err("IRC: new demand requires a complete catalog identity".into());
    }
    let id = claim.tmdb_id.ok_or("IRC: missing catalog identity")?;
    let movie = claim.kind == "movie";
    let details = fresh_catalog(
        config,
        &format!("{}/{id}", if movie { "movie" } else { "tv" }),
        &[],
        deadline,
    )?;
    let title = string(&details, if movie { "title" } else { "name" })
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
        .ok_or("IRC: catalog title is missing or invalid")?;
    let date = string(
        &details,
        if movie {
            "release_date"
        } else {
            "first_air_date"
        },
    )
    .filter(|s| crate::date::day(s).is_ok())
    .ok_or("IRC: catalog date is missing or invalid")?;
    if integer(&details, "id") != Some(id)
        || tokens(title).is_empty()
        || tokens(title) != tokens(&claim.title)
        || date[..4].parse::<u32>().ok() != Some(claim.year)
        || date > crate::date::today().as_str()
    {
        return Err("IRC: catalog facts differ from the announcement or are not released".into());
    }
    let mut canonical = claim.clone();
    canonical.title = title.into();
    if !movie {
        let mut scope = retained.map_or_else(|| canonical.clone(), |r| r.plan.request.clone());
        scope.kind = "series".into();
        scope.title = canonical.title.clone();
        scope.year = canonical.year;
        if retained.is_some() {
            // Retained mappings can point outside the original catalog season.
            scope.season = 0;
            scope.episode = 0;
        }
        let plan = series_plan_details_before(config, &scope, true, id, &details, deadline)?;
        if let Some(retained) = retained {
            let mut checked = retained.clone();
            checked.accept_catalog(retained.normalize_catalog(plan)?)?;
            let episode = checked
                .plan
                .episodes
                .iter()
                .find(|e| e.season == claim.season && e.episode == claim.episode)
                .ok_or("IRC: claimed episode is absent from the retained canonical scope")?;
            if episode.catalog_id.is_none()
                || episode
                    .air_date
                    .as_deref()
                    .is_none_or(|d| d > crate::date::today().as_str())
            {
                return Err(
                    "IRC: claimed episode is not released with a stable catalog identity".into(),
                );
            }
            canonical = checked.episode_request(episode);
        } else {
            let episode = plan
                .episodes
                .iter()
                .find(|e| e.season == claim.season && e.episode == claim.episode)
                .ok_or("IRC: claimed episode is absent from the catalog")?;
            if episode.catalog_id.is_none()
                || episode
                    .air_date
                    .as_deref()
                    .is_none_or(|d| d > crate::date::today().as_str())
            {
                return Err(
                    "IRC: claimed episode is not released with a stable catalog identity".into(),
                );
            }
        }
    }
    canonical.validate()?;
    remaining_search_time(deadline)?;
    Ok(canonical)
}

fn fresh_catalog(
    config: &Config,
    path: &str,
    pairs: &[(&str, String)],
    deadline: Instant,
) -> Result<Value> {
    if !config.catalog.enabled {
        return Err("TMDB catalog is disabled".into());
    }
    let mut headers = vec![("Accept".into(), "application/json".into())];
    let Some(token) = config::optional_secret(&config.catalog.token_env)? else {
        return Err("TMDB catalog: missing read access token".into());
    };
    headers.push(("Authorization".into(), format!("Bearer {token}")));
    let url = query(&endpoint(&config.catalog.url, path)?, pairs)?;
    let response = client()
        .with_timeout(remaining_search_time(deadline)?.min(Duration::from_secs(20)))
        .request("GET", &url, &headers, &[])
        .map_err(|_| "TMDB catalog: request failed")?;
    if !(200..300).contains(&response.status) {
        return Err(format!("TMDB catalog: HTTP response {}", response.status));
    }
    let value = json::parse(
        std::str::from_utf8(&response.body).map_err(|_| "TMDB catalog: invalid UTF-8")?,
    )
    .map_err(|_| "TMDB catalog: invalid JSON")?;
    remaining_search_time(deadline)?;
    Ok(value)
}

/// Canonical TMDB title and year of one movie or series identity.
pub(crate) fn catalog_canonical_before(
    config: &Config,
    kind: &str,
    id: u64,
    deadline: Instant,
) -> Result<(String, u32)> {
    let tv = kind == "episode";
    let value = fresh_catalog(
        config,
        &format!("{}/{id}", if tv { "tv" } else { "movie" }),
        &[],
        deadline,
    )
    .map_err(|_| "TMDB catalog: canonical identity unavailable")?;
    let title = string(&value, if tv { "name" } else { "title" })
        .map(str::trim)
        .filter(|t| !t.is_empty() && t.len() <= 512 && !t.chars().any(char::is_control));
    let year = string(&value, if tv { "first_air_date" } else { "release_date" })
        .filter(|text| crate::date::day(text).is_ok())
        .and_then(|text| text[..4].parse::<u32>().ok());
    match (integer(&value, "id"), title, year) {
        (Some(actual), Some(title), Some(year)) if actual == id => Ok((title.into(), year)),
        _ => Err("TMDB catalog: canonical identity unavailable".into()),
    }
}

/// One Torznab item, identified by its details page. GUIDs are never read; the
/// enclosure may carry a credential, so it is never displayed, stored or reported.
#[derive(Default)]
pub(crate) struct DetailsEntry {
    pub(crate) details: String,
    pub(crate) tmdb_ids: Vec<String>,
    pub(crate) info_hashes: Vec<String>,
    pub(crate) enclosure: Option<String>,
}

/// One bounded text search of a Torznab source, or of a JSON source whose
/// mapping describes details; failures carry only fixed messages.
pub(crate) fn details_before(
    source: &Source,
    text: &str,
    deadline: Instant,
) -> Result<Vec<DetailsEntry>> {
    match source.kind.as_str() {
        "torznab" => torznab_details_before(source, text, deadline),
        "json" if source.options.mapping.describes_details() => {
            json_details_before(source, text, deadline)
        }
        _ => Err("Indexer: lookup requires a Torznab source or a JSON mapping with details".into()),
    }
}

/// Only the text query, fixed parameters and API key are sent: the lookup
/// names a release, not a catalog title.
fn json_details_before(
    source: &Source,
    text: &str,
    deadline: Instant,
) -> Result<Vec<DetailsEntry>> {
    let mapping = &source.options.mapping;
    let body = json_text_search(source, Some(text), deadline)?;
    let entries = (|| {
        let text = std::str::from_utf8(&body).map_err(|_| "Indexer: invalid UTF-8")?;
        let value = json::parse(text)?;
        let items = mapping
            .list(&value)
            .ok_or("JSON indexer: missing results/items list")?;
        if items.len() > MAX_FEED_ENTRIES {
            return Err("JSON indexer: too many lookup results".into());
        }
        Ok(items
            .iter()
            .map(|item| {
                let (details, tmdb_ids, info_hashes) = mapping.details(item);
                DetailsEntry {
                    details: details.map_or(String::new(), |d| d.trim().to_owned()),
                    tmdb_ids,
                    info_hashes,
                    enclosure: json_download(item, &source.url, mapping),
                }
            })
            .collect())
    })();
    crate::indexers::parsed(source, entries.is_ok());
    entries
}

/// The mapped acquisition address, with any download parameters appended to an
/// HTTP(S) address; a magnet is left unchanged.
fn json_download(item: &Value, base: &str, mapping: &Mapping) -> Option<String> {
    let url = acquisition_url(mapping.url(item)?, base).ok()?;
    let extra = mapping.download_params();
    if extra.is_empty() || url.starts_with("magnet:") {
        return Some(url);
    }
    let pairs: Vec<(&str, String)> = extra.iter().map(|(n, v)| (n.as_str(), v.clone())).collect();
    query(&url, &pairs).ok()
}

/// A JSON request with the text (when given) as its query role, the fixed
/// parameters and the API key; no catalog role is sent.
fn json_text_search(source: &Source, text: Option<&str>, deadline: Instant) -> Result<Vec<u8>> {
    let mapping = &source.options.mapping;
    let mut pairs: Vec<(&str, String)> = Vec::new();
    if let (Some(name), Some(text)) = (mapping.parameter("query"), text) {
        pairs.push((name, text.to_owned()));
    }
    pairs.extend(mapping.fixed().iter().map(|(n, v)| (n.as_str(), v.clone())));
    if let Some(name) = mapping.parameter("api_key")
        && let Some(key) = config::optional_secret(&source.api_key_env)
            .map_err(|_| "Indexer: authentication unavailable")?
    {
        pairs.push((name, key));
    }
    let url = query(&source.url, &pairs)?;
    let response = crate::indexers::fetch(source, &url, "application/json", deadline)?;
    remaining_search_time(deadline).map_err(|_| "Indexer: request budget expired")?;
    Ok(response.body)
}

fn torznab_details_before(
    source: &Source,
    text: &str,
    deadline: Instant,
) -> Result<Vec<DetailsEntry>> {
    let mut pairs = vec![("t", "search".to_owned()), ("q", text.to_owned())];
    if let Some(key) = config::optional_secret(&source.api_key_env)? {
        pairs.push(("apikey", key));
    }
    let url = query(&source.url, &pairs)?;
    let response = crate::indexers::fetch(
        source,
        &url,
        "application/rss+xml, application/xml",
        deadline,
    )?;
    remaining_search_time(deadline)?;
    let entries = (|| {
        let text = std::str::from_utf8(&response.body).map_err(|_| "Indexer: invalid UTF-8")?;
        let root = parse_xml(text)?;
        let channel = (root.local() == "rss")
            .then(|| root.child("channel"))
            .flatten()
            .ok_or("Torznab indexer: invalid response")?;
        let mut out = Vec::new();
        for item in channel.children.iter().filter(|c| c.local() == "item") {
            if out.len() == MAX_FEED_ENTRIES {
                return Err("Torznab indexer: too many lookup results".into());
            }
            let mut entry = DetailsEntry {
                details: item
                    .child("comments")
                    .map_or(String::new(), |c| c.text.trim().to_owned()),
                enclosure: item
                    .child("enclosure")
                    .and_then(|e| e.attrs.get("url"))
                    .cloned(),
                ..DetailsEntry::default()
            };
            for attr in item.children.iter().filter(|c| c.local() == "attr") {
                let value = attr.attrs.get("value").cloned().unwrap_or_default();
                match attr.attrs.get("name").map(String::as_str) {
                    Some("tmdbid") => entry.tmdb_ids.push(value),
                    Some("infohash") => entry.info_hashes.push(value),
                    _ => {}
                }
            }
            out.push(entry);
        }
        Ok(out)
    })();
    crate::indexers::parsed(source, entries.is_ok());
    entries
}

/// Download one `.torrent` from the indexer's own origin, without redirects.
pub(crate) fn indexer_metainfo_before(
    source: &Source,
    url: &str,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let url = acquisition_url(url, &source.url).map_err(|_| "Indexer: invalid download")?;
    if url.starts_with("magnet:") {
        return Err("Indexer: invalid download".into());
    }
    let response = crate::indexers::fetch(source, &url, "application/x-bittorrent", deadline)?;
    remaining_search_time(deadline)?;
    Ok(response.body)
}

// Current UTC date, converted from Unix days to the Gregorian calendar.
fn today() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400;
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn valid_date(date: &str) -> bool {
    let b = date.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7) || c.is_ascii_digit())
    {
        return false;
    }
    let year = date[..4].parse::<u32>().unwrap_or(0);
    let month = date[5..7].parse::<u32>().unwrap_or(0);
    let day = date[8..].parse::<u32>().unwrap_or(0);
    let max = match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    };
    year >= 1800 && day != 0 && day <= max
}

/// Series without an explicit source become episode requests.
pub fn expand(config: &Config, request: &Request) -> Result<Vec<Request>> {
    request.validate()?;
    if request.kind != "series" {
        let mut expanded = request.clone();
        if request.kind == "movie"
            && config.catalog.enabled
            && request.source_path.is_none()
            && request.source_url.is_none()
        {
            let id = resolve_catalog(config, request)?;
            expanded.tmdb_id = Some(id);
            if expanded.year == 0 {
                let details = catalog_json(config, &format!("movie/{id}"), &[])?;
                expanded.year = string(&details, "release_date")
                    .filter(|s| valid_date(s))
                    .and_then(|s| s[..4].parse().ok())
                    .unwrap_or(0);
            }
        }
        return Ok(vec![expanded]);
    }
    if request.source_path.is_some() || request.source_url.is_some() {
        return Err("Series: submit an episode when providing an explicit source".into());
    }
    if !config.catalog.enabled {
        return Err(
            "Complete series: enable TMDB to identify episodes that have already aired".into(),
        );
    }
    let id = resolve_catalog(config, request)?;
    let details = catalog_json(config, &format!("tv/{id}"), &[])?;
    let seasons = details
        .get("seasons")
        .and_then(Value::as_array)
        .ok_or("TMDB catalog: missing seasons")?;
    if seasons.len() > 1_000 {
        return Err("TMDB catalog: too many seasons".into());
    }
    let cutoff = today();
    let year = string(&details, "first_air_date")
        .filter(|s| valid_date(s))
        .and_then(|s| s[..4].parse().ok())
        .unwrap_or(request.year);
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for season in seasons {
        let number =
            integer(season, "season_number").ok_or("TMDB catalog: missing season number")?;
        if number == 0 || (request.season != 0 && number != u64::from(request.season)) {
            continue;
        }
        let number = u32::try_from(number).map_err(|_| "TMDB catalog: invalid season")?;
        if let Some(date) = string(season, "air_date").filter(|s| valid_date(s))
            && date > cutoff.as_str()
        {
            continue;
        }
        let value = catalog_json(config, &format!("tv/{id}/season/{number}"), &[])?;
        let episodes = value
            .get("episodes")
            .and_then(Value::as_array)
            .ok_or("TMDB catalog: missing episodes")?;
        if episodes.len() > 1_000 {
            return Err("TMDB catalog: too many episodes in a season".into());
        }
        for episode in episodes {
            let Some(date) = string(episode, "air_date").filter(|s| valid_date(s)) else {
                continue;
            };
            if date > cutoff.as_str() {
                continue;
            }
            let episode = u32::try_from(
                integer(episode, "episode_number").ok_or("TMDB catalog: missing episode number")?,
            )
            .map_err(|_| "TMDB catalog: invalid episode")?;
            if episode == 0
                || (request.episode != 0 && episode != request.episode)
                || !seen.insert((number, episode))
            {
                continue;
            }
            let expanded = Request {
                kind: "episode".into(),
                title: request.title.clone(),
                year,
                season: number,
                episode,
                source_path: None,
                source_url: None,
                source_numbering: None,
                tmdb_id: Some(id),
            };
            expanded.validate()?;
            out.push(expanded);
            if out.len() > MAX_ITEMS {
                return Err("TMDB catalog: too many episodes".into());
            }
        }
    }
    out.sort_by_key(|r| (r.season, r.episode));
    Ok(out)
}

#[derive(Clone, Debug)]
pub(crate) struct Release {
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) seeders: u64,
}

fn episode_marker(text: &str) -> Option<(u32, u32)> {
    if let Some(rest) = text.strip_prefix('s') {
        let (season, episode) = rest.split_once('e')?;
        if season.is_empty()
            || episode.is_empty()
            || !season
                .bytes()
                .chain(episode.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        return Some((season.parse().ok()?, episode.parse().ok()?));
    }
    let (season, episode) = text.split_once('x')?;
    if season.is_empty()
        || episode.is_empty()
        || !season
            .bytes()
            .chain(episode.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let season = season.parse::<u32>().ok()?;
    // Match the title-policy parser: video-sized numeric pairs denote image
    // dimensions, whereas small pairs such as 2x03 denote an episode.
    if season >= 320 {
        return None;
    }
    Some((season, episode.parse().ok()?))
}

fn release_matches(request: &Request, title: &str) -> bool {
    let expected = tokens(&request.title);
    let actual = tokens(title);
    if expected.is_empty() || !actual.starts_with(&expected) {
        return false;
    }
    let rest = &actual[expected.len()..];
    if request.kind == "episode" {
        let at = usize::from(
            rest.first()
                .is_some_and(|s| s.parse::<u32>().ok() == Some(request.year) && request.year != 0),
        );
        let later_episode = rest.iter().skip(at + 1).any(|s| {
            episode_marker(s).is_some()
                || s.strip_prefix('e').is_some_and(|number| {
                    !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
                })
        });
        let expected =
            request
                .source_numbering
                .unwrap_or(crate::numbering::SourceNumber::SeasonEpisode(
                    crate::numbering::EpisodeNumber {
                        season: request.season,
                        episode: request.episode,
                    },
                ));
        return !later_episode
            && rest.get(at).is_some_and(|label| expected.matches(label))
            && !rest.iter().skip(at + 1).any(|label| {
                crate::numbering::conflicting_marker(expected, label, 0)
                    || matches!(expected, crate::numbering::SourceNumber::Absolute(_))
                        && crate::numbering::decimal_token(label).is_some()
            });
    }
    if request.kind != "movie" {
        return false;
    }
    if rest.iter().any(|s| episode_marker(s).is_some()) {
        return false;
    }
    if request.year != 0 {
        rest.first().and_then(|s| s.parse::<u32>().ok()) == Some(request.year)
    } else {
        // Without a year, accept the title alone or directly followed by a year/quality.
        rest.first().is_none_or(|s| {
            s.parse::<u32>().is_ok_and(|n| (1888..=2200).contains(&n))
                || [
                    "2160p", "1080p", "720p", "480p", "bluray", "brrip", "bdrip", "webrip", "web",
                    "dvdrip", "hdtv",
                ]
                .contains(&s.as_str())
        })
    }
}

/// Verifies a recorded baseline against the same title, year and episode
/// identity rules used for automatic indexer selection.
pub fn release_identity_matches(request: &Request, title: &str) -> bool {
    release_matches(request, title)
}

pub(crate) fn acquisition_url(url: &str, base: &str) -> Result<String> {
    if url.len() > 8_192 || url.chars().any(char::is_control) {
        return Err("Invalid acquisition URL".into());
    }
    if let Some(query) = url.strip_prefix("magnet:?") {
        if query
            .split('&')
            .any(|part| part.starts_with("xt=urn:btih:") || part.starts_with("xt=urn:btmh:"))
        {
            return Ok(url.into());
        }
        return Err("Magnet: missing hash".into());
    }
    let url = if url.starts_with('/') && !url.starts_with("//") {
        format!("{}{}", net::parse_url(base)?.origin(), url)
    } else {
        url.into()
    };
    net::parse_url(&url)?;
    Ok(url)
}

fn json_releases(value: &Value, base: &str, mapping: &Mapping) -> Result<Vec<Release>> {
    let items = mapping
        .list(value)
        .ok_or("JSON indexer: missing results/items list")?;
    if items.len() > MAX_ITEMS {
        return Err("JSON indexer: too many results".into());
    }
    Ok(items
        .iter()
        .filter_map(|item| json_release(item, base, mapping))
        .collect())
}

/// An item without a bounded title or a valid acquisition address is skipped.
fn json_release(item: &Value, base: &str, mapping: &Mapping) -> Option<Release> {
    let title = mapping.title(item).filter(|title| title.len() <= 2_048)?;
    let url = json_download(item, base, mapping)?;
    Some(Release {
        title: title.into(),
        url,
        seeders: mapping.seeders(item),
    })
}

pub(crate) fn probe_source(source: &Source, request: &Request, deadline: Instant) -> bool {
    source_releases(source, request, deadline).is_ok()
}

fn source_releases(source: &Source, request: &Request, deadline: Instant) -> Result<Vec<Release>> {
    let mut pairs = vec![("q", request.title.clone())];
    let numbering =
        request
            .source_numbering
            .unwrap_or(crate::numbering::SourceNumber::SeasonEpisode(
                crate::numbering::EpisodeNumber {
                    season: request.season,
                    episode: request.episode,
                },
            ));
    let (season, episode) = match numbering {
        crate::numbering::SourceNumber::SeasonEpisode(number) => (number.season, number.episode),
        crate::numbering::SourceNumber::Absolute(number) => {
            pairs[0].1 = format!("{} {number:03}", request.title);
            (0, 0)
        }
    };
    if source.kind == "torznab" {
        pairs.push((
            "t",
            if matches!(request.kind.as_str(), "episode" | "series") {
                "tvsearch"
            } else {
                "movie"
            }
            .into(),
        ));
        if matches!(request.kind.as_str(), "episode" | "series")
            && !matches!(numbering, crate::numbering::SourceNumber::Absolute(_))
        {
            pairs.push(("season", season.to_string()));
            if request.kind == "episode" {
                pairs.push(("ep", episode.to_string()));
            }
        }
    } else if source.kind == "json" {
        let mapping = &source.options.mapping;
        // A catalog ID selects the title exactly; the text query would only add
        // punctuation-sensitive filtering. Titles are still verified locally.
        // Absolute numbers live in the text query, which they keep.
        let catalog = request.tmdb_id.filter(|_| {
            mapping.parameter("tmdb_id").is_some()
                && !matches!(numbering, crate::numbering::SourceNumber::Absolute(_))
        });
        match mapping.parameter("query") {
            Some(name) if catalog.is_none() => pairs[0].0 = name,
            _ => pairs.clear(),
        }
        let mut extra = vec![
            ("kind", mapping.kind_value(&request.kind).to_owned()),
            ("year", request.year.to_string()),
        ];
        // As for Torznab, numbers are sent only where they identify an episode
        // or a season: a zero would filter movies to a nonexistent season.
        match numbering {
            crate::numbering::SourceNumber::Absolute(number) => {
                extra.push(("absolute", number.to_string()));
            }
            _ if matches!(request.kind.as_str(), "episode" | "series") => {
                extra.push(("season", season.to_string()));
                if request.kind == "episode" {
                    extra.push(("episode", episode.to_string()));
                }
            }
            _ => {}
        }
        if let Some(id) = catalog {
            extra.push(("tmdb_id", id.to_string()));
        }
        for (role, value) in extra {
            if let Some(name) = mapping.parameter(role) {
                pairs.push((name, value));
            }
        }
        pairs.extend(mapping.fixed().iter().map(|(n, v)| (n.as_str(), v.clone())));
    }
    if let Some(key) = config::optional_secret(&source.api_key_env)? {
        let name = if source.kind == "json" {
            source.options.mapping.parameter("api_key")
        } else {
            Some("apikey")
        };
        if let Some(name) = name {
            pairs.push((name, key));
        }
    }
    let url = query(&source.url, &pairs)?;
    let response = crate::indexers::fetch(
        source,
        &url,
        if source.kind == "json" {
            "application/json"
        } else {
            "application/rss+xml, application/xml"
        },
        deadline,
    )?;
    remaining_search_time(deadline)?;
    let releases = (|| {
        let text = std::str::from_utf8(&response.body).map_err(|_| "Indexer: invalid UTF-8")?;
        if source.kind == "json" {
            json_releases(&json::parse(text)?, &source.url, &source.options.mapping)
        } else {
            rss_releases(text, &source.url)
        }
    })();
    crate::indexers::parsed(source, releases.is_ok());
    let releases = releases?;
    remaining_search_time(deadline)?;
    Ok(releases)
}

#[derive(Clone)]
struct Candidate {
    release: Release,
    source: String,
    id: String,
    assessment: Assessment,
}

impl Candidate {
    fn to_json(&self) -> Value {
        let mut value = Value::object();
        value.insert("id", self.id.clone());
        value.insert("title", report_text(&self.release.title, 2_048));
        value.insert("source", report_text(&self.source, 128));
        // An indexer may report integers outside JSON's exact range.
        value.insert(
            "seeders",
            if self.release.seeders <= 9_007_199_254_740_991 {
                Value::Number(self.release.seeders as f64)
            } else {
                Value::String(self.release.seeders.to_string())
            },
        );
        value.insert("assessment", self.assessment.to_json());
        value
    }
}

/// Display labels are bounded and cannot turn a malformed source name or title
/// into a download URL in the public preview.
pub(crate) fn report_text(text: &str, limit: usize) -> String {
    let lowercase = text.to_ascii_lowercase();
    if lowercase.contains("://")
        || lowercase.contains("magnet:")
        || ["apikey=", "api_key=", "token=", "authorization:"]
            .iter()
            .any(|marker| lowercase.contains(marker))
    {
        return "[redacted]".into();
    }
    let mut result = String::new();
    for character in text.chars().filter(|character| !character.is_control()) {
        if result.len() + character.len_utf8() > limit {
            break;
        }
        result.push(character);
    }
    result
}

fn candidate_digest(profile: &str, source: &str, release: &Release) -> [u8; 32] {
    let mut hash = Sha256::new();
    for field in [profile, source, &release.title, &release.url] {
        hash.update(&(field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    hash.finalize()
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 15) as usize] as char);
    }
    text
}

fn candidate_id(profile: &str, source: &str, release: &Release) -> String {
    hex(&candidate_digest(profile, source, release))
}

/// A candidate's report position: accepted first, then higher rank, more
/// seeders and finally the candidate ID. Byte order is report order, so a
/// position doubles as an opaque cursor that reveals no title or URL.
type Position = [u8; POSITION_BYTES];
const POSITION_BYTES: usize = 1 + 5 * 4 + 8 + 32;
const DEFAULT_REPORT_PAGE: usize = 100;
const MAX_REPORT_PAGE: usize = 200;
/// Accepted season-pack releases whose metadata a pack search may inspect.
const PACK_CANDIDATES: usize = 8;

fn position(assessment: &Assessment, seeders: u64, digest: &[u8; 32]) -> Position {
    let rank = &assessment.rank;
    let mut bytes = [0; POSITION_BYTES];
    bytes[0] = u8::from(!assessment.accepted);
    // Descending fields are inverted; the signed score is biased first.
    for (at, value) in [
        rank.custom_score.cast_unsigned() ^ 0x8000_0000,
        rank.resolution,
        rank.source,
        rank.codec,
        rank.language,
    ]
    .into_iter()
    .enumerate()
    {
        bytes[1 + at * 4..5 + at * 4].copy_from_slice(&(!value).to_be_bytes());
    }
    bytes[21..29].copy_from_slice(&(!seeders).to_be_bytes());
    bytes[29..].copy_from_slice(digest);
    bytes
}

fn decode_position(text: &str) -> Result<Position> {
    if text.len() != POSITION_BYTES * 2
        || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err("Search: invalid candidate cursor".into());
    }
    let mut position = [0; POSITION_BYTES];
    for (i, byte) in position.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
            .map_err(|_| "Search: invalid candidate cursor")?;
    }
    if position[0] > 1 {
        return Err("Search: invalid candidate cursor".into());
    }
    Ok(position)
}

/// One page of a search report: at most `limit` candidates after the
/// exclusive `after` cursor, in report order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchPage {
    pub after: Option<String>,
    pub limit: usize,
}

impl Default for SearchPage {
    fn default() -> Self {
        Self {
            after: None,
            limit: DEFAULT_REPORT_PAGE,
        }
    }
}

impl SearchPage {
    /// Parses `after=CURSOR&limit=N` (1 to 200, 100 by default).
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() > 256 {
            return Err("Search: page query is too large".into());
        }
        let mut page = Self::default();
        let mut seen = BTreeSet::new();
        for pair in text.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').ok_or("Search: invalid page query")?;
            if !seen.insert(name) {
                return Err("Search: duplicate page query field".into());
            }
            match name {
                "after" => {
                    decode_position(value)?;
                    page.after = Some(value.to_owned());
                }
                "limit" => {
                    page.limit = value
                        .parse()
                        .ok()
                        .filter(|limit| {
                            (1..=MAX_REPORT_PAGE).contains(limit) && !value.starts_with('0')
                        })
                        .ok_or("Search: page limit must be 1 to 200")?;
                }
                _ => return Err("Search: unknown page query field".into()),
            }
        }
        Ok(page)
    }
}

/// Bounded state of one search. Candidates are assessed as each indexer
/// response arrives; only the selection, the requested report window and the
/// accepted pack shortlist are retained, so no total caps the candidates.
struct SearchResult {
    profile: String,
    total: u64,
    best: Option<(Position, Candidate)>,
    after: Option<Position>,
    /// Report page size; the window keeps one more to know a next page exists.
    limit: usize,
    window: BTreeMap<Position, Candidate>,
    pack_limit: usize,
    packs: BTreeMap<Position, Candidate>,
    configured: usize,
    successful: usize,
    /// The candidate an operator chose, retained whatever its assessment.
    pick: Option<String>,
    picked: Option<Candidate>,
}

impl SearchResult {
    fn offer(&mut self, position: Position, candidate: Candidate) {
        self.total = self.total.saturating_add(1);
        if self.pick.as_deref() == Some(candidate.id.as_str()) {
            self.picked = Some(candidate.clone());
        }
        if candidate.assessment.accepted {
            if self.pack_limit > 0 {
                self.packs.insert(position, candidate.clone());
                if self.packs.len() > self.pack_limit {
                    self.packs.pop_last();
                }
            }
            if self.best.as_ref().is_none_or(|(best, _)| position < *best) {
                self.best = Some((position, candidate.clone()));
            }
        }
        if self.limit > 0 && self.after.is_none_or(|after| position > after) {
            self.window.insert(position, candidate);
            if self.window.len() > self.limit + 1 {
                self.window.pop_last();
            }
        }
    }

    fn report(&self) -> Value {
        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        for candidate in self.window.values().take(self.limit) {
            if candidate.assessment.accepted {
                accepted.push(candidate.to_json());
            } else {
                rejected.push(candidate.to_json());
            }
        }
        let mut indexers = Value::object();
        indexers.insert("configured", Value::Number(self.configured as f64));
        indexers.insert("successful", Value::Number(self.successful as f64));
        indexers.insert(
            "failed",
            Value::Number((self.configured - self.successful) as f64),
        );
        let mut report = Value::object();
        report.insert("profile", report_text(&self.profile, 128));
        report.insert("manual_override", false);
        report.insert("indexers", indexers);
        report.insert("accepted", Value::Array(accepted));
        report.insert("rejected", Value::Array(rejected));
        report.insert("candidate_count", Value::Number(self.total as f64));
        report.insert(
            "reported_count",
            Value::Number(self.window.len().min(self.limit) as f64),
        );
        report.insert(
            "next",
            if self.window.len() > self.limit {
                self.window
                    .keys()
                    .nth(self.limit - 1)
                    .map_or(Value::Null, |position| hex(position).into())
            } else {
                Value::Null
            },
        );
        report.insert(
            "selected_candidate_id",
            self.best
                .as_ref()
                .map_or(Value::Null, |(_, candidate)| candidate.id.clone().into()),
        );
        report
    }
}

/// The identity, profile, provenance and seed gates shared by every automatic
/// acquisition path: request-driven search and feed watching.
fn assess_release(
    config: &Config,
    profile_name: &str,
    profile: &Profile,
    request: &Request,
    release: &Release,
    pack: bool,
) -> Assessment {
    let mut assessment = profile.assess(&release.title, &request.title);
    if (RecordedRelease {
        title: release.title.clone(),
        profile: profile_name.into(),
    })
    .validate()
    .is_err()
    {
        assessment.accepted = false;
        assessment
            .reasons
            .push("Release title or profile is invalid for acquisition provenance".into());
    }
    if !(if pack {
        crate::pack::season_title_matches(request, &release.title)
    } else {
        release_matches(request, &release.title)
    }) {
        assessment.accepted = false;
        assessment.reasons.push(
            if pack {
                "Release does not match the requested series, year or single season pack"
            } else {
                "Release does not match the requested title, year or episode"
            }
            .into(),
        );
    }
    if release.seeders < config.minimum_seeders {
        assessment.accepted = false;
        assessment
            .reasons
            .push("Release has fewer than the minimum seeders".into());
    }
    assessment
}

fn search_candidates(config: &Config, request: &Request) -> Result<SearchResult> {
    search_candidates_since(config, request, Instant::now())
}

fn search_candidates_since(
    config: &Config,
    request: &Request,
    started: Instant,
) -> Result<SearchResult> {
    let deadline = started
        .checked_add(SEARCH_BUDGET)
        .ok_or("Search: invalid search time budget")?;
    search_candidates_before(config, request, deadline)
}

fn search_candidates_before(
    config: &Config,
    request: &Request,
    deadline: Instant,
) -> Result<SearchResult> {
    search_candidates_mode(config, request, deadline, false, None, 0, None)
}

/// A search keeps the selection, at most `limit + 1` report candidates after
/// `after`, for packs the accepted shortlist and, when given, the candidate
/// `pick`; `limit` 0 only selects.
fn search_candidates_mode(
    config: &Config,
    request: &Request,
    deadline: Instant,
    pack: bool,
    after: Option<Position>,
    limit: usize,
    pick: Option<&str>,
) -> Result<SearchResult> {
    remaining_search_time(deadline)?;
    request.validate()?;
    if (!pack && !matches!(request.kind.as_str(), "movie" | "episode"))
        || (pack && request.kind != "series")
    {
        return Err("Search: unsupported search identity".into());
    }
    let (profile_name, profile) =
        config
            .selection
            .profile(if pack { "episode" } else { &request.kind })?;
    if config.sources.is_empty() {
        return Err("Search: no indexer configured".into());
    }
    if config.sources.len() > 1_000 {
        return Err("Search: too many indexers".into());
    }
    let mut result = SearchResult {
        profile: profile_name.into(),
        total: 0,
        best: None,
        after,
        limit,
        window: BTreeMap::new(),
        pack_limit: if pack { PACK_CANDIDATES } else { 0 },
        packs: BTreeMap::new(),
        configured: config.sources.len(),
        successful: 0,
        pick: pick.map(str::to_owned),
        picked: None,
    };
    let mut used_sources = Vec::new();
    for source in &config.sources {
        remaining_search_time(deadline)?;
        if let Ok(releases) = source_releases(source, request, deadline) {
            used_sources.push(source);
            result.successful += 1;
            // Hash and bound source metadata once, rather than duplicating an
            // arbitrarily long configured label for every indexer result.
            let source_identity = candidate_id(
                profile_name,
                &source.name,
                &Release {
                    title: String::new(),
                    url: source.url.clone(),
                    seeders: 0,
                },
            );
            let source_label = report_text(&source.name, 128);
            for (index, release) in releases.into_iter().enumerate() {
                if index % 64 == 0 {
                    remaining_search_time(deadline)?;
                }
                let assessment =
                    assess_release(config, profile_name, profile, request, &release, pack);
                let digest = candidate_digest(profile_name, &source_identity, &release);
                result.offer(
                    position(&assessment, release.seeders, &digest),
                    Candidate {
                        id: hex(&digest),
                        source: source_label.clone(),
                        release,
                        assessment,
                    },
                );
            }
        }
    }
    remaining_search_time(deadline)?;
    if result.successful == 0 {
        return Err("Search: no indexer returned a usable response".into());
    }
    // A search can query several sources sharing one owner. Take its lease only
    // once: recursively taking read leases can deadlock a waiting revocation.
    let mut protected_networks = Vec::new();
    for source in used_sources {
        if source.options.protection == crate::source_network::Protection::Protected {
            let network = source
                .options
                .network
                .as_ref()
                .ok_or("Source: protected network unavailable")?;
            if !protected_networks
                .iter()
                .any(|previous| Arc::ptr_eq(previous, network))
            {
                protected_networks.push(network.clone());
            }
        }
    }
    let _publications: Vec<_> = protected_networks
        .iter()
        .map(|network| crate::source_network::Protection::Protected.publication(Some(network)))
        .collect::<Result<_>>()?;
    Ok(result)
}

fn remaining_search_time(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| SEARCH_EXPIRED.into())
}

/// Explains source selection without downloading, storing or exposing URLs.
/// Usable indexers with no accepted release produce an empty selection.
pub fn search_report(config: &Config, request: &Request) -> Result<Value> {
    search_report_page(config, request, &SearchPage::default())
}

/// One page of the report. Each page repeats the live search, so it reflects
/// the indexers at that moment; the selection always considers every
/// candidate, not only those of the page.
pub fn search_report_page(config: &Config, request: &Request, page: &SearchPage) -> Result<Value> {
    request.validate()?;
    if page.limit == 0 || page.limit > MAX_REPORT_PAGE {
        return Err("Search: page limit must be 1 to 200".into());
    }
    let after = page.after.as_deref().map(decode_position).transpose()?;
    if request.source_path.is_some() || request.source_url.is_some() {
        if let Some(source) = &request.source_url {
            acquisition_url(source, &config.plex.url)?;
        }
        let mut report = Value::object();
        report.insert("profile", Value::Null);
        report.insert("manual_override", true);
        report.insert("accepted", Value::Array(Vec::new()));
        report.insert("rejected", Value::Array(Vec::new()));
        report.insert("candidate_count", Value::Number(0.0));
        report.insert("reported_count", Value::Number(0.0));
        report.insert("next", Value::Null);
        report.insert("selected_candidate_id", Value::Null);
        let mut indexers = Value::object();
        indexers.insert("configured", Value::Number(config.sources.len() as f64));
        indexers.insert("successful", Value::Number(0.0));
        indexers.insert("failed", Value::Number(0.0));
        report.insert("indexers", indexers);
        return Ok(report);
    }
    let deadline = Instant::now()
        .checked_add(SEARCH_BUDGET)
        .ok_or("Search: invalid search time budget")?;
    search_candidates_mode(config, request, deadline, false, after, page.limit, None)
        .map(|result| result.report())
}

/// Exact identity and profile preferences precede seed count. A manually
/// supplied acquisition URL retains its explicit override semantics.
pub fn search(config: &Config, request: &Request) -> Result<String> {
    request.validate()?;
    if let Some(source) = &request.source_url {
        return acquisition_url(source, &config.plex.url);
    }
    selected_release(search_candidates(config, request)?).map(|release| release.url)
}

/// Acquisition metadata for an automatically selected release. The URL is
/// private acquisition data and must never be included in public previews.
pub struct SelectedRelease {
    pub url: String,
    pub title: String,
    pub profile: String,
    pub assessment: Assessment,
    pub id: String,
}

fn selected_release(result: SearchResult) -> Result<SelectedRelease> {
    let (_, candidate) = result
        .best
        .ok_or("Search: no release satisfies the identity, seed count and selection profile")?;
    Ok(SelectedRelease {
        url: candidate.release.url,
        title: candidate.release.title,
        profile: result.profile,
        assessment: candidate.assessment,
        id: candidate.id,
    })
}

/// Separate season-pack identity rules retain the episode profile and normal
/// ranking. The report is the first page; the shortlist holds the best
/// `PACK_CANDIDATES` accepted releases.
pub(crate) fn search_pack_candidates(
    config: &Config,
    request: &Request,
    deadline: Instant,
) -> Result<(Value, Vec<SelectedRelease>)> {
    let result = search_candidates_mode(
        config,
        request,
        deadline,
        true,
        None,
        DEFAULT_REPORT_PAGE,
        None,
    )?;
    let report = result.report();
    let profile = result.profile;
    let candidates = result
        .packs
        .into_values()
        .map(|candidate| SelectedRelease {
            url: candidate.release.url,
            title: candidate.release.title,
            profile: profile.clone(),
            assessment: candidate.assessment,
            id: candidate.id,
        })
        .collect();
    Ok((report, candidates))
}

/// Selects the same accepted, ranked candidate as search and search_report,
/// retaining its assessment for upgrade decisions and durable history.
pub fn select_release(config: &Config, request: &Request) -> Result<SelectedRelease> {
    let deadline = Instant::now()
        .checked_add(SEARCH_BUDGET)
        .ok_or("Search: invalid search time budget")?;
    select_release_before(config, request, deadline)
}

/// Applies an existing monitoring-pass deadline to all indexer requests and
/// candidate processing. Exceeding it discards the entire partial search.
pub fn select_release_before(
    config: &Config,
    request: &Request,
    deadline: Instant,
) -> Result<SelectedRelease> {
    let deadline = deadline.min(
        Instant::now()
            .checked_add(SEARCH_BUDGET)
            .ok_or("Search: invalid search time budget")?,
    );
    remaining_search_time(deadline)?;
    request.validate()?;
    if request.source_path.is_some() || request.source_url.is_some() {
        return Err("Search: manual sources do not have a verified release assessment".into());
    }
    selected_release(search_candidates_before(config, request, deadline)?)
}

/// The release an operator chose from a search report, found again by its
/// candidate ID in a fresh search. A rejected release may be chosen; the
/// returned flag says whether the profile accepted it.
pub fn pick_release_before(
    config: &Config,
    request: &Request,
    id: &str,
    deadline: Instant,
) -> Result<(SelectedRelease, bool)> {
    let deadline = deadline.min(
        Instant::now()
            .checked_add(SEARCH_BUDGET)
            .ok_or("Search: invalid search time budget")?,
    );
    request.validate()?;
    if request.source_path.is_some() || request.source_url.is_some() {
        return Err("Search: manual sources cannot be searched".into());
    }
    if id.len() != 64 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("Search: invalid candidate ID".into());
    }
    let result = search_candidates_mode(config, request, deadline, false, None, 0, Some(id))?;
    let candidate = result
        .picked
        .ok_or("Search: this release is no longer offered by the indexers; search again")?;
    RecordedRelease {
        title: candidate.release.title.clone(),
        profile: result.profile.clone(),
    }
    .validate()?;
    let accepted = candidate.assessment.accepted;
    Ok((
        SelectedRelease {
            url: candidate.release.url,
            title: candidate.release.title,
            profile: result.profile,
            assessment: candidate.assessment,
            id: candidate.id,
        },
        accepted,
    ))
}

fn plex_section<'a>(config: &'a Config, request: &Request) -> Result<&'a str> {
    let section = if matches!(request.kind.as_str(), "episode" | "series") {
        &config.plex.series_section
    } else {
        &config.plex.movies_section
    };
    if section.is_empty() || !section.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Plex: invalid section number".into());
    }
    Ok(section)
}

/// Asks Plex to scan the section after the atomic import.
pub fn refresh(config: &Config, request: &Request) -> Result<()> {
    if !config.plex.enabled {
        return Ok(());
    }
    let headers = plex_headers(config)?;
    let url = endpoint(
        &config.plex.url,
        &format!(
            "library/sections/{}/refresh",
            plex_section(config, request)?
        ),
    )?;
    let response = client()
        .request("GET", &url, &headers, &[])
        .map_err(|_| "Plex: refresh failed")?;
    if !(200..300).contains(&response.status) {
        return Err(format!("Plex: refresh HTTP response {}", response.status));
    }
    Ok(())
}

fn identity_matches(item: &Value, request: &Request) -> bool {
    if let (Some(expected), Some(actual)) = (request.tmdb_id, tmdb_guid(item)) {
        return expected == actual;
    }
    string(item, "title").is_some_and(|title| tokens(title) == tokens(&request.title))
        && (request.year == 0 || integer(item, "year") == Some(u64::from(request.year)))
}

fn playable(item: &Value) -> bool {
    item.get("Media")
        .and_then(Value::as_array)
        .is_some_and(|media| {
            media.iter().any(|media| {
                media
                    .get("Part")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|part| string(part, "file").is_some_and(|s| !s.is_empty()))
                    })
            })
        })
}

/// The job remains pending until a readable entry has been indexed.
pub fn available(config: &Config, request: &Request) -> Result<bool> {
    if !config.plex.enabled {
        return Ok(true);
    }
    plex_available(config, request, playable)
}

/// Existing Plex media can fulfill routed demand only below its captured root.
pub(crate) fn available_destination(config: &Config, request: &Request) -> Result<bool> {
    available_destination_before(config, request, Instant::now() + SEARCH_BUDGET)
}
fn available_destination_before(
    config: &Config,
    request: &Request,
    deadline: Instant,
) -> Result<bool> {
    let root = if request.kind == "episode" {
        &config.series_root
    } else {
        &config.movies_root
    };
    let text = root.to_str().ok_or("Plex: invalid routed library root")?;
    let root = absolute_path_components(text).ok_or("Plex: invalid routed library root")?;
    let mut routes = Vec::new();
    for mapping in &config.plex.path_mappings {
        routes.push((
            absolute_path_components(&mapping.mynou_prefix).ok_or("Plex: invalid path mapping")?,
            absolute_path_components(&mapping.plex_prefix).ok_or("Plex: invalid path mapping")?,
        ));
    }
    routes.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    let expected = routes
        .iter()
        .find(|(from, _)| root.starts_with(from))
        .map_or_else(
            || root.clone(),
            |(from, to)| {
                let mut result = to.clone();
                result.extend_from_slice(&root[from.len()..]);
                result
            },
        );
    plex_available_before(
        config,
        request,
        |item| {
            item.get("Media")
                .and_then(Value::as_array)
                .is_some_and(|media| {
                    media.iter().any(|m| {
                        m.get("Part")
                            .and_then(Value::as_array)
                            .is_some_and(|parts| {
                                parts.iter().any(|p| {
                                    string(p, "file")
                                        .and_then(absolute_path_components)
                                        .is_some_and(|path| {
                                            path.len() > expected.len()
                                                && path.starts_with(&expected)
                                        })
                                })
                            })
                    })
                })
        },
        deadline,
    )
}

/// IRC preflight shares one deadline with metadata inspection and uses captured routing.
pub(crate) fn available_before(
    config: &Config,
    request: &Request,
    routed: bool,
    deadline: Instant,
) -> Result<bool> {
    if routed {
        available_destination_before(config, request, deadline)
    } else {
        plex_available_before(config, request, playable, deadline)
    }
}

/// Checks Plex for every newly imported path, rather than accepting an older
/// playable copy of the same movie or episode. Paths use absolute POSIX
/// components; they are never compared by basename or suffix.
pub fn available_import(config: &Config, request: &Request, imports: &[String]) -> Result<bool> {
    if imports.is_empty() {
        return Ok(false);
    }
    if !config.plex.enabled {
        return Ok(true);
    }
    request.validate()?;
    if !matches!(request.kind.as_str(), "movie" | "episode") {
        return Err("Plex: import confirmation requires a movie or episode".into());
    }
    // Import paths are bounded by their total bytes below, not by a count.
    if config.plex.path_mappings.len() > 32 {
        return Err("Plex: too many path mappings".into());
    }
    let mut mappings = Vec::new();
    let mut prefixes = BTreeSet::new();
    for mapping in &config.plex.path_mappings {
        let local = absolute_path_components(&mapping.mynou_prefix)
            .ok_or("Plex: invalid local path mapping prefix")?;
        let remote = absolute_path_components(&mapping.plex_prefix)
            .ok_or("Plex: invalid Plex path mapping prefix")?;
        if !prefixes.insert(&mapping.mynou_prefix) {
            return Err("Plex: duplicate local path mapping prefix".into());
        }
        mappings.push((local, remote));
    }
    let mut expected = BTreeSet::new();
    let mut path_bytes = 0usize;
    for imported in imports {
        let components = absolute_path_components(imported)
            .filter(|components| !components.is_empty())
            .ok_or("Plex: invalid imported file path")?;
        let mapped = mappings
            .iter()
            .filter(|(local, _)| components.starts_with(local))
            .max_by_key(|(local, _)| local.len());
        let path = if let Some((local, remote)) = mapped {
            let mut mapped_components = remote.clone();
            mapped_components.extend_from_slice(&components[local.len()..]);
            format!("/{}", mapped_components.join("/"))
        } else {
            imported.clone()
        };
        if absolute_path_components(&path).is_none_or(|components| components.is_empty()) {
            return Err("Plex: invalid mapped import path".into());
        }
        path_bytes = path_bytes.saturating_add(path.len());
        if path_bytes > MAX_XML {
            return Err("Plex: imported path data exceeds the memory limit".into());
        }
        expected.insert(path);
    }
    plex_available(config, request, |item| {
        if let Some(media) = item.get("Media").and_then(Value::as_array) {
            for media in media {
                if let Some(parts) = media.get("Part").and_then(Value::as_array) {
                    for part in parts {
                        if let Some(path) = string(part, "file")
                            .filter(|path| absolute_path_components(path).is_some())
                        {
                            expected.remove(path);
                        }
                    }
                }
            }
        }
        expected.is_empty()
    })
}

fn absolute_path_components(path: &str) -> Option<Vec<&str>> {
    if path.len() > 4096
        || !path.starts_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
    {
        return None;
    }
    if path == "/" {
        return Some(Vec::new());
    }
    let components: Vec<_> = path[1..].split('/').collect();
    if components
        .iter()
        .any(|component| matches!(*component, "" | "." | ".."))
    {
        return None;
    }
    Some(components)
}

fn plex_available(
    config: &Config,
    request: &Request,
    matches: impl FnMut(&Value) -> bool,
) -> Result<bool> {
    plex_available_before(config, request, matches, Instant::now() + SEARCH_BUDGET)
}
fn plex_available_before(
    config: &Config,
    request: &Request,
    mut matches: impl FnMut(&Value) -> bool,
    deadline: Instant,
) -> Result<bool> {
    remaining_search_time(deadline)?;
    let headers = plex_headers(config)?;
    let url = endpoint(
        &config.plex.url,
        &format!("library/sections/{}/all", plex_section(config, request)?),
    )?;
    let episode = request.kind == "episode";
    let url = query(
        &url,
        &[
            ("type", if episode { "2" } else { "1" }.into()),
            ("title", request.title.clone()),
        ],
    )?;
    plex_find_before(&url, &headers, deadline, |item| {
        if !identity_matches(item, request) {
            return Ok(false);
        }
        remaining_search_time(deadline)?;
        if !episode {
            return Ok(matches(item));
        }
        let key = string(item, "ratingKey").ok_or("Plex: missing series ratingKey")?;
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
            return Err("Plex: invalid ratingKey".into());
        }
        let leaves = endpoint(
            &config.plex.url,
            &format!("library/metadata/{key}/allLeaves"),
        )?;
        plex_find_before(&leaves, &headers, deadline, |episode| {
            remaining_search_time(deadline)?;
            Ok(
                integer(episode, "parentIndex") == Some(u64::from(request.season))
                    && integer(episode, "index") == Some(u64::from(request.episode))
                    && matches(episode),
            )
        })
    })
}

struct RssItem {
    guid: Option<String>,
    release: Release,
    size: Option<u64>,
    info_hash: Option<String>,
}

/// Valid items in document order, and the number of items without a usable
/// title or acquisition URL.
fn rss_items(text: &str, base: &str) -> Result<(Vec<RssItem>, u32)> {
    let root = parse_xml(text)?;
    if root.local() == "error" {
        return Err("Torznab indexer: service error".into());
    }
    let channel = if root.local() == "rss" {
        root.child("channel").ok_or("RSS: missing channel")?
    } else {
        return Err("RSS: expected an rss root element".into());
    };
    let mut out = Vec::new();
    let mut skipped = 0_u32;
    for item in channel
        .children
        .iter()
        .filter(|item| item.local() == "item")
    {
        let Some(title) = item
            .child("title")
            .map(|v| v.text.trim())
            .filter(|s| !s.is_empty() && s.len() <= 2048)
        else {
            skipped = skipped.saturating_add(1);
            continue;
        };
        let mut url = item
            .child("enclosure")
            .and_then(|v| v.attrs.get("url"))
            .map(String::as_str);
        let mut seeders = item
            .child("seeders")
            .and_then(|v| v.text.trim().parse().ok())
            .unwrap_or(0);
        let mut size = item
            .child("size")
            .and_then(|v| v.text.trim().parse().ok())
            .or_else(|| {
                item.child("enclosure")
                    .and_then(|v| v.attrs.get("length"))
                    .and_then(|v| v.parse().ok())
            })
            .filter(|size| *size > 0);
        let mut info_hash = None;
        for attr in item.children.iter().filter(|child| child.local() == "attr") {
            match attr.attrs.get("name").map(String::as_str) {
                Some("seeders") => {
                    seeders = attr
                        .attrs
                        .get("value")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0)
                }
                Some("size") => {
                    size = attr
                        .attrs
                        .get("value")
                        .and_then(|v| v.parse().ok())
                        .filter(|size| *size > 0)
                        .or(size)
                }
                Some("infohash") => {
                    info_hash = attr
                        .attrs
                        .get("value")
                        .map(|v| v.trim().to_ascii_lowercase())
                        .filter(|v| v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                }
                Some("magneturl") if url.is_none() => {
                    url = attr.attrs.get("value").map(String::as_str)
                }
                _ => {}
            }
        }
        let url = url.or_else(|| item.child("link").map(|v| v.text.trim()));
        let Some(url) = url else {
            skipped = skipped.saturating_add(1);
            continue;
        };
        let Ok(url) = acquisition_url(url, base) else {
            skipped = skipped.saturating_add(1);
            continue;
        };
        out.push(RssItem {
            guid: item
                .child("guid")
                .map(|v| v.text.trim())
                .filter(|s| !s.is_empty() && s.len() <= 2048)
                .map(str::to_owned),
            release: Release {
                title: title.into(),
                url,
                seeders,
            },
            size,
            info_hash,
        });
    }
    Ok((out, skipped))
}

fn rss_releases(text: &str, base: &str) -> Result<Vec<Release>> {
    Ok(rss_items(text, base)?
        .0
        .into_iter()
        .map(|item| item.release)
        .collect())
}

/// One entry observed on a watched feed. Its identity is an opaque, bounded
/// digest that never reveals the title, URL or any credential.
#[derive(Clone, Debug)]
pub(crate) struct FeedEntry {
    pub(crate) id: String,
    pub(crate) release: Release,
}

#[derive(Debug)]
pub(crate) struct FeedPoll {
    pub(crate) entries: Vec<FeedEntry>,
    pub(crate) skipped: u32,
    pub(crate) truncated: bool,
}

pub(crate) const MAX_FEED_ENTRIES: usize = 500;

/// A GUID is the feed's own stable identity; without one the title and URL
/// identify the entry.
fn entry_identity(guid: Option<&str>, release: &Release) -> String {
    let mut hash = Sha256::new();
    let fields: Vec<&str> = match guid {
        Some(guid) => vec!["guid", guid],
        None => vec!["release", release.title.as_str(), release.url.as_str()],
    };
    for field in fields {
        hash.update(&(field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(32);
    for byte in &hash.finalize()[..16] {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 15) as usize] as char);
    }
    id
}

fn feed_entries(text: &str, base: &str) -> Result<FeedPoll> {
    let (items, skipped) = rss_items(text, base)?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in items {
        if entries.len() == MAX_FEED_ENTRIES {
            truncated = true;
            break;
        }
        entries.push(FeedEntry {
            id: entry_identity(item.guid.as_deref(), &item.release),
            release: item.release,
        });
    }
    Ok(FeedPoll {
        entries,
        skipped,
        truncated,
    })
}

/// JSON entries carry no GUID: the title and acquisition URL identify them.
fn json_feed_entries(text: &str, base: &str, mapping: &Mapping) -> Result<FeedPoll> {
    let value = json::parse(text)?;
    let listed = mapping
        .list(&value)
        .ok_or("JSON indexer: missing results/items list")?
        .len();
    let releases = json_releases(&value, base, mapping)?;
    let skipped = (listed - releases.len()).min(u32::MAX as usize) as u32;
    let truncated = releases.len() > MAX_FEED_ENTRIES;
    let entries = releases
        .into_iter()
        .take(MAX_FEED_ENTRIES)
        .map(|release| FeedEntry {
            id: entry_identity(None, &release),
            release,
        })
        .collect();
    Ok(FeedPoll {
        entries,
        skipped,
        truncated,
    })
}

/// Fixed, redacted outcome for a failed source request.
fn fetch_outcome(error: &str) -> &'static str {
    match error {
        "Indexer: source is busy" => "busy",
        "Indexer: source is disabled" => "disabled",
        "Indexer: source rate limit reached" => "rate_limited",
        "Indexer: authentication rejected" | "Indexer: authentication unavailable" => {
            "authentication_failed"
        }
        "Indexer: response rejected" => "response_rejected",
        "Indexer: network request failed" => "network_failed",
        "Indexer: source policy changed during the request" => "policy_changed",
        "Indexer: request budget expired" => "timed_out",
        _ => "request_failed",
    }
}

/// Fetches the newest entries of an RSS, Torznab or JSON source once. The
/// result is bounded and holds no storage lock; failures carry only a fixed
/// outcome code.
pub(crate) fn poll_feed(
    source: &Source,
    deadline: Instant,
) -> std::result::Result<FeedPoll, &'static str> {
    let response = feed_response(source, deadline)?;
    let parsed = std::str::from_utf8(&response)
        .map_err(|_| "parse_rejected")
        .and_then(|text| {
            if source.kind == "json" {
                json_feed_entries(text, &source.url, &source.options.mapping)
            } else {
                feed_entries(text, &source.url)
            }
            .map_err(|_| "parse_rejected")
        });
    crate::indexers::parsed(source, parsed.is_ok());
    parsed
}

/// One query-less read of a source's newest entries. A JSON source sends only
/// its fixed parameters and API key, so its mapping decides the ordering.
fn feed_response(source: &Source, deadline: Instant) -> std::result::Result<Vec<u8>, &'static str> {
    if source.kind == "json" {
        return json_text_search(source, None, deadline).map_err(|error| fetch_outcome(&error));
    }
    let mut pairs: Vec<(&str, String)> = Vec::new();
    match source.kind.as_str() {
        "torznab" => pairs.push(("t", "search".into())),
        "rss" => {}
        _ => return Err("unsupported_format"),
    }
    if let Some(key) =
        config::optional_secret(&source.api_key_env).map_err(|_| "authentication_failed")?
    {
        pairs.push(("apikey", key));
    }
    let url = query(&source.url, &pairs).map_err(|_| "request_failed")?;
    let response = crate::indexers::fetch(
        source,
        &url,
        "application/rss+xml, application/xml",
        deadline,
    )
    .map_err(|error| fetch_outcome(&error))?;
    remaining_search_time(deadline).map_err(|_| "timed_out")?;
    Ok(response.body)
}

/// A release offered for cross-seed discovery. Its URL may carry a credential:
/// it is used once for the metainfo download and never stored or reported.
pub(crate) struct Offer {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) url: String,
    /// Payload bytes advertised by the source, when it states them.
    pub(crate) size: Option<u64>,
    pub(crate) info_hash: Option<String>,
}

/// One bounded feed read (`text` is `None`) or Torznab text search. Failures
/// carry only a fixed outcome code.
pub(crate) fn offers_before(
    source: &Source,
    text: Option<&str>,
    deadline: Instant,
) -> std::result::Result<Vec<Offer>, &'static str> {
    match (source.kind.as_str(), text) {
        ("torznab", _) | ("rss", None) => {}
        ("json", _) => return json_offers_before(source, text, deadline),
        _ => return Err("unsupported_format"),
    }
    let mut pairs: Vec<(&str, String)> = Vec::new();
    if source.kind == "torznab" {
        pairs.push(("t", "search".into()));
    }
    if let Some(text) = text {
        pairs.push(("q", text.to_owned()));
    }
    if let Some(key) =
        config::optional_secret(&source.api_key_env).map_err(|_| "authentication_failed")?
    {
        pairs.push(("apikey", key));
    }
    let url = query(&source.url, &pairs).map_err(|_| "request_failed")?;
    let response = crate::indexers::fetch(
        source,
        &url,
        "application/rss+xml, application/xml",
        deadline,
    )
    .map_err(|error| fetch_outcome(&error))?;
    remaining_search_time(deadline).map_err(|_| "timed_out")?;
    let parsed = std::str::from_utf8(&response.body)
        .map_err(|_| "parse_rejected")
        .and_then(|text| rss_items(text, &source.url).map_err(|_| "parse_rejected"));
    crate::indexers::parsed(source, parsed.is_ok());
    Ok(parsed?
        .0
        .into_iter()
        .take(MAX_FEED_ENTRIES)
        .map(|item| Offer {
            id: entry_identity(item.guid.as_deref(), &item.release),
            title: item.release.title,
            url: item.release.url,
            size: item.size,
            info_hash: item.info_hash,
        })
        .collect())
}

fn json_offers_before(
    source: &Source,
    text: Option<&str>,
    deadline: Instant,
) -> std::result::Result<Vec<Offer>, &'static str> {
    let body = json_text_search(source, text, deadline).map_err(|error| fetch_outcome(&error))?;
    let mapping = &source.options.mapping;
    let parsed = (|| {
        let value = json::parse(std::str::from_utf8(&body).map_err(|_| "Indexer: invalid UTF-8")?)?;
        let items = mapping
            .list(&value)
            .ok_or("JSON indexer: missing results/items list")?;
        if items.len() > MAX_ITEMS {
            return Err("JSON indexer: too many results".to_owned());
        }
        let mut out = Vec::new();
        for item in items {
            if out.len() == MAX_FEED_ENTRIES {
                break;
            }
            let Some(release) = json_release(item, &source.url, mapping) else {
                continue;
            };
            let (_, _, hashes) = mapping.details(item);
            out.push(Offer {
                id: entry_identity(None, &release),
                title: release.title,
                url: release.url,
                size: mapping.size(item),
                info_hash: match hashes.as_slice() {
                    [hash] => Some(hash.trim().to_ascii_lowercase())
                        .filter(|v| v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit())),
                    _ => None,
                },
            });
        }
        Ok(out)
    })();
    crate::indexers::parsed(source, parsed.is_ok());
    parsed.map_err(|_: String| "parse_rejected")
}

/// A watched release that satisfies the request's identity and profile gates.
#[derive(Debug)]
pub(crate) struct WatchCandidate {
    pub(crate) url: String,
    pub(crate) title: String,
    pub(crate) profile: String,
    pub(crate) assessment: Assessment,
    pub(crate) seeders: u64,
}

impl WatchCandidate {
    /// Larger rank wins, then seeders; remaining ties are deterministic.
    pub(crate) fn beats(&self, other: &Self) -> bool {
        (&self.assessment.rank, self.seeders)
            .cmp(&(&other.assessment.rank, other.seeders))
            .then_with(|| (&other.title, &other.url).cmp(&(&self.title, &self.url)))
            .is_gt()
    }
}

/// Applies the same gates as an indexer search to one watched entry. A request
/// whose manual source overrides discovery never matches.
pub(crate) fn assess_feed_entry(
    config: &Config,
    request: &Request,
    entry: &FeedEntry,
) -> Option<WatchCandidate> {
    if request.source_path.is_some()
        || request.source_url.is_some()
        || !release_matches(request, &entry.release.title)
    {
        return None;
    }
    let (profile_name, profile) = config.selection.profile(&request.kind).ok()?;
    let assessment = assess_release(
        config,
        profile_name,
        profile,
        request,
        &entry.release,
        false,
    );
    assessment.accepted.then(|| WatchCandidate {
        url: entry.release.url.clone(),
        title: entry.release.title.clone(),
        profile: profile_name.into(),
        assessment,
        seeders: entry.release.seeders,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_positions_order_like_reports_and_round_trip_as_cursors() {
        let assessment = |accepted: bool, custom_score: i32, resolution: u32| Assessment {
            accepted,
            reasons: Vec::new(),
            rank: crate::selection::Rank {
                custom_score,
                resolution,
                ..Default::default()
            },
            attributes: Default::default(),
        };
        let low = [0; 32];
        let high = [255; 32];
        let ordered = [
            position(&assessment(true, 7, 0), 0, &low),
            position(&assessment(true, -3, 9), u64::MAX, &low),
            position(&assessment(true, -3, 1), 9, &low),
            position(&assessment(true, -3, 1), 2, &low),
            position(&assessment(true, -3, 1), 2, &high),
            position(&assessment(false, i32::MAX, u32::MAX), u64::MAX, &low),
            position(&assessment(false, i32::MIN, 0), 0, &high),
        ];
        assert!(ordered.windows(2).all(|pair| pair[0] < pair[1]));
        for position in ordered {
            assert_eq!(decode_position(&hex(&position)).unwrap(), position);
        }
    }

    #[test]
    fn feed_identities_ignore_order_prefer_guids_and_count_invalid_items() {
        let first = "<item><title>A.Movie.2024</title><guid>g-1</guid><link>magnet:?xt=urn:btih:aa</link></item>";
        let second = "<item><title>B.Movie.2024</title><link>magnet:?xt=urn:btih:bb</link></item>";
        let untitled = "<item><link>magnet:?xt=urn:btih:cc</link></item>";
        let document = |items: &str| format!("<rss><channel>{items}</channel></rss>");
        let base = "http://127.0.0.1/feed";
        let one = feed_entries(&document(&format!("{first}{second}{untitled}")), base).unwrap();
        let two = feed_entries(&document(&format!("{second}{first}")), base).unwrap();
        assert_eq!((one.skipped, two.skipped), (1, 0));
        assert_eq!(one.entries[0].id.len(), 32);
        assert_eq!(one.entries[0].id, two.entries[1].id);
        assert_eq!(one.entries[1].id, two.entries[0].id);
        assert_ne!(one.entries[0].id, one.entries[1].id);
        // A feed GUID survives a changed download link; without one the link counts.
        let moved = feed_entries(&document(&first.replace("aa", "dd")), base).unwrap();
        assert_eq!(moved.entries[0].id, one.entries[0].id);
        let moved = feed_entries(&document(&second.replace("bb", "dd")), base).unwrap();
        assert_ne!(moved.entries[0].id, one.entries[1].id);
    }

    fn cache_key(id: u32) -> [u8; 32] {
        let mut key = [0; 32];
        key[..4].copy_from_slice(&id.to_be_bytes());
        key
    }

    #[test]
    fn catalog_cache_expires_at_one_hour_and_replaces_without_overcounting() {
        let mut cache = CatalogCache::default();
        let now = Instant::now();
        let key = cache_key(1);
        let value = Arc::new(Value::String("metadata".into()));
        cache.insert(key, Arc::clone(&value), 10, now);
        assert!(Arc::ptr_eq(
            &value,
            &cache
                .lookup(&key, now + CATALOG_CACHE_TTL - Duration::from_secs(1))
                .unwrap()
        ));
        cache.insert(key, Arc::clone(&value), 12, now);
        assert_eq!(cache.payload_bytes, 12);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.lookup(&key, now + CATALOG_CACHE_TTL).is_none());
        assert_eq!(cache.payload_bytes, 0);
        assert!(cache.insertion_order.is_empty());
    }

    #[test]
    fn catalog_cache_enforces_entry_and_payload_budgets_oldest_first() {
        let now = Instant::now();
        let mut cache = CatalogCache::default();
        for id in 0..=CATALOG_CACHE_ENTRIES as u32 {
            cache.insert(cache_key(id), Arc::new(Value::Null), 1, now);
        }
        assert_eq!(cache.entries.len(), CATALOG_CACHE_ENTRIES);
        assert_eq!(cache.payload_bytes, CATALOG_CACHE_ENTRIES);
        assert!(cache.lookup(&cache_key(0), now).is_none());
        assert!(cache.lookup(&cache_key(256), now).is_some());

        let mut cache = CatalogCache::default();
        for id in 0..5 {
            cache.insert(cache_key(id), Arc::new(Value::Null), MAX_XML, now);
        }
        assert_eq!(cache.payload_bytes, CATALOG_CACHE_BYTES);
        assert_eq!(cache.entries.len(), 4);
        assert!(cache.lookup(&cache_key(0), now).is_none());
        cache.insert(
            cache_key(5),
            Arc::new(Value::Null),
            CATALOG_CACHE_BYTES + 1,
            now,
        );
        assert!(cache.lookup(&cache_key(5), now).is_none());
        assert_eq!(cache.payload_bytes, CATALOG_CACHE_BYTES);
    }

    #[test]
    fn catalog_cache_hash_covers_source_and_exact_credentials() {
        let headers = vec![("Authorization".into(), "Bearer first-secret".into())];
        let key = catalog_cache_key("https://catalog.example/tv/1", &headers);
        assert_ne!(
            key,
            catalog_cache_key("https://other.example/tv/1", &headers)
        );
        assert_ne!(
            key,
            catalog_cache_key(
                "https://catalog.example/tv/1",
                &[("Authorization".into(), "Bearer second-secret".into())]
            )
        );
        assert_ne!(
            catalog_cache_key("a", &[("bc".into(), "d".into())]),
            catalog_cache_key("ab", &[("c".into(), "d".into())])
        );
    }

    fn movie() -> Request {
        Request {
            kind: "movie".into(),
            title: "Café Night".into(),
            year: 2024,
            season: 0,
            episode: 0,
            source_path: None,
            source_url: None,
            source_numbering: None,
            tmdb_id: None,
        }
    }

    #[test]
    fn expired_search_budget_returns_before_contacting_a_configured_indexer() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut config =
            config::from_json(&config::default_json(), std::path::Path::new(".")).unwrap();
        config.sources.push(Source {
            options: Default::default(),
            name: "private-indexer-label".into(),
            kind: "json".into(),
            url: format!("http://{}", listener.local_addr().unwrap()),
            api_key_env: "MYNOU_EXPIRED_SEARCH_ABSENT_KEY_390815".into(),
        });
        let expired = Instant::now()
            .checked_sub(SEARCH_BUDGET + Duration::from_secs(1))
            .unwrap();
        let error = match search_candidates_since(&config, &movie(), expired) {
            Ok(_) => panic!("an expired search must not select a partial result"),
            Err(error) => error,
        };
        assert_eq!(error, "Search: indexer search exceeded its time budget");
        assert!(!error.contains("private-indexer-label"));
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    /// A loopback Plex peer for one request: it hangs up at once, or holds
    /// the request unanswered until the client hangs up.
    fn plex_peer(hold: bool) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/identity", listener.local_addr().unwrap());
        let peer = std::thread::spawn(move || {
            use std::io::Read;
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            if hold {
                assert!(matches!(stream.read(&mut [0; 64]), Ok(0) | Err(_)));
            }
        });
        (url, peer)
    }

    #[test]
    fn a_plex_request_cut_off_by_its_deadline_reports_the_deadline() {
        let expired = "Fixture deadline reached";
        let (url, peer) = plex_peer(true);
        let started = Instant::now();
        let deadline = started + Duration::from_millis(200);
        assert_eq!(
            fetch_json_before(&url, &[], deadline, expired).unwrap_err(),
            expired
        );
        assert!(Instant::now() >= deadline);
        assert!(started.elapsed() < Duration::from_secs(10));
        peer.join().unwrap();

        // A failure before the deadline is still a network failure.
        let (url, peer) = plex_peer(false);
        let deadline = Instant::now() + Duration::from_secs(10);
        assert_eq!(
            fetch_json_before(&url, &[], deadline, expired).unwrap_err(),
            "Plex: network request failed"
        );
        peer.join().unwrap();

        // A deadline already reached contacts nobody.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/identity", listener.local_addr().unwrap());
        assert_eq!(
            fetch_json_before(&url, &[], Instant::now(), expired).unwrap_err(),
            expired
        );
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn release_identity_rejects_sequels_wrong_years_and_episodes() {
        let mut request = movie();
        assert!(release_matches(&request, "Cafe.Night.2024.1080p.WEB-DL"));
        assert!(!release_matches(&request, "Cafe.Night.2.2024.1080p"));
        assert!(!release_matches(&request, "Cafe.Night.2023.1080p"));
        assert!(!release_matches(&request, "Cafe.Night.2024.S01E01"));
        assert!(!release_matches(&request, "The.Sequel.To.Cafe.Night.2024"));
        request.kind = "episode".into();
        request.season = 2;
        request.episode = 3;
        assert!(release_matches(&request, "Cafe.Night.S02E03.1080p"));
        assert!(release_matches(&request, "Cafe.Night.2024.2x03.1080p"));
        assert!(!release_matches(&request, "Cafe.Night.S02E04.1080p"));
        assert!(!release_matches(&request, "Cafe.Night.S02E03E04.1080p"));
        assert!(!release_matches(&request, "Cafe.Night.S02E03-E04.1080p"));
        assert!(!release_matches(&request, "Cafe.Night.S02E03.S03E01.1080p"));
        assert!(!release_matches(&request, "Cafe.Night.Another.Show.S02E03"));
    }

    #[test]
    fn rss_namespace_cdata_entities_and_enclosures_are_decoded() {
        let feed = r#"<?xml version="1.0" encoding="UTF-8"?>
            <rss xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel>
            <item><title><![CDATA[Café Night.2024.1080p]]></title>
            <link>https://invalid.example/details</link>
            <enclosure url="https://index.example/download?id=2&amp;key=a%26b"/>
            <torznab:attr name="seeders" value="19"/></item>
            <item><title>Movie &#xE9; &#233;</title><torznab:attr name="magneturl" value="magnet:?xt=urn:btih:abc&amp;dn=test"/>
            <seeders>4</seeders></item></channel></rss>"#;
        let releases = rss_releases(feed, "https://index.example/rss").unwrap();
        assert_eq!(releases.len(), 2);
        assert_eq!(
            releases[0].url,
            "https://index.example/download?id=2&key=a%26b"
        );
        assert_eq!(releases[0].seeders, 19);
        assert_eq!(releases[1].title, "Movie é é");
        assert_eq!(releases[1].url, "magnet:?xt=urn:btih:abc&dn=test");
    }

    #[test]
    fn xml_rejects_dtd_duplicate_attributes_depth_and_invalid_entities() {
        for text in [
            "<!DOCTYPE rss [<!ENTITY e SYSTEM 'file:///etc/passwd'>]><rss/>",
            "<rss a='1' a='2'/>",
            "<rss><channel></rss>",
            "<rss>&unknown;</rss>",
            "<rss>&#0;</rss>",
            "<rss><!-- bad -- comment --></rss>",
            "<rss/>ignored",
            "<rss><!DOCTYPE foo></rss>",
        ] {
            assert!(parse_xml(text).is_err(), "accepted: {text}");
        }
        assert!(parse_xml(&format!("{}{}", "<a>".repeat(66), "</a>".repeat(66))).is_err());
    }

    #[test]
    fn url_queries_encode_secrets_and_preserve_existing_query() {
        assert_eq!(
            query(
                "https://index.example/rss?token=existing",
                &[("apikey", "a&b= c/é".into())]
            )
            .unwrap(),
            "https://index.example:443/rss?token=existing&apikey=a%26b%3D%20c%2F%C3%A9"
        );
        assert!(acquisition_url("file:///tmp/source.torrent", "https://index.example").is_err());
        assert!(acquisition_url("magnet:?dn=missing", "https://index.example").is_err());
        assert_eq!(
            acquisition_url("/download?id=1", "https://index.example/rss").unwrap(),
            "https://index.example:443/download?id=1"
        );
    }

    #[test]
    fn dates_reject_invalid_calendar_days() {
        assert!(valid_date("2024-02-29"));
        assert!(!valid_date("2023-02-29"));
        assert!(!valid_date("2024-04-31"));
        assert!(!valid_date("2024-00-01"));
        assert!(!valid_date("2024-01-00"));
        assert!(valid_date(&today()));
    }
}
