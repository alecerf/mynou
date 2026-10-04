//! Library upgrades use synthetic releases, media and native local torrent peers.
mod library_support;

use library_support::*;
use mynou::bencode::{self, Value as Bencode};
use mynou::config;
use mynou::crypto::sha1;
use mynou::engine::{Engine, lock};
use mynou::json::Value;
use mynou::torrent::{Client, DownloadConfig};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const HD: &str = r#"{"resolutions":[1080,720],"languages":["en"]}"#;
const BASELINE: &str = "Fixture.Movie.2024.720p.WEB-DL.EN";
const BETTER: &str = "Fixture.Movie.2024.1080p.WEB-DL.EN";

fn queued(report: &Value) -> u64 {
    report.get("queued").and_then(Value::as_u64).unwrap()
}

fn children(engine: &Engine, parent: &str) -> Vec<mynou::store::Job> {
    lock(&engine.store)
        .unwrap()
        .list()
        .into_iter()
        .filter(|job| job.upgrade_parent.as_deref() == Some(parent))
        .collect()
}

fn unavailable_magnet(suffix: &str) -> String {
    format!("magnet:?xt=urn:btih:0123456789012345678901234567890123456789&dn={suffix}")
}

#[test]
fn preview_is_read_only_and_application_is_idempotent_without_hiding_the_import() {
    let directory = Directory::new();
    let candidate_url = format!("http://127.0.0.1:1/upgrade.torrent?token={DOWNLOAD_SECRET}");
    let indexer = Indexer::open(Value::Array(vec![release(BETTER, 2, &candidate_url)]));
    let engine = Engine::open(config(&directory.0, Some(&indexer), HD)).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    assert!(original.release.is_none());
    let original = engine.set_baseline(&original.id, BASELINE).unwrap();
    assert_eq!(original.release.as_ref().unwrap().profile, "fixture");
    let before = files(&directory.0);
    let events = lock(&engine.store).unwrap().events(&original.id);
    let first = engine.check_upgrades(false).unwrap();
    assert_eq!(first.get("apply"), Some(&Value::Bool(false)));
    assert_eq!(queued(&first), 0);
    assert_eq!(first.get("checked").and_then(Value::as_u64), Some(1));
    assert_eq!(engine.check_upgrades(false).unwrap(), first);
    assert_redacted(&first);
    assert_eq!(files(&directory.0), before);
    assert_eq!(lock(&engine.store).unwrap().events(&original.id), events);
    assert!(children(&engine, &original.id).is_empty());
    assert_eq!(
        library_ids(&engine).as_slice(),
        std::slice::from_ref(&original.id)
    );

    let applied = engine.check_upgrades(true).unwrap();
    assert_eq!(applied.get("apply"), Some(&Value::Bool(true)));
    assert_eq!(queued(&applied), 1);
    assert_redacted(&applied);
    let pending = children(&engine, &original.id);
    assert_eq!(pending.len(), 1);
    assert_ne!(pending[0].id, original.id);
    assert_eq!(
        pending[0].request.source_url.as_deref(),
        Some(candidate_url.as_str())
    );
    assert!(pending[0].request.source_path.is_none());
    assert_eq!(pending[0].request.media_key(), original.request.media_key());
    assert_eq!(pending[0].release.as_ref().unwrap().title, BETTER);
    assert_eq!(
        library_ids(&engine).as_slice(),
        std::slice::from_ref(&original.id)
    );
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 0);
    assert_eq!(children(&engine, &original.id).len(), 1);
    assert_eq!(
        fs::read(&original.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );

    let cfg = engine.config.clone();
    drop(engine);
    let resumed = Engine::open(cfg).unwrap();
    assert_eq!(
        library_ids(&resumed).as_slice(),
        std::slice::from_ref(&original.id)
    );
    assert_eq!(children(&resumed, &original.id), pending);
}

