//! Original BitTorrent implementation using only Rust's standard library.
#[path = "torrent/control.rs"]
mod control;
#[path = "torrent/cross_seed.rs"]
mod cross_seed;
#[path = "torrent/discovery.rs"]
mod discovery;
#[path = "torrent/index.rs"]
mod index;
#[path = "torrent/inspection.rs"]
mod inspection;
#[path = "torrent/metainfo.rs"]
mod metainfo;
#[path = "torrent/parallel.rs"]
mod parallel;
#[path = "torrent/queue.rs"]
mod queue;
#[path = "torrent/selection.rs"]
mod selection;
#[path = "torrent/transport.rs"]
pub mod transport;
#[path = "torrent/wire.rs"]
pub(crate) mod wire;
use control::RateGate;
pub use control::{FilePriority, TorrentControl, TransferPolicy};
pub use cross_seed::{
    CandidateLayout, CrossSeedRequest, MAX_CROSS_SEED_METADATA, ReuseSource, candidate_layout,
};
pub use inspection::{MetadataFile, TorrentMetadata, inspect_metadata};
pub use selection::{FileSelection, SelectionUpdate};

use crate::{Result, bencode::Value};
use metainfo::{BLOCK, MAX_META, Meta, confined};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
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
    /// User download queue limit; zero imposes no user queue limit.
    pub max_active: usize,
    /// User seed queue limit; zero imposes no user queue limit.
    pub max_seeding: usize,
    pub max_peers: usize,
}
impl DownloadConfig {
    /// Execution remains bounded by the existing global peer resource budget.
    /// A zero user queue limit does not allocate one thread per retained item.
    fn download_slots(&self) -> usize {
        match self.max_active {
            0 => parallel::CONNECTION_BUDGET,
            n => n.min(parallel::CONNECTION_BUDGET),
        }
    }
}
#[derive(Clone, Debug)]
pub struct DownloadStatus {
    pub id: String,
    pub progress: f64,
    pub ready: bool,
    /// All retained file interests are verified, independently of whole-torrent readiness.
    pub selected_ready: bool,
    pub files: Vec<PathBuf>,
    /// Only paths published after piece verification, file-root checks and disk synchronization.
    pub available_files: Vec<PathBuf>,
    pub message: String,
}
#[derive(Clone)]
struct Source {
    original: String,
    v1: Option<[u8; 20]>,
    v2: Option<[u8; 32]>,
    trackers: Vec<String>,
    // Unresolved authorities: persisted magnets are parsed without network I/O.
    peers: Vec<String>,
    meta: Option<Meta>,
}
#[derive(Default)]
pub(crate) struct TransferCounters {
    pub downloaded: AtomicU64,
    pub uploaded: AtomicU64,
    pub verified: AtomicU64,
}
/// One connection of a running download, as the transfer details show it.
struct PeerView {
    address: SocketAddr,
    /// False until the handshake completes.
    connected: bool,
    /// None until the peer says which pieces it has; some never do.
    progress: Option<f64>,
    downloading: bool,
    received_bytes: u64,
}
/// A peer this transfer seeds to, kept while its incoming connection lasts.
#[derive(Clone)]
struct SeedingPeer {
    address: SocketAddr,
    /// None until the peer says which pieces it has.
    progress: Option<f64>,
    interested: bool,
    sent_bytes: u64,
    last_sent: Option<Instant>,
}
/// Keys of `Job::seeding_peers`, unique across transfers.
static SEEDING_PEER_KEY: AtomicU64 = AtomicU64::new(0);
/// Takes a seeding peer off its transfer when the connection ends.
struct SeedingRegistration<'a> {
    jobs: &'a Jobs,
    id: String,
    key: u64,
}
impl Drop for SeedingRegistration<'_> {
    fn drop(&mut self) {
        if let Ok(mut jobs) = self.jobs.lock()
            && let Some(job) = jobs.get_mut(&self.id)
        {
            job.seeding_peers.remove(&self.key);
        }
    }
}
struct AnnounceState {
    started: bool,
    completed: bool,
    stopped: bool,
    in_flight: bool,
    next: Instant,
    retry_after: Instant,
    /// What the last answer said, for the transfer details.
    last_at: u64,
    last_error: Option<String>,
    seeders: Option<u64>,
    leechers: Option<u64>,
    peers: usize,
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
            last_at: 0,
            last_error: None,
            seeders: None,
            leechers: None,
            peers: 0,
        }
    }
}
struct Job {
    persistence_failed: AtomicBool,
    retiring: bool,
    /// Set by `Client::remove`: once its activity drains, the manager forgets
    /// the transfer instead of keeping it, and deletes its files when true.
    removing: Option<bool>,
    /// The peers of the running download, refreshed by its coordinator.
    live_peers: Vec<PeerView>,
    /// Addresses the download knows of, from trackers, DHT and PEX.
    known_peers: usize,
    /// Incoming peers served from verified data, by registration key.
    seeding_peers: BTreeMap<u64, SeedingPeer>,
    /// Where the running download takes incoming peers of its torrent.
    inbound: Option<std::sync::mpsc::SyncSender<wire::Inbound>>,
    admitted: bool,
    needs_rehash: bool,
    seed_queued: bool,
    index: Option<Arc<Mutex<index::TransferIndex>>>,
    cross_seed: Option<Arc<cross_seed::CrossSeed>>,
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
    control: TorrentControl,
    piece_order: Arc<Vec<usize>>,
    download_gate: Arc<RateGate>,
    upload_gate: Arc<RateGate>,
    seed_clock: Option<(Instant, u64)>,
    seed_partial: Duration,
    upload_reserved: u64,
    saved_control: Vec<u8>,
}
struct PolicyRuntime {
    policy: TransferPolicy,
    seed_queue: queue::SeedQueue,
    download: Arc<RateGate>,
    upload: Arc<RateGate>,
    /// Every outbound peer, tracker and DHT exchange of the client.
    network: Arc<dyn transport::Network>,
}
pub(crate) fn add_payload(counter: &AtomicU64, bytes: u64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(bytes))
    });
}
type AdmissionGate = Arc<dyn Fn(&str) -> Result<bool> + Send + Sync>;
type Jobs = Arc<Mutex<BTreeMap<String, Job>>>;
pub struct Client {
    admission: Option<AdmissionGate>,
    config: DownloadConfig,
    index: Arc<Mutex<index::TransferIndex>>,
    jobs: Jobs,
    stop: Arc<AtomicBool>,
    manager: Option<JoinHandle<()>>,
    listener: Option<JoinHandle<()>>,
    policy: Arc<PolicyRuntime>,
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
pub(crate) fn url_decode(s: &str) -> Result<String> {
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
    fn parse(network: &dyn transport::Network, source: &str) -> Result<Self> {
        Self::parse_with_deadline(network, source, None)
    }
    fn parse_with_deadline(
        network: &dyn transport::Network,
        source: &str,
        deadline: Option<Instant>,
    ) -> Result<Self> {
        if source.starts_with("magnet:?") {
            return Self::parse_magnet_with_deadline(source, deadline);
        }
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Err("Torrent metadata deadline exceeded".into());
        }
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
        let encoded = if source.starts_with("http://") || source.starts_with("https://") {
            let mut client = network.http().with_max_body(MAX_META);
            if let Some(end) = deadline {
                client = client.with_timeout(
                    end.checked_duration_since(Instant::now())
                        .filter(|remaining| !remaining.is_zero())
                        .ok_or("Torrent metadata deadline exceeded")?
                        .min(Duration::from_secs(20)),
                );
            }
            let r = client.get(source)?;
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
        if result.v1.is_some_and(|h| h == [0; 20]) || result.v2.is_some_and(|h| h[..20] == [0; 20])
        {
            return Err("Zero torrent hash is not allowed".into());
        }
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Err("Torrent metadata deadline exceeded".into());
        }
        Ok(result)
    }
    // Restored magnet state has no metadata-fetch branch. Keeping this parser
    // separate makes the local-state trust boundary explicit.
    fn parse_magnet_with_deadline(source: &str, deadline: Option<Instant>) -> Result<Self> {
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Err("Torrent metadata deadline exceeded".into());
        }
        if source.len() > 64 * 1024 {
            return Err("Torrent source is too long".into());
        }
        let query = source
            .strip_prefix("magnet:?")
            .ok_or("Persisted source must be a magnet")?;
        let mut result = Self {
            original: source.to_owned(),
            v1: None,
            v2: None,
            trackers: Vec::new(),
            peers: Vec::new(),
            meta: None,
        };
        for part in query.split('&') {
            if deadline.is_some_and(|end| Instant::now() >= end) {
                return Err("Torrent metadata deadline exceeded".into());
            }
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
                    // Validate syntax only. Resolution belongs to the active
                    // transport, never to restore, admission or offline parsing.
                    let (_, port) = value.rsplit_once(':').ok_or("Invalid x.pe address")?;
                    if port.is_empty()
                        || !port.bytes().all(|byte| byte.is_ascii_digit())
                        || value.contains(['/', '?', '#'])
                    {
                        return Err("Invalid x.pe address".into());
                    }
                    crate::net::parse_url(&format!("http://{value}"))
                        .map_err(|_| "Invalid x.pe address")?;
                    result.peers.push(value);
                }
                _ => {}
            }
        }
        if result.v1.is_none() && result.v2.is_none() {
            return Err("Magnet has no valid btih or btmh hash".into());
        }
        if result.v1.is_some_and(|h| h == [0; 20]) || result.v2.is_some_and(|h| h[..20] == [0; 20])
        {
            return Err("Zero torrent hash is not allowed".into());
        }
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Err("Torrent metadata deadline exceeded".into());
        }
        Ok(result)
    }
    /// Resolve only when an active operation has its explicit network context.
    fn peer_addresses(
        &self,
        network: &dyn transport::Network,
        deadline: Instant,
    ) -> Result<Vec<SocketAddr>> {
        if Instant::now() >= deadline {
            return Err("Torrent metadata deadline exceeded".into());
        }
        let mut addresses = Vec::new();
        for authority in &self.peers {
            if Instant::now() >= deadline {
                return Err("Torrent metadata deadline exceeded".into());
            }
            let resolved = network
                .resolve(authority, deadline)
                .map_err(|_| "Could not resolve magnet peer")?;
            if Instant::now() >= deadline {
                return Err("Torrent metadata deadline exceeded".into());
            }
            addresses.extend(resolved.into_iter().take(4).take(256 - addresses.len()));
            if addresses.len() == 256 {
                break;
            }
        }
        Ok(addresses)
    }
    fn matches_identity(&self, expected: &str) -> bool {
        self.v1.is_some_and(|hash| hex(&hash) == expected)
            || self.v2.is_some_and(|hash| hex(&hash) == expected)
            || self.meta.as_ref().is_some_and(|meta| {
                meta.v1.is_some_and(|hash| hex(&hash) == expected)
                    || meta.v2.is_some_and(|hash| hex(&hash) == expected)
            })
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
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            private_state_file(&metadata)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("Could not inspect state file".into()),
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    static TEMPORARY: AtomicU64 = AtomicU64::new(0);
    let temporary = parent.join(format!(
        ".tmp-{}-{nonce}-{}",
        std::process::id(),
        TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = crate::store::private_options()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| crate::store::migration_io("Transfer storage", "write", &error))?;
        file.write_all(data)
            .and_then(|()| crate::store::sync_file(&file))
            .map_err(|error| crate::store::migration_io("Transfer storage", "write", &error))?;
        fs::rename(&temporary, path)
            .map_err(|error| crate::store::migration_io("Transfer storage", "write", &error))?;
        crate::store::sync_directory(parent)
            .map_err(|error| crate::store::migration_sync("Transfer storage", &error))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn private_state_file(metadata: &fs::Metadata) -> Result<()> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Special state files are not allowed".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("Hardlinked state files are not allowed".into());
        }
    }
    Ok(())
}
fn inspect_media_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => private_state_file(&metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("Could not inspect media file".into()),
    }
}
fn persisted_pause(config: &DownloadConfig, id: &str) -> Result<bool> {
    let path = config.state_dir.join(format!("{id}.paused"));
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            private_state_file(&metadata)?;
            if metadata.len() != 7 {
                return Err("Invalid persisted pause marker".into());
            }
            let mut marker = Vec::new();
            crate::store::open_inspected(&path, &metadata)
                .and_then(|file| {
                    file.take(8)
                        .read_to_end(&mut marker)
                        .map_err(|error| error.to_string())
                })
                .map_err(|_| "Could not read persisted pause marker")?;
            if marker != b"paused\n" {
                return Err("Invalid persisted pause marker".into());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Could not inspect persisted pause marker".into()),
    }
}
fn peer_id() -> Result<[u8; 20]> {
    let random = crate::crypto::random_bytes::<12>()?;
    let mut id = [0; 20];
    id[..8].copy_from_slice(b"-MY0600-");
    id[8..].copy_from_slice(&random);
    Ok(id)
}

fn piece_order(meta: &Meta, priorities: &BTreeMap<usize, FilePriority>) -> Vec<usize> {
    let mut ranks = vec![0u8; meta.count()];
    for (index, file) in meta
        .files
        .iter()
        .enumerate()
        .filter(|(_, file)| !file.padding && file.length != 0)
    {
        let rank = priorities.get(&index).copied().unwrap_or_default().rank();
        let first = (file.offset / meta.piece_length as u64) as usize;
        let last = ((file.offset + file.length - 1) / meta.piece_length as u64) as usize;
        for value in ranks.iter_mut().take(last.saturating_add(1)).skip(first) {
            *value = (*value).max(rank);
        }
    }
    let mut order = Vec::with_capacity(meta.count());
    for rank in (0..=2).rev() {
        order.extend(
            ranks
                .iter()
                .enumerate()
                .filter_map(|(index, value)| (*value == rank).then_some(index)),
        );
    }
    order
}
fn verified_piece_bytes(meta: &Meta, index: usize) -> u64 {
    let start = index as u64 * meta.piece_length as u64;
    let end = start + meta.piece_size(index) as u64;
    meta.files
        .iter()
        .filter(|file| !file.padding)
        .map(|file| {
            end.min(file.offset + file.length)
                .saturating_sub(start.max(file.offset))
        })
        .sum()
}

fn seed_clock_snapshot(job: &Job) -> (u64, Duration) {
    if let Some((started, base)) = job.seed_clock {
        let elapsed = started.elapsed().saturating_add(job.seed_partial);
        (
            base.saturating_add(elapsed.as_secs()),
            Duration::from_nanos(u64::from(elapsed.subsec_nanos())),
        )
    } else {
        (job.control.seed_elapsed_secs, job.seed_partial)
    }
}
fn clock_control(job: &Job) -> (TorrentControl, Duration) {
    let mut control = job.control.clone();
    control.downloaded_bytes = job.counters.downloaded.load(Ordering::Relaxed);
    control.uploaded_bytes = job.counters.uploaded.load(Ordering::Relaxed);
    let (seconds, partial) = seed_clock_snapshot(job);
    control.seed_elapsed_secs = seconds;
    (control, partial)
}
fn snapshot_control(job: &Job) -> TorrentControl {
    clock_control(job).0
}
fn settle_seed_clock(job: &mut Job) {
    let (control, partial) = clock_control(job);
    job.control = control;
    job.seed_partial = partial;
    job.seed_clock = None;
}
fn persist_index(job: &Job) -> Result<()> {
    if let Some(index) = &job.index {
        let result = index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned".to_owned())
            .and_then(|mut catalog| catalog.save(job));
        if let Err(error) = result {
            job.persistence_failed.store(true, Ordering::Release);
            job.cancel.store(true, Ordering::Release);
            return Err(error);
        }
        job.persistence_failed.store(false, Ordering::Release);
    }
    Ok(())
}

/// Forgets a removed transfer: its index record first, so an interruption
/// leaves at worst unreferenced state files, then that state and, when asked,
/// its downloaded files. Each step tolerates a previous partial run.
fn forget(
    config: &DownloadConfig,
    index: &Mutex<index::TransferIndex>,
    id: &str,
    delete_files: bool,
) -> Result<()> {
    index
        .lock()
        .map_err(|_| "Native transfer index lock is poisoned")?
        .remove(id)?;
    let absent = |result: std::io::Result<()>| {
        result.or_else(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(error),
        })
    };
    for suffix in ["control", "torrent", "source", "paused", "seed"] {
        absent(fs::remove_file(
            config.state_dir.join(format!("{id}.{suffix}")),
        ))
        .map_err(|_| "Could not remove the transfer state")?;
    }
    crate::store::sync_directory(&config.state_dir)
        .map_err(|_| "Could not synchronize the transfer state")?;
    if delete_files {
        absent(fs::remove_dir_all(config.data_dir.join(id)))
            .map_err(|_| "Could not delete the downloaded files")?;
    }
    Ok(())
}

