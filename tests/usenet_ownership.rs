//! Original held-owner/permission/recovery fixtures. No canonical admission or public content.
mod library_support;
mod usenet_support;
mod web_support;

use library_support::{Directory, files};
use mynou::{
    config,
    crypto::sha256,
    engine::Engine,
    json::{self, Value},
    store,
    usenet::queue::{Client, OwnedTransfer, Owner},
};
use std::{fs, path::Path, sync::atomic::Ordering, thread};
use usenet_support::*;
use web_support::TOKEN;

fn owner(id: &str) -> Owner {
    Owner {
        job_id: id.into(),
        binding: "a".repeat(64),
    }
}
fn stage(
    c: &Client,
    cfg: &mynou::usenet::Settings,
    source: &[u8],
    index: usize,
    owner: &Owner,
) -> OwnedTransfer {
    c.stage_owned(source, "original", &cfg.servers[0].binding(), index, owner)
        .unwrap()
}
fn authorize(c: &Client, r: &OwnedTransfer) {
    c.authorize_owned(&r.id, &r.owner, store::now() + 60)
        .unwrap();
}
fn data(path: &Path) -> Value {
    let b = fs::read(path).unwrap();
    json::parse(std::str::from_utf8(&b[16..b.len() - 32]).unwrap()).unwrap()
}
fn replace(path: &Path, magic: &[u8; 8], v: &Value) {
    let p = json::stringify(v).into_bytes();
    let mut b = magic.to_vec();
    b.extend_from_slice(&(p.len() as u64).to_le_bytes());
    b.extend(p);
    b.extend_from_slice(&sha256(&b));
    fs::write(path, b).unwrap();
}
fn row(v: &mut Value) -> &mut Value {
    let Value::Array(rows) = v.get_mut("records").unwrap() else {
        panic!()
    };
    &mut rows[0]
}
fn private(v: &Value) {
    let text = json::stringify(v);
    for secret in [
        "@fixture.test",
        "127.0.0.1",
        "Private original",
        "alt.binaries",
        "original queue fixture.bin",
        "state_dir",
        "username_env",
        "password_env",
        TOKEN,
        "binding",
    ] {
        assert!(!text.contains(secret), "Owner report exposed private data");
    }
}

#[test]
fn exact_source_preparation_stays_held_and_idempotent_without_article_io() {
    let d = Directory::new();
    let p = Provider::open();
    let cfg = settings(&d.0, &p);
    let src = source(1, 2);
    let c = Client::open(&cfg, false).unwrap();
    let owned = stage(&c, &cfg, &src, 0, &owner("original-job"));
    assert_eq!(owned.state, "held");
    assert_eq!(owned.attempts, 0);
    assert_eq!(owned.verified_parts, 0);
    let before = files(&d.0);
    assert_eq!(stage(&c, &cfg, &src, 0, &owned.owner), owned);
    assert_eq!(files(&d.0), before);
    for _ in 0..5 {
        assert!(!c.tick().unwrap());
    }
    assert!(p.requests.lock().unwrap().is_empty());
    let snapshot = cfg.downloads.as_ref().unwrap().state_dir.join("queue.bin");
    assert_eq!(&fs::read(&snapshot).unwrap()[..8], b"MYNOUU02");
    assert!(!json::stringify(&data(&snapshot)).contains("permit"));
    let report = record(&c, &owned.id);
    assert_eq!(report.get("library_owned"), Some(&Value::Bool(true)));
    assert_eq!(report.get("owner_authorized"), Some(&Value::Bool(false)));
    private(&c.report().unwrap());
    assert!(c.verified_file(&owned.id).is_err());
}

