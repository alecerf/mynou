//! Independent seed admission over the existing priority/FIFO catalog. No
//! payload or persistent format changes. Scan at most one listing page per
//! scheduling turn; publish a complete replacement set atomically.
use super::{AdmissionGate, BTreeSet, Job, Mutex, Result, index};
use std::sync::{Arc, RwLock, RwLockReadGuard};

pub(super) struct SeedQueue {
    limit: usize,
    selected: RwLock<BTreeSet<String>>,
}

#[derive(Default)]
pub(super) struct SeedScan {
    after: Option<String>,
    selected: BTreeSet<String>,
}

impl SeedQueue {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            selected: RwLock::new(BTreeSet::new()),
        }
    }

    pub(super) fn allowed(&self, id: &str) -> bool {
        self.limit == 0 || self.selected.read().is_ok_and(|set| set.contains(id))
    }

    pub(super) fn waiting(&self, seed: bool, job: &Job) -> bool {
        seed && (job.status.ready || job.needs_rehash) && !self.allowed(&job.status.id)
    }

    /// A replacement cannot admit a new seed while an old seed still writes a
    /// payload block. Callers must release this before acquiring the job table.
    pub(super) fn permit(&self, id: &str) -> Result<Option<RwLockReadGuard<'_, BTreeSet<String>>>> {
        if self.limit == 0 {
            return Ok(None);
        }
        let selected = self
            .selected
            .read()
            .map_err(|_| "Seed queue lock is poisoned")?;
        if !selected.contains(id) {
            return Err("Torrent is waiting in the seed queue".into());
        }
        Ok(Some(selected))
    }

    pub(super) fn advance(
        &self,
        index: &Arc<Mutex<index::TransferIndex>>,
        scan: &mut SeedScan,
        admission: Option<&AdmissionGate>,
    ) -> Result<()> {
        if self.limit == 0 {
            return Ok(());
        }
        let mut index = index
            .lock()
            .map_err(|_| "Native transfer index lock is poisoned")?;
        let (ids, next, _, _) = index.page(scan.after.as_deref(), 200)?;
        for id in ids {
            let record = index.get(&id)?.ok_or("Missing seed queue record")?;
            // Keep an admitted seed's slot while its persisted payload is
            // being reverified. This does not grant upload authority: serving
            // still requires live readiness and all existing proof fences.
            if (record.status.ready || self.allowed(&id))
                && !record.paused
                && !record.failed
                && !record.control.user_paused
                && !record.control.seed_limited
                // Retain prior admission through transient journal contention;
                // persistent pause/failure and upload proof checks still fence it.
                && (self.allowed(&id) || admission.is_none_or(|gate| gate(&id).unwrap_or(false)))
            {
                scan.selected.insert(id);
                if scan.selected.len() == self.limit {
                    break;
                }
            }
        }
        scan.after = next;
        if scan.selected.len() >= self.limit || scan.after.is_none() {
            *self
                .selected
                .write()
                .map_err(|_| "Seed queue lock is poisoned")? = std::mem::take(&mut scan.selected);
            scan.after = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_seeding_needs_no_admission_set() {
        let queue = SeedQueue::new(0);
        assert!(queue.allowed("any retained identity"));
        assert!(queue.permit("any retained identity").unwrap().is_none());
    }

    #[test]
    fn queued_seed_cannot_upload_and_replacement_waits_for_payload_write() {
        let queue = SeedQueue::new(1);
        queue.selected.write().unwrap().insert("first".into());
        assert!(queue.permit("second").is_err());
        let permit = queue.permit("first").unwrap();
        assert!(queue.selected.try_write().is_err());
        drop(permit);
        *queue.selected.write().unwrap() = BTreeSet::from(["second".into()]);
        assert!(queue.permit("first").is_err());
        assert!(queue.permit("second").is_ok());
    }
}
