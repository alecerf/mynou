//! Exact scheduled job backlog, independent of bounded wait-list promotion.
use super::{Result, Store, layout::*, ops};
use crate::{inventory::Change, json::Value};

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
    /// queue promotion or writes are permitted on this status path.
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
