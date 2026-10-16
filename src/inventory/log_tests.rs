//! Value-log segments: one flushed file per commit, bounded cleaning and
//! recovery, beside record files written before the log existed.

use super::{
    CLEAN_SLACK_OVERRIDE, Change, Index, Key, Query,
    persistence::{ENVELOPE_BYTES, Fault, SEGMENT_HEADER_BYTES},
    tree::Header,
};
use crate::crypto::{random_bytes, sha256};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let suffix: String = random_bytes::<16>()
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self(std::env::temp_dir().join(format!("mynou-log-{suffix}")))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Restores the production cleaning threshold when a test ends.
struct Slack;
impl Slack {
    fn set(bytes: u64) -> Self {
        CLEAN_SLACK_OVERRIDE.with(|value| value.set(Some(bytes)));
        Self
    }
}
impl Drop for Slack {
    fn drop(&mut self) {
        CLEAN_SLACK_OVERRIDE.with(|value| value.set(None));
    }
}

fn key(id: u64) -> Key {
    let mut key = [0; 64];
    key[56..].copy_from_slice(&id.to_be_bytes());
    key
}

fn value(id: u64, round: u64) -> Vec<u8> {
    let mut data = vec![(id + round) as u8; 300 + (id as usize % 7) * 50];
    data[..8].copy_from_slice(&round.to_be_bytes());
    data
}

fn segment(directory: &Path, number: u64) -> PathBuf {
    directory.join("segments").join(format!("{number:016x}"))
}

/// Segment files present on disk, by number.
fn segment_files(directory: &Path) -> BTreeMap<u64, u64> {
    let Ok(entries) = fs::read_dir(directory.join("segments")) else {
        return BTreeMap::new();
    };
    entries
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            assert!(!name.starts_with('.'), "leftover temporary {name}");
            (
                u64::from_str_radix(&name, 16).unwrap(),
                entry.metadata().unwrap().len(),
            )
        })
        .collect()
}

/// The header's log totals agree with the files and the expected values.
fn assert_accounting(index: &Index, directory: &Path, model: &BTreeMap<u64, Vec<u8>>) {
    let header = index.header;
    let files = segment_files(directory);
    assert_eq!(
        files.keys().copied().collect::<Vec<_>>(),
        (header.tail..header.head).collect::<Vec<_>>()
    );
    assert_eq!(files.values().sum::<u64>(), header.log_bytes);
    let live: u64 = model
        .values()
        .map(|data| ENVELOPE_BYTES + data.len() as u64)
        .sum();
    assert_eq!(header.live_bytes, live);
}

fn assert_values(index: &mut Index, model: &BTreeMap<u64, Vec<u8>>) {
    assert_eq!(index.len().unwrap(), model.len() as u64);
    for (id, data) in model {
        assert_eq!(index.get(&key(*id)).unwrap().as_ref(), Some(data));
    }
}

#[test]
fn a_commit_writes_one_segment_whatever_its_record_count() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    let mut model = BTreeMap::new();
    for id in 0..500 {
        model.insert(id, value(id, 0));
    }
    index
        .commit(
            model
                .iter()
                .map(|(id, data)| Change::Put(key(*id), data.clone()))
                .collect(),
        )
        .unwrap();
    assert!(!directory.0.join("objects/00").exists());
    assert_eq!(segment_files(&directory.0).len(), 1);
    assert_accounting(&index, &directory.0, &model);
    drop(index);
    let mut index = Index::open(&directory.0, true).unwrap();
    assert_values(&mut index, &model);
    let page = index.page(&Query::default()).unwrap();
    assert_eq!(page.entries[0].1, model[&0]);
}

