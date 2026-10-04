//! Durable series monitoring, bounded acquisition and mapping safety. CI only.
mod library_support;
mod series_support;
use library_support::Directory;
use mynou::{
    date,
    engine::{Engine, lock},
    json::{self, Value},
    series::CalendarQuery,
};
use series_support::*;
use std::{
    fs,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

#[test]
fn background_monitor_queues_an_episode_when_its_catalog_air_date_becomes_due() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine.track_series(&request(), false, false).unwrap();
    assert!(lock(&engine.store).unwrap().list().is_empty());
    catalog.episodes(1, vec![episode(1, 1, Some("2024-01-01"), "Now aired")]);
    engine
        .configure_series(id(&record), Some(true), None, None)
        .unwrap();
    // Repeated Plex/API tracking must not postpone the now-due catalog refresh.
    engine.track_series(&request(), false, false).unwrap();
    let workers = engine.start();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let jobs = lock(&engine.store).unwrap().list();
        if !jobs.is_empty() {
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].request.season, 1);
            assert_eq!(jobs[0].request.episode, 1);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Background series monitor did not queue the aired episode"
        );
        thread::sleep(Duration::from_millis(10));
    }
    drop(workers);
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
}

#[test]
fn repeated_tracking_keeps_monitor_choices_due_time_and_snapshot_unchanged() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let record = engine.track_series(&request(), false, false).unwrap();
    for due_now in [false, true] {
        if due_now {
            engine
                .configure_series(id(&record), Some(true), None, None)
                .unwrap();
        }
        let expected = engine.series_record(id(&record)).unwrap();
        let snapshot = fs::read(cfg.store_dir.join("series.json")).unwrap();
        // Reusing an existing scope does not apply the new-scope flags.
        engine.track_series(&request(), true, true).unwrap();
        assert_eq!(engine.series_record(id(&record)).unwrap(), expected);
        assert_eq!(
            fs::read(cfg.store_dir.join("series.json")).unwrap(),
            snapshot
        );
    }
}

#[test]
fn background_catalog_failure_is_visible_in_series_and_service_status() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine.track_series(&request(), false, false).unwrap();
    catalog.response("/3/tv/42", 503, Value::object());
    engine
        .configure_series(id(&record), Some(true), None, None)
        .unwrap();
    let workers = engine.start();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = engine.status().unwrap();
        if let Some(error) = status.get("last_series_error").and_then(Value::as_str) {
            assert!(error.contains("HTTP response 503"));
            assert!(!error.contains(&catalog.url));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Background catalog error did not reach service status"
        );
        thread::sleep(Duration::from_millis(10));
    }
    drop(workers);
    let saved = engine.series_record(id(&record)).unwrap();
    assert!(
        saved
            .get("last_error")
            .and_then(Value::as_str)
            .unwrap()
            .contains("HTTP response 503")
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
}

#[test]
fn aired_future_and_undated_episodes_keep_distinct_acquisition_states() {
    let directory = Directory::new();
    let tomorrow = date::add(&date::today(), 1).unwrap();
    let catalog = Catalog::open(vec![
        episode(1, 1, Some("2024-01-01"), "Aired"),
        episode(1, 2, Some(&tomorrow), "Future"),
        episode(1, 3, None, "Undated"),
        episode(1, 4, Some("2024-02-30"), "Invalid date"),
    ]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine.track_series(&request(), false, false).unwrap();
    assert_eq!(record.get("submitted"), Some(&Value::Number(1.0)));
    assert_eq!(episodes(&record).len(), 4);
    assert_eq!(episodes(&record)[2].get("air_date"), Some(&Value::Null));
    assert_eq!(episodes(&record)[3].get("air_date"), Some(&Value::Null));
    let jobs = lock(&engine.store).unwrap().list();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].request.episode, 1);
    let summaries = engine.series().unwrap();
    let summary = &summaries.as_array().unwrap()[0];
    assert_eq!(summary.get("undated_count"), Some(&Value::Number(2.0)));
    assert!(summary.get("episodes").is_none());
    let calendar = engine
        .episode_calendar(&CalendarQuery::new(Some(&date::today()), Some(&tomorrow)).unwrap())
        .unwrap();
    assert_eq!(episodes(&calendar).len(), 1);
    assert_eq!(
        episodes(&calendar)[0].get("state"),
        Some(&Value::String("scheduled".into()))
    );
}

