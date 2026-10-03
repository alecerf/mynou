//! Verified transactional journal without an external database engine.
//!
//! A transaction is acknowledged only after the journal is synchronized.
//! An incomplete final record is recoverable; a complete record with an
//! incorrect digest produces an explicit error.

use crate::Result;
use crate::crypto::sha256;
use crate::json::{self, Value};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const JOURNAL_MAGIC: &[u8; 8] = b"MYNOUJ01";
const SNAPSHOT_MAGIC: &[u8; 8] = b"MYNOUS01";
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
    events: VecDeque<Event>,
    sequence: u64,
    chain: [u8; 32],
    poisoned: bool,
    journal_bytes: u64,
    next_compaction_at: u64,
    maintenance_error: Option<String>,
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
    map.get(key)
        .ok_or_else(|| format!("missing field: {key}"))
}

fn string(map: &BTreeMap<String, Value>, key: &str) -> Result<String> {
    match field(map, key)? {
        Value::String(value) => Ok(value.clone()),
        _ => Err(format!("expected a string: {key}")),
    }
}

fn integer(map: &BTreeMap<String, Value>, key: &str) -> Result<u64> {
    match field(map, key)? {
        Value::String(value) => value
            .parse()
            .map_err(|_| format!("invalid integer: {key}")),
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
            return Err(
                "a request cannot contain both a source file and a source URL"
                    .to_owned(),
            );
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

    pub fn to_json(&self) -> Value {
        object([
            ("kind", Value::String(self.kind.clone())),
            ("title", Value::String(self.title.clone())),
            ("year", Value::Number(self.year as f64)),
            ("season", Value::Number(self.season as f64)),
            ("episode", Value::Number(self.episode as f64)),
            ("source_path", optional_string(&self.source_path)),
            ("source_url", optional_string(&self.source_url)),
            ("tmdb_id", self.tmdb_id.map_or(Value::Null, number)),
        ])
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
            tmdb_id: optional_u64(map, "tmdb_id")?,
        };
        request.validate()?;
        Ok(request)
    }
}

