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
fn closing_an_index_releases_ownership_even_with_an_inherited_descriptor() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit(vec![put(1)]).unwrap();
    let inherited = index._lock.try_clone().unwrap();
    assert!(Index::open(&directory.0, false).is_err());
    drop(index);
    let mut reopened = Index::open(&directory.0, false).unwrap();
    assert_eq!(
        reopened.get(&key(1)).unwrap(),
        Some(1_u64.to_be_bytes().to_vec())
    );
    drop(inherited);
    assert!(Index::open(&directory.0, true).is_err());
    drop(reopened);
    assert!(Index::open(&directory.0, true).is_ok());
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
        let summary = index.range_summary(key(0), None).unwrap();
        assert_eq!(summary.count, count);
        assert_eq!(summary.first, Some(key(0)));
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
        let saved = fault != Fault::Staged;
        let expect = |index: &mut Index| {
            assert_eq!(index.get(&key(3)).unwrap().is_none(), saved, "{fault:?}");
            let eight = if saved {
                b"replacement".to_vec()
            } else {
                8u64.to_be_bytes().to_vec()
            };
            assert_eq!(index.get(&key(8)).unwrap(), Some(eight));
            assert_eq!(index.get(&key(70)).unwrap().is_some(), saved);
            assert_eq!(index.len().unwrap(), 40);
        };
        // The poisoned index never checkpointed: a read-only open replays
        // its records in memory and writes nothing.
        let before = contents(&directory.0);
        expect(&mut Index::open(&directory.0, true).unwrap());
        assert_eq!(contents(&directory.0), before);
        let mut index = Index::open(&directory.0, false).unwrap();
        expect(&mut index);
        drop(index);
        let mut again = Index::open(&directory.0, true).unwrap();
        assert_eq!(again.len().unwrap(), 40);
        assert!(again.get(&key(71)).unwrap().is_none());
    }
}

#[test]
fn records_restore_writes_a_power_loss_discarded_since_the_checkpoint() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit((0..40).map(put).collect()).unwrap();
    index
        .commit(vec![Change::Put(key(41), vec![7; 3000])])
        .unwrap();
    index.checkpoint();
    // Only the redo record of each later commit is flushed.
    let checkpointed = |name: &str| fs::read(directory.0.join(name)).unwrap();
    let tree = checkpointed("tree.bin");
    let segment = checkpointed("segments/0000000000000000");
    for id in 100..130 {
        index
            .commit(vec![
                Change::Delete(key(id - 100)),
                Change::Put(key(id), vec![9; 2000]),
            ])
            .unwrap();
    }
    index.fault = Some(Fault::Published);
    assert!(index.commit(vec![put(200)]).is_err());
    drop(index);
    // The disk kept nothing written after the checkpoint but the redo log.
    fs::write(directory.0.join("tree.bin"), tree).unwrap();
    fs::write(directory.0.join("segments/0000000000000000"), segment).unwrap();
    let expect = |index: &mut Index| {
        for id in 0..30 {
            assert!(index.get(&key(id)).unwrap().is_none());
        }
        for id in (30..40).chain(100..130) {
            assert!(index.get(&key(id)).unwrap().is_some(), "{id}");
        }
        assert_eq!(index.get(&key(41)).unwrap(), Some(vec![7; 3000]));
        assert_eq!(index.get(&key(129)).unwrap(), Some(vec![9; 2000]));
        assert!(index.get(&key(200)).unwrap().is_some());
        assert_eq!(index.len().unwrap(), 42);
    };
    let before = contents(&directory.0);
    expect(&mut Index::open(&directory.0, true).unwrap());
    assert_eq!(contents(&directory.0), before);
    expect(&mut Index::open(&directory.0, false).unwrap());
    expect(&mut Index::open(&directory.0, true).unwrap());
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
    index.legacy_records = true;
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
fn an_interrupted_last_record_is_dropped_and_damage_before_it_is_refused() {
    for truncate in [false, true] {
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        index.commit(vec![put(1)]).unwrap();
        index.checkpoint();
        index.fault = Some(Fault::Published);
        assert!(index.commit(vec![put(2)]).is_err());
        drop(index);
        let path = directory.0.join("redo.bin");
        let mut bytes = fs::read(&path).unwrap();
        // The only record after the checkpoint reads as never written.
        if truncate {
            bytes.truncate(16384 + 24 + 100);
        } else {
            bytes[16384 + 100] ^= 1;
        }
        fs::write(&path, bytes).unwrap();
        let mut index = Index::open(&directory.0, false).unwrap();
        assert!(index.get(&key(1)).unwrap().is_some());
        assert!(index.get(&key(2)).unwrap().is_none());
    }
    // A damaged record followed by an intact one is not an interrupted
    // write: nothing is replayed or changed.
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit(vec![put(1)]).unwrap();
    index.fault = Some(Fault::Published);
    assert!(index.commit(vec![put(2)]).is_err());
    drop(index);
    let path = directory.0.join("redo.bin");
    let mut bytes = fs::read(&path).unwrap();
    bytes[16384 + 100] ^= 1;
    fs::write(&path, bytes).unwrap();
    let before = contents(&directory.0);
    assert!(
        Index::open(&directory.0, false)
            .err()
            .unwrap()
            .contains("damaged")
    );
    assert_eq!(contents(&directory.0), before);

    let source = Directory::new();
    let target = Directory::new();
    let mut index = Index::create(&source.0).unwrap();
    index.fault = Some(Fault::Published);
    assert!(index.commit(vec![put(1)]).is_err());
    drop(index);
    drop(Index::create(&target.0).unwrap());
    fs::copy(source.0.join("redo.bin"), target.0.join("redo.bin")).unwrap();
    fs::set_permissions(target.0.join("redo.bin"), fs::Permissions::from_mode(0o600)).unwrap();
    let before = contents(&target.0);
    assert!(Index::open(&target.0, false).is_err());
    assert_eq!(contents(&target.0), before);
}