fn persist_control(config: &DownloadConfig, job: &mut Job) -> Result<()> {
    let control = snapshot_control(job);
    let encoded = control.encode()?;
    if encoded != job.saved_control {
        atomic_write(
            &config.state_dir.join(format!("{}.control", job.status.id)),
            &encoded,
        )?;
        job.saved_control = encoded;
    }
    job.control = control;
    persist_index(job)
}
fn commit_control(config: &DownloadConfig, job: &mut Job, control: TorrentControl) -> Result<()> {
    let encoded = control.encode()?;
    atomic_write(
        &config.state_dir.join(format!("{}.control", job.status.id)),
        &encoded,
    )?;
    job.control = control;
    job.saved_control = encoded;
    if let Some(meta) = &job.meta {
        job.piece_order = Arc::new(piece_order(meta, &job.control.file_priorities));
    }
    job.download_gate.set_limit(
        job.control
            .policy
            .as_ref()
            .map_or(0, |policy| policy.download_limit_bps),
    )?;
    job.upload_gate.set_limit(
        job.control
            .policy
            .as_ref()
            .map_or(0, |policy| policy.upload_limit_bps),
    )?;
    persist_index(job)
}
fn restore_controls(config: &DownloadConfig, jobs: &mut BTreeMap<String, Job>) -> Result<()> {
    for (id, job) in jobs.iter_mut() {
        let path = config.state_dir.join(format!("{id}.control"));
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                private_state_file(&metadata)?;
                if metadata.len() > 4 * 1024 * 1024 + 44 {
                    return Err("Invalid native transfer control file".into());
                }
                let mut encoded = Vec::new();
                crate::store::open_inspected(&path, &metadata)
                    .and_then(|file| {
                        file.take(4 * 1024 * 1024 + 45)
                            .read_to_end(&mut encoded)
                            .map_err(|error| error.to_string())
                    })
                    .map_err(|_| "Could not read native transfer controls")?;
                let control = TorrentControl::decode(&encoded)?;
                if control.id != *id {
                    return Err("Native transfer control identity or queue order mismatch".into());
                }
                job.control = control;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                job.control.queue_order = 0;
            }
            Err(_) => return Err("Could not inspect native transfer controls".into()),
        }
    }
    for job in jobs.values_mut() {
        if let Some(meta) = &job.meta {
            if job
                .control
                .file_priorities
                .keys()
                .any(|index| meta.files.get(*index).is_none_or(|file| file.padding))
            {
                return Err("Invalid persisted torrent file priority".into());
            }
            job.piece_order = Arc::new(piece_order(meta, &job.control.file_priorities));
        } else if !job.control.file_priorities.is_empty() {
            return Err("File priorities require authenticated torrent metadata".into());
        }
        job.counters
            .downloaded
            .store(job.control.downloaded_bytes, Ordering::Relaxed);
        job.counters
            .uploaded
            .store(job.control.uploaded_bytes, Ordering::Relaxed);
        job.download_gate.set_limit(
            job.control
                .policy
                .as_ref()
                .map_or(0, |policy| policy.download_limit_bps),
        )?;
        job.upload_gate.set_limit(
            job.control
                .policy
                .as_ref()
                .map_or(0, |policy| policy.upload_limit_bps),
        )?;
    }
    Ok(())
}

fn expand_selection(config: &DownloadConfig, job: &mut Job, added: &FileSelection) -> Result<()> {
    let merged = job
        .control
        .file_selection
        .merge(added, job.meta.as_deref())?;
    if merged == job.control.file_selection {
        return Ok(());
    }
    let mut control = snapshot_control(job);
    control.file_selection = merged;
    commit_control(config, job, control)?;
    if !job.status.ready {
        job.cancel.store(true, Ordering::Release);
        job.cancel = Arc::new(AtomicBool::new(job.paused || job.control.user_paused));
        job.failed = false;
        job.status.selected_ready = false;
        job.status.progress = 0.0;
        job.status.message = "File selection expanded; verification queued".into();
    }
    Ok(())
}
fn seed_budget(job: &Job, defaults: &TransferPolicy) -> Option<u64> {
    let ratio = job
        .control
        .policy
        .as_ref()
        .unwrap_or(defaults)
        .seed_ratio_milli?;
    let total: u128 = job
        .meta
        .as_ref()?
        .files
        .iter()
        .filter(|file| !file.padding)
        .map(|file| u128::from(file.length))
        .sum();
    Some((total.saturating_mul(u128::from(ratio)).div_ceil(1000)).min(u128::from(u64::MAX)) as u64)
}
fn limit_seed(job: &mut Job) {
    settle_seed_clock(job);
    job.control.seed_limited = true;
    job.cancel.store(true, Ordering::Release);
    job.status.message = "Download verified; seeding policy reached".into();
}
fn apply_seed_limits(job: &mut Job, config: &DownloadConfig, defaults: &TransferPolicy) {
    if !job.status.ready
        || job.seed_queued
        || job.retiring
        || job.needs_rehash
        || !config.seed
        || job.paused
        || job.control.user_paused
        || job.control.seed_limited
        || job.failed
        || job.cancel.load(Ordering::Acquire)
    {
        if job.seed_clock.is_some() {
            settle_seed_clock(job);
        }
        if job.status.ready && job.control.seed_limited {
            limit_seed(job);
        }
        return;
    }
    if job.seed_clock.is_none() {
        job.seed_clock = Some((Instant::now(), job.control.seed_elapsed_secs));
    }
    let control = snapshot_control(job);
    let policy = control.policy.as_ref().unwrap_or(defaults);
    if policy
        .seed_time_secs
        .is_some_and(|limit| control.seed_elapsed_secs >= limit)
        || seed_budget(job, defaults).is_some_and(|limit| control.uploaded_bytes >= limit)
    {
        limit_seed(job);
    }
}

fn transfer_state(job: &Job) -> &'static str {
    if job.paused || job.control.user_paused {
        "paused"
    } else if job.failed {
        "failed"
    } else if job.status.ready && job.control.seed_limited {
        "seed_limited"
    } else if job.seed_queued {
        "queued_seed"
    } else if job.status.ready {
        "ready"
    } else if job.status.selected_ready {
        "selected_ready"
    } else if job.running && job.cross_seed.is_some() {
        "verifying"
    } else if job.running {
        "downloading"
    } else {
        "queued"
    }
}

/// The scheme, host and port of a tracker: its path and query often carry
/// a private passkey, which never leaves the service.
fn tracker_host(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = crate::integrations::report_text(host, 255);
    if scheme.is_empty() {
        host
    } else {
        format!("{}://{host}", scheme.to_ascii_lowercase())
    }
}

/// What one transfer's detail page adds to its list entry: identity, pieces,
/// trackers and the peers of a running download.
fn transfer_details(job: &Job, config: &DownloadConfig, value: &mut crate::json::Value) {
    use crate::json::Value as Json;
    let hashes: Vec<_> = job
        .source
        .v1
        .iter()
        .map(|hash| Json::String(hex(hash)))
        .chain(job.source.v2.iter().map(|hash| Json::String(hex(hash))))
        .collect();
    value.insert("info_hashes", Json::Array(hashes));
    if let Some(meta) = &job.meta {
        let name = meta
            .files
            .iter()
            .find(|file| !file.padding)
            .and_then(|file| file.path.components().next())
            .map(|part| part.as_os_str().to_string_lossy().into_owned());
        value.insert("name", name.map_or(Json::Null, Json::String));
        value.insert("total_bytes", meta.total.to_string());
        value.insert("piece_count", meta.count().to_string());
        value.insert("piece_bytes", meta.piece_length.to_string());
        value.insert("private", meta.private);
    }
    let private = job.meta.as_ref().is_some_and(|meta| meta.private);
    value.insert("dht", config.dht && !private);
    value.insert("pex", config.pex && !private);
    let now = Instant::now();
    let trackers = job
        .source
        .trackers
        .iter()
        .take(64)
        .map(|url| {
            let mut tracker = Json::object();
            tracker.insert("host", tracker_host(url));
            let state = job.announcements.get(url);
            let status = match state {
                Some(state) if state.in_flight => "announcing",
                Some(state) if state.last_error.is_some() => "error",
                Some(state) if state.last_at != 0 => "working",
                _ => "waiting",
            };
            tracker.insert("status", status);
            if let Some(state) = state {
                let optional =
                    |count: Option<u64>| count.map_or(Json::Null, |n| n.to_string().into());
                tracker.insert("seeders", optional(state.seeders));
                tracker.insert("leechers", optional(state.leechers));
                tracker.insert("peers", state.peers.to_string());
                tracker.insert(
                    "last_at",
                    if state.last_at == 0 {
                        Json::Null
                    } else {
                        state.last_at.to_string().into()
                    },
                );
                tracker.insert(
                    "next_in_secs",
                    state
                        .next
                        .saturating_duration_since(now)
                        .as_secs()
                        .to_string(),
                );
                tracker.insert(
                    "error",
                    state.last_error.clone().map_or(Json::Null, Json::String),
                );
            }
            tracker
        })
        .collect();
    value.insert("trackers", Json::Array(trackers));
    let peers = if job.running {
        job.live_peers.as_slice()
    } else {
        &[]
    };
    value.insert(
        "known_peers",
        (if job.running { job.known_peers } else { 0 }).to_string(),
    );
    value.insert(
        "peers",
        Json::Array(
            peers
                .iter()
                .map(|peer| {
                    let mut entry = Json::object();
                    entry.insert("address", peer.address.to_string());
                    entry.insert("connected", peer.connected);
                    entry.insert("progress", peer.progress.map_or(Json::Null, Json::Number));
                    entry.insert("downloading", peer.downloading);
                    entry.insert("received_bytes", peer.received_bytes.to_string());
                    entry
                })
                .collect(),
        ),
    );
    value.insert(
        "seeding_peers",
        Json::Array(
            job.seeding_peers
                .values()
                .map(|peer| {
                    let mut entry = Json::object();
                    entry.insert("address", peer.address.to_string());
                    entry.insert("progress", peer.progress.map_or(Json::Null, Json::Number));
                    entry.insert("interested", peer.interested);
                    entry.insert(
                        "uploading",
                        peer.last_sent
                            .is_some_and(|at| at.elapsed() < Duration::from_secs(3)),
                    );
                    entry.insert("sent_bytes", peer.sent_bytes.to_string());
                    entry
                })
                .collect(),
        ),
    );
}

fn transfer_value(
    job: &Job,
    queue_position: usize,
    defaults: &TransferPolicy,
    config: &DownloadConfig,
    seed_queue: &queue::SeedQueue,
) -> crate::json::Value {
    use crate::json::Value as Json;
    let mut value = snapshot_control(job).to_json();
    value.insert("ready", job.status.ready);
    value.insert("seed_only", job.cross_seed.is_some());
    value.insert(
        "cross_seed_source_id",
        job.cross_seed
            .as_ref()
            .map_or(Json::Null, |reuse| Json::String(reuse.source_id.clone())),
    );
    value.insert("selected_ready", job.status.selected_ready);
    value.insert("running", job.running);
    value.insert("paused", job.paused || job.control.user_paused);
    let seed_queued = seed_queue.waiting(config.seed, job);
    value.insert("seed_queued", seed_queued);
    value.insert(
        "status",
        if seed_queued
            && !job.paused
            && !job.control.user_paused
            && !job.failed
            && !job.control.seed_limited
        {
            "queued_seed"
        } else {
            transfer_state(job)
        },
    );
    value.insert("download_queue_limit", config.max_active.to_string());
    value.insert("seed_queue_limit", config.max_seeding.to_string());
    value.insert(
        "download_execution_slots",
        config.download_slots().to_string(),
    );
    value.insert("failed", job.failed);
    value.insert("queue_position", Json::Number(queue_position as f64));
    value.insert("progress", Json::Number(job.status.progress));
    value.insert(
        "message",
        if seed_queued
            && !job.paused
            && !job.control.user_paused
            && !job.failed
            && !job.control.seed_limited
        {
            "Waiting for a seed queue slot".into()
        } else {
            job.status.message.clone()
        },
    );
    value.insert(
        "verified_bytes",
        job.counters.verified.load(Ordering::Relaxed).to_string(),
    );
    let mut effective = job.control.policy.as_ref().unwrap_or(defaults).clone();
    let capped = |global: u64, local: u64| match (global, local) {
        (0, value) | (value, 0) => value,
        (global, local) => global.min(local),
    };
    effective.download_limit_bps =
        capped(defaults.download_limit_bps, effective.download_limit_bps);
    effective.upload_limit_bps = capped(defaults.upload_limit_bps, effective.upload_limit_bps);
    value.insert("effective_policy", effective.to_json());
    let available: BTreeSet<_> = job.status.available_files.iter().collect();
    value.insert(
        "files",
        Json::Array(job.meta.as_ref().map_or_else(Vec::new, |meta| {
            meta.files
                .iter()
                .enumerate()
                .filter(|(_, file)| !file.padding)
                .map(|(index, file)| {
                    let mut value = Json::object();
                    value.insert("index", Json::Number(index as f64));
                    value.insert("path", file.path.to_string_lossy().into_owned());
                    value.insert("size_bytes", file.length.to_string());
                    value.insert(
                        "selected",
                        match &job.control.file_selection {
                            FileSelection::All => true,
                            FileSelection::Paths(paths) => {
                                paths.contains(&file.path.to_string_lossy().into_owned())
                            }
                        },
                    );
                    value.insert(
                        "verified",
                        job.status.ready
                            || available
                                .contains(&config.data_dir.join(&job.status.id).join(&file.path)),
                    );
                    value.insert(
                        "priority",
                        job.control
                            .file_priorities
                            .get(&index)
                            .copied()
                            .unwrap_or_default()
                            .to_json(),
                    );
                    value
                })
                .collect()
        })),
    );
    value
}

