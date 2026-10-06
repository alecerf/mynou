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
    time::{Duration, Instant},
};

pub(super) struct Permit {
    until: u64,
    deadline: Instant,
    serial: u64,
}
impl Permit {
    fn active(&self) -> bool {
        self.until > store::now() && self.deadline > Instant::now()
    }
}

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
        r.owner.is_none() || self.permits.get(&r.id).is_some_and(Permit::active)
    }
    pub(super) fn expire_permits(&mut self) -> Result<bool> {
        let ids = self
            .permits
            .iter()
            .filter(|(_, permit)| !permit.active())
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
    pub(crate) fn private_root(&self) -> Result<PathBuf> {
        Ok(self.lock()?.settings.state_dir.clone())
    }
    pub(crate) fn owned_source(&self, id: &str, owner: &Owner) -> Result<Arc<Vec<u8>>> {
        let inner = self.lock()?;
        let row = inner
            .data
            .records
            .get(id)
            .ok_or("Usenet queue: transfer is absent")?;
        if inner.poisoned || row.owner.as_ref() != Some(owner) {
            return Err("Usenet queue: checked owner source is unavailable".into());
        }
        inner
            .sources
            .get(&row.source)
            .map(|s| s.bytes.clone())
            .ok_or_else(|| "Usenet queue: retained source is absent".into())
    }
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
    /// Reopen an explicit library retry without granting any worker permission.
    pub(crate) fn prepare_owned_retry(&self, id: &str, owner: &Owner) -> Result<()> {
        owner.validate()?;
        let mut inner = self.lock()?;
        inner.writable()?;
        let r = inner
            .data
            .records
            .get(id)
            .ok_or("Usenet queue: transfer is absent")?
            .clone();
        if self.stopped.load(Ordering::Acquire) || r.owner.as_ref() != Some(owner) {
            return Err("Usenet queue: captured retry owner is unavailable".into());
        }
        inner.permits.remove(id);
        if !matches!(r.phase, Phase::Failed | Phase::Cancelled) {
            return Ok(());
        }
        let workspace = inner
            .workspaces
            .get(id)
            .ok_or("Usenet queue: retry workspace is unavailable")?;
        if workspace
            .first_missing()
            .is_some_and(|n| r.attempts[n as usize - 1] >= r.limit)
        {
            return Err("Usenet queue: retained attempt budget is exhausted".into());
        }
        let mut next = inner.data.clone();
        let revision = next.next()?;
        let r = next
            .records
            .get_mut(id)
            .ok_or("Usenet queue: retry record is absent")?;
        r.phase = Phase::Paused;
        r.revision = revision;
        r.reservation = None;
        r.next_attempt = 0;
        r.error = Some("owner_inactive".into());
        inner.persist(next)
    }
    fn set_owned_permit(&self, id: &str, owner: &Owner, until: u64, retry: bool) -> Result<()> {
        owner.validate()?;
        let now = store::now();
        if until <= now || until > now.saturating_add(60) || self.stopped.load(Ordering::Acquire) {
            return Err("Usenet queue: invalid or expired owner permission".into());
        }
        let mut permit = Permit {
            until,
            deadline: Instant::now()
                .checked_add(Duration::from_secs(until - now))
                .ok_or("Usenet queue: invalid owner time budget")?,
            serial: 0,
        };
        let mut inner = self.lock()?;
        inner.writable()?;
        inner.expire_permits()?;
        if !permit.active() {
            return Err("Usenet queue: owner permission expired before application".into());
        }
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
        if !permit.active() {
            return Err("Usenet queue: owner permission expired during application".into());
        }
        permit.serial = match inner.permits.get(id).filter(|p| p.active()) {
            Some(previous) => previous.serial,
            None => {
                inner.permission_revision = inner
                    .permission_revision
                    .checked_add(1)
                    .ok_or("Usenet queue: owner permission generation exhausted")?;
                inner.permission_revision
            }
        };
        inner.permits.insert(id.into(), permit);
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
        self.verified_owned_by(id, owner, Workspace::verify_ready)
    }
    /// Runs a private library operation in a bounded slot outside the queue mutex.
    /// Callback errors do not invalidate good article proofs. Source corruption
    /// does, and permission revocation/regrant fences the operation's result.
    pub(crate) fn verified_owned_operation<T>(
        &self,
        id: &str,
        owner: &Owner,
        operation: impl FnOnce(&std::path::Path, u64, &str) -> Result<T>,
    ) -> Result<T> {
        self.verified_owned_by(id, owner, |workspace| {
            let path = workspace.verify_ready()?;
            let (_, bytes, sha) = workspace.output_identity()?;
            let result = operation(&path, bytes, &sha);
            workspace.verify_ready()?;
            Ok(result)
        })?
    }
    /// Checked startup inventory only; this does not grant operational rights.
    pub(crate) fn retained_owned_output(&self, id: &str, owner: &Owner) -> Result<PathBuf> {
        let inner = self.lock()?;
        let row = inner
            .data
            .records
            .get(id)
            .ok_or("Usenet queue: transfer is absent")?;
        if inner.poisoned || row.owner.as_ref() != Some(owner) {
            return Err("Usenet queue: checked owner output is unavailable".into());
        }
        inner
            .workspaces
            .get(id)
            .and_then(Workspace::available_file)
            .ok_or_else(|| "Usenet queue: checked output is absent".into())
    }
    fn verified_owned_by<T>(
        &self,
        id: &str,
        owner: &Owner,
        verify: impl FnOnce(&mut Workspace) -> Result<T>,
    ) -> Result<T> {
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
        if inner.active.len() >= inner.settings.max_active || inner.active.contains(id) {
            return Err("Usenet queue: verification slot is busy".into());
        }
        let serial = inner
            .permits
            .get(id)
            .ok_or("Usenet queue: owner permission is absent")?
            .serial;
        let mut workspace = inner
            .workspaces
            .remove(id)
            .ok_or("Usenet queue: workspace is unavailable")?;
        inner.active.insert(id.into());
        drop(inner);
        let _active = super::Active {
            inner: self.inner.clone(),
            id: id.into(),
        };
        // Disk hashing never prevents owner renewal/revocation or other bounded work.
        let result = verify(&mut workspace);
        let mut inner = self.lock()?;
        inner.workspaces.insert(id.into(), workspace);
        if result.is_err() {
            inner.poisoned = true;
        }
        if inner.poisoned
            || self.stopped.load(Ordering::Acquire)
            || inner.data.records.get(id).is_none_or(|current| {
                current.phase != Phase::Complete || current.owner.as_ref() != Some(owner)
            })
            || !inner
                .permits
                .get(id)
                .is_some_and(|p| p.active() && p.serial == serial)
        {
            return Err(
                "Usenet queue: owner permission changed or verification requires recovery".into(),
            );
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::{atomic::AtomicU64, mpsc},
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "mynou-owner-verification-{}-{timestamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn completed() -> (Directory, Client, OwnedTransfer) {
        let root = Directory::new();
        let mut cfg = crate::config::default_json();
        cfg.get_mut("downloads").unwrap().insert("enabled", false);
        cfg.insert("usenet", json::parse(r#"{"servers":[{"id":"original","host":"127.0.0.1","port":119,"tls":false}],"downloads":{"enabled":true,"state_dir":"queue","max_file_bytes":4096}}"#).unwrap());
        let settings = crate::config::from_json(&cfg, &root.0).unwrap().usenet;
        let client = Client::open(&settings, false).unwrap();
        let owner = Owner {
            job_id: "original-owner".into(),
            binding: "e".repeat(64),
        };
        let transfer = client.stage_owned(
            b"<nzb><file subject=\"Original lock fixture\"><groups><group>alt.binaries.fixture</group></groups><segments><segment number=\"1\" bytes=\"1024\">proof@fixture.test</segment></segments></file></nzb>",
            "original", &settings.servers[0].binding(), 0, &owner,
        ).unwrap();
        // Original checked disk fixture, with no NNTP connection or library import.
        let payload = b"proof";
        let mut article = b"=ybegin line=128 size=5 name=original.bin\r\n".to_vec();
        article.extend(payload.iter().map(|b| b.wrapping_add(42)));
        article.extend_from_slice(
            format!(
                "\r\n=yend size=5 crc32={:08x}\r\n",
                crate::usenet::yenc::crc32(payload)
            )
            .as_bytes(),
        );
        let part = crate::usenet::yenc::decode(&article).unwrap();
        {
            let mut inner = client.lock().unwrap();
            let mut next = inner.data.clone();
            let revision = next.next().unwrap();
            let row = next.records.get_mut(&transfer.id).unwrap();
            row.attempts[0] = 1;
            row.phase = Phase::Queued;
            row.revision = revision;
            inner.persist(next).unwrap();
            let workspace = inner.workspaces.get_mut(&transfer.id).unwrap();
            workspace.accept(&part, "proof@fixture.test").unwrap();
            workspace.assemble().unwrap();
            let mut next = inner.data.clone();
            let revision = next.next().unwrap();
            let row = next.records.get_mut(&transfer.id).unwrap();
            row.phase = Phase::Complete;
            row.revision = revision;
            inner.verified.insert(transfer.id.clone(), 1);
            inner.persist(next).unwrap();
        }
        (root, client, transfer)
    }
    fn verification_gate(revoke: bool) {
        let (_root, client, transfer) = completed();
        client
            .authorize_owned(&transfer.id, &transfer.owner, store::now() + 60)
            .unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_client = client.clone();
        let worker_transfer = transfer.clone();
        let worker = thread::spawn(move || {
            worker_client.verified_owned_by(
                &worker_transfer.id,
                &worker_transfer.owner,
                |workspace| {
                    started_tx
                        .send(())
                        .map_err(|_| "Original verification gate closed")?;
                    release_rx
                        .recv_timeout(Duration::from_secs(4))
                        .map_err(|_| "Original verification gate timed out")?;
                    workspace.verify_ready()
                },
            )
        });
        started_rx.recv_timeout(Duration::from_secs(4)).unwrap();
        // Both calls must complete while the worker owns its private workspace.
        assert_eq!(
            client
                .report()
                .unwrap()
                .get("active")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert!(
            client
                .verified_owned_file(&transfer.id, &transfer.owner)
                .is_err()
        );
        if revoke {
            client.hold_owned(&transfer.id, &transfer.owner).unwrap();
        }
        client
            .authorize_owned(&transfer.id, &transfer.owner, store::now() + 60)
            .unwrap();
        release_tx.send(()).unwrap();
        let verified = worker.join().unwrap();
        assert_eq!(verified.is_err(), revoke);
        assert_eq!(
            client
                .report()
                .unwrap()
                .get("active")
                .and_then(Value::as_u64),
            Some(0)
        );
        let ready = client
            .verified_owned_file(&transfer.id, &transfer.owner)
            .unwrap();
        assert_eq!(fs::read(ready).unwrap(), b"proof");
        assert_eq!(client.retained_owned().unwrap()[0].attempts, 1);
    }
    #[test]
    fn disk_verification_allows_renewal_without_holding_the_queue_mutex() {
        verification_gate(false);
    }
    #[test]
    fn revocation_and_regrant_fence_a_preceding_disk_verification() {
        verification_gate(true);
    }
    #[test]
    fn a_future_wall_deadline_cannot_extend_an_elapsed_monotonic_permission() {
        let permit = Permit {
            until: u64::MAX,
            deadline: Instant::now(),
            serial: 1,
        };
        assert!(!permit.active());
    }
    #[test]
    fn a_future_monotonic_deadline_cannot_restore_an_expired_wall_permission() {
        let permit = Permit {
            until: 0,
            deadline: Instant::now() + Duration::from_secs(60),
            serial: 1,
        };
        assert!(!permit.active());
    }
}
