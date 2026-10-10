//! CI-only migration of requester snapshots written before approvals and quotas
//! were removed. Fixtures are synthetic and use loopback peers.
mod library_support;
mod requester_support;
mod series_support;
use library_support::Directory;
use mynou::{
    engine::{Engine, lock},
    json::Value,
};
use requester_support::*;
use std::path::Path;

fn rows(snapshot: &mut Value) -> &mut Vec<Value> {
    let Value::Array(rows) = snapshot.get_mut("demands").unwrap() else {
        panic!("demand array")
    };
    rows
}
fn title(d: &Value) -> String {
    d.get("request")
        .unwrap()
        .get("title")
        .unwrap()
        .as_str()
        .unwrap()
        .into()
}
fn find(engine: &Engine, name: &str) -> Value {
    demands(engine, "alice")
        .into_iter()
        .find(|d| title(d) == name)
        .unwrap()
}
fn state(engine: &Engine, name: &str) -> String {
    find(engine, name)
        .get("state")
        .unwrap()
        .as_str()
        .unwrap()
        .into()
}
/// Rewrites the current snapshot into the pre-removal shape. `shape` maps a
/// demand title to its old `(state, approved, outcome)`.
fn make_legacy(dir: &Path, shape: &[(&str, &str, bool, &str)]) {
    let mut snapshot = read_snapshot(dir);
    let Value::Array(accounts) = snapshot.get_mut("accounts").unwrap() else {
        panic!("account array")
    };
    for a in accounts {
        let p = a.get_mut("policy").unwrap();
        p.insert("approval_required", true);
        p.insert("max_active", 8_u32);
        p.insert("max_daily", 32_u32);
    }
    for d in rows(&mut snapshot) {
        let name = title(d);
        let charged = d.get("admitted_at").cloned().unwrap_or(Value::Null);
        if let Value::Object(m) = d {
            m.remove("admitted_at");
        }
        let (_, state, approved, outcome) = shape
            .iter()
            .find(|s| s.0 == name)
            .copied()
            .unwrap_or((name.as_str(), "", true, ""));
        if !state.is_empty() {
            d.insert("state", state);
            d.insert("outcome", outcome);
        }
        d.insert("approved", approved);
        d.insert("charged_at", charged);
    }
    write_snapshot(dir, &snapshot);
}

#[test]
fn unapproved_quota_and_rejected_demand_loads_held_or_removed_and_never_admits_itself() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist(
        "alice",
        vec![
            movie(7, "Pending Movie"),
            movie(8, "Quota Movie"),
            movie(9, "Rejected Movie"),
            movie(10, "Removed Movie"),
        ],
    );
    let cfg = accounts.config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    engine.sync_requesters().unwrap();
    drop(engine);
    make_legacy(
        &cfg.store_dir,
        &[
            ("Pending Movie", "pending", false, "pending"),
            ("Quota Movie", "quota", true, "quota"),
            ("Rejected Movie", "rejected", false, "rejected"),
            ("Removed Movie", "removed", false, "removed"),
        ],
    );
    // The old operator had enabled the account; that is not approval of old demand.
    let mut legacy = read_snapshot(&cfg.store_dir);
    let Value::Array(a) = legacy.get_mut("accounts").unwrap() else {
        panic!("account array")
    };
    a[0].get_mut("policy").unwrap().insert("enabled", true);
    write_snapshot(&cfg.store_dir, &legacy);

    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    engine.sync_requesters().unwrap();
    assert!(lock(&engine.store).unwrap().list().is_empty());
    assert_eq!(demands(&engine, "alice").len(), 4);
    assert_eq!(state(&engine, "Pending Movie"), "held");
    assert_eq!(state(&engine, "Quota Movie"), "held");
    assert_eq!(state(&engine, "Rejected Movie"), "removed");
    assert_eq!(state(&engine, "Removed Movie"), "removed");
    drop(engine);

    // The migrated snapshot is current-format and re-migration is a no-op.
    let migrated = read_snapshot(&cfg.store_dir);
    let text = mynou::json::stringify(&migrated);
    for key in [
        "approved",
        "charged_at",
        "approval_required",
        "max_active",
        "max_daily",
    ] {
        assert!(!text.contains(&format!("\"{key}\"")), "{key}");
    }
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    engine.sync_requesters().unwrap();
    assert!(lock(&engine.store).unwrap().list().is_empty());
    assert_eq!(state(&engine, "Pending Movie"), "held");
    drop(engine);
    assert_eq!(
        mynou::json::stringify(&read_snapshot(&cfg.store_dir)),
        mynou::json::stringify(&migrated)
    );

    // Held demand needs an explicit reviewed decision.
    let engine = Engine::open_for_management(cfg).unwrap();
    let pending = find(&engine, "Pending Movie");
    apply(&engine, "alice", demand_query("admit", id(&pending)));
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    assert_ne!(state(&engine, "Pending Movie"), "held");
    let quota = find(&engine, "Quota Movie");
    apply(&engine, "alice", demand_query("remove", id(&quota)));
    assert_eq!(state(&engine, "Quota Movie"), "removed");
    let rejected = find(&engine, "Rejected Movie");
    assert!(
        engine
            .requester_control("alice", &demand_query("admit", id(&rejected)))
            .is_err()
    );
    engine.sync_requesters().unwrap();
    assert_eq!(demands(&engine, "alice").len(), 4);
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
}

