//! Original local catalog, guarded demand, crash and native import fixtures. CI only.
mod irc_routing_support;
mod irc_support;
mod library_support;
mod requester_support;
mod series_support;
#[allow(dead_code)]
mod transfer_support;
mod web_support;
use irc_routing_support::{job, no_private, read_checked, row_mut, write_checked};
use library_support::{Directory, run_until};
use mynou::{
    config,
    engine::{Engine, lock},
    irc::ControlRequest,
    json::{self, Value},
};
use requester_support::{
    Accounts, apply, demand, demand_query, demands, enable, movie, policy, policy_query,
};
use series_support::{Catalog, episode};
use std::{fs, process::Command, sync::atomic::Ordering, thread};
use transfer_support::{Seeder, Torrent};
use web_support::{Browser, Server, TOKEN};

fn catalog() -> Catalog {
    let c = Catalog::open(vec![episode(1, 1, Some("2024-01-01"), "Original Episode")]);
    for id in [7, 8] {
        c.response(&format!("/3/movie/{id}"), 200, details(id));
    }
    c
}
fn details(id: u32) -> Value {
    let mut v = Value::object();
    v.insert("id", id);
    v.insert("title", "Fixture Movie");
    v.insert("release_date", "2024-01-01");
    v
}
fn value(dir: &Directory, catalog: &Catalog, accounts: &Accounts, peer: u16) -> Value {
    let mut v = irc_support::value(&dir.0, "irc://127.0.0.1:1");
    let irc = v.get_mut("irc").unwrap();
    let Value::Array(sources) = irc.get_mut("sources").unwrap() else {
        panic!()
    };
    sources[0].insert(
        "magnet_template",
        format!("magnet:?xt={{xt}}&x.pe=127.0.0.1:{}", peer.max(1)),
    );
    let Value::Array(rules) = irc.get_mut("rules").unwrap() else {
        panic!()
    };
    rules[0].insert("action", "request");
    rules[0].insert("requester", "alice");
    let c = v.get_mut("catalog").unwrap();
    c.insert("enabled", true);
    c.insert("url", format!("{}/3", catalog.url));
    c.insert("token_env", "PATH");
    c.insert("api_key_env", "MYNOU_IRC_ADMISSION_ABSENT_KEY_94214");
    let mut r = Value::object();
    r.insert(
        "accounts",
        Value::Array(
            [("alice", "101", "PATH"), ("bob", "202", "PWD")]
                .into_iter()
                .map(|(name, id, env)| {
                    let mut a = Value::object();
                    a.insert("id", name);
                    a.insert("expected_user_id", id);
                    a.insert("token_env", env);
                    a.insert("identity_url", format!("{}/{name}/identity", accounts.url));
                    a.insert(
                        "watchlist_url",
                        format!("{}/{name}/watchlist", accounts.url),
                    );
                    a
                })
                .collect(),
        ),
    );
    v.insert("requesters", r);
    if peer != 0 {
        v.get_mut("downloads").unwrap().insert("enabled", true);
    }
    v
}
fn configured(dir: &Directory, c: &Catalog, a: &Accounts) -> config::Config {
    config::from_json(&value(dir, c, a, 0), &dir.0).unwrap()
}
fn request(engine: &Engine, row: &Value) -> Value {
    let id = irc_support::record_id(row);
    let q = irc_support::reviewed(engine, id, "request");
    engine.irc_control(id, &q).unwrap()
}
fn phase(engine: &Engine, row: &Value) -> String {
    engine
        .irc_announcement(irc_support::record_id(row))
        .unwrap()
        .get("admission")
        .unwrap()
        .get("phase")
        .unwrap()
        .as_str()
        .unwrap()
        .into()
}
fn pending(snapshot: &mut Value, id: &str) {
    let r = row_mut(snapshot, id);
    let a = r.get_mut("admission").unwrap();
    a.insert("phase", "prepared");
    a.insert("committed_at", Value::Null);
    r.insert("decision", "pending");
    r.insert("decided_at", Value::Null);
    r.insert("revision", "2");
}
fn post(server: &Server, route: &str, v: &Value) -> web_support::Reply {
    server.call(
        "POST",
        route,
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", "application/json"),
        ],
        &json::stringify(v),
    )
}
fn guard(body: &str) -> &str {
    body.split("name=\"plan_id\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
}

#[test]
fn request_rules_require_explicit_known_accounts_and_pinned_sources() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let original = value(&dir, &c, &a, 0);
    assert!(config::from_json(&original, &dir.0).is_ok());
    for selector in [Value::Null, "".into(), "retired".into(), "Alice".into()] {
        let mut v = original.clone();
        let Value::Array(r) = v.get_mut("irc").unwrap().get_mut("rules").unwrap() else {
            panic!()
        };
        r[0].insert("requester", selector);
        assert!(config::from_json(&v, &dir.0).is_err());
    }
    let mut v = original;
    let Value::Array(s) = v.get_mut("irc").unwrap().get_mut("sources").unwrap() else {
        panic!()
    };
    s[0].insert("magnet_template", Value::Null);
    assert!(config::from_json(&v, &dir.0).is_err());
    for invalid in [
        r#"{"action":"request","apply":true}"#,
        r#"{"action":"request","plan_id":"bad"}"#,
        r#"{"action":"request","account_id":"bob"}"#,
    ] {
        assert!(ControlRequest::from_json(&json::parse(invalid).unwrap()).is_err());
    }
}

