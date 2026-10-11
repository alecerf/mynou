use super::{
    Change, Index, Key, Query,
    persistence::Fault,
    tree::{self, Header, PAGE_SIZE},
};
use crate::crypto::{random_bytes, sha256};
use std::{
    fs::{self, File},
    io::{Seek, SeekFrom, Write},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    time::Instant,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let suffix = random_bytes::<16>()
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Self(std::env::temp_dir().join(format!("mynou-inventory-{suffix}")))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn key(id: u64) -> Key {
    let mut key = [0; 64];
    key[56..].copy_from_slice(&id.to_be_bytes());
    key
}
fn put(id: u64) -> Change {
    Change::Put(key(id), id.to_be_bytes().to_vec())
}
fn contents(path: &Path) -> Vec<(PathBuf, Vec<u8>, u32)> {
    let mut result = Vec::new();
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            result.extend(contents(&path));
        } else {
            let data = fs::read(&path).unwrap();
            result.push((path, data, metadata.permissions().mode()));
        }
    }
    result.sort_by(|left, right| left.0.cmp(&right.0));
    result
}

#[test]
fn grows_past_old_caps_and_pages_every_record_after_restart() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    let started = Instant::now();
    let mut previous = started;
    for start in (0..10_241u64).step_by(256) {
        index
            .commit((start..(start + 256).min(10_241)).rev().map(put).collect())
            .unwrap();
        let count = index.len().unwrap();
        if count == 256 || count == 2048 || count == 8192 || count == 10_241 {
            eprintln!(
                "inventory-growth records={count} elapsed_ms={} interval_ms={} staged_nodes={} height={}",
                started.elapsed().as_millis(),
                previous.elapsed().as_millis(),
                index.last_staged_nodes,
                index.header.level + 1
            );
            previous = Instant::now();
        }
    }
    assert_eq!(index.len().unwrap(), 10_241);
    drop(index);
    let mut index = Index::open(&directory.0, true).unwrap();
    let mut query = Query {
        limit: 137,
        ..Query::default()
    };
    let mut seen = 0u64;
    loop {
        let page = index.page(&query).unwrap();
        assert!(index.last_page_reads <= (index.header.level as usize + 1) * 139);
        query.sequence = Some(page.sequence);
        assert_eq!(page.total, 10_241);
        for (id, data) in &page.entries {
            assert_eq!(*id, key(seen));
            assert_eq!(*data, seen.to_be_bytes());
            seen += 1;
        }
        query.after = page.next;
        if page.next.is_none() {
            break;
        }
    }
    assert_eq!(seen, 10_241);
    assert!(index.get(&key(10_241)).unwrap().is_none());
    assert!(index.commit(vec![put(20_000)]).is_err());
}

#[test]
fn byte_bounded_pages_resume_after_last_emitted_key_and_reject_mutations() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index
        .commit(
            (0..7)
                .map(|id| Change::Put(key(id), vec![id as u8; super::MAX_VALUE_BYTES]))
                .collect(),
        )
        .unwrap();
    let first = index.page(&Query::default()).unwrap();
    assert_eq!(first.entries.len(), 4);
    assert_eq!(first.next, Some(key(3)));
    let query = Query {
        after: first.next,
        sequence: Some(first.sequence),
        ..Query::default()
    };
    let second = index.page(&query).unwrap();
    assert_eq!(
        second
            .entries
            .iter()
            .map(|(key, _)| *key)
            .collect::<Vec<_>>(),
        vec![key(4), key(5), key(6)]
    );
    assert_eq!(second.next, None);
    index.commit(vec![put(9)]).unwrap();
    assert!(index.page(&query).unwrap_err().contains("changed"));
    let range = index
        .page(&Query {
            start: key(2),
            end: Some(key(5)),
            limit: 2,
            ..Query::default()
        })
        .unwrap();
    assert_eq!(
        range
            .entries
            .iter()
            .map(|(key, _)| *key)
            .collect::<Vec<_>>(),
        vec![key(2), key(3)]
    );
    assert_eq!(range.next, Some(key(3)));
}

