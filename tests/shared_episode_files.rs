//! Original shared-file lifecycle scenarios. Executed by GitHub Actions only.
mod automatic_pack_support;
mod library_support;
mod series_support;
#[allow(dead_code)]
mod transfer_support;
use automatic_pack_support::{Provider, no_sources, snapshot};
use library_support::{Directory, run_until};
use mynou::{
    engine::{Engine, lock},
    json::{self, Value},
    pack::SharedFileRequest,
    store::{Job, RecordedRelease, Store, now},
};
use series_support::{Catalog, episode, id, request};
use std::{
    fs,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};
use transfer_support::{BLOCK, RecordingProxy, Seeder, Torrent, engine_config, payload, wait};

fn catalog(count: u32) -> Catalog {
    Catalog::open(
        (1..=count)
            .map(|n| episode(1, n, Some("2024-01-01"), "Catalog episode"))
            .collect(),
    )
}
fn torrent(directory: &Directory, tag: &str) -> Torrent {
    Torrent::multiple(
        &directory.0.join(tag),
        "Pack",
        vec![
            (
                "shared.mp4".into(),
                include_bytes!("../examples/demo.mp4").to_vec(),
            ),
            ("untouched.txt".into(), payload(BLOCK * 8, 43)),
        ],
    )
}
fn query(source: &str) -> SharedFileRequest {
    SharedFileRequest {
        source_url: source.into(),
        file_path: "Pack/shared.mp4".into(),
        season: 1,
        episodes: vec![1, 2],
        apply: false,
        plan_id: None,
    }
}
fn tracked(engine: &Engine) -> String {
    id(&engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap())
    .into()
}
fn apply(engine: &Engine, series: &str, query: &SharedFileRequest) -> Value {
    let preview = engine.shared_file(series, query).unwrap();
    let mut apply = query.clone();
    apply.apply = true;
    apply.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    engine.shared_file(series, &apply).unwrap()
}
fn jobs(report: &Value) -> Vec<String> {
    report
        .get("jobs")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|job| id(job).into())
        .collect()
}
fn native_config(directory: &Directory, catalog: &Catalog) -> mynou::config::Config {
    let mut cfg = catalog.config(&directory.0.join("engine"));
    cfg.downloads = engine_config(&directory.0.join("engine")).downloads;
    cfg.downloads_enabled = true;
    cfg.max_attempts = 1;
    cfg.workers = 2;
    cfg
}

#[test]
fn metadata_preview_is_read_only_and_sixty_four_owners_commit_in_one_verified_frame() {
    let directory = Directory::new();
    let catalog = catalog(64);
    let torrent = torrent(&directory, "metadata");
    let cfg = catalog.config(&directory.0.join("engine"));
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let series = tracked(&engine);
    let mut query = query(torrent.path.to_str().unwrap());
    query.episodes = (1..=64).rev().collect();
    let before = snapshot(&cfg.store_dir);
    let preview = engine.shared_file(&series, &query).unwrap();
    assert_eq!(preview.get("new_owners"), Some(&Value::Number(64.0)));
    assert_eq!(snapshot(&cfg.store_dir), before);
    assert!(!cfg.series_root.exists());
    let result = apply(&engine, &series, &query);
    assert_eq!(result.get("submitted"), Some(&Value::Number(64.0)));
    let saved = lock(&engine.store).unwrap().list();
    assert_eq!(saved.len(), 64);
    let binding = saved[0].shared_file.as_ref().unwrap();
    assert_eq!(binding.torrent_id, torrent.id);
    assert!(binding.import_path.contains("S01E01-E64"));
    assert!(
        saved
            .iter()
            .all(|j| j.shared_file.as_ref() == Some(binding) && j.download_id.is_none())
    );
    let bytes = fs::read(cfg.store_dir.join("journal.bin")).unwrap();
    assert_eq!(&bytes[..8], b"MYNOUJ02");
    let length = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    assert_eq!(bytes.len(), length + 116);
    assert_eq!(lock(&engine.store).unwrap().events("").len(), 1);
    drop(engine);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    assert_eq!(lock(&engine.store).unwrap().list(), saved);
    lock(&engine.store).unwrap().compact().unwrap();
    assert_eq!(
        &fs::read(cfg.store_dir.join("snapshot.bin")).unwrap()[..8],
        b"MYNOUS02"
    );
    drop(engine);
    let preview = Engine::open_for_preview(cfg.clone()).unwrap();
    let before = snapshot(&cfg.store_dir);
    assert_eq!(lock(&preview.store).unwrap().list(), saved);
    let report = preview.shared_file(&series, &query).unwrap();
    assert_eq!(report.get("new_owners"), Some(&Value::Number(0.0)));
    query.apply = true;
    query.plan_id = Some(report.get("plan_id").unwrap().as_str().unwrap().into());
    assert!(preview.shared_file(&series, &query).is_err());
    assert_eq!(snapshot(&cfg.store_dir), before);
}

