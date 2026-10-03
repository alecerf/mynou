//! Intégrations Plex, TMDB et indexeurs, sans dépendance externe.
//! Les réponses et le XML sont bornés ; les erreurs ne contiennent jamais une
//! URL de téléchargement ni une clé d'API.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::Result;
use crate::config::{self, Config, Source};
use crate::crypto::Sha256;
use crate::json::{self, Value};
use crate::net::{self, HttpClient};
use crate::store::Request;

const MAX_ITEMS: usize = 100_000;
const MAX_PAGES: usize = 1_000;
const PAGE_SIZE: usize = 200;
const MAX_XML: usize = 8 * 1024 * 1024;
const CATALOG_CACHE_TTL: Duration = Duration::from_secs(3_600);
const CATALOG_CACHE_BYTES: usize = 32 * 1024 * 1024;
const CATALOG_CACHE_ENTRIES: usize = 256;

struct CatalogEntry {
    value: Arc<Value>,
    inserted: Instant,
    payload_bytes: usize,
}

/// Seules les données JSON de TMDB sont conservées. Les clés sont des SHA-256
/// cadrés de l'URL et des en-têtes ; aucune URL ou clé d'API n'est stockée ici.
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
        return Err("URL de service : paramètres interdits dans la base".into());
    }
    parsed.path = format!(
        "{}/{}",
        parsed.path.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    Ok(parsed.as_string())
}

fn fetch_json(url: &str, headers: &[(String, String)], service: &str) -> Result<Value> {
    fetch_json_sized(url, headers, service).map(|(value, _)| value)
}

fn fetch_json_sized(
    url: &str,
    headers: &[(String, String)],
    service: &str,
) -> Result<(Value, usize)> {
    let response = client()
        .request("GET", url, headers, &[])
        .map_err(|_| format!("{service} : requête réseau impossible"))?;
    if !(200..300).contains(&response.status) {
        return Err(format!("{service} : réponse HTTP {}", response.status));
    }
    let text =
        std::str::from_utf8(&response.body).map_err(|_| format!("{service} : UTF-8 invalide"))?;
    let value = json::parse(text).map_err(|_| format!("{service} : réponse JSON invalide"))?;
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
        Err(_) => Err(format!("Secret {name} invalide")),
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
        .ok_or("Plex : MediaContainer absent")?;
    if container.as_object().is_none() {
        return Err("Plex : MediaContainer invalide".into());
    }
    let items = match container.get("Metadata") {
        Some(v) => v.as_array().ok_or("Plex : Metadata invalide")?,
        None if integer(container, "size") == Some(0) => &[],
        None => return Err("Plex : Metadata absent".into()),
    };
    Ok((items, integer(container, "totalSize")))
}

fn plex_items(url: &str, headers: &[(String, String)]) -> Result<Vec<Value>> {
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
        let value = fetch_json(&url, headers, "Plex")?;
        let (items, total) = metadata_page(&value)?;
        if out.len() + items.len() > MAX_ITEMS || total.is_some_and(|n| n > MAX_ITEMS as u64) {
            return Err("Plex : trop de contenus".into());
        }
        if items.is_empty() {
            if total.is_some_and(|n| n > out.len() as u64) {
                return Err("Plex : pagination incomplète".into());
            }
            return Ok(out);
        }
        if page > 0 && out.last() == items.last() {
            return Err("Plex : pagination ignorée par le serveur".into());
        }
        out.extend_from_slice(items);
        if total.is_some_and(|n| out.len() as u64 >= n)
            || (total.is_none() && items.len() < PAGE_SIZE)
        {
            return Ok(out);
        }
    }
    Err("Plex : nombre de pages excessif".into())
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