#[test]
fn multi_key_delete_prunes_empty_branches_and_collapses_the_root() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit((0..1024).map(put).collect()).unwrap();
    index
        .commit((0..1024).map(|id| Change::Delete(key(id))).collect())
        .unwrap();
    assert!(index.is_empty().unwrap());
    assert_eq!(index.header.root, 0);
    index
        .commit(vec![put(7000), put(1), Change::Delete(key(555))])
        .unwrap();
    assert_eq!(index.len().unwrap(), 2);
    assert_eq!(index.header.level, 0);
    assert_eq!(
        index
            .page(&Query::default())
            .unwrap()
            .entries
            .iter()
            .map(|(key, _)| *key)
            .collect::<Vec<_>>(),
        vec![key(1), key(7000)]
    );
    drop(index);
    let mut reopened = Index::open(&directory.0, false).unwrap();
    assert_eq!(
        reopened.get(&key(1)).unwrap(),
        Some(1u64.to_be_bytes().to_vec())
    );
}

#[test]
fn every_commit_boundary_has_truthful_poisoning_and_idempotent_recovery() {
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
        index.commit((0..40).map(put).collect()).unwrap();
        index.fault = Some(fault);
        assert!(
            index
                .commit(vec![
                    Change::Delete(key(3)),
                    Change::Put(key(8), b"replacement".to_vec()),
                    put(70)
                ])
                .is_err()
        );
        assert!(index.len().is_err());
        assert!(index.sequence().is_err());
        assert!(index.get(&key(8)).is_err());
        assert!(index.page(&Query::default()).is_err());
        assert!(index.commit(vec![put(71)]).is_err());
        drop(index);
        let before = contents(&directory.0);
        if fault != Fault::Unlinked {
            assert!(Index::open(&directory.0, true).is_err());
        } else {
            drop(Index::open(&directory.0, true).unwrap());
        }
        assert_eq!(contents(&directory.0), before);
        let mut index = Index::open(&directory.0, false).unwrap();
        if fault == Fault::Staged {
            assert!(index.get(&key(3)).unwrap().is_some());
            assert_eq!(
                index.get(&key(8)).unwrap(),
                Some(8u64.to_be_bytes().to_vec())
            );
            assert!(index.get(&key(70)).unwrap().is_none());
        } else {
            assert!(index.get(&key(3)).unwrap().is_none());
            assert_eq!(index.get(&key(8)).unwrap(), Some(b"replacement".to_vec()));
            assert!(index.get(&key(70)).unwrap().is_some());
        }
        assert_eq!(index.len().unwrap(), 40);
        drop(index);
        let mut again = Index::open(&directory.0, true).unwrap();
        assert_eq!(again.len().unwrap(), 40);
        assert!(again.get(&key(71)).unwrap().is_none());
    }
}

#[test]
fn io_write_failure_keeps_published_proof_and_blocks_reads_until_recovery() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit(vec![put(1)]).unwrap();
    index.tree = File::open(directory.0.join("tree.bin")).unwrap();
    assert!(index.commit(vec![put(2)]).is_err());
    assert!(index.get(&key(1)).is_err());
    assert!(index.len().is_err());
    drop(index);
    let mut index = Index::open(&directory.0, false).unwrap();
    assert_eq!(index.len().unwrap(), 2);
    assert!(index.get(&key(2)).unwrap().is_some());
}

#[test]
fn published_redo_repairs_covered_torn_root_pages_and_records_only() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit((0..40).map(put).collect()).unwrap();
    index.fault = Some(Fault::Published);
    assert!(
        index
            .commit(vec![Change::Put(key(3), b"new".to_vec())])
            .is_err()
    );
    let root = index.header.root;
    drop(index);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(directory.0.join("tree.bin"))
        .unwrap();
    file.write_all(&[0; 51]).unwrap();
    // The root and affected leaf both have complete redo images.
    file.seek(SeekFrom::Start(tree::offset(root).unwrap()))
        .unwrap();
    file.write_all(&[0; 71]).unwrap();
    let object = directory.0.join("objects/00").join(
        key(3)
            .iter()
            .skip(1)
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    );
    fs::write(&object, b"torn").unwrap();
    let mut reopened = Index::open(&directory.0, false).unwrap();
    assert_eq!(reopened.get(&key(3)).unwrap(), Some(b"new".to_vec()));
    drop(reopened);
    let unrelated = directory.0.join("objects/00").join(
        key(9)
            .iter()
            .skip(1)
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    );
    fs::write(unrelated, b"corrupt").unwrap();
    let mut index = Index::open(&directory.0, true).unwrap();
    assert!(index.get(&key(9)).is_err());
}

