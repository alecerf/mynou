//! Jobs, their history and the indexes that answer ownership, deduplication,
//! lineage and due-work questions, in the disk-resident index (`jobs.idx`).
//!
//! A job is one record plus small entries in the same ordered index (see
//! `layout`), so a transaction replaces a job and everything derived from it
//! atomically, and no routine operation enumerates the inventory: lookups read
//! one tree path, due work comes from a time-ordered queue in bounded batches,
//! and lists are cursor pages. There is no total job or byte limit; only the
//! per-record, per-transaction and per-page bounds of the index remain.
//! Earlier versions kept every job in one in-memory map behind a journal and
//! snapshot (`journal.bin`, `snapshot.bin`); `migration` converts them.

use crate::Result;
use crate::crypto::sha256;
use crate::inventory::{Change, Index, Key, MAX_PAGE_ROWS, Query};
use crate::json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

mod children;
mod groups;
mod irc;
mod layout;
mod legacy;
mod migration;
mod monitoring;
#[cfg(test)]
pub(crate) mod open_tests;
mod ops;
mod ownership;
mod replay;
mod rules;
#[cfg(test)]
mod tests;
pub use groups::SharedUpgrade;
use groups::group_format;
pub use migration::MigrationReport;

/// Events kept per job (a per-record bound); the total is not limited.
pub const MAX_JOB_EVENTS: usize = 1_000;
/// Events the journal of earlier versions retained in total.
const MAX_EVENTS: usize = 1_000;
const MAX_EVENT_MESSAGE: usize = 4_096;

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
    /// Native transfers an automatic retry detached from this request. Each one
    /// stays recorded here, in the retry's own transaction, until its pause (or
    /// the decision to keep it for another owner) is itself durable.
    pub retiring_transfers: Vec<String>,
}

/// Bounds the retirements one request can owe at the same time.
pub const MAX_RETIRING_TRANSFERS: usize = 8;

fn valid_transfer_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: u64,
    pub job_id: String,
    pub state: String,
    pub message: String,
    pub at: u64,
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
        let mut value = object([
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
        ]);
        if !self.retiring_transfers.is_empty() {
            value.insert("retiring_transfers", list(&self.retiring_transfers));
        }
        value
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
            retiring_transfers: match map.get("retiring_transfers") {
                None | Some(Value::Null) => Vec::new(),
                Some(_) => strings(map, "retiring_transfers")?,
            },
        };
        if job.retiring_transfers.len() > MAX_RETIRING_TRANSFERS
            || job
                .retiring_transfers
                .iter()
                .enumerate()
                .any(|(index, id)| {
                    !valid_transfer_id(id) || job.retiring_transfers[..index].contains(id)
                })
        {
            return Err("invalid pending transfer retirement".to_owned());
        }
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

// `open(2)` flags std does not name, from each target's `<fcntl.h>`. Tests pin
// the values per target; another target must add its own before it builds.
#[cfg(target_os = "macos")]
pub(crate) const O_NOFOLLOW: i32 = 0x0100;
#[cfg(target_os = "macos")]
pub(crate) const O_NONBLOCK: i32 = 0x0004;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) const O_NOFOLLOW: i32 = 0o400_000;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub(crate) const O_NOFOLLOW: i32 = 0o100_000;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(crate) const O_NONBLOCK: i32 = 0o4_000;
#[cfg(not(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
)))]
compile_error!("private store opens need this target's O_NOFOLLOW and O_NONBLOCK values");

/// Private store opens refuse a final symbolic link and never wait on a FIFO
/// or device swapped in after inspection. `O_NONBLOCK` has no effect on regular
/// files; callers still refuse anything else after opening.
#[cfg(unix)]
pub(crate) fn private_options() -> OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.mode(0o600).custom_flags(O_NOFOLLOW | O_NONBLOCK);
    options
}

#[cfg(test)]
type OpenHook = Box<dyn FnMut(&Path)>;
#[cfg(test)]
thread_local! {
    /// Runs between a private path's inspection and its open, so tests can
    /// swap the path inside that window.
    pub(crate) static BEFORE_PRIVATE_OPEN: std::cell::RefCell<Option<OpenHook>> =
        const { std::cell::RefCell::new(None) };
}
/// Opens an inspected private file for reading. The open refuses a final
/// symbolic link and never blocks; the descriptor must be the inspected regular
/// file.
pub(crate) fn open_inspected(path: &Path, inspected: &fs::Metadata) -> Result<File> {
    before_private_open(path);
    let file = private_options()
        .read(true)
        .open(path)
        .map_err(|error| format!("cannot open private file: {error}"))?;
    let opened = file.metadata().map_err(|error| error.to_string())?;
    if !opened.is_file() || !same_inode(inspected, &opened) {
        return Err("private file changed while opening".to_owned());
    }
    Ok(file)
}

/// Whether `opened` is the inode that `inspected` described.
pub(crate) fn same_inode(inspected: &fs::Metadata, opened: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    inspected.dev() == opened.dev() && inspected.ino() == opened.ino()
}

pub(crate) fn before_private_open(path: &Path) {
    #[cfg(test)]
    BEFORE_PRIVATE_OPEN.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(path);
        }
    });
    let _ = path;
}

/// Takes `file`'s exclusive lock without waiting. A test build first waits for
/// any process a test is spawning: until it execs, the child shares every
/// descriptor, so a lock its parent just released by closing one stays held.
pub(crate) fn try_lock_file(file: &File) -> std::result::Result<(), fs::TryLockError> {
    #[cfg(test)]
    let _spawns = SPAWNING
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    file.try_lock()
}

/// Held for writing while a test spawns a process; see [`try_lock_file`].
#[cfg(test)]
pub(crate) static SPAWNING: std::sync::RwLock<()> = std::sync::RwLock::new(());

pub(crate) fn reject_symlinks(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("symbolic link not allowed in a storage path".to_owned());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot access path: {error}")),
        }
    }
    Ok(())
}

/// Error text marking a full disk or exhausted quota; inventory errors test
/// for it to tell the operator to free space rather than retry.
pub(crate) const STORAGE_FULL: &str = "no space left on the storage device";

/// Describes an I/O error without paths, naming exhaustion uniformly.
pub(crate) fn io_text(error: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded | ErrorKind::FileTooLarge => {
            STORAGE_FULL.to_owned()
        }
        _ => error.to_string(),
    }
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    // A FIFO swapped in for the directory must not block the open.
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot synchronize directory: {}", io_text(&error)))?;
    if !directory
        .metadata()
        .map_err(|error| format!("cannot synchronize directory: {}", io_text(&error)))?
        .is_dir()
    {
        return Err("cannot synchronize directory: not a directory".to_owned());
    }
    sync_file(&directory)
        .map_err(|error| format!("cannot synchronize directory: {}", io_text(&error)))
}

