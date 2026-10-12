//! Recoverable migration of the request journal and snapshot of earlier
//! versions into the job index.
//!
//! Steps, each safe to repeat after an interruption:
//! 1. Parse and validate the whole legacy state (checksums, hash chain,
//!    records, lineage, ownership). Nothing is written when it is invalid.
//! 2. Preserve the exact bytes as `snapshot.legacy-<digest>.bin` and
//!    `journal.legacy-<digest>.bin`, never replacing a file with other content.
//! 3. Build `jobs.idx.migrating`, one transaction per few jobs, recording the
//!    source digest and a cursor in the same transactions so a restart resumes.
//! 4. Verify every record, entry, counter and history row against the parse.
//! 5. Promote the staging directory to `jobs.idx` (atomic rename).
//! 6. Replace `snapshot.bin` with a marker earlier versions reject, then empty
//!    `journal.bin`. Until step 6 an older binary still finds the legacy files.
//!
//! Downgrade means restoring both preserved files as `snapshot.bin` and
//! `journal.bin` and moving `jobs.idx` aside; changes made after the migration
//! exist only in the index and are not in that copy.
use super::{
    Backend, Change, Event, Index, Result, Store,
    layout::{self, Entry, FORMAT, Root, id_bytes, owned},
    legacy::{self, Inspection, State},
    ops::{Counts, Db, encode_event},
};
use crate::{
    crypto::{random_bytes, sha256},
    inventory::{Key, MAX_PAGE_ROWS, Query},
    store::{private_options, reject_symlinks, sync_directory},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

const INDEX: &str = "jobs.idx";
const STAGING: &str = "jobs.idx.migrating";
/// Jobs written by one migration transaction.
const CHUNK: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationReport {
    pub jobs: u64,
    pub events: u64,
    /// The migration continued an earlier interrupted attempt.
    pub resumed: bool,
    /// Digest prefix naming the preserved `snapshot.legacy-*.bin` and
    /// `journal.legacy-*.bin` files inside the store directory.
    pub backup: String,
}

/// Interruption points exercised by the crash tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    BackupWritten,
    StagingCreated,
    Chunk(u64),
    Verified,
    Promoted,
    MarkerWritten,
    JournalCleared,
}

pub(super) fn open(directory: &Path, lock: File, read_only: bool) -> Result<Store> {
    open_with(directory, lock, read_only, &mut |_| Ok(()))
}

fn ready(
    directory: &Path,
    lock: File,
    index: Index,
    read_only: bool,
    report: Option<MigrationReport>,
) -> Result<Store> {
    Ok(Store::from_parts(
        directory,
        lock,
        Backend::Ready(Box::new(index)),
        read_only,
        report,
    ))
}

pub(super) fn open_with(
    directory: &Path,
    lock: File,
    read_only: bool,
    hook: &mut dyn FnMut(Stage) -> Result<()>,
) -> Result<Store> {
    let index_path = directory.join(INDEX);
    reject_symlinks(&index_path)?;
    let inspection = legacy::inspect(directory, false)?;
    match (inspection, exists(&index_path)?) {
        // Nothing stored yet. A leftover staging directory holds only derived
        // data (or an unfinished empty index) and is replaced on first write.
        (Inspection::Missing, false) => {
            if read_only {
                return Ok(Store::from_parts(directory, lock, Backend::Absent, true, None));
            }
            let index = create_fresh(directory)?;
            ready(directory, lock, index, false, None)
        }
        (Inspection::Missing, true) => {
            let index = open_index(&index_path, read_only)?;
            if !read_only {
                write_marker(directory, &index)?;
            }
            ready(directory, lock, index, read_only, None)
        }
        (Inspection::Marker { binding }, true) => {
            let mut index = open_index(&index_path, read_only)?;
            if binding != legacy::marker_binding(&index.identity()) {
                return Err(
                    "snapshot.bin names a different job index; restore the preserved snapshot or the matching index"
                        .into(),
                );
            }
            settle_journal(directory, &mut index, read_only)?;
            ready(directory, lock, index, read_only, None)
        }
        (Inspection::Marker { .. }, false) => Err(
            "The job index is missing. Restore it, or restore the preserved snapshot and journal (snapshot.legacy-*.bin, journal.legacy-*.bin) to migrate again"
                .into(),
        ),
        (Inspection::Legacy(files), true) => {
            // A crash between promotion and the marker, or legacy files that
            // were restored after migrating. Only unchanged files are safe.
            let mut index = open_index(&index_path, read_only)?;
            if source_of(&mut index)?.as_deref() != Some(&files.digest[..]) {
                return Err(
                    "snapshot.bin and journal.bin differ from the files that produced jobs.idx; move jobs.idx aside to migrate them again, or restore the index marker"
                        .into(),
                );
            }
            if !read_only {
                write_marker(directory, &index)?;
                settle_journal(directory, &mut index, false)?;
            }
            ready(directory, lock, index, read_only, None)
        }
        (Inspection::Legacy(_), false) if read_only => Ok(Store::from_parts(
            directory,
            lock,
            Backend::Unmigrated,
            true,
            None,
        )),
        (Inspection::Legacy(files), false) => migrate(directory, lock, files, hook),
    }
}

