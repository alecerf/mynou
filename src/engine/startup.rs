//! Startup validation and recovery as bounded steps. Each step reads at most
//! one page (200 rows) of one store with the locks it needs, then releases
//! them. Nothing writes before every read-only provenance stage passes, and
//! workers, API mutations and native transfers wait until the last stage.
//! A restart validates again from the first stage: the stores may have
//! changed while Mynou was stopped, so no earlier progress is trusted.
use super::{Engine, cancel_orphaned_transfers, lock, retire_transfers};
use crate::{Result, inventory::Key, json::Value, torrent::Client};
use std::sync::{Arc, atomic::Ordering};

/// Stage names, in order, as reported by status.
pub(crate) const STAGES: [&str; 13] = [
    "requester-demands",
    "requester-jobs",
    "irc-admissions",
    "irc-demands",
    "irc-routes",
    "irc-origins",
    "initialize",
    "irc-admission-recovery",
    "irc-demand-recovery",
    "irc-route-recovery",
    "irc-origin-recovery",
    "requester-cancellation",
    "transfers",
];

#[derive(Default)]
pub(crate) struct Startup {
    stage: usize,
    after: Option<Key>,
    /// The current stage already ran a page.
    entered: bool,
    pages: u64,
    rows: u64,
    stage_pages: u64,
    error: Option<String>,
}

impl Startup {
    fn state(&self) -> &'static str {
        if self.error.is_some() {
            "failed"
        } else if self.stage >= STAGES.len() {
            "ready"
        } else {
            "validating"
        }
    }

    pub(crate) fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("state", self.state());
        v.insert(
            "stage",
            STAGES
                .get(self.stage)
                .map_or(Value::Null, |stage| (*stage).into()),
        );
        v.insert("stages_done", self.stage.min(STAGES.len()) as u32);
        v.insert("stages", STAGES.len() as u32);
        v.insert("stage_pages", self.stage_pages.to_string());
        v.insert("pages", self.pages.to_string());
        v.insert("rows", self.rows.to_string());
        v.insert(
            "error",
            self.error.clone().map_or(Value::Null, Value::String),
        );
        v
    }
}

impl Engine {
    /// Runs one bounded startup step; true once startup is complete. A
    /// failure is kept: every later call returns it and the gate stays shut.
    pub(crate) fn startup_step(&self) -> Result<bool> {
        let mut startup = lock(&self.startup)?;
        if let Some(error) = &startup.error {
            return Err(error.clone());
        }
        if startup.stage < STAGES.len() {
            match self.startup_page(&startup) {
                Ok((rows, next)) => {
                    startup.pages += 1;
                    startup.stage_pages += 1;
                    startup.rows += rows as u64;
                    startup.after = next;
                    startup.entered = next.is_some();
                    if next.is_none() {
                        startup.stage += 1;
                        startup.stage_pages = 0;
                    }
                }
                Err(error) => {
                    startup.error = Some(error.clone());
                    return Err(error);
                }
            }
        }
        if startup.stage < STAGES.len() {
            return Ok(false);
        }
        self.starting.store(false, Ordering::Release);
        Ok(true)
    }

