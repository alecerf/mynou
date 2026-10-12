//! The invariants every job transaction must keep, enforced with indexed
//! lookups: lineage and upgrade rules, shared-file ownership, IRC and pack
//! ownership of physical videos, pending-upgrade exclusivity and the promotion
//! of a ready import. Each check reads the entries of the identities involved;
//! none enumerates the inventory.
use super::{
    Job, JobLookup, Result,
    layout::*,
    ops::Db,
};

/// Checks that need only to read other jobs by identifier, so the same rules
/// validate a legacy snapshot in memory and a transaction against the index.
pub(super) fn validate_lineage<L: JobLookup>(lookup: &mut L, job: &Job) -> Result<()> {
    if let Some(origin) = &job.irc_origin {
        origin.validate_job(job)?;
    }
    if let Some(release) = &job.release {
        release.validate()?;
    }
    let Some(parent_id) = &job.upgrade_parent else {
        return Ok(());
    };
    if job.shared_upgrade.is_some() {
        return super::groups::validate_group_lineage(lookup, job);
    }
    let parent = lookup
        .lookup(parent_id)?
        .ok_or_else(|| "missing upgrade parent".to_owned())?;
    if parent.requester != job.requester {
        return Err("Upgrade requester provenance is immutable".into());
    }
    if parent.shared_file.is_some() {
        return Err("Individual upgrades cannot replace shared-file owners".into());
    }
    if parent_id == &job.id
        || parent.request.media_key() != job.request.media_key()
        || parent.state != "ready"
        || parent.imports.is_empty()
        || parent.release.is_none()
        || job.release.is_none()
        || job.request.source_path.is_some()
        || job.request.source_url.is_none()
        || (job.state == "ready" && job.imports.is_empty())
    {
        return Err("invalid upgrade lineage or release baseline".to_owned());
    }
    let url = job.request.source_url.as_deref();
    if url == parent.request.source_url.as_deref() || url == parent.acquisition_url.as_deref() {
        return Err("upgrade acquisition repeats its parent's source URL".to_owned());
    }
    if job
        .acquisition_url
        .as_deref()
        .is_some_and(|acquisition| Some(acquisition) != url)
    {
        return Err("upgrade acquisition differs from its recorded source URL".to_owned());
    }
    Ok(())
}

