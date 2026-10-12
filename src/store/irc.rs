//! Admission attaches an immutable verified candidate in one index transaction.
use super::{
    Job, Request, Result, layout, now,
    ops::{Db, key_part},
};
use crate::irc::Origin;

#[derive(Default)]
pub(crate) struct Candidates {
    pub job: Option<Job>,
    pub count: u8,
    pub available: bool,
    pub blocked: bool,
}

impl Db<'_> {
    pub(super) fn irc_candidates(&mut self, request: &Request) -> Result<Candidates> {
        let mut found = Candidates::default();
        if request.tmdb_id.is_none() {
            return Ok(found);
        }
        let prefix = layout::identity_prefix(
            &request.kind,
            request.tmdb_id,
            request.season,
            request.episode,
        );
        let mut after = None;
        loop {
            let page = self.scan(&prefix, after, crate::inventory::MAX_PAGE_ROWS)?;
            for (key, _) in &page.entries {
                let job = self.require(&key_part(key, 17)?)?;
                if job.request.kind != request.kind
                    || job.request.tmdb_id != request.tmdb_id
                    || job.request.season != request.season
                    || job.request.episode != request.episode
                {
                    continue;
                }
                if crate::irc::routing::eligible(&job) {
                    found.count = found.count.saturating_add(1);
                    if found.job.is_none() {
                        found.job = Some(job.clone());
                    }
                }
                found.available |= job.state == "ready"
                    && job.request.source_url.is_none()
                    && job.request.source_path.is_none();
                found.blocked |= job.state == "ready"
                    || job.shared_file.is_some()
                    || job.pack_file.is_some()
                    || job.upgrade_parent.is_some();
            }
            after = page.next;
            if after.is_none() {
                return Ok(found);
            }
        }
    }

    pub(super) fn check_irc_origin(&mut self, origin: &Origin) -> Result<()> {
        origin.validate()?;
        let job = self
            .get(&origin.job_id)?
            .ok_or("IRC: admitted job is missing")?;
        if !crate::irc::routing::eligible(&job) {
            return Err("IRC: admitted job is no longer eligible".into());
        }
        let mut routed = job;
        routed.acquisition_url = Some(origin.magnet.clone());
        routed.release = Some(origin.release.clone());
        routed.irc_origin = Some(origin.clone());
        origin.validate_job(&routed)?;
        self.validate_transaction(&routed)
    }

    pub(super) fn attach_irc_origin(&mut self, origin: Origin) -> Result<Job> {
        self.check_irc_origin(&origin)?;
        let mut job = self
            .get(&origin.job_id)?
            .ok_or("IRC: admitted job is missing")?;
        job.acquisition_url = Some(origin.magnet.clone());
        job.release = Some(origin.release.clone());
        job.irc_origin = Some(origin);
        job.next_attempt_at = 0;
        job.updated_at = now();
        self.commit_jobs(vec![job.clone()], "verified IRC candidate routed")?;
        Ok(job)
    }

    /// One page of the jobs that carry an IRC acquisition origin, in key order.
    pub(super) fn irc_origin_page(
        &mut self,
        after: Option<crate::inventory::Key>,
        limit: usize,
    ) -> Result<(Vec<Job>, Option<crate::inventory::Key>)> {
        let page = self.scan(&[layout::T_IRC_ORIGIN], after, limit)?;
        let mut jobs = Vec::new();
        for (key, _) in &page.entries {
            let job = self.require(&key_part(key, 1)?)?;
            if job.irc_origin.is_none() {
                return Err("IRC origin index disagrees with its record".into());
            }
            jobs.push(job);
        }
        Ok((jobs, page.next))
    }
}
