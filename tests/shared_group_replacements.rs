//! CI-only atomic replacement, recovery, native transfer and exact Plex journeys.
mod automatic_pack_support;
mod jobs_state;
mod library_support;
mod series_support;
mod shared_group_support;
#[allow(dead_code)]
mod transfer_support;
use automatic_pack_support::{Provider, no_sources, snapshot};
use library_support::{Directory, library_ids};
use mynou::{
    engine::{Engine, lock},
    json::{self, Value},
    library::GroupRequest,
    store::{Store, now},
};
use series_support::Catalog;
use shared_group_support::{
    Fixture, apply, baseline, baseline_query, confirm, guarded, ids, leaves, video,
};
use std::{
    fs,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};
use transfer_support::{RecordingProxy, Seeder, engine_config, wait};

#[test]
fn sixty_four_owners_receive_one_baseline_and_one_atomic_replacement_before_any_payload() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 64);
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    let before = snapshot(&fixture.cfg.store_dir);
    let preview = engine
        .library_group(&parents[0].id, &baseline_query())
        .unwrap();
    no_sources(&preview);
    assert_eq!(snapshot(&fixture.cfg.store_dir), before);
    assert_eq!(preview.get("owners"), Some(&Value::Number(64.0)));
    let before_events = lock(&engine.store).unwrap().events("").unwrap().len();
    baseline(&engine, &parents[0].id);
    let mut store = lock(&engine.store).unwrap();
    let saved = store.shared_group(&parents[0].id).unwrap();
    assert!(saved.iter().all(|job| job.release == saved[0].release));
    assert_eq!(store.events("").unwrap().len(), before_events + 1);
    drop(store);
    let before = snapshot(&fixture.cfg.store_dir);
    let query = fixture.replacement_query();
    let plan = guarded(&engine, &parents[0].id, &query);
    assert_eq!(snapshot(&fixture.cfg.store_dir), before);
    let created = engine.library_group(&parents[0].id, &plan).unwrap();
    no_sources(&created);
    assert_eq!(created.get("submitted"), Some(&Value::Number(64.0)));
    assert_eq!(library_ids(&engine).len(), 64);
    let children = ids(&created);
    let mut store = lock(&engine.store).unwrap();
    assert_eq!(store.list().unwrap().len(), 128);
    assert!(
        children
            .iter()
            .all(|id| store.get(id).unwrap().unwrap().state == "queued")
    );
    let lineage = store
        .get(&children[0])
        .unwrap()
        .unwrap()
        .shared_upgrade
        .unwrap();
    assert_eq!(
        lineage.parent_jobs,
        saved.iter().map(|job| job.id.clone()).collect::<Vec<_>>()
    );
    assert!(!fixture.cfg.downloads.data_dir.exists());
    drop(store);
    drop(engine);
    let mut store = Store::open(&fixture.cfg.store_dir).unwrap();
    assert_eq!(store.list().unwrap().len(), 128);
    drop(store);
    assert_eq!(
        Store::open_read_only(&fixture.cfg.store_dir)
            .unwrap()
            .list()
            .unwrap()
            .len(),
        128
    );
}

