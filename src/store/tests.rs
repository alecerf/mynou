//! Job store behavior: ownership and deduplication on the index, the claim
//! queue, history, a large store, atomic group commits at
//! every crash point, and recoverable migration of every legacy format.
use super::{
    legacy::{self, fixture},
    migration::{self, Stage},
    *,
};
use crate::inventory::Fault;
use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[test]
fn watchlist_cursor_is_durable_before_the_first_job() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let cursor = crate::integrations::SweepCursor {
        position: 1_593,
        total: Some(1_700),
        anchor: Some([7; 32]),
        consistent: true,
    };
    store.save_watchlist_cursor(&cursor).unwrap();
    assert_eq!(store.counts().unwrap().jobs, 0);
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(store.watchlist_cursor().unwrap(), Some(cursor.clone()));
    assert_eq!(store.counts().unwrap().jobs, 0);
    drop(store);
    let mut store = Store::open_read_only(&directory.0).unwrap();
    assert_eq!(store.watchlist_cursor().unwrap(), Some(cursor.clone()));
    assert_eq!(
        store.save_watchlist_cursor(&cursor).unwrap_err(),
        "storage is read-only"
    );
}

#[test]
fn an_absent_read_only_store_cannot_acknowledge_a_watchlist_cursor() {
    let directory = Directory::new();
    let lock = secure_file(&directory.0.join(".lock")).unwrap();
    drop(lock);
    let mut store = Store::open_read_only(&directory.0).unwrap();
    assert_eq!(store.watchlist_cursor().unwrap(), None);
    assert_eq!(
        store
            .save_watchlist_cursor(&crate::integrations::SweepCursor::start())
            .unwrap_err(),
        "storage is read-only"
    );
    assert!(!directory.0.join("jobs.idx").exists());
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mynou-store-{}-{}-{}",
            std::process::id(),
            now(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("temporary directory");
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request(title: &str) -> Request {
    Request {
        kind: "movie".to_owned(),
        title: title.to_owned(),
        year: 2026,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: None,
        source_numbering: None,
        tmdb_id: None,
    }
}

/// A complete job with a chosen identity and creation time, as a fixture.
fn raw_job(number: u64, created_at: u64) -> Job {
    let mut request = request(&format!("Synthetic movie {number}"));
    request.tmdb_id = Some(1_000 + number);
    Job {
        id: format!("{number:032x}"),
        key: request.canonical_key(),
        request,
        state: "queued".into(),
        progress: 0.0,
        files: Vec::new(),
        imports: Vec::new(),
        attempts: 0,
        last_error: None,
        created_at,
        updated_at: created_at,
        next_attempt_at: 0,
        acquisition_url: None,
        download_id: None,
        lease_id: None,
        lease_until: 0,
        release: None,
        upgrade_parent: None,
        monitored: true,
        monitor_checked_at: 0,
        pack_file: None,
        pack_origin: None,
        shared_file: None,
        shared_upgrade: None,
        requester: None,
        irc_origin: None,
        retiring_transfers: Vec::new(),
    }
}

fn ready_job(number: u64, created_at: u64) -> Job {
    let mut job = raw_job(number, created_at);
    job.state = "ready".into();
    job.progress = 1.0;
    job.imports = vec![format!("/library/{number}.mkv")];
    job
}

fn private(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private fixture");
}

/// Writes the files an earlier version left: a snapshot of `snapshot` jobs and
/// then one journal transaction per job of `tail`.
fn write_legacy(directory: &Path, snapshot: &[Job], tail: &[Job], format: usize) {
    let snapshot_magics = [
        legacy::SNAPSHOT_MAGIC,
        legacy::SHARED_SNAPSHOT_MAGIC,
        legacy::GROUP_SNAPSHOT_MAGIC,
        legacy::REQUESTER_SNAPSHOT_MAGIC,
        legacy::IRC_SNAPSHOT_MAGIC,
    ];
    let journal_magics = [
        legacy::JOURNAL_MAGIC,
        legacy::SHARED_JOURNAL_MAGIC,
        legacy::GROUP_JOURNAL_MAGIC,
        legacy::REQUESTER_JOURNAL_MAGIC,
        legacy::IRC_JOURNAL_MAGIC,
    ];
    fs::create_dir_all(directory).expect("directory");
    let events: Vec<Event> = snapshot
        .iter()
        .enumerate()
        .map(|(index, job)| Event {
            id: index as u64 + 1,
            job_id: job.id.clone(),
            state: job.state.clone(),
            message: "legacy snapshot".into(),
            at: job.updated_at,
        })
        .collect();
    let retained = events.len().saturating_sub(MAX_EVENTS);
    let mut sequence = events.len() as u64;
    let mut chain = if snapshot.is_empty() {
        [0; 32]
    } else {
        sha256(b"chain")
    };
    if !snapshot.is_empty() || tail.is_empty() {
        private(
            &directory.join(legacy::SNAPSHOT),
            &fixture::snapshot(
                snapshot,
                &events[retained..],
                sequence,
                chain,
                snapshot_magics[format],
            ),
        );
    }
    let mut journal = Vec::new();
    for job in tail {
        sequence += 1;
        let event = Event {
            id: sequence,
            job_id: job.id.clone(),
            state: job.state.clone(),
            message: "legacy journal".into(),
            at: job.updated_at,
        };
        let (frame, digest) = fixture::frame(
            std::slice::from_ref(job),
            &event,
            sequence,
            &chain,
            journal_magics[format],
            None,
        );
        journal.extend_from_slice(&frame);
        chain = digest;
    }
    if !journal.is_empty() {
        private(&directory.join(legacy::JOURNAL), &journal);
    }
}

fn contents(directory: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    fn walk(path: &Path, found: &mut BTreeMap<PathBuf, (Vec<u8>, u32)>) {
        for entry in fs::read_dir(path).expect("directory") {
            let path = entry.expect("entry").path();
            let metadata = fs::metadata(&path).expect("metadata");
            if metadata.is_dir() {
                walk(&path, found);
            } else {
                found.insert(
                    path.clone(),
                    (
                        fs::read(&path).expect("read"),
                        metadata.permissions().mode() & 0o777,
                    ),
                );
            }
        }
    }
    let mut found = BTreeMap::new();
    walk(directory, &mut found);
    found
}

fn ids(jobs: &[Job]) -> Vec<String> {
    jobs.iter().map(|job| job.id.clone()).collect()
}

// ---------------------------------------------------------------------------
// Ordinary behavior

#[test]
fn durable_dedup_and_exclusive_owner() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let job = store.submit(request("A  movie")).unwrap();
    assert_eq!(store.submit(request("a movie")).unwrap().id, job.id);
    assert!(Store::open(&directory.0).is_err());
    drop(store);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.get(&job.id).unwrap().unwrap(), job);
    assert_eq!(reopened.events(&job.id).unwrap().len(), 1);
    assert_eq!(reopened.by_key(&job.key).unwrap().unwrap(), job);
    assert!(reopened.get("not-an-identifier").unwrap().is_none());
    assert!(reopened.get(&"0".repeat(32)).unwrap().is_none());
    assert_eq!(
        fs::metadata(&directory.0).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for (path, (_, mode)) in contents(&directory.0) {
        assert_eq!(mode & 0o077, 0, "{} is not private", path.display());
    }
}

#[test]
fn leases_survive_restart_expire_and_reject_stale_completion() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    store.submit(request("With lease")).unwrap();
    let at = now();
    let stale = store.claim(at, 60).unwrap().unwrap();
    assert!(store.claim(at + 1, 60).unwrap().is_none());
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    assert!(store.claim(at + 59, 60).unwrap().is_none());
    let active = store.claim(at + 61, 60).unwrap().unwrap();
    assert_ne!(stale.lease_id, active.lease_id);
    let mut completion = stale;
    completion.state = "ready".to_owned();
    assert!(store.update(completion).unwrap_err().contains("stale"));
    let cancelled = store.cancel(&active.id).unwrap();
    assert_eq!(cancelled.state, "cancelled");
    assert!(
        store.update(active).is_err(),
        "a cancelled lease must no longer modify the job"
    );
}

#[test]
fn exact_u64_json_and_bounded_requests() {
    let mut request = request("Identity");
    request.tmdb_id = Some(u64::MAX);
    assert_eq!(Request::from_json(&request.to_json()).unwrap(), request);
    request.title = "".to_owned();
    assert!(request.validate().is_err());
    let mut ambiguous = self::request("Ambiguous");
    ambiguous.source_path = Some("/media/file.mkv".to_owned());
    ambiguous.source_url = Some("http://127.0.0.1/file.mkv".to_owned());
    assert!(ambiguous.validate().is_err());
}

#[test]
fn explicit_sources_are_distinct_and_lease_release_is_durable() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut first_request = request("Same title");
    first_request.source_path = Some("/media/first.mkv".to_owned());
    let first = store.submit(first_request).unwrap();
    let mut second_request = request("Same title");
    second_request.source_path = Some("/media/second.mkv".to_owned());
    let second = store.submit(second_request).unwrap();
    assert_ne!(first.id, second.id);
    // Both sources share one media identity (creation order, then identifier).
    let found = store.media_jobs(&first.request.media_key()).unwrap();
    assert_eq!(
        found
            .iter()
            .map(|job| job.id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([first.id.clone(), second.id.clone()])
    );
    let lease = store.claim(now(), 60).unwrap().unwrap();
    let lease_id = lease.lease_id.clone().unwrap();
    store.renew(&lease.id, &lease_id, now(), 120).unwrap();
    assert!(store.release_lease(&lease.id, "old-lease").is_err());
    let released = store.release_lease(&lease.id, &lease_id).unwrap();
    assert!(released.lease_id.is_none());
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(store.get(&released.id).unwrap().unwrap(), released);
    assert!(store.update(lease).is_err());
    assert!(store.claim(now(), 60).unwrap().is_some());
}

#[test]
fn workflow_and_heartbeat_survive_polling_without_counting_attempts() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    store.submit(request("Download")).unwrap();
    let at = now();
    let mut job = store.claim(at, 60).unwrap().unwrap();
    assert_eq!(job.attempts, 0);
    let id = job.lease_id.clone().unwrap();
    let heartbeat = store.renew(&job.id, &id, at, 300).unwrap();
    job.state = "downloading".to_owned();
    job.acquisition_url =
        Some("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".to_owned());
    job.download_id = Some("0123456789abcdef0123456789abcdef01234567".to_owned());
    store.update(job.clone()).unwrap();
    assert_eq!(
        store.get(&job.id).unwrap().unwrap().lease_until,
        heartbeat.lease_until
    );
    assert!(
        store
            .transfer_claimed(
                job.download_id.as_deref().unwrap(),
                job.acquisition_url.as_deref()
            )
            .unwrap()
    );
    store.release_lease(&job.id, &id).unwrap();
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    let resumed = store.claim(at, 60).unwrap().unwrap();
    assert_eq!(resumed.state, "downloading");
    assert_eq!(resumed.attempts, 0);
    assert_eq!(resumed.acquisition_url, job.acquisition_url);
    assert_eq!(resumed.download_id, job.download_id);
}

#[test]
fn due_retry_reenters_processing_and_retains_its_lease() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    store.submit(request("To retry")).unwrap();
    let at = now();
    let mut job = store.claim(at, 60).unwrap().unwrap();
    job.state = "failed".to_owned();
    job.attempts = 1;
    job.last_error = Some("transient error".to_owned());
    job.next_attempt_at = at + 5;
    store.update(job).unwrap();
    assert!(store.claim(at + 4, 60).unwrap().is_none());
    let mut resumed = store.claim(at + 5, 60).unwrap().unwrap();
    assert_eq!(resumed.state, "processing");
    assert_eq!(resumed.attempts, 1);
    resumed.acquisition_url = Some("http://127.0.0.1/source.torrent".to_owned());
    store.update(resumed.clone()).unwrap();
    assert_eq!(
        store.get(&resumed.id).unwrap().unwrap().lease_id,
        resumed.lease_id
    );
}

#[test]
fn history_is_bounded_per_job_keeps_sequence_ids_and_folds_heartbeats() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut job = store.submit(request("Bounded history")).unwrap();
    let other = store.submit(request("Other history")).unwrap();
    for _ in 0..MAX_JOB_EVENTS + 7 {
        job.progress = 0.5;
        store.update(job.clone()).unwrap();
    }
    let history = store.events(&job.id).unwrap();
    assert_eq!(history.len(), MAX_JOB_EVENTS);
    // Ids are the store-wide sequence: the other job's event holds number 2.
    assert!(history.windows(2).all(|pair| pair[0].id < pair[1].id));
    assert_eq!(history.last().unwrap().id, 2 + MAX_JOB_EVENTS as u64 + 7);
    assert_eq!(store.events(&other.id).unwrap().len(), 1);
    // The bound never evicts another job's history, and the feed holds the
    // retained events only.
    let mut feed = Vec::new();
    let mut cursor = None;
    loop {
        let (page, next) = store.event_feed(cursor, MAX_PAGE_ROWS).unwrap();
        feed.extend(page);
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(feed.len(), MAX_JOB_EVENTS + 1);
    assert!(feed.windows(2).all(|pair| pair[0].id < pair[1].id));
    drop(store);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.events(&job.id).unwrap(), history);
    job.last_error = Some("é".repeat(MAX_EVENT_MESSAGE));
    reopened.update(job.clone()).unwrap();
    let history = reopened.events(&job.id).unwrap();
    assert_eq!(history.len(), MAX_JOB_EVENTS);
    assert_eq!(history.last().unwrap().message.len(), MAX_EVENT_MESSAGE);
    // Heartbeats fold into one entry instead of pushing real history out.
    let queued = reopened.submit(request("Heartbeats")).unwrap();
    let claimed = reopened
        .claim_filtered(now(), 60, |candidate| Ok(candidate.id == queued.id))
        .unwrap()
        .unwrap();
    let lease = claimed.lease_id.clone().unwrap();
    for _ in 0..20 {
        reopened.renew(&claimed.id, &lease, now(), 120).unwrap();
    }
    let events = reopened.events(&claimed.id).unwrap();
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0].message, "request recorded");
    assert_eq!(events[1].message, "processing lease acquired");
    assert_eq!(events[2].message, "processing lease renewed");
    drop(reopened);
    let again = Store::open(&directory.0).unwrap();
    drop(again);
}

#[test]
fn symlinked_or_foreign_storage_is_rejected() {
    use std::os::unix::fs::symlink;
    let directory = Directory::new();
    let real = directory.0.join("real");
    fs::create_dir(&real).unwrap();
    symlink(&real, directory.0.join("alias")).unwrap();
    assert!(Store::open(&directory.0.join("alias")).is_err());
    let store = directory.0.join("store");
    drop(Store::open(&store).unwrap());
    let moved = directory.0.join("moved");
    fs::rename(store.join("jobs.idx"), &moved).unwrap();
    symlink(&moved, store.join("jobs.idx")).unwrap();
    let error = Store::open(&store).err().unwrap();
    assert!(error.contains("symbolic link"), "{error}");
    // A hard-linked lock file could create two owners.
    let original = directory.0.join("original");
    let alias = directory.0.join("alias-store");
    drop(Store::open(&original).unwrap());
    fs::create_dir(&alias).unwrap();
    fs::hard_link(original.join(".lock"), alias.join(".lock")).unwrap();
    assert!(Store::open(&alias).err().unwrap().contains("hard links"));
}

#[test]
fn a_failed_write_never_discards_acknowledged_jobs() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let kept: Vec<Job> = (0..5)
        .map(|n| store.submit(request(&format!("Kept {n}"))).unwrap())
        .collect();
    // No room for the redo log or a record: the index directory is read-only.
    let index = directory.0.join("jobs.idx");
    fs::set_permissions(&index, fs::Permissions::from_mode(0o500)).unwrap();
    let outcome = store.submit(request("Refused"));
    fs::set_permissions(&index, fs::Permissions::from_mode(0o700)).unwrap();
    if fs::metadata(&index).unwrap().permissions().mode() & 0o200 != 0 && outcome.is_ok() {
        // Running as a user that ignores permissions (for example root):
        // nothing failed, so there is nothing to prove.
        return;
    }
    assert!(outcome.is_err());
    // The failure is reported, never silently absorbed, and the store keeps
    // refusing writes until it is reopened.
    drop(store);
    let mut reopened = Store::open(&directory.0).unwrap();
    for job in &kept {
        assert_eq!(&reopened.get(&job.id).unwrap().unwrap(), job);
    }
    assert_eq!(reopened.counts().unwrap().jobs, 5);
    assert!(reopened.submit(request("Accepted later")).is_ok());
}

#[test]
fn an_oversized_record_is_refused_and_leaves_the_store_unchanged() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut job = store.submit(request("Oversized")).unwrap();
    let before = store.get(&job.id).unwrap().unwrap();
    job.files = vec!["x".repeat(legacy::MAX_RECORD)];
    let error = store.update(job.clone()).unwrap_err();
    assert!(error.contains("16 MiB"), "{error}");
    assert_eq!(store.get(&job.id).unwrap().unwrap(), before);
    job.files = vec!["x".repeat(1_000)];
    store.update(job).unwrap();
}

// ---------------------------------------------------------------------------
// Indexed ownership, deduplication and the claim queue

#[test]
fn transfer_ownership_is_answered_from_the_transfer_entries() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let transfer = "0123456789abcdef0123456789abcdef01234567";
    let mut first = store.submit(request("Shared transfer one")).unwrap();
    let mut second = store.submit(request("Shared transfer two")).unwrap();
    let alone = store.submit(request("Alone")).unwrap();
    for job in [&mut first, &mut second] {
        job.download_id = Some(transfer.into());
        job.acquisition_url = Some("magnet:?xt=urn:btih:shared".into());
        store.update(job.clone()).unwrap();
    }
    assert_eq!(store.transfer_jobs(transfer).unwrap().len(), 2);
    assert!(store.transfer_has_owner(transfer).unwrap());
    assert!(store.has_co_owner(&first).unwrap());
    assert!(!store.has_co_owner(&alone).unwrap());
    assert!(
        store
            .transfer_claimed("f".repeat(40).as_str(), Some("magnet:?xt=urn:btih:shared"))
            .unwrap()
    );
    store.cancel(&second.id).unwrap();
    assert!(!store.has_co_owner(&first).unwrap());
    store.cancel(&first.id).unwrap();
    assert!(!store.transfer_has_owner(transfer).unwrap());
    // Retrying detaches the transfer from the request and queues its retirement.
    let retried = store.retry(&first.id).unwrap();
    assert_eq!(retried.retiring_transfers, vec![transfer.to_owned()]);
    assert!(store.transfer_jobs(transfer).unwrap().len() <= 2);
    assert_eq!(
        store.pending_retirements(10).unwrap(),
        vec![(first.id.clone(), transfer.to_owned())]
    );
    store.complete_retirement(&first.id, transfer).unwrap();
    assert!(store.pending_retirements(10).unwrap().is_empty());
}

#[test]
fn upgrade_lineage_tips_and_pending_upgrades_follow_the_entries() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut parent = store
        .submit(Request {
            source_url: Some("https://example.invalid/first.torrent".into()),
            ..request("Lineage")
        })
        .unwrap();
    parent.state = "ready".into();
    parent.progress = 1.0;
    parent.imports = vec!["/library/lineage.mkv".into()];
    parent.release = Some(RecordedRelease {
        title: "Lineage.720p".into(),
        profile: "hd".into(),
    });
    store.update(parent.clone()).unwrap();
    assert!(store.is_library_tip(&parent.id).unwrap());
    assert_eq!(store.counts().unwrap().library, 1);
    let child = store
        .submit_upgrade(
            &parent.id,
            Request {
                source_url: Some("https://example.invalid/second.torrent".into()),
                ..request("Lineage")
            },
            RecordedRelease {
                title: "Lineage.1080p".into(),
                profile: "hd".into(),
            },
        )
        .unwrap();
    assert_eq!(
        store
            .pending_for(&parent.request.media_key())
            .unwrap()
            .unwrap()
            .id,
        child.id
    );
    assert!(store.is_library_tip(&parent.id).unwrap());
    // The parent stays the library version until the child is ready.
    let mut promoted = child;
    promoted.state = "ready".into();
    promoted.progress = 1.0;
    promoted.imports = vec!["/library/lineage-better.mkv".into()];
    store.update(promoted.clone()).unwrap();
    assert!(!store.is_library_tip(&parent.id).unwrap());
    assert!(store.is_library_tip(&promoted.id).unwrap());
    assert_eq!(store.counts().unwrap().library, 1);
    assert!(
        store
            .pending_for(&parent.request.media_key())
            .unwrap()
            .is_none()
    );
    let page = store.library_page(None, 10).unwrap();
    assert_eq!(ids(&page.jobs), vec![promoted.id.clone()]);
    assert_eq!(page.total, 1);
    drop(store);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert!(reopened.is_library_tip(&promoted.id).unwrap());
    assert_eq!(reopened.library_jobs().unwrap().len(), 1);
}

