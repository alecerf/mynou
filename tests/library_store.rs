//! Durable library identity, upgrade provenance and monitoring transactions.
use mynou::crypto::sha256;
use mynou::json::{self, Value};
use mynou::store::{self, Job, RecordedRelease, Request, Store};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mynou-library-store-{}-{}-{}",
            std::process::id(),
            store::now(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request(title: &str, url: Option<&str>) -> Request {
    Request {
        kind: "movie".into(),
        title: title.into(),
        year: 2026,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: url.map(str::to_owned),
        tmdb_id: None,
    }
}

fn release(title: &str) -> RecordedRelease {
    RecordedRelease {
        title: title.into(),
        profile: "hd".into(),
    }
}

fn ready(store: &mut Store, title: &str, url: Option<&str>) -> Job {
    let mut job = store.submit(request(title, url)).unwrap();
    job.state = "ready".into();
    job.progress = 1.0;
    job.imports = vec![format!("/library/{}.mkv", job.id)];
    // Acquisitions record their selected release before promotion. Independent
    // imports need not win root precedence to retain that provenance.
    job.release = Some(release("Example.2026.720p.WEB-DL.ENG"));
    store.update(job.clone()).unwrap();
    store.get(&job.id).unwrap()
}

fn complete(store: &mut Store, mut job: Job) -> Job {
    job.state = "ready".into();
    job.progress = 1.0;
    job.imports = vec![format!("/library/{}.mkv", job.id)];
    store.update(job.clone()).unwrap();
    store.get(&job.id).unwrap()
}

fn edit_snapshot(path: &Path, edit: impl FnOnce(&mut Value)) {
    let bytes = fs::read(path).unwrap();
    let mut value =
        json::parse(std::str::from_utf8(&bytes[16..bytes.len() - 32]).unwrap()).unwrap();
    edit(&mut value);
    let payload = json::stringify(&value);
    let mut replacement = b"MYNOUS01".to_vec();
    replacement.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    replacement.extend_from_slice(payload.as_bytes());
    replacement.extend_from_slice(&sha256(&replacement));
    fs::write(path, replacement).unwrap();
}

fn edit_first_journal(path: &Path, edit: impl FnOnce(&mut Value)) {
    let bytes = fs::read(path).unwrap();
    let length = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    assert_eq!(
        bytes.len(),
        length + 116,
        "fixture has exactly one transaction"
    );
    let mut value = json::parse(std::str::from_utf8(&bytes[84..84 + length]).unwrap()).unwrap();
    edit(&mut value);
    let payload = json::stringify(&value);
    let mut replacement = bytes[..52].to_vec();
    replacement[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    replacement.extend_from_slice(&sha256(&replacement));
    replacement.extend_from_slice(payload.as_bytes());
    replacement.extend_from_slice(&sha256(&replacement));
    fs::write(path, replacement).unwrap();
}

fn edit_last_journal(path: &Path, edit: impl FnOnce(&mut Value)) {
    let bytes = fs::read(path).unwrap();
    let mut offset = 0;
    loop {
        let length =
            u32::from_le_bytes(bytes[offset + 16..offset + 20].try_into().unwrap()) as usize;
        if offset + length + 116 == bytes.len() {
            break;
        }
        offset += length + 116;
    }
    let mut value =
        json::parse(std::str::from_utf8(&bytes[offset + 84..bytes.len() - 32]).unwrap()).unwrap();
    edit(&mut value);
    let payload = json::stringify(&value);
    let mut frame = bytes[offset..offset + 52].to_vec();
    frame[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&sha256(&frame));
    frame.extend_from_slice(payload.as_bytes());
    frame.extend_from_slice(&sha256(&frame));
    let mut replacement = bytes[..offset].to_vec();
    replacement.extend_from_slice(&frame);
    fs::write(path, replacement).unwrap();
}

fn remove_library_fields(job: &mut Value) {
    let Value::Object(map) = job else {
        panic!("job object");
    };
    for key in [
        "release",
        "upgrade_parent",
        "monitored",
        "monitor_checked_at",
    ] {
        map.remove(key);
    }
}

#[test]
fn logical_identity_ignores_explicit_sources_without_changing_request_keys() {
    let original = request("  EXAMPLE   movie ", None);
    let mut acquired = request("example movie", Some("https://example.invalid/a.torrent"));
    assert_eq!(original.media_key(), acquired.media_key());
    assert_ne!(original.canonical_key(), acquired.canonical_key());
    acquired.source_url = None;
    acquired.source_path = Some("/downloads/a.mkv".into());
    assert_eq!(original.media_key(), acquired.media_key());
    acquired.tmdb_id = Some(42);
    assert_ne!(original.media_key(), acquired.media_key());
    let mut known = original.clone();
    known.tmdb_id = Some(42);
    known.title = "Another localized name".into();
    known.year = 1990;
    assert_eq!(known.media_key(), acquired.media_key());
    known.kind = "episode".into();
    assert_ne!(known.media_key(), acquired.media_key());
    acquired.kind = "episode".into();
    known.season = 1;
    assert_ne!(known.media_key(), acquired.media_key());
    acquired.season = 1;
    known.episode = 2;
    assert_ne!(known.media_key(), acquired.media_key());
}

#[test]
fn bounded_release_metadata_and_legacy_json_defaults() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let original = store.submit(request("Example", None)).unwrap();
    assert_eq!(original.release, None);
    assert_eq!(original.upgrade_parent, None);
    assert!(original.monitored);
    assert_eq!(original.monitor_checked_at, 0);
    let mut value = original.to_json();
    remove_library_fields(&mut value);
    assert_eq!(Job::from_json(&value).unwrap(), original);
    let valid = release(&"a".repeat(2_048));
    assert_eq!(RecordedRelease::from_json(&valid.to_json()).unwrap(), valid);
    for invalid in [
        release(""),
        release("   "),
        release(&"a".repeat(2_049)),
        release("title\0suffix"),
        RecordedRelease {
            title: "Example".into(),
            profile: "x".repeat(65),
        },
        RecordedRelease {
            title: "Example".into(),
            profile: "café".into(),
        },
    ] {
        assert!(RecordedRelease::from_json(&invalid.to_json()).is_err());
    }
    value.insert("monitored", "true");
    assert!(Job::from_json(&value).is_err());
}

#[test]
fn missing_import_baseline_can_be_recorded_once_and_survives_restart() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut job = store.submit(request("Manual import", None)).unwrap();
    let baseline = release("Manual.import.2026.720p.WEB-DL.ENG");
    assert!(store.set_baseline(&job.id, baseline.clone()).is_err());
    job.state = "ready".into();
    job.imports = vec!["/library/manual.mkv".into()];
    store.update(job.clone()).unwrap();
    let recorded = store.set_baseline(&job.id, baseline.clone()).unwrap();
    assert_eq!(recorded.release, Some(baseline));
    assert!(
        store
            .set_baseline(&job.id, release("Manual.import.1080p"))
            .is_err()
    );
    drop(store);
    assert_eq!(
        Store::open(&directory.0).unwrap().get(&job.id).unwrap(),
        recorded
    );
}

