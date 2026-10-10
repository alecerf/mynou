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
fn new_accounts_require_explicit_opt_in_before_queueing() {
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
    enable(&engine, "alice");
    let queued = job(&engine, "alice");
    assert_eq!(queued.state, "queued");
    assert_eq!(queued.requester.as_ref().unwrap().account_id, "alice");
    let ledger = read_snapshot(&cfg.store_dir);
    assert!(
        ledger.get("demands").unwrap().as_array().unwrap()[0]
            .get("admitted_at")
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
        demand(&engine, "alice").get("state").unwrap().as_str(),
        Some("active")
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
        enable(&engine, a);
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
    for a in ["alice", "bob"] {
        assert_eq!(
            demand(&engine, a).get("state").unwrap().as_str(),
            Some("removed")
        );
    }
}
#[test]
fn conflicting_profiles_and_destinations_do_not_merge_unadmitted_demand() {
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
    enable(&engine, "alice");
    let mut p = policy(&engine, "bob");
    p.enabled = true;
    p.movie_profile = "strict".into();
    p.destination = "family".into();
    apply(&engine, "bob", policy_query(p));
    engine.sync_requesters().unwrap();
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    let other = demand(&engine, "bob");
    assert_eq!(other.get("state").unwrap().as_str(), Some("conflict"));
    assert_eq!(other.get("admitted_at"), Some(&Value::Null));
    let mut p = policy(&engine, "bob");
    p.movie_profile = "any".into();
    p.destination = "default".into();
    apply(&engine, "bob", policy_query(p));
    assert_eq!(job(&engine, "alice").id, job(&engine, "bob").id);
}
#[test]
fn uncaptured_operator_jobs_require_verified_ready_imports_and_compatible_quality() {
    for case in [
        "pending",
        "foreign",
        "missing",
        "directory",
        "parent",
        "symlink_file",
        "symlink_parent",
        "valid",
        "restricted_without_baseline",
        "blocked_baseline",
        "accepted_baseline",
        "profile_mismatch",
    ] {
        let dir = Directory::new();
        let accounts = Accounts::open();
        accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
        let mut cfg = accounts.config(&dir.0);
        if matches!(
            case,
            "restricted_without_baseline" | "blocked_baseline" | "accepted_baseline"
        ) {
            cfg.selection.profiles.insert(
                "any".into(),
                Profile {
                    blocked_terms: vec!["blocked".into()],
                    ..Profile::default()
                },
            );
        }
        let engine = Engine::open_for_management(cfg.clone()).unwrap();
        let mut operator = lock(&engine.store)
            .unwrap()
            .submit(request(7, "Fixture Movie"))
            .unwrap();
        let root = &cfg.movies_root;
        fs::create_dir_all(root).unwrap();
        if case != "pending" {
            let path = match case {
                "foreign" => root.with_file_name("movies-sibling").join("fixture.mp4"),
                "parent" => root.join("../outside/fixture.mp4"),
                "symlink_parent" => root.join("link/fixture.mp4"),
                _ => root.join("fixture.mp4"),
            };
            match case {
                "missing" => {}
                "directory" => fs::create_dir_all(&path).unwrap(),
                "symlink_file" | "symlink_parent" => {
                    #[cfg(unix)]
                    {
                        let outside = dir.0.join("outside");
                        fs::create_dir_all(&outside).unwrap();
                        let source = outside.join("fixture.mp4");
                        fs::write(&source, include_bytes!("../examples/demo.mp4")).unwrap();
                        if case == "symlink_file" {
                            std::os::unix::fs::symlink(source, &path).unwrap();
                        } else {
                            std::os::unix::fs::symlink(outside, root.join("link")).unwrap();
                        }
                    }
                }
                _ => {
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(&path, include_bytes!("../examples/demo.mp4")).unwrap();
                }
            }
            operator.state = "ready".into();
            operator.progress = 1.0;
            operator.imports = vec![path.to_str().unwrap().into()];
            if matches!(
                case,
                "blocked_baseline" | "accepted_baseline" | "profile_mismatch"
            ) {
                operator.release = Some(mynou::store::RecordedRelease {
                    title: format!(
                        "Fixture.Movie.2024.1080p.BluRay.x264{}",
                        if case == "blocked_baseline" {
                            ".blocked"
                        } else {
                            ""
                        }
                    ),
                    profile: if case == "profile_mismatch" {
                        "other"
                    } else {
                        "any"
                    }
                    .into(),
                });
            }
            lock(&engine.store)
                .unwrap()
                .update(operator.clone())
                .unwrap();
            operator = lock(&engine.store).unwrap().get(&operator.id).unwrap();
        }
        let journal = fs::read(cfg.store_dir.join("journal.bin")).unwrap();
        enable(&engine, "alice");
        engine.sync_requesters().unwrap();
        let admitted = matches!(case, "valid" | "accepted_baseline");
        let row = demand(&engine, "alice");
        assert_eq!(
            row.get("state").unwrap().as_str(),
            Some(if admitted { "ready" } else { "conflict" }),
            "{case}"
        );
        assert_eq!(
            row.get("admitted_at") != Some(&Value::Null),
            admitted,
            "{case}"
        );
        if admitted {
            assert_eq!(
                row.get("job_id").unwrap().as_str(),
                Some(operator.id.as_str())
            );
        } else {
            assert_eq!(row.get("job_id"), Some(&Value::Null));
        }
        assert_eq!(
            lock(&engine.store).unwrap().list(),
            vec![operator.clone()],
            "{case}"
        );
        assert_eq!(
            fs::read(cfg.store_dir.join("journal.bin")).unwrap(),
            journal,
            "{case}"
        );
        drop(engine);
        let reopened = Engine::open_for_management(cfg).unwrap();
        assert_eq!(demand(&reopened, "alice"), row, "{case}");
        assert_eq!(
            lock(&reopened.store).unwrap().list(),
            vec![operator],
            "{case}"
        );
    }
}
#[test]
fn review_guards_fence_parallel_removals_and_retry_reuses_the_admitted_job() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(1, "First"), movie(2, "Second")]);
    let engine = Engine::open_for_management(accounts.config(&dir.0)).unwrap();
    enable(&engine, "alice");
    engine.sync_requesters().unwrap();
    assert_eq!(lock(&engine.store).unwrap().list().len(), 2);
    let rows = demands(&engine, "alice");
    let guards: Vec<_> = rows
        .iter()
        .map(|d| {
            let mut q = demand_query("remove", id(d));
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
    let rows = demands(&engine, "alice");
    let kept = rows
        .iter()
        .find(|d| d.get("state").and_then(Value::as_str) != Some("removed"))
        .unwrap();
    assert_eq!(
        rows.iter()
            .filter(|d| d.get("state").and_then(Value::as_str) == Some("removed"))
            .count(),
        1
    );
    let queued = kept.get("job_id").unwrap().as_str().unwrap().to_owned();
    engine.cancel(&queued).unwrap();
    apply(&engine, "alice", demand_query("retry", id(kept)));
    assert_eq!(
        lock(&engine.store).unwrap().get(&queued).unwrap().state,
        "queued"
    );
    assert_eq!(lock(&engine.store).unwrap().list().len(), 2);
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
        enable(&engine, a);
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
    enable(&engine, "alice");
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
fn removal_tombstones_and_imported_media_survive_policy_edits_and_restart() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist(
        "alice",
        vec![movie(7, "Fixture Movie"), movie(8, "Removed Movie")],
    );
    let cfg = accounts.config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    enable(&engine, "alice");
    engine.sync_requesters().unwrap();
    let rows = demands(&engine, "alice");
    let chosen = &rows[0];
    let removed = &rows[1];
    apply(&engine, "alice", demand_query("remove", id(removed)));
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
            .all(|d| d.get("state").unwrap().as_str() == Some("removed"))
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
fn captured_destination_and_profile_cannot_be_rebound_through_job_updates() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let engine = Engine::open_for_management(accounts.config(&dir.0)).unwrap();
    let mut p = policy(&engine, "alice");
    p.enabled = true;
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
    enable(&engine, "alice");
    engine.sync_requesters().unwrap();
    let acquired = job(&engine, "alice");
    let admitted = demand(&engine, "alice").get("admitted_at").cloned();
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
        demand(&engine, "alice").get("admitted_at").cloned(),
        admitted
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
    enable(&engine, "alice");
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
fn polling_series_creates_canonical_episode_demand_without_enabling_global_acquisition() {
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
    enable(&engine, "alice");
    let acquired = job(&engine, "alice");
    assert_eq!((acquired.request.season, acquired.request.episode), (1, 1));
    drop(engine);
    let engine = Engine::open_for_management(cfg).unwrap();
    engine.sync_requesters().unwrap();
    assert_eq!(job(&engine, "alice").request, acquired.request);
    assert_eq!(demands(&engine, "alice").len(), 1);
}

#[test]
fn canonical_episode_demand_retains_captured_source_numbering_when_series_policy_changes() {
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
    engine.sync_requesters().unwrap();
    choices.apply = false;
    choices.plan_id = None;
    choices.changes[0].source = mynou::numbering::SourceNumber::Absolute(14);
    let preview = engine.series_numbering(series_id, &choices).unwrap();
    choices.apply = true;
    choices.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    engine.series_numbering(series_id, &choices).unwrap();
    engine.sync_requesters().unwrap();
    enable(&engine, "alice");
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
