//! Synthetic growth, recovery and provenance fixtures proposed by QA.
use super::*;
use crate::inventory::Fault;
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mynou-feed-index-{}",
            crate::requesters::digest(&crate::crypto::random_bytes::<16>().unwrap())
        ));
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
fn record(n: u64) -> Record {
    let mut record = Record::new(format!("source-{n:057x}"), format!("{n:064x}"));
    record.revision = u64::MAX;
    record.baselined_at = u64::MAX;
    record.last_success_at = u64::MAX;
    record.successes = u64::MAX;
    record.new_entries = u64::MAX;
    record
}
fn contents(path: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    fn walk(path: &Path, rows: &mut BTreeMap<PathBuf, (Vec<u8>, u32)>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                walk(&path, rows);
            } else {
                rows.insert(
                    path.clone(),
                    (
                        fs::read(&path).unwrap(),
                        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    ),
                );
            }
        }
    }
    let mut rows = BTreeMap::new();
    walk(path, &mut rows);
    rows
}
fn index(storage: &mut Storage) -> &mut Index {
    match &mut storage.backend {
        Backend::Ready(index) => index,
        Backend::Absent => panic!("indexed fixture"),
    }
}

#[test]
fn retained_growth_exceeds_old_count_and_bytes_with_bounded_pages_and_startup() {
    let directory = Directory::new();
    let mut storage = Storage::open(&directory.0, false).unwrap();
    storage.initialize().unwrap();
    let mut encoded_bytes = 0;
    for start in (1..=10_001_u64).step_by(200) {
        let batch: Vec<_> = (start..(start + 200).min(10_002))
            .map(|n| {
                let mut row = record(n);
                if n <= 2_000 {
                    row.baselined = true;
                    row.seen = (0..16).map(|id| format!("{id:032x}")).collect();
                    row.cursor = row.seen.last().unwrap().clone();
                }
                row
            })
            .collect();
        encoded_bytes += batch
            .iter()
            .map(|row| json::stringify(&row.to_json()).len())
            .sum::<usize>();
        put_with(index(&mut storage), &batch, Vec::new()).unwrap();
    }
    assert!(encoded_bytes > 4 * 1024 * 1024);
    for start in (1..=2_000_u64).step_by(200) {
        let batch: Vec<_> = (start..(start + 200).min(2_001)).map(record).collect();
        put_with(index(&mut storage), &batch, Vec::new()).unwrap();
    }
    assert_eq!(counters(index(&mut storage)).unwrap(), (10_001, 0));
    let before = index(&mut storage).requests();
    assert_eq!(
        storage.row(&record(10_001).id).unwrap(),
        Some(record(10_001))
    );
    assert!(index(&mut storage).requests() - before <= 2);
    let mut seen = BTreeSet::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let before = index(&mut storage).requests();
        let page = storage.page(after, 200).unwrap();
        assert!(index(&mut storage).requests() - before <= 3);
        assert_eq!(page.total, 10_001);
        assert!(!page.rows.is_empty());
        assert!(page.rows.len() <= 200);
        for row in page.rows {
            assert!(!row.baselined);
            assert!(row.seen.is_empty());
            assert!(seen.insert(row.id));
        }
        pages += 1;
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(pages, 51);
    assert_eq!(seen.len(), 10_001);
    drop(storage);
    let before = contents(&directory.0);
    let mut preview = Storage::open(&directory.0, true).unwrap();
    assert_eq!(preview.total().unwrap(), 10_001);
    assert_eq!(
        preview.row(&record(10_001).id).unwrap(),
        Some(record(10_001))
    );
    assert!(preview.commit(record(10_002)).is_err());
    drop(preview);
    assert_eq!(contents(&directory.0), before);
    let mut value = crate::config::default_json();
    value.insert("indexers", json::Value::Array(Vec::new()));
    let config = crate::config::from_json(&value, &directory.0).unwrap();
    let mut feeds = super::super::FeedStore::open(&directory.0, &config, false).unwrap();
    let before = index(&mut feeds.storage).requests();
    feeds.initialize().unwrap();
    let requests = index(&mut feeds.storage).requests() - before;
    assert!(
        requests <= 2,
        "startup inspected {requests} requests with 10001 inactive sources"
    );
    assert_eq!(feeds.total().unwrap(), 10_001);
}

