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
        };
        if !valid_digest(&origin.candidate_id)
            || !valid_digest(&origin.binding)
            || origin.target.password_protected
            || origin.transfer_id.is_some() && origin.document.is_none()
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
    ) -> Result<Self> {
        let mut origin = Self {
            candidate_id,
            target,
            profile,
            binding: String::new(),
            document: None,
            transfer_id: None,
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
        Ok(digest(
            json::stringify(&Value::Array(vec![
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
            ]))
            .as_bytes(),
        ))
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
                && origin.transfer_id.is_none() =>
        {
            origin.validate_job(next)
        }
        (Some(old), Some(origin))
            if old.candidate_id == origin.candidate_id
                && old.target == origin.target
                && old.profile == origin.profile
                && old.binding == origin.binding
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
                let parent = root.join("files").join(id).join("output");
                if Path::new(&job.files[0]).parent() != Some(parent.as_path()) {
                    return Err(
                        "Usenet admission: file is outside its captured private workspace".into(),
                    );
                }
            }
        }
    }
    Ok(())
}
