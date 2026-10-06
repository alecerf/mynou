//! Original ZIP-to-library fixtures. All indexer, article and Plex calls are loopback.
use super::admission::{configured, progress, read_snapshot, retained, wait_state, write_snapshot};
use super::archive_support::{Fixture, archive, hex, stored_zip};
use super::requester_support as accounts;
use super::*;
use mynou::{archive::Limits, requesters::digest, store::Job};
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

const MEDIA: &[u8] = include_bytes!("../../examples/demo.mp4");

fn enabled(d: &Directory, p: &Provider, h: &Http) -> Config {
    let mut c = configured(d, p, h);
    c.usenet.downloads.as_mut().unwrap().zip = Some(Limits::default());
    c
}
fn articles(p: &Provider, bytes: &[u8], title: &str) {
    p.set(
        "file0-part1@fixture.test",
        usenet_support::Response::Body(usenet_support::article_named(
            bytes,
            1,
            1,
            &format!("{title}.zip"),
        )),
    );
}
fn namespace(c: &Config) -> std::path::PathBuf {
    c.usenet
        .downloads
        .as_ref()
        .unwrap()
        .state_dir
        .join("archives")
}
fn fail(c: Config, p: &Provider, h: &Http) -> Job {
    let engine = Engine::open(c).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    let workers = engine.start();
    let failed = wait_state(&engine, &id, "failed");
    assert!(failed.imports.is_empty());
    assert!(lock(&engine.store).unwrap().library_jobs().is_empty());
    assert_eq!(
        progress(&engine).get("state").and_then(Value::as_str),
        Some("complete")
    );
    assert_eq!(
        progress(&engine).get("owner_authorized"),
        Some(&Value::Bool(false))
    );
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    assert_eq!(h.count(), 2);
    drop(workers);
    failed
}

#[test]
fn stored_and_deflated_media_have_captured_proofs_and_independent_library_inodes() {
    let compressed = hex(include_str!("../archive_support/demo.hex"));
    for deflated in [false, true] {
        let d = Directory::new();
        let p = Provider::open();
        let h = Http::open();
        let name = format!("Original/{TITLE}.mp4");
        let mut entry = Fixture::stored(&name, MEDIA);
        if deflated {
            entry.deflate = Some(&compressed);
            entry.descriptor = Some(true);
        }
        let bytes = archive(&[
            Fixture::stored("Original/original.nfo", b"Original synthetic release"),
            entry,
        ]);
        articles(&p, &bytes, TITLE);
        let c = enabled(&d, &p, &h);
        let engine = Engine::open(c.clone()).unwrap();
        let id = engine.submit(movie()).unwrap().remove(0).id;
        let workers = engine.start();
        let ready = wait_state(&engine, &id, "ready");
        let origin = ready.usenet_origin.as_ref().unwrap();
        let prepared = origin.archive.as_ref().unwrap();
        let proof = prepared.output.as_ref().unwrap();
        assert_eq!(prepared.plan.owner, origin.owner(&ready));
        assert_eq!(
            Some(&prepared.plan.transfer_id),
            origin.transfer_id.as_ref()
        );
        assert_eq!(prepared.plan.source_sha256, digest(&bytes));
        assert_eq!(prepared.plan.source_bytes, bytes.len() as u64);
        assert_eq!(prepared.plan.entry_name, name);
        assert_eq!(prepared.plan.entry_index, 1);
        assert_eq!(
            prepared.plan.entry_method,
            if deflated { "deflate" } else { "stored" }
        );
        assert_eq!(prepared.plan.limits, Limits::default());
        assert_eq!(proof.bytes, MEDIA.len() as u64);
        assert_eq!(proof.crc32, super::archive_support::reference_crc(MEDIA));
        assert_eq!(proof.sha256, digest(MEDIA));
        assert_eq!(ready.files.len(), 1);
        assert_eq!(ready.imports.len(), 1);
        assert_eq!(
            Path::new(&ready.files[0]).parent().unwrap(),
            namespace(&c)
                .join(&prepared.plan.transfer_id)
                .join("output")
        );
        assert_eq!(fs::read(&ready.files[0]).unwrap(), MEDIA);
        assert_eq!(fs::read(&ready.imports[0]).unwrap(), MEDIA);
        assert!(
            !Path::new(&ready.files[0])
                .parent()
                .unwrap()
                .join("original.nfo")
                .exists()
        );
        let private = fs::metadata(&ready.files[0]).unwrap();
        let imported = fs::metadata(&ready.imports[0]).unwrap();
        assert_eq!(private.nlink(), 1);
        assert_eq!(private.mode() & 0o077, 0);
        assert_ne!(
            (private.dev(), private.ino()),
            (imported.dev(), imported.ino())
        );
        assert!(ready.download_id.is_none());
        assert_eq!(p.requests.lock().unwrap().len(), 1);
        assert_eq!(h.count(), 2);
        let public = mynou::engine::public_job(&ready);
        let projection = public.get("usenet_origin").unwrap();
        assert_eq!(projection.get("archive_verified"), Some(&Value::Bool(true)));
        let public_text = json::stringify(projection);
        for private in [
            "binding",
            "entry_name",
            "source_sha256",
            "limits",
            PRIVATE_KEY,
        ] {
            assert!(!public_text.contains(private), "{private}");
        }
        drop(workers);
        lock(&engine.store).unwrap().compact().unwrap();
        drop(engine);
        let snapshot = c.store_dir.join("snapshot.bin");
        assert_eq!(&fs::read(&snapshot).unwrap()[..8], b"MYNOUS07");
        let before = files(&d.0);
        let reopened = Engine::open_for_preview(c.clone()).unwrap();
        assert_eq!(retained(&reopened, &id).usenet_origin, ready.usenet_origin);
        drop(reopened);
        assert_eq!(files(&d.0), before);
        let data = read_snapshot(&snapshot);
        write_snapshot(&snapshot, &data, b"MYNOUS06");
        let before = files(&d.0);
        assert!(Engine::open(c).is_err());
        assert_eq!(files(&d.0), before);
    }
}

