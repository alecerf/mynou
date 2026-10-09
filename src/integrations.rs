//! Plex, TMDB and indexer integrations without external dependencies.
//! Responses and XML are bounded; errors never contain a download URL
//! or an API key.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::Result;
use crate::config::{self, Config, Source};
use crate::crypto::Sha256;
use crate::json::{self, Value};
use crate::net::{self, HttpClient};
use crate::selection::{Assessment, tokens};
use crate::store::{RecordedRelease, Request};
use crate::xml::{MAX_XML, parse_xml};

const MAX_ITEMS: usize = 100_000;
const MAX_PAGES: usize = 1_000;
const PAGE_SIZE: usize = 200;
const MAX_REPORTED_CANDIDATES: usize = 1_000;
const MAX_CANDIDATE_BYTES: usize = 16 * 1024 * 1024;
const SEARCH_BUDGET: Duration = Duration::from_secs(90);
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

fn optional_secret(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => config::secret(name).map(Some),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(format!("Invalid secret {name}")),
    }
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

fn plex_items_before(
    url: &str,
    headers: &[(String, String)],
    deadline: Instant,
    limit: usize,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for page in 0..MAX_PAGES {
        let url = query(
            url,
            &[
                ("X-Plex-Container-Start", out.len().to_string()),
                ("X-Plex-Container-Size", PAGE_SIZE.to_string()),
                ("includeGuids", "1".into()),
            ],
        )?;
        let value = fetch_json_before(&url, headers, deadline)?;
        let (items, total) = metadata_page(&value)?;
        if out.len() + items.len() > limit || total.is_some_and(|n| n > limit as u64) {
            return Err("Plex: too many items".into());
        }
        if items.is_empty() {
            if total.is_some_and(|n| n > out.len() as u64) {
                return Err("Plex: incomplete pagination".into());
            }
            return Ok(out);
        }
        if page > 0 && out.last() == items.last() {
            return Err("Plex: pagination ignored by the server".into());
        }
        out.extend_from_slice(items);
        if total.is_some_and(|n| out.len() as u64 >= n)
            || (total.is_none() && items.len() < PAGE_SIZE)
        {
            return Ok(out);
        }
    }
    Err("Plex: page count exceeds the limit".into())
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

/// Reads the watchlist and expands series into episodes that have already aired.
pub fn watchlist(config: &Config) -> Result<Vec<Request>> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for request in watchlist_identities(config)? {
        for expanded in expand(config, &request)? {
            if out.len() >= MAX_ITEMS {
                return Err("Watchlist: too many episodes".into());
            }
            if seen.insert(expanded.canonical_key()) {
                out.push(expanded);
            }
        }
    }
    Ok(out)
}

pub(crate) fn watchlist_identities(config: &Config) -> Result<Vec<Request>> {
    watchlist_identities_before(config, Instant::now() + Duration::from_secs(90), MAX_ITEMS)
}
fn watchlist_identities_before(
    config: &Config,
    deadline: Instant,
    limit: usize,
) -> Result<Vec<Request>> {
    if !config.plex.enabled {
        return Ok(Vec::new());
    }
    let headers = plex_headers(config)?;
    let items = plex_items_before(&config.plex.watchlist_url, &headers, deadline, limit)?;
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for item in items {
        let kind = match string(&item, "type") {
            Some("movie") => "movie",
            Some("show") => "series",
            Some("episode") => "episode",
            _ => continue,
        };
        let title = if kind == "episode" {
            string(&item, "grandparentTitle").or_else(|| string(&item, "title"))
        } else {
            string(&item, "title")
        }
        .ok_or("Plex: missing title")?;
        let request = Request {
            kind: kind.into(),
            title: title.into(),
            year: u32::try_from(
                integer(
                    &item,
                    if kind == "episode" {
                        "grandparentYear"
                    } else {
                        "year"
                    },
                )
                .unwrap_or(0),
            )
            .map_err(|_| "Plex: invalid year")?,
            season: u32::try_from(integer(&item, "parentIndex").unwrap_or(0))
                .map_err(|_| "Plex: invalid season")?,
            episode: u32::try_from(integer(&item, "index").unwrap_or(0))
                .map_err(|_| "Plex: invalid episode")?,
            source_path: None,
            source_url: None,
            source_numbering: None,
            tmdb_id: if kind == "episode" {
                None
            } else {
                tmdb_guid(&item)
            },
        };
        request.validate()?;
        if out.len() >= MAX_ITEMS {
            return Err("Watchlist: too many requests".into());
        }
        if seen.insert(request.canonical_key()) {
            out.push(request);
        }
    }
    Ok(out)
}

