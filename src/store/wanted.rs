//! Wanted requests in the job index: when each is searched next, in time
//! order (`T_WANTED`), and the one-time adoption of requests that failed for
//! good without a release before wanted titles existed. See `crate::wanted`.
use super::{Job, Result, Store, layout, now, ops};
use crate::inventory::{Change, MAX_PAGE_ROWS};

/// Entries one turn examines, past any a rolled-back version left behind.
const SCAN: usize = 16;

/// A request that failed for good on a search that found nothing to take or
/// reached no indexer, which Mynou now keeps wanted.
pub(crate) fn adoptable(job: &Job) -> bool {
    job.state == "failed"
        && job.next_attempt_at == 0
        && job.lease_id.is_none()
        && job
            .last_error
            .as_deref()
            .is_some_and(|error| crate::wanted::unfulfilled(error).is_some())
        && crate::wanted::searchable(job)
}

impl Store {
    /// Puts back in the queue the wanted request whose next search comes
    /// first, once that time is `at` or earlier, for a worker to search it.
    pub(crate) fn requeue_wanted(&mut self, at: u64) -> Result<Option<Job>> {
        self.mutate(|db| {
            let page = db.scan(&[layout::T_WANTED], None, SCAN.min(MAX_PAGE_ROWS))?;
            for (key, _) in &page.entries {
                let due = u64::from_be_bytes(ops::key_part(key, 1)?);
                if due > at {
                    return Ok(None);
                }
                let id: [u8; 16] = ops::key_part(key, 9)?;
                let Some(mut job) = db
                    .job(&id)?
                    .filter(|job| job.state == "wanted" && job.next_attempt_at == due)
                else {
                    // Not derived from any current record: drop it.
                    db.0.commit(vec![Change::Delete(*key)])?;
                    continue;
                };
                job.state = "queued".into();
                job.next_attempt_at = 0;
                job.updated_at = at;
                db.commit_jobs(vec![job.clone()], "wanted title searched again")?;
                return Ok(Some(job));
            }
            Ok(None)
        })
    }

    /// Makes one request wanted that failed for good without a release, if
    /// it still did; its last search was its last update.
    pub(crate) fn adopt_wanted(&mut self, id: &str, interval: u64) -> Result<bool> {
        self.mutate(|db| {
            let Some(mut job) = db.get(id)?.filter(adoptable) else {
                return Ok(false);
            };
            job.state = "wanted".into();
            job.searched_at = job.updated_at;
            job.next_attempt_at = job.updated_at.saturating_add(interval).max(1);
            job.updated_at = now();
            db.commit_jobs(vec![job], "no release yet: wanted")?;
            Ok(true)
        })
    }

    /// Whether the adoption already ran on this store.
    pub(crate) fn wanted_adopted(&mut self) -> Result<bool> {
        self.read(true, |db| {
            Ok(db.0.get(&layout::WANTED_ADOPTED_KEY)?.is_some())
        })
    }

    pub(crate) fn finish_wanted_adoption(&mut self) -> Result<()> {
        self.mutate(|db| {
            db.0.commit(vec![Change::PutInline(layout::WANTED_ADOPTED_KEY, vec![1])])
                .map(|_| ())
        })
    }
}
