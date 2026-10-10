//! Original local requester-selective IRC routing and immutable demand fixtures.
mod api_support;
mod irc_routing_support;
mod irc_support;
mod library_support;
mod requester_support;
#[allow(dead_code)]
mod transfer_support;
use api_support::{Server, TOKEN};
use irc_routing_support::*;
use library_support::{Directory, run_until};
use mynou::{
    config,
    engine::{Engine, lock},
    irc::Settings,
    json::{self, Value},
};
use requester_support::{
    Accounts, apply, demand, demand_query, enable, movie, policy, policy_query,
};
use std::{fs, sync::atomic::Ordering, thread};
use transfer_support::{RecordingProxy, Seeder, Torrent};

fn rule(v: &mut Value) -> &mut Value {
    let Value::Array(r) = v.get_mut("irc").unwrap().get_mut("rules").unwrap() else {
        panic!()
    };
    &mut r[0]
}
fn configured_json(dir: &Directory) -> Value {
    let mut v = irc_support::value(&dir.0, "irc://127.0.0.1:1");
    v.insert("requesters", json::parse(r#"{"accounts":[{"id":"alice","expected_user_id":"101","token_env":"MYNOU_TEST_ALICE"}]}"#).unwrap());
    v
}
fn selected(dir: &Directory, accounts: &Accounts, port: u16, id: &str) -> config::Config {
    let mut cfg = configuration(&dir.0.join("engine"), port);
    cfg.requesters = accounts.config(&dir.0.join("engine")).requesters;
    cfg.irc.rules[0].requester = Some(id.into());
    cfg
}
fn outcome(v: &Value) -> &str {
    v.get("outcome").unwrap().as_str().unwrap()
}
fn fixture(dir: &Directory) -> Torrent {
    Torrent::single(
        &dir.0.join("source"),
        "Fixture.Movie.2024.1080p.mp4",
        include_bytes!("../examples/demo.mp4").to_vec(),
    )
}
fn shared(engine: &Engine, accounts: &Accounts) -> String {
    enable(engine, "alice");
    enable(engine, "bob");
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    engine.sync_requesters().unwrap();
    let id = demand(engine, "alice")
        .get("job_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(job(engine, &id).requester.unwrap().account_id, "alice");
    accounts.watchlist("bob", vec![movie(7, "Fixture Movie")]);
    engine.sync_requesters().unwrap();
    assert_eq!(
        demand(engine, "bob").get("job_id").unwrap().as_str(),
        Some(id.as_str())
    );
    id
}

#[test]
fn selectors_are_strict_configured_aliases_and_null_retains_original_rule_serialization() {
    let dir = Directory::new();
    let mut v = configured_json(&dir);
    let original = config::from_json(&v, &dir.0).unwrap();
    rule(&mut v).insert("requester", Value::Null);
    assert_eq!(
        config::from_json(&v, &dir.0).unwrap().irc.rules[0].to_json(),
        original.irc.rules[0].to_json()
    );
    for selector in [
        Value::from(""),
        Value::from("Alice"),
        Value::from("alice bob"),
        Value::from("retired"),
        Value::from("a".repeat(65)),
        Value::from(7_u32),
    ] {
        rule(&mut v).insert("requester", selector);
        assert!(config::from_json(&v, &dir.0).is_err());
    }
    rule(&mut v).insert("requester", "alice");
    let cfg = config::from_json(&v, &dir.0).unwrap();
    assert_eq!(cfg.irc.rules[0].requester.as_deref(), Some("alice"));
    assert_eq!(
        cfg.irc.rules[0]
            .to_json()
            .get("requester")
            .unwrap()
            .as_str(),
        Some("alice")
    );
    assert!(!dir.0.join("jobs").exists());
}

#[test]
fn an_explicit_default_selector_preserves_pending_legacy_reviews_and_storage() {
    let dir = Directory::new();
    let mut v = configured_json(&dir);
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let e = Engine::open(cfg.clone()).unwrap();
    let row = irc_support::receive(&e, 7);
    let query = irc_support::reviewed(&e, row_id(&row), "acknowledge");
    let before = irc_support::bytes(&cfg.store_dir);
    drop(e);
    rule(&mut v).insert("requester", Value::Null);
    let e = Engine::open(config::from_json(&v, &dir.0).unwrap()).unwrap();
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    assert_eq!(
        e.irc_control(row_id(&row), &irc_support::query("acknowledge"))
            .unwrap()
            .get("plan_id")
            .unwrap()
            .as_str(),
        query.plan_id.as_deref()
    );
    irc_support::no_jobs(&e);
}

#[test]
fn selected_grabs_cannot_substitute_an_operator_or_another_unadmitted_account() {
    for mode in ["operator", "missing", "pending"] {
        let dir = Directory::new();
        let accounts = Accounts::open();
        let cfg = selected(&dir, &accounts, 1, "bob");
        let e = Engine::open(cfg.clone()).unwrap();
        let id = if mode == "operator" {
            e.submit(request(7)).unwrap().remove(0).id
        } else {
            enable(&e, "alice");
            accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
            if mode == "pending" {
                accounts.watchlist("bob", vec![movie(7, "Fixture Movie")]);
            }
            e.sync_requesters().unwrap();
            demand(&e, "alice")
                .get("job_id")
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        };
        receive(&e, "1234567890abcdef1234567890abcdef12345678", 7);
        let before = irc_support::bytes(&cfg.store_dir);
        assert_eq!(
            outcome(&e.irc_route_pending().unwrap()),
            "requester_mismatch",
            "{mode}"
        );
        assert_eq!(irc_support::bytes(&cfg.store_dir), before);
        no_candidate_work(&e, &id);
        if mode == "pending" {
            assert_eq!(demand(&e, "bob").get("admitted_at"), Some(&Value::Null));
        }
    }
}

#[test]
fn a_compatible_selected_coowner_routes_one_native_import_under_the_creators_capture() {
    let dir = Directory::new();
    let torrent = fixture(&dir);
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let accounts = Accounts::open();
    let cfg = selected(&dir, &accounts, proxy.port, "bob");
    let e = Engine::open(cfg.clone()).unwrap();
    let id = shared(&e, &accounts);
    assert!(!e.tick().unwrap());
    no_candidate_work(&e, &id);
    let row = receive(&e, &torrent.id, 7);
    let report = e.irc_route_pending().unwrap();
    assert_eq!(outcome(&report), "routed");
    assert!(proxy.requests.lock().unwrap().is_empty());
    let origin = job(&e, &id).irc_origin.unwrap();
    assert_eq!(origin.announcement_id, row_id(&row));
    proxy.payloads_enabled.store(true, Ordering::Release);
    let ready = run_until(&e, &id, "ready");
    assert_eq!(ready.requester.as_ref().unwrap().account_id, "alice");
    assert_eq!(ready.imports.len(), 1);
    assert_eq!(
        fs::read(&ready.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
    let before = irc_support::bytes(&cfg.store_dir);
    assert!(
        receive(&e, &torrent.id, 7)
            .get("duplicate")
            .unwrap()
            .as_bool()
            .unwrap()
    );
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    drop(e);
    let e = Engine::open(cfg).unwrap();
    assert_eq!(job(&e, &id).irc_origin, Some(origin));
    assert_eq!(lock(&e.store).unwrap().list().len(), 1);
}

#[test]
fn incompatible_selected_routes_do_not_borrow_another_accounts_admission() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    let cfg = selected(&dir, &accounts, 1, "bob");
    let e = Engine::open(cfg.clone()).unwrap();
    enable(&e, "alice");
    enable(&e, "bob");
    let mut changed = policy(&e, "bob");
    changed.destination = "family".into();
    apply(&e, "bob", policy_query(changed));
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    e.sync_requesters().unwrap();
    let id = demand(&e, "alice")
        .get("job_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    accounts.watchlist("bob", vec![movie(7, "Fixture Movie")]);
    e.sync_requesters().unwrap();
    assert_eq!(
        demand(&e, "bob").get("state").unwrap().as_str(),
        Some("conflict")
    );
    receive(&e, "1234567890abcdef1234567890abcdef12345678", 7);
    let before = irc_support::bytes(&cfg.store_dir);
    assert_eq!(
        outcome(&e.irc_route_pending().unwrap()),
        "requester_mismatch"
    );
    assert_eq!(demand(&e, "bob").get("admitted_at"), Some(&Value::Null));
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    no_candidate_work(&e, &id);
}

#[test]
fn removing_the_selected_coowner_during_metadata_rechecks_before_reservation() {
    let dir = Directory::new();
    let torrent = fixture(&dir);
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let gate = MetadataGate::open(seed.client.listen_port());
    let accounts = Accounts::open();
    let cfg = selected(&dir, &accounts, gate.port, "bob");
    let e = Engine::open(cfg).unwrap();
    let id = shared(&e, &accounts);
    let row = receive(&e, &torrent.id, 7);
    let worker = {
        let engine = e.clone();
        thread::spawn(move || engine.irc_route_pending().unwrap())
    };
    irc_support::wait(|| gate.accepted.load(Ordering::Acquire) == 1);
    let d = demand(&e, "bob");
    apply(&e, "bob", demand_query("remove", requester_support::id(&d)));
    gate.released.store(true, Ordering::Release);
    assert_eq!(outcome(&worker.join().unwrap()), "requester_mismatch");
    assert_eq!(
        route_phase(&e.irc_announcement(row_id(&row)).unwrap()),
        None
    );
    assert_ne!(job(&e, &id).state, "cancelled");
    no_candidate_work(&e, &id);
}

#[test]
fn selector_edits_invalidate_pending_grabs_and_review_guards_without_reinterpreting_claims() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    let mut cfg = selected(&dir, &accounts, 1, "alice");
    let e = Engine::open(cfg.clone()).unwrap();
    let id = shared(&e, &accounts);
    let row = receive(&e, "1234567890abcdef1234567890abcdef12345678", 7);
    let review = irc_support::reviewed(&e, row_id(&row), "acknowledge");
    let original = e.irc_announcement(row_id(&row)).unwrap();
    let before = irc_support::bytes(&cfg.store_dir);
    drop(e);
    cfg.irc.rules[0].requester = Some("bob".into());
    let e = Engine::open(cfg.clone()).unwrap();
    assert_eq!(outcome(&e.irc_route_pending().unwrap()), "idle");
    assert!(e.irc_control(row_id(&row), &review).is_err());
    assert_eq!(e.irc_announcement(row_id(&row)).unwrap(), original);
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    no_candidate_work(&e, &id);
}

#[test]
fn protected_source_reports_and_rules_show_only_requester_aliases() {
    let dir = Directory::new();
    let mut v = configured_json(&dir);
    rule(&mut v).insert("requester", "alice");
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let server = Server::open(cfg.clone());
    let e = &server.engine;
    assert_eq!(server.call("GET", "/api/irc", &[], "").status, 401);
    let report = server.call(
        "GET",
        "/api/irc",
        &[("Authorization", &format!("Bearer {TOKEN}"))],
        "",
    );
    assert_eq!(report.status, 200);
    let public = json::parse(&report.body).unwrap();
    assert_eq!(
        public.get("rules").unwrap().as_array().unwrap()[0]
            .get("requester")
            .unwrap()
            .as_str(),
        Some("alice")
    );
    no_private(&public);
    irc_support::no_jobs(e);
    let mut s = irc_support::settings("irc://127.0.0.1:1");
    let Value::Array(r) = s.get_mut("rules").unwrap() else {
        panic!()
    };
    r[0].insert("requester", "alice");
    assert!(Settings::from_json(&s, &cfg.selection).is_ok());
}