/// Every store durability flush goes through here. On Apple platforms std
/// issues `F_FULLFSYNC`, which flushes the whole drive cache, so tests count
/// flushes per operation to keep that cost bounded.
pub(crate) fn sync_file(file: &File) -> std::io::Result<()> {
    #[cfg(test)]
    FLUSHES.with(|count| count.set(count.get() + 1));
    file.sync_all()
}

#[cfg(test)]
thread_local! { static FLUSHES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }

/// Durability flushes issued so far by the calling test thread.
#[cfg(test)]
pub(crate) fn flushes() -> u64 {
    FLUSHES.with(std::cell::Cell::get)
}

fn secure_file(path: &Path) -> Result<File> {
    // An existing file must be the inspected inode; a missing one is created
    // exclusively. A creator racing this one is inspected again, once.
    let mut attempts = 0;
    let (file, metadata) = loop {
        attempts += 1;
        reject_symlinks(path)?;
        let inspected = match fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err("storage must be a regular file".to_owned());
            }
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        before_private_open(path);
        let mut options = private_options();
        options.read(true).write(true);
        if inspected.is_none() {
            options.create_new(true);
        }
        let file = match options.open(path) {
            Ok(file) => file,
            Err(error)
                if attempts < 2
                    && inspected.is_none()
                    && error.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                continue;
            }
            Err(error) => return Err(format!("cannot open storage: {error}")),
        };
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() {
            return Err("storage must be a regular file".to_owned());
        }
        if inspected.is_some_and(|inspected| !same_inode(&inspected, &metadata)) {
            return Err("storage changed while opening".to_owned());
        }
        break (file, metadata);
    };
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
    let inspected = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot access existing storage: {error}"))?;
    if !inspected.is_file() {
        return Err("storage must be a regular file".to_owned());
    }
    before_private_open(path);
    let file = private_options()
        .read(true)
        .open(path)
        .map_err(|error| format!("cannot open existing storage for reading: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("storage must be a regular file".to_owned());
    }
    if !same_inode(&inspected, &metadata) {
        return Err("storage changed while opening".to_owned());
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
    sync_directory(path)?;
    sync_directory(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )
}

/// Reads other jobs by identifier. Implemented over a legacy snapshot held in
/// memory and over the index, so lineage rules are written once.
pub(crate) trait JobLookup {
    fn lookup(&mut self, id: &str) -> Result<Option<Job>>;
}

use rules::validate_lineage;

pub use ops::Counts;
use ops::{Db, cursor_text, parse_cursor};

/// Jobs examined by one claim attempt. Work per attempt is bounded by this,
/// not by how many jobs are open; a rotating cursor reaches the rest.
const CLAIM_SCAN: usize = 256;
/// Seconds of claim clock between durable claim cursor checkpoints. The live
/// cursor advances in memory on every attempt; a restart resumes from the
/// last checkpoint, and a fresh process checkpoints its first attempt.
const CLAIM_CHECKPOINT_SECS: u64 = 60;
const UNMIGRATED: &str =
    "Job storage has not been migrated; start the service or any command with write access once";

pub(crate) enum Backend {
    /// Nothing has ever been stored (a read-only open of an empty directory).
    Absent,
    /// A legacy journal exists but this open cannot migrate it (read-only).
    Unmigrated,
    Ready(Box<Index>),
}

/// Read-only job access shared by indexed state and bounded legacy preflight.
pub(crate) trait RecoveryJobs {
    fn check_irc_ownership(&mut self, job: &Job) -> Result<()>;
    fn get(&mut self, id: &str) -> Result<Option<Job>>;
    fn requester_jobs_page(
        &mut self,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Job>, Option<Key>)>;
    fn irc_origin_page(
        &mut self,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Job>, Option<Key>)>;
}

impl RecoveryJobs for Store {
    fn check_irc_ownership(&mut self, job: &Job) -> Result<()> {
        self.read((), |db| db.check_irc_ownership(job))
    }
    fn get(&mut self, id: &str) -> Result<Option<Job>> {
        Store::get(self, id)
    }
    fn requester_jobs_page(
        &mut self,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Job>, Option<Key>)> {
        Store::requester_jobs_page(self, after, limit)
    }
    fn irc_origin_page(
        &mut self,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Job>, Option<Key>)> {
        Store::irc_origin_page(self, after, limit)
    }
}

/// The job inventory. One owner holds the directory lock; the index owns its
/// own directory (`jobs.idx`).
pub struct Store {
    _lock: File,
    backend: Backend,
    read_only: bool,
    poisoned: bool,
    maintenance_error: Option<String>,
    /// Most recent bounded queue and requester sweep work, for diagnostics.
    claim_pass: [usize; 3],
    /// The live claim cursor once loaded, and the claim clock of its last
    /// durable checkpoint in this process.
    claim_cursor: Option<Option<Key>>,
    claim_saved: Option<u64>,
    requester_sweep_rows: usize,
    pub(crate) migration: Option<MigrationReport>,
}

impl Drop for Store {
    fn drop(&mut self) {
        // A child spawned concurrently may inherit the open description until
        // exec. Release this owner's lock without waiting for that descriptor.
        let _ = self._lock.unlock();
    }
}

/// One page of jobs ordered by creation time.
#[derive(Debug)]
pub struct JobPage {
    pub sequence: u64,
    pub jobs: Vec<Job>,
    /// Exclusive cursor of the next page, absent after the last one.
    pub next: Option<String>,
    /// All jobs (or all library entries) in the store.
    pub total: u64,
}

/// Query of `GET /api/jobs` and `GET /api/library`: a cursor page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageQuery {
    pub after: Option<String>,
    pub limit: usize,
}

impl PageQuery {
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() > 256 {
            return Err("Page query is too large".into());
        }
        let mut query = Self {
            after: None,
            limit: 50,
        };
        let mut seen = BTreeSet::new();
        for pair in text.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').ok_or("Invalid page query")?;
            if !seen.insert(name) {
                return Err("Duplicate page query field".into());
            }
            match name {
                "after" => {
                    parse_cursor(value)?;
                    query.after = Some(value.to_owned());
                }
                "limit" => {
                    query.limit = value
                        .parse()
                        .ok()
                        .filter(|limit| (1..=200).contains(limit) && !value.starts_with('0'))
                        .ok_or("Page limit must be 1 to 200")?;
                }
                _ => return Err("Unknown page query field".into()),
            }
        }
        Ok(query)
    }
}

