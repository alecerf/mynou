//! Durable library identity, upgrade provenance and monitoring transactions.
mod jobs_state;
use mynou::json::Value;
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
        source_numbering: None,
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
    store.get(&job.id).unwrap().unwrap()
}

fn complete(store: &mut Store, mut job: Job) -> Job {
    job.state = "ready".into();
    job.progress = 1.0;
    job.imports = vec![format!("/library/{}.mkv", job.id)];
    store.update(job.clone()).unwrap();
    store.get(&job.id).unwrap().unwrap()
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
        Store::open(&directory.0)
            .unwrap()
            .get(&job.id)
            .unwrap()
            .unwrap(),
        recorded
    );
}

#[test]
fn legacy_ready_cancellation_migrates_as_cancelled_and_leaves_the_library() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut job = store
        .submit(request("Legacy ready cancellation", None))
        .unwrap();
    job.state = "ready".into();
    job.imports = vec!["/library/legacy.mkv".into()];
    store.update(job.clone()).unwrap();
    assert!(store.cancel(&job.id).is_err());
    let mut cancelled = store.get(&job.id).unwrap().unwrap();
    drop(store);
    // An earlier version could record a cancellation of a ready import.
    cancelled.state = "cancelled".into();
    jobs_state::downgrade(&directory.0, std::slice::from_ref(&cancelled));
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.get(&job.id).unwrap().unwrap().state, "cancelled");
    assert!(reopened.library_jobs().unwrap().is_empty());
    assert_eq!(reopened.counts().unwrap().library, 0);
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
    assert_eq!(store.library_jobs().unwrap(), vec![parent.clone()]);
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(store.library_jobs().unwrap(), vec![parent.clone()]);
    assert_eq!(store.get(&child.id).unwrap().unwrap(), child);
    let child = complete(&mut store, child);
    assert_eq!(store.library_jobs().unwrap(), vec![child.clone()]);
    assert_eq!(store.get(&parent.id).unwrap().unwrap(), parent);
    store.record_monitor_check(&child.id, 500).unwrap();
    store.record_monitor_check(&child.id, 400).unwrap();
    let child = store.set_monitored(&child.id, false).unwrap();
    assert_eq!(child.monitor_checked_at, 500);
    drop(store);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.library_jobs().unwrap(), vec![child]);
    assert_eq!(reopened.get(&parent.id).unwrap().unwrap(), parent);
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
    let failed = store.get(&child.id).unwrap().unwrap();
    assert_eq!(
        store
            .submit_upgrade(&parent.id, request, candidate)
            .unwrap(),
        failed
    );
    assert_eq!(store.library_jobs().unwrap(), vec![parent]);
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
    assert_eq!(store.library_jobs().unwrap(), vec![replacement]);
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
    let promoted = store.get(&child.id).unwrap().unwrap();
    assert!(!promoted.monitored);
    assert_eq!(store.library_jobs().unwrap(), vec![promoted.clone()]);
    drop(store);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.library_jobs().unwrap(), vec![promoted.clone()]);
    drop(reopened);
    assert_eq!(
        Store::open(&directory.0).unwrap().library_jobs().unwrap(),
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
    assert_eq!(store.library_jobs().unwrap(), vec![parent.clone()]);
    assert_eq!(store.get(&unrelated.id).unwrap().unwrap().state, "queued");
    drop(store);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.library_jobs().unwrap(), vec![parent]);
    reopened.cancel(&child.id).unwrap();
    reopened.check_ready_promotion(&unrelated).unwrap();
    reopened.update(unrelated).unwrap();
    assert!(reopened.claim(store::now(), 600).unwrap().is_none());
}

fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>, u32, std::time::SystemTime)> {
    use std::os::unix::fs::PermissionsExt;
    fn walk(directory: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            found.push(path.clone());
            if path.is_dir() {
                walk(&path, found);
            }
        }
    }
    let mut paths = Vec::new();
    walk(root, &mut paths);
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let metadata = fs::metadata(&path).unwrap();
            let bytes = if path.is_file() {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            (
                path,
                bytes,
                metadata.permissions().mode() & 0o777,
                metadata.modified().unwrap(),
            )
        })
        .collect()
}