#[test]
fn seed_count_alone_never_upgrades_and_a_lower_preference_never_replaces_a_better_import() {
    let directory = Directory::new();
    let url = unavailable_magnet("more-seeders");
    let indexer = Indexer::open(Value::Array(vec![release(BASELINE, 100_000, &url)]));
    let engine = Engine::open(config(&directory.0, Some(&indexer), HD)).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    engine.set_baseline(&original.id, BASELINE).unwrap();
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 0);
    assert!(children(&engine, &original.id).is_empty());

    indexer.replace(Value::Array(vec![release(
        "Fixture.Movie.2024.480p.WEB-DL.EN",
        999_999,
        &url,
    )]));
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 0);
    assert_eq!(
        library_ids(&engine).as_slice(),
        std::slice::from_ref(&original.id)
    );

    // A configured rejection is not evidence that the current file is worse.
    let cfg = engine.config.clone();
    drop(engine);
    let mut value = configuration(
        Some(&indexer),
        r#"{"resolutions":[1080],"languages":["en"]}"#,
    );
    value.insert("store_dir", cfg.store_dir.to_str().unwrap());
    let engine = Engine::open(config::from_json(&value, &directory.0).unwrap()).unwrap();
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 0);
    indexer.replace(Value::Array(vec![release(
        BETTER,
        2,
        &unavailable_magnet("preferred"),
    )]));
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 1);
}

#[test]
fn cutoffs_and_unmonitoring_skip_searches_and_survive_restart() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![release(
        "Fixture.Movie.2024.2160p.WEB-DL.EN",
        2,
        &unavailable_magnet("uhd"),
    )]));
    let profile = r#"{"resolutions":[2160,1080,720],"cutoff_resolution":1080,"languages":["en"]}"#;
    let engine = Engine::open(config(&directory.0, Some(&indexer), profile)).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    engine.set_baseline(&original.id, BETTER).unwrap();
    let before = indexer.calls.load(Ordering::Relaxed);
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 0);
    assert_eq!(indexer.calls.load(Ordering::Relaxed), before);

    let second = local_import(&engine, &directory.0, "Another Movie");
    engine
        .set_baseline(&second.id, "Another.Movie.2024.720p.WEB-DL.EN")
        .unwrap();
    engine.set_monitored(&second.id, false).unwrap();
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 0);
    assert_eq!(indexer.calls.load(Ordering::Relaxed), before);
    let cfg = engine.config.clone();
    drop(engine);
    let resumed = Engine::open(cfg).unwrap();
    let second = lock(&resumed.store).unwrap().get(&second.id).unwrap();
    assert!(!second.monitored);
    assert!(second.release.is_some());
    assert_eq!(queued(&resumed.check_upgrades(true).unwrap()), 0);
    assert_eq!(indexer.calls.load(Ordering::Relaxed), before);
}

#[test]
fn missing_baselines_and_invalid_baselines_do_not_change_the_library() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![release(
        BETTER,
        2,
        &unavailable_magnet("new"),
    )]));
    let engine = Engine::open(config(&directory.0, Some(&indexer), HD)).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 0);
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
    let before = files(&directory.0);
    for title in [
        "",
        "Different.Movie.2024.720p.WEB-DL.EN",
        "Fixture.Movie.2023.720p.WEB-DL.EN",
        "Fixture.Movie.2024.S01E01.720p.WEB-DL.EN",
    ] {
        assert!(
            engine.set_baseline(&original.id, title).is_err(),
            "Accepted invalid baseline: {title}"
        );
        assert_eq!(files(&directory.0), before);
    }
    engine.set_baseline(&original.id, BASELINE).unwrap();
    assert!(engine.set_baseline(&original.id, BETTER).is_err());
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 1);
    let child = children(&engine, &original.id).remove(0);
    assert!(engine.set_baseline(&child.id, BETTER).is_err());
    assert!(engine.set_monitored(&child.id, false).is_err());
}