#[test]
fn future_only_episode_exclusions_and_cancellation_survive_repeated_refresh() {
    let directory = Directory::new();
    let today = date::today();
    let tomorrow = date::add(&today, 1).unwrap();
    let catalog = Catalog::open(vec![
        episode(1, 1, Some("2024-01-01"), "Backlog"),
        episode(1, 2, Some(&today), "Today"),
        episode(1, 3, Some(&tomorrow), "Next"),
    ]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine.track_series(&request(), false, true).unwrap();
    let id = id(&record);
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    engine.monitor_series_episode(id, 1, 3, false).unwrap();
    catalog.episodes(
        1,
        vec![
            episode(1, 1, Some("2024-01-01"), "Backlog"),
            episode(1, 2, Some(&today), "Today"),
            episode(1, 3, Some(&today), "Now aired"),
        ],
    );
    assert_eq!(
        engine.refresh_series(id).unwrap().get("submitted"),
        Some(&Value::Number(0.0))
    );
    engine.monitor_series_episode(id, 1, 3, true).unwrap();
    assert_eq!(
        engine.refresh_series(id).unwrap().get("submitted"),
        Some(&Value::Number(1.0))
    );
    let job = lock(&engine.store)
        .unwrap()
        .list()
        .into_iter()
        .find(|job| job.request.episode == 3)
        .unwrap();
    engine.cancel(&job.id).unwrap();
    engine.refresh_series(id).unwrap();
    assert_eq!(
        lock(&engine.store).unwrap().get(&job.id).unwrap().state,
        "cancelled"
    );
    assert_eq!(lock(&engine.store).unwrap().list().len(), 2);
}

#[test]
fn specials_are_explicit_and_series_disable_keeps_existing_jobs() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2024-01-01"), "Regular")]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine.track_series(&request(), false, false).unwrap();
    let id = id(&record);
    assert_eq!(episodes(&record).len(), 1);
    engine.configure_series(id, None, Some(true), None).unwrap();
    engine.refresh_series(id).unwrap();
    assert!(
        lock(&engine.store)
            .unwrap()
            .list()
            .iter()
            .any(|job| job.request.season == 0)
    );
    engine
        .configure_series(id, Some(false), None, None)
        .unwrap();
    catalog.episodes(
        1,
        vec![
            episode(1, 1, Some("2024-01-01"), "Regular"),
            episode(1, 2, Some("2024-01-02"), "New"),
        ],
    );
    engine.refresh_series(id).unwrap();
    assert_eq!(lock(&engine.store).unwrap().list().len(), 2);
    engine.configure_series(id, Some(true), None, None).unwrap();
    engine.refresh_series(id).unwrap();
    assert_eq!(lock(&engine.store).unwrap().list().len(), 3);
}

#[test]
fn series_snapshot_restart_and_read_only_preview_preserve_settings_and_bytes() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let record = engine.track_series(&request(), false, true).unwrap();
    let id = id(&record).to_owned();
    engine.monitor_series_episode(&id, 1, 1, false).unwrap();
    engine
        .configure_series(&id, Some(false), None, None)
        .unwrap();
    let expected = engine.series_record(&id).unwrap();
    let path = cfg.store_dir.join("series.json");
    let bytes = fs::read(&path).unwrap();
    drop(engine);
    let preview = Engine::open_for_preview(cfg.clone()).unwrap();
    assert_eq!(preview.series_record(&id).unwrap(), expected);
    assert!(
        preview
            .configure_series(&id, Some(true), None, None)
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    drop(preview);
    let reopened = Engine::open_for_management(cfg).unwrap();
    assert_eq!(reopened.series_record(&id).unwrap(), expected);
    assert!(lock(&reopened.store).unwrap().list().is_empty());
}

