//! Synthetic history growth, migration crash stages and storage-failure proofs.
use super::super::{
    Announcement, Evaluation, Record, Settings, State, digest, index,
    persistence::{AnnouncementStore, Backend, INDEX, MARKER, STAGING},
    routing::{RETRY_ENTRIES, Runtime},
};
use crate::{
    crypto::{random_bytes, sha256},
    inventory::{Change, Fault, Index, Key},
    json::{self, Value},
    store::private_options,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "mynou-irc-index-{}",
            digest(&random_bytes::<16>().unwrap())
        ));
        fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
        Self(p)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn row(n: u64, large: bool) -> Record {
    let binding = "1".repeat(64);
    let mut v = Value::object();
    v.insert(
        "title",
        if large {
            "Fixture.Movie.2024.".to_owned() + &"X".repeat(1900)
        } else {
            "Fixture.Movie.2024.1080p".into()
        },
    );
    v.insert("media_title", "Fixture Movie");
    v.insert("kind", "movie");
    v.insert("year", 2024_u32);
    v.insert("tmdb_id", n.to_string());
    v.insert("info_hash", "1234567890abcdef1234567890abcdef12345678");
    let announcement = Announcement::from_json(&v).unwrap();
    Record {
        id: announcement.id(&binding),
        source_id: "local".into(),
        binding,
        announcement,
        evaluations: if large {
            (0..64)
                .map(|i| Evaluation {
                    rule_id: format!("r{i:02}{}", "x".repeat(60)),
                    profile: "x".repeat(64),
                    outcome: "matched".into(),
                })
                .collect()
        } else {
            Vec::new()
        },
        fingerprint: "2".repeat(64),
        decision: "pending".into(),
        revision: 1,
        first_seen: n,
        decided_at: None,
        route: None,
        admission: None,
    }
}
fn state(rows: Vec<Record>) -> State {
    State {
        revision: 17,
        bindings: BTreeMap::from([("local".into(), "1".repeat(64))]),
        records: rows.into_iter().map(|r| (r.id.clone(), r)).collect(),
    }
}
fn legacy(dir: &Path, state: &State) -> Vec<u8> {
    let payload = json::stringify(&state.to_json()).into_bytes();
    let mut bytes = b"MYNOUI03".to_vec();
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&sha256(&bytes));
    let mut file = private_options()
        .write(true)
        .create_new(true)
        .open(dir.join("announcements.bin"))
        .unwrap();
    file.write_all(&bytes).unwrap();
    file.sync_all().unwrap();
    bytes
}
fn contents(path: &Path) -> Vec<(PathBuf, Vec<u8>, u32)> {
    let mut found = Vec::new();
    for e in fs::read_dir(path).unwrap() {
        let p = e.unwrap().path();
        let m = fs::symlink_metadata(&p).unwrap();
        if m.is_dir() {
            found.extend(contents(&p));
        } else {
            found.push((p.clone(), fs::read(p).unwrap(), m.permissions().mode()));
        }
    }
    found.sort();
    found
}
fn open(dir: &Path, read_only: bool) -> AnnouncementStore {
    AnnouncementStore::open(dir, &Settings::default(), read_only).unwrap()
}
fn requests(store: &AnnouncementStore) -> u64 {
    match &*store.backend.borrow() {
        Backend::Ready(i) => i.requests(),
        _ => 0,
    }
}
#[test]
fn disk_history_exceeds_both_caps_with_bounded_points_pages_and_restart() {
    let dir = Directory::new();
    let initial = state(vec![row(1, false)]);
    legacy(&dir.0, &initial);
    let mut store = open(&dir.0, false);
    store.initialize().unwrap();
    let mut payload = 0;
    for start in (2..=1202).step_by(100) {
        let rows: Vec<_> = (start..(start + 100).min(1203))
            .map(|n| row(n, true))
            .collect();
        payload += rows
            .iter()
            .map(|r| json::stringify(&r.to_json()).len())
            .sum::<usize>();
        store.save_rows(&rows).unwrap();
    }
    assert!(payload > 8 * 1024 * 1024);
    assert_eq!(store.count().unwrap(), 1202);
    assert!(store.state.records.is_empty());
    let before = requests(&store);
    assert_eq!(store.get(&row(501, true).id).unwrap(), Some(row(501, true)));
    assert!(requests(&store) - before <= 2);
    drop(store);
    let before = contents(&dir.0);
    let store = open(&dir.0, true);
    assert_eq!(store.count().unwrap(), 1202);
    let mut seen = BTreeSet::new();
    let mut after = None;
    let mut previous = u64::MAX;
    loop {
        let before = requests(&store);
        let (rows, next) = store.page(2, after, 200).unwrap();
        assert!(!rows.is_empty() && rows.len() <= 200);
        assert!(requests(&store) - before <= 202);
        assert!(
            rows.iter()
                .map(|r| json::stringify(&r.to_json()).len())
                .sum::<usize>()
                <= crate::inventory::MAX_PAGE_BYTES
        );
        for r in rows {
            assert!(seen.insert(r.id));
            assert!(r.first_seen <= previous);
            previous = r.first_seen;
        }
        after = next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 1202);
    drop(store);
    assert_eq!(contents(&dir.0), before);
}
#[test]
fn every_migration_checkpoint_resumes_exactly_and_preview_is_inert() {
    for stop in 1..=16 {
        let dir = Directory::new();
        let initial = state((1..=401).map(|n| row(n, false)).collect());
        let bytes = legacy(&dir.0, &initial);
        let mut store = open(&dir.0, false);
        store.stop = Some(stop);
        assert!(store.initialize().is_err());
        drop(store);
        let before = contents(&dir.0);
        let preview = open(&dir.0, true);
        assert_eq!(preview.count().unwrap(), 401);
        assert_eq!(
            preview.get(&row(401, false).id).unwrap(),
            Some(row(401, false))
        );
        drop(preview);
        assert_eq!(contents(&dir.0), before);
        let mut store = open(&dir.0, false);
        store.initialize().unwrap();
        assert_eq!(store.state, super::header(&initial));
        assert_eq!(store.count().unwrap(), 401);
        for r in initial.records.values() {
            assert_eq!(store.get(&r.id).unwrap().as_ref(), Some(r));
        }
        assert_eq!(
            fs::read(dir.0.join(format!(
                "announcements.legacy-{}.bin",
                digest(&sha256(&bytes))
            )))
            .unwrap(),
            bytes
        );
        assert!(
            fs::read(dir.0.join("announcements.bin"))
                .unwrap()
                .starts_with(MARKER)
        );
        // Older snapshot decoders reject the marker.
        assert!(super::parse(&fs::read(dir.0.join("announcements.bin")).unwrap()).is_err());
    }
}
#[test]
fn pending_migration_redo_is_bound_to_proof_and_resumes() {
    let dir = Directory::new();
    let initial = state((1..=201).map(|n| row(n, false)).collect());
    let bytes = legacy(&dir.0, &initial);
    let source = sha256(&bytes);
    let mut store = open(&dir.0, false);
    store.stop = Some(3);
    assert!(store.initialize().is_err());
    drop(store);
    let staging = dir.0.join(format!("{STAGING}-{}", digest(&source)));
    let mut index = Index::open(&staging, false).unwrap();
    index.inject(Some(Fault::Published));
    let rows: Vec<_> = initial.records.values().take(200).cloned().collect();
    assert!(
        index::commit(
            &mut index,
            &super::header(&initial),
            &rows,
            rows.last().map(|r| r.id.as_str())
        )
        .is_err()
    );
    drop(index);
    let before = contents(&dir.0);
    drop(open(&dir.0, true));
    assert_eq!(contents(&dir.0), before);
    let mut store = open(&dir.0, false);
    store.initialize().unwrap();
    assert_eq!(store.count().unwrap(), 201);
    for r in initial.records.values() {
        assert_eq!(store.get(&r.id).unwrap().as_ref(), Some(r));
    }
}
#[test]
fn save_failures_never_evict_or_claim_success_and_uncertainty_requires_restart() {
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let dir = Directory::new();
        legacy(&dir.0, &state(vec![row(1, false)]));
        let mut store = open(&dir.0, false);
        store.initialize().unwrap();
        if let Backend::Ready(i) = &mut *store.backend.borrow_mut() {
            i.inject(Some(fault));
        }
        assert!(store.save_rows(&[row(2, false)]).is_err());
        if !matches!(fault, Fault::Staged) {
            assert!(store.get(&row(1, false).id).is_err());
            assert!(store.save_rows(&[row(3, false)]).is_err());
        }
        drop(store);
        let store = open(&dir.0, false);
        assert_eq!(store.get(&row(1, false).id).unwrap(), Some(row(1, false)));
        assert_eq!(
            store.get(&row(2, false).id).unwrap().is_some(),
            !matches!(fault, Fault::Staged)
        );
        assert!(store.get(&row(3, false).id).unwrap().is_none());
    }
}
#[test]
fn markers_backups_proofs_and_foreign_staging_are_rejected_without_repair() {
    for corruption in [
        "backup",
        "public_backup",
        "linked_backup",
        "foreign",
        "source",
        "extra",
        "public_staging",
        "symlink_staging",
        "proof",
    ] {
        let dir = Directory::new();
        let initial = state(vec![row(1, false)]);
        let bytes = legacy(&dir.0, &initial);
        let source = sha256(&bytes);
        let mut store = open(&dir.0, false);
        store.stop = Some(3);
        assert!(store.initialize().is_err());
        drop(store);
        let backup = dir
            .0
            .join(format!("announcements.legacy-{}.bin", digest(&source)));
        let staging = dir.0.join(format!("{STAGING}-{}", digest(&source)));
        match corruption {
            "backup" => fs::write(&backup, b"wrong").unwrap(),
            "public_backup" => {
                fs::set_permissions(&backup, fs::Permissions::from_mode(0o644)).unwrap()
            }
            "linked_backup" => fs::hard_link(&backup, dir.0.join("link")).unwrap(),
            "foreign" => {
                fs::remove_dir_all(&staging).unwrap();
                drop(Index::create(&staging).unwrap());
            }
            "source" => {
                let mut i = Index::open(&staging, false).unwrap();
                i.commit(vec![Change::PutInline(index::SOURCE, vec![8; 32])])
                    .unwrap();
            }
            "extra" => {
                let mut i = Index::open(&staging, false).unwrap();
                let mut k = [0; 64];
                k[0] = 9;
                i.commit(vec![Change::PutInline(k, vec![1])]).unwrap();
            }
            "public_staging" => {
                fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).unwrap()
            }
            "symlink_staging" => {
                fs::rename(&staging, dir.0.join("other")).unwrap();
                symlink(dir.0.join("other"), &staging).unwrap();
            }
            "proof" => fs::write(
                dir.0
                    .join(format!("announcements.migration-{}.bin", digest(&source))),
                b"wrong",
            )
            .unwrap(),
            _ => unreachable!(),
        }
        let source_before = fs::read(dir.0.join("announcements.bin")).unwrap();
        let mut store = open(&dir.0, false);
        assert!(store.initialize().is_err());
        assert_eq!(
            fs::read(dir.0.join("announcements.bin")).unwrap(),
            source_before
        );
    }
}
#[test]
fn restored_legacy_bytes_cannot_overwrite_acknowledged_index_changes() {
    let dir = Directory::new();
    let initial = state(vec![row(1, false)]);
    let bytes = legacy(&dir.0, &initial);
    let mut store = open(&dir.0, false);
    store.initialize().unwrap();
    store.save_rows(&[row(2, false)]).unwrap();
    drop(store);
    fs::write(dir.0.join("announcements.bin"), bytes).unwrap();
    let before = contents(&dir.0);
    let mut store = open(&dir.0, false);
    assert!(store.initialize().is_err());
    drop(store);
    assert_eq!(contents(&dir.0), before);
    let mut index = Index::open(&dir.0.join(INDEX), true).unwrap();
    assert_eq!(
        index::get(&mut index, &row(2, false).id).unwrap(),
        Some(row(2, false))
    );
}
#[test]
fn cursor_queries_reject_mixed_unknown_duplicate_or_foreign_inputs() {
    for query in [
        "after=x",
        "after=",
        "offset=0",
        "limit=0",
        "limit=201",
        "limit=+1",
        "limit=1&limit=2",
        "after=x&offset=0",
        "limit=1&",
        "wat=1",
    ] {
        assert!(super::super::cursor_page(query).is_err(), "{query}");
    }
    let mut k: Key = [0; 64];
    k[0] = 2;
    let text = index::cursor(&k);
    assert_eq!(
        super::super::cursor_page(&format!("after={text}&limit=200")).unwrap(),
        (Some(text), 200)
    );
}

