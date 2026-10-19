//! Manual read-only v1 reuse. A .seed record is never an ordinary queue entry.
use super::*;
use crate::json::Value as Json;

pub const MAX_CROSS_SEED_METADATA: usize = 256 * 1024;
const MAX_FILES: usize = 1024;
const MAX_RECORD: usize = MAX_CROSS_SEED_METADATA + 64 * 1024;

fn state_directory(config: &DownloadConfig) -> Result<PathBuf> {
    let configured = if config.state_dir.is_absolute() {
        config.state_dir.clone()
    } else {
        std::env::current_dir()
            .map_err(|_| "Could not resolve cross-seed state directory")?
            .join(&config.state_dir)
    };
    safe_directory(&configured)?;
    let normalized = configured
        .canonicalize()
        .map_err(|_| "Could not normalize cross-seed state directory")?;
    if !normalized.starts_with(&configured) {
        return Err(
            "Cross-seed state directory does not match its physical configured root".into(),
        );
    }
    Ok(normalized)
}

fn safe_directory(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|_| "Could not resolve cross-seed directory")?
            .join(path)
    };
    for ancestor in absolute.ancestors() {
        let metadata =
            fs::symlink_metadata(ancestor).map_err(|_| "Cross-seed directory is missing")?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("Links or special cross-seed directories are not allowed".into());
        }
    }
    Ok(())
}

