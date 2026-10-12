//! Persisted job state as bytes, and the files of earlier versions, for tests
//! that assert nothing was written or that exercise migration.
#![allow(dead_code)]

use mynou::crypto::sha256;
use mynou::json::{self, Value};
use mynou::store::{Event, Job};
use std::fs;
use std::path::{Path, PathBuf};

/// Every file of the job index, with its path relative to the index. Equal
/// results mean no job record, entry or history row changed.
pub fn fingerprint(store_dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, directory: &Path, found: &mut Vec<(PathBuf, Vec<u8>)>) {
        let directory = directory.canonicalize().unwrap();
        assert!(directory.starts_with(root));
        let entries = fs::read_dir(&directory).unwrap();
        for entry in entries {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                walk(root, &path, found);
            } else if path.file_name().is_some_and(|name| name != ".lock") {
                found.push((
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(&path).unwrap(),
                ));
            }
        }
    }
    let root = store_dir.join("jobs.idx").canonicalize().unwrap();
    let mut found = Vec::new();
    walk(&root, &root, &mut found);
    found.sort();
    found
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The magic numbers of the journal and snapshot formats a set of jobs needs.
pub fn magics(jobs: &[Job]) -> (&'static [u8; 8], &'static [u8; 8]) {
    let group = |job: &Job| {
        job.shared_upgrade.is_some() || (job.shared_file.is_some() && job.release.is_some())
    };
    if jobs.iter().any(|j| j.irc_origin.is_some()) {
        (b"MYNOUJ05", b"MYNOUS05")
    } else if jobs.iter().any(|j| j.requester.is_some()) {
        (b"MYNOUJ04", b"MYNOUS04")
    } else if jobs.iter().any(group) {
        (b"MYNOUJ03", b"MYNOUS03")
    } else if jobs.iter().any(|j| j.shared_file.is_some()) {
        (b"MYNOUJ02", b"MYNOUS02")
    } else {
        (b"MYNOUJ01", b"MYNOUS01")
    }
}

pub fn snapshot(
    jobs: &[Job],
    events: &[Event],
    sequence: u64,
    chain: &[u8; 32],
    magic: &[u8; 8],
) -> Vec<u8> {
    let mut value = Value::object();
    value.insert("sequence", Value::String(sequence.to_string()));
    value.insert("chain", hex(chain));
    value.insert(
        "jobs",
        Value::Array(jobs.iter().map(Job::to_json).collect()),
    );
    value.insert(
        "events",
        Value::Array(events.iter().map(Event::to_json).collect()),
    );
    let payload = json::stringify(&value).into_bytes();
    let mut bytes = magic.to_vec();
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&payload);
    let digest = sha256(&bytes);
    bytes.extend_from_slice(&digest);
    bytes
}

/// One journal frame and its digest, which is the chain value of the next.
pub fn frame(
    jobs: &[Job],
    event: &Event,
    sequence: u64,
    chain: &[u8; 32],
    magic: &[u8; 8],
    action: Option<&str>,
) -> (Vec<u8>, [u8; 32]) {
    let mut value = Value::object();
    value.insert("job", jobs[0].to_json());
    value.insert("event", event.to_json());
    if let Some(action) = action {
        value.insert("group_action", action);
    }
    if jobs.len() > 1 {
        value.insert(
            if action.is_some() {
                "group_jobs"
            } else {
                "shared_owners"
            },
            Value::Array(jobs.iter().skip(1).map(Job::to_json).collect()),
        );
    }
    let payload = json::stringify(&value).into_bytes();
    let mut frame = magic.to_vec();
    frame.extend_from_slice(&sequence.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(chain);
    frame.extend_from_slice(&sha256(&frame));
    frame.extend_from_slice(&payload);
    let digest = sha256(&frame);
    frame.extend_from_slice(&digest);
    (frame, digest)
}

/// Removes every job file, leaving an empty store directory.
pub fn reset(store_dir: &Path) {
    let store_dir = store_dir.canonicalize().unwrap();
    remove_owned(&store_dir, "jobs.idx", true);
    remove_owned(&store_dir, "snapshot.bin", false);
    remove_owned(&store_dir, "journal.bin", false);
}

/// Replaces a migrated store's files by the legacy snapshot of the same jobs,
/// as an earlier version would have left them. The store must be closed.
pub fn downgrade(store_dir: &Path, jobs: &[Job]) {
    downgrade_as(store_dir, jobs, None);
}

/// As `downgrade`, with an explicit (possibly too old) snapshot format.
pub fn downgrade_as(store_dir: &Path, jobs: &[Job], magic: Option<&[u8; 8]>) {
    let store_dir = store_dir.canonicalize().unwrap();
    remove_owned(&store_dir, "jobs.idx", true);
    let snapshot_magic = magic.unwrap_or_else(|| magics(jobs).1);
    let events: Vec<Event> = jobs
        .iter()
        .enumerate()
        .map(|(index, job)| Event {
            id: index as u64 + 1,
            job_id: job.id.clone(),
            state: job.state.clone(),
            message: "legacy".into(),
            at: job.updated_at,
        })
        .collect();
    write_private(
        &store_dir.join("snapshot.bin"),
        &snapshot(jobs, &events, events.len() as u64, &[0; 32], snapshot_magic),
    );
    remove_owned(&store_dir, "journal.bin", false);
}

/// Writes a file as the earlier store did: readable by its owner only.
pub fn write_private(path: &Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    let root = path.parent().unwrap().canonicalize().unwrap();
    let path = root.join(path.file_name().unwrap());
    assert!(path.starts_with(&root));
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        assert!(metadata.is_file() && !metadata.file_type().is_symlink());
    }
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn remove_owned(root: &Path, name: &str, directory: bool) {
    let path = root.join(name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => panic!("Cannot inspect fixture: {error}"),
    };
    assert!(!metadata.file_type().is_symlink());
    let path = path.canonicalize().unwrap();
    assert!(path.starts_with(root) && path != root);
    if directory {
        fs::remove_dir_all(&path).unwrap();
    } else {
        fs::remove_file(&path).unwrap();
    }
}
