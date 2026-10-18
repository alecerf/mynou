//! Independently authored synthetic requester inventory fixtures; CI only.
use super::{
    Account, Capture, Demand, LedgerRead, Policy, RequesterStore, State,
    index::{self, Progress, Stage, Sweep},
};
use crate::{
    config::{self, Config},
    crypto::{random_bytes, sha256},
    inventory::{Change, Fault, Index, MAX_PAGE_ROWS},
    json,
    store::Request,
};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Instant,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let suffix = super::digest(&random_bytes::<16>().unwrap());
        let path = std::env::temp_dir().join(format!("mynou-requester-index-{suffix}"));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn config(root: &Path) -> Config {
    let mut config = config::from_json(&config::default_json(), root).unwrap();
    config.requesters.accounts = vec![Account {
        id: "fixture".into(),
        expected_user_id: "812".into(),
        token_env: "MYNOU_SYNTHETIC_TOKEN".into(),
        identity_url: "http://127.0.0.1:1/identity".into(),
        watchlist_url: "http://127.0.0.1:1/watchlist".into(),
    }];
    config
}

fn demand(config: &Config, number: u64, origin_bytes: usize) -> Demand {
    let request = Request {
        kind: "movie".into(),
        title: format!("Synthetic inventory movie {number}"),
        year: 2026,
        season: 0,
        episode: 0,
        tmdb_id: Some(number + 1),
        source_path: None,
        source_url: None,
        source_numbering: None,
    };
    Demand {
        id: Demand::identity("fixture", &request),
        account_id: "fixture".into(),
        origins: BTreeSet::from([format!("synthetic:{number}:{}", "x".repeat(origin_bytes))]),
        request,
        revision: 1,
        capture: Capture::new(config, &Policy::initial(config), "movie").unwrap(),
        state: "pending".into(),
        job_id: None,
        admitted_at: None,
        outcome: "pending".into(),
    }
}

fn legacy_bytes(state: &State) -> Vec<u8> {
    let payload = json::stringify(&state.to_json()).into_bytes();
    let mut bytes = b"MYNOUR01".to_vec();
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&sha256(&bytes));
    bytes
}

fn write_private(path: &Path, bytes: &[u8]) {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn all_ids(store: &RequesterStore) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    let mut after = None;
    loop {
        let (page, next) = store.demands_page(after, 137).unwrap();
        assert!(page.len() <= 137);
        for record in page {
            assert!(ids.insert(record.id), "cursor repeated a demand");
        }
        if next.is_none() {
            return ids;
        }
        assert_ne!(after, next, "cursor made no progress");
        after = next;
    }
}