#[test]
fn cancelled_and_failed_upgrades_leave_the_existing_file_current() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![release(
        BETTER,
        2,
        &unavailable_magnet("cancelled"),
    )]));
    let engine = Engine::open(config(&directory.0, Some(&indexer), HD)).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    engine.set_baseline(&original.id, BASELINE).unwrap();
    let original_bytes = fs::read(&original.imports[0]).unwrap();
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 1);
    let cancelled = children(&engine, &original.id).remove(0);
    assert_eq!(engine.cancel(&cancelled.id).unwrap().state, "cancelled");
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 0);
    assert_eq!(
        lock(&engine.store)
            .unwrap()
            .get(&cancelled.id)
            .unwrap()
            .state,
        "cancelled"
    );
    assert_eq!(
        library_ids(&engine).as_slice(),
        std::slice::from_ref(&original.id)
    );

    indexer.replace(Value::Array(vec![release(
        BETTER,
        2,
        &unavailable_magnet("failed"),
    )]));
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 1);
    let failed = children(&engine, &original.id)
        .into_iter()
        .find(|job| job.state == "queued")
        .unwrap();
    engine.tick().unwrap();
    let failed = lock(&engine.store).unwrap().get(&failed.id).unwrap();
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.next_attempt_at, 0);
    assert_eq!(library_ids(&engine), [original.id]);
    assert_eq!(fs::read(&original.imports[0]).unwrap(), original_bytes);
    assert!(failed.imports.is_empty());
}

#[test]
fn manual_checks_are_bounded_and_isolate_an_unavailable_indexer() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(Vec::new()));
    let mut value = configuration(Some(&indexer), HD);
    let Value::Object(monitoring) = value.get_mut("monitoring").unwrap() else {
        panic!("monitoring section")
    };
    monitoring.insert("max_checks".into(), 1_u32.into());
    let engine = Engine::open(config::from_json(&value, &directory.0).unwrap()).unwrap();
    for title in ["First Movie", "Second Movie"] {
        let original = local_import(&engine, &directory.0, title);
        engine
            .set_baseline(
                &original.id,
                &format!("{}.2024.720p.WEB-DL.EN", title.replace(' ', ".")),
            )
            .unwrap();
    }
    indexer.fail();
    let report = engine.check_upgrades(false).unwrap();
    assert_eq!(report.get("checked").and_then(Value::as_u64), Some(1));
    assert_eq!(report.get("limited"), Some(&Value::Bool(true)));
    assert_eq!(queued(&report), 0);
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 1);
    assert_redacted(&report);
    assert_eq!(library_ids(&engine).len(), 2);
}

#[test]
fn an_accepted_release_can_replace_a_baseline_that_violates_current_language_policy() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![release(
        BASELINE,
        2,
        &unavailable_magnet("english"),
    )]));
    let engine = Engine::open(config(&directory.0, Some(&indexer), HD)).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    engine
        .set_baseline(&original.id, "Fixture.Movie.2024.1080p.WEB-DL.FR")
        .unwrap();
    let report = engine.check_upgrades(false).unwrap();
    let entry = &report.get("entries").unwrap().as_array().unwrap()[0];
    assert_eq!(
        entry.get("action").and_then(Value::as_str),
        Some("upgrade_available")
    );
    assert_eq!(
        entry.get("current_assessment").unwrap().get("accepted"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        entry.get("candidate_assessment").unwrap().get("accepted"),
        Some(&Value::Bool(true))
    );
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 1);
    let child = children(&engine, &original.id).remove(0);
    assert_eq!(child.release.as_ref().unwrap().title, BASELINE);
    assert_eq!(library_ids(&engine), [original.id]);
}

#[test]
fn missing_owned_imports_are_reported_without_searching_or_acquiring() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![release(
        BETTER,
        2,
        &unavailable_magnet("new"),
    )]));
    let engine = Engine::open(config(&directory.0, Some(&indexer), HD)).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    engine.set_baseline(&original.id, BASELINE).unwrap();
    fs::remove_file(&original.imports[0]).unwrap();
    let library = engine.library().unwrap();
    assert_eq!(
        library.as_array().unwrap()[0].get("imports_present"),
        Some(&Value::Bool(false))
    );
    let report = engine.check_upgrades(true).unwrap();
    assert_eq!(queued(&report), 0);
    assert_eq!(
        report.get("entries").unwrap().as_array().unwrap()[0]
            .get("action")
            .and_then(Value::as_str),
        Some("imports_missing")
    );
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
    assert!(children(&engine, &original.id).is_empty());
}