/// Lit la watchlist, puis développe les séries en épisodes déjà diffusés.
pub fn watchlist(config: &Config) -> Result<Vec<Request>> {
    if !config.plex.enabled {
        return Ok(Vec::new());
    }
    let headers = plex_headers(config)?;
    let items = plex_items(&config.plex.watchlist_url, &headers)?;
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
        .ok_or("Plex : titre absent")?;
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
            .map_err(|_| "Plex : année invalide")?,
            season: u32::try_from(integer(&item, "parentIndex").unwrap_or(0))
                .map_err(|_| "Plex : saison invalide")?,
            episode: u32::try_from(integer(&item, "index").unwrap_or(0))
                .map_err(|_| "Plex : épisode invalide")?,
            source_path: None,
            source_url: None,
            tmdb_id: if kind == "episode" {
                None
            } else {
                tmdb_guid(&item)
            },
        };
        request.validate()?;
        for expanded in expand(config, &request)? {
            if out.len() >= MAX_ITEMS {
                return Err("Watchlist : trop d'épisodes".into());
            }
            if seen.insert(expanded.canonical_key()) {
                out.push(expanded);
            }
        }
    }
    Ok(out)
}

fn catalog_json(config: &Config, path: &str, pairs: &[(&str, String)]) -> Result<Value> {
    if !config.catalog.enabled {
        return Err("Catalogue TMDB désactivé".into());
    }
    let mut headers = vec![("Accept".into(), "application/json".into())];
    let mut pairs = pairs.to_vec();
    if let Some(token) = optional_secret(&config.catalog.token_env)? {
        headers.push(("Authorization".into(), format!("Bearer {token}")));
    } else if let Some(key) = optional_secret(&config.catalog.api_key_env)? {
        pairs.push(("api_key", key));
    } else {
        return Err("Catalogue TMDB : jeton ou clé d'API absent".into());
    }
    let url = query(&endpoint(&config.catalog.url, path)?, &pairs)?;
    let key = catalog_cache_key(&url, &headers);
    let cached = catalog_cache()
        .lock()
        .map_err(|_| "Catalogue TMDB : cache indisponible")?
        .lookup(&key, Instant::now());
    if let Some(value) = cached {
        // La copie de l'arbre JSON reste en dehors du verrou partagé.
        return Ok(value.as_ref().clone());
    }
    let (value, payload_bytes) = fetch_json_sized(&url, &headers, "Catalogue TMDB")?;
    let cached = Arc::new(value.clone());
    catalog_cache()
        .lock()
        .map_err(|_| "Catalogue TMDB : cache indisponible")?
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
    let items = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or("Catalogue TMDB : résultats absents")?;
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
        return Err("Catalogue TMDB : identification absente ou ambiguë ; préciser tmdb_id".into());
    }
    Ok(*found
        .first()
        .ok_or("Catalogue TMDB : identification absente")?)
}

// Date UTC courante, conversion grégorienne depuis le nombre de jours Unix.
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

