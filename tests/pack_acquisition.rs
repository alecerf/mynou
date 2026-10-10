//! Explicit pack mappings through catalog, durable jobs and verified local peers. CI only.
mod library_support;
mod series_support;
#[allow(dead_code)]
mod transfer_support;
use library_support::{Directory, run_until};
use mynou::{
    engine::{Engine, lock},
    json::{self, Value},
    pack::{PackEpisode, PackSubmission},
    store::Job,
};
use series_support::{Catalog, episode, id, request};
use std::{fs, path::PathBuf, sync::atomic::Ordering};
use transfer_support::{RecordingProxy, Seeder, Torrent, engine_config, wait};

fn mapping(source: &str, count: u32) -> PackSubmission {
    PackSubmission {
        source_url: source.into(),
        episodes: (1..=count)
            .map(|episode| PackEpisode {
                season: 1,
                episode,
                file_path: format!("Pack/{episode:03}.mp4"),
            })
            .collect(),
    }
}
fn catalog(count: u32) -> Catalog {
    Catalog::open(
        (1..=count)
            .map(|n| episode(1, n, Some("2024-01-01"), "Catalog episode"))
            .collect(),
    )
}
fn jobs(value: &Value) -> Vec<String> {
    value
        .get("jobs")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|job| id(job).to_owned())
        .collect()
}
fn torrent(directory: &Directory, count: u32) -> Torrent {
    let media = include_bytes!("../examples/demo.mp4").to_vec();
    let mut files: Vec<_> = (1..=count)
        .map(|n| (PathBuf::from(format!("{n:03}.mp4")), media.clone()))
        .collect();
    // An unmapped, larger invalid video must neither be analyzed nor selected for import.
    files.push((
        PathBuf::from("unmapped-sample.mp4"),
        vec![0; media.len() * 2],
    ));
    Torrent::multiple(&directory.0.join("metadata"), "Pack", files)
}
fn native_config(directory: &Directory, catalog: &Catalog) -> mynou::config::Config {
    let mut cfg = catalog.config(&directory.0.join("engine"));
    cfg.downloads = engine_config(&directory.0.join("engine")).downloads;
    cfg.downloads_enabled = true;
    cfg.max_attempts = 1;
    cfg.workers = 1;
    cfg
}

