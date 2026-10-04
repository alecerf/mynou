//! Original explicit numbering, identity retention and restart scenarios. CI only.
mod library_support;
mod series_support;
use library_support::{Directory, Indexer};
use mynou::{
    crypto::sha256,
    engine::{Engine, lock},
    integrations,
    json::{self, Value},
    numbering::{EpisodeNumber, SourceNumber},
    pack::AutoPackRequest,
    series::{NumberingChoice, NumberingRequest},
    store::{Request, Store},
};
use series_support::{Catalog, episode, episodes, id, request};
use std::{fs, sync::atomic::Ordering, thread};

fn number(season: u32, episode: u32) -> EpisodeNumber {
    EpisodeNumber { season, episode }
}
fn choice(catalog_id: u64, catalog: EpisodeNumber, source: SourceNumber) -> NumberingChoice {
    NumberingChoice {
        catalog_id,
        catalog,
        source,
    }
}
fn query(changes: Vec<NumberingChoice>) -> NumberingRequest {
    NumberingRequest {
        changes,
        apply: false,
        plan_id: None,
    }
}
fn apply(engine: &Engine, id: &str, mut query: NumberingRequest) -> Value {
    let report = engine.series_numbering(id, &query).unwrap();
    assert_eq!(
        report.get("resolved"),
        Some(&Value::Bool(true)),
        "{}",
        json::stringify(&report)
    );
    query.apply = true;
    query.plan_id = Some(report.get("plan_id").unwrap().as_str().unwrap().into());
    let result = engine.series_numbering(id, &query).unwrap();
    assert_eq!(result.get("applied"), Some(&Value::Bool(true)));
    result
}
fn moved(season: u32, number: u32, catalog_id: u32, air: &str) -> Value {
    let mut value = episode(season, number, Some(air), "Retained identity");
    value.insert("id", catalog_id);
    value
}

#[test]
fn explicit_source_labels_preserve_request_keys_and_durable_job_snapshots() {
    let directory = Directory::new();
    let mut original = request();
    original.kind = "episode".into();
    original.season = 1;
    original.episode = 2;
    let key = original.canonical_key();
    let media_key = original.media_key();
    let legacy = original.to_json();
    assert!(legacy.get("source_numbering").is_none());
    assert_eq!(Request::from_json(&legacy).unwrap(), original);
    original.source_numbering = Some(SourceNumber::Absolute(13));
    assert_eq!(original.canonical_key(), key);
    assert_eq!(original.media_key(), media_key);
    assert_eq!(Request::from_json(&original.to_json()).unwrap(), original);
    let mut store = Store::open(&directory.0).unwrap();
    let job = store.submit(original.clone()).unwrap();
    original.source_numbering = Some(SourceNumber::SeasonEpisode(number(2, 1)));
    assert_eq!(store.submit(original).unwrap(), job);
    drop(store);
    let reopened = Store::open(&directory.0).unwrap();
    assert_eq!(reopened.get(&job.id).unwrap(), job);
}

#[test]
fn source_numbering_rejects_invalid_fields_and_never_guesses_absolute_titles() {
    for text in [
        "null",
        "{}",
        r#"{"absolute":0}"#,
        r#"{"absolute":100000}"#,
        r#"{"absolute":1.5}"#,
        r#"{"absolute":13,"season":1}"#,
        r#"{"season":1,"episode":0}"#,
        r#"{"season":-1,"episode":1}"#,
        r#"{"season":1,"episode":1,"extra":true}"#,
    ] {
        assert!(
            SourceNumber::from_json(&json::parse(text).unwrap()).is_err(),
            "{text}"
        );
    }
    let mut request = request();
    request.kind = "episode".into();
    request.season = 1;
    request.episode = 2;
    assert!(!integrations::release_identity_matches(
        &request,
        "Fixture Series 013 1080p"
    ));
    request.source_numbering = Some(SourceNumber::Absolute(13));
    for title in [
        "Fixture Series 013 1080p",
        "Fixture Series 2024 13 WEB-DL x264",
    ] {
        assert!(
            integrations::release_identity_matches(&request, title),
            "{title}"
        );
    }
    for title in [
        "Fixture Series 014 1080p",
        "Fixture Series 13-14 1080p",
        "Fixture Series 13 S01E13 1080p",
        "Fixture Series 13 E14 1080p",
        "Fixture Series 13 13 1080p",
        "Fixture Series S01E02 1080p",
        "Fixture Series 2024 1080p",
    ] {
        assert!(
            !integrations::release_identity_matches(&request, title),
            "{title}"
        );
    }
    request.source_numbering = Some(SourceNumber::SeasonEpisode(number(2, 3)));
    assert!(integrations::release_identity_matches(
        &request,
        "Fixture Series S02E03 1080p"
    ));
    assert!(!integrations::release_identity_matches(
        &request,
        "Fixture Series S01E02 1080p"
    ));
    request.kind = "movie".into();
    assert!(request.validate().is_err());
}