fn open_index(path: &Path, read_only: bool) -> Result<Index> {
    let mut index = Index::open(path, read_only)?;
    check_format(&mut index, read_only)?;
    Ok(index)
}

fn check_format(index: &mut Index, read_only: bool) -> Result<()> {
    match index.get(&layout::FORMAT_KEY)? {
        Some(value) if value == FORMAT => Db(index).counts().map(|_| ()),
        Some(_) => Err("Unsupported job index format".into()),
        None if index.is_empty()? && !read_only => initialize(index),
        None => Err("Job index is missing its format marker".into()),
    }
}

/// Writes the format marker and zero counters into an empty index.
fn initialize(index: &mut Index) -> Result<()> {
    index.commit(vec![
        Change::PutInline(layout::FORMAT_KEY, FORMAT.to_vec()),
        Change::PutInline(layout::COUNTS_KEY, Counts::default().encode()),
        Change::PutInline(layout::LIBRARY_COUNT_KEY, 0_u64.to_le_bytes().to_vec()),
    ])?;
    Ok(())
}

fn source_of(index: &mut Index) -> Result<Option<Vec<u8>>> {
    index.get(&layout::MIGRATION_SOURCE_KEY)
}

fn cursor_of(index: &mut Index) -> Result<Option<String>> {
    Ok(index
        .get(&layout::MIGRATION_CURSOR_KEY)?
        .map(|bytes| layout::id_text(&bytes)))
}

/// After the marker, `journal.bin` must be empty. A leftover from an
/// interrupted migration is emptied once it provably equals the preserved copy.
fn settle_journal(directory: &Path, index: &mut Index, read_only: bool) -> Result<()> {
    let path = directory.join(legacy::JOURNAL);
    let Some(bytes) = legacy::read_bounded(&path, legacy::MAX_JOURNAL_BYTES, !read_only)? else {
        return Ok(());
    };
    if bytes.is_empty() {
        return Ok(());
    }
    let preserved = source_of(index)?.and_then(|digest| {
        digest
            .get(..8)
            .map(|prefix| directory.join(format!("journal.legacy-{}.bin", super::hex(prefix))))
    });
    let matches = match preserved {
        Some(copy) => legacy::read_bounded(&copy, legacy::MAX_JOURNAL_BYTES, false)?
            .is_some_and(|saved| sha256(&saved) == sha256(&bytes)),
        None => false,
    };
    if !matches {
        return Err(
            "journal.bin has content that is not in the preserved copy; refusing to continue"
                .into(),
        );
    }
    if read_only {
        return Ok(());
    }
    let file = private_options()
        .write(true)
        .open(&path)
        .map_err(|_| "Cannot open the legacy journal".to_owned())?;
    file.set_len(0)
        .and_then(|()| file.sync_all())
        .map_err(|_| "Cannot empty the legacy journal".to_owned())?;
    sync_directory(directory)
}