#[test]
fn segment_envelopes_bind_their_key_store_and_private_file() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    // Identical payloads: only the envelope's key tells them apart.
    index
        .commit(vec![
            Change::Put(key(1), vec![4; 100]),
            Change::Put(key(2), vec![4; 100]),
        ])
        .unwrap();
    drop(index);
    let path = segment(&directory.0, 0);
    let original = fs::read(&path).unwrap();
    let first = SEGMENT_HEADER_BYTES as usize;
    let size = ENVELOPE_BYTES as usize + 100;
    let mut swapped = original.clone();
    swapped.copy_within(first + size..first + 2 * size, first);
    fs::write(&path, &swapped).unwrap();
    let mut index = Index::open(&directory.0, true).unwrap();
    assert!(index.get(&key(1)).is_err());
    assert_eq!(index.get(&key(2)).unwrap(), Some(vec![4; 100]));
    drop(index);

    let mut flipped = original.clone();
    flipped[first + 80] ^= 1;
    fs::write(&path, &flipped).unwrap();
    let mut index = Index::open(&directory.0, true).unwrap();
    assert!(index.get(&key(1)).is_err());
    drop(index);

    // The same bytes in another store fail its identity check.
    let other = Directory::new();
    let mut foreign = Index::create(&other.0).unwrap();
    foreign
        .commit(vec![Change::Put(key(1), vec![4; 100])])
        .unwrap();
    drop(foreign);
    fs::write(segment(&other.0, 0), &original).unwrap();
    let mut foreign = Index::open(&other.0, true).unwrap();
    assert!(foreign.get(&key(1)).is_err());
    drop(foreign);

    fs::write(&path, &original).unwrap();
    let link = directory.0.join("linked-segment");
    fs::hard_link(&path, &link).unwrap();
    let mut index = Index::open(&directory.0, true).unwrap();
    assert!(index.get(&key(1)).is_err());
    drop(index);
    fs::remove_file(&link).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let mut index = Index::open(&directory.0, true).unwrap();
    assert!(index.get(&key(1)).is_err());
    drop(index);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let mut index = Index::open(&directory.0, true).unwrap();
    assert_eq!(index.get(&key(1)).unwrap(), Some(vec![4; 100]));
}

#[test]
fn published_redo_rewrites_a_torn_or_missing_segment() {
    for damage in [Some(10_u64), Some(SEGMENT_HEADER_BYTES + 5), None] {
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        index
            .commit(vec![Change::Put(key(1), vec![1; 50])])
            .unwrap();
        index.inject(Some(Fault::FirstRecord));
        assert!(
            index
                .commit(vec![
                    Change::Put(key(2), vec![2; 70]),
                    Change::Put(key(1), vec![3; 90]),
                ])
                .is_err()
        );
        drop(index);
        let path = segment(&directory.0, 1);
        match damage {
            Some(length) => fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(length)
                .unwrap(),
            None => fs::remove_file(&path).unwrap(),
        }
        let mut index = Index::open(&directory.0, false).unwrap();
        let model = BTreeMap::from([(1, vec![3; 90]), (2, vec![2; 70])]);
        assert_values(&mut index, &model);
        assert_accounting(&index, &directory.0, &model);
    }
}

#[derive(Clone, Copy, Debug)]
enum UnsafeEntry {
    Symlink,
    DanglingSymlink,
    HardLink,
    Public,
    Directory,
    Fifo,
}

const UNSAFE_ENTRIES: [UnsafeEntry; 6] = [
    UnsafeEntry::Symlink,
    UnsafeEntry::DanglingSymlink,
    UnsafeEntry::HardLink,
    UnsafeEntry::Public,
    UnsafeEntry::Directory,
    UnsafeEntry::Fifo,
];

fn make_unsafe(directory: &Path, path: &Path, kind: UnsafeEntry) {
    let other = directory.join("untouched-target");
    if matches!(kind, UnsafeEntry::HardLink) {
        fs::hard_link(path, other).unwrap();
    } else if matches!(kind, UnsafeEntry::Public) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    } else {
        fs::remove_file(path).unwrap();
        match kind {
            UnsafeEntry::Symlink => {
                fs::write(&other, b"synthetic untouched bytes").unwrap();
                fs::set_permissions(&other, fs::Permissions::from_mode(0o600)).unwrap();
                symlink(other, path).unwrap();
            }
            UnsafeEntry::DanglingSymlink => symlink(other, path).unwrap(),
            UnsafeEntry::Directory => {
                fs::create_dir(path).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            UnsafeEntry::Fifo => crate::store::open_tests::fifo(path),
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SavedEntry {
    mode: u32,
    inode: u64,
    links: u64,
    bytes: Option<Vec<u8>>,
    target: Option<PathBuf>,
}

/// Captures every entry without following links or opening special files.
fn saved_entries(directory: &Path) -> BTreeMap<PathBuf, SavedEntry> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, SavedEntry>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        entries.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            SavedEntry {
                mode: metadata.mode(),
                inode: metadata.ino(),
                links: metadata.nlink(),
                bytes: metadata.is_file().then(|| fs::read(path).unwrap()),
                target: metadata.is_symlink().then(|| fs::read_link(path).unwrap()),
            },
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), entries);
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(directory, directory, &mut entries);
    entries
}