fn fetch_json_before(url: &str, headers: &[(String, String)], deadline: Instant) -> Result<Value> {
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or("Plex: account poll deadline reached")?;
    let response = client()
        .with_timeout(timeout.min(Duration::from_secs(10)))
        .request("GET", url, headers, &[])
        .map_err(|_| "Plex: network request failed")?;
    if !(200..300).contains(&response.status) {
        return Err(format!("Plex: HTTP response {}", response.status));
    }
    json::parse(std::str::from_utf8(&response.body).map_err(|_| "Plex: invalid UTF-8")?)
        .map_err(|_| "Plex: invalid JSON response".into())
}
pub(crate) fn requester_watchlist(
    config: &Config,
    account: &crate::requesters::Account,
    deadline: Instant,
) -> Result<Vec<Request>> {
    let cfg = verified_requester_config_before(config, account, deadline)?;
    watchlist_identities_before(&cfg, deadline, crate::requesters::MAX_POLL_ITEMS)
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
    let identity = fetch_json_before(&account.identity_url, &headers, deadline)?;
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
    let mut pairs = pairs.to_vec();
    if let Some(token) = optional_secret(&config.catalog.token_env)? {
        headers.push(("Authorization".into(), format!("Bearer {token}")));
    } else if let Some(key) = optional_secret(&config.catalog.api_key_env)? {
        pairs.push(("api_key", key));
    } else {
        return Err("TMDB catalog: missing token or API key".into());
    }
    let url = query(&endpoint(&config.catalog.url, path)?, &pairs)?;
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

/// A bounded fresh plan includes known future/undated episodes without queuing them.
pub fn series_plan(
    config: &Config,
    request: &Request,
    include_specials: bool,
) -> Result<crate::series::Plan> {
    series_plan_before(
        config,
        request,
        include_specials,
        Instant::now() + SEARCH_BUDGET,
    )
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
    let mut pairs = pairs.to_vec();
    if let Some(token) = optional_secret(&config.catalog.token_env)? {
        headers.push(("Authorization".into(), format!("Bearer {token}")));
    } else if let Some(key) = optional_secret(&config.catalog.api_key_env)? {
        pairs.push(("api_key", key));
    } else {
        return Err("TMDB catalog: missing token or API key".into());
    }
    let url = query(&endpoint(&config.catalog.url, path)?, &pairs)?;
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

#[derive(Debug)]
struct Release {
    title: String,
    url: String,
    seeders: u64,
    usenet: Option<crate::usenet::newznab::Target>,
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

fn acquisition_url(url: &str, base: &str) -> Result<String> {
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

fn json_releases(value: &Value, base: &str) -> Result<Vec<Release>> {
    let items = value
        .as_array()
        .or_else(|| value.get("results").and_then(Value::as_array))
        .or_else(|| value.get("items").and_then(Value::as_array))
        .ok_or("JSON indexer: missing results/items list")?;
    if items.len() > MAX_ITEMS {
        return Err("JSON indexer: too many results".into());
    }
    let mut out = Vec::new();
    for item in items {
        let Some(title) = string(item, "title") else {
            continue;
        };
        if title.len() > 2_048 {
            continue;
        }
        let Some(url) = ["download_url", "magnet", "magnet_url", "url", "link"]
            .iter()
            .find_map(|k| string(item, k))
        else {
            continue;
        };
        let Ok(url) = acquisition_url(url, base) else {
            continue;
        };
        let seeders = integer(item, "seeders")
            .or_else(|| integer(item, "seeders_count"))
            .unwrap_or(0);
        out.push(Release {
            title: title.into(),
            url,
            seeders,
            usenet: None,
        });
    }
    Ok(out)
}

pub(crate) fn probe_source(
    config: &Config,
    source: &Source,
    request: &Request,
    deadline: Instant,
) -> bool {
    source_releases(config, source, request, deadline).is_ok()
}

fn source_releases(
    config: &Config,
    source: &Source,
    request: &Request,
    deadline: Instant,
) -> Result<Vec<Release>> {
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
    if matches!(source.kind.as_str(), "torznab" | "newznab") {
        pairs.push((
            "t",
            if matches!(request.kind.as_str(), "episode" | "series") {
                "tvsearch"
            } else {
                "movie"
            }
            .into(),
        ));
        if source.kind == "newznab" && request.kind == "movie" {
            if let Some(id) = request.tmdb_id {
                pairs.push(("tmdbid", id.to_string()));
            }
            if request.year != 0 {
                pairs.push(("year", request.year.to_string()));
            }
        }
        if matches!(request.kind.as_str(), "episode" | "series")
            && !matches!(numbering, crate::numbering::SourceNumber::Absolute(_))
        {
            pairs.push(("season", season.to_string()));
            if request.kind == "episode" {
                pairs.push(("ep", episode.to_string()));
            }
        }
    } else if source.kind == "json" {
        pairs.push(("kind", request.kind.clone()));
        pairs.push(("year", request.year.to_string()));
        pairs.push(("season", season.to_string()));
        pairs.push(("episode", episode.to_string()));
        if let crate::numbering::SourceNumber::Absolute(number) = numbering {
            pairs.push(("absolute", number.to_string()));
        }
    }
    if let Some(key) = optional_secret(&source.api_key_env)? {
        pairs.push(("apikey", key));
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
            json_releases(&json::parse(text)?, &source.url)
        } else if source.kind == "newznab" {
            crate::usenet::newznab::parse_feed(text, source, config).map(|rows| {
                rows.into_iter()
                    .map(|a| Release {
                        title: a.title,
                        url: a.url,
                        seeders: 0,
                        usenet: Some(a.target),
                    })
                    .collect()
            })
        } else {
            rss_releases(text, &source.url)
        }
    })();
    crate::indexers::parsed(source, releases.is_ok());
    let releases = releases?;
    remaining_search_time(deadline)?;
    Ok(releases)
}

struct Candidate {
    release: Release,
    source: String,
    id: String,
    assessment: Assessment,
    library_admission_supported: bool,
}

impl Candidate {
    fn to_json(&self) -> Value {
        let mut value = Value::object();
        value.insert("id", self.id.clone());
        value.insert("title", report_text(&self.release.title, 2_048));
        value.insert("source", report_text(&self.source, 128));
        value.insert(
            "transport",
            if self.release.usenet.is_some() {
                "usenet"
            } else {
                "torrent"
            },
        );
        if let Some(target) = &self.release.usenet {
            value.insert("advertised_bytes", target.advertised_bytes.to_string());
            value.insert("password_protected", target.password_protected);
            value.insert("download_advertised", true);
            value.insert("content_verified", false);
            value.insert(
                "library_admission_supported",
                self.library_admission_supported,
            );
        }
        // An indexer may report integers outside JSON's exact range.
        value.insert(
            "seeders",
            if self.release.usenet.is_some() {
                Value::Null
            } else if self.release.seeders <= 9_007_199_254_740_991 {
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

fn candidate_id(profile: &str, source: &str, release: &Release) -> String {
    let mut hash = Sha256::new();
    for field in [profile, source, &release.title, &release.url] {
        hash.update(&(field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    if let Some(target) = &release.usenet {
        hash.update(json::stringify(&target.to_json()).as_bytes());
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(64);
    for byte in hash.finalize() {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 15) as usize] as char);
    }
    id
}

struct SearchResult {
    profile: String,
    candidates: Vec<Candidate>,
    configured: usize,
    successful: usize,
}

fn candidate_allocation_bytes(
    release: &Release,
    source_len: usize,
    assessment: &Assessment,
) -> usize {
    let attributes = &assessment.attributes;
    let string_bytes = assessment
        .reasons
        .iter()
        .chain(&attributes.languages)
        .chain(&attributes.issues)
        .chain(attributes.source.iter())
        .chain(attributes.codec.iter())
        .map(String::capacity)
        .sum::<usize>();
    let vector_bytes = (assessment.reasons.capacity()
        + attributes.languages.capacity()
        + attributes.issues.capacity())
        * std::mem::size_of::<String>();
    // Fixed allowance covers the candidate, vector growth, its opaque ID and
    // allocation metadata. Counting capacities also covers spare string space.
    release.title.capacity()
        + release.url.capacity()
        + source_len
        + string_bytes
        + vector_bytes
        + release
            .usenet
            .as_ref()
            .map_or(0, crate::usenet::newznab::Target::allocation_bytes)
        + 512
}

impl SearchResult {
    fn report(&self) -> Value {
        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        for candidate in self.candidates.iter().take(MAX_REPORTED_CANDIDATES) {
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
        report.insert(
            "candidate_count",
            Value::Number(self.candidates.len() as f64),
        );
        report.insert(
            "reported_count",
            Value::Number(self.candidates.len().min(MAX_REPORTED_CANDIDATES) as f64),
        );
        report.insert("truncated", self.candidates.len() > MAX_REPORTED_CANDIDATES);
        report.insert(
            "selected_candidate_id",
            self.candidates
                .first()
                .filter(|candidate| candidate.assessment.accepted)
                .map_or(Value::Null, |candidate| candidate.id.clone().into()),
        );
        report
    }
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
    search_candidates_mode(config, request, deadline, false)
}

fn search_candidates_mode(
    config: &Config,
    request: &Request,
    deadline: Instant,
    pack: bool,
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
    let mut candidates = Vec::new();
    let mut candidate_bytes = 0usize;
    let mut successful = 0;
    for source in &config.sources {
        remaining_search_time(deadline)?;
        if pack && source.kind == "newznab" {
            continue;
        }
        if let Ok(releases) = source_releases(config, source, request, deadline) {
            successful += 1;
            // Hash and bound source metadata once, rather than duplicating an
            // arbitrarily long configured label for every indexer result.
            let source_identity = candidate_id(
                profile_name,
                &source.name,
                &Release {
                    title: String::new(),
                    url: source.url.clone(),
                    seeders: 0,
                    usenet: None,
                },
            );
            let source_label = report_text(&source.name, 128);
            for (index, release) in releases.into_iter().enumerate() {
                if index % 64 == 0 {
                    remaining_search_time(deadline)?;
                }
                if candidates.len() >= MAX_ITEMS {
                    return Err("Search: too many candidates".into());
                }
                let mut assessment = profile.assess(&release.title, &request.title);
                if (RecordedRelease {
                    title: release.title.clone(),
                    profile: profile_name.into(),
                })
                .validate()
                .is_err()
                {
                    assessment.accepted = false;
                    assessment.reasons.push(
                        "Release title or profile is invalid for acquisition provenance".into(),
                    );
                }
                if !(if pack {
                    crate::pack::season_title_matches(request, &release.title)
                } else {
                    release_matches(request, &release.title)
                }) {
                    assessment.accepted = false;
                    assessment
                        .reasons
                        .push(if pack { "Release does not match the requested series, year or single season pack" }
                            else { "Release does not match the requested title, year or episode" }.into());
                }
                if let Some(target) = &release.usenet {
                    let options = source
                        .options
                        .newznab
                        .as_ref()
                        .ok_or("Newznab: missing source policy")?;
                    if target.password_protected {
                        assessment.accepted = false;
                        assessment
                            .reasons
                            .push("Newznab release is advertised as password protected".into());
                    }
                    if target.advertised_bytes < options.minimum_bytes
                        || target.advertised_bytes > options.maximum_bytes
                        || config
                            .usenet
                            .downloads
                            .as_ref()
                            .is_some_and(|d| target.advertised_bytes > d.max_file_bytes)
                    {
                        assessment.accepted = false;
                        assessment
                            .reasons
                            .push("Newznab release is outside the configured size interval".into());
                    }
                } else if release.seeders < config.minimum_seeders {
                    assessment.accepted = false;
                    assessment
                        .reasons
                        .push("Release has fewer than the minimum seeders".into());
                }
                candidate_bytes = candidate_bytes.saturating_add(candidate_allocation_bytes(
                    &release,
                    source_label.len(),
                    &assessment,
                ));
                if candidate_bytes > MAX_CANDIDATE_BYTES {
                    return Err("Search: candidate data exceeds the memory limit".into());
                }
                candidates.push(Candidate {
                    library_admission_supported: release.usenet.is_some()
                        && config.usenet.downloads.as_ref().is_some_and(|d| d.enabled),
                    id: candidate_id(profile_name, &source_identity, &release),
                    source: source_label.clone(),
                    release,
                    assessment,
                });
            }
        }
    }
    remaining_search_time(deadline)?;
    if successful == 0 {
        return Err("Search: no indexer returned a usable response".into());
    }
    candidates.sort_by(|a, b| {
        b.assessment
            .accepted
            .cmp(&a.assessment.accepted)
            .then_with(|| b.assessment.rank.cmp(&a.assessment.rank))
            .then_with(|| b.release.seeders.cmp(&a.release.seeders))
            .then_with(|| a.release.title.cmp(&b.release.title))
            .then_with(|| a.release.url.cmp(&b.release.url))
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.id.cmp(&b.id))
    });
    remaining_search_time(deadline)?;
    Ok(SearchResult {
        profile: profile_name.into(),
        candidates,
        configured: config.sources.len(),
        successful,
    })
}

fn remaining_search_time(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "Search: indexer search exceeded its time budget".into())
}

/// Explains source selection without downloading, storing or exposing URLs.
/// Usable indexers with no accepted release produce an empty selection.
pub fn search_report(config: &Config, request: &Request) -> Result<Value> {
    request.validate()?;
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
        report.insert("truncated", false);
        report.insert("selected_candidate_id", Value::Null);
        let mut indexers = Value::object();
        indexers.insert("configured", Value::Number(config.sources.len() as f64));
        indexers.insert("successful", Value::Number(0.0));
        indexers.insert("failed", Value::Number(0.0));
        report.insert("indexers", indexers);
        return Ok(report);
    }
    search_candidates(config, request).map(|result| result.report())
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
    pub usenet: Option<crate::usenet::newznab::Target>,
}

fn selected_release(result: SearchResult) -> Result<SelectedRelease> {
    let selected = selected_acquisition(result)?;
    if selected.usenet.is_some() {
        return Err(
            "Search: native Usenet library admission is not enabled in this increment".into(),
        );
    }
    Ok(selected)
}

fn selected_acquisition(result: SearchResult) -> Result<SelectedRelease> {
    let candidate = result
        .candidates
        .into_iter()
        .find(|candidate| candidate.assessment.accepted)
        .ok_or("Search: no release satisfies the identity, seed count and selection profile")?;
    Ok(SelectedRelease {
        url: candidate.release.url,
        title: candidate.release.title,
        profile: result.profile,
        assessment: candidate.assessment,
        id: candidate.id,
        usenet: candidate.release.usenet,
    })
}

/// Separate season-pack identity rules retain the episode profile and normal ranking.
pub(crate) fn search_pack_candidates(
    config: &Config,
    request: &Request,
    deadline: Instant,
) -> Result<(Value, Vec<SelectedRelease>)> {
    let result = search_candidates_mode(config, request, deadline, true)?;
    let report = result.report();
    let candidates = result
        .candidates
        .into_iter()
        .filter(|candidate| candidate.assessment.accepted)
        .take(8)
        .map(|candidate| SelectedRelease {
            url: candidate.release.url,
            title: candidate.release.title,
            profile: result.profile.clone(),
            assessment: candidate.assessment,
            id: candidate.id,
            usenet: candidate.release.usenet,
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

/// Typed metadata for a prospective acquisition. No source content is acquired.
pub fn select_acquisition(
    config: &Config,
    request: &Request,
    deadline: Instant,
) -> Result<SelectedRelease> {
    request.validate()?;
    if request.source_path.is_some() || request.source_url.is_some() {
        return Err("Search: manual sources do not have a verified release assessment".into());
    }
    let deadline = deadline.min(
        Instant::now()
            .checked_add(SEARCH_BUDGET)
            .ok_or("Search: invalid time budget")?,
    );
    selected_acquisition(search_candidates_before(config, request, deadline)?)
}

/// Captured document URLs are persisted in job records, so the indexer's API
/// key is left out whenever one is configured; `newznab_document_url` adds the
/// current key back when the document is fetched.
pub(crate) fn newznab_capture_url(source: &Source, url: &str) -> Result<String> {
    Ok(match optional_secret(&source.api_key_env)? {
        Some(_) => without_api_key(url),
        None => url.to_owned(),
    })
}

fn without_api_key(url: &str) -> String {
    let (base, fragment) = url
        .split_once('#')
        .map_or((url, None), |(b, f)| (b, Some(f)));
    let Some((path, query)) = base.split_once('?') else {
        return url.to_owned();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let name = pair.split_once('=').map_or(*pair, |(name, _)| name);
            !name.eq_ignore_ascii_case("apikey")
        })
        .collect();
    let mut result = path.to_owned();
    if !kept.is_empty() {
        result.push('?');
        result.push_str(&kept.join("&"));
    }
    if let Some(fragment) = fragment {
        result.push('#');
        result.push_str(fragment);
    }
    result
}

pub(crate) fn newznab_document_url(source: &Source, url: &str) -> Result<String> {
    let path = net::parse_url(url)?.path;
    let has_key = path
        .split_once('?')
        .is_some_and(|(_, q)| q.split('&').any(|p| p.split('=').next() == Some("apikey")));
    if has_key {
        return Ok(url.to_owned());
    }
    match optional_secret(&source.api_key_env)? {
        Some(key) => query(url, &[("apikey", key)]),
        None => Ok(url.to_owned()),
    }
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
    if imports.len() > MAX_ITEMS || config.plex.path_mappings.len() > 32 {
        return Err("Plex: too many import paths or path mappings".into());
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
    let items = plex_items_before(&url, &headers, deadline, MAX_ITEMS)?;
    for item in items.iter().filter(|item| identity_matches(item, request)) {
        remaining_search_time(deadline)?;
        if !episode {
            if matches(item) {
                return Ok(true);
            }
            continue;
        }
        let key = string(item, "ratingKey").ok_or("Plex: missing series ratingKey")?;
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
            return Err("Plex: invalid ratingKey".into());
        }
        let leaves = endpoint(
            &config.plex.url,
            &format!("library/metadata/{key}/allLeaves"),
        )?;
        for episode in plex_items_before(&leaves, &headers, deadline, MAX_ITEMS)? {
            remaining_search_time(deadline)?;
            if integer(&episode, "parentIndex") == Some(u64::from(request.season))
                && integer(&episode, "index") == Some(u64::from(request.episode))
                && matches(&episode)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn rss_releases(text: &str, base: &str) -> Result<Vec<Release>> {
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
        for attr in item.children.iter().filter(|child| child.local() == "attr") {
            match attr.attrs.get("name").map(String::as_str) {
                Some("seeders") => {
                    seeders = attr
                        .attrs
                        .get("value")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0)
                }
                Some("magneturl") if url.is_none() => {
                    url = attr.attrs.get("value").map(String::as_str)
                }
                _ => {}
            }
        }
        let url = url.or_else(|| item.child("link").map(|v| v.text.trim()));
        let Some(url) = url else {
            continue;
        };
        let Ok(url) = acquisition_url(url, base) else {
            continue;
        };
        out.push(Release {
            title: title.into(),
            url,
            seeders,
            usenet: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn captured_newznab_urls_drop_only_the_api_key() {
        for (url, expected) in [
            ("https://i.example/get?id=1&apikey=secret", "https://i.example/get?id=1"),
            ("https://i.example/get?apikey=secret&id=1", "https://i.example/get?id=1"),
            ("https://i.example/get?ApiKey=secret", "https://i.example/get"),
            ("https://i.example/get?id=1&apikey", "https://i.example/get?id=1"),
            ("https://i.example/get?id=1&apikeys=2", "https://i.example/get?id=1&apikeys=2"),
            ("https://i.example/get?id=1&apikey=s#part", "https://i.example/get?id=1#part"),
            ("https://i.example/get", "https://i.example/get"),
        ] {
            assert_eq!(without_api_key(url), expected);
        }
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