fn migrate(
    directory: &Path,
    lock: File,
    files: legacy::Files,
    hook: &mut dyn FnMut(Stage) -> Result<()>,
) -> Result<Store> {
    let state = legacy::parse(files.snapshot.as_deref(), &files.journal)?;
    legacy::validate(&state)?;
    let plan = Plan::new(&state)?;
    let tag = super::hex(&files.digest[..8]);
    if let Some(snapshot) = &files.snapshot {
        write_backup(directory, &format!("snapshot.legacy-{tag}.bin"), snapshot)?;
    }
    if !files.journal.is_empty() {
        write_backup(
            directory,
            &format!("journal.legacy-{tag}.bin"),
            &files.journal,
        )?;
    }
    hook(Stage::BackupWritten)?;

    let staging_path = directory.join(STAGING);
    let mut resumed = false;
    let mut staging = None;
    if exists(&staging_path)? {
        // Inspect without recovery writes. Unrecognized or corrupt data is
        // preserved for the operator; it is never treated as disposable.
        let mut index = Index::open(&staging_path, true)?;
        check_format(&mut index, true)?;
        let matches = source_of(&mut index)?.as_deref() == Some(&files.digest[..]);
        drop(index);
        if matches {
            resumed = true;
            staging = Some(Index::open(&staging_path, false)?);
        } else {
            preserve_staging(directory)?;
        }
    }
    let mut staging = match staging {
        Some(index) => index,
        None => {
            let mut index = Index::create(&staging_path)?;
            initialize(&mut index)?;
            index.commit(vec![Change::PutInline(
                layout::MIGRATION_SOURCE_KEY,
                files.digest.to_vec(),
            )])?;
            index
        }
    };
    hook(Stage::StagingCreated)?;

    let cursor = cursor_of(&mut staging)?;
    let ids: Vec<&String> = state.jobs.keys().collect();
    let mut written = 0_u64;
    for chunk in ids.chunks(CHUNK) {
        written += 1;
        let last = chunk.last().map(|id| id.as_str()).unwrap_or_default();
        if cursor.as_deref().is_some_and(|done| last <= done) {
            continue;
        }
        let mut entries: BTreeMap<Key, Entry> = BTreeMap::new();
        for id in chunk {
            plan.entries(&state, id, &mut entries)?;
        }
        let mut changes: Vec<Change> = entries
            .into_iter()
            .map(|(key, entry)| entry.change(key))
            .collect();
        changes.push(Change::PutInline(
            layout::MIGRATION_CURSOR_KEY,
            id_bytes(last)?.to_vec(),
        ));
        staging.commit(changes)?;
        hook(Stage::Chunk(written))?;
    }
    // Counters and the format marker are written last, together, so a
    // half-built staging index is never taken for a complete one.
    let counts = plan.counts(&state);
    staging.commit(vec![
        Change::PutInline(layout::COUNTS_KEY, counts.encode()),
        Change::PutInline(
            layout::LIBRARY_COUNT_KEY,
            counts.library.to_le_bytes().to_vec(),
        ),
    ])?;
    verify(&mut staging, &state, &plan, counts)?;
    if staging.get(&layout::MIGRATION_CURSOR_KEY)?.is_some() {
        staging.commit(vec![Change::Delete(layout::MIGRATION_CURSOR_KEY)])?;
    }
    hook(Stage::Verified)?;
    drop(staging);

    let index_path = directory.join(INDEX);
    fs::rename(&staging_path, &index_path)
        .map_err(|_| "Cannot promote the job index".to_owned())?;
    sync_directory(directory)?;
    hook(Stage::Promoted)?;
    let mut index = Index::open(&index_path, false)?;
    write_marker(directory, &index)?;
    hook(Stage::MarkerWritten)?;
    settle_journal(directory, &mut index, false)?;
    hook(Stage::JournalCleared)?;
    let report = MigrationReport {
        jobs: state.jobs.len() as u64,
        events: plan.events.values().map(|list| list.len() as u64).sum(),
        resumed,
        backup: format!("legacy-{tag}"),
    };
    ready(directory, lock, index, false, Some(report))
}

/// What the legacy state becomes: lineage roots, the library tip of each chain
/// and media identity, and the retained history per job.
struct Plan {
    roots: BTreeMap<String, Root>,
    /// Candidate per (media, root) chain: the newest import not replaced.
    candidates: BTreeMap<Key, [u8; 24]>,
    winners: BTreeMap<[u8; 32], [u8; 24]>,
    events: BTreeMap<String, Vec<Event>>,
    sequence: u64,
}

