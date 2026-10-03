//! Original BitTorrent implementation using only Rust's standard library.
#[path = "torrent/discovery.rs"]
mod discovery;
#[path = "torrent/metainfo.rs"]
mod metainfo;
#[path = "torrent/wire.rs"]
mod wire;

use crate::{Result, bencode::Value};
use metainfo::{BLOCK, MAX_META, Meta, confined};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub struct DownloadConfig {
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub listen_port: u16,
    pub seed: bool,
    pub dht: bool,
    pub pex: bool,
    pub max_active: usize,
}
#[derive(Clone, Debug)]
pub struct DownloadStatus {
    pub id: String,
    pub progress: f64,
    pub ready: bool,
    pub files: Vec<PathBuf>,
    pub message: String,
}
#[derive(Clone)]
struct Source {
    original: String,
    v1: Option<[u8; 20]>,
    v2: Option<[u8; 32]>,
    trackers: Vec<String>,
    peers: Vec<SocketAddr>,
    meta: Option<Meta>,
}
#[derive(Default)]
pub(crate) struct TransferCounters {
    pub downloaded: AtomicU64,
    pub uploaded: AtomicU64,
    pub verified: AtomicU64,
}
struct AnnounceState {
    started: bool,
    completed: bool,
    stopped: bool,
    in_flight: bool,
    next: Instant,
    retry_after: Instant,
}
impl Default for AnnounceState {
    fn default() -> Self {
        Self {
            started: false,
            completed: false,
            stopped: false,
            in_flight: false,
            next: Instant::now(),
            retry_after: Instant::now(),
        }
    }
}
struct Job {
    source: Source,
    meta: Option<Arc<Meta>>,
    status: DownloadStatus,
    running: bool,
    paused: bool,
    failed: bool,
    cancel: Arc<AtomicBool>,
    peers: Vec<SocketAddr>,
    tracker_peers: BTreeSet<SocketAddr>,
    counters: Arc<TransferCounters>,
    announcements: BTreeMap<String, AnnounceState>,
    announce_hash: Option<[u8; 20]>,
    dht_next: Instant,
    dht_in_flight: bool,
}
type Jobs = Arc<Mutex<BTreeMap<String, Job>>>;
pub struct Client {
    config: DownloadConfig,
    jobs: Jobs,
    stop: Arc<AtomicBool>,
    manager: Option<JoinHandle<()>>,
    listener: Option<JoinHandle<()>>,
}

fn hex(input: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(input.len() * 2);
    for b in input {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}
fn decode_hex<const N: usize>(input: &str) -> Result<[u8; N]> {
    if input.len() != N * 2 {
        return Err("Invalid torrent hash length".into());
    }
    let mut out = [0; N];
    for (i, c) in input.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let digit = |b: u8| -> Result<u8> {
            match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                b'A'..=b'F' => Ok(b - b'A' + 10),
                _ => Err("Invalid torrent hash".into()),
            }
        };
        out[i] = digit(c[0])? * 16 + digit(c[1])?;
    }
    Ok(out)
}
fn decode_base32(input: &str) -> Result<[u8; 20]> {
    if input.len() != 32 {
        return Err("Invalid base32 hash".into());
    }
    let mut out = [0; 20];
    let mut acc = 0u32;
    let mut bits = 0;
    let mut n = 0;
    for b in input.bytes() {
        let v = match b.to_ascii_uppercase() {
            b'A'..=b'Z' => b.to_ascii_uppercase() - b'A',
            b'2'..=b'7' => b - b'2' + 26,
            _ => return Err("Invalid base32 hash".into()),
        };
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out[n] = (acc >> bits) as u8;
            n += 1;
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}
fn url_decode(s: &str) -> Result<String> {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                if i + 2 >= b.len() {
                    return Err("Truncated URL escape".into());
                }
                let n = decode_hex::<1>(
                    std::str::from_utf8(&b[i + 1..i + 3]).map_err(|_| "Invalid escape")?,
                )?;
                out.push(n[0]);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            v => {
                out.push(v);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "Magnet parameter is not valid UTF-8".into())
}

impl Source {
    fn parse(source: &str) -> Result<Self> {
        if source.len() > 64 * 1024 {
            return Err("Torrent source is too long".into());
        }
        let mut result = Self {
            original: source.to_owned(),
            v1: None,
            v2: None,
            trackers: Vec::new(),
            peers: Vec::new(),
            meta: None,
        };
        if let Some(query) = source.strip_prefix("magnet:?") {
            for part in query.split('&') {
                let Some((key, value)) = part.split_once('=') else {
                    continue;
                };
                let value = url_decode(value)?;
                match key {
                    "xt" => {
                        if let Some(hash) = value.strip_prefix("urn:btih:") {
                            let hash = if hash.len() == 32 {
                                decode_base32(hash)?
                            } else {
                                decode_hex(hash)?
                            };
                            if result.v1.is_some_and(|v| v != hash) {
                                return Err("Magnet contains conflicting v1 hashes".into());
                            }
                            result.v1 = Some(hash);
                        } else if let Some(hash) = value.strip_prefix("urn:btmh:1220") {
                            let hash = decode_hex(hash)?;
                            if result.v2.is_some_and(|v| v != hash) {
                                return Err("Magnet contains conflicting v2 hashes".into());
                            }
                            result.v2 = Some(hash);
                        }
                    }
                    "tr" => {
                        if result.trackers.len() >= 256 {
                            return Err("Too many magnet trackers".into());
                        }
                        if value.starts_with("http://")
                            || value.starts_with("https://")
                            || value.starts_with("udp://")
                        {
                            result.trackers.push(value);
                        }
                    }
                    "x.pe" => {
                        if result.peers.len() >= 256 {
                            return Err("Too many magnet peers".into());
                        }
                        result.peers.extend(
                            value
                                .to_socket_addrs()
                                .map_err(|_| "Invalid x.pe address")?
                                .take(4),
                        );
                    }
                    _ => {}
                }
            }
            if result.v1.is_none() && result.v2.is_none() {
                return Err("Magnet has no valid btih or btmh hash".into());
            }
        } else {
            let encoded = if source.starts_with("http://") || source.starts_with("https://") {
                let r = crate::net::HttpClient::new()
                    .with_max_body(MAX_META)
                    .get(source)?;
                if r.status != 200 {
                    return Err("Metadata download was rejected".into());
                }
                r.body
            } else {
                let file = File::open(source).map_err(|_| "Torrent file not found")?;
                let mut data = Vec::new();
                file.take((MAX_META + 1) as u64)
                    .read_to_end(&mut data)
                    .map_err(|_| "Could not read torrent file")?;
                data
            };
            let meta = Meta::parse(&encoded)?;
            result.v1 = meta.v1;
            result.v2 = meta.v2;
            result.trackers = meta.trackers.clone();
            result.meta = Some(meta);
        }
        if result.v1.is_some_and(|h| h == [0; 20]) || result.v2.is_some_and(|h| h[..20] == [0; 20])
        {
            return Err("Zero torrent hash is not allowed".into());
        }
        Ok(result)
    }
    fn id(&self) -> String {
        if let Some(hash) = self.v2 {
            hex(&hash)
        } else {
            hex(&self.v1.unwrap_or([0; 20]))
        }
    }
    fn wire_hash(&self) -> [u8; 20] {
        self.v1.unwrap_or_else(|| {
            let mut h = [0; 20];
            if let Some(v) = self.v2 {
                h.copy_from_slice(&v[..20]);
            }
            h
        })
    }
}

fn mkdir_private(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|_| "Could not create download directory")?;
    let meta = fs::symlink_metadata(path).map_err(|_| "Directory not found")?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("Invalid download directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Could not set download permissions")?;
    }
    Ok(())
}
/// Suspend a persisted native job while no Client owns its state directory.
/// The caller must hold the application's offline store lock.
pub fn pause_persisted(config: &DownloadConfig, id: &str) -> Result<()> {
    if !(id.len() == 40 || id.len() == 64) || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Invalid download identity".into());
    }
    atomic_write(&config.state_dir.join(format!("{id}.paused")), b"paused\n")
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("State path has no parent directory")?;
    mkdir_private(parent)?;
    if fs::symlink_metadata(path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        return Err("Special state files are not allowed".into());
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".tmp-{}-{nonce}", std::process::id()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| "Could not create state file")?;
        file.write_all(data)
            .and_then(|()| file.sync_all())
            .map_err(|_| "Could not write state file")?;
        fs::rename(&temporary, path).map_err(|_| "Could not publish state file")?;
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| "Could not synchronize directory")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn peer_id() -> Result<[u8; 20]> {
    let random = crate::crypto::random_bytes::<12>()?;
    let mut id = [0; 20];
    id[..8].copy_from_slice(b"-MY0600-");
    id[8..].copy_from_slice(&random);
    Ok(id)
}

