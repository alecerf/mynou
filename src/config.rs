use crate::{
    Result,
    json::Value,
    torrent::{DownloadConfig, TransferPolicy},
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;

/// The variable commands read the access token from; Mynou itself keeps only
/// the token's digest, never a saved secret of this name.
pub const API_TOKEN_ENV: &str = "MYNOU_API_TOKEN";
/// Secret values in one store record, and the size of each.
pub const MAX_SECRETS: usize = 256;
pub const MAX_SECRET_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitoring {
    pub enabled: bool,
    pub interval_secs: u64,
    pub max_checks: usize,
}

impl Default for Monitoring {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 3600,
            max_checks: 32,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathMapping {
    pub mynou_prefix: String,
    pub plex_prefix: String,
}

#[derive(Clone, Debug)]
pub struct Plex {
    pub enabled: bool,
    pub url: String,
    pub watchlist_url: String,
    pub token_env: String,
    pub movies_section: String,
    pub series_section: String,
    pub token_override: Option<String>,
    pub path_mappings: Vec<PathMapping>,
}
#[derive(Clone, Debug)]
pub struct Catalog {
    pub enabled: bool,
    pub url: String,
    pub token_env: String,
    pub api_key_env: String,
}
#[derive(Clone, Debug)]
pub struct Source {
    pub options: crate::indexers::Options,
    pub name: String,
    pub kind: String,
    pub url: String,
    pub api_key_env: String,
}
#[derive(Clone, Debug)]
pub struct Config {
    /// Holds `state/`; relative settings paths resolve against it.
    pub data_dir: PathBuf,
    /// The settings document this configuration was validated from.
    pub document: Value,
    pub store_dir: PathBuf,
    pub listen: String,
    pub movies_root: PathBuf,
    pub series_root: PathBuf,
    pub downloads: DownloadConfig,
    pub download_policy: TransferPolicy,
    pub downloads_enabled: bool,
    pub poll_interval_ms: u64,
    pub lease_duration_secs: u64,
    pub workers: usize,
    pub max_attempts: u32,
    pub minimum_seeders: u64,
    pub selection: crate::selection::SelectionConfig,
    pub monitoring: Monitoring,
    pub series_packs_enabled: bool,
    /// Install published updates in the background, the way Plex does.
    pub updates_automatic: bool,
    pub plex: Plex,
    pub catalog: Catalog,
    pub sources: Vec<Source>,
    pub requesters: crate::requesters::Settings,
    pub irc: crate::irc::Settings,
}

fn object(items: Vec<(&str, Value)>) -> Value {
    Value::Object(items.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn n(v: u64) -> Value {
    Value::Number(v as f64)
}
pub fn default_json() -> Value {
    object(vec![
        ("schema_version", n(1)),
        ("listen", "127.0.0.1:8787".into()),
        ("poll_interval_ms", n(500)),
        ("lease_duration_secs", n(60)),
        ("workers", n(2)),
        ("max_attempts", n(10)),
        ("minimum_seeders", n(1)),
        (
            "selection",
            crate::selection::SelectionConfig::default().to_json(),
        ),
        (
            "monitoring",
            object(vec![
                ("enabled", false.into()),
                ("interval_secs", n(3600)),
                ("max_checks", n(32)),
            ]),
        ),
        (
            "library",
            object(vec![
                ("movies_root", "library/movies".into()),
                ("series_root", "library/series".into()),
            ]),
        ),
        (
            "downloads",
            object(vec![
                ("enabled", true.into()),
                ("data_dir", "downloads".into()),
                ("state_dir", "state/torrents".into()),
                ("listen_port", n(6881)),
                ("seed", true.into()),
                ("dht", true.into()),
                ("pex", true.into()),
                ("max_active", n(2)),
                ("max_seeding", n(0)),
                ("max_peers", n(50)),
                ("download_limit_bps", n(0)),
                ("upload_limit_bps", n(0)),
                ("seed_ratio_milli", Value::Null),
                ("seed_time_secs", Value::Null),
            ]),
        ),
        (
            "plex",
            object(vec![
                ("enabled", false.into()),
                ("url", "http://127.0.0.1:32400".into()),
                (
                    "watchlist_url",
                    "https://discover.provider.plex.tv/library/sections/watchlist/all".into(),
                ),
                ("token_env", "MYNOU_PLEX_TOKEN".into()),
                ("movies_section", "1".into()),
                ("series_section", "2".into()),
                ("path_mappings", Value::Array(Vec::new())),
            ]),
        ),
        (
            "catalog",
            object(vec![
                ("enabled", false.into()),
                ("url", "https://api.themoviedb.org/3".into()),
                ("token_env", "MYNOU_TMDB_TOKEN".into()),
                ("api_key_env", "MYNOU_TMDB_API_KEY".into()),
            ]),
        ),
        ("series_packs", object(vec![("enabled", false.into())])),
        ("updates", object(vec![("automatic", true.into())])),
        ("indexers", Value::Array(Vec::new())),
    ])
}

fn text(v: &Value, k: &str, default: &str) -> Result<String> {
    match v.get(k) {
        None => Ok(default.into()),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().into()),
        _ => Err(format!("Configuration: {k} must be a nonempty string")),
    }
}
fn number(v: &Value, k: &str, default: u64, max: u64) -> Result<u64> {
    let n = match v.get(k) {
        None => default,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| format!("Configuration: {k} must be an integer"))?,
    };
    if n > max {
        return Err(format!("Configuration: {k} is too large"));
    }
    Ok(n)
}
fn boolean(v: &Value, k: &str, default: bool) -> Result<bool> {
    match v.get(k) {
        None => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| format!("Configuration: {k} must be a boolean")),
    }
}
fn section<'a>(v: &'a Value, k: &str) -> Result<&'a Value> {
    let s = v
        .get(k)
        .ok_or_else(|| format!("Configuration: missing section {k}"))?;
    if s.as_object().is_none() {
        return Err(format!("Configuration: {k} must be an object"));
    }
    Ok(s)
}
fn keys(v: &Value, allowed: &[&str]) -> Result<()> {
    let map = v.as_object().ok_or("Configuration: expected an object")?;
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("Configuration: unknown field {key}"));
        }
    }
    Ok(())
}
fn absolute_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("Configuration: cannot determine current directory: {error}"))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Collapsing a parent after a symlink changes its filesystem meaning.
                // Keep ordinary symlink components visible to storage/import checks.
                crate::store::reject_symlinks(&normalized)
                    .map_err(|error| format!("Configuration: {error}"))?;
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

