//! CI-only multiple-account ownership, policy fencing and restart scenarios.
mod library_support;
mod requester_support;
mod series_support;
use library_support::Directory;
use mynou::{
    config,
    crypto::sha256,
    engine::{Engine, lock},
    json::{self, Value},
    selection::Profile,
};
use requester_support::*;
use std::{fs, path::Path, sync::atomic::Ordering, thread};

#[test]
fn account_configuration_is_bounded_strict_and_backwards_compatible() {
    let legacy = config::from_json(&config::default_json(), Path::new(".")).unwrap();
    assert!(legacy.requesters.accounts.is_empty());
    let account =
        json::parse(r#"{"id":"alice","expected_user_id":"101","token_env":"MYNOU_ALICE_TOKEN"}"#)
            .unwrap();
    let mut settings = Value::object();
    settings.insert("accounts", Value::Array(vec![account.clone()]));
    let mut value = config::default_json();
    value.insert("requesters", settings.clone());
    assert!(config::from_json(&value, Path::new(".")).is_ok());
    for key in ["token", "url", "policy"] {
        let mut a = account.clone();
        a.insert(key, "private credential");
        settings.insert("accounts", Value::Array(vec![a]));
        value.insert("requesters", settings.clone());
        assert!(config::from_json(&value, Path::new(".")).is_err());
    }
    settings.insert("accounts", Value::Array(vec![account.clone(), account]));
    value.insert("requesters", settings.clone());
    assert!(config::from_json(&value, Path::new(".")).is_err());
    for query in [
        "offset=0&limit=201",
        "offset=10001",
        "limit=0",
        "offset=-1",
        "limit=1&limit=2",
        "token=private",
    ] {
        assert!(mynou::requesters::page(query).is_err());
    }
    assert_eq!(mynou::requesters::page("offset=2&limit=3").unwrap(), (2, 3));
}
#[test]
fn new_accounts_require_explicit_opt_in_and_durable_approval_before_queueing() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let cfg = accounts.config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let report = engine.sync_requesters().unwrap();
    no_credentials(&report);
    assert!(lock(&engine.store).unwrap().list().is_empty());
    assert_eq!(
        demand(&engine, "alice").get("state").unwrap().as_str(),
        Some("pending")
    );
    enable(&engine, "alice", true);
    assert!(lock(&engine.store).unwrap().list().is_empty());
    let pending = demand(&engine, "alice");
    apply(&engine, "alice", demand_query("approve", id(&pending)));
    let queued = job(&engine, "alice");
    assert_eq!(queued.state, "queued");
    assert_eq!(queued.requester.as_ref().unwrap().account_id, "alice");
    let ledger = read_snapshot(&cfg.store_dir);
    assert!(
        ledger.get("demands").unwrap().as_array().unwrap()[0]
            .get("charged_at")
            .unwrap()
            .as_str()
            .is_some()
    );
    assert_eq!(
        &fs::read(cfg.store_dir.join("journal.bin")).unwrap()[..8],
        b"MYNOUJ04"
    );
    no_credentials(&ledger);
    drop(engine);
    let engine = Engine::open_for_management(cfg).unwrap();
    assert_eq!(job(&engine, "alice").requester, queued.requester);
    assert_eq!(
        engine
            .requester("alice", 0, 100)
            .unwrap()
            .get("daily")
            .unwrap()
            .as_u64(),
        Some(1)
    );
}
#[test]
fn compatible_accounts_share_one_acquisition_and_removals_retain_other_interest() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    for a in ["alice", "bob"] {
        accounts.watchlist(
            a,
            vec![movie(7, "Fixture Movie"), movie(7, "Fixture Movie")],
        );
    }
    let engine = Engine::open_for_management(accounts.config(&dir.0)).unwrap();
    for a in ["alice", "bob"] {
        enable(&engine, a, false);
    }
    engine.sync_requesters().unwrap();
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    assert_eq!(job(&engine, "alice").id, job(&engine, "bob").id);
    assert_eq!(demands(&engine, "alice").len(), 1);
    let first = demand(&engine, "alice");
    let acquired = job(&engine, "alice");
    apply(&engine, "alice", demand_query("remove", id(&first)));
    assert_eq!(
        lock(&engine.store)
            .unwrap()
            .get(&acquired.id)
            .unwrap()
            .state,
        "queued"
    );
    let second = demand(&engine, "bob");
    apply(&engine, "bob", demand_query("remove", id(&second)));
    assert_eq!(
        lock(&engine.store)
            .unwrap()
            .get(&acquired.id)
            .unwrap()
            .state,
        "cancelled"
    );
    assert!(engine.retry(&acquired.id).is_err());
    engine.sync_requesters().unwrap();
    assert_eq!(
        lock(&engine.store)
            .unwrap()
            .get(&acquired.id)
            .unwrap()
            .state,
        "cancelled"
    );
    assert_eq!(
        engine
            .requester("alice", 0, 100)
            .unwrap()
            .get("daily")
            .unwrap()
            .as_u64(),
        Some(1)
    );
}
#[test]
fn conflicting_profiles_and_destinations_do_not_merge_or_charge_unadmitted_demand() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    for a in ["alice", "bob"] {
        accounts.watchlist(a, vec![movie(7, "Fixture Movie")]);
    }
    let mut cfg = accounts.config(&dir.0);
    let profile = Profile {
        blocked_terms: vec!["blocked".into()],
        ..Profile::default()
    };
    cfg.selection.profiles.insert("strict".into(), profile);
    let engine = Engine::open_for_management(cfg).unwrap();
    enable(&engine, "alice", false);
    let mut p = policy(&engine, "bob");
    p.enabled = true;
    p.approval_required = false;
    p.movie_profile = "strict".into();
    p.destination = "family".into();
    apply(&engine, "bob", policy_query(p));
    engine.sync_requesters().unwrap();
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    let other = demand(&engine, "bob");
    assert_eq!(other.get("state").unwrap().as_str(), Some("conflict"));
    assert_eq!(other.get("charged_at"), Some(&Value::Null));
    let mut p = policy(&engine, "bob");
    p.movie_profile = "any".into();
    p.destination = "default".into();
    apply(&engine, "bob", policy_query(p));
    apply(&engine, "bob", demand_query("approve", id(&other)));
    assert_eq!(job(&engine, "alice").id, job(&engine, "bob").id);
}
#[test]
fn quota_guards_fence_parallel_approvals_and_retries_without_double_charging() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(1, "First"), movie(2, "Second")]);
    let engine = Engine::open_for_management(accounts.config(&dir.0)).unwrap();
    let mut p = policy(&engine, "alice");
    p.enabled = true;
    p.max_active = 1;
    p.max_daily = 1;
    apply(&engine, "alice", policy_query(p));
    engine.sync_requesters().unwrap();
    let rows = demands(&engine, "alice");
    let guards: Vec<_> = rows
        .iter()
        .map(|d| {
            let mut q = demand_query("approve", id(d));
            let v = engine.requester_control("alice", &q).unwrap();
            q.apply = true;
            q.plan_id = Some(v.get("plan_id").unwrap().as_str().unwrap().into());
            q
        })
        .collect();
    let handles: Vec<_> = guards
        .into_iter()
        .map(|q| {
            let e = engine.clone();
            thread::spawn(move || e.requester_control("alice", &q))
        })
        .collect();
    let passed = handles
        .into_iter()
        .map(|h| h.join().unwrap().is_ok())
        .filter(|ok| *ok)
        .count();
    assert_eq!(passed, 1);
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    let rows = demands(&engine, "alice");
    let pending = rows
        .iter()
        .find(|d| d.get("job_id") == Some(&Value::Null))
        .unwrap();
    apply(&engine, "alice", demand_query("approve", id(pending)));
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    let queued = lock(&engine.store).unwrap().list().remove(0);
    engine.cancel(&queued.id).unwrap();
    let selected = rows
        .iter()
        .find(|d| d.get("job_id").and_then(Value::as_str) == Some(&queued.id))
        .unwrap();
    apply(&engine, "alice", demand_query("retry", id(selected)));
    assert_eq!(
        lock(&engine.store).unwrap().get(&queued.id).unwrap().state,
        "queued"
    );
    assert_eq!(
        engine
            .requester("alice", 0, 100)
            .unwrap()
            .get("daily")
            .unwrap()
            .as_u64(),
        Some(1)
    );
}
#[test]
fn partial_failure_and_identity_mismatch_preserve_each_accounts_cursor_and_demand() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    for a in ["alice", "bob"] {
        accounts.watchlist(a, vec![movie(7, "Fixture Movie")]);
    }
    let engine = Engine::open_for_management(accounts.config(&dir.0)).unwrap();
    for a in ["alice", "bob"] {
        enable(&engine, a, false);
    }
    engine.sync_requesters().unwrap();
    accounts.response(
        "/alice/watchlist",
        503,
        json::parse(r#"{"error":"private bearer token"}"#).unwrap(),
    );
    accounts.watchlist("bob", vec![]);
    let result = engine.sync_requesters().unwrap();
    no_credentials(&result);
    assert_eq!(
        engine
            .requester("alice", 0, 100)
            .unwrap()
            .get("cursor")
            .unwrap()
            .as_str(),
        Some("1")
    );
    assert_eq!(
        engine
            .requester("bob", 0, 100)
            .unwrap()
            .get("cursor")
            .unwrap()
            .as_str(),
        Some("2")
    );
    assert_eq!(job(&engine, "alice").state, "queued");
    assert_eq!(
        demand(&engine, "bob").get("state").unwrap().as_str(),
        Some("removed")
    );
    accounts.response(
        "/alice/identity",
        200,
        json::parse(r#"{"id":999}"#).unwrap(),
    );
    engine.sync_requesters().unwrap();
    assert_eq!(
        engine
            .requester("alice", 0, 100)
            .unwrap()
            .get("last_error")
            .unwrap()
            .as_str(),
        Some("Plex account identity changed")
    );
    assert_eq!(job(&engine, "alice").state, "queued");
}
#[test]
fn policy_changes_during_poll_discard_the_result_before_acquisition() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let engine = Engine::open_for_management(accounts.config(&dir.0)).unwrap();
    enable(&engine, "alice", false);
    accounts.blocked.store(true, Ordering::Release);
    let old = accounts.calls.lock().unwrap().len();
    let e = engine.clone();
    let poll = thread::spawn(move || e.sync_requesters());
    accounts.wait(old);
    let mut p = policy(&engine, "alice");
    p.enabled = false;
    apply(&engine, "alice", policy_query(p));
    accounts.blocked.store(false, Ordering::Release);
    poll.join().unwrap().unwrap();
    assert!(lock(&engine.store).unwrap().list().is_empty());
    assert_eq!(
        engine
            .requester("alice", 0, 100)
            .unwrap()
            .get("cursor")
            .unwrap()
            .as_str(),
        Some("0")
    );
}
#[test]
fn pending_rejection_tombstones_and_imported_media_survive_policy_edits_and_restart() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist(
        "alice",
        vec![movie(7, "Fixture Movie"), movie(8, "Rejected Movie")],
    );
    let cfg = accounts.config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    enable(&engine, "alice", true);
    engine.sync_requesters().unwrap();
    let rows = demands(&engine, "alice");
    let chosen = &rows[0];
    let rejected = &rows[1];
    apply(&engine, "alice", demand_query("reject", id(rejected)));
    apply(&engine, "alice", demand_query("approve", id(chosen)));
    let job_id = demand_id_job(&engine, id(chosen));
    let imported = ready(&engine, &job_id);
    let mut p = policy(&engine, "alice");
    p.destination = "family".into();
    p.movie_profile = "any".into();
    apply(&engine, "alice", policy_query(p));
    apply(&engine, "alice", demand_query("remove", id(chosen)));
    let bytes = fs::read(&imported.imports[0]).unwrap();
    drop(engine);
    let engine = Engine::open_for_management(cfg).unwrap();
    engine.sync_requesters().unwrap();
    assert_eq!(
        lock(&engine.store).unwrap().get(&job_id).unwrap().state,
        "ready"
    );
    assert_eq!(fs::read(&imported.imports[0]).unwrap(), bytes);
    assert!(
        demands(&engine, "alice")
            .iter()
            .any(|d| d.get("state").unwrap().as_str() == Some("rejected"))
    );
}
fn demand_id_job(engine: &Engine, id: &str) -> String {
    demands(engine, "alice")
        .into_iter()
        .find(|d| self::id(d) == id)
        .unwrap()
        .get("job_id")
        .unwrap()
        .as_str()
        .unwrap()
        .into()
}
#[test]
fn notifications_respect_preferences_and_deduplicate_repeated_outcomes() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    for a in ["alice", "bob"] {
        accounts.watchlist(a, vec![movie(7, "Fixture Movie")]);
    }
    let engine = Engine::open_for_management(accounts.config(&dir.0)).unwrap();
    for (a, n) in [("alice", "none"), ("bob", "all")] {
        let mut p = policy(&engine, a);
        p.enabled = true;
        p.approval_required = false;
        p.notifications = n.into();
        apply(&engine, a, policy_query(p));
    }
    engine.sync_requesters().unwrap();
    let before = engine
        .requester("bob", 0, 100)
        .unwrap()
        .get("notifications")
        .unwrap()
        .clone();
    engine.sync_requesters().unwrap();
    assert_eq!(
        engine
            .requester("bob", 0, 100)
            .unwrap()
            .get("notifications"),
        Some(&before)
    );
    assert!(
        engine
            .requester("alice", 0, 100)
            .unwrap()
            .get("notifications")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    let acquired = job(&engine, "bob");
    ready(&engine, &acquired.id);
    engine.sync_requesters().unwrap();
    let outcomes = engine.requester("bob", 0, 100).unwrap();
    assert_eq!(
        outcomes
            .get("notifications")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n.get("outcome").and_then(Value::as_str) == Some("ready"))
            .count(),
        1
    );
    no_credentials(&outcomes);
}
#[test]
fn captured_destination_and_profile_cannot_be_rebound_through_job_updates() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let engine = Engine::open_for_management(accounts.config(&dir.0)).unwrap();
    let mut p = policy(&engine, "alice");
    p.enabled = true;
    p.approval_required = false;
    p.destination = "family".into();
    apply(&engine, "alice", policy_query(p));
    engine.sync_requesters().unwrap();
    let acquired = job(&engine, "alice");
    assert!(
        acquired
            .requester
            .as_ref()
            .unwrap()
            .capture
            .movies_root
            .ends_with("family/movies")
    );
    let mut changed = acquired.clone();
    changed.requester.as_mut().unwrap().capture.destination = "default".into();
    assert!(lock(&engine.store).unwrap().update(changed).is_err());
    let before = fs::read(engine.config.store_dir.join("journal.bin")).unwrap();
    let mut q = demand_query("remove", id(&demand(&engine, "alice")));
    let preview = engine.requester_control("alice", &q).unwrap();
    engine.cancel(&acquired.id).unwrap();
    q.apply = true;
    q.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    assert!(engine.requester_control("alice", &q).is_err());
    assert_ne!(
        fs::read(engine.config.store_dir.join("journal.bin")).unwrap(),
        before
    );
}
#[test]
fn reservation_recovery_is_idempotent_and_corruption_or_missing_provenance_fails_closed() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let cfg = accounts.config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    enable(&engine, "alice", false);
    engine.sync_requesters().unwrap();
    let acquired = job(&engine, "alice");
    drop(engine);
    let mut snapshot = read_snapshot(&cfg.store_dir);
    let Value::Array(rows) = snapshot.get_mut("demands").unwrap() else {
        panic!("demand array")
    };
    let d = &mut rows[0];
    d.insert("state", "reserved");
    d.insert("job_id", Value::Null);
    write_snapshot(&cfg.store_dir, &snapshot);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    engine.sync_requesters().unwrap();
    assert_eq!(job(&engine, "alice").id, acquired.id);
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    assert_eq!(
        engine
            .requester("alice", 0, 100)
            .unwrap()
            .get("daily")
            .unwrap()
            .as_u64(),
        Some(1)
    );
    drop(engine);
    let saved = fs::read(cfg.store_dir.join("requesters.bin")).unwrap();
    let mut corrupt = saved.clone();
    corrupt[20] ^= 1;
    fs::write(cfg.store_dir.join("requesters.bin"), corrupt).unwrap();
    assert!(Engine::open_for_management(cfg.clone()).is_err());
    fs::write(cfg.store_dir.join("requesters.bin"), &saved).unwrap();
    fs::remove_file(cfg.store_dir.join("requesters.bin")).unwrap();
    assert!(Engine::open_for_management(cfg.clone()).is_err());
    fs::write(cfg.store_dir.join("requesters.bin"), saved).unwrap();
    let mut frame = fs::read(cfg.store_dir.join("journal.bin")).unwrap();
    frame[..8].copy_from_slice(b"MYNOUJ01");
    let h = sha256(&frame[..52]);
    frame[52..84].copy_from_slice(&h);
    let len = u32::from_le_bytes(frame[16..20].try_into().unwrap()) as usize;
    let check = sha256(&frame[..84 + len]);
    frame[84 + len..116 + len].copy_from_slice(&check);
    fs::write(cfg.store_dir.join("journal.bin"), frame).unwrap();
    assert!(Engine::open_for_management(cfg).is_err());
}
#[test]
fn legacy_operator_interest_is_retained_when_the_last_requester_removes_demand() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let engine = Engine::open_for_management(accounts.config(&dir.0)).unwrap();
    enable(&engine, "alice", false);
    engine.sync_requesters().unwrap();
    let acquired = job(&engine, "alice");
    engine.submit(acquired.request.clone()).unwrap();
    apply(
        &engine,
        "alice",
        demand_query("remove", id(&demand(&engine, "alice"))),
    );
    assert_eq!(
        lock(&engine.store)
            .unwrap()
            .get(&acquired.id)
            .unwrap()
            .state,
        "queued"
    );
}
#[test]
fn polling_series_creates_canonical_approved_episode_demand_without_enabling_global_acquisition() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    let catalog = series_support::Catalog::open(vec![
        series_support::episode(1, 1, Some("2024-01-01"), "Aired"),
        series_support::episode(1, 2, Some("2200-01-01"), "Future"),
    ]);
    let mut cfg = accounts.config(&dir.0);
    let c = catalog.config(&dir.0);
    cfg.catalog = c.catalog;
    accounts.watchlist("alice", vec![show()]);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    enable(&engine, "alice", true);
    engine.sync_requesters().unwrap();
    assert_eq!(demands(&engine, "alice").len(), 1);
    assert!(lock(&engine.store).unwrap().list().is_empty());
    assert_eq!(
        engine.series().unwrap().as_array().unwrap()[0]
            .get("monitored")
            .unwrap()
            .as_bool(),
        Some(false)
    );
    let d = demand(&engine, "alice");
    apply(&engine, "alice", demand_query("approve", id(&d)));
    let acquired = job(&engine, "alice");
    assert_eq!((acquired.request.season, acquired.request.episode), (1, 1));
    drop(engine);
    let engine = Engine::open_for_management(cfg).unwrap();
    engine.sync_requesters().unwrap();
    assert_eq!(job(&engine, "alice").request, acquired.request);
    assert_eq!(demands(&engine, "alice").len(), 1);
}