#[test]
fn claim_work_per_attempt_is_bounded_and_the_cursor_reaches_every_job() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let jobs: Vec<Job> = (0..600).map(|n| raw_job(n, 1_000 + n)).collect();
    bulk(&mut store, jobs.clone());
    // The oldest 300 cannot run (for example, they wait for another system).
    let blocked: BTreeSet<String> = jobs[..300].iter().map(|job| job.id.clone()).collect();
    let examined = std::cell::Cell::new(0);
    let eligible = |job: &Job| {
        examined.set(examined.get() + 1);
        Ok(!blocked.contains(&job.id))
    };
    // Each attempt promotes a bounded batch and examines a bounded window of
    // the queue; resuming after the entries already examined reaches the
    // oldest job that can run, behind 300 that cannot.
    let mut claimed = None;
    let mut attempts = 0;
    while claimed.is_none() && attempts < 8 {
        examined.set(0);
        claimed = store.claim_filtered(now(), 60, eligible).unwrap();
        assert!(examined.get() <= CLAIM_SCAN, "examined {}", examined.get());
        attempts += 1;
    }
    assert!((2..=6).contains(&attempts), "{attempts} attempts");
    assert_eq!(claimed.unwrap().id, jobs[300].id);
}

#[test]
fn due_jobs_are_promoted_in_bounded_batches_and_claimed_oldest_first() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let at = now();
    let mut jobs: Vec<Job> = (0..250).map(|n| raw_job(n, 1_000 + n)).collect();
    for job in &mut jobs {
        job.state = "failed".into();
        job.attempts = 1;
        job.next_attempt_at = at - 1;
    }
    // One job that is not due yet and one that is queued now.
    let mut later = raw_job(900, 1_005);
    later.state = "failed".into();
    later.next_attempt_at = at + 3_600;
    jobs.push(later.clone());
    bulk(&mut store, jobs.clone());
    let mut claimed = Vec::new();
    while let Some(job) = store.claim(at, 100_000).unwrap() {
        claimed.push(job.id);
    }
    let expected: Vec<String> = jobs[..250].iter().map(|job| job.id.clone()).collect();
    assert_eq!(claimed, expected);
    assert!(store.claim(at, 100_000).unwrap().is_none());
    // Once its wait ends the remaining job is handed out.
    assert_eq!(
        store.claim(at + 3_601, 100_000).unwrap().unwrap().id,
        later.id
    );
}

#[test]
fn the_claim_applies_group_leases_and_current_parents() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut parent = store.submit(request("Claim parent")).unwrap();
    parent.state = "ready".into();
    parent.progress = 1.0;
    parent.imports = vec!["/library/parent.mkv".into()];
    parent.release = Some(RecordedRelease {
        title: "Claim.parent.720p".into(),
        profile: "hd".into(),
    });
    store.update(parent.clone()).unwrap();
    let child = store
        .submit_upgrade(
            &parent.id,
            Request {
                source_url: Some("https://example.invalid/child.torrent".into()),
                ..request("Claim parent")
            },
            RecordedRelease {
                title: "Claim.parent.1080p".into(),
                profile: "hd".into(),
            },
        )
        .unwrap();
    assert_eq!(store.claim(now(), 60).unwrap().unwrap().id, child.id);
    let lease = store.get(&child.id).unwrap().unwrap().lease_id.unwrap();
    store.release_lease(&child.id, &lease).unwrap();
    // Monitoring off: an upgrade of an unmonitored parent never runs.
    store.set_monitored(&parent.id, false).unwrap();
    assert!(store.claim(now(), 60).unwrap().is_none());
    store.set_monitored(&parent.id, true).unwrap();
    assert!(store.claim(now(), 60).unwrap().is_some());
}

// ---------------------------------------------------------------------------
// A large store

/// Writes jobs through the index in transactions of up to 100, as migration
/// does, so tests can build stores far above the old limits quickly.
fn bulk(store: &mut Store, jobs: Vec<Job>) {
    for chunk in jobs.chunks(100) {
        store
            .mutate(|db| {
                let changes = db.stage(chunk, "bulk fixture")?;
                db.0.commit(changes)?;
                Ok(())
            })
            .expect("bulk transaction");
    }
}

// Independently proposed QA fixtures for the indexed monitoring window.
fn monitored_fixture(n: u64, checked: u64) -> Job {
    let mut job = ready_job(n, 1_000 + n);
    if n.is_multiple_of(2) {
        job.request.kind = "episode".into();
        job.request.season = 1;
        job.request.episode = 1;
        job.key = job.request.canonical_key();
    }
    job.release = Some(RecordedRelease {
        title: format!("Synthetic {n} 2026 720p"),
        profile: "hd".into(),
    });
    job.monitor_checked_at = checked;
    job
}

#[test]
fn monitoring_windows_reach_old_tips_beyond_inventory_prefixes() {
    for total in [201_u64, 10_001] {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        bulk(
            &mut store,
            (1..=total).map(|n| monitored_fixture(n, n)).collect(),
        );
        assert_eq!(store.monitoring_counts().unwrap(), [total, 0, 0, 0, 0]);
        let prefix = store.library_page(None, 200).unwrap();
        assert!(
            !prefix
                .jobs
                .iter()
                .any(|job| job.id == format!("{:032x}", 1))
        );
        let before_requests = match &store.backend {
            Backend::Ready(index) => index.requests(),
            _ => unreachable!(),
        };
        let started = std::time::Instant::now();
        let window = store.monitoring_due(total + 1_000, 60, false, 4).unwrap();
        assert_eq!(window.total, total);
        assert_eq!(
            window
                .jobs
                .iter()
                .map(|job| job.id.clone())
                .collect::<Vec<_>>(),
            (1..=4).map(|n| format!("{n:032x}")).collect::<Vec<_>>()
        );
        let requests = match &store.backend {
            Backend::Ready(index) => index.requests() - before_requests,
            _ => unreachable!(),
        };
        assert!(
            requests <= 32,
            "{requests} index requests for four monitoring rows with {total} tips"
        );
        eprintln!(
            "monitoring-window size={total} rows={} index_requests={requests} micros={}",
            window.jobs.len(),
            started.elapsed().as_micros()
        );
        assert!(
            store
                .monitoring_due(5, 60, false, 4)
                .unwrap()
                .jobs
                .iter()
                .all(|job| job.monitor_checked_at > 5)
        );
        assert_eq!(
            store
                .monitoring_due(total + 1_000, 60, true, 257)
                .unwrap()
                .jobs
                .len(),
            total.min(257) as usize
        );
        drop(store);
        let mut reopened = Store::open_read_only(&directory.0).unwrap();
        assert_eq!(reopened.monitoring_counts().unwrap(), [total, 0, 0, 0, 0]);
        assert_eq!(
            reopened
                .monitoring_due(total + 1_000, 60, false, 4)
                .unwrap()
                .jobs[0]
                .id,
            format!("{:032x}", 1)
        );
    }
}

#[test]
fn same_second_checks_rotate_but_unrelated_tip_writes_do_not() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let at = now().saturating_sub(10);
    bulk(
        &mut store,
        (1..=201).map(|n| monitored_fixture(n, at)).collect(),
    );
    let first = store.monitoring_due(at, 0, true, 3).unwrap();
    let mut unrelated = first.jobs[0].clone();
    unrelated.last_error = Some("synthetic unrelated update".into());
    store.update(unrelated).unwrap();
    assert_eq!(
        store
            .monitoring_due(at, 0, true, 3)
            .unwrap()
            .jobs
            .iter()
            .map(|j| &j.id)
            .collect::<Vec<_>>(),
        first.jobs.iter().map(|j| &j.id).collect::<Vec<_>>()
    );
    let mut seen = BTreeSet::new();
    while seen.len() < 201 {
        let window = store.monitoring_due(at, 0, true, 3).unwrap();
        for job in window.jobs.into_iter().take(201 - seen.len()) {
            assert!(
                seen.insert(job.id.clone()),
                "same-second forced checks repeated before covering every tip"
            );
            store.record_monitor_check(&job.id, at).unwrap();
        }
    }
    assert_eq!(
        store.monitoring_due(at, 0, true, 1).unwrap().jobs[0].id,
        first.jobs[0].id
    );
    assert!(
        store
            .monitoring_due(at, 60, false, 3)
            .unwrap()
            .jobs
            .is_empty()
    );
    assert_eq!(store.monitoring_counts().unwrap(), [201, 0, 0, 0, 0]);
}

fn pending_monitoring_fixture(store: &mut Store) {
    let prefix = store.library_page(None, 100).unwrap().jobs;
    let index = match &mut store.backend {
        Backend::Ready(index) => index,
        _ => unreachable!(),
    };
    let mut changes = Vec::new();
    for table in [
        layout::T_MONITOR_REF,
        layout::T_MONITOR_DUE,
        layout::T_MONITOR_SHARED,
        layout::T_MONITOR_UNCHECKED,
    ] {
        let mut after = None;
        loop {
            let page = ops::Db(index).scan(&[table], after, 200).unwrap();
            changes.extend(page.entries.into_iter().map(|(key, _)| Change::Delete(key)));
            after = page.next;
            if after.is_none() {
                break;
            }
        }
    }
    index.commit(changes).unwrap();
    let mut changes = Vec::new();
    for job in &prefix {
        let id = layout::id_bytes(&job.id).unwrap();
        let at = job.monitor_checked_at.to_be_bytes();
        let round = 0_u64.to_be_bytes();
        let mut reference = vec![0];
        reference.extend_from_slice(&at);
        reference.extend_from_slice(&round);
        changes.push(Change::PutInline(
            layout::key(&[&[layout::T_MONITOR_REF], &id]),
            reference,
        ));
        changes.push(Change::PutInline(
            layout::key(&[
                &[layout::T_MONITOR_UNCHECKED],
                &job.created_at.to_be_bytes(),
                &round,
                &id,
            ]),
            Vec::new(),
        ));
    }
    let last = prefix.last().unwrap();
    changes.push(Change::PutInline(
        layout::FORMAT_KEY,
        monitoring::PENDING.to_vec(),
    ));
    changes.push(Change::Put(
        layout::MONITOR_COUNTS_KEY,
        monitoring::encode([100, 0, 0, 0, 0]),
    ));
    changes.push(Change::Put(
        layout::MONITOR_CURSOR_KEY,
        layout::library_key(last.created_at, &layout::id_bytes(&last.id).unwrap()).to_vec(),
    ));
    index.commit(changes).unwrap();
}

#[test]
fn pending_monitoring_upgrade_is_read_only_safe_and_recovers_each_crash_boundary() {
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let jobs: Vec<Job> = (1..=201).map(|n| monitored_fixture(n, 0)).collect();
        bulk(&mut store, jobs.clone());
        pending_monitoring_fixture(&mut store);
        drop(store);
        let before = contents(&directory.0);
        let error = Store::open_read_only(&directory.0).err().unwrap();
        assert!(error.contains("writable restart"), "{error}");
        assert_eq!(contents(&directory.0), before);
        let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
        index.inject(Some(fault));
        assert!(monitoring::upgrade(&mut index, false, Path::new("/nonexistent")).is_err());
        assert!(!index.is_usable());
        drop(index);
        let mut reopened = Store::open(&directory.0).unwrap();
        assert_eq!(
            reopened.monitoring_counts().unwrap(),
            [201, 0, 0, 0, 0],
            "{fault:?}"
        );
        assert_eq!(
            reopened
                .monitoring_due(now(), 60, false, 257)
                .unwrap()
                .jobs
                .len(),
            201
        );
        assert_eq!(reopened.counts().unwrap().library, 201);
        for job in &jobs {
            assert_eq!(reopened.get(&job.id).unwrap().unwrap(), *job);
        }
        let index = match &mut reopened.backend {
            Backend::Ready(index) => index,
            _ => unreachable!(),
        };
        assert_eq!(
            index.get(&layout::FORMAT_KEY).unwrap().unwrap(),
            backlog::FORMAT
        );
        assert!(index.get(&layout::MONITOR_CURSOR_KEY).unwrap().is_none());
        for table in [layout::T_MONITOR_REF, layout::T_MONITOR_UNCHECKED] {
            let mut count = 0;
            let mut after = None;
            loop {
                let page = ops::Db(index).scan(&[table], after, 200).unwrap();
                count += page.entries.len();
                after = page.next;
                if after.is_none() {
                    break;
                }
            }
            assert_eq!(count, 201, "derived table {table} after {fault:?}");
        }
        drop(reopened);
        let mut read_only = Store::open_read_only(&directory.0).unwrap();
        assert_eq!(read_only.monitoring_counts().unwrap(), [201, 0, 0, 0, 0]);
    }
}

#[test]
fn monitoring_counters_follow_baselines_flags_shared_owners_and_current_tips() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let eligible = monitored_fixture(1, 0);
    let mut unmonitored = monitored_fixture(2, 0);
    unmonitored.monitored = false;
    let mut baseline = monitored_fixture(3, 0);
    baseline.release = None;
    let mut unsupported = monitored_fixture(4, 0);
    unsupported.request.kind = "file".into();
    unsupported.request.season = 0;
    unsupported.request.episode = 0;
    unsupported.key = unsupported.request.canonical_key();
    bulk(
        &mut store,
        vec![
            eligible.clone(),
            unmonitored.clone(),
            baseline.clone(),
            unsupported,
        ],
    );
    assert_eq!(store.monitoring_counts().unwrap(), [1, 1, 1, 1, 0]);
    let file = shared_file();
    let owners = store
        .submit_shared_file(file.clone(), shared_requests(&file))
        .unwrap();
    for mut owner in owners {
        owner.state = "ready".into();
        owner.progress = 1.0;
        owner.imports = vec![file.import_path.clone()];
        store.update(owner).unwrap();
    }
    assert_eq!(store.monitoring_counts().unwrap(), [1, 1, 1, 1, 3]);
    store
        .set_baseline(&baseline.id, eligible.release.clone().unwrap())
        .unwrap();
    store.set_monitored(&unmonitored.id, true).unwrap();
    store.set_monitored(&eligible.id, false).unwrap();
    assert_eq!(store.monitoring_counts().unwrap(), [2, 1, 0, 1, 3]);
    let mut request = baseline.request.clone();
    request.source_url = Some("http://127.0.0.1:9/synthetic-upgrade.torrent".into());
    let mut child = store
        .submit_upgrade(
            &baseline.id,
            request,
            RecordedRelease {
                title: "Synthetic 3 2026 1080p".into(),
                profile: "hd".into(),
            },
        )
        .unwrap();
    assert_eq!(store.monitoring_counts().unwrap(), [2, 1, 0, 1, 3]);
    child.state = "ready".into();
    child.progress = 1.0;
    child.imports = vec!["/library/synthetic-upgrade.mkv".into()];
    store.update(child.clone()).unwrap();
    assert!(!store.is_library_tip(&baseline.id).unwrap());
    assert!(store.is_library_tip(&child.id).unwrap());
    assert_eq!(store.monitoring_counts().unwrap(), [2, 1, 0, 1, 3]);
    let window = store.monitoring_due(1_000, 60, true, 257).unwrap();
    assert!(window.jobs.iter().any(|job| job.id == child.id));
    assert!(!window.jobs.iter().any(|job| job.id == baseline.id));
    store.set_monitored(&child.id, false).unwrap();
    assert_eq!(store.monitoring_counts().unwrap(), [1, 2, 0, 1, 3]);
    let shared_id = store.monitored_shared(200).unwrap()[0].id.clone();
    store.set_monitored(&shared_id, false).unwrap();
    assert_eq!(store.monitoring_counts().unwrap(), [1, 3, 0, 1, 2]);
    assert_eq!(store.monitored_shared(200).unwrap().len(), 2);
}

#[test]
fn rollback_checks_correct_future_times_with_one_slot() {
    for force in [false, true] {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let at = now();
        let mut jobs: Vec<Job> = (1..=3)
            .map(|n| monitored_fixture(n, at + 86_400 * n))
            .collect();
        let mut unchecked = monitored_fixture(4, 0);
        unchecked.created_at = at + 86_400;
        unchecked.updated_at = unchecked.created_at;
        jobs.push(unchecked);
        bulk(&mut store, jobs.clone());
        let mut seen = BTreeSet::new();
        for expected in &jobs {
            let window = store.monitoring_due(at, 60, force, 1).unwrap();
            assert_eq!(window.jobs.len(), 1);
            assert_eq!(window.jobs[0].id, expected.id);
            assert!(seen.insert(window.jobs[0].id.clone()));
            store.record_monitor_check(&window.jobs[0].id, at).unwrap();
            assert_eq!(
                store.get(&expected.id).unwrap().unwrap().monitor_checked_at,
                at
            );
        }
        assert_eq!(seen.len(), jobs.len());
        assert!(
            store
                .monitoring_due(at, 60, false, 1)
                .unwrap()
                .jobs
                .is_empty()
        );
        drop(store);
        let mut reopened = Store::open(&directory.0).unwrap();
        assert!(
            reopened
                .monitoring_due(at, 60, false, 1)
                .unwrap()
                .jobs
                .is_empty()
        );
        assert_eq!(
            reopened
                .monitoring_due(at + 60, 60, false, 4)
                .unwrap()
                .jobs
                .len(),
            4
        );
        assert_eq!(reopened.monitoring_counts().unwrap(), [4, 0, 0, 0, 0]);
    }
}

#[test]
fn new_unchecked_imports_do_not_starve_old_due_checks() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let at = now();
    let old = monitored_fixture(1, at - 120);
    bulk(&mut store, vec![old.clone()]);
    for n in 2..=25 {
        let mut new = monitored_fixture(n, 0);
        new.created_at = at;
        new.updated_at = at;
        bulk(&mut store, vec![new]);
        let next = store.monitoring_due(at, 60, false, 1).unwrap();
        assert_eq!(next.jobs[0].id, old.id);
    }
    store.record_monitor_check(&old.id, at).unwrap();
    let next = store.monitoring_due(at, 60, false, 1).unwrap();
    assert_ne!(next.jobs[0].id, old.id);
    assert_eq!(next.jobs[0].monitor_checked_at, 0);
}

#[test]
fn closing_a_store_releases_ownership_with_an_inherited_descriptor() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let job = monitored_fixture(1, 0);
    bulk(&mut store, vec![job.clone()]);
    let inherited = store._lock.try_clone().unwrap();
    assert!(Store::open(&directory.0).is_err());
    drop(store);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.get(&job.id).unwrap().unwrap(), job);
    assert_eq!(reopened.monitoring_counts().unwrap(), [1, 0, 0, 0, 0]);
    drop(inherited);
    assert!(Store::open_read_only(&directory.0).is_err());
    drop(reopened);
    let mut read_only = Store::open_read_only(&directory.0).unwrap();
    assert_eq!(read_only.get(&job.id).unwrap().unwrap(), job);
}

#[test]
fn rollback_correction_keeps_past_monotonicity_and_worker_metadata_guards() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let at = now();
    let past = monitored_fixture(1, at - 120);
    let future = monitored_fixture(2, at + 86_400);
    bulk(&mut store, vec![past.clone(), future.clone()]);
    let mut backwards = past.clone();
    backwards.monitor_checked_at -= 1;
    let error = store
        .mutate(|db| db.commit_jobs(vec![backwards], "synthetic backwards check"))
        .unwrap_err();
    assert_eq!(error, "monitor check timestamp moved backwards");
    assert_eq!(store.get(&past.id).unwrap().unwrap(), past);
    let mut still_future = future.clone();
    still_future.monitor_checked_at -= 1;
    let error = store
        .mutate(|db| db.commit_jobs(vec![still_future], "synthetic incomplete correction"))
        .unwrap_err();
    assert_eq!(error, "monitor check timestamp moved backwards");
    assert_eq!(store.get(&future.id).unwrap().unwrap(), future);
    let mut worker = future.clone();
    worker.monitor_checked_at = at;
    assert_eq!(
        store.update(worker).unwrap_err(),
        "library metadata cannot be changed by a worker"
    );
    assert_eq!(store.get(&future.id).unwrap().unwrap(), future);
    store.record_monitor_check(&past.id, at - 121).unwrap();
    assert_eq!(
        store.get(&past.id).unwrap().unwrap().monitor_checked_at,
        past.monitor_checked_at
    );
    store.record_monitor_check(&future.id, at).unwrap();
    assert_eq!(
        store.get(&future.id).unwrap().unwrap().monitor_checked_at,
        at
    );
    assert_eq!(store.monitoring_counts().unwrap(), [2, 0, 0, 0, 0]);
}

