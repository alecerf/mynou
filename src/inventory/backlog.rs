//! Private scheduler indexes. Callers reserve tables 240..245 and update
//! membership in the same transaction as their authoritative records.
use super::{Change, Index, Key, Query};
use crate::{Result, json::Value};

const DUE: u8 = 240;
const MEMBER: u8 = 241;
/// Held members of unknown age, which only removed upgrades wrote.
const RETIRED_UNKNOWN: u8 = 245;
const FORMAT: Key = key(242);
const LAST: Key = key(244);
const VERSION: &[u8] = b"mynou-backlog/1";
pub(crate) const CHECKPOINT_SECS: u64 = 60;

const fn key(table: u8) -> Key {
    let mut key = [0; 64];
    key[0] = table;
    key
}
fn member(id: &Key) -> Key {
    let mut key = key(MEMBER);
    key[1..33].copy_from_slice(&crate::crypto::sha256(id));
    key
}
fn due(member: &Key, at: u64) -> Key {
    let mut key = key(DUE);
    key[1..9].copy_from_slice(&at.to_be_bytes());
    key[9..41].copy_from_slice(&member[1..33]);
    key
}
fn number(bytes: Vec<u8>) -> Result<u64> {
    Ok(u64::from_be_bytes(
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| "Invalid backlog timestamp")?,
    ))
}

/// Keeps membership in the same transaction as the record it describes, and
/// preserves the age while work stays pending. Re-entry starts a new wait.
pub(crate) fn maintain(
    index: &mut Index,
    changes: &mut Vec<Change>,
    id: &Key,
    pending: bool,
    at: u64,
) -> Result<()> {
    match index.get(&FORMAT)?.as_deref() {
        Some(VERSION) => {}
        None => return Err("Backlog index is not prepared".into()),
        _ => return Err("Unsupported backlog index format".into()),
    }
    let member = member(id);
    let previous = index.get(&member)?.map(number).transpose()?;
    match (previous, pending) {
        (None, true) => {
            changes.push(Change::PutInline(member, at.to_be_bytes().to_vec()));
            changes.push(Change::PutInline(due(&member, at), Vec::new()));
        }
        (Some(at), false) => {
            changes.push(Change::Delete(member));
            changes.push(Change::Delete(due(&member, at)));
        }
        _ => {}
    }
    Ok(())
}

/// The marker did not change when upgrades went away, so an index one of
/// them prepared is told apart by the members of unknown age it left.
/// Read-only opens skip `prepare`, so their stores call this when opening.
pub(crate) fn current(index: &mut Index) -> Result<()> {
    let unknown = index.range_summary(key(RETIRED_UNKNOWN), Some(key(RETIRED_UNKNOWN + 1)))?;
    if unknown.count != 0 {
        return Err(
            "Backlog index is from an older Mynou that this version cannot read; \
             move the data folder aside to start over"
                .into(),
        );
    }
    Ok(())
}

/// Marks an index that holds no record of `start..end` yet as maintaining
/// its backlog; an index with records must already maintain it.
pub(crate) fn prepare(index: &mut Index, start: Key, end: Key) -> Result<()> {
    match index.get(&FORMAT)?.as_deref() {
        // The marker did not change when upgrades went away, so an index one
        // of them prepared is told apart by the members it left.
        Some(VERSION) => current(index),
        Some(_) => Err("Unsupported backlog index format".into()),
        None => {
            let page = index.page(&Query {
                start,
                end: Some(end),
                limit: 1,
                ..Query::default()
            })?;
            if !page.entries.is_empty() {
                return Err("Backlog index is missing from a store with records".into());
            }
            index.commit(vec![Change::PutInline(FORMAT, VERSION.to_vec())])?;
            Ok(())
        }
    }
}

/// Persist only successful bounded scheduler passes, at most once a minute.
/// Errors and shutdown must be checked by the caller before invoking this.
pub(crate) fn finish(index: &mut Index, at: u64) -> Result<()> {
    let last = last(index)?;
    if last.is_none_or(|last| at < last || at.saturating_sub(last) >= CHECKPOINT_SECS) {
        index.commit(vec![Change::PutInline(LAST, at.to_be_bytes().to_vec())])?;
    }
    Ok(())
}
pub(crate) fn last(index: &mut Index) -> Result<Option<u64>> {
    index.get(&LAST)?.map(number).transpose()
}
pub(crate) fn report(
    unit: &str,
    at: u64,
    count: u64,
    oldest: Option<u64>,
    last: Option<u64>,
) -> Value {
    let number = |v: Option<u64>| v.map_or(Value::Null, |v| v.to_string().into());
    let mut value = Value::object();
    value.insert("state", "ready");
    value.insert("unit", unit);
    value.insert("due", count.to_string());
    value.insert("oldest_due_at", number(oldest));
    value.insert(
        "oldest_due_age_secs",
        number(oldest.map(|time| at.saturating_sub(time))),
    );
    value.insert("last_completed_pass_at", number(last));
    value.insert("freshness_checkpoint_secs", CHECKPOINT_SECS.to_string());
    value
}
pub(crate) fn read(index: &mut Index, unit: &str, at: u64) -> Result<Value> {
    if index.get(&FORMAT)?.as_deref() != Some(VERSION) {
        return Err("Backlog requires writable preparation".into());
    }
    // These queues have no retry timer: every member is scheduled now,
    // including observations made before a wall-clock rollback.
    let summary = index.range_summary(key(DUE), Some(key(DUE + 1)))?;
    let oldest = summary
        .first
        .map(|key| u64::from_be_bytes(key[1..9].try_into().unwrap()).min(at));
    Ok(report(unit, at, summary.count, oldest, last(index)?))
}

