//! Original loopback outcome delivery, durable retries and protected reviews. CI only.
mod irc_support;
mod library_support;
mod requester_support;
mod web_support;
use library_support::Directory;
use mynou::{
    config,
    engine::Engine,
    json::{self, Value},
    notifications::ControlRequest,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use web_support::{Browser, Server, TOKEN};
struct Receiver {
    url: String,
    calls: Arc<Mutex<Vec<Value>>>,
    response: Arc<Mutex<(u16, Option<String>)>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Receiver {
    fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/notify", listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let response = Arc::new(Mutex::new((204, None::<String>)));
        let stop = Arc::new(AtomicBool::new(false));
        let (c, r, s) = (calls.clone(), response.clone(), stop.clone());
        let t = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("Receiver accept failed: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    assert!(bytes.len() < 16384);
                    let mut b = [0];
                    stream.read_exact(&mut b).unwrap();
                    bytes.push(b[0]);
                }
                let header = String::from_utf8(bytes).unwrap();
                assert!(header.starts_with("POST /notify "));
                let fields: BTreeMap<_, _> = header
                    .lines()
                    .skip(1)
                    .filter_map(|s| s.split_once(':'))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                let len = fields["content-length"].parse::<usize>().unwrap();
                assert!(len <= 4096);
                let mut body = vec![0; len];
                stream.read_exact(&mut body).unwrap();
                let payload = json::parse(std::str::from_utf8(&body).unwrap()).unwrap();
                let id = payload.get("id").unwrap().as_str().unwrap();
                assert!(fields["idempotency-key"] == id && fields["x-mynou-event-id"] == id);
                assert_eq!(fields["content-type"], "application/json");
                if let Some(auth) = fields.get("authorization") {
                    assert!(
                        auth == &format!("Bearer {}", std::env::var("PATH").unwrap().trim()),
                        "Receiver credential mismatch"
                    );
                }
                c.lock().unwrap().push(payload);
                let (status, location) = r.lock().unwrap().clone();
                let location = location
                    .map(|l| format!("Location: {l}\r\n"))
                    .unwrap_or_default();
                write!(stream,"HTTP/1.1 {status} Fixture\r\n{location}Content-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        });
        Self {
            url,
            calls,
            response,
            stop,
            thread: Some(t),
        }
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}
fn route(id: &str, kind: &str, scope: &str, url: &str) -> Value {
    let mut r = Value::object();
    r.insert("id", id);
    r.insert(
        if kind == "irc" {
            "source_id"
        } else {
            "account_id"
        },
        scope,
    );
    r.insert("enabled", true);
    r.insert("url", url);
    r.insert("max_attempts", 2_u32);
    r
}
fn configured(dir: &Directory, receiver: &Receiver) -> config::Config {
    let mut v = irc_support::value(&dir.0, "irc://127.0.0.1:1");
    let mut n = Value::object();
    n.insert(
        "routes",
        Value::Array(vec![route("announce", "irc", "local", &receiver.url)]),
    );
    v.insert("notifications", n);
    config::from_json(&v, &dir.0).unwrap()
}
fn events(e: &Engine) -> Vec<Value> {
    e.notifications(0, 200)
        .unwrap()
        .get("events")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec()
}
fn query(v: &Value, action: &str) -> ControlRequest {
    ControlRequest::from_json(
        &json::parse(&format!(
            r#"{{"kind":"{}","event_id":"{}","action":"{action}"}}"#,
            v.get("kind").unwrap().as_str().unwrap(),
            v.get("id").unwrap().as_str().unwrap()
        ))
        .unwrap(),
    )
    .unwrap()
}
fn reviewed(e: &Engine, v: &Value, action: &str) -> ControlRequest {
    let mut q = query(v, action);
    q.plan_id = Some(
        e.notification_control(&q)
            .unwrap()
            .get("plan_id")
            .unwrap()
            .as_str()
            .unwrap()
            .into(),
    );
    q.apply = true;
    q
}
fn phase(v: &Value) -> &str {
    v.get("phase").unwrap().as_str().unwrap()
}
fn read(path: &std::path::Path) -> Value {
    let b = fs::read(path).unwrap();
    json::parse(std::str::from_utf8(&b[16..b.len() - 32]).unwrap()).unwrap()
}
fn write(path: &std::path::Path, magic: &[u8; 8], v: &Value) {
    let payload = json::stringify(v).into_bytes();
    let mut b = magic.to_vec();
    b.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    b.extend_from_slice(&payload);
    b.extend_from_slice(&mynou::crypto::sha256(&b));
    fs::write(path, b).unwrap();
}
fn change_events(cfg: &config::Config, change: impl FnOnce(&mut Vec<Value>)) {
    let p = cfg.store_dir.join("announcements.bin");
    let mut v = read(&p);
    let Value::Array(events) = v.get_mut("delivery").unwrap().get_mut("events").unwrap() else {
        panic!()
    };
    change(events);
    write(&p, b"MYNOUI04", &v);
}
fn redacted(v: &Value) {
    let s = json::stringify(v);
    for forbidden in [
        "http://",
        "https://",
        "token_env",
        "binding",
        "magnet:",
        "private-receiver-secret",
    ] {
        assert!(
            !s.contains(forbidden),
            "Public delivery exposed private data"
        );
    }
}
#[test]
fn strict_route_scope_transport_credentials_and_control_bounds() {
    let d = Directory::new();
    let receiver = Receiver::open();
    let cfg = configured(&d, &receiver);
    assert_eq!(cfg.notifications.routes.len(), 1);
    let mut v = irc_support::value(&d.0, "irc://127.0.0.1:1");
    let r = route("announce", "irc", "local", &receiver.url);
    for (key, value) in [
        ("id", Value::from("BAD")),
        ("source_id", "unknown".into()),
        ("url", "http://example.com/notify".into()),
        ("url", "https://example.com/notify?token=secret".into()),
        ("url", "https://user:pass@example.com/notify".into()),
        ("token_env", "bad\nname".into()),
        ("max_attempts", 9_u32.into()),
        ("account_id", "alice".into()),
        ("unknown", true.into()),
    ] {
        let mut changed = r.clone();
        changed.insert(key, value);
        let mut n = Value::object();
        n.insert("routes", Value::Array(vec![changed]));
        v.insert("notifications", n);
        assert!(config::from_json(&v, &d.0).is_err());
    }
    let mut n = Value::object();
    n.insert("routes", Value::Array(vec![r.clone(), r]));
    v.insert("notifications", n);
    assert!(config::from_json(&v, &d.0).is_err());
    assert!(
        ControlRequest::from_json(
            &json::parse(r#"{"kind":"irc","event_id":"bad","action":"retry"}"#).unwrap()
        )
        .is_err()
    );
}
#[test]
fn receipt_and_decision_events_are_atomic_duplicate_safe_and_endpoint_redacted() {
    let d = Directory::new();
    let receiver = Receiver::open();
    let cfg = configured(&d, &receiver);
    let e = Engine::open(cfg.clone()).unwrap();
    let row = irc_support::receive(&e, 7);
    assert_eq!(events(&e).len(), 1);
    let before = irc_support::bytes(&cfg.store_dir);
    irc_support::receive(&e, 7);
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    assert!(receiver.calls.lock().unwrap().is_empty());
    e.irc_control(
        irc_support::record_id(&row),
        &irc_support::reviewed(&e, irc_support::record_id(&row), "acknowledge"),
    )
    .unwrap();
    let out = events(&e);
    assert_eq!(out.len(), 2);
    assert!(
        out.iter()
            .any(|v| v.get("outcome").unwrap().as_str() == Some("acknowledged"))
    );
    redacted(&e.notifications(0, 100).unwrap());
    assert_eq!(
        &fs::read(cfg.store_dir.join("announcements.bin")).unwrap()[..8],
        b"MYNOUI04"
    );
    drop(e);
    let before = irc_support::bytes(&cfg.store_dir);
    let e = Engine::open_for_preview(cfg.clone()).unwrap();
    assert_eq!(events(&e), out);
    drop(e);
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
}
#[test]
fn native_http_delivery_acknowledges_fixed_payload_and_never_replays_terminal_events() {
    let d = Directory::new();
    let r = Receiver::open();
    let cfg = configured(&d, &r);
    let e = Engine::open(cfg.clone()).unwrap();
    irc_support::receive(&e, 7);
    let original = events(&e).remove(0);
    let result = e.dispatch_notifications().unwrap();
    assert_eq!(result.get("delivered").unwrap().as_u64(), Some(1));
    let out = events(&e).remove(0);
    assert_eq!(phase(&out), "delivered");
    assert_eq!(out.get("id"), original.get("id"));
    assert_eq!(out.get("attempts").unwrap().as_u64(), Some(1));
    let sent = r.calls.lock().unwrap()[0].clone();
    redacted(&sent);
    assert_eq!(sent.get("subject_id"), original.get("subject_id"));
    assert!(sent.get("title").is_none());
    assert!(sent.get("phase").is_none());
    drop(e);
    let e = Engine::open(cfg).unwrap();
    e.dispatch_notifications().unwrap();
    assert_eq!(r.calls.lock().unwrap().len(), 1);
    irc_support::no_jobs(&e);
}
#[test]
fn failed_delivery_uses_same_event_id_and_persistent_attempt_budget() {
    let d = Directory::new();
    let r = Receiver::open();
    *r.response.lock().unwrap() = (503, None);
    let cfg = configured(&d, &r);
    let e = Engine::open(cfg.clone()).unwrap();
    irc_support::receive(&e, 7);
    e.dispatch_notifications().unwrap();
    let out = events(&e).remove(0);
    assert_eq!(phase(&out), "pending");
    assert_eq!(
        out.get("last_error").unwrap().as_str(),
        Some("Delivery failed")
    );
    drop(e);
    change_events(&cfg, |es| {
        for v in es {
            let at = v.get("at").cloned();
            assert!(at.is_none());
            let signal = v.get("signal").unwrap();
            v.insert("next_at", signal.get("at").unwrap().clone());
        }
    });
    let e = Engine::open(cfg.clone()).unwrap();
    e.dispatch_notifications().unwrap();
    let out = events(&e).remove(0);
    assert_eq!(phase(&out), "failed");
    assert_eq!(out.get("attempts").unwrap().as_u64(), Some(2));
    assert!(e.notification_control(&query(&out, "retry")).is_err());
    drop(e);
    let e = Engine::open(cfg).unwrap();
    e.dispatch_notifications().unwrap();
    let calls = r.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].get("id"), calls[1].get("id"));
}
#[test]
fn redirects_never_send_an_outcome_to_another_endpoint() {
    let d = Directory::new();
    let first = Receiver::open();
    let other = Receiver::open();
    *first.response.lock().unwrap() = (307, Some(other.url.clone()));
    let e = Engine::open(configured(&d, &first)).unwrap();
    irc_support::receive(&e, 7);
    e.dispatch_notifications().unwrap();
    assert_eq!(first.calls.lock().unwrap().len(), 1);
    assert!(other.calls.lock().unwrap().is_empty());
    assert_eq!(phase(&events(&e)[0]), "pending");
    redacted(&e.notifications(0, 100).unwrap());
}
#[test]
fn expired_sending_leases_recover_without_writes_in_read_only_views() {
    let d = Directory::new();
    let r = Receiver::open();
    let cfg = configured(&d, &r);
    let e = Engine::open(cfg.clone()).unwrap();
    irc_support::receive(&e, 7);
    let id = events(&e)[0].get("id").unwrap().clone();
    drop(e);
    change_events(&cfg, |es| {
        es[0].insert("phase", "sending");
        es[0].insert("attempts", 1_u32);
        let at = es[0].get("signal").unwrap().get("at").unwrap().clone();
        es[0].insert("next_at", at);
    });
    let before = irc_support::bytes(&cfg.store_dir);
    let e = Engine::open_for_preview(cfg.clone()).unwrap();
    assert_eq!(phase(&events(&e)[0]), "sending");
    assert!(e.dispatch_notifications().is_err());
    drop(e);
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    let e = Engine::open(cfg).unwrap();
    e.dispatch_notifications().unwrap();
    let out = events(&e).remove(0);
    assert_eq!(out.get("id"), Some(&id));
    assert_eq!(phase(&out), "delivered");
    assert_eq!(out.get("attempts").unwrap().as_u64(), Some(2));
}
#[test]
fn disabled_or_removed_routes_pause_work_and_added_routes_do_not_backfill() {
    let d = Directory::new();
    let r = Receiver::open();
    let mut cfg = configured(&d, &r);
    cfg.notifications.routes[0].enabled = false;
    let e = Engine::open(cfg.clone()).unwrap();
    irc_support::receive(&e, 7);
    assert!(events(&e).is_empty());
    drop(e);
    cfg.notifications.routes[0].enabled = true;
    let e = Engine::open(cfg.clone()).unwrap();
    irc_support::receive(&e, 7);
    assert!(events(&e).is_empty());
    irc_support::receive(&e, 8);
    assert_eq!(events(&e).len(), 1);
    drop(e);
    let active = cfg.clone();
    cfg.notifications.routes.clear();
    let e = Engine::open(cfg).unwrap();
    e.dispatch_notifications().unwrap();
    assert!(r.calls.lock().unwrap().is_empty());
    assert_eq!(phase(&events(&e)[0]), "pending");
    drop(e);
    let e = Engine::open(active).unwrap();
    e.dispatch_notifications().unwrap();
    assert_eq!(r.calls.lock().unwrap().len(), 1);
}
#[test]
fn captured_bindings_and_notification_magic_reject_semantic_rebinding() {
    for corruption in ["binding", "magic", "orphan", "digest", "attempts"] {
        let d = Directory::new();
        let r = Receiver::open();
        let cfg = configured(&d, &r);
        let e = Engine::open(cfg.clone()).unwrap();
        irc_support::receive(&e, 7);
        drop(e);
        let path = cfg.store_dir.join("announcements.bin");
        let mut v = read(&path);
        let mut changed = cfg.clone();
        match corruption {
            "binding" => changed.notifications.routes[0].url.push_str("/different"),
            "magic" => write(&path, b"MYNOUI03", &v),
            _ => {
                let Value::Array(events) =
                    v.get_mut("delivery").unwrap().get_mut("events").unwrap()
                else {
                    panic!()
                };
                match corruption {
                    "orphan" => events[0].insert("route_id", "missing"),
                    "digest" => events[0].insert("id", "b".repeat(64)),
                    "attempts" => events[0].insert("attempts", 99_u32),
                    _ => unreachable!(),
                }
                write(&path, b"MYNOUI04", &v);
            }
        }
        let before = irc_support::bytes(&cfg.store_dir);
        assert!(
            Engine::open_for_preview(changed.clone()).is_err(),
            "{corruption}"
        );
        assert!(Engine::open(changed).is_err(), "{corruption}");
        assert_eq!(irc_support::bytes(&cfg.store_dir), before);
        assert!(r.calls.lock().unwrap().is_empty());
    }
}
#[test]
fn requester_preferences_filter_durable_delivery_without_affecting_admission() {
    for preference in ["none", "decisions", "all"] {
        let d = Directory::new();
        let r = Receiver::open();
        let accounts = requester_support::Accounts::open();
        let mut cfg = accounts.config(&d.0);
        cfg.notifications.routes.push(mynou::notifications::Route {
            id: "alice-events".into(),
            enabled: true,
            kind: "requester".into(),
            scope: "alice".into(),
            url: r.url.clone(),
            token_env: None,
            max_attempts: 2,
        });
        let e = Engine::open(cfg.clone()).unwrap();
        requester_support::enable(&e, "alice");
        let mut p = requester_support::policy(&e, "alice");
        p.notifications = preference.into();
        requester_support::apply(&e, "alice", requester_support::policy_query(p));
        accounts.watchlist("alice", vec![requester_support::movie(7, "Fixture Movie")]);
        e.sync_requesters().unwrap();
        let out = events(&e);
        assert_eq!(
            out.len(),
            match preference {
                "none" => 0,
                "decisions" => 1,
                _ => 3,
            }
        );
        assert_eq!(mynou::engine::lock(&e.store).unwrap().list().len(), 1);
        e.sync_requesters().unwrap();
        assert_eq!(events(&e), out);
        e.dispatch_notifications().unwrap();
        assert_eq!(r.calls.lock().unwrap().len(), out.len());
        assert_eq!(
            &fs::read(cfg.store_dir.join("requesters.bin")).unwrap()[..8],
            b"MYNOUR03"
        );
        drop(e);
        assert!(Engine::open(cfg).is_ok());
    }
}
#[test]
fn missing_credentials_produce_fixed_failures_without_network_or_secret_leaks() {
    let d = Directory::new();
    let r = Receiver::open();
    let mut cfg = configured(&d, &r);
    cfg.notifications.routes[0].token_env =
        Some("MYNOU_ABSENT_NOTIFICATION_FIXTURE_CREDENTIAL_68212".into());
    let e = Engine::open(cfg).unwrap();
    irc_support::receive(&e, 7);
    let report = e.dispatch_notifications().unwrap();
    assert_eq!(report.get("failed_attempts").unwrap().as_u64(), Some(1));
    assert!(r.calls.lock().unwrap().is_empty());
    let out = events(&e).remove(0);
    assert_eq!(
        out.get("last_error").unwrap().as_str(),
        Some("Delivery failed")
    );
    redacted(&e.notifications(0, 100).unwrap());
}
fn guard(body: &str) -> &str {
    body.split("name=\"plan_id\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
}
#[test]
fn protected_browser_api_and_cli_bind_delivery_reviews_and_preserve_job_state() {
    let d = Directory::new();
    let r = Receiver::open();
    let server = Server::open(configured(&d, &r));
    irc_support::receive(&server.engine, 7);
    let event = events(&server.engine).remove(0);
    let id = event.get("id").unwrap().as_str().unwrap();
    assert_eq!(
        server.call("GET", "/api/notifications", &[], "").status,
        401
    );
    let response = server.call(
        "GET",
        "/api/notifications?offset=0&limit=1",
        &[("Authorization", &format!("Bearer {TOKEN}"))],
        "",
    );
    assert_eq!(response.status, 200);
    response.no_secrets();
    redacted(&json::parse(&response.body).unwrap());
    let first = Browser::login(&server);
    let second = Browser::login(&server);
    assert_eq!(first.get(&server, "/ui/notifications").status, 200);
    let preview = first.post(
        &server,
        "/ui/notifications/control",
        &[("kind", "irc"), ("event_id", id), ("action", "discard")],
    );
    assert_eq!(preview.status, 200, "{}", preview.body);
    let plan = guard(&preview.body);
    let fields = [
        ("kind", "irc"),
        ("event_id", id),
        ("action", "discard"),
        ("apply", "yes"),
        ("plan_id", plan),
    ];
    assert_eq!(
        second
            .post(&server, "/ui/notifications/control", &fields)
            .status,
        400
    );
    assert_eq!(
        first
            .post(&server, "/ui/notifications/control", &fields)
            .status,
        303
    );
    assert_eq!(
        first
            .post(&server, "/ui/notifications/control", &fields)
            .status,
        400
    );
    assert_eq!(phase(&events(&server.engine)[0]), "discarded");
    server.engine.dispatch_notifications().unwrap();
    assert!(r.calls.lock().unwrap().is_empty());
    irc_support::no_jobs(&server.engine);
    let cfg = server.engine.config.clone();
    let path = d.0.join("mynou.json");
    let mut value = irc_support::value(&d.0, "irc://127.0.0.1:1");
    value.insert("listen", server.authority.clone());
    let mut n = Value::object();
    n.insert(
        "routes",
        Value::Array(vec![route("announce", "irc", "local", &r.url)]),
    );
    value.insert("notifications", n);
    fs::write(&path, json::stringify(&value)).unwrap();
    let before = irc_support::bytes(&cfg.store_dir);
    let output = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .env("MYNOU_API_TOKEN", TOKEN)
        .args(["notifications", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    redacted(&json::parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap());
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
}
#[test]
fn stale_review_and_offline_preview_never_mutate_delivery_or_acquisition() {
    let d = Directory::new();
    let r = Receiver::open();
    let cfg = configured(&d, &r);
    let e = Engine::open(cfg.clone()).unwrap();
    irc_support::receive(&e, 7);
    let event = events(&e).remove(0);
    let old = reviewed(&e, &event, "discard");
    e.notification_control(&reviewed(&e, &event, "retry"))
        .unwrap();
    let before = irc_support::bytes(&cfg.store_dir);
    // A retry of a fresh event may be a no-op; the following successful lease changes the guard.
    e.dispatch_notifications().unwrap();
    let after = irc_support::bytes(&cfg.store_dir);
    assert_ne!(before, after);
    assert!(e.notification_control(&old).is_err());
    assert_eq!(irc_support::bytes(&cfg.store_dir), after);
    drop(e);
    let path = d.0.join("mynou.json");
    let mut v = irc_support::value(&d.0, "irc://127.0.0.1:1");
    let mut n = Value::object();
    n.insert(
        "routes",
        Value::Array(vec![route("announce", "irc", "local", &r.url)]),
    );
    v.insert("notifications", n);
    fs::write(&path, json::stringify(&v)).unwrap();
    let before = irc_support::bytes(&cfg.store_dir);
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args(["notifications", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    redacted(&json::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap());
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args(["notifications-dispatch", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
}
#[test]
fn background_worker_delivers_only_opted_in_routes() {
    let d = Directory::new();
    let r = Receiver::open();
    let cfg = configured(&d, &r);
    let e = Engine::open(cfg).unwrap();
    irc_support::receive(&e, 7);
    let workers = e.start();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while phase(&events(&e)[0]) != "delivered" {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    drop(workers);
    assert_eq!(r.calls.lock().unwrap().len(), 1);
    irc_support::no_jobs(&e);
}

#[test]
fn successful_environment_credentials_stay_in_headers_and_out_of_event_history() {
    let d = Directory::new();
    let r = Receiver::open();
    let mut cfg = configured(&d, &r);
    cfg.notifications.routes[0].token_env = Some("PATH".into());
    let e = Engine::open(cfg).unwrap();
    irc_support::receive(&e, 7);
    assert_eq!(
        e.dispatch_notifications()
            .unwrap()
            .get("delivered")
            .unwrap()
            .as_u64(),
        Some(1)
    );
    assert_eq!(r.calls.lock().unwrap().len(), 1);
    redacted(&e.notifications(0, 100).unwrap());
}
