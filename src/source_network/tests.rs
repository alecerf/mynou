//! Real discovery protocols over native WireGuard owners; no host service exists.
use super::*;
use crate::{
    config,
    engine::Engine,
    indexers, integrations,
    json::{self, Value},
    net::NetworkStream,
    pack::tunnel_tests::{Directory, accept, deadline},
    torrent::transport::NetworkListener,
    tunnel_http::tests::Pair,
};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{atomic::Ordering, mpsc},
    thread,
    time::{Duration, Instant},
};

fn child(name: &str) -> bool {
    const FLAG: &str = "MYNOU_PROTECTED_DISCOVERY_FIXTURE";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(FLAG, name)
        .env(
            "MYNOU_CA_FILE",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pki/fixtures/rsa-root.pem"),
        )
        .env("MYNOU_FIXTURE_USER", "fixture-user")
        .env("MYNOU_FIXTURE_PASS", "fixture-pass")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("https_proxy", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}
fn source(url: &str, watch: bool) -> Value {
    let mut v = Value::object();
    v.insert("name", "synthetic");
    v.insert("id", "synthetic");
    v.insert("kind", "torznab");
    v.insert("url", url);
    v.insert("api_key_env", "MYNOU_UNSET_DISCOVERY_KEY");
    v.insert("protection", "protected");
    if watch {
        let mut w = Value::object();
        w.insert("enabled", true);
        v.insert("watch", w);
    }
    v
}
fn settings(source: Value) -> Value {
    let mut v = config::default_json();
    v.insert("indexers", Value::Array(vec![source]));
    v.get_mut("downloads").unwrap().insert("enabled", true);
    v
}
fn open(dir: &Directory, pair: &Pair, settings: &Value) -> Arc<Engine> {
    Engine::open_with_network(
        config::from_json(settings, &dir.0).unwrap(),
        pair.client_network.clone(),
        false,
    )
    .unwrap()
}
fn response(body: &[u8], extra: &str) -> Vec<u8> {
    let mut r = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{extra}\r\n",
        body.len()
    )
    .into_bytes();
    r.extend(body);
    r
}
fn request(method: &str, target: &str, port: u16, headers: &str, body: &str) -> Vec<u8> {
    format!("{method} {target} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\nUser-Agent: Mynou/{}\r\nContent-Length: {}\r\nAccept-Encoding: identity\r\n{headers}\r\n{body}", env!("CARGO_PKG_VERSION"), body.len()).into_bytes()
}
fn listener(pair: &Pair) -> Box<dyn NetworkListener> {
    pair.server_network
        .listen("0.0.0.0:0".parse().unwrap())
        .unwrap()
}
fn read_http(stream: &mut dyn NetworkStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut b = [0];
        stream.read_exact(&mut b).unwrap();
        bytes.push(b[0]);
        assert!(bytes.len() < 65536);
    }
    String::from_utf8(bytes).unwrap()
}
fn wait(mut ready: impl FnMut() -> bool) {
    let end = deadline();
    while !ready() {
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(5));
    }
}
const EMPTY: &[u8] = b"<rss><channel></channel></rss>";