#[test]
fn partial_creation_writes_resume_only_when_they_match_the_prepublished_identity() {
    for artifact in [
        "identity",
        "tree",
        "corrupt_identity",
        "corrupt_tree",
        "owned",
    ] {
        let dir = Directory::new();
        let initial = state(vec![row(1, false)]);
        let bytes = legacy(&dir.0, &initial);
        let source = sha256(&bytes);
        let mut store = open(&dir.0, false);
        store.stop = Some(2);
        assert!(store.initialize().is_err());
        drop(store);
        let stage = dir.0.join(format!("{STAGING}-{}", digest(&source)));
        let mut held = None;
        if artifact.contains("identity") {
            fs::remove_file(stage.join("tree.bin")).unwrap();
            fs::remove_dir(stage.join("objects")).unwrap();
            let mut id = fs::read(stage.join("identity.bin")).unwrap();
            id.truncate(23);
            if artifact.starts_with("corrupt") {
                id[10] ^= 1;
            }
            fs::write(stage.join("identity.bin"), id).unwrap();
        } else {
            let mut tree = fs::read(stage.join("tree.bin")).unwrap();
            tree.truncate(128);
            if artifact.starts_with("corrupt") {
                tree[10] ^= 1;
            }
            fs::write(stage.join("tree.bin"), tree).unwrap();
            if artifact == "owned" {
                let f = private_options()
                    .read(true)
                    .write(true)
                    .open(stage.join(".lock"))
                    .unwrap();
                crate::store::try_lock_file(&f).unwrap();
                held = Some(f);
            }
        }
        let before = contents(&dir.0);
        let mut store = open(&dir.0, false);
        if artifact.starts_with("corrupt") || artifact == "owned" {
            assert!(store.initialize().is_err());
            drop(store);
            assert_eq!(contents(&dir.0), before);
        } else {
            store.initialize().unwrap();
            assert_eq!(store.get(&row(1, false).id).unwrap(), Some(row(1, false)));
        }
        drop(held);
    }
}