// A single authenticated transfer and, for cross-seeds, its original metadata.
// This deliberately never enumerates the state directory.
fn load_job(config: &DownloadConfig, id: &str) -> Result<Job> {
    if ![40, 64].contains(&id.len())
        || !id.bytes().all(|b| b.is_ascii_hexdigit())
        || id != id.to_ascii_lowercase()
    {
        return Err("Invalid native transfer identity".into());
    }
    let mut map = BTreeMap::new();
    if config
        .state_dir
        .join(format!("{id}.seed"))
        .try_exists()
        .map_err(|_| "Could not inspect seed state")?
    {
        let job = cross_seed::load(config, id)?;
        map.insert(id.to_owned(), job);
    } else if config
        .state_dir
        .join(format!("{id}.torrent"))
        .try_exists()
        .map_err(|_| "Could not inspect transfer metadata")?
    {
        let meta_path = config.state_dir.join(format!("{id}.torrent"));
        let meta_metadata =
            fs::symlink_metadata(&meta_path).map_err(|_| "Persisted metadata is inaccessible")?;
        private_state_file(&meta_metadata)?;
        let file = crate::store::open_inspected(&meta_path, &meta_metadata)
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
        let original_metadata = match fs::symlink_metadata(&original_path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err("Persisted source is inaccessible".into()),
        };
        let original = if let Some(m) = original_metadata {
            private_state_file(&m)?;
            let mut value = String::new();
            crate::store::open_inspected(&original_path, &m)
                .and_then(|f| {
                    f.take(64 * 1024 + 1)
                        .read_to_string(&mut value)
                        .map_err(|error| error.to_string())
                })
                .map_err(|_| "Could not read persisted source")?;
            if value.len() > 64 * 1024 {
                return Err("Persisted source is too large".into());
            }
            value
        } else {
            config
                .state_dir
                .join(format!("{id}.torrent"))
                .to_string_lossy()
                .into_owned()
        };
        let mut source = if original.starts_with("magnet:?") {
            Source::parse_magnet_with_deadline(&original, None)?
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
            selected_ready: false,
            available_files: Vec::new(),
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
                index: None,
                persistence_failed: AtomicBool::new(false),
                retiring: false,
                removing: None,
                live_peers: Vec::new(),
                known_peers: 0,
                seeding_peers: BTreeMap::new(),
                inbound: None,
                admitted: true,
                needs_rehash: false,
                seed_queued: false,
                source,
                cross_seed: None,
                meta: Some(Arc::new(meta)),
                status,
                running: false,
                paused: persisted_pause(config, id)?,
                failed: false,
                cancel: Arc::new(AtomicBool::new(false)),
                peers: Vec::new(),
                tracker_peers: BTreeSet::new(),
                counters: Arc::new(TransferCounters::default()),
                announcements: BTreeMap::new(),
                announce_hash: None,
                dht_next: Instant::now(),
                dht_in_flight: false,
                control: TorrentControl::new(id.to_owned(), 1)?,
                piece_order: Arc::new(Vec::new()),
                download_gate: Arc::new(RateGate::default()),
                upload_gate: Arc::new(RateGate::default()),
                seed_clock: None,
                seed_partial: Duration::ZERO,
                upload_reserved: 0,
                saved_control: Vec::new(),
            },
        );
    } else {
        let source_path = config.state_dir.join(format!("{id}.source"));
        let source_metadata =
            fs::symlink_metadata(&source_path).map_err(|_| "Persisted source is inaccessible")?;
        private_state_file(&source_metadata)?;
        let mut original = String::new();
        crate::store::open_inspected(&source_path, &source_metadata)
            .and_then(|f| {
                f.take(64 * 1024 + 1)
                    .read_to_string(&mut original)
                    .map_err(|error| error.to_string())
            })
            .map_err(|_| "Invalid persisted source")?;
        if !original.starts_with("magnet:?") {
            return Err("Missing persisted metadata for this source".into());
        }
        let source = Source::parse_magnet_with_deadline(&original, None)?;
        if source.id() != id {
            return Err("BitTorrent queue hash mismatch".into());
        }
        map.insert(
            id.to_owned(),
            Job {
                index: None,
                persistence_failed: AtomicBool::new(false),
                retiring: false,
                removing: None,
                live_peers: Vec::new(),
                known_peers: 0,
                seeding_peers: BTreeMap::new(),
                inbound: None,
                admitted: true,
                needs_rehash: false,
                seed_queued: false,
                meta: None,
                cross_seed: None,
                source,
                status: DownloadStatus {
                    id: id.to_owned(),
                    progress: 0.0,
                    ready: false,
                    selected_ready: false,
                    available_files: Vec::new(),
                    files: Vec::new(),
                    message: "Resuming metadata discovery".into(),
                },
                running: false,
                paused: persisted_pause(config, id)?,
                failed: false,
                cancel: Arc::new(AtomicBool::new(false)),
                peers: Vec::new(),
                tracker_peers: BTreeSet::new(),
                counters: Arc::new(TransferCounters::default()),
                announcements: BTreeMap::new(),
                announce_hash: None,
                dht_next: Instant::now(),
                dht_in_flight: false,
                control: TorrentControl::new(id.to_owned(), 1)?,
                piece_order: Arc::new(Vec::new()),
                download_gate: Arc::new(RateGate::default()),
                upload_gate: Arc::new(RateGate::default()),
                seed_clock: None,
                seed_partial: Duration::ZERO,
                upload_reserved: 0,
                saved_control: Vec::new(),
            },
        );
    }
    restore_controls(config, &mut map)?;
    let mut job = map.remove(id).ok_or("Missing native transfer state")?;
    if job.paused || job.control.user_paused {
        job.status.message = "Download paused".into();
    }
    Ok(job)
}

fn record_status(config: &DownloadConfig, record: &index::Record) -> Result<DownloadStatus> {
    let mut status = record.status.clone();
    if !record.current {
        status.ready = false;
        status.selected_ready = false;
        status.available_files.clear();
        status.progress = 0.0;
        if !record.failed {
            status.message = "Verification required after restart".into();
        }
    }
    if record.paused || record.control.user_paused || persisted_pause(config, &status.id)? {
        status.message = "Download paused".into();
    }
    Ok(status)
}
fn conservative_policy(
    left: Option<TransferPolicy>,
    right: Option<TransferPolicy>,
    defaults: &TransferPolicy,
) -> Option<TransferPolicy> {
    fn rate(a: u64, b: u64) -> u64 {
        match (a, b) {
            (0, v) | (v, 0) => v,
            _ => a.min(b),
        }
    }
    fn optional<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
        match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    if left == right {
        return left;
    }
    let a = left.as_ref().unwrap_or(defaults);
    let b = right.as_ref().unwrap_or(defaults);
    Some(TransferPolicy {
        download_limit_bps: rate(a.download_limit_bps, b.download_limit_bps),
        upload_limit_bps: rate(a.upload_limit_bps, b.upload_limit_bps),
        seed_ratio_milli: optional(a.seed_ratio_milli, b.seed_ratio_milli),
        seed_time_secs: optional(a.seed_time_secs, b.seed_time_secs),
    })
}
fn indexed_job(
    config: &DownloadConfig,
    index: &Arc<Mutex<index::TransferIndex>>,
    id: &str,
    defaults: &TransferPolicy,
) -> Result<Job> {
    let record = index
        .lock()
        .map_err(|_| "Native transfer index lock is poisoned")?
        .get(id)?
        .ok_or("Unknown download")?;
    let mut job = load_job(config, id)?;
    let status = record_status(config, &record)?;
    let needs_rehash = record.status.ready || record.status.selected_ready;
    let disk_control = job.control;
    job.control = record.control;
    // Offline pauses and a stricter control write interrupted before catalog
    // publication remain safety fences, even when the catalog is older.
    job.control.user_paused |= disk_control.user_paused;
    job.control.seed_limited |= disk_control.seed_limited;
    job.control.downloaded_bytes = job
        .control
        .downloaded_bytes
        .max(disk_control.downloaded_bytes);
    job.control.uploaded_bytes = job.control.uploaded_bytes.max(disk_control.uploaded_bytes);
    job.control.seed_elapsed_secs = job
        .control
        .seed_elapsed_secs
        .max(disk_control.seed_elapsed_secs);
    job.control.policy = conservative_policy(job.control.policy, disk_control.policy, defaults);
    job.status = status;
    job.needs_rehash = needs_rehash;
    job.paused |= record.paused;
    job.failed = record.failed;
    job.counters
        .downloaded
        .store(job.control.downloaded_bytes, Ordering::Relaxed);
    job.counters
        .uploaded
        .store(job.control.uploaded_bytes, Ordering::Relaxed);
    job.saved_control = job.control.encode()?;
    job.index = Some(index.clone());
    job.admitted = false;
    if let Some(meta) = &job.meta {
        job.piece_order = Arc::new(piece_order(meta, &job.control.file_priorities));
    }
    job.download_gate.set_limit(
        job.control
            .policy
            .as_ref()
            .map_or(0, |p| p.download_limit_bps),
    )?;
    job.upload_gate.set_limit(
        job.control
            .policy
            .as_ref()
            .map_or(0, |p| p.upload_limit_bps),
    )?;
    Ok(job)
}
fn trim_jobs(
    config: &DownloadConfig,
    jobs: &mut BTreeMap<String, Job>,
    ceiling: usize,
) -> Result<()> {
    if jobs.len() <= ceiling {
        return Ok(());
    }
    let mut candidates: Vec<_> = jobs
        .iter()
        .filter(|(_, j)| {
            // A transfer being removed stays resident until the manager forgets it.
            j.removing.is_none()
                && !j.running
                && j.cross_seed
                    .as_ref()
                    .is_none_or(|reuse| Arc::strong_count(&reuse.activity) == 1)
                && j.upload_reserved == 0
                && !j.dht_in_flight
                && j.announcements.values().all(|a| !a.in_flight)
                && Arc::strong_count(&j.cancel) == 1
        })
        .collect();
    candidates.sort_by(|(aid, a), (bid, b)| {
        let rank = |job: &Job| {
            if !job.admitted || job.paused || job.control.user_paused || job.failed {
                0
            } else if job.status.ready || job.status.selected_ready {
                1
            } else {
                2
            }
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.control.priority.cmp(&b.control.priority))
            .then_with(|| b.control.queue_order.cmp(&a.control.queue_order))
            .then_with(|| aid.cmp(bid))
    });
    let ids: Vec<_> = candidates
        .into_iter()
        .take(jobs.len() - ceiling)
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        let job = jobs.get_mut(&id).ok_or("Missing resident transfer")?;
        settle_seed_clock(job);
        if job
            .announcements
            .values()
            .any(|state| state.started && !state.stopped)
        {
            job.retiring = true;
            job.cancel.store(true, Ordering::Release);
            continue;
        }
        persist_control(config, job)?;
        job.cancel.store(true, Ordering::Release);
        jobs.remove(&id);
    }
    Ok(())
}

impl Client {
    pub fn open(config: DownloadConfig) -> Result<Self> {
        Self::open_with_policy(config, TransferPolicy::default())
    }
    /// An explicitly unprotected client on the ordinary network.
    pub fn open_with_policy(config: DownloadConfig, policy: TransferPolicy) -> Result<Self> {
        Self::open_internal(config, policy, None, transport::direct())
    }
    pub fn open_with_policy_and_admission(
        config: DownloadConfig,
        policy: TransferPolicy,
        admission: AdmissionGate,
        network: Arc<dyn transport::Network>,
    ) -> Result<Self> {
        Self::open_internal(config, policy, Some(admission), network)
    }
    fn open_internal(
        mut config: DownloadConfig,
        policy: TransferPolicy,
        admission: Option<AdmissionGate>,
        network: Arc<dyn transport::Network>,
    ) -> Result<Self> {
        policy.validate()?;
        let policy = Arc::new(PolicyRuntime {
            seed_queue: queue::SeedQueue::new(config.max_seeding),
            download: Arc::new(RateGate::new(policy.download_limit_bps)?),
            upload: Arc::new(RateGate::new(policy.upload_limit_bps)?),
            policy,
            network,
        });
        if !(1..=256).contains(&config.max_peers) {
            return Err("Invalid peer connection count".into());
        }
        mkdir_private(&config.state_dir)?;
        mkdir_private(&config.data_dir)?;
        let listener = policy
            .network
            .listen(SocketAddr::from(([0, 0, 0, 0], config.listen_port)))?;
        config.listen_port = listener.port();
        let index = Arc::new(Mutex::new(index::TransferIndex::open(&mut config)?));
        let map = BTreeMap::new();
        let jobs = Arc::new(Mutex::new(map));
        let stop = Arc::new(AtomicBool::new(false));
        let id = peer_id()?;
        let manager = {
            let admission = admission.clone();
            let jobs = jobs.clone();
            let index = index.clone();
            let stop = stop.clone();
            let cfg = config.clone();
            {
                let policy = policy.clone();
                thread::spawn(move || manager_loop(cfg, jobs, stop, id, policy, index, admission))
            }
        };
        let listener = {
            let admission = admission.clone();
            let jobs = jobs.clone();
            let index = index.clone();
            let stop = stop.clone();
            let cfg = config.clone();
            {
                let policy = policy.clone();
                thread::spawn(move || {
                    listen_loop(listener, cfg, jobs, stop, id, policy, (index, admission))
                })
            }
        };
        Ok(Self {
            admission,
            config,
            index,
            jobs,
            stop,
            manager: Some(manager),
            listener: Some(listener),
            policy,
        })
    }
    pub fn ensure(&self, source: &str) -> Result<DownloadStatus> {
        self.ensure_selection(source, FileSelection::All, None)
    }

    /// Record exact paths before metadata discovery can schedule any payload.
    pub fn ensure_files(&self, source: &str, paths: &[String]) -> Result<DownloadStatus> {
        self.ensure_selection(source, FileSelection::paths(paths)?, None)
    }

    /// Reject a changed metadata URL before publishing a queue-visible transfer.
    pub fn ensure_bound_files(
        &self,
        source: &str,
        paths: &[String],
        expected: &str,
    ) -> Result<DownloadStatus> {
        self.ensure_selection(source, FileSelection::paths(paths)?, Some(expected))
    }

    pub fn require_bound_files(
        &self,
        id: &str,
        paths: &[String],
        expected: &str,
    ) -> Result<DownloadStatus> {
        {
            let mut jobs = self
                .jobs
                .lock()
                .map_err(|_| "BitTorrent state lock is poisoned")?;
            self.load(&mut jobs, id)?;
            let job = jobs.get(id).ok_or("Unknown download")?;
            if !job.source.matches_identity(expected) {
                return Err("Torrent identity differs from the accepted pack metadata".into());
            }
        }
        self.require_files(id, paths)
    }

    fn ensure_selection(
        &self,
        source: &str,
        selection: FileSelection,
        expected: Option<&str>,
    ) -> Result<DownloadStatus> {
        self.policy.network.publication()?;
        let mut source = Source::parse(&*self.policy.network, source)?;
        let _publication = self.policy.network.publication()?;
        if expected.is_some_and(|hash| !source.matches_identity(hash)) {
            return Err("Torrent identity differs from the accepted pack metadata".into());
        }
        if let Some(meta) = &source.meta {
            selection.validate_metadata(meta)?;
        }
        let id = source.id();
        if id == "0000000000000000000000000000000000000000" {
            return Err(
                "Torrent identity is reserved for the native transfer format marker".into(),
            );
        }
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        if let Some(hash) = source.v2 {
            let wire: [u8; 20] = hash[..20]
                .try_into()
                .map_err(|_| "Invalid v2 wire identity")?;
            let wire_owner = self
                .index
                .lock()
                .map_err(|_| "Native transfer index lock is poisoned")?
                .wire_alias(wire)?;
            if let Some(owner) = wire_owner {
                let seed_only = self
                    .index
                    .lock()
                    .map_err(|_| "Native transfer index lock is poisoned")?
                    .get(&owner)?
                    .is_some_and(|r| r.cross_seed);
                if seed_only {
                    return Err(
                        "Seed-only cross-seed wire identity cannot be used for acquisition".into(),
                    );
                }
            }
        }
        let existing = self
            .index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?
            .alias(source.v1, source.v2)?;
        if let Some(existing) = existing {
            self.load(&mut jobs, &existing)?;
        }
        if jobs.values().any(|job| {
            job.cross_seed.is_some()
                && source.v2.is_some_and(|hash| {
                    job.source.v1.is_some_and(|identity| hash[..20] == identity)
                })
        }) {
            return Err("Seed-only cross-seed wire identity cannot be used for acquisition".into());
        }
        if let Some(job) = jobs.values_mut().find(|job| {
            source.v1.is_some_and(|h| job.source.v1 == Some(h))
                || source.v2.is_some_and(|h| job.source.v2 == Some(h))
        }) {
            if job.cross_seed.is_some() {
                return Err("Seed-only cross-seed identity cannot be used for acquisition".into());
            }
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
            expand_selection(&self.config, job, &selection)?;
            if !job.control.user_paused {
                resume_job(&self.config, job)?;
            }
            persist_index(job)?;
            return Ok(job.status.clone());
        }
        trim_jobs(&self.config, &mut jobs, self.config.download_slots() + 31)?;
        if jobs.len() >= self.config.download_slots() + 64 {
            return Err("Native transfer working set is busy; retry later".into());
        }
        let status = DownloadStatus {
            id: id.clone(),
            progress: 0.0,
            ready: false,
            selected_ready: false,
            available_files: Vec::new(),
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
        let order = self
            .index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?
            .next_order()?;
        let mut control = TorrentControl::new(id.clone(), order)?;
        control.file_selection = selection;
        // A queue-visible source/metadata file must never survive a crash
        // without the selection that prevents unintended full acquisition.
        atomic_write(
            &self.config.state_dir.join(format!("{id}.control")),
            &control.encode()?,
        )?;
        if let Some(meta) = &metadata {
            atomic_write(
                &self.config.state_dir.join(format!("{id}.torrent")),
                &meta.encoded,
            )?;
        }
        atomic_write(
            &self.config.state_dir.join(format!("{id}.source")),
            source.original.as_bytes(),
        )?;
        jobs.insert(
            id.clone(),
            Job {
                index: None,
                persistence_failed: AtomicBool::new(false),
                retiring: false,
                removing: None,
                live_peers: Vec::new(),
                known_peers: 0,
                seeding_peers: BTreeMap::new(),
                inbound: None,
                admitted: true,
                needs_rehash: false,
                seed_queued: false,
                meta: metadata,
                cross_seed: None,
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
                control: TorrentControl::new(id.to_owned(), 1)?,
                piece_order: Arc::new(Vec::new()),
                download_gate: Arc::new(RateGate::default()),
                upload_gate: Arc::new(RateGate::default()),
                seed_clock: None,
                seed_partial: Duration::ZERO,
                upload_reserved: 0,
                saved_control: Vec::new(),
            },
        );
        if let Some(job) = jobs.get_mut(&id) {
            job.control = control;
            if let Some(meta) = &job.meta {
                job.piece_order = Arc::new(piece_order(meta, &job.control.file_priorities));
            }
            job.saved_control = job.control.encode()?;
        }
        if let Some(job) = jobs.get_mut(&id) {
            job.index = Some(self.index.clone());
            job.admitted = self
                .admission
                .as_ref()
                .is_none_or(|gate| gate(&id).unwrap_or(false));
            if let Err(error) = persist_index(job) {
                jobs.remove(&id);
                return Err(error);
            }
        }
        Ok(status)
    }

    pub fn select_files(&self, id: &str, update: &SelectionUpdate) -> Result<DownloadStatus> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        let job = jobs.get_mut(id).ok_or("Unknown download")?;
        if job.cross_seed.is_some() {
            return Err("Seed-only cross-seed selection cannot change".into());
        }
        let selection = match update {
            SelectionUpdate::All => FileSelection::All,
            SelectionUpdate::Indices(indices) => {
                if indices.is_empty() || indices.len() > selection::MAX_SELECTED_FILES {
                    return Err("Selection requires 1 to 1,024 distinct file indices".into());
                }
                let meta = job
                    .meta
                    .as_ref()
                    .ok_or("File selection requires authenticated metadata")?;
                let mut seen = BTreeSet::new();
                let mut paths = Vec::with_capacity(indices.len());
                for index in indices {
                    let file = meta
                        .files
                        .get(*index)
                        .filter(|file| !file.padding)
                        .ok_or("Selected file index is absent or padding")?;
                    if !seen.insert(*index) {
                        return Err("Duplicate selected file index".into());
                    }
                    paths.push(file.path.to_string_lossy().into_owned());
                }
                FileSelection::paths(&paths)?
            }
        };
        expand_selection(&self.config, job, &selection)?;
        // Expansion never undoes a user pause. A later explicit resume or
        // ordinary acquisition retry observes the retained policy.
        persist_index(job)?;
        Ok(job.status.clone())
    }

