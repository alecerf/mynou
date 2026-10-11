//! Recoverable migration of the legacy `series.json` snapshot into the index.
//!
//! Steps, each safe to repeat after an interruption:
//! 1. Validate the whole legacy snapshot (checksum, records, identity
//!    invariants). Nothing is written when it is invalid.
//! 2. Preserve its exact bytes as `series.legacy-<digest>.json`.
//! 3. Build `series.idx.migrating`, one transaction per record, recording the
//!    legacy digest and a cursor in the same transactions so a restart resumes.
//! 4. Verify every record and index entry against the legacy parse.
//! 5. Promote the staging directory to `series.idx` (atomic rename).
//! 6. Replace `series.json` with a schema 3 marker bound to the index identity.
//!    Older binaries reject schema 3, so they can never read stale data.
//!
//! Downgrade means restoring the preserved snapshot as `series.json` and moving
//! `series.idx` aside; changes made after the migration are not in that copy.
use super::{
    legacy::{self, Snapshot},
    storage::{
        Backend, SeriesStore, check_format, clear_cursor, counts_of, cursor_of, initialize,
        load_record, migration_changes, save_record, source_of, verify_record,
    },
};
use crate::{
    Result,
    crypto::{random_bytes, sha256},
    inventory::{Index, Query},
    store::{private_options, reject_symlinks, sync_directory},
};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const SNAPSHOT: &str = "series.json";
const INDEX: &str = "series.idx";
const STAGING: &str = "series.idx.migrating";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationReport {
    pub records: u64,
    pub episodes: u64,
    /// The migration continued an earlier interrupted attempt.
    pub resumed: bool,
    /// File name, inside the store directory, of the preserved legacy snapshot.
    pub backup: String,
}

/// Interruption points exercised by the crash tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    BackupWritten,
    StagingCreated,
    Record(u64),
    Verified,
    Promoted,
    MarkerWritten,
}

pub(super) fn open(directory: &Path, read_only: bool) -> Result<SeriesStore> {
    open_with(directory, read_only, &mut |_| Ok(()))
}

pub(super) fn open_with(
    directory: &Path,
    read_only: bool,
    hook: &mut dyn FnMut(Stage) -> Result<()>,
) -> Result<SeriesStore> {
    let snapshot_path = directory.join(SNAPSHOT);
    let index_path = directory.join(INDEX);
    reject_symlinks(&index_path)?;
    let snapshot = legacy::inspect(&snapshot_path, !read_only)?;
    let ready = |index: Index, report| {
        Ok(SeriesStore::from_parts(
            directory,
            Backend::Ready(Box::new(index)),
            read_only,
            report,
        ))
    };
    match (snapshot, exists(&index_path)?) {
        // Nothing stored yet. A leftover staging directory holds only derived
        // data (or an unfinished empty index) and is replaced on first write.
        (Snapshot::Missing, false) => Ok(SeriesStore::from_parts(
            directory,
            Backend::Absent,
            read_only,
            None,
        )),
        (Snapshot::Missing, true) => {
            let index = open_index(&index_path, read_only)?;
            if !read_only {
                write_marker(directory, &index)?;
            }
            ready(index, None)
        }
        (Snapshot::Marker { binding }, true) => {
            let index = open_index(&index_path, read_only)?;
            if binding != legacy::marker_binding(&index.identity()) {
                return Err(
                    "series.json names a different series index; restore the preserved snapshot or the matching index"
                        .into(),
                );
            }
            ready(index, None)
        }
        (Snapshot::Marker { .. }, false) => Err(
            "The series index is missing. Restore it, or restore the preserved snapshot (series.legacy-*.json) as series.json to migrate again"
                .into(),
        ),
        (Snapshot::Legacy { digest, .. }, true) => {
            // A crash between promotion and the marker, or a snapshot that was
            // restored after migrating. Only an unchanged snapshot is safe.
            let mut index = open_index(&index_path, read_only)?;
            if source_of(&mut index)?.as_deref() != Some(&digest[..]) {
                return Err(
                    "series.json differs from the snapshot that produced series.idx; move series.idx aside to migrate series.json again, or restore the schema 3 marker"
                        .into(),
                );
            }
            if !read_only {
                write_marker(directory, &index)?;
            }
            ready(index, None)
        }
        (Snapshot::Legacy { .. }, false) if read_only => Ok(SeriesStore::from_parts(
            directory,
            Backend::Unmigrated,
            true,
            None,
        )),
        (
            Snapshot::Legacy {
                bytes,
                digest,
                schema,
            },
            false,
        ) => migrate(directory, &bytes, digest, schema, hook),
    }
}