#[test]
fn cursor_and_active_count_commit_atomically_at_every_crash_boundary() {
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut storage = Storage::open(&directory.0, false).unwrap();
        storage.initialize().unwrap();
        let original = record(1);
        storage.commit(original.clone()).unwrap();
        let mut next = original.clone();
        next.baselined = true;
        next.seen = vec![format!("{:032x}", 123)];
        next.cursor = next.seen[0].clone();
        index(&mut storage).inject(Some(fault));
        assert!(storage.commit(next.clone()).is_err());
        assert!(storage.row(&original.id).is_err());
        assert!(storage.total().is_err());
        assert!(storage.page(None, 200).is_err());
        assert!(storage.commit(record(2)).is_err());
        drop(storage);
        let before = contents(&directory.0);
        // A read-only open replays pending records in memory.
        drop(Storage::open(&directory.0, true).unwrap());
        assert_eq!(contents(&directory.0), before);
        let mut reopened = Storage::open(&directory.0, false).unwrap();
        let expected = if fault == Fault::Staged {
            original.clone()
        } else {
            next.clone()
        };
        assert_eq!(
            reopened.row(&original.id).unwrap(),
            Some(expected.clone()),
            "{fault:?}"
        );
        assert_eq!(
            counters(index(&mut reopened)).unwrap(),
            (1, u64::from(expected.baselined))
        );
        assert_eq!(
            reopened.active(None).unwrap().0,
            if expected.baselined {
                vec![expected]
            } else {
                Vec::new()
            }
        );
        assert!(reopened.row(&record(2).id).unwrap().is_none());
    }
}

#[test]
fn a_corrupt_active_proof_is_never_overwritten() {
    let other = Directory::new();
    let mut storage = Storage::open(&other.0, false).unwrap();
    storage.initialize().unwrap();
    let mut row = record(1);
    row.baselined = true;
    storage.commit(row.clone()).unwrap();
    index(&mut storage)
        .commit(vec![Change::Delete(key(ACTIVE, &row.id))])
        .unwrap();
    assert!(
        storage
            .commit(row)
            .err()
            .unwrap()
            .contains("active reference")
    );
    drop(storage);
    let before = contents(&other.0);
    assert!(
        Storage::open(&other.0, false)
            .err()
            .unwrap()
            .contains("cardinality")
    );
    assert_eq!(contents(&other.0), before);
}

#[test]
fn active_full_windows_resume_after_hydrated_bytes_and_deleted_prior_pages() {
    let directory = Directory::new();
    let mut storage = Storage::open(&directory.0, false).unwrap();
    storage.initialize().unwrap();
    let batch: Vec<_> = (1..=200)
        .map(|n| {
            let mut row = record(n);
            row.baselined = true;
            row.seen = (0..super::super::MAX_SEEN)
                .map(|id| format!("{id:032x}"))
                .collect();
            row.cursor = row.seen[0].clone();
            row
        })
        .collect();
    put_with(index(&mut storage), &batch, Vec::new()).unwrap();
    let mut seen = BTreeSet::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let (rows, next) = storage.active(after).unwrap();
        assert!(!rows.is_empty());
        let bytes: usize = rows
            .iter()
            .map(|row| json::stringify(&row.to_json()).len())
            .sum();
        assert!(bytes <= crate::inventory::MAX_PAGE_BYTES);
        for row in rows {
            assert!(seen.insert(row.id.clone()));
            let mut cleared = row;
            cleared.baselined = false;
            cleared.seen.clear();
            cleared.cursor.clear();
            storage.commit(cleared).unwrap();
        }
        pages += 1;
        after = next;
        if after.is_none() {
            break;
        }
    }
    assert!(pages > 1);
    assert_eq!(seen.len(), 200);
    assert_eq!(counters(index(&mut storage)).unwrap(), (200, 0));
}