#[test]
fn numbering_preview_is_read_only_online_and_after_restart() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let id = id(&record).to_owned();
    let snapshot = fs::read(cfg.store_dir.join("series.json")).unwrap();
    let journal = fs::read(cfg.store_dir.join("journal.bin")).unwrap();
    let choices = query(vec![choice(
        11001,
        number(1, 1),
        SourceNumber::Absolute(13),
    )]);
    let preview = engine.series_numbering(&id, &choices).unwrap();
    assert_eq!(preview.get("resolved"), Some(&Value::Bool(true)));
    assert_eq!(
        engine.series_record(&id).unwrap(),
        record_with_no_submission(record.clone())
    );
    assert_eq!(
        fs::read(cfg.store_dir.join("series.json")).unwrap(),
        snapshot
    );
    assert_eq!(
        fs::read(cfg.store_dir.join("journal.bin")).unwrap(),
        journal
    );
    drop(engine);
    let reader = Engine::open_for_preview(cfg.clone()).unwrap();
    assert_eq!(reader.series_numbering(&id, &choices).unwrap(), preview);
    let mut forbidden = choices.clone();
    forbidden.apply = true;
    forbidden.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    assert!(
        reader
            .series_numbering(&id, &forbidden)
            .unwrap_err()
            .contains("writable")
    );
    assert_eq!(
        fs::read(cfg.store_dir.join("series.json")).unwrap(),
        snapshot
    );
    assert_eq!(
        fs::read(cfg.store_dir.join("journal.bin")).unwrap(),
        journal
    );
}

fn record_with_no_submission(mut value: Value) -> Value {
    if let Value::Object(fields) = &mut value {
        fields.remove("submitted");
    }
    value
}

#[test]
fn approved_catalog_renumbering_keeps_jobs_exclusions_and_library_numbers() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![
        episode(1, 1, Some("2024-01-01"), "Existing"),
        episode(1, 2, Some("2200-01-01"), "Future"),
    ]);
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let record = engine.track_series(&request(), false, false).unwrap();
    let id = id(&record).to_owned();
    engine.monitor_series_episode(&id, 1, 2, false).unwrap();
    let existing = lock(&engine.store).unwrap().list();
    catalog.episodes(
        1,
        vec![
            moved(1, 10, 11001, "2024-01-01"),
            moved(1, 11, 11002, "2024-01-01"),
        ],
    );
    assert!(engine.refresh_series(&id).is_err());
    let unresolved = engine.series_numbering(&id, &query(Vec::new())).unwrap();
    assert_eq!(unresolved.get("resolved"), Some(&Value::Bool(false)));
    apply(
        &engine,
        &id,
        query(vec![
            choice(
                11001,
                number(1, 10),
                SourceNumber::SeasonEpisode(number(1, 10)),
            ),
            choice(11002, number(1, 11), SourceNumber::Absolute(14)),
        ]),
    );
    let retained = engine.series_record(&id).unwrap();
    assert_eq!(
        episodes(&retained)[0].get("episode"),
        Some(&Value::Number(1.0))
    );
    assert_eq!(
        episodes(&retained)[1].get("episode"),
        Some(&Value::Number(2.0))
    );
    assert_eq!(
        episodes(&retained)[1].get("excluded"),
        Some(&Value::Bool(true))
    );
    assert_eq!(lock(&engine.store).unwrap().list(), existing);
    engine.refresh_series(&id).unwrap();
    assert_eq!(lock(&engine.store).unwrap().list(), existing);
    engine.monitor_series_episode(&id, 1, 2, true).unwrap();
    assert_eq!(
        engine.refresh_series(&id).unwrap().get("submitted"),
        Some(&Value::Number(1.0))
    );
    let jobs = lock(&engine.store).unwrap().list();
    let new = jobs
        .iter()
        .find(|job| job.request.episode == 2)
        .unwrap()
        .clone();
    assert_eq!(
        new.request.source_numbering,
        Some(SourceNumber::Absolute(14))
    );
    let source = directory.0.join("013.mp4");
    fs::write(&source, b"synthetic numbering fixture").unwrap();
    let layout = mynou::organizer::import_file(&source, &cfg.series_root, &new.request).unwrap();
    assert!(layout.to_string_lossy().contains("S01E02"));
    drop(engine);
    let reopened = Engine::open_for_management(cfg).unwrap();
    assert_eq!(
        reopened.refresh_series(&id).unwrap().get("submitted"),
        Some(&Value::Number(0.0))
    );
    assert_eq!(
        lock(&reopened.store).unwrap().get(&new.id).unwrap().request,
        new.request
    );
}