#[test]
fn retained_inventory_outgrows_both_legacy_limits_and_continues_after_restart() {
    let directory = Directory::new();
    let config = config(&directory.0);
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    let mut expected = BTreeSet::new();
    let mut aggregate_bytes = 0_usize;
    let began = Instant::now();
    for start in (0..10_100_u64).step_by(100) {
        let mut patch = store.state.clone();
        for number in start..start + 100 {
            let record = demand(&config, number, 1400);
            aggregate_bytes += json::stringify(&record.to_json()).len();
            expected.insert(record.id.clone());
            patch.demands.insert(record.id.clone(), record);
            patch.operator_jobs.insert(format!("{number:032x}"));
        }
        store.save_patch(patch).unwrap();
        assert!(store.state.demands.is_empty());
        assert!(store.state.operator_jobs.is_empty());
        if [1000, 5000, 10_100].contains(&(start + 100)) {
            let point = Instant::now();
            let record = demand(&config, start + 99, 1400);
            assert_eq!(store.demand(&record.id).unwrap(), Some(record));
            let (page, _) = store.account_page("fixture", None, 25).unwrap();
            assert_eq!(page.len(), 25);
            eprintln!(
                "requester retained={} encoded_bytes={} elapsed={:?} point_and_page={:?}",
                start + 100,
                aggregate_bytes,
                began.elapsed(),
                point.elapsed()
            );
        }
    }
    assert!(aggregate_bytes > 16 * 1024 * 1024);
    assert_eq!(store.count("fixture", None).unwrap(), 10_100);
    assert_eq!(all_ids(&store), expected);
    assert!(
        store
            .operator_interest(&format!("{:032x}", 10_099))
            .unwrap()
    );
    drop(store);

    let mut index = Index::open(&directory.0.join("requesters.idx"), true).unwrap();
    assert_eq!(
        index::decode_count(index.get(&index::OPERATOR_COUNT).unwrap()).unwrap(),
        10_100
    );
    let page = index::account_page(&mut index, "fixture", None, 25).unwrap();
    assert_eq!(page.demands.len(), 25);
    assert!(
        index.last_page_reads() < 128,
        "a small page traversed the inventory"
    );
    drop(index);

    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    assert_eq!(all_ids(&store), expected);
    let added = demand(&config, 10_100, 1400);
    let mut patch = store.state.clone();
    patch.demands.insert(added.id.clone(), added.clone());
    patch.operator_jobs.insert(format!("{:032x}", 10_100));
    store.save_patch(patch).unwrap();
    expected.insert(added.id.clone());
    assert_eq!(all_ids(&store), expected);
    assert_eq!(store.demand(&added.id).unwrap(), Some(added));
    drop(store);
    let read_only = RequesterStore::open(&directory.0, &config, true).unwrap();
    assert_eq!(all_ids(&read_only), expected);
    assert_eq!(read_only.count("fixture", None).unwrap(), 10_101);
}

#[test]
fn chunked_demands_keep_account_pages_within_the_hydrated_byte_budget() {
    let directory = Directory::new();
    let config = config(&directory.0);
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    let mut patch = store.state.clone();
    for number in 0..5 {
        let mut record = demand(&config, number, 0);
        record.origins = (0..400)
            .map(|origin| format!("synthetic:{number}:{origin}:{}", "x".repeat(3900)))
            .collect();
        assert!(json::stringify(&record.to_json()).len() > crate::inventory::MAX_VALUE_BYTES);
        patch.demands.insert(record.id.clone(), record);
    }
    let expected: BTreeSet<_> = patch.demands.keys().cloned().collect();
    store.save_patch(patch).unwrap();
    let mut after = None;
    let mut observed = BTreeSet::new();
    let mut pages = 0;
    loop {
        let (records, next) = store.account_page("fixture", after, MAX_PAGE_ROWS).unwrap();
        pages += 1;
        let bytes: usize = records
            .iter()
            .map(|record| json::stringify(&record.to_json()).len())
            .sum();
        assert!(bytes <= crate::inventory::MAX_PAGE_BYTES);
        for record in records {
            assert!(observed.insert(record.id.clone()));
            assert_eq!(store.demand(&record.id).unwrap(), Some(record));
        }
        if next.is_none() {
            break;
        }
        assert_ne!(after, next);
        after = next;
    }
    assert!(pages >= 3, "row limit bypassed the hydrated byte bound");
    assert_eq!(observed, expected);
}