#[test]
fn checked_owner_and_explicit_bounded_permission_precede_verified_output() {
    let d = Directory::new();
    let p = Provider::open();
    let payload = b"Original owned synthetic bytes";
    p.populate(1, 2, payload);
    let cfg = settings(&d.0, &p);
    let c = Client::open(&cfg, false).unwrap();
    let owned = stage(&c, &cfg, &source(1, 2), 0, &owner("original-job"));
    let before = files(&d.0);
    for wrong in [
        owner("different-job"),
        Owner {
            binding: "b".repeat(64),
            ..owned.owner.clone()
        },
    ] {
        assert!(
            c.authorize_owned(&owned.id, &wrong, store::now() + 60)
                .is_err()
        );
        assert!(c.hold_owned(&owned.id, &wrong).is_err());
        assert!(c.verified_owned_file(&owned.id, &wrong).is_err());
    }
    for until in [0, store::now(), store::now() + 3600, u64::MAX] {
        assert!(c.authorize_owned(&owned.id, &owned.owner, until).is_err());
    }
    assert_eq!(files(&d.0), before);
    authorize(&c, &owned);
    for _ in 0..3 {
        assert!(c.tick().unwrap());
    }
    assert_eq!(
        fs::read(c.verified_owned_file(&owned.id, &owned.owner).unwrap()).unwrap(),
        payload
    );
    assert!(c.verified_file(&owned.id).is_err());
    assert_eq!(p.requests.lock().unwrap().len(), 2);
    assert_eq!(record(&c, &owned.id).get("ready"), Some(&Value::Bool(true)));
    c.hold_owned(&owned.id, &owned.owner).unwrap();
    assert!(c.verified_owned_file(&owned.id, &owned.owner).is_err());
    assert_eq!(
        record(&c, &owned.id).get("state").and_then(Value::as_str),
        Some("complete")
    );
    authorize(&c, &owned);
    assert_eq!(
        fs::read(c.verified_owned_file(&owned.id, &owned.owner).unwrap()).unwrap(),
        payload
    );
    assert_eq!(p.requests.lock().unwrap().len(), 2);
}

#[test]
fn permission_disappears_on_restart_and_receipts_are_reused_only_after_reauthorization() {
    let d = Directory::new();
    let p = Provider::open();
    p.populate(1, 2, b"Original restart payload");
    let cfg = settings(&d.0, &p);
    let c = Client::open(&cfg, false).unwrap();
    let owned = stage(&c, &cfg, &source(1, 2), 0, &owner("original-job"));
    authorize(&c, &owned);
    assert!(c.tick().unwrap());
    drop(c);
    let c = Client::open(&cfg, false).unwrap();
    for _ in 0..5 {
        assert!(!c.tick().unwrap());
    }
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    assert_eq!(c.retained_owned().unwrap()[0].verified_parts, 1);
    assert_eq!(
        record(&c, &owned.id).get("owner_authorized"),
        Some(&Value::Bool(false))
    );
    authorize(&c, &owned);
    assert!(c.tick().unwrap());
    assert!(c.tick().unwrap());
    assert_eq!(
        fs::read(c.verified_owned_file(&owned.id, &owned.owner).unwrap()).unwrap(),
        b"Original restart payload"
    );
    assert_eq!(
        *p.requests.lock().unwrap(),
        vec!["file0-part1@fixture.test", "file0-part2@fixture.test"]
    );
    drop(c);
    let before = files(&d.0);
    let read_only = Client::open(&cfg, true).unwrap();
    assert_eq!(read_only.retained_owned().unwrap()[0].state, "complete");
    assert!(
        read_only
            .verified_owned_file(&owned.id, &owned.owner)
            .is_err()
    );
    assert!(
        read_only
            .authorize_owned(&owned.id, &owned.owner, store::now() + 60)
            .is_err()
    );
    assert_eq!(files(&d.0), before);
}