/// A store whose pending table holds rows 1..=n, with ids in pending order.
fn pending(dir: &Path, n: u64) -> (AnnouncementStore, Vec<String>) {
    legacy(dir, &state(vec![row(1, false)]));
    let mut store = open(dir, false);
    store.initialize().unwrap();
    let rows: Vec<_> = (2..=n).map(|n| row(n, false)).collect();
    for chunk in rows.chunks(100) {
        store.save_rows(chunk).unwrap();
    }
    let mut ids: Vec<_> = (1..=n).map(|n| row(n, false).id).collect();
    ids.sort();
    (store, ids)
}
fn selected(runtime: &mut Runtime, store: &AnnouncementStore, at: Instant) -> Option<String> {
    runtime.select(store, at, |_| true).unwrap().map(|r| r.id)
}

#[test]
fn a_full_retry_table_never_blocks_announcements_outside_it() {
    let dir = Directory::new();
    let (store, ids) = pending(&dir.0, RETRY_ENTRIES as u64 + 1);
    let eligible = row(RETRY_ENTRIES as u64 + 1, false).id;
    let mut runtime = Runtime::default();
    let now = Instant::now();
    // Every other announcement fails once, one attempt per turn, in pending order.
    let mut attempted = Vec::new();
    for _ in 0..RETRY_ENTRIES {
        let r = runtime
            .select(&store, now, |r| r.id != eligible)
            .unwrap()
            .unwrap();
        runtime.back_off(r.id.clone(), now);
        attempted.push(r.id);
    }
    let mut failing: Vec<_> = ids.iter().filter(|id| **id != eligible).cloned().collect();
    assert_eq!(attempted, failing);
    failing.retain(|id| runtime.backing_off(id, now));
    assert_eq!(failing.len(), RETRY_ENTRIES);
    // The full table holds no slot for it, yet the eligible row is reached at once.
    assert_eq!(selected(&mut runtime, &store, now), Some(eligible.clone()));
    // Remembering it keeps the bound by dropping the entry closest to expiry.
    let later = now + Duration::from_secs(1);
    assert_eq!(runtime.dropped(), 0);
    runtime.back_off(eligible.clone(), later);
    assert_eq!(runtime.dropped(), 1);
    runtime.back_off(eligible.clone(), later);
    assert_eq!(runtime.dropped(), 1);
    assert!(runtime.backing_off(&eligible, later));
    assert!(!runtime.backing_off(&attempted[0], later));
    assert_eq!(
        ids.iter()
            .filter(|id| runtime.backing_off(id, later))
            .count(),
        RETRY_ENTRIES
    );
    let retried: Vec<_> = (0..2)
        .filter_map(|_| selected(&mut runtime, &store, later))
        .collect();
    assert_eq!(retried, [attempted[0].clone()]);
    // Expired entries are forgotten and every row is routable again.
    let expired = now + Duration::from_secs(32);
    assert!(ids.iter().all(|id| !runtime.backing_off(id, expired)));
    runtime.back_off(ids[0].clone(), expired);
    assert_eq!(
        ids.iter()
            .filter(|id| runtime.backing_off(id, expired))
            .count(),
        1
    );
}