#[test]
fn same_source_interrupted_migration_resumes_and_preserves_exact_private_original() {
    let directory = Directory::new();
    let config = config(&directory.0);
    let store = RequesterStore::open(&directory.0, &config, true).unwrap();
    let mut legacy = store.state.clone();
    drop(store);
    for number in 0..203 {
        let record = demand(&config, number, 10);
        legacy.demands.insert(record.id.clone(), record);
    }
    let bytes = legacy_bytes(&legacy);
    write_private(&directory.0.join("requesters.bin"), &bytes);
    let source = sha256(&bytes);
    let mut staging = Index::create(&directory.0.join("requesters.idx.staging")).unwrap();
    staging
        .commit(vec![Change::PutInline(index::SOURCE, source.to_vec())])
        .unwrap();
    let batch: Vec<_> = legacy
        .demands
        .values()
        .take(MAX_PAGE_ROWS)
        .cloned()
        .collect();
    let mut metadata = legacy.clone();
    metadata.demands.clear();
    index::commit(&mut staging, &metadata, &batch, &[]).unwrap();
    // Simulate a crash after a durable batch, before its continuation write.
    let identity = staging.identity();
    drop(staging);
    let read_only = RequesterStore::open(&directory.0, &config, true).unwrap();
    assert_eq!(all_ids(&read_only).len(), 203);
    drop(read_only);
    assert_eq!(fs::read(directory.0.join("requesters.bin")).unwrap(), bytes);
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    assert_eq!(all_ids(&store).len(), 203);
    assert_eq!(store.count("fixture", None).unwrap(), 203);
    drop(store);
    let backup = directory
        .0
        .join(format!("requesters.legacy-{}.bin", super::digest(&source)));
    assert_eq!(fs::read(&backup).unwrap(), bytes);
    assert_eq!(
        fs::metadata(backup).unwrap().permissions().mode() & 0o077,
        0
    );
    let mut published = Index::open(&directory.0.join("requesters.idx"), true).unwrap();
    assert_eq!(published.identity(), identity);
    assert!(published.get(&index::CURSOR).unwrap().is_none());
}

#[test]
fn interrupted_staging_from_another_source_is_preserved_without_importing_its_rows() {
    let directory = Directory::new();
    let config = config(&directory.0);
    let mut staging = Index::create(&directory.0.join("requesters.idx.staging")).unwrap();
    staging
        .commit(vec![Change::PutInline(index::SOURCE, [42; 32].to_vec())])
        .unwrap();
    let identity = staging.identity();
    drop(staging);
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    assert!(all_ids(&store).is_empty());
    drop(store);
    let abandoned: Vec<_> = fs::read_dir(&directory.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("requesters.idx.abandoned-")
        })
        .collect();
    assert_eq!(abandoned.len(), 1);
    let mut preserved = Index::open(&abandoned[0], true).unwrap();
    assert_eq!(preserved.identity(), identity);
    assert_eq!(
        preserved.get(&index::SOURCE).unwrap(),
        Some([42; 32].to_vec())
    );
}

#[test]
fn restoring_a_matching_legacy_source_cannot_rewind_acknowledged_indexed_changes() {
    let directory = Directory::new();
    let config = config(&directory.0);
    let store = RequesterStore::open(&directory.0, &config, true).unwrap();
    let mut legacy = store.state.clone();
    drop(store);
    let original = demand(&config, 65, 10);
    legacy.demands.insert(original.id.clone(), original.clone());
    let bytes = legacy_bytes(&legacy);
    write_private(&directory.0.join("requesters.bin"), &bytes);
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    let mut changed = original.clone();
    changed.outcome = "failed".into();
    let mut patch = store.state.clone();
    patch.demands.insert(changed.id.clone(), changed.clone());
    patch.operator_jobs.insert(format!("{:032x}", 73));
    store.save_patch(patch).unwrap();
    drop(store);
    let mut index = Index::open(&directory.0.join("requesters.idx"), true).unwrap();
    let sequence = index.sequence().unwrap();
    let header = index.get(&index::HEADER).unwrap();
    drop(index);
    // Restore the exact private source whose digest the published index carries.
    fs::write(directory.0.join("requesters.bin"), &bytes).unwrap();
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    assert!(store.initialize().is_err());
    drop(store);
    assert_eq!(fs::read(directory.0.join("requesters.bin")).unwrap(), bytes);
    let mut index = Index::open(&directory.0.join("requesters.idx"), true).unwrap();
    assert_eq!(index.sequence().unwrap(), sequence);
    assert_eq!(index.get(&index::HEADER).unwrap(), header);
    assert_eq!(index::get(&mut index, &changed.id).unwrap(), Some(changed));
    assert!(index::has_operator(&mut index, &format!("{:032x}", 73)).unwrap());
}

