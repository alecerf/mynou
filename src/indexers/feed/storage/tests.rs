//! Synthetic growth, recovery and provenance fixtures proposed by QA.
use super::*;
use crate::inventory::Fault;
use std::{
    collections::BTreeSet,
    os::unix::fs::{PermissionsExt, symlink},
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mynou-feed-index-{}",
            super::super::digest(&random_bytes::<16>().unwrap())
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
fn private(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn snapshot(records: &[Record]) -> Vec<u8> {
    let mut value = json::Value::object();
    value.insert(
        "records",
        json::Value::Array(records.iter().map(Record::to_json).collect()),
    );
    frame(MAGIC, json::stringify(&value).as_bytes())
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
        Backend::Ready(index) | Backend::Unpublished(index, _) => index,
        _ => panic!("indexed fixture"),
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
        put(index(&mut storage), &batch, None).unwrap();
    }
    assert!(
        encoded_bytes > MAX_BYTES,
        "{encoded_bytes} bytes must exceed the former snapshot limit"
    );
    for start in (1..=2_000_u64).step_by(200) {
        let batch: Vec<_> = (start..(start + 200).min(2_001)).map(record).collect();
        put(index(&mut storage), &batch, None).unwrap();
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
fn legacy_migration_resumes_a_proven_prefix_and_preserves_original_bytes() {
    let directory = Directory::new();
    let mut records: Vec<_> = (1..=201).map(record).collect();
    records[0].baselined = true;
    records[0].seen = vec![format!("{:032x}", 1)];
    records[0].cursor = records[0].seen[0].clone();
    let bytes = snapshot(&records);
    private(&directory.0.join("feeds.bin"), &bytes);
    let mut staging = create(&directory.0.join("feeds.idx.migrating"), sha256(&bytes)).unwrap();
    put(&mut staging, &records[..100], Some(&records[99].id)).unwrap();
    drop(staging);
    let before = contents(&directory.0);
    let mut preview = Storage::open(&directory.0, true).unwrap();
    preview.initialize().unwrap();
    assert!(preview.needs_migration());
    assert_eq!(preview.total().unwrap(), 201);
    drop(preview);
    assert_eq!(contents(&directory.0), before);
    let mut storage = Storage::open(&directory.0, false).unwrap();
    storage.initialize().unwrap();
    assert!(!storage.needs_migration());
    assert_eq!(counters(index(&mut storage)).unwrap(), (201, 1));
    assert!(index(&mut storage).get(&CURSOR).unwrap().is_none());
    verify(index(&mut storage), Some(&legacy(bytes.clone()).unwrap())).unwrap();
    for row in &records {
        assert_eq!(storage.row(&row.id).unwrap().as_ref(), Some(row));
    }
    assert_eq!(
        fs::read(
            directory
                .0
                .join(format!("feeds.legacy-{}.bin", super::super::digest(&bytes)))
        )
        .unwrap(),
        bytes
    );
    assert_eq!(&read(&directory.0.join("feeds.bin")).unwrap()[..8], MARKER);
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
        if fault != Fault::Unlinked {
            assert!(Storage::open(&directory.0, true).is_err());
        } else {
            drop(Storage::open(&directory.0, true).unwrap());
        }
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
fn snapshot_links_and_public_files_fail_without_changes() {
    for kind in 0..3 {
        let directory = Directory::new();
        let original = directory.0.join("original.bin");
        private(&original, &snapshot(&[record(1)]));
        let path = directory.0.join("feeds.bin");
        match kind {
            0 => symlink(&original, &path).unwrap(),
            1 => fs::hard_link(&original, &path).unwrap(),
            _ => {
                fs::rename(&original, &path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
        let before = contents(&directory.0);
        assert!(Storage::open(&directory.0, false).is_err());
        assert_eq!(contents(&directory.0), before);
    }
}

#[test]
fn foreign_or_interrupted_staging_is_preserved_intact_before_rebuilding() {
    for interrupted in [false, true] {
        let directory = Directory::new();
        let bytes = snapshot(&[record(1)]);
        private(&directory.0.join("feeds.bin"), &bytes);
        let staging = directory.0.join("feeds.idx.migrating");
        let mut index = create(&staging, sha256(b"foreign original")).unwrap();
        if interrupted {
            index.inject(Some(Fault::Published));
        }
        let result = put(&mut index, &[record(2)], Some(&record(2).id));
        assert_eq!(result.is_err(), interrupted);
        drop(index);
        private(
            &staging.join("unexpected-private-evidence"),
            b"synthetic preservation evidence",
        );
        let before: BTreeMap<_, _> = contents(&staging)
            .into_iter()
            .map(|(path, bytes)| (path.strip_prefix(&staging).unwrap().to_owned(), bytes))
            .collect();
        let mut storage = Storage::open(&directory.0, false).unwrap();
        storage.initialize().unwrap();
        assert_eq!(storage.row(&record(1).id).unwrap(), Some(record(1)));
        assert!(storage.row(&record(2).id).unwrap().is_none());
        let abandoned = fs::read_dir(&directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("feeds.idx.abandoned-")
            })
            .unwrap();
        let preserved: BTreeMap<_, _> = contents(&abandoned)
            .into_iter()
            .map(|(path, bytes)| (path.strip_prefix(&abandoned).unwrap().to_owned(), bytes))
            .collect();
        assert_eq!(preserved, before);
    }
}

#[test]
fn promoted_index_resumes_publication_but_foreign_pending_indexes_are_untouched() {
    let directory = Directory::new();
    let bytes = snapshot(&[record(1)]);
    private(&directory.0.join("feeds.bin"), &bytes);
    let mut index = create(&directory.0.join("feeds.idx"), sha256(&bytes)).unwrap();
    put(&mut index, &[record(1)], None).unwrap();
    drop(index);
    let mut storage = Storage::open(&directory.0, false).unwrap();
    assert!(storage.needs_migration());
    storage.initialize().unwrap();
    assert!(!storage.needs_migration());
    let marker = read(&directory.0.join("feeds.bin")).unwrap();
    assert_eq!(
        read(
            &directory
                .0
                .join(format!("feeds.legacy-{}.bin", super::super::digest(&bytes)))
        )
        .unwrap(),
        bytes
    );
    drop(storage);
    for marked in [false, true] {
        let other = Directory::new();
        private(
            &other.0.join("feeds.bin"),
            if marked { &marker } else { &bytes },
        );
        let mut index = create(&other.0.join("feeds.idx"), sha256(b"foreign original")).unwrap();
        index.inject(Some(Fault::Published));
        assert!(put(&mut index, &[record(2)], None).is_err());
        drop(index);
        let before = contents(&other.0);
        assert!(Storage::open(&other.0, false).is_err());
        assert!(Storage::open(&other.0, true).is_err());
        assert_eq!(contents(&other.0), before);
    }
}

#[test]
fn conflicting_backup_and_corrupt_active_proof_are_never_overwritten() {
    let directory = Directory::new();
    let bytes = snapshot(&[record(1)]);
    private(&directory.0.join("feeds.bin"), &bytes);
    private(
        &directory
            .0
            .join(format!("feeds.legacy-{}.bin", super::super::digest(&bytes))),
        &snapshot(&[record(2)]),
    );
    let before = contents(&directory.0);
    let mut storage = Storage::open(&directory.0, false).unwrap();
    assert!(storage.initialize().is_err());
    drop(storage);
    assert_eq!(contents(&directory.0), before);
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
    put(index(&mut storage), &batch, None).unwrap();
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
fn changed_configuration_fails_before_migration_publication() {
    let directory = Directory::new();
    let mut value = crate::config::default_json();
    value.insert("indexers", json::Value::Array(Vec::new()));
    let mut config = crate::config::from_json(&value, &directory.0).unwrap();
    let mut options = crate::indexers::Options::default();
    options.id = Some("configured".into());
    options.watch.enabled = true;
    let source = crate::config::Source {
        options,
        name: "Synthetic".into(),
        kind: "rss".into(),
        url: "http://127.0.0.1:9/feed".into(),
        api_key_env: "MYNOU_ABSENT_FEED_FIXTURE_KEY_110".into(),
    };
    let row = Record::new("configured".into(), source.options.binding(&source));
    let bytes = snapshot(std::slice::from_ref(&row));
    private(&directory.0.join("feeds.bin"), &bytes);
    let mut staging = create(&directory.0.join("feeds.idx.migrating"), sha256(&bytes)).unwrap();
    put(&mut staging, std::slice::from_ref(&row), Some(&row.id)).unwrap();
    drop(staging);
    config.sources = vec![source];
    config.sources[0].url = "http://127.0.0.1:9/changed-feed".into();
    let before = contents(&directory.0);
    assert!(
        super::super::FeedStore::open(&directory.0, &config, false)
            .err()
            .unwrap()
            .contains("source binding changed")
    );
    assert_eq!(contents(&directory.0), before);
    assert!(!directory.0.join("feeds.idx").exists());
}