pub(crate) fn path(base: &Path, value: String) -> Result<PathBuf> {
    let p = PathBuf::from(value);
    absolute_path(&if p.is_absolute() { p } else { base.join(p) })
}

fn mapping_path(value: &Value, name: &str) -> Result<String> {
    let path = value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Configuration: path mapping {name} must be a string"))?;
    if path.len() > 4096
        || !path.starts_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || (path != "/"
            && path[1..]
                .split('/')
                .any(|component| matches!(component, "" | "." | "..")))
    {
        return Err(format!(
            "Configuration: path mapping {name} requires a normalized absolute path of at most 4096 bytes"
        ));
    }
    Ok(path.into())
}

fn path_mappings(plex: &Value) -> Result<Vec<PathMapping>> {
    let Some(value) = plex.get("path_mappings") else {
        return Ok(Vec::new());
    };
    let entries = value
        .as_array()
        .ok_or("Configuration: plex.path_mappings must be an array")?;
    if entries.len() > 32 {
        return Err("Configuration: plex.path_mappings allows at most 32 entries".into());
    }
    let mut mappings = Vec::with_capacity(entries.len());
    let mut prefixes = std::collections::BTreeSet::new();
    for entry in entries {
        keys(entry, &["mynou_prefix", "plex_prefix"])?;
        let mynou_prefix = mapping_path(entry, "mynou_prefix")?;
        let plex_prefix = mapping_path(entry, "plex_prefix")?;
        if !prefixes.insert(mynou_prefix.clone()) {
            return Err("Configuration: duplicate Plex path mapping Mynou prefix".into());
        }
        mappings.push(PathMapping {
            mynou_prefix,
            plex_prefix,
        });
    }
    Ok(mappings)
}

fn monitoring(value: &Value) -> Result<Monitoring> {
    let Some(section) = value.get("monitoring") else {
        return Ok(Monitoring::default());
    };
    keys(section, &["enabled", "interval_secs", "max_checks"])?;
    let interval_secs = number(section, "interval_secs", 3600, 86400)?;
    let max_checks = number(section, "max_checks", 32, 256)? as usize;
    if interval_secs < 60 || max_checks == 0 {
        return Err(
            "Configuration: monitoring requires interval_secs between 60 and 86400 and max_checks between 1 and 256"
                .into(),
        );
    }
    Ok(Monitoring {
        enabled: boolean(section, "enabled", false)?,
        interval_secs,
        max_checks,
    })
}