#[test]
fn protected_authentication_search_feed_detail_and_metainfo_use_virtual_tls_dns() {
    if child(
        "source_network::tests::protected_authentication_search_feed_detail_and_metainfo_use_virtual_tls_dns",
    ) {
        return;
    }
    let pair = Pair::new();
    let dir = Directory::new();
    let service = listener(&pair);
    let port = service.port();
    let mut s = source(&format!("https://localhost:{port}/api"), true);
    let mut auth = Value::object();
    for (k, v) in [
        ("method", "form"),
        ("username_env", "MYNOU_FIXTURE_USER"),
        ("password_env", "MYNOU_FIXTURE_PASS"),
        ("username_field", "user"),
        ("password_field", "password"),
        ("cookie_name", "sid"),
    ] {
        auth.insert(k, v);
    }
    auth.insert("login_url", format!("https://localhost:{port}/login"));
    s.insert("authentication", auth);
    let engine = open(&dir, &pair, &settings(s));
    let body = "user=fixture-user&password=fixture-pass";
    let login = request(
        "POST",
        "/login",
        port,
        "Content-Type: application/x-www-form-urlencoded\r\n",
        body,
    );
    let feed = request(
        "GET",
        "/api?t=search",
        port,
        "Accept: application/rss+xml, application/xml\r\nCookie: sid=fixture-session\r\n",
        "",
    );
    let detail = request(
        "GET",
        "/api?t=search&q=Fixture",
        port,
        "Accept: application/rss+xml, application/xml\r\nCookie: sid=fixture-session\r\n",
        "",
    );
    let download = request(
        "GET",
        "/download",
        port,
        "Accept: application/x-bittorrent\r\nCookie: sid=fixture-session\r\n",
        "",
    );
    let worker = thread::spawn(move || {
        for (expected, reply) in [(login,response(b"", "Set-Cookie: sid=fixture-session; Path=/; Max-Age=60; Secure\r\n")), (feed.clone(),response(EMPTY,"")),(detail,response(EMPTY,"")),(download,response(b"synthetic-metainfo","")), (feed,b"HTTP/1.1 302 Found\r\nLocation: https://must-not-contact.invalid/\r\nContent-Length: 0\r\n\r\n".to_vec())] {
            let mut stream=accept(&*service);
            crate::tls::tests::serve_dialogue(&mut stream, &[(&expected,&reply)]).unwrap();
            let _ = stream.read(&mut [0]);
        }
    });
    let s = &engine.config.sources[0];
    assert!(
        integrations::poll_feed(s, deadline())
            .unwrap()
            .entries
            .is_empty()
    );
    assert!(
        integrations::torznab_details_before(s, "Fixture", deadline())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        integrations::indexer_metainfo_before(
            s,
            &format!("https://localhost:{port}/download"),
            deadline()
        )
        .unwrap(),
        b"synthetic-metainfo"
    );
    assert!(integrations::poll_feed(s, deadline()).is_err()); // redirects refused, no second origin
    worker.join().unwrap();
    assert!(pair.dns.load(Ordering::Relaxed) >= 5);
    let report = json::stringify(&engine.indexers().unwrap());
    for secret in [
        "fixture-user",
        "fixture-pass",
        "fixture-session",
        "localhost",
        "/login",
    ] {
        assert!(!report.contains(secret));
    }
    assert!(report.contains("\"protection_ready\":true"));
    pair.client.cancel();
    assert!(indexers::fetch(s, &s.url, "text/plain", deadline()).is_err());
    assert!(
        engine
            .indexers()
            .unwrap()
            .get("sources")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("protection_ready")
            == Some(&Value::Bool(false))
    );
}

#[test]
fn protected_feed_cursors_and_pending_demand_survive_restart_and_loss_without_host_io() {
    if child(
        "source_network::tests::protected_feed_cursors_and_pending_demand_survive_restart_and_loss_without_host_io",
    ) {
        return;
    }
    let pair = Pair::new();
    let dir = Directory::new();
    let service = listener(&pair);
    let port = service.port();
    let cfg = settings(source(&format!("http://feed.example:{port}/api"), true));
    let engine = open(&dir, &pair, &cfg);
    let (entered, received) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        for n in 0..3 {
            let mut stream = accept(&*service);
            let request = read_http(&mut *stream);
            assert!(request.starts_with("GET /api?t=search HTTP/1.1"));
            if n == 2 {
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            let _ = stream.write_all(&response(EMPTY, ""));
            let _ = stream.read(&mut [0]);
        }
    });
    assert!(json::stringify(&engine.feeds_poll(None, true).unwrap()).contains("\"baseline\":true"));
    let before = engine.feeds().unwrap();
    drop(engine);
    let engine = open(&dir, &pair, &cfg);
    assert_eq!(
        before.get("sources").unwrap().as_array().unwrap()[0].get("baselined_at"),
        engine
            .feeds()
            .unwrap()
            .get("sources")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("baselined_at")
    );
    assert!(
        json::stringify(&engine.feeds_poll(None, true).unwrap()).contains("\"baseline\":false")
    );
    let pending = crate::store::Request {
        kind: "movie".into(),
        title: "Wanted Fixture".into(),
        year: 2024,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: None,
        tmdb_id: Some(42),
        source_numbering: None,
    };
    let id = engine.submit(pending).unwrap()[0].id.clone();
    let poll = {
        let e = engine.clone();
        thread::spawn(move || e.feeds_poll(None, true))
    };
    received.recv_timeout(Duration::from_secs(10)).unwrap();
    pair.client.cancel();
    release.send(()).unwrap();
    let report = poll.join().unwrap().unwrap();
    assert!(!json::stringify(&report).contains("\"outcome\":\"ok\""));
    worker.join().unwrap();
    assert_eq!(
        crate::engine::lock(&engine.store)
            .unwrap()
            .get(&id)
            .unwrap()
            .unwrap()
            .state,
        "queued"
    );
    let count = pair.dns.load(Ordering::Relaxed);
    assert!(
        indexers::fetch(
            &engine.config.sources[0],
            &engine.config.sources[0].url,
            "text/plain",
            deadline()
        )
        .is_err()
    );
    assert_eq!(pair.dns.load(Ordering::Relaxed), count);
}