#[test]
fn legacy_direct_media_configuration_does_not_enable_zip_extraction() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(&p, &stored_zip(&format!("{TITLE}.mp4"), MEDIA), TITLE);
    let c = configured(&d, &p, &h);
    let root = namespace(&c);
    let failed = fail(c, &p, &h);
    assert!(
        failed
            .usenet_origin
            .as_ref()
            .unwrap()
            .archive_limits
            .is_none()
    );
    assert!(failed.usenet_origin.as_ref().unwrap().archive.is_none());
    assert!(!root.exists());
}

#[test]
fn multiple_media_entries_cannot_implicitly_choose_a_film_or_sample() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let name = format!("{TITLE}.mp4");
    let bytes = archive(&[
        Fixture::stored(&name, MEDIA),
        Fixture::stored("sample.mp4", MEDIA),
    ]);
    articles(&p, &bytes, TITLE);
    let c = enabled(&d, &p, &h);
    let root = namespace(&c);
    let failed = fail(c, &p, &h);
    assert!(failed.last_error.as_ref().unwrap().contains("exactly one"));
    assert!(failed.usenet_origin.as_ref().unwrap().archive.is_none());
    assert!(!root.exists());
}

#[test]
fn inner_title_year_and_profile_must_match_before_an_extraction_intent() {
    for name in [
        "Different.Movie.2024.1080p.mp4",
        "Fixture.Movie.2025.1080p.mp4",
        "Fixture.Movie.2024.1080p.blocked.mp4",
    ] {
        let d = Directory::new();
        let p = Provider::open();
        let h = Http::open();
        articles(&p, &stored_zip(name, MEDIA), TITLE);
        let mut c = enabled(&d, &p, &h);
        c.selection.profiles.get_mut("any").unwrap().blocked_terms = vec!["blocked".into()];
        let root = namespace(&c);
        let failed = fail(c, &p, &h);
        assert!(
            failed.usenet_origin.as_ref().unwrap().archive.is_none(),
            "{name}"
        );
        assert!(!root.exists(), "{name}");
    }
}

#[test]
fn corrupt_payload_crc_keeps_partial_bytes_private_and_article_proofs_valid() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let name = format!("{TITLE}.mp4");
    let mut bytes = stored_zip(&name, MEDIA);
    bytes[30 + name.len() + 100] ^= 1;
    articles(&p, &bytes, TITLE);
    let c = enabled(&d, &p, &h);
    let root = namespace(&c);
    let failed = fail(c.clone(), &p, &h);
    let prepared = failed
        .usenet_origin
        .as_ref()
        .unwrap()
        .archive
        .as_ref()
        .unwrap();
    assert!(prepared.output.is_none());
    assert!(failed.files.is_empty());
    assert!(
        root.join(&prepared.plan.transfer_id)
            .join("archive.bin")
            .exists()
    );
    let engine = Engine::open_for_preview(c).unwrap();
    assert_eq!(
        retained(&engine, &failed.id).usenet_origin,
        failed.usenet_origin
    );
    assert_eq!(p.requests.lock().unwrap().len(), 1);
}