/// Turns a closed store back into the format of releases before the redo
/// log: a SHA-256 root and no redo file.
pub(crate) fn legacy_root(directory: &Path) {
    let path = directory.join("tree.bin");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut header = Header::decode(&tree::read_page(&mut file, 0).unwrap()).unwrap();
    // Earlier releases sealed every segment: `head` named the next one.
    if header.head_length > 0 {
        header.head += 1;
        header.head_length = 0;
    }
    file.write_all(&header.encode_legacy()).unwrap();
    fs::remove_file(directory.join("redo.bin")).unwrap();
}

#[test]
fn stores_from_before_the_redo_log_adopt_one_only_when_writable() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit(vec![put(1)]).unwrap();
    drop(index);
    legacy_root(&directory.0);
    let before = contents(&directory.0);
    let mut index = Index::open(&directory.0, true).unwrap();
    assert!(index.get(&key(1)).unwrap().is_some());
    drop(index);
    assert_eq!(contents(&directory.0), before);
    let mut index = Index::open(&directory.0, false).unwrap();
    assert!(directory.0.join("redo.bin").exists());
    let root = tree::read_page(&mut index.tree, 0).unwrap();
    assert!(
        !Header::is_legacy(&root),
        "earlier releases must refuse the store"
    );
    index.commit(vec![put(2)]).unwrap();
    drop(index);
    // A new-format root without its redo log is never treated as legacy.
    fs::remove_file(directory.0.join("redo.bin")).unwrap();
    assert!(
        Index::open(&directory.0, false)
            .err()
            .unwrap()
            .contains("missing")
    );
}

#[test]
fn incomplete_unpublished_staging_is_discarded_only_by_writable_open() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit(vec![put(1)]).unwrap();
    drop(index);
    legacy_root(&directory.0);
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
    index.legacy_records = true;
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
    let path = directory.0.join("redo.bin");
    let saved = fs::read(&path).unwrap();
    // Keep the full framing/checksum valid but change a new-key put into an
    // existing-key put. Counts no longer agree; no recovery write is allowed.
    let mut body = super::persistence::redo_bodies(&directory.0).remove(0);
    let count_offset = 2 * PAGE_SIZE;
    let pages =
        u32::from_le_bytes(body[count_offset..count_offset + 4].try_into().unwrap()) as usize;
    let action_offset = count_offset + 4 + pages * (PAGE_SIZE + 8) + 4;
    body[action_offset + 65] = 1;
    super::persistence::rewrite_redo_body(&directory.0, 0, &body);
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
        let summary = index.range_summary(key(50), Some(key(351))).unwrap();
        assert_eq!(summary.count, reference.len() as u64);
        assert_eq!(summary.first, reference.first().map(|(key, _)| *key));
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
    index.legacy_records = true;
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

fn object_exists(directory: &Path, id: u64) -> bool {
    directory
        .join("objects/00")
        .join(
            key(id)
                .iter()
                .skip(1)
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        )
        .exists()
}

fn inline(id: u64, data: &[u8]) -> Change {
    Change::PutInline(key(id), data.to_vec())
}

