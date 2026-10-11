//! Admission attaches an immutable verified candidate in one index transaction.
use super::{
    Job, Request, Result,
    layout::{self, id_bytes},
    now,
    ops::{Db, key_part},
};
use crate::irc::Origin;

impl Db<'_> {
    /// Jobs with the same catalog identity as an announcement. Only requests
    /// with a catalog identifier can match.
    pub(super) fn irc_jobs(&mut self, request: &Request) -> Result<Vec<Job>> {
        if request.tmdb_id.is_none() {
            return Ok(Vec::new());
        }
        let prefix = layout::identity_prefix(
            &request.kind,
            request.tmdb_id,
            request.season,
            request.episode,
        );
        let mut jobs = Vec::new();
        for key in self.all_keys(&prefix)? {
            let job = self.require(&key_part(&key, 17)?)?;
            if job.request.kind == request.kind
                && job.request.tmdb_id == request.tmdb_id
                && job.request.season == request.season
                && job.request.episode == request.episode
            {
                jobs.push(job);
            }
        }
        Ok(jobs)
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
        let page = self.scan(&[layout::T_IRC], after, limit)?;
        let mut jobs: Vec<Job> = Vec::new();
        for (key, _) in &page.entries {
            let id = key_part::<16>(key, 33)?;
            if jobs
                .iter()
                .all(|job| id_bytes(&job.id).is_ok_and(|raw| raw != id))
            {
                jobs.push(self.require(&id)?);
            }
        }
        Ok((jobs, page.next))
    }
}