#[test]
fn held_admission_requires_an_enabled_account() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(7, "Pending Movie")]);
    let cfg = accounts.config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    engine.sync_requesters().unwrap();
    drop(engine);
    make_legacy(
        &cfg.store_dir,
        &[("Pending Movie", "pending", false, "pending")],
    );
    let engine = Engine::open_for_management(cfg).unwrap();
    let held = find(&engine, "Pending Movie");
    assert_eq!(state(&engine, "Pending Movie"), "held");
    assert!(
        engine
            .requester_control("alice", &demand_query("admit", id(&held)))
            .is_err()
    );
    assert!(
        engine
            .requester_control("alice", &demand_query("approve", id(&held)))
            .is_err()
    );
}

#[test]
fn admitted_demand_keeps_its_job_ready_media_and_admission_time() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let cfg = accounts.config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    enable(&engine, "alice");
    engine.sync_requesters().unwrap();
    let acquired = job(&engine, "alice");
    let finished = ready(&engine, &acquired.id);
    engine.sync_requesters().unwrap();
    let before = find(&engine, "Fixture Movie");
    drop(engine);
    make_legacy(&cfg.store_dir, &[]);
    let engine = Engine::open_for_management(cfg).unwrap();
    engine.sync_requesters().unwrap();
    let after = find(&engine, "Fixture Movie");
    assert_eq!(after.get("state"), before.get("state"));
    assert_eq!(after.get("job_id"), before.get("job_id"));
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    assert_eq!(
        lock(&engine.store)
            .unwrap()
            .get(&acquired.id)
            .unwrap()
            .imports,
        finished.imports
    );
    assert!(
        read_snapshot(&engine.config.store_dir)
            .get("demands")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("admitted_at")
            .unwrap()
            .as_str()
            .is_some()
    );
}

#[test]
fn corrupt_or_unknown_legacy_records_are_still_rejected() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let cfg = accounts.config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    engine.sync_requesters().unwrap();
    drop(engine);
    make_legacy(
        &cfg.store_dir,
        &[("Fixture Movie", "pending", false, "pending")],
    );
    let good = read_snapshot(&cfg.store_dir);
    type Edit = fn(&mut Value);
    let cases: [(&str, Edit); 8] = [
        ("unknown demand field", |s| {
            rows(s)[0].insert("unexpected", true)
        }),
        ("partial legacy policy", |s| {
            let Value::Array(a) = s.get_mut("accounts").unwrap() else {
                panic!()
            };
            if let Value::Object(m) = a[0].get_mut("policy").unwrap() {
                m.remove("max_daily");
            }
        }),
        ("zero legacy limit", |s| {
            let Value::Array(a) = s.get_mut("accounts").unwrap() else {
                panic!()
            };
            a[0].get_mut("policy").unwrap().insert("max_active", 0_u32);
        }),
        ("mixed demand fields", |s| {
            rows(s)[0].insert("admitted_at", Value::Null)
        }),
        ("unapproved reserved", |s| {
            rows(s)[0].insert("state", "reserved")
        }),
        ("unknown state", |s| rows(s)[0].insert("state", "approved")),
        ("bad charge", |s| rows(s)[0].insert("charged_at", "later")),
        ("unadmitted with job", |s| {
            rows(s)[0].insert("job_id", "a".repeat(32))
        }),
    ];
    for (name, edit) in cases {
        let mut bad = good.clone();
        edit(&mut bad);
        write_snapshot(&cfg.store_dir, &bad);
        assert!(Engine::open_for_management(cfg.clone()).is_err(), "{name}");
    }
    write_snapshot(&cfg.store_dir, &good);
    assert!(Engine::open_for_management(cfg).is_ok());
}