#[test]
fn protection_is_strict_and_never_uses_an_unbound_or_direct_network() {
    let dir = Directory::new();
    let cfg = settings(source("http://must-not-contact.invalid:9/api", false));
    let config = config::from_json(&cfg, &dir.0).unwrap();
    assert!(
        indexers::fetch(
            &config.sources[0],
            &config.sources[0].url,
            "text/plain",
            deadline()
        )
        .is_err()
    );
    let engine =
        Engine::open_with_network(config, Arc::new(crate::torrent::transport::Direct), false)
            .unwrap();
    assert!(
        indexers::fetch(
            &engine.config.sources[0],
            &engine.config.sources[0].url,
            "text/plain",
            deadline()
        )
        .is_err()
    );
    assert!(engine.source_publication(Protection::Protected).is_err());
    for invalid in [
        Value::Null,
        Value::Bool(true),
        Value::from("vpn"),
        Value::from("Protected"),
    ] {
        let mut s = source("http://example.invalid/api", false);
        s.insert("protection", invalid);
        assert!(config::from_json(&settings(s), &dir.0).is_err());
    }
}

fn irc_settings(port: u16) -> Value {
    let mut v = config::default_json();
    let mut irc = Value::object();
    let mut s = Value::object();
    for (k, val) in [
        ("id", "protected"),
        ("protection", "protected"),
        ("nickname", "mynou"),
        ("channel", "#announces"),
        ("sender", "announce!bot@fixture"),
    ] {
        s.insert(k, val);
    }
    s.insert("url", format!("ircs://localhost:{port}"));
    s.insert("enabled", true);
    s.insert("reconnect_min_secs", 1_u32);
    s.insert("reconnect_max_secs", 1_u32);
    let mut sasl = Value::object();
    sasl.insert("mechanism", "PLAIN");
    sasl.insert("username_env", "MYNOU_FIXTURE_USER");
    sasl.insert("password_env", "MYNOU_FIXTURE_PASS");
    s.insert("sasl", sasl);
    irc.insert("sources", Value::Array(vec![s]));
    v.insert("irc", irc);
    v
}
fn irc_session(
    service: &dyn NetworkListener,
    expected: &[u8],
    notice: &[u8],
) -> Box<dyn NetworkStream> {
    let mut stream = accept(service);
    crate::tls::tests::serve_dialogue(
        &mut stream,
        &[
            (b"CAP LS 302\r\n", b""),
            (b"NICK mynou\r\n", b""),
            (
                b"USER mynou 0 * :Mynou\r\n",
                b":server CAP * LS :sasl=PLAIN\r\n",
            ),
            (b"CAP REQ :sasl\r\n", b":server CAP mynou ACK :sasl\r\n"),
            (b"AUTHENTICATE PLAIN\r\n", b"AUTHENTICATE +\r\n"),
            (expected, b":server 903 mynou :Authenticated\r\n"),
            (b"CAP END\r\n", b":server 001 mynou :Welcome\r\n"),
            (b"JOIN #announces\r\n", notice),
            (b"PONG :fixture\r\n", b""),
        ],
    )
    .unwrap();
    stream
    // The worker retains the idle stream until the test permits close.
}
#[test]
fn protected_irc_tls_sasl_reconnect_restart_and_dedup_use_only_virtual_services() {
    if child(
        "source_network::tests::protected_irc_tls_sasl_reconnect_restart_and_dedup_use_only_virtual_services",
    ) {
        return;
    }
    let pair = Pair::new();
    let dir = Directory::new();
    let service = listener(&pair);
    let cfg = irc_settings(service.port());
    let notice=format!(":mynou!user@fixture JOIN :#announces\r\n:announce!bot@fixture PRIVMSG #announces :MYNOU {{\"title\":\"Fixture.Movie.2024.1080p\",\"media_title\":\"Fixture Movie\",\"kind\":\"movie\",\"year\":2024,\"tmdb_id\":\"42\",\"info_hash\":\"{}\"}}\r\nPING :fixture\r\n","1".repeat(40)).into_bytes();
    let expected = "AGZpeHR1cmUtdXNlcgBmaXh0dXJlLXBhc3M=";
    let expected = format!("AUTHENTICATE {expected}\r\n").into_bytes();
    let (entered, observed) = mpsc::channel();
    let (next, go) = mpsc::channel();
    let worker = thread::spawn(move || {
        for _ in 0..3 {
            let stream = irc_session(&*service, &expected, &notice);
            entered.send(()).unwrap();
            go.recv_timeout(Duration::from_secs(10)).unwrap();
            drop(stream);
        }
    });
    let engine = open(&dir, &pair, &cfg);
    let mut handles = Vec::new();
    crate::irc::client::start(&engine, &mut handles);
    observed.recv_timeout(Duration::from_secs(10)).unwrap();
    next.send(()).unwrap();
    observed.recv_timeout(Duration::from_secs(10)).unwrap();
    wait(|| {
        engine
            .irc_sources()
            .unwrap()
            .get("sources")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("health")
            .unwrap()
            .get("duplicates")
            .and_then(Value::as_str)
            == Some("1")
    });
    let stopping = Instant::now();
    engine.stopped.store(true, Ordering::Release);
    for h in handles {
        h.join().unwrap();
    }
    assert!(stopping.elapsed() < Duration::from_secs(2));
    drop(engine);
    let engine = open(&dir, &pair, &cfg);
    let mut handles = Vec::new();
    crate::irc::client::start(&engine, &mut handles);
    next.send(()).unwrap();
    observed.recv_timeout(Duration::from_secs(10)).unwrap();
    wait(|| {
        engine
            .irc_sources()
            .unwrap()
            .get("sources")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("health")
            .unwrap()
            .get("duplicates")
            .and_then(Value::as_str)
            == Some("1")
    });
    assert_eq!(
        engine
            .irc_announcements(0, 10)
            .unwrap()
            .get("announcements")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    pair.client.cancel();
    assert!(engine.source_publication(Protection::Protected).is_err());
    let stopping = Instant::now();
    engine.stopped.store(true, Ordering::Release);
    for h in handles {
        h.join().unwrap();
    }
    assert!(stopping.elapsed() < Duration::from_secs(2));
    next.send(()).unwrap();
    worker.join().unwrap();
    assert!(pair.dns.load(Ordering::Relaxed) >= 3);
    let report = json::stringify(&engine.irc_sources().unwrap());
    assert!(!report.contains("fixture-pass"));
    assert!(!report.contains("fixture-user"));
}

#[test]
fn replaced_generation_requires_a_fresh_engine_and_ipv6_never_falls_back() {
    let pair = Pair::new();
    let dir = Directory::new();
    let cfg = settings(source("http://[::1]:9/api", false));
    let engine = open(&dir, &pair, &cfg);
    let s = &engine.config.sources[0];
    let before = pair.dns.load(Ordering::Relaxed);
    assert!(indexers::fetch(s, &s.url, "text/plain", deadline()).is_err());
    assert_eq!(pair.dns.load(Ordering::Relaxed), before);
    pair.replace_client_generation();
    assert!(engine.source_publication(Protection::Protected).is_err());
    let end = deadline();
    let fresh = loop {
        if let Ok(network) = pair.client.network(end) {
            break network;
        }
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(5));
    };
    assert!(engine.source_publication(Protection::Protected).is_err());
    drop(engine);
    let engine =
        Engine::open_with_network(config::from_json(&cfg, &dir.0).unwrap(), fresh, false).unwrap();
    assert!(engine.source_publication(Protection::Protected).is_ok());
    assert!(
        indexers::fetch(
            &engine.config.sources[0],
            &engine.config.sources[0].url,
            "text/plain",
            deadline()
        )
        .is_err()
    );
}