    pub fn require_files(&self, id: &str, paths: &[String]) -> Result<DownloadStatus> {
        self.require_selection(id, &FileSelection::paths(paths)?)
    }

    pub fn require_all(&self, id: &str) -> Result<DownloadStatus> {
        self.require_selection(id, &FileSelection::All)
    }

    fn require_selection(&self, id: &str, selection: &FileSelection) -> Result<DownloadStatus> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        let job = jobs.get_mut(id).ok_or("Unknown download")?;
        if job.cross_seed.is_some() {
            return Err("Seed-only cross-seed identity cannot supply acquisition files".into());
        }
        if !job.failed
            && match selection {
                FileSelection::All => job.control.file_selection == FileSelection::All,
                FileSelection::Paths(paths) => paths.iter().all(|path| {
                    job.status
                        .available_files
                        .contains(&self.config.data_dir.join(id).join(path))
                }),
            }
        {
            persist_index(job)?;
            return Ok(job.status.clone());
        }
        expand_selection(&self.config, job, selection)?;
        if job.failed {
            return Err(job.status.message.clone());
        }
        persist_index(job)?;
        Ok(job.status.clone())
    }
    /// Pause a native transfer without removing its verified files or metadata.
    pub fn cancel(&self, id: &str) -> Result<()> {
        let gate = cross_seed::gate(self, id)?;
        let _admission = gate
            .as_ref()
            .map(|gate| {
                gate.lock()
                    .map_err(|_| "Cross-seed activity lock is poisoned")
            })
            .transpose()?;
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        let job = jobs.get_mut(id).ok_or("Unknown download")?;
        atomic_write(
            &self.config.state_dir.join(format!("{id}.paused")),
            b"paused\n",
        )?;
        settle_seed_clock(job);
        job.paused = true;
        job.cancel.store(true, Ordering::Release);
        job.status.message = "Download paused".into();
        persist_control(&self.config, job)?;
        Ok(())
    }
    /// Stops a transfer for good. The manager forgets it once its peers,
    /// announces and workers have drained, with its files when asked; until
    /// then it stays paused, also across a restart.
    pub fn remove(&self, id: &str, delete_files: bool) -> Result<()> {
        if cross_seed::reused_by(&self.config, id)? {
            return Err("A cross-seed transfer seeds from these files; remove it first".into());
        }
        let gate = cross_seed::gate(self, id)?;
        let _admission = gate
            .as_ref()
            .map(|gate| {
                gate.lock()
                    .map_err(|_| "Cross-seed activity lock is poisoned")
            })
            .transpose()?;
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        let job = jobs.get_mut(id).ok_or("Unknown download")?;
        // Paused as `cancel` does, so an interrupted removal leaves a paused transfer.
        atomic_write(
            &self.config.state_dir.join(format!("{id}.paused")),
            b"paused\n",
        )?;
        settle_seed_clock(job);
        job.paused = true;
        // A cross-seed transfer reads another transfer's files: it has none of its own.
        job.removing = Some(delete_files && job.cross_seed.is_none());
        job.retiring = true;
        job.cancel.store(true, Ordering::Release);
        job.status.message = "Removing".into();
        persist_control(&self.config, job)
    }
    /// Resume using the cached, authenticated identity; never reload a source URL.
    pub fn resume(&self, id: &str) -> Result<DownloadStatus> {
        let gate = cross_seed::gate(self, id)?;
        let _admission = gate
            .as_ref()
            .map(|gate| {
                gate.lock()
                    .map_err(|_| "Cross-seed activity lock is poisoned")
            })
            .transpose()?;
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        let job = jobs.get_mut(id).ok_or("Unknown download")?;
        let user_paused = job.control.user_paused;
        let mut control = snapshot_control(job);
        control.user_paused = false;
        atomic_write(
            &self.config.state_dir.join(format!("{id}.control")),
            &control.encode()?,
        )?;
        job.control = control;
        resume_job(&self.config, job)?;
        if user_paused && !job.control.seed_limited {
            job.cancel.store(true, Ordering::Release);
            job.cancel = Arc::new(AtomicBool::new(false));
            job.status.message = if job.status.ready {
                "Download verified and available"
            } else {
                "Download resumed"
            }
            .into();
        }
        cross_seed::retire_proof(job);
        persist_index(job)?;
        Ok(job.status.clone())
    }
    pub fn resume_if_allowed(&self, id: &str) -> Result<DownloadStatus> {
        let gate = cross_seed::gate(self, id)?;
        let _admission = gate
            .as_ref()
            .map(|gate| {
                gate.lock()
                    .map_err(|_| "Cross-seed activity lock is poisoned")
            })
            .transpose()?;
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        let job = jobs.get_mut(id).ok_or("Unknown download")?;
        if !job.control.user_paused {
            resume_job(&self.config, job)?;
            cross_seed::retire_proof(job);
        }
        persist_index(job)?;
        Ok(job.status.clone())
    }
    pub fn pause(&self, id: &str) -> Result<()> {
        let gate = cross_seed::gate(self, id)?;
        let _admission = gate
            .as_ref()
            .map(|gate| {
                gate.lock()
                    .map_err(|_| "Cross-seed activity lock is poisoned")
            })
            .transpose()?;
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        let job = jobs.get_mut(id).ok_or("Unknown download")?;
        let (mut control, partial) = clock_control(job);
        control.user_paused = true;
        let encoded = control.encode()?;
        atomic_write(
            &self.config.state_dir.join(format!("{id}.control")),
            &encoded,
        )?;
        job.control = control;
        job.saved_control = encoded;
        job.seed_partial = partial;
        job.seed_clock = None;
        job.cancel.store(true, Ordering::Release);
        job.status.message = "Download paused".into();
        persist_index(job)
    }
    pub fn controls(&self, id: &str) -> Result<TorrentControl> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        Ok(snapshot_control(jobs.get(id).ok_or("Unknown download")?))
    }
    fn change_control(
        &self,
        id: &str,
        edit: impl FnOnce(&mut TorrentControl, Option<&Meta>) -> Result<()>,
    ) -> Result<()> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        let job = jobs.get_mut(id).ok_or("Unknown download")?;
        let mut control = snapshot_control(job);
        edit(&mut control, job.meta.as_deref())?;
        commit_control(&self.config, job, control)
    }
    pub fn set_priority(&self, id: &str, priority: i32) -> Result<()> {
        self.change_control(id, |control, _| {
            control.priority = priority;
            control.validate()
        })
    }
    pub fn set_file_priority(&self, id: &str, index: usize, priority: FilePriority) -> Result<()> {
        self.change_control(id, |control, meta| {
            if meta
                .and_then(|meta| meta.files.get(index))
                .is_none_or(|file| file.padding)
            {
                return Err("Unknown torrent file index".into());
            }
            if priority == FilePriority::Normal {
                control.file_priorities.remove(&index);
            } else {
                control.file_priorities.insert(index, priority);
            }
            Ok(())
        })
    }
    pub fn set_policy(&self, id: &str, policy: Option<TransferPolicy>) -> Result<()> {
        let gate = cross_seed::gate(self, id)?;
        let _admission = gate
            .as_ref()
            .map(|gate| {
                gate.lock()
                    .map_err(|_| "Cross-seed activity lock is poisoned")
            })
            .transpose()?;
        if let Some(policy) = &policy {
            policy.validate()?;
        }
        let retiring = {
            let mut jobs = self
                .jobs
                .lock()
                .map_err(|_| "BitTorrent state lock is poisoned")?;
            self.load(&mut jobs, id)?;
            let job = jobs.get_mut(id).ok_or("Unknown download")?;
            if !job.status.ready {
                let mut control = snapshot_control(job);
                control.policy = policy;
                control.seed_limited = false;
                return commit_control(&self.config, job, control);
            }
            settle_seed_clock(job);
            job.cancel.store(true, Ordering::Release);
            job.cancel.clone()
        };
        // Retire ready-job upload workers before publishing a tighter policy.
        // Their guards release the reserved budget without holding this lock.
        loop {
            let outstanding = {
                let mut jobs = self
                    .jobs
                    .lock()
                    .map_err(|_| "BitTorrent state lock is poisoned")?;
                self.load(&mut jobs, id)?;
                let job = jobs.get(id).ok_or("Unknown download")?;
                if !Arc::ptr_eq(&job.cancel, &retiring) {
                    return Err(
                        "Transfer activity changed while updating the seeding policy".into(),
                    );
                }
                job.upload_reserved
            };
            if outstanding == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        let job = jobs.get_mut(id).ok_or("Unknown download")?;
        if !Arc::ptr_eq(&job.cancel, &retiring) || job.upload_reserved != 0 {
            return Err("Transfer activity changed while updating the seeding policy".into());
        }
        let mut control = snapshot_control(job);
        control.policy = policy;
        control.seed_limited = false;
        if let Err(error) = commit_control(&self.config, job, control) {
            if Arc::ptr_eq(&job.cancel, &retiring)
                && !job.paused
                && !job.control.user_paused
                && !job.control.seed_limited
            {
                job.cancel = Arc::new(AtomicBool::new(false));
                cross_seed::retire_proof(job);
            }
            return Err(error);
        }
        if Arc::ptr_eq(&job.cancel, &retiring) && !job.paused && !job.control.user_paused {
            job.cancel = Arc::new(AtomicBool::new(false));
            job.status.message = "Download verified and available".into();
        }
        cross_seed::retire_proof(job);
        apply_seed_limits(job, &self.config, &self.policy.policy);
        persist_control(&self.config, job)
    }
    pub fn transfer(&self, id: &str) -> Result<crate::json::Value> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        let job = jobs.get(id).ok_or("Unknown download")?;
        let mut value = transfer_value(
            job,
            0,
            &self.policy.policy,
            &self.config,
            &self.policy.seed_queue,
        );
        transfer_details(job, &self.config, &mut value);
        Ok(value)
    }
    pub fn transfers(&self) -> Result<crate::json::Value> {
        Ok(self
            .transfers_page(50, None)?
            .get("transfers")
            .cloned()
            .ok_or("Missing transfer page")?)
    }
    pub fn transfers_page(&self, limit: usize, after: Option<&str>) -> Result<crate::json::Value> {
        use crate::json::Value as Json;
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        let (ids, next, total, sequence) = self
            .index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?
            .page(after, limit)?;
        let mut values = Vec::with_capacity(ids.len());
        for (position, id) in ids.iter().enumerate() {
            if let Some(job) = jobs.get(id) {
                values.push(transfer_value(
                    job,
                    position,
                    &self.policy.policy,
                    &self.config,
                    &self.policy.seed_queue,
                ));
            } else {
                match indexed_job(&self.config, &self.index, id, &self.policy.policy) {
                    Ok(job) => values.push(transfer_value(
                        &job,
                        position,
                        &self.policy.policy,
                        &self.config,
                        &self.policy.seed_queue,
                    )),
                    Err(error) => {
                        // A damaged inactive metadata file must not hide every
                        // following transfer by preventing a continuation page.
                        // The descriptor remains authenticated by the index;
                        // reporting it grants no payload or network authority.
                        let record = self
                            .index
                            .lock()
                            .map_err(|_| "Native transfer index lock is poisoned")?
                            .get(id)?
                            .ok_or("Missing indexed transfer")?;
                        let mut value = record.control.to_json();
                        value.insert("ready", false);
                        value.insert("selected_ready", false);
                        value.insert("running", false);
                        value.insert("failed", true);
                        value.insert("paused", record.paused || record.control.user_paused);
                        value.insert("status", "failed");
                        value.insert("seed_only", record.cross_seed);
                        value.insert("cross_seed_source_id", Json::Null);
                        value.insert("queue_position", Json::Number(position as f64));
                        value.insert("progress", Json::Number(0.0));
                        value.insert("verified_bytes", "0");
                        value.insert("message", error);
                        value.insert("files", Json::Array(Vec::new()));
                        values.push(value);
                    }
                }
            }
        }
        let mut page = Json::object();
        page.insert("transfers", Json::Array(values));
        page.insert("next", next.map(Json::String).unwrap_or(Json::Null));
        page.insert("total", Json::Number(total as f64));
        page.insert("sequence", sequence.to_string());
        page.insert(
            "revision",
            self.index
                .lock()
                .map_err(|_| "Native transfer index lock is poisoned")?
                .revision()?
                .to_string(),
        );
        Ok(page)
    }
    pub fn check(&self, id: &str) -> Result<DownloadStatus> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, id)?;
        let job = jobs.get(id).ok_or("Unknown download")?;
        if job.failed {
            return Err(job.status.message.clone());
        }
        let mut status = job.status.clone();
        if !job.admitted {
            status.ready = false;
            status.selected_ready = false;
            status.available_files.clear();
        }
        Ok(status)
    }
    pub fn statuses(&self) -> Result<Vec<DownloadStatus>> {
        Ok(self.statuses_page(None, 50)?.0)
    }
    pub fn statuses_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<DownloadStatus>, Option<String>)> {
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        let mut index = self
            .index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?;
        let (ids, next, _, _) = index.primary_page(after, limit)?;
        let mut values = Vec::with_capacity(ids.len());
        for id in ids {
            values.push(if let Some(job) = jobs.get(&id) {
                job.status.clone()
            } else {
                record_status(
                    &self.config,
                    &index.get(&id)?.ok_or("Missing indexed transfer")?,
                )?
            });
        }
        Ok((values, next))
    }
    pub fn contains(&self, id: &str) -> Result<bool> {
        Ok(self
            .index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?
            .get(id)?
            .is_some())
    }
    /// Transfer index size and state for diagnostics; `busy` while another
    /// operation holds the index, so status never waits on it.
    pub fn storage(&self) -> Result<crate::json::Value> {
        match self.index.try_lock() {
            Ok(index) => Ok(index.storage()),
            Err(std::sync::TryLockError::WouldBlock) => Ok(crate::inventory::report("busy", None)),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                Err("Native transfer index lock is poisoned".into())
            }
        }
    }
    pub(crate) fn backlog(&self, at: u64) -> crate::json::Value {
        match self.index.try_lock() {
            Ok(mut index) => index
                .backlog(at)
                .unwrap_or_else(|_| crate::inventory::backlog::unavailable()),
            Err(std::sync::TryLockError::WouldBlock) => crate::inventory::report("busy", None),
            Err(std::sync::TryLockError::Poisoned(_)) => crate::inventory::backlog::unavailable(),
        }
    }
    pub fn aggregate_stats(&self) -> Result<(u64, u64, u64)> {
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        let mut index = self
            .index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?;
        aggregate(&jobs, &mut index)
    }
    /// As `aggregate_stats`, but `None` while another operation holds the
    /// transfer state, so status never waits on it.
    pub fn try_aggregate_stats(&self) -> Result<Option<(u64, u64, u64)>> {
        use std::sync::TryLockError;
        let jobs = match self.jobs.try_lock() {
            Ok(jobs) => jobs,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Poisoned(_)) => {
                return Err("BitTorrent state lock is poisoned".into());
            }
        };
        let mut index = match self.index.try_lock() {
            Ok(index) => index,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Poisoned(_)) => {
                return Err("Native transfer index lock is poisoned".into());
            }
        };
        aggregate(&jobs, &mut index).map(Some)
    }
    pub fn transfer_stats(&self, id: &str) -> Result<(u64, u64)> {
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        if let Some(job) = jobs.get(id) {
            return Ok((
                job.counters.downloaded.load(Ordering::Relaxed),
                job.counters.uploaded.load(Ordering::Relaxed),
            ));
        }
        let record = self
            .index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?
            .get(id)?
            .ok_or("Unknown download")?;
        Ok((
            record.control.downloaded_bytes,
            record.control.uploaded_bytes,
        ))
    }
    fn load(&self, jobs: &mut BTreeMap<String, Job>, id: &str) -> Result<()> {
        if !jobs.contains_key(id) {
            trim_jobs(&self.config, jobs, self.config.download_slots() + 31)?;
            if jobs.len() >= self.config.download_slots() + 64 {
                return Err("Native transfer working set is busy; retry later".into());
            }
            let mut job = indexed_job(&self.config, &self.index, id, &self.policy.policy)?;
            job.admitted = self
                .admission
                .as_ref()
                .is_none_or(|gate| gate(id).unwrap_or(false));
            job.seed_queued = self.policy.seed_queue.waiting(self.config.seed, &job);
            if job.seed_queued {
                job.cancel.store(true, Ordering::Release);
            }
            persist_index(&job)?;
            jobs.insert(id.to_owned(), job);
        }
        Ok(())
    }
    pub fn listen_port(&self) -> u16 {
        self.config.listen_port
    }
}

