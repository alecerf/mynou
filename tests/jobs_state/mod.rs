//! Persisted job state as bytes, for tests that assert nothing was written,
//! and stores rebuilt with chosen jobs.
#![allow(dead_code)]

use mynou::store::Job;
use std::fs;
use std::path::{Path, PathBuf};

/// Every file of the job index, with its path relative to the index. Equal
/// results mean no job record, entry or history row changed.
pub fn fingerprint(store_dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(
        root: &Path,
        directory: &Path,
        found: &mut Vec<(PathBuf, Vec<u8>)>,
    ) -> mynou::Result<()> {
        let directory = fixture_root(directory).unwrap();
        if !directory.as_path().starts_with(root) {
            return Err("Fixture directory escaped its index".into());
        }
        let entries = fs::read_dir(&directory).unwrap();
        for entry in entries {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                walk(root, &path, found)?;
            } else if path.file_name().is_some_and(|name| name != ".lock") {
                found.push((
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(&path).unwrap(),
                ));
            }
        }
        Ok(())
    }
    let root = fixture_root(&store_dir.join("jobs.idx")).unwrap();
    let mut found = Vec::new();
    walk(&root, &root, &mut found).unwrap();
    found.sort();
    found
}

/// Removes the job index, leaving a store directory without jobs.
pub fn reset(store_dir: &Path) {
    let store_dir = fixture_root(store_dir).unwrap();
    remove_owned(&store_dir, "jobs.idx", true);
}

/// Replaces a closed store by one holding exactly `jobs`.
pub fn rebuild(store_dir: &Path, jobs: &[Job]) {
    mynou::store::Store::rebuild_for_tests(&fixture_root(store_dir).unwrap(), jobs).unwrap();
}

fn remove_owned(root: &Path, name: &str, directory: bool) {
    remove_checked(root, name, directory).unwrap();
}

fn remove_checked(root: &Path, name: &str, directory: bool) -> mynou::Result<()> {
    let requested = root.join(name);
    let path = match requested.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    if !path.as_path().starts_with(root) {
        return Err("Fixture removal escaped its root".into());
    }
    if path == root || path != requested {
        return Err("Fixture removal cannot follow links or remove its root".into());
    }
    let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() {
        return Err("Fixture removal cannot follow links".into());
    }
    if directory {
        fs::remove_dir_all(&path).map_err(|error| error.to_string())?;
    } else {
        fs::remove_file(&path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn fixture_root(path: &Path) -> mynou::Result<PathBuf> {
    let temporary = std::env::temp_dir()
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let normalized = path.canonicalize().map_err(|error| error.to_string())?;
    if !normalized.as_path().starts_with(&temporary) {
        return Err("Fixture path escaped temporary storage".into());
    }
    if normalized == temporary {
        return Err("Fixture path must be below temporary storage".into());
    }
    Ok(normalized)
}