#[test]
fn uncertain_reservation_commit_requires_reopen_and_recovers_frozen_capture() {
    for fault in [
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
    ] {
        let directory = Directory::new();
        let config = config(&directory.0);
        let store = RequesterStore::open(&directory.0, &config, true).unwrap();
        let metadata = store.state.clone();
        drop(store);
        let mut record = demand(&config, 91, 20);
        record.state = "reserved".into();
        record.outcome = "reserved".into();
        record.admitted_at = Some(1234);
        let mut index = Index::create(&directory.0.join("inventory")).unwrap();
        index::commit(&mut index, &metadata, &[], &[]).unwrap();
        index.inject(Some(fault));
        assert!(index::commit(&mut index, &metadata, std::slice::from_ref(&record), &[]).is_err());
        assert!(!index.is_usable());
        assert!(index::get(&mut index, &record.id).is_err());
        drop(index);
        let mut recovered = Index::open(&directory.0.join("inventory"), false).unwrap();
        assert_eq!(
            index::get(&mut recovered, &record.id).unwrap(),
            Some(record.clone())
        );
        assert_eq!(index::count(&mut recovered, "fixture", 0).unwrap(), 1);
        // An idempotent replay must not duplicate reservations or counters.
        index::commit(
            &mut recovered,
            &metadata,
            std::slice::from_ref(&record),
            &[],
        )
        .unwrap();
        assert_eq!(index::count(&mut recovered, "fixture", 0).unwrap(), 1);
        assert_eq!(
            index::get(&mut recovered, &record.id).unwrap(),
            Some(record)
        );
    }
}

#[test]
fn dangling_ownership_and_corrupt_record_reads_return_errors_instead_of_absence() {
    let directory = Directory::new();
    let config = config(&directory.0);
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    let record = demand(&config, 81, 10);
    let mut patch = store.state.clone();
    patch.demands.insert(record.id.clone(), record.clone());
    store.save_patch(patch).unwrap();
    drop(store);
    let mut index = Index::open(&directory.0.join("requesters.idx"), false).unwrap();
    let primary = index::primary_cursor(&record.id).unwrap();
    index
        .commit(vec![Change::Put(primary, b"broken-manifest".to_vec())])
        .unwrap();
    drop(index);
    let store = RequesterStore::open(&directory.0, &config, true).unwrap();
    assert!(store.demand(&record.id).is_err());
    assert!(store.account_page("fixture", None, 10).is_err());
    assert!(store.demands_page(None, 10).is_err());
    drop(store);
    let mut index = Index::open(&directory.0.join("requesters.idx"), false).unwrap();
    index.commit(vec![Change::Delete(primary)]).unwrap();
    drop(index);
    let store = RequesterStore::open(&directory.0, &config, true).unwrap();
    assert!(store.account_page("fixture", None, 10).is_err());
}

#[test]
fn only_a_completed_poll_generation_authorizes_removing_unseen_origins() {
    let directory = Directory::new();
    let config = config(&directory.0);
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    let first = demand(&config, 1, 10);
    let second = demand(&config, 2, 10);
    let mut patch = store.state.clone();
    patch.demands.insert(first.id.clone(), first.clone());
    patch.demands.insert(second.id.clone(), second.clone());
    store.save_patch(patch).unwrap();
    // A metadata-only failed/partial poll has no completed generation proof.
    let mut patch = store.state.clone();
    patch.accounts.get_mut("fixture").unwrap().last_error = Some("Plex account poll failed".into());
    store.save_patch(patch).unwrap();
    assert_eq!(store.demand(&second.id).unwrap(), Some(second.clone()));
    let mut patch = store.state.clone();
    patch.accounts.get_mut("fixture").unwrap().cursor = 1;
    let origins = first.origins.clone();
    store
        .save_poll_patch(patch, Some(publish("fixture", &origins, 1)))
        .unwrap();
    assert_eq!(store.demand(&first.id).unwrap(), Some(first.clone()));
    let removed = store.demand(&second.id).unwrap().unwrap();
    assert_eq!(removed.state, "removed");
    assert!(removed.origins.is_empty());
    assert!(!store.poll_ready("fixture").unwrap());
    drop(store);
    let store = RequesterStore::open(&directory.0, &config, true).unwrap();
    assert_eq!(store.demand(&first.id).unwrap(), Some(first));
    assert_eq!(store.demand(&second.id).unwrap().unwrap().state, "removed");
    assert!(!store.poll_ready("fixture").unwrap());
}

