//! Original direct-media library fixtures. Only local NNTP, Newznab and Plex.
use super::requester_support as accounts;
use super::*;
use mynou::{
    store::Job,
    usenet::queue::{Client, Owner},
};
use std::{os::unix::fs::PermissionsExt, path::Path};
const MEDIA: &[u8] = include_bytes!("../../examples/demo.mp4");

pub(super) fn configured(d: &Directory, p: &Provider, h: &Http) -> Config {
    let mut v = value(p, h);
    source(&mut v)
        .get_mut("usenet")
        .unwrap()
        .insert("maximum_bytes", 1_048_576_u32);
    v.get_mut("usenet")
        .unwrap()
        .get_mut("downloads")
        .unwrap()
        .insert("enabled", true);
    v.insert("poll_interval_ms", 100_u32);
    config::from_json(&v, &d.0).unwrap()
}
fn articles(p: &Provider, name: &str, count: usize) {
    for n in 1..=count {
        p.set(
            &format!("file0-part{n}@fixture.test"),
            usenet_support::Response::Body(usenet_support::article_named(MEDIA, n, count, name)),
        );
    }
}
pub(super) fn retained(engine: &Engine, id: &str) -> Job {
    lock(&engine.store).unwrap().get(id).unwrap()
}
pub(super) fn progress(engine: &Engine) -> Value {
    engine
        .usenet_queue()
        .unwrap()
        .get("records")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .clone()
}
pub(super) fn wait_state(engine: &Arc<Engine>, id: &str, state: &str) -> Job {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let job = retained(engine, id);
        if job.state == state {
            return job;
        }
        assert!(
            job.state != "failed",
            "Original job failed before {state}: {:?}",
            job.last_error
        );
        assert!(
            Instant::now() < deadline,
            "Original job did not reach {state}: {} {:?}",
            job.state,
            job.last_error
        );
        thread::sleep(Duration::from_millis(5));
    }
}
pub(super) fn write_snapshot(path: &Path, value: &Value, magic: &[u8; 8]) {
    let payload = json::stringify(value).into_bytes();
    let mut bytes = magic.to_vec();
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend(payload);
    bytes.extend_from_slice(&sha256(&bytes));
    fs::write(path, bytes).unwrap();
}
pub(super) fn read_snapshot(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap();
    json::parse(std::str::from_utf8(&bytes[16..bytes.len() - 32]).unwrap()).unwrap()
}
fn first<'a>(v: &'a mut Value, key: &str) -> &'a mut Value {
    let Value::Array(items) = v.get_mut(key).unwrap() else {
        panic!("Original snapshot array")
    };
    &mut items[0]
}