#[test]
fn inline_values_live_in_the_tree_and_switch_kinds_without_orphan_files() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.legacy_records = true;
    index
        .commit(vec![
            inline(1, b"abc"),
            inline(2, &[]),
            inline(3, &[7; 32]),
            Change::Put(key(4), vec![9; 100]),
        ])
        .unwrap();
    assert!(inline_rejected(&mut index));
    assert!(!object_exists(&directory.0, 1));
    assert!(!object_exists(&directory.0, 3));
    assert!(object_exists(&directory.0, 4));
    assert_eq!(index.get(&key(1)).unwrap(), Some(b"abc".to_vec()));
    assert_eq!(index.get(&key(2)).unwrap(), Some(Vec::new()));
    assert_eq!(index.get(&key(3)).unwrap(), Some(vec![7; 32]));
    let page = index.page(&Query::default()).unwrap();
    assert_eq!(page.entries.len(), 4);
    assert_eq!(page.entries[0].1, b"abc");
    assert_eq!(page.entries[3].1, vec![9; 100]);
    // A file value becomes inline and its record file is removed; the reverse
    // creates the file. The cache never serves the earlier value.
    index.get(&key(4)).unwrap();
    index
        .commit(vec![
            inline(4, b"now tiny"),
            Change::Put(key(1), vec![5; 40]),
        ])
        .unwrap();
    assert_eq!(index.get(&key(4)).unwrap(), Some(b"now tiny".to_vec()));
    assert_eq!(index.get(&key(1)).unwrap(), Some(vec![5; 40]));
    assert!(!object_exists(&directory.0, 4));
    assert!(object_exists(&directory.0, 1));
    index
        .commit(vec![Change::Delete(key(2)), Change::Delete(key(4))])
        .unwrap();
    assert_eq!(index.len().unwrap(), 2);
    assert!(index.get(&key(2)).unwrap().is_none());
    drop(index);
    let mut index = Index::open(&directory.0, true).unwrap();
    assert_eq!(index.get(&key(3)).unwrap(), Some(vec![7; 32]));
    assert_eq!(index.get(&key(1)).unwrap(), Some(vec![5; 40]));
    assert_eq!(index.len().unwrap(), 2);
}

fn inline_rejected(index: &mut Index) -> bool {
    index.commit(vec![inline(9, &[1; 33])]).is_err()
        && index
            .commit(vec![inline(9, b"x"), Change::Delete(key(9))])
            .is_err()
        && index.get(&key(9)).unwrap().is_none()
}

#[test]
fn inline_values_recover_at_every_commit_boundary_without_leftover_files() {
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
        index
            .commit((0..40).map(|id| inline(id, &id.to_be_bytes())).collect())
            .unwrap();
        // A record file from before the value log, replaced by an inline value.
        index.legacy_records = true;
        index.commit(vec![put(100)]).unwrap();
        index.legacy_records = false;
        index.fault = Some(fault);
        assert!(
            index
                .commit(vec![
                    Change::Delete(key(3)),
                    inline(8, b"replacement"),
                    inline(100, b"was a file"),
                    Change::Put(key(70), vec![1; 64]),
                ])
                .is_err()
        );
        assert!(index.get(&key(8)).is_err());
        drop(index);
        let mut index = Index::open(&directory.0, false).unwrap();
        if fault == Fault::Staged {
            assert_eq!(
                index.get(&key(8)).unwrap(),
                Some(8u64.to_be_bytes().to_vec())
            );
            assert_eq!(
                index.get(&key(100)).unwrap(),
                Some(100u64.to_be_bytes().to_vec())
            );
            assert!(object_exists(&directory.0, 100));
            assert!(index.get(&key(70)).unwrap().is_none());
        } else {
            assert!(index.get(&key(3)).unwrap().is_none());
            assert_eq!(index.get(&key(8)).unwrap(), Some(b"replacement".to_vec()));
            assert_eq!(index.get(&key(100)).unwrap(), Some(b"was a file".to_vec()));
            assert!(!object_exists(&directory.0, 100));
            assert_eq!(index.get(&key(70)).unwrap(), Some(vec![1; 64]));
        }
        assert!(contents(&directory.0).iter().all(|(path, _, _)| {
            let path = path.to_string_lossy();
            !path.contains(".record-") && !path.contains(".segment-")
        }));
        assert_eq!(index.len().unwrap(), 41);
    }
}

#[test]
fn malformed_inline_references_are_rejected_as_corruption() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit(vec![inline(1, b"ab")]).unwrap();
    let root = index.header.root;
    drop(index);
    let path = directory.0.join("tree.bin");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let original = tree::read_page(&mut file, root).unwrap();
    for (offset, value) in [
        // Data beyond the declared inline length must be zero padding.
        (64 + 68 + 5, 1u8),
        // An inline length larger than the 32 inline bytes.
        (64 + 64, 40u8),
    ] {
        let mut page = original;
        page[offset] = value;
        let digest = sha256(&page[..PAGE_SIZE - 32]);
        page[PAGE_SIZE - 32..].copy_from_slice(&digest);
        file.seek(SeekFrom::Start(tree::offset(root).unwrap()))
            .unwrap();
        file.write_all(&page).unwrap();
        assert!(Index::open(&directory.0, false).is_err(), "{offset}");
    }
    file.seek(SeekFrom::Start(tree::offset(root).unwrap()))
        .unwrap();
    file.write_all(&original).unwrap();
    let mut index = Index::open(&directory.0, false).unwrap();
    assert_eq!(index.get(&key(1)).unwrap(), Some(b"ab".to_vec()));
}