#[test]
fn unmonitoring_a_parent_prevents_its_queued_child_from_being_claimed() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![release(
        BETTER,
        2,
        &unavailable_magnet("queued"),
    )]));
    let engine = Engine::open(config(&directory.0, Some(&indexer), HD)).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    engine.set_baseline(&original.id, BASELINE).unwrap();
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 1);
    let child = children(&engine, &original.id).remove(0);
    engine.set_monitored(&original.id, false).unwrap();
    assert!(!engine.tick().unwrap());
    let pending = lock(&engine.store).unwrap().get(&child.id).unwrap();
    assert_eq!(pending.state, "queued");
    assert!(pending.download_id.is_none());
    assert_eq!(
        library_ids(&engine).as_slice(),
        std::slice::from_ref(&original.id)
    );
    engine.set_monitored(&original.id, true).unwrap();
    assert!(engine.tick().unwrap());
    assert_eq!(
        lock(&engine.store).unwrap().get(&child.id).unwrap().state,
        "failed"
    );
    assert_eq!(library_ids(&engine), [original.id]);
}

#[test]
fn enabled_background_monitoring_records_a_check_and_respects_the_polling_interval() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(Vec::new()));
    let mut value = configuration(Some(&indexer), HD);
    let Value::Object(monitoring) = value.get_mut("monitoring").unwrap() else {
        panic!("monitoring section")
    };
    monitoring.insert("enabled".into(), true.into());
    monitoring.insert("interval_secs".into(), 60_u32.into());
    let engine = Engine::open(config::from_json(&value, &directory.0).unwrap()).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    engine.set_baseline(&original.id, BASELINE).unwrap();
    let workers = engine.start();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let job = lock(&engine.store).unwrap().get(&original.id).unwrap();
        if job.monitor_checked_at > 0 && indexer.calls.load(Ordering::Relaxed) == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Background monitor did not check the ready import"
        );
        thread::sleep(Duration::from_millis(5));
    }
    thread::sleep(Duration::from_millis(250));
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 1);
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    drop(workers);
    assert!(lock(&engine.last_upgrade_error).unwrap().is_none());
}

struct Seeder {
    _client: Client,
    magnet: String,
}

impl Seeder {
    fn open(directory: &Path) -> Self {
        Self::with_bytes(directory, include_bytes!("../examples/demo.mp4"))
    }