#[test]
fn legacy_journal_and_snapshot_load_without_library_fields() {
    for snapshot in [false, true] {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let original = store.submit(request("Legacy", None)).unwrap();
        if snapshot {
            store.compact().unwrap();
        }
        drop(store);
        if snapshot {
            edit_snapshot(&directory.0.join("snapshot.bin"), |value| {
                let Value::Array(jobs) = value.get_mut("jobs").unwrap() else {
                    panic!("jobs");
                };
                remove_library_fields(&mut jobs[0]);
            });
        } else {
            edit_first_journal(&directory.0.join("journal.bin"), |value| {
                remove_library_fields(value.get_mut("job").unwrap());
            });
        }
        let reopened = Store::open(&directory.0).unwrap();
        assert_eq!(reopened.get(&original.id).unwrap(), original);
    }
}

#[test]
fn legacy_ready_cancellation_replays_without_weakening_current_cancellation_rules() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut job = store
        .submit(request("Legacy ready cancellation", None))
        .unwrap();
    job.state = "ready".into();
    job.imports = vec!["/library/legacy.mkv".into()];
    store.update(job.clone()).unwrap();
    assert!(store.cancel(&job.id).is_err());
    store.record_monitor_check(&job.id, 0).unwrap();
    drop(store);
    let journal = directory.0.join("journal.bin");
    let original = fs::read(&journal).unwrap();
    let mut offset = 0;
    let mut chain = [0_u8; 32];
    let mut rewritten = Vec::new();
    while offset < original.len() {
        let length =
            u32::from_le_bytes(original[offset + 16..offset + 20].try_into().unwrap()) as usize;
        let end = offset + length + 116;
        let mut value =
            json::parse(std::str::from_utf8(&original[offset + 84..offset + 84 + length]).unwrap())
                .unwrap();
        remove_library_fields(value.get_mut("job").unwrap());
        if end == original.len() {
            value.get_mut("job").unwrap().insert("state", "cancelled");
            value.get_mut("event").unwrap().insert("state", "cancelled");
        }
        let payload = json::stringify(&value);
        let mut frame = b"MYNOUJ01".to_vec();
        frame.extend_from_slice(&original[offset + 8..offset + 16]);
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&chain);
        frame.extend_from_slice(&sha256(&frame));
        frame.extend_from_slice(payload.as_bytes());
        chain = sha256(&frame);
        frame.extend_from_slice(&chain);
        rewritten.extend_from_slice(&frame);
        offset = end;
    }
    fs::write(journal, rewritten).unwrap();
    let reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.get(&job.id).unwrap().state, "cancelled");
    assert!(reopened.library_jobs().is_empty());
}