/// Les séries sans source explicite deviennent des demandes d'épisodes.
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
        return Err("Série : soumettre un épisode pour une source explicite".into());
    }
    if !config.catalog.enabled {
        return Err(
            "Série entière : activer TMDB pour identifier les épisodes déjà diffusés".into(),
        );
    }
    let id = resolve_catalog(config, request)?;
    let details = catalog_json(config, &format!("tv/{id}"), &[])?;
    let seasons = details
        .get("seasons")
        .and_then(Value::as_array)
        .ok_or("Catalogue TMDB : saisons absentes")?;
    if seasons.len() > 1_000 {
        return Err("Catalogue TMDB : trop de saisons".into());
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
            integer(season, "season_number").ok_or("Catalogue TMDB : numéro de saison absent")?;
        if number == 0 || (request.season != 0 && number != u64::from(request.season)) {
            continue;
        }
        let number = u32::try_from(number).map_err(|_| "Catalogue TMDB : saison invalide")?;
        if let Some(date) = string(season, "air_date").filter(|s| valid_date(s))
            && date > cutoff.as_str()
        {
            continue;
        }
        let value = catalog_json(config, &format!("tv/{id}/season/{number}"), &[])?;
        let episodes = value
            .get("episodes")
            .and_then(Value::as_array)
            .ok_or("Catalogue TMDB : épisodes absents")?;
        if episodes.len() > 1_000 {
            return Err("Catalogue TMDB : trop d'épisodes dans une saison".into());
        }
        for episode in episodes {
            let Some(date) = string(episode, "air_date").filter(|s| valid_date(s)) else {
                continue;
            };
            if date > cutoff.as_str() {
                continue;
            }
            let episode = u32::try_from(
                integer(episode, "episode_number")
                    .ok_or("Catalogue TMDB : numéro d'épisode absent")?,
            )
            .map_err(|_| "Catalogue TMDB : épisode invalide")?;
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
                tmdb_id: Some(id),
            };
            expanded.validate()?;
            out.push(expanded);
            if out.len() > MAX_ITEMS {
                return Err("Catalogue TMDB : trop d'épisodes".into());
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
}

fn tokens(text: &str) -> Vec<String> {
    let mut normalized = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        let c = match c {
            'à' | 'â' | 'ä' | 'á' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'ï' | 'î' | 'í' | 'ì' => 'i',
            'ô' | 'ö' | 'ó' | 'ò' | 'õ' => 'o',
            'ù' | 'û' | 'ü' | 'ú' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            c => c,
        };
        normalized.push(if c.is_alphanumeric() { c } else { ' ' });
    }
    normalized.split_whitespace().map(str::to_owned).collect()
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
    Some((season.parse().ok()?, episode.parse().ok()?))
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
        return !later_episode
            && rest.get(at).and_then(|s| episode_marker(s))
                == Some((request.season, request.episode));
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
        // Sans année, un titre seul ou immédiatement suivi d'une année/qualité.
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

fn acquisition_url(url: &str, base: &str) -> Result<String> {
    if url.len() > 8_192 || url.chars().any(char::is_control) {
        return Err("URL d'acquisition invalide".into());
    }
    if let Some(query) = url.strip_prefix("magnet:?") {
        if query
            .split('&')
            .any(|part| part.starts_with("xt=urn:btih:") || part.starts_with("xt=urn:btmh:"))
        {
            return Ok(url.into());
        }
        return Err("Magnet : empreinte absente".into());
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
        .ok_or("Indexeur JSON : liste results/items absente")?;
    if items.len() > MAX_ITEMS {
        return Err("Indexeur JSON : trop de résultats".into());
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
        });
    }
    Ok(out)
}

fn source_releases(source: &Source, request: &Request) -> Result<Vec<Release>> {
    let mut pairs = vec![("q", request.title.clone())];
    if source.kind == "torznab" {
        pairs.push((
            "t",
            if request.kind == "episode" {
                "tvsearch"
            } else {
                "movie"
            }
            .into(),
        ));
        if request.kind == "episode" {
            pairs.push(("season", request.season.to_string()));
            pairs.push(("ep", request.episode.to_string()));
        }
    } else if source.kind == "json" {
        pairs.push(("kind", request.kind.clone()));
        pairs.push(("year", request.year.to_string()));
        pairs.push(("season", request.season.to_string()));
        pairs.push(("episode", request.episode.to_string()));
    }
    if let Some(key) = optional_secret(&source.api_key_env)? {
        pairs.push(("apikey", key));
    }
    let url = query(&source.url, &pairs)?;
    let response = client()
        .request(
            "GET",
            &url,
            &[(
                "Accept".into(),
                if source.kind == "json" {
                    "application/json"
                } else {
                    "application/rss+xml, application/xml"
                }
                .into(),
            )],
            &[],
        )
        .map_err(|_| "Indexeur : requête réseau impossible")?;
    if !(200..300).contains(&response.status) {
        return Err(format!("Indexeur : réponse HTTP {}", response.status));
    }
    let text = std::str::from_utf8(&response.body).map_err(|_| "Indexeur : UTF-8 invalide")?;
    if source.kind == "json" {
        json_releases(&json::parse(text)?, &source.url)
    } else {
        rss_releases(text, &source.url)
    }
}