fn download_policy(downloads: &Value) -> Result<TransferPolicy> {
    let policy = Value::Object(
        [
            "download_limit_bps",
            "upload_limit_bps",
            "seed_ratio_milli",
            "seed_time_secs",
        ]
        .into_iter()
        .filter_map(|name| {
            downloads
                .get(name)
                .map(|value| (name.into(), value.clone()))
        })
        .collect(),
    );
    TransferPolicy::from_json(&policy).map_err(|error| format!("Configuration: downloads: {error}"))
}

/// The data directory: `--dir`, else `MYNOU_HOME`, else the account's
/// `~/Library/Application Support/Mynou`.
pub fn data_dir(option: Option<&str>) -> Result<PathBuf> {
    let path = match option {
        Some(path) if !path.trim().is_empty() => PathBuf::from(path),
        Some(_) => return Err("--dir requires a directory".into()),
        None => match std::env::var_os("MYNOU_HOME").filter(|v| !v.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(std::env::var_os("HOME").ok_or(
                "Set --dir or MYNOU_HOME: HOME is missing for the default data directory",
            )?)
            .join("Library/Application Support/Mynou"),
        },
    };
    absolute_path(&path)
}

/// The job store, which also holds the settings and secrets.
pub fn store_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("state/jobs")
}

/// Loads the saved settings and secrets of a stopped installation; without
/// saved settings, the defaults apply. A running service holds the store.
pub fn load(data_dir: &Path) -> Result<Config> {
    let data_dir = absolute_path(data_dir)?;
    let store = store_dir(&data_dir);
    let (document, secrets) = if crate::store::Store::directory_exists(&store)? {
        let mut store = crate::store::Store::open_read_only(&store).map_err(|error| {
            if error.contains("already open") {
                format!(
                    "{error}; Mynou is running: set {API_TOKEN_ENV} to its access token to reach it"
                )
            } else {
                error
            }
        })?;
        (store.settings()?, store.secrets()?)
    } else {
        (None, BTreeMap::new())
    };
    install_secrets(secrets);
    from_json(&document.unwrap_or_else(default_json), &data_dir)
}
pub fn from_json(v: &Value, base: &Path) -> Result<Config> {
    keys(
        v,
        &[
            "schema_version",
            "listen",
            "poll_interval_ms",
            "lease_duration_secs",
            "workers",
            "max_attempts",
            "minimum_seeders",
            "selection",
            "monitoring",
            "series_packs",
            "updates",
            "library",
            "downloads",
            "plex",
            "catalog",
            "indexers",
            "requesters",
            "irc",
        ],
    )?;
    if number(v, "schema_version", 0, 1)? != 1 {
        return Err(
            "Configuration: use Rust schema 1; previous Go SQLite data remains separate".into(),
        );
    }
    let library = section(v, "library")?;
    keys(library, &["movies_root", "series_root"])?;
    let d = section(v, "downloads")?;
    keys(
        d,
        &[
            "enabled",
            "data_dir",
            "state_dir",
            "listen_port",
            "seed",
            "dht",
            "pex",
            "max_active",
            "max_seeding",
            "max_peers",
            "download_limit_bps",
            "upload_limit_bps",
            "seed_ratio_milli",
            "seed_time_secs",
        ],
    )?;
    let p = section(v, "plex")?;
    keys(
        p,
        &[
            "enabled",
            "url",
            "watchlist_url",
            "token_env",
            "movies_section",
            "series_section",
            "path_mappings",
        ],
    )?;
    let c = section(v, "catalog")?;
    keys(c, &["enabled", "url", "token_env", "api_key_env"])?;
    let mut sources = Vec::new();
    if let Some(indexers) = v.get("indexers") {
        for source in indexers
            .as_array()
            .ok_or("Configuration: indexers must be an array")?
        {
            keys(
                source,
                &[
                    "name",
                    "kind",
                    "url",
                    "api_key_env",
                    "enabled",
                    "min_interval_ms",
                    "authentication",
                    "id",
                    "watch",
                    "mapping",
                    "protection",
                    "cross_seed",
                ],
            )?;
            let kind = text(source, "kind", "json")?;
            if !["rss", "json", "torznab"].contains(&kind.as_str()) {
                return Err("Configuration: expected an rss, json or torznab source".into());
            }
            let url = text(source, "url", "")?;
            crate::net::parse_url(&url)?;
            sources.push(Source {
                options: crate::indexers::Options::from_json(source, &url)?,
                name: text(source, "name", "source")?,
                kind,
                url,
                api_key_env: text(source, "api_key_env", "MYNOU_INDEXER_API_KEY")?,
            });
        }
    }
    if sources.iter().filter(|s| s.options.watch.enabled).count()
        > crate::indexers::feed::MAX_WATCHED
    {
        return Err("Configuration: too many watched feeds".into());
    }
    let workers = number(v, "workers", 2, 32)? as usize;
    let max_active = number(d, "max_active", 2, u32::MAX as u64)? as usize;
    let max_peers = number(d, "max_peers", 50, 256)? as usize;
    let poll_interval_ms = number(v, "poll_interval_ms", 500, 3600000)?;
    let lease_duration_secs = number(v, "lease_duration_secs", 60, 86400)?;
    if workers == 0 || max_peers == 0 || poll_interval_ms < 10 || lease_duration_secs < 5 {
        return Err("Configuration: invalid concurrency or timing settings".into());
    }
    let listen = text(v, "listen", "127.0.0.1:8787")?;
    listen
        .parse::<std::net::SocketAddr>()
        .map_err(|_| "Configuration: listen requires an IP address and port")?;
    for url in [
        text(p, "url", "http://127.0.0.1:32400")?,
        text(
            p,
            "watchlist_url",
            "https://discover.provider.plex.tv/library/sections/watchlist/all",
        )?,
        text(c, "url", "https://api.themoviedb.org/3")?,
    ] {
        crate::net::parse_url(&url)?;
    }
    let selection = match v.get("selection") {
        Some(selection) => crate::selection::SelectionConfig::from_json(selection)?,
        None => crate::selection::SelectionConfig::default(),
    };
    let series_packs_enabled = match v.get("series_packs") {
        None => false,
        Some(section) => {
            keys(section, &["enabled"])?;
            boolean(section, "enabled", false)?
        }
    };
    let updates_automatic = match v.get("updates") {
        None => true,
        Some(section) => {
            keys(section, &["automatic"])?;
            boolean(section, "automatic", true)?
        }
    };
    let base = absolute_path(base)?;
    let requesters = crate::requesters::Settings::from_json(v.get("requesters"), &base)?;
    let irc = match v.get("irc") {
        Some(v) => crate::irc::Settings::from_json(v, &selection)?,
        None => crate::irc::Settings::default(),
    };
    for source in &irc.sources {
        if source.protection == crate::source_network::Protection::Protected
            && let Some(lookup) = &source.announcement_lookup
            && !sources.iter().any(|s| {
                s.options.id.as_deref() == Some(&lookup.indexer)
                    && s.options.protection == crate::source_network::Protection::Protected
            })
        {
            return Err("IRC: protected lookup requires a protected indexer".into());
        }
    }
    if irc.rules.iter().any(|r| {
        r.requester
            .as_ref()
            .is_some_and(|id| !requesters.accounts.iter().any(|a| &a.id == id))
    }) {
        return Err("IRC: rule requester is not configured".into());
    }
    for lookup in irc
        .sources
        .iter()
        .filter_map(|s| s.announcement_lookup.as_ref())
    {
        if !sources.iter().any(|s| {
            s.options.id.as_deref() == Some(&lookup.indexer)
                && (s.kind == "torznab"
                    || (s.kind == "json" && s.options.mapping.describes_details()))
        }) {
            return Err("IRC: lookup_indexer must name a Torznab indexer, or a JSON indexer whose mapping has details, tmdb_id and info_hash, with that id".into());
        }
    }
    Ok(Config {
        store_dir: store_dir(&base),
        data_dir: base.clone(),
        document: v.clone(),
        listen,
        movies_root: path(&base, text(library, "movies_root", "library/movies")?)?,
        series_root: path(&base, text(library, "series_root", "library/series")?)?,
        downloads: DownloadConfig {
            data_dir: path(&base, text(d, "data_dir", "downloads")?)?,
            state_dir: path(&base, text(d, "state_dir", "state/torrents")?)?,
            listen_port: number(d, "listen_port", 6881, 65535)? as u16,
            seed: boolean(d, "seed", true)?,
            dht: boolean(d, "dht", true)?,
            pex: boolean(d, "pex", true)?,
            max_active,
            max_seeding: number(d, "max_seeding", 0, u32::MAX as u64)? as usize,
            max_peers,
        },
        download_policy: download_policy(d)?,
        downloads_enabled: boolean(d, "enabled", true)?,
        poll_interval_ms,
        lease_duration_secs,
        workers,
        max_attempts: number(v, "max_attempts", 10, 1000)? as u32,
        minimum_seeders: number(v, "minimum_seeders", 1, 1000000)?,
        selection,
        monitoring: monitoring(v)?,
        series_packs_enabled,
        updates_automatic,
        plex: Plex {
            enabled: boolean(p, "enabled", false)?,
            url: text(p, "url", "http://127.0.0.1:32400")?,
            watchlist_url: text(
                p,
                "watchlist_url",
                "https://discover.provider.plex.tv/library/sections/watchlist/all",
            )?,
            token_env: text(p, "token_env", "MYNOU_PLEX_TOKEN")?,
            movies_section: text(p, "movies_section", "1")?,
            series_section: text(p, "series_section", "2")?,
            token_override: None,
            path_mappings: path_mappings(p)?,
        },
        catalog: Catalog {
            enabled: boolean(c, "enabled", false)?,
            url: text(c, "url", "https://api.themoviedb.org/3")?,
            token_env: text(c, "token_env", "MYNOU_TMDB_TOKEN")?,
            api_key_env: text(c, "api_key_env", "MYNOU_TMDB_API_KEY")?,
        },
        sources,
        requesters,
        irc,
    })
}
/// Secrets saved in the store, installed when settings load and whenever one
/// is saved, so that a change applies without a restart.
static STORED: RwLock<BTreeMap<String, String>> = RwLock::new(BTreeMap::new());

