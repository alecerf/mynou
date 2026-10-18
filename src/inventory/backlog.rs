//! Private scheduler indexes. Callers reserve tables 240..245 and update
//! membership in the same transaction as their authoritative records.
use super::{Change, Index, Key, MAX_PAGE_ROWS, Query};
use crate::{Result, json::Value};

const DUE: u8 = 240;
const MEMBER: u8 = 241;
const UNKNOWN: u8 = 245;
const FORMAT: Key = key(242);
const CURSOR: Key = key(243);
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
fn due(member: &Key, at: Option<u64>) -> Key {
    let mut key = key(if at.is_some() { DUE } else { UNKNOWN });
    key[1..9].copy_from_slice(&at.unwrap_or(0).to_be_bytes());
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

/// Preserve the age while work stays pending. Re-entry starts a new wait.
pub(crate) fn update(
    index: &mut Index,
    changes: &mut Vec<Change>,
    id: &Key,
    pending: bool,
    at: Option<u64>,
) -> Result<()> {
    let member = member(id);
    let previous = index
        .get(&member)?
        .map(|bytes| {
            if bytes.is_empty() {
                Ok(None)
            } else {
                number(bytes).map(Some)
            }
        })
        .transpose()?;
    match (previous, pending) {
        (None, true) => {
            changes.push(Change::PutInline(
                member,
                at.map_or_else(Vec::new, |at| at.to_be_bytes().to_vec()),
            ));
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

/// Legacy migrations must first verify their exact original tables. They
/// prepare derived membership only after promotion has been proved.
pub(crate) fn maintain(
    index: &mut Index,
    changes: &mut Vec<Change>,
    id: &Key,
    pending: bool,
    at: u64,
) -> Result<()> {
    match index.get(&FORMAT)?.as_deref() {
        Some(VERSION) => update(index, changes, id, pending, Some(at)),
        None => Ok(()),
        _ => Err("Unsupported backlog index format".into()),
    }
}

/// Explicit writable startup only. Pages and continuation share one WAL
/// transaction, including when a page has no pending work. No primary is changed.
pub(crate) fn prepare(
    index: &mut Index,
    start: Key,
    end: Key,
    mut classify: impl FnMut(&mut Index, &Key, &[u8]) -> Result<(bool, Option<u64>)>,
) -> Result<()> {
    let format = index.get(&FORMAT)?;
    if format.as_deref().is_some_and(|format| format != VERSION) {
        return Err("Unsupported backlog index format".into());
    }
    if format.is_none() {
        let mut after = index
            .get(&CURSOR)?
            .map(|bytes| {
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| "Invalid backlog upgrade cursor".to_owned())
            })
            .transpose()?;
        if after.is_some_and(|k| k < start || k >= end) {
            return Err("Backlog upgrade cursor outside its primary table".into());
        }
        loop {
            let page = index.page(&Query {
                start,
                end: Some(end),
                after,
                limit: MAX_PAGE_ROWS,
                ..Query::default()
            })?;
            if page.entries.is_empty() {
                break;
            }
            let mut changes = Vec::new();
            for (id, bytes) in page.entries {
                let (pending, at) = classify(index, &id, &bytes)?;
                update(index, &mut changes, &id, pending, at)?;
                after = Some(id);
            }
            changes.push(Change::Put(CURSOR, after.unwrap().to_vec()));
            index.commit(changes)?;
            if page.next.is_none() {
                break;
            }
        }
        index.commit(vec![
            Change::PutInline(FORMAT, VERSION.to_vec()),
            Change::Delete(CURSOR),
        ])?;
    }
    while !index.prepare_range_counts(64)? {}
    Ok(())
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
    let unknown = index
        .range_summary(key(UNKNOWN), Some(key(UNKNOWN + 1)))?
        .count;
    let oldest = if unknown == 0 {
        summary
            .first
            .map(|key| u64::from_be_bytes(key[1..9].try_into().unwrap()).min(at))
    } else {
        None
    };
    let mut value = report(
        unit,
        at,
        summary
            .count
            .checked_add(unknown)
            .ok_or("Backlog count overflow")?,
        oldest,
        last(index)?,
    );
    value.insert("unknown_due_times", unknown.to_string());
    Ok(value)
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
    use crate::inventory::{Fault, legacy_range_index};
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
    fn fixture() -> (Directory, Index) {
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        index
            .commit(
                (0..421)
                    .map(|n| Change::PutInline(id(n), vec![1]))
                    .collect(),
            )
            .unwrap();
        (directory, index)
    }
    fn prepare_fixture(index: &mut Index) {
        prepare(index, key(1), key(2), |_, id, _| {
            Ok((true, Some(u64::from_be_bytes(id[1..9].try_into().unwrap()))))
        })
        .unwrap();
    }
    #[test]
    fn backlog_preserves_unknown_legacy_ages_until_work_leaves_the_queue() {
        let (directory, mut index) = fixture();
        prepare(&mut index, key(1), key(2), |_, _, _| Ok((true, None))).unwrap();
        let value = read(&mut index, "fixture", 10).unwrap();
        assert_eq!(value.get("due"), Some(&Value::from("421")));
        assert_eq!(value.get("unknown_due_times"), Some(&Value::from("421")));
        assert_eq!(value.get("oldest_due_at"), Some(&Value::Null));
        let mut changes = Vec::new();
        for n in 0..421 {
            update(&mut index, &mut changes, &id(n), false, Some(20)).unwrap();
        }
        index.commit(changes).unwrap();
        let mut changes = Vec::new();
        update(&mut index, &mut changes, &id(0), true, Some(30)).unwrap();
        index.commit(changes).unwrap();
        drop(index);
        let mut index = Index::open(&directory.0, true).unwrap();
        let value = read(&mut index, "fixture", 40).unwrap();
        assert_eq!(value.get("due"), Some(&Value::from("1")));
        assert_eq!(value.get("unknown_due_times"), Some(&Value::from("0")));
        assert_eq!(value.get("oldest_due_age_secs"), Some(&Value::from("10")));
        let value = read(&mut index, "fixture", 20).unwrap();
        assert_eq!(value.get("due"), Some(&Value::from("1")));
        assert_eq!(value.get("oldest_due_age_secs"), Some(&Value::from("0")));
    }

    #[test]
    fn backlog_full_disk_does_not_publish_freshness_or_lose_legacy_rows() {
        let (directory, mut index) = fixture();
        assert!(
            crate::store::with_full_disk_after(0, || prepare(
                &mut index,
                key(1),
                key(2),
                |_, _, _| Ok((true, None))
            ))
            .is_err()
        );
        drop(index);
        let mut index = Index::open(&directory.0, false).unwrap();
        prepare_fixture(&mut index);
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
    fn backlog_counts_growth_removal_rollback_and_restart_without_payload_reads() {
        let (directory, mut index) = fixture();
        legacy_range_index(&mut index);
        assert!(read(&mut index, "fixture", 500).is_err());
        prepare_fixture(&mut index);
        let before = index.requests();
        let sequence = index.sequence().unwrap();
        let value = read(&mut index, "fixture", 500).unwrap();
        assert_eq!(
            index.requests() - before,
            4,
            "format, two range summaries, last-pass metadata only"
        );
        assert_eq!(value.get("due"), Some(&Value::from("421")));
        assert_eq!(value.get("oldest_due_at"), Some(&Value::from("0")));
        assert_eq!(index.sequence().unwrap(), sequence);
        assert_eq!(
            read(&mut index, "fixture", 10).unwrap().get("due"),
            Some(&Value::from("421"))
        );
        let mut changes = Vec::new();
        update(&mut index, &mut changes, &id(0), false, Some(500)).unwrap();
        update(&mut index, &mut changes, &id(900), true, Some(500)).unwrap();
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
        let value = read(&mut index, "fixture", u64::MAX).unwrap();
        assert_eq!(value.get("due"), Some(&Value::from("421")));
        assert_eq!(value.get("oldest_due_at"), Some(&Value::from("1")));
        assert_eq!(
            value.get("last_completed_pass_at"),
            Some(&Value::from("560"))
        );
    }
    #[test]
    fn backlog_upgrade_and_membership_recover_at_every_wal_boundary() {
        for fault in [
            Fault::Staged,
            Fault::Published,
            Fault::FirstPage,
            Fault::Applied,
            Fault::Unlinked,
        ] {
            let (directory, mut index) = fixture();
            index.inject(Some(fault));
            assert!(prepare(&mut index, key(1), key(2), |_, _, _| Ok((true, Some(10)))).is_err());
            drop(index);
            let mut index = Index::open(&directory.0, false).unwrap();
            prepare(&mut index, key(1), key(2), |_, _, _| Ok((true, Some(10)))).unwrap();
            assert_eq!(
                read(&mut index, "fixture", 20).unwrap().get("due"),
                Some(&Value::from("421"))
            );
            finish(&mut index, 20).unwrap();
            let mut changes = Vec::new();
            update(&mut index, &mut changes, &id(0), false, Some(30)).unwrap();
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
            assert_eq!(value.get("due"), Some(&Value::from(count)));
            assert_eq!(
                value.get("last_completed_pass_at"),
                Some(&Value::from(last))
            );
        }
    }
}