#[test]
fn receipt_duplicates_and_catalog_previews_never_create_demand_or_mutate_storage() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let cfg = configured(&dir, &c, &a);
    let e = Engine::open(cfg.clone()).unwrap();
    enable(&e, "alice", false);
    let row = irc_support::receive(&e, 7);
    let before = irc_support::bytes(&cfg.store_dir);
    let preview = e
        .irc_control(irc_support::record_id(&row), &irc_support::query("request"))
        .unwrap();
    assert_eq!(preview.get("new_demand"), Some(&Value::Bool(true)));
    assert_eq!(
        preview.get("acquisition_started"),
        Some(&Value::Bool(false))
    );
    no_private(&preview);
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    assert!(demands(&e, "alice").is_empty());
    assert!(
        irc_support::receive(&e, 7)
            .get("duplicate")
            .unwrap()
            .as_bool()
            .unwrap()
    );
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    irc_support::no_jobs(&e);
}

#[test]
fn explicit_origins_survive_empty_watchlists_approval_and_removal_tombstones() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let cfg = configured(&dir, &c, &a);
    let e = Engine::open(cfg.clone()).unwrap();
    enable(&e, "alice", true);
    let row = irc_support::receive(&e, 7);
    let accepted = request(&e, &row);
    assert_eq!(accepted.get("applied"), Some(&Value::Bool(true)));
    assert_eq!(phase(&e, &row), "committed");
    assert_eq!(
        demand(&e, "alice").get("state").unwrap().as_str(),
        Some("pending")
    );
    irc_support::no_jobs(&e);
    e.sync_requesters().unwrap();
    let d = demand(&e, "alice");
    assert_eq!(d.get("state").unwrap().as_str(), Some("pending"));
    assert_eq!(d.get("charged_at"), Some(&Value::Null));
    apply(
        &e,
        "alice",
        demand_query("approve", requester_support::id(&d)),
    );
    let d = demand(&e, "alice");
    let job_id = d.get("job_id").unwrap().as_str().unwrap().to_owned();
    assert_eq!(job(&e, &job_id).requester.unwrap().account_id, "alice");
    assert_eq!(
        e.requester("alice", 0, 100)
            .unwrap()
            .get("daily")
            .unwrap()
            .as_u64(),
        Some(1)
    );
    apply(
        &e,
        "alice",
        demand_query("remove", requester_support::id(&d)),
    );
    let mut changed = irc_support::announcement(7);
    changed.insert("info_hash", "2234567890abcdef1234567890abcdef12345678");
    let new = e
        .irc_receive("local", irc_support::SENDER, "#announces", &changed)
        .unwrap();
    let before = irc_support::bytes(&cfg.store_dir);
    assert!(
        e.irc_control(irc_support::record_id(&new), &irc_support::query("request"))
            .is_err()
    );
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    drop(e);
    let e = Engine::open(cfg).unwrap();
    assert_eq!(phase(&e, &row), "committed");
    assert_eq!(
        demand(&e, "alice").get("state").unwrap().as_str(),
        Some("removed")
    );
    assert_eq!(lock(&e.store).unwrap().list().len(), 1);
}