/// Sélection déterministe : identité exacte, puis nombre de seeders décroissant.
pub fn search(config: &Config, request: &Request) -> Result<String> {
    request.validate()?;
    if let Some(source) = &request.source_url {
        return acquisition_url(source, &config.plex.url);
    }
    if !matches!(request.kind.as_str(), "movie" | "episode") {
        return Err("Recherche : film ou épisode attendu".into());
    }
    if config.sources.is_empty() {
        return Err("Recherche : aucun indexeur configuré".into());
    }
    if config.sources.len() > 1_000 {
        return Err("Recherche : trop d'indexeurs".into());
    }
    let mut candidates = Vec::new();
    let mut successful = 0;
    for source in &config.sources {
        if let Ok(releases) = source_releases(source, request) {
            successful += 1;
            for release in releases.into_iter().filter(|release| {
                release.seeders >= config.minimum_seeders
                    && release_matches(request, &release.title)
            }) {
                if candidates.len() >= MAX_ITEMS {
                    return Err("Recherche : trop de candidats".into());
                }
                candidates.push(release);
            }
        }
    }
    if successful == 0 {
        return Err("Recherche : aucun indexeur n'a fourni une réponse exploitable".into());
    }
    candidates.sort_by(|a, b| {
        b.seeders
            .cmp(&a.seeders)
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.url.cmp(&b.url))
    });
    candidates.into_iter().next().map(|r| r.url).ok_or_else(|| "Recherche : aucune release correspondant au titre, à l'année ou à l'épisode et au minimum de seeders".into())
}

fn plex_section<'a>(config: &'a Config, request: &Request) -> Result<&'a str> {
    let section = if matches!(request.kind.as_str(), "episode" | "series") {
        &config.plex.series_section
    } else {
        &config.plex.movies_section
    };
    if section.is_empty() || !section.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Plex : numéro de section invalide".into());
    }
    Ok(section)
}

/// Demande à Plex de scanner la section après l'import atomique.
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
        .map_err(|_| "Plex : rafraîchissement impossible")?;
    if !(200..300).contains(&response.status) {
        return Err(format!("Plex : rafraîchissement HTTP {}", response.status));
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

/// La tâche reste en attente tant qu'une entrée lisible n'est pas indexée.
pub fn available(config: &Config, request: &Request) -> Result<bool> {
    if !config.plex.enabled {
        return Ok(true);
    }
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
    let items = plex_items(&url, &headers)?;
    for item in items.iter().filter(|item| identity_matches(item, request)) {
        if !episode {
            if playable(item) {
                return Ok(true);
            }
            continue;
        }
        let key = string(item, "ratingKey").ok_or("Plex : ratingKey de série absent")?;
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
            return Err("Plex : ratingKey invalide".into());
        }
        let leaves = endpoint(
            &config.plex.url,
            &format!("library/metadata/{key}/allLeaves"),
        )?;
        if plex_items(&leaves, &headers)?.iter().any(|episode| {
            integer(episode, "parentIndex") == Some(u64::from(request.season))
                && integer(episode, "index") == Some(u64::from(request.episode))
                && playable(episode)
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug)]
struct Element {
    name: String,
    attrs: BTreeMap<String, String>,
    text: String,
    children: Vec<Element>,
}

impl Element {
    fn local(&self) -> &str {
        self.name.rsplit(':').next().unwrap_or(&self.name)
    }
    fn child(&self, name: &str) -> Option<&Self> {
        self.children.iter().find(|child| child.local() == name)
    }
}

fn xml_unescape(text: &str) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let end = rest.find(';').ok_or("XML : entité incomplète")?;
        if end > 16 {
            return Err("XML : entité trop longue".into());
        }
        let entity = &rest[..end];
        let c = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            text => {
                let number = if let Some(n) = text.strip_prefix("#x") {
                    u32::from_str_radix(n, 16).ok()
                } else {
                    text.strip_prefix('#').and_then(|n| n.parse::<u32>().ok())
                };
                number
                    .and_then(char::from_u32)
                    .filter(|c| matches!(*c, '\t' | '\n' | '\r') || !c.is_control())
                    .ok_or("XML : entité inconnue ou caractère invalide")?
            }
        };
        out.push(c);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    if out
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
    {
        return Err("XML : caractère invalide".into());
    }
    Ok(out)
}

struct Xml<'a> {
    input: &'a str,
    at: usize,
    nodes: usize,
}