impl Plan {
    fn new(state: &State) -> Result<Self> {
        // Every record must fit its bound before anything is written.
        for job in state.jobs.values() {
            if layout::encode_job(job).len() > legacy::MAX_RECORD {
                return Err("A job record exceeds the 16 MiB record limit".into());
            }
        }
        let root_ids = legacy::roots(&state.jobs)?;
        let mut roots = BTreeMap::new();
        for (id, root) in &root_ids {
            let created = state
                .jobs
                .get(root)
                .ok_or("missing upgrade parent reference")?
                .created_at;
            roots.insert(
                id.clone(),
                Root {
                    id: id_bytes(root)?,
                    created_at: created,
                },
            );
        }
        let replaced: BTreeSet<&str> = state
            .jobs
            .values()
            .filter(|job| layout::imported(job))
            .filter_map(|job| job.upgrade_parent.as_deref())
            .collect();
        let mut candidates = BTreeMap::new();
        let mut winners: BTreeMap<[u8; 32], (Key, [u8; 24])> = BTreeMap::new();
        for job in state.jobs.values() {
            if !layout::imported(job) || replaced.contains(job.id.as_str()) {
                continue;
            }
            let media = layout::digest_bytes(&job.request.media_key())?;
            let root = roots
                .get(&job.id)
                .ok_or("Missing lineage root for a library entry")?;
            let key = layout::candidate_key(&media, root.created_at, &root.id);
            let mut value = [0_u8; 24];
            value[..16].copy_from_slice(&id_bytes(&job.id)?);
            value[16..].copy_from_slice(&job.created_at.to_be_bytes());
            if candidates.insert(key, value).is_some() {
                return Err("Two current library versions share one lineage".into());
            }
            if winners.get(&media).is_none_or(|(known, _)| key > *known) {
                winners.insert(media, (key, value));
            }
        }
        let mut events: BTreeMap<String, Vec<Event>> = BTreeMap::new();
        for event in &state.events {
            events
                .entry(event.job_id.clone())
                .or_default()
                .push(event.clone());
        }
        for list in events.values_mut() {
            list.sort_by_key(|event| event.id);

        }
        Ok(Self {
            roots,
            candidates,
            winners: winners
                .into_iter()
                .map(|(media, (_, value))| (media, value))
                .collect(),
            events,
            sequence: state.sequence,
        })
    }

    fn counts(&self, state: &State) -> Counts {
        Counts {
            jobs: state.jobs.len() as u64,
            active: state
                .jobs
                .values()
                .filter(|job| !layout::terminal(&job.state))
                .count() as u64,
            ready: state
                .jobs
                .values()
                .filter(|job| job.state == "ready")
                .count() as u64,
            events: self.sequence,
            library: self.winners.len() as u64,
        }
    }

    /// Every entry the migrated job owns, including its derived library rows
    /// and history.
    fn entries(&self, state: &State, id: &str, out: &mut BTreeMap<Key, Entry>) -> Result<()> {
        let job = state.jobs.get(id).ok_or("Missing legacy job")?;
        let root = self.roots.get(id).ok_or("Missing lineage root")?;
        if layout::encode_job(job).len() > legacy::MAX_RECORD {
            return Err("A job record exceeds the 16 MiB record limit".into());
        }
        out.extend(owned(job, root)?);
        let raw = id_bytes(id)?;
        let media = layout::digest_bytes(&job.request.media_key())?;
        let key = layout::candidate_key(&media, root.created_at, &root.id);
        if let Some(value) = self.candidates.get(&key)
            && value[..16] == raw
        {
            out.insert(key, Entry::Inline(value.to_vec()));
            if self.winners.get(&media) == Some(value) {
                out.insert(layout::media_tip_key(&media), Entry::Inline(value.to_vec()));
                out.insert(
                    layout::library_key(job.created_at, &raw),
                    Entry::Inline(Vec::new()),
                );
                out.insert(
                    layout::tip_key(&raw),
                    Entry::Inline(vec![
                        u8::from(job.monitored),
                        u8::from(job.release.is_some()),
                    ]),
                );
            }
        }
        if let Some(log) = self.events.get(id) {
            let mut metadata = (log.len() as u64).to_le_bytes().to_vec();
            metadata.extend_from_slice(&log.last().map_or(0, |event| event.id).to_le_bytes());
            out.insert(layout::events_key(&raw, 0), Entry::Inline(metadata));
            for event in log {
                out.insert(layout::events_key(&raw, event.id), Entry::File(encode_event(event)));
                out.insert(layout::sequence_key(event.id), Entry::Inline(raw.to_vec()));
            }
        }
        Ok(())
    }
}

/// Confirms every job, entry, counter and history row against the parse, and
/// that the index holds nothing else.
fn verify(index: &mut Index, state: &State, plan: &Plan, counts: Counts) -> Result<()> {
    let failed = |what: &str| format!("Job migration verification failed: {what}");
    let stored = Db(index).counts()?;
    if stored != counts {
        return Err(failed("counters disagree"));
    }
    let mut expected_events = 0_u64;
    for id in state.jobs.keys() {
        let mut entries = BTreeMap::new();
        plan.entries(state, id, &mut entries)?;
        let raw = id_bytes(id)?;
        let job = Db(index).job(&raw)?;
        if job.as_ref() != state.jobs.get(id) {
            return Err(failed("a record differs"));
        }
        for (key, entry) in entries {
            if index.get(&key)?.as_deref() != Some(entry.bytes()) {
                return Err(failed("an index entry differs"));
            }
        }
        let log = Db(index).events(&raw)?;
        if log != plan.events.get(id).cloned().unwrap_or_default() {
            return Err(failed("a job history differs"));
        }
        expected_events += log.len() as u64;
    }
    let count_table = |index: &mut Index, table: u8| -> Result<u64> {
        let (start, end) = layout::prefix_range(&[table]);
        let mut after = None;
        let mut total = 0_u64;
        loop {
            let page = index.page(&Query {
                start,
                end,
                after,
                limit: MAX_PAGE_ROWS,
                sequence: None,
            })?;
            total += page.entries.len() as u64;
            after = page.next;
            if after.is_none() {
                return Ok(total);
            }
        }
    };
    if count_table(index, layout::T_JOB)? != state.jobs.len() as u64
        || count_table(index, layout::T_ALL)? != state.jobs.len() as u64
        || count_table(index, layout::T_SEQUENCE)? != expected_events
        || count_table(index, layout::T_CANDIDATE)? != plan.candidates.len() as u64
        || count_table(index, layout::T_LIBRARY)? != plan.winners.len() as u64
        || count_table(index, layout::T_TIP)? != plan.winners.len() as u64
    {
        return Err(failed("an index holds entries the legacy state does not"));
    }
    Ok(())
}