#[test]
fn one_transaction_may_change_up_to_the_operation_bound_and_no_more() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    let limit = super::MAX_CHANGES as u64;
    index
        .commit(
            (0..limit)
                .rev()
                .map(|id| inline(id, &id.to_be_bytes()))
                .collect(),
        )
        .unwrap();
    assert_eq!(index.len().unwrap(), limit);
    assert!(index.last_staged_nodes <= super::tree::MAX_DIRTY_PAGES);
    assert!(
        index
            .commit((limit..=limit * 2).map(|id| inline(id, b"x")).collect())
            .is_err()
    );
    assert_eq!(index.len().unwrap(), limit);
    drop(index);
    let mut index = Index::open(&directory.0, true).unwrap();
    assert_eq!(
        index.get(&key(limit - 1)).unwrap(),
        Some((limit - 1).to_be_bytes().to_vec())
    );
}

#[test]
fn cached_pages_never_outlive_a_commit_or_a_recovery() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit((0..300).map(put).collect()).unwrap();
    // Populate the cache along several paths, then rewrite those pages.
    for id in 0..300 {
        assert!(index.get(&key(id)).unwrap().is_some());
    }
    index
        .commit(
            (0..300)
                .step_by(7)
                .map(|id| Change::Delete(key(id)))
                .collect(),
        )
        .unwrap();
    for id in 0..300u64 {
        assert_eq!(index.get(&key(id)).unwrap().is_some(), id % 7 != 0, "{id}");
    }
    let page = index
        .page(&Query {
            limit: 200,
            ..Query::default()
        })
        .unwrap();
    assert!(page.entries.iter().all(|(found, _)| found != &key(0)));
    index.fault = Some(Fault::Published);
    assert!(index.commit(vec![Change::Delete(key(1))]).is_err());
    drop(index);
    let mut index = Index::open(&directory.0, false).unwrap();
    assert!(index.get(&key(1)).unwrap().is_none());
}

/// Each private index file, swapped after inspection for a symlink to its
/// regular original or for a FIFO, is refused at open time without blocking.
#[test]
fn swapped_index_files_are_refused_at_open_without_blocking() {
    use crate::store::open_tests::{Swap, swapped};
    for name in ["identity.bin", ".lock", "tree.bin", "redo.bin", "segment"] {
        for swap in [Swap::Symlink, Swap::Fifo, Swap::Replaced] {
            let directory = Directory::new();
            let mut index = Index::create(&directory.0).unwrap();
            // Too large to inline, so the value lives in a log segment.
            let value = vec![7; super::MAX_INLINE_BYTES + 1];
            index.commit(vec![Change::Put(key(1), value)]).unwrap();
            drop(index);
            assert!(directory.0.join(name).exists() || name == "segment");
            let path = directory.0.clone();
            let error = if name == "segment" {
                let mut index = Index::open(&directory.0, false).unwrap();
                let segments: Vec<_> = fs::read_dir(directory.0.join("segments"))
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect();
                let [segment] = segments.as_slice() else {
                    panic!("expected one value segment, found {segments:?}");
                };
                swapped(segment, 0, swap, move || index.get(&key(1)).map(drop))
            } else {
                swapped(name, 0, swap, move || Index::open(&path, false).map(drop))
            };
            match swap {
                Swap::Symlink => assert!(
                    error.starts_with("Cannot open inventory file"),
                    "{name}: {error}"
                ),
                Swap::Fifo => assert!(
                    error.starts_with("Cannot open inventory file")
                        || error.starts_with("Inventory files must be private regular files"),
                    "{name}: {error}"
                ),
                Swap::Replaced => {
                    assert_eq!(error, "Inventory file changed while opening", "{name}")
                }
            }
        }
    }
}

#[test]
fn stats_follow_growth_deletion_restart_and_never_read_records() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    let empty = index.stats();
    assert_eq!(
        (empty.entries, empty.log_bytes, empty.live_bytes),
        (0, 0, 0)
    );
    for batch in 0..3u64 {
        index
            .commit((batch * 200..(batch + 1) * 200).map(put).collect())
            .unwrap();
    }
    let grown = index.stats();
    assert_eq!(grown.entries, 600);
    assert!(grown.live_bytes > 0 && grown.log_bytes >= grown.live_bytes);
    assert_eq!(
        grown.index_bytes,
        fs::metadata(directory.0.join("tree.bin")).unwrap().len()
    );
    index
        .commit((0..150).map(|id| Change::Delete(key(id))).collect())
        .unwrap();
    let shrunk = index.stats();
    assert_eq!(shrunk.entries, 450);
    assert!(shrunk.live_bytes < grown.live_bytes);
    assert!(!shrunk.uncertain);
    let reads = index.requests();
    let _ = index.stats();
    assert_eq!(index.requests(), reads);
    drop(index);
    let reopened = Index::open(&directory.0, true).unwrap();
    assert_eq!(reopened.stats(), shrunk);
    let report = super::report("index", Some(&reopened));
    assert_eq!(
        report.get("entries").and_then(crate::json::Value::as_u64),
        Some(450)
    );
    assert_eq!(
        report.get("state").and_then(crate::json::Value::as_str),
        Some("index")
    );
}