fn clean_path(directory: &Path) -> Result<()> {
    if directory.file_name().is_none()
        || directory
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("storage requires a clean directory path without traversal".to_owned());
    }
    reject_symlinks(directory)
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

    /// Opens for writing, creating the store, recovering an interrupted write
    /// and migrating a journal of an earlier version.
    pub fn open(directory: &Path) -> Result<Self> {
        Self::open_checked(directory, &mut |_| Ok(()))
    }

    fn open_checked(
        directory: &Path,
        preflight: &mut dyn FnMut(&legacy::State) -> Result<()>,
    ) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        clean_path(directory)?;
        create_private_directory(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("cannot set storage permissions: {error}"))?;
        let lock = secure_file(&directory.join(".lock"))?;
        try_lock_file(&lock)
            .map_err(|error| format!("storage is already open or cannot be locked: {error}"))?;
        sync_directory(directory)?;
        match legacy::inspect(directory, false)? {
            legacy::Inspection::Legacy(files) => {
                let state = legacy::parse(files.snapshot.as_deref(), &files.journal)?;
                legacy::validate(&state)?;
                preflight(&state)?;
            }
            legacy::Inspection::Missing => {
                if !directory
                    .join("jobs.idx")
                    .try_exists()
                    .map_err(|_| "Cannot inspect the job index")?
                {
                    preflight(&legacy::parse(None, &[])?)?;
                }
            }
            legacy::Inspection::Marker { .. } => {}
        }
        migration::open(directory, lock, false)
    }

    /// Loads existing durable state without creating files, changing permissions,
    /// recovering or migrating. The lock excludes concurrent writers.
    pub fn open_read_only(directory: &Path) -> Result<Self> {
        clean_path(directory)?;
        if !fs::metadata(directory)
            .map_err(|error| format!("cannot access existing storage: {error}"))?
            .is_dir()
        {
            return Err("storage must be a directory".to_owned());
        }
        let lock = read_only_file(&directory.join(".lock"))?;
        try_lock_file(&lock)
            .map_err(|error| format!("storage is already open or cannot be locked: {error}"))?;
        migration::open(directory, lock, true)
    }

    pub(crate) fn prepare(directory: &Path, config: &crate::config::Config) -> Result<Self> {
        Self::open_checked(directory, &mut |state| {
            let requesters = crate::requesters::RequesterStore::open(directory, config, true)?;
            let mut irc = crate::irc::AnnouncementStore::open(directory, &config.irc, true)?;
            let mut jobs = legacy::View(state);
            crate::requesters::engine::validate_storage(&requesters, &mut jobs)?;
            crate::irc::admission::recover(&mut irc, &requesters, false)?;
            crate::irc::routing::recover(&mut irc, &mut jobs, false)?;
            Ok(())
        })
    }

    pub fn open_for_management(config: &crate::config::Config) -> Result<Self> {
        Self::prepare(&config.store_dir, config)
    }

    pub(crate) fn initialize(&mut self) -> Result<()> {
        Ok(())
    }

    pub(super) fn from_parts(
        _directory: &Path,
        lock: File,
        backend: Backend,
        read_only: bool,
        migration: Option<MigrationReport>,
    ) -> Self {
        Self {
            _lock: lock,
            backend,
            read_only,
            poisoned: false,
            maintenance_error: None,
            claim_pass: [0; 3],
            claim_cursor: None,
            claim_saved: None,
            requester_sweep_rows: 0,
            migration,
        }
    }

    fn read<T>(&mut self, empty: T, work: impl FnOnce(&mut Db<'_>) -> Result<T>) -> Result<T> {
        match &mut self.backend {
            Backend::Absent => Ok(empty),
            Backend::Unmigrated => Err(UNMIGRATED.to_owned()),
            Backend::Ready(index) => work(&mut Db(index)),
        }
    }

    fn mutate<T>(&mut self, work: impl FnOnce(&mut Db<'_>) -> Result<T>) -> Result<T> {
        if self.read_only {
            return Err("storage is read-only".to_owned());
        }
        if self.poisoned {
            return Err("storage unavailable after a write error; reopen storage".to_owned());
        }
        let Backend::Ready(index) = &mut self.backend else {
            return Err(UNMIGRATED.to_owned());
        };
        let result = work(&mut Db(index));
        if result.is_err() && !index.is_usable() {
            self.poisoned = true;
        }
        result
    }

    /// A maintenance error does not revoke an acknowledged, synchronized write.
    /// Writes with uncertain outcomes block further storage operations.
    pub fn maintenance_error(&self) -> Option<&str> {
        self.maintenance_error.as_deref()
    }

    /// Index size and state for diagnostics, from the durable root only.
    pub(crate) fn storage(&self) -> Value {
        let mut value = match &self.backend {
            Backend::Absent => crate::inventory::report("empty", None),
            Backend::Unmigrated => crate::inventory::report("needs_migration", None),
            Backend::Ready(index) => crate::inventory::report("index", Some(index)),
        };
        value.insert("migrated_at_start", self.migration.is_some());
        value
    }

    /// Storage state for status reports; never fails on an unreadable store.
    pub fn describe(&mut self) -> Value {
        let state = match &self.backend {
            Backend::Absent => "empty",
            Backend::Unmigrated => "needs_migration",
            Backend::Ready(_) => "index",
        };
        let mut value = Value::object();
        value.insert("state", state);
        let mut scheduling = Value::object();
        for (name, count) in [
            ("claim_promoted", self.claim_pass[0]),
            ("claim_pages", self.claim_pass[1]),
            ("claim_examined", self.claim_pass[2]),
            ("requester_sweep_rows", self.requester_sweep_rows),
        ] {
            scheduling.insert(name, Value::Number(count as f64));
        }
        value.insert("scheduling", scheduling);
        match self.counts() {
            Ok(counts) => {
                value.insert("jobs", Value::Number(counts.jobs as f64));
                value.insert("active", Value::Number(counts.active as f64));
                value.insert("ready", Value::Number(counts.ready as f64));
            }
            Err(_) => {
                for name in ["jobs", "active", "ready"] {
                    value.insert(name, Value::Null);
                }
            }
        }
        value.insert(
            "migration",
            self.migration.as_ref().map_or(Value::Null, |report| {
                let mut value = Value::object();
                value.insert("jobs", Value::Number(report.jobs as f64));
                value.insert("events", Value::Number(report.events as f64));
                value.insert("resumed", report.resumed);
                value.insert("preserved_snapshot", report.backup.clone());
                value
            }),
        );
        value
    }

    pub fn counts(&mut self) -> Result<Counts> {
        self.read(Counts::default(), |db| db.counts())
    }

    pub fn get(&mut self, id: &str) -> Result<Option<Job>> {
        self.read(None, |db| db.get(id))
    }

    /// Where the single-account watchlist sweep resumes; absent at the start.
    pub(crate) fn watchlist_cursor(&mut self) -> Result<Option<crate::integrations::SweepCursor>> {
        self.read(None, |db| {
            db.0.get(&layout::WATCHLIST_CURSOR_KEY)?
                .map(|bytes| crate::integrations::SweepCursor::decode(&bytes))
                .transpose()
        })
    }

    /// Records sweep progress even when no watchlist item has created a job.
    /// Writable opens create the index; read-only stores must reject writes.
    pub(crate) fn save_watchlist_cursor(
        &mut self,
        cursor: &crate::integrations::SweepCursor,
    ) -> Result<()> {
        self.mutate(|db| {
            db.0.commit(vec![Change::Put(
                layout::WATCHLIST_CURSOR_KEY,
                cursor.encode().to_vec(),
            )])
            .map(|_| ())
        })
    }

    /// Identifies the stored state for stale-plan fences; changes on every commit.
    pub fn sequence(&mut self) -> Result<u64> {
        match &mut self.backend {
            Backend::Absent => Ok(0),
            Backend::Unmigrated => Err(UNMIGRATED.to_owned()),
            Backend::Ready(index) => index.sequence(),
        }
    }

    /// Jobs, newest first, after the exclusive `after` cursor.
    pub fn page(&mut self, after: Option<&str>, limit: usize) -> Result<JobPage> {
        self.job_page(layout::T_ALL, after, limit, false)
    }

    /// The current library versions, newest first.
    pub fn library_page(&mut self, after: Option<&str>, limit: usize) -> Result<JobPage> {
        self.job_page(layout::T_LIBRARY, after, limit, true)
    }

    fn job_page(
        &mut self,
        table: u8,
        after: Option<&str>,
        limit: usize,
        library: bool,
    ) -> Result<JobPage> {
        if limit == 0 || limit > MAX_PAGE_ROWS {
            return Err("Job page limit must be 1 to 200".into());
        }
        let after = after
            .map(|text| {
                parse_cursor(text)
                    .map(|(created, id)| layout::key(&[&[table], &(!created).to_be_bytes(), &id]))
            })
            .transpose()?;
        let empty = JobPage {
            sequence: 0,
            jobs: Vec::new(),
            next: None,
            total: 0,
        };
        self.read(empty, |db| {
            let counts = db.counts()?;
            let total = if library { counts.library } else { counts.jobs };
            let page = db.scan(&[table], after, limit)?;
            let mut jobs = Vec::with_capacity(page.entries.len());
            let mut last = None;
            let mut bytes = 0_usize;
            let mut more = page.next.is_some();
            for (found, _) in &page.entries {
                let created = !u64::from_be_bytes(ops::key_part(found, 1)?);
                let id: [u8; 16] = ops::key_part(found, 9)?;
                let job = db.require(&id)?;
                let length = layout::encode_job(&job).len();
                if !jobs.is_empty() && bytes + length > crate::inventory::MAX_PAGE_BYTES {
                    more = true;
                    break;
                }
                bytes += length;
                jobs.push(job);
                last = Some(cursor_text(created, &id));
            }
            Ok(JobPage {
                sequence: db.0.sequence()?,
                jobs,
                next: if more { last } else { None },
                total,
            })
        })
    }

    /// Every job, oldest first (sorted after reading). This reads the whole store into memory: use it
    /// in tests and small tools, never on a path whose cost should not grow
    /// with the inventory; production code reads pages.
    pub fn list(&mut self) -> Result<Vec<Job>> {
        let mut jobs = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = self.page(after.as_deref(), MAX_PAGE_ROWS)?;
            jobs.extend(page.jobs);
            after = page.next;
            if after.is_none() {
                jobs.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
                return Ok(jobs);
            }
        }
    }

    /// Every current library version; see `list` for when to use it.
    pub fn library_jobs(&mut self) -> Result<Vec<Job>> {
        let mut jobs = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = self.library_page(after.as_deref(), MAX_PAGE_ROWS)?;
            jobs.extend(page.jobs);
            after = page.next;
            if after.is_none() {
                jobs.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
                return Ok(jobs);
            }
        }
    }

    /// Retained history of one job, or of every job when `id` is empty (the
    /// first page-sized window of the global sequence).
    pub fn events(&mut self, id: &str) -> Result<Vec<Event>> {
        if id.is_empty() {
            return self
                .event_feed(None, MAX_PAGE_ROWS)
                .map(|(events, _)| events);
        }
        let Ok(raw) = layout::id_bytes(id) else {
            return Ok(Vec::new());
        };
        self.read(Vec::new(), |db| db.events(&raw))
    }

    /// Events of every job in sequence order after `after` (exclusive), at most
    /// `limit`, with the sequence to continue from when more remain.
    pub fn event_feed(
        &mut self,
        after: Option<u64>,
        limit: usize,
    ) -> Result<(Vec<Event>, Option<u64>)> {
        if limit == 0 || limit > MAX_PAGE_ROWS {
            return Err("Event page limit must be 1 to 200".into());
        }
        self.read((Vec::new(), None), |db| {
            let mut events = Vec::new();
            let mut cursor = after.map(layout::sequence_key);
            loop {
                let want = limit.saturating_sub(events.len()).clamp(1, MAX_PAGE_ROWS);
                let page = db.scan(&[layout::T_SEQUENCE], cursor, want)?;
                for (found, value) in &page.entries {
                    let sequence = u64::from_be_bytes(ops::key_part(found, 1)?);
                    let id: [u8; 16] = ops::array(value)?;
                    let event = db
                        .event(&id, sequence)?
                        .ok_or("Job history sequence refers to a missing event")?;
                    events.push(event);
                }
                cursor = page.next;
                if cursor.is_none() || events.len() >= limit {
                    let next = cursor.and_then(|_| events.last().map(|event| event.id));
                    return Ok((events, next));
                }
            }
        })
    }

    pub(crate) fn media_exists(&mut self, media_key: &str) -> Result<bool> {
        self.read(false, |db| db.media_exists(media_key))
    }

    /// The job representing each requested media key: a ready job wins,
    /// otherwise the newest. Each key reads only its own entries.
    pub(crate) fn states_for(&mut self, keys: &BTreeSet<String>) -> Result<BTreeMap<String, Job>> {
        self.read(BTreeMap::new(), |db| {
            let mut states = BTreeMap::new();
            for key in keys {
                if let Some(job) = db.media_state(key)? {
                    states.insert(key.clone(), job);
                }
            }
            Ok(states)
        })
    }

    /// Jobs of one media identity, oldest first, scanning only this identity.
    #[cfg(test)]
    pub(crate) fn media_jobs(&mut self, media_key: &str) -> Result<Vec<Job>> {
        self.read(Vec::new(), |db| db.media_jobs(media_key))
    }

    /// The inventory key of a job record, so tests can damage it.
    #[cfg(test)]
    pub(crate) fn record_key(id: &str) -> Result<crate::inventory::Key> {
        Ok(layout::job_key(&layout::id_bytes(id)?))
    }

    pub(crate) fn media_page(
        &mut self,
        media: &str,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Job>, Option<Key>)> {
        self.read((Vec::new(), None), |db| {
            let digest = layout::digest_bytes(media)?;
            db.jobs_page(
                &[&[layout::T_MEDIA][..], &digest].concat(),
                41,
                after,
                limit,
            )
        })
    }

    pub(crate) fn current_media_job(&mut self, media: &str) -> Result<Option<Job>> {
        self.read(None, |db| db.current_media_job(media))
    }

    pub(crate) fn transfer_jobs_page(
        &mut self,
        transfer: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Job>, Option<String>)> {
        self.read((Vec::new(), None), |db| {
            let prefix = [
                &[layout::T_TRANSFER][..],
                &layout::short("transfer", &[transfer]),
            ]
            .concat();
            let after = after
                .map(|text| {
                    let (created, id) = parse_cursor(text)?;
                    if created != 0 {
                        return Err("Invalid transfer request cursor".to_owned());
                    }
                    Ok(layout::key(&[&prefix, &id]))
                })
                .transpose()?;
            let (jobs, next) = db.jobs_page(&prefix, 17, after, limit)?;
            Ok((
                jobs,
                next.map(|key| ops::key_part::<16>(&key, 17).map(|id| cursor_text(0, &id)))
                    .transpose()?,
            ))
        })
    }

    /// The job owning a canonical request key.
    pub(crate) fn by_key(&mut self, key: &str) -> Result<Option<Job>> {
        self.read(None, |db| db.by_key(key))
    }

    /// Whether a job that is not cancelled still depends on this transfer.
    pub(crate) fn transfer_has_owner(&mut self, transfer: &str) -> Result<bool> {
        self.read(false, |db| db.transfer_has_owner(transfer))
    }

    /// Jobs that depend on a transfer through any binding.
    #[cfg(test)]
    pub fn transfer_jobs(&mut self, transfer: &str) -> Result<Vec<Job>> {
        self.read(Vec::new(), |db| db.transfer_jobs(transfer))
    }

    /// Whether another request than `job` shares its native transfer, through
    /// its own download or its shared-file binding. Each transfer reads at
    /// most two live-owner entries, one of which may be `job` itself.
    pub fn has_co_owner(&mut self, job: &Job) -> Result<bool> {
        self.read(false, |db| {
            for transfer in layout::transfers(job) {
                if db
                    .live_owners(transfer, 2)?
                    .iter()
                    .any(|other| other.id != job.id)
                {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }

    pub(crate) fn transfer_cancelled(&mut self, transfer: &str) -> Result<bool> {
        self.read(false, |db| {
            Ok(!db
                .owners(&layout::transfer_cancelled_prefix(transfer), 17, 1, |job| {
                    job.state == "cancelled" && job.download_id.as_deref() == Some(transfer)
                })?
                .is_empty())
        })
    }

    /// Whether any request that is not cancelled still claims a transfer.
    pub(crate) fn transfer_claimed(
        &mut self,
        transfer: &str,
        acquisition: Option<&str>,
    ) -> Result<bool> {
        self.read(false, |db| {
            let claims = [
                &layout::transfer_live_prefix(transfer)[..],
                &[layout::CLAIMS],
            ]
            .concat();
            if !db
                .owners(&claims, 18, 1, |job| {
                    job.state != "cancelled" && layout::claims(job, transfer)
                })?
                .is_empty()
            {
                return Ok(true);
            }
            let Some(url) = acquisition else {
                return Ok(false);
            };
            Ok(!db
                .owners(&layout::acquisition_live_prefix(url), 17, 1, |job| {
                    job.state != "cancelled" && job.acquisition_url.as_deref() == Some(url)
                })?
                .is_empty())
        })
    }

    /// The pending upgrade of a media identity, if any.
    pub(crate) fn pending_for(&mut self, media_key: &str) -> Result<Option<Job>> {
        self.read(None, |db| {
            let digest = layout::digest_bytes(media_key)?;
            let Some(found) = db
                .keys(&[&[layout::T_PENDING][..], &digest].concat(), 1)?
                .first()
                .copied()
            else {
                return Ok(None);
            };
            db.require(&ops::key_part(&found, 33)?).map(Some)
        })
    }

    pub(crate) fn irc_candidates(&mut self, request: &Request) -> Result<irc::Candidates> {
        self.read(irc::Candidates::default(), |db| db.irc_candidates(request))
    }

    pub(crate) fn irc_origin_page(
        &mut self,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Job>, Option<Key>)> {
        self.read((Vec::new(), None), |db| db.irc_origin_page(after, limit))
    }

    pub(crate) fn check_irc_origin(&mut self, origin: &crate::irc::Origin) -> Result<()> {
        self.read((), |db| db.check_irc_origin(origin))
    }

    pub(crate) fn attach_irc_origin(&mut self, origin: crate::irc::Origin) -> Result<Job> {
        self.mutate(|db| db.attach_irc_origin(origin))
    }

    /// Requester-provenance jobs that are not ready or cancelled, oldest first.
    pub(crate) fn requester_open_page(
        &mut self,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Job>, Option<Key>)> {
        self.read((Vec::new(), None), |db| {
            let page = db.scan(&[layout::T_REQUESTER], after, limit)?;
            let mut jobs = Vec::new();
            for (found, _) in &page.entries {
                jobs.push(db.require(&ops::key_part(found, 9)?)?);
            }
            Ok((jobs, page.next))
        })
    }

    /// Requests with requester provenance, any state, one page at a time.
    pub(crate) fn requester_jobs_page(
        &mut self,
        after: Option<Key>,
        limit: usize,
    ) -> Result<(Vec<Job>, Option<Key>)> {
        self.read((Vec::new(), None), |db| {
            let page = db.scan(&[layout::T_DEMAND], after, limit)?;
            let mut jobs = Vec::new();
            for (found, _) in &page.entries {
                jobs.push(db.require(&ops::key_part(found, 33)?)?);
            }
            Ok((jobs, page.next))
        })
    }

    /// Read a bounded window without acknowledging it. Progress is committed
    /// only after every returned row has settled, so an error replays the page.
    pub(crate) fn sweep_requester_jobs(&mut self, limit: usize) -> Result<(Vec<Job>, Option<Key>)> {
        let after = self.read(None, |db| {
            db.cursor(&layout::REQUESTER_SWEEP_CURSOR_KEY, layout::T_REQUESTER)
        })?;
        let page = self.requester_open_page(after, limit)?;
        self.requester_sweep_rows = page.0.len();
        Ok(page)
    }

    pub(crate) fn finish_requester_sweep(&mut self, next: Option<Key>) -> Result<()> {
        self.mutate(|db| db.save_cursor(layout::REQUESTER_SWEEP_CURSOR_KEY, next))
    }

    /// The shared-file binding recorded for one physical video, if any: one
    /// verified entry, however many pack mappings ever named the file.
    pub(crate) fn shared_binding(
        &mut self,
        torrent_id: &str,
        file_path: &str,
    ) -> Result<Option<crate::pack::SharedFile>> {
        self.read(None, |db| {
            let known = db.owners(
                &layout::shared_physical_prefix(torrent_id, file_path),
                17,
                1,
                |job| {
                    job.pack_file.as_deref() == Some(file_path)
                        && job.shared_file.as_ref().is_some_and(|file| {
                            file.torrent_id == torrent_id && file.file_path == file_path
                        })
                },
            )?;
            Ok(known.into_iter().next().and_then(|job| job.shared_file))
        })
    }

    /// Owners of a shared video in canonical episode order.
    pub(crate) fn group_members(&mut self, file: &crate::pack::SharedFile) -> Result<Vec<Job>> {
        self.read(Vec::new(), |db| db.group_members(file))
    }

    pub fn shared_group(&mut self, id: &str) -> Result<Vec<Job>> {
        self.read(Vec::new(), |db| db.shared_group(id))
    }

    pub(crate) fn current_shared_group(&mut self, id: &str) -> Result<Vec<Job>> {
        self.read(Vec::new(), |db| db.current_shared_group(id))
    }

    pub(crate) fn is_library_tip(&mut self, id: &str) -> Result<bool> {
        self.read(false, |db| db.is_tip(id))
    }

    pub fn submit_upgrade(
        &mut self,
        parent_id: &str,
        request: Request,
        release: RecordedRelease,
    ) -> Result<Job> {
        request.validate()?;
        release.validate()?;
        self.mutate(|db| {
            let parent = db
                .get(parent_id)?
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
            if let Some(existing) = db.by_key(&key)? {
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
                || !db.is_tip(&parent.id)?
            {
                return Err(
                    "an upgrade requires a monitored current library import with a release baseline"
                        .to_owned(),
                );
            }
            if db.other_pending(&request.media_key(), None)? {
                return Err("an upgrade for this media is already pending".to_owned());
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
                retiring_transfers: Vec::new(),
            };
            db.commit_jobs(vec![job.clone()], "library upgrade recorded")?;
            Ok(job)
        })
    }

    pub fn record_monitor_check(&mut self, id: &str, at: u64) -> Result<()> {
        self.mutate(|db| {
            let mut job = db
                .get(id)?
                .ok_or_else(|| "unknown library entry".to_owned())?;
            if !db.is_tip(&job.id)? {
                return Err("monitor checks require a current library import".to_owned());
            }
            // A real wall-clock rollback must not leave this entry permanently
            // ahead of the remaining queue. Preserve monotonic caller updates
            // when the stored check is not in the future.
            job.monitor_checked_at = if job.monitor_checked_at > now() {
                at
            } else {
                job.monitor_checked_at.max(at)
            };
            job.updated_at = now();
            db.commit_jobs(vec![job], "library monitor check recorded")
        })
    }

    pub fn set_monitored(&mut self, id: &str, monitored: bool) -> Result<Job> {
        self.mutate(|db| {
            let mut job = db
                .get(id)?
                .ok_or_else(|| "unknown library entry".to_owned())?;
            if !db.is_tip(&job.id)? {
                return Err("monitoring changes require a current library import".to_owned());
            }
            job.monitored = monitored;
            job.updated_at = now();
            db.commit_jobs(vec![job.clone()], "library monitoring updated")?;
            Ok(job)
        })
    }

    pub fn set_baseline(&mut self, id: &str, release: RecordedRelease) -> Result<Job> {
        release.validate()?;
        self.mutate(|db| {
            let mut job = db
                .get(id)?
                .ok_or_else(|| "unknown library entry".to_owned())?;
            if job.shared_file.is_some() || !db.is_tip(&job.id)? || job.release.is_some() {
                return Err(
                    "a release baseline can only be added to a current import without a baseline"
                        .to_owned(),
                );
            }
            job.release = Some(release);
            job.updated_at = now();
            db.commit_jobs(vec![job.clone()], "library release baseline recorded")?;
            Ok(job)
        })
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

    fn submit_with_mapping(
        &mut self,
        request: Request,
        pack_file: Option<String>,
        pack_origin: Option<crate::pack::PackOrigin>,
        requester: Option<crate::requesters::Provenance>,
    ) -> Result<Job> {
        request.validate()?;
        let key = request.canonical_key();
        self.mutate(|db| {
            if let Some(existing) = db.by_key(&key)? {
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
            let media = layout::digest_bytes(&request.media_key())?;
            if db.exists(&layout::shared_media_key(&media))? {
                return Err("Episode already belongs to immutable shared-file ownership".into());
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
                retiring_transfers: Vec::new(),
            };
            db.commit_jobs(vec![job.clone()], "request recorded")?;
            Ok(job)
        })
    }

    pub(crate) fn check_shared_submission(
        &mut self,
        file: &crate::pack::SharedFile,
        requests: &[Request],
    ) -> Result<Vec<Job>> {
        file.validate()?;
        if requests.is_empty() || requests.len() > crate::pack::MAX_PACK_EPISODES {
            return Err("Invalid shared owner count".into());
        }
        self.read(Vec::new(), |db| {
            let mut numbers = BTreeSet::new();
            let known = db.group_members(file)?;
            let mut reused = Vec::new();
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
                } else if known.is_empty() && db.media_exists(&request.media_key())? {
                    return Err("Shared-file owner already has an unrelated episode request".into());
                }
                if db.unshared_pack_mapping(
                    &file.file_path,
                    request.source_url.as_deref(),
                    &file.torrent_id,
                )? {
                    return Err("Physical video is already mapped without shared ownership".into());
                }
            }
            if db
                .0
                .get(&layout::physical_key(&file.torrent_id, &file.file_path))?
                .is_some_and(|known| known != layout::file_digest(file))
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
            } else if reused.len() != requests.len() {
                return Err("Stored shared ownership is incomplete".into());
            }
            Ok(reused)
        })
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
                retiring_transfers: Vec::new(),
            });
        }
        self.mutate(|db| {
            db.validate_shared_batch(&jobs)?;
            db.commit_jobs(jobs.clone(), "shared-file ownership recorded atomically")?;
            Ok(jobs)
        })
    }

    pub fn remap_pack(&mut self, id: &str, path: String) -> Result<Job> {
        crate::pack::validate_file_path(&path)?;
        self.mutate(|db| {
            let mut job = db.get(id)?.ok_or("Unknown pack episode request")?;
            if job.shared_file.is_some()
                || job.pack_file.is_none()
                || !matches!(job.state.as_str(), "failed" | "cancelled")
                || job.lease_id.is_some()
                || !job.imports.is_empty()
            {
                return Err("Mapping correction requires a failed or cancelled pack request without imports or an active lease".into());
            }
            let conflict = db.pack_mapping_conflict(&job, &path)?;
            if conflict {
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
            db.commit_jobs(vec![job.clone()], "pack mapping corrected; request requeued")?;
            Ok(job)
        })
    }

    pub fn update(&mut self, mut job: Job) -> Result<()> {
        self.mutate(|db| {
            let current = db.get(&job.id)?.ok_or_else(|| "unknown job".to_owned())?;
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
            // Only retry and retirement change the debt; a worker's stale copy cannot.
            job.retiring_transfers = current.retiring_transfers.clone();
            let ready_upgrade = current.state != "ready"
                && job.state == "ready"
                && current.upgrade_parent.is_some();
            if ready_upgrade {
                let parent = db
                    .get(current.upgrade_parent.as_deref().unwrap_or_default())?
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
            if matches!(current.state.as_str(), "ready" | "cancelled") && current.state != job.state
            {
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
                return db.finish_group_owner(job);
            }
            db.commit_jobs(vec![job], &message)
        })
    }

    pub fn claim(&mut self, at: u64, ttl: u64) -> Result<Option<Job>> {
        self.claim_filtered(at, ttl, |_| Ok(true))
    }

    /// Hands out the oldest claimable job. Work per attempt is one promotion
    /// batch plus at most `CLAIM_SCAN` queue entries, however many jobs exist;
    /// when the head of the queue is not claimable the next attempt resumes
    /// after the entries already examined, so no job waits forever behind
    /// jobs that cannot run. That position is written at most once per
    /// `CLAIM_CHECKPOINT_SECS`, so a backlog of unclaimable jobs does not cost
    /// a durable commit on every attempt.
    pub(crate) fn claim_filtered(
        &mut self,
        at: u64,
        ttl: u64,
        eligible: impl Fn(&Job) -> Result<bool>,
    ) -> Result<Option<Job>> {
        if ttl == 0 {
            return Err("lease duration must be positive".to_owned());
        }
        let deadline = at
            .checked_add(ttl)
            .ok_or_else(|| "lease duration too large".to_owned())?;
        let mut stats = [0; 3];
        let live = self.claim_cursor;
        let checkpoint = self
            .claim_saved
            .is_none_or(|saved| at.abs_diff(saved) >= CLAIM_CHECKPOINT_SECS);
        // The position after this attempt, and whether it is now durable.
        let mut next = None;
        let mut saved = false;
        let outcome = self.mutate(|db| {
            let mut cursor = match live {
                Some(cursor) => cursor,
                None => db.cursor(&layout::CLAIM_CURSOR_KEY, layout::T_QUEUE)?,
            };
            stats[0] = db.promote(at)?;
            let mut examined = 0;
            loop {
                let (keys, more) = db.due(
                    cursor,
                    CLAIM_SCAN.saturating_sub(examined).min(MAX_PAGE_ROWS),
                )?;
                stats[1] += 1;
                for found in &keys {
                    examined += 1;
                    stats[2] = examined;
                    let id: [u8; 16] = ops::key_part(found, 9)?;
                    let job = db.require(&id)?;
                    if !db.claimable(&job, at, &eligible)? {
                        continue;
                    }
                    let mut job = job;
                    if matches!(job.state.as_str(), "queued" | "failed") {
                        job.state = "processing".to_owned();
                    }
                    job.lease_id = Some(random_id()?);
                    job.lease_until = deadline;
                    job.updated_at = at;
                    db.validate_group_single(&job)?;
                    db.validate_transaction(&job)?;
                    let mut changes =
                        db.stage(std::slice::from_ref(&job), "processing lease acquired")?;
                    changes.push(Change::Delete(layout::CLAIM_CURSOR_KEY));
                    db.0.commit(changes)?;
                    (next, saved) = (Some(None), true);
                    return Ok(Some(job));
                }
                cursor = keys.last().copied().or(cursor);
                if !more || examined >= CLAIM_SCAN {
                    let position = more.then_some(cursor).flatten();
                    if checkpoint {
                        db.save_cursor(layout::CLAIM_CURSOR_KEY, position)?;
                    }
                    (next, saved) = (Some(position), checkpoint);
                    return Ok(None);
                }
            }
        });
        self.claim_pass = stats;
        if next.is_some() {
            self.claim_cursor = next;
        }
        if saved {
            self.claim_saved = Some(at);
        }
        outcome
    }

    pub fn renew(&mut self, id: &str, lease_id: &str, at: u64, ttl: u64) -> Result<Job> {
        self.mutate(|db| {
            let mut job = db.get(id)?.ok_or_else(|| "unknown job".to_owned())?;
            if job.lease_id.as_deref() != Some(lease_id) || job.lease_until <= at || ttl == 0 {
                return Err("expired or invalid processing lease".to_owned());
            }
            job.lease_until = job.lease_until.max(
                at.checked_add(ttl)
                    .ok_or_else(|| "lease duration too large".to_owned())?,
            );
            job.updated_at = at;
            db.commit_jobs(vec![job.clone()], ops::RENEWED)?;
            Ok(job)
        })
    }

    pub fn release_lease(&mut self, id: &str, lease_id: &str) -> Result<Job> {
        self.mutate(|db| {
            let mut job = db.get(id)?.ok_or_else(|| "unknown job".to_owned())?;
            if job.lease_id.as_deref() != Some(lease_id) {
                return Err("stale processing lease".to_owned());
            }
            job.lease_id = None;
            job.lease_until = 0;
            job.updated_at = now();
            db.commit_jobs(vec![job.clone()], "processing lease released")?;
            Ok(job)
        })
    }

    pub fn cancel(&mut self, id: &str) -> Result<Job> {
        self.mutate(|db| {
            let mut job = db.get(id)?.ok_or_else(|| "unknown job".to_owned())?;
            if job.shared_upgrade.is_some() {
                return db.cancel_group(id);
            }
            if job.state == "ready" {
                return Err("a completed job cannot be cancelled".to_owned());
            }
            job.state = "cancelled".to_owned();
            job.lease_id = None;
            job.lease_until = 0;
            job.updated_at = now();
            db.commit_jobs(vec![job.clone()], "request cancelled")?;
            Ok(job)
        })
    }

    pub fn retry(&mut self, id: &str) -> Result<Job> {
        self.mutate(|db| {
            let mut job = db.get(id)?.ok_or_else(|| "unknown job".to_owned())?;
            if job.shared_upgrade.is_some() {
                return db.retry_group(id);
            }
            if !matches!(job.state.as_str(), "failed" | "cancelled") {
                return Err("only a failed or cancelled job can be retried".to_owned());
            }
            if job.upgrade_parent.is_some() {
                if !db.upgrade_eligible(&job)? {
                    return Err("the upgrade parent is obsolete or no longer monitored".to_owned());
                }
                if db.other_pending(&job.request.media_key(), Some(&job.id))? {
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
                // The detached transfer is recorded in this same transaction, so a
                // stop before its retirement leaves it discoverable.
                if let Some(previous) = job.download_id.take()
                    && valid_transfer_id(&previous)
                    && !job.retiring_transfers.contains(&previous)
                {
                    if job.retiring_transfers.len() >= MAX_RETIRING_TRANSFERS {
                        return Err(
                            "previous transfers still await retirement; restart or retry later"
                                .to_owned(),
                        );
                    }
                    job.retiring_transfers.push(previous);
                }
                job.release = None;
                job.acquisition_url = None;
                job.files.clear();
                job.progress = 0.0;
            }
            db.commit_jobs(vec![job.clone()], "request retried")?;
            Ok(job)
        })
    }

    /// Transfers detached by an automatic retry whose retirement is not yet
    /// durable, as `(request id, transfer id)` pairs, at most `limit` requests.
    pub fn pending_retirements(&mut self, limit: usize) -> Result<Vec<(String, String)>> {
        self.read(Vec::new(), |db| {
            let mut pending = Vec::new();
            for found in db.keys(&[layout::T_RETIRE], limit.clamp(1, MAX_PAGE_ROWS))? {
                let job = db.require(&ops::key_part(&found, 1)?)?;
                for id in &job.retiring_transfers {
                    pending.push((job.id.clone(), id.clone()));
                }
            }
            Ok(pending)
        })
    }

    /// Records that one detached transfer was paused, or deliberately kept for
    /// another owner. Unknown requests and transfers are already settled.
    pub fn complete_retirement(&mut self, job_id: &str, transfer_id: &str) -> Result<()> {
        self.mutate(|db| {
            let Some(mut job) = db.get(job_id)? else {
                return Ok(());
            };
            let before = job.retiring_transfers.len();
            job.retiring_transfers.retain(|id| id != transfer_id);
            if job.retiring_transfers.len() == before {
                return Ok(());
            }
            job.updated_at = now();
            db.commit_jobs(vec![job], "previous transfer retired")
        })
    }

    /// Checks whether a completed acquisition can become the current import.
    /// Callers can turn a rejected promotion into a durable processing failure
    /// before relinquishing their lease. Validation repeats this check at commit.
    pub fn check_ready_promotion(&mut self, job: &Job) -> Result<()> {
        self.read((), |db| db.check_ready_promotion(job))
    }

    pub(crate) fn check_group_replacement(
        &mut self,
        parents: &[Job],
        file: &crate::pack::SharedFile,
        requests: &[Request],
        release: &RecordedRelease,
    ) -> Result<Vec<Job>> {
        self.read(Vec::new(), |db| {
            db.check_group_replacement(parents, file, requests, release)
        })
    }

    pub(crate) fn submit_group_replacement(
        &mut self,
        parents: &[Job],
        file: crate::pack::SharedFile,
        requests: Vec<Request>,
        release: RecordedRelease,
    ) -> Result<Vec<Job>> {
        self.mutate(|db| db.submit_group_replacement(parents, file, requests, release))
    }

    pub(crate) fn set_group_baseline(
        &mut self,
        id: &str,
        release: RecordedRelease,
    ) -> Result<Vec<Job>> {
        self.mutate(|db| db.set_group_baseline(id, release))
    }
}

impl JobLookup for Store {
    fn lookup(&mut self, id: &str) -> Result<Option<Job>> {
        self.get(id)
    }
}

impl Db<'_> {
    /// Whether a queued job may run now, under the rules the claim applies.
    fn claimable(
        &mut self,
        job: &Job,
        at: u64,
        eligible: &impl Fn(&Job) -> Result<bool>,
    ) -> Result<bool> {
        if !layout::open(job) || layout::wake(job) > at || !eligible(job)? {
            return Ok(false);
        }
        if let Some(file) = &job.shared_file {
            let group = layout::short("group", &[&file.id()]);
            let mut after = None;
            loop {
                let page = self.scan(
                    &[&[layout::T_GROUP][..], &group].concat(),
                    after,
                    MAX_PAGE_ROWS,
                )?;
                for (_, value) in &page.entries {
                    let member = self.require(&ops::array(value)?)?;
                    if member.lease_id.is_some() && member.lease_until > at {
                        return Ok(false);
                    }
                }
                after = page.next;
                if after.is_none() {
                    break;
                }
            }
        }
        let current_parent = |db: &mut Self, id: &str| -> Result<bool> {
            Ok(db.tip(&layout::id_bytes(id)?)? == Some((true, true)))
        };
        if let Some(parent) = &job.upgrade_parent
            && !current_parent(self, parent)?
        {
            return Ok(false);
        }
        if let Some(lineage) = &job.shared_upgrade {
            for parent in &lineage.parent_jobs {
                if !current_parent(self, parent)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}