#[test]
fn zip_checksums_do_not_admit_an_invalid_media_container() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(
        &p,
        &stored_zip(&format!("{TITLE}.mp4"), b"Original invalid media fixture"),
        TITLE,
    );
    let c = enabled(&d, &p, &h);
    let failed = fail(c.clone(), &p, &h);
    assert!(
        failed
            .usenet_origin
            .as_ref()
            .unwrap()
            .archive
            .as_ref()
            .unwrap()
            .output
            .is_some()
    );
    let engine = Engine::open_for_preview(c).unwrap();
    assert_eq!(
        retained(&engine, &failed.id).usenet_origin,
        failed.usenet_origin
    );
}

#[test]
fn captured_zip_bounds_cannot_be_raised_after_selection_and_restart() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(&p, &stored_zip(&format!("{TITLE}.mp4"), MEDIA), TITLE);
    let mut c = enabled(&d, &p, &h);
    c.usenet
        .downloads
        .as_mut()
        .unwrap()
        .zip
        .as_mut()
        .unwrap()
        .max_entry_bytes = MEDIA.len() as u64 - 1;
    let engine = Engine::open(c.clone()).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    engine.tick().unwrap();
    let captured = retained(&engine, &id)
        .usenet_origin
        .as_ref()
        .unwrap()
        .archive_limits
        .unwrap();
    drop(engine);
    c.usenet.downloads.as_mut().unwrap().zip = Some(Limits::default());
    let engine = Engine::open(c.clone()).unwrap();
    let workers = engine.start();
    let failed = wait_state(&engine, &id, "failed");
    assert_eq!(
        failed.usenet_origin.as_ref().unwrap().archive_limits,
        Some(captured)
    );
    assert!(failed.usenet_origin.as_ref().unwrap().archive.is_none());
    assert!(!namespace(&c).exists());
    assert!(failed.imports.is_empty());
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    drop(workers);
}

#[test]
fn disabling_captured_zip_capability_withholds_articles_after_restart() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(&p, &stored_zip(&format!("{TITLE}.mp4"), MEDIA), TITLE);
    let mut c = enabled(&d, &p, &h);
    let engine = Engine::open(c.clone()).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    engine.tick().unwrap();
    let original = retained(&engine, &id);
    drop(engine);
    c.usenet.downloads.as_mut().unwrap().zip = None;
    let engine = Engine::open(c.clone()).unwrap();
    let workers = engine.start();
    let failed = wait_state(&engine, &id, "failed");
    assert_eq!(failed.usenet_origin, original.usenet_origin);
    assert!(failed.imports.is_empty());
    assert!(p.requests.lock().unwrap().is_empty());
    assert!(!namespace(&c).exists());
    drop(workers);
}

#[test]
fn decoded_media_size_must_satisfy_the_bound_source_policy() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let bytes = stored_zip(&format!("{TITLE}.mp4"), MEDIA);
    articles(&p, &bytes, TITLE);
    h.replace(&rss(&item(TITLE, &bytes.len().to_string(), "")));
    let mut v = value(&p, &h);
    source(&mut v)
        .get_mut("usenet")
        .unwrap()
        .insert("maximum_bytes", 1_048_576_u32);
    source(&mut v)
        .get_mut("usenet")
        .unwrap()
        .insert("minimum_bytes", MEDIA.len() as u32 + 1);
    let downloads = v.get_mut("usenet").unwrap().get_mut("downloads").unwrap();
    downloads.insert("enabled", true);
    downloads.insert("zip", Value::object());
    let c = config::from_json(&v, &d.0).unwrap();
    let root = namespace(&c);
    let failed = fail(c, &p, &h);
    assert!(
        failed
            .last_error
            .as_ref()
            .unwrap()
            .contains("decoded ZIP media")
    );
    assert!(failed.usenet_origin.as_ref().unwrap().archive.is_none());
    assert!(!root.exists());
}