#[test]
fn pending_upgrade_preserves_parent_and_ready_child_survives_compaction_and_restart() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = ready(&mut store, "Example", None);
    let child = store
        .submit_upgrade(
            &parent.id,
            request("example", Some("https://example.invalid/1080.torrent")),
            release("Example.2026.1080p.WEB-DL.ENG"),
        )
        .unwrap();
    assert_eq!(child.upgrade_parent.as_deref(), Some(parent.id.as_str()));
    assert_eq!(store.library_jobs(), vec![parent.clone()]);
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(store.library_jobs(), vec![parent.clone()]);
    assert_eq!(store.get(&child.id).unwrap(), child);
    let child = complete(&mut store, child);
    assert_eq!(store.library_jobs(), vec![child.clone()]);
    assert_eq!(store.get(&parent.id).unwrap(), parent);
    store.record_monitor_check(&child.id, 500).unwrap();
    store.record_monitor_check(&child.id, 400).unwrap();
    let child = store.set_monitored(&child.id, false).unwrap();
    assert_eq!(child.monitor_checked_at, 500);
    store.compact().unwrap();
    drop(store);
    let reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.library_jobs(), vec![child]);
    assert_eq!(reopened.get(&parent.id).unwrap(), parent);
}

#[test]
fn upgrade_source_deduplication_never_requeues_failed_or_cancelled_children() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = ready(&mut store, "Example", None);
    let request = request("Example", Some("https://example.invalid/upgraded.torrent"));
    let candidate = release("Example.2026.1080p.WEB-DL.ENG");
    let mut child = store
        .submit_upgrade(&parent.id, request.clone(), candidate.clone())
        .unwrap();
    child.acquisition_url = request.source_url.clone();
    child.download_id = Some("partial-upgrade".into());
    child.files = vec!["/downloads/partial.mkv".into()];
    child.progress = 0.5;
    store.update(child.clone()).unwrap();
    let cancelled = store.cancel(&child.id).unwrap();
    assert_eq!(
        store
            .submit_upgrade(&parent.id, request.clone(), candidate.clone())
            .unwrap(),
        cancelled
    );
    let retried = store.retry(&child.id).unwrap();
    assert_eq!(retried.release, Some(candidate.clone()));
    assert_eq!(retried.request.source_url, request.source_url);
    assert_eq!(retried.acquisition_url, child.acquisition_url);
    assert_eq!(retried.download_id, child.download_id);
    assert_eq!(retried.files, child.files);
    assert_eq!(retried.progress, 0.5);
    let mut failed = retried;
    failed.state = "failed".into();
    failed.last_error = Some("test acquisition failure".into());
    failed.next_attempt_at = 0;
    store.update(failed).unwrap();
    let failed = store.get(&child.id).unwrap();
    assert_eq!(
        store
            .submit_upgrade(&parent.id, request, candidate)
            .unwrap(),
        failed
    );
    assert_eq!(store.library_jobs(), vec![parent]);
}

