//! Native library admission. Network and verification work occur outside journal locks.
use super::{
    admission::{Document, Origin},
    newznab,
    nzb::Nzb,
    queue::OwnedTransfer,
};
use crate::{
    Result,
    engine::{Engine, lock},
    integrations,
    store::{self, Job, RecordedRelease},
};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

impl Engine {
    pub(crate) fn native_usenet_enabled(&self) -> bool {
        !self.read_only
            && self
                .usenet_queue
                .as_ref()
                .is_some_and(|c| c.worker_count() > 0)
    }
    pub(crate) fn require_usenet_lease(&self, job: &Job) -> Result<()> {
        if self.stopped.load(Ordering::Acquire) || !self.native_usenet_enabled() {
            return Err("Native Usenet downloads are disabled or stopped".into());
        }
        let ledger = lock(&self.requester_store)?;
        let store = lock(&self.store)?;
        let current = store.get(&job.id).ok_or("Missing job")?;
        if current.lease_id.is_none()
            || current.lease_id != job.lease_id
            || current.lease_until <= store::now()
            || !crate::requesters::engine::interest(&ledger.state, &current, &self.config)
            || matches!(current.state.as_str(), "ready" | "cancelled" | "failed")
        {
            return Err(
                "Usenet admission: current processing lease or approved demand is absent".into(),
            );
        }
        Ok(())
    }
    pub(crate) fn select_native_acquisition(&self, job: &mut Job) -> Result<()> {
        if !self.config.downloads_enabled && !self.native_usenet_enabled() {
            return Err("Native downloads are disabled".into());
        }
        let config = self.configuration_for(job);
        let selected = integrations::select_acquisition(
            &config,
            &job.request,
            Instant::now() + Duration::from_secs(90),
        )?;
        if let Some(target) = &selected.usenet {
            self.require_usenet_lease(job)?;
            target.configured(&config)?;
        }
        job.release = Some(RecordedRelease {
            title: selected.title,
            profile: selected.profile,
        });
        job.acquisition_url = Some(selected.url);
        if let Some(target) = selected.usenet {
            let (_, profile) = config.selection.profile(&job.request.kind)?;
            job.usenet_origin = Some(Origin::capture(job, selected.id, target, profile.clone())?);
            self.persist_usenet_job(job)?;
        } else {
            lock(&self.store)?.update(job.clone())?;
        }
        Ok(())
    }
    fn persist_usenet_job(&self, job: &Job) -> Result<()> {
        let ledger = lock(&self.requester_store)?;
        let mut store = lock(&self.store)?;
        let current = store.get(&job.id).ok_or("Missing job")?;
        if self.stopped.load(Ordering::Acquire)
            || current.lease_id.is_none()
            || current.lease_id != job.lease_id
            || current.lease_until <= store::now()
            || !crate::requesters::engine::interest(&ledger.state, &current, &self.config)
            || matches!(current.state.as_str(), "ready" | "cancelled" | "failed")
        {
            return Err("Usenet admission: publication lost its lease or approved demand".into());
        }
        job.usenet_origin
            .as_ref()
            .ok_or("Usenet admission: missing origin")?
            .validate_job(job)?;
        store.update(job.clone())
    }
    pub(crate) fn authorize_usenet_job(&self, job: &Job) -> Result<()> {
        let origin = job
            .usenet_origin
            .as_ref()
            .ok_or("Usenet admission: missing origin")?;
        let id = origin
            .transfer_id
            .as_deref()
            .ok_or("Usenet admission: missing transfer")?;
        let config = self.configuration_for(job);
        origin.target.configured(&config)?;
        let (name, profile) = config.selection.profile(&job.request.kind)?;
        if job.release.as_ref().is_none_or(|r| r.profile != name) || profile != &origin.profile {
            return Err("Usenet admission: captured profile changed".into());
        }
        let ledger = lock(&self.requester_store)?;
        let store = lock(&self.store)?;
        let current = store.get(&job.id).ok_or("Missing job")?;
        if self.stopped.load(Ordering::Acquire)
            || current.lease_id.is_none()
            || current.lease_id != job.lease_id
            || current.lease_until <= store::now()
            || current.usenet_origin != job.usenet_origin
            || !crate::requesters::engine::interest(&ledger.state, &current, &self.config)
            || matches!(current.state.as_str(), "ready" | "cancelled" | "failed")
        {
            return Err("Usenet admission: authorization lost its lease or approved demand".into());
        }
        let client = self
            .usenet_queue
            .as_ref()
            .ok_or("Native Usenet downloads are disabled")?;
        let transfer = client
            .retained_owned()?
            .into_iter()
            .find(|r| r.id == id)
            .ok_or("Usenet admission: captured transfer is absent")?;
        origin.check_transfer(&current, &transfer)?;
        client.authorize_owned(
            id,
            &origin.owner(&current),
            current.lease_until.min(store::now().saturating_add(30)),
        )
    }
    pub(crate) fn hold_usenet_job(&self, job: &Job) -> Result<()> {
        if !self.native_usenet_enabled() {
            return Ok(());
        }
        if let (Some(origin), Some(client)) = (&job.usenet_origin, &self.usenet_queue) {
            // A crash/cancellation may occur between held preparation and journal linking.
            for transfer in client
                .retained_owned()?
                .into_iter()
                .filter(|r| r.owner == origin.owner(job))
            {
                origin.check_transfer(job, &transfer)?;
                client.hold_owned(&transfer.id, &transfer.owner)?;
            }
        }
        Ok(())
    }
    pub(crate) fn retry_usenet_job(&self, job: &Job) -> Result<()> {
        if !job.imports.is_empty() {
            return Ok(());
        }
        let origin = job
            .usenet_origin
            .as_ref()
            .ok_or("Usenet admission: missing origin")?;
        if let Some(id) = &origin.transfer_id {
            let client = self
                .usenet_queue
                .as_ref()
                .ok_or("Native Usenet downloads are disabled")?;
            let transfer = client
                .retained_owned()?
                .into_iter()
                .find(|r| &r.id == id)
                .ok_or("Usenet admission: captured transfer is absent")?;
            origin.check_transfer(job, &transfer)?;
            client.prepare_owned_retry(id, &transfer.owner)?;
        }
        Ok(())
    }
    fn owned_for(&self, job: &Job) -> Result<Option<OwnedTransfer>> {
        let origin = job
            .usenet_origin
            .as_ref()
            .ok_or("Usenet admission: missing origin")?;
        self.usenet_queue
            .as_ref()
            .ok_or("Native Usenet downloads are disabled")?
            .retained_owned()?
            .into_iter()
            .find(|r| r.owner == origin.owner(job))
            .map(|r| {
                origin.check_transfer(job, &r)?;
                Ok(r)
            })
            .transpose()
    }
    /// Returns true only after current receipts and output have been reverified.
    pub(crate) fn advance_usenet(&self, job: &mut Job, active: &AtomicBool) -> Result<bool> {
        self.require_usenet_lease(job)?;
        if !active.load(Ordering::Acquire) {
            return Err("Processing interrupted".into());
        }
        let config = self.configuration_for(job);
        let mut origin = job
            .usenet_origin
            .clone()
            .ok_or("Usenet admission: missing origin")?;
        origin.validate_job(job)?;
        origin.target.configured(&config)?;
        let mut source = None;
        if origin.document.is_none() {
            let bytes = newznab::fetch_document(
                &config,
                &origin.target,
                job.acquisition_url.as_deref().ok_or("Missing source")?,
                Instant::now() + Duration::from_secs(30),
            )?;
            let nzb = Nzb::parse(&bytes)?;
            if nzb.files.len() != 1 {
                return Err(
                    "Usenet admission: only single-file direct-media documents are supported"
                        .into(),
                );
            }
            let settings = config
                .usenet
                .downloads
                .as_ref()
                .ok_or("Native Usenet downloads are disabled")?;
            origin.document = Some(Document {
                source_id: nzb.id,
                max_file_bytes: settings.max_file_bytes,
                max_attempts: settings.max_attempts,
            });
            job.usenet_origin = Some(origin.clone());
            self.persist_usenet_job(job)?;
            source = Some(bytes);
        }
        if origin.transfer_id.is_none() {
            let transfer = match self.owned_for(job)? {
                Some(retained) => retained,
                None => {
                    let document = origin
                        .document
                        .as_ref()
                        .ok_or("Usenet admission: missing document")?;
                    let settings = config
                        .usenet
                        .downloads
                        .as_ref()
                        .ok_or("Native Usenet downloads are disabled")?;
                    if settings.max_file_bytes != document.max_file_bytes
                        || settings.max_attempts != document.max_attempts
                    {
                        return Err(
                            "Usenet admission: preparation limits changed before staging".into(),
                        );
                    }
                    let bytes = match source {
                        Some(bytes) => bytes,
                        None => newznab::fetch_document(
                            &config,
                            &origin.target,
                            job.acquisition_url.as_deref().ok_or("Missing source")?,
                            Instant::now() + Duration::from_secs(30),
                        )?,
                    };
                    let nzb = Nzb::parse(&bytes)?;
                    if nzb.id != document.source_id || nzb.files.len() != 1 {
                        return Err(
                            "Usenet admission: original document changed before staging".into()
                        );
                    }
                    self.require_usenet_lease(job)?;
                    self.usenet_queue
                        .as_ref()
                        .ok_or("Native Usenet downloads are disabled")?
                        .stage_owned(
                            &bytes,
                            &origin.target.server_id,
                            &origin.target.server_binding,
                            0,
                            &origin.owner(job),
                        )?
                }
            };
            origin.check_transfer(job, &transfer)?;
            origin.transfer_id = Some(transfer.id);
            job.usenet_origin = Some(origin.clone());
            self.persist_usenet_job(job)?;
        }
        self.authorize_usenet_job(job)?;
        let transfer = self
            .owned_for(job)?
            .ok_or("Usenet admission: captured transfer is absent")?;
        job.progress = f64::from(transfer.verified_parts) / f64::from(transfer.total_parts);
        if transfer.state != "complete" {
            job.state = "downloading".into();
            job.next_attempt_at = store::now().saturating_add(1);
            return Ok(false);
        }
        let id = origin
            .transfer_id
            .as_deref()
            .ok_or("Usenet admission: missing transfer")?;
        let client = self
            .usenet_queue
            .as_ref()
            .ok_or("Native Usenet downloads are disabled")?;
        let path = match client.verified_owned_file(id, &origin.owner(job)) {
            Err(e) if e == "Usenet queue: verification slot is busy" => {
                job.state = "downloading".into();
                job.next_attempt_at = store::now().saturating_add(1);
                return Ok(false);
            }
            other => other?,
        };
        origin.validate_file(job, &path)?;
        let source = origin.target.configured(&config)?;
        let options = source
            .options
            .newznab
            .as_ref()
            .ok_or("Newznab: missing source settings")?;
        let bytes = std::fs::metadata(&path)
            .map_err(|_| "Usenet admission: cannot inspect verified output")?
            .len();
        if bytes < options.minimum_bytes || bytes > options.maximum_bytes {
            return Err("Usenet admission: verified size is outside captured source policy".into());
        }
        if !job.files.is_empty() && job.files != [path.to_string_lossy().into_owned()] {
            return Err("Usenet admission: verified file provenance changed".into());
        }
        job.files = vec![path.to_string_lossy().into_owned()];
        self.persist_usenet_job(job)?;
        Ok(true)
    }
}