impl Client {
    pub fn open(mut config: DownloadConfig) -> Result<Self> {
        if config.max_active == 0 || config.max_active > 128 {
            return Err("Invalid active download count".into());
        }
        mkdir_private(&config.state_dir)?;
        mkdir_private(&config.data_dir)?;
        let listener = TcpListener::bind(("0.0.0.0", config.listen_port))
            .map_err(|_| "BitTorrent port unavailable")?;
        config.listen_port = listener
            .local_addr()
            .map_err(|_| "BitTorrent address unavailable")?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|_| "Could not configure the BitTorrent port")?;
        let mut map = BTreeMap::new();
        for entry in fs::read_dir(&config.state_dir)
            .map_err(|_| "Could not read BitTorrent state")?
        {
            let entry = entry.map_err(|_| "Could not read BitTorrent state")?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = name.strip_suffix(".torrent") else {
                continue;
            };
            if !((id.len() == 40 || id.len() == 64) && id.bytes().all(|b| b.is_ascii_hexdigit())) {
                continue;
            }
            let kind = entry
                .file_type()
                .map_err(|_| "Persisted metadata is inaccessible")?;
            if !kind.is_file() || kind.is_symlink() {
                return Err("Special persisted metadata files are not allowed".into());
            }
            let file = File::open(entry.path())
                .map_err(|_| "Could not read persisted metadata")?;
            let mut encoded = Vec::new();
            file.take((MAX_META + 1) as u64)
                .read_to_end(&mut encoded)
                .map_err(|_| "Could not read persisted metadata")?;
            let meta = Meta::parse(&encoded)?;
            if ![meta.v1.map(|h| hex(&h)), meta.v2.map(|h| hex(&h))]
                .into_iter()
                .flatten()
                .any(|h| h == id)
            {
                return Err("BitTorrent state hash mismatch".into());
            }
            let original_path = config.state_dir.join(format!("{id}.source"));
            let original = if original_path.exists() {
                let m = fs::symlink_metadata(&original_path)
                    .map_err(|_| "Persisted source is inaccessible")?;
                if !m.is_file() || m.file_type().is_symlink() {
                    return Err("Special persisted source files are not allowed".into());
                }
                let mut value = String::new();
                File::open(&original_path)
                    .and_then(|f| f.take(64 * 1024 + 1).read_to_string(&mut value))
                    .map_err(|_| "Could not read persisted source")?;
                if value.len() > 64 * 1024 {
                    return Err("Persisted source is too large".into());
                }
                value
            } else {
                entry.path().to_string_lossy().into_owned()
            };
            let mut source = if original.starts_with("magnet:?") {
                Source::parse(&original)?
            } else {
                Source {
                    original: original.clone(),
                    v1: meta.v1,
                    v2: meta.v2,
                    trackers: meta.trackers.clone(),
                    peers: Vec::new(),
                    meta: None,
                }
            };
            if source.v1.is_some_and(|h| Some(h) != meta.v1)
                || source.v2.is_some_and(|h| Some(h) != meta.v2)
            {
                return Err("Persisted source and metadata are incompatible".into());
            }
            source.v1 = meta.v1;
            source.v2 = meta.v2;
            let status = DownloadStatus {
                id: id.to_owned(),
                progress: 0.0,
                ready: false,
                files: meta
                    .files
                    .iter()
                    .filter(|f| !f.padding)
                    .map(|f| config.data_dir.join(id).join(&f.path))
                    .collect(),
                message: "Verifying persisted data".into(),
            };
            map.insert(
                id.to_owned(),
                Job {
                    source,
                    meta: Some(Arc::new(meta)),
                    status,
                    running: false,
                    paused: config.state_dir.join(format!("{id}.paused")).exists(),
                    failed: false,
                    cancel: Arc::new(AtomicBool::new(false)),
                    peers: Vec::new(),
                    tracker_peers: BTreeSet::new(),
                    counters: Arc::new(TransferCounters::default()),
                    announcements: BTreeMap::new(),
                    announce_hash: None,
                    dht_next: Instant::now(),
                    dht_in_flight: false,
                },
            );
        }
        // Persisted magnets without metadata are also durable queue entries.
        for entry in fs::read_dir(&config.state_dir)
            .map_err(|_| "Could not read BitTorrent queue")?
        {
            let entry = entry.map_err(|_| "Could not read BitTorrent queue")?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = name.strip_suffix(".source") else {
                continue;
            };
            if map.contains_key(id)
                || !((id.len() == 40 || id.len() == 64)
                    && id.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                continue;
            }
            let kind = entry
                .file_type()
                .map_err(|_| "Persisted source is inaccessible")?;
            if !kind.is_file() || kind.is_symlink() {
                return Err("Special persisted source files are not allowed".into());
            }
            let mut original = String::new();
            File::open(entry.path())
                .and_then(|f| f.take(64 * 1024 + 1).read_to_string(&mut original))
                .map_err(|_| "Invalid persisted source")?;
            if !original.starts_with("magnet:?") {
                return Err("Missing persisted metadata for this source".into());
            }
            let source = Source::parse(&original)?;
            if source.id() != id {
                return Err("BitTorrent queue hash mismatch".into());
            }
            map.insert(
                id.to_owned(),
                Job {
                    meta: None,
                    source,
                    status: DownloadStatus {
                        id: id.to_owned(),
                        progress: 0.0,
                        ready: false,
                        files: Vec::new(),
                        message: "Resuming metadata discovery".into(),
                    },
                    running: false,
                    paused: config.state_dir.join(format!("{id}.paused")).exists(),
                    failed: false,
                    cancel: Arc::new(AtomicBool::new(false)),
                    peers: Vec::new(),
                    tracker_peers: BTreeSet::new(),
                    counters: Arc::new(TransferCounters::default()),
                    announcements: BTreeMap::new(),
                    announce_hash: None,
                    dht_next: Instant::now(),
                    dht_in_flight: false,
                },
            );
        }
        for job in map.values_mut() {
            if job.paused {
                job.status.message = "Download paused".into();
            }
        }
        let jobs = Arc::new(Mutex::new(map));
        let stop = Arc::new(AtomicBool::new(false));
        let id = peer_id()?;
        let manager = {
            let jobs = jobs.clone();
            let stop = stop.clone();
            let cfg = config.clone();
            thread::spawn(move || manager_loop(cfg, jobs, stop, id))
        };
        let listener = {
            let jobs = jobs.clone();
            let stop = stop.clone();
            let cfg = config.clone();
            thread::spawn(move || listen_loop(listener, cfg, jobs, stop, id))
        };
        Ok(Self {
            config,
            jobs,
            stop,
            manager: Some(manager),
            listener: Some(listener),
        })
    }
    pub fn ensure(&self, source: &str) -> Result<DownloadStatus> {
        let mut source = Source::parse(source)?;
        let id = source.id();
        let mut jobs = self.jobs.lock().map_err(|_| "BitTorrent state lock is poisoned")?;
        if let Some(job) = jobs.values_mut().find(|job| {
            source.v1.is_some_and(|h| job.source.v1 == Some(h))
                || source.v2.is_some_and(|h| job.source.v2 == Some(h))
        }) {
            if (source.v1.is_some() && job.source.v1.is_none())
                || (source.v2.is_some() && job.source.v2.is_none())
            {
                return Err("Additional torrent alias has not been verified".into());
            }
            if source
                .v1
                .is_some_and(|h| job.source.v1.is_some_and(|v| v != h))
                || source
                    .v2
                    .is_some_and(|h| job.source.v2.is_some_and(|v| v != h))
            {
                return Err("Incompatible torrent hash alias".into());
            }
            resume_job(&self.config, job)?;
            return Ok(job.status.clone());
        }
        atomic_write(
            &self.config.state_dir.join(format!("{id}.source")),
            source.original.as_bytes(),
        )?;
        if let Some(meta) = &source.meta {
            atomic_write(
                &self.config.state_dir.join(format!("{id}.torrent")),
                &meta.encoded,
            )?;
        }
        let status = DownloadStatus {
            id: id.clone(),
            progress: 0.0,
            ready: false,
            files: source
                .meta
                .as_ref()
                .map(|m| {
                    m.files
                        .iter()
                        .filter(|f| !f.padding)
                        .map(|f| self.config.data_dir.join(&id).join(&f.path))
                        .collect()
                })
                .unwrap_or_default(),
            message: "Download queued".into(),
        };
        let metadata = source.meta.take().map(Arc::new);
        jobs.insert(
            id,
            Job {
                meta: metadata,
                source,
                status: status.clone(),
                running: false,
                paused: false,
                failed: false,
                cancel: Arc::new(AtomicBool::new(false)),
                peers: Vec::new(),
                tracker_peers: BTreeSet::new(),
                counters: Arc::new(TransferCounters::default()),
                announcements: BTreeMap::new(),
                announce_hash: None,
                dht_next: Instant::now(),
                dht_in_flight: false,
            },
        );
        Ok(status)
    }
    /// Pause a native transfer without removing its verified files or metadata.
    pub fn cancel(&self, id: &str) -> Result<()> {
        let mut jobs = self.jobs.lock().map_err(|_| "BitTorrent state lock is poisoned")?;
        let job = jobs.get_mut(id).ok_or("Unknown download")?;
        job.paused = true;
        job.cancel.store(true, Ordering::Release);
        job.status.message = "Download paused".into();
        atomic_write(
            &self.config.state_dir.join(format!("{id}.paused")),
            b"paused\n",
        )?;
        job.paused = true;
        job.cancel.store(true, Ordering::Release);
        job.status.message = "Download paused".into();
        Ok(())
    }
    /// Resume using the cached, authenticated identity; never reload a source URL.
    pub fn resume(&self, id: &str) -> Result<DownloadStatus> {
        let mut jobs = self.jobs.lock().map_err(|_| "BitTorrent state lock is poisoned")?;
        let job = jobs.get_mut(id).ok_or("Unknown download")?;
        resume_job(&self.config, job)?;
        Ok(job.status.clone())
    }
    pub fn check(&self, id: &str) -> Result<DownloadStatus> {
        let jobs = self.jobs.lock().map_err(|_| "BitTorrent state lock is poisoned")?;
        let job = jobs.get(id).ok_or("Unknown download")?;
        if job.failed {
            return Err(job.status.message.clone());
        }
        Ok(job.status.clone())
    }
    pub fn statuses(&self) -> Result<Vec<DownloadStatus>> {
        Ok(self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?
            .values()
            .map(|j| j.status.clone())
            .collect())
    }
    pub fn transfer_stats(&self, id: &str) -> Result<(u64, u64)> {
        let jobs = self.jobs.lock().map_err(|_| "BitTorrent state lock is poisoned")?;
        let counters = &jobs.get(id).ok_or("Unknown download")?.counters;
        Ok((
            counters.downloaded.load(Ordering::Relaxed),
            counters.uploaded.load(Ordering::Relaxed),
        ))
    }
    pub fn listen_port(&self) -> u16 {
        self.config.listen_port
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.manager.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
    }
}

