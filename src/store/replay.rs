//! Read-only compatibility rules retained from the former journal store.
//! Migration validates each complete transaction before changing legacy state.
use super::groups::GroupAction;
use super::{Job, Result};
use crate::pack::{MAX_PACK_EPISODES, SharedFile};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Replay {
    pub jobs: BTreeMap<String, Job>,
    by_key: BTreeMap<String, String>,
    shared_by_physical: BTreeMap<(String, String), SharedFile>,
    shared_by_media: BTreeMap<String, String>,
}
impl Replay {
    pub fn new(jobs: BTreeMap<String, Job>) -> Self {
        let mut replay = Self {
            jobs: BTreeMap::new(),
            by_key: BTreeMap::new(),
            shared_by_physical: BTreeMap::new(),
            shared_by_media: BTreeMap::new(),
        };
        for job in jobs.into_values() {
            replay.insert(job);
        }
        replay
    }
    pub fn insert(&mut self, job: Job) {
        self.index_shared(&job);
        self.by_key.insert(job.key.clone(), job.id.clone());
        self.jobs.insert(job.id.clone(), job);
    }
    pub fn validate_record(
        &self,
        jobs: &[Job],
        action: Option<GroupAction>,
        shared: bool,
        legacy: bool,
    ) -> Result<()> {
        match action {
            Some(action) => self.validate_group_operation(jobs, action),
            None if shared => self.validate_shared_batch(jobs),
            None => {
                let job = &jobs[0];
                if job.shared_file.is_some() && !self.jobs.contains_key(&job.id) {
                    return Err("Shared owners must be created in one transaction".into());
                }
                self.validate_transaction(job, legacy)?;
                self.validate_group_single(job)
            }
        }
    }
    fn index_shared(&mut self, job: &Job) {
        if let Some(file) = &job.shared_file {
            if job.shared_upgrade.is_none() {
                self.shared_by_media
                    .insert(job.request.media_key(), job.id.clone());
            }
            self.shared_by_physical
                .entry((file.torrent_id.clone(), file.file_path.clone()))
                .or_insert_with(|| file.clone());
        }
    }

    pub fn library_jobs(&self) -> Vec<Job> {
        let replaced: BTreeSet<&str> = self
            .jobs
            .values()
            .filter(|job| job.state == "ready" && !job.imports.is_empty())
            .filter_map(|job| job.upgrade_parent.as_deref())
            .collect();
        let Ok(roots) = self.lineage_roots() else {
            return Vec::new();
        };
        let mut current: BTreeMap<String, (&Job, &Job)> = BTreeMap::new();
        for job in self.jobs.values().filter(|job| {
            job.state == "ready" && !job.imports.is_empty() && !replaced.contains(job.id.as_str())
        }) {
            let key = job.request.media_key();
            let root = roots.get(job.id.as_str()).copied().unwrap_or(job);
            match current.get(&key) {
                Some((previous, _))
                    if (previous.created_at, &previous.id) >= (root.created_at, &root.id) => {}
                _ => {
                    current.insert(key, (root, job));
                }
            }
        }
        let mut jobs: Vec<_> = current.into_values().map(|(_, job)| job.clone()).collect();
        jobs.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        jobs
    }

    fn is_library_tip(&self, job: &Job) -> bool {
        self.library_jobs().iter().any(|tip| tip.id == job.id)
    }

    fn pending_upgrade(job: &Job) -> bool {
        job.upgrade_parent.is_some()
            && !matches!(job.state.as_str(), "ready" | "cancelled")
            && (job.shared_upgrade.is_some() || job.state != "failed" || job.next_attempt_at > 0)
    }

    fn upgrade_eligible(&self, job: &Job) -> bool {
        if job.shared_upgrade.is_some() {
            return self.group_eligible(job).is_ok();
        }
        let Some(parent_id) = &job.upgrade_parent else {
            return true;
        };
        self.jobs.get(parent_id).is_some_and(|parent| {
            parent.state == "ready"
                && !parent.imports.is_empty()
                && parent.release.is_some()
                && parent.monitored
                && self.is_library_tip(parent)
        })
    }