    /// Completes startup in bounded steps until it finishes, fails or the
    /// engine stops. True when the engine may start its workers.
    pub(crate) fn finish_startup(&self) -> Result<bool> {
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return Ok(false);
            }
            if self.startup_step()? {
                return Ok(true);
            }
        }
    }

    /// `validating`, `ready` or `failed`.
    pub(crate) fn startup_state(&self) -> Result<&'static str> {
        Ok(lock(&self.startup)?.state())
    }

    pub(crate) fn startup_status(&self) -> Result<Value> {
        Ok(lock(&self.startup)?.to_json())
    }

    /// One page of the current stage: rows read and the cursor of the next
    /// page, or `None` when the stage is complete.
    fn startup_page(&self, startup: &Startup) -> Result<(usize, Option<Key>)> {
        let after = startup.after;
        let stage = startup.stage;
        let writable = !self.read_only;
        match stage {
            0 | 1 => {
                let ledger = lock(&self.requester_store)?;
                let mut jobs = lock(&self.store)?;
                crate::requesters::engine::validate_storage_page(
                    &*ledger,
                    &mut *jobs,
                    stage as u8,
                    after,
                )
            }
            2 | 3 | 7 | 8 if writable || stage < 7 => {
                let mut irc = lock(&self.irc_store)?;
                let ledger = lock(&self.requester_store)?;
                crate::irc::admission::recover_page(
                    &mut irc,
                    &*ledger,
                    stage >= 7,
                    u8::from(matches!(stage, 3 | 8)),
                    after,
                )
            }
            4 | 5 | 9 | 10 if writable || stage < 7 => {
                let mut irc = lock(&self.irc_store)?;
                let mut jobs = lock(&self.store)?;
                crate::irc::routing::recover_page(
                    &mut irc,
                    &mut *jobs,
                    stage >= 7,
                    u8::from(matches!(stage, 5 | 10)),
                    after,
                )
            }
            6 => {
                self.initialize_stores()?;
                Ok((0, None))
            }
            11 if writable => {
                let ledger = lock(&self.requester_store)?;
                let mut jobs = lock(&self.store)?;
                if !startup.entered {
                    jobs.finish_requester_sweep(None)?;
                }
                let (rows, done) = crate::requesters::engine::cancel_unwanted_page(
                    &*ledger,
                    &mut jobs,
                    &self.config,
                )?;
                // The sweep keeps its own durable cursor; a placeholder marks
                // that this stage continues.
                Ok((rows, (!done).then_some([0; 64])))
            }
            12 => {
                self.start_transfers()?;
                Ok((0, None))
            }
            _ => Ok((0, None)),
        }
    }

    /// Store initialization, run only after every read-only stage passed.
    fn initialize_stores(&self) -> Result<()> {
        if !self.read_only {
            lock(&self.store)?.initialize()?;
        }
        lock(&self.indexer_store)?.initialize(&self.config)?;
        let mut feeds = lock(&self.feed_store)?;
        feeds.initialize()?;
        if !self.read_only {
            for source in &self.config.sources {
                if !source.options.watched() {
                    feeds.suspend(&crate::indexers::feed::source_id(source))?;
                }
            }
        }
        drop(feeds);
        lock(&self.requester_store)?.initialize()?;
        lock(&self.irc_store)?.initialize()?;
        Ok(())
    }

    /// Opens the native client once requester cancellation settled, then
    /// finishes retirements a previous run recorded but did not complete.
    fn start_transfers(&self) -> Result<()> {
        if self.start_downloads && self.config.downloads_enabled && self.downloads.get().is_none() {
            let admission_store = Arc::clone(&self.store);
            let client = Client::open_with_policy_and_admission(
                self.config.downloads.clone(),
                self.config.download_policy.clone(),
                Arc::new(move |id| {
                    // Client workers must never wait for the request journal
                    // while holding their own state lock: request mutations
                    // can call native controls while holding the journal.
                    let mut store = match admission_store.try_lock() {
                        Ok(store) => store,
                        Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
                        Err(std::sync::TryLockError::Poisoned(_)) => {
                            return Err("Request journal lock is poisoned".into());
                        }
                    };
                    Ok(!store.transfer_cancelled(id)? || store.transfer_has_owner(id)?)
                }),
            )?;
            let mut sweep = lock(&self.transfer_sweep)?;
            cancel_orphaned_transfers(&client, &mut *lock(&self.store)?, &mut sweep)?;
            drop(sweep);
            self.downloads
                .set(client)
                .map_err(|_| "Native transfers already started")?;
        }
        if !self.read_only {
            retire_transfers(self.downloads.get(), &mut *lock(&self.store)?)?;
        }
        Ok(())
    }
}