#[test]
fn native_direct_media_reaches_the_library_without_a_torrent_client() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    *h.document.lock().unwrap() = (200, usenet_support::source(1, 2));
    articles(&p, &format!("{TITLE}.mp4"), 2);
    let c = configured(&d, &p, &h);
    let engine = Engine::open(c.clone()).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    assert!(engine.tick().unwrap());
    let prepared = retained(&engine, &id);
    assert_eq!(prepared.state, "downloading");
    assert!(prepared.download_id.is_none());
    let origin = prepared.usenet_origin.as_ref().unwrap();
    assert_eq!(
        origin.document.as_ref().unwrap().source_id,
        mynou::usenet::nzb::Nzb::parse(&usenet_support::source(1, 2))
            .unwrap()
            .id
    );
    assert!(origin.transfer_id.is_some());
    assert!(p.requests.lock().unwrap().is_empty());
    assert_eq!(h.count(), 2);
    let workers = engine.start();
    let ready = wait_state(&engine, &id, "ready");
    assert_eq!(fs::read(&ready.imports[0]).unwrap(), MEDIA);
    {
        use std::os::unix::fs::MetadataExt;
        let source = fs::metadata(&ready.files[0]).unwrap();
        let imported = fs::metadata(&ready.imports[0]).unwrap();
        assert_eq!(source.nlink(), 1);
        assert_eq!(source.mode() & 0o077, 0);
        assert_ne!(
            (source.dev(), source.ino()),
            (imported.dev(), imported.ino())
        );
    }
    assert_eq!(p.requests.lock().unwrap().len(), 2);
    assert_eq!(h.count(), 2);
    assert_eq!(ready.release.as_ref().unwrap().title, TITLE);
    let public = mynou::engine::public_job(&ready);
    let projection = json::stringify(public.get("usenet_origin").unwrap());
    assert!(!projection.contains("binding"));
    assert!(!projection.contains(PRIVATE_KEY));
    assert!(!projection.contains("@fixture.test"));
    assert!(ready.download_id.is_none());
    assert_eq!(
        progress(&engine).get("owner_authorized"),
        Some(&Value::Bool(false))
    );
    drop(workers);
    lock(&engine.store).unwrap().compact().unwrap();
    assert_eq!(
        &fs::read(c.store_dir.join("snapshot.bin")).unwrap()[..8],
        b"MYNOUS06"
    );
    drop(engine);
    let reopened = Engine::open_for_preview(c.clone()).unwrap();
    assert_eq!(retained(&reopened, &id).usenet_origin, ready.usenet_origin);
    assert_eq!(
        progress(&reopened).get("owner_authorized"),
        Some(&Value::Bool(false))
    );
    drop(reopened);
    let snapshot = c.store_dir.join("snapshot.bin");
    let data = read_snapshot(&snapshot);
    write_snapshot(&snapshot, &data, b"MYNOUS05");
    let before = files(&d.0);
    assert!(Engine::open(c).is_err());
    assert_eq!(files(&d.0), before);
}

#[test]
fn constructor_rejects_an_orphan_before_tail_repair_or_permission_changes() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let c = configured(&d, &p, &h);
    drop(Engine::open(c.clone()).unwrap());
    let queue = Client::open(&c.usenet, false).unwrap();
    queue
        .stage_owned(
            &usenet_support::source(1, 1),
            "original",
            &c.usenet.servers[0].binding(),
            0,
            &Owner {
                job_id: "orphan-job".into(),
                binding: "b".repeat(64),
            },
        )
        .unwrap();
    drop(queue);
    let journal = c.store_dir.join("journal.bin");
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(b"partial")
        .unwrap();
    fs::set_permissions(&c.store_dir, fs::Permissions::from_mode(0o750)).unwrap();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o640)).unwrap();
    let before = files(&d.0);
    assert!(Engine::open(c.clone()).is_err());
    assert_eq!(files(&d.0), before);
    assert_eq!(
        fs::metadata(&c.store_dir).unwrap().permissions().mode() & 0o777,
        0o750
    );
    assert_eq!(
        fs::metadata(&journal).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn valid_held_preparation_is_joined_after_restart_without_refetching_the_document() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(&p, &format!("{TITLE}.mp4"), 1);
    let c = configured(&d, &p, &h);
    let engine = Engine::open(c.clone()).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    engine.tick().unwrap();
    let original = retained(&engine, &id);
    lock(&engine.store).unwrap().compact().unwrap();
    drop(engine);
    // Original crash window: document intent and held preparation are durable,
    // but the successful preparation has not yet been linked in the journal.
    let snapshot = c.store_dir.join("snapshot.bin");
    let mut journal = read_snapshot(&snapshot);
    first(&mut journal, "jobs")
        .get_mut("usenet_origin")
        .unwrap()
        .insert("transfer_id", Value::Null);
    write_snapshot(&snapshot, &journal, b"MYNOUS06");
    let queue = c
        .usenet
        .downloads
        .as_ref()
        .unwrap()
        .state_dir
        .join("queue.bin");
    let mut data = read_snapshot(&queue);
    first(&mut data, "records").insert("state", "held");
    write_snapshot(&queue, &data, b"MYNOUU02");
    let engine = Engine::open(c).unwrap();
    assert_eq!(
        progress(&engine).get("owner_authorized"),
        Some(&Value::Bool(false))
    );
    let workers = engine.start();
    let ready = wait_state(&engine, &id, "ready");
    assert_eq!(
        ready.usenet_origin.as_ref().unwrap().transfer_id,
        original.usenet_origin.as_ref().unwrap().transfer_id
    );
    assert_eq!(h.count(), 2);
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    drop(workers);
}