/// Transfer count and byte totals: durable index counters plus each live
/// job's progress since its last saved record.
fn aggregate(
    jobs: &BTreeMap<String, Job>,
    index: &mut index::TransferIndex,
) -> Result<(u64, u64, u64)> {
    let (total, mut down, mut up) = index.aggregates()?;
    for (id, job) in jobs.iter() {
        if let Some(record) = index.get(id)? {
            down = down.saturating_add(
                job.counters
                    .downloaded
                    .load(Ordering::Relaxed)
                    .saturating_sub(record.control.downloaded_bytes),
            );
            up = up.saturating_add(
                job.counters
                    .uploaded
                    .load(Ordering::Relaxed)
                    .saturating_sub(record.control.uploaded_bytes),
            );
        }
    }
    Ok((total, down, up))
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(mut jobs) = self.jobs.lock() {
            for job in jobs.values_mut() {
                settle_seed_clock(job);
                job.cancel.store(true, Ordering::Release);
            }
        }
        if let Some(handle) = self.manager.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
        if let Ok(mut jobs) = self.jobs.lock() {
            for job in jobs.values_mut() {
                if let Err(error) = persist_control(&self.config, job) {
                    eprintln!("Could not flush native transfer controls: {error}");
                }
            }
        }
    }
}

fn resume_job(config: &DownloadConfig, job: &mut Job) -> Result<()> {
    if job.removing.is_some() {
        return Err("This transfer is being removed".into());
    }
    if job.paused || job.failed {
        let marker = config.state_dir.join(format!("{}.paused", job.status.id));
        if fs::symlink_metadata(&marker).is_ok() {
            fs::remove_file(marker).map_err(|_| "Could not resume download")?;
            crate::store::sync_directory(&config.state_dir)
                .map_err(|_| "Could not synchronize resumed state")?;
        }
        job.paused = false;
        job.failed = false;
        job.cancel.store(true, Ordering::Release);
        job.cancel = Arc::new(AtomicBool::new(job.control.seed_limited));
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
    cancel: Arc<AtomicBool>,
}
fn select_announce(
    config: &DownloadConfig,
    jobs: &Jobs,
    cursor: &mut Option<String>,
) -> Option<AnnounceTask> {
    let mut jobs = jobs.lock().ok()?;
    let now = Instant::now();
    let ids: Vec<_> = jobs.keys().cloned().collect();
    let split = ids.partition_point(|id| cursor.as_ref().is_some_and(|cursor| id <= cursor));
    for id in ids[split..].iter().chain(&ids[..split]) {
        let job = jobs.get_mut(id)?;
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
                m.files
                    .iter()
                    .filter(|file| !file.padding)
                    .map(|file| file.length)
                    .sum::<u64>()
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
            let inactive = job.paused
                || job.retiring
                || job.seed_queued
                || !job.admitted
                || job.persistence_failed.load(Ordering::Acquire)
                || job.cancel.load(Ordering::Acquire)
                || job.needs_rehash
                || (job.cross_seed.is_some() && !job.status.ready)
                || job.control.user_paused
                || job.control.seed_limited
                || job.failed
                || (job.status.selected_ready && !job.status.ready);
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
                *cursor = Some(id.clone());
                return Some(AnnounceTask {
                    id: id.clone(),
                    url: url.clone(),
                    hash,
                    left,
                    downloaded,
                    uploaded,
                    event,
                    cancel: job.cancel.clone(),
                });
            }
        }
    }
    None
}
fn announce(
    network: &dyn transport::Network,
    task: AnnounceTask,
    port: u16,
    peer_id: [u8; 20],
    jobs: &Jobs,
) {
    let reuse = jobs
        .lock()
        .ok()
        .and_then(|map| map.get(&task.id).and_then(|job| job.cross_seed.clone()));
    let _admission = match reuse
        .as_ref()
        .map(|reuse| reuse.activity.lock())
        .transpose()
    {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if reuse.is_some() {
        let Ok(mut map) = jobs.lock() else {
            return;
        };
        let Some(job) = map.get_mut(&task.id) else {
            return;
        };
        if !Arc::ptr_eq(&job.cancel, &task.cancel)
            || (task.event != discovery::TrackerEvent::Stopped
                && (!job.status.ready
                    || job.failed
                    || job.paused
                    || job.control.user_paused
                    || job.control.seed_limited
                    || task.cancel.load(Ordering::Acquire)))
        {
            if let Some(state) = job.announcements.get_mut(&task.url) {
                state.in_flight = false;
            }
            return;
        }
    }
    let request = discovery::TrackerRequest {
        hash: &task.hash,
        peer_id: &peer_id,
        port,
        left: task.left,
        downloaded: task.downloaded,
        uploaded: task.uploaded,
        event: task.event,
    };
    let result = discovery::tracker(network, &task.url, &request);
    let publication = network.publication();
    let result = result.and_then(|reply| {
        publication.as_ref().map_err(Clone::clone)?;
        Ok(reply)
    });
    if let Ok(mut jobs) = jobs.lock()
        && let Some(job) = jobs.get_mut(&task.id)
        && (job.cross_seed.is_none() || Arc::ptr_eq(&job.cancel, &task.cancel))
        && let Some(state) = job.announcements.get_mut(&task.url)
    {
        state.in_flight = false;
        state.last_at = crate::store::now();
        match result {
            Ok(response) => {
                state.retry_after = Instant::now();
                state.next = Instant::now() + response.interval;
                state.last_error = None;
                state.seeders = response.seeders;
                state.leechers = response.leechers;
                state.peers = response.peers.len();
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
                if task.event != discovery::TrackerEvent::Stopped
                    && Arc::ptr_eq(&job.cancel, &task.cancel)
                    && !task.cancel.load(Ordering::Acquire)
                    && !job.paused
                    && !job.control.user_paused
                    && !job.control.seed_limited
                    && !job.failed
                {
                    merge_peers(&mut job.tracker_peers, response.peers);
                }
            }
            Err(error) => {
                state.next = Instant::now() + Duration::from_secs(30);
                state.retry_after = state.next;
                state.last_error = Some(crate::integrations::report_text(&error, 256));
            }
        }
    }
}

struct DhtTask {
    id: String,
    hash: [u8; 20],
    cancel: Arc<AtomicBool>,
}
fn select_seed_dht(
    config: &DownloadConfig,
    jobs: &Jobs,
    cursor: &mut Option<String>,
) -> Option<DhtTask> {
    if !config.dht || !config.seed {
        return None;
    }
    let mut jobs = jobs.lock().ok()?;
    let now = Instant::now();
    let ids: Vec<_> = jobs.keys().cloned().collect();
    let split = ids.partition_point(|id| cursor.as_ref().is_some_and(|cursor| id <= cursor));
    for id in ids[split..].iter().chain(&ids[..split]) {
        let job = jobs.get_mut(id)?;
        if !job.status.ready
            || job.seed_queued
            || !job.admitted
            || job.persistence_failed.load(Ordering::Acquire)
            || job.retiring
            || job.cancel.load(Ordering::Acquire)
            || job.needs_rehash
            || job.paused
            || job.control.user_paused
            || job.control.seed_limited
            || job.failed
            || job.dht_in_flight
            || now < job.dht_next
            || job.meta.as_ref().is_none_or(|m| m.private)
        {
            continue;
        }
        job.dht_in_flight = true;
        *cursor = Some(id.clone());
        return Some(DhtTask {
            id: id.clone(),
            hash: job.announce_hash.unwrap_or_else(|| job.source.wire_hash()),
            cancel: job.cancel.clone(),
        });
    }
    None
}
fn announce_seed_dht(network: &dyn transport::Network, task: DhtTask, port: u16, jobs: &Jobs) {
    let reuse = jobs
        .lock()
        .ok()
        .and_then(|map| map.get(&task.id).and_then(|job| job.cross_seed.clone()));
    let _admission = match reuse
        .as_ref()
        .map(|reuse| reuse.activity.lock())
        .transpose()
    {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if reuse.is_some() {
        let Ok(mut map) = jobs.lock() else {
            return;
        };
        let Some(job) = map.get_mut(&task.id) else {
            return;
        };
        if !Arc::ptr_eq(&job.cancel, &task.cancel)
            || !job.status.ready
            || job.seed_queued
            || job.failed
            || job.paused
            || job.control.user_paused
            || job.control.seed_limited
            || task.cancel.load(Ordering::Acquire)
            || job.meta.as_ref().is_none_or(|meta| meta.private)
        {
            job.dht_in_flight = false;
            return;
        }
    }
    let result = discovery::dht_lookup(network, &task.hash, port, true, &task.cancel, &[]);
    let publication = network.publication();
    let result = result.and_then(|reply| {
        publication.as_ref().map_err(Clone::clone)?;
        Ok(reply)
    });
    if let Ok(mut jobs) = jobs.lock()
        && let Some(job) = jobs.get_mut(&task.id)
    {
        job.dht_in_flight = false;
        job.dht_next = if Arc::ptr_eq(&job.cancel, &task.cancel) {
            Instant::now() + Duration::from_secs(600)
        } else {
            Instant::now()
        };
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

fn manager_loop(
    config: DownloadConfig,
    jobs: Jobs,
    stop: Arc<AtomicBool>,
    peer_id: [u8; 20],
    policy: Arc<PolicyRuntime>,
    index: Arc<Mutex<index::TransferIndex>>,
    admission: Option<AdmissionGate>,
) {
    let mut flushed = Instant::now();
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    let mut announcements: Vec<JoinHandle<()>> = Vec::new();
    let mut tracker_cursor = None;
    let mut dht_cursor = None;
    let mut dht_turn = false;
    let mut queue_cursor = None;
    let mut seed_cursor = None;
    let mut seed_rotation = Instant::now();
    let mut seed_scan = queue::SeedScan::default();
    while !stop.load(Ordering::Acquire) {
        if policy.network.generation().is_err() {
            // Keep controls and verified data available for a new protected
            // engine; generation loss is not a failed transfer.
            thread::sleep(Duration::from_millis(30));
            continue;
        }
        if index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned".to_owned())
            .and_then(|catalog| catalog.revision())
            .is_err()
        {
            stop.store(true, Ordering::Release);
            break;
        }
        let mut n = 0;
        while n < workers.len() {
            if workers[n].is_finished() {
                let handle = workers.swap_remove(n);
                let _ = handle.join();
            } else {
                n += 1;
            }
        }
        if let Ok(mut map) = jobs.lock() {
            if let Err(error) =
                policy
                    .seed_queue
                    .advance(&index, &mut seed_scan, admission.as_ref())
            {
                eprintln!("Could not read seed queue: {error}");
                stop.store(true, Ordering::Release);
                break;
            }
            for job in map.values_mut() {
                let queued = policy.seed_queue.waiting(config.seed, job);
                if queued != job.seed_queued {
                    if queued {
                        settle_seed_clock(job);
                        job.cancel.store(true, Ordering::Release);
                    } else if !job.paused
                        && !job.control.user_paused
                        && !job.retiring
                        && !job.failed
                        && !job.control.seed_limited
                    {
                        job.cancel = Arc::new(AtomicBool::new(false));
                    }
                    job.seed_queued = queued;
                }
            }
            if let Err(error) = trim_jobs(&config, &mut map, config.download_slots() + 32) {
                eprintln!("Could not retire indexed transfer: {error}");
            }
            // Rotation only makes room: when every retained seed fits in the
            // working set, retiring them would only reverify their data.
            let seeds_waiting = seed_cursor.is_some() || map.len() >= config.download_slots() + 32;
            if config.seed && seeds_waiting && seed_rotation.elapsed() >= Duration::from_secs(30) {
                // Retire a bounded seed window. Existing peer/announcement guards
                // pin jobs until cancellation has drained their activity.
                let ids: Vec<_> = map
                    .iter()
                    .filter(|(_, j)| j.status.ready && !j.running && j.upload_reserved == 0)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    if let Some(job) = map.get_mut(&id) {
                        settle_seed_clock(job);
                        job.retiring = true;
                        job.cancel.store(true, Ordering::Release);
                    }
                }
                seed_rotation = Instant::now();
            }
            let retired: Vec<_> = map
                .iter()
                .filter(|(_, j)| {
                    j.retiring
                        && !j.running
                        && j.upload_reserved == 0
                        && j.cross_seed
                            .as_ref()
                            .is_none_or(|reuse| Arc::strong_count(&reuse.activity) == 1)
                        && Arc::strong_count(&j.cancel) == 1
                        && !j.dht_in_flight
                        && j.announcements
                            .values()
                            .all(|a| !a.in_flight && (!a.started || a.stopped))
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in retired {
                if let Some(mut job) = map.remove(&id) {
                    let result = match job.removing {
                        Some(delete_files) => forget(&config, &index, &id, delete_files),
                        None => persist_control(&config, &mut job),
                    };
                    if let Err(error) = result {
                        eprintln!("Could not retire transfer {id}: {error}");
                        map.insert(id, job);
                    }
                }
            }
            let candidates = index
                .lock()
                .map_err(|_| "Native transfer index lock is poisoned".to_owned())
                .and_then(|mut catalog| {
                    // Refresh the leading window on every scheduling turn so
                    // completed jobs cannot leave holes in priority/FIFO order.
                    let (mut head, _) = catalog.queue_page(None, config.download_slots() + 32)?;
                    let (tail, next) = catalog.queue_page(queue_cursor.as_deref(), 32)?;
                    let mut present: BTreeSet<_> = head.iter().cloned().collect();
                    head.extend(tail.into_iter().filter(|id| present.insert(id.clone())));
                    if !stop.load(Ordering::Acquire) {
                        catalog.finish_backlog_pass(crate::store::now())?;
                    }
                    Ok((head, next))
                });
            match candidates {
                Ok((ids, next)) => {
                    queue_cursor = next;
                    for id in ids {
                        if map.len() >= config.download_slots() + 64 {
                            break;
                        }
                        if let std::collections::btree_map::Entry::Vacant(slot) =
                            map.entry(id.clone())
                        {
                            match indexed_job(&config, &index, &id, &policy.policy) {
                                Ok(mut job) => {
                                    job.seed_queued = policy.seed_queue.waiting(config.seed, &job);
                                    if job.seed_queued {
                                        job.cancel.store(true, Ordering::Release);
                                    }
                                    slot.insert(job);
                                }
                                Err(error) => {
                                    eprintln!("Could not load indexed transfer: {error}");
                                    if let Ok(mut catalog) = index.lock() {
                                        let _ = catalog.mark_failed(&id, &error);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(error) => eprintln!("Could not read indexed queue: {error}"),
            }
            if config.seed && map.len() < config.download_slots() + 32 {
                let candidates = index
                    .lock()
                    .map_err(|_| "Native transfer index lock is poisoned".to_owned())
                    .and_then(|mut catalog| {
                        catalog.seeds_page(
                            seed_cursor.as_deref(),
                            (config.download_slots() + 32 - map.len()).min(32),
                        )
                    });
                match candidates {
                    Ok((ids, next)) => {
                        seed_cursor = next;
                        for id in ids {
                            if let std::collections::btree_map::Entry::Vacant(slot) =
                                map.entry(id.clone())
                            {
                                match indexed_job(&config, &index, &id, &policy.policy) {
                                    Ok(mut job) => {
                                        job.seed_queued =
                                            policy.seed_queue.waiting(config.seed, &job);
                                        if job.seed_queued {
                                            job.cancel.store(true, Ordering::Release);
                                        }
                                        slot.insert(job);
                                    }
                                    Err(error) => {
                                        eprintln!("Could not load retained seed: {error}");
                                        if let Ok(mut catalog) = index.lock() {
                                            let _ = catalog.mark_failed(&id, &error);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => eprintln!("Could not read indexed seeds: {error}"),
                }
            }
        }
        if let Ok(mut map) = jobs.lock() {
            for (id, job) in map.iter_mut() {
                if !job.admitted {
                    job.admitted = admission
                        .as_ref()
                        .is_none_or(|gate| gate(id).unwrap_or(false));
                }
            }
        }
        if workers.len() < config.download_slots() {
            let selected = jobs.lock().ok().and_then(|mut jobs| {
                let id = jobs
                    .iter()
                    .filter(|(_, j)| {
                        j.admitted
                            && !j.persistence_failed.load(Ordering::Acquire)
                            && !j.retiring
                            && !j.seed_queued
                            && !j.running
                            && !j.paused
                            && !j.control.user_paused
                            && !j.failed
                            && ((!j.status.ready && !j.status.selected_ready)
                                || (j.needs_rehash && config.seed && !j.control.seed_limited))
                    })
                    .min_by(|(aid, a), (bid, b)| {
                        b.control
                            .priority
                            .cmp(&a.control.priority)
                            .then_with(|| a.control.queue_order.cmp(&b.control.queue_order))
                            .then_with(|| aid.cmp(bid))
                    })
                    .map(|(id, _)| id.clone())?;
                let job = jobs.get_mut(&id)?;
                if job.needs_rehash {
                    job.needs_rehash = false;
                    job.status.ready = false;
                    job.status.selected_ready = false;
                    job.status.available_files.clear();
                    job.status.progress = 0.0;
                    job.status.message = "Verifying persisted data".into();
                }
                job.running = true;
                if job.cancel.load(Ordering::Acquire) {
                    job.cancel.store(true, Ordering::Release);
                    job.cancel = Arc::new(AtomicBool::new(false));
                }
                Some((id, job.cancel.clone()))
            });
            if let Some((id, st)) = selected {
                let cfg = config.clone();
                let js = jobs.clone();
                let policy = policy.clone();
                workers.push(thread::spawn(move || {
                    if let Err(error) = download(&cfg, &js, &st, &id, &peer_id, &policy)
                        && let Ok(mut jobs) = js.lock()
                        && let Some(job) = jobs.get_mut(&id)
                        && !job.paused
                        && !st.load(Ordering::Acquire)
                        && Arc::ptr_eq(&job.cancel, &st)
                    {
                        if policy.network.generation().is_ok() {
                            job.failed = true;
                            job.status.message = error;
                        } else {
                            job.status.message =
                                "Torrent network unavailable; waiting for recovery".into();
                        }
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
        enum DiscoveryTask {
            Tracker(AnnounceTask),
            Dht(DhtTask),
        }
        while announcements.len() < config.download_slots().min(4) {
            let task = if dht_turn {
                select_seed_dht(&config, &jobs, &mut dht_cursor)
                    .map(DiscoveryTask::Dht)
                    .or_else(|| {
                        select_announce(&config, &jobs, &mut tracker_cursor)
                            .map(DiscoveryTask::Tracker)
                    })
            } else {
                select_announce(&config, &jobs, &mut tracker_cursor)
                    .map(DiscoveryTask::Tracker)
                    .or_else(|| {
                        select_seed_dht(&config, &jobs, &mut dht_cursor).map(DiscoveryTask::Dht)
                    })
            };
            let Some(task) = task else { break };
            dht_turn = !dht_turn;
            let jobs = jobs.clone();
            let port = config.listen_port;
            let network = policy.network.clone();
            announcements.push(thread::spawn(move || match task {
                DiscoveryTask::Tracker(task) => announce(&*network, task, port, peer_id, &jobs),
                DiscoveryTask::Dht(task) => announce_seed_dht(&*network, task, port, &jobs),
            }));
        }
        let flush = if let Ok(mut map) = jobs.lock() {
            for job in map.values_mut() {
                apply_seed_limits(job, &config, &policy.policy);
            }
            // Byte counters and progress change every second while a transfer
            // moves, and on macOS each durable sync also flushes the disk cache
            // full of downloaded data: saving them every second kept this lock
            // in flushes for about half of each second. Commands are saved when
            // given, `Client::drop` saves everything on shutdown, and verified
            // pieces are found again on restart.
            (flushed.elapsed() >= Duration::from_secs(30))
                .then(|| map.keys().cloned().collect::<Vec<_>>())
        } else {
            None
        };
        if let Some(ids) = flush {
            // One job per lock hold: each save is several durable syncs, and
            // peer workers, inbound peers and status calls need this lock.
            for id in ids {
                if let Ok(mut map) = jobs.lock()
                    && let Some(job) = map.get_mut(&id)
                    && persist_control(&config, job).is_err()
                {
                    job.failed = true;
                    job.cancel.store(true, Ordering::Release);
                    job.status.message = "Could not persist native transfer controls".into();
                }
            }
            flushed = Instant::now();
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
    let mut jobs = jobs
        .lock()
        .map_err(|_| "BitTorrent state lock is poisoned")?;
    let job = jobs.get_mut(id).ok_or("Download no longer exists")?;
    if job.paused
        || job.control.user_paused
        || stop.load(Ordering::Acquire)
        || !std::ptr::eq(&*job.cancel, stop)
    {
        return Err("Download interrupted".into());
    }
    let completed = ready && !job.status.ready;
    job.status.progress = progress;
    job.status.ready = ready;
    if ready {
        job.status.selected_ready = true;
    }
    job.status.message = message.to_owned();
    if let Some(meta) = meta {
        if job.meta.is_none() || ready {
            job.meta = Some(Arc::new(meta.clone()));
            job.piece_order = Arc::new(piece_order(meta, &job.control.file_priorities));
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
    if ready {
        job.status.available_files = job.status.files.clone();
    }
    // The seed queue reads readiness from the index: save a completion now
    // rather than at the next periodic save, up to 30 seconds later.
    if completed {
        persist_index(job)?;
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
    policy: &PolicyRuntime,
) -> Result<()> {
    policy.network.publication()?;
    let seed_only = jobs
        .lock()
        .map_err(|_| "BitTorrent state lock is poisoned")?
        .get(id)
        .ok_or("Download no longer exists")?
        .cross_seed
        .is_some();
    if seed_only {
        return cross_seed::verify(config, jobs, stop, id);
    }
    let full = jobs
        .lock()
        .map_err(|_| "BitTorrent state lock is poisoned")?
        .get(id)
        .ok_or("Download no longer exists")?
        .control
        .file_selection
        == FileSelection::All;
    if config.max_peers == 1 && full {
        download_sequential(config, jobs, stop, id, peer_id, policy)
    } else {
        parallel::download(config, jobs, stop, id, peer_id, policy)
    }
}

fn download_sequential(
    config: &DownloadConfig,
    jobs: &Jobs,
    stop: &AtomicBool,
    id: &str,
    peer_id: &[u8; 20],
    policy: &PolicyRuntime,
) -> Result<()> {
    let (mut source, mut meta, counters) = {
        let jobs = jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
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
    let mut addresses: BTreeSet<SocketAddr> = source
        .peer_addresses(&*policy.network, Instant::now() + Duration::from_secs(20))?
        .into_iter()
        .collect();
    let hash = source.wire_hash();
    let v2_wire = source.v1.is_none();
    let mut next_dht = Instant::now();
    while !stop.load(Ordering::Acquire) {
        if let Some(m) = &meta {
            if !scanned {
                let fresh = prepare_files(m, &base, stop)?;
                have = verify_existing(m, &base, stop, &fresh);
                counters.verified.store(
                    have.iter()
                        .enumerate()
                        .filter(|(_, v)| **v)
                        .map(|(i, _)| verified_piece_bytes(m, i))
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
                let _publication = policy.network.publication()?;
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
            let peers = discovery::dht_lookup(
                &*policy.network,
                &hash,
                config.listen_port,
                known,
                stop,
                &[],
            )
            .unwrap_or_default();
            next_dht = Instant::now()
                + if peers.is_empty() {
                    discovery::DHT_RETRY
                } else {
                    Duration::from_secs(60)
                };
            merge_peers(&mut addresses, peers);
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
                    download_gate: policy.download.clone(),
                    local_download_gate: jobs
                        .lock()
                        .map_err(|_| "BitTorrent state lock is poisoned")?
                        .get(id)
                        .ok_or("Download no longer exists")?
                        .download_gate
                        .clone(),
                    network: &*policy.network,
                },
            ) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if meta.is_none() {
                match peer.metadata(source.v1, source.v2, stop) {
                    Ok(m) => {
                        let publication = policy.network.publication()?;
                        let untrusted_private =
                            m.private && source.trackers.is_empty() && source.peers.is_empty();
                        source.v1 = m.v1;
                        source.v2 = m.v2;
                        source.trackers.extend(m.trackers.clone());
                        persist_meta(config, id, &m)?;
                        let fresh = prepare_files(&m, &base, stop)?;
                        have = verify_existing(&m, &base, stop, &fresh);
                        counters.verified.store(
                            have.iter()
                                .enumerate()
                                .filter(|(_, v)| **v)
                                .map(|(i, _)| verified_piece_bytes(&m, i))
                                .sum(),
                            Ordering::Relaxed,
                        );
                        peer.pex_allowed = config.pex && !m.private;
                        update(
                            (jobs, stop),
                            id,
                            Some(&m),
                            if m.count() == 0 {
                                1.0
                            } else {
                                have.iter().filter(|present| **present).count() as f64
                                    / m.count() as f64
                            },
                            false,
                            "Torrent metadata authenticated",
                            config,
                        )?;
                        drop(publication);
                        if peer.extensions {
                            wire::write_message(
                                &mut *peer.stream,
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
            // Read the current order before each piece, so priority changes
            // take effect after the in-flight piece without skipping any file.
            loop {
                let order = {
                    let mut map = jobs
                        .lock()
                        .map_err(|_| "BitTorrent state lock is poisoned")?;
                    let job = map.get_mut(id).ok_or("Download no longer exists")?;
                    if job.piece_order.len() != m.count() {
                        job.piece_order = Arc::new(piece_order(m, &job.control.file_priorities));
                    }
                    job.piece_order.clone()
                };
                let Some(i) = order.iter().copied().find(|i| {
                    !have[*i]
                        && (peer.bitfield.is_empty()
                            || (*i / 8 < peer.bitfield.len()
                                && peer.bitfield[*i / 8] & (0x80 >> (*i % 8)) != 0))
                }) else {
                    break;
                };
                match peer.fetch_piece(m, i, &mut || None, stop) {
                    Ok(data) => {
                        let _publication = policy.network.publication()?;
                        write_piece(m, &base, i, &data)?;
                        have[i] = true;
                        counters
                            .verified
                            .fetch_add(verified_piece_bytes(m, i), Ordering::Relaxed);
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

fn prepare_files(meta: &Meta, base: &Path, stop: &AtomicBool) -> Result<Vec<bool>> {
    prepare_selected_files(meta, base, stop, None)
}
/// Creates and sizes the files, and says which held no byte before: their
/// pieces are zeros and need no reading.
fn prepare_selected_files(
    meta: &Meta,
    base: &Path,
    stop: &AtomicBool,
    selected: Option<&[bool]>,
) -> Result<Vec<bool>> {
    let mut fresh = vec![false; meta.files.len()];
    for (index, f) in meta.files.iter().enumerate() {
        if stop.load(Ordering::Acquire) {
            return Err("Download interrupted".into());
        }
        if f.padding || selected.is_some_and(|files| !files[index]) {
            continue;
        }
        let path = confined(base, &f.path)?;
        inspect_media_file(&path)?;
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
                    fs::create_dir(&current).map_err(|_| "Could not create media directory")?;
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
        let length = file
            .metadata()
            .map_err(|_| "Media file is inaccessible")?
            .len();
        fresh[index] = length == 0;
        if length != f.length {
            file.set_len(f.length)
                .map_err(|_| "Could not resize media file")?;
        }
    }
    Ok(fresh)
}

/// Which pieces the files on disk already hold, hashed on every core. A piece
/// that touches a file created empty holds zeros and is not read.
fn verify_existing(meta: &Meta, base: &Path, stop: &AtomicBool, fresh: &[bool]) -> Vec<bool> {
    let count = meta.count();
    let piece_length = meta.piece_length as u64;
    let mut skip = vec![false; count];
    for (file, _) in meta
        .files
        .iter()
        .zip(fresh)
        .filter(|(f, fresh)| **fresh && f.length > 0)
    {
        let first = (file.offset / piece_length) as usize;
        let last = ((file.offset + file.length - 1) / piece_length) as usize;
        for flag in &mut skip[first.min(count)..(last + 1).min(count)] {
            *flag = true;
        }
    }
    let next = AtomicUsize::new(0);
    let have: Vec<AtomicBool> = (0..count).map(|_| AtomicBool::new(false)).collect();
    // Each thread holds one piece; eight bound that memory and saturate a disk.
    let threads = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(8);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count || stop.load(Ordering::Acquire) {
                        break;
                    }
                    let held = !skip[index]
                        && (meta.pieces[index].is_some() || meta.v2_pieces[index].is_some())
                        && read_piece(meta, base, index)
                            .is_ok_and(|data| meta.verify(index, &data));
                    have[index].store(held, Ordering::Relaxed);
                }
            });
        }
    });
    have.into_iter().map(AtomicBool::into_inner).collect()
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
        inspect_media_file(&path)?;
        let mut file = File::open(path).map_err(|_| "Could not read media file")?;
        file.seek(SeekFrom::Start(a - f.offset))
            .and_then(|_| file.read_exact(&mut data[(a - start) as usize..(b - start) as usize]))
            .map_err(|_| "Truncated media read")?;
    }
    Ok(data)
}
/// Writes a piece that `Peer::fetch_piece` has already checked against `meta`;
/// hashing it again here would double the CPU cost of every download.
fn write_piece(meta: &Meta, base: &Path, index: usize, data: &[u8]) -> Result<()> {
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
        inspect_media_file(&path)?;
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
    sync_selected_files(meta, base, stop, None)
}
fn sync_selected_files(
    meta: &Meta,
    base: &Path,
    stop: &AtomicBool,
    selected: Option<&[bool]>,
) -> Result<()> {
    for (index, f) in meta.files.iter().enumerate() {
        if stop.load(Ordering::Acquire) {
            return Err("Download interrupted".into());
        }
        if !f.padding && selected.is_none_or(|files| files[index]) {
            let path = confined(base, &f.path)?;
            inspect_media_file(&path)?;
            OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|f| crate::store::sync_file(&f))
                .map_err(|_| "Could not synchronize media files")?;
        }
    }
    File::open(base)
        .and_then(|f| crate::store::sync_file(&f))
        .map_err(|_| "Could not synchronize download")?;
    Ok(())
}
fn authenticate_v2(meta: &mut Meta, base: &Path, stop: &AtomicBool) -> Result<()> {
    authenticate_selected_v2(meta, base, stop, None)
}
fn authenticate_selected_v2(
    meta: &mut Meta,
    base: &Path,
    stop: &AtomicBool,
    selected: Option<&[bool]>,
) -> Result<()> {
    if meta.v2.is_none() {
        return Ok(());
    }
    for (index, f) in meta.files.iter().enumerate() {
        if selected.is_some_and(|files| !files[index]) {
            continue;
        }
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
    listener: Box<dyn transport::NetworkListener>,
    config: DownloadConfig,
    jobs: Jobs,
    stop: Arc<AtomicBool>,
    peer_id: [u8; 20],
    policy: Arc<PolicyRuntime>,
    catalog: (Arc<Mutex<index::TransferIndex>>, Option<AdmissionGate>),
) {
    let (index, admission) = catalog;
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
            Ok((stream, address)) if peers.len() < 32 => {
                let cfg = config.clone();
                let js = jobs.clone();
                let st = stop.clone();
                let policy = policy.clone();
                let index = index.clone();
                let admission = admission.clone();
                peers.push(thread::spawn(move || {
                    let _ = serve_peer(
                        (stream, address),
                        &cfg,
                        &js,
                        &st,
                        &peer_id,
                        &policy,
                        (&index, admission.as_ref()),
                    );
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
struct UploadReservation {
    jobs: Jobs,
    id: String,
    bytes: u64,
}
impl Drop for UploadReservation {
    fn drop(&mut self) {
        if let Ok(mut jobs) = self.jobs.lock()
            && let Some(job) = jobs.get_mut(&self.id)
        {
            job.upload_reserved = job.upload_reserved.saturating_sub(self.bytes);
        }
    }
}
fn peer_available(
    job: &mut Job,
    config: &DownloadConfig,
    defaults: &TransferPolicy,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    apply_seed_limits(job, config, defaults);
    if job.persistence_failed.load(Ordering::Acquire) {
        return Err("Native transfer persistence is unavailable".into());
    }
    if !job.admitted {
        return Err("Transfer ownership has not been admitted".into());
    }
    if job.paused
        || job.seed_queued
        || job.control.user_paused
        || job.control.seed_limited
        || job.failed
        || cancel.load(Ordering::Acquire)
        || !Arc::ptr_eq(&job.cancel, cancel)
    {
        return Err("Torrent peer activity is suspended".into());
    }
    Ok(())
}
fn reserve_upload(
    jobs: &Jobs,
    id: &str,
    bytes: u64,
    config: &DownloadConfig,
    defaults: &TransferPolicy,
    cancel: &Arc<AtomicBool>,
) -> Result<UploadReservation> {
    let mut map = jobs
        .lock()
        .map_err(|_| "BitTorrent state lock is poisoned")?;
    let job = map.get_mut(id).ok_or("Unknown incoming torrent")?;
    peer_available(job, config, defaults, cancel)?;
    if !job.status.ready || !config.seed {
        return Err("Torrent payload is unavailable for seeding".into());
    }
    let reserved = job
        .upload_reserved
        .checked_add(bytes)
        .ok_or("Upload reservation overflow")?;
    if seed_budget(job, defaults).is_some_and(|limit| {
        u128::from(job.counters.uploaded.load(Ordering::Relaxed)) + u128::from(reserved)
            > u128::from(limit)
    }) {
        return Err("Block request exceeds the remaining seed payload budget".into());
    }
    job.upload_reserved = reserved;
    Ok(UploadReservation {
        jobs: jobs.clone(),
        id: id.into(),
        bytes,
    })
}
fn serve_peer(
    (mut stream, address): (Box<dyn crate::net::NetworkStream>, SocketAddr),
    config: &DownloadConfig,
    jobs: &Jobs,
    stop: &AtomicBool,
    peer_id: &[u8; 20],
    policy: &PolicyRuntime,
    catalog: (&Arc<Mutex<index::TransferIndex>>, Option<&AdmissionGate>),
) -> Result<()> {
    let (index, admission) = catalog;
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
    if config.seed {
        let owner = index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?
            .wire_alias(hash)?;
        if let Some(id) = owner {
            if !policy.seed_queue.allowed(&id) {
                return Err("Torrent is waiting in the seed queue".into());
            }
            let mut map = jobs
                .lock()
                .map_err(|_| "BitTorrent state lock is poisoned")?;
            if !map.contains_key(&id) {
                trim_jobs(config, &mut map, config.download_slots() + 31)?;
                if map.len() >= config.download_slots() + 64 {
                    return Err("Native transfer working set is busy".into());
                }
                let mut job = indexed_job(config, index, &id, &policy.policy)?;
                job.admitted = admission.is_none_or(|gate| gate(&id).unwrap_or(false));
                if job.needs_rehash {
                    job.status.ready = false;
                    job.status.selected_ready = false;
                    job.status.available_files.clear();
                    persist_index(&job)?;
                }
                map.insert(id, job);
            }
        }
    }
    let (torrent_id, meta, ready, known_peers, cancel, counters, upload_gate, reuse, inbound) = {
        let mut jobs = jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        jobs.iter_mut()
            .find_map(|(id, job)| {
                apply_seed_limits(job, config, &policy.policy);
                if job.paused
                    || !job.admitted
                    || job.persistence_failed.load(Ordering::Acquire)
                    || job.retiring
                    || job.seed_queued
                    || job.needs_rehash
                    || (job.cross_seed.is_some() && (!job.status.ready || !config.seed))
                    || job.control.user_paused
                    || job.control.seed_limited
                    || job.failed
                    || job.cancel.load(Ordering::Acquire)
                {
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
                        job.upload_gate.clone(),
                        job.cross_seed.clone(),
                        job.inbound.clone(),
                    ))
                } else {
                    None
                }
            })
            .ok_or("Unknown incoming torrent")?
    };
    // A download in progress takes the peer as one of its own: an empty
    // bitfield here would leave it idle until it gives up.
    if !ready && let Some(inbound) = inbound {
        let handoff = wire::Inbound {
            stream,
            address,
            hash,
            extensions: incoming[25] & 0x10 != 0,
        };
        match inbound.try_send(handoff) {
            Ok(()) => return Ok(()),
            Err(
                std::sync::mpsc::TrySendError::Full(back)
                | std::sync::mpsc::TrySendError::Disconnected(back),
            ) => {
                stream = back.stream;
            }
        }
    }
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
    // What the detail page shows of this peer, published at each message.
    let mut view = SeedingPeer {
        address,
        progress: None,
        interested: false,
        sent_bytes: 0,
        last_sent: None,
    };
    let mut have = vec![0u8; count.div_ceil(8)];
    let mut have_count = 0usize;
    let registration = if ready && config.seed {
        let mut map = jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        let job = map.get_mut(&torrent_id).ok_or("Unknown incoming torrent")?;
        let key = SEEDING_PEER_KEY.fetch_add(1, Ordering::Relaxed);
        job.seeding_peers.insert(key, view.clone());
        Some(SeedingRegistration {
            jobs,
            id: torrent_id.clone(),
            key,
        })
    } else {
        None
    };
    let publish = |job: &mut Job, view: &SeedingPeer| {
        if let Some(registration) = &registration
            && let Some(slot) = job.seeding_peers.get_mut(&registration.key)
        {
            *slot = view.clone();
        }
    };
    let mut metadata_id = None;
    let mut pex_id = None;
    let base = config.data_dir.join(&torrent_id);
    let mut cached_piece: Option<(usize, Vec<u8>)> = None;
    while !stop.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
        let (id, payload) = wire::read_message(&mut stream, Some(&cancel))?;
        match id {
            2 => view.interested = true,
            3 => view.interested = false,
            4 if payload.len() == 4 => {
                let index =
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                if index < count && have[index / 8] & (0x80 >> (index % 8)) == 0 {
                    have[index / 8] |= 0x80 >> (index % 8);
                    have_count += 1;
                    view.progress = Some(have_count as f64 / count as f64);
                }
            }
            5 if payload.len() == have.len() => {
                have.copy_from_slice(&payload);
                have_count = (0..count)
                    .filter(|i| have[i / 8] & (0x80 >> (i % 8)) != 0)
                    .count();
                view.progress = Some(have_count as f64 / count as f64);
            }
            _ => {}
        }
        {
            let mut map = jobs
                .lock()
                .map_err(|_| "BitTorrent state lock is poisoned")?;
            let job = map.get_mut(&torrent_id).ok_or("Unknown incoming torrent")?;
            peer_available(job, config, &policy.policy, &cancel)?;
            publish(job, &view);
        }
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
                // Reused files can disappear or change between requests. Never
                // let a cached piece bypass read-only path and content checks.
                if reuse.is_some() || cached_piece.as_ref().is_none_or(|(i, _)| *i != index) {
                    let data = if let Some(reuse) = &reuse {
                        match reuse.read_piece(config, &meta, index) {
                            Ok(data) => data,
                            Err(error) => {
                                let _admission = reuse
                                    .activity
                                    .lock()
                                    .map_err(|_| "Cross-seed activity lock is poisoned")?;
                                let mut map = jobs
                                    .lock()
                                    .map_err(|_| "BitTorrent state lock is poisoned")?;
                                if let Some(job) = map.get_mut(&torrent_id)
                                    && Arc::ptr_eq(&job.cancel, &cancel)
                                {
                                    job.failed = true;
                                    job.status.ready = false;
                                    job.status.selected_ready = false;
                                    job.status.available_files.clear();
                                    job.status.progress = 0.0;
                                    job.status.message = error.clone();
                                    job.cancel.store(true, Ordering::Release);
                                }
                                return Err(error);
                            }
                        }
                    } else {
                        read_piece(&meta, &base, index)?
                    };
                    if !meta.verify(index, &data) {
                        return Err("Seeding data has been modified".into());
                    }
                    cached_piece = Some((index, data));
                }
                let reservation = reserve_upload(
                    jobs,
                    &torrent_id,
                    length as u64,
                    config,
                    &policy.policy,
                    &cancel,
                )?;
                wire::reserve_payload(
                    &mut stream,
                    &policy.upload,
                    &upload_gate,
                    length as u64,
                    &cancel,
                )?;
                {
                    let mut map = jobs
                        .lock()
                        .map_err(|_| "BitTorrent state lock is poisoned")?;
                    let job = map.get_mut(&torrent_id).ok_or("Unknown incoming torrent")?;
                    peer_available(job, config, &policy.policy, &cancel)?;
                    if seed_budget(job, &policy.policy).is_some_and(|limit| {
                        u128::from(job.counters.uploaded.load(Ordering::Relaxed))
                            + u128::from(job.upload_reserved)
                            > u128::from(limit)
                    }) {
                        return Err("Seeding policy changed while reserving a block".into());
                    }
                }
                let data = &cached_piece.as_ref().ok_or("Missing seeding piece")?.1;
                // Hold only across the payload write, never while acquiring
                // the job table or waiting for a rate reservation.
                let seed_permit = policy.seed_queue.permit(&torrent_id)?;
                wire::write_payload_message(
                    &mut stream,
                    &payload[..8],
                    &data[offset..offset + length],
                    &cancel,
                    &counters.uploaded,
                )?;
                drop(seed_permit);
                drop(reservation);
                view.sent_bytes += length as u64;
                view.last_sent = Some(Instant::now());
                {
                    let mut map = jobs
                        .lock()
                        .map_err(|_| "BitTorrent state lock is poisoned")?;
                    let job = map.get_mut(&torrent_id).ok_or("Unknown incoming torrent")?;
                    apply_seed_limits(job, config, &policy.policy);
                    publish(job, &view);
                }
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

#[cfg(test)]
#[path = "torrent/source_network_tests.rs"]
mod source_network_tests;

#[cfg(test)]
mod control_runtime_tests {
    use super::*;

    #[test]
    fn restored_magnet_parser_cannot_fetch_metadata_locations() {
        let root = std::env::temp_dir().join(format!(
            "mynou-restored-magnet-{}",
            hex(&crate::crypto::random_bytes::<16>().unwrap())
        ));
        mkdir_private(&root).unwrap();
        let path = root.join("valid.torrent");
        let info = Value::Dict(BTreeMap::from([
            (b"name".to_vec(), Value::Bytes(b"empty.bin".to_vec())),
            (b"length".to_vec(), Value::Int(0)),
            (b"piece length".to_vec(), Value::Int(16_384)),
            (b"pieces".to_vec(), Value::Bytes(Vec::new())),
        ]));
        fs::write(
            &path,
            crate::bencode::encode(&Value::Dict(BTreeMap::from([(b"info".to_vec(), info)]))),
        )
        .unwrap();
        assert!(
            Source::parse(&transport::Direct, path.to_str().unwrap())
                .unwrap()
                .meta
                .is_some()
        );
        let trap = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        trap.set_nonblocking(true).unwrap();
        for location in [
            path.to_str().unwrap().to_owned(),
            format!("http://{}/metadata", trap.local_addr().unwrap()),
            format!("https://{}/metadata", trap.local_addr().unwrap()),
        ] {
            assert!(
                Source::parse_magnet_with_deadline(
                    &location,
                    Some(Instant::now() + Duration::from_millis(100))
                )
                .is_err()
            );
        }
        assert!(
            matches!(trap.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        let magnet = format!("magnet:?xt=urn:btih:{}", "1".repeat(40));
        let parsed = Source::parse_magnet_with_deadline(&magnet, None).unwrap();
        assert_eq!(parsed.v1, Some([0x11; 20]));
        assert!(parsed.meta.is_none());
        assert!(Source::parse_magnet_with_deadline(&magnet, Some(Instant::now())).is_err());
        assert!(
            Source::parse_magnet_with_deadline(
                &format!("{magnet}&dn={}", "x".repeat(64 * 1024)),
                None
            )
            .is_err()
        );
        assert!(
            Source::parse_magnet_with_deadline(
                &format!("magnet:?xt=urn:btih:{}", "0".repeat(40)),
                None
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
    use metainfo::MediaFile;

    fn geometry(lengths: &[(u64, bool)]) -> Meta {
        let mut offset = 0;
        let files = lengths
            .iter()
            .enumerate()
            .map(|(index, (length, padding))| {
                let file = MediaFile {
                    path: PathBuf::from(format!("file-{index}")),
                    offset,
                    length: *length,
                    padding: *padding,
                    root: None,
                };
                offset += length;
                file
            })
            .collect();
        Meta {
            info: Vec::new(),
            encoded: Vec::new(),
            v1: Some([0; 20]),
            v2: None,
            private: false,
            piece_length: BLOCK,
            pieces: vec![None; offset.div_ceil(BLOCK as u64) as usize],
            v2_pieces: vec![None; offset.div_ceil(BLOCK as u64) as usize],
            files,
            total: offset,
            trackers: Vec::new(),
        }
    }

    #[test]
    fn mixed_file_piece_uses_highest_priority_and_keeps_every_piece() {
        let meta = geometry(&[
            (BLOCK as u64, false),
            (BLOCK as u64 / 2, false),
            (BLOCK as u64 / 2, false),
            (BLOCK as u64, false),
            (BLOCK as u64, true),
        ]);
        let priorities = BTreeMap::from([
            (0, FilePriority::Low),
            (2, FilePriority::High),
            (3, FilePriority::Low),
        ]);
        assert_eq!(piece_order(&meta, &priorities), vec![1, 0, 2, 3]);
        assert_eq!(piece_order(&meta, &BTreeMap::new()), vec![0, 1, 2, 3]);
    }

    #[test]
    fn verified_payload_accounting_excludes_padding_and_empty_files() {
        let meta = geometry(&[(9_000, false), (7_384, true), (0, false), (16_433, false)]);
        assert_eq!(verified_piece_bytes(&meta, 0), 9_000);
        assert_eq!(verified_piece_bytes(&meta, 1), BLOCK as u64);
        assert_eq!(verified_piece_bytes(&meta, 2), 49);
        assert_eq!(
            (0..meta.count())
                .map(|index| verified_piece_bytes(&meta, index))
                .sum::<u64>(),
            25_433
        );
    }

    #[test]
    fn payload_totals_saturate_without_wrapping() {
        let counter = AtomicU64::new(u64::MAX - 3);
        add_payload(&counter, 2);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX - 1);
        add_payload(&counter, 16_384);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    fn discovery_job(byte: u8, trackers: usize) -> Job {
        let id: String = [byte; 20]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut meta = geometry(&[(BLOCK as u64, false)]);
        meta.v1 = Some([byte; 20]);
        Job {
            index: None,
            persistence_failed: AtomicBool::new(false),
            retiring: false,
            removing: None,
            live_peers: Vec::new(),
            known_peers: 0,
            seeding_peers: BTreeMap::new(),
            inbound: None,
            admitted: true,
            needs_rehash: false,
            seed_queued: false,
            cross_seed: None,
            source: Source {
                original: format!("magnet:?xt=urn:btih:{id}"),
                v1: meta.v1,
                v2: None,
                trackers: (0..trackers)
                    .map(|index| format!("http://tracker.invalid/{index}"))
                    .collect(),
                peers: Vec::new(),
                meta: None,
            },
            meta: Some(Arc::new(meta)),
            status: DownloadStatus {
                id: id.clone(),
                progress: 0.0,
                ready: false,
                selected_ready: false,
                available_files: Vec::new(),
                files: Vec::new(),
                message: String::new(),
            },
            running: false,
            paused: false,
            failed: false,
            cancel: Arc::new(AtomicBool::new(false)),
            peers: Vec::new(),
            tracker_peers: BTreeSet::new(),
            counters: Arc::new(TransferCounters::default()),
            announcements: (0..trackers)
                .map(|index| {
                    let due = Instant::now() - Duration::from_secs(1);
                    (
                        format!("http://tracker.invalid/{index}"),
                        AnnounceState {
                            next: due,
                            retry_after: due,
                            ..AnnounceState::default()
                        },
                    )
                })
                .collect(),
            announce_hash: None,
            dht_next: Instant::now(),
            dht_in_flight: false,
            control: TorrentControl::new(id, u64::from(byte) + 1).unwrap(),
            piece_order: Arc::new(vec![0]),
            download_gate: Arc::new(RateGate::default()),
            upload_gate: Arc::new(RateGate::default()),
            seed_clock: None,
            seed_partial: Duration::ZERO,
            upload_reserved: 0,
            saved_control: Vec::new(),
        }
    }

    fn discovery_configuration() -> DownloadConfig {
        DownloadConfig {
            data_dir: "data".into(),
            state_dir: "state".into(),
            listen_port: 0,
            seed: true,
            dht: true,
            pex: false,
            max_active: 1,
            max_seeding: 0,
            max_peers: 4,
        }
    }

    #[test]
    fn seed_queue_pages_past_paused_heads_and_reorders_without_payload_hydration() {
        let root = std::env::temp_dir().join(format!(
            "mynou-seed-queue-{}",
            hex(&crate::crypto::random_bytes::<8>().unwrap())
        ));
        let mut config = discovery_configuration();
        config.state_dir = root.join("state");
        config.data_dir = root.join("data");
        mkdir_private(&config.state_dir).unwrap();
        mkdir_private(&config.data_dir).unwrap();
        let mut index = index::TransferIndex::open(&mut config).unwrap();
        for n in 1..=202u8 {
            let mut job = discovery_job(n, 0);
            job.status.ready = true;
            job.control.user_paused = n <= 200;
            index.save(&job).unwrap();
        }
        let index = Arc::new(Mutex::new(index));
        let queue = queue::SeedQueue::new(1);
        let mut scan = queue::SeedScan::default();
        let first = discovery_job(201, 0).status.id;
        let second = discovery_job(202, 0).status.id;
        queue.advance(&index, &mut scan, None).unwrap();
        assert!(!queue.allowed(&first));
        queue.advance(&index, &mut scan, None).unwrap();
        assert!(queue.allowed(&first));
        assert!(!queue.allowed(&second));
        let mut promoted = discovery_job(202, 0);
        promoted.status.ready = true;
        promoted.control.priority = 10;
        index.lock().unwrap().save(&promoted).unwrap();
        queue.advance(&index, &mut scan, None).unwrap();
        assert!(queue.allowed(&second));
        assert!(!queue.allowed(&first));
        let busy: AdmissionGate = Arc::new(|_| Ok(false));
        queue.advance(&index, &mut scan, Some(&busy)).unwrap();
        assert!(queue.allowed(&second));
        promoted.control.user_paused = true;
        index.lock().unwrap().save(&promoted).unwrap();
        queue.advance(&index, &mut scan, None).unwrap();
        queue.advance(&index, &mut scan, None).unwrap();
        assert!(queue.allowed(&first));
        assert!(!queue.allowed(&second));
        promoted.control.user_paused = false;
        index.lock().unwrap().save(&promoted).unwrap();
        let denied = second.clone();
        let admission: AdmissionGate = Arc::new(move |id| Ok(id != denied));
        queue.advance(&index, &mut scan, Some(&admission)).unwrap();
        queue.advance(&index, &mut scan, Some(&admission)).unwrap();
        assert!(queue.allowed(&first));
        assert!(!queue.allowed(&second));
        drop(index);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_many_tracker_job_does_not_monopolize_discovery_selection() {
        let first = discovery_job(1, 3);
        let second = discovery_job(2, 1);
        let ids = [first.status.id.clone(), second.status.id.clone()];
        let jobs = Arc::new(Mutex::new(BTreeMap::from([
            (ids[0].clone(), first),
            (ids[1].clone(), second),
        ])));
        let mut cursor = None;
        let config = discovery_configuration();
        assert_eq!(
            select_announce(&config, &jobs, &mut cursor).unwrap().id,
            ids[0]
        );
        assert_eq!(
            select_announce(&config, &jobs, &mut cursor).unwrap().id,
            ids[1]
        );
        assert_eq!(
            select_announce(&config, &jobs, &mut cursor).unwrap().id,
            ids[0]
        );
    }

    #[test]
    fn ready_torrents_share_seed_discovery_even_when_the_first_is_due_again() {
        let mut first = discovery_job(1, 0);
        let mut second = discovery_job(2, 0);
        first.status.ready = true;
        second.status.ready = true;
        let ids = [first.status.id.clone(), second.status.id.clone()];
        let jobs = Arc::new(Mutex::new(BTreeMap::from([
            (ids[0].clone(), first),
            (ids[1].clone(), second),
        ])));
        let config = discovery_configuration();
        let mut cursor = None;
        assert_eq!(
            select_seed_dht(&config, &jobs, &mut cursor).unwrap().id,
            ids[0]
        );
        jobs.lock().unwrap().get_mut(&ids[0]).unwrap().dht_in_flight = false;
        assert_eq!(
            select_seed_dht(&config, &jobs, &mut cursor).unwrap().id,
            ids[1]
        );
    }
    #[test]
    fn retained_inventory_stays_cold_and_pages_survive_mutations_and_restart() {
        let root = std::env::temp_dir().join(format!(
            "mynou-transfer-cold-{}-{}",
            std::process::id(),
            hex(&crate::crypto::random_bytes::<8>().unwrap())
        ));
        let state = root.join("state");
        mkdir_private(&root).unwrap();
        mkdir_private(&state).unwrap();
        let config = DownloadConfig {
            data_dir: root.join("data"),
            state_dir: state.clone(),
            listen_port: 0,
            seed: false,
            dht: false,
            pex: false,
            max_active: 1,
            max_seeding: 0,
            max_peers: 1,
        };
        const RETAINED: usize = 384;
        for n in 1..=RETAINED {
            let id = format!("{n:040x}");
            atomic_write(
                &state.join(format!("{id}.source")),
                format!("magnet:?xt=urn:btih:{id}").as_bytes(),
            )
            .unwrap();
            let mut control = TorrentControl::new(id.clone(), n as u64).unwrap();
            control.user_paused = true;
            control.downloaded_bytes = n as u64;
            atomic_write(
                &state.join(format!("{id}.control")),
                &control.encode().unwrap(),
            )
            .unwrap();
        }
        let mut client = Client::open(config.clone()).unwrap();
        assert_eq!(
            client.jobs.lock().unwrap().len(),
            0,
            "startup must not hydrate retained paused transfers"
        );
        let mut cursor = None;
        let mut seen = BTreeSet::new();
        loop {
            let (page, next) = client.statuses_page(cursor.as_deref(), 31).unwrap();
            assert!(page.len() <= 31);
            for status in page {
                assert!(seen.insert(status.id.clone()));
                client.set_priority(&status.id, 7).unwrap();
                assert!(client.controls(&status.id).unwrap().user_paused);
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(seen.len(), RETAINED);
        assert!(client.jobs.lock().unwrap().len() <= config.download_slots() + 33);
        assert_eq!(
            client.aggregate_stats().unwrap(),
            (RETAINED as u64, (RETAINED * (RETAINED + 1) / 2) as u64, 0)
        );
        client.stop.store(true, Ordering::Release);
        if let Some(manager) = client.manager.take() {
            manager.join().unwrap();
        }
        drop(client);
        // Routine open must ignore unrelated legacy-looking files after publication.
        atomic_write(
            &state.join("ffffffffffffffffffffffffffffffffffffffff.source"),
            b"invalid ignored stale legacy file",
        )
        .unwrap();
        let client = Client::open(config).unwrap();
        assert_eq!(client.jobs.lock().unwrap().len(), 0);
        let id = format!("{:040x}", RETAINED);
        assert_eq!(client.controls(&id).unwrap().priority, 7);
        assert!(client.contains(&id).unwrap());
        assert_eq!(client.transfer_stats(&id).unwrap(), (RETAINED as u64, 0));
        drop(client);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn offline_pause_fences_cold_admission_and_reserved_marker_identity_never_writes() {
        let root = std::env::temp_dir().join(format!(
            "mynou-transfer-offline-{}-{}",
            std::process::id(),
            hex(&crate::crypto::random_bytes::<8>().unwrap())
        ));
        let config = DownloadConfig {
            data_dir: root.join("data"),
            state_dir: root.join("state"),
            listen_port: 0,
            seed: false,
            dht: false,
            pex: false,
            max_active: 1,
            max_seeding: 0,
            max_peers: 1,
        };
        let denied: AdmissionGate = Arc::new(|_| Ok(false));
        let client = Client::open_with_policy_and_admission(
            config.clone(),
            TransferPolicy::default(),
            denied,
            transport::direct(),
        )
        .unwrap();
        let id = "0123456789012345678901234567890123456789";
        client.ensure(&format!("magnet:?xt=urn:btih:{id}")).unwrap();
        let reserved = "0000000000000000000000000000000000000000";
        assert!(
            client
                .ensure(&format!("magnet:?xt=urn:btih:{reserved}"))
                .is_err()
        );
        assert!(!config.state_dir.join(format!("{reserved}.source")).exists());
        assert!(
            !config
                .state_dir
                .join(format!("{reserved}.control"))
                .exists()
        );
        drop(client);
        pause_persisted(&config, id).unwrap();
        let client = Client::open(config.clone()).unwrap();
        let status = client.check(id).unwrap();
        assert_eq!(status.message, "Download paused");
        assert!(!client.controls(id).unwrap().user_paused);
        thread::sleep(Duration::from_millis(75));
        let map = client.jobs.lock().unwrap();
        let job = map.get(id).unwrap();
        assert!(job.paused);
        assert!(!job.running);
        drop(map);
        drop(client);
        fs::remove_dir_all(root).unwrap();
    }

    /// One scripted inbound connection; records how much was read and when the
    /// client drops it.
    struct Inbound {
        input: Vec<u8>,
        read: Arc<AtomicU64>,
        dropped: Arc<AtomicBool>,
    }

    impl Read for Inbound {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.input.is_empty() {
                return Ok(0);
            }
            let n = buffer.len().min(self.input.len());
            buffer[..n].copy_from_slice(&self.input[..n]);
            self.input.drain(..n);
            self.read.fetch_add(n as u64, Ordering::SeqCst);
            Ok(n)
        }
    }

    impl Write for Inbound {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl crate::net::NetworkStream for Inbound {
        fn set_read_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }

        fn set_write_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for Inbound {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    struct InboundListener(Mutex<Option<Inbound>>);

    impl transport::NetworkListener for InboundListener {
        fn accept(&self) -> std::io::Result<(Box<dyn crate::net::NetworkStream>, SocketAddr)> {
            match self.0.lock().unwrap().take() {
                Some(stream) => Ok((Box::new(stream), "198.51.100.9:6881".parse().unwrap())),
                None => Err(std::io::ErrorKind::WouldBlock.into()),
            }
        }

        fn port(&self) -> u16 {
            51413
        }
    }

    /// A network with only a listener: any outbound attempt is an error.
    struct ListenOnly {
        bound: Mutex<Vec<SocketAddr>>,
        inbound: Mutex<Option<Inbound>>,
    }

    impl transport::Network for ListenOnly {
        fn connect(
            &self,
            _: SocketAddr,
            _: Duration,
        ) -> Result<Box<dyn crate::net::NetworkStream>> {
            Err("No peer connection in this fixture".into())
        }

        fn resolve(&self, _: &str, _: Instant) -> Result<Vec<SocketAddr>> {
            Err("No resolver in this fixture".into())
        }

        fn datagram(&self, _: bool) -> Result<Box<dyn crate::net::NetworkDatagram>> {
            Err("No UDP in this fixture".into())
        }

        fn http(&self) -> crate::net::HttpClient {
            unreachable!("the listener fixture never uses HTTP")
        }

        fn listen(&self, address: SocketAddr) -> Result<Box<dyn transport::NetworkListener>> {
            self.bound.lock().unwrap().push(address);
            Ok(Box::new(InboundListener(Mutex::new(
                self.inbound.lock().unwrap().take(),
            ))))
        }
    }

    #[test]
    fn inbound_peers_arrive_through_the_client_network() {
        let root = std::env::temp_dir().join(format!(
            "mynou-inbound-network-{}-{}",
            std::process::id(),
            hex(&crate::crypto::random_bytes::<8>().unwrap())
        ));
        let config = DownloadConfig {
            data_dir: root.join("data"),
            state_dir: root.join("state"),
            listen_port: 6999,
            seed: true,
            dht: false,
            pex: false,
            max_active: 1,
            max_seeding: 0,
            max_peers: 1,
        };
        let mut handshake = vec![19];
        handshake.extend_from_slice(b"BitTorrent protocol");
        handshake.extend_from_slice(&[0; 8]);
        handshake.extend_from_slice(&[0x5a; 20]);
        handshake.extend_from_slice(&[0x33; 20]);
        let read = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let network = Arc::new(ListenOnly {
            bound: Mutex::new(Vec::new()),
            inbound: Mutex::new(Some(Inbound {
                input: handshake,
                read: read.clone(),
                dropped: dropped.clone(),
            })),
        });
        let client = Client::open_with_policy_and_admission(
            config,
            TransferPolicy::default(),
            Arc::new(|_| Ok(true)),
            network.clone(),
        )
        .unwrap();
        assert_eq!(
            *network.bound.lock().unwrap(),
            vec![SocketAddr::from(([0, 0, 0, 0], 6999))]
        );
        assert_eq!(client.listen_port(), 51413);
        let end = Instant::now() + Duration::from_secs(5);
        while !dropped.load(Ordering::SeqCst) && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(read.load(Ordering::SeqCst), 68);
        drop(client);
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
#[path = "torrent/tunnel_tests.rs"]
mod tunnel_tests;

#[cfg(test)]
#[path = "torrent/load_tests.rs"]
mod load_tests;
