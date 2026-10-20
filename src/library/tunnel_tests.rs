//! Shared-group replacement through native tunnel owners (#317).
//! No catalog, indexer, payload peer or management socket exists on the host.
use super::GroupRequest;
use crate::{
    config,
    engine::{Engine, lock},
    json::{self, Value},
    pack::{
        SharedFileRequest,
        tunnel_tests::{
            Directory, accept, deadline, engine_with, marked_metadata, request, response, server,
        },
    },
    store::Job,
    tunnel_http::tests::Pair,
};
use std::{
    fs,
    path::Path,
    sync::{atomic::Ordering, mpsc},
    thread,
    time::Duration,
};

/// Ranked resolutions, so a 1080p release improves a 720p baseline below no cutoff.
fn engine(directory: &Directory, pair: &Pair) -> (std::sync::Arc<Engine>, String) {
    let mut settings = config::default_json();
    let selection = json::parse(
        r#"{"profiles":{"any":{"resolutions":[2160,1080,720,480]}},"movie_profile":"any","episode_profile":"any"}"#,
    )
    .unwrap();
    settings.insert("selection", selection);
    engine_with(directory, pair.client_network.clone(), settings)
}

/// Imported, monitored two-owner group with a recorded 720p baseline.
fn imported_group(engine: &Engine, series: &str, pair: &Pair) -> (Vec<Job>, Vec<u8>) {
    let (metadata, _) = marked_metadata(7);
    let (port, worker) = server(&*pair.server_network, vec![response(&metadata); 2]);
    let mut query = SharedFileRequest {
        source_url: format!("http://original.example:{port}/pack.torrent"),
        file_path: "Pack/shared.mp4".into(),
        season: 1,
        episodes: vec![1, 2],
        apply: false,
        plan_id: None,
    };
    let preview = engine
        .shared_file_before(series, &query, deadline())
        .unwrap();
    query.apply = true;
    query.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    engine
        .shared_file_before(series, &query, deadline())
        .unwrap();
    assert_eq!(worker.join().unwrap().len(), 2);

    let video = include_bytes!("../../examples/demo.mp4").to_vec();
    let mut store = lock(&engine.store).unwrap();
    let owners = store.list().unwrap();
    let import = owners[0].shared_file.as_ref().unwrap().import_path.clone();
    fs::create_dir_all(Path::new(&import).parent().unwrap()).unwrap();
    fs::write(&import, &video).unwrap();
    for mut job in owners {
        job.state = "ready".into();
        job.progress = 1.0;
        job.imports = vec![import.clone()];
        store.update(job).unwrap();
    }
    drop(store);
    let owner = lock(&engine.store).unwrap().list().unwrap()[0].id.clone();
    let baseline = GroupRequest {
        action: "baseline".into(),
        release_title: "Fixture.Series.S01.720p.WEB-DL".into(),
        source_url: None,
        file_path: None,
        apply: false,
        plan_id: None,
    };
    guarded(engine, &owner, &baseline);
    let group = lock(&engine.store).unwrap().shared_group(&owner).unwrap();
    assert!(
        group
            .iter()
            .all(|job| job.monitored && job.release.is_some())
    );
    (group, video)
}

fn guarded(engine: &Engine, owner: &str, query: &GroupRequest) -> Value {
    let preview = engine
        .library_group_before(owner, query, deadline())
        .unwrap();
    let mut apply = query.clone();
    apply.apply = true;
    apply.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    engine
        .library_group_before(owner, &apply, deadline())
        .unwrap()
}

fn replacement(port: u16) -> GroupRequest {
    GroupRequest {
        action: "replace".into(),
        release_title: "Fixture.Series.S01.1080p.WEB-DL".into(),
        source_url: Some(format!(
            "http://first.example:{port}/better.torrent?token=synthetic-group-secret"
        )),
        file_path: Some("Pack/shared.mp4".into()),
        apply: false,
        plan_id: None,
    }
}

/// The original group, its import and the disabled transfer runtime are untouched.
fn original_kept(engine: &Engine, group: &[Job], video: &[u8]) {
    let mut store = lock(&engine.store).unwrap();
    assert_eq!(store.shared_group(&group[0].id).unwrap(), group);
    drop(store);
    let import = &group[0].shared_file.as_ref().unwrap().import_path;
    assert_eq!(fs::read(import).unwrap(), video);
    assert_eq!(
        engine.transfers().unwrap_err(),
        "Native downloads are disabled"
    );
    assert!(!engine.config.downloads.data_dir.exists());
    assert!(!engine.config.downloads.state_dir.exists());
}