#[test]
fn retained_queries_reject_ambiguous_or_unbounded_fields() {
    for query in [
        "limit=0",
        "limit=201",
        "limit=01",
        "limit=2&limit=2",
        "unknown=1",
        "after=bad",
        "after=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    ] {
        assert!(super::super::retained_query(query).is_err(), "{query}");
    }
    assert!(super::super::retained_query(&"x".repeat(257)).is_err());
    let text = "a".repeat(64);
    assert_eq!(
        super::super::retained_query(&format!("limit=200&after={text}")).unwrap(),
        (Some(text), 200)
    );
}

#[test]
fn observation_and_capture_commit_atomically_at_every_crash_boundary() {
    use crate::indexers::feed::generation::Generation;
    use crate::integrations::{FeedEntry, Release};
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut storage = Storage::open(&directory.0, false).unwrap();
        storage.initialize().unwrap();
        let original = Record::new("capture".into(), "1".repeat(64));
        storage.commit(original.clone()).unwrap();
        let mut next = original.clone();
        next.baselined = true;
        next.revision = 1;
        next.seen = vec!["a".repeat(32)];
        next.cursor = next.seen[0].clone();
        let generation = Generation::new(
            &next,
            1,
            vec![FeedEntry {
                id: "a".repeat(32),
                release: Release {
                    title: "Synthetic.Movie.2024.1080p.WEB-DL".into(),
                    url: format!("magnet:?xt=urn:btih:{}", "a".repeat(40)),
                    seeders: 5,
                    size: None,
                    freeleech: None,
                },
            }],
            100,
        )
        .unwrap();
        index(&mut storage).inject(Some(fault));
        assert!(storage.capture(next.clone(), generation.clone()).is_err());
        assert!(storage.pending("capture").is_err());
        assert!(storage.row("capture").is_err());
        assert!(storage.checkpoint(&generation.header, None).is_err());
        drop(storage);
        let mut recovered = Storage::open(&directory.0, false).unwrap();
        let committed = fault != Fault::Staged;
        assert_eq!(
            recovered.row("capture").unwrap(),
            Some(if committed { next } else { original })
        );
        assert_eq!(recovered.pending("capture").unwrap().is_some(), committed);
        assert_eq!(
            recovered.routing_counts().unwrap(),
            if committed { (1, 1) } else { (0, 0) }
        );
        if committed {
            let pending = recovered.pending("capture").unwrap().unwrap();
            assert_eq!(pending.header, generation.header);
            assert_eq!(
                pending.entries[0].release.url,
                generation.entries[0].release.url
            );
            assert!(recovered.checkpoint(&pending.header, None).unwrap());
            assert!(recovered.pending("capture").unwrap().is_none());
            assert_eq!(recovered.routing_counts().unwrap(), (0, 0));
        }
    }
}