#[test]
fn scheduled_failed_upgrade_blocks_competitors_and_retry_obeys_current_monitoring() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = ready(&mut store, "Example", None);
    let mut child = store
        .submit_upgrade(
            &parent.id,
            request("Example", Some("https://example.invalid/first.torrent")),
            release("Example.2026.1080p"),
        )
        .unwrap();
    child.state = "failed".into();
    child.next_attempt_at = store::now() + 60;
    store.update(child.clone()).unwrap();
    assert!(
        store
            .submit_upgrade(
                &parent.id,
                request("Example", Some("https://example.invalid/second.torrent")),
                release("Example.2026.2160p")
            )
            .unwrap_err()
            .contains("pending")
    );
    store.set_monitored(&parent.id, false).unwrap();
    assert!(store.retry(&child.id).unwrap_err().contains("monitored"));
    assert!(store.claim(store::now() + 120, 600).unwrap().is_none());
    store.set_monitored(&parent.id, true).unwrap();
    store.cancel(&child.id).unwrap();
    let replacement = store
        .submit_upgrade(
            &parent.id,
            request("Example", Some("https://example.invalid/second.torrent")),
            release("Example.2026.2160p"),
        )
        .unwrap();
    assert!(store.retry(&child.id).unwrap_err().contains("pending"));
    let replacement = complete(&mut store, replacement);
    assert!(store.retry(&child.id).unwrap_err().contains("obsolete"));
    assert!(store.set_monitored(&parent.id, true).is_err());
    assert!(
        store
            .set_baseline(&parent.id, release("Alternative"))
            .is_err()
    );
    assert_eq!(store.library_jobs(), vec![replacement]);
}

#[test]
fn claimed_upgrade_inherits_monitoring_disabled_during_acquisition() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = ready(&mut store, "Example", None);
    let child = store
        .submit_upgrade(
            &parent.id,
            request("Example", Some("https://example.invalid/in-flight.torrent")),
            release("Example.1080p"),
        )
        .unwrap();
    let mut claimed = store.claim(store::now(), 600).unwrap().unwrap();
    assert_eq!(claimed.id, child.id);
    assert!(claimed.monitored);
    store.set_monitored(&parent.id, false).unwrap();
    claimed.state = "ready".into();
    claimed.imports = vec!["/library/upgraded.mkv".into()];
    claimed.progress = 1.0;
    // This is the stale worker copy from before monitoring was disabled.
    assert!(claimed.monitored);
    store.update(claimed).unwrap();
    let promoted = store.get(&child.id).unwrap();
    assert!(!promoted.monitored);
    assert_eq!(store.library_jobs(), vec![promoted.clone()]);
    drop(store);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.library_jobs(), vec![promoted.clone()]);
    reopened.compact().unwrap();
    drop(reopened);
    assert_eq!(
        Store::open(&directory.0).unwrap().library_jobs(),
        vec![promoted]
    );
}

#[test]
fn unrelated_root_cannot_obsolete_a_pending_upgrade() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = ready(&mut store, "Example", None);
    let child = store
        .submit_upgrade(
            &parent.id,
            request("Example", Some("https://example.invalid/pending.torrent")),
            release("Example.1080p"),
        )
        .unwrap();
    let mut unrelated = store
        .submit(request(
            "Example",
            Some("https://example.invalid/unrelated.torrent"),
        ))
        .unwrap();
    unrelated.state = "ready".into();
    unrelated.imports = vec!["/library/unrelated.mkv".into()];
    assert!(
        store
            .check_ready_promotion(&unrelated)
            .unwrap_err()
            .contains("cancel the pending upgrade")
    );
    let error = store.update(unrelated.clone()).unwrap_err();
    assert!(error.contains("cancel the pending upgrade"));
    assert_eq!(store.library_jobs(), vec![parent.clone()]);
    assert_eq!(store.get(&unrelated.id).unwrap().state, "queued");
    drop(store);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.library_jobs(), vec![parent]);
    reopened.cancel(&child.id).unwrap();
    reopened.check_ready_promotion(&unrelated).unwrap();
    reopened.update(unrelated).unwrap();
    assert!(reopened.claim(store::now(), 600).unwrap().is_none());
}