#[test]
fn group_replacement_preview_and_guarded_apply_use_tunnel_metadata_without_workers() {
    let directory = Directory::new();
    let pair = Pair::new();
    let (engine, series) = engine(&directory, &pair);
    let (group, video) = imported_group(&engine, &series, &pair);
    let (metadata, hash) = marked_metadata(9);
    let (port, worker) = server(&*pair.server_network, vec![response(&metadata); 2]);
    let redirect = format!("HTTP/1.1 302 Found\r\nLocation: http://metadata.example:{port}/source.torrent\r\nContent-Length: 0\r\n\r\n").into_bytes();
    let (redirect_port, redirect_worker) = server(&*pair.server_network, vec![redirect; 2]);
    let mut query = replacement(redirect_port);

    let preview = engine
        .library_group_before(&group[0].id, &query, deadline())
        .unwrap();
    assert_eq!(preview.get("submitted").and_then(Value::as_u64), Some(0));
    assert_eq!(
        preview
            .get("binding")
            .unwrap()
            .get("torrent_id")
            .and_then(Value::as_str),
        Some(hash.as_str())
    );
    assert!(!json::stringify(&preview).contains("synthetic-group-secret"));
    assert_eq!(lock(&engine.store).unwrap().list().unwrap().len(), 2);
    original_kept(&engine, &group, &video);

    query.apply = true;
    query.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    let applied = engine
        .library_group_before(&group[0].id, &query, deadline())
        .unwrap();
    assert_eq!(applied.get("submitted").and_then(Value::as_u64), Some(2));
    let jobs = lock(&engine.store).unwrap().list().unwrap();
    let created: Vec<_> = jobs
        .iter()
        .filter(|job| group.iter().all(|parent| parent.id != job.id))
        .collect();
    assert_eq!(created.len(), 2);
    assert!(created.iter().all(|job| job.state == "queued"
        && job.attempts == 0
        && job.download_id.is_none()
        && job.imports.is_empty()
        && job.shared_file.as_ref().unwrap().torrent_id == hash));
    original_kept(&engine, &group, &video);
    assert_eq!(worker.join().unwrap().len(), 2);
    assert_eq!(redirect_worker.join().unwrap().len(), 2);
    assert!(pair.dns.load(Ordering::Relaxed) >= 6);
}

#[test]
fn cancellation_during_group_replacement_metadata_refuses_preview_apply_and_retry() {
    for apply in [false, true] {
        cancel_pending_replacement(apply);
    }
}

fn cancel_pending_replacement(apply: bool) {
    let directory = Directory::new();
    let pair = Pair::new();
    let (engine, series) = engine(&directory, &pair);
    let (group, video) = imported_group(&engine, &series, &pair);
    let (metadata, _) = marked_metadata(9);
    let listener = pair
        .server_network
        .listen("0.0.0.0:0".parse().unwrap())
        .unwrap();
    let mut query = replacement(listener.port());
    let (entered, waiting) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        // First fetch supplies a valid preview guard; the second fetch is held.
        for delay in [false, true] {
            let mut stream = accept(&*listener);
            request(&mut *stream);
            if delay {
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            let result = stream.write_all(&response(&metadata));
            if !delay {
                result.unwrap();
            }
            let _ = stream.read(&mut [0]);
        }
    });
    let preview = engine
        .library_group_before(&group[0].id, &query, deadline())
        .unwrap();
    query.apply = apply;
    query.plan_id = apply.then(|| preview.get("plan_id").unwrap().as_str().unwrap().into());
    let pending = {
        let (engine, owner, query) = (engine.clone(), group[0].id.clone(), query.clone());
        thread::spawn(move || engine.library_group_before(&owner, &query, deadline()))
    };
    waiting.recv_timeout(Duration::from_secs(10)).unwrap();
    pair.client.cancel();
    release.send(()).unwrap();
    assert!(
        pending.join().unwrap().is_err(),
        "late response published a preview or created replacement owners"
    );
    assert!(
        engine
            .library_group_before(&group[0].id, &query, deadline())
            .is_err()
    );
    query.apply = false;
    query.plan_id = None;
    assert!(
        engine
            .library_group_before(&group[0].id, &query, deadline())
            .is_err()
    );
    assert_eq!(lock(&engine.store).unwrap().list().unwrap().len(), 2);
    original_kept(&engine, &group, &video);
    server.join().unwrap();
    assert!(pair.dns.load(Ordering::Relaxed) >= 3);
}