#[test]
fn redo_rejects_unsafe_segment_targets_before_changing_any_file() {
    for kind in UNSAFE_ENTRIES {
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        index.inject(Some(Fault::FirstRecord));
        assert!(
            index
                .commit(vec![Change::Put(key(1), value(1, 0))])
                .is_err()
        );
        drop(index);
        make_unsafe(&directory.0, &segment(&directory.0, 0), kind);
        let before = saved_entries(&directory.0);
        assert!(Index::open(&directory.0, false).is_err(), "{kind:?}");
        assert_eq!(saved_entries(&directory.0), before, "{kind:?}");
    }
}

#[test]
fn redo_rejects_unsafe_retired_segments_before_changing_any_file() {
    let _slack = Slack::set(u64::MAX / 4);
    for kind in UNSAFE_ENTRIES {
        CLEAN_SLACK_OVERRIDE.with(|value| value.set(Some(u64::MAX / 4)));
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        for round in 0..3 {
            index
                .commit(
                    (0..12)
                        .map(|id| Change::Put(key(id), value(id, round)))
                        .collect(),
                )
                .unwrap();
        }
        CLEAN_SLACK_OVERRIDE.with(|value| value.set(Some(0)));
        index.inject(Some(Fault::Published));
        assert!(
            index
                .commit(vec![Change::Put(key(99), value(99, 0))])
                .is_err()
        );
        drop(index);
        let wal = fs::read(directory.0.join("transaction.bin")).unwrap();
        let page_size = super::tree::PAGE_SIZE;
        let after = Header::decode(&wal[16 + page_size..16 + 2 * page_size]).unwrap();
        assert!(after.tail >= 2, "the fixture must retire multiple segments");
        make_unsafe(&directory.0, &segment(&directory.0, 1), kind);
        let before = saved_entries(&directory.0);
        assert!(Index::open(&directory.0, false).is_err(), "{kind:?}");
        assert_eq!(saved_entries(&directory.0), before, "{kind:?}");
    }
}

#[test]
fn inline_commits_reject_an_unsafe_segments_directory_without_mutation() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index
        .commit(vec![Change::Put(key(1), value(1, 0))])
        .unwrap();
    let segments = directory.0.join("segments");
    fs::rename(&segments, directory.0.join("original-segments")).unwrap();
    symlink(directory.0.join("original-segments"), &segments).unwrap();
    let before = saved_entries(&directory.0);
    assert!(
        index
            .commit(vec![Change::PutInline(key(2), vec![1])])
            .is_err()
    );
    assert_eq!(saved_entries(&directory.0), before);
    assert!(index.is_usable(), "preflight must precede WAL publication");
}

/// Overwrites and deletes keys for `rounds` commits, mirroring them in `model`.
fn churn(index: &mut Index, model: &mut BTreeMap<u64, Vec<u8>>, rounds: u64) {
    for round in 1..=rounds {
        let mut changes = Vec::new();
        for step in 0..12 {
            let id = (round * 7 + step * 13) % 60;
            if (round + step) % 5 == 0 {
                if model.remove(&id).is_some() {
                    changes.push(Change::Delete(key(id)));
                }
            } else {
                let data = value(id, round);
                model.insert(id, data.clone());
                changes.push(Change::Put(key(id), data));
            }
        }
        // Each step names a distinct key, so no change repeats a key.
        index.commit(changes).unwrap();
    }
}