#[test]
fn a_full_disk_at_each_commit_stage_keeps_every_committed_record() {
    use super::persistence::Stage;
    for (stage, legacy) in [
        (Stage::Wal, false),
        (Stage::Objects, false),
        (Stage::Objects, true),
        (Stage::Tree, false),
        (Stage::Directory, false),
    ] {
        let parent = Directory::new();
        fs::create_dir(&parent.0).unwrap();
        let path = parent.0.join("jobs.idx");
        let mut index = Index::create(&path).unwrap();
        index.legacy_records = legacy;
        index.commit((0..40).map(put).collect()).unwrap();
        index.fault = Some(Fault::Full(stage));
        let error = index
            .commit(vec![Change::Delete(key(3)), put(70)])
            .unwrap_err();
        assert!(error.starts_with("Job storage: "), "{error}");
        assert!(error.contains("disk"), "{error}");
        assert!(!error.contains(parent.0.to_str().unwrap()), "{error}");
        let saved = stage != Stage::Wal;
        if saved {
            assert!(
                error.contains("uncertain") && error.contains("restart"),
                "{error}"
            );
        } else {
            assert!(error.contains("was not saved"), "{error}");
        }
        assert!(index.stats().uncertain);
        let paused = index.get(&key(1)).unwrap_err();
        assert!(paused.starts_with("Job storage: "), "{paused}");
        drop(index);
        // Space is free again: recovery replays a published change or
        // discards an unpublished one, and nothing committed is lost.
        let mut index = Index::open(&path, false).unwrap();
        for id in (0..40).filter(|id| *id != 3) {
            assert_eq!(
                index.get(&key(id)).unwrap(),
                Some(id.to_be_bytes().to_vec())
            );
        }
        assert_eq!(index.get(&key(3)).unwrap().is_none(), saved, "{stage:?}");
        assert_eq!(index.get(&key(70)).unwrap().is_some(), saved, "{stage:?}");
        assert_eq!(index.len().unwrap(), 40);
        assert!(!index.stats().uncertain);
        index.commit(vec![put(71)]).unwrap();
    }
}

#[test]
fn operation_bounds_have_distinct_errors_and_change_nothing() {
    let parent = Directory::new();
    fs::create_dir(&parent.0).unwrap();
    let mut index = Index::create(&parent.0.join("series.idx.migrating")).unwrap();
    index.commit(vec![put(1)]).unwrap();
    let before = index.stats();
    let oversized = index
        .commit(vec![Change::Put(
            key(2),
            vec![0; super::MAX_VALUE_BYTES + 1],
        )])
        .unwrap_err();
    assert!(oversized.starts_with("Series storage: a single record is larger than 1 MiB"));
    let bound = index
        .commit((0..super::MAX_CHANGES as u64 + 1).map(put).collect())
        .unwrap_err();
    assert!(bound.contains("operation bound") && bound.contains("not an inventory limit"));
    assert_ne!(oversized, bound);
    for error in [&oversized, &bound] {
        assert!(error.contains("not saved") && error.contains("existing records are unchanged"));
    }
    assert!(index.is_usable());
    assert_eq!(index.stats(), before);
    drop(index);
    let read_only = Index::open(&parent.0.join("series.idx.migrating"), true).unwrap();
    assert_eq!(read_only.stats(), before);
}

#[test]
fn a_change_over_the_wal_bound_only_with_its_pages_is_an_operation_bound() {
    let parent = Directory::new();
    fs::create_dir(&parent.0).unwrap();
    let path = parent.0.join("series.idx");
    let mut index = Index::create(&path).unwrap();
    index.commit(vec![put(1)]).unwrap();
    let before = index.stats();
    // The values alone fill the bound exactly, which the check before
    // staging accepts; tree pages and WAL framing then exceed it.
    let count = (super::persistence::MAX_WAL_BYTES / super::MAX_VALUE_BYTES) as u64;
    let error = index
        .commit(
            (100..100 + count)
                .map(|id| Change::Put(key(id), vec![0; super::MAX_VALUE_BYTES]))
                .collect(),
        )
        .unwrap_err();
    assert!(
        error.starts_with("Series storage: one change needs more than 128 MiB"),
        "{error}"
    );
    for part in [
        "operation bound",
        "not an inventory limit",
        "not saved",
        "existing records are unchanged",
        "Report the item",
    ] {
        assert!(error.contains(part), "{error}");
    }
    assert!(!error.contains(parent.0.to_str().unwrap()), "{error}");
    assert!(index.is_usable());
    assert_eq!(index.stats(), before);
    index.commit(vec![put(2)]).unwrap();
    drop(index);
    let mut reopened = Index::open(&path, true).unwrap();
    assert_eq!(reopened.len().unwrap(), 2);
    assert!(reopened.get(&key(100)).unwrap().is_none());
}