#[test]
fn published_corruption_truncation_and_cross_store_replay_never_modify_state() {
    for truncate in [false, true] {
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        index.fault = Some(Fault::Published);
        assert!(index.commit(vec![put(1)]).is_err());
        drop(index);
        let path = directory.0.join("transaction.bin");
        let mut bytes = fs::read(&path).unwrap();
        if truncate {
            bytes.truncate(bytes.len() / 2);
        } else {
            bytes[100] ^= 1;
        }
        fs::write(&path, bytes).unwrap();
        let before = contents(&directory.0);
        assert!(Index::open(&directory.0, false).is_err());
        assert_eq!(contents(&directory.0), before);
    }
    let source = Directory::new();
    let target = Directory::new();
    let mut index = Index::create(&source.0).unwrap();
    index.fault = Some(Fault::Published);
    assert!(index.commit(vec![put(1)]).is_err());
    drop(index);
    drop(Index::create(&target.0).unwrap());
    fs::copy(
        source.0.join("transaction.bin"),
        target.0.join("transaction.bin"),
    )
    .unwrap();
    fs::set_permissions(
        target.0.join("transaction.bin"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let before = contents(&target.0);
    assert!(Index::open(&target.0, false).is_err());
    assert_eq!(contents(&target.0), before);
}

#[test]
fn incomplete_unpublished_staging_is_discarded_only_by_writable_open() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit(vec![put(1)]).unwrap();
    drop(index);
    let staging = directory.0.join("transaction.tmp");
    fs::write(&staging, b"partial").unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o600)).unwrap();
    let before = contents(&directory.0);
    assert!(Index::open(&directory.0, true).is_err());
    assert_eq!(contents(&directory.0), before);
    let mut index = Index::open(&directory.0, false).unwrap();
    assert!(index.get(&key(1)).unwrap().is_some());
    assert!(!staging.exists());
}

#[test]
fn legacy_directories_links_private_permissions_and_owner_exclusion_are_preserved() {
    let directory = Directory::new();
    fs::create_dir(&directory.0).unwrap();
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(directory.0.join("journal.bin"), b"legacy").unwrap();
    let before = contents(&directory.0);
    assert!(Index::create(&directory.0).is_err());
    assert!(Index::open(&directory.0, false).is_err());
    assert_eq!(contents(&directory.0), before);
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit(vec![put(1)]).unwrap();
    assert!(Index::open(&directory.0, true).is_err());
    drop(index);
    let object = directory.0.join("objects/00").join(
        key(1)
            .iter()
            .skip(1)
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    );
    let link = directory.0.join("linked-record");
    fs::hard_link(&object, &link).unwrap();
    let mut index = Index::open(&directory.0, true).unwrap();
    assert!(index.get(&key(1)).is_err());
    drop(index);
    fs::remove_file(&link).unwrap();
    fs::set_permissions(&object, fs::Permissions::from_mode(0o644)).unwrap();
    let mut index = Index::open(&directory.0, true).unwrap();
    assert!(index.get(&key(1)).is_err());
    drop(index);
    fs::set_permissions(&object, fs::Permissions::from_mode(0o600)).unwrap();
    let alias = Directory::new();
    symlink(&directory.0, &alias.0).unwrap();
    assert!(Index::open(&alias.0, false).is_err());
    fs::set_permissions(
        directory.0.join("tree.bin"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(Index::open(&directory.0, false).is_err());
}

#[test]
fn invalid_operations_and_complete_page_corruption_are_errors_not_empty_results() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    assert!(index.commit(vec![put(1), put(1)]).is_err());
    assert!(
        index
            .commit(vec![Change::Put(
                key(1),
                vec![0; super::MAX_VALUE_BYTES + 1]
            )])
            .is_err()
    );
    assert!(
        index
            .page(&Query {
                limit: 0,
                ..Query::default()
            })
            .is_err()
    );
    index.commit((0..70).map(put).collect()).unwrap();
    let root = index.header.root;
    let identity = index.header.identity;
    drop(index);
    let tree_path = directory.0.join("tree.bin");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&tree_path)
        .unwrap();
    let mut page = tree::read_page(&mut file, root).unwrap();
    // Correct checksum, but the child references its own parent: reject.
    page[128..136].copy_from_slice(&root.to_le_bytes());
    let digest = sha256(&page[..PAGE_SIZE - 32]);
    page[PAGE_SIZE - 32..].copy_from_slice(&digest);
    file.seek(SeekFrom::Start(tree::offset(root).unwrap()))
        .unwrap();
    file.write_all(&page).unwrap();
    assert!(Index::open(&directory.0, false).is_err());
    let mut header = Header::empty(identity).encode();
    header[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
    let digest = sha256(&header[..PAGE_SIZE - 32]);
    header[PAGE_SIZE - 32..].copy_from_slice(&digest);
    assert!(Header::decode(&header).is_err());
}

#[test]
fn stale_valid_wal_and_inconsistent_checksummed_redo_are_rejected_before_writes() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.fault = Some(Fault::Published);
    assert!(index.commit(vec![put(1)]).is_err());
    drop(index);
    let path = directory.0.join("transaction.bin");
    let saved = fs::read(&path).unwrap();
    // Keep the full framing/checksum valid but change a new-key put into an
    // existing-key put. Counts no longer agree; no recovery write is allowed.
    let mut invalid = saved.clone();
    let count_offset = 16 + 2 * PAGE_SIZE;
    let pages =
        u32::from_le_bytes(invalid[count_offset..count_offset + 4].try_into().unwrap()) as usize;
    let action_offset = count_offset + 4 + pages * (PAGE_SIZE + 8) + 4;
    invalid[action_offset + 65] = 1;
    let end = invalid.len() - 32;
    let digest = sha256(&invalid[..end]);
    invalid[end..].copy_from_slice(&digest);
    fs::write(&path, invalid).unwrap();
    let before = contents(&directory.0);
    assert!(Index::open(&directory.0, false).is_err());
    assert_eq!(contents(&directory.0), before);
    fs::write(&path, &saved).unwrap();
    let mut index = Index::open(&directory.0, false).unwrap();
    index.commit(vec![put(2)]).unwrap();
    drop(index);
    fs::write(&path, saved).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let before = contents(&directory.0);
    assert!(
        Index::open(&directory.0, false)
            .err()
            .unwrap()
            .contains("Stale")
    );
    assert_eq!(contents(&directory.0), before);
}

