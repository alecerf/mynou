//! Derived live-ownership index: transfers with an owner that is not
//! cancelled, cancelled downloads and live acquisition addresses. The upgrade
//! only writes secondary entries; each resumable page of the creation-ordered
//! job table and its cursor share one inventory transaction.
use super::{
    Result,
    layout::*,
    ops::{self, Db},
};
use crate::inventory::{Change, Index, MAX_PAGE_ROWS};

pub(super) const FORMAT: &[u8] = b"mynou-jobs/4";
pub(super) const PENDING: &[u8] = b"mynou-jobs/4-building";

/// Called only after the imported-child upgrade, which leaves `mynou-jobs/3`.
pub(super) fn upgrade(index: &mut Index, read_only: bool) -> Result<()> {
    let format = index.get(&FORMAT_KEY)?.ok_or("Missing job index format")?;
    if format == FORMAT {
        return Ok(());
    }
    if format != super::children::FORMAT && format != PENDING {
        return Err("Unsupported job ownership index format".into());
    }
    if read_only {
        return Err(
            "Transfer ownership index needs a writable restart to complete its upgrade".into(),
        );
    }
    if format != PENDING {
        index.commit(vec![
            Change::PutInline(FORMAT_KEY, PENDING.to_vec()),
            Change::Delete(OWNERSHIP_CURSOR_KEY),
        ])?;
    }
    let mut cursor = index
        .get(&OWNERSHIP_CURSOR_KEY)?
        .map(|bytes| ops::array::<64>(&bytes))
        .transpose()?;
    if let Some(cursor) = &cursor {
        let (start, end) = prefix_range(&[T_ALL]);
        if *cursor < start
            || end.is_some_and(|end| *cursor >= end)
            || cursor[25..].iter().any(|byte| *byte != 0)
        {
            return Err("Invalid transfer ownership upgrade cursor".into());
        }
    }
    loop {
        let page = Db(index).scan(&[T_ALL], cursor, MAX_PAGE_ROWS)?;
        if page.entries.is_empty() {
            break;
        }
        let mut changes = Vec::new();
        for (key, value) in &page.entries {
            if !value.is_empty() || key[25..].iter().any(|byte| *byte != 0) {
                return Err("Invalid job entry during transfer ownership upgrade".into());
            }
            let job = Db(index).require(&ops::key_part(key, 9)?)?;
            if job.created_at != !u64::from_be_bytes(ops::key_part(key, 1)?) {
                return Err("Transfer ownership upgrade found an inconsistent job entry".into());
            }
            for entry in ownership(&job)? {
                changes.push(Change::PutInline(entry, Vec::new()));
            }
            cursor = Some(*key);
        }
        changes.push(Change::Put(
            OWNERSHIP_CURSOR_KEY,
            cursor.ok_or("Missing ownership upgrade cursor")?.to_vec(),
        ));
        index.commit(changes)?;
        if page.next.is_none() {
            break;
        }
    }
    index.commit(vec![
        Change::PutInline(FORMAT_KEY, FORMAT.to_vec()),
        Change::Delete(OWNERSHIP_CURSOR_KEY),
    ])?;
    Ok(())
}