fn resume_job(config: &DownloadConfig, job: &mut Job) -> Result<()> {
    if job.paused || job.failed {
        let marker = config.state_dir.join(format!("{}.paused", job.status.id));
        if fs::symlink_metadata(&marker).is_ok() {
            fs::remove_file(marker).map_err(|_| "Could not resume download")?;
            File::open(&config.state_dir)
                .and_then(|f| f.sync_all())
                .map_err(|_| "Could not synchronize resumed state")?;
        }
        job.paused = false;
        job.failed = false;
        job.cancel = Arc::new(AtomicBool::new(false));
        job.status.message = if job.status.ready {
            "Download verified and available"
        } else {
            "Download resumed"
        }
        .into();
    }
    Ok(())
}

struct AnnounceTask {
    id: String,
    url: String,
    hash: [u8; 20],
    left: u64,
    downloaded: u64,
    uploaded: u64,
    event: discovery::TrackerEvent,
}
fn select_announce(config: &DownloadConfig, jobs: &Jobs) -> Option<AnnounceTask> {
    let mut jobs = jobs.lock().ok()?;
    let now = Instant::now();
    for (id, job) in jobs.iter_mut() {
        let mut urls: BTreeSet<String> = job.source.trackers.iter().cloned().collect();
        if let Some(meta) = &job.meta {
            urls.extend(meta.trackers.iter().cloned());
        }
        for url in urls.into_iter().take(256) {
            job.announcements.entry(url).or_default();
        }
        let hash = *job
            .announce_hash
            .get_or_insert_with(|| job.source.wire_hash());
        let downloaded = job.counters.downloaded.load(Ordering::Relaxed);
        let uploaded = job.counters.uploaded.load(Ordering::Relaxed);
        let total = job
            .meta
            .as_ref()
            .map(|m| {
                if m.v1 == Some(hash) {
                    m.total
                } else {
                    m.files
                        .iter()
                        .filter(|f| !f.padding)
                        .map(|f| f.length)
                        .sum()
                }
            })
            .unwrap_or(1);
        let left = if job.status.ready {
            0
        } else {
            total.saturating_sub(job.counters.verified.load(Ordering::Relaxed))
        };
        for (url, state) in &mut job.announcements {
            if state.in_flight || now < state.retry_after {
                continue;
            }
            let inactive = job.paused || job.failed;
            let event = if inactive {
                if state.started && !state.stopped {
                    Some(discovery::TrackerEvent::Stopped)
                } else {
                    None
                }
            } else if job.status.ready
                && !config.seed
                && (state.stopped || (!state.started && downloaded == 0))
            {
                None
            } else if !state.started || state.stopped {
                if now >= state.next {
                    Some(discovery::TrackerEvent::Started)
                } else {
                    None
                }
            } else if job.status.ready && downloaded > 0 && !state.completed {
                Some(discovery::TrackerEvent::Completed)
            } else if job.status.ready && !config.seed {
                if !state.stopped {
                    Some(discovery::TrackerEvent::Stopped)
                } else {
                    None
                }
            } else if now >= state.next {
                Some(discovery::TrackerEvent::None)
            } else {
                None
            };
            if let Some(event) = event {
                state.in_flight = true;
                return Some(AnnounceTask {
                    id: id.clone(),
                    url: url.clone(),
                    hash,
                    left,
                    downloaded,
                    uploaded,
                    event,
                });
            }
        }
    }
    None
}
fn announce(task: AnnounceTask, port: u16, peer_id: [u8; 20], jobs: &Jobs) {
    let request = discovery::TrackerRequest {
        hash: &task.hash,
        peer_id: &peer_id,
        port,
        left: task.left,
        downloaded: task.downloaded,
        uploaded: task.uploaded,
        event: task.event,
    };
    let result = discovery::tracker(&task.url, &request);
    if let Ok(mut jobs) = jobs.lock()
        && let Some(job) = jobs.get_mut(&task.id)
        && let Some(state) = job.announcements.get_mut(&task.url)
    {
        state.in_flight = false;
        match result {
            Ok(response) => {
                state.retry_after = Instant::now();
                state.next = Instant::now() + response.interval;
                match task.event {
                    discovery::TrackerEvent::Started => {
                        state.started = true;
                        state.stopped = false;
                    }
                    discovery::TrackerEvent::Completed => state.completed = true,
                    discovery::TrackerEvent::Stopped => {
                        state.stopped = true;
                        state.next = Instant::now();
                    }
                    discovery::TrackerEvent::None => {}
                }
                if task.event != discovery::TrackerEvent::Stopped {
                    merge_peers(&mut job.tracker_peers, response.peers);
                }
            }
            Err(_) => {
                state.next = Instant::now() + Duration::from_secs(30);
                state.retry_after = state.next;
            }
        }
    }
}