    fn with_bytes(directory: &Path, bytes: &[u8]) -> Self {
        let filename = "Fixture.Movie.2024.mp4";
        let info = Bencode::Dict(BTreeMap::from([
            (b"length".to_vec(), Bencode::Int(bytes.len() as i64)),
            (
                b"name".to_vec(),
                Bencode::Bytes(filename.as_bytes().to_vec()),
            ),
            (b"piece length".to_vec(), Bencode::Int(16_384)),
            (
                b"pieces".to_vec(),
                Bencode::Bytes(bytes.chunks(16_384).flat_map(sha1).collect()),
            ),
            (b"private".to_vec(), Bencode::Int(1)),
        ]));
        let id: String = sha1(&bencode::encode(&info))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let torrent = bencode::encode(&Bencode::Dict(BTreeMap::from([(b"info".to_vec(), info)])));
        let metadata = directory.join("fixture.torrent");
        fs::write(&metadata, torrent).unwrap();
        let data_dir = directory.join("seeder/data");
        fs::create_dir_all(data_dir.join(&id)).unwrap();
        fs::write(data_dir.join(&id).join(filename), bytes).unwrap();
        let client = Client::open(DownloadConfig {
            data_dir,
            state_dir: directory.join("seeder/state"),
            listen_port: 0,
            seed: true,
            dht: false,
            pex: false,
            max_active: 1,
            max_peers: 4,
        })
        .unwrap();
        client.ensure(metadata.to_str().unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !client.check(&id).unwrap().ready {
            assert!(
                Instant::now() < deadline,
                "Synthetic peer did not become ready"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let magnet = format!(
            "magnet:?xt=urn:btih:{id}&x.pe=127.0.0.1:{}",
            client.listen_port()
        );
        Self {
            _client: client,
            magnet,
        }
    }
}

#[test]
fn verified_torrent_with_invalid_media_cannot_replace_the_existing_import() {
    let directory = Directory::new();
    let seeder = Seeder::with_bytes(&directory.0, b"Synthetic invalid MP4 payload");
    let indexer = Indexer::open(Value::Array(vec![release(BETTER, 1, &seeder.magnet)]));
    let mut value = configuration(Some(&indexer), HD);
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads section")
    };
    downloads.insert("enabled".into(), true.into());
    let engine = Engine::open(config::from_json(&value, &directory.0).unwrap()).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    engine.set_baseline(&original.id, BASELINE).unwrap();
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 1);
    let child = children(&engine, &original.id).remove(0);
    let failed = run_until(&engine, &child.id, "failed");
    assert!(failed.download_id.is_some());
    assert!(failed.last_error.is_some());
    assert!(failed.imports.is_empty());
    assert_eq!(library_ids(&engine), [original.id]);
    assert_eq!(
        fs::read(&original.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
}

#[test]
fn native_upgrade_imports_a_distinct_version_and_retains_both_files_after_restart() {
    let directory = Directory::new();
    let seeder = Seeder::open(&directory.0);
    let indexer = Indexer::open(Value::Array(vec![release(BETTER, 1, &seeder.magnet)]));
    let mut value = configuration(Some(&indexer), HD);
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads section")
    };
    downloads.insert("enabled".into(), true.into());
    let engine = Engine::open(config::from_json(&value, &directory.0).unwrap()).unwrap();
    let original = local_import(&engine, &directory.0, "Fixture Movie");
    engine.set_baseline(&original.id, BASELINE).unwrap();
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 1);
    let child = children(&engine, &original.id).remove(0);
    let ready = run_until(&engine, &child.id, "ready");
    assert_eq!(ready.release.as_ref().unwrap().title, BETTER);
    assert_eq!(ready.upgrade_parent.as_deref(), Some(original.id.as_str()));
    assert_eq!(ready.imports.len(), 1);
    assert_ne!(ready.imports, original.imports);
    assert!(
        Path::new(&ready.imports[0])
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains(&child.id)
    );
    assert_eq!(
        Path::new(&ready.imports[0]).parent(),
        Path::new(&original.imports[0]).parent()
    );
    for imported in [&original.imports[0], &ready.imports[0]] {
        assert_eq!(
            fs::read(imported).unwrap(),
            include_bytes!("../examples/demo.mp4")
        );
    }
    assert_eq!(
        library_ids(&engine).as_slice(),
        std::slice::from_ref(&child.id)
    );
    assert!(engine.set_monitored(&original.id, false).is_err());
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 0);
    let library = engine.library().unwrap();
    assert_eq!(library.as_array().unwrap().len(), 1);
    assert_redacted(&library);
    let cfg = engine.config.clone();
    drop(engine);
    drop(seeder);
    let resumed = Engine::open(cfg).unwrap();
    assert_eq!(library_ids(&resumed), [child.id]);
    assert_eq!(
        lock(&resumed.store)
            .unwrap()
            .get(&original.id)
            .unwrap()
            .imports,
        original.imports
    );
    assert_eq!(
        lock(&resumed.store)
            .unwrap()
            .get(&ready.id)
            .unwrap()
            .imports,
        ready.imports
    );
}

#[test]
fn initial_automatic_acquisition_records_the_selected_release_for_future_comparisons() {
    let directory = Directory::new();
    let seeder = Seeder::open(&directory.0);
    let indexer = Indexer::open(Value::Array(vec![release(BETTER, 1, &seeder.magnet)]));
    let mut value = configuration(Some(&indexer), HD);
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads section")
    };
    downloads.insert("enabled".into(), true.into());
    let engine = Engine::open(config::from_json(&value, &directory.0).unwrap()).unwrap();
    let id = engine.submit(movie("Fixture Movie")).unwrap().remove(0).id;
    let ready = run_until(&engine, &id, "ready");
    let release = ready.release.unwrap();
    assert_eq!(release.title, BETTER);
    assert_eq!(release.profile, "fixture");
    assert!(ready.upgrade_parent.is_none());
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 0);
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
}