#[test]
fn quota_reservations_are_shared_with_ordinary_requester_admission() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let e = Engine::open(configured(&dir, &c, &a)).unwrap();
    enable(&e, "alice", false);
    let mut p = policy(&e, "alice");
    p.max_active = 1;
    p.max_daily = 1;
    apply(&e, "alice", policy_query(p));
    request(&e, &irc_support::receive(&e, 7));
    let row = irc_support::receive(&e, 8);
    let accepted = request(&e, &row);
    let d = accepted.get("demand").unwrap();
    assert_eq!(d.get("state").unwrap().as_str(), Some("quota"));
    assert_eq!(d.get("job_id"), Some(&Value::Null));
    assert_eq!(lock(&e.store).unwrap().list().len(), 1);
    assert_eq!(
        e.requester("alice", 0, 100)
            .unwrap()
            .get("daily")
            .unwrap()
            .as_u64(),
        Some(1)
    );
    let before = irc_support::bytes(&e.config.store_dir);
    assert!(
        e.irc_control(irc_support::record_id(&row), &irc_support::query("request"))
            .is_err()
    );
    assert_eq!(irc_support::bytes(&e.config.store_dir), before);
}

#[test]
fn incorrect_missing_future_or_unavailable_catalog_facts_cannot_admit() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let e = Engine::open(configured(&dir, &c, &a)).unwrap();
    enable(&e, "alice", false);
    let row = irc_support::receive(&e, 7);
    let before = irc_support::bytes(&e.config.store_dir);
    for (key, v) in [
        ("id", Value::from(8_u32)),
        ("title", "Wrong title".into()),
        ("title", Value::Null),
        ("release_date", "2200-01-01".into()),
        ("release_date", "2023-01-01".into()),
        ("release_date", "2024-99-99".into()),
    ] {
        let mut d = details(7);
        d.insert(key, v);
        c.response("/3/movie/7", 200, d);
        assert_eq!(
            e.irc_control(irc_support::record_id(&row), &irc_support::query("request"))
                .unwrap_err(),
            "IRC: catalog identity could not be verified"
        );
        assert_eq!(irc_support::bytes(&e.config.store_dir), before);
    }
    c.response("/3/movie/7", 503, Value::from("private fixture failure"));
    assert_eq!(
        e.irc_control(irc_support::record_id(&row), &irc_support::query("request"))
            .unwrap_err(),
        "IRC: catalog identity could not be verified"
    );
    irc_support::no_jobs(&e);
    assert!(demands(&e, "alice").is_empty());
}

#[test]
fn stale_policy_job_scope_and_catalog_races_reject_before_intent() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let e = Engine::open(configured(&dir, &c, &a)).unwrap();
    enable(&e, "alice", false);
    let row = irc_support::receive(&e, 7);
    let id = irc_support::record_id(&row);
    let old = irc_support::reviewed(&e, id, "request");
    let mut p = policy(&e, "alice");
    p.max_daily += 1;
    apply(&e, "alice", policy_query(p));
    let before = irc_support::bytes(&e.config.store_dir);
    assert!(e.irc_control(id, &old).is_err());
    assert_eq!(irc_support::bytes(&e.config.store_dir), before);
    let old = irc_support::reviewed(&e, id, "request");
    e.submit(irc_routing_support::request(8)).unwrap();
    let before = irc_support::bytes(&e.config.store_dir);
    assert!(e.irc_control(id, &old).is_err());
    assert_eq!(irc_support::bytes(&e.config.store_dir), before);
    let calls = c.calls.load(Ordering::Acquire);
    c.blocked.store(true, Ordering::Release);
    let worker = {
        let e = e.clone();
        let id = id.to_owned();
        thread::spawn(move || e.irc_control(&id, &irc_support::query("request")))
    };
    c.wait_for_calls(calls);
    e.irc_control(id, &irc_support::reviewed(&e, id, "dismiss"))
        .unwrap();
    let before = irc_support::bytes(&e.config.store_dir);
    c.blocked.store(false, Ordering::Release);
    assert!(worker.join().unwrap().is_err());
    assert_eq!(irc_support::bytes(&e.config.store_dir), before);
    assert!(demands(&e, "alice").is_empty());
}

