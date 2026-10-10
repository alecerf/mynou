//! CI-only loopback scenarios: a retry that starts a new search must retire the
//! previous native transfer once, durably, and only when nobody else needs it.
mod library_support;
mod requester_support;
use library_support::Directory;
use mynou::{
    config::Config,
    engine::{Engine, lock},
    json::{self, Value},
    store::Job,
    torrent::Client,
};
use requester_support::*;
use std::sync::Arc;

const HASH: &str = "abababababababababababababababababababab";

fn configuration(directory: &Directory, accounts: &Accounts) -> Config {
    let mut cfg = accounts.config(&directory.0);
    cfg.downloads_enabled = true;
    cfg
}

/// Creates a peerless native transfer whose durable identity is all that matters.
fn seed_transfer(cfg: &Config, hash: &str) -> String {
    let client = Client::open(cfg.downloads.clone()).unwrap();
    let status = client
        .ensure(&format!("magnet:?xt=urn:btih:{hash}"))
        .unwrap();
    status.id
}

fn flag(engine: &Engine, transfer: &str, name: &str) -> bool {
    engine.transfer(transfer).unwrap().get(name) == Some(&Value::Bool(true))
}

/// Paused by retirement or by the user; the second is the durable operator choice.
fn paused(engine: &Engine, transfer: &str) -> bool {
    flag(engine, transfer, "paused")
}

fn user_paused(engine: &Engine, transfer: &str) -> bool {
    flag(engine, transfer, "user_paused")
}

fn stored(engine: &Engine, job_id: &str) -> Job {
    lock(&engine.store).unwrap().get(job_id).unwrap()
}

/// Gives a request a failed acquisition that owns `transfer`.
fn fail_holding(engine: &Engine, job_id: &str, transfer: &str) {
    let mut job = stored(engine, job_id);
    job.state = "failed".into();
    job.last_error = Some("Media validation failed".into());
    job.download_id = Some(transfer.into());
    lock(&engine.store).unwrap().update(job).unwrap();
}

/// An opted-in requester account whose acquisition failed while holding a transfer.
fn failed_requester_job(
    directory: &Directory,
    accounts: &Accounts,
) -> (Arc<Engine>, Config, String, String) {
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let cfg = configuration(directory, accounts);
    let transfer = seed_transfer(&cfg, HASH);
    let engine = Engine::open(cfg.clone()).unwrap();
    engine.sync_requesters().unwrap();
    enable(&engine, "alice");
    let job_id = job(&engine, "alice").id;
    fail_holding(&engine, &job_id, &transfer);
    (engine, cfg, transfer, job_id)
}

fn retry(engine: &Engine) {
    let demand = demand(engine, "alice");
    apply(engine, "alice", demand_query("retry", id(&demand)));
}

#[test]
fn requester_retry_pauses_the_unowned_previous_transfer_without_deleting_it() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let (engine, _cfg, transfer, job_id) = failed_requester_job(&directory, &accounts);
    assert!(!paused(&engine, &transfer));
    retry(&engine);
    let retried = stored(&engine, &job_id);
    assert_eq!(retried.state, "queued");
    assert!(retried.download_id.is_none());
    assert!(retried.retiring_transfers.is_empty());
    assert!(paused(&engine, &transfer));
    assert!(!user_paused(&engine, &transfer));
    assert!(json::stringify(&engine.transfers().unwrap()).contains(&transfer));
}

#[test]
fn requester_retry_keeps_a_transfer_another_request_still_owns() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let (engine, _cfg, transfer, job_id) = failed_requester_job(&directory, &accounts);
    let other = lock(&engine.store)
        .unwrap()
        .submit(request(9, "Other Movie"))
        .unwrap();
    let mut owner = other.clone();
    owner.download_id = Some(transfer.clone());
    lock(&engine.store).unwrap().update(owner).unwrap();
    retry(&engine);
    let retried = stored(&engine, &job_id);
    assert_eq!(retried.state, "queued");
    assert!(retried.download_id.is_none());
    assert!(retried.retiring_transfers.is_empty());
    assert!(!paused(&engine, &transfer));
    assert_eq!(
        stored(&engine, &other.id).download_id.as_deref(),
        Some(transfer.as_str())
    );
}

