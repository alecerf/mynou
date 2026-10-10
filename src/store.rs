//! Verified transactional journal without an external database engine.
//!
//! A transaction is acknowledged only after the journal is synchronized.
//! An incomplete final record is recoverable; a complete record with an
//! incorrect digest produces an explicit error.

use crate::Result;
use crate::crypto::sha256;
use crate::json::{self, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const JOURNAL_MAGIC: &[u8; 8] = b"MYNOUJ01";
const SNAPSHOT_MAGIC: &[u8; 8] = b"MYNOUS01";
// Old readers must reject shared ownership instead of silently ignoring its owners.
const SHARED_JOURNAL_MAGIC: &[u8; 8] = b"MYNOUJ02";
const SHARED_SNAPSHOT_MAGIC: &[u8; 8] = b"MYNOUS02";
const GROUP_JOURNAL_MAGIC: &[u8; 8] = b"MYNOUJ03";
const GROUP_SNAPSHOT_MAGIC: &[u8; 8] = b"MYNOUS03";
const REQUESTER_JOURNAL_MAGIC: &[u8; 8] = b"MYNOUJ04";
const REQUESTER_SNAPSHOT_MAGIC: &[u8; 8] = b"MYNOUS04";
const IRC_JOURNAL_MAGIC: &[u8; 8] = b"MYNOUJ05";
const IRC_SNAPSHOT_MAGIC: &[u8; 8] = b"MYNOUS05";
// Formats 6 to 8 recorded the removed Usenet provenance. Never reuse their numbers.
mod groups;
mod irc;
pub use groups::SharedUpgrade;
use groups::{GroupAction, GroupState, group_format};
const MAX_RECORD: usize = 16 * 1024 * 1024;
const MAX_SNAPSHOT: usize = 16 * 1024 * 1024;
const MAX_EVENTS: usize = 1_000;
const MAX_EVENT_MESSAGE: usize = 4_096;
const MAX_JOBS: usize = 10_000;
const COMPACTION_BYTES: u64 = 4 * 1024 * 1024;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub kind: String,
    pub title: String,
    pub year: u32,
    pub season: u32,
    pub episode: u32,
    pub source_path: Option<String>,
    pub source_url: Option<String>,
    pub tmdb_id: Option<u64>,
    pub source_numbering: Option<crate::numbering::SourceNumber>,
}

/// The release description used to compare future acquisitions with an import.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedRelease {
    pub title: String,
    pub profile: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: String,
    pub key: String,
    pub request: Request,
    pub state: String,
    pub progress: f64,
    pub files: Vec<String>,
    pub imports: Vec<String>,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub next_attempt_at: u64,
    pub acquisition_url: Option<String>,
    pub download_id: Option<String>,
    pub lease_id: Option<String>,
    pub lease_until: u64,
    pub release: Option<RecordedRelease>,
    pub upgrade_parent: Option<String>,
    pub monitored: bool,
    pub monitor_checked_at: u64,
    pub pack_file: Option<String>,
    pub pack_origin: Option<crate::pack::PackOrigin>,
    pub shared_file: Option<crate::pack::SharedFile>,
    pub shared_upgrade: Option<SharedUpgrade>,
    pub requester: Option<crate::requesters::Provenance>,
    pub irc_origin: Option<crate::irc::Origin>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: u64,
    pub job_id: String,
    pub state: String,
    pub message: String,
    pub at: u64,
}

pub struct Store {
    directory: PathBuf,
    _lock: File,
    journal: File,
    jobs: BTreeMap<String, Job>,
    by_key: BTreeMap<String, String>,
    shared_by_media: BTreeMap<String, String>,
    shared_by_physical: BTreeMap<(String, String), crate::pack::SharedFile>,
    events: VecDeque<Event>,
    sequence: u64,
    chain: [u8; 32],
    poisoned: bool,
    journal_bytes: u64,
    next_compaction_at: u64,
    maintenance_error: Option<String>,
    read_only: bool,
    preparing: bool,
    recovery_tail: Option<u64>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    )
}

fn number(value: u64) -> Value {
    // Identifiers and timestamps use decimal strings to preserve every bit,
    // unlike JSON floating-point numbers.
    Value::String(value.to_string())
}

fn optional_string(value: &Option<String>) -> Value {
    value
        .as_ref()
        .map_or(Value::Null, |s| Value::String(s.clone()))
}

fn bounded_message(message: &str) -> String {
    let mut length = message.len().min(MAX_EVENT_MESSAGE);
    while !message.is_char_boundary(length) {
        length -= 1;
    }
    message[..length].to_owned()
}

fn fields(value: &Value) -> Result<&BTreeMap<String, Value>> {
    match value {
        Value::Object(value) => Ok(value),
        _ => Err("expected a JSON object".to_owned()),
    }
}

fn field<'a>(map: &'a BTreeMap<String, Value>, key: &str) -> Result<&'a Value> {
    map.get(key).ok_or_else(|| format!("missing field: {key}"))
}

fn string(map: &BTreeMap<String, Value>, key: &str) -> Result<String> {
    match field(map, key)? {
        Value::String(value) => Ok(value.clone()),
        _ => Err(format!("expected a string: {key}")),
    }
}

fn integer(map: &BTreeMap<String, Value>, key: &str) -> Result<u64> {
    match field(map, key)? {
        Value::String(value) => value.parse().map_err(|_| format!("invalid integer: {key}")),
        Value::Number(value)
            if value.is_finite()
                && *value >= 0.0
                && *value <= 9_007_199_254_740_991.0
                && value.fract() == 0.0 =>
        {
            Ok(*value as u64)
        }
        _ => Err(format!("expected an integer: {key}")),
    }
}

fn optional(map: &BTreeMap<String, Value>, key: &str) -> Result<Option<String>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(format!("invalid optional string: {key}")),
    }
}

fn optional_u64(map: &BTreeMap<String, Value>, key: &str) -> Result<Option<u64>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => integer(map, key).map(Some),
    }
}

fn strings(map: &BTreeMap<String, Value>, key: &str) -> Result<Vec<String>> {
    match field(map, key)? {
        Value::Array(values) => values
            .iter()
            .map(|value| match value {
                Value::String(value) => Ok(value.clone()),
                _ => Err(format!("expected a list of strings: {key}")),
            })
            .collect(),
        _ => Err(format!("expected a list: {key}")),
    }
}

fn small_integer(map: &BTreeMap<String, Value>, key: &str) -> Result<u32> {
    u32::try_from(integer(map, key)?).map_err(|_| format!("integer too large: {key}"))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}

fn unhex_digest(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64 {
        return Err("invalid storage digest".to_owned());
    }
    let mut digest = [0; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let decode = |byte: u8| -> Result<u8> {
            match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => Err("invalid storage digest".to_owned()),
            }
        };
        digest[index] = (decode(pair[0])? << 4) | decode(pair[1])?;
    }
    Ok(digest)
}

fn random_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("system randomness unavailable: {error}"))?;
    Ok(hex(&bytes))
}

impl Request {
    pub fn validate(&self) -> Result<()> {
        if self.source_path.is_some() && self.source_url.is_some() {
            return Err("a request cannot contain both a source file and a source URL".to_owned());
        }
        if !matches!(self.kind.as_str(), "movie" | "series" | "episode" | "file") {
            return Err("invalid request kind".to_owned());
        }
        if self.title.trim().is_empty() || self.title.len() > 4096 {
            return Err("title is empty or too long".to_owned());
        }
        if self.year > 9999 || self.season > 9999 || self.episode > 99999 {
            return Err("invalid year, season, or episode".to_owned());
        }
        if let Some(number) = self.source_numbering {
            if self.kind != "episode" || self.episode == 0 {
                return Err("Source numbering requires a canonical episode request".into());
            }
            number.validate()?;
        }
        for source in [&self.source_path, &self.source_url].into_iter().flatten() {
            if source.is_empty() || source.len() > 65_536 || source.contains('\0') {
                return Err("source is empty or invalid".to_owned());
            }
        }
        Ok(())
    }

    pub fn canonical_key(&self) -> String {
        let title = self
            .title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let identity = if let Some(path) = &self.source_path {
            format!("file:{path}")
        } else if let Some(url) = &self.source_url {
            format!("url:{url}")
        } else if let Some(id) = self.tmdb_id {
            format!("tmdb:{id}")
        } else {
            format!("title:{title}:{}", self.year)
        };
        hex(&sha256(
            format!(
                "{}\0{identity}\0{}\0{}",
                self.kind, self.season, self.episode
            )
            .as_bytes(),
        ))
    }

    /// Logical media identity deliberately excludes acquisition paths and URLs.
    /// The historical request key continues to deduplicate explicit sources.
    pub fn media_key(&self) -> String {
        let identity = self.tmdb_id.map_or_else(
            || {
                let title = self
                    .title
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                format!("title:{title}:{}", self.year)
            },
            |id| format!("tmdb:{id}"),
        );
        hex(&sha256(
            format!(
                "{}\0{identity}\0{}\0{}",
                self.kind, self.season, self.episode
            )
            .as_bytes(),
        ))
    }

    pub fn to_json(&self) -> Value {
        let mut value = object([
            ("kind", Value::String(self.kind.clone())),
            ("title", Value::String(self.title.clone())),
            ("year", Value::Number(self.year as f64)),
            ("season", Value::Number(self.season as f64)),
            ("episode", Value::Number(self.episode as f64)),
            ("source_path", optional_string(&self.source_path)),
            ("source_url", optional_string(&self.source_url)),
            ("tmdb_id", self.tmdb_id.map_or(Value::Null, number)),
        ]);
        if let Some(number) = self.source_numbering {
            value.insert("source_numbering", number.to_json());
        }
        value
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let map = fields(value)?;
        let request = Self {
            kind: string(map, "kind")?,
            title: string(map, "title")?,
            year: map
                .get("year")
                .map_or(Ok(0), |_| small_integer(map, "year"))?,
            season: map
                .get("season")
                .map_or(Ok(0), |_| small_integer(map, "season"))?,
            episode: map
                .get("episode")
                .map_or(Ok(0), |_| small_integer(map, "episode"))?,
            source_path: optional(map, "source_path")?,
            source_url: optional(map, "source_url")?,
            source_numbering: match map.get("source_numbering") {
                None | Some(Value::Null) => None,
                Some(value) => Some(crate::numbering::SourceNumber::from_json(value)?),
            },
            tmdb_id: optional_u64(map, "tmdb_id")?,
        };
        request.validate()?;
        Ok(request)
    }
}

impl RecordedRelease {
    pub fn validate(&self) -> Result<()> {
        if self.title.trim().is_empty() || self.title.len() > 2_048 || self.title.contains('\0') {
            return Err(
                "recorded release title must contain 1 to 2,048 bytes without NUL".to_owned(),
            );
        }
        if self.profile.is_empty()
            || self.profile.len() > 64
            || !self
                .profile
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("recorded release profile must contain 1 to 64 ASCII letters, digits, hyphens or underscores".to_owned());
        }
        Ok(())
    }

    pub fn to_json(&self) -> Value {
        object([
            ("title", Value::String(self.title.clone())),
            ("profile", Value::String(self.profile.clone())),
        ])
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let map = fields(value)?;
        let release = Self {
            title: string(map, "title")?,
            profile: string(map, "profile")?,
        };
        release.validate()?;
        Ok(release)
    }
}