#[test]
fn absolute_source_numbering_keeps_the_canonical_episode_destination_for_zip() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let title = "Fixture.Series.007.1080p.WEB-DL.x264";
    h.replace(&rss(&item(title, "1024", "")));
    articles(
        &p,
        &stored_zip(&format!("Original/{title}.mp4"), MEDIA),
        title,
    );
    let mut request = movie();
    request.kind = "episode".into();
    request.title = "Fixture Series".into();
    request.season = 1;
    request.episode = 2;
    request.source_numbering = Some(SourceNumber::Absolute(7));
    let engine = Engine::open(enabled(&d, &p, &h)).unwrap();
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
fn corrupt_archive_preflight_runs_before_journal_tail_repair_and_permission_changes() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(&p, &stored_zip(&format!("{TITLE}.mp4"), MEDIA), TITLE);
    let c = enabled(&d, &p, &h);
    let engine = Engine::open(c.clone()).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    let workers = engine.start();
    let ready = wait_state(&engine, &id, "ready");
    drop(workers);
    lock(&engine.store).unwrap().compact().unwrap();
    drop(engine);
    let plan = &ready
        .usenet_origin
        .as_ref()
        .unwrap()
        .archive
        .as_ref()
        .unwrap()
        .plan;
    let path = namespace(&c).join(&plan.transfer_id).join("archive.bin");
    let mut bytes = fs::read(&path).unwrap();
    bytes[25] ^= 1;
    fs::write(path, bytes).unwrap();
    let journal = c.store_dir.join("journal.bin");
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
    assert_eq!(fs::metadata(&c.store_dir).unwrap().mode() & 0o777, 0o750);
    assert_eq!(fs::metadata(&journal).unwrap().mode() & 0o777, 0o640);
    assert!(Engine::open_for_preview(c).is_err());
    assert_eq!(files(&d.0), before);
    assert_eq!(p.requests.lock().unwrap().len(), 1);
}

#[test]
fn requester_approval_captured_route_and_exact_plex_confirmation_gate_zip_import() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let a = accounts::Accounts::open();
    articles(&p, &stored_zip(&format!("{TITLE}.mp4"), MEDIA), TITLE);
    let mut c = enabled(&d, &p, &h);
    c.requesters = a.config(&d.0).requesters;
    c.plex.enabled = true;
    c.plex.url = a.url.clone();
    c.plex.token_env = "PATH".into();
    c.plex.token_override = None;
    a.watchlist("alice", vec![accounts::movie(42, "Fixture Movie")]);
    a.response("/library/sections/1/refresh", 200, Value::object());
    let mut unrelated = accounts::movie(42, "Fixture Movie");
    let mut part = Value::object();
    part.insert("file", "/unrelated/Fixture Movie.mp4");
    let mut media = Value::object();
    media.insert("Part", Value::Array(vec![part]));
    unrelated.insert("Media", Value::Array(vec![media]));
    a.response(
        "/library/sections/1/all",
        200,
        accounts::container(vec![unrelated]),
    );
    let engine = Engine::open(c.clone()).unwrap();
    let mut policy = accounts::policy(&engine, "alice");
    policy.enabled = true;
    policy.destination = "family".into();
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
    let mut policy = accounts::policy(&engine, "alice");
    policy.destination = "default".into();
    accounts::apply(&engine, "alice", accounts::policy_query(policy));
    drop(engine);
    c.selection.profiles.get_mut("any").unwrap().blocked_terms = vec!["fixture".into()];
    let engine = Engine::open(c.clone()).unwrap();
    let workers = engine.start();
    let imported = wait_state(&engine, &admitted.id, "scanning");
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
    let ready = wait_state(&engine, &admitted.id, "ready");
    assert_eq!(fs::read(&ready.imports[0]).unwrap(), MEDIA);
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    assert_eq!(h.count(), 2);
    drop(workers);
}

#[test]
fn zip_configuration_rejects_unknown_or_unbounded_fields() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    for policy in [
        r#"{"command":"unzip"}"#,
        r#"{"max_entries":0}"#,
        r#"{"max_ratio":10001}"#,
        r#"{"max_blocks":65537}"#,
        r#"{"max_entry_bytes":"4294967295"}"#,
        r#"{"max_entry_bytes":"1024","max_total_bytes":"1023"}"#,
    ] {
        let mut v = value(&p, &h);
        v.get_mut("usenet")
            .unwrap()
            .get_mut("downloads")
            .unwrap()
            .insert("zip", json::parse(policy).unwrap());
        assert!(config::from_json(&v, &d.0).is_err(), "{policy}");
    }
    let mut v = value(&p, &h);
    v.get_mut("usenet")
        .unwrap()
        .get_mut("downloads")
        .unwrap()
        .insert("zip", Value::object());
    assert_eq!(
        config::from_json(&v, &d.0)
            .unwrap()
            .usenet
            .downloads
            .unwrap()
            .zip,
        Some(Limits::default())
    );
    assert!(p.requests.lock().unwrap().is_empty());
    assert_eq!(h.count(), 0);
}