#[test]
fn checkpoints_keep_or_advance_the_settled_position_at_every_crash_boundary() {
    use crate::indexers::feed::generation::Generation;
    use crate::integrations::{FeedEntry, Release};
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut storage = Storage::open(&directory.0, false).unwrap();
        storage.initialize().unwrap();
        let mut observed = Record::new("capture".into(), "1".repeat(64));
        observed.baselined = true;
        observed.revision = 1;
        observed.seen = vec!["a".repeat(32)];
        observed.cursor = observed.seen[0].clone();
        let generation = Generation::new(
            &observed,
            1,
            vec![FeedEntry {
                id: "a".repeat(32),
                release: Release {
                    title: "Synthetic.Movie.2024.1080p.WEB-DL".into(),
                    url: format!("magnet:?xt=urn:btih:{}", "a".repeat(40)),
                    seeders: 5,
                    size: None,
                    freeleech: None,
                },
            }],
            100,
        )
        .unwrap();
        storage.capture(observed, generation.clone()).unwrap();
        let mut advanced = generation.header.clone();
        advanced.after = Some(format!("{:016x}{}", 7, "b".repeat(32)));
        index(&mut storage).inject(Some(fault));
        assert!(
            storage
                .checkpoint(&generation.header, Some(&advanced))
                .is_err()
        );
        assert!(storage.pending("capture").is_err());
        drop(storage);
        let mut recovered = Storage::open(&directory.0, false).unwrap();
        let (durable, stale) = if fault == Fault::Staged {
            (generation.header.clone(), advanced)
        } else {
            (advanced, generation.header.clone())
        };
        let pending = recovered.pending("capture").unwrap().unwrap();
        assert_eq!(pending.header, durable);
        assert_eq!(pending.entries.len(), 1);
        assert_eq!(recovered.routing_counts().unwrap(), (1, 1));
        // Only the durable position may settle the generation.
        assert!(recovered.checkpoint(&stale, None).is_err());
        assert_eq!(recovered.routing_counts().unwrap(), (1, 1));
        assert!(recovered.checkpoint(&durable, None).unwrap());
        assert!(recovered.pending("capture").unwrap().is_none());
        assert_eq!(recovered.routing_counts().unwrap(), (0, 0));
    }
}

#[test]
fn feed_backlog_freshness_recovers_without_changing_observations() {
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut storage = Storage::open(&directory.0, false).unwrap();
        storage.initialize().unwrap();
        storage.commit(record(1)).unwrap();
        storage.finish_pass(10).unwrap();
        index(&mut storage).inject(Some(fault));
        assert!(storage.finish_pass(80).is_err());
        drop(storage);
        let mut storage = Storage::open(&directory.0, false).unwrap();
        storage.initialize().unwrap();
        assert_eq!(
            storage.last_pass().unwrap(),
            Some(if fault == Fault::Staged { 10 } else { 80 })
        );
        assert_eq!(storage.row(&record(1).id).unwrap(), Some(record(1)));
    }
}

#[test]
fn inventory_growth_report() {
    let Some(mut f) = crate::growth::Fixture::get("feeds") else {
        return;
    };
    let directory = f.directory.clone();
    let size = f.size;
    let mut storage = f.measure("open_us", || {
        let mut storage = Storage::open(&directory, false).unwrap();
        storage.initialize().unwrap();
        storage
    });
    if f.build {
        f.measure("build_us", || {
            for start in (1..=size).step_by(200) {
                let rows: Vec<_> = (start..(start + 200).min(size + 1))
                    .map(|n| {
                        let row = record(n);
                        let mut row = row;
                        row.baselined = true;
                        row.seen = (0..16).map(|id| format!("{id:032x}")).collect();
                        row.cursor = row.seen.last().unwrap().clone();
                        row
                    })
                    .collect();
                put_with(index(&mut storage), &rows, Vec::new()).unwrap();
            }
        });
    } else {
        assert_eq!(storage.total().unwrap(), size);
        f.measure("page_us", || {
            let after = None;
            let page = storage.page(after, 200).unwrap();
            assert_eq!(page.rows.len(), 200);
        });
        f.measure("pass_us", || {
            assert_eq!(storage.active(None).unwrap().0.len(), 200);
        });
        f.measure("update_us", || {
            let mut row = storage.row(&record(size).id).unwrap().unwrap();
            row.successes -= 1;
            storage.commit(row).unwrap();
        });
        let mut after = None;
        let mut count = 0;
        f.measure("walk_us", || {
            loop {
                let page = storage.page(after, 200).unwrap();
                count += page.rows.len() as u64;
                after = page.next;
                if after.is_none() {
                    break;
                }
            }
        });
        assert_eq!(count, size);
    }
    f.finish();
}