pub fn install_secrets(secrets: BTreeMap<String, String>) {
    *STORED.write().unwrap_or_else(|e| e.into_inner()) = secrets;
}

fn stored(name: &str) -> Option<String> {
    STORED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
}

/// The raw saved secret, else the environment variable of the same name.
pub fn lookup(name: &str) -> Option<String> {
    stored(name).or_else(|| std::env::var(name).ok())
}

/// A saved secret, else the environment variable of the same name.
pub fn secret(name: &str) -> Result<String> {
    let value = lookup(name).unwrap_or_default().trim().to_owned();
    if value.is_empty() {
        Err(format!("Missing secret {name}"))
    } else if value.contains(['\r', '\n', '\0']) {
        Err(format!("Invalid secret {name}"))
    } else {
        Ok(value)
    }
}

/// Like `secret`, but an unset secret is `None` instead of an error.
pub fn optional_secret(name: &str) -> Result<Option<String>> {
    match source(name) {
        "missing" => Ok(None),
        _ => secret(name).map(Some),
    }
}

/// Where a secret comes from: `stored`, `environment` or `missing`.
pub fn source(name: &str) -> &'static str {
    if stored(name).is_some_and(|v| !v.trim().is_empty()) {
        "stored"
    } else if std::env::var(name).is_ok_and(|v| !v.trim().is_empty()) {
        "environment"
    } else {
        "missing"
    }
}