#[test]
fn every_owner_stages_before_one_atomic_promotion_and_restart_keeps_old_bytes() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 64);
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    let old_path = parents[0].imports[0].clone();
    let old_bytes = fs::read(&old_path).unwrap();
    baseline(&engine, &parents[0].id);
    let children = ids(&apply(
        &engine,
        &parents[0].id,
        &fixture.replacement_query(),
    ));
    {
        let mut store = lock(&engine.store).unwrap();
        for child in children.iter().take(63) {
            confirm(&mut store, child, &fixture.replacement.files[0].1);
        }
        assert_eq!(
            store
                .library_jobs()
                .unwrap()
                .iter()
                .map(|job| &job.id)
                .collect::<std::collections::BTreeSet<_>>(),
            parents.iter().map(|job| &job.id).collect()
        );
        assert!(
            children
                .iter()
                .take(63)
                .all(|id| store.get(id).unwrap().unwrap().state == "staged")
        );
        assert!(store.claim(now(), 30).unwrap().is_some());
    }
    // Release the remaining owner's lease so the simulated final confirmation is current.
    {
        let mut store = lock(&engine.store).unwrap();
        let job = store.get(children.last().unwrap()).unwrap().unwrap();
        if let Some(lease) = job.lease_id {
            store.release_lease(&job.id, &lease).unwrap();
        }
    }
    drop(engine);
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    assert_eq!(library_ids(&engine).len(), 64);
    {
        let mut store = lock(&engine.store).unwrap();
        confirm(
            &mut store,
            children.last().unwrap(),
            &fixture.replacement.files[0].1,
        );
        assert!(
            children
                .iter()
                .all(|id| store.get(id).unwrap().unwrap().state == "ready")
        );
        assert!(
            store
                .library_jobs()
                .unwrap()
                .iter()
                .all(|job| children.contains(&job.id))
        );
    }
    assert_eq!(fs::read(&old_path).unwrap(), old_bytes);
    drop(engine);
    let mut store = Store::open(&fixture.cfg.store_dir).unwrap();
    assert!(
        store
            .library_jobs()
            .unwrap()
            .iter()
            .all(|job| children.contains(&job.id))
    );
    drop(store);
    assert!(
        Store::open_read_only(&fixture.cfg.store_dir)
            .unwrap()
            .library_jobs()
            .unwrap()
            .iter()
            .all(|job| children.contains(&job.id))
    );
    assert_eq!(fs::read(old_path).unwrap(), old_bytes);
}

#[test]
fn partial_legacy_groups_are_refused_by_migration_without_writing() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 3);
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    baseline(&engine, &parents[0].id);
    let children = ids(&apply(
        &engine,
        &parents[0].id,
        &fixture.replacement_query(),
    ));
    {
        let mut store = lock(&engine.store).unwrap();
        confirm(&mut store, &children[0], &fixture.replacement.files[0].1);
    }
    let all = lock(&engine.store).unwrap().list().unwrap();
    drop(engine);
    let store_dir = &fixture.cfg.store_dir;
    let mut cases = Vec::new();
    // A replacement group missing its last owner.
    let mut missing = all.clone();
    missing.retain(|job| job.id != children[2]);
    cases.push((missing, "Incomplete shared ownership"));
    // One owner promoted while the others are still staged or queued.
    let mut partial = all;
    partial
        .iter_mut()
        .find(|job| job.id == children[0])
        .unwrap()
        .state = "ready".into();
    cases.push((partial, "shared replacement"));
    for (jobs, expected) in cases {
        jobs_state::downgrade(store_dir, &jobs);
        let before = snapshot(store_dir);
        let error = Store::open(store_dir).err().unwrap();
        assert!(
            error.to_lowercase().contains(&expected.to_lowercase()),
            "{error}"
        );
        assert!(Store::open_read_only(store_dir).unwrap().list().is_err());
        assert_eq!(snapshot(store_dir), before);
        assert!(!store_dir.join("jobs.idx").exists());
    }
}

#[test]
fn lineage_mutation_and_single_baselines_fail_closed() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    assert!(
        engine
            .set_baseline(&parents[0].id, "Fixture.Series.S01E01.720p.WEB-DL")
            .is_err()
    );
    baseline(&engine, &parents[0].id);
    let children = ids(&apply(
        &engine,
        &parents[0].id,
        &fixture.replacement_query(),
    ));
    {
        let mut store = lock(&engine.store).unwrap();
        let child = store.get(&children[0]).unwrap().unwrap();
        let mut wrong = child.clone();
        wrong.shared_upgrade.as_mut().unwrap().parent_jobs.pop();
        assert!(store.update(wrong).is_err());
        let mut wrong = child;
        wrong.upgrade_parent = Some(parents[1].id.clone());
        assert!(store.update(wrong).is_err());
        confirm(&mut store, &children[0], &fixture.replacement.files[0].1);
    }
}