#[test]
fn a_page_that_is_entirely_backing_off_never_hides_later_rows() {
    let dir = Directory::new();
    let (store, ids) = pending(&dir.0, 2 * RETRY_ENTRIES as u64 + 1);
    let mut runtime = Runtime::default();
    let now = Instant::now();
    for id in &ids[..RETRY_ENTRIES] {
        runtime.back_off(id.clone(), now);
    }
    // One idle turn skips the waiting page; later rows then follow in order.
    let turns: Vec<_> = (0..3)
        .map(|_| selected(&mut runtime, &store, now))
        .collect();
    assert_eq!(
        turns,
        [
            None,
            Some(ids[RETRY_ENTRIES].clone()),
            Some(ids[RETRY_ENTRIES + 1].clone())
        ]
    );
}

/// The legacy snapshot, its preserved backup and the initial staging lock,
/// swapped after inspection, are refused at open time without blocking.
#[test]
fn swapped_migration_files_are_refused_at_open_without_blocking() {
    use crate::store::open_tests::{Swap, swapped};
    for path in ["snapshot", "backup", "lock"] {
        for swap in [Swap::Symlink, Swap::Fifo, Swap::Replaced] {
            let dir = Directory::new();
            let bytes = legacy(&dir.0, &state(vec![row(1, false)]));
            let source = sha256(&bytes);
            let (name, skip, opened, special) = match path {
                "snapshot" => (
                    "announcements.bin".to_owned(),
                    0,
                    "IRC: cannot open history",
                    "IRC: invalid history file, permissions or size",
                ),
                "backup" => {
                    let mut store = open(&dir.0, false);
                    store.stop = Some(1);
                    assert!(store.initialize().is_err());
                    (
                        format!("announcements.legacy-{}.bin", digest(&source)),
                        0,
                        "IRC: cannot open history",
                        "IRC: invalid history file, permissions or size",
                    )
                }
                _ => {
                    // An unpopulated initial staging, as in the resume test.
                    let mut store = open(&dir.0, false);
                    store.stop = Some(2);
                    assert!(store.initialize().is_err());
                    drop(store);
                    let stage = dir.0.join(format!("{STAGING}-{}", digest(&source)));
                    let mut tree = fs::read(stage.join("tree.bin")).unwrap();
                    tree.truncate(128);
                    fs::write(stage.join("tree.bin"), tree).unwrap();
                    // The preflight reads the lock twice before opening it.
                    (
                        ".lock".to_owned(),
                        2,
                        "IRC: cannot open creation lock",
                        "IRC: creation lock must be a regular file",
                    )
                }
            };
            let directory = dir.0.clone();
            let error = swapped(&name, skip, swap, move || {
                AnnouncementStore::open(&directory, &Settings::default(), false)?.initialize()
            });
            match swap {
                Swap::Symlink => assert_eq!(error, opened, "{path}"),
                Swap::Fifo => assert!(error == opened || error == special, "{path}: {error}"),
                Swap::Replaced if path == "lock" => {
                    assert_eq!(error, "IRC: creation lock changed during open")
                }
                Swap::Replaced => assert_eq!(error, "IRC: history changed during open", "{path}"),
            }
            if path != "snapshot" {
                assert_eq!(fs::read(dir.0.join("announcements.bin")).unwrap(), bytes);
            }
        }
    }
}

