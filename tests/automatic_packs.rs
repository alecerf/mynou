//! Automatic pack acquisition uses synthetic catalog/indexer metadata and local peers.
mod automatic_pack_support;
mod library_support;
mod series_support;
#[allow(dead_code)]
mod transfer_support;
use automatic_pack_support::*;
use library_support::{Directory, run_until};
use mynou::{
    engine::{Engine, lock},
    integrations,
    json::Value,
    pack::AutoPackRequest,
};
use series_support::{Catalog, episode, id, request};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};
use transfer_support::{BLOCK, RecordingProxy, Seeder, Torrent, engine_config, payload, wait};

fn catalog() -> Catalog {
    Catalog::open(
        (1..=2)
            .map(|n| episode(1, n, Some("2024-01-01"), "Catalog episode"))
            .collect(),
    )
}
fn torrent(directory: &Directory, name: &str, episodes: &[u32]) -> Torrent {
    let mut files: Vec<_> = episodes
        .iter()
        .map(|n| {
            (
                PathBuf::from(format!("S01E{n:02}.mp4")),
                include_bytes!("../examples/demo.mp4").to_vec(),
            )
        })
        .collect();
    files.push(("unneeded.bin".into(), payload(BLOCK * 8, 71)));
    files.push(("untouched.txt".into(), payload(BLOCK, 79)));
    Torrent::multiple(&directory.0.join(name), "Fixture.Series.S01", files)
}
fn tracked(engine: &Engine) -> String {
    id(&engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap())
    .into()
}