#[test]
fn read_only_store_loads_verified_state_without_file_or_permission_changes() {
    use std::os::unix::fs::PermissionsExt;
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = ready(&mut store, "Example", None);
    let pending = store.submit(request("Queued", None)).unwrap();
    drop(store);
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o750)).unwrap();
    fs::set_permissions(directory.0.join(".lock"), fs::Permissions::from_mode(0o640)).unwrap();
    let before = tree(&directory.0);
    let mut read_only = Store::open_read_only(&directory.0).unwrap();
    assert_eq!(read_only.library_jobs().unwrap(), vec![parent]);
    assert_eq!(read_only.get(&pending.id).unwrap().unwrap(), pending);
    assert_eq!(read_only.page(None, 10).unwrap().jobs.len(), 2);
    assert!(
        Store::open_read_only(&directory.0).is_err(),
        "read-only loading still excludes concurrent writers"
    );
    for error in [
        read_only
            .submit(request("Forbidden write", None))
            .unwrap_err(),
        read_only.cancel(&pending.id).unwrap_err(),
        read_only.claim(store::now(), 60).unwrap_err(),
    ] {
        assert!(error.contains("read-only"), "{error}");
    }
    drop(read_only);
    assert_eq!(
        fs::metadata(&directory.0).unwrap().permissions().mode() & 0o777,
        0o750
    );
    assert_eq!(tree(&directory.0), before);
    let absent = directory.0.join("absent");
    assert!(Store::open_read_only(&absent).is_err());
    assert!(!absent.exists());
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
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.get(&retried.id).unwrap().unwrap(), retried);
}

#[test]
fn unrelated_ready_roots_use_creation_order_even_after_monitor_timestamp_changes() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut first = ready(
        &mut store,
        "Example",
        Some("https://example.invalid/first.torrent"),
    );
    let mut second = ready(
        &mut store,
        "Example",
        Some("https://example.invalid/second.torrent"),
    );
    drop(store);
    first.created_at = 100;
    first.updated_at = 999_999;
    first.monitor_checked_at = 999_999;
    second.created_at = 200;
    second.updated_at = 200;
    jobs_state::downgrade(&directory.0, &[first.clone(), second.clone()]);
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(store.library_jobs().unwrap()[0].id, second.id);
    store.record_monitor_check(&second.id, u64::MAX).unwrap();
    assert_eq!(store.library_jobs().unwrap()[0].id, second.id);
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
    let parent = store.library_jobs().unwrap().remove(0);
    let other = if parent.id == first.id { second } else { first };
    let child = store
        .submit_upgrade(
            &parent.id,
            request("Example", Some("https://example.invalid/child.torrent")),
            release("Example.1080p"),
        )
        .unwrap();
    let child = complete(&mut store, child);
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
    let jobs: Vec<Job> = [parent.clone(), other.clone(), child.clone()]
        .into_iter()
        .map(|mut job| {
            job.id = mapped_id(&job.id);
            job.created_at = 100;
            job.upgrade_parent = job.upgrade_parent.as_deref().map(mapped_id);
            job
        })
        .collect();
    jobs_state::downgrade(&directory.0, &jobs);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.library_jobs().unwrap()[0].id, child_id);
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
    assert_eq!(reopened.library_jobs().unwrap(), vec![grandchild.clone()]);
    drop(reopened);
    assert_eq!(
        Store::open(&directory.0).unwrap().library_jobs().unwrap(),
        vec![grandchild]
    );
}

#[test]
fn migration_rejects_missing_parent_cross_media_and_cycles() {
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
        drop(store);
        let (mut parent, mut child) = (parent, child);
        match damage {
            "missing" => child.upgrade_parent = Some("0".repeat(32)),
            "media" => child.request.title = "Unrelated media".into(),
            _ => parent.upgrade_parent = Some(child.id.clone()),
        }
        jobs_state::downgrade(&directory.0, &[parent, child]);
        let snapshot = directory.0.join("snapshot.bin");
        let before = fs::read(&snapshot).unwrap();
        assert!(
            Store::open(&directory.0).is_err(),
            "verified {damage} snapshot must be rejected"
        );
        assert_eq!(fs::read(&snapshot).unwrap(), before);
        assert!(!directory.0.join("journal.bin").exists());
        assert!(!directory.0.join("jobs.idx").exists());
    }
}

#[test]
fn migration_rejects_a_fabricated_upgrade_parent() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut job = store
        .submit(request(
            "Example",
            Some("https://example.invalid/child.torrent"),
        ))
        .unwrap();
    drop(store);
    job.upgrade_parent = Some("0".repeat(32));
    job.release = Some(release("Example.1080p"));
    jobs_state::downgrade(&directory.0, &[job]);
    assert!(Store::open(&directory.0).err().unwrap().contains("parent"));
}