#[test]
fn full_disk_at_every_migration_flush_preserves_legacy_and_restarts() {
    for boundary in 0..1000 {
        let directory = Directory::new();
        let initial = state((1..=3).map(|n| row(n, false)).collect());
        let original = legacy(&directory.0, &initial);
        let result = crate::store::with_full_disk_after(boundary, || {
            AnnouncementStore::open(&directory.0, &Settings::default(), false)
                .and_then(|mut s| s.initialize())
        });
        let error = match result {
            Ok(()) => {
                assert!(boundary > 10);
                return;
            }
            Err(error) => error,
        };
        assert!(
            error.starts_with("IRC announcement storage: "),
            "boundary {boundary}: {error}"
        );
        assert!(
            error.contains("disk") || error.contains(crate::store::STORAGE_FULL),
            "boundary {boundary}: {error}"
        );
        assert!(!error.contains(directory.0.to_str().unwrap()), "{error}");
        // The original remains at its live name until marker publication;
        // afterwards the exact private backup must already exist.
        let live = fs::read(directory.0.join("announcements.bin")).unwrap();
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
        let mut store = open(&directory.0, false);
        store.initialize().unwrap();
        for r in initial.records.values() {
            assert_eq!(store.get(&r.id).unwrap().as_ref(), Some(r));
        }
    }
    panic!("migration fixture exceeded its durability boundary budget");
}