fn publish<'a>(account: &'a str, origins: &'a BTreeSet<String>, epoch: u64) -> Stage<'a> {
    Stage {
        account,
        origins,
        progress: Progress::Publish(Sweep::new(epoch, 1)),
    }
}

fn staged<'a>(account: &'a str, origins: &'a BTreeSet<String>, epoch: u64) -> Stage<'a> {
    let mut sweep = Sweep::new(epoch, 1);
    sweep.cursor.position = 7;
    sweep.cursor.anchor = Some([7; 32]);
    Stage {
        account,
        origins,
        progress: Progress::Continue(sweep),
    }
}

fn origin_rows(directory: &Path) -> usize {
    let mut index = Index::open(&directory.join("requesters.idx"), true).unwrap();
    let (start, end) = index::range(14, &[]);
    let mut after = None;
    let mut rows = 0;
    loop {
        let page = index
            .page(&crate::inventory::Query {
                start,
                end: Some(end),
                after,
                limit: MAX_PAGE_ROWS,
                sequence: None,
            })
            .unwrap();
        rows += page.entries.len();
        after = page.next;
        if after.is_none() {
            return rows;
        }
    }
}

#[test]
fn a_running_sweep_never_removes_and_keeps_new_origins_visible() {
    let directory = Directory::new();
    let config = config(&directory.0);
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    let first = demand(&config, 1, 10);
    let second = demand(&config, 2, 10);
    let mut patch = store.state.clone();
    patch.demands.insert(first.id.clone(), first.clone());
    patch.accounts.get_mut("fixture").unwrap().cursor = 1;
    store
        .save_poll_patch(patch, Some(publish("fixture", &first.origins, 1)))
        .unwrap();
    while !store.poll_ready("fixture").unwrap() {
        store.reconcile_origins("fixture").unwrap();
    }
    // Generation 2 stages only the second demand: the first keeps its origin
    // because the sweep has not finished, and the new one is not removed by
    // the still-published generation 1.
    let mut patch = store.state.clone();
    patch.demands.insert(second.id.clone(), second.clone());
    store
        .save_poll_patch(patch, Some(staged("fixture", &second.origins, 2)))
        .unwrap();
    assert_eq!(store.demand(&first.id).unwrap(), Some(first.clone()));
    assert_eq!(store.demand(&second.id).unwrap(), Some(second.clone()));
    let sweep = store.sweep("fixture").unwrap().unwrap();
    assert_eq!((sweep.epoch, sweep.cursor.position), (2, 7));
    drop(store);
    // The sweep survives a restart and publishing it removes the unseen origin.
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    assert_eq!(store.sweep("fixture").unwrap().unwrap().epoch, 2);
    let mut patch = store.state.clone();
    patch.accounts.get_mut("fixture").unwrap().cursor = 2;
    store
        .save_poll_patch(patch, Some(publish("fixture", &BTreeSet::new(), 2)))
        .unwrap();
    assert!(store.sweep("fixture").unwrap().is_none());
    assert_eq!(store.demand(&first.id).unwrap().unwrap().state, "removed");
    assert_eq!(store.demand(&second.id).unwrap(), Some(second));
}