#[test]
fn old_job_format_builds_monitoring_without_changing_primary_records() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let jobs: Vec<Job> = (1..=201)
        .map(|n| {
            let mut job = monitored_fixture(n, 0);
            match n % 4 {
                0 => (),
                1 => job.monitored = false,
                2 => job.release = None,
                _ => {
                    job.request.kind = "file".into();
                    job.request.season = 0;
                    job.request.episode = 0;
                    job.key = job.request.canonical_key();
                }
            }
            job
        })
        .collect();
    bulk(&mut store, jobs);
    let file = shared_file();
    let owners = store
        .submit_shared_file(file.clone(), shared_requests(&file))
        .unwrap();
    for mut owner in owners {
        owner.state = "ready".into();
        owner.progress = 1.0;
        owner.imports = vec![file.import_path.clone()];
        store.update(owner).unwrap();
    }
    let primary = store.list().unwrap();
    let events = store.events("").unwrap();
    let expected = [50, 51, 50, 50, 3];
    assert_eq!(store.monitoring_counts().unwrap(), expected);
    let index = match &mut store.backend {
        Backend::Ready(index) => index,
        _ => unreachable!(),
    };
    let mut changes = Vec::new();
    for table in [
        layout::T_MONITOR_REF,
        layout::T_MONITOR_DUE,
        layout::T_MONITOR_SHARED,
        layout::T_MONITOR_UNCHECKED,
    ] {
        let mut after = None;
        loop {
            let page = ops::Db(index).scan(&[table], after, 200).unwrap();
            changes.extend(page.entries.into_iter().map(|(key, _)| Change::Delete(key)));
            after = page.next;
            if after.is_none() {
                break;
            }
        }
    }
    changes.push(Change::Delete(layout::MONITOR_COUNTS_KEY));
    changes.push(Change::Delete(layout::MONITOR_CURSOR_KEY));
    changes.push(Change::PutInline(
        layout::FORMAT_KEY,
        layout::FORMAT.to_vec(),
    ));
    index.commit(changes).unwrap();
    drop(store);
    let before = contents(&directory.0);
    assert!(
        Store::open_read_only(&directory.0)
            .err()
            .unwrap()
            .contains("writable restart")
    );
    assert_eq!(contents(&directory.0), before);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.list().unwrap(), primary);
    assert_eq!(reopened.events("").unwrap(), events);
    assert_eq!(reopened.monitoring_counts().unwrap(), expected);
    assert_eq!(
        reopened
            .monitoring_due(now(), 60, false, 257)
            .unwrap()
            .jobs
            .len(),
        50
    );
    assert_eq!(reopened.monitored_shared(200).unwrap().len(), 3);
    assert_eq!(reopened.counts().unwrap().library, 204);
    drop(reopened);
    let mut read_only = Store::open_read_only(&directory.0).unwrap();
    assert_eq!(read_only.monitoring_counts().unwrap(), expected);
}

#[test]
fn a_large_store_pages_looks_up_and_claims_with_bounded_work() {
    let directory = Directory::new();
    // The inventory above the former 10,000-job limit, through migration, HTTP,
    // WebSocket and the command line, is covered by the web interface scale
    // test; this one keeps the per-operation bounds quick to check.
    let jobs: Vec<Job> = (0..1_250)
        .map(|n| {
            if n % 100 == 0 {
                ready_job(n, 1_000 + n)
            } else {
                raw_job(n, 1_000 + n)
            }
        })
        .collect();
    let mut store = Store::open(&directory.0).unwrap();
    bulk(&mut store, jobs.clone());
    let counts = store.counts().unwrap();
    assert_eq!(counts.jobs, 1_250);
    assert_eq!(counts.ready, 13);
    assert_eq!(counts.library, 13);
    // Every page is reachable and the walk visits each job exactly once.
    let mut seen = BTreeSet::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    let mut previous = u64::MAX;
    loop {
        let page = store.page(after.as_deref(), 200).unwrap();
        assert_eq!(page.total, 1_250);
        for job in &page.jobs {
            assert!(job.created_at <= previous, "newest first");
            previous = job.created_at;
            assert!(seen.insert(job.id.clone()));
        }
        pages += 1;
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 1_250);
    assert_eq!(pages, 7);
    let mut library = 0;
    let mut after: Option<String> = None;
    loop {
        let page = store.library_page(after.as_deref(), 50).unwrap();
        library += page.jobs.len();
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(library, 13);
    // Lookups read one tree path however many jobs exist.
    let probe = &jobs[777];
    assert_eq!(store.get(&probe.id).unwrap().unwrap(), *probe);
    assert_eq!(store.by_key(&probe.key).unwrap().unwrap().id, probe.id);
    assert_eq!(
        store.media_jobs(&probe.request.media_key()).unwrap().len(),
        1
    );
    let reads = match &store.backend {
        Backend::Ready(index) => index.last_page_reads(),
        _ => unreachable!(),
    };
    assert!(reads <= 12, "{reads} tree pages for a bounded page");
    // The oldest queued job is claimed without reading the inventory.
    let claimed = store.claim(now(), 60).unwrap().unwrap();
    assert_eq!(claimed.id, jobs[1].id);
    // Submissions keep succeeding: no capacity error remains.
    for n in 0..25 {
        let job = store
            .submit(request(&format!("Beyond the old limit {n}")))
            .unwrap();
        assert_eq!(job.state, "queued");
    }
    assert_eq!(store.counts().unwrap().jobs, 1_275);
    drop(store);
    let mut reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.counts().unwrap().jobs, 1_275);
    assert_eq!(reopened.page(None, 200).unwrap().total, 1_275);
    assert_eq!(reopened.get(&probe.id).unwrap().unwrap(), *probe);
    let mut read_only = Store::open_read_only(&directory.0);
    assert!(
        read_only.is_err(),
        "the writable store still holds the lock"
    );
    drop(reopened);
    read_only = Store::open_read_only(&directory.0);
    assert_eq!(read_only.unwrap().counts().unwrap().jobs, 1_275);
}

// ---------------------------------------------------------------------------
// Atomic group updates at every crash point

fn shared_file() -> crate::pack::SharedFile {
    let mut file = crate::pack::SharedFile {
        torrent_id: "a".repeat(40),
        file_path: "Pack/shared.mp4".into(),
        tmdb_id: 42,
        title: "Synthetic Series".into(),
        year: 2024,
        season: 1,
        first_episode: 1,
        last_episode: 3,
        import_path: String::new(),
    };
    file.import_path = crate::organizer::shared_target(Path::new("/library"), &file)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    file
}

fn shared_requests(file: &crate::pack::SharedFile) -> Vec<Request> {
    (file.first_episode..=file.last_episode)
        .map(|episode| Request {
            kind: "episode".into(),
            title: file.title.clone(),
            year: file.year,
            season: file.season,
            episode,
            source_path: None,
            source_url: Some("https://example.invalid/pack.torrent".into()),
            source_numbering: None,
            tmdb_id: Some(file.tmdb_id),
        })
        .collect()
}

fn inject(store: &mut Store, fault: Fault) {
    match &mut store.backend {
        Backend::Ready(index) => index.inject(Some(fault)),
        _ => unreachable!("a ready index"),
    }
}

#[test]
fn a_shared_group_commits_all_owners_or_none_at_every_crash_point() {
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let file = shared_file();
        let bystander = store.submit(request("Bystander")).unwrap();
        inject(&mut store, fault);
        assert!(
            store
                .submit_shared_file(file.clone(), shared_requests(&file))
                .is_err()
        );
        // After an uncertain commit every operation is refused until reopening.
        assert!(store.get(&bystander.id).is_err());
        assert!(store.submit(request("Refused")).is_err());
        drop(store);
        if fault != Fault::Unlinked {
            // A read-only open reports that recovery is required and writes nothing.
            let before = contents(&directory.0);
            let error = Store::open_read_only(&directory.0).err().unwrap();
            assert!(error.to_lowercase().contains("recover"), "{error}");
            assert_eq!(contents(&directory.0), before);
        }
        let mut store = Store::open(&directory.0).unwrap();
        let owners = store.list().unwrap();
        let expected = if fault == Fault::Staged { 1 } else { 4 };
        assert_eq!(owners.len(), expected, "{fault:?}");
        assert_eq!(store.counts().unwrap().jobs, expected as u64);
        if fault != Fault::Staged {
            let group = store
                .shared_group(
                    &owners
                        .iter()
                        .find(|job| job.shared_file.is_some())
                        .unwrap()
                        .id,
                )
                .unwrap();
            assert_eq!(group.len(), 3, "{fault:?}");
            assert!(
                group
                    .iter()
                    .all(|job| job.shared_file.as_ref() == Some(&file))
            );
            assert_eq!(store.events("").unwrap().len(), 2);
        } else {
            assert!(
                store
                    .shared_binding(&file.torrent_id, &file.file_path)
                    .unwrap()
                    .is_none()
            );
        }
        assert!(store.submit(request("Accepted after recovery")).is_ok());
    }
}

#[test]
fn shared_ownership_rejects_conflicts_through_the_entries() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let file = shared_file();
    let owners = store
        .submit_shared_file(file.clone(), shared_requests(&file))
        .unwrap();
    assert_eq!(owners.len(), 3);
    assert_eq!(
        store
            .submit_shared_file(file.clone(), shared_requests(&file))
            .unwrap(),
        owners
    );
    assert_eq!(
        store
            .shared_binding(&file.torrent_id, &file.file_path)
            .unwrap()
            .unwrap(),
        file
    );
    // An independent request for an owned episode is refused; so is an
    // individual upgrade of a shared owner.
    let mut single = shared_requests(&file).remove(0);
    single.source_url = Some("https://example.invalid/other.torrent".into());
    assert!(store.submit(single).is_err());
    assert!(
        store
            .submit_upgrade(
                &owners[0].id,
                Request {
                    source_url: Some("https://example.invalid/upgrade.torrent".into()),
                    ..shared_requests(&file).remove(0)
                },
                RecordedRelease {
                    title: "Pack.1080p".into(),
                    profile: "hd".into()
                }
            )
            .is_err()
    );
    // The same physical video with another range is refused.
    let mut other = file.clone();
    other.last_episode = 2;
    other.import_path = crate::organizer::shared_target(Path::new("/library"), &other)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        store
            .check_shared_submission(&other, &shared_requests(&other))
            .is_err()
    );
    // A group is never partly visible to its readers.
    let group = store.shared_group(&owners[1].id).unwrap();
    assert_eq!(
        group
            .iter()
            .map(|job| job.request.episode)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(store.group_members(&file).unwrap(), group);
}

// ---------------------------------------------------------------------------
// Migration

/// The final state of a legacy store, as the migration is expected to write it.
fn expect_migrated(directory: &Path, expected: &[Job]) {
    let mut store = Store::open(directory).unwrap();
    assert_eq!(store.counts().unwrap().jobs, expected.len() as u64);
    let mut all = store.list().unwrap();
    all.sort_by(|a, b| a.id.cmp(&b.id));
    let mut wanted = expected.to_vec();
    wanted.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(all, wanted);
    for job in expected {
        assert_eq!(&store.get(&job.id).unwrap().unwrap(), job);
        assert_eq!(&store.by_key(&job.key).unwrap().unwrap(), job);
    }
    let library: BTreeSet<String> = expected
        .iter()
        .filter(|job| layout::imported(job))
        .map(|job| job.id.clone())
        .collect();
    let tips: BTreeSet<String> = store
        .library_jobs()
        .unwrap()
        .into_iter()
        .map(|job| job.id)
        .collect();
    assert_eq!(tips, library);
}

#[test]
fn every_legacy_format_migrates_as_snapshot_journal_or_both() {
    for format in 0..5 {
        for (snapshot, tail) in [(40, 0), (0, 5), (40, 5)] {
            let directory = Directory::new();
            let mut jobs: Vec<Job> = (0..snapshot + tail)
                .map(|n| {
                    if n % 7 == 0 {
                        ready_job(n as u64, 100 + n as u64)
                    } else {
                        raw_job(n as u64, 100 + n as u64)
                    }
                })
                .collect();
            for job in &mut jobs {
                job.updated_at = job.created_at;
            }
            write_legacy(&directory.0, &jobs[..snapshot], &jobs[snapshot..], format);
            let original = contents(&directory.0);
            let mut store = Store::open(&directory.0).unwrap();
            let report = store.migration.clone().unwrap();
            assert_eq!(report.jobs, jobs.len() as u64, "format {format}");
            assert!(!report.resumed);
            assert!(store.list().unwrap().len() == jobs.len());
            drop(store);
            expect_migrated(&directory.0, &jobs);
            // The exact legacy bytes are preserved, private and named by digest.
            let after = contents(&directory.0);
            for (path, (bytes, _)) in &original {
                let name = path.file_name().unwrap().to_str().unwrap();
                let preserved = after
                    .iter()
                    .find(|(found, _)| {
                        found.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                            n.contains("legacy-")
                                && n.starts_with(&name[..name.find('.').unwrap_or(name.len())])
                        })
                    })
                    .unwrap_or_else(|| panic!("{name} was not preserved"));
                assert_eq!(&preserved.1.0, bytes, "format {format}");
                assert_eq!(preserved.1.1 & 0o077, 0);
            }
            // Older versions find an unknown snapshot format and an empty journal.
            let marker = fs::read(directory.0.join("snapshot.bin")).unwrap();
            assert_eq!(&marker[..8], legacy::MARKER_MAGIC);
            assert!(
                legacy::parse(Some(&marker), &[])
                    .unwrap_err()
                    .contains("format")
            );
            assert_eq!(
                fs::metadata(directory.0.join("journal.bin")).map_or(0, |m| m.len()),
                0
            );
            // Reopening is idempotent and a read-only open writes nothing.
            let before = contents(&directory.0);
            let mut read_only = Store::open_read_only(&directory.0).unwrap();
            assert_eq!(read_only.counts().unwrap().jobs, jobs.len() as u64);
            drop(read_only);
            assert_eq!(contents(&directory.0), before);
            let reopened = Store::open(&directory.0).unwrap();
            assert!(reopened.migration.is_none());
        }
    }
}

#[test]
fn legacy_events_become_per_job_history_and_numbering_continues() {
    let directory = Directory::new();
    let jobs: Vec<Job> = (0..3).map(|n| raw_job(n, 100 + n)).collect();
    let mut tail = jobs[0].clone();
    tail.progress = 0.5;
    tail.updated_at = 500;
    write_legacy(&directory.0, &jobs, &[tail.clone()], 0);
    let mut store = Store::open(&directory.0).unwrap();
    let history = store.events(&jobs[0].id).unwrap();
    assert_eq!(history.iter().map(|e| e.id).collect::<Vec<_>>(), vec![1, 4]);
    assert_eq!(store.get(&jobs[0].id).unwrap().unwrap().progress, 0.5);
    assert_eq!(store.events("").unwrap().len(), 4);
    let created = store.submit(request("After migration")).unwrap();
    assert_eq!(store.events(&created.id).unwrap()[0].id, 5);
}

#[test]
fn invalid_legacy_state_is_refused_without_writing_anything() {
    let good: Vec<Job> = (0..4).map(|n| raw_job(n, 100 + n)).collect();
    type Damage = Box<dyn Fn(&Path)>;
    let cases: Vec<(&str, Damage)> = vec![
        (
            "checksum",
            Box::new(|dir: &Path| {
                let mut bytes = fs::read(dir.join("snapshot.bin")).unwrap();
                bytes[30] ^= 1;
                private(&dir.join("snapshot.bin"), &bytes);
            }),
        ),
        (
            "journal frame",
            Box::new(|dir: &Path| {
                let mut bytes = fs::read(dir.join("journal.bin")).unwrap();
                bytes[100] ^= 1;
                private(&dir.join("journal.bin"), &bytes);
            }),
        ),
        (
            "journal header",
            Box::new(|dir: &Path| {
                let mut bytes = fs::read(dir.join("journal.bin")).unwrap();
                bytes[18] ^= 0x08;
                private(&dir.join("journal.bin"), &bytes);
            }),
        ),
        (
            "snapshot length",
            Box::new(|dir: &Path| {
                let mut bytes = fs::read(dir.join("snapshot.bin")).unwrap();
                bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
                private(&dir.join("snapshot.bin"), &bytes);
            }),
        ),
        (
            "public permissions on the journal",
            Box::new(|dir: &Path| {
                // A legacy file must be private: this one is repaired on a
                // writable open only when it is otherwise valid, never read
                // from a link.
                let target = dir.join("elsewhere.bin");
                fs::copy(dir.join("journal.bin"), &target).unwrap();
                fs::remove_file(dir.join("journal.bin")).unwrap();
                std::os::unix::fs::symlink(&target, dir.join("journal.bin")).unwrap();
            }),
        ),
    ];
    for (name, damage) in cases {
        let directory = Directory::new();
        write_legacy(&directory.0, &good[..2], &good[2..], 0);
        damage(&directory.0);
        let before = contents(&directory.0);
        let error = Store::open(&directory.0)
            .err()
            .unwrap_or_else(|| panic!("{name} opened"));
        assert!(!error.contains(directory.0.to_str().unwrap()), "{error}");
        let after = contents(&directory.0);
        let extra: Vec<_> = after
            .keys()
            .filter(|key| !before.contains_key(*key))
            .collect();
        assert!(
            extra
                .iter()
                .all(|path| path.file_name().is_some_and(|n| n == ".lock")),
            "{name} wrote {extra:?}"
        );
        assert!(!directory.0.join("jobs.idx").exists(), "{name}");
        assert!(!directory.0.join("jobs.idx.migrating").exists(), "{name}");
    }
    // State rules the old store enforced on every transaction.
    let mut duplicate = good.clone();
    duplicate[1].key = duplicate[0].key.clone();
    let mut uppercase = good.clone();
    uppercase[0].id = uppercase[0].id.to_uppercase();
    uppercase[0].id.replace_range(..1, "A");
    let mut oversized = good.clone();
    oversized[0].files = vec!["x".repeat(legacy::MAX_RECORD)];
    let mut orphan = good.clone();
    orphan[1].upgrade_parent = Some("f".repeat(32));
    orphan[1].release = Some(RecordedRelease {
        title: "x".into(),
        profile: "hd".into(),
    });
    for (name, jobs) in [
        ("duplicate key", duplicate),
        ("uppercase identifier", uppercase),
        ("oversized record", oversized),
        ("orphan upgrade", orphan),
    ] {
        let directory = Directory::new();
        write_legacy(&directory.0, &jobs, &[], 0);
        let before = contents(&directory.0);
        let error = Store::open(&directory.0)
            .err()
            .unwrap_or_else(|| panic!("{name} opened"));
        assert!(!error.is_empty());
        let after = contents(&directory.0);
        assert!(
            after
                .keys()
                .filter(|key| !before.contains_key(*key))
                .all(|path| path.file_name().is_some_and(|n| n == ".lock")),
            "{name}"
        );
        assert!(!directory.0.join("jobs.idx").exists(), "{name}");
    }
    // Formats too old for what they hold are refused instead of reinterpreted.
    let directory = Directory::new();
    let mut job = raw_job(1, 100);
    job.shared_file = Some(shared_file());
    write_legacy(&directory.0, std::slice::from_ref(&job), &[], 0);
    assert!(Store::open(&directory.0).is_err());
}

