//! Checked journal provenance for one direct-media NZB. It never grants queue rights.
use super::{
    newznab::Target,
    nzb::Nzb,
    queue::{Client, OwnedTransfer, Owner},
};
use crate::{
    Result, integrations,
    json::{self, Value},
    requesters::{digest, valid_digest},
    selection::Profile,
    store::{Job, Store},
};
use std::path::{Component, Path};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    pub source_id: String,
    pub max_file_bytes: u64,
    pub max_attempts: u8,
}
impl Document {
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("source_id", self.source_id.clone());
        v.insert("max_file_bytes", self.max_file_bytes.to_string());
        v.insert("max_attempts", u32::from(self.max_attempts));
        v
    }
    fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(v, &["source_id", "max_file_bytes", "max_attempts"])?;
        let d = Self {
            source_id: text(v, "source_id")?,
            max_file_bytes: v
                .get("max_file_bytes")
                .and_then(|n| n.as_u64().or_else(|| n.as_str()?.parse().ok()))
                .filter(|n| *n > 0 && *n <= 1 << 40)
                .ok_or("Usenet admission: invalid captured file limit")?,
            max_attempts: v
                .get("max_attempts")
                .and_then(Value::as_u64)
                .filter(|n| (1..=10).contains(n))
                .ok_or("Usenet admission: invalid captured attempt limit")?
                as u8,
        };
        if !valid_digest(&d.source_id) {
            return Err("Usenet admission: invalid document identity".into());
        }
        Ok(d)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub candidate_id: String,
    pub target: Target,
    pub profile: Profile,
    pub binding: String,
    pub document: Option<Document>,
    pub transfer_id: Option<String>,
    pub archive_limits: Option<crate::archive::Limits>,
    pub rar_limits: Option<crate::archive::Limits>,
    pub archive: Option<super::archive::Preparation>,
}
fn text(v: &Value, k: &str) -> Result<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Usenet admission: invalid {k}"))
}
impl Origin {
    pub fn public_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("transport", "usenet");
        v.insert("candidate_id", self.candidate_id.clone());
        v.insert("indexer_id", self.target.indexer_id.clone());
        v.insert("server_id", self.target.server_id.clone());
        v.insert("advertised_bytes", self.target.advertised_bytes.to_string());
        v.insert("document_captured", self.document.is_some());
        v.insert("zip_enabled", self.archive_limits.is_some());
        v.insert("rar_enabled", self.rar_limits.is_some());
        v.insert("archive_prepared", self.archive.is_some());
        v.insert(
            "archive_verified",
            self.archive.as_ref().is_some_and(|p| p.output.is_some()),
        );
        v.insert(
            "transfer_id",
            self.transfer_id.clone().map_or(Value::Null, Value::from),
        );
        v
    }
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("candidate_id", self.candidate_id.clone());
        v.insert("target", self.target.to_json());
        v.insert("profile", self.profile.to_json());
        v.insert("binding", self.binding.clone());
        v.insert(
            "document",
            self.document
                .as_ref()
                .map_or(Value::Null, Document::to_json),
        );
        v.insert(
            "transfer_id",
            self.transfer_id.clone().map_or(Value::Null, Value::from),
        );
        if let Some(limits) = self.archive_limits {
            v.insert("archive_limits", limits.to_json());
        }
        if let Some(limits) = self.rar_limits {
            v.insert("rar_limits", limits.to_json());
        }
        if let Some(archive) = &self.archive {
            v.insert("archive", archive.to_json());
        }
        v
    }
    pub fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(
            v,
            &[
                "candidate_id",
                "target",
                "profile",
                "binding",
                "document",
                "transfer_id",
                "archive_limits",
                "rar_limits",
                "archive",
            ],
        )?;
        let origin = Self {
            candidate_id: text(v, "candidate_id")?,
            target: Target::from_json(v.get("target").ok_or("Usenet admission: missing target")?)?,
            profile: Profile::from_json(
                v.get("profile")
                    .ok_or("Usenet admission: missing profile")?,
            )?,
            binding: text(v, "binding")?,
            document: match v.get("document") {
                Some(Value::Null) => None,
                Some(v) => Some(Document::from_json(v)?),
                None => return Err("Usenet admission: missing document state".into()),
            },
            transfer_id: match v.get("transfer_id") {
                Some(Value::Null) => None,
                Some(Value::String(s)) if valid_digest(s) => Some(s.clone()),
                _ => return Err("Usenet admission: invalid transfer identity".into()),
            },
            archive_limits: v
                .get("archive_limits")
                .map(crate::archive::Limits::from_json)
                .transpose()?,
            rar_limits: v
                .get("rar_limits")
                .map(crate::archive::Limits::from_json)
                .transpose()?,
            archive: v
                .get("archive")
                .map(super::archive::Preparation::from_json)
                .transpose()?,
        };
        if !valid_digest(&origin.candidate_id)
            || !valid_digest(&origin.binding)
            || origin.target.password_protected
            || origin.transfer_id.is_some() && origin.document.is_none()
            || origin.archive.as_ref().is_some_and(|p| {
                origin.limits(p.plan.format).is_none() || origin.transfer_id.is_none()
            })
            || origin
                .archive_limits
                .is_some_and(|p| v.get("archive_limits") != Some(&p.to_json()))
            || origin
                .rar_limits
                .is_some_and(|p| v.get("rar_limits") != Some(&p.to_json()))
        {
            return Err("Usenet admission: invalid selection provenance".into());
        }
        Ok(origin)
    }
    pub(crate) fn capture(
        job: &Job,
        candidate_id: String,
        target: Target,
        profile: Profile,
        archive_limits: Option<crate::archive::Limits>,
        rar_limits: Option<crate::archive::Limits>,
    ) -> Result<Self> {
        let mut origin = Self {
            candidate_id,
            target,
            profile,
            binding: String::new(),
            document: None,
            transfer_id: None,
            archive_limits,
            rar_limits,
            archive: None,
        };
        origin.binding = origin.identity(job)?;
        origin.validate_job(job)?;
        Ok(origin)
    }
    fn identity(&self, job: &Job) -> Result<String> {
        let release = job
            .release
            .as_ref()
            .ok_or("Usenet admission: missing release assessment")?;
        let url = job
            .acquisition_url
            .as_ref()
            .ok_or("Usenet admission: missing selected acquisition")?;
        let mut fields = vec![
            job.id.clone().into(),
            job.key.clone().into(),
            job.request.to_json(),
            job.created_at.to_string().into(),
            job.requester
                .as_ref()
                .map_or(Value::Null, crate::requesters::Provenance::to_json),
            release.to_json(),
            url.clone().into(),
            self.target.to_json(),
            self.candidate_id.clone().into(),
            self.profile.to_json(),
        ];
        if let Some(limits) = self.archive_limits {
            fields.push(limits.to_json());
        }
        if let Some(limits) = self.rar_limits {
            let mut capability = Value::object();
            capability.insert("rar5-stored", limits.to_json());
            fields.push(capability);
        }
        Ok(digest(json::stringify(&Value::Array(fields)).as_bytes()))
    }
    pub fn owner(&self, job: &Job) -> Owner {
        Owner {
            job_id: job.id.clone(),
            binding: self.binding.clone(),
        }
    }
    pub fn validate_job(&self, job: &Job) -> Result<()> {
        Self::from_json(&self.to_json())?;
        let release = job
            .release
            .as_ref()
            .ok_or("Usenet admission: missing release")?;
        let url = job
            .acquisition_url
            .as_deref()
            .ok_or("Usenet admission: missing acquisition")?;
        crate::net::parse_url(url)?;
        if self.identity(job)? != self.binding
            || !matches!(job.request.kind.as_str(), "movie" | "episode")
            || job.request.source_path.is_some()
            || job.request.source_url.is_some()
            || job.upgrade_parent.is_some()
            || job.pack_file.is_some()
            || job.pack_origin.is_some()
            || job.shared_file.is_some()
            || job.shared_upgrade.is_some()
            || job.irc_origin.is_some()
            || job.download_id.is_some()
            || job.files.len() > 1
            || job.imports.len() > 1
            || !job.files.is_empty() && self.transfer_id.is_none()
            || !job.imports.is_empty() && job.files.len() != 1
            || !integrations::release_identity_matches(&job.request, &release.title)
            || !self
                .profile
                .assess(&release.title, &job.request.title)
                .accepted
        {
            return Err("Usenet admission: job differs from its captured selection".into());
        }
        if let Some(preparation) = &self.archive {
            let plan = &preparation.plan;
            if plan.owner != self.owner(job)
                || self.transfer_id.as_deref() != Some(plan.transfer_id.as_str())
                || self.limits(plan.format) != Some(plan.limits)
                || self
                    .document
                    .as_ref()
                    .is_none_or(|d| plan.source_bytes > d.max_file_bytes)
                || !job.files.is_empty() && preparation.output.is_none()
            {
                return Err(
                    "Usenet admission: archive differs from its captured owner or policy".into(),
                );
            }
            if self.validate_archive_source(job, Path::new(&plan.source_name))? != plan.format {
                return Err(
                    "Usenet admission: archive format differs from its captured source".into(),
                );
            }
            self.validate_file(job, Path::new(&plan.entry_name))?;
        }
        if let Some(path) = job.files.first() {
            let path = Path::new(path);
            self.validate_file(job, path)?;
            let id = self
                .transfer_id
                .as_deref()
                .ok_or("Usenet admission: file lacks a transfer")?;
            let parent = path
                .parent()
                .ok_or("Usenet admission: invalid private path")?;
            if !path.is_absolute()
                || path.components().any(|c| c == Component::ParentDir)
                || parent.file_name().and_then(|n| n.to_str()) != Some("output")
                || parent
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|n| n.to_str())
                    != Some(id)
            {
                return Err("Usenet admission: private path differs from its owner".into());
            }
            if let Some(preparation) = &self.archive
                && path.file_name().and_then(|n| n.to_str())
                    != Some(preparation.plan.output_name()?.as_str())
            {
                return Err(
                    "Usenet admission: archive output filename differs from its captured entry"
                        .into(),
                );
            }
        }
        Ok(())
    }
    pub(crate) fn validate_file(&self, job: &Job, path: &Path) -> Result<()> {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let title = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !["mp4", "m4v", "mov", "mkv", "webm", "avi"].contains(&ext.as_str())
            || !integrations::release_identity_matches(&job.request, title)
            || !self.profile.assess(title, &job.request.title).accepted
        {
            return Err(
                "Usenet admission: verified filename does not satisfy media identity and profile"
                    .into(),
            );
        }
        Ok(())
    }
    pub(crate) fn limits(&self, format: super::archive::Format) -> Option<crate::archive::Limits> {
        match format {
            super::archive::Format::Zip => self.archive_limits,
            super::archive::Format::Rar5 => self.rar_limits,
        }
    }
    pub(crate) fn validate_archive_source(
        &self,
        job: &Job,
        path: &Path,
    ) -> Result<super::archive::Format> {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let format = super::archive::Format::from_path(path)?;
        if self.limits(format).is_none()
            || !integrations::release_identity_matches(&job.request, stem)
            || !self.profile.assess(stem, &job.request.title).accepted
        {
            return Err("Usenet admission: archive source does not satisfy captured identity/profile or opt-in policy".into());
        }
        Ok(format)
    }
    pub(crate) fn check_transfer(&self, job: &Job, transfer: &OwnedTransfer) -> Result<()> {
        self.validate_job(job)?;
        let document = self
            .document
            .as_ref()
            .ok_or("Usenet admission: preparation lacks a captured document")?;
        if transfer.owner != self.owner(job)
            || transfer.source_id != document.source_id
            || transfer.file_index != 0
            || transfer.server_id != self.target.server_id
            || transfer.server_binding != self.target.server_binding
            || transfer.max_file_bytes != document.max_file_bytes
            || transfer.max_attempts != document.max_attempts
            || self
                .transfer_id
                .as_ref()
                .is_some_and(|id| id != &transfer.id)
            || self.transfer_id.is_none()
                && (!matches!(transfer.state.as_str(), "held" | "preparing")
                    || transfer.attempts != 0
                    || transfer.verified_parts != 0)
            || self.archive.is_some() && transfer.state != "complete"
        {
            return Err("Usenet admission: queue differs from its captured preparation".into());
        }
        Ok(())
    }
}
pub(crate) fn validate_transition(current: &Job, next: &Job) -> Result<()> {
    match (&current.usenet_origin, &next.usenet_origin) {
        (None, None) => Ok(()),
        (None, Some(origin))
            if current.acquisition_url.is_none()
                && current.release.is_none()
                && current.files.is_empty()
                && current.imports.is_empty()
                && current.download_id.is_none()
                && current.lease_id.is_some()
                && next.state == "processing"
                && origin.document.is_none()
                && origin.transfer_id.is_none()
                && origin.archive.is_none() =>
        {
            origin.validate_job(next)
        }
        (Some(old), Some(origin))
            if old.candidate_id == origin.candidate_id
                && old.target == origin.target
                && old.profile == origin.profile
                && old.binding == origin.binding
                && old.archive_limits == origin.archive_limits
                && old.rar_limits == origin.rar_limits
                && old.archive.as_ref().is_none_or(|p| {
                    origin.archive.as_ref().is_some_and(|next| {
                        p.plan == next.plan
                            && p.output
                                .as_ref()
                                .is_none_or(|o| next.output.as_ref() == Some(o))
                    })
                })
                && old
                    .document
                    .as_ref()
                    .is_none_or(|d| origin.document.as_ref() == Some(d))
                && old
                    .transfer_id
                    .as_ref()
                    .is_none_or(|id| origin.transfer_id.as_ref() == Some(id)) =>
        {
            if old != origin
                && (current.lease_id.is_none()
                    || !current.files.is_empty()
                    || !current.imports.is_empty()
                    || origin.document.is_none())
            {
                return Err("Usenet admission: preparation requires an active job intent".into());
            }
            origin.validate_job(next)
        }
        _ => Err("Usenet admission: acquisition provenance is immutable".into()),
    }
}
pub(crate) fn validate_storage(store: &Store, client: Option<&Client>) -> Result<()> {
    let jobs = store.list();
    let retained = client
        .map(Client::retained_owned)
        .transpose()?
        .unwrap_or_default();
    if let Some(client) = client {
        let plans = jobs
            .iter()
            .filter_map(|job| {
                job.usenet_origin
                    .as_ref()?
                    .archive
                    .as_ref()
                    .map(|p| p.plan.transfer_id.clone())
            })
            .collect::<std::collections::BTreeSet<_>>();
        super::archive::validate_namespace(&client.private_root()?.join("archives"), &plans)?;
    }
    for transfer in &retained {
        let job = store
            .get(&transfer.owner.job_id)
            .ok_or("Usenet admission: retained preparation has no library job")?;
        let origin = job
            .usenet_origin
            .as_ref()
            .ok_or("Usenet admission: retained preparation has no captured selection")?;
        origin.check_transfer(&job, transfer)?;
        let source = client
            .ok_or("Usenet admission: queue is absent")?
            .owned_source(&transfer.id, &transfer.owner)?;
        if Nzb::parse(&source)?.files.len() != 1 {
            return Err(
                "Usenet admission: only single-file direct-media documents are supported".into(),
            );
        }
    }
    for job in jobs {
        if let Some(origin) = &job.usenet_origin {
            origin.validate_job(&job)?;
            if origin
                .transfer_id
                .as_ref()
                .is_some_and(|id| !retained.iter().any(|r| &r.id == id))
            {
                return Err("Usenet admission: captured transfer is absent".into());
            }
            if !job.files.is_empty() {
                let root = client
                    .ok_or("Usenet admission: queue is absent")?
                    .private_root()?;
                let id = origin
                    .transfer_id
                    .as_deref()
                    .ok_or("Usenet admission: transfer is absent")?;
                let parent = root
                    .join(if origin.archive.is_some() {
                        "archives"
                    } else {
                        "files"
                    })
                    .join(id)
                    .join("output");
                if Path::new(&job.files[0]).parent() != Some(parent.as_path()) {
                    return Err(
                        "Usenet admission: file is outside its captured private workspace".into(),
                    );
                }
            }
            if let Some(preparation) = &origin.archive {
                let client = client.ok_or("Usenet admission: queue is absent")?;
                let source = client
                    .retained_owned_output(&preparation.plan.transfer_id, &origin.owner(&job))?;
                let root = client
                    .private_root()?
                    .join("archives")
                    .join(&preparation.plan.transfer_id);
                super::archive::Workspace::preflight(&root, &source, preparation)?;
            }
        }
    }
    Ok(())
}
