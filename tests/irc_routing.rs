//! CI-only IRC candidate journeys, metadata gates, ownership and checked recovery.
mod irc_routing_support;
mod irc_support;
mod library_support;
mod requester_support;
#[allow(dead_code)]
mod transfer_support;
use irc_routing_support::*;
use library_support::{Directory, run_until};
use mynou::{
    engine::{Engine, lock, public_job},
    irc::Settings,
    json::Value,
    numbering::{EpisodeNumber, SourceNumber},
    selection::Profile,
};
use std::{fs, path::PathBuf, sync::atomic::Ordering, thread};
use transfer_support::{RecordingProxy, Seeder, Torrent};

fn movie(dir: &Directory, name: &str) -> Torrent {
    Torrent::single(
        &dir.0.join("source"),
        name,
        include_bytes!("../examples/demo.mp4").to_vec(),
    )
}
fn routing(report: &Value) -> bool {
    report.get("routed").and_then(Value::as_bool) == Some(true)
}
fn outcome(report: &Value) -> &str {
    report.get("outcome").unwrap().as_str().unwrap()
}

#[test]
fn strict_templates_are_pure_bounded_and_grabs_require_explicit_discovery() {
    let dir = Directory::new();
    let mut settings = irc_support::settings("irc://127.0.0.1:1");
    let Value::Array(rules) = settings.get_mut("rules").unwrap() else {
        panic!()
    };
    rules[0].insert("action", "grab");
    assert!(Settings::from_json(&settings, &Default::default()).is_err());
    for template in [
        "https://127.0.0.1/metadata/{xt}",
        "magnet:?xt={xt}",
        "magnet:?xt={xt}&xs=http://127.0.0.1/a",
        "magnet:?xt={xt}&xt={xt}&x.pe=127.0.0.1:1",
        "magnet:?xt={xt}&x.pe=localhost:1",
        "magnet:?xt={xt}&x.pe=127.0.0.1:0",
        "magnet:?xt={xt}&x.pe=0.0.0.0:1",
        "magnet:?xt={xt}&x.pe=127.0.0.1:1&x.pe=127.0.0.1:1",
        "magnet:?xt={xt}&tr=http%3A%2F%2Ftracker.invalid%2Fa",
        "magnet:?xt={xt}&tr=https%3A%2F%2Fuser%3Apass%40tracker.invalid%2Fa",
        "magnet:?xt={xt}&x.pe=127.0.0.1:1&tr=%ZZ",
        "magnet:?xt={xt}&x.pe=127.0.0.1:1&dn={name}",
    ] {
        let Value::Array(sources) = settings.get_mut("sources").unwrap() else {
            panic!()
        };
        sources[0].insert("magnet_template", template);
        assert!(Settings::from_json(&settings, &Default::default()).is_err());
    }
    let Value::Array(sources) = settings.get_mut("sources").unwrap() else {
        panic!()
    };
    sources[0].insert("magnet_template", "magnet:?xt={xt}&x.pe=[::1]:1");
    let parsed = Settings::from_json(&settings, &Default::default()).unwrap();
    assert_eq!(parsed.rules[0].action, "grab");
    assert_eq!(
        parsed.sources[0]
            .public_json()
            .get("tls_required")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert!(!dir.0.join("jobs").exists());
}

#[test]
fn admitted_jobs_wait_for_irc_and_metadata_precedes_selective_payload_and_exact_import() {
    let dir = Directory::new();
    let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let mut cfg = configuration(&dir.0.join("engine"), proxy.port);
    cfg.irc.sources[0].magnet_template = Some(format!(
        "magnet:?xt={{xt}}&x.pe=127.0.0.1:{}&tr=http%3A%2F%2F127.0.0.1%3A1%2Fa%3Fpasskey%3Dirc-route-private-fixture",
        proxy.port
    ));
    let engine = Engine::open(cfg.clone()).unwrap();
    let admitted = engine.submit(request(7)).unwrap().remove(0);
    assert!(!engine.tick().unwrap());
    no_candidate_work(&engine, &admitted.id);
    let row = receive(&engine, &torrent.id, 7);
    let before = fs::read(cfg.store_dir.join("journal.bin")).unwrap();
    let report = engine.irc_route_pending().unwrap();
    assert!(routing(&report), "{report:?}");
    assert_ne!(before, fs::read(cfg.store_dir.join("journal.bin")).unwrap());
    assert!(proxy.requests.lock().unwrap().is_empty());
    assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
    let routed = job(&engine, &admitted.id);
    let origin = routed.irc_origin.clone().unwrap();
    assert_eq!(origin.torrent_id, torrent.id);
    assert_eq!(origin.file, "Fixture.Movie.2024.1080p.mp4");
    assert_eq!(
        origin.file_length,
        include_bytes!("../examples/demo.mp4").len() as u64
    );
    assert_eq!(
        route_phase(&engine.irc_announcement(row_id(&row)).unwrap()),
        Some("routed")
    );
    for v in [
        public_job(&routed),
        engine.irc_sources().unwrap(),
        engine.irc_announcement(row_id(&row)).unwrap(),
        report,
    ] {
        no_private(&v);
    }
    proxy.payloads_enabled.store(true, Ordering::Release);
    let ready = run_until(&engine, &admitted.id, "ready");
    assert_eq!(ready.irc_origin, Some(origin));
    assert_eq!(ready.files.len(), 1);
    assert_eq!(ready.imports.len(), 1);
    assert_eq!(
        fs::read(&ready.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
    let before = irc_support::bytes(&cfg.store_dir);
    assert!(
        receive(&engine, &torrent.id, 7)
            .get("duplicate")
            .unwrap()
            .as_bool()
            .unwrap()
    );
    assert_eq!(outcome(&engine.irc_route_pending().unwrap()), "idle");
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    lock(&engine.store).unwrap().compact().unwrap();
    assert_eq!(
        &fs::read(cfg.store_dir.join("snapshot.bin")).unwrap()[..8],
        b"MYNOUS05"
    );
    assert_eq!(
        &fs::read(cfg.store_dir.join("announcements.bin")).unwrap()[..8],
        b"MYNOUI02"
    );
    drop(engine);
    let engine = Engine::open(cfg).unwrap();
    assert_eq!(job(&engine, &admitted.id), ready);
}

#[test]
fn unrequested_claims_never_create_demand_and_review_rules_never_hold_jobs() {
    let dir = Directory::new();
    let mut cfg = configuration(&dir.0, 1);
    let engine = Engine::open(cfg.clone()).unwrap();
    receive(&engine, "1".repeat(40).as_str(), 7);
    let before = irc_support::bytes(&cfg.store_dir);
    assert_eq!(
        outcome(&engine.irc_route_pending().unwrap()),
        "waiting_for_admitted_job"
    );
    assert_eq!(before, irc_support::bytes(&cfg.store_dir));
    assert!(lock(&engine.store).unwrap().list().is_empty());
    assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
    drop(engine);
    cfg.irc.rules[0].action = "review".into();
    let engine = Engine::open(cfg).unwrap();
    let admitted = engine.submit(request(7)).unwrap().remove(0);
    assert!(engine.tick().unwrap());
    assert_eq!(job(&engine, &admitted.id).state, "failed");
    assert!(job(&engine, &admitted.id).irc_origin.is_none());
}

#[test]
fn first_claim_cannot_be_rewritten_into_a_grab_and_wrong_titles_or_years_stop_before_metadata() {
    for (changed, expected_evaluation, expected_outcome) in [
        ("title", "profile_rejected", "idle"),
        ("year", "matched", "claim_mismatch"),
        ("media_title", "profile_rejected", "idle"),
    ] {
        let dir = Directory::new();
        let cfg = configuration(&dir.0, 1);
        let engine = Engine::open(cfg.clone()).unwrap();
        let admitted = engine.submit(request(7)).unwrap().remove(0);
        let mut claim = irc_support::announcement(7);
        match changed {
            "title" => claim.insert("title", "Other.Movie.2024.1080p"),
            "year" => claim.insert("year", 2023_u32),
            _ => claim.insert("media_title", "Other Movie"),
        }
        let row = engine
            .irc_receive("local", irc_support::SENDER, "#announces", &claim)
            .unwrap();
        assert_eq!(
            row.get("evaluations").unwrap().as_array().unwrap()[0]
                .get("outcome")
                .unwrap()
                .as_str(),
            Some(expected_evaluation)
        );
        assert_eq!(
            row.get("outcome").unwrap().as_str(),
            Some(if changed == "year" {
                "matched"
            } else {
                "unmatched"
            })
        );
        let before = irc_support::bytes(&cfg.store_dir);
        assert_eq!(
            outcome(&engine.irc_route_pending().unwrap()),
            expected_outcome
        );
        let duplicate = engine
            .irc_receive(
                "local",
                irc_support::SENDER,
                "#announces",
                &irc_support::announcement(7),
            )
            .unwrap();
        assert!(duplicate.get("claim_changed").unwrap().as_bool().unwrap());
        assert_eq!(
            route_phase(&engine.irc_announcement(row_id(&row)).unwrap()),
            None
        );
        assert_eq!(irc_support::bytes(&cfg.store_dir), before);
        no_candidate_work(&engine, &admitted.id);
    }
}

#[test]
fn conflicting_rules_and_reviewed_decisions_never_route() {
    for mode in ["conflict", "dismiss", "acknowledge"] {
        let dir = Directory::new();
        let mut cfg = configuration(&dir.0, 1);
        if mode == "conflict" {
            let mut second = cfg.irc.rules[0].clone();
            second.id = "second".into();
            cfg.irc.rules.push(second);
        }
        let engine = Engine::open(cfg.clone()).unwrap();
        let admitted = engine.submit(request(7)).unwrap().remove(0);
        let row = receive(&engine, &"1".repeat(40), 7);
        if mode != "conflict" {
            let q = irc_support::reviewed(&engine, row_id(&row), mode);
            engine.irc_control(row_id(&row), &q).unwrap();
        }
        let before = irc_support::bytes(&cfg.store_dir);
        assert_eq!(outcome(&engine.irc_route_pending().unwrap()), "idle");
        assert_eq!(before, irc_support::bytes(&cfg.store_dir));
        no_candidate_work(&engine, &admitted.id);
    }
}

#[test]
fn ambiguous_or_wrong_metadata_never_reserves_or_requests_payload() {
    for names in [
        vec![
            "Fixture.Movie.2024.1080p.mp4",
            "Fixture.Movie.2024.720p.mp4",
        ],
        vec!["Other.Movie.2024.1080p.mp4"],
        vec!["Fixture.Movie.2023.1080p.mp4"],
    ] {
        let dir = Directory::new();
        let torrent = Torrent::multiple(
            &dir.0.join("source"),
            "Release",
            names
                .into_iter()
                .map(|n| {
                    (
                        PathBuf::from(n),
                        include_bytes!("../examples/demo.mp4").to_vec(),
                    )
                })
                .collect(),
        );
        let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
        let proxy = RecordingProxy::open(seed.client.listen_port());
        let cfg = configuration(&dir.0.join("engine"), proxy.port);
        let engine = Engine::open(cfg.clone()).unwrap();
        let admitted = engine.submit(request(7)).unwrap().remove(0);
        let row = receive(&engine, &torrent.id, 7);
        let before = irc_support::bytes(&cfg.store_dir);
        assert_eq!(
            outcome(&engine.irc_route_pending().unwrap()),
            "metadata_rejected"
        );
        assert!(proxy.requests.lock().unwrap().is_empty());
        assert_eq!(before, irc_support::bytes(&cfg.store_dir));
        assert_eq!(
            route_phase(&engine.irc_announcement(row_id(&row)).unwrap()),
            None
        );
        no_candidate_work(&engine, &admitted.id);
    }
}

#[test]
fn concurrent_automatic_passes_route_once_and_cancellation_retry_retains_the_verified_origin() {
    let dir = Directory::new();
    let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let cfg = configuration(&dir.0.join("engine"), seed.client.listen_port());
    let engine = Engine::open(cfg.clone()).unwrap();
    let admitted = engine.submit(request(7)).unwrap().remove(0);
    receive(&engine, &torrent.id, 7);
    let passes: Vec<_> = (0..4)
        .map(|_| {
            let e = engine.clone();
            thread::spawn(move || e.irc_route_pending().unwrap())
        })
        .collect();
    assert_eq!(
        passes
            .into_iter()
            .map(|p| p.join().unwrap())
            .filter(routing)
            .count(),
        1
    );
    let origin = job(&engine, &admitted.id).irc_origin;
    engine.cancel(&admitted.id).unwrap();
    let retried = engine.retry(&admitted.id).unwrap();
    assert_eq!(retried.irc_origin, origin);
    assert!(retried.acquisition_url.is_some() && retried.release.is_some());
    drop(engine);
    let mut changed = cfg;
    changed.irc.sources[0].enabled = false;
    changed
        .selection
        .profiles
        .get_mut("any")
        .unwrap()
        .blocked_terms = vec!["fixture".into()];
    let engine = Engine::open(changed).unwrap();
    let ready = run_until(&engine, &admitted.id, "ready");
    assert_eq!(ready.irc_origin, origin);
}

#[test]
fn cancellation_or_review_during_metadata_inspection_fences_the_admission() {
    for action in ["cancel", "dismiss", "stop"] {
        let dir = Directory::new();
        let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
        let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
        let gate = MetadataGate::open(seed.client.listen_port());
        let cfg = configuration(&dir.0.join("engine"), gate.port);
        let engine = Engine::open(cfg.clone()).unwrap();
        let admitted = engine.submit(request(7)).unwrap().remove(0);
        let row = receive(&engine, &torrent.id, 7);
        let worker = {
            let e = engine.clone();
            thread::spawn(move || e.irc_route_pending().unwrap())
        };
        gate.wait();
        if action == "cancel" {
            engine.cancel(&admitted.id).unwrap();
        } else if action == "stop" {
            engine.stopped.store(true, Ordering::Release);
        } else {
            let q = irc_support::reviewed(&engine, row_id(&row), "dismiss");
            engine.irc_control(row_id(&row), &q).unwrap();
        }
        gate.released.store(true, Ordering::Release);
        assert!(!routing(&worker.join().unwrap()));
        assert_eq!(
            route_phase(&engine.irc_announcement(row_id(&row)).unwrap()),
            None
        );
        no_candidate_work(&engine, &admitted.id);
    }
}

#[test]
fn requester_approval_quotas_and_removal_are_never_bypassed_by_irc() {
    let dir = Directory::new();
    let accounts = requester_support::Accounts::open();
    let mut cfg = accounts.config(&dir.0);
    let routing_config = configuration(&dir.0, 1);
    cfg.irc = routing_config.irc;
    cfg.downloads_enabled = true;
    accounts.watchlist(
        "alice",
        vec![
            requester_support::movie(7, "Fixture Movie"),
            requester_support::movie(8, "Fixture Movie"),
        ],
    );
    let engine = Engine::open(cfg.clone()).unwrap();
    let mut policy = requester_support::policy(&engine, "alice");
    policy.enabled = true;
    policy.max_daily = 1;
    policy.max_active = 1;
    requester_support::apply(&engine, "alice", requester_support::policy_query(policy));
    engine.sync_requesters().unwrap();
    receive(&engine, &"1".repeat(40), 7);
    receive(&engine, &"2".repeat(40), 8);
    assert!(!routing(&engine.irc_route_pending().unwrap()));
    assert!(lock(&engine.store).unwrap().list().is_empty());
    for d in requester_support::demands(&engine, "alice") {
        requester_support::apply(
            &engine,
            "alice",
            requester_support::demand_query("approve", requester_support::id(&d)),
        );
    }
    let jobs = lock(&engine.store).unwrap().list();
    assert_eq!(jobs.len(), 1);
    assert!(
        requester_support::demands(&engine, "alice")
            .iter()
            .any(|d| d.get("state").and_then(Value::as_str) == Some("quota"))
    );
    assert!(!engine.tick().unwrap());
    accounts.watchlist("alice", Vec::new());
    engine.sync_requesters().unwrap();
    assert_eq!(job(&engine, &jobs[0].id).state, "cancelled");
    assert!(!routing(&engine.irc_route_pending().unwrap()));
    assert_eq!(
        engine
            .requester("alice", 0, 20)
            .unwrap()
            .get("daily")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
}

#[test]
fn edited_templates_or_profiles_do_not_rebind_old_claims() {
    for edit in ["template", "profile", "disabled"] {
        let dir = Directory::new();
        let cfg = configuration(&dir.0, 1);
        let engine = Engine::open(cfg.clone()).unwrap();
        let admitted = engine.submit(request(7)).unwrap().remove(0);
        let row = receive(&engine, &"1".repeat(40), 7);
        drop(engine);
        let mut changed = cfg.clone();
        match edit {
            "template" => {
                changed.irc.sources[0].magnet_template =
                    Some("magnet:?xt={xt}&x.pe=127.0.0.1:2".into())
            }
            "profile" => {
                changed.selection.profiles.insert(
                    "any".into(),
                    Profile {
                        blocked_terms: vec!["fixture".into()],
                        ..Profile::default()
                    },
                );
            }
            _ => changed.irc.sources[0].enabled = false,
        }
        let engine = Engine::open(changed).unwrap();
        let before = irc_support::bytes(&cfg.store_dir);
        assert_eq!(outcome(&engine.irc_route_pending().unwrap()), "idle");
        assert_eq!(
            route_phase(&engine.irc_announcement(row_id(&row)).unwrap()),
            None
        );
        assert_eq!(before, irc_support::bytes(&cfg.store_dir));
        no_candidate_work(&engine, &admitted.id);
    }
}

#[test]
fn source_numbering_is_exact_and_canonical_imports_keep_the_original_episode() {
    for source in [
        SourceNumber::Absolute(13),
        SourceNumber::SeasonEpisode(EpisodeNumber {
            season: 2,
            episode: 3,
        }),
    ] {
        let dir = Directory::new();
        let label = match source {
            SourceNumber::Absolute(_) => "Fixture.Series.2024.13.1080p",
            _ => "Fixture.Series.2024.S02E03.1080p",
        };
        let torrent = movie(&dir, &format!("{label}.mp4"));
        let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
        let mut cfg = configuration(&dir.0.join("engine"), seed.client.listen_port());
        cfg.irc.rules[0].kind = "episode".into();
        let engine = Engine::open(cfg).unwrap();
        let mut request = request(42);
        request.kind = "episode".into();
        request.title = "Fixture Series".into();
        request.season = 1;
        request.episode = 1;
        request.source_numbering = Some(source);
        let admitted = engine.submit(request).unwrap().remove(0);
        let mut a = irc_support::announcement(42);
        a.insert("kind", "episode");
        a.insert("media_title", "Fixture Series");
        a.insert("season", 1_u32);
        a.insert("episode", 1_u32);
        a.insert("title", label);
        a.insert("info_hash", torrent.id.clone());
        engine
            .irc_receive("local", irc_support::SENDER, "#announces", &a)
            .unwrap();
        assert!(routing(&engine.irc_route_pending().unwrap()));
        let ready = run_until(&engine, &admitted.id, "ready");
        assert_eq!(ready.request.source_numbering, Some(source));
        assert!(ready.imports[0].contains("S01E01"));
        assert_eq!(ready.request.season, 1);
        assert_eq!(ready.request.episode, 1);
    }
}

#[test]
fn live_irc_receivers_and_background_routing_complete_an_existing_request() {
    let dir = Directory::new();
    let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let (listener, url) = irc_support::listener();
    let mut cfg = configuration(&dir.0.join("engine"), seed.client.listen_port());
    cfg.irc.sources[0].url = url;
    let engine = Engine::open(cfg).unwrap();
    let admitted = engine.submit(request(7)).unwrap().remove(0);
    let receiver = thread::spawn(move || {
        let mut stream = irc_support::accept(&listener);
        irc_support::registration(&mut stream, false);
        let mut a = irc_support::announcement(7);
        a.insert("info_hash", torrent.id);
        irc_support::announce(&mut stream, irc_support::SENDER, "#announces", &a);
        irc_support::closed(&mut stream);
    });
    let workers = engine.start();
    irc_support::wait(|| job(&engine, &admitted.id).state == "ready");
    assert!(job(&engine, &admitted.id).irc_origin.is_some());
    drop(workers);
    receiver.join().unwrap();
}

#[test]
fn interrupted_reservations_recover_without_replaying_an_uncommitted_grab() {
    for committed in [true, false] {
        let dir = Directory::new();
        let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
        let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
        let cfg = configuration(&dir.0.join("engine"), seed.client.listen_port());
        let engine = Engine::open(cfg.clone()).unwrap();
        let admitted = engine.submit(request(7)).unwrap().remove(0);
        let row = receive(&engine, &torrent.id, 7);
        assert!(routing(&engine.irc_route_pending().unwrap()));
        lock(&engine.store).unwrap().compact().unwrap();
        drop(engine);
        let history_path = cfg.store_dir.join("announcements.bin");
        let mut history = read_checked(&history_path);
        row_mut(&mut history, row_id(&row))
            .get_mut("route")
            .unwrap()
            .insert("phase", "reserved");
        write_checked(&history_path, b"MYNOUI02", &history);
        if !committed {
            let path = cfg.store_dir.join("snapshot.bin");
            let mut state = read_checked(&path);
            let j = only_job_mut(&mut state);
            j.insert("irc_origin", Value::Null);
            j.insert("acquisition_url", Value::Null);
            j.insert("release", Value::Null);
            write_checked(&path, b"MYNOUS01", &state);
        }
        let before = irc_support::bytes(&cfg.store_dir);
        let preview = Engine::open_for_preview(cfg.clone()).unwrap();
        assert_eq!(
            route_phase(&preview.irc_announcement(row_id(&row)).unwrap()),
            Some("reserved")
        );
        assert!(preview.irc_route_pending().is_err());
        assert_eq!(before, irc_support::bytes(&cfg.store_dir));
        drop(preview);
        let engine = Engine::open(cfg).unwrap();
        assert_eq!(
            route_phase(&engine.irc_announcement(row_id(&row)).unwrap()),
            Some(if committed { "routed" } else { "aborted" })
        );
        assert_eq!(job(&engine, &admitted.id).irc_origin.is_some(), committed);
        assert_eq!(outcome(&engine.irc_route_pending().unwrap()), "idle");
        assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
    }
}

#[test]
fn forged_origin_changes_and_silent_format_downgrades_fail_before_downloads_start() {
    for corruption in [
        "hash",
        "revision",
        "missing_route",
        "mismatched_job",
        "downgrade_job",
        "downgrade_history",
    ] {
        let dir = Directory::new();
        let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
        let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
        let cfg = configuration(&dir.0.join("engine"), seed.client.listen_port());
        let engine = Engine::open(cfg.clone()).unwrap();
        let admitted = engine.submit(request(7)).unwrap().remove(0);
        let row = receive(&engine, &torrent.id, 7);
        assert!(routing(&engine.irc_route_pending().unwrap()));
        let mut changed = job(&engine, &admitted.id);
        changed.irc_origin.as_mut().unwrap().file = "Other.Movie.mp4".into();
        assert!(lock(&engine.store).unwrap().update(changed).is_err());
        lock(&engine.store).unwrap().compact().unwrap();
        drop(engine);
        let p = cfg.store_dir.join("announcements.bin");
        let mut history = read_checked(&p);
        match corruption {
            "revision" => row_mut(&mut history, row_id(&row)).insert("revision", "1"),
            "hash" => row_mut(&mut history, row_id(&row))
                .get_mut("route")
                .unwrap()
                .get_mut("origin")
                .unwrap()
                .insert("torrent_id", "1".repeat(40)),
            "missing_route" => row_mut(&mut history, row_id(&row)).insert("route", Value::Null),
            "mismatched_job" => row_mut(&mut history, row_id(&row))
                .get_mut("route")
                .unwrap()
                .get_mut("origin")
                .unwrap()
                .insert("job_id", "0".repeat(32)),
            _ => {}
        }
        write_checked(
            &p,
            if corruption == "downgrade_history" {
                b"MYNOUI01"
            } else {
                b"MYNOUI02"
            },
            &history,
        );
        if corruption == "downgrade_job" {
            let p = cfg.store_dir.join("snapshot.bin");
            let value = read_checked(&p);
            write_checked(&p, b"MYNOUS01", &value);
        }
        assert!(Engine::open(cfg.clone()).is_err());
        assert!(!cfg.downloads.data_dir.join(&torrent.id).exists());
    }
}

#[test]
fn approved_requester_routes_keep_their_capture_and_charge_through_import_and_restart() {
    let dir = Directory::new();
    let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let accounts = requester_support::Accounts::open();
    let mut cfg = accounts.config(&dir.0.join("engine"));
    let native = configuration(&dir.0.join("engine"), seed.client.listen_port());
    cfg.downloads = native.downloads;
    cfg.irc = native.irc;
    cfg.downloads_enabled = true;
    accounts.watchlist("alice", vec![requester_support::movie(7, "Fixture Movie")]);
    let engine = Engine::open(cfg.clone()).unwrap();
    let mut policy = requester_support::policy(&engine, "alice");
    policy.enabled = true;
    policy.approval_required = false;
    policy.destination = "family".into();
    policy.max_daily = 1;
    requester_support::apply(&engine, "alice", requester_support::policy_query(policy));
    engine.sync_requesters().unwrap();
    let admitted = requester_support::job(&engine, "alice");
    let capture = admitted.requester.clone();
    receive(&engine, &torrent.id, 7);
    assert!(!engine.tick().unwrap());
    assert!(routing(&engine.irc_route_pending().unwrap()));
    let mut policy = requester_support::policy(&engine, "alice");
    policy.destination = "default".into();
    requester_support::apply(&engine, "alice", requester_support::policy_query(policy));
    drop(engine);
    let mut changed = cfg.clone();
    changed.requesters.destinations[0].movies_root = dir.0.join("future/movies");
    changed
        .selection
        .profiles
        .get_mut("any")
        .unwrap()
        .blocked_terms = vec!["fixture".into()];
    let engine = Engine::open(changed).unwrap();
    let ready = run_until(&engine, &admitted.id, "ready");
    assert_eq!(ready.requester, capture);
    assert!(
        PathBuf::from(&ready.imports[0]).starts_with(&cfg.requesters.destinations[0].movies_root)
    );
    assert_eq!(
        fs::read(&ready.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
    assert_eq!(
        engine
            .requester("alice", 0, 20)
            .unwrap()
            .get("daily")
            .and_then(Value::as_u64),
        Some(1)
    );
}

#[test]
fn requester_removal_during_metadata_work_preserves_the_charge_and_prevents_a_transfer() {
    let dir = Directory::new();
    let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let gate = MetadataGate::open(seed.client.listen_port());
    let accounts = requester_support::Accounts::open();
    let mut cfg = accounts.config(&dir.0.join("engine"));
    let native = configuration(&dir.0.join("engine"), gate.port);
    cfg.downloads = native.downloads;
    cfg.irc = native.irc;
    cfg.downloads_enabled = true;
    accounts.watchlist("alice", vec![requester_support::movie(7, "Fixture Movie")]);
    let engine = Engine::open(cfg).unwrap();
    requester_support::enable(&engine, "alice", false);
    engine.sync_requesters().unwrap();
    let admitted = requester_support::job(&engine, "alice");
    let row = receive(&engine, &torrent.id, 7);
    let pass = {
        let e = engine.clone();
        thread::spawn(move || e.irc_route_pending().unwrap())
    };
    gate.wait();
    accounts.watchlist("alice", Vec::new());
    engine.sync_requesters().unwrap();
    assert_eq!(job(&engine, &admitted.id).state, "cancelled");
    gate.released.store(true, Ordering::Release);
    assert!(!routing(&pass.join().unwrap()));
    assert_eq!(
        route_phase(&engine.irc_announcement(row_id(&row)).unwrap()),
        None
    );
    no_candidate_work(&engine, &admitted.id);
    assert_eq!(
        engine
            .requester("alice", 0, 20)
            .unwrap()
            .get("daily")
            .and_then(Value::as_u64),
        Some(1)
    );
}

#[test]
fn a_verified_physical_video_cannot_be_routed_to_two_different_canonical_jobs() {
    let dir = Directory::new();
    let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let cfg = configuration(&dir.0.join("engine"), seed.client.listen_port());
    let engine = Engine::open(cfg.clone()).unwrap();
    engine.submit(request(7)).unwrap();
    engine.submit(request(8)).unwrap();
    receive(&engine, &torrent.id, 7);
    receive(&engine, &torrent.id, 8);
    let a = engine.irc_route_pending().unwrap();
    let b = engine.irc_route_pending().unwrap();
    assert_eq!(usize::from(routing(&a)) + usize::from(routing(&b)), 1);
    assert!([outcome(&a), outcome(&b)].contains(&"admission_or_storage_rejected"));
    assert_eq!(
        lock(&engine.store)
            .unwrap()
            .list()
            .iter()
            .filter(|j| j.irc_origin.is_some())
            .count(),
        1
    );
    assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
    drop(engine);
    assert!(Engine::open(cfg).is_ok());
}

#[test]
fn unverified_metadata_and_mutated_acquisition_fields_cannot_publish_work() {
    let dir = Directory::new();
    let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let cfg = configuration(&dir.0.join("engine"), proxy.port);
    let engine = Engine::open(cfg.clone()).unwrap();
    let admitted = engine.submit(request(7)).unwrap().remove(0);
    receive(&engine, &"1".repeat(40), 7);
    assert_eq!(
        outcome(&engine.irc_route_pending().unwrap()),
        "metadata_unavailable"
    );
    no_candidate_work(&engine, &admitted.id);
    receive(&engine, &torrent.id, 7);
    assert!(routing(&engine.irc_route_pending().unwrap()));
    let original = job(&engine, &admitted.id);
    let before = irc_support::bytes(&cfg.store_dir);
    for field in ["url", "release", "download_id", "files", "aliases"] {
        let mut j = original.clone();
        match field {
            "url" => j.acquisition_url = Some("http://127.0.0.1:1/changed".into()),
            "release" => j.release.as_mut().unwrap().title = "Other Movie 2024".into(),
            "download_id" => j.download_id = Some("2".repeat(40)),
            "files" => j.files.push("/tmp/../unverified.mp4".into()),
            _ => j
                .irc_origin
                .as_mut()
                .unwrap()
                .torrent_aliases
                .push("3".repeat(40)),
        }
        assert!(lock(&engine.store).unwrap().update(j).is_err());
    }
    assert_eq!(before, irc_support::bytes(&cfg.store_dir));
    assert!(proxy.requests.lock().unwrap().is_empty());
}

#[test]
fn existing_plex_media_fulfills_waiting_jobs_before_or_during_irc_selection() {
    for check_first in [true, false] {
        let dir = Directory::new();
        let accounts = requester_support::Accounts::open();
        let mut cfg = accounts.config(&dir.0);
        cfg.irc = configuration(&dir.0, 1).irc;
        cfg.downloads_enabled = true;
        cfg.plex.enabled = true;
        cfg.plex.url = accounts.url.clone();
        cfg.plex.token_env = "PATH".into();
        cfg.plex.path_mappings = vec![mynou::config::PathMapping {
            mynou_prefix: cfg.requesters.destinations[0]
                .movies_root
                .to_string_lossy()
                .into_owned(),
            plex_prefix: "/plex/family".into(),
        }];
        accounts.watchlist("alice", vec![requester_support::movie(7, "Fixture Movie")]);
        let mut present = requester_support::movie(7, "Fixture Movie");
        let mut media = Value::object();
        let mut part = Value::object();
        part.insert("file", "/plex/family/Fixture Movie/Feature.mp4");
        media.insert("Part", Value::Array(vec![part]));
        present.insert("Media", Value::Array(vec![media]));
        accounts.response(
            "/library/sections/1/all",
            200,
            requester_support::container(vec![present]),
        );
        let engine = Engine::open(cfg).unwrap();
        let mut policy = requester_support::policy(&engine, "alice");
        policy.enabled = true;
        policy.approval_required = false;
        policy.destination = "family".into();
        requester_support::apply(&engine, "alice", requester_support::policy_query(policy));
        engine.sync_requesters().unwrap();
        let admitted = requester_support::job(&engine, "alice");
        if check_first {
            assert!(engine.tick().unwrap());
        }
        receive(&engine, &"1".repeat(40), 7);
        let pass = engine.irc_route_pending().unwrap();
        assert_eq!(outcome(&pass), "already_available");
        assert!(!routing(&pass));
        assert_eq!(job(&engine, &admitted.id).state, "ready");
        no_candidate_work(&engine, &admitted.id);
        assert_eq!(
            requester_support::demand(&engine, "alice")
                .get("state")
                .and_then(Value::as_str),
            Some("ready")
        );
    }
}

#[test]
fn negative_plex_checks_wait_without_selecting_a_source_and_routing_wakes_the_job() {
    let dir = Directory::new();
    let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let accounts = requester_support::Accounts::open();
    let mut cfg = configuration(&dir.0.join("engine"), seed.client.listen_port());
    cfg.plex.enabled = true;
    cfg.plex.url = accounts.url.clone();
    cfg.plex.token_env = "PATH".into();
    accounts.response(
        "/library/sections/1/all",
        200,
        requester_support::container(Vec::new()),
    );
    let engine = Engine::open(cfg.clone()).unwrap();
    let admitted = engine.submit(request(7)).unwrap().remove(0);
    assert!(engine.tick().unwrap());
    let waiting = job(&engine, &admitted.id);
    assert_eq!(waiting.state, "queued");
    assert!(waiting.lease_id.is_none());
    assert!(waiting.next_attempt_at > mynou::store::now());
    no_candidate_work(&engine, &admitted.id);
    receive(&engine, &torrent.id, 7);
    assert!(routing(&engine.irc_route_pending().unwrap()));
    assert_eq!(job(&engine, &admitted.id).next_attempt_at, 0);
    assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
}

#[test]
fn an_origin_cannot_create_its_own_admission_in_journal_replay() {
    let dir = Directory::new();
    let torrent = movie(&dir, "Fixture.Movie.2024.1080p.mp4");
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let cfg = configuration(&dir.0.join("engine"), seed.client.listen_port());
    let engine = Engine::open(cfg.clone()).unwrap();
    engine.submit(request(7)).unwrap();
    receive(&engine, &torrent.id, 7);
    assert!(routing(&engine.irc_route_pending().unwrap()));
    drop(engine);
    let path = cfg.store_dir.join("journal.bin");
    let bytes = fs::read(&path).unwrap();
    let second = 116 + u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    assert_eq!(&bytes[second..second + 8], b"MYNOUJ05");
    let length = u32::from_le_bytes(bytes[second + 16..second + 20].try_into().unwrap()) as usize;
    let mut value =
        mynou::json::parse(std::str::from_utf8(&bytes[second + 84..second + 84 + length]).unwrap())
            .unwrap();
    value.get_mut("event").unwrap().insert("id", "1");
    let payload = mynou::json::stringify(&value).into_bytes();
    let mut frame = b"MYNOUJ05".to_vec();
    frame.extend_from_slice(&1_u64.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&[0; 32]);
    frame.extend_from_slice(&mynou::crypto::sha256(&frame));
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&mynou::crypto::sha256(&frame));
    fs::write(path, frame).unwrap();
    assert!(Engine::open(cfg.clone()).is_err());
    assert!(!cfg.downloads.data_dir.join(&torrent.id).exists());
}