#[test]
fn read_only_open_never_migrates_or_writes() {
    let directory = Directory::new();
    let jobs: Vec<Job> = (0..6).map(|n| raw_job(n, 100 + n)).collect();
    write_legacy(&directory.0, &jobs, &[], 1);
    private(&directory.0.join(".lock"), b"");
    let before = contents(&directory.0);
    let modified = fs::metadata(directory.0.join("snapshot.bin"))
        .unwrap()
        .modified()
        .unwrap();
    let mut store = Store::open_read_only(&directory.0).unwrap();
    assert!(store.migration.is_none());
    for error in [
        store.get(&jobs[0].id).unwrap_err(),
        store.page(None, 10).map(|_| ()).unwrap_err(),
        store.counts().map(|_| ()).unwrap_err(),
        store.submit(request("Forbidden")).map(|_| ()).unwrap_err(),
    ] {
        assert!(
            error.contains("migrated") || error.contains("read-only"),
            "{error}"
        );
        assert!(!error.contains(directory.0.to_str().unwrap()));
    }
    assert_eq!(
        store.describe().get("state").and_then(Value::as_str),
        Some("needs_migration")
    );
    drop(store);
    assert_eq!(contents(&directory.0), before);
    assert_eq!(
        fs::metadata(directory.0.join("snapshot.bin"))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
    // An empty directory reads as an empty store, and creates nothing.
    let empty = Directory::new();
    private(&empty.0.join(".lock"), b"");
    let mut store = Store::open_read_only(&empty.0).unwrap();
    assert_eq!(store.counts().unwrap().jobs, 0);
    assert!(store.page(None, 10).unwrap().jobs.is_empty());
    drop(store);
    assert_eq!(fs::read_dir(&empty.0).unwrap().count(), 1);
}

/// Opens with a hook that interrupts at one migration stage.
fn interrupted(directory: &Path, mut stop: impl FnMut(Stage) -> bool) -> Result<Store> {
    let lock = secure_file(&directory.join(".lock"))?;
    try_lock_file(&lock).map_err(|error| error.to_string())?;
    migration::open_with(directory, lock, false, &mut |stage| {
        if stop(stage) {
            Err(format!("interrupted at {stage:?}"))
        } else {
            Ok(())
        }
    })
}

#[test]
fn migration_resumes_after_an_interruption_at_every_stage() {
    // 150 jobs are three chunks; the legacy files hold a snapshot and a tail.
    let jobs: Vec<Job> = (0..150)
        .map(|n| {
            if n % 5 == 0 {
                ready_job(n, 100 + n)
            } else {
                raw_job(n, 100 + n)
            }
        })
        .collect();
    let stages = [
        Stage::BackupWritten,
        Stage::StagingCreated,
        Stage::Chunk(1),
        Stage::Chunk(2),
        Stage::Chunk(3),
        Stage::Verified,
        Stage::Promoted,
        Stage::MarkerWritten,
        Stage::JournalCleared,
    ];
    for stop in stages {
        let directory = Directory::new();
        write_legacy(&directory.0, &jobs[..140], &jobs[140..], 0);
        let original = fs::read(directory.0.join("snapshot.bin")).unwrap();
        let error = interrupted(&directory.0, |stage| stage == stop)
            .err()
            .unwrap_or_else(|| panic!("{stop:?} did not interrupt"));
        assert!(error.contains("interrupted"), "{error}");
        // Until the marker replaces it, the legacy snapshot is untouched, so an
        // older version could still read it.
        if !matches!(stop, Stage::MarkerWritten | Stage::JournalCleared) {
            assert_eq!(
                fs::read(directory.0.join("snapshot.bin")).unwrap(),
                original
            );
        }
        let resumed = Store::open(&directory.0).unwrap();
        let report = resumed.migration.clone();
        drop(resumed);
        match stop {
            Stage::StagingCreated | Stage::Chunk(_) | Stage::Verified => {
                assert!(report.as_ref().unwrap().resumed, "{stop:?}");
            }
            Stage::BackupWritten => assert!(!report.unwrap().resumed),
            _ => {}
        }
        expect_migrated(&directory.0, &jobs);
        let mut store = Store::open(&directory.0).unwrap();
        assert!(store.migration.is_none());
        assert_eq!(
            fs::metadata(directory.0.join("journal.bin")).map_or(0, |m| m.len()),
            0
        );
        assert!(store.submit(request("After the migration")).is_ok());
    }
}

#[test]
fn a_staging_index_from_other_bytes_is_rebuilt_and_foreign_copies_are_never_replaced() {
    let jobs: Vec<Job> = (0..70).map(|n| raw_job(n, 100 + n)).collect();
    let directory = Directory::new();
    write_legacy(&directory.0, &jobs, &[], 0);
    interrupted(&directory.0, |stage| stage == Stage::Chunk(1))
        .err()
        .unwrap();
    // The legacy files change before the next start: the staging index came
    // from other bytes and is derived data, so it is rebuilt.
    let mut changed = jobs.clone();
    changed[3].progress = 0.25;
    fs::remove_file(directory.0.join("snapshot.bin")).unwrap();
    write_legacy(&directory.0, &changed, &[], 0);
    expect_migrated(&directory.0, &changed);
    // A preserved copy with other content is never overwritten.
    let other = Directory::new();
    write_legacy(&other.0, &jobs, &[], 0);
    let digest = match legacy::inspect(&other.0, false).unwrap() {
        legacy::Inspection::Legacy(files) => files.digest,
        _ => unreachable!(),
    };
    let name = format!("snapshot.legacy-{}.bin", hex(&digest[..8]));
    private(&other.0.join(&name), b"someone else's data");
    let before = contents(&other.0);
    let error = Store::open(&other.0).err().unwrap();
    assert!(error.contains("refusing to overwrite"), "{error}");
    assert_eq!(
        fs::read(other.0.join(&name)).unwrap(),
        b"someone else's data"
    );
    assert!(!other.0.join("jobs.idx").exists());
    assert!(
        contents(&other.0)
            .keys()
            .all(|key| before.contains_key(key) || key.file_name().is_some_and(|n| n == ".lock"))
    );
}

#[test]
fn restored_legacy_files_are_accepted_only_when_they_match_the_index() {
    let directory = Directory::new();
    let jobs: Vec<Job> = (0..8).map(|n| raw_job(n, 100 + n)).collect();
    write_legacy(&directory.0, &jobs, &[], 0);
    let original = fs::read(directory.0.join("snapshot.bin")).unwrap();
    let mut store = Store::open(&directory.0).unwrap();
    store.submit(request("Added after migration")).unwrap();
    drop(store);
    // Restoring the identical snapshot over the marker (a crash or a copy)
    // is accepted: the index is its product, and later changes remain.
    private(&directory.0.join("snapshot.bin"), &original);
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(store.counts().unwrap().jobs, 9);
    assert_eq!(
        &fs::read(directory.0.join("snapshot.bin")).unwrap()[..8],
        legacy::MARKER_MAGIC
    );
    drop(store);
    // Different legacy files beside the index are refused, never merged.
    let mut changed = jobs.clone();
    changed[0].progress = 0.75;
    fs::remove_file(directory.0.join("snapshot.bin")).unwrap();
    write_legacy(&directory.0, &changed, &[], 0);
    let error = Store::open(&directory.0).err().unwrap();
    assert!(
        error.contains("differ from the files that produced"),
        "{error}"
    );
    // A marker bound to a different index is refused too.
    let elsewhere = Directory::new();
    let other = Store::open(&elsewhere.0).unwrap();
    drop(other);
    fs::copy(
        elsewhere.0.join("snapshot.bin"),
        directory.0.join("snapshot.bin"),
    )
    .unwrap();
    let error = Store::open(&directory.0).err().unwrap();
    assert!(error.contains("different job index"), "{error}");
}

#[test]
fn downgrade_restores_the_preserved_files_without_later_changes() {
    let directory = Directory::new();
    let jobs: Vec<Job> = (0..5).map(|n| raw_job(n, 100 + n)).collect();
    write_legacy(&directory.0, &jobs[..3], &jobs[3..], 0);
    let mut store = Store::open(&directory.0).unwrap();
    let later = store.submit(request("Only in the index")).unwrap();
    let backup = store.migration.clone().unwrap().backup;
    drop(store);
    let snapshot = fs::read(directory.0.join(format!("snapshot.{backup}.bin"))).unwrap();
    let journal = fs::read(directory.0.join(format!("journal.{backup}.bin"))).unwrap();
    let state = legacy::parse(Some(&snapshot), &journal).unwrap();
    legacy::validate(&state).unwrap();
    assert_eq!(state.jobs.len(), 5);
    assert!(!state.jobs.contains_key(&later.id));
    assert_eq!(state.jobs.values().cloned().collect::<Vec<_>>(), jobs);
}

#[test]
fn a_journal_left_after_the_marker_is_emptied_only_when_it_matches_the_preserved_copy() {
    let jobs: Vec<Job> = (0..3).map(|n| raw_job(n, 100 + n)).collect();
    let directory = Directory::new();
    write_legacy(&directory.0, &[], &jobs, 0);
    interrupted(&directory.0, |stage| stage == Stage::MarkerWritten)
        .err()
        .unwrap();
    assert!(fs::metadata(directory.0.join("journal.bin")).unwrap().len() > 0);
    // This interruption precedes the scheduling upgrade: read-only opening
    // refuses the required upgrade and writes nothing; writable opening first
    // settles the matching journal and then finishes the derived index.
    let before = contents(&directory.0);
    let error = Store::open_read_only(&directory.0).err().unwrap();
    assert!(error.contains("writable restart"), "{error}");
    assert_eq!(contents(&directory.0), before);
    drop(Store::open(&directory.0).unwrap());
    let before = contents(&directory.0);
    drop(Store::open_read_only(&directory.0).unwrap());
    assert_eq!(contents(&directory.0), before);
    assert_eq!(
        fs::metadata(directory.0.join("journal.bin")).unwrap().len(),
        0
    );
    // Content that is not the preserved copy is refused and kept.
    private(&directory.0.join("journal.bin"), b"unexpected content");
    let error = Store::open(&directory.0).err().unwrap();
    assert!(error.contains("not in the preserved copy"), "{error}");
    assert_eq!(
        fs::read(directory.0.join("journal.bin")).unwrap(),
        b"unexpected content"
    );
}

/// A leftover journal swapped after its inspection is refused before it is
/// emptied, so no other file is ever truncated.
#[test]
fn a_journal_swapped_before_it_is_emptied_is_refused_and_kept() {
    use super::open_tests::{Swap, swapped};
    for swap in [Swap::Symlink, Swap::Fifo, Swap::Replaced] {
        let jobs: Vec<Job> = (0..3).map(|n| raw_job(n, 100 + n)).collect();
        let directory = Directory::new();
        write_legacy(&directory.0, &[], &jobs, 0);
        interrupted(&directory.0, |stage| stage == Stage::MarkerWritten)
            .err()
            .unwrap();
        let journal = directory.0.join("journal.bin");
        let bytes = fs::read(&journal).unwrap();
        assert!(!bytes.is_empty());
        let path = directory.0.clone();
        let error = swapped("journal.bin", 0, swap, move || Store::open(&path).map(drop));
        let expected = match swap {
            Swap::Symlink => "Cannot open the legacy journal",
            Swap::Fifo => "The legacy journal must be a regular file",
            Swap::Replaced => "The legacy journal changed while opening",
        };
        assert!(error.contains(expected), "{swap:?}: {error}");
        assert_eq!(
            fs::read(directory.0.join("journal.bin.moved")).unwrap(),
            bytes
        );
        if matches!(swap, Swap::Replaced) {
            assert_eq!(fs::read(&journal).unwrap(), bytes);
        }
    }
}

/// Interrupts a migration after its marker, then reopens the store with
/// `change` applied to the leftover journal between its inspection and its
/// open for emptying. Returns the directory, the journal's bytes before
/// `change` and the open error.
fn settle_changed(change: fn(&Path)) -> (Directory, Vec<u8>, String) {
    let jobs: Vec<Job> = (0..3).map(|n| raw_job(n, 100 + n)).collect();
    let directory = Directory::new();
    write_legacy(&directory.0, &[], &jobs, 0);
    interrupted(&directory.0, |stage| stage == Stage::MarkerWritten)
        .err()
        .unwrap();
    let bytes = fs::read(directory.0.join("journal.bin")).unwrap();
    assert!(!bytes.is_empty());
    let mut pending = true;
    BEFORE_PRIVATE_OPEN.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |path: &Path| {
            if pending && path.file_name().and_then(|name| name.to_str()) == Some("journal.bin") {
                pending = false;
                change(path);
            }
        }));
    });
    let opened = Store::open(&directory.0);
    BEFORE_PRIVATE_OPEN.with(|hook| *hook.borrow_mut() = None);
    let error = opened.err().expect("a changed journal was settled");
    (directory, bytes, error)
}

/// A second link to the leftover journal is never emptied through it, even
/// when it appears after the journal was read and inspected.
#[test]
fn a_journal_linked_before_it_is_emptied_is_refused_and_kept() {
    let (directory, bytes, error) = settle_changed(|path| {
        fs::hard_link(path, path.with_file_name("journal.link")).unwrap();
    });
    assert_eq!(error, "The legacy journal changed while opening");
    for name in ["journal.bin", "journal.link"] {
        assert_eq!(fs::read(directory.0.join(name)).unwrap(), bytes, "{name}");
    }
}

/// The emptied bytes are read again through the truncating descriptor, so
/// content written in place after the first read is refused and kept.
#[test]
fn a_journal_rewritten_before_it_is_emptied_is_refused_and_kept() {
    let (directory, bytes, error) = settle_changed(|path| {
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        std::io::Write::write_all(&mut file, b"rewritten").unwrap();
    });
    assert_eq!(error, "The legacy journal changed while it was settled");
    let mut expected = b"rewritten".to_vec();
    expected.extend_from_slice(&bytes[expected.len()..]);
    assert_eq!(fs::read(directory.0.join("journal.bin")).unwrap(), expected);
}

#[test]
fn a_store_that_never_held_jobs_is_created_atomically() {
    let directory = Directory::new();
    let path = directory.0.join("fresh");
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.counts().unwrap().jobs, 0);
    assert_eq!(
        store.describe().get("state").and_then(Value::as_str),
        Some("index")
    );
    assert!(path.join("jobs.idx").is_dir());
    assert_eq!(
        &fs::read(path.join("snapshot.bin")).unwrap()[..8],
        legacy::MARKER_MAGIC
    );
    assert!(!path.join("jobs.idx.migrating").exists());
    drop(store);
    // A foreign public staging directory is refused and never removed.
    fs::create_dir(path.join("jobs.idx.migrating")).unwrap();
    fs::write(path.join("jobs.idx.migrating").join("junk"), b"x").unwrap();
    fs::remove_dir_all(path.join("jobs.idx")).unwrap();
    fs::remove_file(path.join("snapshot.bin")).unwrap();
    assert!(Store::open(&path).is_err());
    assert_eq!(
        fs::read(path.join("jobs.idx.migrating/junk")).unwrap(),
        b"x"
    );
    fs::rename(
        path.join("jobs.idx.migrating"),
        path.join("foreign-staging-kept"),
    )
    .unwrap();
    assert!(Store::open(&path).is_ok());
}

#[test]
fn no_error_text_contains_a_store_path_or_a_secret() {
    let directory = Directory::new();
    let jobs: Vec<Job> = (0..3).map(|n| raw_job(n, 100 + n)).collect();
    write_legacy(&directory.0, &jobs, &[], 0);
    let mut bytes = fs::read(directory.0.join("snapshot.bin")).unwrap();
    bytes.truncate(bytes.len() - 5);
    private(&directory.0.join("snapshot.bin"), &bytes);
    let path = directory.0.to_str().unwrap().to_owned();
    for error in [
        Store::open(&directory.0).err().unwrap(),
        Store::open_read_only(&directory.0)
            .err()
            .unwrap_or_default(),
    ] {
        assert!(!error.contains(&path), "{error}");
    }
}

#[test]
fn migration_preserves_a_thousand_maximum_messages_and_large_legacy_jobs() {
    let directory = Directory::new();
    let mut job = raw_job(1, 100);
    job.files = vec!["x".repeat(1_100_000)];
    let events: Vec<Event> = (1..=1_000)
        .map(|id| Event {
            id,
            job_id: job.id.clone(),
            state: job.state.clone(),
            message: "x".repeat(MAX_EVENT_MESSAGE),
            at: job.updated_at,
        })
        .collect();
    private(
        &directory.0.join("snapshot.bin"),
        &fixture::snapshot(
            std::slice::from_ref(&job),
            &events,
            1_000,
            [0; 32],
            legacy::SNAPSHOT_MAGIC,
        ),
    );
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(store.get(&job.id).unwrap().unwrap(), job);
    assert_eq!(store.events(&job.id).unwrap(), events);
    assert_eq!(store.migration.as_ref().unwrap().events, 1_000);
    job.progress = 0.5;
    store.update(job.clone()).unwrap();
    let history = store.events(&job.id).unwrap();
    assert_eq!(history.len(), 1_000);
    assert_eq!(history.first().unwrap().id, 2);
    assert_eq!(history.last().unwrap().id, 1_001);
    job = store.get(&job.id).unwrap().unwrap();
    drop(store);
    let mut reopened = Store::open_read_only(&directory.0).unwrap();
    assert_eq!(reopened.get(&job.id).unwrap().unwrap(), job);
    assert_eq!(reopened.events(&job.id).unwrap(), history);
}

#[test]
fn complete_checksummed_illegal_legacy_transitions_are_refused() {
    for ready in [false, true] {
        let directory = Directory::new();
        let job = if ready {
            ready_job(1, 100)
        } else {
            raw_job(1, 100)
        };
        let mut changed = job.clone();
        if ready {
            changed.state = "failed".into();
        } else {
            changed.created_at += 1;
        }
        write_legacy(&directory.0, std::slice::from_ref(&job), &[changed], 0);
        private(&directory.0.join(".lock"), b"");
        let before = contents(&directory.0);
        assert!(Store::open(&directory.0).is_err());
        assert_eq!(contents(&directory.0), before);
    }
}

#[test]
fn ownership_and_media_checks_reach_beyond_the_first_page() {
    let directory = Directory::new();
    let transfer = "a".repeat(40);
    let mut jobs: Vec<Job> = (0..205)
        .map(|n| {
            let mut job = raw_job(n, 100 + n);
            job.download_id = Some(transfer.clone());
            job.state = "cancelled".into();
            job
        })
        .collect();
    jobs[204].state = "queued".into();
    let mut store = Store::open(&directory.0).unwrap();
    bulk(&mut store, jobs.clone());
    assert_eq!(store.transfer_jobs(&transfer).unwrap().len(), 205);
    assert!(store.transfer_has_owner(&transfer).unwrap());
    assert!(store.transfer_claimed(&transfer, None).unwrap());
    assert!(store.has_co_owner(&jobs[0]).unwrap());
    jobs[204].state = "cancelled".into();
    store.update(jobs[204].clone()).unwrap();
    assert!(!store.transfer_has_owner(&transfer).unwrap());
    assert!(!store.has_co_owner(&jobs[0]).unwrap());
    let media = request("One media");
    let same: Vec<Job> = (300..505)
        .map(|n| {
            let mut job = raw_job(n, 100 + n);
            job.request = media.clone();
            job.request.source_url = Some(format!("http://127.0.0.1/{n}"));
            job.key = job.request.canonical_key();
            if n == 504 {
                job.state = "ready".into();
                job.imports = vec!["/library/current.mkv".into()];
                job.progress = 1.0;
            }
            job
        })
        .collect();
    bulk(&mut store, same.clone());
    let key = media.media_key();
    assert_eq!(store.media_jobs(&key).unwrap().len(), 205);
    assert_eq!(
        store.states_for(&BTreeSet::from([key.clone()])).unwrap()[&key].id,
        same[204].id
    );
}