#[test]
fn held_preparation_recovers_a_committed_constructor_intent_without_authorizing_it() {
    let d = Directory::new();
    let p = Provider::open();
    let cfg = settings(&d.0, &p);
    let src = source(1, 1);
    let c = Client::open(&cfg, false).unwrap();
    let owned = stage(&c, &cfg, &src, 0, &owner("original-job"));
    drop(c);
    let root = &cfg.downloads.as_ref().unwrap().state_dir;
    let path = root.join("queue.bin");
    let mut v = data(&path);
    row(&mut v).insert("state", "preparing");
    replace(&path, b"MYNOUU02", &v);
    fs::remove_dir_all(root.join("files").join(&owned.id)).unwrap();
    let before = files(&d.0);
    let read_only = Client::open(&cfg, true).unwrap();
    assert_eq!(read_only.retained_owned().unwrap()[0].state, "preparing");
    drop(read_only);
    assert_eq!(files(&d.0), before);
    let c = Client::open(&cfg, false).unwrap();
    assert_eq!(c.retained_owned().unwrap()[0].state, "held");
    assert!(!c.tick().unwrap());
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn expired_permission_fences_late_articles_and_keeps_spent_attempts() {
    let d = Directory::new();
    let p = Provider::open();
    p.populate(1, 1, b"Original late owner result");
    p.gate.store(true, Ordering::Release);
    let mut cfg = settings(&d.0, &p);
    let mut server = Value::object();
    server.insert("id", "original");
    server.insert("host", "127.0.0.1");
    server.insert("port", u32::from(p.port));
    server.insert("tls", false);
    server.insert("timeout_ms", 4000_u32);
    cfg.servers[0] = mynou::usenet::Server::from_json(&server).unwrap();
    let c = Client::open(&cfg, false).unwrap();
    let owned = stage(&c, &cfg, &source(1, 1), 0, &owner("original-job"));
    let until = store::now() + 2;
    c.authorize_owned(&owned.id, &owned.owner, until).unwrap();
    let cloned = c.clone();
    let worker = thread::spawn(move || cloned.tick());
    p.wait_requests(1);
    wait(|| store::now() >= until);
    let _ = c.tick().unwrap();
    p.gate.store(false, Ordering::Release);
    assert!(worker.join().unwrap().unwrap());
    let r = record(&c, &owned.id);
    assert_eq!(r.get("state").and_then(Value::as_str), Some("paused"));
    assert_eq!(r.get("verified_parts").and_then(Value::as_u64), Some(0));
    assert_eq!(r.get("attempts").and_then(Value::as_u64), Some(1));
    assert!(!c.tick().unwrap());
    authorize(&c, &owned);
    assert!(c.tick().unwrap());
    assert!(c.tick().unwrap());
    assert_eq!(
        record(&c, &owned.id)
            .get("attempts")
            .and_then(Value::as_u64),
        Some(2)
    );
    assert_eq!(p.requests.lock().unwrap().len(), 2);
}

#[test]
fn owner_revoke_and_immediate_reauthorization_retain_the_live_slot_and_fence_old_results() {
    let d = Directory::new();
    let p = Provider::open();
    p.populate(1, 1, b"Original revocation payload");
    p.gate.store(true, Ordering::Release);
    let mut cfg = settings(&d.0, &p);
    cfg.downloads.as_mut().unwrap().max_active = 1;
    let c = Client::open(&cfg, false).unwrap();
    let owned = stage(&c, &cfg, &source(1, 1), 0, &owner("original-job"));
    authorize(&c, &owned);
    let cloned = c.clone();
    let worker = thread::spawn(move || cloned.tick());
    p.wait_requests(1);
    c.hold_owned(&owned.id, &owned.owner).unwrap();
    authorize(&c, &owned);
    assert!(!c.tick().unwrap());
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    assert_eq!(
        c.report().unwrap().get("active").and_then(Value::as_u64),
        Some(1)
    );
    p.gate.store(false, Ordering::Release);
    assert!(worker.join().unwrap().unwrap());
    assert_eq!(
        record(&c, &owned.id)
            .get("verified_parts")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert!(c.tick().unwrap());
    assert!(c.tick().unwrap());
    assert_eq!(p.requests.lock().unwrap().len(), 2);
    assert_eq!(
        fs::read(c.verified_owned_file(&owned.id, &owned.owner).unwrap()).unwrap(),
        b"Original revocation payload"
    );
}

#[test]
fn renewing_live_owner_permission_preserves_the_existing_reservation() {
    let d = Directory::new();
    let p = Provider::open();
    p.populate(1, 1, b"Original renewed owner");
    p.gate.store(true, Ordering::Release);
    let cfg = settings(&d.0, &p);
    let c = Client::open(&cfg, false).unwrap();
    let owned = stage(&c, &cfg, &source(1, 1), 0, &owner("original-job"));
    authorize(&c, &owned);
    let cloned = c.clone();
    let worker = thread::spawn(move || cloned.tick());
    p.wait_requests(1);
    let snapshot = cfg.downloads.as_ref().unwrap().state_dir.join("queue.bin");
    let before = fs::read(&snapshot).unwrap();
    authorize(&c, &owned);
    assert_eq!(fs::read(&snapshot).unwrap(), before);
    p.gate.store(false, Ordering::Release);
    assert!(worker.join().unwrap().unwrap());
    assert!(c.tick().unwrap());
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    assert_eq!(
        fs::read(c.verified_owned_file(&owned.id, &owned.owner).unwrap()).unwrap(),
        b"Original renewed owner"
    );
}

#[test]
fn raw_controls_and_rebinding_cannot_take_over_an_owned_source_or_job() {
    let d = Directory::new();
    let p = Provider::open();
    let cfg = settings(&d.0, &p);
    let c = Client::open(&cfg, false).unwrap();
    let src = source(2, 1);
    let owned = stage(&c, &cfg, &src, 0, &owner("original-job"));
    let before = files(&d.0);
    for action in ["pause", "resume", "cancel", "retry"] {
        assert!(c.control(&owned.id, action, &preview()).is_err());
    }
    assert!(c.enqueue(&src, "original", 0, &preview()).is_err());
    assert!(
        c.stage_owned(
            &src,
            "original",
            &cfg.servers[0].binding(),
            0,
            &owner("other-job")
        )
        .is_err()
    );
    assert!(
        c.stage_owned(&src, "original", &cfg.servers[0].binding(), 1, &owned.owner)
            .is_err()
    );
    assert_eq!(files(&d.0), before);
    assert_eq!(c.retained_owned().unwrap().len(), 1);
    let _ = enqueue(&c, &src, 1);
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn existing_raw_sources_are_never_implicitly_adopted_by_library_ownership() {
    let d = Directory::new();
    let p = Provider::open();
    let cfg = settings(&d.0, &p);
    let c = Client::open(&cfg, false).unwrap();
    let src = source(1, 1);
    let raw = enqueue(&c, &src, 0);
    let before = files(&d.0);
    assert!(
        c.stage_owned(
            &src,
            "original",
            &cfg.servers[0].binding(),
            0,
            &owner("original-job")
        )
        .is_err()
    );
    assert_eq!(files(&d.0), before);
    assert!(c.retained_owned().unwrap().is_empty());
    assert_eq!(
        record(&c, &raw).get("library_owned"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        &fs::read(cfg.downloads.as_ref().unwrap().state_dir.join("queue.bin")).unwrap()[..8],
        b"MYNOUU01"
    );
    drop(c);
    let c = Client::open(&cfg, false).unwrap();
    assert_eq!(
        record(&c, &raw).get("state").and_then(Value::as_str),
        Some("queued")
    );
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn owned_retry_never_resets_exhausted_attempts_after_configuration_or_restart() {
    let d = Directory::new();
    let p = Provider::open();
    let mut cfg = settings(&d.0, &p);
    cfg.downloads.as_mut().unwrap().max_attempts = 1;
    let c = Client::open(&cfg, false).unwrap();
    let owned = stage(&c, &cfg, &source(1, 1), 0, &owner("original-job"));
    authorize(&c, &owned);
    assert!(c.tick().unwrap());
    assert_eq!(
        record(&c, &owned.id).get("state").and_then(Value::as_str),
        Some("failed")
    );
    assert!(
        c.retry_owned(&owned.id, &owned.owner, store::now() + 60)
            .is_err()
    );
    drop(c);
    cfg.downloads.as_mut().unwrap().max_attempts = 10;
    let c = Client::open(&cfg, false).unwrap();
    assert!(
        c.retry_owned(&owned.id, &owned.owner, store::now() + 60)
            .is_err()
    );
    assert!(
        c.authorize_owned(&owned.id, &owned.owner, store::now() + 60)
            .is_err()
    );
    assert!(!c.tick().unwrap());
    assert_eq!(c.retained_owned().unwrap()[0].max_attempts, 1);
    assert_eq!(p.requests.lock().unwrap().len(), 1);
}

#[test]
fn owner_format_downgrade_and_binding_corruption_fail_before_storage_repair() {
    let d = Directory::new();
    let p = Provider::open();
    let cfg = settings(&d.0, &p);
    let c = Client::open(&cfg, false).unwrap();
    let _ = stage(&c, &cfg, &source(1, 1), 0, &owner("original-job"));
    drop(c);
    let path = cfg.downloads.as_ref().unwrap().state_dir.join("queue.bin");
    let original = data(&path);
    replace(&path, b"MYNOUU01", &original);
    let before = files(&d.0);
    assert!(Client::open(&cfg, false).is_err());
    assert_eq!(files(&d.0), before);
    let mut altered = original.clone();
    row(&mut altered)
        .get_mut("owner")
        .unwrap()
        .insert("binding", "b".repeat(64));
    replace(&path, b"MYNOUU02", &altered);
    let before = files(&d.0);
    assert!(Client::open(&cfg, false).is_err());
    assert_eq!(files(&d.0), before);
    replace(&path, b"MYNOUU02", &original);
    let c = Client::open(&cfg, false).unwrap();
    assert_eq!(c.retained_owned().unwrap()[0].state, "held");
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn another_valid_owner_identity_cannot_adopt_a_renamed_checked_workspace() {
    let d = Directory::new();
    let other = Directory::new();
    let p = Provider::open();
    let cfg = settings(&d.0, &p);
    let src = source(1, 1);
    let c = Client::open(&cfg, false).unwrap();
    let original = stage(&c, &cfg, &src, 0, &owner("original-job"));
    drop(c);
    let other_cfg = settings(&other.0, &p);
    let c = Client::open(&other_cfg, false).unwrap();
    let different = Owner {
        binding: "b".repeat(64),
        ..original.owner.clone()
    };
    let replacement = stage(&c, &other_cfg, &src, 0, &different);
    drop(c);
    let root = &cfg.downloads.as_ref().unwrap().state_dir;
    let path = root.join("queue.bin");
    let mut v = data(&path);
    row(&mut v).insert("id", replacement.id.clone());
    row(&mut v).insert("owner", replacement.owner.to_json());
    replace(&path, b"MYNOUU02", &v);
    fs::rename(
        root.join("files").join(&original.id),
        root.join("files").join(&replacement.id),
    )
    .unwrap();
    let before = files(&d.0);
    assert!(Client::open(&cfg, false).is_err());
    assert_eq!(files(&d.0), before);
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn verified_owned_output_corruption_withholds_readiness_and_requires_recovery() {
    let d = Directory::new();
    let p = Provider::open();
    p.populate(1, 1, b"Original checked owned output");
    let cfg = settings(&d.0, &p);
    let c = Client::open(&cfg, false).unwrap();
    let owned = stage(&c, &cfg, &source(1, 1), 0, &owner("original-job"));
    authorize(&c, &owned);
    assert!(c.tick().unwrap());
    assert!(c.tick().unwrap());
    let path = c.verified_owned_file(&owned.id, &owned.owner).unwrap();
    fs::write(path, b"Changed private bytes").unwrap();
    assert!(c.verified_owned_file(&owned.id, &owned.owner).is_err());
    assert_eq!(
        record(&c, &owned.id).get("ready"),
        Some(&Value::Bool(false))
    );
    assert!(c.retained_owned().is_err());
    drop(c);
    assert!(Client::open(&cfg, true).is_err());
    assert_eq!(p.requests.lock().unwrap().len(), 1);
}

#[test]
fn provider_changes_read_only_clients_and_stop_cannot_grant_owner_permissions() {
    let d = Directory::new();
    let p = Provider::open();
    let cfg = settings(&d.0, &p);
    let c = Client::open(&cfg, true).unwrap();
    let before = files(&d.0);
    assert!(
        c.stage_owned(
            &source(1, 1),
            "original",
            &cfg.servers[0].binding(),
            0,
            &owner("original-job")
        )
        .is_err()
    );
    assert_eq!(files(&d.0), before);
    drop(c);
    let c = Client::open(&cfg, false).unwrap();
    let before = files(&d.0);
    assert!(
        c.stage_owned(
            &source(1, 1),
            "original",
            &"b".repeat(64),
            0,
            &owner("original-job")
        )
        .is_err()
    );
    assert_eq!(files(&d.0), before);
    let owned = stage(&c, &cfg, &source(1, 1), 0, &owner("original-job"));
    c.stop();
    assert!(
        c.authorize_owned(&owned.id, &owned.owner, store::now() + 60)
            .is_err()
    );
    assert!(!c.tick().unwrap());
    drop(c);
    let mut changed = cfg.clone();
    changed.servers[0] = mynou::usenet::Server::from_json(
        &json::parse(r#"{"id":"original","host":"127.0.0.1","port":119,"tls":false}"#).unwrap(),
    )
    .unwrap();
    let before = files(&d.0);
    assert!(Client::open(&changed, false).is_err());
    assert_eq!(files(&d.0), before);
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn a_service_rejects_an_unadmitted_owned_preparation_before_views_can_resume_it() {
    let d = Directory::new();
    let p = Provider::open();
    let mut v = config::default_json();
    v.insert("listen", "127.0.0.1:0");
    v.get_mut("downloads").unwrap().insert("enabled", false);
    v.insert("usenet", usenet(&p));
    let cfg = config::from_json(&v, &d.0).unwrap();
    let c = Client::open(&cfg.usenet, false).unwrap();
    stage(&c, &cfg.usenet, &source(1, 1), 0, &owner("original-job"));
    drop(c);
    let before = files(&d.0);
    assert!(Engine::open(cfg).is_err());
    assert_eq!(files(&d.0), before);
    assert!(p.requests.lock().unwrap().is_empty());
}