fn migrate(
    directory: &Path,
    bytes: &[u8],
    digest: [u8; 32],
    schema: u64,
    hook: &mut dyn FnMut(Stage) -> Result<()>,
) -> Result<SeriesStore> {
    let records = legacy::records(bytes, schema)?;
    let backup = format!("series.legacy-{}.json", super::hex(&digest[..8]));
    write_backup(directory, &backup, bytes)?;
    hook(Stage::BackupWritten)?;

    let staging_path = directory.join(STAGING);
    let mut resumed = false;
    let mut staging = None;
    if exists(&staging_path)? {
        // Derived data: reuse it only when it provably came from this snapshot.
        if let Ok(mut index) = Index::open(&staging_path, false)
            && check_format(&mut index, false).is_ok()
            && source_of(&mut index).ok().flatten().as_deref() == Some(&digest[..])
        {
            resumed = true;
            staging = Some(index);
        } else {
            discard_staging(directory)?;
        }
    }
    let mut staging = match staging {
        Some(index) => index,
        None => {
            let mut index = Index::create(&staging_path)?;
            initialize(&mut index)?;
            index.commit(migration_changes(Some(&digest), None)?)?;
            index
        }
    };
    hook(Stage::StagingCreated)?;

    let cursor = cursor_of(&mut staging)?;
    for (position, record) in records.iter().enumerate() {
        if cursor.as_ref().is_some_and(|done| record.id <= *done) {
            continue;
        }
        save_record(
            &mut staging,
            record,
            migration_changes(None, Some(&record.id))?,
        )?;
        hook(Stage::Record(position as u64 + 1))?;
    }

    let counts = counts_of(&mut staging)?;
    if counts.series != records.len() as u64
        || counts.monitored != records.iter().filter(|record| record.monitored).count() as u64
    {
        return Err("Series migration verification failed: counters disagree".into());
    }
    for record in &records {
        if load_record(&mut staging, &record.id)?.as_ref() != Some(record) {
            return Err("Series migration verification failed: a record differs".into());
        }
        verify_record(&mut staging, record)?;
    }
    let mut after = None;
    let mut seen = 0_u64;
    loop {
        let page = staging.page(&Query {
            start: header_start(),
            end: header_end(),
            after,
            limit: 200,
            sequence: None,
        })?;
        seen += page.entries.len() as u64;
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    if seen != records.len() as u64 {
        return Err("Series migration verification failed: record count differs".into());
    }
    staging.commit(vec![clear_cursor()])?;
    hook(Stage::Verified)?;
    drop(staging);

    let index_path = directory.join(INDEX);
    fs::rename(&staging_path, &index_path)
        .map_err(|error| format!("Cannot promote the series index: {error}"))?;
    sync_directory(directory)?;
    hook(Stage::Promoted)?;
    let index = Index::open(&index_path, false)?;
    write_marker(directory, &index)?;
    hook(Stage::MarkerWritten)?;
    let report = MigrationReport {
        records: records.len() as u64,
        episodes: records
            .iter()
            .map(|record| record.plan.episodes.len() as u64)
            .sum(),
        resumed,
        backup,
    };
    Ok(SeriesStore::from_parts(
        directory,
        Backend::Ready(Box::new(index)),
        false,
        Some(report),
    ))
}

fn header_start() -> crate::inventory::Key {
    let mut key = [0; 64];
    key[0] = 0x10;
    key
}

fn header_end() -> Option<crate::inventory::Key> {
    let mut key = [0; 64];
    key[0] = 0x11;
    Some(key)
}

/// Creates the index for a store that has never held series, atomically.
pub(super) fn create_fresh(directory: &Path) -> Result<Index> {
    let snapshot = legacy::inspect(&directory.join(SNAPSHOT), false)?;
    if !matches!(snapshot, Snapshot::Missing) {
        return Err("A series snapshot appeared; reopen storage to migrate it".into());
    }
    let staging_path = directory.join(STAGING);
    if exists(&staging_path)? {
        discard_staging(directory)?;
    }
    let mut index = Index::create(&staging_path)?;
    initialize(&mut index)?;
    drop(index);
    let index_path = directory.join(INDEX);
    fs::rename(&staging_path, &index_path)
        .map_err(|error| format!("Cannot publish the series index: {error}"))?;
    sync_directory(directory)?;
    let index = Index::open(&index_path, false)?;
    write_marker(directory, &index)?;
    Ok(index)
}

/// Removes an unfinished staging directory. It holds derived data only: the
/// source snapshot or an empty new index. Links and foreign files are refused.
fn discard_staging(directory: &Path) -> Result<()> {
    let path = directory.join(STAGING);
    reject_symlinks(&path)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Cannot inspect the series staging directory: {error}"))?;
    if !metadata.is_dir() {
        return Err("The series staging path is not a directory".into());
    }
    fs::remove_dir_all(&path)
        .map_err(|error| format!("Cannot discard the series staging directory: {error}"))?;
    sync_directory(directory)
}

fn open_index(path: &Path, read_only: bool) -> Result<Index> {
    let mut index = Index::open(path, read_only)?;
    check_format(&mut index, read_only)?;
    Ok(index)
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Cannot inspect series storage: {error}")),
    }
}

/// Atomically replaces `series.json` with the schema 3 marker.
fn write_marker(directory: &Path, index: &Index) -> Result<()> {
    replace_file(
        directory,
        &directory.join(SNAPSHOT),
        &legacy::marker_bytes(&index.identity()),
    )
}

fn write_backup(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = directory.join(name);
    reject_symlinks(&path)?;
    if exists(&path)? {
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || sha256(&fs::read(&path).map_err(|error| error.to_string())?) != sha256(bytes)
        {
            return Err(format!(
                "{name} exists with different content; refusing to overwrite a preserved snapshot"
            ));
        }
        return Ok(());
    }
    replace_file(directory, &path, bytes)
}

fn replace_file(directory: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    let temporary: PathBuf = directory.join(format!(
        ".series-{}.tmp",
        super::hex(&random_bytes::<16>()?)
    ));
    let write = (|| -> Result<()> {
        let mut file = private_options()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("Cannot create series file: {error}"))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("Cannot persist series file: {error}"))?;
        fs::rename(&temporary, target)
            .map_err(|error| format!("Cannot replace series file: {error}"))?;
        sync_directory(directory)
    })();
    if write.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write
}