#[test]
fn captured_feed_routes_reach_jobs_and_library_beyond_the_former_prefix() {
    use crate::{
        engine::{Engine, lock},
        indexers::feed::generation::Generation,
        integrations::{FeedEntry, Release},
    };
    for library in [false, true] {
        let directory = Directory::new();
        let imported = directory.0.join("synthetic-import.mp4");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo.mp4"),
            &imported,
        )
        .unwrap();
        let mut store = Store::open(&directory.0).unwrap();
        let mut target = if library {
            ready_job(1, 1000)
        } else {
            raw_job(1, 1000)
        };
        if library {
            target.imports = vec![imported.to_str().unwrap().into()];
            target.release = Some(RecordedRelease {
                title: "Synthetic.Movie.1.2026.720p.WEB-DL.EN".into(),
                profile: "hd".into(),
            });
        }
        let mut jobs = vec![target.clone()];
        jobs.extend((2..=1002).map(|n| {
            if library {
                ready_job(n, 2000 + n)
            } else {
                raw_job(n, 2000 + n)
            }
        }));
        bulk(&mut store, jobs);
        let prefix = if library {
            store.library_page(None, 200).unwrap()
        } else {
            store.page(None, 200).unwrap()
        };
        assert!(prefix.jobs.iter().all(|row| row.id != target.id));
        drop(store);
        let mut value = crate::config::default_json();
        value.insert("store_dir", directory.0.to_str().unwrap());
        value.insert("indexers",Value::Array(vec![crate::json::parse(r#"{"id":"capture","name":"Synthetic","kind":"rss","url":"http://127.0.0.1:9/feed","api_key_env":"MYNOU_ABSENT_FEED_FIXTURE_KEY_110","watch":{"enabled":true,"interval_secs":60}}"#).unwrap()]));
        value.insert(
            "monitoring",
            crate::json::parse(&format!(
                "{{\"enabled\":{library},\"interval_secs\":3600,\"max_checks\":8}}"
            ))
            .unwrap(),
        );
        // The default profile ranks every resolution alike; an upgrade needs an order.
        value.insert(
            "selection",
            crate::json::parse(
                r#"{"movie_profile":"hd","episode_profile":"hd","profiles":{"hd":{"resolutions":[1080,720],"languages":["en"]}}}"#,
            )
            .unwrap(),
        );
        let config = crate::config::from_json(&value, &directory.0).unwrap();
        let engine = Engine::open_for_management(config).unwrap();
        let revision = lock(&engine.indexer_store)
            .unwrap()
            .authorization("capture")
            .unwrap()
            .1;
        let url = format!("magnet:?xt=urn:btih:{}", "a".repeat(40));
        // The second round replays the same candidate after its effect is durable.
        for _ in 0..2 {
            let mut feeds = lock(&engine.feed_store).unwrap();
            let mut record = feeds.row("capture").unwrap().unwrap();
            record.baselined = true;
            record.revision += 1;
            record.seen = vec!["a".repeat(32)];
            record.cursor = record.seen[0].clone();
            let generation = Generation::new(
                &record,
                revision,
                vec![FeedEntry {
                    id: "a".repeat(32),
                    release: Release {
                        title: "Synthetic.Movie.1.2026.1080p.BluRay.EN".into(),
                        url: url.clone(),
                        seeders: 5,
                    },
                }],
                now(),
            )
            .unwrap();
            feeds.capture(record, generation).unwrap();
            drop(feeds);
            let mut passes = 0;
            while lock(&engine.feed_store)
                .unwrap()
                .pending_header("capture")
                .unwrap()
                .is_some()
            {
                assert!(passes < 16);
                let report = engine.feeds_poll(None, true).unwrap();
                let row = &report.get("sources").unwrap().as_array().unwrap()[0];
                assert_eq!(row.get("outcome").and_then(Value::as_str), Some("ok"));
                assert!(row.get("routing_examined").unwrap().as_u64().unwrap() <= 200);
                passes += 1;
            }
            assert!(passes > 5);
        }
        let mut store = lock(&engine.store).unwrap();
        if library {
            assert_eq!(store.counts().unwrap().jobs, 1003);
            let children: Vec<_> = store
                .list()
                .unwrap()
                .into_iter()
                .filter(|row| row.upgrade_parent.as_deref() == Some(target.id.as_str()))
                .collect();
            assert_eq!(children.len(), 1);
            assert_eq!(
                children[0].request.source_url.as_deref(),
                Some(url.as_str())
            );
        } else {
            assert_eq!(
                store.get(&target.id).unwrap().unwrap().acquisition_url,
                Some(url)
            );
        }
    }
}

#[test]
fn interrupted_staging_commits_are_preserved_and_rebuilt_without_foreign_data_loss() {
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
    ] {
        let directory = Directory::new();
        let jobs = vec![raw_job(1, 100)];
        write_legacy(&directory.0, &jobs, &[], 0);
        interrupted(&directory.0, |stage| stage == Stage::StagingCreated)
            .err()
            .unwrap();
        let path = directory.0.join("jobs.idx.migrating");
        let mut index = Index::open(&path, false).unwrap();
        index.inject(Some(fault));
        assert!(
            index
                .commit(vec![Change::Put(
                    layout::job_key(&[9; 16]),
                    b"partial fixture".to_vec()
                )])
                .is_err()
        );
        drop(index);
        private(&path.join("foreign.txt"), b"retain this unrelated file");
        expect_migrated(&directory.0, &jobs);
        let preserved = fs::read_dir(&directory.0)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("jobs.idx.abandoned-"))
                    .then_some(path)
            })
            .next()
            .unwrap();
        assert_eq!(
            fs::read(preserved.join("foreign.txt")).unwrap(),
            b"retain this unrelated file"
        );
    }
}

#[test]
fn hydrated_page_byte_limits_continue_without_skipping_large_jobs() {
    let directory = Directory::new();
    let jobs: Vec<Job> = (0..3)
        .map(|n| {
            let mut job = raw_job(n, 100 + n);
            job.files = vec!["x".repeat(2_100_000)];
            job
        })
        .collect();
    let mut store = Store::open(&directory.0).unwrap();
    bulk(&mut store, jobs.clone());
    let mut seen = BTreeSet::new();
    let mut cursor = None;
    loop {
        let page = store.page(cursor.as_deref(), 200).unwrap();
        assert_eq!(page.jobs.len(), 1);
        assert!(seen.insert(page.jobs[0].id.clone()));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, jobs.iter().map(|job| job.id.clone()).collect());
}

#[test]
fn extra_secondary_keys_in_a_clean_staging_index_prevent_promotion() {
    let directory = Directory::new();
    let jobs = vec![raw_job(1, 100)];
    write_legacy(&directory.0, &jobs, &[], 0);
    interrupted(&directory.0, |stage| stage == Stage::Verified)
        .err()
        .unwrap();
    let mut index = Index::open(&directory.0.join("jobs.idx.migrating"), false).unwrap();
    index
        .commit(vec![Change::PutInline(
            layout::key(&[&[layout::T_TRANSFER], &[7; 16], &[8; 16]]),
            Vec::new(),
        )])
        .unwrap();
    drop(index);
    let error = Store::open(&directory.0).err().unwrap();
    assert!(error.contains("an index holds entries"), "{error}");
    assert!(!directory.0.join("jobs.idx").exists());
    assert!(directory.0.join("snapshot.bin").exists());
}

#[test]
fn transfer_reference_pages_reach_every_owner() {
    let directory = Directory::new();
    let transfer = "b".repeat(40);
    let jobs: Vec<Job> = (0..205)
        .map(|n| {
            let mut job = raw_job(n, 100 + n);
            job.download_id = Some(transfer.clone());
            job
        })
        .collect();
    let mut store = Store::open(&directory.0).unwrap();
    bulk(&mut store, jobs.clone());
    let mut seen = BTreeSet::new();
    let mut after = None;
    loop {
        let (page, next) = store
            .transfer_jobs_page(&transfer, after.as_deref(), 200)
            .unwrap();
        assert!(!page.is_empty() && page.len() <= 200);
        for job in page {
            assert!(seen.insert(job.id));
        }
        after = next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(seen, jobs.iter().map(|job| job.id.clone()).collect());
}

#[test]
fn irc_origin_enumeration_returns_each_job_once_across_alias_pages() {
    let directory = Directory::new();
    let jobs: Vec<Job> = (1..=201_u64).map(irc_fixture_job).collect();
    let mut store = Store::open(&directory.0).unwrap();
    bulk(&mut store, jobs.clone());
    let mut seen = BTreeSet::new();
    let mut cursor = None;
    loop {
        let (page, next) = store.irc_origin_page(cursor, 200).unwrap();
        for job in page {
            assert!(seen.insert(job.id));
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, jobs.iter().map(|job| job.id.clone()).collect());
}

fn irc_fixture_job(n: u64) -> Job {
    let mut job = raw_job(n, 100 + n);
    let torrent = format!("{n:040x}");
    let release = RecordedRelease {
        title: format!("Synthetic movie {n} 2026 1080p"),
        profile: "hd".into(),
    };
    let origin = crate::irc::Origin {
        announcement_id: format!("{n:064x}"),
        job_id: job.id.clone(),
        source_id: "fixture".into(),
        rule_id: "admitted".into(),
        binding: "a".repeat(64),
        fingerprint: "b".repeat(64),
        media_key: job.request.media_key(),
        torrent_id: torrent.clone(),
        torrent_aliases: vec![torrent.clone(), format!("{}{:024x}", "e".repeat(40), n)],
        magnet: format!("magnet:?xt=urn:btih:{torrent}&x.pe=127.0.0.1:9"),
        file: format!("Synthetic.movie.{n}.2026.1080p.mp4"),
        file_length: 100,
        release: release.clone(),
    };
    job.acquisition_url = Some(origin.magnet.clone());
    job.release = Some(release);
    origin.validate_job(&job).unwrap();
    job.irc_origin = Some(origin);
    job
}

#[test]
fn management_preflight_rejects_missing_irc_reservations_before_migration_writes() {
    let directory = Directory::new();
    let job = irc_fixture_job(1);
    write_legacy(&directory.0, &[job], &[], 4);
    private(&directory.0.join(".lock"), b"");
    let mut value = crate::config::default_json();
    value.insert("store_dir", directory.0.to_str().unwrap().to_owned());
    value.get_mut("downloads").unwrap().insert("enabled", false);
    let config = crate::config::from_json(&value, &directory.0).unwrap();
    let before = contents(&directory.0);
    let error = Store::prepare(&directory.0, &config).err().unwrap();
    assert!(
        error.contains("durable announcement reservation"),
        "{error}"
    );
    assert_eq!(contents(&directory.0), before);
    assert!(!directory.0.join("jobs.idx").exists());
    assert!(!directory.0.join("jobs.idx.migrating").exists());
}

fn remove_historical_library_fields(mut bytes: Vec<u8>, snapshot: bool) -> Vec<u8> {
    let at = if snapshot { 16 } else { 84 };
    let mut value =
        crate::json::parse(std::str::from_utf8(&bytes[at..bytes.len() - 32]).unwrap()).unwrap();
    let remove = |job: &mut Value| {
        let Value::Object(fields) = job else {
            panic!("Expected fixture job");
        };
        for field in [
            "release",
            "upgrade_parent",
            "monitored",
            "monitor_checked_at",
        ] {
            fields.remove(field);
        }
    };
    if snapshot {
        let Value::Array(jobs) = value.get_mut("jobs").unwrap() else {
            panic!("Expected fixture jobs");
        };
        for job in jobs {
            remove(job);
        }
    } else {
        remove(value.get_mut("job").unwrap());
    }
    let payload = crate::json::stringify(&value).into_bytes();
    if snapshot {
        bytes.truncate(8);
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    } else {
        bytes.truncate(52);
        bytes[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&crate::crypto::sha256(&bytes));
    }
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&crate::crypto::sha256(&bytes));
    bytes
}

#[test]
fn historical_missing_fields_and_ready_cancellation_replay_without_weakening_new_records() {
    for historical in [true, false] {
        let directory = Directory::new();
        let original = ready_job(1, 100);
        let mut cancelled = original.clone();
        cancelled.state = "cancelled".into();
        cancelled.updated_at = 101;
        let first = Event {
            id: 1,
            job_id: original.id.clone(),
            state: "ready".into(),
            message: "old import".into(),
            at: 100,
        };
        let second = Event {
            id: 2,
            job_id: original.id.clone(),
            state: "cancelled".into(),
            message: "old cancel".into(),
            at: 101,
        };
        let snapshot = fixture::snapshot(
            std::slice::from_ref(&original),
            &[first],
            1,
            [0; 32],
            legacy::SNAPSHOT_MAGIC,
        );
        let (frame, _) = fixture::frame(
            std::slice::from_ref(&cancelled),
            &second,
            2,
            &[0; 32],
            legacy::JOURNAL_MAGIC,
            None,
        );
        private(
            &directory.0.join("snapshot.bin"),
            &if historical {
                remove_historical_library_fields(snapshot, true)
            } else {
                snapshot
            },
        );
        private(
            &directory.0.join("journal.bin"),
            &if historical {
                remove_historical_library_fields(frame, false)
            } else {
                frame
            },
        );
        private(&directory.0.join(".lock"), b"");
        let before = contents(&directory.0);
        if historical {
            let mut store = Store::open(&directory.0).unwrap();
            assert_eq!(store.get(&original.id).unwrap().unwrap(), cancelled);
            assert_eq!(store.events(&original.id).unwrap().len(), 2);
        } else {
            let error = Store::open(&directory.0).err().unwrap();
            assert!(
                error.contains("ready import cannot change state"),
                "{error}"
            );
            assert_eq!(contents(&directory.0), before);
        }
    }
}

#[test]
fn cancellation_preserves_owners_bound_only_by_an_irc_alias() {
    let directory = Directory::new();
    let origin_job = irc_fixture_job(1);
    let mut cancelled = raw_job(2, 102);
    cancelled.download_id =
        Some(origin_job.irc_origin.as_ref().unwrap().torrent_aliases[1].clone());
    cancelled.state = "cancelled".into();
    let mut store = Store::open(&directory.0).unwrap();
    bulk(&mut store, vec![origin_job, cancelled.clone()]);
    assert!(store.has_co_owner(&cancelled).unwrap());
    assert!(
        store
            .transfer_has_owner(cancelled.download_id.as_deref().unwrap())
            .unwrap()
    );
}

fn child_fixture(number: u64, parent: &Job, imported: bool) -> Job {
    let mut child = if imported {
        ready_job(number, 5_000 + number)
    } else {
        raw_job(number, 5_000 + number)
    };
    child.upgrade_parent = Some(parent.id.clone());
    child
}

#[test]
fn imported_child_lookup_ignores_the_parents_other_children() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = monitored_fixture(1, 0);
    let bare = monitored_fixture(2, 0);
    // Parents are committed first: a child resolves its parent's lineage root
    // from the index, not from its own transaction.
    bulk(&mut store, vec![parent.clone(), bare.clone()]);
    let mut jobs = Vec::new();
    jobs.extend((10..310).map(|n| child_fixture(n, &parent, false)));
    jobs.extend((310..320).map(|n| child_fixture(n, &bare, false)));
    jobs.push(child_fixture(400, &parent, true));
    bulk(&mut store, jobs);
    let (parent_id, bare_id) = (
        layout::id_bytes(&parent.id).unwrap(),
        layout::id_bytes(&bare.id).unwrap(),
    );
    let id = |n: u64| layout::id_bytes(&format!("{n:032x}")).unwrap();
    store
        .mutate(|db| {
            let none = BTreeMap::new();
            assert!(db.replaced_by_child(&parent_id, &none)?);
            assert!(!db.replaced_by_child(&bare_id, &none)?);
            // The stored import is overridden away; a stored non-import is
            // overridden into one; an unrelated child changes nothing.
            let away = BTreeMap::from([(id(400), false)]);
            assert!(!db.replaced_by_child(&parent_id, &away)?);
            let into = BTreeMap::from([(id(400), false), (id(12), true)]);
            assert!(db.replaced_by_child(&parent_id, &into)?);
            let both = BTreeMap::from([(id(400), false), (id(12), false)]);
            assert!(!db.replaced_by_child(&parent_id, &both)?);
            let other = BTreeMap::from([(id(312), true)]);
            // Another parent's child never counts for this parent.
            let foreign = BTreeMap::from([(id(400), false), (id(312), true)]);
            assert!(!db.replaced_by_child(&parent_id, &foreign)?);
            assert!(db.replaced_by_child(&bare_id, &other)?);
            assert!(!db.replaced_by_child(&bare_id, &BTreeMap::from([(id(12), true)]))?);
            Ok(())
        })
        .unwrap();
}

#[test]
fn imported_child_index_upgrade_rebuilds_after_interruption_and_refuses_read_only() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = monitored_fixture(1, 0);
    bulk(&mut store, vec![parent.clone()]);
    let mut jobs = Vec::new();
    jobs.extend((10..230).map(|n| child_fixture(n, &parent, n % 2 == 0)));
    bulk(&mut store, jobs);
    drop(store);
    let count = |index: &mut Index| {
        let mut count = 0;
        let mut after = None;
        loop {
            let page = ops::Db(index)
                .scan(&[layout::T_IMPORTED_CHILD], after, 200)
                .unwrap();
            count += page.entries.len();
            after = page.next;
            if after.is_none() {
                return count;
            }
        }
    };
    let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
    assert_eq!(count(&mut index), 110);
    // Return to the previous format: drop every derived entry.
    let mut changes = vec![Change::PutInline(
        layout::FORMAT_KEY,
        monitoring::FORMAT.to_vec(),
    )];
    let mut after = None;
    loop {
        let page = ops::Db(&mut index)
            .scan(&[layout::T_IMPORTED_CHILD], after, 200)
            .unwrap();
        changes.extend(page.entries.iter().map(|(key, _)| Change::Delete(*key)));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    index.commit(changes).unwrap();
    assert_eq!(count(&mut index), 0);
    drop(index);
    let before = contents(&directory.0);
    let error = Store::open_read_only(&directory.0).err().unwrap();
    assert!(error.contains("writable restart"), "{error}");
    assert_eq!(contents(&directory.0), before);
    let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
    index.inject(Some(Fault::Applied));
    assert!(children::upgrade(&mut index, false, Path::new("/nonexistent")).is_err());
    assert!(!index.is_usable());
    drop(index);
    let reopened = Store::open(&directory.0).unwrap();
    drop(reopened);
    let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
    assert_eq!(
        index.get(&layout::FORMAT_KEY).unwrap().unwrap(),
        backlog::FORMAT
    );
    assert!(index.get(&layout::CHILD_CURSOR_KEY).unwrap().is_none());
    assert_eq!(count(&mut index), 110);
    drop(index);
    assert!(Store::open_read_only(&directory.0).is_ok());
}