impl Xml<'_> {
    fn tail(&self) -> &str {
        &self.input[self.at..]
    }
    fn whitespace(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.at += 1;
        }
    }
    fn name(&mut self) -> Result<String> {
        let start = self.at;
        while self
            .input
            .as_bytes()
            .get(self.at)
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b':'))
        {
            self.at += 1;
        }
        let name = &self.input[start..self.at];
        if name.is_empty()
            || name.len() > 256
            || !name.as_bytes()[0].is_ascii_alphabetic() && name.as_bytes()[0] != b'_'
        {
            return Err("XML : nom invalide".into());
        }
        Ok(name.into())
    }
    fn comment(&mut self) -> Result<()> {
        self.at += 4;
        let end = self
            .tail()
            .find("-->")
            .ok_or("XML : commentaire incomplet")?;
        if self.tail()[..end].contains("--") {
            return Err("XML : commentaire invalide".into());
        }
        self.at += end + 3;
        Ok(())
    }
    fn element(&mut self, depth: usize) -> Result<Element> {
        if depth > 64 || self.nodes >= MAX_ITEMS {
            return Err("XML : complexité excessive".into());
        }
        self.nodes += 1;
        if !self.tail().starts_with('<') {
            return Err("XML : élément attendu".into());
        }
        self.at += 1;
        let name = self.name()?;
        let mut attrs = BTreeMap::new();
        let closed;
        loop {
            let before = self.at;
            self.whitespace();
            if self.tail().starts_with("/>") {
                self.at += 2;
                closed = true;
                break;
            }
            if self.tail().starts_with('>') {
                self.at += 1;
                closed = false;
                break;
            }
            if before == self.at || attrs.len() >= 64 {
                return Err("XML : attributs invalides".into());
            }
            let key = self.name()?;
            self.whitespace();
            if !self.tail().starts_with('=') {
                return Err("XML : signe égal attendu".into());
            }
            self.at += 1;
            self.whitespace();
            let quote = self
                .input
                .as_bytes()
                .get(self.at)
                .copied()
                .filter(|q| matches!(q, b'\'' | b'"'))
                .ok_or("XML : attribut non cité")?;
            self.at += 1;
            let end = self
                .tail()
                .find(quote as char)
                .ok_or("XML : attribut incomplet")?;
            if end > 8192 || self.tail()[..end].contains('<') {
                return Err("XML : attribut trop long ou invalide".into());
            }
            let value = xml_unescape(&self.tail()[..end])?;
            self.at += end + 1;
            if attrs.insert(key, value).is_some() {
                return Err("XML : attribut dupliqué".into());
            }
        }
        let mut out = Element {
            name,
            attrs,
            text: String::new(),
            children: Vec::new(),
        };
        if closed {
            return Ok(out);
        }
        loop {
            if self.tail().starts_with("</") {
                self.at += 2;
                let name = self.name()?;
                self.whitespace();
                if name != out.name || !self.tail().starts_with('>') {
                    return Err("XML : fermeture incorrecte".into());
                }
                self.at += 1;
                return Ok(out);
            }
            if self.tail().starts_with("<!--") {
                self.comment()?;
                continue;
            }
            if self.tail().starts_with("<![CDATA[") {
                self.at += 9;
                let end = self.tail().find("]]>").ok_or("XML : CDATA incomplet")?;
                let value = &self.tail()[..end];
                if value
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
                {
                    return Err("XML : CDATA invalide".into());
                }
                out.text.push_str(value);
                self.at += end + 3;
                continue;
            }
            if self.tail().starts_with("<!") || self.tail().starts_with("<?") {
                return Err("XML : DTD et instructions internes interdits".into());
            }
            if self.tail().starts_with('<') {
                out.children.push(self.element(depth + 1)?);
                continue;
            }
            let end = self.tail().find('<').ok_or("XML : élément incomplet")?;
            out.text.push_str(&xml_unescape(&self.tail()[..end])?);
            self.at += end;
        }
    }
}

