//! Torrents added by hand: a magnet link, a torrent URL or the bytes of a
//! .torrent file. The release name is matched against the library with the
//! identity rules of automatic selection; a film or an episode takes the
//! torrent as its chosen release, a season pack fills the missing episodes of
//! a followed series, and anything else downloads as a plain transfer.
use crate::{
    Result,
    crypto::sha256,
    engine::{Engine, lock, public_job},
    integrations,
    json::Value,
    pack::season_title_matches,
    store::{RecordedRelease, Request},
    torrent::{TorrentMetadata, inspect_metadata, url_decode},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::Path,
    time::{Duration, Instant, SystemTime},
};

/// Bounds the bytes of an uploaded .torrent file (the API body holds its Base64).
const MAX_TORRENT: usize = 768 * 1024;
const METADATA_BUDGET: Duration = Duration::from_secs(30);
const MANUAL_GRACE: Duration = Duration::from_secs(3600);
const MANUAL_SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 3600);
const MANUAL_SWEEP_ENTRIES: usize = 200;
const MANUAL_SWEEP_JOB_PAGES: usize = 8;

/// Directory cursors and candidate paths bound each maintenance step's work
/// and memory independently of the number of jobs or transfers.
#[derive(Default)]
pub(crate) struct ManualSweep {
    last_run: Option<Instant>,
    files: Option<fs::ReadDir>,
    batch: Option<ManualBatch>,
}

#[derive(Default)]
struct ManualBatch {
    candidates: BTreeSet<String>,
    after: Option<String>,
    sequence: Option<u64>,
    jobs_done: bool,
    sources: Option<fs::ReadDir>,
}

fn manual_file_name(name: &str) -> bool {
    let Some(digest) = name
        .strip_suffix(".torrent")
        .or_else(|| name.strip_suffix(".partial"))
    else {
        return false;
    };
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn old_manual_file(path: &Path, now: SystemTime) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_file()
            && metadata.modified().is_ok_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|age| age >= MANUAL_GRACE)
            })
    })
}

fn manual_directory(path: &Path) -> Result<()> {
    // Inspect every ancestor too: read_dir would otherwise traverse a link.
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|error| error.to_string())?;
        if !metadata.is_dir() {
            return Err("Manual torrent cleanup requires real directories".into());
        }
    }
    Ok(())
}

pub enum ManualTorrent {
    /// A magnet link or an http(s) URL of a .torrent file.
    Link(String),
    /// The contents of a .torrent file.
    File(Vec<u8>),
}

impl ManualTorrent {
    pub fn from_json(value: &Value) -> Result<Self> {
        match (
            value.get("source").and_then(Value::as_str),
            value.get("torrent").and_then(Value::as_str),
        ) {
            (Some(source), None) => {
                let source = source.trim();
                if !(source.starts_with("magnet:?")
                    || source.starts_with("http://")
                    || source.starts_with("https://"))
                {
                    return Err("Paste a magnet link or the http(s) URL of a .torrent file".into());
                }
                Ok(Self::Link(integrations::acquisition_url(source, "")?))
            }
            (None, Some(torrent)) => Ok(Self::File(decode_base64(torrent)?)),
            _ => {
                Err("Give either a magnet link or URL (source) or a .torrent file (torrent)".into())
            }
        }
    }
}

/// What a release name refers to in the library.
enum Target {
    Media(Request),
    Season {
        series_id: String,
        label: String,
        season: u32,
    },
}