#[test]
fn imported_child_upgrade_resumes_from_a_mid_upgrade_cursor() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let parent = monitored_fixture(1, 0);
    bulk(&mut store, vec![parent.clone()]);
    let jobs: Vec<Job> = (10..610).map(|n| child_fixture(n, &parent, true)).collect();
    bulk(&mut store, jobs);
    drop(store);
    let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
    let mut keys = Vec::new();
    let mut after = None;
    loop {
        let page = ops::Db(&mut index)
            .scan(&[layout::T_IMPORTED_CHILD], after, 200)
            .unwrap();
        keys.extend(page.entries.iter().map(|(key, _)| *key));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(keys.len(), 600);
    // A crash after the second page: its entries exist, the rest do not.
    let mut changes: Vec<Change> = keys[400..].iter().map(|key| Change::Delete(*key)).collect();
    let mut cursor = layout::key(&[&[layout::T_CHILD]]);
    cursor[1..17].copy_from_slice(&keys[399][1..17]);
    cursor[17..33].copy_from_slice(&keys[399][17..33]);
    changes.push(Change::PutInline(
        layout::FORMAT_KEY,
        children::PENDING.to_vec(),
    ));
    changes.push(Change::Put(layout::CHILD_CURSOR_KEY, cursor.to_vec()));
    index.commit(changes).unwrap();
    drop(index);
    assert!(Store::open_read_only(&directory.0).is_err());
    drop(Store::open(&directory.0).unwrap());
    let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
    let mut count = 0;
    let mut after = None;
    loop {
        let page = ops::Db(&mut index)
            .scan(&[layout::T_IMPORTED_CHILD], after, 200)
            .unwrap();
        count += page.entries.len();
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(count, 600);
    assert!(index.get(&layout::CHILD_CURSOR_KEY).unwrap().is_none());
    // A cursor outside the child table is refused, not skipped over.
    index
        .commit(vec![
            Change::PutInline(layout::FORMAT_KEY, children::PENDING.to_vec()),
            Change::Put(layout::CHILD_CURSOR_KEY, layout::key(&[&[0xff]]).to_vec()),
        ])
        .unwrap();
    assert!(children::upgrade(&mut index, false, Path::new("/nonexistent")).is_err());
}

#[test]
fn status_reports_held_and_failed_stores_without_waiting_or_failing() {
    use crate::engine::{Engine, lock};
    let directory = Directory::new();
    let mut value = crate::config::default_json();
    value.insert("store_dir", directory.0.to_str().unwrap());
    let config = crate::config::from_json(&value, &directory.0).unwrap();
    let engine = Engine::open_for_management(config).unwrap();
    engine.submit(request("Kept")).unwrap();
    let state = |status: &Value, store: &str| {
        let storage = status.get("storage").unwrap().get(store).unwrap();
        storage
            .get("state")
            .and_then(Value::as_str)
            .unwrap()
            .to_owned()
    };
    let status = engine.status().unwrap();
    assert_eq!(status.get("jobs").and_then(Value::as_u64), Some(1));
    assert_eq!(state(&status, "jobs"), "index");
    assert_eq!(
        status.get("storage_errors"),
        Some(&Value::Array(Vec::new()))
    );

    // A long operation holds the job and series stores: status answers at
    // once, with their readings unavailable and the stores marked busy.
    let held = (
        lock(&engine.store).unwrap(),
        lock(&engine.series_store).unwrap(),
    );
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::sync::Arc::clone(&engine);
    let waiting = std::thread::spawn(move || sender.send(reader.status()).unwrap());
    let status = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("status waited for a held store")
        .unwrap();
    waiting.join().unwrap();
    drop(held);
    for reading in ["jobs", "active", "ready", "monitored_series"] {
        assert_eq!(status.get(reading), Some(&Value::Null), "{reading}");
    }
    assert_eq!(state(&status, "jobs"), "busy");
    assert_eq!(state(&status, "series"), "busy");
    assert_eq!(
        status
            .get("inventory")
            .unwrap()
            .get("series")
            .unwrap()
            .get("state"),
        Some(&Value::from("busy"))
    );
    assert_eq!(
        status
            .get("series_storage")
            .and_then(|storage| storage.get("state"))
            .and_then(Value::as_str),
        Some("busy")
    );
    assert_eq!(state(&status, "requesters"), "index");

    // A failed write pauses the job store: status still answers, and its
    // storage report shows the change awaiting recovery.
    let index = directory.0.join("jobs.idx");
    fs::set_permissions(&index, fs::Permissions::from_mode(0o500)).unwrap();
    let outcome = engine.submit(request("Refused"));
    fs::set_permissions(&index, fs::Permissions::from_mode(0o700)).unwrap();
    if outcome.is_ok() {
        // Running as a user that ignores permissions (for example root).
        return;
    }
    let status = engine.status().unwrap();
    assert_eq!(status.get("jobs"), Some(&Value::Null));
    let jobs = status.get("storage").unwrap().get("jobs").unwrap();
    assert_eq!(
        jobs.get("commit_uncertain").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(jobs.get("state").and_then(Value::as_str), Some("index"));
    let Some(Value::Array(errors)) = status.get("storage_errors") else {
        panic!("storage_errors is not a list");
    };
    let error = errors[0].as_str().unwrap();
    assert!(error.starts_with("Job storage: "), "{error}");
    assert!(!error.contains(directory.0.to_str().unwrap()), "{error}");
    assert_eq!(state(&status, "series"), "empty");
}

#[test]
fn claim_scan_resumes_after_restart_and_reaches_past_new_old_entries() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    // Above the former 10,000-job limit, with an ineligible oldest prefix.
    bulk(
        &mut store,
        (1..=10_101).map(|n| raw_job(n, n + 100)).collect(),
    );
    let mut reached = false;
    // Allow one complete promotion phase plus one complete queue sweep.
    let max_turns = 10_101_u64.div_ceil(200) + 10_101_u64.div_ceil(CLAIM_SCAN as u64) + 2;
    for turn in 0..max_turns {
        let before = match &store.backend {
            Backend::Ready(index) => index.requests(),
            _ => unreachable!(),
        };
        let job = store
            .claim_filtered(50_000, 60, |job| Ok(job.id == format!("{:032x}", 10_101)))
            .unwrap();
        let requests = match &store.backend {
            Backend::Ready(index) => index.requests() - before,
            _ => unreachable!(),
        };
        assert!(store.claim_pass[1] <= 2);
        assert!(store.claim_pass[2] <= CLAIM_SCAN);
        assert!(requests <= 1024, "{requests} index requests in turn {turn}");
        if let Some(job) = job {
            assert_eq!(job.id, format!("{:032x}", 10_101));
            reached = true;
            break;
        }
        // New work before the cursor must wait for the next sweep rather
        // than displacing the older eligible tail on every restart.
        bulk(&mut store, vec![raw_job(20_000 + turn, 1)]);
        drop(store);
        store = Store::open(&directory.0).unwrap();
    }
    assert!(reached, "eligible tail was starved across restarts");
    // Successful lease and cursor reset are committed together.
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    assert!(
        store
            .read(None, |db| db
                .cursor(&layout::CLAIM_CURSOR_KEY, layout::T_QUEUE))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get(&format!("{:032x}", 10_101))
            .unwrap()
            .unwrap()
            .lease_id
            .is_some()
    );
}

#[test]
fn claim_cursor_is_checkpointed_at_most_once_per_interval() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    // Unclaimable jobs four scans long, ahead of the only eligible one.
    bulk(
        &mut store,
        (1..=1_001).map(|n| raw_job(n, n + 100)).collect(),
    );
    let at = 50_000;
    let durable = |store: &mut Store| {
        store
            .read(None, |db| {
                db.cursor(&layout::CLAIM_CURSOR_KEY, layout::T_QUEUE)
            })
            .unwrap()
    };
    // Promotion moves the backlog into the queue in its own transactions.
    loop {
        store.claim_filtered(at, 60, |_| Ok(false)).unwrap();
        if store.claim_pass[0] == 0 {
            break;
        }
    }
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    // A fresh process checkpoints its first attempt.
    assert!(
        store
            .claim_filtered(at, 60, |_| Ok(false))
            .unwrap()
            .is_none()
    );
    let first = store.claim_cursor.unwrap();
    assert!(first.is_some());
    assert_eq!(durable(&mut store), first);
    // Within the interval the position advances, and wraps, only in memory.
    let before = flushes();
    let mut second = None;
    for pass in 0..10 {
        assert!(
            store
                .claim_filtered(at + CLAIM_CHECKPOINT_SECS - 1, 60, |_| Ok(false))
                .unwrap()
                .is_none()
        );
        assert!(store.claim_pass[2] <= CLAIM_SCAN);
        if pass == 0 {
            second = store.claim_cursor.unwrap();
        }
    }
    assert_eq!(flushes() - before, 0, "claim attempts flushed storage");
    assert_eq!(durable(&mut store), first);
    assert_ne!(second, first);
    // A restart resumes from the checkpoint, not from the queue head.
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    let at = at + CLAIM_CHECKPOINT_SECS;
    store.claim_filtered(at, 60, |_| Ok(false)).unwrap();
    assert_eq!(store.claim_cursor.unwrap(), second);
    assert_eq!(durable(&mut store), second);
    for _ in 0..2 {
        store.claim_filtered(at + 1, 60, |_| Ok(false)).unwrap();
    }
    assert_eq!(durable(&mut store), second);
    // Once the interval elapses the live position is written again.
    store
        .claim_filtered(at + CLAIM_CHECKPOINT_SECS, 60, |_| Ok(false))
        .unwrap();
    assert_ne!(store.claim_cursor.unwrap(), second);
    assert_eq!(durable(&mut store), store.claim_cursor.unwrap());
    // The live position still reaches the eligible tail within one sweep,
    // and the lease clears the checkpoint in its own transaction.
    let target = format!("{:032x}", 1_001);
    let mut reached = false;
    for _ in 0..=1_001_usize.div_ceil(CLAIM_SCAN) {
        if let Some(job) = store
            .claim_filtered(at + CLAIM_CHECKPOINT_SECS + 1, 60, |job| {
                Ok(job.id == target)
            })
            .unwrap()
        {
            assert_eq!(job.id, target);
            reached = true;
            break;
        }
    }
    assert!(reached, "eligible tail was not reached within one sweep");
    assert_eq!(store.claim_cursor, Some(None));
    assert_eq!(durable(&mut store), None);
}

#[test]
fn requester_sweep_checkpoint_replays_errors_and_survives_restart() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    // The sweep indexes requester jobs. Build genuine provenance from the
    // default policy without needing an upstream peer.
    let config = crate::config::from_json(&crate::config::default_json(), &directory.0).unwrap();
    let capture = crate::requesters::Capture::new(
        &config,
        &crate::requesters::Policy::initial(&config),
        "movie",
    )
    .unwrap();
    let owned = |n, created| {
        let mut job = raw_job(n, created);
        job.requester = Some(crate::requesters::Provenance {
            account_id: "synthetic".into(),
            demand_id: crate::requesters::Demand::identity("synthetic", &job.request),
            policy_revision: 1,
            capture: capture.clone(),
        });
        job
    };
    bulk(
        &mut store,
        (1..=10_101).map(|n| owned(n, n + 100)).collect(),
    );
    let mut seen = BTreeSet::new();
    for turn in 0..60 {
        let before = match &store.backend {
            Backend::Ready(index) => index.requests(),
            _ => unreachable!(),
        };
        let (page, next) = store.sweep_requester_jobs(200).unwrap();
        let requests = match &store.backend {
            Backend::Ready(index) => index.requests() - before,
            _ => unreachable!(),
        };
        assert!(page.len() <= 200);
        assert!(requests <= 202, "{requests} sweep reads");
        if turn == 0 {
            // No acknowledgement models a failed ownership lookup. Reopen
            // must return the same page, not silently skip its cancellation.
            drop(store);
            store = Store::open(&directory.0).unwrap();
            assert_eq!(store.sweep_requester_jobs(200).unwrap().0, page);
        }
        seen.extend(page.iter().map(|job| job.id.clone()));
        store.finish_requester_sweep(next).unwrap();
        bulk(&mut store, vec![owned(20_000 + turn, 1)]);
        drop(store);
        store = Store::open(&directory.0).unwrap();
        if next.is_none() {
            break;
        }
    }
    assert!((1..=10_101).all(|n| seen.contains(&format!("{n:032x}"))));
    // A wrap reaches newly inserted entries before the previous cursor.
    assert!(
        store
            .sweep_requester_jobs(200)
            .unwrap()
            .0
            .iter()
            .any(|job| job.created_at == 1)
    );
}

fn index_requests(store: &Store) -> u64 {
    match &store.backend {
        Backend::Ready(index) => index.requests(),
        _ => unreachable!(),
    }
}

/// Entries of every derived `mynou-jobs/4` lookup table.
fn ownership_entries(index: &mut Index) -> BTreeSet<Key> {
    let mut found = BTreeSet::new();
    for table in [
        layout::T_TRANSFER_LIVE,
        layout::T_TRANSFER_CANCELLED,
        layout::T_ACQUISITION_LIVE,
        layout::T_SHARED_PHYSICAL,
        layout::T_PACK_MAPPING,
        layout::T_MEDIA_READY,
        layout::T_MEDIA_RECENT,
        layout::T_MEDIA_BASE,
        layout::T_PENDING_CHILD,
    ] {
        let mut after = None;
        loop {
            let page = ops::Db(index).scan(&[table], after, 200).unwrap();
            found.extend(page.entries.iter().map(|(key, _)| *key));
            after = page.next;
            if after.is_none() {
                break;
            }
        }
    }
    found
}

#[test]
fn transfer_ownership_reads_a_fixed_window_beyond_former_limits() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    // A pack/IRC reference alone neither claims the transfer nor counts as
    // its cancelled download, but it is a live owner.
    let referrer = irc_fixture_job(30_000);
    let transfer = format!("{:040x}", 30_000);
    let url = "magnet:?xt=urn:btih:retried-acquisition";
    // Above the former 10,000-job limit: every older attempt was cancelled.
    let history: Vec<Job> = (1..=10_101)
        .map(|n| {
            let mut job = raw_job(n, 100 + n);
            job.download_id = Some(transfer.clone());
            job.acquisition_url = Some(url.into());
            job.state = "cancelled".into();
            job
        })
        .collect();
    bulk(&mut store, history.clone());
    let measure = |store: &mut Store, check: &dyn Fn(&mut Store) -> bool| {
        let before = index_requests(store);
        let answer = check(store);
        let requests = index_requests(store) - before;
        assert!(requests <= 6, "{requests} index requests");
        answer
    };
    let has_owner = |store: &mut Store| store.transfer_has_owner(&transfer).unwrap();
    let cancelled = |store: &mut Store| store.transfer_cancelled(&transfer).unwrap();
    let claimed = |store: &mut Store| store.transfer_claimed(&transfer, Some(url)).unwrap();
    let co_owned = |store: &mut Store| store.has_co_owner(&history[0]).unwrap();
    assert!(!measure(&mut store, &has_owner));
    assert!(measure(&mut store, &cancelled));
    assert!(!measure(&mut store, &claimed));
    assert!(!measure(&mut store, &co_owned));
    bulk(&mut store, vec![referrer.clone()]);
    assert!(measure(&mut store, &has_owner));
    assert!(!measure(&mut store, &claimed));
    assert!(measure(&mut store, &co_owned));
    assert!(
        !store.has_co_owner(&referrer).unwrap(),
        "only cancelled attempts share it"
    );
    // A new live attempt that claims it is found ahead of nothing older.
    let mut newest = raw_job(10_200, 20_000);
    newest.download_id = Some(transfer.clone());
    bulk(&mut store, vec![newest.clone()]);
    assert!(measure(&mut store, &claimed));
    assert!(store.has_co_owner(&referrer).unwrap());
    drop(store);
    // Restart keeps every answer: nothing is held in memory.
    let mut store = Store::open(&directory.0).unwrap();
    assert!(measure(&mut store, &has_owner));
    assert!(measure(&mut store, &claimed));
    assert!(measure(&mut store, &cancelled));
    newest.state = "cancelled".into();
    let mut gone = referrer.clone();
    gone.state = "cancelled".into();
    bulk(&mut store, vec![newest, gone]);
    assert!(!measure(&mut store, &has_owner));
    assert!(!measure(&mut store, &claimed));
    assert!(measure(&mut store, &cancelled));
    // Retrying an attempt makes it live again without its old download, so
    // only its acquisition address still claims the work.
    let mut retried = history[5_000].clone();
    retried.state = "queued".into();
    retried.download_id = None;
    bulk(&mut store, vec![retried]);
    assert!(!measure(&mut store, &has_owner));
    assert!(measure(&mut store, &claimed));
    assert!(!store.transfer_claimed(&transfer, None).unwrap());
}

#[test]
fn an_ownership_entry_without_a_matching_record_is_an_error() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut job = raw_job(1, 101);
    job.download_id = Some("c".repeat(40));
    job.state = "cancelled".into();
    bulk(&mut store, vec![job.clone()]);
    store
        .mutate(|db| {
            let id = layout::id_bytes(&job.id)?;
            db.0.commit(vec![Change::PutInline(
                layout::transfer_live_key(&job.download_id.clone().unwrap(), layout::CLAIMS, &id),
                Vec::new(),
            )])?;
            Ok(())
        })
        .unwrap();
    let error = store.transfer_has_owner(&"c".repeat(40)).unwrap_err();
    assert!(error.contains("disagrees"), "{error}");
}

#[test]
fn transfer_ownership_upgrade_resumes_and_refuses_read_only() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let jobs: Vec<Job> = (1..=450)
        .map(|n| {
            // IRC origins stay exactly as verified; they reference transfers.
            if n % 5 == 0 {
                return irc_fixture_job(n);
            }
            let mut job = raw_job(n, 100 + n);
            job.download_id = Some(format!("{:040x}", n % 7));
            job.acquisition_url = Some(format!("magnet:?xt=urn:btih:{n}"));
            if n % 3 == 0 {
                job.state = "cancelled".into();
            }
            job
        })
        .collect();
    bulk(&mut store, jobs.clone());
    drop(store);
    let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
    let expected = ownership_entries(&mut index);
    let derived: BTreeSet<Key> = jobs
        .iter()
        .flat_map(|job| layout::ownership(job).unwrap())
        .collect();
    assert_eq!(expected, derived);
    // Return to the previous format: drop every derived entry.
    let mut changes: Vec<Change> = expected.iter().map(|key| Change::Delete(*key)).collect();
    changes.push(Change::PutInline(
        layout::FORMAT_KEY,
        children::FORMAT.to_vec(),
    ));
    index.commit(changes).unwrap();
    drop(index);
    let before = contents(&directory.0);
    let error = Store::open_read_only(&directory.0).err().unwrap();
    assert!(error.contains("writable restart"), "{error}");
    assert_eq!(contents(&directory.0), before);
    // A crash while publishing the first page leaves the pending marker.
    let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
    index.inject(Some(Fault::Applied));
    assert!(ownership::upgrade(&mut index, false).is_err());
    assert!(!index.is_usable());
    drop(index);
    drop(Store::open(&directory.0).unwrap());
    let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
    assert_eq!(
        index.get(&layout::FORMAT_KEY).unwrap().unwrap(),
        backlog::FORMAT
    );
    assert!(index.get(&layout::OWNERSHIP_CURSOR_KEY).unwrap().is_none());
    assert_eq!(ownership_entries(&mut index), expected);
    // A crash after the second page: only the remaining jobs lack entries.
    let mut keys = Vec::new();
    let mut after = None;
    loop {
        let page = ops::Db(&mut index)
            .scan(&[layout::T_ALL], after, 200)
            .unwrap();
        keys.extend(page.entries.iter().map(|(key, _)| *key));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(keys.len(), 450);
    let mut changes = Vec::new();
    for key in &keys[400..] {
        let job = ops::Db(&mut index)
            .require(&ops::key_part(key, 9).unwrap())
            .unwrap();
        changes.extend(
            layout::ownership(&job)
                .unwrap()
                .into_iter()
                .map(Change::Delete),
        );
    }
    assert!(!changes.is_empty());
    changes.push(Change::PutInline(
        layout::FORMAT_KEY,
        ownership::PENDING.to_vec(),
    ));
    changes.push(Change::Put(
        layout::OWNERSHIP_CURSOR_KEY,
        keys[399].to_vec(),
    ));
    index.commit(changes).unwrap();
    drop(index);
    assert!(Store::open_read_only(&directory.0).is_err());
    let mut store = Store::open(&directory.0).unwrap();
    for job in &jobs {
        assert_eq!(store.get(&job.id).unwrap().unwrap(), *job);
    }
    drop(store);
    let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
    assert_eq!(ownership_entries(&mut index), expected);
    assert!(index.get(&layout::OWNERSHIP_CURSOR_KEY).unwrap().is_none());
    // A cursor outside the job table is refused, not skipped over.
    index
        .commit(vec![
            Change::PutInline(layout::FORMAT_KEY, ownership::PENDING.to_vec()),
            Change::Put(
                layout::OWNERSHIP_CURSOR_KEY,
                layout::key(&[&[0xff]]).to_vec(),
            ),
        ])
        .unwrap();
    assert!(ownership::upgrade(&mut index, false).is_err());
    drop(index);
    assert!(Store::open_read_only(&directory.0).is_err());
}

#[test]
fn shared_binding_reads_one_entry_beyond_former_limits() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let file = shared_file();
    // Above the former 10,000-job limit: unshared pack mappings of the same
    // physical video all precede the shared owner in the pack-file entries.
    let history: Vec<Job> = (1..=10_101)
        .map(|n| {
            let mut job = raw_job(n, 100 + n);
            job.pack_file = Some(file.file_path.clone());
            job.download_id = Some(file.torrent_id.clone());
            job.state = "cancelled".into();
            job
        })
        .collect();
    bulk(&mut store, history);
    let measure = |store: &mut Store| {
        let before = index_requests(store);
        let found = store
            .shared_binding(&file.torrent_id, &file.file_path)
            .unwrap();
        let requests = index_requests(store) - before;
        assert!(requests <= 4, "{requests} index requests");
        found
    };
    assert_eq!(measure(&mut store), None);
    // Owners staged as a validated shared group would store them.
    let mut owners: Vec<Job> = shared_requests(&file)
        .into_iter()
        .enumerate()
        .map(|(n, request)| {
            let mut job = raw_job(20_000 + n as u64, 20_000);
            job.key = request.canonical_key();
            job.request = request;
            job.shared_file = Some(file.clone());
            job.pack_file = Some(file.file_path.clone());
            file.validate_job(&job).unwrap();
            job
        })
        .collect();
    bulk(&mut store, owners.clone());
    assert_eq!(measure(&mut store), Some(file.clone()));
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(measure(&mut store), Some(file.clone()));
    // Cancelled shared owners still bind the physical video, as before.
    for owner in &mut owners {
        owner.state = "cancelled".into();
    }
    bulk(&mut store, owners);
    assert_eq!(measure(&mut store), Some(file));
}

#[test]
fn irc_ownership_conflicts_read_at_most_two_entries_per_alias() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let first = irc_fixture_job(1);
    bulk(&mut store, vec![first.clone()]);
    // A second acquisition of the same physical video and alias conflicts.
    let mut second = irc_fixture_job(2);
    let mut origin = second.irc_origin.take().unwrap();
    origin.file = first.irc_origin.as_ref().unwrap().file.clone();
    origin.torrent_aliases = first.irc_origin.as_ref().unwrap().torrent_aliases.clone();
    origin.torrent_id = origin.torrent_aliases[0].clone();
    second.irc_origin = Some(origin);
    let aliases = second.irc_origin.as_ref().unwrap().torrent_aliases.len() as u64;
    let error = store
        .mutate(|db| {
            let before = db.0.requests();
            let result = db.check_irc_ownership(&second);
            assert!(db.0.requests() - before <= 2 * aliases);
            result
        })
        .unwrap_err();
    assert!(error.contains("already belongs"), "{error}");
    // The job itself is never its own conflict.
    store.mutate(|db| db.check_irc_ownership(&first)).unwrap();
}

