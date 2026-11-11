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
    /// The source file index of each candidate payload file, in order; `None`
    /// downloads that file instead. Absent, the source's files map in order.
    pub mapping: Option<Vec<Option<usize>>>,
    /// Bytes of missing non-video files the cross-seed may download.
    pub missing_limit: u64,
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
                    .map(|entry| match entry {
                        Json::Null => Ok(None),
                        entry => entry
                            .as_u64()
                            .and_then(|index| usize::try_from(index).ok())
                            .map(Some)
                            .ok_or_else(|| {
                                "mapping must contain nonnegative integer indices or null".into()
                            }),
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        Ok(Self {
            metadata,
            mapping,
            missing_limit: 0,
        })
    }
}

/// Video files: a cross-seed reuses them and never downloads any of their
/// bytes. Samples count as small extras.
const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "ts", "m2ts", "mts", "wmv", "mov", "webm", "mpg", "mpeg", "vob",
    "iso", "divx", "flv", "ogm", "evo",
];

/// Whether a cross-seed may download this missing file: anything but a video,
/// a sample excepted (`Sample/x.mkv`, `x-sample.mkv`).
pub fn downloadable_extra(path: &Path) -> bool {
    let video = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            VIDEO_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        });
    let sample =
        |word: &str| word.eq_ignore_ascii_case("sample") || word.eq_ignore_ascii_case("samples");
    !video
        || path
            .parent()
            .is_some_and(|parent| parent.iter().any(|part| part.to_str().is_some_and(sample)))
        || path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| {
                let mut words = stem
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|word| !word.is_empty());
                words.next().is_some_and(sample) || words.next_back().is_some_and(sample)
            })
}

/// Where each candidate file's bytes come from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Padding,
    /// This original file of the source transfer, read-only.
    Reused(usize),
    /// Missing from the source: downloaded under the cross-seed's own folder.
    Downloaded,
}