impl Engine {
    pub fn add_torrent(&self, torrent: &ManualTorrent) -> Result<Value> {
        if self.read_only {
            return Err("Read-only preview cannot add a torrent".into());
        }
        let client = self.native_client()?;
        // Keep inspection and publication exclusive with cleanup and duplicate
        // uploads, including when an old file is reused without being written.
        let mut manual_files = lock(&self.manual_sweep)?;
        manual_files.batch = None;
        let (source, kept) = match torrent {
            ManualTorrent::Link(url) => (url.clone(), false),
            ManualTorrent::File(bytes) => self.keep_torrent_file(bytes)?,
        };
        // A magnet names its release; anything else is read for its file names.
        let mut metadata = None;
        let name = match magnet_name(&source)? {
            Some(name) => Some(name),
            None => {
                let inspected = inspect_metadata(
                    &*self.torrent_network,
                    &source,
                    Instant::now() + METADATA_BUDGET,
                );
                let inspected = match (inspected, torrent) {
                    (Ok(inspected), _) => Some(inspected),
                    // A file or URL that cannot be read would not download either.
                    // Nothing refers to a file this call kept yet.
                    (Err(error), ManualTorrent::File(_)) => {
                        if kept {
                            let _ = fs::remove_file(&source);
                        }
                        return Err(error);
                    }
                    (Err(error), _) if !source.starts_with("magnet:") => return Err(error),
                    (Err(_), _) => None,
                };
                let name = inspected.as_ref().and_then(release_name);
                metadata = inspected;
                name
            }
        };
        let mut result = Value::object();
        result.insert("name", name.clone().map_or(Value::Null, Value::String));
        let targets = match &name {
            Some(name) => self.manual_targets(name)?,
            None => Vec::new(),
        };
        let note = match targets.as_slice() {
            [Target::Media(request)] => {
                let job =
                    self.grab_manual(request, &source, name.as_deref().unwrap_or_default())?;
                result.insert("matched", request.kind.clone());
                result.insert("job", public_job(&job));
                return Ok(result);
            }
            [
                Target::Season {
                    series_id,
                    label,
                    season,
                },
            ] => {
                let metadata = match metadata {
                    Some(metadata) => metadata,
                    None => inspect_metadata(
                        &*self.torrent_network,
                        &source,
                        Instant::now() + METADATA_BUDGET,
                    )
                    .map_err(|_| {
                        "The season pack's file list could not be read from its peers; try again or add the .torrent file"
                    })?,
                };
                match self.submit_manual_pack(series_id, *season, &source, &metadata)? {
                    Some(pack) => {
                        result.insert("matched", "season");
                        result.insert("pack", pack);
                        return Ok(result);
                    }
                    None => format!(
                        "{label} season {season} has no missing episode left to fill from a pack"
                    ),
                }
            }
            [] => "Nothing in your library matches this release".into(),
            _ => "This release matches several titles of your library".into(),
        };
        let status = client.ensure(&source)?;
        result.insert("matched", Value::Null);
        result.insert("note", note);
        result.insert("transfer_id", status.id);
        Ok(result)
    }

    /// The uploaded file is kept for as long as its transfer: the native
    /// client and the worker read it again by its path. True when this call
    /// wrote it: the same bytes uploaded earlier share one file.
    fn keep_torrent_file(&self, bytes: &[u8]) -> Result<(String, bool)> {
        let directory = self.config.data_dir.join("state/manual");
        fs::create_dir_all(&directory)
            .map_err(|e| format!("Cannot create {}: {e}", directory.display()))?;
        let digest: String = sha256(bytes).iter().map(|b| format!("{b:02x}")).collect();
        let path = directory.join(format!("{digest}.torrent"));
        let text = path
            .to_str()
            .map(str::to_owned)
            .ok_or("The data directory is not valid UTF-8")?;
        if path.exists() {
            return Ok((text, false));
        }
        let partial = directory.join(format!("{digest}.partial"));
        fs::write(&partial, bytes)
            .and_then(|()| fs::rename(&partial, &path))
            .map_err(|e| format!("Cannot keep the torrent file: {e}"))?;
        Ok((text, true))
    }

