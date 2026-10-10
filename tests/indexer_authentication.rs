//! Original native HTTP source protocol fixtures. Never public trackers or torrents.
mod library_support;
mod web_support;
use library_support::Directory;
use mynou::{config, integrations, json::{self, Value}, store::Request};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    net::TcpListener,
    process::Command,
    sync::{
        Arc,
        Mutex,
        atomic::{
            AtomicBool,
            AtomicUsize,
            Ordering,
        },
    },
    thread::{
        self,
        JoinHandle,
    },
    time::{
        Duration,
        Instant,
    },
};
use web_support::{Server, TOKEN};
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: String,
    body: String,
}
struct Provider {
    url: String,
    mode: Arc<Mutex<String>>,
    feed: Arc<Mutex<Reply>>,
    cookie: Arc<Mutex<String>>,
    logins: Arc<AtomicUsize>,
    searches: Arc<AtomicUsize>,
    reject: Arc<AtomicUsize>,
    blocked: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
fn decode64(s: &str) -> Vec<u8> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut acc = 0_u32;
    let mut bits = 0;
    let mut out = Vec::new();
    for b in s.bytes().take_while(|b| *b != b'=') {
        let n = alphabet.iter().position(|a| *a == b).unwrap() as u32;
        acc = (acc << 6) | n;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}
fn encoded(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char)
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
impl Provider {
    fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let feed=Arc::new(Mutex::new(Reply {status:200,headers:String::new(),body:r#"[{"title":"Fixture.Movie.2024.1080p","seeders":5,"url":"magnet:?xt=urn:btih:original-fixture"}]"#.into()}));
        let mode = Arc::new(Mutex::new("none".to_owned()));
        let cookie = Arc::new(Mutex::new(
            "sid=private-session-fixture; Path=/feed; Max-Age=60; HttpOnly".to_owned(),
        ));
        let logins = Arc::new(AtomicUsize::new(0));
        let searches = Arc::new(AtomicUsize::new(0));
        let reject = Arc::new(AtomicUsize::new(0));
        let blocked = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (f, m, c, l, q, r, b, s) = (
            feed.clone(),
            mode.clone(),
            cookie.clone(),
            logins.clone(),
            searches.clone(),
            reject.clone(),
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
                    Err(e) => panic!("Source fixture accept failed: {e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 16384);
                    let mut b = [0];
                    stream.read_exact(&mut b).unwrap();
                    head.push(b[0]);
                }
                let head = String::from_utf8(head).unwrap();
                let fields: BTreeMap<_, _> = head
                    .lines()
                    .skip(1)
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                let len = fields
                    .get("content-length")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                assert!(len < 32768);
                let mut body = vec![0; len];
                stream.read_exact(&mut body).unwrap();
                let result = if head.starts_with("POST /login ") {
                    assert_eq!(fields["content-type"], "application/x-www-form-urlencoded");
                    let expected = format!(
                        "username={}&password={}",
                        encoded(&std::env::var("PWD").unwrap()),
                        encoded(&std::env::var("PATH").unwrap())
                    );
                    assert!(
                        body == expected.as_bytes(),
                        "Source form credential mismatch"
                    );
                    l.fetch_add(1, Ordering::AcqRel);
                    Reply {
                        status: 200,
                        headers: format!("Set-Cookie: {}\r\n", c.lock().unwrap()),
                        body: String::new(),
                    }
                } else {
                    assert!(head.starts_with("GET /feed?"));
                    q.fetch_add(1, Ordering::AcqRel);
                    let mode = m.lock().unwrap().clone();
                    let authorized = match mode.as_str() {
                        "basic" => fields
                            .get("authorization")
                            .and_then(|v| v.strip_prefix("Basic "))
                            .is_some_and(|v| {
                                decode64(v)
                                    == format!(
                                        "{}:{}",
                                        std::env::var("PWD").unwrap(),
                                        std::env::var("PATH").unwrap()
                                    )
                                    .as_bytes()
                            }),
                        "bearer" => fields.get("authorization").is_some_and(|v| {
                            v == &format!("Bearer {}", std::env::var("PWD").unwrap())
                        }),
                        "form" => fields
                            .get("cookie")
                            .is_some_and(|v| v == "sid=private-session-fixture"),
                        _ => {
                            !fields.contains_key("authorization") && !fields.contains_key("cookie")
                        }
                    };
                    assert!(authorized, "Source credential or session mismatch");
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while b.load(Ordering::Acquire) && !s.load(Ordering::Acquire) {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(2));
                    }
                    if r.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                        .is_ok()
                    {
                        Reply {
                            status: 401,
                            headers: String::new(),
                            body: String::new(),
                        }
                    } else {
                        f.lock().unwrap().clone()
                    }
                };
                write!(
                    stream,
                    "HTTP/1.1 {} Fixture\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    result.status,
                    result.headers,
                    result.body.len(),
                    result.body
                )
                .unwrap();
            }
        });
        Self {
            url,
            mode,
            feed,
            cookie,
            logins,
            searches,
            reject,
            blocked,
            stop,
            thread: Some(t),
        }
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.blocked.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}
fn movie() -> Request {
    Request {
        kind: "movie".into(),
        title: "Fixture Movie".into(),
        year: 2024,
        season: 0,
        episode: 0,
        tmdb_id: Some(7),
        source_numbering: None,
        source_path: None,
        source_url: None,
    }
}
fn auth(provider: &Provider, mode: &str) -> Value {
    let mut a = Value::object();
    a.insert("method", mode);
    match mode {
        "basic" => {
            a.insert("username_env", "PWD");
            a.insert("password_env", "PATH");
        }
        "bearer" => a.insert("token_env", "PWD"),
        "form" => {
            a.insert("login_url", format!("{}/login", provider.url));
            a.insert("username_env", "PWD");
            a.insert("password_env", "PATH");
            a.insert("username_field", "username");
            a.insert("password_field", "password");
            a.insert("cookie_name", "sid");
            a.insert("max_age_secs", 60_u32);
        }
        _ => {}
    }
    a
}
fn value(dir: &Directory, p: &Provider, mode: &str) -> Value {
    let mut v = config::default_json();
    v.insert("listen", "127.0.0.1:0");
    v.insert("store_dir", dir.0.join("jobs").to_str().unwrap());
    v.get_mut("downloads").unwrap().insert("enabled", false);
    let mut s = Value::object();
    s.insert("name", "fixture");
    s.insert("kind", "json");
    s.insert("url", format!("{}/feed", p.url));
    s.insert("api_key_env", "MYNOU_ABSENT_SOURCE_FIXTURE_KEY_28342");
    s.insert("authentication", auth(p, mode));
    v.insert("indexers", Value::Array(vec![s]));
    v
}
fn cfg(dir: &Directory, p: &Provider, mode: &str) -> config::Config {
    *p.mode.lock().unwrap() = mode.into();
    config::from_json(&value(dir, p, mode), &dir.0).unwrap()
}
fn source_mut(v: &mut Value) -> &mut Value {
    let Value::Array(s) = v.get_mut("indexers").unwrap() else {
        panic!()
    };
    &mut s[0]
}
fn health(c: &config::Config) -> Value {
    mynou::indexers::report(&c.sources)
        .get("sources")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .clone()
}
fn redacted(v: &Value) {
    let s = json::stringify(v);
    for private in [
        "http://",
        "https://",
        "token_env",
        "username_env",
        "password_env",
        "cookie_name",
        "private-session-fixture",
    ] {
        assert!(!s.contains(private), "Source report exposed private data");
    }
}
#[test]
fn native_basic_and_bearer_headers_keep_credentials_out_of_results_and_health() {
    for mode in ["basic", "bearer"] {
        let d = Directory::new();
        let p = Provider::open();
        let c = cfg(&d, &p, mode);
        let result = integrations::search_report(&c, &movie()).unwrap();
        assert_eq!(p.searches.load(Ordering::Acquire), 1);
        redacted(&result);
        let h = health(&c);
        assert_eq!(h.get("successes").unwrap().as_str(), Some("1"));
        assert_eq!(h.get("last_parse"), Some(&Value::Bool(true)));
        redacted(&h);
    }
}
#[test]
fn configured_form_session_is_shared_by_clones_and_renews_once_after_401() {
    let d = Directory::new();
    let p = Provider::open();
    let c = cfg(&d, &p, "form");
    integrations::search(&c, &movie()).unwrap();
    integrations::search(&c.clone(), &movie()).unwrap();
    assert_eq!(p.logins.load(Ordering::Acquire), 1);
    p.reject.store(1, Ordering::Release);
    integrations::search(&c, &movie()).unwrap();
    assert_eq!(p.logins.load(Ordering::Acquire), 2);
    assert_eq!(p.searches.load(Ordering::Acquire), 4);
    redacted(&health(&c));
    assert!(!format!("{:?}", c.sources[0].options).contains("private-session-fixture"));
}
#[test]
fn repeated_unauthorized_session_response_stops_without_unauthenticated_fallback() {
    let d = Directory::new();
    let p = Provider::open();
    let c = cfg(&d, &p, "form");
    p.reject.store(10, Ordering::Release);
    assert!(integrations::search(&c, &movie()).is_err());
    assert_eq!(p.logins.load(Ordering::Acquire), 2);
    assert_eq!(p.searches.load(Ordering::Acquire), 2);
    assert_eq!(
        health(&c).get("last_error").unwrap().as_str(),
        Some("authentication_rejected")
    );
    assert_eq!(health(&c).get("session_active"), Some(&Value::Bool(false)));
}
#[test]
fn invalid_cookie_claims_stop_before_any_authenticated_search() {
    for cookie in [
        "other=untrusted",
        "sid=value; Domain=wrong.test",
        "sid=value; Path=/wrong",
        "sid=value; Max-Age=0",
        "sid=value, other=bad",
        "sid=value; Path=/feed; Path=/",
        "sid=value; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
    ] {
        let d = Directory::new();
        let p = Provider::open();
        *p.cookie.lock().unwrap() = cookie.into();
        let c = cfg(&d, &p, "form");
        assert!(integrations::search(&c, &movie()).is_err());
        assert_eq!(p.searches.load(Ordering::Acquire), 0);
        redacted(&health(&c));
    }
}
#[test]
fn native_session_expiry_requires_a_new_login_before_the_next_fetch() {
    let d = Directory::new();
    let p = Provider::open();
    let mut v = value(&d, &p, "form");
    source_mut(&mut v)
        .get_mut("authentication")
        .unwrap()
        .insert("max_age_secs", 1_u32);
    *p.mode.lock().unwrap() = "form".into();
    let c = config::from_json(&v, &d.0).unwrap();
    integrations::search(&c, &movie()).unwrap();
    thread::sleep(Duration::from_millis(1050));
    assert_eq!(health(&c).get("session_active"), Some(&Value::Bool(false)));
    integrations::search(&c, &movie()).unwrap();
    assert_eq!(p.logins.load(Ordering::Acquire), 2);
}
#[test]
fn strict_source_authentication_configuration_rejects_remote_plaintext_and_invalid_scope() {
    let d = Directory::new();
    let p = Provider::open();
    let original = value(&d, &p, "form");
    for (key, val) in [
        ("login_url", Value::from("http://127.0.0.1:2/login")),
        ("login_url", format!("{}/login?token=private", p.url).into()),
        ("cookie_name", "bad;value".into()),
        ("username_field", "password".into()),
        ("max_age_secs", 0_u32.into()),
        ("max_age_secs", 3601_u32.into()),
        ("username_env", "invalid name".into()),
        ("extra", true.into()),
    ] {
        let mut v = original.clone();
        source_mut(&mut v)
            .get_mut("authentication")
            .unwrap()
            .insert(key, val);
        assert!(config::from_json(&v, &d.0).is_err());
    }
    let mut v = value(&d, &p, "basic");
    source_mut(&mut v).insert("url", "http://example.test/feed");
    assert!(config::from_json(&v, &d.0).is_err());
    let mut v = value(&d, &p, "none");
    source_mut(&mut v).insert("min_interval_ms", 60001_u32);
    assert!(config::from_json(&v, &d.0).is_err());
}
#[test]
fn rate_limits_and_disabled_sources_reject_without_repeated_network_calls() {
    let d = Directory::new();
    let p = Provider::open();
    let mut v = value(&d, &p, "none");
    source_mut(&mut v).insert("min_interval_ms", 60_000_u32);
    let c = config::from_json(&v, &d.0).unwrap();
    integrations::search(&c, &movie()).unwrap();
    assert!(integrations::search(&c, &movie()).is_err());
    assert_eq!(p.searches.load(Ordering::Acquire), 1);
    assert_eq!(
        health(&c).get("last_error").unwrap().as_str(),
        Some("rate_limited")
    );
    let mut v = value(&d, &p, "none");
    source_mut(&mut v).insert("enabled", false);
    let c = config::from_json(&v, &d.0).unwrap();
    assert!(integrations::search(&c, &movie()).is_err());
    assert_eq!(p.searches.load(Ordering::Acquire), 1);
    assert_eq!(
        health(&c).get("last_error").unwrap().as_str(),
        Some("disabled")
    );
}
#[test]
fn numeric_retry_after_stops_immediate_fetches_and_other_sources_remain_available() {
    let d = Directory::new();
    let p = Provider::open();
    let healthy = Provider::open();
    *p.feed.lock().unwrap() = Reply {
        status: 429,
        headers: "Retry-After: 120\r\n".into(),
        body: String::new(),
    };
    let mut c = cfg(&d, &p, "none");
    c.sources.push(cfg(&d, &healthy, "none").sources.remove(0));
    integrations::search(&c, &movie()).unwrap();
    integrations::search(&c, &movie()).unwrap();
    assert_eq!(p.searches.load(Ordering::Acquire), 1);
    assert_eq!(healthy.searches.load(Ordering::Acquire), 2);
    assert_eq!(health(&c).get("last_status").unwrap().as_u64(), Some(429));
    redacted(&mynou::indexers::report(&c.sources));
}
#[test]
fn redirects_and_missing_or_invalid_credentials_never_reach_another_origin() {
    let d = Directory::new();
    let p = Provider::open();
    let other = Provider::open();
    *p.feed.lock().unwrap() = Reply {
        status: 307,
        headers: format!("Location: {}/feed\r\n", other.url),
        body: String::new(),
    };
    let c = cfg(&d, &p, "basic");
    assert!(integrations::search(&c, &movie()).is_err());
    assert_eq!(other.searches.load(Ordering::Acquire), 0);
    redacted(&health(&c));
    for env in ["MYNOU_ABSENT_NATIVE_AUTH_FIXTURE_7853", "PATH"] {
        let mut v = value(&d, &p, "bearer");
        source_mut(&mut v)
            .get_mut("authentication")
            .unwrap()
            .insert("token_env", env);
        let c = config::from_json(&v, &d.0).unwrap();
        assert!(integrations::search(&c, &movie()).is_err());
        assert_eq!(p.searches.load(Ordering::Acquire), 1);
        redacted(&health(&c));
    }
}
#[test]
fn concurrent_source_fetch_is_busy_without_waiting_or_duplicate_login() {
    let d = Directory::new();
    let p = Provider::open();
    let c = cfg(&d, &p, "form");
    p.blocked.store(true, Ordering::Release);
    let copy = c.clone();
    let worker = thread::spawn(move || integrations::search(&copy, &movie()));
    let deadline = Instant::now() + Duration::from_secs(2);
    while p.searches.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(health(&c).get("busy"), Some(&Value::Bool(true)));
    let start = Instant::now();
    assert!(integrations::search(&c, &movie()).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    p.blocked.store(false, Ordering::Release);
    assert!(worker.join().unwrap().is_ok());
    assert_eq!(p.logins.load(Ordering::Acquire), 1);
    assert_eq!(p.searches.load(Ordering::Acquire), 1);
}
#[test]
fn native_rss_and_torznab_parsers_use_authenticated_fetch_and_report_parse_failures() {
    for kind in ["rss", "torznab"] {
        let d = Directory::new();
        let p = Provider::open();
        p.feed.lock().unwrap().body=r#"<rss xmlns:torznab="urn:original"><channel><item><title>Fixture.Movie.2024.1080p</title><torznab:attr name="seeders" value="5"/><torznab:attr name="magneturl" value="magnet:?xt=urn:btih:original-fixture"/></item></channel></rss>"#.into();
        let mut v = value(&d, &p, "basic");
        source_mut(&mut v).insert("kind", kind);
        *p.mode.lock().unwrap() = "basic".into();
        let c = config::from_json(&v, &d.0).unwrap();
        assert!(integrations::search(&c, &movie()).is_ok());
        assert_eq!(health(&c).get("last_parse"), Some(&Value::Bool(true)));
        p.feed.lock().unwrap().body = "<!DOCTYPE invalid><rss>".into();
        assert!(integrations::search(&c, &movie()).is_err());
        assert_eq!(
            health(&c).get("last_error").unwrap().as_str(),
            Some("parse_rejected")
        );
        redacted(&health(&c));
    }
}

fn policy_id(engine: &mynou::engine::Engine) -> String {
    engine
        .indexers()
        .unwrap()
        .get("sources")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .get("id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}
fn control(action: &str) -> mynou::indexers::ControlRequest {
    mynou::indexers::ControlRequest {
        action: action.into(),
        apply: false,
        plan_id: None,
    }
}
fn reviewed(
    engine: &mynou::engine::Engine,
    id: &str,
    action: &str,
) -> mynou::indexers::ControlRequest {
    let mut q = control(action);
    q.plan_id = Some(
        engine
            .indexer_control(id, &q)
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
#[test]
fn guarded_pause_enable_persists_and_previews_do_not_search_or_write() {
    let d = Directory::new();
    let p = Provider::open();
    let c = cfg(&d, &p, "none");
    let engine = mynou::engine::Engine::open_for_management(c.clone()).unwrap();
    let id = policy_id(&engine);
    let path = c.store_dir.join("indexers.bin");
    let before = fs::read(&path).unwrap();
    let pause = reviewed(&engine, &id, "pause");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(p.searches.load(Ordering::Acquire), 0);
    engine.indexer_control(&id, &pause).unwrap();
    assert!(integrations::search(&c, &movie()).is_err());
    assert_eq!(p.searches.load(Ordering::Acquire), 0);
    drop(engine);
    let fresh = cfg(&d, &p, "none");
    let engine = mynou::engine::Engine::open_for_management(fresh.clone()).unwrap();
    assert_eq!(
        engine
            .indexers()
            .unwrap()
            .get("sources")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("enabled"),
        Some(&Value::Bool(false))
    );
    assert!(integrations::search(&fresh, &movie()).is_err());
    let enable = reviewed(&engine, &id, "enable");
    engine.indexer_control(&id, &enable).unwrap();
    assert!(integrations::search(&fresh, &movie()).is_ok());
    assert_eq!(p.searches.load(Ordering::Acquire), 1);
}
#[test]
fn guards_cover_other_source_changes_and_cannot_be_replayed() {
    let d = Directory::new();
    let p = Provider::open();
    let mut v = value(&d, &p, "none");
    source_mut(&mut v).insert("id", "primary");
    let mut second = source_mut(&mut v).clone();
    second.insert("id", "secondary");
    second.insert("name", "other");
    let Value::Array(a) = v.get_mut("indexers").unwrap() else {
        panic!()
    };
    a.push(second);
    let engine =
        mynou::engine::Engine::open_for_management(config::from_json(&v, &d.0).unwrap()).unwrap();
    let old = reviewed(&engine, "primary", "pause");
    let reset = reviewed(&engine, "secondary", "reset_session");
    engine.indexer_control("secondary", &reset).unwrap();
    assert!(
        engine
            .indexer_control("primary", &old)
            .unwrap_err()
            .contains("stale")
    );
    let reset = reviewed(&engine, "primary", "reset_session");
    engine.indexer_control("primary", &reset).unwrap();
    assert!(
        engine
            .indexer_control("primary", &reset)
            .unwrap_err()
            .contains("stale")
    );
    assert_eq!(p.searches.load(Ordering::Acquire), 0);
}
#[test]
fn reset_discards_ephemeral_sessions_and_keeps_snapshot_secret_free() {
    let d = Directory::new();
    let p = Provider::open();
    let c = cfg(&d, &p, "form");
    let engine = mynou::engine::Engine::open_for_management(c.clone()).unwrap();
    let id = policy_id(&engine);
    integrations::search(&c, &movie()).unwrap();
    assert_eq!(health(&c).get("session_active"), Some(&Value::Bool(true)));
    let q = reviewed(&engine, &id, "reset_session");
    engine.indexer_control(&id, &q).unwrap();
    assert_eq!(health(&c).get("session_active"), Some(&Value::Bool(false)));
    integrations::search(&c, &movie()).unwrap();
    assert_eq!(p.logins.load(Ordering::Acquire), 2);
    let bytes = fs::read(c.store_dir.join("indexers.bin")).unwrap();
    let payload = std::str::from_utf8(&bytes[16..bytes.len() - 32]).unwrap();
    redacted(&json::parse(payload).unwrap());
    assert!(!payload.contains("PWD") && !payload.contains("PATH"));
}
#[test]
fn policy_changes_fence_inflight_responses_without_waiting_for_provider() {
    let d = Directory::new();
    let p = Provider::open();
    let c = cfg(&d, &p, "none");
    let engine = mynou::engine::Engine::open_for_management(c.clone()).unwrap();
    let id = policy_id(&engine);
    let q = reviewed(&engine, &id, "pause");
    p.blocked.store(true, Ordering::Release);
    let worker = thread::spawn(move || integrations::search(&c, &movie()));
    let deadline = Instant::now() + Duration::from_secs(2);
    while p.searches.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    let start = Instant::now();
    engine.indexer_control(&id, &q).unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    p.blocked.store(false, Ordering::Release);
    assert!(worker.join().unwrap().is_err());
    let row = engine
        .indexers()
        .unwrap()
        .get("sources")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .clone();
    assert_eq!(
        row.get("last_error").unwrap().as_str(),
        Some("policy_changed")
    );
}
#[test]
fn probes_require_review_make_one_source_search_and_never_queue_media() {
    let d = Directory::new();
    let p = Provider::open();
    let c = cfg(&d, &p, "none");
    let engine = mynou::engine::Engine::open_for_management(c).unwrap();
    let id = policy_id(&engine);
    let q = reviewed(&engine, &id, "probe");
    assert_eq!(p.searches.load(Ordering::Acquire), 0);
    let r = engine.indexer_control(&id, &q).unwrap();
    redacted(&r);
    assert_eq!(r.get("probe_success"), Some(&Value::Bool(true)));
    assert_eq!(p.searches.load(Ordering::Acquire), 1);
    assert!(engine.indexer_control(&id, &q).is_err());
    assert!(engine.store.lock().unwrap().list().is_empty());
    p.feed.lock().unwrap().body = "invalid-json".into();
    let q = reviewed(&engine, &id, "probe");
    assert_eq!(
        engine
            .indexer_control(&id, &q)
            .unwrap()
            .get("probe_success"),
        Some(&Value::Bool(false))
    );
    let q = reviewed(&engine, &id, "pause");
    engine.indexer_control(&id, &q).unwrap();
    assert!(engine.indexer_control(&id, &control("probe")).is_err());
    assert_eq!(p.searches.load(Ordering::Acquire), 2);
}
#[test]
fn stable_source_bindings_cannot_change_and_removed_sources_keep_policy() {
    let d = Directory::new();
    let p = Provider::open();
    let mut v = value(&d, &p, "none");
    source_mut(&mut v).insert("id", "primary");
    let c = config::from_json(&v, &d.0).unwrap();
    let engine = mynou::engine::Engine::open_for_management(c.clone()).unwrap();
    let pause = reviewed(&engine, "primary", "pause");
    engine.indexer_control("primary", &pause).unwrap();
    drop(engine);
    let original = fs::read(c.store_dir.join("indexers.bin")).unwrap();
    let mut bad = v.clone();
    source_mut(&mut bad).insert("min_interval_ms", 100_u32);
    assert!(
        mynou::engine::Engine::open_for_management(config::from_json(&bad, &d.0).unwrap()).is_err()
    );
    assert_eq!(
        fs::read(c.store_dir.join("indexers.bin")).unwrap(),
        original
    );
    let mut removed = v.clone();
    removed.insert("indexers", Value::Array(Vec::new()));
    let engine =
        mynou::engine::Engine::open_for_management(config::from_json(&removed, &d.0).unwrap())
            .unwrap();
    assert!(
        engine
            .indexer_control("primary", &control("enable"))
            .is_err()
    );
    drop(engine);
    let engine =
        mynou::engine::Engine::open_for_management(config::from_json(&v, &d.0).unwrap()).unwrap();
    assert_eq!(
        engine
            .indexers()
            .unwrap()
            .get("sources")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("enabled"),
        Some(&Value::Bool(false))
    );
}
#[test]
fn duplicate_ids_and_corrupt_source_snapshots_fail_closed_before_mutation() {
    let d = Directory::new();
    let p = Provider::open();
    let mut v = value(&d, &p, "none");
    source_mut(&mut v).insert("id", "primary");
    let c = config::from_json(&v, &d.0).unwrap();
    let engine = mynou::engine::Engine::open_for_management(c.clone()).unwrap();
    drop(engine);
    let path = c.store_dir.join("indexers.bin");
    let original = fs::read(&path).unwrap();
    let row = source_mut(&mut v).clone();
    let Value::Array(a) = v.get_mut("indexers").unwrap() else {
        panic!()
    };
    a.push(row);
    assert!(
        mynou::engine::Engine::open_for_management(config::from_json(&v, &d.0).unwrap()).is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    for index in [0, 8, 25, original.len() - 1] {
        let mut bad = original.clone();
        bad[index] ^= 1;
        fs::write(&path, &bad).unwrap();
        assert!(mynou::engine::Engine::open_for_preview(c.clone()).is_err());
        assert_eq!(fs::read(&path).unwrap(), bad);
    }
}
#[cfg(unix)]
#[test]
fn source_snapshot_symlinks_hardlinks_and_public_permissions_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let d = Directory::new();
    let p = Provider::open();
    let c = cfg(&d, &p, "none");
    drop(mynou::engine::Engine::open_for_management(c.clone()).unwrap());
    let path = c.store_dir.join("indexers.bin");
    let original = fs::read(&path).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    let target = d.0.join("outside");
    fs::rename(&path, &target).unwrap();
    symlink(&target, &path).unwrap();
    assert!(mynou::engine::Engine::open_for_preview(c.clone()).is_err());
    assert_eq!(fs::read(&target).unwrap(), original);
    fs::remove_file(&path).unwrap();
    fs::rename(&target, &path).unwrap();
    fs::hard_link(&path, &target).unwrap();
    assert!(mynou::engine::Engine::open_for_preview(c.clone()).is_err());
    fs::remove_file(&target).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(mynou::engine::Engine::open_for_preview(c).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
}
#[test]
fn offline_source_previews_never_create_missing_snapshot_or_apply_control() {
    let d = Directory::new();
    let p = Provider::open();
    let c = cfg(&d, &p, "none");
    drop(mynou::engine::Engine::open_for_management(c.clone()).unwrap());
    let path = c.store_dir.join("indexers.bin");
    fs::remove_file(&path).unwrap();
    let preview = mynou::engine::Engine::open_for_preview(c).unwrap();
    let id = policy_id(&preview);
    let q = reviewed(&preview, &id, "pause");
    assert!(preview.indexer_control(&id, &q).is_err());
    assert!(!path.exists());
    assert_eq!(p.searches.load(Ordering::Acquire), 0);
}
#[test]
fn cli_source_control_uses_online_guard_and_offline_persisted_policy() {
    let d = Directory::new();
    let p = Provider::open();
    let server = Server::open(cfg(&d, &p, "none"));
    let id = policy_id(&server.engine);
    let mut v = value(&d, &p, "none");
    v.insert("listen", server.authority.clone());
    let path = d.0.join("mynou.json");
    fs::write(&path, json::stringify(&v)).unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_mynou"))
            .env("MYNOU_API_TOKEN", TOKEN)
            .args(["indexer-control", &id, "--action", "pause", "--config"])
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
    let report = json::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap();
    redacted(&report);
    let guard = report.get("plan_id").unwrap().as_str().unwrap();
    assert!(run(&["--apply", "--plan-id", guard]).status.success());
    assert!(!run(&["--apply", "--plan-id", guard]).status.success());
    drop(server);
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .env_remove("MYNOU_API_TOKEN")
        .args(["indexers", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = json::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap();
    assert_eq!(
        report.get("sources").unwrap().as_array().unwrap()[0].get("enabled"),
        Some(&Value::Bool(false))
    );
    assert_eq!(p.searches.load(Ordering::Acquire), 0);
}