pub(super) struct CrossSeed {
    pub source_id: String,
    pub activity: Arc<Mutex<()>>,
    /// The cross-seed's own identity, which names the folder of its
    /// downloaded files.
    id: String,
    original: Arc<Meta>,
    slots: Vec<Slot>,
    /// Every reused piece verified; the missing ones are being downloaded.
    fetching: AtomicBool,
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
    /// `limit` bounds the bytes to download at registration; a reloaded
    /// record was bounded when it was registered.
    fn bind(
        (source_id, id): (&str, &str),
        original: Arc<Meta>,
        meta: &Meta,
        mapping: Option<&[Option<usize>]>,
        limit: Option<u64>,
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
                .filter_map(|(index, file)| (!file.padding).then_some(Some(index)))
                .take(MAX_FILES + 1)
                .collect()
        };
        let mapping = mapping.unwrap_or(&defaults);
        if mapping.len() != meta.files.iter().filter(|file| !file.padding).count() {
            return Err("Mapping must cover every candidate payload file".into());
        }
        let mut seen = BTreeSet::new();
        let mut next = mapping.iter();
        let mut slots = Vec::with_capacity(meta.files.len());
        let mut missing = 0_u64;
        for file in &meta.files {
            if file.padding {
                slots.push(Slot::Padding);
                continue;
            }
            let Some(index) = *next.next().ok_or("Missing source file mapping")? else {
                if !downloadable_extra(&file.path) {
                    return Err("A cross-seed never downloads video; map every video file".into());
                }
                missing = missing.saturating_add(file.length);
                slots.push(Slot::Downloaded);
                continue;
            };
            let source = original
                .files
                .get(index)
                .ok_or("Source file index is out of bounds")?;
            if source.padding || source.length != file.length || !seen.insert(index) {
                return Err(
                    "Mapped files must have equal sizes, be non-padding and distinct".into(),
                );
            }
            slots.push(Slot::Reused(index));
        }
        if seen.is_empty() {
            return Err("A cross-seed must reuse at least one source file".into());
        }
        if limit.is_some_and(|limit| missing > limit) {
            return Err(format!(
                "The missing files hold {missing} bytes, more than the cross-seed download limit"
            ));
        }
        Ok(Self {
            source_id: source_id.to_owned(),
            id: id.to_owned(),
            original,
            slots,
            activity: Arc::new(Mutex::new(())),
            fetching: AtomicBool::new(false),
        })
    }

    /// Whether some files are downloaded rather than reused.
    pub fn downloads(&self) -> bool {
        self.slots.contains(&Slot::Downloaded)
    }

    /// Every reused piece verified, missing ones still downloading.
    pub fn fetching(&self) -> bool {
        self.fetching.load(Ordering::Acquire)
    }

    fn own_base(&self, config: &DownloadConfig) -> PathBuf {
        config.data_dir.join(&self.id)
    }

    /// The candidate files downloaded into the cross-seed's own folder.
    fn own_files(&self) -> Vec<bool> {
        self.slots
            .iter()
            .map(|slot| *slot == Slot::Downloaded)
            .collect()
    }

    fn open_file(&self, config: &DownloadConfig, index: usize) -> Result<File> {
        let Some(Slot::Reused(original)) = self.slots.get(index).copied() else {
            return Err("Missing source file mapping".into());
        };
        let source = self
            .original
            .files
            .get(original)
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

    /// A downloaded file of this cross-seed, under its own folder.
    fn own_file(&self, config: &DownloadConfig, meta: &Meta, index: usize) -> Result<PathBuf> {
        let base = self.own_base(config);
        safe_directory(&base)?;
        let path = confined(&base, &meta.files[index].path)?;
        inspect_media_file(&path)?;
        Ok(path)
    }

    /// The piece as held here: reused bytes, downloaded files as they stand
    /// and zero padding, not yet verified.
    fn assemble(&self, config: &DownloadConfig, meta: &Meta, index: usize) -> Result<Vec<u8>> {
        let start = index as u64 * meta.piece_length as u64;
        let mut data = vec![0; meta.piece_size(index)];
        let end = start + data.len() as u64;
        for (file_index, entry) in meta.files.iter().enumerate() {
            if entry.padding || entry.offset >= end || entry.offset + entry.length <= start {
                continue;
            }
            let a = start.max(entry.offset);
            let b = end.min(entry.offset + entry.length);
            let target = &mut data[(a - start) as usize..(b - start) as usize];
            match self.slots[file_index] {
                Slot::Padding => {}
                Slot::Reused(_) => {
                    let mut file = self.open_file(config, file_index)?;
                    file.seek(SeekFrom::Start(a - entry.offset))
                        .and_then(|_| file.read_exact(target))
                        .map_err(|_| "Cross-seed source read failed; no download attempted")?;
                }
                Slot::Downloaded => {
                    let mut file = File::open(self.own_file(config, meta, file_index)?)
                        .map_err(|_| "Could not read a downloaded cross-seed file")?;
                    file.seek(SeekFrom::Start(a - entry.offset))
                        .and_then(|_| file.read_exact(target))
                        .map_err(|_| "Truncated downloaded cross-seed file")?;
                }
            }
        }
        Ok(data)
    }

    /// The byte ranges of a piece that belong to downloaded files.
    fn own_ranges(&self, meta: &Meta, index: usize) -> Vec<(usize, usize)> {
        let start = index as u64 * meta.piece_length as u64;
        let end = start + meta.piece_size(index) as u64;
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for (file_index, entry) in meta.files.iter().enumerate() {
            if self.slots[file_index] != Slot::Downloaded
                || entry.offset >= end
                || entry.offset + entry.length <= start
            {
                continue;
            }
            let a = (start.max(entry.offset) - start) as usize;
            let b = (end.min(entry.offset + entry.length) - start) as usize;
            match ranges.last_mut() {
                Some(last) if last.0 + last.1 == a => last.1 += b - a,
                _ => ranges.push((a, b - a)),
            }
        }
        ranges
    }

    /// Writes the downloaded files' part of a verified piece; reused files
    /// are never written.
    fn write_own(
        &self,
        config: &DownloadConfig,
        meta: &Meta,
        index: usize,
        data: &[u8],
    ) -> Result<()> {
        let start = index as u64 * meta.piece_length as u64;
        let end = start + data.len() as u64;
        for (file_index, entry) in meta.files.iter().enumerate() {
            if self.slots[file_index] != Slot::Downloaded
                || entry.offset >= end
                || entry.offset + entry.length <= start
            {
                continue;
            }
            let a = start.max(entry.offset);
            let b = end.min(entry.offset + entry.length);
            let mut file = OpenOptions::new()
                .write(true)
                .open(self.own_file(config, meta, file_index)?)
                .map_err(|_| "Could not write a downloaded cross-seed file")?;
            file.seek(SeekFrom::Start(a - entry.offset))
                .and_then(|_| file.write_all(&data[(a - start) as usize..(b - start) as usize]))
                .map_err(|_| "Could not write a downloaded cross-seed file")?;
        }
        Ok(())
    }

    pub fn read_piece(
        &self,
        config: &DownloadConfig,
        meta: &Meta,
        index: usize,
    ) -> Result<Vec<u8>> {
        let data = self.assemble(config, meta, index)?;
        if !meta.verify(index, &data) {
            return Err(format!(
                "Cross-seed mismatch at piece {index}; no download attempted"
            ));
        }
        Ok(data)
    }

    fn paths(&self, config: &DownloadConfig, meta: &Meta) -> Vec<PathBuf> {
        self.slots
            .iter()
            .zip(&meta.files)
            .filter_map(|(slot, file)| match slot {
                Slot::Padding => None,
                Slot::Reused(index) => Some(
                    config
                        .data_dir
                        .join(&self.source_id)
                        .join(&self.original.files[*index].path),
                ),
                Slot::Downloaded => Some(self.own_base(config).join(&file.path)),
            })
            .collect()
    }
    /// Schema 1 reuses every file; schema 2 marks downloaded files with -1.
    fn encode(&self, meta: &Meta) -> Vec<u8> {
        crate::bencode::encode(&Value::Dict(BTreeMap::from([
            (
                b"schema".to_vec(),
                Value::Int(if self.downloads() { 2 } else { 1 }),
            ),
            (
                b"source".to_vec(),
                Value::Bytes(self.source_id.as_bytes().to_vec()),
            ),
            (b"metadata".to_vec(), Value::Bytes(meta.encoded.clone())),
            (
                b"mapping".to_vec(),
                Value::List(
                    self.slots
                        .iter()
                        .filter_map(|slot| match slot {
                            Slot::Padding => None,
                            Slot::Reused(index) => Some(Value::Int(*index as i64)),
                            Slot::Downloaded => Some(Value::Int(-1)),
                        })
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
        removing: None,
        live_peers: Vec::new(),
        known_peers: 0,
        seeding_peers: BTreeMap::new(),
        inbound: None,
        admitted: true,
        needs_rehash: false,
        seed_queued: false,
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
            files: reuse.paths(config, &meta),
            available_files: Vec::new(),
            message: "Cross-seed verifying existing files".into(),
        },
        meta: Some(Arc::new(meta)),
        cross_seed: Some(Arc::new(reuse)),
        fresh: false,
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
        if !source.status.ready
            || source.failed
            || source.removing.is_some()
            || source.cross_seed.is_some()
        {
            return Err("Cross-seed source must be a fully verified ordinary transfer".into());
        }
        let reuse = CrossSeed::bind(
            (source_id, &id),
            source.meta.clone().ok_or("Missing source metadata")?,
            &meta,
            request.mapping.as_deref(),
            Some(request.missing_limit),
        )?;
        trim_jobs(&self.config, &mut jobs, self.config.resident(31))?;
        if jobs.len() >= self.config.resident(64) {
            return Err("Native transfer working set is busy; retry later".into());
        }
        let order = self
            .index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?
            .next_order()?;
        let encoded = reuse.encode(&meta);
        let mut job = queued(meta, reuse, id.clone(), order, &self.config)?;
        job.fresh = true;
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
    // Seed records require the same physical root binding as registration.
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
    // Schema 2 records download some files (mapping -1); schema 1 reuses all.
    let downloads = match fields.get(b"schema".as_slice()) {
        Some(Value::Int(1)) => false,
        Some(Value::Int(2)) => true,
        _ => return Err("Invalid cross-seed schema".into()),
    };
    if fields.len() != 4 {
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
        .map(|value| match metainfo::integer(value)? {
            -1 if downloads => Ok(None),
            index => usize::try_from(index)
                .map(Some)
                .map_err(|_| "Invalid cross-seed mapping".into()),
        })
        .collect::<Result<Vec<_>>>()?;
    if downloads != mapping.contains(&None) {
        return Err("Invalid cross-seed mapping".into());
    }
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
        (source_id, id),
        source
            .meta
            .clone()
            .ok_or("Cross-seed source metadata is missing")?,
        &meta,
        Some(&mapping),
        None,
    )?;
    queued(meta, reuse, id.to_owned(), 1, config)
}

/// Tries of one piece holding a missing file: past them its reused bytes
/// cannot be right, and a cross-seed never downloads them.
const MAX_PIECE_ATTEMPTS: u32 = 3;
/// A cross-seed whose missing files no peer serves for this long gives its
/// download slot back.
const FETCH_STALL: Duration = Duration::from_secs(600);

/// Verifies every reused piece, then downloads the missing files' bytes, and
/// only those, before the cross-seed seeds.
pub(super) fn verify(
    (config, jobs, stop, id): (&DownloadConfig, &Jobs, &AtomicBool, &str),
    peer_id: &[u8; 20],
    policy: &PolicyRuntime,
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
    let own = reuse.own_files();
    let base = reuse.own_base(config);
    if reuse.downloads() {
        mkdir_private(&base)?;
        safe_directory(&base)?;
        prepare_selected_files(&meta, &base, stop, Some(&own))?;
    }
    // Includes zero-length files that no piece read would otherwise touch.
    let mut stamps = Vec::with_capacity(meta.files.len());
    for (index, slot) in reuse.slots.iter().enumerate() {
        if stop.load(Ordering::Acquire) {
            return Err("Cross-seed verification interrupted".into());
        }
        stamps.push(match slot {
            Slot::Reused(_) => Some(
                reuse
                    .open_file(config, index)?
                    .metadata()
                    .map_err(|_| "Could not inspect cross-seed source")?,
            ),
            _ => None,
        });
    }
    let mut missing = Vec::new();
    let mut verified = 0;
    for index in 0..meta.count() {
        if stop.load(Ordering::Acquire) {
            return Err("Cross-seed verification interrupted".into());
        }
        let data = reuse.assemble(config, &meta, index)?;
        if meta.verify(index, &data) {
            verified += verified_piece_bytes(&meta, index);
        } else if reuse.own_ranges(&meta, index).is_empty() {
            return Err(format!(
                "Cross-seed mismatch at piece {index}; no download attempted"
            ));
        } else {
            missing.push(index);
        }
    }
    counters.verified.store(verified, Ordering::Relaxed);
    if !missing.is_empty() {
        fetch_missing(
            (config, jobs, stop, id),
            (peer_id, policy),
            &reuse,
            &meta,
            missing,
        )?;
    }
    if reuse.downloads() {
        sync_selected_files(&meta, &base, stop, Some(&own))?;
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
    reuse.fetching.store(false, Ordering::Release);
    job.status.progress = 1.0;
    job.status.ready = true;
    job.status.selected_ready = true;
    // Seed-only status never supplies acquisition/import availability.
    job.status.available_files.clear();
    job.status.message = if reuse.downloads() {
        "Cross-seed verified; small extras downloaded, other files reused read-only"
    } else {
        "Cross-seed verified; existing files reused read-only"
    }
    .into();
    Ok(())
}

/// Downloads the pieces that hold missing files. Only the missing files'
/// bytes are requested; the reused bytes come from disk and the whole piece
/// is verified before anything is written.
fn fetch_missing(
    (config, jobs, stop, id): (&DownloadConfig, &Jobs, &AtomicBool, &str),
    (peer_id, policy): (&[u8; 20], &PolicyRuntime),
    reuse: &CrossSeed,
    meta: &Meta,
    mut missing: Vec<usize>,
) -> Result<()> {
    let hash = meta.v1.ok_or("Missing v1 cross-seed identity")?;
    let count = meta.count();
    let (counters, local_gate) = {
        let mut map = jobs
            .lock()
            .map_err(|_| "BitTorrent state lock is poisoned")?;
        let job = map.get_mut(id).ok_or("Unknown cross-seed transfer")?;
        if stop.load(Ordering::Acquire) || !std::ptr::eq(&*job.cancel, stop) {
            return Err("Cross-seed verification interrupted".into());
        }
        // Announces now start, with what is left to download.
        reuse.fetching.store(true, Ordering::Release);
        job.status.progress = (count - missing.len()) as f64 / count as f64;
        job.status.message = "Cross-seed downloading its missing small files".into();
        (job.counters.clone(), job.download_gate.clone())
    };
    let mut attempts = BTreeMap::<usize, u32>::new();
    let mut addresses = BTreeSet::new();
    let mut next_dht = Instant::now();
    let mut progressed = Instant::now();
    while !missing.is_empty() {
        if stop.load(Ordering::Acquire) {
            return Err("Cross-seed verification interrupted".into());
        }
        if progressed.elapsed() >= FETCH_STALL {
            return Err("No peer sent the cross-seed's missing files; resume to try again".into());
        }
        if let Ok(map) = jobs.lock()
            && let Some(job) = map.get(id)
        {
            merge_peers(&mut addresses, job.tracker_peers.iter().copied());
        }
        if config.dht && !meta.private && Instant::now() >= next_dht {
            let peers =
                discovery::dht_lookup(&*policy.network, &hash, config.listen_port, true, stop, &[])
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
        if let Ok(mut map) = jobs.lock()
            && let Some(job) = map.get_mut(id)
        {
            job.peers = peers.clone();
        }
        let mut advanced = false;
        for address in peers {
            if stop.load(Ordering::Acquire) {
                return Err("Cross-seed verification interrupted".into());
            }
            let Ok(mut peer) = wire::Peer::connect_cancellable(
                address,
                &hash,
                peer_id,
                wire::PeerSettings {
                    meta: Some(meta),
                    port: config.listen_port,
                    pex: false,
                    v2_wire: false,
                    counters: counters.clone(),
                    download_gate: policy.download.clone(),
                    local_download_gate: local_gate.clone(),
                    network: &*policy.network,
                    serving: None,
                },
                stop,
            ) else {
                continue;
            };
            for index in missing.clone() {
                if !peer.has_piece(index) {
                    continue;
                }
                let data = reuse.assemble(config, meta, index)?;
                let ranges = reuse.own_ranges(meta, index);
                match peer.fetch_piece_ranges(meta, index, data, &ranges, stop) {
                    Ok(data) => {
                        let _publication = policy.network.publication()?;
                        reuse.write_own(config, meta, index, &data)?;
                        missing.retain(|other| *other != index);
                        counters
                            .verified
                            .fetch_add(verified_piece_bytes(meta, index), Ordering::Relaxed);
                        advanced = true;
                        progressed = Instant::now();
                        if let Ok(mut map) = jobs.lock()
                            && let Some(job) = map.get_mut(id)
                            && std::ptr::eq(&*job.cancel, stop)
                        {
                            job.status.progress = (count - missing.len()) as f64 / count as f64;
                        }
                    }
                    Err(error) if error.contains("does not advertise") => continue,
                    Err(error) if error == "Piece hash mismatch" => {
                        let tries = attempts.entry(index).or_default();
                        *tries += 1;
                        if *tries >= MAX_PIECE_ATTEMPTS {
                            return Err(format!(
                                "Cross-seed piece {index} does not match its reused files; video is never downloaded"
                            ));
                        }
                        break;
                    }
                    Err(_) => break,
                }
            }
            if missing.is_empty() {
                break;
            }
        }
        if !advanced {
            for _ in 0..20 {
                if stop.load(Ordering::Acquire) {
                    return Err("Cross-seed verification interrupted".into());
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
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

/// Whether a seed-only transfer reads the files of transfer `id`. Its seed
/// record names the source; an unreadable one counts as a reuse.
pub(super) fn reused_by(config: &DownloadConfig, id: &str) -> Result<bool> {
    let entries = match fs::read_dir(&config.state_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err("Could not list the transfer state".into()),
    };
    for entry in entries {
        let path = entry
            .map_err(|_| "Could not list the transfer state")?
            .path();
        if path.extension().is_none_or(|extension| extension != "seed") {
            continue;
        }
        let mut encoded = Vec::new();
        let read = (|| -> Result<()> {
            let inspected =
                fs::symlink_metadata(&path).map_err(|_| "Could not inspect cross-seed state")?;
            if !inspected.is_file() || inspected.len() > MAX_RECORD as u64 {
                return Err("Unsafe cross-seed state".into());
            }
            crate::store::open_inspected(&path, &inspected)?
                .take((MAX_RECORD + 1) as u64)
                .read_to_end(&mut encoded)
                .map_err(|_| "Could not read cross-seed state")?;
            if encoded.len() > MAX_RECORD {
                return Err("Cross-seed state exceeds limits".into());
            }
            Ok(())
        })();
        let source = read
            .ok()
            .and_then(|_| crate::bencode::parse(&encoded).ok())
            .and_then(|value| match value {
                Value::Dict(fields) => match fields.get(b"source".as_slice()) {
                    Some(Value::Bytes(source)) => Some(source.clone()),
                    _ => None,
                },
                _ => None,
            });
        if source.is_none_or(|source| source == id.as_bytes()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A completed ordinary transfer that automatic discovery may compare with
/// candidates. Only `ready` sources can be registered; the others wait for
/// their next verification.
pub struct ReuseSource {
    pub id: String,
    /// The metadata name: the single file or the root directory.
    pub name: String,
    /// Original non-padding metadata index, relative path and length.
    pub files: Vec<(usize, PathBuf, u64)>,
    pub total: u64,
    pub ready: bool,
    pub paused: bool,
}

/// The identity and payload layout of a supported cross-seed candidate.
pub struct CandidateLayout {
    pub id: String,
    /// Relative path and length of each non-padding file, in metadata order.
    pub files: Vec<(PathBuf, u64)>,
}

/// Parses candidate metadata under the manual cross-seed limits (v1 only).
pub fn candidate_layout(metadata: &[u8]) -> Result<CandidateLayout> {
    let meta = candidate(metadata)?;
    Ok(CandidateLayout {
        id: hex(&meta.v1.ok_or("Missing v1 cross-seed identity")?),
        files: meta
            .files
            .iter()
            .filter(|file| !file.padding)
            .map(|file| (file.path.clone(), file.length))
            .collect(),
    })
}

impl Client {
    /// Completed ordinary transfers in index order. Seed-only, failed and
    /// unfinished transfers are omitted but still advance the cursor.
    pub fn reuse_sources(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<ReuseSource>, Option<String>)> {
        let (ids, next, _, _) = self
            .index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?
            .page(after, limit)?;
        let mut sources = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(record) = self
                .index
                .lock()
                .map_err(|_| "Native transfer index lock is poisoned")?
                .get(&id)?
            else {
                continue;
            };
            if record.cross_seed || record.failed {
                continue;
            }
            let resident = {
                let jobs = self
                    .jobs
                    .lock()
                    .map_err(|_| "BitTorrent state lock is poisoned")?;
                jobs.get(&id).map(|job| {
                    (
                        job.meta.clone(),
                        job.status.ready && !job.needs_rehash && !job.failed,
                        job.paused || job.control.user_paused,
                    )
                })
            };
            // The index lags a resident verification by up to one save.
            if !record.status.ready && !resident.as_ref().is_some_and(|state| state.1) {
                continue;
            }
            let (meta, ready, paused) = match resident {
                Some(state) => state,
                // Damaged inactive metadata is reported by the transfer itself.
                None => match super::load_job(&self.config, &id) {
                    Ok(job) => (job.meta, false, false),
                    Err(_) => continue,
                },
            };
            let Some(meta) = meta else { continue };
            let files: Vec<_> = meta
                .files
                .iter()
                .enumerate()
                .filter(|(_, file)| !file.padding)
                .map(|(index, file)| (index, file.path.clone(), file.length))
                .collect();
            let Some(name) = meta
                .files
                .first()
                .and_then(|file| file.path.components().next())
                .and_then(|part| part.as_os_str().to_str())
            else {
                continue;
            };
            sources.push(ReuseSource {
                id,
                name: name.to_owned(),
                total: files.iter().map(|file| file.2).sum(),
                files,
                ready,
                paused: paused || record.paused || record.control.user_paused,
            });
        }
        Ok((sources, next))
    }
}

pub(super) fn retire_proof(job: &mut Job) {
    if let Some(reuse) = &job.cross_seed {
        reuse.fetching.store(false, Ordering::Release);
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
    fn only_small_extras_and_samples_may_be_downloaded() {
        for extra in [
            "Show/info.nfo",
            "Show/cover.JPG",
            "Show/Subs/en.srt",
            "Show/Sample/show.mkv",
            "Show/show-sample.MKV",
            "notes.txt",
        ] {
            assert!(downloadable_extra(Path::new(extra)), "{extra}");
        }
        for video in [
            "Show/show.mkv",
            "Show/SHOW.MP4",
            "Show/disc.iso",
            "Show/Samples.of.Life.2020.mkv/extra.m2ts",
            "Example.Show.S01E01.avi",
            "The.Sample.2023.1080p.mkv",
        ] {
            assert!(!downloadable_extra(Path::new(video)), "{video}");
        }
    }

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
            max_seeding: 0,
            max_peers: 1,
        };
        fs::create_dir_all(&config.state_dir).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
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
        let reuse =
            CrossSeed::bind((&source_id, &id), Arc::new(meta.clone()), &meta, None, None).unwrap();
        let job = queued(meta, reuse, id.clone(), 1, &config).unwrap();
        let policy = PolicyRuntime {
            policy: TransferPolicy::default(),
            seed_queue: queue::SeedQueue::new(0),
            download: Arc::new(RateGate::default()),
            upload: Arc::new(RateGate::default()),
            network: super::super::transport::direct(),
        };
        let old = job.cancel.clone();
        let jobs: Jobs = Arc::new(Mutex::new(BTreeMap::from([(id.clone(), job)])));
        assert!(select_announce(&config, &jobs, &mut None).is_none());
        // Even an uncancelled old verifier must not publish into a replacement generation.
        jobs.lock().unwrap().get_mut(&id).unwrap().cancel = Arc::new(AtomicBool::new(false));
        assert!(verify((&config, &jobs, &old, &id), &[0; 20], &policy).is_err());
        assert!(!jobs.lock().unwrap()[&id].status.ready);
        let current = jobs.lock().unwrap()[&id].cancel.clone();
        verify((&config, &jobs, &current, &id), &[0; 20], &policy).unwrap();
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
