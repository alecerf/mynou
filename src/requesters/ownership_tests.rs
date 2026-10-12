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
    let name: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
    let object = config
        .store_dir
        .join("requesters.idx/objects")
        .join(&name[..2])
        .join(&name[2..]);
    fs::remove_file(object).unwrap();
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