#[test]
fn joint_document_conflict_preserves_journal_queue_and_private_storage() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let c = configured(&d, &p, &h);
    let engine = Engine::open(c.clone()).unwrap();
    engine.submit(movie()).unwrap();
    engine.tick().unwrap();
    lock(&engine.store).unwrap().compact().unwrap();
    drop(engine);
    let snapshot = c.store_dir.join("snapshot.bin");
    let mut data = read_snapshot(&snapshot);
    first(&mut data, "jobs")
        .get_mut("usenet_origin")
        .unwrap()
        .get_mut("document")
        .unwrap()
        .insert("source_id", "c".repeat(64));
    write_snapshot(&snapshot, &data, b"MYNOUS06");
    let before = files(&d.0);
    assert!(Engine::open(c).is_err());
    assert_eq!(files(&d.0), before);
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn cancellation_rejects_a_blocked_document_without_preparing_articles() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let c = configured(&d, &p, &h);
    let engine = Engine::open(c).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    h.document_blocked.store(true, Ordering::Release);
    let e = engine.clone();
    let worker = thread::spawn(move || e.tick());
    usenet_support::wait(|| h.count() >= 2);
    assert!(retained(&engine, &id).usenet_origin.is_some());
    engine.cancel(&id).unwrap();
    h.document_blocked.store(false, Ordering::Release);
    worker.join().unwrap().unwrap();
    let job = retained(&engine, &id);
    assert_eq!(job.state, "cancelled");
    assert!(job.usenet_origin.as_ref().unwrap().document.is_none());
    assert!(
        engine
            .usenet_queue()
            .unwrap()
            .get("records")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn cancellation_fences_a_late_article_and_releases_its_slot_after_return() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(&p, &format!("{TITLE}.mp4"), 1);
    let engine = Engine::open(configured(&d, &p, &h)).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    engine.tick().unwrap();
    p.gate.store(true, Ordering::Release);
    let workers = engine.start();
    p.wait_requests(1);
    engine.cancel(&id).unwrap();
    assert_eq!(
        progress(&engine).get("owner_authorized"),
        Some(&Value::Bool(false))
    );
    p.gate.store(false, Ordering::Release);
    usenet_support::wait(|| {
        progress(&engine).get("state").and_then(Value::as_str) == Some("paused")
            && engine
                .usenet_queue()
                .unwrap()
                .get("active")
                .and_then(Value::as_u64)
                == Some(0)
    });
    assert_eq!(
        progress(&engine)
            .get("verified_parts")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        progress(&engine).get("attempts").and_then(Value::as_u64),
        Some(1)
    );
    let cancelled = retained(&engine, &id);
    engine.retry(&id).unwrap();
    let ready = wait_state(&engine, &id, "ready");
    assert_eq!(ready.usenet_origin, cancelled.usenet_origin);
    assert_eq!(p.requests.lock().unwrap().len(), 2);
    assert_eq!(h.count(), 2);
    drop(workers);
}