#[test]
fn two_absolute_named_episodes_share_one_verified_pack_and_survive_restart() {
    let directory = Directory::new();
    let catalog = catalog(2);
    let torrent = torrent(&directory, 2);
    let seed = Seeder::open(&directory.0.join("seed"), &[&torrent]);
    let cfg = native_config(&directory, &catalog);
    let engine = Engine::open(cfg.clone()).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    assert!(lock(&engine.store).unwrap().list().is_empty());
    let pack = mapping(&torrent.magnet(seed.client.listen_port()), 2);
    let report = engine.submit_pack(id(&record), &pack).unwrap();
    assert_eq!(report.get("submitted"), Some(&Value::Number(2.0)));
    assert!(!json::stringify(&report).contains("x.pe="));
    let ids = jobs(&report);
    for (n, id) in ids.iter().enumerate() {
        let ready = run_until(&engine, id, "ready");
        assert_eq!(ready.download_id.as_deref(), Some(torrent.id.as_str()));
        assert_eq!(ready.files.len(), 1);
        assert_eq!(
            ready.pack_file.as_deref(),
            Some(pack.episodes[n].file_path.as_str())
        );
        assert_eq!(
            fs::read(&ready.imports[0]).unwrap(),
            include_bytes!("../examples/demo.mp4")
        );
        assert!(ready.imports[0].contains(&format!("S01E{:02}", n + 1)));
        // The import must not give the native payload a second name: the
        // client refuses to read a media file whose link count is not one.
        assert_eq!(
            std::os::unix::fs::MetadataExt::nlink(&fs::metadata(&ready.files[0]).unwrap()),
            1
        );
    }
    assert_eq!(engine.transfers().unwrap().as_array().unwrap().len(), 1);
    assert_eq!(
        engine
            .transfer(&torrent.id)
            .unwrap()
            .get("files")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let saved: Vec<_> = ids
        .iter()
        .map(|id| lock(&engine.store).unwrap().get(id).unwrap())
        .collect();
    drop(engine);
    let engine = Engine::open(cfg.clone()).unwrap();
    for job in saved {
        assert_eq!(lock(&engine.store).unwrap().get(&job.id).unwrap(), job);
    }
    let reused = engine.submit_pack(id(&record), &pack).unwrap();
    assert_eq!(reused.get("submitted"), Some(&Value::Number(0.0)));
    assert_eq!(jobs(&reused), ids);
    drop(engine);
    let preview = Engine::open_for_preview(cfg).unwrap();
    assert!(preview.submit_pack(id(&record), &pack).is_err());
}

#[test]
fn mapped_episodes_import_from_a_partial_torrent_and_cancellation_retains_shared_interests() {
    let directory = Directory::new();
    let catalog = catalog(2);
    let media = include_bytes!("../examples/demo.mp4").to_vec();
    let torrent = Torrent::multiple(
        &directory.0.join("metadata"),
        "Pack",
        vec![
            ("001.mp4".into(), media.clone()),
            ("002.mp4".into(), media),
            (
                "unmapped-sample.mp4".into(),
                transfer_support::payload(transfer_support::BLOCK * 8, 43),
            ),
            (
                "untouched.txt".into(),
                transfer_support::payload(transfer_support::BLOCK, 59),
            ),
        ],
    );
    let seed = Seeder::open(&directory.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let cfg = native_config(&directory, &catalog);
    let engine = Engine::open(cfg.clone()).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let report = engine
        .submit_pack(id(&record), &mapping(&torrent.magnet(proxy.port), 2))
        .unwrap();
    let ids = jobs(&report);
    wait(|| {
        engine.tick().unwrap();
        lock(&engine.store)
            .unwrap()
            .list()
            .iter()
            .all(|job| job.download_id.is_some())
    });
    engine.cancel(&ids[0]).unwrap();
    proxy.payloads_enabled.store(true, Ordering::Release);
    let imported = run_until(&engine, &ids[1], "ready");
    assert_eq!(
        fs::read(&imported.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
    assert_eq!(
        lock(&engine.store).unwrap().get(&ids[0]).unwrap().state,
        "cancelled"
    );
    let native = engine.transfer(&torrent.id).unwrap();
    assert_eq!(native.get("ready"), Some(&Value::Bool(false)));
    assert_eq!(native.get("selected_ready"), Some(&Value::Bool(true)));
    assert_eq!(native.get("user_paused"), Some(&Value::Bool(false)));
    let required = (2 * include_bytes!("../examples/demo.mp4").len())
        .div_ceil(transfer_support::BLOCK)
        * transfer_support::BLOCK;
    assert_eq!(
        native.get("downloaded_bytes"),
        Some(&Value::String(required.to_string()))
    );
    assert!(
        !cfg.downloads
            .data_dir
            .join(&torrent.id)
            .join("Pack/untouched.txt")
            .exists()
    );
    assert_eq!(engine.library().unwrap().as_array().unwrap().len(), 1);
    drop(engine);
    proxy.wait_idle();
    let engine = Engine::open(cfg).unwrap();
    let retained = lock(&engine.store).unwrap().get(&ids[1]).unwrap();
    assert_eq!(retained, imported);
    assert_eq!(engine.transfers().unwrap().as_array().unwrap().len(), 1);
}

#[test]
fn absent_mapped_file_never_falls_back_to_another_video() {
    let directory = Directory::new();
    let catalog = catalog(1);
    let torrent = torrent(&directory, 1);
    let seed = Seeder::open(&directory.0.join("seed"), &[&torrent]);
    let cfg = native_config(&directory, &catalog);
    let engine = Engine::open(cfg).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let mut pack = mapping(&torrent.magnet(seed.client.listen_port()), 1);
    pack.episodes[0].file_path = "Pack/absent.mp4".into();
    let report = engine.submit_pack(id(&record), &pack).unwrap();
    let job = run_until(&engine, &jobs(&report)[0], "failed");
    assert!(job.imports.is_empty());
    assert!(job.last_error.unwrap().contains("absent or ambiguous"));
    assert!(engine.library().unwrap().as_array().unwrap().is_empty());
    engine.remap_pack(&job.id, "Pack/001.mp4".into()).unwrap();
    let repaired = run_until(&engine, &job.id, "ready");
    assert_eq!(repaired.download_id.as_deref(), Some(torrent.id.as_str()));
    assert_eq!(
        fs::read(&repaired.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
    assert_eq!(engine.transfers().unwrap().as_array().unwrap().len(), 1);
    assert!(engine.remap_pack(&job.id, "Pack/other.mp4".into()).is_err());
}

#[test]
fn cancelling_one_pack_episode_keeps_another_shared_episode_download_active() {
    let directory = Directory::new();
    let catalog = catalog(2);
    let torrent = torrent(&directory, 2);
    let seed = Seeder::open(&directory.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let engine = Engine::open(native_config(&directory, &catalog)).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let report = engine
        .submit_pack(id(&record), &mapping(&torrent.magnet(proxy.port), 2))
        .unwrap();
    let ids = jobs(&report);
    wait(|| {
        engine.tick().unwrap();
        lock(&engine.store)
            .unwrap()
            .list()
            .iter()
            .all(|job| job.download_id.is_some())
    });
    engine.cancel(&ids[0]).unwrap();
    proxy.payloads_enabled.store(true, Ordering::Release);
    let retained = run_until(&engine, &ids[1], "ready");
    assert_eq!(retained.download_id.as_deref(), Some(torrent.id.as_str()));
    let cancelled = lock(&engine.store).unwrap().get(&ids[0]).unwrap();
    assert_eq!(cancelled.state, "cancelled");
    assert!(cancelled.imports.is_empty());
    assert!(engine.remap_pack(&ids[0], "Pack/002.mp4".into()).is_err());
    assert_eq!(
        lock(&engine.store).unwrap().get(&ids[0]).unwrap(),
        cancelled
    );
    assert_eq!(
        engine.transfer(&torrent.id).unwrap().get("user_paused"),
        Some(&Value::Bool(false))
    );
}

#[test]
fn unsafe_unknown_or_duplicate_mappings_are_rejected_before_any_job_is_recorded() {
    let directory = Directory::new();
    let catalog = catalog(2);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    for path in [
        "../escape.mp4",
        "/absolute.mp4",
        "Pack//file.mp4",
        "Pack/./file.mp4",
        "Pack/../file.mp4",
        "C:/file.mp4",
        "Pack\\file.mp4",
        "Pack/file.txt",
        "Pack/a\n.mp4",
    ] {
        let mut pack = mapping("fixture.torrent", 2);
        pack.episodes[1].file_path = path.into();
        assert!(
            engine.submit_pack(id(&record), &pack).is_err(),
            "Accepted {path}"
        );
        assert!(lock(&engine.store).unwrap().list().is_empty());
    }
    for duplicate_file in [false, true] {
        let mut pack = mapping("fixture.torrent", 2);
        if duplicate_file {
            pack.episodes[1].file_path = pack.episodes[0].file_path.clone();
        } else {
            pack.episodes[1].episode = 1;
        }
        assert!(engine.submit_pack(id(&record), &pack).is_err());
    }
    let mut pack = mapping("fixture.torrent", 2);
    pack.episodes[1].episode = 99;
    assert!(engine.submit_pack(id(&record), &pack).is_err());
    assert!(lock(&engine.store).unwrap().list().is_empty());
}

#[test]
fn pack_plan_excludes_future_undated_and_unidentified_episodes() {
    let directory = Directory::new();
    let mut unknown = episode(1, 3, Some("2024-01-01"), "No identity");
    if let Value::Object(fields) = &mut unknown {
        fields.remove("id");
    }
    let catalog = Catalog::open(vec![
        episode(1, 1, Some("2200-01-01"), "Future"),
        episode(1, 2, None, "No date"),
        unknown,
    ]);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    for number in 1..=3 {
        let mut pack = mapping("fixture.torrent", 1);
        pack.episodes[0].episode = number;
        assert!(engine.submit_pack(id(&record), &pack).is_err());
    }
    assert!(lock(&engine.store).unwrap().list().is_empty());
}

#[test]
fn pack_mapping_is_immutable_idempotent_and_compatible_with_older_unmapped_jobs() {
    let directory = Directory::new();
    let catalog = catalog(2);
    let engine = Engine::open_for_management(catalog.config(&directory.0)).unwrap();
    let record = engine
        .track_series_with_policy(&request(), false, false, false)
        .unwrap();
    let report = engine
        .submit_pack(id(&record), &mapping("fixture.torrent", 1))
        .unwrap();
    let id = jobs(&report).remove(0);
    let original = lock(&engine.store).unwrap().get(&id).unwrap();
    let mut changed = original.clone();
    changed.pack_file = Some("Pack/different.mp4".into());
    assert!(lock(&engine.store).unwrap().update(changed).is_err());
    let mut conflict = mapping("fixture.torrent", 2);
    conflict.episodes[0].file_path = "Pack/different.mp4".into();
    assert!(
        engine
            .submit_pack(series_support::id(&record), &conflict)
            .is_err()
    );
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    engine.cancel(&id).unwrap();
    assert_eq!(
        engine
            .submit_pack(series_support::id(&record), &mapping("fixture.torrent", 1))
            .unwrap()
            .get("submitted"),
        Some(&Value::Number(0.0))
    );
    let mut value = original.to_json();
    if let Value::Object(fields) = &mut value {
        fields.remove("pack_file");
    }
    assert!(Job::from_json(&value).unwrap().pack_file.is_none());
    value.insert("pack_file", "../escape.mp4");
    assert!(Job::from_json(&value).is_err());
}

#[test]
fn pack_payload_types_bounds_and_fields_are_strict() {
    for text in [
        r#"{"source_url":"fixture.torrent","episodes":[]}"#,
        r#"{"source_url":"fixture.torrent","episodes":[{"season":1.5,"episode":1,"file_path":"Pack/001.mp4"}]}"#,
        r#"{"source_url":"fixture.torrent","episodes":[{"season":1,"episode":1,"file_path":"Pack/001.mp4","unknown":true}]}"#,
        r#"{"source_url":"fixture.torrent","episodes":[{"season":1,"episode":1,"file_path":"Pack/001.mp4"}],"unknown":true}"#,
    ] {
        assert!(PackSubmission::from_json(&json::parse(text).unwrap()).is_err());
    }
    assert!(mapping("fixture.torrent", 65).validate().is_err());
    assert!(mapping("\nfixture.torrent", 1).validate().is_err());
}