/// Secret names are environment-variable names; the API token is not one.
pub fn check_secret_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_uppercase() || i > 0 && b.is_ascii_digit())
    {
        return Err("Secret names use 1 to 128 uppercase letters, digits and underscores".into());
    }
    if name == API_TOKEN_ENV {
        return Err(
            "The access token is not a saved secret; `mynou token --reset` replaces it".into(),
        );
    }
    Ok(())
}

pub fn check_secret_value(value: &str) -> Result<()> {
    if value.trim().is_empty()
        || value.len() > MAX_SECRET_BYTES
        || value.chars().any(char::is_control)
    {
        return Err("Secret values use 1 to 4096 bytes without control characters".into());
    }
    Ok(())
}

/// Every secret a settings document names (fields ending in `_env`), with
/// the usual ones, so that the interface can offer to set them.
pub fn secret_names(document: &Value) -> BTreeSet<String> {
    fn walk(value: &Value, names: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    match value {
                        Value::String(name) if key.ends_with("_env") => {
                            if check_secret_name(name).is_ok() {
                                names.insert(name.clone());
                            }
                        }
                        _ => walk(value, names),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, names)),
            _ => {}
        }
    }
    let mut names: BTreeSet<String> = [
        "MYNOU_PLEX_TOKEN",
        "MYNOU_TMDB_TOKEN",
        "MYNOU_TMDB_API_KEY",
        "MYNOU_INDEXER_API_KEY",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    walk(document, &mut names);
    names
}