#[test]
fn failed_article_budget_cannot_be_reset_by_retry_restart_or_larger_settings() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let mut c = configured(&d, &p, &h);
    c.usenet.downloads.as_mut().unwrap().max_attempts = 1;
    let engine = Engine::open(c.clone()).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    let workers = engine.start();
    let failed = wait_state(&engine, &id, "failed");
    assert_eq!(failed.next_attempt_at, 0);
    assert!(engine.retry(&id).is_err());
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    drop(workers);
    drop(engine);
    c.usenet.downloads.as_mut().unwrap().max_attempts = 10;
    let engine = Engine::open(c).unwrap();
    assert_eq!(retained(&engine, &id).usenet_origin, failed.usenet_origin);
    assert!(engine.retry(&id).is_err());
    assert_eq!(
        progress(&engine).get("attempts").and_then(Value::as_u64),
        Some(1)
    );
    assert!(lock(&engine.store).unwrap().library_jobs().is_empty());
    assert_eq!(h.count(), 2);
}

#[test]
fn multiple_file_documents_are_withheld_before_any_article_acquisition() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    *h.document.lock().unwrap() = (200, usenet_support::source(2, 1));
    let engine = Engine::open(configured(&d, &p, &h)).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    engine.tick().unwrap();
    let failed = retained(&engine, &id);
    assert_eq!(failed.state, "failed");
    assert!(failed.usenet_origin.as_ref().unwrap().document.is_none());
    assert!(
        engine
            .usenet_queue()
            .unwrap()
            .get("records")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn decoded_title_year_archive_and_media_errors_never_enter_the_library() {
    for name in [
        "Different.Movie.2024.1080p.mp4",
        "Fixture.Movie.2025.1080p.mp4",
        "Fixture.Movie.2024.1080p.rar",
        "Fixture.Movie.2024.1080p.mp3",
    ] {
        let d = Directory::new();
        let p = Provider::open();
        let h = Http::open();
        articles(&p, name, 1);
        let engine = Engine::open(configured(&d, &p, &h)).unwrap();
        let id = engine.submit(movie()).unwrap().remove(0).id;
        let workers = engine.start();
        let failed = wait_state(&engine, &id, "failed");
        assert!(failed.imports.is_empty(), "{name}");
        assert!(lock(&engine.store).unwrap().library_jobs().is_empty());
        assert_eq!(
            progress(&engine).get("owner_authorized"),
            Some(&Value::Bool(false))
        );
        drop(workers);
    }
}

#[test]
fn verified_actual_size_must_satisfy_the_captured_newznab_policy() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(&p, &format!("{TITLE}.mp4"), 1);
    let mut v = value(&p, &h);
    source(&mut v)
        .get_mut("usenet")
        .unwrap()
        .insert("maximum_bytes", 1_048_576_u32);
    v.get_mut("usenet")
        .unwrap()
        .get_mut("downloads")
        .unwrap()
        .insert("enabled", true);
    source(&mut v)
        .get_mut("usenet")
        .unwrap()
        .insert("minimum_bytes", (MEDIA.len() + 1) as u32);
    h.replace(&rss(&item(TITLE, &(MEDIA.len() + 1024).to_string(), "")));
    let engine = Engine::open(config::from_json(&v, &d.0).unwrap()).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    let workers = engine.start();
    let failed = wait_state(&engine, &id, "failed");
    assert!(
        failed
            .last_error
            .as_ref()
            .unwrap()
            .contains("verified size")
    );
    assert!(failed.imports.is_empty());
    drop(workers);
}

#[test]
fn valid_yenc_checksums_cannot_admit_an_invalid_media_payload() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    p.set(
        "file0-part1@fixture.test",
        usenet_support::Response::Body(usenet_support::article_named(
            b"Original invalid media fixture",
            1,
            1,
            &format!("{TITLE}.mp4"),
        )),
    );
    let engine = Engine::open(configured(&d, &p, &h)).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    let workers = engine.start();
    let failed = wait_state(&engine, &id, "failed");
    assert!(failed.imports.is_empty());
    assert!(lock(&engine.store).unwrap().library_jobs().is_empty());
    assert_eq!(
        progress(&engine).get("state").and_then(Value::as_str),
        Some("complete")
    );
    drop(workers);
}