#[test]
fn cleaning_keeps_the_log_near_its_live_bytes_and_empties_it_after_deletes() {
    let _slack = Slack::set(0);
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    let mut model = BTreeMap::new();
    for round in 0..15 {
        churn(&mut index, &mut model, 20);
        let header = index.header;
        assert!(
            header.log_bytes <= 2 * header.live_bytes + 64 * 1024,
            "round {round}: log {} for live {}",
            header.log_bytes,
            header.live_bytes
        );
        assert_accounting(&index, &directory.0, &model);
    }
    assert!(index.header.tail > 0, "the cleaner never retired a segment");
    drop(index);
    let mut index = Index::open(&directory.0, false).unwrap();
    assert_values(&mut index, &model);
    let ids: Vec<_> = model.keys().copied().collect();
    index
        .commit(ids.iter().map(|id| Change::Delete(key(*id))).collect())
        .unwrap();
    model.clear();
    index
        .commit(vec![Change::PutInline(key(999), b"x".to_vec())])
        .unwrap();
    let header = index.header;
    assert_eq!(
        (header.tail, header.log_bytes, header.live_bytes),
        (header.head, 0, 0)
    );
    assert!(segment_files(&directory.0).is_empty());
}

#[test]
fn the_production_threshold_leaves_small_logs_alone() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    let mut model = BTreeMap::new();
    churn(&mut index, &mut model, 30);
    assert_eq!(index.header.tail, 0);
    assert_accounting(&index, &directory.0, &model);
}

#[test]
fn cleaning_commits_recover_at_every_boundary() {
    let _slack = Slack::set(0);
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        let mut model = BTreeMap::new();
        for id in 0..40 {
            model.insert(id, value(id, 0));
        }
        index
            .commit(
                model
                    .iter()
                    .map(|(id, data)| Change::Put(key(*id), data.clone()))
                    .collect(),
            )
            .unwrap();
        // Two full rewrites leave the first two segments entirely garbage,
        // while a huge threshold keeps those commits from cleaning.
        let mut replaced = model.clone();
        CLEAN_SLACK_OVERRIDE.with(|value| value.set(Some(u64::MAX / 4)));
        for round in 1..=2 {
            for id in 0..40 {
                replaced.insert(id, value(id, round));
            }
            index
                .commit(
                    (0..40)
                        .map(|id| Change::Put(key(id), replaced[&id].clone()))
                        .collect(),
                )
                .unwrap();
        }
        CLEAN_SLACK_OVERRIDE.with(|value| value.set(Some(0)));
        let before = index.header;
        index.inject(Some(fault));
        assert!(
            index
                .commit(vec![Change::Put(key(100), vec![9; 40])])
                .is_err()
        );
        drop(index);
        let mut index = Index::open(&directory.0, false).unwrap();
        if fault != Fault::Staged {
            replaced.insert(100, vec![9; 40]);
            assert!(index.header.tail > before.tail, "{fault:?} did not clean");
        } else {
            assert_eq!(index.header, before);
        }
        assert_values(&mut index, &replaced);
        assert_accounting(&index, &directory.0, &replaced);
    }
}

#[test]
fn record_files_from_before_the_log_are_read_replaced_and_deleted() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.legacy_records = true;
    index
        .commit(
            (0..6)
                .map(|id| Change::Put(key(id), value(id, 0)))
                .collect(),
        )
        .unwrap();
    index.legacy_records = false;
    let files = || {
        fs::read_dir(directory.0.join("objects/00"))
            .unwrap()
            .count()
    };
    assert_eq!(files(), 6);
    assert_eq!(index.header.live_bytes, 0);
    index
        .commit(vec![
            Change::Put(key(1), value(1, 1)),
            Change::Delete(key(2)),
            Change::PutInline(key(3), b"tiny".to_vec()),
        ])
        .unwrap();
    assert_eq!(files(), 3);
    let model = BTreeMap::from([(1, value(1, 1))]);
    assert_accounting(&index, &directory.0, &model);
    drop(index);
    let mut index = Index::open(&directory.0, true).unwrap();
    assert_eq!(index.get(&key(0)).unwrap(), Some(value(0, 0)));
    assert_eq!(index.get(&key(1)).unwrap(), Some(value(1, 1)));
    assert!(index.get(&key(2)).unwrap().is_none());
    assert_eq!(index.get(&key(3)).unwrap(), Some(b"tiny".to_vec()));
    assert_eq!(index.len().unwrap(), 5);
}

