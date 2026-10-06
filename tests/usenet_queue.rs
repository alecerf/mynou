//! Checked durable queue behavior using original loopback articles and private staging.
mod library_support;
mod usenet_support;
mod web_support;
use library_support::{Directory, files};
use mynou::{
    config,
    crypto::sha256,
    engine::Engine,
    json::{self, Value},
    usenet::{ProbeRequest, QueueControl, queue::Client, workspace::Workspace},
};
use std::{fs, path::Path, process::Command, sync::atomic::Ordering, thread};
use usenet_support::*;
use web_support::{Browser, Server, TOKEN};
fn data(path: &Path) -> Value {
    let b = fs::read(path).unwrap();
    assert_eq!(&b[..8], b"MYNOUU01");
    json::parse(std::str::from_utf8(&b[16..b.len() - 32]).unwrap()).unwrap()
}
fn replace(path: &Path, v: &Value) {
    let p = json::stringify(v).into_bytes();
    let mut b = b"MYNOUU01".to_vec();
    b.extend_from_slice(&(p.len() as u64).to_le_bytes());
    b.extend(p);
    b.extend_from_slice(&sha256(&b));
    fs::write(path, b).unwrap();
}
fn snapshot_record<'a>(v: &'a mut Value, id: &str) -> &'a mut Value {
    let Value::Array(rows) = v.get_mut("records").unwrap() else {
        panic!("records")
    };
    rows.iter_mut()
        .find(|r| r.get("id").and_then(Value::as_str) == Some(id))
        .unwrap()
}
fn private(v: &Value) {
    let s = json::stringify(v);
    for forbidden in [
        "127.0.0.1",
        "@fixture.test",
        "Private original",
        "alt.binaries",
        "original queue fixture.bin",
        "state_dir",
        "username_env",
        "password_env",
        TOKEN,
    ] {
        assert!(
            !s.contains(forbidden),
            "Public queue report leaked {forbidden}"
        );
    }
}
#[test]
fn previews_are_non_mutating_and_never_acquire_or_admit_library_jobs() {
    let d = Directory::new();
    let p = Provider::open();
    let cfg = settings(&d.0, &p);
    let src = source(1, 2);
    let c = Client::open(&cfg, true).unwrap();
    let before = files(&d.0);
    let v = c.enqueue(&src, "original", 0, &preview()).unwrap();
    private(&v);
    assert!(c.enqueue(&src, "original", 0, &apply(&v)).is_err());
    assert_eq!(files(&d.0), before);
    assert!(p.requests.lock().unwrap().is_empty());
    drop(c);
    let c = Client::open(&cfg, false).unwrap();
    let before = files(&d.0);
    let _ = c.enqueue(&src, "original", 0, &preview()).unwrap();
    assert_eq!(files(&d.0), before);
    let id = enqueue(&c, &src, 0);
    assert_eq!(record(&c, &id).get("ready").unwrap().as_bool(), Some(false));
    assert!(c.verified_file(&id).is_err());
    private(&c.report().unwrap());
    assert!(p.requests.lock().unwrap().is_empty());
}
#[test]
fn verified_receipts_survive_restart_without_repeating_completed_articles() {
    let d = Directory::new();
    let p = Provider::open();
    let bytes = (0..=255).collect::<Vec<u8>>();
    p.populate(1, 2, &bytes);
    let cfg = settings(&d.0, &p);
    let src = source(1, 2);
    let c = Client::open(&cfg, false).unwrap();
    let id = enqueue(&c, &src, 0);
    assert!(c.tick().unwrap());
    assert_eq!(
        record(&c, &id).get("verified_parts").unwrap().as_u64(),
        Some(1)
    );
    drop(c);
    let c = Client::open(&cfg, false).unwrap();
    assert!(c.tick().unwrap());
    assert!(c.tick().unwrap());
    let out = c.verified_file(&id).unwrap();
    assert_eq!(fs::read(&out).unwrap(), bytes);
    let r = record(&c, &id);
    assert_eq!(r.get("state").unwrap().as_str(), Some("complete"));
    assert_eq!(r.get("attempts").unwrap().as_u64(), Some(2));
    assert_eq!(
        *p.requests.lock().unwrap(),
        vec!["file0-part1@fixture.test", "file0-part2@fixture.test"]
    );
    private(&c.report().unwrap());
    drop(c);
    let before = files(&d.0);
    let c = Client::open(&cfg, true).unwrap();
    assert_eq!(fs::read(c.verified_file(&id).unwrap()).unwrap(), bytes);
    assert_eq!(files(&d.0), before);
}
#[test]
fn exhausted_attempts_remain_exhausted_after_restart_and_configuration_change() {
    let d = Directory::new();
    let p = Provider::open();
    let mut cfg = settings(&d.0, &p);
    cfg.downloads.as_mut().unwrap().max_attempts = 1;
    let src = source(1, 1);
    let c = Client::open(&cfg, false).unwrap();
    let id = enqueue(&c, &src, 0);
    assert!(c.tick().unwrap());
    assert_eq!(
        record(&c, &id).get("state").unwrap().as_str(),
        Some("failed")
    );
    drop(c);
    cfg.downloads.as_mut().unwrap().max_attempts = 10;
    let c = Client::open(&cfg, false).unwrap();
    assert!(!c.tick().unwrap());
    assert!(c.control(&id, "retry", &preview()).is_err());
    control(&c, &id, "cancel");
    assert!(c.control(&id, "resume", &preview()).is_err());
    assert!(c.control(&id, "retry", &preview()).is_err());
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    assert_eq!(
        record(&c, &id).get("attempt_limit").unwrap().as_u64(),
        Some(1)
    );
}
#[test]
fn reservations_precede_network_and_interrupted_attempts_are_counted_on_restart() {
    let d = Directory::new();
    let p = Provider::open();
    p.populate(1, 1, b"Original durable interrupted queue bytes");
    p.gate.store(true, Ordering::Release);
    let cfg = settings(&d.0, &p);
    let src = source(1, 1);
    let c = Client::open(&cfg, false).unwrap();
    let id = enqueue(&c, &src, 0);
    let worker = c.clone();
    let h = thread::spawn(move || worker.tick().unwrap());
    p.wait_requests(1);
    let mut v = data(&cfg.downloads.as_ref().unwrap().state_dir.join("queue.bin"));
    let r = snapshot_record(&mut v, &id);
    assert_eq!(
        r.get("attempts").unwrap().as_array().unwrap()[0].as_u64(),
        Some(1)
    );
    assert_eq!(r.get("state").unwrap().as_str(), Some("downloading"));
    assert!(r.get("reservation").unwrap().as_object().is_some());
    c.stop();
    p.gate.store(false, Ordering::Release);
    assert!(h.join().unwrap());
    assert_eq!(
        record(&c, &id).get("verified_parts").unwrap().as_u64(),
        Some(0)
    );
    drop(c);
    let c = Client::open(&cfg, false).unwrap();
    assert!(c.tick().unwrap());
    assert!(c.tick().unwrap());
    assert!(c.verified_file(&id).is_ok());
    assert_eq!(record(&c, &id).get("attempts").unwrap().as_u64(), Some(2));
    assert_eq!(p.requests.lock().unwrap().len(), 2);
}
#[test]
fn pause_resume_and_cancel_fence_inflight_results_without_releasing_live_slots() {
    let d = Directory::new();
    let p = Provider::open();
    p.populate(1, 1, b"Original fenced queue content");
    p.gate.store(true, Ordering::Release);
    let cfg = settings(&d.0, &p);
    let src = source(1, 1);
    let c = Client::open(&cfg, false).unwrap();
    let id = enqueue(&c, &src, 0);
    let worker = c.clone();
    let h = thread::spawn(move || worker.tick().unwrap());
    p.wait_requests(1);
    control(&c, &id, "pause");
    control(&c, &id, "resume");
    assert!(!c.tick().unwrap());
    assert_eq!(c.report().unwrap().get("active").unwrap().as_u64(), Some(1));
    p.gate.store(false, Ordering::Release);
    h.join().unwrap();
    assert_eq!(
        record(&c, &id).get("verified_parts").unwrap().as_u64(),
        Some(0)
    );
    assert!(c.tick().unwrap());
    assert!(c.tick().unwrap());
    let output = c.verified_file(&id).unwrap();
    let bytes = fs::read(&output).unwrap();
    control(&c, &id, "cancel");
    assert!(c.verified_file(&id).is_err());
    assert_eq!(fs::read(&output).unwrap(), bytes);
    assert!(!c.tick().unwrap());
    assert_eq!(p.requests.lock().unwrap().len(), 2);
}
#[test]
fn two_providers_share_at_most_two_slots_even_after_cancellation() {
    let d = Directory::new();
    let p = Provider::open();
    let q = Provider::open();
    p.populate(3, 1, b"Original parallel queue content");
    q.populate(3, 1, b"Original parallel queue content");
    p.gate.store(true, Ordering::Release);
    q.gate.store(true, Ordering::Release);
    let mut cfg = settings(&d.0, &p);
    let mut second = usenet(&q).get("servers").unwrap().as_array().unwrap()[0].clone();
    second.insert("id", "second");
    cfg.servers
        .push(mynou::usenet::Server::from_json(&second).unwrap());
    let c = Client::open(&cfg, false).unwrap();
    let src = source(3, 1);
    let first = enqueue(&c, &src, 0);
    let v = c.enqueue(&src, "second", 1, &preview()).unwrap();
    let second = v.get("id").unwrap().as_str().unwrap().to_owned();
    c.enqueue(&src, "second", 1, &apply(&v)).unwrap();
    let third = enqueue(&c, &src, 2);
    let a = c.clone();
    let h = thread::spawn(move || a.tick().unwrap());
    p.wait_requests(1);
    let b = c.clone();
    let j = thread::spawn(move || b.tick().unwrap());
    q.wait_requests(1);
    assert_eq!(c.report().unwrap().get("active").unwrap().as_u64(), Some(2));
    assert!(!c.tick().unwrap());
    control(&c, &first, "cancel");
    control(&c, &second, "cancel");
    assert!(!c.tick().unwrap());
    p.gate.store(false, Ordering::Release);
    q.gate.store(false, Ordering::Release);
    h.join().unwrap();
    j.join().unwrap();
    assert!(c.tick().unwrap());
    assert!(c.tick().unwrap());
    assert!(c.verified_file(&third).is_ok());
    assert_eq!(p.maximum.load(Ordering::Acquire), 1);
    assert_eq!(q.maximum.load(Ordering::Acquire), 1);
}
#[test]
fn snapshots_sources_receipts_and_ready_output_fail_closed_without_repair() {
    let d = Directory::new();
    let p = Provider::open();
    let bytes = b"Original private corruption fixture";
    p.populate(1, 2, bytes);
    let cfg = settings(&d.0, &p);
    let src = source(1, 2);
    let c = Client::open(&cfg, false).unwrap();
    let id = enqueue(&c, &src, 0);
    c.tick().unwrap();
    drop(c);
    let root = &cfg.downloads.as_ref().unwrap().state_dir;
    let source_id = mynou::usenet::nzb::Nzb::parse(&src).unwrap().id;
    for path in [
        root.join("queue.bin"),
        root.join("sources").join(format!("{source_id}.bin")),
        root.join("files").join(&id).join("part-00001.bin"),
    ] {
        let original = fs::read(&path).unwrap();
        let mut bad = original.clone();
        bad[24] ^= 1;
        fs::write(&path, &bad).unwrap();
        let before = files(root);
        assert!(Client::open(&cfg, false).is_err());
        assert_eq!(files(root), before);
        fs::write(&path, original).unwrap();
    }
    let path = root.join("queue.bin");
    let original = fs::read(&path).unwrap();
    let mut v = data(&path);
    snapshot_record(&mut v, &id).insert("attempts", Value::Array(vec![0_u32.into(), 0_u32.into()]));
    replace(&path, &v);
    let before = files(root);
    assert!(Client::open(&cfg, false).is_err());
    assert_eq!(files(root), before);
    fs::write(&path, original).unwrap();
    let c = Client::open(&cfg, false).unwrap();
    c.tick().unwrap();
    c.tick().unwrap();
    let output = c.verified_file(&id).unwrap();
    fs::write(&output, b"Corrupt bytes").unwrap();
    assert!(c.verified_file(&id).is_err());
    assert_eq!(record(&c, &id).get("ready").unwrap().as_bool(), Some(false));
    drop(c);
    assert!(Client::open(&cfg, false).is_err());
}
#[test]
fn completed_private_workspace_is_recovered_only_from_checked_output_and_receipts() {
    let d = Directory::new();
    let p = Provider::open();
    let bytes = b"Original verified output recovery";
    p.populate(1, 1, bytes);
    let cfg = settings(&d.0, &p);
    let src = source(1, 1);
    let c = Client::open(&cfg, false).unwrap();
    let id = enqueue(&c, &src, 0);
    c.tick().unwrap();
    drop(c);
    let root = &cfg.downloads.as_ref().unwrap().state_dir;
    let mut v = data(&root.join("queue.bin"));
    let row = snapshot_record(&mut v, &id);
    let binding = row.get("binding").unwrap().as_str().unwrap().to_owned();
    let max_file = row
        .get("max_file")
        .unwrap()
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut w = Workspace::open(
        &root.join("files").join(&id),
        &src,
        0,
        &binding,
        max_file,
        false,
    )
    .unwrap();
    w.assemble().unwrap();
    drop(w);
    let mut reserved = Value::object();
    reserved.insert("part", 0_u32);
    reserved.insert("token", "a".repeat(64));
    row.insert("state", "verifying");
    row.insert("reservation", reserved);
    replace(&root.join("queue.bin"), &v);
    let before = files(root);
    let c = Client::open(&cfg, true).unwrap();
    assert_eq!(
        record(&c, &id).get("state").unwrap().as_str(),
        Some("verifying")
    );
    assert_eq!(files(root), before);
    drop(c);
    let c = Client::open(&cfg, false).unwrap();
    assert_eq!(fs::read(c.verified_file(&id).unwrap()).unwrap(), bytes);
    assert!(!c.tick().unwrap());
    assert_eq!(p.requests.lock().unwrap().len(), 1);
}
#[test]
fn provider_binding_is_immutable_and_removal_pauses_without_fallback() {
    let d = Directory::new();
    let p = Provider::open();
    let mut cfg = settings(&d.0, &p);
    let src = source(2, 1);
    let c = Client::open(&cfg, false).unwrap();
    let a = enqueue(&c, &src, 0);
    let b = enqueue(&c, &src, 1);
    drop(c);
    assert_eq!(
        fs::read_dir(cfg.downloads.as_ref().unwrap().state_dir.join("sources"))
            .unwrap()
            .count(),
        1
    );
    let mut s = usenet(&p).get("servers").unwrap().as_array().unwrap()[0].clone();
    s.insert("max_article_bytes", 1024_u32);
    cfg.servers[0] = mynou::usenet::Server::from_json(&s).unwrap();
    assert!(Client::open(&cfg, false).is_err());
    cfg.servers.clear();
    let c = Client::open(&cfg, false).unwrap();
    for id in [&a, &b] {
        assert_eq!(
            record(&c, id).get("state").unwrap().as_str(),
            Some("paused")
        );
        assert!(c.control(id, "resume", &preview()).is_err());
    }
    assert!(!c.tick().unwrap());
    assert!(p.requests.lock().unwrap().is_empty());
}
#[test]
fn reviews_bind_source_action_queue_state_and_service_identity() {
    let d = Directory::new();
    let p = Provider::open();
    let cfg = settings(&d.0, &p);
    let src = source(2, 1);
    let c = Client::open(&cfg, false).unwrap();
    let early = c.enqueue(&src, "original", 0, &preview()).unwrap();
    let b = enqueue(&c, &src, 1);
    assert!(c.enqueue(&src, "original", 0, &apply(&early)).is_err());
    let a = enqueue(&c, &src, 0);
    let reviewed = c.control(&a, "pause", &preview()).unwrap();
    assert!(c.control(&a, "cancel", &apply(&reviewed)).is_err());
    control(&c, &b, "pause");
    assert!(c.control(&a, "pause", &apply(&reviewed)).is_err());
    let reviewed = c.control(&a, "pause", &preview()).unwrap();
    drop(c);
    let c = Client::open(&cfg, false).unwrap();
    assert!(c.control(&a, "pause", &apply(&reviewed)).is_err());
    let reviewed = c.control(&a, "pause", &preview()).unwrap();
    c.control(&a, "pause", &apply(&reviewed)).unwrap();
    assert!(c.control(&a, "pause", &apply(&reviewed)).is_err());
    let q = ProbeRequest {
        apply: true,
        plan_id: None,
    };
    assert!(c.enqueue(&src, "original", 0, &q).is_err());
    assert!(p.requests.lock().unwrap().is_empty());
}
#[test]
fn malformed_and_truncated_articles_spend_the_budget_without_poisoning_other_work() {
    let d = Directory::new();
    let p = Provider::open();
    let mut cfg = settings(&d.0, &p);
    cfg.downloads.as_mut().unwrap().max_attempts = 1;
    let bytes = b"Original independently checked bytes";
    let src = source(2, 1);
    p.populate(2, 1, bytes);
    p.set(
        "file0-part1@fixture.test",
        Response::Truncated(article(bytes, 1, 1)),
    );
    let c = Client::open(&cfg, false).unwrap();
    let bad = enqueue(&c, &src, 0);
    let good = enqueue(&c, &src, 1);
    c.tick().unwrap();
    assert_eq!(
        record(&c, &bad).get("state").unwrap().as_str(),
        Some("failed")
    );
    assert_eq!(
        c.report()
            .unwrap()
            .get("recovery_required")
            .unwrap()
            .as_bool(),
        Some(false)
    );
    c.tick().unwrap();
    c.tick().unwrap();
    assert_eq!(fs::read(c.verified_file(&good).unwrap()).unwrap(), bytes);
    assert!(c.verified_file(&bad).is_err());
}
#[test]
fn complete_part_receipts_with_wrong_whole_crc_never_publish_a_ready_transfer() {
    let d = Directory::new();
    let p = Provider::open();
    let bytes = b"Original whole CRC verification fixture";
    p.populate(1, 2, bytes);
    let mut last = article(bytes, 2, 2);
    let expected = format!(" crc32={:08x}", crc(bytes));
    let position = last
        .windows(expected.len())
        .position(|p| p == expected.as_bytes())
        .unwrap();
    last[position + 7] = if last[position + 7] == b'0' {
        b'1'
    } else {
        b'0'
    };
    p.set("file0-part2@fixture.test", Response::Body(last));
    let cfg = settings(&d.0, &p);
    let c = Client::open(&cfg, false).unwrap();
    let id = enqueue(&c, &source(1, 2), 0);
    c.tick().unwrap();
    c.tick().unwrap();
    c.tick().unwrap();
    assert_eq!(
        record(&c, &id).get("state").unwrap().as_str(),
        Some("failed")
    );
    assert_eq!(
        record(&c, &id).get("last_error").unwrap().as_str(),
        Some("verification_failed")
    );
    assert!(c.verified_file(&id).is_err());
    assert_eq!(
        fs::read_dir(
            cfg.downloads
                .as_ref()
                .unwrap()
                .state_dir
                .join("files")
                .join(id)
                .join("output")
        )
        .unwrap()
        .count(),
        0
    );
}
#[test]
fn service_workers_acquire_only_reviewed_staging_and_join_on_shutdown() {
    let d = Directory::new();
    let p = Provider::open();
    let bytes = b"Original background manager bytes";
    p.populate(1, 1, bytes);
    let mut v = config::default_json();
    v.get_mut("downloads").unwrap().insert("enabled", false);
    v.insert("usenet", usenet(&p));
    let cfg = config::from_json(&v, &d.0).unwrap();
    let root = cfg.usenet.downloads.as_ref().unwrap().state_dir.clone();
    let engine = Engine::open(cfg).unwrap();
    let src = source(1, 1);
    let plan = engine
        .usenet_enqueue(&src, "original", 0, &preview())
        .unwrap();
    engine
        .usenet_enqueue(&src, "original", 0, &apply(&plan))
        .unwrap();
    let id = plan.get("id").unwrap().as_str().unwrap();
    let workers = engine.start();
    wait(|| {
        engine
            .usenet_queue()
            .unwrap()
            .get("records")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("ready")
            .unwrap()
            .as_bool()
            == Some(true)
    });
    assert_eq!(
        fs::read(
            root.join("files")
                .join(id)
                .join("output/original queue fixture.bin")
        )
        .unwrap(),
        bytes
    );
    assert!(engine.store.lock().unwrap().list().is_empty());
    assert!(!engine.config.movies_root.exists());
    drop(workers);
    assert!(engine.stopped.load(Ordering::Acquire));
    assert_eq!(p.requests.lock().unwrap().len(), 1);
}
#[test]
fn strict_download_settings_reject_invalid_bounds_and_overlapping_roots() {
    let d = Directory::new();
    let p = Provider::open();
    let cases: [(&str, Value); 8] = [
        ("max_active", 0_u32.into()),
        ("max_active", 3_u32.into()),
        ("max_attempts", 0_u32.into()),
        ("max_attempts", 11_u32.into()),
        ("max_file_bytes", 0_u32.into()),
        ("enabled", "yes".into()),
        ("state_dir", "state/jobs".into()),
        ("unknown", true.into()),
    ];
    for (key, value) in cases {
        let mut v = config::default_json();
        let mut u = usenet(&p);
        u.get_mut("downloads").unwrap().insert(key, value);
        v.insert("usenet", u);
        assert!(config::from_json(&v, &d.0).is_err(), "accepted {key}");
    }
    let mut v = config::default_json();
    let mut u = usenet(&p);
    u.get_mut("downloads").unwrap().insert("enabled", false);
    v.insert("usenet", u);
    let cfg = config::from_json(&v, &d.0).unwrap();
    let e = Engine::open(cfg).unwrap();
    assert!(!d.0.join("state/usenet").exists());
    assert!(e.store.lock().unwrap().list().is_empty());
    assert!(p.requests.lock().unwrap().is_empty());
}
#[test]
fn queue_api_browser_and_cli_enforce_authentication_and_bound_reviews() {
    let d = Directory::new();
    let p = Provider::open();
    let mut v = config::default_json();
    v.insert("listen", "127.0.0.1:0");
    v.get_mut("downloads").unwrap().insert("enabled", false);
    v.insert("usenet", usenet(&p));
    let server = Server::open(config::from_json(&v, &d.0).unwrap());
    let src = source(1, 1);
    let mut q = Value::object();
    q.insert("nzb", std::str::from_utf8(&src).unwrap());
    q.insert("server_id", "original");
    q.insert("file_index", 0_u32);
    assert_eq!(server.call("GET", "/api/usenet/queue", &[], "").status, 401);
    assert_eq!(
        server
            .call(
                "POST",
                "/api/usenet/queue",
                &[("Content-Type", "application/json")],
                &json::stringify(&q)
            )
            .status,
        401
    );
    let auth = format!("Bearer {TOKEN}");
    let headers = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
    ];
    let before = files(&d.0);
    let reply = server.call("POST", "/api/usenet/queue", &headers, &json::stringify(&q));
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(files(&d.0), before);
    let plan = json::parse(&reply.body).unwrap();
    private(&plan);
    q.insert("apply", true);
    q.insert("plan_id", plan.get("plan_id").unwrap().clone());
    assert_eq!(
        server
            .call("POST", "/api/usenet/queue", &headers, &json::stringify(&q))
            .status,
        200
    );
    assert_eq!(
        server
            .call("POST", "/api/usenet/queue", &headers, &json::stringify(&q))
            .status,
        400
    );
    let id = plan.get("id").unwrap().as_str().unwrap();
    let browser = Browser::login(&server);
    let other = Browser::login(&server);
    let page = browser.get(&server, "/ui/usenet");
    assert_eq!(page.status, 200);
    page.no_secrets();
    assert!(!page.body.contains("Private original"));
    let review = browser.post(
        &server,
        "/ui/usenet/control",
        &[("id", id), ("action", "pause")],
    );
    assert_eq!(review.status, 200, "{}", review.body);
    let guard = review
        .body
        .split("name=\"plan_id\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let fields = [
        ("id", id),
        ("action", "pause"),
        ("apply", "yes"),
        ("plan_id", guard),
    ];
    assert_eq!(
        other.post(&server, "/ui/usenet/control", &fields).status,
        400
    );
    assert_eq!(
        browser
            .raw_post(&server, "/ui/usenet/control", &web_support::fields(&fields))
            .status,
        403
    );
    assert_eq!(
        browser.post(&server, "/ui/usenet/control", &fields).status,
        303
    );
    assert_eq!(
        browser.post(&server, "/ui/usenet/control", &fields).status,
        400
    );
    v.insert("listen", server.authority.clone());
    let config_path = d.0.join("mynou.json");
    fs::write(&config_path, json::stringify(&v)).unwrap();
    let config_path = config_path.to_str().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args(["usenet-queue", "--config", config_path])
        .env("MYNOU_API_TOKEN", TOKEN)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    private(&json::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap());
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args([
            "usenet-control",
            id,
            "--action",
            "resume",
            "--config",
            config_path,
        ])
        .env("MYNOU_API_TOKEN", TOKEN)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    private(&json::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap());
    assert!(server.engine.store.lock().unwrap().list().is_empty());
    assert!(p.requests.lock().unwrap().is_empty());
    let q = QueueControl {
        action: "resume".into(),
        apply: true,
        plan_id: Some("a".repeat(64)),
    };
    server.engine.stopped.store(true, Ordering::Release);
    assert!(server.engine.usenet_queue_control(id, &q).is_err());
}
#[cfg(unix)]
#[test]
fn private_process_ownership_modes_and_links_are_checked_before_resume() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let d = Directory::new();
    let p = Provider::open();
    let cfg = settings(&d.0, &p);
    let src = source(1, 1);
    let c = Client::open(&cfg, false).unwrap();
    let id = enqueue(&c, &src, 0);
    assert!(Client::open(&cfg, false).is_err());
    let root = &cfg.downloads.as_ref().unwrap().state_dir;
    assert_eq!(fs::metadata(root).unwrap().permissions().mode() & 0o077, 0);
    drop(c);
    for path in [
        root.join(".owner"),
        root.join("queue.bin"),
        root.join("files").join(&id).join(".owner"),
    ] {
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Client::open(&cfg, false).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let duplicate = d.0.join("duplicate");
        fs::hard_link(&path, &duplicate).unwrap();
        assert!(Client::open(&cfg, false).is_err());
        fs::remove_file(&duplicate).unwrap();
        let saved = path.with_extension("saved");
        fs::rename(&path, &saved).unwrap();
        symlink(&saved, &path).unwrap();
        assert!(Client::open(&cfg, false).is_err());
        fs::remove_file(&path).unwrap();
        fs::rename(&saved, &path).unwrap();
    }
    assert!(p.requests.lock().unwrap().is_empty());
}