#[test]
fn corrupt_and_linked_snapshots_fail_before_series_data_is_used() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    engine.track_series(&request(), false, true).unwrap();
    drop(engine);
    let path = cfg.store_dir.join("series.json");
    let original = fs::read(&path).unwrap();
    let text = std::str::from_utf8(&original)
        .unwrap()
        .replace("Fixture Series", "Altered Series");
    fs::write(&path, text).unwrap();
    let error = Engine::open_for_management(cfg.clone()).err().unwrap();
    assert!(error.contains("checksum mismatch"));
    fs::write(&path, &original).unwrap();
    #[cfg(unix)]
    {
        let linked = directory.0.join("snapshot-alias");
        fs::hard_link(&path, &linked).unwrap();
        assert!(
            Engine::open_for_management(cfg.clone())
                .err()
                .unwrap()
                .contains("hard links")
        );
        fs::remove_file(linked).unwrap();
        fs::rename(&path, directory.0.join("original-snapshot")).unwrap();
        std::os::unix::fs::symlink(directory.0.join("original-snapshot"), &path).unwrap();
        assert!(Engine::open_for_management(cfg).is_err());
    }
}

#[test]
fn ambiguous_numbering_and_known_episode_identity_changes_never_queue_new_content() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Known")]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine.track_series(&request(), false, false).unwrap();
    let id = id(&record);
    let mut changed = episode(1, 1, Some("2024-01-01"), "Different identity");
    changed.insert("id", 123_u32);
    catalog.episodes(1, vec![changed]);
    assert!(
        engine
            .refresh_series(id)
            .unwrap_err()
            .contains("explicit mapping decision")
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
    let retained = engine.series_record(id).unwrap();
    assert_eq!(episodes(&retained), episodes(&record));
    assert!(
        retained
            .get("last_error")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("mapping decision")
    );
    let mut moved = episode(1, 2, Some("2024-01-01"), "Renumbered");
    moved.insert("id", 11001_u32);
    catalog.episodes(1, vec![moved]);
    assert!(
        engine
            .refresh_series(id)
            .unwrap_err()
            .contains("explicit mapping decision")
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
    catalog.episodes(
        1,
        vec![
            episode(1, 1, Some("2024-01-01"), "Duplicate"),
            episode(1, 1, Some("2024-01-01"), "Duplicate"),
        ],
    );
    assert!(engine.refresh_series(id).is_err());
    assert!(lock(&engine.store).unwrap().list().is_empty());
    catalog.episodes(1, vec![episode(1, 1, Some("2024-01-01"), "Known")]);
    engine.refresh_series(id).unwrap();
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
}

#[test]
fn missing_catalog_episode_identity_requires_mapping_before_automatic_acquisition() {
    let directory = Directory::new();
    let mut unmapped = episode(1, 1, Some("2024-01-01"), "Unmapped");
    if let Value::Object(fields) = &mut unmapped {
        fields.remove("id");
    }
    let catalog = Catalog::open(vec![unmapped]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    engine.track_series(&request(), false, false).unwrap();
    assert!(lock(&engine.store).unwrap().list().is_empty());
    let calendar = engine
        .episode_calendar(&CalendarQuery::new(Some("2024-01-01"), Some("2024-01-02")).unwrap())
        .unwrap();
    assert_eq!(
        episodes(&calendar)[0].get("state"),
        Some(&Value::String("mapping_required".into()))
    );
}

#[test]
fn catalog_failure_retains_the_last_plan_and_never_leaks_request_urls() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine.track_series(&request(), false, false).unwrap();
    let id = id(&record);
    catalog.response("/3/tv/42", 503, Value::object());
    assert!(engine.refresh_series(id).is_err());
    let failed = engine.series_record(id).unwrap();
    assert_eq!(episodes(&failed), episodes(&record));
    let text = json::stringify(&failed);
    assert!(!text.contains(&catalog.url));
    assert!(!text.contains("Bearer "));
    assert!(text.contains("HTTP response 503"));
}

#[test]
fn acquisitions_are_batched_and_restart_or_overlapping_scope_never_duplicates_jobs() {
    let directory = Directory::new();
    let catalog = Catalog::open(
        (1..=100)
            .map(|number| episode(1, number, Some("2024-01-01"), "Backlog"))
            .collect(),
    );
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let record = engine.track_series(&request(), false, false).unwrap();
    let id = id(&record).to_owned();
    assert_eq!(record.get("submitted"), Some(&Value::Number(64.0)));
    assert_eq!(lock(&engine.store).unwrap().list().len(), 64);
    drop(engine);
    let engine = Engine::open_for_management(cfg).unwrap();
    assert_eq!(
        engine.refresh_series(&id).unwrap().get("submitted"),
        Some(&Value::Number(36.0))
    );
    let mut scope = request();
    scope.season = 1;
    engine.track_series(&scope, false, false).unwrap();
    assert_eq!(lock(&engine.store).unwrap().list().len(), 100);
    assert_eq!(
        engine.refresh_series(&id).unwrap().get("submitted"),
        Some(&Value::Number(0.0))
    );
}

#[test]
fn monitoring_revision_discards_a_catalog_response_that_arrives_after_disable() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine.track_series(&request(), false, false).unwrap();
    let id = id(&record).to_owned();
    catalog.episodes(1, vec![episode(1, 1, Some("2024-01-01"), "Now aired")]);
    let calls = catalog.calls.load(Ordering::Acquire);
    catalog.blocked.store(true, Ordering::Release);
    let worker_engine = engine.clone();
    let worker_id = id.clone();
    let worker = thread::spawn(move || worker_engine.refresh_series(&worker_id));
    catalog.wait_for_calls(calls);
    engine
        .configure_series(&id, Some(false), None, None)
        .unwrap();
    catalog.blocked.store(false, Ordering::Release);
    assert!(
        worker
            .join()
            .unwrap()
            .unwrap_err()
            .contains("result discarded")
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
    assert_eq!(
        engine.series_record(&id).unwrap().get("monitored"),
        Some(&Value::Bool(false))
    );
}

#[test]
fn calendar_date_bounds_pagination_and_series_scope_are_strict_and_read_only() {
    let directory = Directory::new();
    let catalog = Catalog::open(
        (1..=80)
            .map(|number| episode(1, number, Some("2024-02-29"), "Leap day"))
            .collect(),
    );
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let record = engine.track_series(&request(), false, true).unwrap();
    let snapshot = fs::read(cfg.store_dir.join("series.json")).unwrap();
    let mut query = CalendarQuery::parse(&format!(
        "from=2024-02-29&to=2024-03-01&series_id={}&offset=50&limit=20",
        id(&record)
    ))
    .unwrap();
    let value = engine.episode_calendar(&query).unwrap();
    assert_eq!(value.get("total"), Some(&Value::Number(80.0)));
    assert_eq!(episodes(&value).len(), 20);
    assert_eq!(
        episodes(&value)[0].get("episode"),
        Some(&Value::Number(51.0))
    );
    assert_eq!(
        fs::read(cfg.store_dir.join("series.json")).unwrap(),
        snapshot
    );
    for text in [
        "from=2024-02-30",
        "from=2024-01-01&to=2026-01-01",
        "limit=201",
        "limit=0",
        "offset=20001",
        "series_id=invalid",
        "from=2024-01-01&from=2024-01-02",
        "redirect=evil",
    ] {
        assert!(CalendarQuery::parse(text).is_err(), "Accepted {text}");
    }
    query.series_id = Some("f".repeat(32));
    assert!(episodes(&engine.episode_calendar(&query).unwrap()).is_empty());
}

#[test]
fn plex_series_watchlist_creates_persistent_monitoring_beyond_watchlist_removal() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    catalog.response("/watchlist", 200, json::parse(r#"{"MediaContainer":{"Metadata":[{"type":"show","title":"Fixture Series","year":2024,"Guid":[{"id":"tmdb://42"}]}]}}"#).unwrap());
    let mut cfg = catalog.config(&directory.0);
    cfg.plex.enabled = true;
    cfg.plex.url = catalog.url.clone();
    cfg.plex.watchlist_url = format!("{}/watchlist", catalog.url);
    cfg.plex.token_override = Some("synthetic-series-plex-token".into());
    let engine = Engine::open_for_management(cfg).unwrap();
    engine.sync().unwrap();
    assert_eq!(engine.series().unwrap().as_array().unwrap().len(), 1);
    catalog.response(
        "/watchlist",
        200,
        json::parse(r#"{"MediaContainer":{"Metadata":[]}}"#).unwrap(),
    );
    engine.sync().unwrap();
    assert_eq!(
        engine.series().unwrap().as_array().unwrap()[0].get("monitored"),
        Some(&Value::Bool(true))
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
}