struct DhtTask {
    id: String,
    hash: [u8; 20],
    cancel: Arc<AtomicBool>,
}
fn select_seed_dht(config: &DownloadConfig, jobs: &Jobs) -> Option<DhtTask> {
    if !config.dht || !config.seed {
        return None;
    }
    let mut jobs = jobs.lock().ok()?;
    let now = Instant::now();
    for (id, job) in jobs.iter_mut() {
        if !job.status.ready
            || job.paused
            || job.failed
            || job.dht_in_flight
            || now < job.dht_next
            || job.meta.as_ref().is_none_or(|m| m.private)
        {
            continue;
        }
        job.dht_in_flight = true;
        return Some(DhtTask {
            id: id.clone(),
            hash: job.announce_hash.unwrap_or_else(|| job.source.wire_hash()),
            cancel: job.cancel.clone(),
        });
    }
    None
}
fn announce_seed_dht(task: DhtTask, port: u16, jobs: &Jobs) {
    let result = discovery::dht_lookup(&task.hash, port, true, &task.cancel, &[]);
    if let Ok(mut jobs) = jobs.lock()
        && let Some(job) = jobs.get_mut(&task.id)
    {
        job.dht_in_flight = false;
        job.dht_next = Instant::now() + Duration::from_secs(600);
        if !job.paused
            && Arc::ptr_eq(&job.cancel, &task.cancel)
            && let Ok(peers) = result
        {
            let mut known: BTreeSet<_> = job.peers.iter().copied().collect();
            merge_peers(&mut known, peers);
            job.peers = known.into_iter().collect();
        }
    }
}

