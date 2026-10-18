//! Exact scheduled job backlog, independent of bounded wait-list promotion.
use super::{
    Result, Store,
    layout::*,
    ops::{self, Db},
};
use crate::{
    inventory::{Change, Index, MAX_PAGE_ROWS},
    json::Value,
};

pub(super) const FORMAT: &[u8] = b"mynou-jobs/5";
pub(super) const PENDING: &[u8] = b"mynou-jobs/5-building";

/// The cursor and each derived page commit together. Existing primary records,
/// history, leases and legacy backups are never rewritten by this upgrade.
pub(super) fn upgrade(index: &mut Index, read_only: bool) -> Result<()> {
    let format = index.get(&FORMAT_KEY)?.ok_or("Missing job index format")?;
    if format == FORMAT {
        return Ok(());
    }
    if format != super::ownership::FORMAT && format != PENDING {
        return Err("Unsupported job backlog index format".into());
    }
    if read_only {
        return Err("Job backlog index needs a writable restart to complete its upgrade".into());
    }
    if format != PENDING {
        index.commit(vec![
            Change::PutInline(FORMAT_KEY, PENDING.to_vec()),
            Change::Delete(JOB_BACKLOG_CURSOR_KEY),
        ])?;
    }
    let mut cursor = index
        .get(&JOB_BACKLOG_CURSOR_KEY)?
        .map(|bytes| ops::array::<64>(&bytes))
        .transpose()?;
    if let Some(cursor) = &cursor {
        let (start, end) = prefix_range(&[T_ALL]);
        if *cursor < start
            || end.is_some_and(|end| *cursor >= end)
            || cursor[25..].iter().any(|byte| *byte != 0)
        {
            return Err("Invalid job backlog upgrade cursor".into());
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
                return Err("Invalid job entry during backlog upgrade".into());
            }
            let job = Db(index).require(&ops::key_part(key, 9)?)?;
            if job.created_at != !u64::from_be_bytes(ops::key_part(key, 1)?) {
                return Err("Job backlog upgrade found an inconsistent job entry".into());
            }
            if open(&job) {
                changes.push(Change::PutInline(job_due_key(&job)?, Vec::new()));
            }
            cursor = Some(*key);
        }
        changes.push(Change::Put(
            JOB_BACKLOG_CURSOR_KEY,
            cursor.ok_or("Missing job backlog upgrade cursor")?.to_vec(),
        ));
        index.commit(changes)?;
        if page.next.is_none() {
            break;
        }
    }
    index.commit(vec![
        Change::PutInline(FORMAT_KEY, FORMAT.to_vec()),
        Change::Delete(JOB_BACKLOG_CURSOR_KEY),
    ])?;
    Ok(())
}

pub(super) fn completed(at: u64) -> Change {
    Change::PutInline(JOB_LAST_PASS_KEY, at.to_be_bytes().to_vec())
}

fn report(at: u64, due: u64, oldest: Option<u64>, last: Option<u64>) -> Value {
    let mut value = Value::object();
    let number = |n: Option<u64>| n.map_or(Value::Null, |n| n.to_string().into());
    value.insert("state", "ready");
    value.insert("due", due.to_string());
    value.insert("oldest_due_at", number(oldest));
    value.insert(
        "oldest_due_age_secs",
        number(oldest.map(|time| at.saturating_sub(time))),
    );
    value.insert("last_completed_pass_at", number(last));
    value.insert("unit", "job_claim_scan");
    value.insert(
        "freshness_checkpoint_secs",
        super::CLAIM_CHECKPOINT_SECS.to_string(),
    );
    value
}

impl Store {
    /// One exact time range and two metadata lookups. No job/history payloads,
    /// queue promotion, migration or writes are permitted on this status path.
    pub(crate) fn job_backlog(&mut self, at: u64) -> Result<Value> {
        self.read(report(at, 0, None, None), |db| {
            if db.0.get(&FORMAT_KEY)?.as_deref() != Some(FORMAT) {
                return Err("Job backlog index requires writable preparation".into());
            }
            let end = at.checked_add(1).map_or_else(
                || prefix_range(&[T_JOB_DUE]).1,
                |next| Some(key(&[&[T_JOB_DUE], &next.to_be_bytes()])),
            );
            let summary = db.0.range_summary(key(&[&[T_JOB_DUE]]), end)?;
            let oldest = summary
                .first
                .map(|first| ops::key_part(&first, 1).map(u64::from_be_bytes))
                .transpose()?;
            let last =
                db.0.get(&JOB_LAST_PASS_KEY)?
                    .map(|bytes| ops::array(&bytes).map(u64::from_be_bytes))
                    .transpose()?;
            Ok(report(at, summary.count, oldest, last))
        })
    }
}