/// A pack episode request; its episode number keeps canonical keys apart.
fn pack_episode(n: u64, created: u64) -> Job {
    let mut job = raw_job(n, created);
    job.request.kind = "episode".into();
    job.request.season = 1;
    job.request.episode = u32::try_from(n).unwrap();
    job.request.source_url = Some("http://127.0.0.1/pack.torrent".into());
    job.key = job.request.canonical_key();
    job
}

#[test]
fn pack_mapping_checks_read_a_fixed_window_beyond_former_limits() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let path = "Show/S01E02.mkv";
    let url = "http://127.0.0.1/pack.torrent";
    let transfer = "b".repeat(40);
    // Above the former 10,000-job limit: every earlier mapping of the file
    // for this source and transfer belongs to another TMDB identity.
    let history: Vec<Job> = (1..=10_101)
        .map(|n| {
            let mut job = pack_episode(n, 100 + n);
            job.pack_file = Some(path.into());
            job.request.source_url = Some(url.into());
            job.key = job.request.canonical_key();
            job.download_id = Some(transfer.clone());
            job.state = "cancelled".into();
            job
        })
        .collect();
    bulk(&mut store, history.clone());
    let mut moved = pack_episode(20_000, 20_000);
    moved.pack_file = Some("Show/wrong.mkv".into());
    moved.request.source_url = Some(url.into());
    moved.key = moved.request.canonical_key();
    moved.download_id = Some(transfer.clone());
    moved.state = "failed".into();
    let mut clash = pack_episode(20_001, 20_001);
    clash.request.tmdb_id = history[5_000].request.tmdb_id;
    clash.pack_file = Some("Show/wrong.mkv".into());
    clash.request.source_url = Some("http://127.0.0.1/other.torrent".into());
    clash.key = clash.request.canonical_key();
    clash.download_id = Some(transfer.clone());
    clash.state = "failed".into();
    bulk(&mut store, vec![moved.clone(), clash.clone()]);
    let measure = |store: &mut Store, check: &dyn Fn(&mut ops::Db) -> Result<bool>| {
        store
            .read(false, |db| {
                let before = db.0.requests();
                let answer = check(db)?;
                let requests = db.0.requests() - before;
                assert!(requests <= 16, "{requests} index requests");
                Ok(answer)
            })
            .unwrap()
    };
    assert!(!measure(&mut store, &|db| db.pack_mapping_conflict(&moved, path)));
    assert!(measure(&mut store, &|db| db.pack_mapping_conflict(&clash, path)));
    assert!(measure(&mut store, &|db| db.unshared_pack_mapping(
        path,
        Some(url),
        &transfer
    )));
    let other = "c".repeat(40);
    assert!(measure(&mut store, &|db| db.unshared_pack_mapping(
        path,
        Some("http://127.0.0.1/other.torrent"),
        &transfer
    )));
    assert!(!measure(&mut store, &|db| db.unshared_pack_mapping(
        path,
        Some("http://127.0.0.1/other.torrent"),
        &other
    )));
    drop(store);
    // Nothing is held in memory: a restart gives the same answers.
    let mut store = Store::open(&directory.0).unwrap();
    assert!(measure(&mut store, &|db| db.pack_mapping_conflict(&clash, path)));
    let error = store.remap_pack(&clash.id, path.into()).unwrap_err();
    assert!(error.contains("already belongs"), "{error}");
    let remapped = store.remap_pack(&moved.id, path.into()).unwrap();
    assert_eq!(remapped.pack_file.as_deref(), Some(path));
    // Remapping onto the file it already names does not conflict with itself.
    let mut again = remapped.clone();
    again.state = "failed".into();
    bulk(&mut store, vec![again.clone()]);
    assert!(!measure(&mut store, &|db| db.pack_mapping_conflict(&again, path)));
}

#[test]
fn shared_owners_are_not_unshared_pack_mappings() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let file = shared_file();
    let owners: Vec<Job> = shared_requests(&file)
        .into_iter()
        .enumerate()
        .map(|(n, request)| {
            let mut job = raw_job(n as u64 + 1, 100);
            job.key = request.canonical_key();
            job.request = request;
            job.shared_file = Some(file.clone());
            job.pack_file = Some(file.file_path.clone());
            job.download_id = Some(file.torrent_id.clone());
            file.validate_job(&job).unwrap();
            job
        })
        .collect();
    let url = owners[0].request.source_url.clone();
    bulk(&mut store, owners);
    let unshared = |store: &mut Store| {
        store
            .read(false, |db| {
                db.unshared_pack_mapping(&file.file_path, url.as_deref(), &file.torrent_id)
            })
            .unwrap()
    };
    assert!(!unshared(&mut store));
    let mut single = pack_episode(100, 200);
    single.request.source_url = url.clone();
    single.key = single.request.canonical_key();
    single.pack_file = Some(file.file_path.clone());
    single.download_id = Some(file.torrent_id.clone());
    bulk(&mut store, vec![single]);
    assert!(unshared(&mut store));
}

#[test]
fn media_versions_read_a_fixed_window_beyond_former_limits() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let media = request("One media");
    let key = media.media_key();
    let version = |n: u64, created: u64| {
        let mut job = raw_job(n, created);
        job.request = media.clone();
        job.request.source_url = Some(format!("http://127.0.0.1/{n}"));
        job.key = job.request.canonical_key();
        job
    };
    // Above the former 10,000-job limit: cancelled upgrade attempts precede
    // the only base request and none of them is ready.
    let parent = raw_job(99_999, 50);
    bulk(&mut store, vec![parent.clone()]);
    let parent = parent.id;
    let history: Vec<Job> = (1..=10_101)
        .map(|n| {
            let mut job = version(n, 100 + n);
            job.upgrade_parent = Some(parent.clone());
            job.state = "cancelled".into();
            job
        })
        .collect();
    bulk(&mut store, history.clone());
    let base = version(20_000, 20_000);
    bulk(&mut store, vec![base.clone()]);
    let measure = |store: &mut Store, check: &dyn Fn(&mut Store) -> Option<Job>| {
        let before = index_requests(store);
        let answer = check(store);
        let requests = index_requests(store) - before;
        assert!(requests <= 6, "{requests} index requests");
        answer.map(|job| job.id)
    };
    let state = |store: &mut Store| {
        store
            .states_for(&BTreeSet::from([key.clone()]))
            .unwrap()
            .remove(&key)
    };
    let current = |store: &mut Store| store.current_media_job(&key).unwrap();
    assert_eq!(measure(&mut store, &state), Some(base.id.clone()));
    assert_eq!(measure(&mut store, &current), Some(base.id.clone()));
    // The oldest ready version wins over newer ones and over the newest.
    let mut ready = history[5_000].clone();
    ready.state = "ready".into();
    ready.progress = 1.0;
    ready.imports = vec!["/library/one.mkv".into()];
    let mut later = history[7_000].clone();
    later.state = "ready".into();
    later.progress = 1.0;
    later.imports = vec!["/library/two.mkv".into()];
    bulk(&mut store, vec![later.clone(), ready.clone()]);
    assert_eq!(measure(&mut store, &state), Some(ready.id.clone()));
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(measure(&mut store, &state), Some(ready.id.clone()));
    ready.state = "cancelled".into();
    later.state = "cancelled".into();
    let newest = version(30_000, 30_000);
    bulk(&mut store, vec![ready, later, newest.clone()]);
    assert_eq!(measure(&mut store, &state), Some(newest.id.clone()));
    // Same creation time: the larger identifier is the newer, as in the
    // creation-ordered media entries.
    let twin = version(30_001, 30_000);
    bulk(&mut store, vec![twin.clone()]);
    assert_eq!(measure(&mut store, &state), Some(twin.id.clone()));
    // An entry its record does not derive is an error, never a silent pick.
    store
        .mutate(|db| {
            let digest = layout::digest_bytes(&key)?;
            let id = layout::id_bytes(&newest.id)?;
            db.0.commit(vec![Change::PutInline(
                layout::key(&[
                    &layout::media_ready_prefix(&digest),
                    &newest.created_at.to_be_bytes(),
                    &id,
                ]),
                Vec::new(),
            )])?;
            Ok(())
        })
        .unwrap();
    let error = store
        .states_for(&BTreeSet::from([key.clone()]))
        .unwrap_err();
    assert!(error.contains("disagrees"), "{error}");
}

#[test]
fn a_filtered_search_reaches_rows_beyond_every_former_page_with_bounded_scans() {
    let directory = Directory::new();
    // 2,500 jobs: a match near the oldest end lies beyond any first page and
    // beyond one scan window, so it is reached only by following `next`.
    let jobs: Vec<Job> = (0..2_500)
        .map(|n| {
            let mut job = if n % 500 == 7 {
                ready_job(n, 1_000 + n)
            } else {
                raw_job(n, 1_000 + n)
            };
            if n == 3 {
                job.request.title = "Needle In The Haystack".into();
            }
            job
        })
        .collect();
    let mut store = Store::open(&directory.0).unwrap();
    bulk(&mut store, jobs);
    let filter = JobFilter {
        text: Some("needle in".into()),
        state: None,
    };
    let mut after: Option<String> = None;
    let mut found = Vec::new();
    let mut requests = 0;
    loop {
        let (page, scanned) = store.search(&filter, false, after.as_deref(), 50).unwrap();
        assert!(scanned as usize <= SEARCH_SCAN_ROWS);
        assert_eq!(page.total, 2_500);
        found.extend(page.jobs.into_iter().map(|job| job.id));
        requests += 1;
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(found, vec![format!("{:032x}", 3)]);
    assert_eq!(requests, 3, "2,500 rows in windows of 1,000");
    // A state filter over the library index, and the limit ends a page early.
    let ready = JobFilter {
        text: None,
        state: Some("ready".into()),
    };
    let (page, scanned) = store.search(&ready, true, None, 2).unwrap();
    assert_eq!(page.jobs.len(), 2);
    assert_eq!(scanned, 2);
    let (rest, _) = store
        .search(&ready, true, page.next.as_deref(), 50)
        .unwrap();
    assert_eq!(rest.jobs.len(), 3);
    assert!(rest.next.is_none());
    // Identifiers and states match too; nothing matches an absent text.
    let by_id = JobFilter {
        text: Some(format!("{:032x}", 1_234)),
        state: Some("queued".into()),
    };
    let mut after: Option<String> = None;
    let mut hits = 0;
    loop {
        let (page, _) = store.search(&by_id, false, after.as_deref(), 200).unwrap();
        hits += page.jobs.len();
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(hits, 1);
}

#[test]
fn a_job_beyond_the_first_pages_enters_and_leaves_a_state_search_when_it_changes() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    bulk(
        &mut store,
        (0..2_500).map(|n| raw_job(n, 1_000 + n)).collect(),
    );
    let cancelled = JobFilter {
        text: None,
        state: Some("cancelled".into()),
    };
    // Every window of a search from the start, as the web UI rereads it after
    // a live change, with the sequence the change produced.
    let search = |store: &mut Store| {
        let mut after: Option<String> = None;
        let mut found = Vec::new();
        loop {
            let (page, _) = store
                .search(&cancelled, false, after.as_deref(), 50)
                .unwrap();
            found.extend(page.jobs.into_iter().map(|job| (job.id, job.state)));
            after = page.next;
            if after.is_none() {
                return (found, page.sequence);
            }
        }
    };
    let (found, before) = search(&mut store);
    assert!(found.is_empty());
    // Job 3 is among the oldest: beyond any first page and the first window.
    let id = format!("{:032x}", 3);
    store.cancel(&id).unwrap();
    let (found, entered) = search(&mut store);
    assert_eq!(found, vec![(id.clone(), "cancelled".to_owned())]);
    assert!(
        entered > before,
        "the change advances the sequence clients watch"
    );
    store.retry(&id).unwrap();
    let (found, left) = search(&mut store);
    assert!(found.is_empty());
    assert!(left > entered);
}

#[test]
fn job_cursors_stay_stable_under_writes_and_reject_invalid_or_stale_positions() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    bulk(
        &mut store,
        (0..120).map(|n| raw_job(n, 1_000 + n)).collect(),
    );
    let first = store.page(None, 50).unwrap();
    // Newer jobs written between pages do not shift the continuation.
    bulk(
        &mut store,
        (120..140).map(|n| raw_job(n, 5_000 + n)).collect(),
    );
    let mut seen: BTreeSet<_> = first.jobs.iter().map(|job| job.id.clone()).collect();
    let mut after = first.next;
    while let Some(cursor) = after {
        let page = store.page(Some(&cursor), 50).unwrap();
        assert_eq!(page.total, 140);
        for job in page.jobs {
            assert!(
                job.created_at < 5_000,
                "a later write never enters an older page"
            );
            assert!(seen.insert(job.id));
        }
        after = page.next;
    }
    assert_eq!(seen.len(), 120);
    for invalid in ["", "zz", &"0".repeat(47), &"g".repeat(48)] {
        assert!(store.page(Some(invalid), 10).is_err(), "{invalid}");
    }
    // A well-formed cursor naming no job of this store is stale.
    let stale = format!("{:016x}{:032x}", 1_005_u64, 999_u64);
    let error = store.page(Some(&stale), 10).unwrap_err();
    assert!(error.contains("Stale job page cursor"), "{error}");
    assert!(store.library_page(Some(&stale), 10).is_err());
    let filter = JobFilter {
        text: Some("movie".into()),
        state: None,
    };
    assert!(store.search(&filter, false, Some(&stale), 10).is_err());
    // A job cursor names a creation time and identifier together.
    let moved = format!("{:016x}{:032x}", 1_006_u64, 5_u64);
    assert!(store.page(Some(&moved), 10).is_err());
}

#[test]
fn job_queries_parse_filters_strictly() {
    let query = JobQuery::parse("limit=20&q=Needle%20in%2Bhay&state=ready").unwrap();
    assert_eq!(query.limit, 20);
    assert_eq!(query.filter.text.as_deref(), Some("needle in+hay"));
    assert_eq!(query.filter.state.as_deref(), Some("ready"));
    assert!(JobQuery::parse("q=").unwrap().filter.is_empty());
    for invalid in [
        "state=unknown",
        "q=a&q=b",
        "state=ready&state=failed",
        "q=%ff",
        "q=%0a",
        "q=%2",
        "limit=0",
        "unknown=1",
        &format!("q={}", "a".repeat(129)),
    ] {
        assert!(JobQuery::parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn a_staged_migration_reports_resumable_progress_while_it_runs() {
    use crate::progress::{Migration, migrations};
    let jobs: Vec<Job> = (0..150).map(|n| raw_job(n, 100 + n)).collect();
    let directory = Directory::new();
    write_legacy(&directory.0, &jobs, &[], 0);
    // The second of three chunks is durable when the hook observes it.
    let mut during = Vec::new();
    let error = interrupted(&directory.0, |stage| {
        if stage == Stage::Chunk(2) {
            during = migrations(&[&directory.0]);
        }
        stage == Stage::Chunk(2)
    })
    .err()
    .unwrap();
    assert!(error.contains("interrupted"), "{error}");
    let copying = Migration {
        store: "jobs",
        stage: "copying",
        done: 128,
        total: Some(150),
        resumed: false,
    };
    assert_eq!(during, vec![copying.clone()]);
    // The interrupted attempt stays visible at the count it reached.
    let failed = Migration {
        stage: "failed",
        ..copying
    };
    assert_eq!(migrations(&[&directory.0]), vec![failed.clone()]);
    // The next writable start resumes after the durable chunks.
    let mut resumed_during = Vec::new();
    let store = migration::open_with(
        &directory.0,
        {
            let lock = secure_file(&directory.0.join(".lock")).unwrap();
            lock.try_lock().unwrap();
            lock
        },
        false,
        &mut |stage| {
            if stage == Stage::Chunk(3) {
                resumed_during = migrations(&[&directory.0]);
            }
            Ok(())
        },
    )
    .unwrap();
    drop(store);
    assert_eq!(
        resumed_during.last(),
        Some(&Migration {
            store: "jobs",
            stage: "copying",
            done: 150,
            total: Some(150),
            resumed: true,
        })
    );
    assert_eq!(
        migrations(&[&directory.0]),
        vec![
            failed,
            Migration {
                store: "jobs",
                stage: "complete",
                done: 150,
                total: Some(150),
                resumed: true,
            }
        ]
    );
    let report = crate::json::stringify(&crate::progress::report(&[&directory.0]));
    assert!(!report.contains(directory.0.to_str().unwrap()));
    assert!(!report.contains("Synthetic"));
}

#[test]
fn full_disk_at_every_migration_flush_preserves_legacy_and_restarts() {
    for boundary in 0..1000 {
        let directory = Directory::new();
        let jobs: Vec<_> = (0..70).map(|n| raw_job(n, 100 + n)).collect();
        write_legacy(&directory.0, &jobs[..65], &jobs[65..], 0);
        let original = fs::read(directory.0.join("snapshot.bin")).unwrap();
        let result = crate::store::with_full_disk_after(boundary, || Store::open(&directory.0));
        let error = match result {
            Ok(value) => {
                drop(value);
                assert!(boundary > 10);
                return;
            }
            Err(error) => error,
        };
        assert!(
            error.starts_with("Job storage: "),
            "boundary {boundary}: {error}"
        );
        assert!(
            error.contains("disk") || error.contains(crate::store::STORAGE_FULL),
            "boundary {boundary}: {error}"
        );
        assert!(!error.contains(directory.0.to_str().unwrap()), "{error}");
        // The original remains at its live name until marker publication;
        // afterwards the exact private backup must already exist.
        let live = fs::read(directory.0.join("snapshot.bin")).unwrap();
        if live != original {
            assert!(
                fs::read_dir(&directory.0).unwrap().any(|entry| {
                    let path = entry.unwrap().path();
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(".legacy-")
                        && fs::read(path).is_ok_and(|bytes| bytes == original)
                }),
                "boundary {boundary}: original snapshot lost"
            );
        }
        let store = Store::open(&directory.0).unwrap();
        drop(store);
        expect_migrated(&directory.0, &jobs);
    }
    panic!("migration fixture exceeded its durability boundary budget");
}

fn assert_library_backlog(
    store: &mut Store,
    at: u64,
    interval: u64,
    due: u64,
    oldest: Option<u64>,
) {
    let before = match &store.backend {
        Backend::Ready(index) => (index.sequence().unwrap(), index.requests()),
        _ => unreachable!(),
    };
    let value = store.library_backlog(at, interval, true).unwrap();
    let number = |n: Option<u64>| n.map_or(Value::Null, |n| n.to_string().into());
    assert_eq!(value.get("state"), Some(&Value::from("ready")));
    assert_eq!(value.get("due"), Some(&Value::from(due.to_string())));
    assert_eq!(value.get("oldest_due_at"), Some(&number(oldest)));
    assert_eq!(
        value.get("oldest_due_age_secs"),
        Some(&number(oldest.map(|oldest| at.saturating_sub(oldest))))
    );
    if let Backend::Ready(index) = &store.backend {
        assert_eq!(index.sequence().unwrap(), before.0, "status wrote storage");
        assert!(index.requests() - before.1 <= 4, "status loaded records");
        assert!(index.last_page_reads() <= 24, "status scanned tree pages");
    }
}

#[test]
fn library_backlog_counts_growth_rescheduling_removal_and_restart_without_payload_reads() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    assert_library_backlog(&mut store, 100, 60, 0, None);
    let mut jobs: Vec<_> = (1..=5_001).map(|n| monitored_fixture(n, 150)).collect();
    jobs[0].monitor_checked_at = 0;
    jobs[1].monitor_checked_at = 100;
    jobs[2].monitor_checked_at = 101;
    jobs[3].monitor_checked_at = 101;
    jobs[0].created_at = 1;
    jobs[0].updated_at = 1;
    bulk(&mut store, jobs.clone());
    assert_library_backlog(&mut store, 160, 60, 2, Some(1));
    assert_library_backlog(&mut store, 161, 60, 4, Some(1));
    // A point update removes the unchecked row from the current due range.
    store.record_monitor_check(&jobs[0].id, 161).unwrap();
    assert_library_backlog(&mut store, 161, 60, 3, Some(160));
    store.set_monitored(&jobs[1].id, false).unwrap();
    assert_library_backlog(&mut store, 161, 60, 2, Some(161));
    store.finish_library_pass(123).unwrap();
    // Damage an otherwise due primary record. Reading status must not touch it.
    let record = Store::record_key(&jobs[2].id).unwrap();
    assert!(crate::inventory::damage_record(&directory.0.join("jobs.idx"), &record) > 0);
    assert_library_backlog(&mut store, 161, 60, 2, Some(161));
    drop(store);
    let mut store = Store::open_read_only(&directory.0).unwrap();
    let before = flushes();
    assert_library_backlog(&mut store, 161, 60, 2, Some(161));
    assert_eq!(
        store
            .library_backlog(161, 60, true)
            .unwrap()
            .get("last_completed_pass_at"),
        Some(&Value::from("123"))
    );
    let disabled = store.library_backlog(161, 60, false).unwrap();
    assert_eq!(disabled.get("state"), Some(&Value::from("disabled")));
    assert_eq!(disabled.get("due"), Some(&Value::from("0")));
    assert_eq!(disabled.get("oldest_due_at"), Some(&Value::Null));
    assert_eq!(
        disabled.get("last_completed_pass_at"),
        Some(&Value::from("123"))
    );
    assert_eq!(flushes(), before);
}

#[test]
fn library_backlog_matches_interval_edges_and_clock_rollback_scheduling() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let mut future = monitored_fixture(4, 0);
    future.created_at = u64::MAX;
    future.updated_at = u64::MAX;
    bulk(
        &mut store,
        vec![
            monitored_fixture(1, 100),
            monitored_fixture(2, 101),
            monitored_fixture(3, u64::MAX),
            future,
        ],
    );
    assert_library_backlog(&mut store, 159, 60, 2, Some(159));
    assert_library_backlog(&mut store, 160, 60, 3, Some(160));
    assert_library_backlog(&mut store, 161, 60, 4, Some(160));
    assert_library_backlog(&mut store, 0, 60, 4, Some(0));
    assert_library_backlog(&mut store, 100, 0, 4, Some(100));
    assert_library_backlog(&mut store, u64::MAX, 60, 3, Some(160));
    assert_library_backlog(&mut store, u64::MAX, 0, 4, Some(100));
    assert_library_backlog(&mut store, 1, u64::MAX, 4, Some(1));
    // The independently implemented scheduler agrees on the due identities.
    for at in [0, 100, 159, 160, 161, u64::MAX] {
        let window = store.monitoring_due(at, 60, false, 10).unwrap();
        let value = store.library_backlog(at, 60, true).unwrap();
        assert_eq!(
            value.get("due"),
            Some(&Value::from(window.jobs.len().to_string()))
        );
    }
}

#[test]
fn library_backlog_prepares_legacy_counts_only_on_writable_startup() {
    let directory = Directory::new();
    let jobs: Vec<_> = (1..=401).map(|n| monitored_fixture(n, 100)).collect();
    write_legacy(&directory.0, &jobs, &[], 0);
    drop(secure_file(&directory.0.join(".lock")).unwrap());
    let mut preview = Store::open_read_only(&directory.0).unwrap();
    assert!(preview.library_backlog(160, 60, true).is_err());
    drop(preview);
    let mut store = Store::open(&directory.0).unwrap();
    assert_library_backlog(&mut store, 160, 60, 401, Some(160));
    if let Backend::Ready(index) = &mut store.backend {
        crate::inventory::legacy_range_index(index);
    }
    drop(store);
    let snapshot = contents(&directory.0);
    let mut preview = Store::open_read_only(&directory.0).unwrap();
    let before = preview.sequence().unwrap();
    assert!(
        preview
            .library_backlog(160, 60, true)
            .unwrap_err()
            .contains("explicit writable upgrade")
    );
    assert_eq!(preview.sequence().unwrap(), before);
    drop(preview);
    assert_eq!(contents(&directory.0), snapshot);
    // Target the count-upgrade commit, rather than the store's opening flush.
    {
        let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
        assert!(with_full_disk_after(0, || index.prepare_range_counts(1)).is_err());
    }
    let mut store = Store::open(&directory.0).unwrap();
    assert_library_backlog(&mut store, 160, 60, 401, Some(160));
    for row in jobs {
        assert_eq!(store.get(&row.id).unwrap().unwrap(), row);
    }
}

#[test]
fn library_pass_freshness_recovers_at_every_commit_boundary() {
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.finish_library_pass(10).unwrap();
        if let Backend::Ready(index) = &mut store.backend {
            index.inject(Some(fault));
        }
        assert!(store.finish_library_pass(20).is_err());
        assert!(store.library_backlog(30, 60, true).is_err());
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        let expected = if fault == Fault::Staged { "10" } else { "20" };
        assert_eq!(
            store
                .library_backlog(30, 60, true)
                .unwrap()
                .get("last_completed_pass_at"),
            Some(&Value::from(expected))
        );
    }
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    store.finish_library_pass(10).unwrap();
    assert!(with_full_disk_after(0, || store.finish_library_pass(20)).is_err());
    drop(store);
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(
        store
            .library_backlog(30, 60, true)
            .unwrap()
            .get("last_completed_pass_at"),
        Some(&Value::from("10"))
    );
}