    fn validate_shared_batch(&self, jobs: &[Job]) -> Result<()> {
        let first = jobs.first().ok_or("Empty shared ownership transaction")?;
        let file = first
            .shared_file
            .as_ref()
            .ok_or("Ownership transaction requires a shared binding")?;
        let mut numbers = BTreeSet::new();
        let mut keys = BTreeSet::new();
        let mut ids = BTreeSet::new();
        if !(2..=crate::pack::MAX_PACK_EPISODES).contains(&jobs.len()) {
            return Err("Invalid shared ownership transaction size".into());
        }
        for job in jobs {
            if job.shared_file.as_ref() != Some(file)
                || self.jobs.contains_key(&job.id)
                || job.state != "queued"
                || job.lease_id.is_some()
                || job.lease_until != 0
                || job.progress != 0.0
                || job.attempts != 0
                || !job.files.is_empty()
                || !job.imports.is_empty()
                || job.download_id.is_some()
                || job.acquisition_url.is_some()
                || job.release.is_some()
                || job.shared_upgrade.is_some()
                || job.last_error.is_some()
                || job.next_attempt_at != 0
                || !job.monitored
                || job.monitor_checked_at != 0
                || job.created_at != first.created_at
                || job.updated_at != first.updated_at
                || job.request.source_url != first.request.source_url
                || !numbers.insert(job.request.episode)
                || !keys.insert(&job.key)
                || !ids.insert(&job.id)
            {
                return Err("Inconsistent new shared ownership transaction".into());
            }
            self.validate_transaction(job, false)?;
        }
        if numbers
            .into_iter()
            .ne(file.first_episode..=file.last_episode)
        {
            return Err("Incomplete shared ownership transaction".into());
        }
        Ok(())
    }