impl Job {
    pub fn to_json(&self) -> Value {
        let list =
            |values: &[String]| Value::Array(values.iter().cloned().map(Value::String).collect());
        object([
            (
                "irc_origin",
                self.irc_origin
                    .as_ref()
                    .map_or(Value::Null, crate::irc::Origin::to_json),
            ),
            (
                "requester",
                self.requester
                    .as_ref()
                    .map_or(Value::Null, crate::requesters::Provenance::to_json),
            ),
            ("id", Value::String(self.id.clone())),
            ("key", Value::String(self.key.clone())),
            ("request", self.request.to_json()),
            ("state", Value::String(self.state.clone())),
            ("progress", Value::Number(self.progress)),
            ("files", list(&self.files)),
            ("imports", list(&self.imports)),
            ("attempts", Value::Number(self.attempts as f64)),
            ("last_error", optional_string(&self.last_error)),
            ("created_at", number(self.created_at)),
            ("updated_at", number(self.updated_at)),
            ("next_attempt_at", number(self.next_attempt_at)),
            ("acquisition_url", optional_string(&self.acquisition_url)),
            ("download_id", optional_string(&self.download_id)),
            ("lease_id", optional_string(&self.lease_id)),
            ("lease_until", number(self.lease_until)),
            (
                "release",
                self.release
                    .as_ref()
                    .map_or(Value::Null, RecordedRelease::to_json),
            ),
            ("upgrade_parent", optional_string(&self.upgrade_parent)),
            ("monitored", Value::Bool(self.monitored)),
            ("monitor_checked_at", number(self.monitor_checked_at)),
            ("pack_file", optional_string(&self.pack_file)),
            (
                "pack_origin",
                self.pack_origin
                    .as_ref()
                    .map_or(Value::Null, crate::pack::PackOrigin::to_json),
            ),
            (
                "shared_file",
                self.shared_file
                    .as_ref()
                    .map_or(Value::Null, crate::pack::SharedFile::to_json),
            ),
            (
                "shared_upgrade",
                self.shared_upgrade
                    .as_ref()
                    .map_or(Value::Null, SharedUpgrade::to_json),
            ),
        ])
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let map = fields(value)?;
        let progress = match field(map, "progress")? {
            Value::Number(value) if value.is_finite() && (0.0..=1.0).contains(value) => *value,
            _ => return Err("invalid job progress".to_owned()),
        };
        let job = Self {
            irc_origin: match map.get("irc_origin") {
                None | Some(Value::Null) => None,
                Some(v) => Some(crate::irc::Origin::from_json(v)?),
            },
            requester: match map.get("requester") {
                None | Some(Value::Null) => None,
                Some(v) => Some(crate::requesters::Provenance::from_json(v)?),
            },
            id: string(map, "id")?,
            key: string(map, "key")?,
            request: Request::from_json(field(map, "request")?)?,
            state: string(map, "state")?,
            progress,
            files: strings(map, "files")?,
            imports: strings(map, "imports")?,
            attempts: small_integer(map, "attempts")?,
            last_error: optional(map, "last_error")?,
            created_at: integer(map, "created_at")?,
            updated_at: integer(map, "updated_at")?,
            next_attempt_at: integer(map, "next_attempt_at")?,
            acquisition_url: optional(map, "acquisition_url")?,
            download_id: optional(map, "download_id")?,
            lease_id: optional(map, "lease_id")?,
            lease_until: integer(map, "lease_until")?,
            release: match map.get("release") {
                None | Some(Value::Null) => None,
                Some(value) => Some(RecordedRelease::from_json(value)?),
            },
            upgrade_parent: optional(map, "upgrade_parent")?,
            monitored: match map.get("monitored") {
                None => true,
                Some(Value::Bool(value)) => *value,
                Some(_) => return Err("invalid monitoring flag".to_owned()),
            },
            monitor_checked_at: map
                .get("monitor_checked_at")
                .map_or(Ok(0), |_| integer(map, "monitor_checked_at"))?,
            pack_file: optional(map, "pack_file")?,
            pack_origin: match map.get("pack_origin") {
                None | Some(Value::Null) => None,
                Some(value) => Some(crate::pack::PackOrigin::from_json(value)?),
            },
            shared_file: match map.get("shared_file") {
                None | Some(Value::Null) => None,
                Some(value) => Some(crate::pack::SharedFile::from_json(value)?),
            },
            shared_upgrade: match map.get("shared_upgrade") {
                None | Some(Value::Null) => None,
                Some(value) => Some(SharedUpgrade::from_json(value)?),
            },
        };
        if let Some(p) = &job.requester
            && (!matches!(job.request.kind.as_str(), "movie" | "episode")
                || p.demand_id != crate::requesters::Demand::identity(&p.account_id, &job.request))
        {
            return Err("Invalid requester media identity or captured provenance".into());
        }
        if let Some(origin) = &job.irc_origin {
            origin.validate_job(&job)?;
        }
        if job.shared_upgrade.is_some()
            && (job.shared_file.is_none() || job.upgrade_parent.is_none() || job.release.is_none())
        {
            return Err("Shared replacement requires file, lineage and release provenance".into());
        }
        if job.state == "staged"
            && (job.shared_upgrade.is_none()
                || job.imports.is_empty()
                || job.progress != 1.0
                || job.lease_id.is_some()
                || job.lease_until != 0
                || job.next_attempt_at != 0
                || job.last_error.is_some())
        {
            return Err("Invalid staged shared-owner confirmation".into());
        }
        if let Some(file) = &job.shared_file {
            file.validate_job(&job)?;
        }
        if let Some(path) = &job.pack_file {
            crate::pack::validate_file_path(path)?;
            if job.request.kind != "episode"
                || job.request.source_url.is_none()
                || job.request.source_path.is_some()
            {
                return Err("Pack mapping requires an episode torrent request".into());
            }
        }
        if job
            .pack_origin
            .as_ref()
            .is_some_and(|origin| job.pack_file.is_none() || origin.season != job.request.season)
        {
            return Err("Automatic pack provenance requires a mapped episode".into());
        }
        if job.id.len() != 32
            || !job.id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || job.key != job.request.canonical_key()
            || job.state.is_empty()
            || job.state.len() > 64
            || job.upgrade_parent.as_ref().is_some_and(|parent| {
                parent.len() != 32 || !parent.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err("invalid job identity or state".to_owned());
        }
        Ok(job)
    }
}

impl Event {
    pub fn to_json(&self) -> Value {
        object([
            ("id", number(self.id)),
            ("job_id", Value::String(self.job_id.clone())),
            ("state", Value::String(self.state.clone())),
            ("message", Value::String(self.message.clone())),
            ("at", number(self.at)),
        ])
    }

    fn from_json(value: &Value) -> Result<Self> {
        let map = fields(value)?;
        Ok(Self {
            id: integer(map, "id")?,
            job_id: string(map, "job_id")?,
            state: string(map, "state")?,
            message: string(map, "message")?,
            at: integer(map, "at")?,
        })
    }
}

#[cfg(unix)]
pub(crate) fn private_options() -> OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.mode(0o600);
    #[cfg(target_os = "linux")]
    options.custom_flags(0o400000 | 0o4000); // O_NOFOLLOW | O_NONBLOCK.
    options
}

#[cfg(not(unix))]
pub(crate) fn private_options() -> OpenOptions {
    OpenOptions::new()
}

pub(crate) fn reject_symlinks(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("symbolic link not allowed: {}", ancestor.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot access path: {error}")),
        }
    }
    Ok(())
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("cannot synchronize directory: {error}"))
}