impl Db<'_> {
    pub(super) fn upgrade_eligible(&mut self, job: &Job) -> Result<bool> {
        if job.shared_upgrade.is_some() {
            return Ok(self.group_eligible(job).is_ok());
        }
        let Some(parent_id) = &job.upgrade_parent else {
            return Ok(true);
        };
        let Some(parent) = self.get(parent_id)? else {
            return Ok(false);
        };
        Ok(parent.state == "ready"
            && !parent.imports.is_empty()
            && parent.release.is_some()
            && parent.monitored
            && self.is_tip(&parent.id)?)
    }

    pub(super) fn validate_lineage(&mut self, job: &Job) -> Result<()> {
        self.check_irc_ownership(job)?;
        validate_lineage(self, job)
    }

    pub(super) fn validate_transaction(&mut self, job: &Job) -> Result<()> {
        self.validate_lineage(job)?;
        let id = id_bytes(&job.id)?;
        let media = digest_bytes(&job.request.media_key())?;
        let owner = self.0.get(&shared_media_key(&media))?;
        if let Some(file) = &job.shared_file {
            file.validate_job(job)?;
            let physical = self
                .0
                .get(&physical_key(&file.torrent_id, &file.file_path))?;
            if physical.is_some_and(|known| known != file_digest(file))
                || owner.is_some_and(|owner| owner != id && job.shared_upgrade.is_none())
            {
                return Err("Shared-file physical identity or logical ownership conflicts".into());
            }
        } else if owner.is_some() {
            return Err("An unrelated request cannot replace shared-file ownership".into());
        }
        if let Some(current) = self.get(&job.id)? {
            self.validate_transition(&current, job)?;
        } else {
            if job.irc_origin.is_some() {
                return Err("IRC provenance requires a previously admitted job".into());
            }
            if self.exists(&canonical_key(&digest_bytes(&job.key)?))? {
                return Err("duplicate request identity".to_owned());
            }
            if job.upgrade_parent.is_some()
                && job.shared_upgrade.is_none()
                && !self.upgrade_eligible(job)?
            {
                return Err("an upgrade requires a monitored current library parent".to_owned());
            }
        }
        if pending_upgrade(job) {
            let media_key = job.request.media_key();
            if self.other_pending(&media_key, Some(&job.id))? {
                return Err("multiple pending upgrades for the same media".to_owned());
            }
            let was_pending = self.get(&job.id)?.is_some_and(|old| pending_upgrade(&old));
            if !was_pending && job.shared_upgrade.is_none() && !self.upgrade_eligible(job)? {
                return Err("the upgrade parent is obsolete or no longer monitored".to_owned());
            }
        }
        self.check_ready_promotion(job)
    }

    fn validate_transition(&mut self, current: &Job, job: &Job) -> Result<()> {
        if current.irc_origin != job.irc_origin
            && !(current.irc_origin.is_none()
                && job.irc_origin.is_some()
                && crate::irc::routing::eligible(current)
                && job.state == "queued"
                && job.lease_id.is_none()
                && job.files.is_empty()
                && job.imports.is_empty()
                && job.download_id.is_none())
        {
            return Err("IRC acquisition provenance is immutable".into());
        }
        if current.key != job.key
            || current.request != job.request
            || current.created_at != job.created_at
            || current.upgrade_parent != job.upgrade_parent
            || current.shared_file != job.shared_file
            || current.shared_upgrade != job.shared_upgrade
            || current.requester != job.requester
            || (current.shared_file.is_some() && current.pack_file != job.pack_file)
        {
            return Err("job identity or upgrade parent changed".to_owned());
        }
        if current.shared_file.is_some()
            && !current.imports.is_empty()
            && current.imports != job.imports
        {
            return Err("Shared import ownership is immutable".into());
        }
        let fresh_search = matches!(current.state.as_str(), "failed" | "cancelled")
            && job.state == "queued"
            && job.imports.is_empty()
            && job.request.source_path.is_none()
            && job.request.source_url.is_none()
            && job.release.is_none()
            && job.acquisition_url.is_none()
            && job.download_id.is_none()
            && job.files.is_empty()
            && job.progress == 0.0;
        if current.release.is_some() && current.release != job.release && !fresh_search {
            return Err("recorded release is immutable".to_owned());
        }
        if job.monitor_checked_at < current.monitor_checked_at {
            return Err("monitor check timestamp moved backwards".to_owned());
        }
        let ready_upgrade =
            current.state != "ready" && job.state == "ready" && current.upgrade_parent.is_some();
        if ready_upgrade {
            let parent = self
                .get(current.upgrade_parent.as_deref().unwrap_or_default())?
                .ok_or_else(|| "missing upgrade parent".to_owned())?;
            if job.monitored != parent.monitored {
                return Err(
                    "promoted upgrade monitoring differs from its current parent".to_owned(),
                );
            }
        } else if current.state != "ready" && current.monitored != job.monitored {
            return Err("monitoring can only change on a ready library import".to_owned());
        }
        if current.state == "ready" && job.state != "ready" {
            return Err("a ready import cannot change state".to_owned());
        }
        if current.state == "ready"
            && (current.files != job.files
                || current.imports != job.imports
                || current.acquisition_url != job.acquisition_url
                || current.download_id != job.download_id
                || current.progress != job.progress)
        {
            return Err("ready import provenance is immutable".to_owned());
        }
        Ok(())
    }

    /// Checks whether a completed acquisition can become the current import.
    pub(super) fn check_ready_promotion(&mut self, job: &Job) -> Result<()> {
        if job.state != "ready"
            || self
                .get(&job.id)?
                .is_some_and(|current| current.state == "ready")
        {
            return Ok(());
        }
        if job.shared_upgrade.is_some() {
            return self.group_eligible(job);
        }
        let media_key = job.request.media_key();
        if let Some(parent_id) = &job.upgrade_parent {
            let parent = self
                .get(parent_id)?
                .ok_or_else(|| "missing upgrade parent".to_owned())?;
            if !self.is_tip(&parent.id)? || self.other_pending(&media_key, Some(&job.id))? {
                return Err(
                    "an upgrade cannot replace an obsolete parent or a competing pending upgrade"
                        .to_owned(),
                );
            }
        } else if !job.imports.is_empty() && self.other_pending(&media_key, Some(&job.id))? {
            return Err(
                "cancel the pending upgrade before importing an unrelated root for the same media"
                    .to_owned(),
            );
        }
        Ok(())
    }

    /// Physical-video ownership by an IRC acquisition, a pack mapping or a
    /// shared file, through the ownership entries of the file and its aliases.
    pub(super) fn check_irc_ownership(&mut self, job: &Job) -> Result<()> {
        let Some(origin) = &job.irc_origin else {
            if let Some(file) = &job.pack_file {
                for transfer in pack_transfers(job) {
                    if self.find_job(&irc_prefix(file, transfer), 33, |other| other.id != job.id
                        && other.irc_origin.as_ref().is_some_and(|known| &known.file == file
                            && known.torrent_aliases.iter().any(|alias| alias == transfer)))?.is_some()
                    { return Err("IRC: pack mapping conflicts with retained announcement ownership".into()); }
                }
            }
            return Ok(());
        };
        for alias in &origin.torrent_aliases {
            if self.find_job(&irc_prefix(&origin.file, alias), 33, |other| other.id != job.id
                && other.irc_origin.as_ref().is_some_and(|known| known.file == origin.file
                    && known.torrent_aliases.iter().any(|other| origin.torrent_aliases.contains(other))))?.is_some()
            { return Err("IRC: physical video already belongs to another acquisition".into()); }
            if self.find_job(&packref_prefix(&origin.file, "t", alias), 33, |other| other.id != job.id
                && other.pack_file.as_ref() == Some(&origin.file)
                && pack_transfers(other).iter().any(|transfer| origin.torrent_aliases.iter().any(|a| a == transfer)))?.is_some()
            { return Err("IRC: candidate conflicts with retained pack ownership".into()); }
        }
        Ok(())
    }

    /// Group members whose record still carries the binding that keyed them.
    pub(super) fn group_jobs_of(&mut self, id: &str) -> Result<Vec<Job>> {
        let owner = self.get(id)?.ok_or("Unknown shared-group owner")?;
        let binding = owner
            .shared_file
            .as_ref()
            .ok_or("Library entry has no shared ownership")?;
        let jobs = self.group_members(binding)?;
        if jobs.len() != (binding.last_episode - binding.first_episode + 1) as usize
            || jobs
                .iter()
                .map(|job| job.request.episode)
                .ne(binding.first_episode..=binding.last_episode)
        {
            return Err("Shared group is incomplete".into());
        }
        Ok(jobs)
    }
}