#[test]
fn protected_lookup_cannot_select_a_direct_indexer_and_policy_changes_require_new_identity() {
    let dir = Directory::new();
    let mut cfg = settings(source("https://source.invalid/api", false));
    let engine = Engine::open_with_network(
        config::from_json(&cfg, &dir.0).unwrap(),
        Arc::new(crate::torrent::transport::Direct),
        false,
    )
    .unwrap();
    drop(engine);
    let Value::Array(sources) = cfg.get_mut("indexers").unwrap() else {
        panic!()
    };
    sources[0].insert("protection", "direct");
    assert!(
        Engine::open_with_network(
            config::from_json(&cfg, &dir.0).unwrap(),
            Arc::new(crate::torrent::transport::Direct),
            false
        )
        .is_err()
    );
    let mut irc = irc_settings(6697).get("irc").unwrap().clone();
    let Value::Array(irc_sources) = irc.get_mut("sources").unwrap() else {
        panic!()
    };
    let mut lookup = Value::object();
    lookup.insert("type", "bracketed");
    lookup.insert("lookup_indexer", "synthetic");
    lookup.insert("details_prefix", "https://source.invalid/details/");
    let mut categories = Value::object();
    categories.insert("Movies", "movie");
    lookup.insert("categories", categories);
    irc_sources[0].insert("announcement_format", lookup);
    cfg.insert("irc", irc);
    assert!(
        config::from_json(&cfg, &dir.0)
            .unwrap_err()
            .contains("protected lookup requires a protected indexer")
    );
    let Value::Array(sources) = cfg.get_mut("indexers").unwrap() else {
        panic!()
    };
    sources[0].insert("protection", "protected");
    assert!(config::from_json(&cfg, &dir.0).is_ok());
}

