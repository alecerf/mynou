use crate::{
    Result,
    json::{self, Value},
    torrent::{DownloadConfig, TransferPolicy},
};
use std::fs;
use std::path::{Component, Path, PathBuf};

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
    pub store_dir: PathBuf,
    pub listen: String,
    pub api_token_env: String,
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
    pub plex: Plex,
    pub catalog: Catalog,
    pub sources: Vec<Source>,
    pub requesters: crate::requesters::Settings,
    pub irc: crate::irc::Settings,
    pub notifications: crate::notifications::Settings,
    pub usenet: crate::usenet::Settings,
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
        ("store_dir", "state/jobs".into()),
        ("listen", "127.0.0.1:8787".into()),
        ("api_token_env", "MYNOU_API_TOKEN".into()),
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
                ("max_peers", n(4)),
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
                ("url", "http://host.docker.internal:32400".into()),
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

pub fn load(file: &Path) -> Result<Config> {
    use std::io::Read;
    let mut data = String::new();
    fs::File::open(file)
        .and_then(|f| f.take(1_048_577).read_to_string(&mut data))
        .map_err(|e| format!("Configuration: {e}"))?;
    if data.len() > 1_048_576 {
        return Err("Configuration is too large (maximum 1 MiB)".into());
    }
    let v = json::parse(&data)?;
    let base = file.parent().unwrap_or(Path::new("."));
    from_json(&v, base)
}
pub fn from_json(v: &Value, base: &Path) -> Result<Config> {
    keys(
        v,
        &[
            "schema_version",
            "store_dir",
            "listen",
            "api_token_env",
            "poll_interval_ms",
            "lease_duration_secs",
            "workers",
            "max_attempts",
            "minimum_seeders",
            "selection",
            "monitoring",
            "series_packs",
            "library",
            "downloads",
            "plex",
            "catalog",
            "indexers",
            "requesters",
            "irc",
            "notifications",
            "usenet",
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
    let workers = number(v, "workers", 2, 32)? as usize;
    let max_active = number(d, "max_active", 2, 64)? as usize;
    let max_peers = number(d, "max_peers", 4, 8)? as usize;
    let poll_interval_ms = number(v, "poll_interval_ms", 500, 3600000)?;
    let lease_duration_secs = number(v, "lease_duration_secs", 60, 86400)?;
    if workers == 0
        || max_active == 0
        || max_peers == 0
        || poll_interval_ms < 10
        || lease_duration_secs < 5
    {
        return Err("Configuration: invalid concurrency or timing settings".into());
    }
    let listen = text(v, "listen", "127.0.0.1:8787")?;
    listen
        .parse::<std::net::SocketAddr>()
        .map_err(|_| "Configuration: listen requires an IP address and port")?;
    for url in [
        text(p, "url", "http://host.docker.internal:32400")?,
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
    let base = absolute_path(base)?;
    let requesters = crate::requesters::Settings::from_json(v.get("requesters"), &base)?;
    let irc = match v.get("irc") {
        Some(v) => crate::irc::Settings::from_json(v, &selection)?,
        None => crate::irc::Settings::default(),
    };
    if irc.rules.iter().any(|r| {
        r.requester
            .as_ref()
            .is_some_and(|id| !requesters.accounts.iter().any(|a| &a.id == id))
    }) {
        return Err("IRC: rule requester is not configured".into());
    }
    let notifications =
        crate::notifications::Settings::from_json(v.get("notifications"), &requesters, &irc)?;
    Ok(Config {
        store_dir: path(&base, text(v, "store_dir", "state/jobs")?)?,
        listen,
        api_token_env: text(v, "api_token_env", "MYNOU_API_TOKEN")?,
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
        plex: Plex {
            enabled: boolean(p, "enabled", false)?,
            url: text(p, "url", "http://host.docker.internal:32400")?,
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
        notifications,
        usenet: crate::usenet::Settings::from_json(v.get("usenet"))?,
    })
}
pub fn secret(name: &str) -> Result<String> {
    let value = std::env::var(name).unwrap_or_default().trim().to_owned();
    if value.is_empty() {
        Err(format!("Missing secret {name}"))
    } else if value.contains(['\r', '\n', '\0']) {
        Err(format!("Invalid secret {name}"))
    } else {
        Ok(value)
    }
}