#[test]
fn canonical_episode_demand_retains_approved_source_numbering_when_series_policy_changes() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    let catalog = series_support::Catalog::open(vec![series_support::episode(
        1,
        1,
        Some("2024-01-01"),
        "Aired",
    )]);
    let mut cfg = accounts.config(&dir.0);
    cfg.catalog = catalog.config(&dir.0).catalog;
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let tracked = engine
        .track_series_with_policy(&series_support::request(), false, false, false)
        .unwrap();
    let series_id = id(&tracked);
    let mut choices = mynou::series::NumberingRequest {
        changes: vec![mynou::series::NumberingChoice {
            catalog_id: 11001,
            catalog: mynou::numbering::EpisodeNumber {
                season: 1,
                episode: 1,
            },
            source: mynou::numbering::SourceNumber::Absolute(13),
        }],
        apply: false,
        plan_id: None,
    };
    let preview = engine.series_numbering(series_id, &choices).unwrap();
    choices.apply = true;
    choices.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    engine.series_numbering(series_id, &choices).unwrap();
    accounts.watchlist("alice", vec![show()]);
    enable(&engine, "alice", true);
    engine.sync_requesters().unwrap();
    let d = demand(&engine, "alice");
    choices.apply = false;
    choices.plan_id = None;
    choices.changes[0].source = mynou::numbering::SourceNumber::Absolute(14);
    let preview = engine.series_numbering(series_id, &choices).unwrap();
    choices.apply = true;
    choices.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    engine.series_numbering(series_id, &choices).unwrap();
    engine.sync_requesters().unwrap();
    apply(&engine, "alice", demand_query("approve", id(&d)));
    let acquired = job(&engine, "alice");
    assert_eq!(
        acquired.request.source_numbering,
        Some(mynou::numbering::SourceNumber::Absolute(13))
    );
    drop(engine);
    let engine = Engine::open_for_management(cfg).unwrap();
    engine.sync_requesters().unwrap();
    assert_eq!(job(&engine, "alice").request, acquired.request);
}