#[test]
fn concurrent_review_application_charges_one_canonical_demand_once() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let e = Engine::open(configured(&dir, &c, &a)).unwrap();
    enable(&e, "alice", false);
    let row = irc_support::receive(&e, 7);
    let q = irc_support::reviewed(&e, irc_support::record_id(&row), "request");
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let e = e.clone();
            let q = q.clone();
            let id = irc_support::record_id(&row).to_owned();
            thread::spawn(move || e.irc_control(&id, &q))
        })
        .collect();
    assert_eq!(
        workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .filter(Result::is_ok)
            .count(),
        1
    );
    assert_eq!(demands(&e, "alice").len(), 1);
    assert_eq!(lock(&e.store).unwrap().list().len(), 1);
    assert_eq!(
        e.requester("alice", 0, 100)
            .unwrap()
            .get("daily")
            .unwrap()
            .as_u64(),
        Some(1)
    );
}

#[test]
fn a_new_irc_coowner_shares_one_existing_captured_requester_job() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let mut cfg = configured(&dir, &c, &a);
    cfg.irc.rules[0].requester = Some("bob".into());
    let e = Engine::open(cfg.clone()).unwrap();
    enable(&e, "alice", false);
    enable(&e, "bob", false);
    a.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    e.sync_requesters().unwrap();
    let first = demand(&e, "alice")
        .get("job_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let row = irc_support::receive(&e, 7);
    let accepted = request(&e, &row);
    assert_eq!(
        accepted
            .get("demand")
            .unwrap()
            .get("job_id")
            .unwrap()
            .as_str(),
        Some(first.as_str())
    );
    assert_eq!(job(&e, &first).requester.unwrap().account_id, "alice");
    assert_eq!(lock(&e.store).unwrap().list().len(), 1);
    e.sync_requesters().unwrap();
    assert_eq!(
        demand(&e, "bob").get("state").unwrap().as_str(),
        Some("active")
    );
    for account in ["alice", "bob"] {
        assert_eq!(
            e.requester(account, 0, 100)
                .unwrap()
                .get("daily")
                .unwrap()
                .as_u64(),
            Some(1)
        );
    }
    drop(e);
    let e = Engine::open(cfg).unwrap();
    assert_eq!(phase(&e, &row), "committed");
    assert_eq!(
        demand(&e, "bob").get("job_id").unwrap().as_str(),
        Some(first.as_str())
    );
}

#[test]
fn unready_operator_work_cannot_replace_captured_requester_admission() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let e = Engine::open(configured(&dir, &c, &a)).unwrap();
    enable(&e, "alice", false);
    let operator = e.submit(irc_routing_support::request(7)).unwrap().remove(0);
    let accepted = request(&e, &irc_support::receive(&e, 7));
    assert_eq!(
        accepted
            .get("demand")
            .unwrap()
            .get("state")
            .unwrap()
            .as_str(),
        Some("conflict")
    );
    assert_eq!(demand(&e, "alice").get("charged_at"), Some(&Value::Null));
    assert_eq!(job(&e, &operator.id), operator);
    assert_eq!(lock(&e.store).unwrap().list().len(), 1);
}