#[test]
fn cancellation_covers_staged_owners_and_retry_rechecks_all_current_parents() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    baseline(&engine, &parents[0].id);
    let children = ids(&apply(
        &engine,
        &parents[0].id,
        &fixture.replacement_query(),
    ));
    {
        let mut store = lock(&engine.store).unwrap();
        confirm(&mut store, &children[0], &fixture.replacement.files[0].1);
        let mut last = store.get(&children[1]).unwrap().unwrap();
        last.state = "failed".into();
        last.last_error = Some("Synthetic failure".into());
        store.update(last).unwrap();
    }
    let third = video(&directory, "competing", 2);
    let mut query = fixture.replacement_query();
    query.source_url = Some(third.path.to_str().unwrap().into());
    assert!(engine.library_group(&parents[0].id, &query).is_err());
    engine.cancel(&children[1]).unwrap();
    {
        let mut store = lock(&engine.store).unwrap();
        assert!(
            children
                .iter()
                .all(|id| store.get(id).unwrap().unwrap().state == "cancelled")
        );
        assert_eq!(store.get(&children[0]).unwrap().unwrap().imports.len(), 1);
    }
    let other = ids(&apply(&engine, &parents[0].id, &query));
    assert!(engine.retry(&children[1]).is_err());
    engine.cancel(&other[0]).unwrap();
    engine.set_monitored(&parents[1].id, false).unwrap();
    assert!(engine.retry(&children[1]).is_err());
    engine.set_monitored(&parents[1].id, true).unwrap();
    engine.retry(&children[1]).unwrap();
    {
        let mut store = lock(&engine.store).unwrap();
        assert!(
            children
                .iter()
                .all(|id| store.get(id).unwrap().unwrap().state == "queued")
        );
        confirm(&mut store, &children[0], &fixture.replacement.files[0].1);
        confirm(&mut store, &children[1], &fixture.replacement.files[0].1);
        assert!(
            store
                .library_jobs()
                .unwrap()
                .iter()
                .all(|job| children.contains(&job.id))
        );
    }
    assert!(engine.retry(&other[0]).is_err());
    assert!(engine.cancel(&children[0]).is_err());
}

#[test]
fn disabling_one_parent_pauses_claims_and_prevents_partial_promotion_until_reenabled() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    baseline(&engine, &parents[0].id);
    let children = ids(&apply(
        &engine,
        &parents[0].id,
        &fixture.replacement_query(),
    ));
    {
        let mut store = lock(&engine.store).unwrap();
        confirm(&mut store, &children[0], &fixture.replacement.files[0].1);
    }
    engine.set_monitored(&parents[1].id, false).unwrap();
    {
        let mut store = lock(&engine.store).unwrap();
        assert!(store.claim(now(), 30).unwrap().is_none());
        let mut last = store.get(&children[1]).unwrap().unwrap();
        last.state = "ready".into();
        last.progress = 1.0;
        last.imports = store.get(&children[0]).unwrap().unwrap().imports;
        assert!(store.update(last).is_err());
        assert_eq!(store.get(&children[1]).unwrap().unwrap().state, "queued");
    }
    assert!(
        library_ids(&engine)
            .iter()
            .all(|id| parents.iter().any(|parent| &parent.id == id))
    );
    engine.set_monitored(&parents[1].id, true).unwrap();
    {
        let mut store = lock(&engine.store).unwrap();
        confirm(&mut store, &children[1], &fixture.replacement.files[0].1);
        assert!(
            store
                .library_jobs()
                .unwrap()
                .iter()
                .all(|job| children.contains(&job.id))
        );
    }
}

#[test]
fn stale_group_guards_bind_monitoring_quality_exact_metadata_and_existing_replacement_state() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    baseline(&engine, &parents[0].id);
    let query = fixture.replacement_query();
    let guard = guarded(&engine, &parents[0].id, &query);
    lock(&engine.store)
        .unwrap()
        .record_monitor_check(&parents[1].id, now() + 100)
        .unwrap();
    assert!(
        engine
            .library_group(&parents[0].id, &guard)
            .unwrap_err()
            .contains("preview changed")
    );
    let original = fs::read(&fixture.replacement.path).unwrap();
    let guard = guarded(&engine, &parents[0].id, &query);
    let changed = video(&directory, "changed", 3);
    fs::write(&fixture.replacement.path, fs::read(&changed.path).unwrap()).unwrap();
    assert!(
        engine
            .library_group(&parents[0].id, &guard)
            .unwrap_err()
            .contains("preview changed")
    );
    fs::write(&fixture.replacement.path, original).unwrap();
    let children = ids(&apply(&engine, &parents[0].id, &query));
    let guard = guarded(&engine, &parents[0].id, &query);
    engine.cancel(&children[0]).unwrap();
    assert!(engine.library_group(&parents[0].id, &guard).is_err());
    let reused = apply(&engine, &parents[0].id, &query);
    assert_eq!(reused.get("reused"), Some(&Value::Number(2.0)));
    assert!(
        ids(&reused)
            .iter()
            .all(|id| lock(&engine.store).unwrap().get(id).unwrap().unwrap().state == "cancelled")
    );
    assert_eq!(lock(&engine.store).unwrap().list().unwrap().len(), 4);
}