#[test]
fn ranking_and_metadata_fallback_cover_every_missing_episode_without_mutation() {
    let directory = Directory::new();
    let catalog = catalog();
    let provider = Provider::open();
    let missing = torrent(&directory, "missing", &[1]);
    let complete = torrent(&directory, "complete", &[1, 2]);
    let wrong = provider.metadata_url("missing", fs::read(&missing.path).unwrap());
    let good = provider.metadata_url("complete", fs::read(&complete.path).unwrap());
    provider.releases(vec![
        release("Fixture.Series.S01.1080p", &wrong, 900),
        release("Fixture.Series.S01.Complete.1080p", &good, 2),
        release("Fixture.Series.S01.Complete.720p", &good, 999),
        release("Fixture.Series.S02.1080p", &good, 1000),
        release("Fixture.Series.S01E01.1080p", &good, 1000),
        release("Fixture.Series.2023.S01.1080p", &good, 1000),
    ]);
    let cfg = configure(
        catalog.config(&directory.0.join("engine")),
        &provider,
        r#"{"resolutions":[1080,720]}"#,
    );
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let series = tracked(&engine);
    let before = snapshot(&cfg.store_dir);
    let report = engine.search_packs(&series, &preview(1)).unwrap();
    assert_eq!(selected_title(&report), "Fixture.Series.S01.Complete.1080p");
    assert_eq!(
        report
            .get("metadata_decisions")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(report.get("mapping").unwrap().as_array().unwrap().len(), 2);
    assert_eq!(report.get("rejected").unwrap().as_array().unwrap().len(), 3);
    no_sources(&report);
    assert!(lock(&engine.store).unwrap().list().is_empty());
    assert_eq!(snapshot(&cfg.store_dir), before);
    let mut ordinary = request();
    ordinary.kind = "episode".into();
    ordinary.season = 1;
    ordinary.episode = 1;
    let normal = integrations::search_report(&cfg, &ordinary).unwrap();
    assert_eq!(normal.get("accepted").unwrap().as_array().unwrap().len(), 1);
    assert!(
        normal.get("accepted").unwrap().as_array().unwrap()[0]
            .get("title")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("S01E01")
    );
}

#[test]
fn magnet_preview_requests_only_metadata_then_bound_jobs_import_from_one_partial_transfer() {
    let directory = Directory::new();
    let catalog = catalog();
    let provider = Provider::open();
    let torrent = torrent(&directory, "media", &[1, 2]);
    let seed = Seeder::open(&directory.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    provider.releases(vec![release(
        "Fixture.Series.S01.Complete.1080p",
        &torrent.magnet(proxy.port),
        8,
    )]);
    let mut cfg = configure(catalog.config(&directory.0.join("engine")), &provider, "{}");
    cfg.downloads = engine_config(&directory.0.join("engine")).downloads;
    cfg.downloads_enabled = true;
    cfg.max_attempts = 1;
    cfg.workers = 1;
    let engine = Engine::open(cfg.clone()).unwrap();
    let series = tracked(&engine);
    let report = engine.search_packs(&series, &preview(1)).unwrap();
    assert!(proxy.requests.lock().unwrap().is_empty());
    assert!(lock(&engine.store).unwrap().list().is_empty());
    assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
    no_sources(&report);
    let applied = engine.search_packs(&series, &apply(&report)).unwrap();
    assert_eq!(
        applied.get("submission").unwrap().get("submitted"),
        Some(&Value::Number(2.0))
    );
    let jobs = lock(&engine.store).unwrap().list();
    assert_eq!(jobs.len(), 2);
    for job in &jobs {
        let origin = job.pack_origin.as_ref().unwrap();
        assert_eq!(origin.series_id, series);
        assert_eq!(origin.torrent_id, torrent.id);
        assert_eq!(
            origin.candidate_id,
            report
                .get("selected_candidate_id")
                .unwrap()
                .as_str()
                .unwrap()
        );
        assert!(job.release.is_none());
        assert!(job.download_id.is_none());
        assert_eq!(mynou::store::Job::from_json(&job.to_json()).unwrap(), *job);
    }
    assert!(proxy.requests.lock().unwrap().is_empty());
    wait(|| {
        engine.tick().unwrap();
        lock(&engine.store)
            .unwrap()
            .list()
            .iter()
            .all(|job| job.download_id.is_some())
    });
    proxy.payloads_enabled.store(true, Ordering::Release);
    for job in jobs {
        let ready = run_until(&engine, &job.id, "ready");
        assert_eq!(
            fs::read(&ready.imports[0]).unwrap(),
            include_bytes!("../examples/demo.mp4")
        );
        assert!(ready.release.is_none());
    }
    let transfer = engine.transfer(&torrent.id).unwrap();
    assert_eq!(transfer.get("selected_ready"), Some(&Value::Bool(true)));
    assert_eq!(transfer.get("ready"), Some(&Value::Bool(false)));
    assert!(
        !cfg.downloads
            .data_dir
            .join(&torrent.id)
            .join("Fixture.Series.S01/untouched.txt")
            .exists()
    );
    let saved = lock(&engine.store).unwrap().list();
    drop(engine);
    proxy.wait_idle();
    let engine = Engine::open(cfg).unwrap();
    assert_eq!(lock(&engine.store).unwrap().list(), saved);
    let calls = provider.calls.lock().unwrap().len();
    let empty = engine
        .search_packs(
            &series,
            &AutoPackRequest {
                apply: true,
                ..preview(1)
            },
        )
        .unwrap();
    assert_eq!(empty.get("scope_empty"), Some(&Value::Bool(true)));
    assert_eq!(provider.calls.lock().unwrap().len(), calls);
}

#[test]
fn policy_and_existing_terminal_requests_restrict_the_scope_without_resurrection() {
    let directory = Directory::new();
    let provider = Provider::open();
    let catalog = Catalog::open(vec![
        episode(1, 1, Some("2024-01-01"), "Earlier"),
        episode(1, 2, Some("2024-01-02"), "Excluded"),
        episode(1, 3, Some("2024-01-03"), "Missing"),
        episode(1, 4, Some("2100-01-01"), "Future"),
        episode(1, 5, None, "Undated"),
    ]);
    let torrent = torrent(&directory, "media", &[1, 2, 3, 4, 5]);
    let url = provider.metadata_url("policy", fs::read(&torrent.path).unwrap());
    provider.releases(vec![release("Fixture.Series.S01.1080p", &url, 1)]);
    let engine = Engine::open_for_management(configure(
        catalog.config(&directory.0.join("engine")),
        &provider,
        "{}",
    ))
    .unwrap();
    let series = tracked(&engine);
    engine
        .configure_series(&series, None, None, Some(Some("2024-01-02".into())))
        .unwrap();
    engine.monitor_series_episode(&series, 1, 2, false).unwrap();
    let mut existing = request();
    existing.kind = "episode".into();
    existing.season = 1;
    existing.episode = 3;
    let queued = engine.submit(existing).unwrap().remove(0);
    engine.cancel(&queued.id).unwrap();
    let calls = provider.calls.lock().unwrap().len();
    let report = engine
        .search_packs(
            &series,
            &AutoPackRequest {
                apply: true,
                ..preview(1)
            },
        )
        .unwrap();
    assert_eq!(report.get("scope_empty"), Some(&Value::Bool(true)));
    assert_eq!(provider.calls.lock().unwrap().len(), calls);
    assert_eq!(
        lock(&engine.store).unwrap().get(&queued.id).unwrap().state,
        "cancelled"
    );
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
}

#[test]
fn changed_policy_during_metadata_discards_the_entire_acquisition() {
    let directory = Directory::new();
    let catalog = catalog();
    let provider = Provider::open();
    let torrent = torrent(&directory, "media", &[1, 2]);
    let url = provider.metadata_url("race", fs::read(&torrent.path).unwrap());
    provider.releases(vec![release("Fixture.Series.S01.1080p", &url, 1)]);
    let engine = Engine::open_for_management(configure(
        catalog.config(&directory.0.join("engine")),
        &provider,
        "{}",
    ))
    .unwrap();
    let series = tracked(&engine);
    provider.blocked.store(true, Ordering::Release);
    let worker = engine.clone();
    let worker_id = series.clone();
    let handle = thread::spawn(move || {
        worker.search_packs(
            &worker_id,
            &AutoPackRequest {
                apply: true,
                ..preview(1)
            },
        )
    });
    provider.wait_metadata();
    engine.monitor_series_episode(&series, 1, 2, false).unwrap();
    provider.blocked.store(false, Ordering::Release);
    assert!(
        handle
            .join()
            .unwrap()
            .unwrap_err()
            .contains("scope changed")
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
}

#[test]
fn a_catalog_refresh_without_a_revision_change_also_invalidates_the_captured_scope() {
    let directory = Directory::new();
    let catalog = catalog();
    let provider = Provider::open();
    let torrent = torrent(&directory, "media", &[1, 2]);
    let url = provider.metadata_url("catalog-race", fs::read(&torrent.path).unwrap());
    provider.releases(vec![release("Fixture.Series.S01.1080p", &url, 1)]);
    let engine = Engine::open_for_management(configure(
        catalog.config(&directory.0.join("engine")),
        &provider,
        "{}",
    ))
    .unwrap();
    let series = tracked(&engine);
    let before = engine.series_record(&series).unwrap();
    provider.blocked.store(true, Ordering::Release);
    let worker = engine.clone();
    let worker_id = series.clone();
    let handle = thread::spawn(move || {
        worker.search_packs(
            &worker_id,
            &AutoPackRequest {
                apply: true,
                ..preview(1)
            },
        )
    });
    provider.wait_metadata();
    catalog.episodes(
        1,
        vec![
            episode(1, 1, Some("2024-01-01"), "Changed title"),
            episode(1, 2, Some("2024-01-01"), "Catalog episode"),
        ],
    );
    let refreshed = engine.refresh_series(&series).unwrap();
    assert_eq!(before.get("revision"), refreshed.get("revision"));
    provider.blocked.store(false, Ordering::Release);
    assert!(
        handle
            .join()
            .unwrap()
            .unwrap_err()
            .contains("scope changed")
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
}

#[test]
fn changed_metadata_breaks_preview_guards_and_queued_hash_binding_before_payload() {
    let directory = Directory::new();
    let catalog = catalog();
    let provider = Provider::open();
    let original = torrent(&directory, "original", &[1, 2]);
    let changed = Torrent::multiple(
        &directory.0.join("changed"),
        "Fixture.Series.S01",
        vec![
            ("S01E01.mp4".into(), payload(BLOCK, 3)),
            ("S01E02.mp4".into(), payload(BLOCK, 5)),
        ],
    );
    let url = provider.metadata_url("mutable", fs::read(&original.path).unwrap());
    provider.releases(vec![release("Fixture.Series.S01.1080p", &url, 1)]);
    let mut cfg = configure(catalog.config(&directory.0.join("engine")), &provider, "{}");
    cfg.downloads = engine_config(&directory.0.join("engine")).downloads;
    cfg.downloads_enabled = true;
    cfg.max_attempts = 1;
    let engine = Engine::open(cfg.clone()).unwrap();
    let series = tracked(&engine);
    let report = engine.search_packs(&series, &preview(1)).unwrap();
    provider.response("/metadata-mutable", 200, fs::read(&changed.path).unwrap());
    assert!(
        engine
            .search_packs(&series, &apply(&report))
            .unwrap_err()
            .contains("candidate changed")
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
    engine
        .search_packs(
            &series,
            &AutoPackRequest {
                apply: true,
                ..preview(1)
            },
        )
        .unwrap();
    let jobs = lock(&engine.store).unwrap().list();
    let mut forged = jobs[0].clone();
    forged.pack_origin = None;
    assert!(
        lock(&engine.store)
            .unwrap()
            .update(forged)
            .unwrap_err()
            .contains("immutable")
    );
    provider.response("/metadata-mutable", 200, fs::read(&original.path).unwrap());
    for job in jobs {
        let failed = run_until(&engine, &job.id, "failed");
        assert!(
            failed
                .last_error
                .unwrap()
                .contains("Torrent identity differs")
        );
        assert_eq!(failed.pack_origin.unwrap().torrent_id, changed.id);
    }
    assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
    let saved = lock(&engine.store).unwrap().list();
    drop(engine);
    let engine = Engine::open(cfg).unwrap();
    assert_eq!(lock(&engine.store).unwrap().list(), saved);
}

#[test]
fn metadata_work_obeys_the_shared_deadline_and_read_only_apply_rejects_before_io() {
    let directory = Directory::new();
    let catalog = catalog();
    let provider = Provider::open();
    let torrent = torrent(&directory, "media", &[1, 2]);
    let url = provider.metadata_url("deadline", fs::read(&torrent.path).unwrap());
    provider.releases(vec![release("Fixture.Series.S01.1080p", &url, 1)]);
    let cfg = configure(catalog.config(&directory.0.join("engine")), &provider, "{}");
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let series = tracked(&engine);
    let before = snapshot(&cfg.store_dir);
    provider.blocked.store(true, Ordering::Release);
    let result = engine.search_packs_before(
        &series,
        &AutoPackRequest {
            apply: true,
            ..preview(1)
        },
        Instant::now() + Duration::from_millis(150),
    );
    provider.blocked.store(false, Ordering::Release);
    assert!(result.is_err());
    assert_eq!(snapshot(&cfg.store_dir), before);
    drop(engine);
    let readonly = Engine::open_for_preview(cfg.clone()).unwrap();
    let calls = provider.calls.lock().unwrap().len();
    assert!(
        readonly
            .search_packs(
                &series,
                &AutoPackRequest {
                    apply: true,
                    ..preview(1)
                }
            )
            .unwrap_err()
            .contains("writable")
    );
    assert_eq!(provider.calls.lock().unwrap().len(), calls);
    let report = readonly.search_packs(&series, &preview(1)).unwrap();
    no_sources(&report);
    assert_eq!(snapshot(&cfg.store_dir), before);
}

#[test]
fn metadata_candidate_limit_is_explicit_and_does_not_reach_a_ninth_pack() {
    let directory = Directory::new();
    let catalog = catalog();
    let provider = Provider::open();
    let missing = torrent(&directory, "missing", &[1]);
    let complete = torrent(&directory, "complete", &[1, 2]);
    let wrong = provider.metadata_url("missing", fs::read(&missing.path).unwrap());
    let good = provider.metadata_url("complete", fs::read(&complete.path).unwrap());
    let mut releases: Vec<_> = (1..=8)
        .map(|n| {
            release(
                &format!("Fixture.Series.S01.1080p.Group{n}"),
                &wrong,
                100 - n,
            )
        })
        .collect();
    releases.push(release("Fixture.Series.S01.1080p.Last", &good, 1));
    provider.releases(releases);
    let engine = Engine::open_for_management(configure(
        catalog.config(&directory.0.join("engine")),
        &provider,
        "{}",
    ))
    .unwrap();
    let series = tracked(&engine);
    let report = engine.search_packs(&series, &preview(1)).unwrap();
    assert_eq!(report.get("selected_candidate_id"), Some(&Value::Null));
    assert_eq!(
        report
            .get("metadata_decisions")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        8
    );
    assert!(
        !provider
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains("metadata-complete"))
    );
    no_sources(&report);
    assert!(
        engine
            .search_packs(
                &series,
                &AutoPackRequest {
                    apply: true,
                    ..preview(1)
                }
            )
            .is_err()
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
}

#[test]
fn torznab_pack_search_queries_a_season_without_an_episode_parameter() {
    let directory = Directory::new();
    let catalog = catalog();
    let provider = Provider::open();
    let torrent = torrent(&directory, "media", &[1, 2]);
    let url = provider.metadata_url("torznab", fs::read(&torrent.path).unwrap());
    provider.response("/indexer", 200, format!("<rss><channel><item><title>Fixture.Series.S01.1080p</title><enclosure url=\"{}\"/><seeders>5</seeders></item></channel></rss>", url.replace('&', "&amp;")).into_bytes());
    let mut cfg = configure(catalog.config(&directory.0.join("engine")), &provider, "{}");
    cfg.sources[0].kind = "torznab".into();
    let engine = Engine::open_for_management(cfg).unwrap();
    let series = tracked(&engine);
    let report = engine.search_packs(&series, &preview(1)).unwrap();
    assert_eq!(selected_title(&report), "Fixture.Series.S01.1080p");
    let calls = provider.calls.lock().unwrap();
    let query = calls
        .iter()
        .find(|request| request.contains("/indexer"))
        .unwrap();
    assert!(query.contains("t=tvsearch"));
    assert!(query.contains("season=1"));
    assert!(!query.contains("&ep="));
    no_sources(&report);
}