#[test]
fn reviewed_new_demand_reaches_one_verified_native_import_and_survives_restart() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let torrent = Torrent::single(
        &dir.0.join("source"),
        "Fixture.Movie.2024.1080p.mp4",
        include_bytes!("../examples/demo.mp4").to_vec(),
    );
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let cfg = config::from_json(&value(&dir, &c, &a, seed.client.listen_port()), &dir.0).unwrap();
    let e = Engine::open(cfg.clone()).unwrap();
    enable(&e, "alice", false);
    let row = irc_routing_support::receive(&e, &torrent.id, 7);
    assert_eq!(
        e.irc_route_pending()
            .unwrap()
            .get("outcome")
            .unwrap()
            .as_str(),
        Some("idle")
    );
    irc_support::no_jobs(&e);
    assert!(e.transfers().unwrap().as_array().unwrap().is_empty());
    let accepted = request(&e, &row);
    let id = accepted
        .get("demand")
        .unwrap()
        .get("job_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!e.tick().unwrap());
    assert!(e.transfers().unwrap().as_array().unwrap().is_empty());
    assert_eq!(
        e.irc_route_pending().unwrap().get("routed"),
        Some(&Value::Bool(true))
    );
    let ready = run_until(&e, &id, "ready");
    assert_eq!(ready.imports.len(), 1);
    assert_eq!(
        fs::read(&ready.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
    assert!(ready.irc_origin.is_some());
    assert_eq!(ready.requester.as_ref().unwrap().account_id, "alice");
    e.sync_requesters().unwrap();
    assert_eq!(
        demand(&e, "alice").get("state").unwrap().as_str(),
        Some("ready")
    );
    let before = irc_support::bytes(&cfg.store_dir);
    assert!(
        irc_routing_support::receive(&e, &torrent.id, 7)
            .get("duplicate")
            .unwrap()
            .as_bool()
            .unwrap()
    );
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    drop(e);
    let e = Engine::open(cfg).unwrap();
    assert_eq!(job(&e, &id).irc_origin, ready.irc_origin);
    assert_eq!(phase(&e, &row), "committed");
    assert_eq!(lock(&e.store).unwrap().list().len(), 1);
}

#[test]
fn new_episode_demand_requires_aired_stable_catalog_facts_without_series_writes() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let mut cfg = configured(&dir, &c, &a);
    cfg.irc.rules[0].kind = "episode".into();
    let e = Engine::open(cfg.clone()).unwrap();
    enable(&e, "alice", false);
    let mut claim = irc_support::announcement(42);
    claim.insert("kind", "episode");
    claim.insert("media_title", "Fixture Series");
    claim.insert("season", 1_u32);
    claim.insert("episode", 1_u32);
    claim.insert("title", "Fixture.Series.2024.S01E01.1080p");
    let row = e
        .irc_receive("local", irc_support::SENDER, "#announces", &claim)
        .unwrap();
    let before = irc_support::bytes(&cfg.store_dir);
    c.episodes(1, vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    assert!(
        e.irc_control(irc_support::record_id(&row), &irc_support::query("request"))
            .is_err()
    );
    let mut unidentified = episode(1, 1, Some("2024-01-01"), "Missing ID");
    unidentified.insert("id", Value::Null);
    c.episodes(1, vec![unidentified]);
    assert!(
        e.irc_control(irc_support::record_id(&row), &irc_support::query("request"))
            .is_err()
    );
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    c.episodes(1, vec![episode(1, 1, Some("2024-01-01"), "Original")]);
    request(&e, &row);
    let d = demand(&e, "alice");
    assert_eq!(
        d.get("request").unwrap().get("season").unwrap().as_u64(),
        Some(1)
    );
    assert_eq!(
        d.get("request").unwrap().get("episode").unwrap().as_u64(),
        Some(1)
    );
    assert_eq!(e.series().unwrap().as_array().unwrap().len(), 0);
    drop(e);
    assert!(Engine::open(cfg).is_ok());
}

#[test]
fn retained_source_numbering_is_captured_without_rewriting_the_series_plan() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let mut cfg = configured(&dir, &c, &a);
    cfg.irc.rules[0].kind = "episode".into();
    let e = Engine::open(cfg).unwrap();
    enable(&e, "alice", false);
    let record = e
        .track_series_with_policy(&series_support::request(), false, false, false)
        .unwrap();
    let id = series_support::id(&record);
    let body = json::parse(r#"{"changes":[{"catalog_id":11001,"catalog":{"season":1,"episode":1},"source":{"absolute":13}}]}"#).unwrap();
    let mut q = mynou::series::NumberingRequest::from_json(&body).unwrap();
    let review = e.series_numbering(id, &q).unwrap();
    q.apply = true;
    q.plan_id = Some(review.get("plan_id").unwrap().as_str().unwrap().into());
    e.series_numbering(id, &q).unwrap();
    let original = e.series_record(id).unwrap();
    let mut claim = irc_support::announcement(42);
    claim.insert("kind", "episode");
    claim.insert("media_title", "Fixture Series");
    claim.insert("season", 1_u32);
    claim.insert("episode", 1_u32);
    claim.insert("title", "Fixture.Series.2024.13.1080p");
    let row = e
        .irc_receive("local", irc_support::SENDER, "#announces", &claim)
        .unwrap();
    let accepted = request(&e, &row);
    assert_eq!(
        accepted
            .get("canonical_request")
            .unwrap()
            .get("source_numbering")
            .unwrap()
            .get("absolute")
            .unwrap()
            .as_u64(),
        Some(13)
    );
    assert_eq!(e.series_record(id).unwrap(), original);
    let id = demand(&e, "alice")
        .get("job_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        job(&e, &id).request.source_numbering,
        Some(mynou::numbering::SourceNumber::Absolute(13))
    );
}

#[test]
fn committed_and_uncommitted_crash_intents_recover_without_replay_or_extra_charge() {
    for durable in [true, false] {
        let dir = Directory::new();
        let c = catalog();
        let a = Accounts::open();
        let cfg = configured(&dir, &c, &a);
        let e = Engine::open(cfg.clone()).unwrap();
        enable(&e, "alice", true);
        let row = irc_support::receive(&e, 7);
        request(&e, &row);
        let id = irc_support::record_id(&row);
        drop(e);
        let path = cfg.store_dir.join("announcements.bin");
        let mut history = read_checked(&path);
        pending(&mut history, id);
        write_checked(&path, b"MYNOUI03", &history);
        if !durable {
            let path = cfg.store_dir.join("requesters.bin");
            let mut ledger = read_checked(&path);
            ledger.insert("demands", Value::Array(Vec::new()));
            write_checked(&path, b"MYNOUR02", &ledger);
        }
        let before = irc_support::bytes(&cfg.store_dir);
        let preview = Engine::open_for_preview(cfg.clone()).unwrap();
        assert_eq!(phase(&preview, &row), "prepared");
        drop(preview);
        assert_eq!(irc_support::bytes(&cfg.store_dir), before);
        let e = Engine::open(cfg.clone()).unwrap();
        assert_eq!(
            phase(&e, &row),
            if durable { "committed" } else { "aborted" }
        );
        irc_support::no_jobs(&e);
        assert_eq!(demands(&e, "alice").len(), usize::from(durable));
        if durable {
            assert_eq!(demand(&e, "alice").get("charged_at"), Some(&Value::Null));
        }
        e.sync_requesters().unwrap();
        irc_support::no_jobs(&e);
        drop(e);
        assert!(Engine::open(cfg).is_ok());
    }
}

#[test]
fn missing_cross_store_provenance_and_semantic_format_downgrades_fail_closed() {
    for corruption in [
        "irc_magic",
        "requester_magic",
        "missing_record",
        "account_binding",
        "missing_origin",
    ] {
        let dir = Directory::new();
        let c = catalog();
        let a = Accounts::open();
        let cfg = configured(&dir, &c, &a);
        let e = Engine::open(cfg.clone()).unwrap();
        enable(&e, "alice", true);
        let row = irc_support::receive(&e, 7);
        request(&e, &row);
        drop(e);
        let hp = cfg.store_dir.join("announcements.bin");
        let rp = cfg.store_dir.join("requesters.bin");
        let mut h = read_checked(&hp);
        let mut r = read_checked(&rp);
        match corruption {
            "irc_magic" => write_checked(&hp, b"MYNOUI02", &h),
            "requester_magic" => write_checked(&rp, b"MYNOUR01", &r),
            "missing_record" => {
                h.insert("records", Value::Array(Vec::new()));
                write_checked(&hp, b"MYNOUI03", &h);
            }
            "account_binding" => {
                row_mut(&mut h, irc_support::record_id(&row))
                    .get_mut("admission")
                    .unwrap()
                    .insert("account_binding", "b".repeat(64));
                write_checked(&hp, b"MYNOUI03", &h);
            }
            "missing_origin" => {
                let Value::Array(ds) = r.get_mut("demands").unwrap() else {
                    panic!()
                };
                ds[0].insert("origins", Value::Array(vec!["retained-plex-origin".into()]));
                write_checked(&rp, b"MYNOUR02", &r);
            }
            _ => unreachable!(),
        }
        let before = irc_support::bytes(&cfg.store_dir);
        assert!(
            Engine::open_for_preview(cfg.clone()).is_err(),
            "{corruption}"
        );
        assert!(Engine::open(cfg.clone()).is_err(), "{corruption}");
        assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    }
}

#[test]
fn requester_identity_mismatch_or_failure_cannot_borrow_configured_authority() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let e = Engine::open(configured(&dir, &c, &a)).unwrap();
    enable(&e, "alice", false);
    let row = irc_support::receive(&e, 7);
    let before = irc_support::bytes(&e.config.store_dir);
    let calls = c.calls.load(Ordering::Acquire);
    for (status, body) in [
        (200, json::parse(r#"{"id":202}"#).unwrap()),
        (503, Value::Null),
    ] {
        a.response("/alice/identity", status, body);
        assert_eq!(
            e.irc_control(irc_support::record_id(&row), &irc_support::query("request"))
                .unwrap_err(),
            "IRC: requester identity could not be verified"
        );
        assert_eq!(irc_support::bytes(&e.config.store_dir), before);
    }
    assert_eq!(c.calls.load(Ordering::Acquire), calls);
    irc_support::no_jobs(&e);
    assert!(demands(&e, "alice").is_empty());
}

#[test]
fn protected_api_browser_and_cli_reviews_bind_the_request_action_and_session() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let server = Server::open(configured(&dir, &c, &a));
    enable(&server.engine, "alice", false);
    let row = irc_support::receive(&server.engine, 7);
    let id = irc_support::record_id(&row);
    let route = format!("/api/irc/announcements/{id}/control");
    let before = irc_support::bytes(&server.engine.config.store_dir);
    assert_eq!(
        server
            .call("POST", &route, &[], r#"{"action":"request"}"#)
            .status,
        401
    );
    let response = post(&server, &route, &irc_support::query("request").to_json());
    assert_eq!(response.status, 200, "{}", response.body);
    response.no_secrets();
    no_private(&json::parse(&response.body).unwrap());
    let first = Browser::login(&server);
    let second = Browser::login(&server);
    assert!(
        first
            .get(&server, &format!("/ui/irc/{id}"))
            .body
            .contains("Review requester demand")
    );
    let review = first.post(
        &server,
        "/ui/irc/control",
        &[("id", id), ("action", "request")],
    );
    assert_eq!(review.status, 200, "{}", review.body);
    assert!(review.body.contains("Confirmed media") && review.body.contains("alice"));
    let plan = guard(&review.body);
    assert_eq!(
        second
            .post(
                &server,
                "/ui/irc/control",
                &[
                    ("id", id),
                    ("action", "request"),
                    ("apply", "yes"),
                    ("plan_id", plan)
                ]
            )
            .status,
        400
    );
    let mut v = value(&dir, &c, &a, 0);
    v.insert("listen", server.authority.clone());
    let path = dir.0.join("mynou.json");
    fs::write(&path, json::stringify(&v)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .env("MYNOU_API_TOKEN", TOKEN)
        .args(["irc-control", id, "--action", "request", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    no_private(&json::parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap());
    assert_eq!(irc_support::bytes(&server.engine.config.store_dir), before);
    assert_eq!(
        first
            .post(
                &server,
                "/ui/irc/control",
                &[
                    ("id", id),
                    ("action", "request"),
                    ("apply", "yes"),
                    ("plan_id", plan)
                ]
            )
            .status,
        303
    );
    assert_eq!(
        first
            .post(
                &server,
                "/ui/irc/control",
                &[
                    ("id", id),
                    ("action", "request"),
                    ("apply", "yes"),
                    ("plan_id", plan)
                ]
            )
            .status,
        400
    );
    assert_eq!(lock(&server.engine.store).unwrap().list().len(), 1);
}

#[test]
fn offline_cli_catalog_review_is_read_only_and_application_requires_the_service() {
    let dir = Directory::new();
    let c = catalog();
    let a = Accounts::open();
    let cfg = configured(&dir, &c, &a);
    let e = Engine::open(cfg.clone()).unwrap();
    enable(&e, "alice", false);
    let row = irc_support::receive(&e, 7);
    let path = dir.0.join("mynou.json");
    fs::write(&path, json::stringify(&value(&dir, &c, &a, 0))).unwrap();
    drop(e);
    let before = irc_support::bytes(&cfg.store_dir);
    let output = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args([
            "irc-control",
            irc_support::record_id(&row),
            "--action",
            "request",
            "--config",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json::parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    no_private(&report);
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    let output = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args([
            "irc-control",
            irc_support::record_id(&row),
            "--action",
            "request",
            "--apply",
            "--plan-id",
        ])
        .arg(report.get("plan_id").unwrap().as_str().unwrap())
        .arg("--config")
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
}