#[test]
fn partial_scope_wrong_titles_same_quality_cutoffs_and_bad_paths_never_acquire_payload() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let provider = Provider::open();
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    baseline(&engine, &parents[0].id);
    let source = provider.metadata_url("candidate", fs::read(&fixture.replacement.path).unwrap());
    let mut query = fixture.replacement_query();
    query.source_url = Some(source);
    let before = snapshot(&fixture.cfg.store_dir);
    for title in [
        "Other.Series.S01.1080p.WEB-DL",
        "Fixture.Series.S01E01.1080p.WEB-DL",
        "Fixture.Series.S01E01-E03.1080p.WEB-DL",
        "Fixture.Series.S01.720p.WEB-DL",
        "Fixture.Series.S02.1080p.WEB-DL",
    ] {
        let mut wrong = query.clone();
        wrong.release_title = title.into();
        assert!(engine.library_group(&parents[0].id, &wrong).is_err());
    }
    let mut missing = query.clone();
    missing.file_path = Some("Pack/missing.mp4".into());
    assert!(engine.library_group(&parents[0].id, &missing).is_err());
    assert_eq!(snapshot(&fixture.cfg.store_dir), before);
    assert!(GroupRequest::from_json(&json::parse(r#"{"action":"replace","release_title":"Fixture.S01.1080p","source_url":"http://127.0.0.1:1","file_path":"../bad.mp4","episodes":[1]}"#).unwrap()).is_err());
    assert!(
        engine
            .library_group_before(
                &parents[0].id,
                &query,
                Instant::now() - Duration::from_secs(1)
            )
            .is_err()
    );
    assert_eq!(lock(&engine.store).unwrap().list().unwrap().len(), 2);
}

#[test]
fn whole_group_cancellation_invalidates_an_inflight_lease_and_future_retry_uses_a_new_lease() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    baseline(&engine, &parents[0].id);
    let children = ids(&apply(
        &engine,
        &parents[0].id,
        &fixture.replacement_query(),
    ));
    let mut store = lock(&engine.store).unwrap();
    let mut stale = store.claim(now(), 30).unwrap().unwrap();
    let old_lease = stale.lease_id.clone().unwrap();
    assert!(store.claim(now(), 30).unwrap().is_none());
    store.cancel(&children[0]).unwrap();
    stale.state = "failed".into();
    assert!(store.update(stale.clone()).is_err());
    store.retry(&children[1]).unwrap();
    let fresh = store.claim(now(), 30).unwrap().unwrap();
    assert_ne!(fresh.lease_id.as_deref(), Some(old_lease.as_str()));
    assert!(store.renew(&stale.id, &old_lease, now(), 30).is_err());
    assert!(store.update(stale).is_err());
}

#[test]
fn exact_range_release_labels_and_a_reached_cutoff_are_enforced_for_the_whole_group() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let engine = Engine::open_for_management(fixture.cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    baseline(&engine, &parents[0].id);
    for title in [
        "Fixture.Series.S01E01-E02.1080p.WEB-DL",
        "Fixture.Series.S01E01E02.1080p.WEB-DL",
        "Fixture.Series.2024.S01E01-S01E02.1080p.WEB-DL",
    ] {
        let mut query = fixture.replacement_query();
        query.release_title = title.into();
        assert!(engine.library_group(&parents[0].id, &query).is_ok());
    }
    assert_eq!(lock(&engine.store).unwrap().list().unwrap().len(), 2);
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let mut cfg = fixture.cfg.clone();
    let name = cfg.selection.episode_profile.clone();
    cfg.selection
        .profiles
        .get_mut(&name)
        .unwrap()
        .cutoff_resolution = Some(720);
    let engine = Engine::open_for_management(cfg).unwrap();
    let parents = fixture.parents(&engine);
    baseline(&engine, &parents[0].id);
    assert!(
        engine
            .library_group(&parents[0].id, &fixture.replacement_query())
            .unwrap_err()
            .contains("cutoff")
    );
    assert_eq!(lock(&engine.store).unwrap().list().unwrap().len(), 2);
}

#[test]
fn native_shared_replacement_waits_for_every_exact_plex_path_across_restart() {
    let directory = Directory::new();
    let fixture = Fixture::new(&directory, 2);
    let plex = Catalog::open(Vec::new());
    let seed = Seeder::open(&directory.0.join("seed"), &[&fixture.replacement]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let mut cfg = fixture.cfg.clone();
    cfg.downloads = engine_config(&directory.0.join("engine")).downloads;
    cfg.downloads_enabled = true;
    cfg.max_attempts = 1;
    cfg.workers = 2;
    cfg.plex.enabled = true;
    cfg.plex.url = plex.url.clone();
    cfg.plex.series_section = "2".into();
    cfg.plex.token_override = Some("group-plex-fixture-secret".into());
    plex.response("/library/sections/2/refresh", 200, Value::object());
    plex.response("/library/sections/2/all",200,json::parse(r#"{"MediaContainer":{"Metadata":[{"title":"Fixture Series","year":2024,"ratingKey":"7","Guid":[{"id":"tmdb://42"}]}]}}"#).unwrap());
    let engine = Engine::open(cfg.clone()).unwrap();
    let parents = fixture.parents(&engine);
    let old_path = parents[0].imports[0].clone();
    let old_bytes = fs::read(&old_path).unwrap();
    baseline(&engine, &parents[0].id);
    let mut query = fixture.replacement_query();
    query.source_url = Some(fixture.replacement.magnet(proxy.port));
    let report = apply(&engine, &parents[0].id, &query);
    let children = ids(&report);
    let path = report
        .get("binding")
        .unwrap()
        .get("import_path")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    plex.response(
        "/library/metadata/7/allLeaves",
        200,
        leaves(&[&path, &old_path]),
    );
    engine.tick().unwrap();
    proxy.payloads_enabled.store(true, Ordering::Release);
    wait(|| {
        engine
            .transfer(&fixture.replacement.id)
            .unwrap()
            .get("selected_ready")
            == Some(&Value::Bool(true))
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let jobs = lock(&engine.store)
            .unwrap()
            .shared_group(&children[0])
            .unwrap();
        assert!(jobs.iter().all(|job| job.state != "failed"), "{jobs:?}");
        if jobs
            .iter()
            .find(|job| job.request.episode == 1)
            .unwrap()
            .state
            == "staged"
            && jobs
                .iter()
                .find(|job| job.request.episode == 2)
                .unwrap()
                .state
                == "scanning"
        {
            break;
        }
        assert!(Instant::now() < deadline, "{jobs:?}");
        engine.tick().unwrap();
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        library_ids(&engine)
            .iter()
            .all(|id| parents.iter().any(|parent| &parent.id == id))
    );
    assert_eq!(fs::read(&path).unwrap(), fixture.replacement.files[0].1);
    assert_eq!(fs::read(&old_path).unwrap(), old_bytes);
    drop(engine);
    proxy.wait_idle();
    let engine = Engine::open(cfg.clone()).unwrap();
    assert!(
        lock(&engine.store)
            .unwrap()
            .shared_group(&children[0])
            .unwrap()
            .iter()
            .any(|job| job.state == "staged")
    );
    plex.response(
        "/library/metadata/7/allLeaves",
        200,
        leaves(&[&path, &path]),
    );
    {
        let mut store = lock(&engine.store).unwrap();
        let mut pending = store
            .shared_group(&children[0])
            .unwrap()
            .into_iter()
            .find(|job| job.state == "scanning")
            .unwrap();
        pending.next_attempt_at = 0;
        store.update(pending).unwrap();
    }
    engine.tick().unwrap();
    assert!(library_ids(&engine).iter().all(|id| children.contains(id)));
    assert_eq!(fs::read(old_path).unwrap(), old_bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let native = cfg
            .downloads
            .data_dir
            .join(&fixture.replacement.id)
            .join("Pack/shared.mp4");
        assert_eq!(fs::metadata(&native).unwrap().nlink(), 1);
        assert_eq!(fs::metadata(&path).unwrap().nlink(), 1);
        assert_ne!(
            fs::metadata(native).unwrap().ino(),
            fs::metadata(&path).unwrap().ino()
        );
    }
    assert!(
        !cfg.downloads
            .data_dir
            .join(&fixture.replacement.id)
            .join("Pack/untouched.txt")
            .exists()
    );
}
