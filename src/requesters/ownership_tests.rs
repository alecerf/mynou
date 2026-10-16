//! Independent ownership fixtures for indexed consumers; validation runs in CI.
use super::{Account, Capture, Demand, LedgerRead, Policy, Provenance};
use crate::{
    config,
    crypto::random_bytes,
    engine::{Engine, lock},
    store::Request,
};
use std::{collections::BTreeSet, fs, path::PathBuf};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let suffix = super::digest(&random_bytes::<16>().unwrap());
        Self(std::env::temp_dir().join(format!("mynou-indexed-ownership-{suffix}")))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn config(directory: &Directory) -> crate::config::Config {
    let mut value = config::default_json();
    value.get_mut("downloads").unwrap().insert("enabled", false);
    let mut config = config::from_json(&value, &directory.0).unwrap();
    config.requesters.accounts = ["source", "coowner"]
        .into_iter()
        .map(|id| Account {
            id: id.into(),
            expected_user_id: "811".into(),
            token_env: "MYNOU_SYNTHETIC_OWNERSHIP".into(),
            identity_url: "http://127.0.0.1:1/identity".into(),
            watchlist_url: "http://127.0.0.1:1/watchlist".into(),
        })
        .collect();
    config
}

fn demand(config: &crate::config::Config, account: &str, number: u64) -> Demand {
    let request = Request {
        kind: "movie".into(),
        title: format!("Synthetic owned movie {number}"),
        year: 2026,
        season: 0,
        episode: 0,
        tmdb_id: Some(number + 1),
        source_path: None,
        source_url: None,
        source_numbering: None,
    };
    Demand {
        id: Demand::identity(account, &request),
        account_id: account.into(),
        origins: BTreeSet::from([format!("synthetic:{number}")]),
        request,
        revision: 1,
        capture: Capture::new(config, &Policy::initial(config), "movie").unwrap(),
        state: "pending".into(),
        job_id: None,
        admitted_at: None,
        outcome: "pending".into(),
    }
}

fn provenance(record: &Demand) -> Provenance {
    Provenance {
        account_id: record.account_id.clone(),
        demand_id: record.id.clone(),
        policy_revision: record.revision,
        capture: record.capture.clone(),
    }
}

#[test]
fn corrupt_ownership_object_stops_cancellation_and_preserves_the_queued_job() {
    let directory = Directory::new();
    let config = config(&directory);
    let engine = Engine::open_for_management(config.clone()).unwrap();
    let mut record = demand(&config, "source", 81);
    record.state = "reserved".into();
    record.outcome = "reserved".into();
    record.admitted_at = Some(1234);
    let mut ledger = lock(&engine.requester_store).unwrap();
    let mut patch = ledger.state.clone();
    patch.demands.insert(record.id.clone(), record.clone());
    ledger.save_patch(patch).unwrap();
    let mut jobs = lock(&engine.store).unwrap();
    let job = jobs
        .submit_requester(record.request.clone(), provenance(&record))
        .unwrap();
    record.state = "active".into();
    record.outcome = "active".into();
    record.job_id = Some(job.id.clone());
    let mut patch = ledger.state.clone();
    patch.demands.insert(record.id.clone(), record.clone());
    ledger.save_patch(patch).unwrap();
    assert!(super::engine::interest(&*ledger, &job, &config).unwrap());
    let key = super::index::primary_cursor(&record.id).unwrap();
    assert!(crate::inventory::damage_record(&config.store_dir.join("requesters.idx"), &key) > 0);
    assert!(ledger.demand(&record.id).is_err());
    assert!(super::engine::interest(&*ledger, &job, &config).is_err());
    assert!(super::engine::cancel_unwanted(&*ledger, &mut jobs, &config, true).is_err());
    let retained = jobs.get(&job.id).unwrap().unwrap();
    assert_eq!(retained.state, "queued");
    assert_eq!(retained.requester, job.requester);
    assert_eq!(jobs.list().unwrap().len(), 1);
}

