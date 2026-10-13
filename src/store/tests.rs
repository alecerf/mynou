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
        assert!(monitoring::upgrade(&mut index, false).is_err());
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
            monitoring::FORMAT
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
fn interrupted(directory: &Path, stop: impl Fn(Stage) -> bool) -> Result<Store> {
    let lock = secure_file(&directory.join(".lock"))?;
    lock.try_lock().map_err(|error| error.to_string())?;
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
fn routing_window_continues_past_an_ineligible_page() {
    let directory = Directory::new();
    let mut store = Store::open(&directory.0).unwrap();
    let jobs: Vec<Job> = (0..205).map(|n| raw_job(n, 100 + n)).collect();
    bulk(&mut store, jobs.clone());
    assert!(
        store
            .open_jobs_filtered(200, |job| Ok(job.id == jobs[204].id))
            .unwrap()
            .is_empty()
    );
    let next = store
        .open_jobs_filtered(200, |job| Ok(job.id == jobs[204].id))
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].id, jobs[204].id);
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