#[test]
fn sweep_generations_only_advance_and_stale_origin_rows_are_collected() {
    let directory = Directory::new();
    let config = config(&directory.0);
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    let origins: BTreeSet<String> = (0..300).map(|n| format!("movie:synthetic-{n}")).collect();
    // A generation at or below the published cursor is refused.
    let patch = store.state.clone();
    assert!(
        store
            .save_poll_patch(patch, Some(staged("fixture", &origins, 0)))
            .is_err()
    );
    let mut peak = 0;
    for epoch in 1..=12_u64 {
        // Each generation stages its origins, then is published.
        let patch = store.state.clone();
        store
            .save_poll_patch(patch, Some(staged("fixture", &origins, epoch)))
            .unwrap();
        let mut patch = store.state.clone();
        patch.accounts.get_mut("fixture").unwrap().cursor = epoch;
        store
            .save_poll_patch(patch, Some(publish("fixture", &BTreeSet::new(), epoch)))
            .unwrap();
        while !store.poll_ready("fixture").unwrap() {
            store.reconcile_origins("fixture").unwrap();
        }
        drop(store);
        peak = peak.max(origin_rows(&directory.0));
        store = RequesterStore::open(&directory.0, &config, false).unwrap();
        store.initialize().unwrap();
    }
    // Without collection twelve generations would retain 3,600 rows; each
    // commit examines a bounded window, so at most a few generations remain.
    assert!(peak <= 4 * origins.len(), "{peak} origin rows retained");
    let patch = store.state.clone();
    assert!(
        store
            .save_poll_patch(patch, Some(staged("fixture", &origins, 12)))
            .is_err()
    );
}

#[test]
fn full_disk_at_every_migration_flush_preserves_legacy_and_restarts() {
    for boundary in 0..1000 {
        let directory = Directory::new();
        let config = config(&directory.0);
        let mut initial = RequesterStore::open(&directory.0, &config, true)
            .unwrap()
            .state
            .clone();
        for n in 0..3 {
            let row = demand(&config, n, 10);
            initial.demands.insert(row.id.clone(), row);
        }
        let original = legacy_bytes(&initial);
        write_private(&directory.0.join("requesters.bin"), &original);
        let result = crate::store::with_full_disk_after(boundary, || {
            RequesterStore::open(&directory.0, &config, false).and_then(|mut s| s.initialize())
        });
        let error = match result {
            Ok(()) => {
                assert!(boundary > 10);
                return;
            }
            Err(error) => error,
        };
        assert!(
            error.starts_with("Requester storage: "),
            "boundary {boundary}: {error}"
        );
        assert!(
            error.contains("disk") || error.contains(crate::store::STORAGE_FULL),
            "boundary {boundary}: {error}"
        );
        assert!(!error.contains(directory.0.to_str().unwrap()), "{error}");
        // The original remains at its live name until marker publication;
        // afterwards the exact private backup must already exist.
        let live = fs::read(directory.0.join("requesters.bin")).unwrap();
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
        let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
        store.initialize().unwrap();
        assert_eq!(all_ids(&store).len(), 3);
    }
    panic!("migration fixture exceeded its durability boundary budget");
}

#[test]
fn requester_backlog_tracks_admission_states_and_survives_restart() {
    let directory = Directory::new();
    let config = config(&directory.0);
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    let mut patch = store.state.clone();
    for n in 0..200 {
        let row = demand(&config, n, 1);
        patch.demands.insert(row.id.clone(), row);
    }
    store.save_patch(patch).unwrap();
    let value = store.backlog(u64::MAX).unwrap();
    assert_eq!(value.get("due"), Some(&json::Value::from("200")));
    assert_eq!(
        value.get("last_completed_pass_at"),
        Some(&json::Value::Null)
    );
    let oldest = value.get("oldest_due_at").cloned();
    let mut row = demand(&config, 0, 1);
    row.state = "conflict".into();
    row.outcome = "conflict".into();
    let mut patch = store.state.clone();
    patch.demands.insert(row.id.clone(), row.clone());
    store.save_patch(patch).unwrap();
    assert_eq!(
        store
            .backlog(u64::MAX)
            .unwrap()
            .get("oldest_due_at")
            .cloned(),
        oldest
    );
    row.state = "removed".into();
    row.outcome = "removed".into();
    row.origins.clear();
    let mut patch = store.state.clone();
    patch.demands.insert(row.id.clone(), row);
    store.save_patch(patch).unwrap();
    store.finish_backlog_pass(80).unwrap();
    drop(store);
    let store = RequesterStore::open(&directory.0, &config, true).unwrap();
    let value = store.backlog(u64::MAX).unwrap();
    assert_eq!(value.get("due"), Some(&json::Value::from("199")));
    assert_eq!(
        value.get("last_completed_pass_at"),
        Some(&json::Value::from("80"))
    );
}