#[test]
fn irc_backlog_excludes_decisions_and_routes_and_keeps_original_wait_age() {
    let directory = Directory::new();
    legacy(
        &directory.0,
        &state((1..206).map(|n| row(n, false)).collect()),
    );
    let mut store = open(&directory.0, false);
    store.initialize().unwrap();
    let value = store.backlog(500).unwrap();
    assert_eq!(value.get("due"), Some(&Value::from("205")));
    assert_eq!(value.get("oldest_due_at"), Some(&Value::from("1")));
    let mut decided = store.get(&row(1, false).id).unwrap().unwrap();
    decided.decision = "dismissed".into();
    decided.decided_at = Some(400);
    decided.revision += 1;
    store.save_rows(&[decided]).unwrap();
    store.finish_backlog_pass(500).unwrap();
    drop(store);
    let store = open(&directory.0, true);
    let value = store.backlog(500).unwrap();
    assert_eq!(value.get("due"), Some(&Value::from("204")));
    assert_eq!(value.get("oldest_due_at"), Some(&Value::from("2")));
    assert_eq!(
        value.get("last_completed_pass_at"),
        Some(&Value::from("500"))
    );
}

#[test]
#[ignore = "opt-in inventory_growth.py measurements"]
fn inventory_growth_report() {
    let Some(mut f) = crate::growth::Fixture::get("irc") else {
        return;
    };
    let directory = f.directory.clone();
    let size = f.size;
    let mut store = f.measure("open_us", || {
        let mut store = open(&directory, false);
        store.state.bindings.insert("local".into(), "1".repeat(64));
        store.initialize().unwrap();
        store
    });
    if f.build {
        f.measure("build_us", || {
            for start in (1..=size).step_by(100) {
                let rows: Vec<_> = (start..(start + 100).min(size + 1))
                    .map(|n| row(n, true))
                    .collect();
                store.save_rows(&rows).unwrap();
            }
        });
    } else {
        f.measure("page_us", || {
            assert_eq!(store.page(2, None, 200).unwrap().0.len(), 200);
        });
        f.measure("pass_us", || {
            assert_eq!(store.page(3, None, 200).unwrap().0.len(), 200);
        });
        f.measure("update_us", || {
            let mut changed = row(size, true);
            changed.revision += 1;
            store.save_rows(&[changed]).unwrap();
        });
        let mut after = None;
        let mut count = 0;
        f.measure("walk_us", || {
            loop {
                let (rows, next) = store.page(2, after, 200).unwrap();
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