pub struct CrossSeedRequest {
    pub metadata: Vec<u8>,
    pub mapping: Option<Vec<usize>>,
}
impl CrossSeedRequest {
    pub fn from_json(value: &Json) -> Result<Self> {
        let Json::Object(fields) = value else {
            return Err("Cross-seed request must be an object".into());
        };
        if fields
            .keys()
            .any(|key| !["metadata_hex", "mapping"].contains(&key.as_str()))
        {
            return Err("Unknown cross-seed field".into());
        }
        let encoded = value
            .get("metadata_hex")
            .and_then(Json::as_str)
            .ok_or("metadata_hex must be a string")?;
        if encoded.is_empty()
            || encoded.len() > MAX_CROSS_SEED_METADATA * 2
            || !encoded.len().is_multiple_of(2)
        {
            return Err("Cross-seed metadata must contain 1 to 262144 bytes".into());
        }
        let metadata = encoded
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                decode_hex::<1>(std::str::from_utf8(pair).map_err(|_| "Invalid metadata hex")?)
                    .map(|byte| byte[0])
            })
            .collect::<Result<Vec<_>>>()?;
        let mapping = value
            .get("mapping")
            .map(|value| {
                let entries = value
                    .as_array()
                    .ok_or("mapping must be an array of source file indices")?;
                if entries.len() > MAX_FILES {
                    return Err("Cross-seed mapping exceeds 1024 files".into());
                }
                entries
                    .iter()
                    .map(|entry| {
                        entry
                            .as_u64()
                            .and_then(|index| usize::try_from(index).ok())
                            .ok_or_else(|| {
                                "mapping must contain nonnegative integer indices".into()
                            })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        Ok(Self { metadata, mapping })
    }
}

pub(super) struct CrossSeed {
    pub source_id: String,
    pub activity: Arc<Mutex<()>>,
    original: Arc<Meta>,
    indices: Vec<Option<usize>>,
}

fn candidate(encoded: &[u8]) -> Result<Meta> {
    if encoded.is_empty() || encoded.len() > MAX_CROSS_SEED_METADATA {
        return Err("Cross-seed metadata must contain 1 to 262144 bytes".into());
    }
    let meta = Meta::parse(encoded)?;
    if meta.v1.is_none() || meta.v2.is_some() {
        return Err("Manual cross-seeding currently supports v1 torrents only".into());
    }
    if meta.files.len() > MAX_FILES {
        return Err("Cross-seed metadata exceeds 1024 files".into());
    }
    Ok(meta)
}

impl CrossSeed {
    fn bind(
        source_id: &str,
        original: Arc<Meta>,
        meta: &Meta,
        mapping: Option<&[usize]>,
    ) -> Result<Self> {
        if ![40, 64].contains(&source_id.len())
            || !source_id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || source_id != source_id.to_ascii_lowercase()
        {
            return Err("Invalid cross-seed source identity".into());
        }
        let defaults: Vec<_> = if mapping.is_some() {
            Vec::new()
        } else {
            original
                .files
                .iter()
                .enumerate()
                .filter_map(|(index, file)| (!file.padding).then_some(index))
                .take(MAX_FILES + 1)
                .collect()
        };
        let mapping = mapping.unwrap_or(&defaults);
        if mapping.len() != meta.files.iter().filter(|file| !file.padding).count() {
            return Err("Mapping must cover every candidate payload file".into());
        }
        let mut seen = BTreeSet::new();
        let mut next = mapping.iter();
        let mut indices = Vec::with_capacity(meta.files.len());
        for file in &meta.files {
            if file.padding {
                indices.push(None);
                continue;
            }
            let index = *next.next().ok_or("Missing source file mapping")?;
            let source = original
                .files
                .get(index)
                .ok_or("Source file index is out of bounds")?;
            if source.padding || source.length != file.length || !seen.insert(index) {
                return Err(
                    "Mapped files must have equal sizes, be non-padding and distinct".into(),
                );
            }
            indices.push(Some(index));
        }
        Ok(Self {
            source_id: source_id.to_owned(),
            original,
            indices,
            activity: Arc::new(Mutex::new(())),
        })
    }

    fn open_file(&self, config: &DownloadConfig, index: usize) -> Result<File> {
        let source = self
            .original
            .files
            .get(
                self.indices
                    .get(index)
                    .copied()
                    .flatten()
                    .ok_or("Missing source file mapping")?,
            )
            .ok_or("Invalid source file mapping")?;
        let base = config.data_dir.join(&self.source_id);
        safe_directory(&base)?;
        let data_root = fs::symlink_metadata(&config.data_dir)
            .map_err(|_| "Cross-seed data directory is missing")?;
        if !data_root.is_dir() || data_root.file_type().is_symlink() {
            return Err("Cross-seed data directory is unsafe".into());
        }
        let root =
            fs::symlink_metadata(&base).map_err(|_| "Cross-seed source directory is missing")?;
        if !root.is_dir() || root.file_type().is_symlink() {
            return Err("Cross-seed source directory is unsafe".into());
        }
        let path = confined(&base, &source.path)?;
        let before =
            fs::symlink_metadata(&path).map_err(|_| "Cross-seed source file is missing")?;
        private_state_file(&before)?;
        let file = File::open(&path).map_err(|_| "Could not open cross-seed source read-only")?;
        let opened = file
            .metadata()
            .map_err(|_| "Could not inspect cross-seed source")?;
        private_state_file(&opened)?;
        safe_directory(&base)?;
        let after = fs::symlink_metadata(confined(&base, &source.path)?)
            .map_err(|_| "Cross-seed source changed")?;
        private_state_file(&after)?;
        if before.len() != source.length
            || opened.len() != source.length
            || after.len() != source.length
        {
            return Err("Cross-seed source file size changed".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if before.dev() != opened.dev()
                || before.ino() != opened.ino()
                || after.dev() != opened.dev()
                || after.ino() != opened.ino()
            {
                return Err("Cross-seed source file identity changed".into());
            }
        }
        Ok(file)
    }

    pub fn read_piece(
        &self,
        config: &DownloadConfig,
        meta: &Meta,
        index: usize,
    ) -> Result<Vec<u8>> {
        let start = index as u64 * meta.piece_length as u64;
        let mut data = vec![0; meta.piece_size(index)];
        let end = start + data.len() as u64;
        for (file_index, entry) in meta.files.iter().enumerate() {
            if entry.padding || entry.offset >= end || entry.offset + entry.length <= start {
                continue;
            }
            let a = start.max(entry.offset);
            let b = end.min(entry.offset + entry.length);
            let mut file = self.open_file(config, file_index)?;
            file.seek(SeekFrom::Start(a - entry.offset))
                .and_then(|_| {
                    file.read_exact(&mut data[(a - start) as usize..(b - start) as usize])
                })
                .map_err(|_| "Cross-seed source read failed; no download attempted")?;
        }
        if !meta.verify(index, &data) {
            return Err(format!(
                "Cross-seed mismatch at piece {index}; no download attempted"
            ));
        }
        Ok(data)
    }

    fn paths(&self, config: &DownloadConfig) -> Vec<PathBuf> {
        self.indices
            .iter()
            .flatten()
            .map(|index| {
                config
                    .data_dir
                    .join(&self.source_id)
                    .join(&self.original.files[*index].path)
            })
            .collect()
    }
    fn encode(&self, meta: &Meta) -> Vec<u8> {
        crate::bencode::encode(&Value::Dict(BTreeMap::from([
            (b"schema".to_vec(), Value::Int(1)),
            (
                b"source".to_vec(),
                Value::Bytes(self.source_id.as_bytes().to_vec()),
            ),
            (b"metadata".to_vec(), Value::Bytes(meta.encoded.clone())),
            (
                b"mapping".to_vec(),
                Value::List(
                    self.indices
                        .iter()
                        .flatten()
                        .map(|index| Value::Int(*index as i64))
                        .collect(),
                ),
            ),
        ])))
    }
}

fn queued(
    meta: Meta,
    reuse: CrossSeed,
    id: String,
    order: u64,
    config: &DownloadConfig,
) -> Result<Job> {
    let control = TorrentControl::new(id.clone(), order)?;
    Ok(Job {
        index: None,
        persistence_failed: AtomicBool::new(false),
        retiring: false,
        admitted: true,
        needs_rehash: false,
        source: Source {
            original: "cross-seed".into(),
            v1: meta.v1,
            v2: None,
            trackers: meta.trackers.clone(),
            peers: Vec::new(),
            meta: None,
        },
        status: DownloadStatus {
            id: id.clone(),
            progress: 0.0,
            ready: false,
            selected_ready: false,
            files: reuse.paths(config),
            available_files: Vec::new(),
            message: "Cross-seed verifying existing files".into(),
        },
        meta: Some(Arc::new(meta)),
        cross_seed: Some(Arc::new(reuse)),
        running: false,
        paused: persisted_pause(config, &id)?,
        failed: false,
        cancel: Arc::new(AtomicBool::new(false)),
        peers: Vec::new(),
        tracker_peers: BTreeSet::new(),
        counters: Arc::new(TransferCounters::default()),
        announcements: BTreeMap::new(),
        announce_hash: None,
        dht_next: Instant::now(),
        dht_in_flight: false,
        control,
        piece_order: Arc::new(Vec::new()),
        download_gate: Arc::new(RateGate::default()),
        upload_gate: Arc::new(RateGate::default()),
        seed_clock: None,
        seed_partial: Duration::ZERO,
        upload_reserved: 0,
        saved_control: Vec::new(),
    })
}

impl Client {
    pub fn cross_seed(
        &self,
        source_id: &str,
        request: &CrossSeedRequest,
    ) -> Result<DownloadStatus> {
        if !self.config.seed {
            return Err("Cross-seeding requires downloads.seed to be enabled".into());
        }
        let meta = candidate(&request.metadata)?;
        let id = hex(&meta.v1.ok_or("Missing v1 cross-seed identity")?);
        // State filenames contain only the authenticated hash, never metadata paths.
        if id.len() != 40
            || id == "0000000000000000000000000000000000000000"
            || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("Invalid cross-seed candidate identity".into());
        }
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        self.load(&mut jobs, source_id)?;
        if self
            .index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?
            .wire_alias(meta.v1.ok_or("Missing seed wire identity")?)?
            .is_some()
            || jobs.values().any(|job| {
                job.source.v1 == meta.v1
                    || job.source.v2.is_some_and(|hash| {
                        Some(&hash[..20]) == meta.v1.as_ref().map(|hash| hash.as_slice())
                    })
            })
        {
            return Err("Candidate swarm already exists; inspect its transfer instead".into());
        }
        state_directory(&self.config)?;
        for suffix in ["torrent", "source", "seed"] {
            for name in [
                format!("{id}.{suffix}"),
                format!("{}.{suffix}", id.to_ascii_uppercase()),
                format!("{id}.{}", suffix.to_ascii_uppercase()),
                format!(
                    "{}.{}",
                    id.to_ascii_uppercase(),
                    suffix.to_ascii_uppercase()
                ),
            ] {
                match fs::symlink_metadata(self.config.state_dir.join(name)) {
                    Ok(_) => {
                        return Err(
                            "Candidate swarm already exists; inspect its transfer instead".into(),
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err("Cannot inspect candidate swarm state".into()),
                }
            }
        }
        let source = jobs
            .get(source_id)
            .ok_or("Unknown cross-seed source transfer")?;
        if !source.status.ready || source.failed || source.cross_seed.is_some() {
            return Err("Cross-seed source must be a fully verified ordinary transfer".into());
        }
        let reuse = CrossSeed::bind(
            source_id,
            source.meta.clone().ok_or("Missing source metadata")?,
            &meta,
            request.mapping.as_deref(),
        )?;
        trim_jobs(&self.config, &mut jobs, self.config.max_active + 31)?;
        if jobs.len() >= self.config.max_active + 64 {
            return Err("Native transfer working set is busy; retry later".into());
        }
        let order = self
            .index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?
            .next_order()?;
        let encoded = reuse.encode(&meta);
        let mut job = queued(meta, reuse, id.clone(), order, &self.config)?;
        job.saved_control = job.control.encode()?;
        // The isolated primary record includes the seed-only binding and metadata
        // in one atomic publication. A crash cannot produce an ordinary torrent.
        atomic_write(
            &self.config.state_dir.join(format!("{id}.control")),
            &job.saved_control,
        )?;
        atomic_write(&self.config.state_dir.join(format!("{id}.seed")), &encoded)?;
        job.index = Some(self.index.clone());
        job.admitted = self
            .admission
            .as_ref()
            .is_none_or(|gate| gate(&id).unwrap_or(false));
        persist_index(&job)?;
        let status = job.status.clone();
        jobs.insert(id, job);
        Ok(status)
    }
}

pub(super) fn load(config: &DownloadConfig, id: &str) -> Result<Job> {
    // Seed records require the same physical root binding as registration;
    // ordinary legacy queue entries retain their existing startup behavior.
    state_directory(config)?;
    let identity = decode_hex::<20>(id).map_err(|_| "Invalid persisted cross-seed identity")?;
    if id != id.to_ascii_lowercase()
        || ["torrent", "source"].iter().any(|suffix| {
            fs::symlink_metadata(config.state_dir.join(format!("{id}.{suffix}"))).is_ok()
        })
    {
        return Err("Cross-seed state conflicts with an ordinary transfer".into());
    }
    let stat = fs::symlink_metadata(config.state_dir.join(format!("{id}.seed")))
        .map_err(|_| "Could not inspect cross-seed state")?;
    private_state_file(&stat)?;
    let control = fs::symlink_metadata(config.state_dir.join(format!("{id}.control")))
        .map_err(|_| "Cross-seed controls are missing; refusing to reset seeding permission")?;
    private_state_file(&control)?;
    if stat.len() > MAX_RECORD as u64 {
        return Err("Cross-seed state exceeds limits".into());
    }
    let mut encoded = Vec::new();
    crate::store::open_inspected(&config.state_dir.join(format!("{id}.seed")), &stat)
        .and_then(|file| {
            file.take((MAX_RECORD + 1) as u64)
                .read_to_end(&mut encoded)
                .map_err(|error| error.to_string())
        })
        .map_err(|_| "Could not read cross-seed state")?;
    if encoded.len() > MAX_RECORD {
        return Err("Cross-seed state exceeds limits".into());
    }
    let Value::Dict(fields) = crate::bencode::parse(&encoded)? else {
        return Err("Invalid cross-seed state".into());
    };
    if fields.len() != 4 || fields.get(b"schema".as_slice()) != Some(&Value::Int(1)) {
        return Err("Invalid cross-seed schema".into());
    }
    let bytes = |key: &[u8]| -> Result<&[u8]> {
        metainfo::bytes(fields.get(key).ok_or("Missing cross-seed field")?)
    };
    let source_id =
        std::str::from_utf8(bytes(b"source")?).map_err(|_| "Invalid cross-seed source identity")?;
    let meta = candidate(bytes(b"metadata")?)?;
    if meta.v1 != Some(identity) {
        return Err("Cross-seed state hash mismatch".into());
    }
    let Value::List(mapping) = fields
        .get(b"mapping".as_slice())
        .ok_or("Missing cross-seed mapping")?
    else {
        return Err("Invalid cross-seed mapping".into());
    };
    if mapping.len() > MAX_FILES {
        return Err("Cross-seed mapping exceeds limits".into());
    }
    let mapping = mapping
        .iter()
        .map(|value| {
            usize::try_from(metainfo::integer(value)?)
                .map_err(|_| "Invalid cross-seed mapping".into())
        })
        .collect::<Result<Vec<_>>>()?;
    if ![40, 64].contains(&source_id.len())
        || !source_id.bytes().all(|b| b.is_ascii_hexdigit())
        || source_id != source_id.to_ascii_lowercase()
    {
        return Err("Invalid cross-seed source identity".into());
    }
    if config
        .state_dir
        .join(format!("{source_id}.seed"))
        .try_exists()
        .map_err(|_| "Could not inspect cross-seed source")?
    {
        return Err("Chained cross-seeding is not supported".into());
    }
    let source = super::load_job(config, source_id)?;
    if source.cross_seed.is_some() {
        return Err("Chained cross-seeding is not supported".into());
    }
    let reuse = CrossSeed::bind(
        source_id,
        source
            .meta
            .clone()
            .ok_or("Cross-seed source metadata is missing")?,
        &meta,
        Some(&mapping),
    )?;
    queued(meta, reuse, id.to_owned(), 1, config)
}

pub(super) fn verify(
    config: &DownloadConfig,
    jobs: &Jobs,
    stop: &AtomicBool,
    id: &str,
) -> Result<()> {
    let (reuse, meta, counters) = {
        let map = jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        let job = map.get(id).ok_or("Unknown cross-seed transfer")?;
        (
            job.cross_seed.clone().ok_or("Missing cross-seed binding")?,
            job.meta.clone().ok_or("Missing cross-seed metadata")?,
            job.counters.clone(),
        )
    };
    // Includes zero-length files that no piece read would otherwise touch.
    let mut stamps = Vec::with_capacity(meta.files.len());
    for (index, file) in meta.files.iter().enumerate() {
        if stop.load(Ordering::Acquire) {
            return Err("Cross-seed verification interrupted".into());
        }
        stamps.push(if file.padding {
            None
        } else {
            Some(
                reuse
                    .open_file(config, index)?
                    .metadata()
                    .map_err(|_| "Could not inspect cross-seed source")?,
            )
        });
    }
    for index in 0..meta.count() {
        if stop.load(Ordering::Acquire) {
            return Err("Cross-seed verification interrupted".into());
        }
        reuse.read_piece(config, &meta, index)?;
    }
    for (index, stamp) in stamps.iter().enumerate() {
        if let Some(before) = stamp {
            let after = reuse
                .open_file(config, index)?
                .metadata()
                .map_err(|_| "Could not inspect cross-seed source")?;
            if before.len() != after.len()
                || before
                    .modified()
                    .map_err(|_| "Could not inspect cross-seed source timestamp")?
                    != after
                        .modified()
                        .map_err(|_| "Could not inspect cross-seed source timestamp")?
            {
                return Err(
                    "Cross-seed source changed during verification; retry explicitly".into(),
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if before.dev() != after.dev() || before.ino() != after.ino() {
                    return Err("Cross-seed source identity changed during verification".into());
                }
            }
        }
    }
    let _admission = reuse
        .activity
        .lock()
        .map_err(|_| "Cross-seed activity lock is poisoned")?;
    let mut map = jobs
        .lock()
        .map_err(|_| "BitTorrent state lock is poisoned")?;
    let job = map.get_mut(id).ok_or("Unknown cross-seed transfer")?;
    if stop.load(Ordering::Acquire)
        || !std::ptr::eq(&*job.cancel, stop)
        || job.paused
        || job.control.user_paused
    {
        return Err("Cross-seed verification interrupted".into());
    }
    counters.verified.store(
        meta.files
            .iter()
            .filter(|file| !file.padding)
            .map(|file| file.length)
            .sum(),
        Ordering::Relaxed,
    );
    job.status.progress = 1.0;
    job.status.ready = true;
    job.status.selected_ready = true;
    // Seed-only status never supplies acquisition/import availability.
    job.status.available_files.clear();
    job.status.message = "Cross-seed verified; existing files reused read-only".into();
    Ok(())
}

pub(super) fn gate(client: &Client, id: &str) -> Result<Option<Arc<Mutex<()>>>> {
    let mut map = client
        .jobs
        .lock()
        .map_err(|_| "BitTorrent state lock is poisoned")?;
    client.load(&mut map, id)?;
    Ok(map
        .get(id)
        .ok_or("Unknown download")?
        .cross_seed
        .as_ref()
        .map(|reuse| reuse.activity.clone()))
}

pub(super) fn retire_proof(job: &mut Job) {
    if job.cross_seed.is_some() {
        job.cancel.store(true, Ordering::Release);
        job.cancel = Arc::new(AtomicBool::new(job.control.seed_limited));
        job.status.ready = false;
        job.status.selected_ready = false;
        job.status.available_files.clear();
        job.status.progress = 0.0;
        job.status.message = "Cross-seed queued for read-only verification".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_verifier_and_announcer_cannot_publish_a_new_generation_and_private_dht_is_disabled() {
        let root = std::env::temp_dir().join(format!(
            "mynou-cross-seed-generation-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let config = DownloadConfig {
            data_dir: root.join("data"),
            state_dir: root.join("state"),
            listen_port: 0,
            seed: true,
            dht: true,
            pex: true,
            max_active: 1,
            max_peers: 1,
        };
        fs::create_dir_all(&config.state_dir).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let info = Value::Dict(BTreeMap::from([
            (b"name".to_vec(), Value::Bytes(b"empty.bin".to_vec())),
            (b"length".to_vec(), Value::Int(0)),
            (b"piece length".to_vec(), Value::Int(16384)),
            (b"pieces".to_vec(), Value::Bytes(Vec::new())),
            (b"private".to_vec(), Value::Int(1)),
        ]));
        let encoded = crate::bencode::encode(&Value::Dict(BTreeMap::from([
            (b"info".to_vec(), info),
            (
                b"announce".to_vec(),
                Value::Bytes(
                    format!("http://{}/announce", listener.local_addr().unwrap()).into_bytes(),
                ),
            ),
        ])));
        let meta = candidate(&encoded).unwrap();
        let id = hex(&meta.v1.unwrap());
        let source_id = "12".repeat(20);
        fs::create_dir_all(config.data_dir.join(&source_id)).unwrap();
        fs::write(config.data_dir.join(&source_id).join("empty.bin"), []).unwrap();
        let reuse = CrossSeed::bind(&source_id, Arc::new(meta.clone()), &meta, None).unwrap();
        let job = queued(meta, reuse, id.clone(), 1, &config).unwrap();
        let old = job.cancel.clone();
        let jobs: Jobs = Arc::new(Mutex::new(BTreeMap::from([(id.clone(), job)])));
        assert!(select_announce(&config, &jobs, &mut None).is_none());
        // Even an uncancelled old verifier must not publish into a replacement generation.
        jobs.lock().unwrap().get_mut(&id).unwrap().cancel = Arc::new(AtomicBool::new(false));
        assert!(verify(&config, &jobs, &old, &id).is_err());
        assert!(!jobs.lock().unwrap()[&id].status.ready);
        let current = jobs.lock().unwrap()[&id].cancel.clone();
        verify(&config, &jobs, &current, &id).unwrap();
        assert!(select_seed_dht(&config, &jobs, &mut None).is_none());
        let task = select_announce(&config, &jobs, &mut None).unwrap();
        retire_proof(jobs.lock().unwrap().get_mut(&id).unwrap());
        announce(&super::super::transport::Direct, task, 6881, [0; 20], &jobs);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(select_announce(&config, &jobs, &mut None).is_none());
    }
}