    pub fn check_ready_promotion(&self, job: &Job) -> Result<()> {
        if job.state != "ready"
            || self
                .jobs
                .get(&job.id)
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
                .jobs
                .get(parent_id)
                .ok_or_else(|| "missing upgrade parent".to_owned())?;
            if !self.is_library_tip(parent)
                || self.jobs.values().any(|other| {
                    other.id != job.id
                        && Self::pending_upgrade(other)
                        && other.request.media_key() == media_key
                })
            {
                return Err(
                    "an upgrade cannot replace an obsolete parent or a competing pending upgrade"
                        .to_owned(),
                );
            }
        } else if !job.imports.is_empty()
            && self
                .jobs
                .values()
                .any(|other| Self::pending_upgrade(other) && other.request.media_key() == media_key)
        {
            return Err(
                "cancel the pending upgrade before importing an unrelated root for the same media"
                    .to_owned(),
            );
        }
        Ok(())
    }

    fn validate_lineage(&self, job: &Job) -> Result<()> {
        self.check_irc_ownership(job)?;
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
            return self.validate_group_lineage(job);
        }
        let parent = self
            .jobs
            .get(parent_id)
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

    fn validate_transaction(&self, job: &Job, legacy_record: bool) -> Result<()> {
        self.validate_lineage(job)?;
        if let Some(file) = &job.shared_file {
            file.validate_job(job)?;
            if self
                .shared_by_physical
                .get(&(file.torrent_id.clone(), file.file_path.clone()))
                .is_some_and(|known| known != file)
                || self
                    .shared_by_media
                    .get(&job.request.media_key())
                    .is_some_and(|owner| owner != &job.id && job.shared_upgrade.is_none())
            {
                return Err("Shared-file physical identity or logical ownership conflicts".into());
            }
        } else if !self.shared_by_media.is_empty()
            && self.shared_by_media.contains_key(&job.request.media_key())
        {
            return Err("An unrelated request cannot replace shared-file ownership".into());
        }
        if let Some(current) = self.jobs.get(&job.id) {
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
            let ready_upgrade = current.state != "ready"
                && job.state == "ready"
                && current.upgrade_parent.is_some();
            if ready_upgrade {
                let parent = self
                    .jobs
                    .get(current.upgrade_parent.as_deref().unwrap_or_default())
                    .ok_or_else(|| "missing upgrade parent".to_owned())?;
                if job.monitored != parent.monitored {
                    return Err(
                        "promoted upgrade monitoring differs from its current parent".to_owned(),
                    );
                }
            } else if current.state != "ready" && current.monitored != job.monitored {
                return Err("monitoring can only change on a ready library import".to_owned());
            }
            let legacy_cancellation = legacy_record
                && job.state == "cancelled"
                && current.release.is_none()
                && current.upgrade_parent.is_none()
                && job.release.is_none()
                && job.upgrade_parent.is_none()
                && current.monitor_checked_at == 0
                && current.monitored
                && !self
                    .jobs
                    .values()
                    .any(|other| other.upgrade_parent.as_deref() == Some(current.id.as_str()));
            if current.state == "ready" && job.state != "ready" && !legacy_cancellation {
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
        } else {
            if job.irc_origin.is_some() {
                return Err("IRC provenance requires a previously admitted job".into());
            }
            if self.by_key.contains_key(&job.key) {
                return Err("duplicate request identity".to_owned());
            }
            if job.upgrade_parent.is_some()
                && job.shared_upgrade.is_none()
                && !self.upgrade_eligible(job)
            {
                return Err("an upgrade requires a monitored current library parent".to_owned());
            }
        }
        if Self::pending_upgrade(job) {
            let media_key = job.request.media_key();
            if self.jobs.values().any(|other| {
                other.id != job.id
                    && Self::pending_upgrade(other)
                    && other.request.media_key() == media_key
            }) {
                return Err("multiple pending upgrades for the same media".to_owned());
            }
            let was_pending = self.jobs.get(&job.id).is_some_and(Self::pending_upgrade);
            if !was_pending && job.shared_upgrade.is_none() && !self.upgrade_eligible(job) {
                return Err("the upgrade parent is obsolete or no longer monitored".to_owned());
            }
        }
        self.check_ready_promotion(job)
    }

    pub fn shared_group(&self, id: &str) -> Result<Vec<Job>> {
        let owner = self.jobs.get(id).ok_or("Unknown shared-group owner")?;
        let binding = owner
            .shared_file
            .as_ref()
            .ok_or("Library entry has no shared ownership")?;
        let mut jobs: Vec<_> = self
            .jobs
            .values()
            .filter(|job| job.shared_file.as_ref() == Some(binding))
            .cloned()
            .collect();
        jobs.sort_by_key(|job| job.request.episode);
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

    pub(crate) fn current_shared_group(&self, id: &str) -> Result<Vec<Job>> {
        let jobs = self.shared_group(id)?;
        let tips: BTreeSet<_> = self.library_jobs().into_iter().map(|job| job.id).collect();
        if jobs
            .iter()
            .any(|job| job.state != "ready" || job.imports.is_empty() || !tips.contains(&job.id))
        {
            return Err("Operation requires the complete current shared library group".into());
        }
        Ok(jobs)
    }

    pub(super) fn lineage_roots(&self) -> Result<BTreeMap<&str, &Job>> {
        let mut roots = BTreeMap::new();
        for job in self.jobs.values() {
            let mut path = Vec::new();
            let mut seen = BTreeSet::new();
            let mut cursor = job;
            let root = loop {
                if let Some(root) = roots.get(cursor.id.as_str()) {
                    break *root;
                }
                if !seen.insert(cursor.id.as_str()) {
                    return Err("Cycle in upgrade lineage".into());
                }
                path.push(cursor.id.as_str());
                match cursor.upgrade_parent.as_deref() {
                    Some(id) => {
                        cursor = self
                            .jobs
                            .get(id)
                            .ok_or("Missing upgrade lineage reference")?
                    }
                    None => break cursor,
                }
            };
            for id in path {
                roots.insert(id, root);
            }
        }
        Ok(roots)
    }

    pub(super) fn validate_group_lineage(&self, job: &Job) -> Result<()> {
        let lineage = job
            .shared_upgrade
            .as_ref()
            .ok_or("Missing shared-group lineage")?;
        lineage.validate()?;
        let child = job
            .shared_file
            .as_ref()
            .ok_or("Shared replacement requires a shared file")?;
        if job.release.is_none()
            || lineage.parent_jobs.len() != (child.last_episode - child.first_episode + 1) as usize
        {
            return Err("Shared replacement lacks its complete baseline or parent scope".into());
        }
        let first = self
            .jobs
            .get(&lineage.parent_jobs[0])
            .ok_or("Missing shared-group parent")?;
        let old = first
            .shared_file
            .as_ref()
            .ok_or("Replacement parent has no shared binding")?;
        if old.id() != lineage.parent_group
            || old.id() == child.id()
            || old.torrent_id == child.torrent_id
            || old.import_path == child.import_path
            || (
                old.tmdb_id,
                &old.title,
                old.year,
                old.season,
                old.first_episode,
                old.last_episode,
            ) != (
                child.tmdb_id,
                &child.title,
                child.year,
                child.season,
                child.first_episode,
                child.last_episode,
            )
        {
            return Err("Shared replacement must preserve every canonical owner and use a new torrent and destination".into());
        }
        for (offset, id) in lineage.parent_jobs.iter().enumerate() {
            let parent = self.jobs.get(id).ok_or("Missing shared-group parent")?;
            if job.upgrade_parent.as_deref() == Some(id) && job.requester != parent.requester {
                return Err("Shared replacement requester provenance is immutable".into());
            }
            if parent.shared_file.as_ref() != Some(old)
                || parent.request.episode != old.first_episode + offset as u32
                || parent.state != "ready"
                || parent.imports.is_empty()
                || parent.release.is_none()
                || parent.release != first.release
            {
                return Err("Shared replacement parents are incomplete or inconsistent".into());
            }
            if parent.request.episode == job.request.episode {
                let mut expected = parent.request.clone();
                expected.source_url = job.request.source_url.clone();
                if job.upgrade_parent.as_ref() != Some(id)
                    || expected != job.request
                    || job.request.source_url == parent.request.source_url
                    || job.request.source_url == parent.acquisition_url
                {
                    return Err(
                        "Shared replacement changes captured identity or repeats an old source"
                            .into(),
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) fn group_eligible(&self, job: &Job) -> Result<()> {
        self.validate_group_lineage(job)?;
        let lineage = job
            .shared_upgrade
            .as_ref()
            .ok_or("Missing shared-group lineage")?;
        let parents = self.current_shared_group(&lineage.parent_jobs[0])?;
        if parents
            .iter()
            .map(|parent| &parent.id)
            .ne(lineage.parent_jobs.iter())
            || parents.iter().any(|parent| !parent.monitored)
        {
            return Err(
                "Shared replacement requires every current parent to remain monitored".into(),
            );
        }
        let group = job.shared_file.as_ref().ok_or("Missing shared file")?.id();
        if self.jobs.values().any(|other| {
            other
                .shared_file
                .as_ref()
                .is_none_or(|file| file.id() != group)
                && Self::pending_upgrade(other)
                && other
                    .upgrade_parent
                    .as_ref()
                    .is_some_and(|id| lineage.parent_jobs.contains(id))
        }) {
            return Err("Another replacement already reserves this shared group".into());
        }
        Ok(())
    }

    pub(super) fn validate_group_single(&self, job: &Job) -> Result<()> {
        let Some(current) = self.jobs.get(&job.id) else {
            if job.shared_upgrade.is_some() {
                return Err("Shared replacements require a whole-group transaction".into());
            }
            return Ok(());
        };
        if job.shared_upgrade.is_some()
            && ((job.state == "ready" && current.state != "ready")
                || (job.state == "cancelled" && current.state != "cancelled")
                || (current.state == "staged" && job.state != "staged")
                || (matches!(current.state.as_str(), "failed" | "cancelled")
                    && job.state == "queued"))
        {
            return Err("Shared replacement control or promotion requires the whole group".into());
        }
        if job.shared_file.is_some() && current.release.is_none() && job.release.is_some() {
            return Err("Shared baselines require a whole-group transaction".into());
        }
        Ok(())
    }

    pub(super) fn validate_group_operation(&self, jobs: &[Job], action: GroupAction) -> Result<()> {
        let first = jobs.first().ok_or("Empty shared-group transaction")?;
        let binding = first
            .shared_file
            .as_ref()
            .ok_or("Shared-group transaction lacks its file")?;
        if !(2..=MAX_PACK_EPISODES).contains(&jobs.len())
            || jobs
                .iter()
                .map(|job| job.request.episode)
                .ne(binding.first_episode..=binding.last_episode)
        {
            return Err(
                "Shared-group transaction must contain every owner in canonical order".into(),
            );
        }
        let mut ids = BTreeSet::new();
        let mut keys = BTreeSet::new();
        let existing = if action == GroupAction::Create {
            Vec::new()
        } else {
            self.shared_group(&first.id)?
        };
        if action == GroupAction::Retry
            && !existing
                .iter()
                .any(|job| matches!(job.state.as_str(), "failed" | "cancelled"))
        {
            return Err("Whole-group retry requires a failed or cancelled owner".into());
        }
        if !existing.is_empty()
            && existing
                .iter()
                .map(|job| &job.id)
                .ne(jobs.iter().map(|job| &job.id))
        {
            return Err("Shared-group transaction changes its complete owner set".into());
        }
        match action {
            GroupAction::Create | GroupAction::Promote | GroupAction::Retry => {
                self.group_eligible(first)?
            }
            GroupAction::Baseline => {
                self.current_shared_group(&first.id)?;
            }
            GroupAction::Cancel => {}
        }
        let mut completing = 0;
        for job in jobs {
            Job::from_json(&job.to_json())?;
            if job.shared_file.as_ref() != Some(binding)
                || job.shared_upgrade != first.shared_upgrade
                || job.requester.as_ref().map(|p| &p.capture)
                    != first.requester.as_ref().map(|p| &p.capture)
                || job.release != first.release
                || job.request.source_url != first.request.source_url
                || job.updated_at != first.updated_at
                || !ids.insert(&job.id)
                || !keys.insert(&job.key)
            {
                return Err("Inconsistent shared-group transaction".into());
            }
            self.validate_transaction(job, false)?;
            match action {
                GroupAction::Create => {
                    if self.jobs.contains_key(&job.id)
                        || job.shared_upgrade.is_none()
                        || job.state != "queued"
                        || job.progress != 0.0
                        || job.attempts != 0
                        || !job.files.is_empty()
                        || !job.imports.is_empty()
                        || job.acquisition_url.is_some()
                        || job.download_id.is_some()
                        || job.lease_id.is_some()
                        || job.lease_until != 0
                        || job.last_error.is_some()
                        || job.next_attempt_at != 0
                        || job.monitor_checked_at != 0
                        || job.created_at != first.created_at
                        || job.updated_at != job.created_at
                    {
                        return Err("Invalid new shared-group replacement".into());
                    }
                }
                GroupAction::Promote => {
                    let current = self
                        .jobs
                        .get(&job.id)
                        .ok_or("Missing shared promotion owner")?;
                    if job.shared_upgrade.is_none()
                        || job.state != "ready"
                        || job.progress != 1.0
                        || job.imports.is_empty()
                        || job.lease_id.is_some()
                        || job.lease_until != 0
                        || job.next_attempt_at != 0
                        || job.last_error.is_some()
                        || matches!(current.state.as_str(), "ready" | "cancelled")
                    {
                        return Err("Incomplete shared-group promotion".into());
                    }
                    if current.state == "staged" {
                        let mut expected = current.clone();
                        expected.state = "ready".into();
                        expected.updated_at = job.updated_at;
                        expected.monitored = job.monitored;
                        if expected != *job {
                            return Err(
                                "Promotion changes already confirmed owner provenance".into()
                            );
                        }
                    } else {
                        completing += 1;
                    }
                }
                _ => {
                    let current = self.jobs.get(&job.id).ok_or("Missing shared-group owner")?;
                    let mut expected = current.clone();
                    expected.updated_at = job.updated_at;
                    match action {
                        GroupAction::Baseline => {
                            if current.release.is_some() || job.release.is_none() {
                                return Err("Invalid shared-group baseline".into());
                            }
                            expected.release = job.release.clone();
                        }
                        GroupAction::Cancel => {
                            if current.shared_upgrade.is_none() || current.state == "ready" {
                                return Err("Invalid shared-group cancellation".into());
                            }
                            expected.state = "cancelled".into();
                            expected.lease_id = None;
                            expected.lease_until = 0;
                        }
                        GroupAction::Retry => {
                            if current.shared_upgrade.is_none()
                                || current.state == "ready"
                                || current.lease_id.is_some()
                            {
                                return Err("Invalid shared-group retry".into());
                            }
                            expected.state = "queued".into();
                            expected.lease_id = None;
                            expected.lease_until = 0;
                            expected.last_error = None;
                            expected.next_attempt_at = 0;
                        }
                        _ => return Err("Invalid shared-group action".into()),
                    }
                    if expected != *job {
                        return Err("Shared-group action changes unrelated metadata".into());
                    }
                }
            }
        }
        if action == GroupAction::Promote && completing != 1 {
            return Err("Promotion requires exactly one final owner confirmation".into());
        }
        Ok(())
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