#[test]
fn interrupted_group_frames_recover_all_or_none_and_complete_corruption_fails_closed() {
    let directory = Directory::new();
    let catalog = catalog(2);
    let torrent = torrent(&directory, "metadata");
    let cfg = catalog.config(&directory.0.join("engine"));
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let series = tracked(&engine);
    apply(&engine, &series, &query(torrent.path.to_str().unwrap()));
    let original = fs::read(cfg.store_dir.join("journal.bin")).unwrap();
    drop(engine);
    for (n, cut) in [0, 8, 83, 84, original.len() / 2, original.len() - 1]
        .into_iter()
        .enumerate()
    {
        let path = directory.0.join(format!("torn-{n}"));
        drop(Store::open(&path).unwrap());
        fs::write(path.join("journal.bin"), &original[..cut]).unwrap();
        let store = Store::open(&path).unwrap();
        assert!(
            store.list().is_empty(),
            "Recovered part of a group at byte {cut}"
        );
        assert!(store.events("").is_empty());
        assert_eq!(fs::metadata(path.join("journal.bin")).unwrap().len(), 0);
    }
    let mut corrupt = original;
    corrupt[90] ^= 1;
    fs::write(cfg.store_dir.join("journal.bin"), &corrupt).unwrap();
    let error = Store::open(&cfg.store_dir).err().unwrap();
    assert!(error.contains("corrupt complete transaction"), "{error}");
    assert_eq!(
        fs::read(cfg.store_dir.join("journal.bin")).unwrap(),
        corrupt
    );
}

#[test]
fn subset_reuse_cannot_change_the_owner_range_or_reset_cancelled_requests() {
    let directory = Directory::new();
    let catalog = catalog(3);
    let torrent = torrent(&directory, "metadata");
    let cfg = catalog.config(&directory.0.join("engine"));
    let engine = Engine::open_for_management(cfg).unwrap();
    let series = tracked(&engine);
    let mut query = query(torrent.path.to_str().unwrap());
    let result = apply(&engine, &series, &query);
    let ids = jobs(&result);
    let cancelled = engine.cancel(&ids[0]).unwrap();
    query.episodes = vec![1];
    let subset = apply(&engine, &series, &query);
    assert_eq!(jobs(&subset), vec![ids[0].clone()]);
    assert_eq!(subset.get("submitted"), Some(&Value::Number(0.0)));
    assert_eq!(
        lock(&engine.store).unwrap().get(&ids[0]).unwrap(),
        cancelled
    );
    assert!(
        engine
            .remap_pack(&ids[0], "Pack/different.mp4".into())
            .is_err()
    );
    query.episodes = vec![1, 2, 3];
    assert!(
        engine
            .shared_file(&series, &query)
            .unwrap_err()
            .contains("different canonical owners")
    );
    let mut request = cancelled.request.clone();
    request.source_url = Some("replacement.torrent".into());
    assert!(lock(&engine.store).unwrap().submit(request).is_err());
    let mut edited = cancelled;
    edited.shared_file.as_mut().unwrap().last_episode = 3;
    assert!(lock(&engine.store).unwrap().update(edited).is_err());
    assert_eq!(lock(&engine.store).unwrap().list().len(), 2);
}