#[test]
fn requester_backlog_migration_keeps_unknown_wait_times_and_exact_legacy_backup() {
    let directory = Directory::new();
    let config = config(&directory.0);
    let store = RequesterStore::open(&directory.0, &config, false).unwrap();
    let mut state = store.state.clone();
    for n in 0..3 {
        let row = demand(&config, n, 1);
        state.demands.insert(row.id.clone(), row);
    }
    drop(store);
    let bytes = legacy_bytes(&state);
    write_private(&directory.0.join("requesters.bin"), &bytes);
    let store = RequesterStore::open(&directory.0, &config, true).unwrap();
    assert!(store.backlog(100).is_err());
    drop(store);
    let mut store = RequesterStore::open(&directory.0, &config, false).unwrap();
    store.initialize().unwrap();
    let value = store.backlog(100).unwrap();
    assert_eq!(value.get("due"), Some(&json::Value::from("3")));
    assert_eq!(
        value.get("unknown_due_times"),
        Some(&json::Value::from("3"))
    );
    assert_eq!(value.get("oldest_due_at"), Some(&json::Value::Null));
    let backup = directory.0.join(format!(
        "requesters.legacy-{}.bin",
        super::digest(&sha256(&bytes))
    ));
    assert_eq!(fs::read(backup).unwrap(), bytes);
}

#[test]
#[ignore = "opt-in inventory_growth.py measurements"]
fn inventory_growth_report() {
    let Some(mut f) = crate::growth::Fixture::get("requesters") else {
        return;
    };
    let directory = f.directory.clone();
    let size = f.size;
    let config = config(&directory);
    let mut store = f.measure("open_us", || {
        let mut store = RequesterStore::open(&directory, &config, false).unwrap();
        store.initialize().unwrap();
        store
    });
    if f.build {
        f.measure("build_us", || {
            for start in (1..=size).step_by(100) {
                let mut patch = store.state.clone();
                for n in start..(start + 100).min(size + 1) {
                    let row = demand(&config, n, 1400);
                    patch.demands.insert(row.id.clone(), row);
                    patch.operator_jobs.insert(format!("{n:032x}"));
                }
                store.save_patch(patch).unwrap();
            }
        });
    } else {
        assert_eq!(store.count("fixture", None).unwrap(), size);
        f.measure("page_us", || {
            assert_eq!(store.demands_page(None, 200).unwrap().0.len(), 200);
        });
        f.measure("pass_us", || {
            assert_eq!(store.queue_page("pending", None, 64).unwrap().0.len(), 64);
        });
        f.measure("update_us", || {
            let mut row = store
                .demand(&demand(&config, size, 1400).id)
                .unwrap()
                .unwrap();
            row.origins.insert("synthetic:point-update".into());
            let mut patch = store.state.clone();
            patch.demands.insert(row.id.clone(), row);
            store.save_patch(patch).unwrap();
        });
        let mut after = None;
        let mut count = 0;
        f.measure("walk_us", || {
            loop {
                let (rows, next) = store.demands_page(after, 200).unwrap();
                count += rows.len() as u64;
                after = next;
                if after.is_none() {
                    break;
                }
            }
        });
        assert_eq!(count, size);
    }
    f.finish();
}