#[test]
fn read_only_store_loads_verified_state_without_file_or_permission_changes() {
    use std::os::unix::fs::PermissionsExt;
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = ready(&mut store, "Example", None);
    store.compact().unwrap();
    let pending = store.submit(request("Queued", None)).unwrap();
    drop(store);
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o750)).unwrap();
    let filenames = [".lock", "journal.bin", "snapshot.bin"];
    let before: Vec<_> = filenames
        .iter()
        .map(|name| {
            let path = directory.0.join(name);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            (
                fs::read(&path).unwrap(),
                fs::metadata(&path).unwrap().modified().unwrap(),
            )
        })
        .collect();
    let mut read_only = Store::open_read_only(&directory.0).unwrap();
    assert_eq!(read_only.library_jobs(), vec![parent]);
    assert_eq!(read_only.get(&pending.id).unwrap(), pending);
    assert!(
        Store::open_read_only(&directory.0).is_err(),
        "read-only loading still excludes concurrent writers"
    );
    assert!(
        read_only
            .submit(request("Forbidden write", None))
            .unwrap_err()
            .contains("read-only")
    );
    assert!(read_only.compact().unwrap_err().contains("read-only"));
    drop(read_only);
    assert_eq!(
        fs::metadata(&directory.0).unwrap().permissions().mode() & 0o777,
        0o750
    );
    for (index, name) in filenames.iter().enumerate() {
        let path = directory.0.join(name);
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before[index].0);
        assert_eq!(metadata.modified().unwrap(), before[index].1);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
    }
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 3);
    let absent = directory.0.join("absent");
    assert!(Store::open_read_only(&absent).is_err());
    assert!(!absent.exists());
}

#[test]
fn read_only_store_refuses_interrupted_tail_without_truncating_it() {
    use std::io::Write;
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let job = store.submit(request("Persisted", None)).unwrap();
    drop(store);
    let journal = directory.0.join("journal.bin");
    let complete = fs::read(&journal).unwrap();
    let mut file = fs::OpenOptions::new().append(true).open(&journal).unwrap();
    file.write_all(b"MYNOU").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let interrupted = fs::read(&journal).unwrap();
    let error = Store::open_read_only(&directory.0).err().unwrap();
    assert!(error.contains("recovery required"));
    assert_eq!(fs::read(&journal).unwrap(), interrupted);
    let repaired = Store::open(&directory.0).unwrap();
    assert_eq!(repaired.get(&job.id).unwrap(), job);
    assert_eq!(fs::read(journal).unwrap(), complete);
}

#[test]
fn upgrade_requests_reject_unrelated_url_collisions_and_unusable_parents() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = ready(
        &mut store,
        "Example",
        Some("https://example.invalid/original.torrent"),
    );
    let collision = "https://example.invalid/shared.torrent";
    store
        .submit(request("Unrelated movie", Some(collision)))
        .unwrap();
    assert!(
        store
            .submit_upgrade(
                &parent.id,
                request("Example", Some(collision)),
                release("Example.1080p")
            )
            .unwrap_err()
            .contains("unrelated")
    );
    assert!(
        store
            .submit_upgrade(
                &parent.id,
                request("Example", parent.request.source_url.as_deref()),
                release("Example.1080p")
            )
            .unwrap_err()
            .contains("different")
    );
    assert!(
        store
            .submit_upgrade(
                &parent.id,
                request("Other", Some("https://example.invalid/new.torrent")),
                release("Other.1080p")
            )
            .is_err()
    );
    assert!(
        store
            .submit_upgrade(
                &parent.id,
                request("Example", None),
                release("Example.1080p")
            )
            .is_err()
    );
    let mut local = request("Example", None);
    local.source_path = Some("/downloads/example.mkv".into());
    assert!(
        store
            .submit_upgrade(&parent.id, local, release("Example.1080p"))
            .is_err()
    );
    store.set_monitored(&parent.id, false).unwrap();
    assert!(
        store
            .submit_upgrade(
                &parent.id,
                request("Example", Some("https://example.invalid/new.torrent")),
                release("Example.1080p")
            )
            .is_err()
    );
    let pending = store.submit(request("Pending", None)).unwrap();
    assert!(
        store
            .set_baseline(&pending.id, release("Pending.720p"))
            .is_err()
    );
    assert!(store.set_monitored(&pending.id, false).is_err());
    assert!(store.record_monitor_check(&pending.id, 100).is_err());
}