fn manager_loop(config: DownloadConfig, jobs: Jobs, stop: Arc<AtomicBool>, peer_id: [u8; 20]) {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    let mut announcements: Vec<JoinHandle<()>> = Vec::new();
    while !stop.load(Ordering::Acquire) {
        let mut n = 0;
        while n < workers.len() {
            if workers[n].is_finished() {
                let handle = workers.swap_remove(n);
                let _ = handle.join();
            } else {
                n += 1;
            }
        }
        if workers.len() < config.max_active {
            let selected = jobs.lock().ok().and_then(|mut jobs| {
                let id = jobs
                    .iter()
                    .find(|(_, j)| !j.running && !j.paused && !j.failed && !j.status.ready)
                    .map(|(id, _)| id.clone())?;
                let job = jobs.get_mut(&id)?;
                job.running = true;
                job.cancel.store(false, Ordering::Release);
                Some((id, job.cancel.clone()))
            });
            if let Some((id, st)) = selected {
                let cfg = config.clone();
                let js = jobs.clone();
                workers.push(thread::spawn(move || {
                    if let Err(error) = download(&cfg, &js, &st, &id, &peer_id)
                        && let Ok(mut jobs) = js.lock()
                        && let Some(job) = jobs.get_mut(&id)
                        && !job.paused
                        && !st.load(Ordering::Acquire)
                        && Arc::ptr_eq(&job.cancel, &st)
                    {
                        job.failed = true;
                        job.status.message = error;
                    }
                    if let Ok(mut jobs) = js.lock()
                        && let Some(job) = jobs.get_mut(&id)
                    {
                        job.running = false;
                    }
                }));
            }
        }
        let mut index = 0;
        while index < announcements.len() {
            if announcements[index].is_finished() {
                let handle = announcements.swap_remove(index);
                let _ = handle.join();
            } else {
                index += 1;
            }
        }
        if announcements.len() < config.max_active.min(4)
            && let Some(task) = select_announce(&config, &jobs)
        {
            let jobs = jobs.clone();
            let port = config.listen_port;
            announcements.push(thread::spawn(move || announce(task, port, peer_id, &jobs)));
        }
        if announcements.len() < config.max_active.min(4)
            && let Some(task) = select_seed_dht(&config, &jobs)
        {
            let jobs = jobs.clone();
            let port = config.listen_port;
            announcements.push(thread::spawn(move || announce_seed_dht(task, port, &jobs)));
        }
        thread::sleep(Duration::from_millis(30));
    }
    if let Ok(jobs) = jobs.lock() {
        for job in jobs.values() {
            job.cancel.store(true, Ordering::Release);
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
    for announcement in announcements {
        let _ = announcement.join();
    }
}
fn update(
    context: (&Jobs, &AtomicBool),
    id: &str,
    meta: Option<&Meta>,
    progress: f64,
    ready: bool,
    message: &str,
    config: &DownloadConfig,
) -> Result<()> {
    let (jobs, stop) = context;
    let mut jobs = jobs.lock().map_err(|_| "BitTorrent state lock is poisoned")?;
    let job = jobs.get_mut(id).ok_or("Download no longer exists")?;
    if job.paused || stop.load(Ordering::Acquire) || !std::ptr::eq(&*job.cancel, stop) {
        return Err("Download interrupted".into());
    }
    job.status.progress = progress;
    job.status.ready = ready;
    job.status.message = message.to_owned();
    if let Some(meta) = meta {
        if job.meta.is_none() || ready {
            job.meta = Some(Arc::new(meta.clone()));
            job.status.files = meta
                .files
                .iter()
                .filter(|f| !f.padding)
                .map(|f| config.data_dir.join(id).join(&f.path))
                .collect();
        }
        job.source.v1 = meta.v1;
        job.source.v2 = meta.v2;
    }
    Ok(())
}

fn merge_peers(addresses: &mut BTreeSet<SocketAddr>, peers: impl IntoIterator<Item = SocketAddr>) {
    for peer in peers {
        if addresses.len() >= 1024 {
            break;
        }
        if peer.port() != 0 && !peer.ip().is_unspecified() && !peer.ip().is_multicast() {
            addresses.insert(peer);
        }
    }
}
fn download(
    config: &DownloadConfig,
    jobs: &Jobs,
    stop: &AtomicBool,
    id: &str,
    peer_id: &[u8; 20],
) -> Result<()> {
    let (mut source, mut meta, counters) = {
        let jobs = jobs.lock().map_err(|_| "BitTorrent state lock is poisoned")?;
        let job = jobs.get(id).ok_or("Download no longer exists")?;
        (
            job.source.clone(),
            job.meta.as_ref().map(|m| (**m).clone()),
            job.counters.clone(),
        )
    };
    let base = config.data_dir.join(id);
    mkdir_private(&base)?;
    let mut have = Vec::new();
    let mut scanned = false;
    let mut addresses: BTreeSet<SocketAddr> = source.peers.iter().copied().collect();
    let hash = source.wire_hash();
    let v2_wire = source.v1.is_none();
    let mut next_dht = Instant::now();
    while !stop.load(Ordering::Acquire) {
        if let Some(m) = &meta {
            if !scanned {
                prepare_files(m, &base, stop)?;
                have = (0..m.count())
                    .map(|i| {
                        if stop.load(Ordering::Acquire) {
                            return false;
                        }
                        if m.pieces[i].is_none() && m.v2_pieces[i].is_none() {
                            return false;
                        }
                        read_piece(m, &base, i)
                            .map(|data| m.verify(i, &data))
                            .unwrap_or(false)
                    })
                    .collect();
                counters.verified.store(
                    have.iter()
                        .enumerate()
                        .filter(|(_, v)| **v)
                        .map(|(i, _)| m.wire_piece_size(i, v2_wire) as u64)
                        .sum(),
                    Ordering::Relaxed,
                );
                scanned = true;
            }
            let done = have.iter().filter(|v| **v).count();
            let progress = if m.count() == 0 {
                1.0
            } else {
                done as f64 / m.count() as f64
            };
            update(
                (jobs, stop),
                id,
                Some(m),
                progress,
                false,
                "Download in progress",
                config,
            )?;
            if done == m.count() {
                let mut complete = meta.take().ok_or("Missing metadata")?;
                authenticate_v2(&mut complete, &base, stop)?;
                sync_files(&complete, &base, stop)?;
                persist_meta(config, id, &complete)?;
                update(
                    (jobs, stop),
                    id,
                    Some(&complete),
                    1.0,
                    true,
                    "Download verified and available",
                    config,
                )?;
                return Ok(());
            }
        }
        let private = meta.as_ref().is_some_and(|m| m.private);
        let known = meta.is_some();
        if let Ok(jobs) = jobs.lock()
            && let Some(job) = jobs.get(id)
        {
            merge_peers(&mut addresses, job.tracker_peers.iter().copied());
        }
        // Tracker/direct hints suppress discovery until privacy is known.
        if Instant::now() >= next_dht
            && config.dht
            && !private
            && (known || (source.trackers.is_empty() && source.peers.is_empty()))
        {
            next_dht = Instant::now() + Duration::from_secs(60);
            if let Ok(peers) = discovery::dht_lookup(&hash, config.listen_port, known, stop, &[]) {
                merge_peers(&mut addresses, peers);
            }
        }
        let peers: Vec<_> = addresses.iter().copied().take(1000).collect();
        if let Ok(mut jobs) = jobs.lock()
            && let Some(job) = jobs.get_mut(id)
        {
            job.peers = peers.clone();
        }
        let mut advanced = false;
        for address in peers {
            if stop.load(Ordering::Relaxed) {
                return Err("Download interrupted".into());
            }
            let mut peer = match wire::Peer::connect(
                address,
                &hash,
                peer_id,
                wire::PeerSettings {
                    meta: meta.as_ref(),
                    port: config.listen_port,
                    pex: config.pex,
                    v2_wire,
                    counters: counters.clone(),
                },
            ) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if meta.is_none() {
                match peer.metadata(source.v1, source.v2, stop) {
                    Ok(m) => {
                        let untrusted_private =
                            m.private && source.trackers.is_empty() && source.peers.is_empty();
                        source.v1 = m.v1;
                        source.v2 = m.v2;
                        source.trackers.extend(m.trackers.clone());
                        persist_meta(config, id, &m)?;
                        prepare_files(&m, &base, stop)?;
                        have = (0..m.count())
                            .map(|i| {
                                if stop.load(Ordering::Acquire)
                                    || (m.pieces[i].is_none() && m.v2_pieces[i].is_none())
                                {
                                    return false;
                                }
                                read_piece(&m, &base, i)
                                    .map(|d| m.verify(i, &d))
                                    .unwrap_or(false)
                            })
                            .collect();
                        counters.verified.store(
                            have.iter()
                                .enumerate()
                                .filter(|(_, v)| **v)
                                .map(|(i, _)| m.wire_piece_size(i, v2_wire) as u64)
                                .sum(),
                            Ordering::Relaxed,
                        );
                        peer.pex_allowed = config.pex && !m.private;
                        if peer.extensions {
                            wire::write_message(
                                &mut peer.stream,
                                20,
                                &wire::extended_handshake(Some(&m), config.listen_port, config.pex),
                                Some(stop),
                            )?;
                        }
                        meta = Some(m);
                        scanned = true;
                        advanced = true;
                        if untrusted_private {
                            addresses.clear();
                            break;
                        }
                    }
                    Err(_) => continue,
                }
            }
            let Some(m) = meta.as_mut() else { continue };
            let mut completed = have.iter().filter(|v| **v).count();
            for (i, present) in have.iter_mut().enumerate() {
                if *present {
                    continue;
                }
                if !peer.bitfield.is_empty()
                    && (i / 8 >= peer.bitfield.len()
                        || peer.bitfield[i / 8] & (0x80 >> (i % 8)) == 0)
                {
                    continue;
                }
                match peer.fetch_piece(m, i, stop) {
                    Ok(data) => {
                        write_piece(m, &base, i, &data)?;
                        *present = true;
                        counters
                            .verified
                            .fetch_add(m.wire_piece_size(i, v2_wire) as u64, Ordering::Relaxed);
                        advanced = true;
                        completed += 1;
                        update(
                            (jobs, stop),
                            id,
                            Some(m),
                            completed as f64 / m.count() as f64,
                            false,
                            "Download in progress",
                            config,
                        )?;
                    }
                    Err(_) => break,
                }
            }
            if config.pex && !m.private {
                merge_peers(&mut addresses, peer.discovered);
            }
            if have.iter().all(|v| *v) {
                break;
            }
        }
        if !advanced {
            update(
                (jobs, stop),
                id,
                meta.as_ref(),
                if have.is_empty() {
                    0.0
                } else {
                    have.iter().filter(|v| **v).count() as f64 / have.len() as f64
                },
                false,
                "Searching for available peers",
                config,
            )?;
            for _ in 0..20 {
                if stop.load(Ordering::Relaxed) {
                    return Err("Download interrupted".into());
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err("Download interrupted".into())
}

fn prepare_files(meta: &Meta, base: &Path, stop: &AtomicBool) -> Result<()> {
    for f in &meta.files {
        if stop.load(Ordering::Acquire) {
            return Err("Download interrupted".into());
        }
        if f.padding {
            continue;
        }
        let path = confined(base, &f.path)?;
        if let Some(parent) = path.parent() {
            let relative = parent
                .strip_prefix(base)
                .map_err(|_| "File path is outside the download directory")?;
            let mut current = base.to_path_buf();
            for part in relative.components() {
                current.push(part);
                if current.exists() {
                    let m = fs::symlink_metadata(&current)
                        .map_err(|_| "Media directory is inaccessible")?;
                    if !m.is_dir() || m.file_type().is_symlink() {
                        return Err("Special media directories are not allowed".into());
                    }
                } else {
                    fs::create_dir(&current)
                        .map_err(|_| "Could not create media directory")?;
                }
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|_| "Could not open media file")?;
        if !file
            .metadata()
            .map_err(|_| "Media file is inaccessible")?
            .is_file()
        {
            return Err("Special media files are not allowed".into());
        }
        if file
            .metadata()
            .map_err(|_| "Media file is inaccessible")?
            .len()
            != f.length
        {
            file.set_len(f.length)
                .map_err(|_| "Could not resize media file")?;
        }
    }
    Ok(())
}
fn read_piece(meta: &Meta, base: &Path, index: usize) -> Result<Vec<u8>> {
    let start = index as u64 * meta.piece_length as u64;
    let mut data = vec![0; meta.piece_size(index)];
    let end = start + data.len() as u64;
    for f in &meta.files {
        if f.padding || f.offset >= end || f.offset + f.length <= start {
            continue;
        }
        let a = start.max(f.offset);
        let b = end.min(f.offset + f.length);
        let path = confined(base, &f.path)?;
        let mut file = File::open(path).map_err(|_| "Could not read media file")?;
        file.seek(SeekFrom::Start(a - f.offset))
            .and_then(|_| file.read_exact(&mut data[(a - start) as usize..(b - start) as usize]))
            .map_err(|_| "Truncated media read")?;
    }
    Ok(data)
}
fn write_piece(meta: &Meta, base: &Path, index: usize, data: &[u8]) -> Result<()> {
    if !meta.verify(index, data) {
        return Err("Piece has not been authenticated".into());
    }
    let start = index as u64 * meta.piece_length as u64;
    let end = start + data.len() as u64;
    for f in &meta.files {
        if f.offset >= end || f.offset + f.length <= start {
            continue;
        }
        let a = start.max(f.offset);
        let b = end.min(f.offset + f.length);
        let block = &data[(a - start) as usize..(b - start) as usize];
        if f.padding {
            if block.iter().any(|b| *b != 0) {
                return Err("Torrent padding is not zero".into());
            }
            continue;
        }
        let path = confined(base, &f.path)?;
        let mut file = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|_| "Could not write media file")?;
        file.seek(SeekFrom::Start(a - f.offset))
            .and_then(|_| file.write_all(block))
            .map_err(|_| "Could not write media file")?;
    }
    Ok(())
}
fn sync_files(meta: &Meta, base: &Path, stop: &AtomicBool) -> Result<()> {
    for f in &meta.files {
        if stop.load(Ordering::Acquire) {
            return Err("Download interrupted".into());
        }
        if !f.padding {
            let path = confined(base, &f.path)?;
            OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|f| f.sync_all())
                .map_err(|_| "Could not synchronize media files")?;
        }
    }
    File::open(base)
        .and_then(|f| f.sync_all())
        .map_err(|_| "Could not synchronize download")?;
    Ok(())
}
fn authenticate_v2(meta: &mut Meta, base: &Path, stop: &AtomicBool) -> Result<()> {
    if meta.v2.is_none() {
        return Ok(());
    }
    for f in &meta.files {
        let Some(root) = f.root else { continue };
        let count = f.length.div_ceil(meta.piece_length as u64) as usize;
        let start = (f.offset / meta.piece_length as u64) as usize;
        let mut hashes = Vec::with_capacity(count);
        for i in 0..count {
            if stop.load(Ordering::Acquire) {
                return Err("Download interrupted".into());
            }
            let data = read_piece(meta, base, start + i)?;
            let n = f
                .length
                .saturating_sub(i as u64 * meta.piece_length as u64)
                .min(meta.piece_length as u64) as usize;
            let blocks = if count == 1 {
                n.div_ceil(BLOCK).next_power_of_two()
            } else {
                meta.piece_length / BLOCK
            };
            hashes.push(metainfo::merkle_data(&data[..n], blocks));
        }
        if metainfo::merkle_hashes(&hashes, metainfo::zero_hash(meta.piece_length / BLOCK)) != root
        {
            return Err("Downloaded data does not match its v2 root".into());
        }
        for (i, h) in hashes.into_iter().enumerate() {
            meta.v2_pieces[start + i] = Some(h);
        }
    }
    Ok(())
}
fn persist_meta(config: &DownloadConfig, id: &str, meta: &Meta) -> Result<()> {
    let mut top = BTreeMap::new();
    if !meta.trackers.is_empty() {
        top.insert(
            b"announce-list".to_vec(),
            Value::List(
                meta.trackers
                    .iter()
                    .map(|t| Value::List(vec![Value::Bytes(t.as_bytes().to_vec())]))
                    .collect(),
            ),
        );
    }
    let mut layers = BTreeMap::new();
    for f in &meta.files {
        let Some(root) = f.root else { continue };
        let n = f.length.div_ceil(meta.piece_length as u64) as usize;
        if n <= 1 {
            continue;
        }
        let start = (f.offset / meta.piece_length as u64) as usize;
        if meta.v2_pieces[start..start + n].iter().all(Option::is_some) {
            let bytes: Vec<u8> = meta.v2_pieces[start..start + n]
                .iter()
                .flatten()
                .flat_map(|h| h.iter().copied())
                .collect();
            layers.insert(root.to_vec(), Value::Bytes(bytes));
        }
    }
    if !layers.is_empty() {
        top.insert(b"piece layers".to_vec(), Value::Dict(layers));
    }
    let mut encoded = b"d".to_vec();
    let mut inserted = false;
    for (key, value) in top {
        if key.as_slice() > b"info" && !inserted {
            encoded.extend_from_slice(b"4:info");
            encoded.extend_from_slice(&meta.info);
            inserted = true;
        }
        encoded.extend(crate::bencode::encode(&Value::Bytes(key)));
        encoded.extend(crate::bencode::encode(&value));
    }
    if !inserted {
        encoded.extend_from_slice(b"4:info");
        encoded.extend_from_slice(&meta.info);
    }
    encoded.push(b'e');
    atomic_write(&config.state_dir.join(format!("{id}.torrent")), &encoded)
}

fn listen_loop(
    listener: TcpListener,
    config: DownloadConfig,
    jobs: Jobs,
    stop: Arc<AtomicBool>,
    peer_id: [u8; 20],
) {
    let mut peers: Vec<JoinHandle<()>> = Vec::new();
    while !stop.load(Ordering::Acquire) {
        let mut i = 0;
        while i < peers.len() {
            if peers[i].is_finished() {
                let h = peers.swap_remove(i);
                let _ = h.join();
            } else {
                i += 1;
            }
        }
        match listener.accept() {
            Ok((stream, _)) if peers.len() < 32 => {
                let cfg = config.clone();
                let js = jobs.clone();
                let st = stop.clone();
                peers.push(thread::spawn(move || {
                    let _ = serve_peer(stream, &cfg, &js, &st, &peer_id);
                }));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(_) => break,
        }
    }
    for peer in peers {
        let _ = peer.join();
    }
}
fn serve_peer(
    mut stream: TcpStream,
    config: &DownloadConfig,
    jobs: &Jobs,
    stop: &AtomicBool,
    peer_id: &[u8; 20],
) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|_| "Could not configure TCP")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|_| "Could not configure TCP")?;
    let mut incoming = [0; 68];
    wire::read_exact_deadline(
        &mut stream,
        &mut incoming,
        Some(stop),
        Instant::now() + Duration::from_secs(5),
    )?;
    if incoming[0] != 19 || &incoming[1..20] != b"BitTorrent protocol" {
        return Err("Invalid incoming handshake".into());
    }
    let mut hash = [0; 20];
    hash.copy_from_slice(&incoming[28..48]);
    let (id, meta, ready, known_peers, cancel, counters) = {
        let jobs = jobs.lock().map_err(|_| "BitTorrent state lock is poisoned")?;
        jobs.iter()
            .find_map(|(id, job)| {
                if job.paused {
                    return None;
                }
                let meta = job.meta.as_ref()?;
                let matches = meta.v1 == Some(hash) || meta.v2.is_some_and(|v| v[..20] == hash);
                if matches {
                    Some((
                        id.clone(),
                        meta.clone(),
                        job.status.ready,
                        job.peers.clone(),
                        job.cancel.clone(),
                        job.counters.clone(),
                    ))
                } else {
                    None
                }
            })
            .ok_or("Unknown incoming torrent")?
    };
    let v2_wire = meta.v1 != Some(hash);
    let mut response = [0; 68];
    response[0] = 19;
    response[1..20].copy_from_slice(b"BitTorrent protocol");
    response[25] = 0x10;
    response[27] = 0;
    response[28..48].copy_from_slice(&hash);
    response[48..].copy_from_slice(peer_id);
    wire::write_all_deadline(
        &mut stream,
        &response,
        Some(&cancel),
        Instant::now() + Duration::from_secs(5),
    )?;
    let extensions = incoming[25] & 0x10 != 0;
    if extensions {
        wire::write_message(
            &mut stream,
            20,
            &wire::extended_handshake(Some(&meta), config.listen_port, config.pex),
            Some(&cancel),
        )?;
    }
    let count = meta.count();
    let mut bitfield = vec![0; count.div_ceil(8)];
    if ready && config.seed {
        for i in 0..count {
            bitfield[i / 8] |= 0x80 >> (i % 8);
        }
    }
    wire::write_message(&mut stream, 5, &bitfield, Some(&cancel))?;
    let mut metadata_id = None;
    let mut pex_id = None;
    let base = config.data_dir.join(&id);
    let mut cached_piece: Option<(usize, Vec<u8>)> = None;
    while !stop.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
        let (id, payload) = wire::read_message(&mut stream, Some(&cancel))?;
        match id {
            2 if ready && config.seed => wire::write_message(&mut stream, 1, &[], Some(&cancel))?,
            6 if ready && config.seed => {
                if payload.len() != 12 {
                    return Err("Invalid block request".into());
                }
                let index =
                    u32::from_be_bytes(payload[..4].try_into().map_err(|_| "Invalid index")?)
                        as usize;
                let offset =
                    u32::from_be_bytes(payload[4..8].try_into().map_err(|_| "Invalid offset")?)
                        as usize;
                let length =
                    u32::from_be_bytes(payload[8..12].try_into().map_err(|_| "Invalid length")?)
                        as usize;
                if index >= count
                    || length == 0
                    || length > BLOCK
                    || offset
                        .checked_add(length)
                        .is_none_or(|v| v > meta.wire_piece_size(index, v2_wire))
                {
                    return Err("Block request is out of bounds".into());
                }
                if cached_piece.as_ref().is_none_or(|(i, _)| *i != index) {
                    let data = read_piece(&meta, &base, index)?;
                    if !meta.verify(index, &data) {
                        return Err("Seeding data has been modified".into());
                    }
                    cached_piece = Some((index, data));
                }
                let data = &cached_piece.as_ref().ok_or("Missing seeding piece")?.1;
                let mut block = payload[..8].to_vec();
                block.extend_from_slice(&data[offset..offset + length]);
                wire::write_message(&mut stream, 7, &block, Some(&cancel))?;
                counters
                    .uploaded
                    .fetch_add(length as u64, Ordering::Relaxed);
            }
            20 if payload.first() == Some(&0) => {
                let v = crate::bencode::parse(&payload[1..])?;
                if let Some(m) = discovery::value_field(&v, b"m") {
                    if let Some(Value::Int(n)) = discovery::value_field(m, b"ut_metadata") {
                        metadata_id = u8::try_from(*n).ok().filter(|v| *v != 0);
                    }
                    if let Some(Value::Int(n)) = discovery::value_field(m, b"ut_pex") {
                        pex_id = u8::try_from(*n).ok().filter(|v| *v != 0);
                    }
                }
                if config.pex
                    && !meta.private
                    && let Some(ext) = pex_id
                {
                    let mut compact = Vec::new();
                    for peer in known_peers.iter().filter(|p| p.is_ipv4()).take(100) {
                        if let std::net::IpAddr::V4(ip) = peer.ip() {
                            compact.extend_from_slice(&ip.octets());
                            compact.extend_from_slice(&peer.port().to_be_bytes());
                        }
                    }
                    let value = discovery::value_dict(&[
                        (b"added", Value::Bytes(compact.clone())),
                        (b"added.f", Value::Bytes(vec![0; compact.len() / 6])),
                    ]);
                    let mut pex = vec![ext];
                    pex.extend(crate::bencode::encode(&value));
                    wire::write_message(&mut stream, 20, &pex, Some(&cancel))?;
                }
            }
            20 if payload.first() == Some(&wire::EXT_METADATA) => {
                let v = crate::bencode::parse(&payload[1..])?;
                let Some(Value::Int(kind)) = discovery::value_field(&v, b"msg_type") else {
                    continue;
                };
                if *kind != 0 {
                    continue;
                }
                let Some(Value::Int(piece)) = discovery::value_field(&v, b"piece") else {
                    return Err("Missing metadata index".into());
                };
                let piece = usize::try_from(*piece).map_err(|_| "Negative metadata index")?;
                let ext = metadata_id.ok_or("Metadata extension has not been negotiated")?;
                if piece >= meta.info.len().div_ceil(BLOCK) {
                    return Err("Metadata index is out of bounds".into());
                }
                let header = discovery::value_dict(&[
                    (b"msg_type", Value::Int(1)),
                    (b"piece", Value::Int(piece as i64)),
                    (b"total_size", Value::Int(meta.info.len() as i64)),
                ]);
                let mut reply = vec![ext];
                reply.extend(crate::bencode::encode(&header));
                let start = piece * BLOCK;
                reply.extend_from_slice(&meta.info[start..(start + BLOCK).min(meta.info.len())]);
                wire::write_message(&mut stream, 20, &reply, Some(&cancel))?;
            }
            21 if ready => match wire::hash_response(&meta, &payload, Some(&base)) {
                Ok(response) => wire::write_message(&mut stream, 22, &response, Some(&cancel))?,
                Err(_) => wire::write_message(&mut stream, 23, &payload, Some(&cancel))?,
            },
            _ => {}
        }
    }
    Ok(())
}