pub(crate) fn unavailable() -> Value {
    let mut value = super::report("unavailable", None);
    value.insert(
        "error",
        "Backlog unavailable; restart with write access to prepare indexes or recover storage",
    );
    value
}
pub(crate) fn inactive(state: &str, unit: &str, last: Option<u64>) -> Value {
    let mut value = report(unit, 0, 0, None, last);
    value.insert("state", state);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Fault;
    use std::{fs, path::PathBuf};

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let suffix: String = crate::crypto::random_bytes::<8>()
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            Self(std::env::temp_dir().join(format!("mynou-backlog-{suffix}")))
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn id(n: u64) -> Key {
        let mut key = key(1);
        key[1..9].copy_from_slice(&n.to_be_bytes());
        key
    }
    /// 421 records, each pending since its number.
    fn fixture() -> (Directory, Index) {
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        prepare(&mut index, key(1), key(2)).unwrap();
        let mut changes = Vec::new();
        for n in 0..421 {
            changes.push(Change::PutInline(id(n), vec![1]));
            maintain(&mut index, &mut changes, &id(n), true, n).unwrap();
        }
        index.commit(changes).unwrap();
        (directory, index)
    }

    #[test]
    fn preparing_needs_an_empty_range_or_an_existing_backlog() {
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        index
            .commit(vec![Change::PutInline(id(0), vec![1])])
            .unwrap();
        let mut changes = Vec::new();
        assert!(maintain(&mut index, &mut changes, &id(0), true, 1).is_err());
        assert!(prepare(&mut index, key(1), key(2)).is_err());
        assert!(read(&mut index, "fixture", 1).is_err());
        let (_directory, mut index) = fixture();
        prepare(&mut index, key(1), key(2)).unwrap();
    }

    #[test]
    fn backlog_keeps_ages_until_work_leaves_the_queue() {
        let (directory, mut index) = fixture();
        let before = index.requests();
        let sequence = index.sequence().unwrap();
        let value = read(&mut index, "fixture", 500).unwrap();
        assert_eq!(
            index.requests() - before,
            3,
            "format, one range summary, last-pass metadata only"
        );
        assert_eq!(index.sequence().unwrap(), sequence);
        assert_eq!(value.get("due"), Some(&Value::from("421")));
        assert_eq!(value.get("oldest_due_at"), Some(&Value::from("0")));
        assert_eq!(value.get("oldest_due_age_secs"), Some(&Value::from("500")));
        let mut changes = Vec::new();
        for n in 0..420 {
            maintain(&mut index, &mut changes, &id(n), false, 600).unwrap();
        }
        // Still pending: the original age stays.
        maintain(&mut index, &mut changes, &id(420), true, 600).unwrap();
        index.commit(changes).unwrap();
        let mut changes = Vec::new();
        maintain(&mut index, &mut changes, &id(0), true, 700).unwrap();
        index.commit(changes).unwrap();
        finish(&mut index, 500).unwrap();
        let sequence = index.sequence().unwrap();
        finish(&mut index, 559).unwrap();
        assert_eq!(
            index.sequence().unwrap(),
            sequence,
            "idle passes do not flush on every tick"
        );
        finish(&mut index, 560).unwrap();
        drop(index);
        let mut index = Index::open(&directory.0, true).unwrap();
        let value = read(&mut index, "fixture", 800).unwrap();
        assert_eq!(value.get("due"), Some(&Value::from("2")));
        assert_eq!(value.get("oldest_due_at"), Some(&Value::from("420")));
        assert_eq!(
            value.get("last_completed_pass_at"),
            Some(&Value::from("560"))
        );
        // A wall-clock rollback never reports a negative age.
        let value = read(&mut index, "fixture", 100).unwrap();
        assert_eq!(value.get("oldest_due_at"), Some(&Value::from("100")));
        assert_eq!(value.get("oldest_due_age_secs"), Some(&Value::from("0")));
    }

    #[test]
    fn backlog_full_disk_does_not_publish_freshness() {
        let (directory, mut index) = fixture();
        finish(&mut index, 10).unwrap();
        assert!(crate::store::with_full_disk_after(0, || finish(&mut index, 80)).is_err());
        drop(index);
        let mut index = Index::open(&directory.0, false).unwrap();
        let value = read(&mut index, "fixture", 100).unwrap();
        assert_eq!(value.get("due"), Some(&Value::from("421")));
        assert_eq!(
            value.get("last_completed_pass_at"),
            Some(&Value::from("10"))
        );
        assert_eq!(index.get(&id(420)).unwrap(), Some(vec![1]));
    }

    #[test]
    fn backlog_membership_recovers_at_every_wal_boundary() {
        for fault in [
            Fault::Staged,
            Fault::Published,
            Fault::FirstPage,
            Fault::Applied,
            Fault::Unlinked,
        ] {
            let (directory, mut index) = fixture();
            finish(&mut index, 20).unwrap();
            let mut changes = Vec::new();
            maintain(&mut index, &mut changes, &id(0), false, 30).unwrap();
            changes.push(Change::PutInline(LAST, 80_u64.to_be_bytes().to_vec()));
            index.inject(Some(fault));
            assert!(index.commit(changes).is_err());
            drop(index);
            let mut index = Index::open(&directory.0, false).unwrap();
            let value = read(&mut index, "fixture", 100).unwrap();
            let (count, last) = if fault == Fault::Staged {
                ("421", "20")
            } else {
                ("420", "80")
            };
            assert_eq!(value.get("due"), Some(&Value::from(count)), "{fault:?}");
            assert_eq!(
                value.get("last_completed_pass_at"),
                Some(&Value::from(last))
            );
        }
    }
}