#[test]
fn daily_quota_rollover_releases_new_admissions_and_retains_old_charge_history() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(7, "First")]);
    let cfg = accounts.config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let mut p = policy(&engine, "alice");
    p.enabled = true;
    p.approval_required = false;
    p.max_daily = 1;
    apply(&engine, "alice", policy_query(p));
    engine.sync_requesters().unwrap();
    let acquired = job(&engine, "alice");
    ready(&engine, &acquired.id);
    accounts.watchlist("alice", vec![movie(7, "First"), movie(8, "Second")]);
    engine.sync_requesters().unwrap();
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    assert!(
        demands(&engine, "alice")
            .iter()
            .any(|d| d.get("state").and_then(Value::as_str) == Some("quota"))
    );
    drop(engine);
    let mut snapshot = read_snapshot(&cfg.store_dir);
    let Value::Array(rows) = snapshot.get_mut("demands").unwrap() else {
        panic!("demand array")
    };
    let charged = rows
        .iter_mut()
        .find(|d| d.get("charged_at").and_then(Value::as_str).is_some())
        .unwrap();
    charged.insert("charged_at", (mynou::store::now() - 86400).to_string());
    write_snapshot(&cfg.store_dir, &snapshot);
    let engine = Engine::open_for_management(cfg).unwrap();
    engine.sync_requesters().unwrap();
    assert_eq!(lock(&engine.store).unwrap().list().len(), 2);
    assert_eq!(
        engine
            .requester("alice", 0, 100)
            .unwrap()
            .get("daily")
            .unwrap()
            .as_u64(),
        Some(1)
    );
}