#[test]
fn upgrade_cannot_repeat_an_automatically_selected_parent_acquisition() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut parent = store.submit(request("Example", None)).unwrap();
    parent.acquisition_url = Some("https://example.invalid/selected.torrent".into());
    parent.release = Some(release("Example.720p"));
    parent.state = "ready".into();
    parent.imports = vec!["/library/example.mkv".into()];
    store.update(parent.clone()).unwrap();
    assert!(
        store
            .submit_upgrade(
                &parent.id,
                request("Example", parent.acquisition_url.as_deref()),
                release("Example.1080p")
            )
            .unwrap_err()
            .contains("different")
    );
    let mut child = store
        .submit_upgrade(
            &parent.id,
            request("Example", Some("https://example.invalid/new.torrent")),
            release("Example.1080p"),
        )
        .unwrap();
    child.acquisition_url = parent.acquisition_url;
    assert!(store.update(child).unwrap_err().contains("recorded source"));
}

#[test]
fn worker_cannot_rewrite_lineage_baseline_or_ready_import_provenance() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = ready(&mut store, "Example", None);
    let child = store
        .submit_upgrade(
            &parent.id,
            request("Example", Some("https://example.invalid/new.torrent")),
            release("Example.1080p"),
        )
        .unwrap();
    let mut changed = child.clone();
    changed.upgrade_parent = None;
    assert!(store.update(changed).is_err());
    let mut changed = child.clone();
    changed.release = Some(release("Unrelated.2160p"));
    assert!(store.update(changed).is_err());
    let mut changed = child.clone();
    changed.monitored = false;
    assert!(store.update(changed).is_err());
    let mut changed = child;
    changed.monitor_checked_at = 100;
    assert!(store.update(changed).is_err());
    let mut changed = parent;
    changed.imports.clear();
    assert!(store.update(changed).is_err());
}

#[test]
fn automatic_retry_atomically_forgets_failed_search_provenance() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut failed = store.submit(request("Automatic", None)).unwrap();
    failed.state = "failed".into();
    failed.release = Some(release("Automatic.720p"));
    failed.acquisition_url = Some("https://example.invalid/old.torrent".into());
    failed.download_id = Some("old-download".into());
    failed.files = vec!["/downloads/old.mkv".into()];
    failed.progress = 0.5;
    store.update(failed.clone()).unwrap();
    let retried = store.retry(&failed.id).unwrap();
    assert_eq!(retried.release, None);
    assert_eq!(retried.acquisition_url, None);
    assert_eq!(retried.download_id, None);
    assert!(retried.files.is_empty());
    assert_eq!(retried.progress, 0.0);
    drop(store);
    let reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.get(&retried.id).unwrap(), retried);
}

#[test]
fn unrelated_ready_roots_use_creation_order_even_after_monitor_timestamp_changes() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let first = ready(
        &mut store,
        "Example",
        Some("https://example.invalid/first.torrent"),
    );
    let second = ready(
        &mut store,
        "Example",
        Some("https://example.invalid/second.torrent"),
    );
    store.compact().unwrap();
    drop(store);
    edit_snapshot(&directory.0.join("snapshot.bin"), |value| {
        let Value::Array(jobs) = value.get_mut("jobs").unwrap() else {
            panic!("jobs");
        };
        for job in jobs {
            if job.get("id").and_then(Value::as_str) == Some(first.id.as_str()) {
                job.insert("created_at", "100");
                job.insert("updated_at", "999999");
                job.insert("monitor_checked_at", "999999");
            } else if job.get("id").and_then(Value::as_str) == Some(second.id.as_str()) {
                job.insert("created_at", "200");
                job.insert("updated_at", "200");
            }
        }
    });
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(store.library_jobs()[0].id, second.id);
    store.record_monitor_check(&second.id, u64::MAX).unwrap();
    assert_eq!(store.library_jobs()[0].id, second.id);
    assert!(store.record_monitor_check(&first.id, u64::MAX).is_err());
}