#[test]
fn unordered_churn_matches_independent_ordered_reference_and_range_scans() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    let mut expected = std::collections::BTreeMap::new();
    for round in 0..6u64 {
        let mut changes = Vec::new();
        for position in 0..401u64 {
            let id = (position * 179 + round * 17) % 401;
            if (id + round) % 3 == 0 {
                expected.remove(&key(id));
                changes.push(Change::Delete(key(id)));
            } else {
                let data = vec![(round + id) as u8; (id as usize % 31) + 1];
                expected.insert(key(id), data.clone());
                changes.push(Change::Put(key(id), data));
            }
        }
        index.commit(changes).unwrap();
        assert_eq!(index.len().unwrap(), expected.len() as u64);
        let mut query = Query {
            start: key(50),
            end: Some(key(351)),
            limit: 23,
            ..Query::default()
        };
        let mut rows = Vec::new();
        loop {
            let page = index.page(&query).unwrap();
            rows.extend(page.entries);
            query.after = page.next;
            if page.next.is_none() {
                break;
            }
        }
        let reference = expected
            .range(key(50)..key(351))
            .map(|(key, value)| (*key, value.clone()))
            .collect::<Vec<_>>();
        assert_eq!(rows, reference);
    }
    drop(index);
    let mut index = Index::open(&directory.0, true).unwrap();
    for (key, value) in expected {
        assert_eq!(index.get(&key).unwrap(), Some(value));
    }
}

#[test]
fn record_envelopes_bind_the_exact_key_even_when_payloads_are_identical() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index
        .commit(vec![
            Change::Put(key(1), Vec::new()),
            Change::Put(key(2), Vec::new()),
        ])
        .unwrap();
    drop(index);
    let object = |id| {
        directory.0.join("objects/00").join(
            key(id)
                .iter()
                .skip(1)
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        )
    };
    fs::copy(object(2), object(1)).unwrap();
    let mut index = Index::open(&directory.0, true).unwrap();
    assert!(index.get(&key(1)).is_err());
    assert_eq!(index.get(&key(2)).unwrap(), Some(Vec::new()));
}