#[test]
fn library_pass_freshness_excludes_manual_preview_failure_and_stop() {
    use crate::engine::{Engine, lock};
    let directory = Directory::new();
    let mut config =
        crate::config::from_json(&crate::config::default_json(), &directory.0).unwrap();
    config.downloads_enabled = false;
    config.monitoring.enabled = true;
    let engine = Engine::open_for_management(config.clone()).unwrap();
    let read = |engine: &Engine| {
        engine
            .status()
            .unwrap()
            .get("inventory")
            .unwrap()
            .get("library")
            .unwrap()
            .get("last_completed_pass_at")
            .unwrap()
            .clone()
    };
    assert_eq!(read(&engine), Value::Null);
    engine.check_due_upgrades().unwrap(); // a completed empty automatic pass
    assert_ne!(read(&engine), Value::Null);
    lock(&engine.store)
        .unwrap()
        .finish_library_pass(123)
        .unwrap();
    engine.check_upgrades(false).unwrap();
    engine.check_upgrades(true).unwrap();
    assert_eq!(read(&engine), Value::from("123"));
    engine.stopped.store(true, Ordering::Release);
    engine.check_due_upgrades().unwrap();
    assert_eq!(read(&engine), Value::from("123"));
    engine.stopped.store(false, Ordering::Release);
    // A valid local import reaches search; absent indexers make it unavailable.
    let path = directory.0.join("synthetic.mp4");
    fs::write(&path, include_bytes!("../../examples/demo.mp4")).unwrap();
    let mut job = monitored_fixture(1, 0);
    job.imports = vec![path.to_str().unwrap().into()];
    bulk(&mut lock(&engine.store).unwrap(), vec![job.clone()]);
    let report = engine.check_due_upgrades().unwrap();
    assert!(crate::json::stringify(&report).contains("search_unavailable"));
    assert_eq!(read(&engine), Value::from("123"));
    // A bounded successful pass may still have more due work after its window.
    let mut rows: Vec<_> = (2..=35).map(|n| monitored_fixture(n, 0)).collect();
    rows.iter_mut().for_each(|row| row.created_at = 1);
    bulk(&mut lock(&engine.store).unwrap(), rows);
    let report = engine.check_due_upgrades().unwrap();
    assert_eq!(report.get("limited"), Some(&Value::Bool(true)));
    assert_ne!(read(&engine), Value::from("123")); // missing imports are settled skips
    let last = read(&engine);
    drop(engine);
    let engine = Engine::open_for_management(config).unwrap();
    assert_eq!(read(&engine), last);
}

#[test]
fn library_backlog_status_reports_busy_and_redacted_failure() {
    use crate::engine::{Engine, lock};
    let directory = Directory::new();
    let mut config =
        crate::config::from_json(&crate::config::default_json(), &directory.0).unwrap();
    config.downloads_enabled = false;
    config.monitoring.enabled = true;
    let engine = Engine::open_for_management(config).unwrap();
    {
        let _guard = lock(&engine.store).unwrap();
        let status = engine.status().unwrap();
        assert_eq!(
            status
                .get("inventory")
                .unwrap()
                .get("library")
                .unwrap()
                .get("state"),
            Some(&Value::from("busy"))
        );
    }
    {
        let mut store = lock(&engine.store).unwrap();
        if let Backend::Ready(index) = &mut store.backend {
            index.inject(Some(Fault::Published));
        }
        assert!(store.finish_library_pass(10).is_err());
    }
    let status = engine.status().unwrap();
    let backlog = status.get("inventory").unwrap().get("library").unwrap();
    assert_eq!(backlog.get("state"), Some(&Value::from("unavailable")));
    assert!(backlog.get("due").is_none());
    let report = crate::json::stringify(backlog);
    assert!(!report.contains(directory.0.to_str().unwrap()));
    assert!(!report.contains("Synthetic"));
    assert!(report.contains("restart with write access"));
    assert!(status.get("version").is_some());
}

fn assert_job_backlog(store: &mut Store, at: u64, due: u64, oldest: Option<u64>) -> Value {
    let before = match &store.backend {
        Backend::Ready(index) => (index.sequence().unwrap(), index.requests()),
        _ => unreachable!(),
    };
    let value = store.job_backlog(at).unwrap();
    let number = |n: Option<u64>| n.map_or(Value::Null, |n| n.to_string().into());
    assert_eq!(value.get("state"), Some(&Value::from("ready")));
    assert_eq!(value.get("due"), Some(&Value::from(due.to_string())));
    assert_eq!(value.get("oldest_due_at"), Some(&number(oldest)));
    assert_eq!(
        value.get("oldest_due_age_secs"),
        Some(&number(oldest.map(|oldest| at.saturating_sub(oldest))))
    );
    if let Backend::Ready(index) = &store.backend {
        assert_eq!(index.sequence().unwrap(), before.0, "status wrote storage");
        assert!(index.requests() - before.1 <= 3, "status loaded records");
        assert!(index.last_page_reads() <= 24, "status scanned tree pages");
    }
    value
}

#[test]
fn job_backlog_counts_unpromoted_growth_leases_retries_and_cancellation() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    assert_job_backlog(&mut store, 100, 0, None);
    let mut jobs: Vec<_> = (1..=5_001)
        .map(|n| {
            let mut job = raw_job(n, n);
            job.next_attempt_at = 100;
            job
        })
        .collect();
    jobs[0].next_attempt_at = 0;
    jobs[1].next_attempt_at = 200;
    jobs[2].state = "failed".into();
    jobs[2].next_attempt_at = 0; // Exhausted retries are retained history.
    jobs[3].state = "cancelled".into();
    jobs[4].state = "staged".into();
    jobs[5].lease_id = Some("fixture-lease".into());
    jobs[5].lease_until = 150;
    bulk(&mut store, jobs.clone());
    assert_job_backlog(&mut store, 99, 1, Some(0));
    assert_job_backlog(&mut store, 100, 4_996, Some(0));
    // The oldest job leaves the due range when claimed, then returns on expiry.
    let claimed = store
        .claim_filtered(100, 20, |j| Ok(j.id == jobs[0].id))
        .unwrap()
        .unwrap();
    assert_job_backlog(&mut store, 119, 4_995, Some(100));
    assert_job_backlog(&mut store, 120, 4_996, Some(100));
    store
        .renew(&claimed.id, claimed.lease_id.as_deref().unwrap(), 119, 100)
        .unwrap();
    assert_job_backlog(&mut store, 120, 4_995, Some(100));
    store
        .release_lease(&claimed.id, claimed.lease_id.as_deref().unwrap())
        .unwrap();
    store.cancel(&claimed.id).unwrap();
    assert_job_backlog(&mut store, 120, 4_995, Some(100));
    // Retry rescheduling updates the diagnostic entry in the same transaction.
    let mut retry = store.get(&jobs[6].id).unwrap().unwrap();
    retry.state = "failed".into();
    retry.next_attempt_at = 300;
    store.update(retry.clone()).unwrap();
    assert_job_backlog(&mut store, 120, 4_994, Some(100));
    retry.next_attempt_at = 0;
    store.update(retry).unwrap();
    assert_job_backlog(&mut store, u64::MAX, 4_996, Some(100));
    // Status reads the derived index, even if a primary payload is damaged.
    let record = Store::record_key(&jobs[7].id).unwrap();
    assert!(crate::inventory::damage_record(&directory.0.join("jobs.idx"), &record) > 0);
    drop(store);
    let mut store = Store::open_read_only(&directory.0).unwrap();
    let before = flushes();
    assert_job_backlog(&mut store, 120, 4_994, Some(100));
    assert_eq!(flushes(), before);
}

#[test]
fn job_backlog_matches_wake_times_after_promotion_and_clock_rollback() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let times = [0, 100, 101, u64::MAX];
    let jobs: Vec<_> = times
        .into_iter()
        .enumerate()
        .map(|(n, time)| {
            let mut job = raw_job(n as u64 + 1, 1);
            job.next_attempt_at = time;
            job
        })
        .collect();
    bulk(&mut store, jobs);
    store
        .claim_filtered(u64::MAX - 1, 1, |_| Ok(false))
        .unwrap();
    for (at, count) in [(0, 1), (99, 1), (100, 2), (101, 3), (u64::MAX, 4)] {
        assert_job_backlog(&mut store, at, count, Some(0));
    }
    store.cancel(&format!("{:032x}", 1)).unwrap();
    assert_job_backlog(&mut store, 99, 0, None);
}

#[test]
fn job_claim_freshness_checkpoints_success_without_idle_write_amplification() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    assert_eq!(
        store.job_backlog(10).unwrap().get("last_completed_pass_at"),
        Some(&Value::Null)
    );
    store.claim(10, 1).unwrap(); // An empty successful scan is a completed pass.
    let before = flushes();
    store.claim(11, 1).unwrap();
    assert_eq!(flushes(), before);
    assert_eq!(
        store.job_backlog(11).unwrap().get("last_completed_pass_at"),
        Some(&Value::from("10"))
    );
    let at = 10 + CLAIM_CHECKPOINT_SECS;
    store.claim(at, 1).unwrap();
    assert_eq!(
        store.job_backlog(at).unwrap().get("last_completed_pass_at"),
        Some(&Value::from(at.to_string()))
    );
    bulk(&mut store, vec![raw_job(1, 1)]);
    let failing = at + CLAIM_CHECKPOINT_SECS;
    assert!(
        store
            .claim_filtered(failing, 1, |_| Err("synthetic eligibility failure".into()))
            .is_err()
    );
    assert_eq!(
        store
            .job_backlog(failing)
            .unwrap()
            .get("last_completed_pass_at"),
        Some(&Value::from(at.to_string()))
    );
    let claimed = store.claim(failing, 1).unwrap().unwrap();
    assert_eq!(
        store
            .job_backlog(failing)
            .unwrap()
            .get("last_completed_pass_at"),
        Some(&Value::from(failing.to_string()))
    );
    drop(store);
    let mut store = Store::open_read_only(&directory.0).unwrap();
    assert_eq!(
        store
            .job_backlog(failing)
            .unwrap()
            .get("last_completed_pass_at"),
        Some(&Value::from(failing.to_string()))
    );
    assert!(store.get(&claimed.id).unwrap().is_some());
}

fn old_job_backlog_fixture(store: &mut Store) {
    store
        .mutate(|db| {
            let mut changes = Vec::new();
            let mut after = None;
            loop {
                let page = db.scan(&[layout::T_JOB_DUE], after, MAX_PAGE_ROWS)?;
                changes.extend(page.entries.iter().map(|(key, _)| Change::Delete(*key)));
                after = page.next;
                if after.is_none() {
                    break;
                }
            }
            changes.push(Change::PutInline(
                layout::FORMAT_KEY,
                ownership::FORMAT.to_vec(),
            ));
            db.0.commit(changes)?;
            Ok(())
        })
        .unwrap();
}

#[test]
fn job_backlog_upgrade_is_read_only_safe_and_resumes_each_commit_boundary() {
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        let jobs: Vec<_> = (1..=401).map(|n| raw_job(n, n)).collect();
        bulk(&mut store, jobs.clone());
        old_job_backlog_fixture(&mut store);
        drop(store);
        let before = contents(&directory.0);
        let error = Store::open_read_only(&directory.0).err().unwrap();
        assert!(error.contains("writable restart"), "{error}");
        assert_eq!(contents(&directory.0), before);
        let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
        // Interrupt a page while an explicit upgrade is in progress.
        index
            .commit(vec![Change::PutInline(
                layout::FORMAT_KEY,
                backlog::PENDING.to_vec(),
            )])
            .unwrap();
        index.inject(Some(fault));
        assert!(backlog::upgrade(&mut index, false).is_err());
        drop(index);
        let mut store = Store::open(&directory.0).unwrap();
        assert_job_backlog(&mut store, 1, 401, Some(0));
        for job in &jobs {
            assert_eq!(store.get(&job.id).unwrap().as_ref(), Some(job));
        }
        if let Backend::Ready(index) = &mut store.backend {
            assert!(
                index
                    .get(&layout::JOB_BACKLOG_CURSOR_KEY)
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[test]
fn job_backlog_upgrade_survives_full_disk_and_prepares_legacy_range_counts() {
    let directory = Directory::new();
    let jobs: Vec<_> = (1..=401).map(|n| raw_job(n, n)).collect();
    write_legacy(&directory.0, &jobs, &[], 0);
    let mut store = Store::open(&directory.0).unwrap();
    assert_job_backlog(&mut store, 1, 401, Some(0));
    old_job_backlog_fixture(&mut store);
    drop(store);
    let before = contents(&directory.0);
    let mut index = Index::open(&directory.0.join("jobs.idx"), false).unwrap();
    assert!(with_full_disk_after(0, || backlog::upgrade(&mut index, false)).is_err());
    drop(index);
    let mut store = Store::open(&directory.0).unwrap();
    assert_job_backlog(&mut store, 1, 401, Some(0));
    for (path, bytes) in before
        .iter()
        .filter(|(path, _)| path.to_string_lossy().contains(".legacy-"))
    {
        assert_eq!(contents(&directory.0).get(path), Some(bytes));
    }
    if let Backend::Ready(index) = &mut store.backend {
        crate::inventory::legacy_range_index(index);
    }
    drop(store);
    let before = contents(&directory.0);
    let mut store = Store::open_read_only(&directory.0).unwrap();
    assert!(
        store
            .job_backlog(1)
            .unwrap_err()
            .contains("explicit writable upgrade")
    );
    drop(store);
    assert_eq!(contents(&directory.0), before);
    let mut store = Store::open(&directory.0).unwrap();
    assert_job_backlog(&mut store, 1, 401, Some(0));
}

#[test]
fn job_claim_freshness_recovers_atomically_with_the_claim_cursor() {
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut store = Store::open(&directory.0).unwrap();
        store.claim(10, 1).unwrap();
        if let Backend::Ready(index) = &mut store.backend {
            index.inject(Some(fault));
        }
        assert!(store.claim(10 + CLAIM_CHECKPOINT_SECS, 1).is_err());
        assert!(store.job_backlog(100).is_err());
        drop(store);
        let mut store = Store::open(&directory.0).unwrap();
        let expected = if fault == Fault::Staged {
            10
        } else {
            10 + CLAIM_CHECKPOINT_SECS
        };
        assert_eq!(
            store
                .job_backlog(100)
                .unwrap()
                .get("last_completed_pass_at"),
            Some(&Value::from(expected.to_string()))
        );
    }
}

#[test]
fn job_backlog_status_redacts_storage_failure_and_keeps_other_fields() {
    use crate::engine::{Engine, lock};
    let directory = Directory::new();
    let mut config =
        crate::config::from_json(&crate::config::default_json(), &directory.0).unwrap();
    config.downloads_enabled = false;
    let engine = Engine::open_for_management(config).unwrap();
    {
        let mut store = lock(&engine.store).unwrap();
        if let Backend::Ready(index) = &mut store.backend {
            index.inject(Some(Fault::Published));
        }
        assert!(store.claim(10, 1).is_err());
    }
    let status = engine.status().unwrap();
    let value = status.get("inventory").unwrap().get("jobs").unwrap();
    assert_eq!(value.get("state"), Some(&Value::from("unavailable")));
    assert!(value.get("due").is_none());
    let report = crate::json::stringify(value);
    assert!(!report.contains(directory.0.to_str().unwrap()));
    assert!(report.contains("restart with write access"));
    assert!(status.get("version").is_some());
}