#[test]
fn store_names_come_from_the_index_directory() {
    for (directory, name) in [
        ("jobs.idx", "Job storage"),
        ("jobs.idx.migrating", "Job storage"),
        ("series.idx", "Series storage"),
        ("requesters.idx.staging", "Requester storage"),
        ("announcements.idx", "IRC announcement storage"),
        ("indexers.idx", "Source policy storage"),
        ("feeds.idx.migrating", "Feed storage"),
        ("transfers.idx", "Transfer storage"),
        ("mynou-inventory-0011", "Inventory storage"),
    ] {
        assert_eq!(super::store_name(Path::new(directory)), name);
    }
    let full = super::persistence::explain_open(
        Path::new("/private/media/feeds.idx"),
        format!(
            "Cannot apply inventory page: {}",
            crate::store::STORAGE_FULL
        ),
    );
    assert!(full.starts_with("Feed storage: the disk is full"));
    assert!(!full.contains("/private"));
    assert_eq!(
        super::persistence::explain_open(Path::new("feeds.idx"), "other".into()),
        "other"
    );
}

/// Synthetic pre-count index: preserve every key/value and encode branches
/// using their original 72-byte links. Production never strips summaries.
pub(crate) fn legacy_range_index(index: &mut Index) {
    // Pending records would restore the counted pages on replay.
    index.checkpoint();
    for id in 1..index.header.next {
        let bytes = tree::read_page(&mut index.tree, id).unwrap();
        let mut node = tree::Node::decode(
            &bytes,
            id,
            bytes[16],
            index.header.next,
            index.header.identity,
        )
        .unwrap();
        if let tree::Items::Branch(links) = &mut node.items {
            for link in links {
                link.count = None;
            }
            index
                .tree
                .seek(SeekFrom::Start(tree::offset(id).unwrap()))
                .unwrap();
            index.tree.write_all(&node.encode().unwrap()).unwrap();
        }
    }
    index.tree.sync_all().unwrap();
    index.cache.clear();
}

fn assert_range(
    index: &mut Index,
    start: u64,
    end: Option<u64>,
    rows: &std::collections::BTreeSet<u64>,
) {
    index.cache.clear(); // Measure cold reads, including both rank boundaries.
    let expected: Vec<_> = rows
        .iter()
        .copied()
        .filter(|id| *id >= start && end.is_none_or(|end| *id < end))
        .collect();
    let summary = index.range_summary(key(start), end.map(key)).unwrap();
    assert_eq!(summary.count, expected.len() as u64, "{start}..{end:?}");
    assert_eq!(summary.first, expected.first().copied().map(key));
    assert!(
        index.last_page_reads() <= 3 * (usize::from(index.header.level) + 1),
        "{} reads at height {}",
        index.last_page_reads(),
        index.header.level + 1
    );
}

#[test]
fn range_summaries_are_exact_and_bounded_through_growth_replacement_and_root_collapse() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    let mut rows = std::collections::BTreeSet::new();
    assert_range(&mut index, 0, None, &rows);
    for start in (0..4096).step_by(256) {
        index
            .commit((start..start + 256).rev().map(put).collect())
            .unwrap();
        rows.extend(start..start + 256);
        for (start, end) in [
            (0, None),
            (13, Some(317)),
            (317, Some(317)),
            (4096, None),
            (97, Some(4096)),
        ] {
            assert_range(&mut index, start, end, &rows);
        }
    }
    // A replacement and deleting an absent key leave cardinalities unchanged.
    index
        .commit(vec![put(17), Change::Delete(key(5000))])
        .unwrap();
    assert_range(&mut index, 0, None, &rows);
    assert!(index.range_summary(key(10), Some(key(9))).is_err());
    drop(index);
    let mut index = Index::open(&directory.0, false).unwrap();
    assert_range(&mut index, 17, Some(4095), &rows);
    for start in (0..4096).step_by(256) {
        index
            .commit(
                (start..start + 256)
                    .map(|id| Change::Delete(key(id)))
                    .collect(),
            )
            .unwrap();
        for id in start..start + 256 {
            rows.remove(&id);
        }
        assert_range(&mut index, 0, None, &rows);
        assert_range(&mut index, 31, Some(2049), &rows);
    }
    assert_eq!(index.header.root, 0);
    index.commit(vec![put(42)]).unwrap();
    rows.insert(42);
    assert_range(&mut index, 0, None, &rows);
}