#[test]
fn unrelated_root_precedence_does_not_change_when_its_ready_child_has_a_lower_id() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let first = ready(
        &mut store,
        "Example",
        Some("https://example.invalid/first.torrent"),
    );
    let second = ready(
        &mut store,
        "Example",
        Some("https://example.invalid/second.torrent"),
    );
    let parent = store.library_jobs().remove(0);
    let other = if parent.id == first.id { second } else { first };
    let child = store
        .submit_upgrade(
            &parent.id,
            request("Example", Some("https://example.invalid/child.torrent")),
            release("Example.1080p"),
        )
        .unwrap();
    let child = complete(&mut store, child);
    store.compact().unwrap();
    drop(store);
    let parent_id = "ffffffffffffffffffffffffffffffff";
    let other_id = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let child_id = "00000000000000000000000000000001";
    let mapped_id = |id: &str| -> String {
        if id == parent.id {
            parent_id.into()
        } else if id == other.id {
            other_id.into()
        } else if id == child.id {
            child_id.into()
        } else {
            panic!("unexpected fixture identity");
        }
    };
    edit_snapshot(&directory.0.join("snapshot.bin"), |value| {
        let Value::Array(jobs) = value.get_mut("jobs").unwrap() else {
            panic!("jobs");
        };
        for job in jobs {
            let id = job.get("id").and_then(Value::as_str).unwrap().to_owned();
            job.insert("id", mapped_id(&id));
            job.insert("created_at", "100");
            if let Some(parent) = job.get("upgrade_parent").and_then(Value::as_str) {
                let parent = mapped_id(parent);
                job.insert("upgrade_parent", parent);
            }
        }
        let Value::Array(events) = value.get_mut("events").unwrap() else {
            panic!("events");
        };
        for event in events {
            let id = event
                .get("job_id")
                .and_then(Value::as_str)
                .unwrap()
                .to_owned();
            event.insert("job_id", mapped_id(&id));
        }
    });
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.library_jobs()[0].id, child_id);
    assert!(reopened.set_monitored(other_id, false).is_err());
    let grandchild = reopened
        .submit_upgrade(
            child_id,
            request(
                "Example",
                Some("https://example.invalid/grandchild.torrent"),
            ),
            release("Example.2160p"),
        )
        .unwrap();
    let grandchild = complete(&mut reopened, grandchild);
    assert_eq!(reopened.library_jobs(), vec![grandchild.clone()]);
    reopened.compact().unwrap();
    drop(reopened);
    assert_eq!(
        Store::open(&directory.0).unwrap().library_jobs(),
        vec![grandchild]
    );
}

#[test]
fn verified_snapshot_rejects_missing_parent_cross_media_and_cycles() {
    for damage in ["missing", "media", "cycle"] {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let parent = ready(
            &mut store,
            "Example",
            Some("https://example.invalid/parent.torrent"),
        );
        let child = store
            .submit_upgrade(
                &parent.id,
                request("Example", Some("https://example.invalid/child.torrent")),
                release("Example.1080p"),
            )
            .unwrap();
        let child = complete(&mut store, child);
        store.compact().unwrap();
        drop(store);
        edit_snapshot(&directory.0.join("snapshot.bin"), |value| {
            let Value::Array(jobs) = value.get_mut("jobs").unwrap() else {
                panic!("jobs");
            };
            for job in jobs {
                let id = job.get("id").and_then(Value::as_str).unwrap().to_owned();
                if id == child.id && damage == "missing" {
                    job.insert("upgrade_parent", "00000000000000000000000000000000");
                } else if id == child.id && damage == "media" {
                    job.get_mut("request")
                        .unwrap()
                        .insert("title", "Unrelated media");
                } else if id == parent.id && damage == "cycle" {
                    job.insert("upgrade_parent", child.id.clone());
                }
            }
        });
        assert!(
            Store::open(&directory.0).is_err(),
            "verified {damage} snapshot must be rejected"
        );
    }
}

#[test]
fn verified_journal_rejects_fabricated_upgrade_parent() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    store
        .submit(request(
            "Example",
            Some("https://example.invalid/child.torrent"),
        ))
        .unwrap();
    drop(store);
    edit_first_journal(&directory.0.join("journal.bin"), |value| {
        let job = value.get_mut("job").unwrap();
        job.insert("upgrade_parent", "00000000000000000000000000000000");
        job.insert("release", release("Example.1080p").to_json());
    });
    assert!(Store::open(&directory.0).err().unwrap().contains("parent"));
}

#[test]
fn verified_journal_rejects_rewriting_an_existing_upgrade_parent() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = ready(&mut store, "Example", None);
    let child = store
        .submit_upgrade(
            &parent.id,
            request("Example", Some("https://example.invalid/child.torrent")),
            release("Example.1080p"),
        )
        .unwrap();
    store.cancel(&child.id).unwrap();
    drop(store);
    edit_last_journal(&directory.0.join("journal.bin"), |value| {
        value
            .get_mut("job")
            .unwrap()
            .insert("upgrade_parent", Value::Null);
    });
    assert!(
        Store::open(&directory.0)
            .err()
            .unwrap()
            .contains("parent changed")
    );
}