#[test]
fn a_cancelled_owner_does_not_keep_the_previous_transfer_running() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let (engine, _cfg, transfer, _job_id) = failed_requester_job(&directory, &accounts);
    let other = lock(&engine.store)
        .unwrap()
        .submit(request(9, "Other Movie"))
        .unwrap();
    fail_holding(&engine, &other.id, &transfer);
    engine.cancel(&other.id).unwrap();
    assert!(!paused(&engine, &transfer));
    retry(&engine);
    assert!(paused(&engine, &transfer));
}

#[test]
fn requester_retry_preserves_a_durable_user_pause_across_restart() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let (engine, cfg, transfer, job_id) = failed_requester_job(&directory, &accounts);
    engine.pause_transfer(&transfer).unwrap();
    assert!(user_paused(&engine, &transfer));
    retry(&engine);
    assert!(stored(&engine, &job_id).retiring_transfers.is_empty());
    assert!(paused(&engine, &transfer));
    assert!(user_paused(&engine, &transfer));
    drop(engine);
    let engine = Engine::open(cfg).unwrap();
    assert!(user_paused(&engine, &transfer));
    assert!(stored(&engine, &job_id).retiring_transfers.is_empty());
}

#[test]
fn an_interruption_after_retry_persistence_is_recovered_at_startup() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let (engine, cfg, transfer, job_id) = failed_requester_job(&directory, &accounts);
    // The process stops right after the retry transaction, before any pause.
    let retried = lock(&engine.store).unwrap().retry(&job_id).unwrap();
    assert!(retried.download_id.is_none());
    assert_eq!(retried.retiring_transfers, vec![transfer.clone()]);
    assert!(!paused(&engine, &transfer));
    // A worker holding an older copy cannot erase the pending record.
    let mut stale = retried.clone();
    stale.retiring_transfers.clear();
    lock(&engine.store).unwrap().update(stale).unwrap();
    assert_eq!(
        stored(&engine, &job_id).retiring_transfers,
        vec![transfer.clone()]
    );
    drop(engine);
    let engine = Engine::open(cfg.clone()).unwrap();
    assert!(paused(&engine, &transfer));
    assert!(!user_paused(&engine, &transfer));
    let recovered = stored(&engine, &job_id);
    assert!(recovered.retiring_transfers.is_empty());
    assert!(recovered.download_id.is_none());
    drop(engine);
    // The completed retirement is itself durable.
    let engine = Engine::open(cfg).unwrap();
    assert!(stored(&engine, &job_id).retiring_transfers.is_empty());
}

#[test]
fn recovery_keeps_a_transfer_the_request_found_again_before_the_restart() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let (engine, cfg, transfer, job_id) = failed_requester_job(&directory, &accounts);
    lock(&engine.store).unwrap().retry(&job_id).unwrap();
    let mut again = stored(&engine, &job_id);
    again.download_id = Some(transfer.clone());
    lock(&engine.store).unwrap().update(again).unwrap();
    drop(engine);
    let engine = Engine::open(cfg).unwrap();
    assert!(!paused(&engine, &transfer));
    let recovered = stored(&engine, &job_id);
    assert!(recovered.retiring_transfers.is_empty());
    assert_eq!(recovered.download_id.as_deref(), Some(transfer.as_str()));
}

#[test]
fn ordinary_retry_uses_the_same_recorded_retirement() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let cfg = configuration(&directory, &accounts);
    let transfer = seed_transfer(&cfg, HASH);
    let engine = Engine::open(cfg).unwrap();
    let job_id = lock(&engine.store)
        .unwrap()
        .submit(request(9, "Ordinary Movie"))
        .unwrap()
        .id;
    fail_holding(&engine, &job_id, &transfer);
    let retried = engine.retry(&job_id).unwrap();
    assert!(retried.download_id.is_none());
    assert!(retried.retiring_transfers.is_empty());
    assert!(paused(&engine, &transfer));
}

#[test]
fn an_explicit_source_keeps_its_transfer_on_retry() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let cfg = configuration(&directory, &accounts);
    let transfer = seed_transfer(&cfg, HASH);
    let engine = Engine::open(cfg).unwrap();
    let mut explicit = request(9, "Pinned Movie");
    explicit.source_url = Some(format!("magnet:?xt=urn:btih:{HASH}"));
    let job_id = lock(&engine.store).unwrap().submit(explicit).unwrap().id;
    fail_holding(&engine, &job_id, &transfer);
    let retried = engine.retry(&job_id).unwrap();
    assert_eq!(retried.download_id.as_deref(), Some(transfer.as_str()));
    assert!(retried.retiring_transfers.is_empty());
    assert!(!paused(&engine, &transfer));
}
