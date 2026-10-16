//! Derived imported-child index. The upgrade only writes secondary entries;
//! each resumable page and its cursor share one inventory transaction.
use super::{
    Result,
    layout::*,
    ops::{self, Db},
};
use crate::inventory::{Change, Index, MAX_PAGE_ROWS};

pub(super) const FORMAT: &[u8] = b"mynou-jobs/3";
pub(super) const PENDING: &[u8] = b"mynou-jobs/3-building";

/// Called only after the monitoring upgrade, which leaves `mynou-jobs/2`.
pub(super) fn upgrade(index: &mut Index, read_only: bool) -> Result<()> {
    let format = index.get(&FORMAT_KEY)?.ok_or("Missing job index format")?;
    if format == FORMAT || format == super::ownership::FORMAT || format == super::ownership::PENDING
    {
        return Ok(());
    }
    if format != super::monitoring::FORMAT && format != PENDING {
        return Err("Unsupported job child index format".into());
    }
    if read_only {
        return Err("Imported-child index needs a writable restart to complete its upgrade".into());
    }
    if format != PENDING {
        index.commit(vec![
            Change::PutInline(FORMAT_KEY, PENDING.to_vec()),
            Change::Delete(CHILD_CURSOR_KEY),
        ])?;
    }
    let mut cursor = index
        .get(&CHILD_CURSOR_KEY)?
        .map(|bytes| ops::array::<64>(&bytes))
        .transpose()?;
    if let Some(cursor) = &cursor {
        let (start, end) = prefix_range(&[T_CHILD]);
        if *cursor < start
            || end.is_some_and(|end| *cursor >= end)
            || cursor[33..].iter().any(|byte| *byte != 0)
        {
            return Err("Invalid imported-child upgrade cursor".into());
        }
    }
    loop {
        let page = Db(index).scan(&[T_CHILD], cursor, MAX_PAGE_ROWS)?;
        if page.entries.is_empty() {
            break;
        }
        let mut changes = Vec::new();
        for (key, value) in &page.entries {
            let parent = ops::key_part::<16>(key, 1)?;
            let child = ops::key_part::<16>(key, 17)?;
            match value.as_slice() {
                [1] => changes.push(Change::PutInline(
                    imported_child_key(&parent, &child),
                    Vec::new(),
                )),
                [0] => {}
                _ => return Err("Invalid child entry during child index upgrade".into()),
            }
            cursor = Some(*key);
        }
        changes.push(Change::Put(
            CHILD_CURSOR_KEY,
            cursor.ok_or("Missing child upgrade cursor")?.to_vec(),
        ));
        index.commit(changes)?;
        if page.next.is_none() {
            break;
        }
    }
    index.commit(vec![
        Change::PutInline(FORMAT_KEY, FORMAT.to_vec()),
        Change::Delete(CHILD_CURSOR_KEY),
    ])?;
    Ok(())
}
