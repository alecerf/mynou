//! Whole-group replacement lineage and synchronized group transitions.
use super::*;
use crate::pack::{MAX_PACK_EPISODES, SharedFile};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedUpgrade {
    pub parent_group: String,
    /// Canonical episode order, never a caller-selected subset.
    pub parent_jobs: Vec<String>,
}

impl SharedUpgrade {
    pub fn validate(&self) -> Result<()> {
        let valid = |id: &str| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        let unique: BTreeSet<_> = self.parent_jobs.iter().collect();
        if !valid(&self.parent_group)
            || !(2..=MAX_PACK_EPISODES).contains(&self.parent_jobs.len())
            || unique.len() != self.parent_jobs.len()
            || self.parent_jobs.iter().any(|id| !valid(id))
        {
            return Err("Invalid shared-group replacement lineage".into());
        }
        Ok(())
    }
    pub fn to_json(&self) -> Value {
        object([
            ("parent_group", self.parent_group.clone().into()),
            (
                "parent_jobs",
                Value::Array(
                    self.parent_jobs
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            ),
        ])
    }
    pub fn from_json(value: &Value) -> Result<Self> {
        let map = fields(value)?;
        if map
            .keys()
            .any(|key| !["parent_group", "parent_jobs"].contains(&key.as_str()))
        {
            return Err("Unknown shared-group lineage field".into());
        }
        let result = Self {
            parent_group: string(map, "parent_group")?,
            parent_jobs: strings(map, "parent_jobs")?,
        };
        result.validate()?;
        Ok(result)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupAction {
    Baseline,
    Create,
    Promote,
    Cancel,
    Retry,
}
impl GroupAction {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Create => "create",
            Self::Promote => "promote",
            Self::Cancel => "cancel",
            Self::Retry => "retry",
        }
    }
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "baseline" => Ok(Self::Baseline),
            "create" => Ok(Self::Create),
            "promote" => Ok(Self::Promote),
            "cancel" => Ok(Self::Cancel),
            "retry" => Ok(Self::Retry),
            _ => Err("Unknown shared-group journal action".into()),
        }
    }
}

pub(super) fn group_format(job: &Job) -> bool {
    job.shared_upgrade.is_some() || (job.shared_file.is_some() && job.release.is_some())
}

pub(super) struct GroupState<'a> {
    pub file: &'a SharedFile,
    pub owners: BTreeSet<u32>,
    pub first: &'a Job,
    pub ready: usize,
    pub cancelled: usize,
}

impl Store {
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

    /// Memoized, iterative traversal avoids recursion and repeated long histories.
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

    pub(crate) fn check_group_replacement(
        &self,
        parents: &[Job],
        file: &SharedFile,
        requests: &[Request],
        release: &RecordedRelease,
    ) -> Result<Vec<Job>> {
        file.validate()?;
        release.validate()?;
        let first = parents.first().ok_or("Missing shared-group parents")?;
        let old = first.shared_file.as_ref().ok_or("Missing parent binding")?;
        if self.current_shared_group(&first.id)? != parents
            || parents.len() != requests.len()
            || parents
                .iter()
                .any(|parent| !parent.monitored || parent.release.is_none())
        {
            return Err(
                "Replacement requires the complete monitored current group and its baseline".into(),
            );
        }
        let lineage = SharedUpgrade {
            parent_group: old.id(),
            parent_jobs: parents.iter().map(|parent| parent.id.clone()).collect(),
        };
        let mut prototype = first.clone();
        prototype.id = "0".repeat(32);
        prototype.shared_file = Some(file.clone());
        prototype.pack_file = Some(file.file_path.clone());
        prototype.shared_upgrade = Some(lineage.clone());
        prototype.upgrade_parent = Some(first.id.clone());
        prototype.request = requests
            .first()
            .ok_or("Missing replacement request")?
            .clone();
        prototype.release = Some(release.clone());
        self.group_eligible(&prototype)?;
        for (parent, request) in parents.iter().zip(requests) {
            request.validate()?;
            let mut expected = parent.request.clone();
            expected.source_url = request.source_url.clone();
            if expected != *request
                || request.source_url != prototype.request.source_url
                || request.source_url.is_none()
            {
                return Err("Replacement must preserve the complete captured request scope".into());
            }
        }
        let known: Vec<_> = self
            .jobs
            .values()
            .filter(|job| job.shared_file.as_ref() == Some(file))
            .cloned()
            .collect();
        if !known.is_empty() {
            let jobs = self.shared_group(&known[0].id)?;
            if jobs.iter().zip(requests).any(|(job, request)| {
                job.request != *request
                    || job.release.as_ref() != Some(release)
                    || job.shared_upgrade.as_ref() != Some(&lineage)
            }) {
                return Err("Replacement video is already owned by different provenance".into());
            }
            return Ok(jobs);
        }
        if self
            .shared_by_physical
            .contains_key(&(file.torrent_id.clone(), file.file_path.clone()))
            || requests
                .iter()
                .any(|request| self.by_key.contains_key(&request.canonical_key()))
            || self.jobs.values().any(|job| {
                job.shared_file.is_none()
                    && job.pack_file.as_deref() == Some(&file.file_path)
                    && (job.download_id.as_deref() == Some(&file.torrent_id)
                        || job.request.source_url == prototype.request.source_url)
            })
        {
            return Err("Replacement physical source or request is already owned".into());
        }
        self.check_submission_capacity(requests.len())?;
        Ok(Vec::new())
    }