#[test]
fn requester_removal_revokes_native_article_permission_before_returning() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let a = accounts::Accounts::open();
    articles(&p, &format!("{TITLE}.mp4"), 1);
    let mut c = configured(&d, &p, &h);
    c.requesters = a.config(&d.0).requesters;
    a.watchlist("alice", vec![accounts::movie(42, "Fixture Movie")]);
    let engine = Engine::open(c).unwrap();
    accounts::enable(&engine, "alice", false);
    engine.sync_requesters().unwrap();
    let job = accounts::job(&engine, "alice");
    engine.tick().unwrap();
    // Keep the already authorized queue worker live, while preventing unrelated
    // job polls and watchlist reconciliation from invalidating this review.
    let mut waiting = retained(&engine, &job.id);
    waiting.next_attempt_at = mynou::store::now().saturating_add(60);
    lock(&engine.store).unwrap().update(waiting).unwrap();
    a.blocked.store(true, Ordering::Release);
    p.gate.store(true, Ordering::Release);
    let workers = engine.start();
    p.wait_requests(1);
    let demand = accounts::demand(&engine, "alice");
    accounts::apply(
        &engine,
        "alice",
        accounts::demand_query("remove", accounts::id(&demand)),
    );
    assert_eq!(retained(&engine, &job.id).state, "cancelled");
    assert_eq!(
        progress(&engine).get("owner_authorized"),
        Some(&Value::Bool(false))
    );
    a.blocked.store(false, Ordering::Release);
    p.gate.store(false, Ordering::Release);
    usenet_support::wait(|| {
        engine
            .usenet_queue()
            .unwrap()
            .get("active")
            .and_then(Value::as_u64)
            == Some(0)
    });
    assert_eq!(
        progress(&engine)
            .get("verified_parts")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert!(engine.retry(&job.id).is_err());
    drop(workers);
}

