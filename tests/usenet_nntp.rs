//! Original loopback-only NNTP fixtures. No public article or provider access.
mod library_support;
mod web_support;
use library_support::Directory;
use mynou::{
    config,
    json::{self, Value},
    usenet::{self, ProbeRequest, nntp},
};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use web_support::{Browser, Server, TOKEN};
#[derive(Clone)]
struct Behavior {
    greeting: Vec<u8>,
    user_reply: Vec<u8>,
    pass_reply: Vec<u8>,
    body_reply: Vec<u8>,
    body: Vec<u8>,
    end: bool,
    tls_reject: bool,
}
impl Default for Behavior {
    fn default() -> Self {
        Self {
            greeting: b"201 Original fixture ready\r\n".to_vec(),
            user_reply: b"381 Password required\r\n".to_vec(),
            pass_reply: b"281 Accepted\r\n".to_vec(),
            body_reply: b"222 0 <article@fixture.test> body follows\r\n".to_vec(),
            body: b"Original body\r\n..leading dot\r\n".to_vec(),
            end: true,
            tls_reject: false,
        }
    }
}
struct Provider {
    port: u16,
    behavior: Arc<Mutex<Behavior>>,
    connections: Arc<AtomicUsize>,
    bodies: Arc<AtomicUsize>,
    users: Arc<AtomicUsize>,
    passwords: Arc<AtomicUsize>,
    blocked: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
fn read_command(stream: &mut std::net::TcpStream) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    while !v.ends_with(b"\r\n") {
        let mut b = [0];
        if stream.read_exact(&mut b).is_err() {
            return None;
        }
        assert!(v.len() < 8192);
        v.push(b[0]);
    }
    Some(v)
}
impl Provider {
    fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let behavior = Arc::new(Mutex::new(Behavior::default()));
        let connections = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(AtomicUsize::new(0));
        let users = Arc::new(AtomicUsize::new(0));
        let passwords = Arc::new(AtomicUsize::new(0));
        let blocked = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (f, c, a, u, p, b, s) = (
            behavior.clone(),
            connections.clone(),
            bodies.clone(),
            users.clone(),
            passwords.clone(),
            blocked.clone(),
            stop.clone(),
        );
        let t = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("NNTP fixture accept failed: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                c.fetch_add(1, Ordering::AcqRel);
                let start = Instant::now();
                while b.load(Ordering::Acquire) && !s.load(Ordering::Acquire) {
                    assert!(start.elapsed() < Duration::from_secs(3));
                    thread::sleep(Duration::from_millis(2));
                }
                let state = f.lock().unwrap().clone();
                if stream.write_all(&state.greeting).is_err() {
                    continue;
                }
                if state.tls_reject {
                    continue;
                }
                while let Some(cmd) = read_command(&mut stream) {
                    let reply = if cmd.starts_with(b"AUTHINFO USER ") {
                        assert!(
                            cmd == format!("AUTHINFO USER {}\r\n", std::env::var("PWD").unwrap())
                                .as_bytes(),
                            "NNTP username mismatch"
                        );
                        u.fetch_add(1, Ordering::AcqRel);
                        state.user_reply.as_slice()
                    } else if cmd.starts_with(b"AUTHINFO PASS ") {
                        assert!(
                            cmd == format!("AUTHINFO PASS {}\r\n", std::env::var("PATH").unwrap())
                                .as_bytes(),
                            "NNTP password mismatch"
                        );
                        p.fetch_add(1, Ordering::AcqRel);
                        state.pass_reply.as_slice()
                    } else if cmd.starts_with(b"BODY ") {
                        assert_eq!(cmd, b"BODY <article@fixture.test>\r\n");
                        a.fetch_add(1, Ordering::AcqRel);
                        let _ = stream.write_all(&state.body_reply);
                        if state.body_reply.starts_with(b"222 ") {
                            let _ = stream.write_all(&state.body);
                            if state.end {
                                let _ = stream.write_all(b".\r\n");
                            }
                        }
                        break;
                    } else {
                        assert_eq!(cmd, b"QUIT\r\n");
                        b"205 Closing\r\n"
                    };
                    if stream.write_all(reply).is_err() || cmd == b"QUIT\r\n" {
                        break;
                    }
                }
            }
        });
        Self {
            port,
            behavior,
            connections,
            bodies,
            users,
            passwords,
            blocked,
            stop,
            thread: Some(t),
        }
    }
    fn json(&self, auth: bool) -> Value {
        let mut v = Value::object();
        v.insert("id", "provider");
        v.insert("host", "127.0.0.1");
        v.insert("port", u32::from(self.port));
        v.insert("tls", false);
        v.insert("timeout_ms", 1000_u32);
        if auth {
            v.insert("username_env", "PWD");
            v.insert("password_env", "PATH");
        }
        v
    }
    fn server(&self, auth: bool) -> usenet::Server {
        usenet::Server::from_json(&self.json(auth)).unwrap()
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.blocked.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take()
            && let Err(e) = t.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(e);
        }
    }
}
fn config_value(d: &Directory, p: &Provider) -> Value {
    let mut v = config::default_json();
    v.insert("listen", "127.0.0.1:0");
    v.insert("store_dir", d.0.join("jobs").to_str().unwrap());
    v.get_mut("downloads").unwrap().insert("enabled", false);
    let mut u = Value::object();
    u.insert("servers", Value::Array(vec![p.json(true)]));
    v.insert("usenet", u);
    v
}
fn configuration(d: &Directory, p: &Provider) -> config::Config {
    config::from_json(&config_value(d, p), &d.0).unwrap()
}
fn private(v: &Value) {
    let s = json::stringify(v);
    for forbidden in [
        "127.0.0.1",
        "host",
        "port",
        "username_env",
        "password_env",
        "PWD",
        "PATH",
        "article@fixture.test",
        "AUTHINFO",
    ] {
        assert!(!s.contains(forbidden), "Usenet report exposed private data");
    }
}
#[test]
fn native_body_authentication_dot_unstuffing_and_read_only_probes_work() {
    let p = Provider::open();
    let s = p.server(true);
    assert_eq!(
        nntp::body(&s, "article@fixture.test").unwrap(),
        b"Original body\r\n.leading dot\r\n"
    );
    nntp::probe(&s).unwrap();
    assert_eq!(p.bodies.load(Ordering::Acquire), 1);
    assert_eq!(p.users.load(Ordering::Acquire), 2);
    assert_eq!(p.passwords.load(Ordering::Acquire), 2);
    let s = p.server(false);
    nntp::probe(&s).unwrap();
    assert_eq!(p.users.load(Ordering::Acquire), 2);
}
#[test]
fn immediate_auth_acceptance_never_sends_password_and_failed_auth_never_reads_article() {
    let p = Provider::open();
    p.behavior.lock().unwrap().user_reply = b"281 Already accepted\r\n".to_vec();
    nntp::probe(&p.server(true)).unwrap();
    assert_eq!(p.passwords.load(Ordering::Acquire), 0);
    p.behavior.lock().unwrap().user_reply = b"481 private-secret-rejection\r\n".to_vec();
    let e = nntp::body(&p.server(true), "article@fixture.test").unwrap_err();
    assert_eq!(e, "NNTP: authentication rejected");
    assert_eq!(p.bodies.load(Ordering::Acquire), 0);
    p.behavior.lock().unwrap().user_reply = b"381 Password required\r\n".to_vec();
    p.behavior.lock().unwrap().pass_reply = b"481 private-secret-rejection\r\n".to_vec();
    assert!(nntp::body(&p.server(true), "article@fixture.test").is_err());
    assert_eq!(p.bodies.load(Ordering::Acquire), 0);
}
#[test]
fn missing_credentials_and_injected_ids_fail_before_connection() {
    let p = Provider::open();
    let mut v = p.json(true);
    v.insert("password_env", "MYNOU_ABSENT_NNTP_FIXTURE_SECRET_425918");
    let s = usenet::Server::from_json(&v).unwrap();
    assert_eq!(
        nntp::probe(&s).unwrap_err(),
        "NNTP: authentication unavailable"
    );
    for id in [
        "a@b\r\nPOST",
        "<article@fixture.test>",
        "missing",
        "a@b c",
        "a@b@c",
    ] {
        assert!(nntp::body(&p.server(false), id).is_err());
    }
    assert_eq!(p.connections.load(Ordering::Acquire), 0);
}
#[test]
fn malformed_greetings_and_statuses_are_rejected_without_raw_server_errors() {
    let p = Provider::open();
    for greeting in [
        b"500 private error\r\n".as_slice(),
        b"200 invalid\n",
        b"200 bad\rvalue\r\n",
        b"200 bad\x00value\r\n",
        b"abc invalid\r\n",
    ] {
        p.behavior.lock().unwrap().greeting = greeting.to_vec();
        let e = nntp::probe(&p.server(false)).unwrap_err();
        assert!(!e.contains("private") && !e.contains("bad"));
    }
    p.behavior.lock().unwrap().greeting = format!("200 {}\r\n", "x".repeat(513)).into_bytes();
    assert!(nntp::probe(&p.server(false)).is_err());
    assert_eq!(p.bodies.load(Ordering::Acquire), 0);
}
#[test]
fn wrong_body_identity_unavailable_articles_and_truncated_or_bad_dot_bodies_fail() {
    let p = Provider::open();
    for reply in [
        b"430 private unavailable\r\n".as_slice(),
        b"222 0 <foreign@fixture.test> body follows\r\n",
        b"222 abc <article@fixture.test> body follows\r\n",
    ] {
        p.behavior.lock().unwrap().body_reply = reply.to_vec();
        let e = nntp::body(&p.server(false), "article@fixture.test").unwrap_err();
        assert!(!e.contains("private") && !e.contains("foreign"));
    }
    p.behavior.lock().unwrap().body_reply = Behavior::default().body_reply;
    p.behavior.lock().unwrap().end = false;
    assert!(nntp::body(&p.server(false), "article@fixture.test").is_err());
    p.behavior.lock().unwrap().end = true;
    p.behavior.lock().unwrap().body = b".unescaped\r\n".to_vec();
    assert!(nntp::body(&p.server(false), "article@fixture.test").is_err());
    p.behavior.lock().unwrap().body = b"bad\x00byte\r\n".to_vec();
    assert!(nntp::body(&p.server(false), "article@fixture.test").is_err());
}
#[test]
fn body_limits_and_absolute_deadline_reject_oversized_or_stalled_providers() {
    let p = Provider::open();
    let mut v = p.json(false);
    v.insert("max_article_bytes", 5_u32);
    assert!(
        nntp::body(
            &usenet::Server::from_json(&v).unwrap(),
            "article@fixture.test"
        )
        .is_err()
    );
    p.behavior.lock().unwrap().body = vec![b'x'; 65537];
    assert!(nntp::body(&p.server(false), "article@fixture.test").is_err());
    p.blocked.store(true, Ordering::Release);
    let mut v = p.json(false);
    v.insert("timeout_ms", 100_u32);
    let start = Instant::now();
    assert!(nntp::probe(&usenet::Server::from_json(&v).unwrap()).is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    p.blocked.store(false, Ordering::Release);
}
#[test]
fn concurrent_server_requests_fail_busy_without_duplicate_connections() {
    let p = Provider::open();
    let s = p.server(false);
    p.blocked.store(true, Ordering::Release);
    let copy = s.clone();
    let worker = thread::spawn(move || nntp::probe(&copy));
    let until = Instant::now() + Duration::from_secs(2);
    while p.connections.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(nntp::probe(&s).unwrap_err(), "NNTP: server is busy");
    assert_eq!(p.connections.load(Ordering::Acquire), 1);
    p.blocked.store(false, Ordering::Release);
    worker.join().unwrap().unwrap();
}
#[test]
fn verified_tls_never_falls_back_to_plaintext_and_remote_cleartext_is_rejected() {
    let p = Provider::open();
    p.behavior.lock().unwrap().tls_reject = true;
    let mut v = p.json(true);
    v.insert("tls", true);
    let s = usenet::Server::from_json(&v).unwrap();
    assert_eq!(nntp::probe(&s).unwrap_err(), "NNTP: verified TLS failed");
    assert_eq!(p.users.load(Ordering::Acquire), 0);
    assert_eq!(p.passwords.load(Ordering::Acquire), 0);
    let mut v = p.json(false);
    v.insert("host", "localhost");
    assert!(usenet::Server::from_json(&v).is_err());
    v.insert("host", "192.0.2.1");
    assert!(usenet::Server::from_json(&v).is_err());
}
#[test]
fn strict_settings_defaults_and_public_debug_keep_endpoints_and_credentials_private() {
    let p = Provider::open();
    let mut minimal = Value::object();
    minimal.insert("id", "provider");
    minimal.insert("host", "nntp.example.test");
    let server = usenet::Server::from_json(&minimal).unwrap();
    assert!(!format!("{server:?}").contains("example.test"));
    let cases: [(&str, Value); 7] = [
        ("host", "http://example.test".into()),
        ("host", "bad host".into()),
        ("port", 0_u32.into()),
        ("timeout_ms", 99_u32.into()),
        ("tls", "yes".into()),
        ("username_env", "bad-name".into()),
        ("extra", true.into()),
    ];
    for (key, value) in cases {
        let mut v = p.json(true);
        v.insert(key, value);
        assert!(usenet::Server::from_json(&v).is_err());
    }
    let mut v = p.json(false);
    v.insert("username_env", "PWD");
    assert!(usenet::Server::from_json(&v).is_err());
    let mut v = Value::object();
    v.insert("servers", Value::Array(vec![p.json(false), p.json(false)]));
    assert!(usenet::Settings::from_json(Some(&v)).is_err());
    v.insert(
        "servers",
        Value::Array(
            (0..9)
                .map(|i| {
                    let mut v = p.json(false);
                    v.insert("id", format!("provider{i}"));
                    v
                })
                .collect(),
        ),
    );
    assert!(usenet::Settings::from_json(Some(&v)).is_err());
    assert!(
        usenet::Settings::from_json(None)
            .unwrap()
            .servers
            .is_empty()
    );
}
#[test]
fn probe_preview_replay_restart_and_read_only_gates_never_admit_article_work() {
    let d = Directory::new();
    let p = Provider::open();
    let engine = mynou::engine::Engine::open_for_management(configuration(&d, &p)).unwrap();
    let mut q = ProbeRequest {
        apply: false,
        plan_id: None,
    };
    let before = fs::read(engine.config.store_dir.join("journal.bin")).unwrap();
    let preview = engine.usenet_probe("provider", &q).unwrap();
    private(&preview);
    assert_eq!(p.connections.load(Ordering::Acquire), 0);
    assert_eq!(
        fs::read(engine.config.store_dir.join("journal.bin")).unwrap(),
        before
    );
    q.apply = true;
    q.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    assert_eq!(
        engine
            .usenet_probe("provider", &q)
            .unwrap()
            .get("probe_success")
            .unwrap()
            .as_bool(),
        Some(true)
    );
    assert!(engine.usenet_probe("provider", &q).is_err());
    private(&engine.usenet_servers());
    assert!(engine.store.lock().unwrap().list().is_empty());
    drop(engine);
    let engine = mynou::engine::Engine::open_for_management(configuration(&d, &p)).unwrap();
    assert!(engine.usenet_probe("provider", &q).is_err());
    drop(engine);
    let preview = mynou::engine::Engine::open_for_preview(configuration(&d, &p)).unwrap();
    let mut fresh = ProbeRequest {
        apply: false,
        plan_id: None,
    };
    fresh.plan_id = Some(
        preview
            .usenet_probe("provider", &fresh)
            .unwrap()
            .get("plan_id")
            .unwrap()
            .as_str()
            .unwrap()
            .into(),
    );
    fresh.apply = true;
    assert!(preview.usenet_probe("provider", &fresh).is_err());
    assert_eq!(p.connections.load(Ordering::Acquire), 1);
    assert_eq!(p.bodies.load(Ordering::Acquire), 0);
}
#[test]
fn guarded_api_browser_and_cli_probes_are_authenticated_bound_and_private() {
    let d = Directory::new();
    let p = Provider::open();
    let server = Server::open(configuration(&d, &p));
    let route = "/api/usenet/provider/probe";
    assert_eq!(server.call("GET", "/api/usenet", &[], "").status, 401);
    assert_eq!(
        server
            .call("POST", route, &[("Content-Type", "application/json")], "{}")
            .status,
        401
    );
    let authorized = [("Authorization", format!("Bearer {TOKEN}"))];
    let reply = server.call(
        "GET",
        "/api/usenet",
        &[(authorized[0].0, &authorized[0].1)],
        "",
    );
    assert_eq!(reply.status, 200);
    private(&json::parse(&reply.body).unwrap());
    let browser = Browser::login(&server);
    let other = Browser::login(&server);
    let page = browser.get(&server, "/ui/usenet");
    assert_eq!(page.status, 200);
    page.no_secrets();
    assert!(!page.body.contains("127.0.0.1"));
    let review = browser.post(&server, "/ui/usenet/probe", &[("id", "provider")]);
    assert_eq!(review.status, 200, "{}", review.body);
    assert_eq!(p.connections.load(Ordering::Acquire), 0);
    let plan = review
        .body
        .split("name=\"plan_id\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let fields = [("id", "provider"), ("apply", "yes"), ("plan_id", plan)];
    assert_eq!(other.post(&server, "/ui/usenet/probe", &fields).status, 400);
    assert_eq!(
        browser
            .raw_post(&server, "/ui/usenet/probe", &web_support::fields(&fields))
            .status,
        403
    );
    assert_eq!(
        browser.post(&server, "/ui/usenet/probe", &fields).status,
        303
    );
    assert_eq!(
        browser.post(&server, "/ui/usenet/probe", &fields).status,
        400
    );
    let mut v = config_value(&d, &p);
    v.insert("listen", server.authority.clone());
    let path = d.0.join("mynou.json");
    fs::write(&path, json::stringify(&v)).unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_mynou"))
            .env("MYNOU_API_TOKEN", TOKEN)
            .args(["usenet-probe", "provider", "--config"])
            .arg(&path)
            .args(extra)
            .output()
            .unwrap()
    };
    let out = run(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap();
    private(&v);
    let plan = v.get("plan_id").unwrap().as_str().unwrap();
    assert!(run(&["--apply", "--plan-id", plan]).status.success());
    assert!(!run(&["--apply", "--plan-id", plan]).status.success());
    assert_eq!(p.bodies.load(Ordering::Acquire), 0);
}
