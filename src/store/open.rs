//! Opening the job index (`jobs.idx`), or creating it empty in its format.
use super::{
    Backend, Change, Index, Result, Store,
    layout::{self, FORMAT},
    ops::{Counts, Db},
};
use crate::store::{reject_symlinks, sync_directory};
use std::{fs, fs::File, path::Path};

const INDEX: &str = "jobs.idx";
/// Where a new index is built before it takes its name, so a crash never
/// leaves a half-created `jobs.idx`.
const STAGING: &str = "jobs.idx.creating";

pub(super) fn open(directory: &Path, lock: File, read_only: bool) -> Result<Store> {
    let path = directory.join(INDEX);
    reject_symlinks(&path)?;
    let indexed = exists(&path)?;
    if !indexed {
        crate::store::refuse_older_format(
            directory,
            &["snapshot.bin", "journal.bin"],
            "Job storage",
        )?;
    }
    let index = if indexed {
        let mut index = Index::open(&path, read_only)?;
        check_format(&mut index)?;
        index
    } else if read_only {
        return Ok(Store::from_parts(lock, Backend::Absent, true));
    } else {
        create(directory)?
    };
    Ok(Store::from_parts(
        lock,
        Backend::Ready(Box::new(index)),
        read_only,
    ))
}

fn check_format(index: &mut Index) -> Result<()> {
    match index.get(&layout::FORMAT_KEY)? {
        Some(value) if value == FORMAT => {
            super::monitoring::verify_counts(index)?;
            Db(index).counts().map(|_| ())
        }
        Some(_) => Err("Unsupported job index format".into()),
        None => Err("Job index is missing its format marker".into()),
    }
}

/// Creates the index of a store that has never held jobs, atomically.
fn create(directory: &Path) -> Result<Index> {
    let staging = directory.join(STAGING);
    reject_symlinks(&staging)?;
    if exists(&staging)? {
        // Only an interrupted creation leaves it, and it held no job.
        fs::remove_dir_all(&staging).map_err(|_| "Cannot remove an unfinished job index")?;
    }
    let mut index = Index::create(&staging)?;
    index.commit(vec![
        Change::PutInline(layout::FORMAT_KEY, FORMAT.to_vec()),
        Change::PutInline(layout::COUNTS_KEY, Counts::default().encode()),
        Change::PutInline(layout::LIBRARY_COUNT_KEY, 0_u64.to_le_bytes().to_vec()),
        Change::Put(
            layout::MONITOR_COUNTS_KEY,
            super::monitoring::encode([0; 5]),
        ),
    ])?;
    drop(index);
    let path = directory.join(INDEX);
    fs::rename(&staging, &path)
        .map_err(|error| crate::store::storage_io("Job storage", "write", &error))?;
    sync_directory(directory).map_err(|error| crate::store::storage_sync("Job storage", &error))?;
    Index::open(&path, false)
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Cannot inspect job storage".to_owned()),
    }
}