#[test]
fn stale_numbering_guard_rejects_changed_policy_catalog_and_choices_without_writes() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let id = id(&record);
    let mut choices = query(vec![choice(
        11001,
        number(1, 1),
        SourceNumber::Absolute(13),
    )]);
    let preview = engine.series_numbering(id, &choices).unwrap();
    choices.apply = true;
    choices.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    engine
        .configure_series(id, Some(false), None, None)
        .unwrap();
    let before = fs::read(cfg.store_dir.join("series.json")).unwrap();
    assert!(
        engine
            .series_numbering(id, &choices)
            .unwrap_err()
            .contains("stale")
    );
    assert_eq!(fs::read(cfg.store_dir.join("series.json")).unwrap(), before);
    choices.apply = false;
    choices.plan_id = None;
    let preview = engine.series_numbering(id, &choices).unwrap();
    choices.apply = true;
    choices.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    catalog.episodes(
        1,
        vec![episode(1, 1, Some("2024-01-01"), "Changed metadata")],
    );
    assert!(
        engine
            .series_numbering(id, &choices)
            .unwrap_err()
            .contains("stale")
    );
    assert_eq!(fs::read(cfg.store_dir.join("series.json")).unwrap(), before);
    choices.apply = false;
    choices.plan_id = None;
    let preview = engine.series_numbering(id, &choices).unwrap();
    choices.apply = true;
    choices.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    choices.changes[0].source = SourceNumber::Absolute(14);
    assert!(
        engine
            .series_numbering(id, &choices)
            .unwrap_err()
            .contains("stale")
    );
    assert_eq!(fs::read(cfg.store_dir.join("series.json")).unwrap(), before);
    assert!(lock(&engine.store).unwrap().list().is_empty());
}

#[test]
fn conflicting_source_labels_require_a_resolved_atomic_decision() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![
        episode(1, 1, Some("2200-01-01"), "One"),
        episode(1, 2, Some("2200-01-01"), "Two"),
    ]);
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let id = id(&record);
    let before = fs::read(cfg.store_dir.join("series.json")).unwrap();
    let mut conflicting = query(vec![choice(
        11001,
        number(1, 1),
        SourceNumber::SeasonEpisode(number(1, 2)),
    )]);
    let report = engine.series_numbering(id, &conflicting).unwrap();
    assert_eq!(report.get("resolved"), Some(&Value::Bool(false)));
    conflicting.apply = true;
    conflicting.plan_id = Some(report.get("plan_id").unwrap().as_str().unwrap().into());
    assert!(engine.series_numbering(id, &conflicting).is_err());
    assert_eq!(fs::read(cfg.store_dir.join("series.json")).unwrap(), before);
    apply(
        &engine,
        id,
        query(vec![
            choice(
                11001,
                number(1, 1),
                SourceNumber::SeasonEpisode(number(1, 2)),
            ),
            choice(
                11002,
                number(1, 2),
                SourceNumber::SeasonEpisode(number(1, 1)),
            ),
        ]),
    );
    assert_eq!(
        engine
            .series_record(id)
            .unwrap()
            .get("numbering")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let pack = AutoPackRequest {
        season: 1,
        apply: false,
        scope_id: None,
        candidate_id: None,
    };
    assert!(
        engine
            .search_packs(id, &pack)
            .unwrap_err()
            .contains("explicit pack")
    );
}