#[test]
fn inconsistent_value_log_metadata_is_rejected() {
    let identity = [7; 32];
    let valid = Header {
        head: 3,
        tail: 1,
        tail_offset: 200,
        log_bytes: 900,
        live_bytes: 400,
        ..Header::empty(identity)
    };
    assert_eq!(Header::decode(&valid.encode()).unwrap(), valid);
    for invalid in [
        Header { tail: 4, ..valid },
        Header {
            head: u64::MAX,
            ..valid
        },
        Header {
            log_bytes: 0,
            live_bytes: 0,
            tail_offset: 0,
            ..valid
        },
        Header {
            tail_offset: 79,
            ..valid
        },
        Header {
            tail_offset: 901,
            ..valid
        },
        Header {
            tail_offset: u64::from(u32::MAX) + 1,
            log_bytes: u64::from(u32::MAX) + 2,
            ..valid
        },
        Header {
            live_bytes: 901,
            ..valid
        },
        Header {
            tail: 3,
            log_bytes: 0,
            live_bytes: 0,
            ..valid
        },
        Header {
            tail: 3,
            tail_offset: 0,
            live_bytes: 0,
            ..valid
        },
    ] {
        assert!(Header::decode(&invalid.encode()).is_err(), "{invalid:?}");
    }
}

#[test]
fn segment_retirement_is_bounded_in_live_commits_and_published_redo() {
    let _slack = Slack::set(u64::MAX / 4);
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    for round in 0..300 {
        index
            .commit(vec![Change::Put(key(1), value(1, round))])
            .unwrap();
    }
    let before = index.header;
    assert_eq!(before.head, 300);
    assert_eq!(before.tail, 0);
    CLEAN_SLACK_OVERRIDE.with(|value| value.set(Some(0)));
    index.inject(Some(Fault::Published));
    let latest = value(1, 300);
    assert!(
        index
            .commit(vec![Change::Put(key(1), latest.clone())])
            .is_err()
    );
    drop(index);

    let wal_path = directory.0.join("transaction.bin");
    let wal = fs::read(&wal_path).unwrap();
    let page_size = super::tree::PAGE_SIZE;
    let header_start = 16 + page_size;
    let header_end = header_start + page_size;
    let mut after = Header::decode(&wal[header_start..header_end]).unwrap();
    assert_eq!(after.tail - before.tail, super::MAX_RETIRED_SEGMENTS);
    // A correctly checksummed but excessive retirement must reject before
    // probing paths or replaying any of the transaction's effects.
    after.tail += 1;
    Header::decode(&after.encode()).unwrap();
    let mut excessive = wal.clone();
    excessive[header_start..header_end].copy_from_slice(&after.encode());
    let checksum_at = excessive.len() - 32;
    let checksum = sha256(&excessive[..checksum_at]);
    excessive[checksum_at..].copy_from_slice(&checksum);
    fs::write(&wal_path, excessive).unwrap();
    let saved = saved_entries(&directory.0);
    assert!(Index::open(&directory.0, false).is_err());
    assert_eq!(saved_entries(&directory.0), saved);

    fs::write(&wal_path, wal).unwrap();
    let mut index = Index::open(&directory.0, false).unwrap();
    assert_eq!(index.header.tail - before.tail, super::MAX_RETIRED_SEGMENTS);
    let model = BTreeMap::from([(1, latest)]);
    assert_values(&mut index, &model);
    assert_accounting(&index, &directory.0, &model);
    let tail = index.header.tail;
    index
        .commit(vec![Change::PutInline(key(2), b"x".to_vec())])
        .unwrap();
    assert!(
        index.header.tail > tail,
        "a later bounded pass must continue cleaning"
    );
    assert!(index.header.tail - tail <= super::MAX_RETIRED_SEGMENTS);
    assert_eq!(index.get(&key(1)).unwrap().as_ref(), model.get(&1));
}