#[test]
fn range_summaries_keep_equal_time_boundaries_and_do_not_read_values() {
    fn due(table: u8, at: u64, id: u64) -> Key {
        let mut key = key(id);
        key[0] = table;
        key[1..9].copy_from_slice(&at.to_be_bytes());
        key
    }
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.legacy_records = true;
    index
        .commit(
            (0..200)
                .map(|id| Change::Put(due(7, id / 100, id), vec![42; 40]))
                .collect(),
        )
        .unwrap();
    assert_eq!(
        index
            .range_summary(due(7, 0, 0), Some(due(7, 1, 0)))
            .unwrap(),
        super::RangeSummary {
            count: 100,
            first: Some(due(7, 0, 0))
        }
    );
    assert_eq!(
        index
            .range_summary(due(7, 1, 0), Some(due(8, 0, 0)))
            .unwrap()
            .count,
        100
    );
    assert_eq!(index.range_summary(due(7, 2, 0), None).unwrap().count, 0);
    let damaged = super::damage_record(&directory.0, &due(7, 0, 0));
    assert_eq!(damaged, 1);
    assert!(index.get(&due(7, 0, 0)).is_err());
    assert_eq!(index.range_summary(due(7, 0, 0), None).unwrap().count, 200);
    drop(index);
    let before = contents(&directory.0);
    let mut index = Index::open(&directory.0, true).unwrap();
    assert_eq!(index.range_summary(due(7, 0, 0), None).unwrap().count, 200);
    assert!(
        index
            .prepare_range_counts(1)
            .unwrap_err()
            .contains("read-only")
    );
    drop(index);
    assert_eq!(contents(&directory.0), before);
}

#[test]
fn legacy_range_upgrade_is_explicit_bounded_resumable_and_preserves_values() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit((0..2049).map(put).collect()).unwrap();
    legacy_range_index(&mut index);
    let original = index.stats();
    drop(index);
    let before = contents(&directory.0);
    let mut index = Index::open(&directory.0, true).unwrap();
    assert!(
        index
            .range_summary(key(0), None)
            .unwrap_err()
            .contains("explicit writable upgrade")
    );
    assert_eq!(
        index
            .page(&Query {
                limit: 1,
                ..Query::default()
            })
            .unwrap()
            .entries[0]
            .0,
        key(0)
    );
    drop(index);
    assert_eq!(contents(&directory.0), before);
    let mut calls = 0;
    loop {
        let mut index = Index::open(&directory.0, false).unwrap();
        assert!(index.prepare_range_counts(0).is_err());
        assert!(index.prepare_range_counts(257).is_err());
        let complete = index.prepare_range_counts(1).unwrap();
        calls += 1;
        assert!(calls < 100);
        assert!(index.last_staged_nodes <= 1 + usize::from(index.header.level));
        assert!(index.last_page_reads() <= 32 * (1 + usize::from(index.header.level)) + 1);
        let stats = index.stats();
        assert_eq!(
            (
                stats.entries,
                stats.index_bytes,
                stats.log_bytes,
                stats.live_bytes
            ),
            (
                original.entries,
                original.index_bytes,
                original.log_bytes,
                original.live_bytes
            )
        );
        if complete {
            assert_eq!(
                index.range_summary(key(17), Some(key(2000))).unwrap().count,
                1983
            );
            for id in 0..2049 {
                assert_eq!(
                    index.get(&key(id)).unwrap(),
                    Some(id.to_be_bytes().to_vec())
                );
            }
            let sequence = index.sequence().unwrap();
            assert!(index.prepare_range_counts(1).unwrap());
            assert_eq!(index.sequence().unwrap(), sequence);
            // Subsequent ordinary changes maintain upgraded summaries.
            index
                .commit(vec![Change::Delete(key(42)), put(4097)])
                .unwrap();
            assert_eq!(
                index.range_summary(key(42), Some(key(2049))).unwrap().count,
                2006
            );
            break;
        }
    }
    assert!(calls > 1);
    let mut index = Index::open(&directory.0, true).unwrap();
    assert_eq!(index.range_summary(key(0), None).unwrap().count, 2049);
}

#[test]
fn interrupted_or_full_disk_range_upgrades_recover_without_record_loss() {
    use super::persistence::Stage;
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
        Fault::Full(Stage::Wal),
        Fault::Full(Stage::Directory),
        Fault::Full(Stage::Tree),
    ] {
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        index.commit((0..200).map(put).collect()).unwrap();
        legacy_range_index(&mut index);
        index.inject(Some(fault));
        let error = index.prepare_range_counts(1).unwrap_err();
        assert!(!error.contains(directory.0.to_str().unwrap()));
        assert!(!index.is_usable());
        assert!(index.range_summary(key(0), None).is_err());
        drop(index);
        let mut index = Index::open(&directory.0, false).unwrap();
        assert!(index.prepare_range_counts(1).unwrap());
        assert_eq!(
            index.range_summary(key(3), Some(key(197))).unwrap().count,
            194
        );
        for id in 0..200 {
            assert_eq!(
                index.get(&key(id)).unwrap(),
                Some(id.to_be_bytes().to_vec())
            );
        }
    }
}