fn parse_xml(text: &str) -> Result<Element> {
    if text.len() > MAX_XML {
        return Err("XML : document trop grand".into());
    }
    let mut parser = Xml {
        input: text.strip_prefix('\u{feff}').unwrap_or(text),
        at: 0,
        nodes: 0,
    };
    parser.whitespace();
    if parser.tail().starts_with("<?xml ") {
        let end = parser
            .tail()
            .find("?>")
            .ok_or("XML : déclaration incomplète")?;
        if end > 256
            || parser.tail()[..end].contains('<') && parser.tail()[..end].matches('<').count() > 1
        {
            return Err("XML : déclaration invalide".into());
        }
        let declaration = &parser.tail()[..end];
        if declaration.contains("encoding") && !declaration.to_ascii_lowercase().contains("utf-8") {
            return Err("XML : seul UTF-8 est pris en charge".into());
        }
        parser.at += end + 2;
    }
    parser.whitespace();
    while parser.tail().starts_with("<!--") {
        parser.comment()?;
        parser.whitespace();
    }
    let element = parser.element(0)?;
    parser.whitespace();
    while parser.tail().starts_with("<!--") {
        parser.comment()?;
        parser.whitespace();
    }
    if !parser.tail().is_empty() {
        return Err("XML : données après le document".into());
    }
    Ok(element)
}

fn rss_releases(text: &str, base: &str) -> Result<Vec<Release>> {
    let root = parse_xml(text)?;
    if root.local() == "error" {
        return Err("Indexeur Torznab : erreur de service".into());
    }
    let channel = if root.local() == "rss" {
        root.child("channel").ok_or("RSS : canal absent")?
    } else {
        return Err("RSS : racine rss attendue".into());
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
            title: "L'Été".into(),
            year: 2024,
            season: 0,
            episode: 0,
            source_path: None,
            source_url: None,
            tmdb_id: None,
        }
    }

    #[test]
    fn release_identity_rejects_sequels_wrong_years_and_episodes() {
        let mut request = movie();
        assert!(release_matches(&request, "L.Ete.2024.1080p.WEB-DL"));
        assert!(!release_matches(&request, "L.Ete.2.2024.1080p"));
        assert!(!release_matches(&request, "L.Ete.2023.1080p"));
        assert!(!release_matches(&request, "L.Ete.2024.S01E01"));
        assert!(!release_matches(&request, "La.Suite.De.L.Ete.2024"));
        request.kind = "episode".into();
        request.season = 2;
        request.episode = 3;
        assert!(release_matches(&request, "L.Ete.S02E03.1080p"));
        assert!(release_matches(&request, "L.Ete.2024.2x03.1080p"));
        assert!(!release_matches(&request, "L.Ete.S02E04.1080p"));
        assert!(!release_matches(&request, "L.Ete.S02E03E04.1080p"));
        assert!(!release_matches(&request, "L.Ete.S02E03-E04.1080p"));
        assert!(!release_matches(&request, "L.Ete.S02E03.S03E01.1080p"));
        assert!(!release_matches(&request, "L.Ete.Un.Autre.Show.S02E03"));
    }

    #[test]
    fn rss_namespace_cdata_entities_and_enclosures_are_decoded() {
        let feed = r#"<?xml version="1.0" encoding="UTF-8"?>
            <rss xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel>
            <item><title><![CDATA[L'Été.2024.1080p]]></title>
            <link>https://invalid.example/details</link>
            <enclosure url="https://index.example/download?id=2&amp;key=a%26b"/>
            <torznab:attr name="seeders" value="19"/></item>
            <item><title>Film &#xE9; &#233;</title><torznab:attr name="magneturl" value="magnet:?xt=urn:btih:abc&amp;dn=test"/>
            <seeders>4</seeders></item></channel></rss>"#;
        let releases = rss_releases(feed, "https://index.example/rss").unwrap();
        assert_eq!(releases.len(), 2);
        assert_eq!(
            releases[0].url,
            "https://index.example/download?id=2&key=a%26b"
        );
        assert_eq!(releases[0].seeders, 19);
        assert_eq!(releases[1].title, "Film é é");
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
