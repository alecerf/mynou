//! Admission attaches an immutable verified candidate in one journal transaction.
use super::{Job, Request, Store, now};
use crate::{Result, irc::Origin};

impl Store {
    pub(crate) fn irc_jobs(&self, request: &Request) -> Vec<Job> {
        self.jobs
            .values()
            .filter(|j| {
                j.request.kind == request.kind
                    && j.request.tmdb_id == request.tmdb_id
                    && j.request.season == request.season
                    && j.request.episode == request.episode
            })
            .cloned()
            .collect()
    }
    pub(crate) fn check_irc_origin(&self, origin: &Origin) -> Result<()> {
        origin.validate()?;
        let job = self
            .get(&origin.job_id)
            .ok_or("IRC: admitted job is missing")?;
        if !crate::irc::routing::eligible(&job) {
            return Err("IRC: admitted job is no longer eligible".into());
        }
        let mut routed = job;
        routed.acquisition_url = Some(origin.magnet.clone());
        routed.release = Some(origin.release.clone());
        routed.irc_origin = Some(origin.clone());
        origin.validate_job(&routed)?;
        self.validate_transaction(&routed, false)
    }
    pub(crate) fn attach_irc_origin(&mut self, origin: Origin) -> Result<Job> {
        self.check_irc_origin(&origin)?;
        let mut job = self
            .get(&origin.job_id)
            .ok_or("IRC: admitted job is missing")?;
        job.acquisition_url = Some(origin.magnet.clone());
        job.release = Some(origin.release.clone());
        job.irc_origin = Some(origin);
        job.updated_at = now();
        self.commit(job.clone(), "verified IRC candidate routed")?;
        Ok(job)
    }
    pub(super) fn check_irc_ownership(&self, job: &Job) -> Result<()> {
        if job.irc_origin.is_none() && job.pack_file.is_none() {
            return Ok(());
        }
        for other in self.jobs.values().filter(|j| j.id != job.id) {
            if job.irc_origin.is_none() {
                if let Some(known) = &other.irc_origin
                    && job.pack_file.as_ref() == Some(&known.file)
                    && (job
                        .pack_origin
                        .as_ref()
                        .is_some_and(|p| known.torrent_aliases.contains(&p.torrent_id))
                        || job
                            .shared_file
                            .as_ref()
                            .is_some_and(|f| known.torrent_aliases.contains(&f.torrent_id))
                        || job
                            .download_id
                            .as_ref()
                            .is_some_and(|id| known.torrent_aliases.contains(id)))
                {
                    return Err(
                        "IRC: pack mapping conflicts with retained announcement ownership".into(),
                    );
                }
                continue;
            }
            let origin = job
                .irc_origin
                .as_ref()
                .ok_or("IRC: missing ownership origin")?;
            if let Some(known) = &other.irc_origin
                && known.file == origin.file
                && known
                    .torrent_aliases
                    .iter()
                    .any(|id| origin.torrent_aliases.contains(id))
            {
                return Err("IRC: physical video already belongs to another acquisition".into());
            }
            if other.pack_file.as_ref() == Some(&origin.file)
                && (other
                    .pack_origin
                    .as_ref()
                    .is_some_and(|p| origin.torrent_aliases.contains(&p.torrent_id))
                    || other
                        .shared_file
                        .as_ref()
                        .is_some_and(|f| origin.torrent_aliases.contains(&f.torrent_id))
                    || other
                        .download_id
                        .as_ref()
                        .is_some_and(|id| origin.torrent_aliases.contains(id)))
            {
                return Err("IRC: candidate conflicts with retained pack ownership".into());
            }
        }
        Ok(())
    }
}