#[test]
fn counted_branches_reject_overflow_and_inconsistent_cardinalities() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit((0..200).map(put).collect()).unwrap();
    let bytes = tree::read_page(&mut index.tree, index.header.root).unwrap();
    assert_eq!(&bytes[..8], b"MYNOUIN4"); // Older readers reject this version.
    let mut node = tree::Node::decode(
        &bytes,
        index.header.root,
        index.header.level,
        index.header.next,
        index.header.identity,
    )
    .unwrap();
    let tree::Items::Branch(links) = &mut node.items else {
        panic!("branch required")
    };
    links[0].count = Some(u64::MAX);
    assert!(node.encode().unwrap_err().contains("overflow"));
    let tree::Items::Branch(links) = &mut node.items else {
        unreachable!()
    };
    links[0].count = Some(1);
    index
        .tree
        .seek(SeekFrom::Start(tree::offset(index.header.root).unwrap()))
        .unwrap();
    index.tree.write_all(&node.encode().unwrap()).unwrap();
    index.cache.clear();
    assert!(
        index
            .range_summary(key(0), None)
            .unwrap_err()
            .contains("disagrees")
    );
}

#[test]
fn forged_count_upgrade_wal_is_rejected_before_writing_any_state() {
    for change_entries in [false, true] {
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        index.commit((0..200).map(put).collect()).unwrap();
        legacy_range_index(&mut index);
        let mut staged = tree::Tree::new(&mut index.tree, index.header, &mut index.cache);
        assert!(staged.prepare_counts(1).unwrap());
        let root = staged.dirty.get_mut(&index.header.root).unwrap();
        let tree::Items::Branch(links) = &mut root.items else {
            unreachable!()
        };
        if change_entries {
            links[0].key[63] ^= 1;
        } else {
            // Keep the root total valid while lying about two subtrees.
            links[0].count = links[0].count.map(|count| count + 1);
            links[1].count = links[1].count.map(|count| count - 1);
        }
        staged.header.sequence += 1;
        let transaction = super::persistence::Transaction {
            before: index.header,
            after: staged.header,
            pages: staged.dirty,
            actions: Vec::new(),
        };
        let wal = transaction.encode().unwrap();
        drop(index);
        let path = directory.0.join("transaction.bin");
        fs::write(&path, wal).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let before = contents(&directory.0);
        assert!(Index::open(&directory.0, false).is_err());
        assert_eq!(contents(&directory.0), before);
    }
}

#[test]
fn partial_range_upgrade_redo_repairs_a_torn_branch_and_resumes_after_mutations() {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    index.commit((0..2049).map(put).collect()).unwrap();
    legacy_range_index(&mut index);
    let root = tree::read_page(&mut index.tree, index.header.root).unwrap();
    let root = tree::Node::decode(
        &root,
        index.header.root,
        index.header.level,
        index.header.next,
        index.header.identity,
    )
    .unwrap();
    let tree::Items::Branch(links) = root.items else {
        unreachable!()
    };
    let first_branch = links[0].page;
    index.inject(Some(Fault::Published));
    assert!(index.prepare_range_counts(1).is_err());
    drop(index);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(directory.0.join("tree.bin"))
        .unwrap();
    file.seek(SeekFrom::Start(tree::offset(first_branch).unwrap()))
        .unwrap();
    file.write_all(&[0; 128]).unwrap();
    file.sync_all().unwrap();
    // A read-only open reads the torn branch from its record, in memory.
    let before = contents(&directory.0);
    let mut reader = Index::open(&directory.0, true).unwrap();
    assert_eq!(reader.len().unwrap(), 2049);
    assert!(reader.get(&key(2048)).unwrap().is_some());
    drop(reader);
    assert_eq!(contents(&directory.0), before);
    let mut index = Index::open(&directory.0, false).unwrap();
    // A saved count stays correct even if ordinary operations run between
    // upgrade batches. Unknown siblings remain explicitly unknown.
    index
        .commit(vec![Change::Delete(key(13)), put(4097)])
        .unwrap();
    while !index.prepare_range_counts(1).unwrap() {}
    assert_eq!(index.range_summary(key(0), None).unwrap().count, 2049);
    assert_eq!(
        index.range_summary(key(0), Some(key(100))).unwrap().count,
        99
    );
    assert_eq!(index.get(&key(13)).unwrap(), None);
    assert!(index.get(&key(4097)).unwrap().is_some());
}