impl Job {
    pub fn to_json(&self) -> Value {
        let list =
            |values: &[String]| Value::Array(values.iter().cloned().map(Value::String).collect());
        object([
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
        ])
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let map = fields(value)?;
        let progress = match field(map, "progress")? {
            Value::Number(value) if value.is_finite() && (0.0..=1.0).contains(value) => *value,
            _ => return Err("invalid job progress".to_owned()),
        };
        let job = Self {
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
        };
        if job.id.len() != 32
            || !job.id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || job.key != job.request.canonical_key()
            || job.state.is_empty()
            || job.state.len() > 64
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
            return Err(
                "storage files cannot share hard links".to_owned(),
            );
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot set storage permissions: {error}"))?;
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
                return Err(
                    "storage requires a clean directory path without traversal".to_owned(),
                );
            }
            reject_symlinks(directory)?;
            create_private_directory(directory)?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("cannot set storage permissions: {error}"))?;
            let lock = secure_file(&directory.join(".lock"))?;
            lock.try_lock().map_err(|error| {
                format!("storage is already open or cannot be locked: {error}")
            })?;
            let journal = secure_file(&directory.join("journal.bin"))?;
            sync_directory(directory)?;
            let mut store = Self {
                directory: directory.to_owned(),
                _lock: lock,
                journal,
                jobs: BTreeMap::new(),
                by_key: BTreeMap::new(),
                events: VecDeque::new(),
                sequence: 0,
                chain: [0; 32],
                poisoned: false,
                journal_bytes: 0,
                next_compaction_at: COMPACTION_BYTES,
                maintenance_error: None,
            };
            store.load_snapshot()?;
            store.replay()?;
            Ok(store)
        }
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.jobs.get(id).cloned()
    }

    /// A maintenance error does not revoke an acknowledged, synchronized write.
    /// Writes with uncertain outcomes block further storage operations.
    pub fn maintenance_error(&self) -> Option<&str> {
        self.maintenance_error.as_deref()
    }

    pub fn list(&self) -> Vec<Job> {
        let mut jobs: Vec<_> = self.jobs.values().cloned().collect();
        jobs.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        jobs
    }

    pub fn events(&self, id: &str) -> Vec<Event> {
        self.events
            .iter()
            .filter(|event| id.is_empty() || event.job_id == id)
            .cloned()
            .collect()
    }

    pub fn submit(&mut self, request: Request) -> Result<Job> {
        request.validate()?;
        let key = request.canonical_key();
        if let Some(id) = self.by_key.get(&key) {
            return self
                .get(id)
                .ok_or_else(|| "inconsistent job index".to_owned());
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
        {
            return Err("job identity is immutable".to_owned());
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
        self.commit(job, &message)
    }

    pub fn claim(&mut self, at: u64, ttl: u64) -> Result<Option<Job>> {
        if ttl == 0 {
            return Err("lease duration must be positive".to_owned());
        }
        let deadline = at
            .checked_add(ttl)
            .ok_or_else(|| "lease duration too large".to_owned())?;
        let candidate = self
            .jobs
            .values()
            .filter(|job| {
                !matches!(job.state.as_str(), "ready" | "cancelled")
                    && (job.state != "failed" || job.next_attempt_at > 0)
                    && job.next_attempt_at <= at
                    && (job.lease_id.is_none() || job.lease_until <= at)
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
        if !matches!(job.state.as_str(), "failed" | "cancelled") {
            return Err("only a failed or cancelled job can be retried".to_owned());
        }
        job.state = "queued".to_owned();
        job.lease_id = None;
        job.lease_until = 0;
        job.last_error = None;
        job.next_attempt_at = 0;
        job.updated_at = now();
        self.commit(job.clone(), "request retried")?;
        Ok(job)
    }

    fn commit(&mut self, job: Job, message: &str) -> Result<()> {
        if self.poisoned {
            return Err(
                "storage unavailable after a write error; reopen storage"
                    .to_owned(),
            );
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
        let payload = json::stringify(&object([
            ("job", job.to_json()),
            ("event", event.to_json()),
        ]))
        .into_bytes();
        if payload.len() > MAX_RECORD {
            return Err("transaction too large".to_owned());
        }
        let mut frame = Vec::with_capacity(116 + payload.len());
        frame.extend_from_slice(JOURNAL_MAGIC);
        frame.extend_from_slice(&sequence.to_le_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&self.chain);
        frame.extend_from_slice(&sha256(&frame));
        frame.extend_from_slice(&payload);
        let digest = sha256(&frame);
        frame.extend_from_slice(&digest);
        let journal_bytes = self.journal_bytes.checked_add(frame.len() as u64)
            .filter(|bytes| *bytes <= MAX_JOURNAL_BYTES)
            .ok_or_else(|| "journal full after a maintenance error; repair storage before continuing".to_owned())?;
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
        self.by_key.insert(job.key.clone(), job.id.clone());
        self.jobs.insert(job.id.clone(), job);
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
        bytes.extend_from_slice(SNAPSHOT_MAGIC);
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
        if &bytes[..8] != SNAPSHOT_MAGIC {
            return Err("invalid snapshot format".to_owned());
        }
        let encoded_length =
            u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| "invalid length")?);
        if encoded_length > MAX_SNAPSHOT as u64 || encoded_length.checked_add(48) != Some(length) {
            return Err("corrupt snapshot size".to_owned());
        }
        let payload_length =
            usize::try_from(encoded_length).map_err(|_| "snapshot too large")?;
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
            self.by_key.insert(job.key.clone(), job.id.clone());
            if keys.insert(job.key.clone(), job.id.clone()).is_some()
                || self.jobs.insert(job.id.clone(), job).is_some()
            {
                return Err("duplicate identities in snapshot".to_owned());
            }
        }
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
            if &header[..8] != JOURNAL_MAGIC {
                return Err(format!("corrupt journal at byte {offset}"));
            }
            if sha256(&header[..52]).as_slice() != &header[52..84] {
                return Err(format!("corrupt journal header at byte {offset}"));
            }
            let sequence = u64::from_le_bytes(
                header[8..16]
                    .try_into()
                    .map_err(|_| "invalid sequence")?,
            );
            let payload_length = u32::from_le_bytes(
                header[16..20]
                    .try_into()
                    .map_err(|_| "invalid length")?,
            ) as usize;
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
                let job = Job::from_json(field(map, "job")?)?;
                let event = Event::from_json(field(map, "event")?)?;
                if event.id != sequence
                    || event.job_id != job.id
                    || event.state != job.state
                    || event.at != job.updated_at
                {
                    return Err("inconsistent journal transaction".to_owned());
                }
                if let Some(current) = self.jobs.get(&job.id) {
                    if current.key != job.key
                        || current.request != job.request
                        || current.created_at != job.created_at
                    {
                        return Err("identity changed in journal".to_owned());
                    }
                } else if self.by_key.contains_key(&job.key) {
                    return Err("duplicate request in journal".to_owned());
                }
                self.by_key.insert(job.key.clone(), job.id.clone());
                self.jobs.insert(job.id.clone(), job);
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
        Ok(())
    }

    fn truncate_tail(&mut self, length: u64) -> Result<()> {
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
        assert!(
            Store::open(&directory.0)
                .err()
                .unwrap()
                .contains("corrupt")
        );
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
        assert!(
            Store::open(&directory.0)
                .err()
                .unwrap()
                .contains("corrupt")
        );
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
        assert!(
            Store::open(&alias)
                .err()
                .unwrap()
                .contains("hard links")
        );
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
