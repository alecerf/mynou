//! Original native stored RAR5 admission. Synthetic media and loopback services only.
use super::admission::{configured, progress, read_snapshot, retained, wait_state, write_snapshot};
use super::archives::fail;
use super::rar_support::{Fixture, archive, stored};
use super::requester_support as accounts;
use super::*;
use mynou::{archive::Limits, usenet::archive::Format};
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};
const MEDIA: &[u8] = include_bytes!("../../examples/demo.mp4");
fn digest(bytes: &[u8]) -> String {
    sha256(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
fn enabled(d: &Directory, p: &Provider, h: &Http) -> Config {
    let mut c = configured(d, p, h);
    c.usenet.downloads.as_mut().unwrap().rar = Some(Limits::default());
    c
}
fn articles(p: &Provider, bytes: &[u8], title: &str) {
    p.set(
        "file0-part1@fixture.test",
        usenet_support::Response::Body(usenet_support::article_named(
            bytes,
            1,
            1,
            &format!("{title}.rar"),
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

#[test]
fn stored_rar_movie_has_explicit_format_proofs_private_storage_and_copied_imports() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let name = format!("Original/{TITLE}.mp4");
    let bytes = archive(&[
        Fixture::stored("Original/original.nfo", b"Original synthetic RAR"),
        Fixture::stored(&name, MEDIA),
    ]);
    articles(&p, &bytes, TITLE);
    let c = enabled(&d, &p, &h);
    let engine = Engine::open(c.clone()).unwrap();
    let id = engine.submit(movie()).unwrap().remove(0).id;
    let workers = engine.start();
    let ready = wait_state(&engine, &id, "ready");
    let origin = ready.usenet_origin.as_ref().unwrap();
    let prepared = origin.archive.as_ref().unwrap();
    let plan = &prepared.plan;
    let proof = prepared.output.as_ref().unwrap();
    assert_eq!(plan.format, Format::Rar5);
    assert!(origin.archive_limits.is_none());
    assert_eq!(origin.rar_limits, Some(Limits::default()));
    assert_eq!(plan.owner, origin.owner(&ready));
    assert_eq!(Some(&plan.transfer_id), origin.transfer_id.as_ref());
    assert_eq!(plan.entry_name, name);
    assert_eq!(plan.entry_index, 1);
    assert_eq!(plan.source_sha256, digest(&bytes));
    assert_eq!(plan.entry_method, "stored");
    assert_eq!(proof.sha256, digest(MEDIA));
    assert_eq!(proof.bytes, MEDIA.len() as u64);
    let frame = namespace(&c).join(&plan.transfer_id).join("archive.bin");
    assert_eq!(&fs::read(&frame).unwrap()[..8], b"MYNOUA02");
    assert_eq!(fs::read(&ready.files[0]).unwrap(), MEDIA);
    assert_eq!(fs::read(&ready.imports[0]).unwrap(), MEDIA);
    assert!(ready.download_id.is_none());
    let source = fs::metadata(&ready.files[0]).unwrap();
    let imported = fs::metadata(&ready.imports[0]).unwrap();
    assert_eq!(source.nlink(), 1);
    assert_eq!(source.mode() & 0o077, 0);
    assert_ne!(
        (source.dev(), source.ino()),
        (imported.dev(), imported.ino())
    );
    assert_eq!(
        fs::read_dir(Path::new(&ready.files[0]).parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    let public = mynou::engine::public_job(&ready);
    let projection = public.get("usenet_origin").unwrap();
    assert_eq!(projection.get("rar_enabled"), Some(&Value::Bool(true)));
    assert_eq!(projection.get("archive_verified"), Some(&Value::Bool(true)));
    let text = json::stringify(projection);
    for private in [
        "binding",
        "entry_name",
        "source_sha256",
        "rar_limits",
        PRIVATE_KEY,
    ] {
        assert!(!text.contains(private), "{private}");
    }
    assert_eq!(
        progress(&engine).get("owner_authorized"),
        Some(&Value::Bool(false))
    );
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    assert_eq!(h.count(), 2);
    drop(workers);
    lock(&engine.store).unwrap().compact().unwrap();
    drop(engine);
    let snapshot = c.store_dir.join("snapshot.bin");
    assert_eq!(&fs::read(&snapshot).unwrap()[..8], b"MYNOUS08");
    let before = files(&d.0);
    let reopened = Engine::open_for_preview(c.clone()).unwrap();
    assert_eq!(retained(&reopened, &id).usenet_origin, ready.usenet_origin);
    drop(reopened);
    assert_eq!(files(&d.0), before);
    let data = read_snapshot(&snapshot);
    write_snapshot(&snapshot, &data, b"MYNOUS07");
    let before = files(&d.0);
    assert!(Engine::open(c).is_err());
    assert_eq!(files(&d.0), before);
}

#[test]
fn zip_capability_cannot_authorize_a_rar_source_or_adopt_disguised_zip_bytes() {
    for (zip_only, bytes) in [
        (true, stored(&format!("{TITLE}.mp4"), MEDIA)),
        (
            false,
            super::archive_support::stored_zip(&format!("{TITLE}.mp4"), MEDIA),
        ),
    ] {
        let d = Directory::new();
        let p = Provider::open();
        let h = Http::open();
        articles(&p, &bytes, TITLE);
        let mut c = enabled(&d, &p, &h);
        if zip_only {
            let downloads = c.usenet.downloads.as_mut().unwrap();
            downloads.rar = None;
            downloads.zip = Some(Limits::default());
        }
        let root = namespace(&c);
        let failed = fail(c, &p, &h);
        assert!(failed.usenet_origin.as_ref().unwrap().archive.is_none());
        assert!(!root.exists());
    }
}

#[test]
fn compressed_rar_is_withheld_without_poisoning_valid_article_proofs() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let name = format!("{TITLE}.mp4");
    let mut entry = Fixture::stored(&name, MEDIA);
    entry.compression = 0x80;
    articles(&p, &archive(&[entry]), TITLE);
    let c = enabled(&d, &p, &h);
    let root = namespace(&c);
    let failed = fail(c, &p, &h);
    assert!(failed.last_error.as_ref().unwrap().contains("compressed"));
    assert!(failed.usenet_origin.as_ref().unwrap().archive.is_none());
    assert!(!root.exists());
}

#[test]
fn rar_multiple_media_and_mismatching_inner_identity_fail_before_extraction_intent() {
    let name = format!("{TITLE}.mp4");
    for bytes in [
        archive(&[
            Fixture::stored(&name, MEDIA),
            Fixture::stored("sample.mp4", MEDIA),
        ]),
        stored("Different.Movie.2024.1080p.mp4", MEDIA),
    ] {
        let d = Directory::new();
        let p = Provider::open();
        let h = Http::open();
        articles(&p, &bytes, TITLE);
        let c = enabled(&d, &p, &h);
        let root = namespace(&c);
        let failed = fail(c, &p, &h);
        assert!(failed.usenet_origin.as_ref().unwrap().archive.is_none());
        assert!(!root.exists());
    }
}

#[test]
fn corrupt_rar_payload_keeps_the_known_writing_intent_private_and_reopenable() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let mut bytes = stored(&format!("{TITLE}.mp4"), MEDIA);
    let data = bytes.len() - 8 - MEDIA.len();
    bytes[data + 100] ^= 1;
    articles(&p, &bytes, TITLE);
    let c = enabled(&d, &p, &h);
    let failed = fail(c.clone(), &p, &h);
    let archive = failed
        .usenet_origin
        .as_ref()
        .unwrap()
        .archive
        .as_ref()
        .unwrap();
    assert!(archive.output.is_none());
    assert!(failed.files.is_empty());
    let path = namespace(&c)
        .join(&archive.plan.transfer_id)
        .join("archive.bin");
    assert_eq!(&fs::read(&path).unwrap()[..8], b"MYNOUA02");
    let before = files(&d.0);
    let engine = Engine::open_for_preview(c).unwrap();
    assert_eq!(
        retained(&engine, &failed.id).usenet_origin,
        failed.usenet_origin
    );
    drop(engine);
    assert_eq!(files(&d.0), before);
}

#[test]
fn rar_format_integrity_cannot_admit_an_invalid_media_container() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(
        &p,
        &stored(&format!("{TITLE}.mp4"), b"Original invalid RAR media"),
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
fn rar_bounds_remain_captured_and_disabling_capability_withholds_articles_on_restart() {
    for disabled in [false, true] {
        let d = Directory::new();
        let p = Provider::open();
        let h = Http::open();
        articles(&p, &stored(&format!("{TITLE}.mp4"), MEDIA), TITLE);
        let mut c = enabled(&d, &p, &h);
        if !disabled {
            c.usenet
                .downloads
                .as_mut()
                .unwrap()
                .rar
                .as_mut()
                .unwrap()
                .max_entry_bytes = MEDIA.len() as u64 - 1;
        }
        let engine = Engine::open(c.clone()).unwrap();
        let id = engine.submit(movie()).unwrap().remove(0).id;
        engine.tick().unwrap();
        let origin = retained(&engine, &id).usenet_origin;
        drop(engine);
        c.usenet.downloads.as_mut().unwrap().rar = if disabled {
            None
        } else {
            Some(Limits::default())
        };
        let engine = Engine::open(c.clone()).unwrap();
        let workers = engine.start();
        let failed = wait_state(&engine, &id, "failed");
        assert_eq!(failed.usenet_origin, origin);
        assert!(failed.imports.is_empty());
        assert!(!namespace(&c).exists());
        assert_eq!(p.requests.lock().unwrap().len(), usize::from(!disabled));
        drop(workers);
    }
}

#[test]
fn absolute_numbered_rar_keeps_the_canonical_episode_and_filename() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let title = "Fixture.Series.007.1080p.WEB-DL.x264";
    h.replace(&rss(&item(title, "1024", "")));
    articles(&p, &stored(&format!("Original/{title}.mp4"), MEDIA), title);
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
fn rar_descriptor_preflight_precedes_main_journal_tail_and_permission_repair() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    articles(&p, &stored(&format!("{TITLE}.mp4"), MEDIA), TITLE);
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
    assert_eq!(fs::metadata(journal).unwrap().mode() & 0o777, 0o640);
    assert!(Engine::open_for_preview(c).is_err());
    assert_eq!(files(&d.0), before);
}

#[test]
fn requester_approval_captured_route_and_exact_plex_path_gate_rar_import() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let a = accounts::Accounts::open();
    articles(&p, &stored(&format!("{TITLE}.mp4"), MEDIA), TITLE);
    let mut c = enabled(&d, &p, &h);
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
fn rar_configuration_uses_only_bounded_native_limits() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    for policy in [
        r#"{"command":"unrar"}"#,
        r#"{"max_entries":0}"#,
        r#"{"max_ratio":10001}"#,
        r#"{"max_total_bytes":"0"}"#,
    ] {
        let mut v = value(&p, &h);
        v.get_mut("usenet")
            .unwrap()
            .get_mut("downloads")
            .unwrap()
            .insert("rar", json::parse(policy).unwrap());
        assert!(config::from_json(&v, &d.0).is_err());
    }
    let mut v = value(&p, &h);
    v.get_mut("usenet")
        .unwrap()
        .get_mut("downloads")
        .unwrap()
        .insert("rar", Value::object());
    assert_eq!(
        config::from_json(&v, &d.0)
            .unwrap()
            .usenet
            .downloads
            .unwrap()
            .rar,
        Some(Limits::default())
    );
    assert_eq!(h.count(), 0);
    assert!(p.requests.lock().unwrap().is_empty());
}