#[test]
fn late_search_results_from_an_earlier_source_are_refused_after_owner_loss() {
    let pair = Pair::new();
    let dir = Directory::new();
    let first = listener(&pair);
    let second = listener(&pair);
    let mut a = source(&format!("http://first.example:{}/api", first.port()), false);
    a.insert("kind", "rss");
    let mut b = source(
        &format!("http://second.example:{}/api", second.port()),
        false,
    );
    b.insert("kind", "rss");
    b.insert("id", "second");
    let mut cfg = settings(a.clone());
    cfg.insert("indexers", Value::Array(vec![a, b]));
    let engine = open(&dir, &pair, &cfg);
    let (entered, observed) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let mut stream = accept(&*first);
        read_http(&mut *stream);
        let body = format!(
            "<rss><channel><item><title>Fixture.Movie.2024.1080p.WEB.x264</title><link>magnet:?xt=urn:btih:{}</link><seeders>20</seeders></item></channel></rss>",
            "1".repeat(40)
        );
        stream.write_all(&response(body.as_bytes(), "")).unwrap();
        let _ = stream.read(&mut [0]);
        let mut stream = accept(&*second);
        read_http(&mut *stream);
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
        let _ = stream.write_all(&response(EMPTY, ""));
    });
    let request = crate::store::Request {
        kind: "movie".into(),
        title: "Fixture Movie".into(),
        year: 2024,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: None,
        tmdb_id: Some(42),
        source_numbering: None,
    };
    let search = {
        let e = engine.clone();
        thread::spawn(move || integrations::search_report(&e.config, &request))
    };
    observed.recv_timeout(Duration::from_secs(10)).unwrap();
    pair.client.cancel();
    release.send(()).unwrap();
    assert!(search.join().unwrap().is_err());
    worker.join().unwrap();
}