    pub(crate) fn submit_group_replacement(
        &mut self,
        parents: &[Job],
        file: SharedFile,
        requests: Vec<Request>,
        release: RecordedRelease,
    ) -> Result<Vec<Job>> {
        let known = self.check_group_replacement(parents, &file, &requests, &release)?;
        if !known.is_empty() {
            return Ok(known);
        }
        let at = now();
        let lineage = SharedUpgrade {
            parent_group: parents[0]
                .shared_file
                .as_ref()
                .ok_or("Missing group binding")?
                .id(),
            parent_jobs: parents.iter().map(|job| job.id.clone()).collect(),
        };
        let mut jobs = Vec::with_capacity(requests.len());
        for (parent, request) in parents.iter().zip(requests) {
            jobs.push(Job {
                irc_origin: None,
                usenet_origin: None,
                id: random_id()?,
                key: request.canonical_key(),
                request,
                state: "queued".into(),
                progress: 0.0,
                files: Vec::new(),
                imports: Vec::new(),
                attempts: 0,
                last_error: None,
                created_at: at,
                updated_at: at,
                next_attempt_at: 0,
                acquisition_url: None,
                download_id: None,
                lease_id: None,
                lease_until: 0,
                release: Some(release.clone()),
                upgrade_parent: Some(parent.id.clone()),
                monitored: parent.monitored,
                monitor_checked_at: 0,
                pack_file: Some(file.file_path.clone()),
                pack_origin: None,
                shared_file: Some(file.clone()),
                shared_upgrade: Some(lineage.clone()),
                requester: parent.requester.clone(),
            });
        }
        self.commit_group(
            jobs.clone(),
            "whole shared-group replacement recorded",
            GroupAction::Create,
        )?;
        Ok(jobs)
    }

    pub(crate) fn set_group_baseline(
        &mut self,
        id: &str,
        release: RecordedRelease,
    ) -> Result<Vec<Job>> {
        release.validate()?;
        let mut jobs = self.current_shared_group(id)?;
        if jobs.iter().any(|job| job.release.is_some()) {
            return Err("Shared-group baseline is already recorded".into());
        }
        let at = now();
        for job in &mut jobs {
            job.release = Some(release.clone());
            job.updated_at = at;
        }
        self.commit_group(
            jobs.clone(),
            "whole shared-group baseline recorded",
            GroupAction::Baseline,
        )?;
        Ok(jobs)
    }

    pub(super) fn finish_group_owner(&mut self, mut job: Job) -> Result<()> {
        self.group_eligible(&job)?;
        job.state = "staged".into();
        job.lease_id = None;
        job.lease_until = 0;
        Job::from_json(&job.to_json())?;
        let mut jobs = self.shared_group(&job.id)?;
        for owner in &mut jobs {
            if owner.id == job.id {
                *owner = job.clone();
            }
        }
        if jobs.iter().all(|owner| owner.state == "staged") {
            let at = now();
            for owner in &mut jobs {
                owner.state = "ready".into();
                owner.updated_at = at;
                owner.monitored = self
                    .jobs
                    .get(owner.upgrade_parent.as_deref().unwrap_or_default())
                    .ok_or("Missing promotion parent")?
                    .monitored;
            }
            self.commit_group(
                jobs,
                "whole shared group promoted atomically",
                GroupAction::Promote,
            )
        } else {
            self.commit(
                job,
                "shared owner confirmed; waiting for the complete group",
            )
        }
    }

    pub(super) fn cancel_group(&mut self, id: &str) -> Result<Job> {
        let mut jobs = self.shared_group(id)?;
        if jobs.iter().any(|job| job.state == "ready") {
            return Err("A promoted shared group cannot be cancelled".into());
        }
        if jobs.iter().all(|job| job.state == "cancelled") {
            return self.get(id).ok_or("Unknown group owner".into());
        }
        let at = now();
        for job in &mut jobs {
            job.state = "cancelled".into();
            job.lease_id = None;
            job.lease_until = 0;
            job.updated_at = at;
        }
        self.commit_group(
            jobs,
            "whole shared-group replacement cancelled",
            GroupAction::Cancel,
        )?;
        self.get(id).ok_or("Unknown group owner".into())
    }

    pub(super) fn retry_group(&mut self, id: &str) -> Result<Job> {
        let mut jobs = self.shared_group(id)?;
        if !jobs
            .iter()
            .any(|job| matches!(job.state.as_str(), "failed" | "cancelled"))
            || jobs
                .iter()
                .any(|job| job.state == "ready" || job.lease_id.is_some())
        {
            return Err(
                "Group retry requires a failed or cancelled replacement without active leases"
                    .into(),
            );
        }
        self.group_eligible(&jobs[0])?;
        let at = now();
        for job in &mut jobs {
            job.state = "queued".into();
            job.lease_id = None;
            job.lease_until = 0;
            job.last_error = None;
            job.next_attempt_at = 0;
            job.updated_at = at;
        }
        self.commit_group(
            jobs,
            "whole shared-group replacement retried",
            GroupAction::Retry,
        )?;
        self.get(id).ok_or("Unknown group owner".into())
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
        if action == GroupAction::Create {
            self.check_submission_capacity(jobs.len())?;
        }
        Ok(())
    }

    pub(super) fn commit_group(
        &mut self,
        jobs: Vec<Job>,
        message: &str,
        action: GroupAction,
    ) -> Result<()> {
        self.validate_group_operation(&jobs, action)?;
        self.commit_operation(jobs, message, Some(action))
    }
}
