//! Held transfer preparation and bounded, nonpersistent owner authorization.
//! Callers must validate their journal/admission proof before authorizing work.
use super::{Client, Inner, Source, model::*, storage};
use crate::{
    Result,
    json::{self, Value},
    requesters::{digest, valid_digest, valid_id},
    store,
    usenet::{nzb::Nzb, workspace::Workspace},
};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
};

/// Immutable owner identity supplied by a trusted library admission caller.
/// A digest alone does not establish approval or authorize a media import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub job_id: String,
    pub binding: String,
}
impl Owner {
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("job_id", self.job_id.clone());
        v.insert("binding", self.binding.clone());
        v
    }
    pub fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(v, &["job_id", "binding"])?;
        let o = Self {
            job_id: text(v, "job_id")?,
            binding: text(v, "binding")?,
        };
        if !valid_id(&o.job_id) || !valid_digest(&o.binding) {
            return Err("Usenet queue: invalid captured owner".into());
        }
        Ok(o)
    }
    fn validate(&self) -> Result<()> {
        Self::from_json(&self.to_json()).map(|_| ())
    }
}

/// Private checked preparation/progress inventory, not a canonical library admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedTransfer {
    pub id: String,
    pub owner: Owner,
    pub source_id: String,
    pub file_index: usize,
    pub server_id: String,
    pub server_binding: String,
    pub max_file_bytes: u64,
    pub max_attempts: u8,
    pub state: String,
    pub verified_parts: u32,
    pub total_parts: u32,
    pub attempts: u32,
}
impl OwnedTransfer {
    fn capture(r: &Record, verified: u32) -> Result<Self> {
        Ok(Self {
            id: r.id.clone(),
            owner: r
                .owner
                .clone()
                .ok_or("Usenet queue: captured owner is absent")?,
            source_id: r.source.clone(),
            file_index: r.file_index,
            server_id: r.server.clone(),
            server_binding: r.binding.clone(),
            max_file_bytes: r.max_file,
            max_attempts: r.limit,
            state: r.phase.name().into(),
            verified_parts: verified,
            total_parts: r.attempts.len() as u32,
            attempts: r.attempts.iter().map(|n| u32::from(*n)).sum(),
        })
    }
}
impl Record {
    pub(super) fn captured_identity(
        source: &str,
        index: usize,
        server: &str,
        binding: &str,
        max_file: u64,
        limit: u8,
        owner: Option<&Owner>,
    ) -> String {
        let original = Self::identity(source, index, server, binding, max_file, limit);
        owner.map_or_else(
            || original.clone(),
            |o| {
                digest(
                    json::stringify(&Value::Array(vec![original.clone().into(), o.to_json()]))
                        .as_bytes(),
                )
            },
        )
    }
}
impl Inner {
    pub(super) fn workspace_binding(r: &Record) -> String {
        r.owner.as_ref().map_or_else(
            || r.binding.clone(),
            |owner| {
                digest(
                    json::stringify(&Value::Array(vec![
                        r.binding.clone().into(),
                        owner.to_json(),
                    ]))
                    .as_bytes(),
                )
            },
        )
    }
    pub(super) fn owner_allowed(&self, r: &Record) -> bool {
        r.owner.is_none()
            || self
                .permits
                .get(&r.id)
                .is_some_and(|until| *until > store::now())
    }
    pub(super) fn expire_permits(&mut self) -> Result<bool> {
        let ids = self
            .permits
            .iter()
            .filter(|(_, until)| **until <= store::now())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut next = self.data.clone();
        let mut changed = false;
        for id in ids {
            self.permits.remove(&id);
            let r = next
                .records
                .get(&id)
                .ok_or("Usenet queue: owner permission lacks a record")?;
            if matches!(
                r.phase,
                Phase::Queued | Phase::Downloading | Phase::Verifying
            ) {
                let revision = next.next()?;
                let r = next
                    .records
                    .get_mut(&id)
                    .ok_or("Usenet queue: owner record is absent")?;
                r.phase = Phase::Paused;
                r.revision = revision;
                r.reservation = None;
                r.next_attempt = 0;
                r.error = Some("owner_inactive".into());
                changed = true;
            }
        }
        if changed {
            self.persist(next)?;
        }
        Ok(changed)
    }
}
impl Client {
    /// Read the checked inventory for joint journal/queue preflight. Never authorizes work.
    pub fn retained_owned(&self) -> Result<Vec<OwnedTransfer>> {
        let inner = self.lock()?;
        if inner.poisoned {
            return Err("Usenet queue: ownership preflight requires recovery".into());
        }
        inner
            .data
            .records
            .values()
            .filter(|r| r.owner.is_some())
            .map(|r| OwnedTransfer::capture(r, inner.verified.get(&r.id).copied().unwrap_or(0)))
            .collect()
    }
    /// Persist an exact source and owner before preparing a held workspace.
    /// This operation never grants a worker permission or creates a library job.
    pub fn stage_owned(
        &self,
        bytes: &[u8],
        server_id: &str,
        server_binding: &str,
        file_index: usize,
        owner: &Owner,
    ) -> Result<OwnedTransfer> {
        owner.validate()?;
        let nzb = Nzb::parse(bytes)?;
        let file = nzb
            .files
            .get(file_index)
            .ok_or("Usenet queue: file index is absent")?;
        if self.stopped.load(Ordering::Acquire) {
            return Err("Usenet queue: service is stopped".into());
        }
        let mut inner = self.lock()?;
        inner.writable()?;
        let server = inner
            .servers
            .get(server_id)
            .ok_or("Usenet queue: provider is not configured")?;
        if server.binding() != server_binding {
            return Err("Usenet queue: captured provider changed".into());
        }
        let id = Record::captured_identity(
            &nzb.id,
            file_index,
            server_id,
            server_binding,
            inner.settings.max_file_bytes,
            inner.settings.max_attempts,
            Some(owner),
        );
        if let Some(r) = inner.data.records.get(&id) {
            if inner
                .sources
                .get(&nzb.id)
                .is_none_or(|s| s.bytes.as_slice() != bytes)
            {
                return Err("Usenet queue: immutable source conflict".into());
            }
            return OwnedTransfer::capture(r, inner.verified.get(&id).copied().unwrap_or(0));
        }
        if inner.data.records.values().any(|r| {
            r.source == nzb.id && r.file_index == file_index
                || r.owner.as_ref().is_some_and(|o| o.job_id == owner.job_id)
        }) {
            return Err(
                "Usenet queue: source file or owner is already bound to another preparation".into(),
            );
        }
        if inner.data.records.len() >= MAX_RECORDS {
            return Err("Usenet queue: retained record limit reached".into());
        }
        let extra = if inner.sources.contains_key(&nzb.id) {
            0
        } else {
            bytes.len()
        };
        if inner.sources.len() + usize::from(extra != 0) > MAX_SOURCES
            || inner.sources.values().map(|s| s.bytes.len()).sum::<usize>() + extra
                > MAX_SOURCE_BYTES
        {
            return Err("Usenet queue: retained source limit reached".into());
        }
        let mut next = inner.data.clone();
        let revision = next.next()?;
        let row = Record {
            owner: Some(owner.clone()),
            id: id.clone(),
            source: nzb.id.clone(),
            file_index,
            server: server_id.into(),
            binding: server_binding.into(),
            max_file: inner.settings.max_file_bytes,
            limit: inner.settings.max_attempts,
            order: revision,
            revision,
            phase: Phase::Preparing,
            attempts: vec![0; file.segments.len()],
            reservation: None,
            next_attempt: 0,
            error: None,
        };
        next.records.insert(id.clone(), row.clone());
        Data::parse(&next.json())?;
        let source_path = inner
            .settings
            .state_dir
            .join("sources")
            .join(format!("{}.bin", nzb.id));
        if storage::exists(&source_path)? {
            if storage::read(&source_path, storage::SOURCE, crate::usenet::nzb::MAX_BYTES)? != bytes
            {
                return Err("Usenet queue: immutable source conflict".into());
            }
        } else if let Err(e) = storage::write(&source_path, storage::SOURCE, bytes) {
            inner.poisoned = true;
            return Err(e);
        }
        inner
            .sources
            .entry(nzb.id.clone())
            .or_insert_with(|| Source {
                bytes: Arc::new(bytes.to_vec()),
                nzb,
            });
        inner.persist(next.clone())?;
        let w = match Workspace::create(
            &inner.workspace_path(&id),
            bytes,
            file_index,
            &Inner::workspace_binding(&row),
            row.max_file,
        ) {
            Ok(w) => w,
            Err(e) => {
                inner.poisoned = true;
                return Err(e);
            }
        };
        inner.workspaces.insert(id.clone(), w);
        inner.verified.insert(id.clone(), 0);
        let revision = next.next()?;
        let r = next
            .records
            .get_mut(&id)
            .ok_or("Usenet queue: missing owned preparation")?;
        r.phase = Phase::Held;
        r.revision = revision;
        inner.persist(next)?;
        OwnedTransfer::capture(&inner.data.records[&id], 0)
    }
    /// Trusted callers grant at most sixty seconds after validating current admission.
    /// Permissions are never persisted or inherited through reopening; budgets stay intact.
    pub fn authorize_owned(&self, id: &str, owner: &Owner, until: u64) -> Result<()> {
        self.set_owned_permit(id, owner, until, false)
    }
    /// Explicit owner-controlled retry retains every captured article attempt.
    pub fn retry_owned(&self, id: &str, owner: &Owner, until: u64) -> Result<()> {
        self.set_owned_permit(id, owner, until, true)
    }
    fn set_owned_permit(&self, id: &str, owner: &Owner, until: u64, retry: bool) -> Result<()> {
        owner.validate()?;
        let now = store::now();
        if until <= now || until > now.saturating_add(60) || self.stopped.load(Ordering::Acquire) {
            return Err("Usenet queue: invalid or expired owner permission".into());
        }
        let mut inner = self.lock()?;
        inner.writable()?;
        inner.expire_permits()?;
        let r = inner
            .data
            .records
            .get(id)
            .ok_or("Usenet queue: transfer is absent")?
            .clone();
        if r.owner.as_ref() != Some(owner) {
            return Err("Usenet queue: captured owner differs".into());
        }
        if if retry {
            !matches!(r.phase, Phase::Failed | Phase::Cancelled)
        } else {
            matches!(r.phase, Phase::Preparing | Phase::Failed | Phase::Cancelled)
        } {
            return Err("Usenet queue: owner permission does not apply to this state".into());
        }
        if r.phase != Phase::Complete && inner.provider(&r).is_none() {
            return Err("Usenet queue: captured provider is unavailable".into());
        }
        match inner.workspaces.get(id) {
            Some(workspace)
                if workspace
                    .first_missing()
                    .is_some_and(|n| r.attempts[n as usize - 1] >= r.limit) =>
            {
                return Err("Usenet queue: retained attempt budget is exhausted".into());
            }
            Some(_) => {}
            // An old reservation still owns its slot and private handle. The
            // normal claim gate checks the retained budget after it returns.
            None if inner.active.contains(id) => {}
            None => return Err("Usenet queue: workspace is unavailable".into()),
        }
        if matches!(
            r.phase,
            Phase::Held | Phase::Paused | Phase::Failed | Phase::Cancelled
        ) {
            let mut next = inner.data.clone();
            let revision = next.next()?;
            let r = next
                .records
                .get_mut(id)
                .ok_or("Usenet queue: owner record is absent")?;
            r.phase = Phase::Queued;
            r.revision = revision;
            r.reservation = None;
            r.next_attempt = 0;
            r.error = None;
            inner.persist(next)?;
        }
        inner.permits.insert(id.into(), until);
        Ok(())
    }
    /// Revoke permission and fence active results without discarding verified bytes.
    pub fn hold_owned(&self, id: &str, owner: &Owner) -> Result<()> {
        owner.validate()?;
        let mut inner = self.lock()?;
        inner.writable()?;
        let r = inner
            .data
            .records
            .get(id)
            .ok_or("Usenet queue: transfer is absent")?
            .clone();
        if r.owner.as_ref() != Some(owner) {
            return Err("Usenet queue: captured owner differs".into());
        }
        inner.permits.remove(id);
        if matches!(
            r.phase,
            Phase::Queued | Phase::Downloading | Phase::Verifying
        ) {
            let mut next = inner.data.clone();
            let revision = next.next()?;
            let r = next
                .records
                .get_mut(id)
                .ok_or("Usenet queue: owner record is absent")?;
            r.phase = Phase::Paused;
            r.revision = revision;
            r.reservation = None;
            r.next_attempt = 0;
            r.error = Some("owner_inactive".into());
            inner.persist(next)?;
        }
        Ok(())
    }
    /// Reverify private bytes only for the same currently authorized owner.
    /// The caller still must apply canonical identity, profile, import and Plex gates.
    pub fn verified_owned_file(&self, id: &str, owner: &Owner) -> Result<PathBuf> {
        owner.validate()?;
        let mut inner = self.lock()?;
        let r = inner
            .data
            .records
            .get(id)
            .ok_or("Usenet queue: transfer is absent")?
            .clone();
        if inner.poisoned
            || r.phase != Phase::Complete
            || r.owner.as_ref() != Some(owner)
            || !inner.owner_allowed(&r)
            || self.stopped.load(Ordering::Acquire)
        {
            return Err("Usenet queue: authorized verified file is unavailable".into());
        }
        let w = inner
            .workspaces
            .get_mut(id)
            .ok_or("Usenet queue: workspace is unavailable")?;
        let result = w.verify_ready();
        if result.is_err() {
            inner.poisoned = true;
        }
        if !inner.owner_allowed(&r) || self.stopped.load(Ordering::Acquire) {
            return Err("Usenet queue: owner permission expired during verification".into());
        }
        result
    }
}