#[test]
fn coowner_beyond_the_first_inventory_page_keeps_a_removed_sources_job_queued() {
    let directory = Directory::new();
    let config = config(&directory);
    let engine = Engine::open_for_management(config.clone()).unwrap();
    let mut candidates: Vec<_> = (0..301)
        .map(|number| demand(&config, "coowner", number))
        .collect();
    candidates.sort_by(|left, right| left.id.cmp(&right.id));
    let mut owner = candidates.last().unwrap().clone();
    let mut source = owner.clone();
    source.account_id = "source".into();
    source.id = Demand::identity("source", &source.request);
    source.state = "reserved".into();
    source.outcome = "reserved".into();
    source.admitted_at = Some(1234);
    owner.state = "reserved".into();
    owner.outcome = "reserved".into();
    owner.admitted_at = Some(1234);
    let mut ledger = lock(&engine.requester_store).unwrap();
    for batch in candidates.chunks(100) {
        let mut patch = ledger.state.clone();
        for record in batch {
            patch.demands.insert(record.id.clone(), record.clone());
        }
        ledger.save_patch(patch).unwrap();
    }
    let mut patch = ledger.state.clone();
    patch.demands.insert(source.id.clone(), source.clone());
    patch.demands.insert(owner.id.clone(), owner.clone());
    ledger.save_patch(patch).unwrap();
    let mut jobs = lock(&engine.store).unwrap();
    let job = jobs
        .submit_requester(source.request.clone(), provenance(&source))
        .unwrap();
    source.state = "removed".into();
    source.outcome = "removed".into();
    source.origins.clear();
    source.job_id = Some(job.id.clone());
    owner.state = "active".into();
    owner.outcome = "active".into();
    owner.job_id = Some(job.id.clone());
    let mut patch = ledger.state.clone();
    patch.demands.insert(source.id.clone(), source);
    patch.demands.insert(owner.id.clone(), owner.clone());
    ledger.save_patch(patch).unwrap();
    let (first_page, next) = ledger.demands_page(None, 200).unwrap();
    assert!(next.is_some());
    assert!(!first_page.iter().any(|record| record.id == owner.id));
    assert!(super::engine::interest(&*ledger, &job, &config).unwrap());
    super::engine::cancel_unwanted(&*ledger, &mut jobs, &config, true).unwrap();
    assert_eq!(jobs.get(&job.id).unwrap().unwrap().state, "queued");
}

/// Admits `count` demands with their requests, as requester sync records them.
fn admitted(engine: &Engine, config: &crate::config::Config, count: u64) -> Vec<Demand> {
    let mut ledger = lock(&engine.requester_store).unwrap();
    let mut records: Vec<Demand> = (0..count)
        .map(|number| {
            let mut record = demand(config, "source", 1_000 + number);
            record.state = "reserved".into();
            record.outcome = "reserved".into();
            record.admitted_at = Some(1234);
            record
        })
        .collect();
    let mut patch = ledger.state.clone();
    for record in &records {
        patch.demands.insert(record.id.clone(), record.clone());
    }
    ledger.save_patch(patch).unwrap();
    let mut jobs = lock(&engine.store).unwrap();
    let mut patch = ledger.state.clone();
    for record in &mut records {
        let job = jobs
            .submit_requester(record.request.clone(), provenance(record))
            .unwrap();
        record.state = "active".into();
        record.outcome = "active".into();
        record.job_id = Some(job.id.clone());
        patch.demands.insert(record.id.clone(), record.clone());
    }
    ledger.save_patch(patch).unwrap();
    records
}

