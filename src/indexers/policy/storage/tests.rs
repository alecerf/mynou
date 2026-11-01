//! Independently proposed synthetic growth, provenance and interruption fixtures.
use super::*;
use crate::inventory::Fault;
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mynou-policy-index-{}",
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
    Record {
        id: format!("source-{n:057x}"),
        binding: format!("{n:064x}"),
        enabled: n.is_multiple_of(2),
        revision: u64::MAX,
        action: "reset_session".into(),
        at: u64::MAX,
    }
}
fn put_all(index: &mut Index, records: &[Record]) {
    for record in records {
        put(index, record).unwrap();
    }
}
fn indexed(storage: &mut Storage) -> &mut Index {
    match &mut storage.backend {
        Backend::Ready(index) => index,
        Backend::Absent => panic!("indexed fixture"),
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
        put_all(indexed(&mut storage), &batch);
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
        // A read-only open replays the logged control in memory only.
        let before = contents(&directory.0);
        let mut reader = Storage::open(&directory.0, true).unwrap();
        assert_eq!(reader.row(&paused.id).unwrap(), Some(paused.clone()));
        drop(reader);
        assert_eq!(contents(&directory.0), before);
        let mut recovered = Storage::open(&directory.0, false).unwrap();
        recovered.initialize().unwrap();
        assert_eq!(recovered.row(&paused.id).unwrap(), Some(paused));
    }
}
#[test]
fn inventory_growth_report() {
    let Some(mut f) = crate::growth::Fixture::get("sources") else {
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
                let rows: Vec<_> = (start..(start + 200).min(size + 1)).map(record).collect();
                put_all(indexed(&mut storage), &rows);
            }
        });
    } else {
        assert_eq!(storage.total().unwrap(), size);
        f.measure("page_us", || {
            let after = None;
            let page = indexed(&mut storage)
                .page(&crate::inventory::Query {
                    start: single(PRIMARY),
                    end: Some(single(PRIMARY + 1)),
                    after,
                    limit: 200,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(page.entries.len(), 200);
        });
        f.measure("pass_us", || {
            storage.guard().unwrap();
            assert_eq!(
                storage
                    .backlog()
                    .get("state")
                    .and_then(crate::json::Value::as_str),
                Some("not_applicable")
            );
        });
        f.measure("update_us", || {
            let mut row = storage.row(&record(size).id).unwrap().unwrap();
            row.enabled = !row.enabled;
            storage.commit(row).unwrap();
        });
        let mut after = None;
        let mut count = 0;
        f.measure("walk_us", || {
            loop {
                let page = indexed(&mut storage)
                    .page(&crate::inventory::Query {
                        start: single(PRIMARY),
                        end: Some(single(PRIMARY + 1)),
                        after,
                        limit: 200,
                        ..Default::default()
                    })
                    .unwrap();
                count += page.entries.len() as u64;
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