fn secure_file(path: &Path) -> Result<File> {
    reject_symlinks(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err("storage must be a regular file".to_owned());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let file = private_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| format!("cannot open storage: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("storage must be a regular file".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.nlink() != 1 {
            return Err("storage files cannot share hard links".to_owned());
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot set storage permissions: {error}"))?;
    }
    Ok(file)
}

fn read_only_file(path: &Path) -> Result<File> {
    reject_symlinks(path)?;
    if !fs::symlink_metadata(path)
        .map_err(|error| format!("cannot access existing storage: {error}"))?
        .is_file()
    {
        return Err("storage must be a regular file".to_owned());
    }
    let file = private_options()
        .read(true)
        .open(path)
        .map_err(|error| format!("cannot open existing storage for reading: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("storage must be a regular file".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("storage files cannot share hard links".to_owned());
        }
    }
    Ok(file)
}

fn create_private_directory(path: &Path) -> Result<()> {
    if path.exists() {
        return if path.is_dir() {
            Ok(())
        } else {
            Err("storage must be a directory".to_owned())
        };
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        create_private_directory(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|error| format!("cannot create storage: {error}"))?;
    }
    #[cfg(not(unix))]
    fs::create_dir(path).map_err(|error| error.to_string())?;
    sync_directory(path)?;
    sync_directory(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )
}

impl Store {
    /// Checks an existing storage path without creating files or following symlinks.
    pub fn directory_exists(directory: &Path) -> Result<bool> {
        reject_symlinks(directory)?;
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() => Ok(true),
            Ok(_) => Err("storage must be a directory".to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("cannot access storage: {error}")),
        }
    }

    pub fn open(directory: &Path) -> Result<Self> {
        #[cfg(not(unix))]
        return Err("durable storage currently requires a Unix system".to_owned());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if directory.file_name().is_none()
                || directory
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err("storage requires a clean directory path without traversal".to_owned());
            }
            reject_symlinks(directory)?;
            create_private_directory(directory)?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("cannot set storage permissions: {error}"))?;
            let lock = secure_file(&directory.join(".lock"))?;
            lock.try_lock()
                .map_err(|error| format!("storage is already open or cannot be locked: {error}"))?;
            let journal = secure_file(&directory.join("journal.bin"))?;
            sync_directory(directory)?;
            let mut store = Self {
                directory: directory.to_owned(),
                _lock: lock,
                journal,
                jobs: BTreeMap::new(),
                by_key: BTreeMap::new(),
                shared_by_media: BTreeMap::new(),
                shared_by_physical: BTreeMap::new(),
                events: VecDeque::new(),
                sequence: 0,
                chain: [0; 32],
                poisoned: false,
                journal_bytes: 0,
                next_compaction_at: COMPACTION_BYTES,
                maintenance_error: None,
                read_only: false,
                preparing: false,
                recovery_tail: None,
            };
            store.load_snapshot()?;
            store.replay()?;
            Ok(store)
        }
    }

    /// Loads existing durable state without creating files, changing permissions,
    /// or repairing an interrupted journal. The lock excludes concurrent writers.
    pub fn open_read_only(directory: &Path) -> Result<Self> {
        Self::read_existing(directory, false)
    }

    pub(crate) fn prepare(directory: &Path) -> Result<Self> {
        reject_symlinks(directory)?;
        match fs::symlink_metadata(directory) {
            Ok(_) => Self::read_existing(directory, true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::open(directory),
            Err(e) => Err(e.to_string()),
        }
    }

    pub(crate) fn initialize(&mut self) -> Result<()> {
        if !self.preparing {
            return Ok(());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
            self._lock
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        self.journal = secure_file(&self.directory.join("journal.bin"))?;
        if let Some(tail) = self.recovery_tail {
            self.journal
                .set_len(tail)
                .and_then(|_| self.journal.sync_all())
                .map_err(|e| e.to_string())?;
        }
        self.journal
            .seek(SeekFrom::End(0))
            .map_err(|e| e.to_string())?;
        sync_directory(&self.directory)?;
        self.read_only = false;
        self.preparing = false;
        self.recovery_tail = None;
        Ok(())
    }

    fn read_existing(directory: &Path, preparing: bool) -> Result<Self> {
        #[cfg(not(unix))]
        return Err("durable storage currently requires a Unix system".to_owned());

        #[cfg(unix)]
        {
            if directory.file_name().is_none()
                || directory
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err("storage requires a clean directory path without traversal".to_owned());
            }
            reject_symlinks(directory)?;
            if !fs::metadata(directory)
                .map_err(|error| format!("cannot access existing storage: {error}"))?
                .is_dir()
            {
                return Err("storage must be a directory".to_owned());
            }
            let lock = read_only_file(&directory.join(".lock"))?;
            lock.try_lock()
                .map_err(|error| format!("storage is already open or cannot be locked: {error}"))?;
            let journal = read_only_file(&directory.join("journal.bin"))?;
            let mut store = Self {
                directory: directory.to_owned(),
                _lock: lock,
                journal,
                jobs: BTreeMap::new(),
                by_key: BTreeMap::new(),
                shared_by_media: BTreeMap::new(),
                shared_by_physical: BTreeMap::new(),
                events: VecDeque::new(),
                sequence: 0,
                chain: [0; 32],
                poisoned: false,
                journal_bytes: 0,
                next_compaction_at: COMPACTION_BYTES,
                maintenance_error: None,
                read_only: true,
                preparing,
                recovery_tail: None,
            };
            store.load_snapshot()?;
            store.replay()?;
            Ok(store)
        }
    }

    pub(crate) fn job_progress(&self, id: &str) -> Option<(&str, f64, u32)> {
        self.jobs
            .get(id)
            .map(|job| (job.state.as_str(), job.progress, job.attempts))
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.jobs.get(id).cloned()
    }

    /// A maintenance error does not revoke an acknowledged, synchronized write.
    /// Writes with uncertain outcomes block further storage operations.
    pub fn maintenance_error(&self) -> Option<&str> {
        self.maintenance_error.as_deref()
    }

    pub(crate) fn media_keys(&self) -> std::collections::BTreeSet<String> {
        self.jobs
            .values()
            .map(|job| job.request.media_key())
            .collect()
    }

    fn index_shared(&mut self, job: &Job) {
        if let Some(file) = &job.shared_file {
            if job.shared_upgrade.is_none() {
                self.shared_by_media
                    .insert(job.request.media_key(), job.id.clone());
            }
            self.shared_by_physical
                .entry((file.torrent_id.clone(), file.file_path.clone()))
                .or_insert_with(|| file.clone());
        }
    }

    pub fn list(&self) -> Vec<Job> {
        let mut jobs: Vec<_> = self.jobs.values().cloned().collect();
        jobs.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        jobs
    }

    /// A pending or failed upgrade never hides the last successful import.
    /// Lineage tips are resolved in O(n log n); monitoring timestamps do not
    /// affect which unrelated root represents the current library entry.
    pub fn library_jobs(&self) -> Vec<Job> {
        let replaced: BTreeSet<&str> = self
            .jobs
            .values()
            .filter(|job| job.state == "ready" && !job.imports.is_empty())
            .filter_map(|job| job.upgrade_parent.as_deref())
            .collect();
        let Ok(roots) = self.lineage_roots() else {
            return Vec::new();
        };
        let mut current: BTreeMap<String, (&Job, &Job)> = BTreeMap::new();
        for job in self.jobs.values().filter(|job| {
            job.state == "ready" && !job.imports.is_empty() && !replaced.contains(job.id.as_str())
        }) {
            let key = job.request.media_key();
            let root = roots.get(job.id.as_str()).copied().unwrap_or(job);
            match current.get(&key) {
                Some((previous, _))
                    if (previous.created_at, &previous.id) >= (root.created_at, &root.id) => {}
                _ => {
                    current.insert(key, (root, job));
                }
            }
        }
        let mut jobs: Vec<_> = current.into_values().map(|(_, job)| job.clone()).collect();
        jobs.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        jobs
    }

    fn is_library_tip(&self, job: &Job) -> bool {
        self.library_jobs().iter().any(|tip| tip.id == job.id)
    }

    fn pending_upgrade(job: &Job) -> bool {
        job.upgrade_parent.is_some()
            && !matches!(job.state.as_str(), "ready" | "cancelled")
            && (job.shared_upgrade.is_some() || job.state != "failed" || job.next_attempt_at > 0)
    }

    fn upgrade_eligible(&self, job: &Job) -> bool {
        if job.shared_upgrade.is_some() {
            return self.group_eligible(job).is_ok();
        }
        let Some(parent_id) = &job.upgrade_parent else {
            return true;
        };
        self.jobs.get(parent_id).is_some_and(|parent| {
            parent.state == "ready"
                && !parent.imports.is_empty()
                && parent.release.is_some()
                && parent.monitored
                && self.is_library_tip(parent)
        })
    }

    pub fn submit_upgrade(
        &mut self,
        parent_id: &str,
        request: Request,
        release: RecordedRelease,
    ) -> Result<Job> {
        request.validate()?;
        release.validate()?;
        let parent = self
            .get(parent_id)
            .ok_or_else(|| "unknown upgrade parent".to_owned())?;
        if parent.shared_file.is_some() {
            return Err("Shared-file owners require a coordinated group upgrade; individual upgrades are blocked".into());
        }
        if request.source_path.is_some() || request.source_url.is_none() {
            return Err(
                "an upgrade requires an explicit source URL without a source path".to_owned(),
            );
        }
        if request.media_key() != parent.request.media_key() {
            return Err("upgrade media identity differs from its parent".to_owned());
        }
        let url = request.source_url.as_deref().unwrap_or_default();
        if parent.request.source_url.as_deref() == Some(url)
            || parent.acquisition_url.as_deref() == Some(url)
        {
            return Err("an upgrade must use a different acquisition URL".to_owned());
        }
        let key = request.canonical_key();
        if let Some(id) = self.by_key.get(&key) {
            let existing = self
                .get(id)
                .ok_or_else(|| "inconsistent job index".to_owned())?;
            if existing.upgrade_parent.as_deref() != Some(parent_id)
                || existing.request.media_key() != request.media_key()
            {
                return Err(
                    "upgrade source URL is already owned by an unrelated request".to_owned(),
                );
            }
            // Idempotent submissions never reset cancellation or exhausted retries.
            return Ok(existing);
        }
        if parent.state != "ready"
            || parent.imports.is_empty()
            || parent.release.is_none()
            || !parent.monitored
            || !self.is_library_tip(&parent)
        {
            return Err(
                "an upgrade requires a monitored current library import with a release baseline"
                    .to_owned(),
            );
        }
        let media_key = request.media_key();
        if self
            .jobs
            .values()
            .any(|job| Self::pending_upgrade(job) && job.request.media_key() == media_key)
        {
            return Err("an upgrade for this media is already pending".to_owned());
        }
        if self.jobs.len() >= MAX_JOBS {
            return Err("storage capacity reached: 10,000 requests".to_owned());
        }
        let at = now();
        let job = Job {
            id: random_id()?,
            key,
            request,
            state: "queued".to_owned(),
            progress: 0.0,
            files: Vec::new(),
            imports: Vec::new(),
            attempts: 0,
            last_error: None,
            created_at: at,
            updated_at: at,
            next_attempt_at: 0,
            acquisition_url: None,
            download_id: None,
            lease_id: None,
            lease_until: 0,
            release: Some(release),
            upgrade_parent: Some(parent.id),
            monitored: parent.monitored,
            monitor_checked_at: 0,
            pack_file: None,
            pack_origin: None,
            shared_file: None,
            shared_upgrade: None,
            requester: parent.requester.clone(),
            irc_origin: None,
        };
        self.commit(job.clone(), "library upgrade recorded")?;
        Ok(job)
    }

    pub fn record_monitor_check(&mut self, id: &str, at: u64) -> Result<()> {
        let mut job = self
            .get(id)
            .ok_or_else(|| "unknown library entry".to_owned())?;
        if !self.is_library_tip(&job) {
            return Err("monitor checks require a current library import".to_owned());
        }
        job.monitor_checked_at = job.monitor_checked_at.max(at);
        job.updated_at = now();
        self.commit(job, "library monitor check recorded")
    }

    pub fn set_monitored(&mut self, id: &str, monitored: bool) -> Result<Job> {
        let mut job = self
            .get(id)
            .ok_or_else(|| "unknown library entry".to_owned())?;
        if !self.is_library_tip(&job) {
            return Err("monitoring changes require a current library import".to_owned());
        }
        job.monitored = monitored;
        job.updated_at = now();
        self.commit(job.clone(), "library monitoring updated")?;
        Ok(job)
    }

    pub fn set_baseline(&mut self, id: &str, release: RecordedRelease) -> Result<Job> {
        release.validate()?;
        let mut job = self
            .get(id)
            .ok_or_else(|| "unknown library entry".to_owned())?;
        if job.shared_file.is_some() || !self.is_library_tip(&job) || job.release.is_some() {
            return Err(
                "a release baseline can only be added to a current import without a baseline"
                    .to_owned(),
            );
        }
        job.release = Some(release);
        job.updated_at = now();
        self.commit(job.clone(), "library release baseline recorded")?;
        Ok(job)
    }

    pub fn events(&self, id: &str) -> Vec<Event> {
        self.events
            .iter()
            .filter(|event| id.is_empty() || event.job_id == id)
            .cloned()
            .collect()
    }

    pub fn submit(&mut self, request: Request) -> Result<Job> {
        self.submit_with_mapping(request, None, None, None)
    }

    pub(crate) fn submit_requester(
        &mut self,
        request: Request,
        provenance: crate::requesters::Provenance,
    ) -> Result<Job> {
        crate::requesters::Provenance::from_json(&provenance.to_json())?;
        self.submit_with_mapping(request, None, None, Some(provenance))
    }

    pub fn submit_pack(&mut self, request: Request, path: String) -> Result<Job> {
        crate::pack::validate_file_path(&path)?;
        if request.kind != "episode"
            || request.source_url.is_none()
            || request.source_path.is_some()
        {
            return Err("Pack mapping requires an episode torrent request".into());
        }
        self.submit_with_mapping(request, Some(path), None, None)
    }

    pub(crate) fn submit_auto_pack(
        &mut self,
        request: Request,
        path: String,
        origin: crate::pack::PackOrigin,
    ) -> Result<Job> {
        origin.validate()?;
        crate::pack::validate_file_path(&path)?;
        if request.kind != "episode"
            || request.source_url.is_none()
            || request.source_path.is_some()
            || request.season != origin.season
        {
            return Err("Automatic pack provenance requires its mapped catalog season".into());
        }
        self.submit_with_mapping(request, Some(path), Some(origin), None)
    }

    pub(crate) fn check_submission_capacity(&self, count: usize) -> Result<()> {
        if self.jobs.len().saturating_add(count) > MAX_JOBS {
            return Err("storage capacity reached: 10,000 requests".into());
        }
        Ok(())
    }

    pub(crate) fn check_shared_submission(
        &self,
        file: &crate::pack::SharedFile,
        requests: &[Request],
    ) -> Result<Vec<Job>> {
        file.validate()?;
        if requests.is_empty() || requests.len() > crate::pack::MAX_PACK_EPISODES {
            return Err("Invalid shared owner count".into());
        }
        let mut numbers = BTreeSet::new();
        let known: Vec<_> = self
            .jobs
            .values()
            .filter(|job| job.shared_file.as_ref() == Some(file))
            .cloned()
            .collect();
        let mut reused = Vec::new();
        let media_keys = if known.is_empty() {
            self.media_keys()
        } else {
            BTreeSet::new()
        };
        for request in requests {
            request.validate()?;
            if request.kind != "episode"
                || request.tmdb_id != Some(file.tmdb_id)
                || request.season != file.season
                || !(file.first_episode..=file.last_episode).contains(&request.episode)
                || request.source_path.is_some()
                || request.source_url.is_none()
                || !numbers.insert(request.episode)
            {
                return Err("Request differs from the shared canonical owner range".into());
            }
            if let Some(job) = known
                .iter()
                .find(|job| job.request.episode == request.episode)
            {
                reused.push(job.clone());
            } else if media_keys.contains(&request.media_key()) {
                return Err("Shared-file owner already has an unrelated episode request".into());
            }
            if self.jobs.values().any(|job| {
                job.shared_file.is_none()
                    && job.pack_file.as_deref() == Some(file.file_path.as_str())
                    && (job.request.source_url == request.source_url
                        || job.download_id.as_deref() == Some(file.torrent_id.as_str()))
            }) {
                return Err("Physical video is already mapped without shared ownership".into());
            }
        }
        if self
            .shared_by_physical
            .get(&(file.torrent_id.clone(), file.file_path.clone()))
            .is_some_and(|known| known != file)
        {
            return Err("Physical video already has different shared ownership".into());
        }
        if known.is_empty() {
            if numbers
                .iter()
                .copied()
                .ne(file.first_episode..=file.last_episode)
                || requests
                    .iter()
                    .any(|r| r.title != file.title || r.year != file.year)
            {
                return Err(
                    "New shared ownership must record every canonical owner together".into(),
                );
            }
            self.check_submission_capacity(requests.len())?;
        } else if reused.len() != requests.len() {
            return Err("Stored shared ownership is incomplete".into());
        }
        Ok(reused)
    }

    pub(crate) fn submit_shared_file(
        &mut self,
        file: crate::pack::SharedFile,
        requests: Vec<Request>,
    ) -> Result<Vec<Job>> {
        let known = self.check_shared_submission(&file, &requests)?;
        if !known.is_empty() {
            return Ok(known);
        }
        let at = now();
        let mut jobs = Vec::with_capacity(requests.len());
        for request in requests {
            jobs.push(Job {
                id: random_id()?,
                key: request.canonical_key(),
                request,
                state: "queued".into(),
                progress: 0.0,
                files: Vec::new(),
                imports: Vec::new(),
                attempts: 0,
                last_error: None,
                created_at: at,
                updated_at: at,
                next_attempt_at: 0,
                acquisition_url: None,
                download_id: None,
                lease_id: None,
                lease_until: 0,
                release: None,
                upgrade_parent: None,
                monitored: true,
                monitor_checked_at: 0,
                pack_file: Some(file.file_path.clone()),
                pack_origin: None,
                shared_file: Some(file.clone()),
                shared_upgrade: None,
                requester: None,
                irc_origin: None,
            });
        }
        self.validate_shared_batch(&jobs)?;
        self.commit_jobs(jobs.clone(), "shared-file ownership recorded atomically")?;
        Ok(jobs)
    }

    fn validate_shared_batch(&self, jobs: &[Job]) -> Result<()> {
        let first = jobs.first().ok_or("Empty shared ownership transaction")?;
        let file = first
            .shared_file
            .as_ref()
            .ok_or("Ownership transaction requires a shared binding")?;
        let mut numbers = BTreeSet::new();
        let mut keys = BTreeSet::new();
        let mut ids = BTreeSet::new();
        if !(2..=crate::pack::MAX_PACK_EPISODES).contains(&jobs.len()) {
            return Err("Invalid shared ownership transaction size".into());
        }
        for job in jobs {
            if job.shared_file.as_ref() != Some(file)
                || self.jobs.contains_key(&job.id)
                || job.state != "queued"
                || job.lease_id.is_some()
                || job.lease_until != 0
                || job.progress != 0.0
                || job.attempts != 0
                || !job.files.is_empty()
                || !job.imports.is_empty()
                || job.download_id.is_some()
                || job.acquisition_url.is_some()
                || job.release.is_some()
                || job.shared_upgrade.is_some()
                || job.last_error.is_some()
                || job.next_attempt_at != 0
                || !job.monitored
                || job.monitor_checked_at != 0
                || job.created_at != first.created_at
                || job.updated_at != first.updated_at
                || job.request.source_url != first.request.source_url
                || !numbers.insert(job.request.episode)
                || !keys.insert(&job.key)
                || !ids.insert(&job.id)
            {
                return Err("Inconsistent new shared ownership transaction".into());
            }
            self.validate_transaction(job, false)?;
        }
        if numbers
            .into_iter()
            .ne(file.first_episode..=file.last_episode)
        {
            return Err("Incomplete shared ownership transaction".into());
        }
        self.check_submission_capacity(jobs.len())
    }

    fn validate_shared_groups(&self) -> Result<()> {
        let mut groups: BTreeMap<String, GroupState<'_>> = BTreeMap::new();
        let mut physical = BTreeMap::new();
        let mut logical = BTreeMap::new();
        let roots = self.lineage_roots()?;
        for job in self.jobs.values() {
            if let Some(file) = &job.shared_file {
                file.validate_job(job)?;
                let group = groups.entry(file.id()).or_insert_with(|| GroupState {
                    file,
                    owners: BTreeSet::new(),
                    first: job,
                    ready: 0,
                    cancelled: 0,
                });
                let root = roots
                    .get(job.id.as_str())
                    .ok_or("Missing shared lineage root")?;
                let previous = logical.insert(job.request.media_key(), root.id.as_str());
                if group.file != file
                    || !group.owners.insert(job.request.episode)
                    || group.first.release != job.release
                    || group.first.shared_upgrade != job.shared_upgrade
                    || group.first.request.source_url != job.request.source_url
                    || previous.is_some_and(|id| id != root.id)
                {
                    return Err(
                        "Duplicate or conflicting shared owner in persistent storage".into(),
                    );
                }
                if job.shared_upgrade.is_some() {
                    self.validate_group_lineage(job)?;
                }
                group.ready += usize::from(job.state == "ready");
                group.cancelled += usize::from(job.state == "cancelled");
                let key = (&file.torrent_id, &file.file_path);
                if physical
                    .insert(key, file)
                    .is_some_and(|known| known != file)
                {
                    return Err("Conflicting physical shared ownership in storage".into());
                }
            }
        }
        for (_, group) in groups {
            if group.first.shared_upgrade.is_some()
                && ((group.ready > 0 && group.ready != group.owners.len())
                    || (group.cancelled > 0 && group.cancelled != group.owners.len()))
            {
                return Err("Partially promoted or cancelled shared replacement in storage".into());
            }
            if group
                .owners
                .into_iter()
                .ne(group.file.first_episode..=group.file.last_episode)
            {
                return Err("Incomplete shared ownership in persistent storage".into());
            }
        }
        if self
            .jobs
            .values()
            .any(|job| job.shared_file.is_none() && logical.contains_key(&job.request.media_key()))
        {
            return Err("Unrelated request collides with shared ownership in storage".into());
        }
        Ok(())
    }

    pub fn remap_pack(&mut self, id: &str, path: String) -> Result<Job> {
        crate::pack::validate_file_path(&path)?;
        let mut job = self.get(id).ok_or("Unknown pack episode request")?;
        if job.shared_file.is_some()
            || job.pack_file.is_none()
            || !matches!(job.state.as_str(), "failed" | "cancelled")
            || job.lease_id.is_some()
            || !job.imports.is_empty()
        {
            return Err("Mapping correction requires a failed or cancelled pack request without imports or an active lease".into());
        }
        if self.jobs.values().any(|other| {
            other.id != job.id
                && other.request.tmdb_id == job.request.tmdb_id
                && (other.request.source_url == job.request.source_url
                    || (job.download_id.is_some() && other.download_id == job.download_id))
                && other.pack_file.as_deref() == Some(&path)
        }) {
            return Err("Mapped file already belongs to another episode in this pack".into());
        }
        job.pack_file = Some(path);
        job.state = "queued".into();
        job.files.clear();
        job.progress = 0.0;
        job.attempts = 0;
        job.last_error = None;
        job.next_attempt_at = 0;
        job.updated_at = now();
        self.commit(job.clone(), "pack mapping corrected; request requeued")?;
        Ok(job)
    }

    fn submit_with_mapping(
        &mut self,
        request: Request,
        pack_file: Option<String>,
        pack_origin: Option<crate::pack::PackOrigin>,
        requester: Option<crate::requesters::Provenance>,
    ) -> Result<Job> {
        request.validate()?;
        let key = request.canonical_key();
        if let Some(id) = self.by_key.get(&key) {
            let existing = self
                .get(id)
                .ok_or_else(|| "inconsistent job index".to_owned())?;
            if pack_file.is_some()
                && (existing.pack_file != pack_file
                    || existing.request.media_key() != request.media_key())
            {
                return Err("Pack mapping conflicts with an existing request".into());
            }
            if let Some(p) = &requester
                && existing
                    .requester
                    .as_ref()
                    .is_none_or(|old| old.capture != p.capture)
            {
                return Err("Requester admission cannot rebind existing job policy".into());
            }
            return Ok(existing);
        }
        if !self.shared_by_media.is_empty()
            && self.shared_by_media.contains_key(&request.media_key())
        {
            return Err("Episode already belongs to immutable shared-file ownership".into());
        }
        if self.jobs.len() >= MAX_JOBS {
            return Err("storage capacity reached: 10,000 requests".to_owned());
        }
        let at = now();
        let job = Job {
            id: random_id()?,
            key,
            request,
            state: "queued".to_owned(),
            progress: 0.0,
            files: Vec::new(),
            imports: Vec::new(),
            attempts: 0,
            last_error: None,
            created_at: at,
            updated_at: at,
            next_attempt_at: 0,
            acquisition_url: None,
            download_id: None,
            lease_id: None,
            lease_until: 0,
            release: None,
            upgrade_parent: None,
            monitored: true,
            monitor_checked_at: 0,
            pack_file,
            pack_origin,
            shared_file: None,
            shared_upgrade: None,
            requester,
            irc_origin: None,
        };
        self.commit(job.clone(), "request recorded")?;
        Ok(job)
    }

    pub fn update(&mut self, mut job: Job) -> Result<()> {
        let current = self
            .jobs
            .get(&job.id)
            .ok_or_else(|| "unknown job".to_owned())?;
        if current.key != job.key
            || current.request != job.request
            || current.created_at != job.created_at
            || current.upgrade_parent != job.upgrade_parent
            || current.pack_file != job.pack_file
            || current.pack_origin != job.pack_origin
            || current.shared_file != job.shared_file
            || current.shared_upgrade != job.shared_upgrade
            || current.requester != job.requester
            || current.irc_origin != job.irc_origin
        {
            return Err("job identity is immutable".to_owned());
        }
        let ready_upgrade =
            current.state != "ready" && job.state == "ready" && current.upgrade_parent.is_some();
        if ready_upgrade {
            let parent = self
                .jobs
                .get(current.upgrade_parent.as_deref().unwrap_or_default())
                .ok_or_else(|| "missing upgrade parent".to_owned())?;
            // A user can disable monitoring while a claimed acquisition is in
            // flight. Promotion inherits that current choice atomically.
            job.monitored = parent.monitored;
        }
        if (!ready_upgrade && current.monitored != job.monitored)
            || current.monitor_checked_at != job.monitor_checked_at
            || (current.release.is_some() && current.release != job.release)
            || (current.state == "ready" && current.release != job.release)
        {
            return Err("library metadata cannot be changed by a worker".to_owned());
        }
        if current.lease_id != job.lease_id {
            return Err("stale processing lease".to_owned());
        }
        if matches!(current.state.as_str(), "ready" | "cancelled") && current.state != job.state {
            return Err("a completed job cannot be modified by a worker".to_owned());
        }
        if current.lease_id.is_some() && current.lease_until <= now() {
            return Err("expired processing lease".to_owned());
        }
        if current.lease_id.is_some() {
            job.lease_until = job.lease_until.max(current.lease_until);
        }
        if !job.progress.is_finite()
            || !(0.0..=1.0).contains(&job.progress)
            || job.state.is_empty()
            || job.state.len() > 64
        {
            return Err("invalid state or progress".to_owned());
        }
        job.updated_at = now();
        if matches!(job.state.as_str(), "ready" | "failed" | "cancelled") {
            job.lease_id = None;
            job.lease_until = 0;
        }
        let message = job
            .last_error
            .clone()
            .unwrap_or_else(|| "state updated".to_owned());
        if job.shared_upgrade.is_some() && job.state == "ready" && current.state != "ready" {
            return self.finish_group_owner(job);
        }
        self.commit(job, &message)
    }

    pub fn claim(&mut self, at: u64, ttl: u64) -> Result<Option<Job>> {
        self.claim_filtered(at, ttl, |_| true)
    }

    pub(crate) fn claim_filtered(
        &mut self,
        at: u64,
        ttl: u64,
        eligible: impl Fn(&Job) -> bool,
    ) -> Result<Option<Job>> {
        if ttl == 0 {
            return Err("lease duration must be positive".to_owned());
        }
        let deadline = at
            .checked_add(ttl)
            .ok_or_else(|| "lease duration too large".to_owned())?;
        let upgrade_parents: BTreeSet<String> = self
            .library_jobs()
            .into_iter()
            .filter(|job| job.monitored && job.release.is_some())
            .map(|job| job.id)
            .collect();
        let active_shared: BTreeSet<String> = self
            .jobs
            .values()
            .filter(|job| job.lease_id.is_some() && job.lease_until > at)
            .filter_map(|job| job.shared_file.as_ref().map(crate::pack::SharedFile::id))
            .collect();
        let candidate = self
            .jobs
            .values()
            .filter(|job| {
                eligible(job)
                    && !matches!(job.state.as_str(), "ready" | "cancelled" | "staged")
                    && (job.state != "failed" || job.next_attempt_at > 0)
                    && job.next_attempt_at <= at
                    && (job.lease_id.is_none() || job.lease_until <= at)
                    && job
                        .shared_file
                        .as_ref()
                        .is_none_or(|file| !active_shared.contains(&file.id()))
                    && job
                        .upgrade_parent
                        .as_ref()
                        .is_none_or(|parent| upgrade_parents.contains(parent))
                    && job.shared_upgrade.as_ref().is_none_or(|lineage| {
                        lineage
                            .parent_jobs
                            .iter()
                            .all(|parent| upgrade_parents.contains(parent))
                    })
            })
            .min_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)))
            .cloned();
        if let Some(mut job) = candidate {
            if matches!(job.state.as_str(), "queued" | "failed") {
                job.state = "processing".to_owned();
            }
            job.lease_id = Some(random_id()?);
            job.lease_until = deadline;
            job.updated_at = at;
            self.commit(job.clone(), "processing lease acquired")?;
            Ok(Some(job))
        } else {
            Ok(None)
        }
    }

    pub fn renew(&mut self, id: &str, lease_id: &str, at: u64, ttl: u64) -> Result<Job> {
        let mut job = self.get(id).ok_or_else(|| "unknown job".to_owned())?;
        if job.lease_id.as_deref() != Some(lease_id) || job.lease_until <= at || ttl == 0 {
            return Err("expired or invalid processing lease".to_owned());
        }
        job.lease_until = job.lease_until.max(
            at.checked_add(ttl)
                .ok_or_else(|| "lease duration too large".to_owned())?,
        );
        job.updated_at = at;
        self.commit(job.clone(), "processing lease renewed")?;
        Ok(job)
    }

    pub fn renew_lease(&mut self, id: &str, lease_id: &str, at: u64, ttl: u64) -> Result<Job> {
        self.renew(id, lease_id, at, ttl)
    }

    pub fn release_lease(&mut self, id: &str, lease_id: &str) -> Result<Job> {
        let mut job = self.get(id).ok_or_else(|| "unknown job".to_owned())?;
        if job.lease_id.as_deref() != Some(lease_id) {
            return Err("stale processing lease".to_owned());
        }
        job.lease_id = None;
        job.lease_until = 0;
        job.updated_at = now();
        self.commit(job.clone(), "processing lease released")?;
        Ok(job)
    }

    pub fn cancel(&mut self, id: &str) -> Result<Job> {
        let mut job = self.get(id).ok_or_else(|| "unknown job".to_owned())?;
        if job.shared_upgrade.is_some() {
            return self.cancel_group(id);
        }
        if job.state == "ready" {
            return Err("a completed job cannot be cancelled".to_owned());
        }
        job.state = "cancelled".to_owned();
        job.lease_id = None;
        job.lease_until = 0;
        job.updated_at = now();
        self.commit(job.clone(), "request cancelled")?;
        Ok(job)
    }

    pub fn retry(&mut self, id: &str) -> Result<Job> {
        let mut job = self.get(id).ok_or_else(|| "unknown job".to_owned())?;
        if job.shared_upgrade.is_some() {
            return self.retry_group(id);
        }
        if !matches!(job.state.as_str(), "failed" | "cancelled") {
            return Err("only a failed or cancelled job can be retried".to_owned());
        }
        if job.upgrade_parent.is_some() {
            if !self.upgrade_eligible(&job) {
                return Err("the upgrade parent is obsolete or no longer monitored".to_owned());
            }
            let media_key = job.request.media_key();
            if self.jobs.values().any(|other| {
                other.id != job.id
                    && Self::pending_upgrade(other)
                    && other.request.media_key() == media_key
            }) {
                return Err("another upgrade for this media is already pending".to_owned());
            }
        }
        job.state = "queued".to_owned();
        job.lease_id = None;
        job.lease_until = 0;
        job.last_error = None;
        job.next_attempt_at = 0;
        job.updated_at = now();
        if job.imports.is_empty()
            && job.request.source_path.is_none()
            && job.request.source_url.is_none()
            && job.irc_origin.is_none()
        {
            // A fresh automatic search must not inherit the previous release's
            // URL or baseline, even if the process stops immediately afterward.
            job.release = None;
            job.acquisition_url = None;
            job.download_id = None;
            job.files.clear();
            job.progress = 0.0;
        }
        self.commit(job.clone(), "request retried")?;
        Ok(job)
    }

    /// Checks whether a completed acquisition can become the current import.
    /// Callers can turn a rejected promotion into a durable processing failure
    /// before relinquishing their lease. Validation repeats this check at commit.
    pub fn check_ready_promotion(&self, job: &Job) -> Result<()> {
        if job.state != "ready"
            || self
                .jobs
                .get(&job.id)
                .is_some_and(|current| current.state == "ready")
        {
            return Ok(());
        }
        if job.shared_upgrade.is_some() {
            return self.group_eligible(job);
        }
        let media_key = job.request.media_key();
        if let Some(parent_id) = &job.upgrade_parent {
            let parent = self
                .jobs
                .get(parent_id)
                .ok_or_else(|| "missing upgrade parent".to_owned())?;
            if !self.is_library_tip(parent)
                || self.jobs.values().any(|other| {
                    other.id != job.id
                        && Self::pending_upgrade(other)
                        && other.request.media_key() == media_key
                })
            {
                return Err(
                    "an upgrade cannot replace an obsolete parent or a competing pending upgrade"
                        .to_owned(),
                );
            }
        } else if !job.imports.is_empty()
            && self
                .jobs
                .values()
                .any(|other| Self::pending_upgrade(other) && other.request.media_key() == media_key)
        {
            return Err(
                "cancel the pending upgrade before importing an unrelated root for the same media"
                    .to_owned(),
            );
        }
        Ok(())
    }

    fn validate_lineage(&self, job: &Job) -> Result<()> {
        self.check_irc_ownership(job)?;
        if let Some(origin) = &job.irc_origin {
            origin.validate_job(job)?;
        }
        if let Some(release) = &job.release {
            release.validate()?;
        }
        let Some(parent_id) = &job.upgrade_parent else {
            return Ok(());
        };
        if job.shared_upgrade.is_some() {
            return self.validate_group_lineage(job);
        }
        let parent = self
            .jobs
            .get(parent_id)
            .ok_or_else(|| "missing upgrade parent".to_owned())?;
        if parent.requester != job.requester {
            return Err("Upgrade requester provenance is immutable".into());
        }
        if parent.shared_file.is_some() {
            return Err("Individual upgrades cannot replace shared-file owners".into());
        }
        if parent_id == &job.id
            || parent.request.media_key() != job.request.media_key()
            || parent.state != "ready"
            || parent.imports.is_empty()
            || parent.release.is_none()
            || job.release.is_none()
            || job.request.source_path.is_some()
            || job.request.source_url.is_none()
            || (job.state == "ready" && job.imports.is_empty())
        {
            return Err("invalid upgrade lineage or release baseline".to_owned());
        }
        let url = job.request.source_url.as_deref();
        if url == parent.request.source_url.as_deref() || url == parent.acquisition_url.as_deref() {
            return Err("upgrade acquisition repeats its parent's source URL".to_owned());
        }
        if job
            .acquisition_url
            .as_deref()
            .is_some_and(|acquisition| Some(acquisition) != url)
        {
            return Err("upgrade acquisition differs from its recorded source URL".to_owned());
        }
        Ok(())
    }

    fn validate_transaction(&self, job: &Job, legacy_record: bool) -> Result<()> {
        self.validate_lineage(job)?;
        if let Some(file) = &job.shared_file {
            file.validate_job(job)?;
            if self
                .shared_by_physical
                .get(&(file.torrent_id.clone(), file.file_path.clone()))
                .is_some_and(|known| known != file)
                || self
                    .shared_by_media
                    .get(&job.request.media_key())
                    .is_some_and(|owner| owner != &job.id && job.shared_upgrade.is_none())
            {
                return Err("Shared-file physical identity or logical ownership conflicts".into());
            }
        } else if !self.shared_by_media.is_empty()
            && self.shared_by_media.contains_key(&job.request.media_key())
        {
            return Err("An unrelated request cannot replace shared-file ownership".into());
        }
        if let Some(current) = self.jobs.get(&job.id) {
            if current.irc_origin != job.irc_origin
                && !(current.irc_origin.is_none()
                    && job.irc_origin.is_some()
                    && crate::irc::routing::eligible(current)
                    && job.state == "queued"
                    && job.lease_id.is_none()
                    && job.files.is_empty()
                    && job.imports.is_empty()
                    && job.download_id.is_none())
            {
                return Err("IRC acquisition provenance is immutable".into());
            }
            if current.key != job.key
                || current.request != job.request
                || current.created_at != job.created_at
                || current.upgrade_parent != job.upgrade_parent
                || current.shared_file != job.shared_file
                || current.shared_upgrade != job.shared_upgrade
                || current.requester != job.requester
                || (current.shared_file.is_some() && current.pack_file != job.pack_file)
            {
                return Err("job identity or upgrade parent changed".to_owned());
            }
            if current.shared_file.is_some()
                && !current.imports.is_empty()
                && current.imports != job.imports
            {
                return Err("Shared import ownership is immutable".into());
            }
            let fresh_search = matches!(current.state.as_str(), "failed" | "cancelled")
                && job.state == "queued"
                && job.imports.is_empty()
                && job.request.source_path.is_none()
                && job.request.source_url.is_none()
                && job.release.is_none()
                && job.acquisition_url.is_none()
                && job.download_id.is_none()
                && job.files.is_empty()
                && job.progress == 0.0;
            if current.release.is_some() && current.release != job.release && !fresh_search {
                return Err("recorded release is immutable".to_owned());
            }
            if job.monitor_checked_at < current.monitor_checked_at {
                return Err("monitor check timestamp moved backwards".to_owned());
            }
            let ready_upgrade = current.state != "ready"
                && job.state == "ready"
                && current.upgrade_parent.is_some();
            if ready_upgrade {
                let parent = self
                    .jobs
                    .get(current.upgrade_parent.as_deref().unwrap_or_default())
                    .ok_or_else(|| "missing upgrade parent".to_owned())?;
                if job.monitored != parent.monitored {
                    return Err(
                        "promoted upgrade monitoring differs from its current parent".to_owned(),
                    );
                }
            } else if current.state != "ready" && current.monitored != job.monitored {
                return Err("monitoring can only change on a ready library import".to_owned());
            }
            let legacy_cancellation = legacy_record
                && job.state == "cancelled"
                && current.release.is_none()
                && current.upgrade_parent.is_none()
                && job.release.is_none()
                && job.upgrade_parent.is_none()
                && current.monitor_checked_at == 0
                && current.monitored
                && !self
                    .jobs
                    .values()
                    .any(|other| other.upgrade_parent.as_deref() == Some(current.id.as_str()));
            if current.state == "ready" && job.state != "ready" && !legacy_cancellation {
                return Err("a ready import cannot change state".to_owned());
            }
            if current.state == "ready"
                && (current.files != job.files
                    || current.imports != job.imports
                    || current.acquisition_url != job.acquisition_url
                    || current.download_id != job.download_id
                    || current.progress != job.progress)
            {
                return Err("ready import provenance is immutable".to_owned());
            }
        } else {
            if job.irc_origin.is_some() {
                return Err("IRC provenance requires a previously admitted job".into());
            }
            if self.by_key.contains_key(&job.key) {
                return Err("duplicate request identity".to_owned());
            }
            if job.upgrade_parent.is_some()
                && job.shared_upgrade.is_none()
                && !self.upgrade_eligible(job)
            {
                return Err("an upgrade requires a monitored current library parent".to_owned());
            }
        }
        if Self::pending_upgrade(job) {
            let media_key = job.request.media_key();
            if self.jobs.values().any(|other| {
                other.id != job.id
                    && Self::pending_upgrade(other)
                    && other.request.media_key() == media_key
            }) {
                return Err("multiple pending upgrades for the same media".to_owned());
            }
            let was_pending = self.jobs.get(&job.id).is_some_and(Self::pending_upgrade);
            if !was_pending && job.shared_upgrade.is_none() && !self.upgrade_eligible(job) {
                return Err("the upgrade parent is obsolete or no longer monitored".to_owned());
            }
        }
        self.check_ready_promotion(job)
    }

    fn validate_snapshot_lineage(&self) -> Result<()> {
        self.validate_shared_groups()?;
        let mut pending = BTreeSet::new();
        let mut ready_parents = BTreeSet::new();
        for job in self.jobs.values() {
            self.validate_lineage(job)?;
            if Self::pending_upgrade(job) && !pending.insert(job.request.media_key()) {
                return Err("multiple pending upgrades in snapshot".to_owned());
            }
            if job.state == "ready"
                && let Some(parent) = &job.upgrade_parent
                && !ready_parents.insert(parent)
            {
                return Err("multiple ready children of the same upgrade parent".to_owned());
            }
        }
        // Every edge is visited at most once, even for long upgrade histories.
        let mut complete: BTreeSet<&str> = BTreeSet::new();
        for job in self.jobs.values() {
            let mut path: BTreeSet<&str> = BTreeSet::new();
            let mut cursor = job.id.as_str();
            while !complete.contains(cursor) {
                if !path.insert(cursor) {
                    return Err("cycle in upgrade lineage".to_owned());
                }
                let current = self
                    .jobs
                    .get(cursor)
                    .ok_or_else(|| "missing upgrade lineage reference".to_owned())?;
                match current.upgrade_parent.as_deref() {
                    Some(parent) => cursor = parent,
                    None => break,
                }
            }
            complete.extend(path);
        }
        let tips: BTreeSet<String> = self.library_jobs().into_iter().map(|job| job.id).collect();
        for job in self.jobs.values().filter(|job| Self::pending_upgrade(job)) {
            if job
                .upgrade_parent
                .as_ref()
                .is_none_or(|parent| !tips.contains(parent))
            {
                return Err("pending upgrade has an obsolete parent in snapshot".to_owned());
            }
        }
        Ok(())
    }

    fn commit(&mut self, job: Job, message: &str) -> Result<()> {
        self.commit_jobs(vec![job], message)
    }

    fn commit_jobs(&mut self, jobs: Vec<Job>, message: &str) -> Result<()> {
        if jobs.len() > 1 {
            self.validate_shared_batch(&jobs)?;
        } else if let Some(job) = jobs.first() {
            self.validate_group_single(job)?;
        }
        self.commit_operation(jobs, message, None)
    }

    fn commit_operation(
        &mut self,
        jobs: Vec<Job>,
        message: &str,
        action: Option<GroupAction>,
    ) -> Result<()> {
        if self.read_only {
            return Err("storage is read-only".to_owned());
        }
        if self.poisoned {
            return Err("storage unavailable after a write error; reopen storage".to_owned());
        }
        let job = jobs.first().ok_or("Empty journal transaction")?;
        for job in &jobs {
            self.validate_transaction(job, false)?;
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| "journal full".to_owned())?;
        let event = Event {
            id: sequence,
            job_id: job.id.clone(),
            state: job.state.clone(),
            message: bounded_message(message),
            at: job.updated_at,
        };
        let mut value = object([("job", job.to_json()), ("event", event.to_json())]);
        if let Some(action) = action {
            value.insert("group_action", action.name());
        }
        if jobs.len() > 1 {
            value.insert(
                if action.is_some() {
                    "group_jobs"
                } else {
                    "shared_owners"
                },
                Value::Array(jobs.iter().skip(1).map(Job::to_json).collect()),
            );
        }
        let payload = json::stringify(&value).into_bytes();
        if payload.len() > MAX_RECORD {
            return Err("transaction too large".to_owned());
        }
        let mut frame = Vec::with_capacity(116 + payload.len());
        frame.extend_from_slice(if jobs.iter().any(|j| j.irc_origin.is_some()) {
            IRC_JOURNAL_MAGIC
        } else if jobs.iter().any(|j| j.requester.is_some()) {
            REQUESTER_JOURNAL_MAGIC
        } else if action.is_some() || group_format(job) {
            GROUP_JOURNAL_MAGIC
        } else if job.shared_file.is_some() {
            SHARED_JOURNAL_MAGIC
        } else {
            JOURNAL_MAGIC
        });
        frame.extend_from_slice(&sequence.to_le_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&self.chain);
        frame.extend_from_slice(&sha256(&frame));
        frame.extend_from_slice(&payload);
        let digest = sha256(&frame);
        frame.extend_from_slice(&digest);
        let journal_bytes = self
            .journal_bytes
            .checked_add(frame.len() as u64)
            .filter(|bytes| *bytes <= MAX_JOURNAL_BYTES)
            .ok_or_else(|| {
                "journal full after a maintenance error; repair storage before continuing"
                    .to_owned()
            })?;
        let write = self
            .journal
            .seek(SeekFrom::End(0))
            .and_then(|_| self.journal.write_all(&frame))
            .and_then(|_| self.journal.sync_data());
        if let Err(error) = write {
            self.poisoned = true;
            return Err(format!(
                "persistence failed; cannot acknowledge transaction: {error}"
            ));
        }
        self.sequence = sequence;
        self.chain = digest;
        self.journal_bytes = journal_bytes;
        for job in jobs {
            self.index_shared(&job);
            self.by_key.insert(job.key.clone(), job.id.clone());
            self.jobs.insert(job.id.clone(), job);
        }
        self.push_event(event);
        if self.journal_bytes >= self.next_compaction_at {
            // The transaction is already acknowledged: failed maintenance must
            // never turn its response into an ambiguous failure.
            if let Err(error) = self.compact() {
                self.maintenance_error = Some(error);
                self.next_compaction_at = self.journal_bytes.saturating_add(COMPACTION_BYTES);
            }
        }
        Ok(())
    }

    fn push_event(&mut self, mut event: Event) {
        event.message = bounded_message(&event.message);
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// Compacts the journal without a window in which a transaction could be lost.
    pub fn compact(&mut self) -> Result<()> {
        if self.read_only {
            return Err("storage is read-only".to_owned());
        }
        if self.poisoned {
            return Err("storage unavailable after a write error".to_owned());
        }
        let value = object([
            ("sequence", number(self.sequence)),
            ("chain", Value::String(hex(&self.chain))),
            (
                "jobs",
                Value::Array(self.jobs.values().map(Job::to_json).collect()),
            ),
            (
                "events",
                Value::Array(self.events.iter().map(Event::to_json).collect()),
            ),
        ]);
        let payload = json::stringify(&value).into_bytes();
        if payload.len() > MAX_SNAPSHOT {
            return Err("snapshot too large".to_owned());
        }
        let mut bytes = Vec::with_capacity(payload.len() + 48);
        bytes.extend_from_slice(if self.jobs.values().any(|j| j.irc_origin.is_some()) {
            IRC_SNAPSHOT_MAGIC
        } else if self.jobs.values().any(|j| j.requester.is_some()) {
            REQUESTER_SNAPSHOT_MAGIC
        } else if self.jobs.values().any(group_format) {
            GROUP_SNAPSHOT_MAGIC
        } else if self.jobs.values().any(|job| job.shared_file.is_some()) {
            SHARED_SNAPSHOT_MAGIC
        } else {
            SNAPSHOT_MAGIC
        });
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&sha256(&bytes));
        let temporary = self
            .directory
            .join(format!("snapshot.{}.tmp", random_id()?));
        let mut owns_temporary = false;
        let mut journal_altered = false;
        let result: Result<()> = (|| {
            let mut file = private_options()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|error| error.to_string())?;
            owns_temporary = true;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|error| error.to_string())?;
            fs::rename(&temporary, self.directory.join("snapshot.bin"))
                .map_err(|error| error.to_string())?;
            sync_directory(&self.directory)?;
            journal_altered = true;
            self.journal
                .set_len(0)
                .and_then(|_| self.journal.sync_all())
                .map_err(|error| error.to_string())?;
            self.journal
                .seek(SeekFrom::Start(0))
                .map_err(|error| error.to_string())?;
            Ok(())
        })();
        if let Err(error) = &result {
            self.poisoned = journal_altered;
            self.maintenance_error = Some(error.clone());
            if owns_temporary {
                let _ = fs::remove_file(&temporary);
            }
        } else {
            self.journal_bytes = 0;
            self.next_compaction_at = COMPACTION_BYTES;
            self.maintenance_error = None;
        }
        result
    }

    fn load_snapshot(&mut self) -> Result<()> {
        let path = self.directory.join("snapshot.bin");
        reject_symlinks(&path)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err("snapshot must be a regular file".to_owned());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        }
        let mut file = match private_options().read(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("cannot access snapshot: {error}")),
        };
        let length = file.metadata().map_err(|error| error.to_string())?.len();
        if !(48..=MAX_SNAPSHOT as u64 + 48).contains(&length) {
            return Err("invalid snapshot size".to_owned());
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if &bytes[..8] != SNAPSHOT_MAGIC
            && &bytes[..8] != SHARED_SNAPSHOT_MAGIC
            && &bytes[..8] != GROUP_SNAPSHOT_MAGIC
            && &bytes[..8] != REQUESTER_SNAPSHOT_MAGIC
            && &bytes[..8] != IRC_SNAPSHOT_MAGIC
        {
            return Err("invalid snapshot format".to_owned());
        }
        let encoded_length =
            u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| "invalid length")?);
        if encoded_length > MAX_SNAPSHOT as u64 || encoded_length.checked_add(48) != Some(length) {
            return Err("corrupt snapshot size".to_owned());
        }
        let payload_length = usize::try_from(encoded_length).map_err(|_| "snapshot too large")?;
        if sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..] {
            return Err("corrupt snapshot".to_owned());
        }
        let value = json::parse(
            std::str::from_utf8(&bytes[16..16 + payload_length])
                .map_err(|_| "snapshot is not UTF-8")?,
        )?;
        let map = fields(&value)?;
        self.sequence = integer(map, "sequence")?;
        self.chain = unhex_digest(&string(map, "chain")?)?;
        let job_values = match field(map, "jobs")? {
            Value::Array(values) => values,
            _ => return Err("invalid snapshot jobs".to_owned()),
        };
        if job_values.len() > MAX_JOBS {
            return Err("snapshot exceeds request capacity".to_owned());
        }
        let mut keys = BTreeMap::new();
        for value in job_values {
            let job = Job::from_json(value)?;
            if job.irc_origin.is_some() && &bytes[..8] != IRC_SNAPSHOT_MAGIC {
                return Err("IRC provenance requires snapshot format 5".into());
            }
            if job.requester.is_some()
                && &bytes[..8] != REQUESTER_SNAPSHOT_MAGIC
                && &bytes[..8] != IRC_SNAPSHOT_MAGIC
            {
                return Err("Requester provenance requires snapshot format 4".into());
            }
            if group_format(&job)
                && (&bytes[..8] != GROUP_SNAPSHOT_MAGIC
                    && &bytes[..8] != REQUESTER_SNAPSHOT_MAGIC
                    && &bytes[..8] != IRC_SNAPSHOT_MAGIC)
            {
                return Err("Shared-group upgrades require snapshot format 3".into());
            }
            if job.shared_file.is_some() && &bytes[..8] == SNAPSHOT_MAGIC {
                return Err("Shared ownership requires snapshot format 2".into());
            }
            self.index_shared(&job);
            self.by_key.insert(job.key.clone(), job.id.clone());
            if keys.insert(job.key.clone(), job.id.clone()).is_some()
                || self.jobs.insert(job.id.clone(), job).is_some()
            {
                return Err("duplicate identities in snapshot".to_owned());
            }
        }
        self.validate_snapshot_lineage()?;
        let event_values = match field(map, "events")? {
            Value::Array(values) => values,
            _ => return Err("invalid snapshot events".to_owned()),
        };
        let retained_count = event_values.len() as u64;
        let minimum_retained = self.sequence.min(MAX_EVENTS as u64);
        if retained_count < minimum_retained || retained_count > self.sequence {
            return Err("missing snapshot events".to_owned());
        }
        let first_id = self.sequence - retained_count;
        for (index, value) in event_values.iter().enumerate() {
            let event = Event::from_json(value)?;
            if event.id != first_id + index as u64 + 1
                || !self.jobs.contains_key(&event.job_id)
                || event.id > self.sequence
            {
                return Err("invalid event sequence".to_owned());
            }
            self.push_event(event);
        }
        Ok(())
    }

    fn replay(&mut self) -> Result<()> {
        self.journal
            .seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        let length = self
            .journal
            .metadata()
            .map_err(|error| error.to_string())?
            .len();
        if length > MAX_JOURNAL_BYTES {
            return Err("journal exceeds the maximum capacity of 64 MiB".to_owned());
        }
        let snapshot_sequence = self.sequence;
        let snapshot_chain = self.chain;
        let mut offset = 0_u64;
        let mut previous: Option<(u64, [u8; 32])> = None;
        while offset < length {
            let remaining = length - offset;
            if remaining < 84 {
                self.truncate_tail(offset)?;
                break;
            }
            let mut header = [0_u8; 84];
            self.journal
                .read_exact(&mut header)
                .map_err(|error| error.to_string())?;
            if &header[..8] != JOURNAL_MAGIC
                && &header[..8] != SHARED_JOURNAL_MAGIC
                && &header[..8] != GROUP_JOURNAL_MAGIC
                && &header[..8] != REQUESTER_JOURNAL_MAGIC
                && &header[..8] != IRC_JOURNAL_MAGIC
            {
                return Err(format!("corrupt journal at byte {offset}"));
            }
            if sha256(&header[..52]).as_slice() != &header[52..84] {
                return Err(format!("corrupt journal header at byte {offset}"));
            }
            let sequence =
                u64::from_le_bytes(header[8..16].try_into().map_err(|_| "invalid sequence")?);
            let payload_length =
                u32::from_le_bytes(header[16..20].try_into().map_err(|_| "invalid length")?)
                    as usize;
            let chain: [u8; 32] = header[20..52].try_into().map_err(|_| "invalid chain")?;
            if payload_length > MAX_RECORD {
                return Err("journal transaction too large".to_owned());
            }
            let frame_length = 116 + payload_length as u64;
            if remaining < frame_length {
                self.truncate_tail(offset)?;
                break;
            }
            let mut frame = Vec::with_capacity(frame_length as usize);
            frame.extend_from_slice(&header);
            frame.resize(84 + payload_length, 0);
            self.journal
                .read_exact(&mut frame[84..])
                .map_err(|error| error.to_string())?;
            let mut digest = [0_u8; 32];
            self.journal
                .read_exact(&mut digest)
                .map_err(|error| error.to_string())?;
            if sha256(&frame) != digest {
                return Err(format!("corrupt complete transaction at byte {offset}"));
            }
            if let Some((previous_sequence, previous_chain)) = previous {
                if previous_sequence.checked_add(1) != Some(sequence) || chain != previous_chain {
                    return Err("invalid journal chain".to_owned());
                }
            } else if snapshot_sequence.checked_add(1) == Some(sequence) {
                if chain != snapshot_chain {
                    return Err("journal chain differs from snapshot".to_owned());
                }
            } else if sequence == 0 || sequence > snapshot_sequence {
                return Err("invalid initial journal sequence".to_owned());
            }
            if sequence == snapshot_sequence && digest != snapshot_chain {
                return Err("journal differs from acknowledged snapshot".to_owned());
            }
            if sequence > snapshot_sequence {
                let value = json::parse(
                    std::str::from_utf8(&frame[84..]).map_err(|_| "journal is not UTF-8")?,
                )?;
                let map = fields(&value)?;
                let job_value = field(map, "job")?;
                let job_map = fields(job_value)?;
                let legacy_record = [
                    "release",
                    "upgrade_parent",
                    "monitored",
                    "monitor_checked_at",
                ]
                .iter()
                .all(|key| !job_map.contains_key(*key));
                let job = Job::from_json(job_value)?;
                if job.irc_origin.is_some() && &header[..8] != IRC_JOURNAL_MAGIC {
                    return Err("IRC provenance requires journal format 5".into());
                }
                if job.requester.is_some()
                    && &header[..8] != REQUESTER_JOURNAL_MAGIC
                    && &header[..8] != IRC_JOURNAL_MAGIC
                {
                    return Err("Requester provenance requires journal format 4".into());
                }
                if group_format(&job)
                    && (&header[..8] != GROUP_JOURNAL_MAGIC
                        && &header[..8] != REQUESTER_JOURNAL_MAGIC)
                    && &header[..8] != IRC_JOURNAL_MAGIC
                {
                    return Err("Shared-group upgrades require journal format 3".into());
                }
                if job.shared_file.is_some() && &header[..8] == JOURNAL_MAGIC {
                    return Err("Shared ownership requires journal format 2".into());
                }
                let event = Event::from_json(field(map, "event")?)?;
                if event.id != sequence
                    || event.job_id != job.id
                    || event.state != job.state
                    || event.at != job.updated_at
                {
                    return Err("inconsistent journal transaction".to_owned());
                }
                let mut jobs = vec![job];
                if map.contains_key("group_jobs") && !map.contains_key("group_action") {
                    return Err("Group owners require a journal action".into());
                }
                if let Some(action) = map.get("group_action") {
                    if (&header[..8] != GROUP_JOURNAL_MAGIC
                        && &header[..8] != REQUESTER_JOURNAL_MAGIC
                        && &header[..8] != IRC_JOURNAL_MAGIC)
                        || map.contains_key("shared_owners")
                    {
                        return Err("Group action requires journal format 3".into());
                    }
                    let action =
                        GroupAction::parse(action.as_str().ok_or("Invalid group action")?)?;
                    let owners = map
                        .get("group_jobs")
                        .and_then(Value::as_array)
                        .ok_or("Missing group transaction owners")?;
                    if owners.is_empty() || owners.len() >= crate::pack::MAX_PACK_EPISODES {
                        return Err("Invalid shared-group journal size".into());
                    }
                    jobs.extend(
                        owners
                            .iter()
                            .map(Job::from_json)
                            .collect::<Result<Vec<_>>>()?,
                    );
                    self.validate_group_operation(&jobs, action)?;
                } else if let Some(owners) = map.get("shared_owners") {
                    if &header[..8] != SHARED_JOURNAL_MAGIC
                        && &header[..8] != REQUESTER_JOURNAL_MAGIC
                        && &header[..8] != IRC_JOURNAL_MAGIC
                    {
                        return Err("Shared ownership requires journal format 2".into());
                    }
                    let owners = owners
                        .as_array()
                        .ok_or("Invalid shared ownership journal array")?;
                    if owners.is_empty() || owners.len() >= crate::pack::MAX_PACK_EPISODES {
                        return Err("Invalid shared ownership journal size".into());
                    }
                    jobs.extend(
                        owners
                            .iter()
                            .map(Job::from_json)
                            .collect::<Result<Vec<_>>>()?,
                    );
                    self.validate_shared_batch(&jobs)?;
                } else {
                    if map.contains_key("group_jobs") {
                        return Err("Group owners require a journal action".into());
                    }
                    let job = &jobs[0];
                    if job.shared_file.is_some() && !self.jobs.contains_key(&job.id) {
                        return Err("Shared owners must be created in one transaction".into());
                    }
                    self.validate_transaction(job, legacy_record)
                        .map_err(|error| format!("invalid journal job: {error}"))?;
                    self.validate_group_single(job)?;
                }
                if jobs.iter().any(|j| j.requester.is_some())
                    && &header[..8] != REQUESTER_JOURNAL_MAGIC
                    && &header[..8] != IRC_JOURNAL_MAGIC
                {
                    return Err("Requester provenance requires journal format 4".into());
                }
                for job in jobs {
                    self.index_shared(&job);
                    self.by_key.insert(job.key.clone(), job.id.clone());
                    self.jobs.insert(job.id.clone(), job);
                }
                if self.jobs.len() > MAX_JOBS {
                    return Err("journal exceeds request capacity".to_owned());
                }
                self.push_event(event);
                self.sequence = sequence;
                self.chain = digest;
            }
            previous = Some((sequence, digest));
            offset += frame_length;
        }
        if let Some((sequence, _)) = previous
            && sequence < snapshot_sequence
        {
            return Err("incomplete stale journal preceding snapshot".to_owned());
        }
        self.journal
            .seek(SeekFrom::End(0))
            .map_err(|error| error.to_string())?;
        self.journal_bytes = offset;
        self.validate_shared_groups()?;
        Ok(())
    }

    fn truncate_tail(&mut self, length: u64) -> Result<()> {
        if self.preparing {
            self.recovery_tail = Some(length);
            return Ok(());
        }
        if self.read_only {
            return Err("storage recovery required: incomplete journal tail; open storage for writing to repair it".to_owned());
        }
        self.journal
            .set_len(length)
            .and_then(|_| self.journal.sync_all())
            .map_err(|error| format!("cannot recover journal: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mynou-store-{}-{}-{}",
                std::process::id(),
                now(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("temporary directory");
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn request(title: &str) -> Request {
        Request {
            kind: "movie".to_owned(),
            title: title.to_owned(),
            year: 2026,
            season: 0,
            episode: 0,
            source_path: None,
            source_url: None,
            source_numbering: None,
            tmdb_id: None,
        }
    }

    #[test]
    fn durable_dedup_and_exclusive_owner() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let job = store.submit(request("A  movie")).unwrap();
        assert_eq!(store.submit(request("a movie")).unwrap().id, job.id);
        assert!(Store::open(&directory.0).is_err());
        drop(store);
        let reopened = Store::open(&directory.0).unwrap();
        assert_eq!(reopened.get(&job.id).unwrap(), job);
        assert_eq!(reopened.events(&job.id).len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(directory.0.join("journal.bin"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn interrupted_tail_is_removed_but_complete_corruption_is_rejected() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let job = store.submit(request("Persisted")).unwrap();
        drop(store);
        let journal = directory.0.join("journal.bin");
        let original = fs::read(&journal).unwrap();
        let mut append = OpenOptions::new().append(true).open(&journal).unwrap();
        append.write_all(&original[..original.len() - 7]).unwrap();
        append.sync_all().unwrap();
        drop(append);
        let reopened = Store::open(&directory.0).unwrap();
        assert_eq!(reopened.get(&job.id).unwrap(), job);
        assert_eq!(fs::metadata(&journal).unwrap().len(), original.len() as u64);
        drop(reopened);
        let mut corrupted = original.clone();
        corrupted[90] ^= 1;
        fs::write(&journal, corrupted).unwrap();
        let error = Store::open(&directory.0).err().unwrap();
        assert!(error.contains("corrupt complete"), "{error}");
    }

    #[test]
    fn leases_survive_restart_expire_and_reject_stale_completion() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.submit(request("With lease")).unwrap();
        let at = now();
        let stale = store.claim(at, 60).unwrap().unwrap();
        assert!(store.claim(at + 1, 60).unwrap().is_none());
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        assert!(store.claim(at + 59, 60).unwrap().is_none());
        let active = store.claim(at + 61, 60).unwrap().unwrap();
        assert_ne!(stale.lease_id, active.lease_id);
        let mut completion = stale;
        completion.state = "ready".to_owned();
        assert!(store.update(completion).unwrap_err().contains("stale"));
        let cancelled = store.cancel(&active.id).unwrap();
        assert_eq!(cancelled.state, "cancelled");
        assert!(
            store.update(active).is_err(),
            "a cancelled lease must no longer modify the job"
        );
    }

    #[test]
    fn snapshot_replays_new_records_and_old_journal_after_rename_crash() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let first = store.submit(request("First")).unwrap();
        let old_journal = fs::read(directory.0.join("journal.bin")).unwrap();
        store.compact().unwrap();
        drop(store);
        fs::write(directory.0.join("journal.bin"), &old_journal).unwrap();
        let mut store = Store::open(&directory.0).unwrap();
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.events(&first.id).len(), 1);
        let second = store.submit(request("Second")).unwrap();
        store.compact().unwrap();
        let third = store.submit(request("Third")).unwrap();
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        assert_eq!(store.list().len(), 3);
        assert_eq!(store.get(&second.id).unwrap(), second);
        assert_eq!(store.get(&third.id).unwrap(), third);
        assert_eq!(store.events("").len(), 3);
        store.compact().unwrap();
        drop(store);
        let mut snapshot = fs::read(directory.0.join("snapshot.bin")).unwrap();
        snapshot[22] ^= 1;
        fs::write(directory.0.join("snapshot.bin"), snapshot).unwrap();
        assert!(Store::open(&directory.0).err().unwrap().contains("corrupt"));
    }

    #[test]
    fn exact_u64_json_and_bounded_requests() {
        let mut request = request("Identity");
        request.tmdb_id = Some(u64::MAX);
        assert_eq!(Request::from_json(&request.to_json()).unwrap(), request);
        request.title = "".to_owned();
        assert!(request.validate().is_err());
    }

    #[test]
    fn explicit_sources_are_distinct_and_lease_release_is_durable() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let mut first_request = request("Same title");
        first_request.source_path = Some("/media/first.mkv".to_owned());
        let first = store.submit(first_request).unwrap();
        let mut second_request = request("Same title");
        second_request.source_path = Some("/media/second.mkv".to_owned());
        let second = store.submit(second_request).unwrap();
        assert_ne!(first.id, second.id);
        let lease = store.claim(now(), 60).unwrap().unwrap();
        let lease_id = lease.lease_id.clone().unwrap();
        store.renew(&lease.id, &lease_id, now(), 120).unwrap();
        assert!(store.release_lease(&lease.id, "old-lease").is_err());
        let released = store.release_lease(&lease.id, &lease_id).unwrap();
        assert!(released.lease_id.is_none());
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        assert_eq!(store.get(&released.id).unwrap(), released);
        assert!(store.update(lease).is_err());
        assert!(store.claim(now(), 60).unwrap().is_some());
    }

    #[test]
    fn corrupted_length_header_never_discards_a_complete_record() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.submit(request("Complete")).unwrap();
        drop(store);
        let path = directory.0.join("journal.bin");
        let mut bytes = fs::read(&path).unwrap();
        bytes[18] ^= 0x08;
        fs::write(&path, &bytes).unwrap();
        let error = Store::open(&directory.0).err().unwrap();
        assert!(error.contains("header"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn workflow_and_heartbeat_survive_polling_without_counting_attempts() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.submit(request("Download")).unwrap();
        let at = now();
        let mut job = store.claim(at, 60).unwrap().unwrap();
        assert_eq!(job.attempts, 0);
        let id = job.lease_id.clone().unwrap();
        let heartbeat = store.renew_lease(&job.id, &id, at, 300).unwrap();
        job.state = "downloading".to_owned();
        job.acquisition_url =
            Some("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".to_owned());
        job.download_id = Some("0123456789abcdef0123456789abcdef01234567".to_owned());
        store.update(job.clone()).unwrap();
        assert_eq!(
            store.get(&job.id).unwrap().lease_until,
            heartbeat.lease_until
        );
        store.release_lease(&job.id, &id).unwrap();
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        let resumed = store.claim(at, 60).unwrap().unwrap();
        assert_eq!(resumed.state, "downloading");
        assert_eq!(resumed.attempts, 0);
        assert_eq!(resumed.acquisition_url, job.acquisition_url);
        assert_eq!(resumed.download_id, job.download_id);
        let mut old_format = job.to_json();
        if let Value::Object(map) = &mut old_format {
            map.remove("acquisition_url");
            map.remove("download_id");
        }
        let old_format = Job::from_json(&old_format).unwrap();
        assert!(old_format.acquisition_url.is_none() && old_format.download_id.is_none());
    }

    #[test]
    fn ambiguous_explicit_sources_are_rejected() {
        let mut request = request("Ambiguous");
        request.source_path = Some("/media/file.mkv".to_owned());
        request.source_url = Some("http://127.0.0.1/file.mkv".to_owned());
        assert!(request.validate().is_err());
    }

    #[test]
    fn due_retry_reenters_processing_and_retains_its_lease() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.submit(request("To retry")).unwrap();
        let at = now();
        let mut job = store.claim(at, 60).unwrap().unwrap();
        job.state = "failed".to_owned();
        job.attempts = 1;
        job.last_error = Some("transient error".to_owned());
        job.next_attempt_at = at + 5;
        store.update(job).unwrap();
        assert!(store.claim(at + 4, 60).unwrap().is_none());
        let mut resumed = store.claim(at + 5, 60).unwrap().unwrap();
        assert_eq!(resumed.state, "processing");
        assert_eq!(resumed.attempts, 1);
        resumed.acquisition_url = Some("http://127.0.0.1/source.torrent".to_owned());
        store.update(resumed.clone()).unwrap();
        assert_eq!(store.get(&resumed.id).unwrap().lease_id, resumed.lease_id);
    }

    #[test]
    fn retained_event_history_keeps_sequence_ids_across_snapshot_and_replay() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let mut job = store.submit(request("Bounded history")).unwrap();
        for _ in 0..MAX_EVENTS + 7 {
            job.progress = 0.5;
            store.update(job.clone()).unwrap();
        }
        let history = store.events(&job.id);
        assert_eq!(history.len(), MAX_EVENTS);
        assert_eq!(history.first().unwrap().id, 9);
        assert_eq!(history.last().unwrap().id, 1008);
        store.compact().unwrap();
        drop(store);
        let mut reopened = Store::open(&directory.0).unwrap();
        assert_eq!(reopened.events(&job.id), history);
        job.last_error = Some("é".repeat(MAX_EVENT_MESSAGE));
        reopened.update(job.clone()).unwrap();
        let history = reopened.events(&job.id);
        assert_eq!(history.len(), MAX_EVENTS);
        assert_eq!(history.last().unwrap().id, 1009);
        assert_eq!(history.last().unwrap().message.len(), MAX_EVENT_MESSAGE);
        drop(reopened);
        let reopened = Store::open(&directory.0).unwrap();
        assert_eq!(reopened.events(&job.id), history);
        assert_eq!(reopened.get(&job.id).unwrap().last_error, job.last_error);
    }

    #[test]
    fn automatic_compaction_bounds_journal_and_restores_acknowledged_jobs() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let mut job = store.submit(request("Automatic compaction")).unwrap();
        job.files = vec!["x".repeat(20_000)];
        for _ in 0..250 {
            store.update(job.clone()).unwrap();
        }
        assert!(directory.0.join("snapshot.bin").is_file());
        assert!(fs::metadata(directory.0.join("journal.bin")).unwrap().len() < COMPACTION_BYTES);
        assert!(store.maintenance_error().is_none());
        let confirmed = store.get(&job.id).unwrap();
        let events = store.events(&job.id);
        drop(store);
        let reopened = Store::open(&directory.0).unwrap();
        assert_eq!(reopened.get(&job.id).unwrap(), confirmed);
        assert_eq!(reopened.events(&job.id), events);
    }

    #[test]
    fn corrupted_snapshot_length_is_rejected_without_integer_overflow() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.submit(request("Corrupt length")).unwrap();
        store.compact().unwrap();
        drop(store);
        let path = directory.0.join("snapshot.bin");
        let mut bytes = fs::read(&path).unwrap();
        bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert!(Store::open(&directory.0).err().unwrap().contains("corrupt"));
    }

    #[test]
    fn maintenance_failure_does_not_undo_confirmation_or_poison_intact_journal() {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let mut job = store.submit(request("Maintenance recovery")).unwrap();
        fs::create_dir(directory.0.join("snapshot.bin")).unwrap();
        job.files = vec!["x".repeat(120_000)];
        for _ in 0..40 {
            store.update(job.clone()).unwrap();
        }
        assert!(store.maintenance_error().is_some());
        assert!(!store.poisoned);
        job.progress = 0.75;
        store.update(job.clone()).unwrap();
        let confirmed = store.get(&job.id).unwrap();
        drop(store);
        fs::remove_dir(directory.0.join("snapshot.bin")).unwrap();
        let mut reopened = Store::open(&directory.0).unwrap();
        assert_eq!(reopened.get(&job.id).unwrap(), confirmed);
        reopened.compact().unwrap();
        assert!(reopened.maintenance_error().is_none());
        drop(reopened);
        assert_eq!(
            Store::open(&directory.0).unwrap().get(&job.id).unwrap(),
            confirmed
        );
    }

    #[cfg(unix)]
    #[test]
    fn journal_hardlink_alias_cannot_create_two_owners() {
        let directory = Directory::new();
        let original = directory.0.join("original");
        let alias = directory.0.join("alias");
        let mut store = Store::open(&original).unwrap();
        store.submit(request("One owner")).unwrap();
        fs::create_dir(&alias).unwrap();
        fs::hard_link(original.join("journal.bin"), alias.join("journal.bin")).unwrap();
        assert!(Store::open(&alias).err().unwrap().contains("hard links"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_is_rejected() {
        use std::os::unix::fs::symlink;
        let directory = Directory::new();
        let real = directory.0.join("real");
        fs::create_dir(&real).unwrap();
        symlink(&real, directory.0.join("alias")).unwrap();
        assert!(Store::open(&directory.0.join("alias")).is_err());
    }
}