/// Creates the index for a store that has never held jobs, atomically.
fn create_fresh(directory: &Path) -> Result<Index> {
    let staging_path = directory.join(STAGING);
    if exists(&staging_path)? {
        preserve_staging(directory)?;
    }
    let mut index = Index::create(&staging_path)?;
    initialize(&mut index)?;
    drop(index);
    let index_path = directory.join(INDEX);
    fs::rename(&staging_path, &index_path)
        .map_err(|_| "Cannot publish the job index".to_owned())?;
    sync_directory(directory)?;
    let index = Index::open(&index_path, false)?;
    write_marker(directory, &index)?;
    Ok(index)
}

/// Preserves a recognized unfinished index under a unique name before
/// rebuilding. Even unexpected files inside it are retained, never deleted.
fn preserve_staging(directory: &Path) -> Result<()> {
    let path = directory.join(STAGING);
    let mut index = Index::open(&path, true)?;
    match index.get(&layout::FORMAT_KEY)? {
        Some(value) if value == FORMAT => {},
        None if index.is_empty()? => {},
        _ => return Err("Unrecognized job staging data; move it aside explicitly before retrying".into()),
    }
    drop(index);
    let mut random = [0; 16];
    random_bytes(&mut random)?;
    let preserved = directory.join(format!("jobs.idx.abandoned-{}", super::hex(&random)));
    if exists(&preserved)? {
        return Err("Cannot choose a unique preserved staging name".into());
    }
    fs::rename(&path, &preserved).map_err(|_| "Cannot preserve unfinished job staging data".to_owned())?;
    sync_directory(directory)
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Cannot inspect job storage".to_owned()),
    }
}

/// Atomically replaces `snapshot.bin` with the index marker.
fn write_marker(directory: &Path, index: &Index) -> Result<()> {
    replace_file(
        directory,
        &directory.join(legacy::SNAPSHOT),
        &legacy::marker_bytes(&index.identity()),
    )
}

fn write_backup(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = directory.join(name);
    reject_symlinks(&path)?;
    if exists(&path)? {
        let refuse = || {
            format!(
                "{name} exists with different content or is not a private regular file; refusing to overwrite a preserved copy"
            )
        };
        let metadata = fs::symlink_metadata(&path).map_err(|_| refuse())?;
        // Only a file of exactly the expected size is ever read, so a foreign
        // or huge file cannot be pulled into memory.
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.len() != bytes.len() as u64
        {
            return Err(refuse());
        }
        let mut file = private_options()
            .read(true)
            .open(&path)
            .map_err(|_| refuse())?;
        let opened = file.metadata().map_err(|_| refuse())?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(refuse());
        }
        let mut existing = Vec::with_capacity(bytes.len());
        Read::take(&mut file, bytes.len() as u64 + 1)
            .read_to_end(&mut existing)
            .map_err(|_| refuse())?;
        if sha256(&existing) != sha256(bytes) {
            return Err(refuse());
        }
        return Ok(());
    }
    replace_file(directory, &path, bytes)
}

fn replace_file(directory: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    let temporary: PathBuf =
        directory.join(format!(".jobs-{}.tmp", super::hex(&random_bytes::<16>()?)));
    let write = (|| -> Result<()> {
        let mut file = private_options()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| "Cannot create a job storage file".to_owned())?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| "Cannot persist a job storage file".to_owned())?;
        fs::rename(&temporary, target)
            .map_err(|_| "Cannot replace a job storage file".to_owned())?;
        sync_directory(directory)
    })();
    if write.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write
}