fn startup_number(engine: &Engine, field: &str) -> u64 {
    engine
        .startup_status()
        .unwrap()
        .get(field)
        .and_then(crate::json::Value::as_str)
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn serving_startup_validates_in_bounded_steps_behind_a_closed_gate() {
    let directory = Directory::new();
    let config = config(&directory);
    let engine = Engine::open_for_management(config.clone()).unwrap();
    // More than two 200-row pages in every requester stage.
    let records = admitted(&engine, &config, 450);
    drop(engine);
    let engine = Engine::open_serving(config.clone()).unwrap();
    assert!(engine.starting.load(std::sync::atomic::Ordering::Acquire));
    // Reads are served; every mutation waits for validation.
    let (status, value) = crate::server::route(&engine, "GET", "/api/status", &[]).unwrap();
    assert_eq!(status, 200);
    let startup = value.get("startup").unwrap();
    assert_eq!(
        startup.get("state").and_then(crate::json::Value::as_str),
        Some("validating")
    );
    assert_eq!(
        crate::server::route(&engine, "POST", "/api/jobs", b"{}")
            .unwrap()
            .0,
        503
    );
    let mut steps = 0;
    loop {
        let rows = startup_number(&engine, "rows");
        let pages = startup_number(&engine, "pages");
        let done = engine.startup_step().unwrap();
        assert_eq!(startup_number(&engine, "pages"), pages + 1);
        assert!(startup_number(&engine, "rows") - rows <= 200);
        steps += 1;
        if done {
            break;
        }
        assert!(engine.starting.load(std::sync::atomic::Ordering::Acquire));
    }
    // Three pages for each requester stage, one per remaining stage, plus
    // the empty page that ends each paged stage.
    assert!(steps <= 3 * 4 + 13, "{steps} startup steps");
    assert!(startup_number(&engine, "rows") >= 450 * 3);
    assert!(!engine.starting.load(std::sync::atomic::Ordering::Acquire));
    assert_ne!(
        crate::server::route(&engine, "POST", "/api/jobs", b"{}").map_or(400, |(status, _)| status),
        503
    );
    // Wanted work survived the startup cancellation sweep.
    let mut jobs = lock(&engine.store).unwrap();
    for record in &records {
        let job = jobs.get(record.job_id.as_ref().unwrap()).unwrap().unwrap();
        assert_eq!(job.state, "queued");
    }
}

#[test]
fn failed_serving_startup_keeps_the_gate_shut_and_reports_the_error() {
    let directory = Directory::new();
    let config = config(&directory);
    let engine = Engine::open_for_management(config.clone()).unwrap();
    let records = admitted(&engine, &config, 3);
    drop(engine);
    // A damaged demand record must stop startup before any effect.
    let key = super::index::primary_cursor(&records[1].id).unwrap();
    assert!(crate::inventory::damage_record(&config.store_dir.join("requesters.idx"), &key) > 0);
    let error = Engine::open_for_management(config.clone()).err().unwrap();
    let engine = Engine::open_serving(config).unwrap();
    let workers = engine.start();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.startup_state().unwrap() == "validating" {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(engine.startup_state().unwrap(), "failed");
    assert!(engine.starting.load(std::sync::atomic::Ordering::Acquire));
    let status = engine.startup_status().unwrap();
    assert!(
        status
            .get("error")
            .and_then(crate::json::Value::as_str)
            .unwrap()
            .contains(&error)
    );
    assert_eq!(
        crate::server::route(&engine, "POST", "/api/jobs", b"{}")
            .unwrap()
            .0,
        503
    );
    assert!(engine.startup_step().is_err());
    drop(workers);
}

#[test]
fn serving_start_runs_workers_only_after_startup() {
    let directory = Directory::new();
    let config = config(&directory);
    let engine = Engine::open_for_management(config.clone()).unwrap();
    admitted(&engine, &config, 250);
    drop(engine);
    let engine = Engine::open_serving(config).unwrap();
    assert_eq!(engine.startup_state().unwrap(), "validating");
    let workers = engine.start();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.starting.load(std::sync::atomic::Ordering::Acquire) {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(engine.startup_state().unwrap(), "ready");
    drop(workers);
}