struct Plex {
    url: String,
    paths: Arc<Mutex<Vec<String>>>,
    refreshes: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Plex {
    fn open(original_path: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let paths = Arc::new(Mutex::new(vec![original_path]));
        let thread_paths = paths.clone();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let thread_refreshes = refreshes.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = stopped.clone();
        let thread = thread::spawn(move || {
            while !thread_stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("Cannot accept Plex fixture request: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    assert!(header.len() < 16_384 && Instant::now() < deadline);
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                }
                let header = std::str::from_utf8(&header).unwrap();
                let route = header.split_whitespace().nth(1).unwrap();
                let body = if route.contains("/refresh") {
                    thread_refreshes.fetch_add(1, Ordering::Relaxed);
                    Value::object()
                } else {
                    let parts = thread_paths
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|path| {
                            let mut part = Value::object();
                            part.insert("file", path.clone());
                            part
                        })
                        .collect();
                    let mut media = Value::object();
                    media.insert("Part", Value::Array(parts));
                    let mut movie = Value::object();
                    movie.insert("type", "movie");
                    movie.insert("title", "Fixture Movie");
                    movie.insert("year", 2024_u32);
                    movie.insert("ratingKey", "fixture-movie");
                    movie.insert("Media", Value::Array(vec![media]));
                    let mut container = Value::object();
                    container.insert("Metadata", Value::Array(vec![movie]));
                    let mut body = Value::object();
                    body.insert("MediaContainer", container);
                    body
                };
                let body = mynou::json::stringify(&body);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            url,
            paths,
            refreshes,
            stopped,
            thread: Some(thread),
        }
    }
}

impl Drop for Plex {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}

#[test]
fn plex_must_confirm_the_mapped_new_version_before_an_upgrade_becomes_current() {
    let directory = Directory::new();
    let nested = directory.0.join("nested");
    fs::create_dir(&nested).unwrap();
    let seeder = Seeder::open(&directory.0);
    let indexer = Indexer::open(Value::Array(vec![release(BETTER, 1, &seeder.magnet)]));
    let plex = Plex::open("/plex/movies/Fixture Movie (2024)/Fixture Movie (2024).mp4".into());
    let mut value = configuration(Some(&indexer), HD);
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads section")
    };
    downloads.insert("enabled".into(), true.into());
    let local_root = nested.join("library/movies");
    let mut mapping = Value::object();
    mapping.insert("mynou_prefix", local_root.to_str().unwrap());
    mapping.insert("plex_prefix", "/plex/movies");
    let Value::Object(plex_config) = value.get_mut("plex").unwrap() else {
        panic!("Plex section")
    };
    plex_config.insert("enabled".into(), true.into());
    plex_config.insert("url".into(), plex.url.clone().into());
    plex_config.insert(
        "watchlist_url".into(),
        format!("{}/watchlist", plex.url).into(),
    );
    plex_config.insert("path_mappings".into(), Value::Array(vec![mapping]));
    let config_path = nested.join("mynou.json");
    fs::write(&config_path, mynou::json::stringify(&value)).unwrap();
    let mut cfg = config::load(&config_path).unwrap();
    cfg.plex.token_override = Some("synthetic-plex-library-token".into());
    assert!(cfg.movies_root.is_absolute());
    let engine = Engine::open(cfg).unwrap();
    let original = local_import(&engine, &nested, "Fixture Movie");
    engine.set_baseline(&original.id, BASELINE).unwrap();
    assert_eq!(queued(&engine.check_upgrades(true).unwrap()), 1);
    let child = children(&engine, &original.id).remove(0);
    let imported = run_until(&engine, &child.id, "imported");
    assert!(engine.tick().unwrap());
    let scanning = lock(&engine.store).unwrap().get(&child.id).unwrap();
    assert_eq!(
        scanning.state, "scanning",
        "Plex reporting only the old file must not confirm the upgrade"
    );
    assert_eq!(
        library_ids(&engine).as_slice(),
        std::slice::from_ref(&original.id)
    );
    assert_eq!(plex.refreshes.load(Ordering::Relaxed), 2);
    let relative = Path::new(&imported.imports[0])
        .strip_prefix(&local_root)
        .unwrap();
    let mapped = format!("/plex/movies/{}", relative.to_str().unwrap());
    assert!(mapped.contains(&child.id));
    plex.paths.lock().unwrap().push(mapped);
    let ready = run_until(&engine, &child.id, "ready");
    assert_eq!(library_ids(&engine), [child.id]);
    for path in [&original.imports[0], &ready.imports[0]] {
        assert_eq!(
            fs::read(path).unwrap(),
            include_bytes!("../examples/demo.mp4")
        );
    }
}
