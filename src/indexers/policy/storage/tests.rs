//! Independently proposed synthetic growth, provenance and interruption fixtures.
use super::*;
use crate::inventory::Fault;
use std::os::unix::fs::PermissionsExt;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mynou-policy-index-{}",
            digest(&random_bytes::<16>().unwrap())
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
    Record {
        id: format!("source-{n:057x}"),
        binding: format!("{n:064x}"),
        enabled: n.is_multiple_of(2),
        revision: u64::MAX,
        action: "reset_session".into(),
        at: u64::MAX,
    }
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
fn indexed(storage: &mut Storage) -> &mut Index {
    match &mut storage.backend {
        Backend::Ready(index) | Backend::Unpublished(index, _) => index,
        _ => panic!("indexed fixture"),
    }
}
fn contents(path: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    fn walk(path: &Path, rows: &mut BTreeMap<PathBuf, (Vec<u8>, u32)>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, rows);
            } else {
                rows.insert(
                    path.clone(),
                    (
                        fs::read(&path).unwrap(),
                        fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    ),
                );
            }
        }
    }
    let mut rows = BTreeMap::new();
    walk(path, &mut rows);
    rows
}
#[test]
fn point_operations_and_restart_have_a_fixed_request_budget() {
    let directory = Directory::new();
    let mut storage = Storage::open(&directory.0, false).unwrap();
    storage.initialize().unwrap();
    for start in (1..=401_u64).step_by(200) {
        let batch: Vec<_> = (start..(start + 200).min(402)).map(record).collect();
        put(indexed(&mut storage), &batch, None).unwrap();
    }
    assert_eq!(storage.total().unwrap(), 401);
    for n in [1, 199, 201, 401] {
        let before = indexed(&mut storage).requests();
        assert_eq!(storage.row(&record(n).id).unwrap(), Some(record(n)));
        assert_eq!(indexed(&mut storage).requests() - before, 1);
    }
    let before = indexed(&mut storage).requests();
    let old_guard = storage.guard().unwrap();
    let mut changed = record(401);
    changed.enabled = true;
    storage.commit(changed.clone()).unwrap();
    assert!(indexed(&mut storage).requests() - before <= 4);
    assert_ne!(storage.guard().unwrap(), old_guard);
    assert_eq!(storage.total().unwrap(), 401);
    drop(storage);
    let before = contents(&directory.0);
    let mut storage = Storage::open(&directory.0, true).unwrap();
    storage.initialize().unwrap();
    assert_eq!(storage.row(&changed.id).unwrap(), Some(changed));
    assert_eq!(storage.total().unwrap(), 401);
    assert_eq!(contents(&directory.0), before);
}
#[test]
fn legacy_migration_preserves_exact_rows_backup_and_readonly_inspection() {
    let directory = Directory::new();
    let records: Vec<_> = (1..=1000).map(record).collect();
    let original = snapshot(&records);
    private(&directory.0.join("indexers.bin"), &original);
    let before = contents(&directory.0);
    let mut storage = Storage::open(&directory.0, true).unwrap();
    storage.initialize().unwrap();
    assert!(storage.needs_migration());
    assert_eq!(
        storage.row(&records[999].id).unwrap(),
        Some(records[999].clone())
    );
    assert!(storage.commit(record(1001)).is_err());
    assert_eq!(contents(&directory.0), before);
    drop(storage);
    let mut storage = Storage::open(&directory.0, false).unwrap();
    storage.initialize().unwrap();
    assert!(!storage.needs_migration());
    for r in &records {
        assert_eq!(storage.row(&r.id).unwrap().as_ref(), Some(r));
    }
    assert_eq!(
        fs::read(
            directory
                .0
                .join(format!("indexers.legacy-{}.bin", digest(&original)))
        )
        .unwrap(),
        original
    );
    let marker = fs::read(directory.0.join("indexers.bin")).unwrap();
    assert_eq!(&marker[..8], MARKER);
    assert!(legacy(marker).is_err());
}
#[test]
fn migration_resumes_cursor_and_unmarked_promotion() {
    for promoted in [false, true] {
        let directory = Directory::new();
        let records: Vec<_> = (1..=250).map(record).collect();
        let original = snapshot(&records);
        private(&directory.0.join("indexers.bin"), &original);
        let staging = directory.0.join(if promoted {
            "indexers.idx"
        } else {
            "indexers.idx.migrating"
        });
        let mut index = create(&staging, sha256(&original)).unwrap();
        if promoted {
            put(&mut index, &records, None).unwrap_err();
            put(&mut index, &records[..200], None).unwrap();
            put(&mut index, &records[200..], None).unwrap();
        } else {
            put(&mut index, &records[..100], Some(&records[99].id)).unwrap();
        }
        let identity = index.identity();
        drop(index);
        let before = contents(&directory.0);
        let mut readonly = Storage::open(&directory.0, true).unwrap();
        readonly.initialize().unwrap();
        assert_eq!(contents(&directory.0), before);
        drop(readonly);
        let mut storage = Storage::open(&directory.0, false).unwrap();
        storage.initialize().unwrap();
        assert_eq!(indexed(&mut storage).identity(), identity);
        assert_eq!(storage.total().unwrap(), 250);
        for r in &records {
            assert_eq!(storage.row(&r.id).unwrap().as_ref(), Some(r));
        }
    }
}
#[test]
fn interrupted_or_foreign_staging_is_preserved_before_rebuild() {
    for corrupt in [false, true] {
        let directory = Directory::new();
        let original = snapshot(&[record(1)]);
        private(&directory.0.join("indexers.bin"), &original);
        let staging = directory.0.join("indexers.idx.migrating");
        if corrupt {
            fs::create_dir(&staging).unwrap();
            fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
            private(&staging.join("evidence"), b"interrupted");
        } else {
            drop(create(&staging, sha256(b"foreign")).unwrap());
        }
        let preserved = contents(&staging);
        let mut storage = Storage::open(&directory.0, false).unwrap();
        storage.initialize().unwrap();
        let abandoned: Vec<_> = fs::read_dir(&directory.0)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("indexers.idx.abandoned-")
            })
            .collect();
        assert_eq!(abandoned.len(), 1);
        for (path, bytes) in preserved {
            assert_eq!(
                contents(&abandoned[0])
                    .get(&abandoned[0].join(path.strip_prefix(&staging).unwrap())),
                Some(&bytes)
            );
        }
        assert_eq!(storage.row(&record(1).id).unwrap(), Some(record(1)));
    }
}
#[test]
fn uncertain_control_blocks_all_reads_until_writable_recovery() {
    for fault in [
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
    ] {
        let directory = Directory::new();
        let mut storage = Storage::open(&directory.0, false).unwrap();
        storage.initialize().unwrap();
        storage.commit(record(1)).unwrap();
        let mut paused = record(1);
        paused.enabled = false;
        paused.revision = 2;
        indexed(&mut storage).inject(Some(fault));
        assert!(storage.commit(paused.clone()).is_err());
        assert!(storage.row(&paused.id).is_err());
        assert!(storage.guard().is_err());
        drop(storage);
        assert!(Storage::open(&directory.0, true).is_err());
        let mut recovered = Storage::open(&directory.0, false).unwrap();
        recovered.initialize().unwrap();
        assert_eq!(recovered.row(&paused.id).unwrap(), Some(paused));
    }
}
#[test]
fn published_marker_rejects_wrong_identity_and_foreign_unmarked_index() {
    let first = Directory::new();
    let second = Directory::new();
    let mut a = Storage::open(&first.0, false).unwrap();
    a.initialize().unwrap();
    let mut b = Storage::open(&second.0, false).unwrap();
    b.initialize().unwrap();
    let other = fs::read(second.0.join("indexers.bin")).unwrap();
    drop(a);
    drop(b);
    private(&first.0.join("indexers.bin"), &other);
    let before = contents(&first.0);
    assert!(Storage::open(&first.0, false).is_err());
    assert_eq!(contents(&first.0), before);
    private(&first.0.join("indexers.bin"), &snapshot(&[record(99)]));
    assert!(Storage::open(&first.0, false).is_err());
}