#[test]
fn retained_identity_history_blocks_replacement_after_disappearance_and_overlapping_scopes() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Known")]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let id = id(&record);
    catalog.episodes(1, Vec::new());
    engine.refresh_series(id).unwrap();
    assert!(episodes(&engine.series_record(id).unwrap()).is_empty());
    catalog.episodes(1, vec![moved(1, 1, 999, "2024-01-01")]);
    assert!(
        engine
            .refresh_series(id)
            .unwrap_err()
            .contains("cannot be reassigned")
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
    catalog.episodes(1, vec![moved(1, 10, 11001, "2024-01-01")]);
    let mut other = request();
    other.season = 1;
    assert!(
        engine
            .track_series_with_policy(&other, false, false, false)
            .unwrap_err()
            .contains("Overlapping")
    );
    apply(
        &engine,
        id,
        query(vec![choice(
            11001,
            number(1, 10),
            SourceNumber::SeasonEpisode(number(1, 10)),
        )]),
    );
    assert_eq!(
        episodes(&engine.series_record(id).unwrap())[0].get("episode"),
        Some(&Value::Number(1.0))
    );
}

#[test]
fn cross_season_catalog_moves_keep_the_original_scope() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Known")]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let mut scoped = request();
    scoped.season = 1;
    let record = engine
        .track_series_with_policy(&scoped, false, false, false)
        .unwrap();
    let id = id(&record);
    catalog.response("/3/tv/42",200,json::parse(r#"{"id":42,"name":"Fixture Series","first_air_date":"2024-01-01","seasons":[{"season_number":0},{"season_number":1},{"season_number":2}]}"#).unwrap());
    catalog.episodes(1, Vec::new());
    catalog.episodes(2, vec![moved(2, 3, 11001, "2024-01-01")]);
    apply(
        &engine,
        id,
        query(vec![choice(
            11001,
            number(2, 3),
            SourceNumber::SeasonEpisode(number(2, 3)),
        )]),
    );
    engine.refresh_series(id).unwrap();
    let retained = engine.series_record(id).unwrap();
    assert_eq!(episodes(&retained).len(), 1);
    assert_eq!(
        episodes(&retained)[0].get("season"),
        Some(&Value::Number(1.0))
    );
    assert_eq!(
        episodes(&retained)[0].get("episode"),
        Some(&Value::Number(1.0))
    );
    assert!(lock(&engine.store).unwrap().list().is_empty());
}

#[test]
fn a_late_catalog_result_cannot_apply_after_monitoring_changes() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let id = id(&record).to_owned();
    let changes = query(vec![choice(
        11001,
        number(1, 1),
        SourceNumber::Absolute(13),
    )]);
    let before = catalog.calls.load(Ordering::Acquire);
    catalog.blocked.store(true, Ordering::Release);
    let worker_engine = engine.clone();
    let worker_id = id.clone();
    let worker = thread::spawn(move || worker_engine.series_numbering(&worker_id, &changes));
    catalog.wait_for_calls(before);
    engine
        .configure_series(&id, Some(false), None, None)
        .unwrap();
    let expected = engine.series_record(&id).unwrap();
    catalog.blocked.store(false, Ordering::Release);
    assert!(
        worker
            .join()
            .unwrap()
            .unwrap_err()
            .contains("changed during")
    );
    assert_eq!(engine.series_record(&id).unwrap(), expected);
}