#[test]
fn active_and_daily_requester_quotas_withhold_other_usenet_acquisitions() {
    for (active, daily) in [(1, 2), (2, 1)] {
        let d = Directory::new();
        let p = Provider::open();
        let h = Http::open();
        let a = accounts::Accounts::open();
        let mut c = configured(&d, &p, &h);
        c.requesters = a.config(&d.0).requesters;
        a.watchlist(
            "alice",
            vec![
                accounts::movie(42, "Fixture Movie"),
                accounts::movie(43, "Other Fixture"),
            ],
        );
        let engine = Engine::open(c).unwrap();
        let mut policy = accounts::policy(&engine, "alice");
        policy.enabled = true;
        policy.max_active = active;
        policy.max_daily = daily;
        accounts::apply(&engine, "alice", accounts::policy_query(policy));
        engine.sync_requesters().unwrap();
        let demands = accounts::demands(&engine, "alice");
        let first = demands
            .iter()
            .find(|d| {
                d.get("request")
                    .unwrap()
                    .get("title")
                    .and_then(Value::as_str)
                    == Some("Fixture Movie")
            })
            .unwrap();
        let second = demands
            .iter()
            .find(|d| {
                d.get("request")
                    .unwrap()
                    .get("title")
                    .and_then(Value::as_str)
                    == Some("Other Fixture")
            })
            .unwrap();
        accounts::apply(
            &engine,
            "alice",
            accounts::demand_query("approve", accounts::id(first)),
        );
        accounts::apply(
            &engine,
            "alice",
            accounts::demand_query("approve", accounts::id(second)),
        );
        let second = accounts::demands(&engine, "alice")
            .into_iter()
            .find(|d| accounts::id(d) == accounts::id(second))
            .unwrap();
        assert_eq!(second.get("state").and_then(Value::as_str), Some("quota"));
        assert_eq!(second.get("job_id"), Some(&Value::Null));
        assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
        engine.tick().unwrap();
        assert_eq!(h.count(), 2);
        assert_eq!(
            engine
                .usenet_queue()
                .unwrap()
                .get("records")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(p.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn queued_library_polling_retains_one_reservation_during_a_delayed_article_response() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(&p, &format!("{TITLE}.mp4"), 1);
    let mut v = value(&p, &h);
    source(&mut v)
        .get_mut("usenet")
        .unwrap()
        .insert("maximum_bytes", 1_048_576_u32);
    v.get_mut("usenet")
        .unwrap()
        .get_mut("downloads")
        .unwrap()
        .insert("enabled", true);
    let Value::Array(servers) = v.get_mut("usenet").unwrap().get_mut("servers").unwrap() else {
        panic!()
    };
    servers[0].insert("timeout_ms", 4000_u32);
    v.insert("lease_duration_secs", 5_u32);
    v.insert("poll_interval_ms", 50_u32);
    let engine = Engine::open(config::from_json(&v, &d.0).unwrap()).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    engine.tick().unwrap();
    p.gate.store(true, Ordering::Release);
    let workers = engine.start();
    p.wait_requests(1);
    thread::sleep(Duration::from_millis(2200));
    assert_eq!(
        progress(&engine).get("owner_authorized"),
        Some(&Value::Bool(true))
    );
    p.gate.store(false, Ordering::Release);
    let ready = wait_state(&engine, &id, "ready");
    assert_eq!(fs::read(&ready.imports[0]).unwrap(), MEDIA);
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    assert_eq!(
        progress(&engine).get("attempts").and_then(Value::as_u64),
        Some(1)
    );
    drop(workers);
}

#[test]
fn source_absolute_numbering_preserves_the_canonical_episode_destination() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let title = "Fixture.Series.007.1080p.WEB-DL.x264";
    h.replace(&rss(&item(title, "1024", "")));
    articles(&p, &format!("{title}.mp4"), 1);
    let mut request = movie();
    request.kind = "episode".into();
    request.title = "Fixture Series".into();
    request.season = 1;
    request.episode = 2;
    request.source_numbering = Some(SourceNumber::Absolute(7));
    let engine = Engine::open(configured(&d, &p, &h)).unwrap();
    let id = engine.submit(request).unwrap().remove(0).id;
    let workers = engine.start();
    let ready = wait_state(&engine, &id, "ready");
    assert!(ready.imports[0].contains("S01E02"));
    assert_eq!(
        ready.request.source_numbering,
        Some(SourceNumber::Absolute(7))
    );
    assert_eq!(fs::read(&ready.imports[0]).unwrap(), MEDIA);
    drop(workers);
}

#[test]
fn requester_approval_captured_route_and_exact_plex_confirmation_gate_native_usenet() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let a = accounts::Accounts::open();
    articles(&p, &format!("{TITLE}.mp4"), 1);
    let mut c = configured(&d, &p, &h);
    c.requesters = a.config(&d.0).requesters;
    c.plex.enabled = true;
    c.plex.url = a.url.clone();
    c.plex.token_env = "PATH".into();
    c.plex.token_override = None;
    a.watchlist("alice", vec![accounts::movie(42, "Fixture Movie")]);
    a.response("/library/sections/1/refresh", 200, Value::object());
    let mut old = accounts::movie(42, "Fixture Movie");
    let mut part = Value::object();
    part.insert("file", "/unrelated/Fixture Movie.mp4");
    let mut media = Value::object();
    media.insert("Part", Value::Array(vec![part]));
    old.insert("Media", Value::Array(vec![media]));
    a.response(
        "/library/sections/1/all",
        200,
        accounts::container(vec![old]),
    );
    let engine = Engine::open(c.clone()).unwrap();
    let mut policy = accounts::policy(&engine, "alice");
    policy.enabled = true;
    policy.destination = "family".into();
    policy.max_active = 1;
    accounts::apply(&engine, "alice", accounts::policy_query(policy));
    engine.sync_requesters().unwrap();
    assert!(!engine.tick().unwrap());
    assert_eq!(h.count(), 0);
    assert!(p.requests.lock().unwrap().is_empty());
    let demand = accounts::demand(&engine, "alice");
    accounts::apply(
        &engine,
        "alice",
        accounts::demand_query("approve", accounts::id(&demand)),
    );
    let admitted = accounts::job(&engine, "alice");
    let id = admitted.id.clone();
    let mut policy = accounts::policy(&engine, "alice");
    policy.destination = "default".into();
    accounts::apply(&engine, "alice", accounts::policy_query(policy));
    drop(engine);
    c.selection.profiles.get_mut("any").unwrap().blocked_terms = vec!["fixture".into()];
    let engine = Engine::open(c.clone()).unwrap();
    let workers = engine.start();
    let imported = wait_state(&engine, &id, "scanning");
    assert_eq!(imported.requester, admitted.requester);
    assert!(Path::new(&imported.imports[0]).starts_with(&c.requesters.destinations[0].movies_root));
    let mut actual = accounts::movie(42, "Fixture Movie");
    let mut part = Value::object();
    part.insert("file", imported.imports[0].clone());
    let mut media = Value::object();
    media.insert("Part", Value::Array(vec![part]));
    actual.insert("Media", Value::Array(vec![media]));
    a.response(
        "/library/sections/1/all",
        200,
        accounts::container(vec![actual]),
    );
    let ready = wait_state(&engine, &id, "ready");
    assert_eq!(fs::read(&ready.imports[0]).unwrap(), MEDIA);
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    assert_eq!(h.count(), 2);
    drop(workers);
}

#[test]
fn protected_api_and_browser_keep_raw_controls_away_from_an_admitted_owner() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(&p, &format!("{TITLE}.mp4"), 1);
    p.gate.store(true, Ordering::Release);
    let c = configured(&d, &p, &h);
    let queue_path = c
        .usenet
        .downloads
        .as_ref()
        .unwrap()
        .state_dir
        .join("queue.bin");
    let server = Server::open(c);
    let id = server.engine.submit(movie()).unwrap().remove(0).id;
    server.engine.tick().unwrap();
    let workers = server.engine.start();
    p.wait_requests(1);
    let job = retained(&server.engine, &id);
    let transfer_id = job
        .usenet_origin
        .as_ref()
        .unwrap()
        .transfer_id
        .as_ref()
        .unwrap();
    let before = fs::read(&queue_path).unwrap();
    let route = format!("/api/usenet/queue/{transfer_id}/control");
    let reply = server.call(
        "POST",
        &route,
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", "application/json"),
        ],
        r#"{"action":"resume"}"#,
    );
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(reply.body.contains("owning library job"));
    let browser = Browser::login(&server);
    let page = browser.get(&server, "/ui/usenet");
    assert_eq!(page.status, 200);
    assert!(
        page.body
            .contains("The library job controls this transfer.")
    );
    assert!(!page.body.contains("/ui/usenet/control"));
    page.no_secrets();
    let page = browser.get(&server, &format!("/ui/jobs/{id}"));
    assert_eq!(page.status, 200);
    assert!(page.body.contains("Open native Usenet transfers"));
    assert!(!page.body.contains(PRIVATE_KEY));
    page.no_secrets();
    let public = server.call(
        "GET",
        &format!("/api/jobs/{id}"),
        &[("Authorization", &format!("Bearer {TOKEN}"))],
        "",
    );
    assert_eq!(public.status, 200);
    assert!(!public.body.contains("binding"));
    assert!(!public.body.contains(PRIVATE_KEY));
    assert_eq!(fs::read(queue_path).unwrap(), before);
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    assert!(retained(&server.engine, &id).imports.is_empty());
    p.gate.store(false, Ordering::Release);
    drop(workers);
}