#[test]
fn stale_source_metadata_and_policy_guards_record_no_owners() {
    let directory = Directory::new();
    let provider = Provider::open();
    let catalog = catalog(2);
    let torrent = torrent(&directory, "metadata");
    let source = provider.metadata_url("shared", fs::read(&torrent.path).unwrap());
    let cfg = catalog.config(&directory.0.join("engine"));
    let engine = Engine::open_for_management(cfg).unwrap();
    let series = tracked(&engine);
    let query = query(&source);
    let preview = engine.shared_file(&series, &query).unwrap();
    no_sources(&preview);
    let mut apply = query.clone();
    apply.apply = true;
    apply.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    engine
        .configure_series(&series, Some(true), None, None)
        .unwrap();
    assert!(
        engine
            .shared_file(&series, &apply)
            .unwrap_err()
            .contains("preview changed")
    );
    engine
        .configure_series(&series, Some(false), None, None)
        .unwrap();
    let preview = engine.shared_file(&series, &query).unwrap();
    apply.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    let other = Torrent::multiple(
        &directory.0.join("changed"),
        "Pack",
        vec![("shared.mp4".into(), payload(BLOCK, 7))],
    );
    provider.response("/metadata-shared", 200, fs::read(&other.path).unwrap());
    assert!(
        engine
            .shared_file(&series, &apply)
            .unwrap_err()
            .contains("preview changed")
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
}

#[test]
fn bad_owner_catalogs_and_inputs_fail_before_metadata_io() {
    let directory = Directory::new();
    let provider = Provider::open();
    let catalog = Catalog::open(vec![
        episode(1, 1, Some("2024-01-01"), "Aired"),
        episode(1, 2, Some("2200-01-01"), "Future"),
    ]);
    let engine = Engine::open_for_management(catalog.config(&directory.0.join("engine"))).unwrap();
    let series = tracked(&engine);
    let query = query(&format!("{}/metadata-never", provider.url));
    assert!(
        engine
            .shared_file(&series, &query)
            .unwrap_err()
            .contains("already aired")
    );
    assert!(provider.calls.lock().unwrap().is_empty());
    for change in [
        r#"{"episodes":[1,1]}"#,
        r#"{"episodes":[0,1]}"#,
        r#"{"season":1.5}"#,
        r#"{"file_path":"../bad.mp4"}"#,
        r#"{"apply":true}"#,
        r#"{"plan_id":null}"#,
        r#"{"unknown":1}"#,
    ] {
        let mut value = query.to_json();
        for (key, field) in json::parse(change).unwrap().as_object().unwrap() {
            value.insert(key, field.clone());
        }
        assert!(
            SharedFileRequest::from_json(&value).is_err(),
            "Accepted {change}"
        );
    }
    assert!(lock(&engine.store).unwrap().list().is_empty());
}

#[test]
fn simultaneous_workers_import_one_range_path_and_block_individual_upgrades() {
    let directory = Directory::new();
    let catalog = catalog(2);
    let torrent = torrent(&directory, "metadata");
    let seed = Seeder::open(&directory.0.join("seed"), &[&torrent]);
    let cfg = native_config(&directory, &catalog);
    let engine = Engine::open(cfg.clone()).unwrap();
    let series = tracked(&engine);
    let query = query(&torrent.magnet(seed.client.listen_port()));
    let ids = jobs(&apply(&engine, &series, &query));
    thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..2 {
            workers.push(scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    let all = lock(&engine.store).unwrap().list();
                    if all.iter().all(|j| j.state == "ready") {
                        break;
                    }
                    assert!(all.iter().all(|j| j.state != "failed"), "{all:?}");
                    assert!(Instant::now() < deadline, "Shared workers timed out");
                    engine.tick().unwrap();
                    thread::sleep(Duration::from_millis(5));
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
    let saved = lock(&engine.store).unwrap().list();
    assert_eq!(saved[0].imports, saved[1].imports);
    assert_eq!(saved[0].imports.len(), 1);
    assert!(saved[0].imports[0].contains("S01E01-E02"));
    assert_eq!(
        fs::read(&saved[0].imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
    let files = snapshot(&cfg.series_root);
    assert_eq!(
        files
            .keys()
            .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
            .count(),
        1
    );
    assert_eq!(engine.library().unwrap().as_array().unwrap().len(), 2);
    assert_eq!(engine.transfers().unwrap().as_array().unwrap().len(), 1);
    assert_eq!(
        engine.transfer(&torrent.id).unwrap().get("ready"),
        Some(&Value::Bool(false))
    );
    assert!(
        !cfg.downloads
            .data_dir
            .join(&torrent.id)
            .join("Pack/untouched.txt")
            .exists()
    );
    let report = engine.check_upgrades(true).unwrap();
    assert_eq!(report.get("queued"), Some(&Value::Number(0.0)));
    assert!(
        report
            .get("entries")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e.get("action").and_then(Value::as_str)
                == Some("shared_group_upgrade_required"))
    );
    let mut replacement = saved[0].request.clone();
    replacement.source_url = Some("new.torrent".into());
    assert!(
        lock(&engine.store)
            .unwrap()
            .submit_upgrade(
                &ids[0],
                replacement,
                RecordedRelease {
                    title: "Fixture.Series.S01E01.1080p".into(),
                    profile: "default".into()
                }
            )
            .unwrap_err()
            .contains("coordinated group")
    );
    assert!(
        engine
            .set_baseline(&ids[0], "Fixture.Series.S01E01.1080p")
            .is_err()
    );
    drop(engine);
    let engine = Engine::open(cfg).unwrap();
    assert_eq!(lock(&engine.store).unwrap().list(), saved);
}

#[test]
fn cancelling_the_first_claim_preserves_an_unclaimed_owner_across_restart_and_retry() {
    let directory = Directory::new();
    let catalog = catalog(2);
    let torrent = torrent(&directory, "metadata");
    let seed = Seeder::open(&directory.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let cfg = native_config(&directory, &catalog);
    let engine = Engine::open(cfg.clone()).unwrap();
    let series = tracked(&engine);
    apply(&engine, &series, &query(&torrent.magnet(proxy.port)));
    engine.tick().unwrap();
    let all = lock(&engine.store).unwrap().list();
    let active = all.iter().find(|j| j.download_id.is_some()).unwrap();
    let other = all.iter().find(|j| j.id != active.id).unwrap();
    assert!(other.download_id.is_none());
    engine.cancel(&active.id).unwrap();
    assert_eq!(
        engine.transfer(&torrent.id).unwrap().get("user_paused"),
        Some(&Value::Bool(false))
    );
    drop(engine);
    proxy.wait_idle();
    let engine = Engine::open(cfg.clone()).unwrap();
    assert_eq!(
        engine.transfer(&torrent.id).unwrap().get("user_paused"),
        Some(&Value::Bool(false))
    );
    proxy.payloads_enabled.store(true, Ordering::Release);
    let ready = run_until(&engine, &other.id, "ready");
    assert_eq!(
        lock(&engine.store).unwrap().get(&active.id).unwrap().state,
        "cancelled"
    );
    assert_eq!(engine.library().unwrap().as_array().unwrap().len(), 1);
    engine.retry(&active.id).unwrap();
    let retried = run_until(&engine, &active.id, "ready");
    assert_eq!(ready.imports, retried.imports);
    assert!(
        engine
            .remap_pack(&active.id, "Pack/reassigned.mp4".into())
            .is_err()
    );
    assert_eq!(
        snapshot(&cfg.series_root)
            .keys()
            .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
            .count(),
        1
    );
}

#[test]
fn changed_source_after_acquisition_commit_never_creates_a_transfer_or_import() {
    let directory = Directory::new();
    let provider = Provider::open();
    let catalog = catalog(2);
    let torrent = torrent(&directory, "metadata");
    let source = provider.metadata_url("worker", fs::read(&torrent.path).unwrap());
    let cfg = native_config(&directory, &catalog);
    let engine = Engine::open(cfg.clone()).unwrap();
    let series = tracked(&engine);
    apply(&engine, &series, &query(&source));
    let changed = Torrent::multiple(
        &directory.0.join("changed"),
        "Pack",
        vec![("shared.mp4".into(), payload(BLOCK, 7))],
    );
    provider.response("/metadata-worker", 200, fs::read(&changed.path).unwrap());
    wait(|| {
        engine.tick().unwrap();
        lock(&engine.store)
            .unwrap()
            .list()
            .iter()
            .all(|j| j.state == "failed")
    });
    assert!(lock(&engine.store).unwrap().list().iter().all(|j| {
        j.imports.is_empty()
            && j.last_error
                .as_deref()
                .unwrap()
                .contains("identity differs")
    }));
    assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
    assert!(!cfg.series_root.exists());
}

#[test]
fn plex_confirms_the_same_exact_import_separately_for_each_canonical_owner() {
    let directory = Directory::new();
    let catalog = catalog(2);
    let plex = Catalog::open(Vec::new());
    let torrent = torrent(&directory, "metadata");
    let mut cfg = catalog.config(&directory.0.join("engine"));
    cfg.plex.enabled = true;
    cfg.plex.url = plex.url.clone();
    cfg.plex.series_section = "2".into();
    cfg.plex.token_override = Some("shared-plex-fixture-secret".into());
    plex.response("/library/sections/2/refresh", 200, Value::object());
    plex.response("/library/sections/2/all",200,json::parse(r#"{"MediaContainer":{"Metadata":[{"title":"Fixture Series","year":2024,"ratingKey":"7","Guid":[{"id":"tmdb://42"}]}]}}"#).unwrap());
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let series = tracked(&engine);
    let ids = jobs(&apply(
        &engine,
        &series,
        &query(torrent.path.to_str().unwrap()),
    ));
    let binding = lock(&engine.store)
        .unwrap()
        .get(&ids[0])
        .unwrap()
        .shared_file
        .unwrap();
    fs::create_dir_all(std::path::Path::new(&binding.import_path).parent().unwrap()).unwrap();
    fs::write(&binding.import_path, include_bytes!("../examples/demo.mp4")).unwrap();
    for id in &ids {
        let mut store = lock(&engine.store).unwrap();
        let mut job = store.get(id).unwrap();
        job.imports = vec![binding.import_path.clone()];
        job.state = "imported".into();
        store.update(job).unwrap();
    }
    let leaves = |second: &str| {
        let mut value=json::parse(r#"{"MediaContainer":{"Metadata":[{"parentIndex":1,"index":1,"Media":[{"Part":[{"file":""}]}]},{"parentIndex":1,"index":2,"Media":[{"Part":[{"file":""}]}]}]}}"#).unwrap();
        let entries = value
            .get_mut("MediaContainer")
            .unwrap()
            .get_mut("Metadata")
            .unwrap();
        if let Value::Array(items) = entries {
            for (n, path) in [binding.import_path.as_str(), second]
                .into_iter()
                .enumerate()
            {
                items[n].insert(
                    "Media",
                    json::parse(&format!(
                        "[{{\"Part\":[{{\"file\":{}}}]}}]",
                        json::stringify(&Value::from(path))
                    ))
                    .unwrap(),
                );
            }
        }
        value
    };
    plex.response(
        "/library/metadata/7/allLeaves",
        200,
        leaves("/other/old-copy.mp4"),
    );
    engine.tick().unwrap();
    engine.tick().unwrap();
    let all = lock(&engine.store).unwrap().list();
    assert_eq!(
        all.iter().find(|j| j.request.episode == 1).unwrap().state,
        "ready"
    );
    assert_eq!(
        all.iter().find(|j| j.request.episode == 2).unwrap().state,
        "scanning"
    );
    assert_eq!(engine.library().unwrap().as_array().unwrap().len(), 1);
    plex.response(
        "/library/metadata/7/allLeaves",
        200,
        leaves(&binding.import_path),
    );
    {
        let mut store = lock(&engine.store).unwrap();
        let mut pending = store
            .list()
            .into_iter()
            .find(|j| j.request.episode == 2)
            .unwrap();
        pending.next_attempt_at = 0;
        store.update(pending).unwrap();
    }
    engine.tick().unwrap();
    assert!(
        lock(&engine.store)
            .unwrap()
            .list()
            .iter()
            .all(|j| j.state == "ready")
    );
    assert_eq!(engine.library().unwrap().as_array().unwrap().len(), 2);
}

#[test]
fn an_active_group_lease_excludes_other_owners_and_recovery_keeps_the_binding() {
    let directory = Directory::new();
    let catalog = catalog(2);
    let torrent = torrent(&directory, "metadata");
    let cfg = catalog.config(&directory.0.join("engine"));
    let engine = Engine::open_for_management(cfg).unwrap();
    let series = tracked(&engine);
    apply(&engine, &series, &query(torrent.path.to_str().unwrap()));
    let mut store = lock(&engine.store).unwrap();
    let at = now();
    let first = store.claim(at, 30).unwrap().unwrap();
    assert!(store.claim(at, 30).unwrap().is_none());
    store.cancel(&first.id).unwrap();
    let second = store.claim(at, 30).unwrap().unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(first.shared_file, second.shared_file);
    let mut wrong: Job = second.clone();
    wrong.download_id = Some("b".repeat(40));
    assert!(store.update(wrong).is_err());
    let mut wrong = second.clone();
    wrong.imports = vec!["/elsewhere/wrong.mp4".into()];
    assert!(store.update(wrong).is_err());
    assert_eq!(store.get(&second.id).unwrap(), second);
}

#[test]
fn restart_reuses_a_verified_shared_file_published_before_its_import_was_journaled() {
    let directory = Directory::new();
    let catalog = catalog(2);
    let torrent = torrent(&directory, "metadata");
    let seed = Seeder::open(&directory.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let cfg = native_config(&directory, &catalog);
    let engine = Engine::open(cfg.clone()).unwrap();
    let series = tracked(&engine);
    let ids = jobs(&apply(
        &engine,
        &series,
        &query(&torrent.magnet(proxy.port)),
    ));
    engine.tick().unwrap();
    proxy.payloads_enabled.store(true, Ordering::Release);
    wait(|| {
        engine.transfer(&torrent.id).unwrap().get("selected_ready") == Some(&Value::Bool(true))
    });
    let owners = lock(&engine.store).unwrap().list();
    assert!(owners.iter().all(|j| j.imports.is_empty()));
    let binding = owners[0].shared_file.as_ref().unwrap();
    let destination = std::path::Path::new(&binding.import_path);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    let source = cfg
        .downloads
        .data_dir
        .join(&torrent.id)
        .join("Pack/shared.mp4");
    fs::hard_link(&source, destination).unwrap();
    let imported = fs::read(destination).unwrap();
    drop(engine);
    proxy.wait_idle();
    let engine = Engine::open(cfg.clone()).unwrap();
    for id in ids {
        run_until(&engine, &id, "ready");
    }
    assert!(
        lock(&engine.store)
            .unwrap()
            .list()
            .iter()
            .all(|j| j.imports == vec![binding.import_path.clone()])
    );
    assert_eq!(fs::read(destination).unwrap(), imported);
    assert_eq!(
        snapshot(&cfg.series_root)
            .keys()
            .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
            .count(),
        1
    );
}

#[test]
fn signed_but_incomplete_ownership_is_rejected_in_both_journal_and_snapshot() {
    let directory = Directory::new();
    let catalog = catalog(3);
    let torrent = torrent(&directory, "metadata");
    let cfg = catalog.config(&directory.0.join("engine"));
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let series = tracked(&engine);
    let mut query = query(torrent.path.to_str().unwrap());
    query.episodes = vec![1, 2, 3];
    apply(&engine, &series, &query);
    let original = fs::read(cfg.store_dir.join("journal.bin")).unwrap();
    drop(engine);
    let size = u32::from_le_bytes(original[16..20].try_into().unwrap()) as usize;
    let mut value = json::parse(std::str::from_utf8(&original[84..84 + size]).unwrap()).unwrap();
    if let Value::Array(owners) = value.get_mut("shared_owners").unwrap() {
        owners.pop();
    }
    let payload = json::stringify(&value).into_bytes();
    let mut frame = original[..52].to_vec();
    frame[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&mynou::crypto::sha256(&frame));
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&mynou::crypto::sha256(&frame));
    fs::write(cfg.store_dir.join("journal.bin"), &frame).unwrap();
    assert!(
        Store::open(&cfg.store_dir)
            .err()
            .unwrap()
            .contains("Incomplete shared ownership")
    );
    assert_eq!(fs::read(cfg.store_dir.join("journal.bin")).unwrap(), frame);
    fs::write(cfg.store_dir.join("journal.bin"), original).unwrap();
    let mut store = Store::open(&cfg.store_dir).unwrap();
    store.compact().unwrap();
    drop(store);
    let bytes = fs::read(cfg.store_dir.join("snapshot.bin")).unwrap();
    let mut value =
        json::parse(std::str::from_utf8(&bytes[16..bytes.len() - 32]).unwrap()).unwrap();
    if let Value::Array(owners) = value.get_mut("jobs").unwrap() {
        owners.pop();
    }
    let payload = json::stringify(&value).into_bytes();
    let mut bytes = b"MYNOUS02".to_vec();
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&mynou::crypto::sha256(&bytes));
    fs::write(cfg.store_dir.join("snapshot.bin"), &bytes).unwrap();
    assert!(
        Store::open(&cfg.store_dir)
            .err()
            .unwrap()
            .contains("Incomplete shared ownership")
    );
    assert_eq!(fs::read(cfg.store_dir.join("snapshot.bin")).unwrap(), bytes);
}

#[test]
fn missing_empty_nonconsecutive_or_independently_owned_files_never_create_shared_jobs() {
    let directory = Directory::new();
    let catalog = catalog(3);
    let empty_torrent = Torrent::multiple(
        &directory.0.join("metadata"),
        "Pack",
        vec![("shared.mp4".into(), Vec::new())],
    );
    let engine = Engine::open_for_management(catalog.config(&directory.0.join("engine"))).unwrap();
    let series = tracked(&engine);
    let query = query(empty_torrent.path.to_str().unwrap());
    assert!(engine.shared_file(&series, &query).is_err());
    let torrent = torrent(&directory, "valid");
    let mut query = query.clone();
    query.source_url = torrent.path.to_str().unwrap().into();
    query.file_path = "Pack/missing.mp4".into();
    assert!(engine.shared_file(&series, &query).is_err());
    query.file_path = "Pack/shared.mp4".into();
    for owners in [vec![1], vec![1, 3]] {
        query.episodes = owners;
        assert!(
            engine
                .shared_file(&series, &query)
                .unwrap_err()
                .contains("consecutive episodes")
        );
    }
    let mut request = request();
    request.kind = "episode".into();
    request.season = 1;
    request.episode = 1;
    lock(&engine.store).unwrap().submit(request).unwrap();
    query.episodes = vec![1, 2];
    assert!(
        engine
            .shared_file(&series, &query)
            .unwrap_err()
            .contains("unrelated episode request")
    );
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
}