fn rewrite_snapshot(path: &std::path::Path, envelope: &mut Value) {
    let digest = sha256(json::stringify(envelope.get("records").unwrap()).as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    envelope.insert("digest", digest);
    fs::write(path, json::stringify(envelope)).unwrap();
}

#[test]
fn legacy_snapshots_are_inferred_read_only_then_versioned_on_explicit_save() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let id = id(&record).to_owned();
    drop(engine);
    let path = cfg.store_dir.join("series.json");
    let mut envelope = json::parse(&fs::read_to_string(&path).unwrap()).unwrap();
    if let Some(Value::Array(records)) = envelope.get_mut("records") {
        for record in records {
            if let Value::Object(fields) = record {
                fields.remove("anchors");
                fields.remove("numbering");
            }
        }
    }
    envelope.insert("schema_version", 1_u32);
    rewrite_snapshot(&path, &mut envelope);
    let bytes = fs::read(&path).unwrap();
    let reader = Engine::open_for_preview(cfg.clone()).unwrap();
    assert_eq!(
        reader
            .series_record(&id)
            .unwrap()
            .get("anchors")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    reader.series_numbering(&id, &query(Vec::new())).unwrap();
    drop(reader);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let writer = Engine::open_for_management(cfg.clone()).unwrap();
    apply(
        &writer,
        &id,
        query(vec![choice(
            11001,
            number(1, 1),
            SourceNumber::Absolute(13),
        )]),
    );
    drop(writer);
    let upgraded = json::parse(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(upgraded.get("schema_version"), Some(&Value::Number(2.0)));
    let reopened = Engine::open_for_preview(cfg).unwrap();
    assert_eq!(
        reopened
            .series_record(&id)
            .unwrap()
            .get("numbering")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn verified_but_invalid_numbering_data_is_rejected_before_use() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    drop(engine);
    let path = cfg.store_dir.join("series.json");
    let valid = json::parse(&fs::read_to_string(&path).unwrap()).unwrap();
    for mode in ["missing", "duplicate", "wrong-owner", "unknown-choice"] {
        let mut envelope = valid.clone();
        let Some(Value::Array(records)) = envelope.get_mut("records") else {
            panic!("records");
        };
        match mode {
            "missing" => {
                if let Value::Object(fields) = &mut records[0] {
                    fields.remove("anchors");
                }
            }
            "duplicate" => {
                let anchor = records[0].get("anchors").unwrap().as_array().unwrap()[0].clone();
                records[0].insert("anchors", Value::Array(vec![anchor.clone(), anchor]));
            }
            "wrong-owner" => {
                records[0].insert(
                    "anchors",
                    json::parse(r#"[{"catalog_id":999,"season":1,"episode":1}]"#).unwrap(),
                );
            }
            "unknown-choice" => {
                records[0].insert(
                    "numbering",
                    Value::Array(vec![
                        choice(999, number(1, 1), SourceNumber::Absolute(13)).to_json(),
                    ]),
                );
            }
            _ => unreachable!(),
        }
        rewrite_snapshot(&path, &mut envelope);
        assert!(Engine::open_for_preview(cfg.clone()).is_err(), "{mode}");
    }
}

#[test]
fn source_search_uses_only_the_approved_labels_under_the_existing_profile() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let indexer=Indexer::open(json::parse(r#"[{"title":"Fixture Series 013 1080p WEB-DL x264","download_url":"magnet:?xt=urn:btih:0123456789012345678901234567890123456789","seeders":20},{"title":"Fixture Series S01E01 1080p WEB-DL x264","download_url":"magnet:?xt=urn:btih:1123456789012345678901234567890123456789","seeders":30}]"#).unwrap());
    let mut cfg = catalog.config(&directory.0);
    cfg.sources.push(mynou::config::Source {
        name: "Local".into(),
        kind: "json".into(),
        url: indexer.url.clone(),
        api_key_env: "MYNOU_NUMBERING_ABSENT_KEY_91".into(),
    });
    let engine = Engine::open_for_management(cfg).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let id = id(&record);
    apply(
        &engine,
        id,
        query(vec![choice(
            11001,
            number(1, 1),
            SourceNumber::Absolute(13),
        )]),
    );
    engine.configure_series(id, Some(true), None, None).unwrap();
    catalog.episodes(1, vec![episode(1, 1, Some("2024-01-01"), "Aired")]);
    engine.refresh_series(id).unwrap();
    let job = lock(&engine.store).unwrap().list()[0].clone();
    assert_eq!(
        job.request.source_numbering,
        Some(SourceNumber::Absolute(13))
    );
    let report = integrations::search_report(&engine.config, &job.request).unwrap();
    assert_eq!(report.get("accepted").unwrap().as_array().unwrap().len(), 1);
    assert_eq!(report.get("rejected").unwrap().as_array().unwrap().len(), 1);
    assert!(!json::stringify(&report).contains("magnet:?"));
}

#[test]
fn numbering_decisions_validate_duplicates_unknown_ids_and_apply_guards() {
    let one = choice(11001, number(1, 1), SourceNumber::Absolute(13));
    assert!(query(vec![one, one]).validate().is_err());
    assert!(query(vec![one; 2001]).validate().is_err());
    for text in [
        r#"{"changes":[],"apply":true}"#,
        r#"{"changes":[],"plan_id":"aaaaaaaa"}"#,
        r#"{"changes":[],"unknown":true}"#,
        r#"{"changes":[{"catalog_id":1,"catalog":{"season":1,"episode":1},"source":{"absolute":1},"unknown":true}]}"#,
    ] {
        assert!(
            NumberingRequest::from_json(&json::parse(text).unwrap()).is_err(),
            "{text}"
        );
    }
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2200-01-01"), "Future")]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    assert!(
        engine
            .series_numbering(
                id(&record),
                &query(vec![choice(999, number(1, 1), SourceNumber::Absolute(13))])
            )
            .is_err()
    );
    assert!(
        engine
            .series_numbering(
                id(&record),
                &query(vec![choice(
                    11001,
                    number(1, 2),
                    SourceNumber::Absolute(13)
                )])
            )
            .is_err()
    );
}

#[test]
fn approved_numbering_does_not_rewrite_ready_library_records_or_imported_bytes() {
    let directory = Directory::new();
    let catalog = Catalog::open(vec![episode(1, 1, Some("2024-01-01"), "Known")]);
    let cfg = catalog.config(&directory.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let id = id(&record);
    let source = directory.0.join("original-S01E01.mp4");
    fs::write(&source, include_bytes!("../examples/demo.mp4")).unwrap();
    let mut request = request();
    request.kind = "episode".into();
    request.season = 1;
    request.episode = 1;
    request.source_path = Some(source.to_str().unwrap().into());
    let job = engine.submit(request).unwrap().remove(0);
    let ready = library_support::run_until(&engine, &job.id, "ready");
    let owned = engine.library().unwrap();
    let imports = library_support::files(&cfg.series_root);
    catalog.episodes(1, vec![moved(1, 10, 11001, "2024-01-01")]);
    apply(
        &engine,
        id,
        query(vec![choice(
            11001,
            number(1, 10),
            SourceNumber::Absolute(13),
        )]),
    );
    engine.configure_series(id, Some(true), None, None).unwrap();
    assert_eq!(
        engine.refresh_series(id).unwrap().get("submitted"),
        Some(&Value::Number(0.0))
    );
    assert_eq!(engine.library().unwrap(), owned);
    assert_eq!(lock(&engine.store).unwrap().get(&ready.id).unwrap(), ready);
    assert_eq!(library_support::files(&cfg.series_root), imports);
    assert_eq!(
        fs::read(&source).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
}

#[test]
fn original_source_queries_use_explicit_season_labels_or_absolute_search_terms() {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let worker = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..4 {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "Source query fixture timed out"
                        );
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => panic!("Source query accept failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            requests.push(first);
            loop {
                let mut line = String::new();
                assert!(
                    reader.read_line(&mut line).unwrap() > 0,
                    "Source query header ended early"
                );
                if line == "\r\n" {
                    break;
                }
            }
            let body = if requests.len() <= 2 {
                "[]"
            } else {
                "<rss><channel></channel></rss>"
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
        requests
    });
    let directory = Directory::new();
    let mut cfg = library_support::config(&directory.0, None, "{}");
    cfg.sources.push(mynou::config::Source {
        name: "Local".into(),
        kind: "json".into(),
        url,
        api_key_env: "MYNOU_NUMBERING_ABSENT_SOURCE_KEY_912".into(),
    });
    let mut request = request();
    request.kind = "episode".into();
    request.season = 1;
    request.episode = 1;
    for kind in ["json", "torznab"] {
        cfg.sources[0].kind = kind.into();
        for numbering in [
            SourceNumber::SeasonEpisode(number(2, 3)),
            SourceNumber::Absolute(13),
        ] {
            request.source_numbering = Some(numbering);
            integrations::search_report(&cfg, &request).unwrap();
        }
    }
    let requests = worker.join().unwrap();
    assert!(requests[0].contains("season=2") && requests[0].contains("episode=3"));
    assert!(requests[1].contains("absolute=13") && requests[1].contains("Fixture%20Series%20013"));
    assert!(
        requests[2].contains("t=tvsearch")
            && requests[2].contains("season=2")
            && requests[2].contains("ep=3")
    );
    assert!(
        requests[3].contains("Fixture%20Series%20013")
            && !requests[3].contains("season=")
            && !requests[3].contains("ep=")
    );
}