#[test]
fn full_disk_at_every_migration_flush_preserves_legacy_and_restarts() {
    for boundary in 0..1000 {
        let directory = Directory::new();
        let records: Vec<_> = (1..=3).map(record).collect();
        let original = snapshot(&records);
        private(&directory.0.join("indexers.bin"), &original);
        let result = crate::store::with_full_disk_after(boundary, || {
            Storage::open(&directory.0, false).and_then(|mut s| s.initialize())
        });
        let error = match result {
            Ok(()) => {
                assert!(boundary > 10);
                return;
            }
            Err(error) => error,
        };
        assert!(
            error.starts_with("Source policy storage: "),
            "boundary {boundary}: {error}"
        );
        assert!(
            error.contains("disk") || error.contains(crate::store::STORAGE_FULL),
            "boundary {boundary}: {error}"
        );
        assert!(!error.contains(directory.0.to_str().unwrap()), "{error}");
        let live = fs::read(directory.0.join("indexers.bin")).unwrap();
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
        let mut storage = Storage::open(&directory.0, false).unwrap();
        storage.initialize().unwrap();
        for r in &records {
            assert_eq!(storage.row(&r.id).unwrap().as_ref(), Some(r));
        }
    }
    panic!("migration fixture exceeded its durability boundary budget");
}
