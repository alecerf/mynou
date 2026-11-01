//! Synthetic history growth, crash stages and storage-failure proofs.
use super::super::{
    Announcement, Evaluation, Record, Settings, digest, index,
    persistence::{AnnouncementStore, Backend},
    routing::{RETRY_ENTRIES, Runtime},
};
use crate::{
    crypto::random_bytes,
    inventory::{Fault, Key},
    json::{self, Value},
};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
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
/// A store bound to the `local` source and holding `rows`.
fn seeded(dir: &Path, rows: &[Record]) -> AnnouncementStore {
    let mut store = open(dir, false);
    store.state.bindings.insert("local".into(), "1".repeat(64));
    store.initialize().unwrap();
    for chunk in rows.chunks(100) {
        store.save_rows(chunk).unwrap();
    }
    store
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
    let mut store = seeded(&dir.0, &[row(1, false)]);
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
        let mut store = seeded(&dir.0, &[row(1, false)]);
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

/// A store whose pending table holds rows 1..=n, with ids in pending order.
fn pending(dir: &Path, n: u64) -> (AnnouncementStore, Vec<String>) {
    let rows: Vec<_> = (1..=n).map(|n| row(n, false)).collect();
    let store = seeded(dir, &rows);
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

#[test]
fn irc_backlog_excludes_decisions_and_routes_and_keeps_original_wait_age() {
    let directory = Directory::new();
    let rows: Vec<_> = (1..206).map(|n| row(n, false)).collect();
    let mut store = seeded(&directory.0, &rows);
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