    pub(crate) fn maintain_manual_torrents(&self) {
        if self.read_only || !self.config.downloads_enabled || self.native_client().is_err() {
            return;
        }
        let result = (|| {
            // An upload holds this lock while it reads its metadata: the
            // statistics loop that calls this does not wait for it.
            let mut sweep = match self.manual_sweep.try_lock() {
                Ok(sweep) => sweep,
                Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err("Manual torrent cleanup lock is poisoned".to_owned());
                }
            };
            let result = self.sweep_manual_files(&mut sweep, SystemTime::now());
            if result.is_err() {
                // An incomplete reference inventory never authorizes deletion.
                sweep.files = None;
                sweep.batch = None;
            }
            result
        })();
        if let Err(error) = result {
            crate::warn!("Manual torrent cleanup skipped: {error}");
        }
    }

    fn sweep_manual_files(&self, sweep: &mut ManualSweep, now: SystemTime) -> Result<()> {
        let directory = self.config.data_dir.join("state/manual");
        if sweep.files.is_none() && sweep.batch.is_none() {
            if sweep
                .last_run
                .is_some_and(|last| last.elapsed() < MANUAL_SWEEP_INTERVAL)
            {
                return Ok(());
            }
            sweep.last_run = Some(Instant::now());
            match fs::symlink_metadata(&directory) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.to_string()),
                Ok(_) => {}
            }
            manual_directory(&directory)?;
            sweep.files = Some(fs::read_dir(&directory).map_err(|error| error.to_string())?);
        }
        manual_directory(&directory)?;
        let mut batch = sweep.batch.take().unwrap_or_default();
        if batch.candidates.is_empty() {
            let entries = sweep
                .files
                .as_mut()
                .ok_or("Missing manual cleanup cursor")?;
            for _ in 0..MANUAL_SWEEP_ENTRIES {
                let Some(entry) = entries.next() else {
                    sweep.files = None;
                    break;
                };
                let entry = entry.map_err(|error| error.to_string())?;
                if entry.file_name().to_str().is_some_and(manual_file_name)
                    && old_manual_file(&entry.path(), now)
                    && let Some(path) = entry.path().to_str()
                {
                    batch.candidates.insert(path.to_owned());
                }
            }
        }
        if !batch.candidates.is_empty() && !self.sweep_manual_batch(&mut batch, now)? {
            sweep.batch = Some(batch);
        }
        Ok(())
    }

    /// True when this batch is finished, including when uncertainty keeps it.
    fn sweep_manual_batch(&self, batch: &mut ManualBatch, now: SystemTime) -> Result<bool> {
        if !batch.jobs_done {
            for _ in 0..MANUAL_SWEEP_JOB_PAGES {
                let page = lock(&self.store)?.page(batch.after.as_deref(), 200)?;
                if batch
                    .sequence
                    .is_some_and(|sequence| sequence != page.sequence)
                {
                    return Ok(true);
                }
                batch.sequence = Some(page.sequence);
                for job in page.jobs {
                    // Terminal jobs can be retried or participate in rollback.
                    // Pack and shared upgrades carry their URL in the request;
                    // their other provenance fields carry IDs and file bindings.
                    for source in [
                        job.acquisition_url.as_deref(),
                        job.request.source_url.as_deref(),
                        job.request.source_path.as_deref(),
                        job.irc_origin.as_ref().map(|origin| origin.magnet.as_str()),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        batch.candidates.remove(source);
                    }
                }
                batch.after = page.next;
                if batch.candidates.is_empty() {
                    return Ok(true);
                }
                if batch.after.is_none() {
                    batch.jobs_done = true;
                    break;
                }
            }
            if !batch.jobs_done {
                return Ok(false);
            }
        }
        if batch.sources.is_none() {
            manual_directory(&self.config.downloads.state_dir)?;
            batch.sources = Some(
                fs::read_dir(&self.config.downloads.state_dir)
                    .map_err(|error| error.to_string())?,
            );
        }
        let sources = batch
            .sources
            .as_mut()
            .ok_or("Missing native cleanup cursor")?;
        for _ in 0..MANUAL_SWEEP_ENTRIES {
            let Some(entry) = sources.next() else {
                // Recheck the journal for every unlink. Only this short metadata
                // check and unlink hold its lock; directory and source reads do not.
                for path in &batch.candidates {
                    let mut store = lock(&self.store)?;
                    if Some(store.sequence()?) != batch.sequence {
                        break;
                    }
                    manual_directory(&self.config.data_dir.join("state/manual"))?;
                    if old_manual_file(Path::new(path), now) {
                        fs::remove_file(path).map_err(|error| error.to_string())?;
                    }
                }
                return Ok(true);
            };
            let entry = entry.map_err(|error| error.to_string())?;
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "source")
            {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if !metadata.is_file() || metadata.len() > 64 * 1024 {
                return Err("Cannot inventory a native torrent source".into());
            }
            let mut source = String::new();
            crate::store::open_inspected(&path, &metadata)?
                .take(64 * 1024 + 1)
                .read_to_string(&mut source)
                .map_err(|error| error.to_string())?;
            if source.len() > 64 * 1024 {
                return Err("Native torrent source is too large".into());
            }
            batch.candidates.remove(&source);
            if batch.candidates.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Films and episodes the name identifies, then season packs. Each media
    /// identity counts once, whether it is followed, requested or both.
    fn manual_targets(&self, name: &str) -> Result<Vec<Target>> {
        let mut media = BTreeMap::new();
        let mut seasons = Vec::new();
        {
            let mut series = lock(&self.series_store)?;
            let mut after: Option<String> = None;
            loop {
                let page = series.page(after.as_deref(), 200)?;
                for summary in &page.series {
                    let Some(id) = summary.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(record) = series.get(id)? else {
                        continue;
                    };
                    let mut numbers = BTreeSet::new();
                    for episode in &record.plan.episodes {
                        numbers.insert(episode.season);
                        let request = record.episode_request(episode);
                        if integrations::release_identity_matches(&request, name) {
                            media.insert(request.media_key(), request);
                        }
                    }
                    for season in numbers {
                        let mut request = record.plan.request.clone();
                        request.season = season;
                        if season_title_matches(&request, name) {
                            seasons.push(Target::Season {
                                series_id: record.id.clone(),
                                label: record.plan.request.title.clone(),
                                season,
                            });
                        }
                    }
                }
                after = page.next;
                if after.is_none() {
                    break;
                }
            }
        }
        let mut after: Option<String> = None;
        loop {
            let page = lock(&self.store)?.page(after.as_deref(), 200)?;
            for job in page.jobs {
                let request = job.request;
                if matches!(request.kind.as_str(), "movie" | "episode")
                    && request.source_path.is_none()
                    && request.source_url.is_none()
                    && integrations::release_identity_matches(&request, name)
                {
                    media.entry(request.media_key()).or_insert(request);
                }
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        if media.is_empty() {
            return Ok(seasons);
        }
        Ok(media.into_values().map(Target::Media).collect())
    }

    /// As choosing a release from a search report does, with the torrent as
    /// that release.
    fn grab_manual(
        &self,
        request: &Request,
        source: &str,
        name: &str,
    ) -> Result<crate::store::Job> {
        let config = self.configuration_for_request(request)?;
        let (profile, _) = config.selection.profile(&request.kind)?;
        let job = {
            let _publication = self.torrent_publication()?;
            let mut store = lock(&self.store)?;
            let job = store.grab(
                request.clone(),
                source.into(),
                RecordedRelease {
                    source_id: None,
                    size_bytes: None,
                    title: name.into(),
                    profile: profile.into(),
                },
            )?;
            self.retire_previous_transfers(&mut store)?;
            job
        };
        self.requester_operator_interest(std::slice::from_ref(&job))?;
        Ok(job)
    }
}

/// The display name of a magnet, if it has one.
fn magnet_name(source: &str) -> Result<Option<String>> {
    let Some(query) = source.strip_prefix("magnet:?") else {
        return Ok(None);
    };
    for part in query.split('&') {
        if let Some(value) = part.strip_prefix("dn=") {
            let name = url_decode(value)?;
            if !name.trim().is_empty() {
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

/// A torrent's name: its directory, or its only file.
fn release_name(metadata: &TorrentMetadata) -> Option<String> {
    let first = metadata.files.iter().find(|file| !file.padding)?;
    first.path.split('/').next().map(str::to_owned)
}

fn decode_base64(text: &str) -> Result<Vec<u8>> {
    let text = text.trim();
    let text = text
        .strip_suffix("==")
        .or_else(|| text.strip_suffix('='))
        .unwrap_or(text);
    if text.is_empty() || text.len() > MAX_TORRENT.div_ceil(3) * 4 {
        return Err("The .torrent file is empty or larger than 768 KiB".into());
    }
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3 + 2);
    let mut buffer = 0_u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err("The .torrent file is not valid Base64".into()),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, sync::Arc};

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            let suffix: String = crate::crypto::random_bytes::<8>()
                .unwrap()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let directory = Self(std::env::temp_dir().join(format!("mynou-manual-{suffix}")));
            fs::create_dir_all(&directory.0).unwrap();
            directory
        }

        fn engine(&self) -> Arc<Engine> {
            let config = crate::config::from_json(&crate::config::default_json(), &self.0).unwrap();
            fs::create_dir_all(&config.downloads.state_dir).unwrap();
            Engine::open_for_management(config).unwrap()
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn old_file(engine: &Engine, byte: u8, extension: &str) -> PathBuf {
        let directory = engine.config.data_dir.join("state/manual");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!(
            "{}.{}",
            format!("{byte:02x}").repeat(32),
            extension
        ));
        fs::write(&path, b"synthetic torrent").unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(SystemTime::now() - MANUAL_GRACE * 2))
            .unwrap();
        path
    }

    fn request(source: Option<String>) -> Request {
        Request {
            kind: "movie".into(),
            title: "Synthetic manual movie".into(),
            year: 2026,
            season: 0,
            episode: 0,
            tmdb_id: None,
            source_path: None,
            source_url: source,
            runtime_minutes: None,
            source_numbering: None,
            original_language: None,
        }
    }

    fn sweep(engine: &Engine) {
        let mut sweep = ManualSweep::default();
        for _ in 0..100 {
            engine
                .sweep_manual_files(&mut sweep, SystemTime::now())
                .unwrap();
            if sweep.files.is_none() && sweep.batch.is_none() {
                return;
            }
        }
        panic!("manual torrent cleanup did not complete");
    }

    #[test]
    fn manual_sweep_keeps_job_and_persisted_native_sources() {
        let directory = Directory::new();
        let engine = directory.engine();
        let acquisition = old_file(&engine, 1, "torrent");
        let cancelled = old_file(&engine, 2, "torrent");
        let native = old_file(&engine, 3, "torrent");
        let ready = old_file(&engine, 4, "torrent");
        let mut store = lock(&engine.store).unwrap();
        store
            .grab(
                request(None),
                acquisition.to_str().unwrap().into(),
                RecordedRelease {
                    title: "Synthetic manual release".into(),
                    profile: "any".into(),
                    source_id: None,
                    size_bytes: None,
                },
            )
            .unwrap();
        let job = store
            .submit(request(Some(cancelled.to_str().unwrap().into())))
            .unwrap();
        store.cancel(&job.id).unwrap();
        let mut job = store
            .submit(request(Some(ready.to_str().unwrap().into())))
            .unwrap();
        job.state = "ready".into();
        job.imports = vec![directory.0.join("movie.mkv").to_str().unwrap().into()];
        store.update(job).unwrap();
        drop(store);
        fs::write(
            engine
                .config
                .downloads
                .state_dir
                .join(format!("{}.source", "ab".repeat(20))),
            native.to_str().unwrap(),
        )
        .unwrap();
        sweep(&engine);
        for path in [acquisition, cancelled, native, ready] {
            assert!(
                path.exists(),
                "needed source was deleted: {}",
                path.display()
            );
        }
    }

    #[test]
    fn manual_sweep_deletes_old_orphans_and_partials_but_keeps_fresh_files() {
        let directory = Directory::new();
        let engine = directory.engine();
        let orphan = old_file(&engine, 1, "torrent");
        let partial = old_file(&engine, 2, "partial");
        let fresh = old_file(&engine, 3, "torrent");
        let fresh_partial = old_file(&engine, 4, "partial");
        for path in [&fresh, &fresh_partial] {
            fs::File::open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(SystemTime::now()))
                .unwrap();
        }
        sweep(&engine);
        assert!(!orphan.exists());
        assert!(!partial.exists());
        assert!(fresh.exists());
        assert!(fresh_partial.exists());
    }

    #[test]
    fn manual_sweep_ignores_unexpected_names_directories_and_symlinks() {
        use std::os::unix::fs::symlink;
        let directory = Directory::new();
        let engine = directory.engine();
        let unexpected = old_file(&engine, 0xab, "torrent");
        let upper = unexpected.with_file_name(format!("{}.torrent", "AB".repeat(32)));
        fs::rename(unexpected, &upper).unwrap();
        let manual = engine.config.data_dir.join("state/manual");
        let target = directory.0.join("outside.torrent");
        fs::write(&target, b"keep").unwrap();
        let link = manual.join(format!("{}.torrent", "12".repeat(32)));
        symlink(&target, &link).unwrap();
        let subdirectory = manual.join(format!("{}.torrent", "34".repeat(32)));
        fs::create_dir(&subdirectory).unwrap();
        let nested = subdirectory.join(format!("{}.torrent", "56".repeat(32)));
        fs::write(&nested, b"keep").unwrap();
        sweep(&engine);
        assert!(upper.exists());
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert!(target.exists());
        assert!(nested.exists());
        fs::remove_dir_all(&manual).unwrap();
        symlink(&directory.0, &manual).unwrap();
        assert!(
            engine
                .sweep_manual_files(&mut ManualSweep::default(), SystemTime::now())
                .is_err()
        );
    }

    #[test]
    fn manual_sweep_is_disabled_without_a_native_client_and_in_preview() {
        let directory = Directory::new();
        let engine = directory.engine();
        let orphan = old_file(&engine, 1, "torrent");
        engine.maintain_manual_torrents();
        assert!(orphan.exists());
        let config = engine.config.clone();
        drop(engine);
        let preview = Engine::open_for_preview(config).unwrap();
        preview.maintain_manual_torrents();
        assert!(orphan.exists());
    }

    #[test]
    fn manual_sweep_resumes_bounded_native_inventory_and_observes_daily_interval() {
        let directory = Directory::new();
        let engine = directory.engine();
        let orphan = old_file(&engine, 1, "torrent");
        for index in 0..MANUAL_SWEEP_ENTRIES + 1 {
            fs::write(
                engine
                    .config
                    .downloads
                    .state_dir
                    .join(format!("{index}.other")),
                b"",
            )
            .unwrap();
        }
        let mut sweep = ManualSweep::default();
        engine
            .sweep_manual_files(&mut sweep, SystemTime::now())
            .unwrap();
        assert!(orphan.exists());
        assert!(sweep.batch.is_some());
        engine
            .sweep_manual_files(&mut sweep, SystemTime::now())
            .unwrap();
        assert!(!orphan.exists());
        let another = old_file(&engine, 2, "torrent");
        engine
            .sweep_manual_files(&mut sweep, SystemTime::now())
            .unwrap();
        assert!(another.exists());
    }

    #[test]
    fn manual_sweep_pages_all_jobs_and_keeps_a_changed_inventory() {
        let directory = Directory::new();
        let engine = directory.engine();
        let needed = old_file(&engine, 1, "torrent");
        let orphan = old_file(&engine, 2, "torrent");
        let mut store = lock(&engine.store).unwrap();
        store
            .submit(request(Some(needed.to_str().unwrap().into())))
            .unwrap();
        for index in 0..201 {
            let mut request = request(None);
            request.title = format!("Synthetic movie {index}");
            store.submit(request).unwrap();
        }
        // The needed source is older than a full page of unrelated jobs.
        assert!(store.page(None, 200).unwrap().next.is_some());
        drop(store);
        sweep(&engine);
        assert!(needed.exists());
        assert!(!orphan.exists());

        let orphan = old_file(&engine, 3, "torrent");
        let page = lock(&engine.store).unwrap().page(None, 200).unwrap();
        let mut batch = ManualBatch {
            candidates: BTreeSet::from([orphan.to_str().unwrap().into()]),
            sequence: Some(page.sequence),
            after: page.next,
            ..ManualBatch::default()
        };
        lock(&engine.store)
            .unwrap()
            .submit(request(Some("magnet:?changed".into())))
            .unwrap();
        assert!(
            engine
                .sweep_manual_batch(&mut batch, SystemTime::now())
                .unwrap()
        );
        assert!(orphan.exists());
    }

    #[test]
    fn manual_sweep_keeps_orphans_when_a_native_source_cannot_be_inventoried() {
        use std::os::unix::fs::symlink;
        let directory = Directory::new();
        let engine = directory.engine();
        let orphan = old_file(&engine, 1, "torrent");
        symlink(
            &orphan,
            engine
                .config
                .downloads
                .state_dir
                .join(format!("{}.source", "ab".repeat(20))),
        )
        .unwrap();
        assert!(
            engine
                .sweep_manual_files(&mut ManualSweep::default(), SystemTime::now())
                .is_err()
        );
        assert!(orphan.exists());
    }

    #[test]
    fn magnets_name_their_release_and_base64_round_trips() {
        assert_eq!(
            magnet_name("magnet:?xt=urn:btih:abc&dn=Dune.Part.Two.2024.2160p").unwrap(),
            Some("Dune.Part.Two.2024.2160p".into())
        );
        assert_eq!(magnet_name("magnet:?xt=urn:btih:abc").unwrap(), None);
        assert_eq!(magnet_name("https://example.test/a.torrent").unwrap(), None);
        for data in [&b"d4:infod"[..], b"x", b"xy", b"xyz"] {
            assert_eq!(decode_base64(&crate::ws::base64(data)).unwrap(), data);
        }
        assert!(decode_base64("not base64!").is_err());
    }
}
