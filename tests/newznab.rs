//! Original loopback Newznab/NZB fixtures. No external indexers or article acquisition.
#[path = "newznab_support/admission.rs"]
mod admission;
mod library_support;
mod requester_support;
mod usenet_support;
mod web_support;

use library_support::{Directory, files};
use mynou::{
    config::{self, Config},
    crypto::sha256,
    engine::{Engine, lock},
    indexers::ControlRequest,
    integrations,
    json::{self, Value},
    numbering::SourceNumber,
    store::Request,
    usenet::newznab::{self, Target},
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use usenet_support::Provider;
use web_support::{Browser, Server, TOKEN};

const PRIVATE_KEY: &str = "original-document-private-key";
const TITLE: &str = "Fixture.Movie.2024.1080p.WEB-DL.x264";
const NS: &str = "http://www.newznab.com/DTD/2010/feeds/attributes/";
type RequestLog = Vec<(String, Option<String>)>;

struct Http {
    url: String,
    feed: Arc<Mutex<(u16, Vec<u8>)>>,
    document: Arc<Mutex<(u16, Vec<u8>)>>,
    calls: Arc<Mutex<RequestLog>>,
    blocked: Arc<AtomicBool>,
    document_blocked: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Http {
    fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let feed = Arc::new(Mutex::new((
            200,
            rss(&item(TITLE, "1024", "")).into_bytes(),
        )));
        let document = Arc::new(Mutex::new((200, usenet_support::source(1, 1))));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let blocked = Arc::new(AtomicBool::new(false));
        let document_blocked = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (f, d, c, b, db, s) = (
            feed.clone(),
            document.clone(),
            calls.clone(),
            blocked.clone(),
            document_blocked.clone(),
            stop.clone(),
        );
        let worker = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("Original Newznab fixture accept failed: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 16_384);
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    head.push(byte[0]);
                }
                if !head.ends_with(b"\r\n\r\n") {
                    continue;
                }
                let head = String::from_utf8(head).unwrap();
                let path = head
                    .lines()
                    .next()
                    .unwrap()
                    .strip_prefix("GET ")
                    .unwrap()
                    .strip_suffix(" HTTP/1.1")
                    .unwrap()
                    .to_owned();
                let fields: BTreeMap<_, _> = head
                    .lines()
                    .skip(1)
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                c.lock()
                    .unwrap()
                    .push((path.clone(), fields.get("authorization").cloned()));
                let response = if path.starts_with("/api?") {
                    f.lock().unwrap().clone()
                } else {
                    assert!(path.starts_with("/get?"));
                    d.lock().unwrap().clone()
                };
                let start = Instant::now();
                while (b.load(Ordering::Acquire)
                    || path.starts_with("/get?") && db.load(Ordering::Acquire))
                    && !s.load(Ordering::Acquire)
                {
                    assert!(start.elapsed() < Duration::from_secs(3));
                    thread::sleep(Duration::from_millis(2));
                }
                let _ = write!(
                    stream,
                    "HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.0,
                    response.1.len()
                );
                let _ = stream.write_all(&response.1);
            }
        });
        Self {
            url,
            feed,
            document,
            calls,
            blocked,
            document_blocked,
            stop,
            thread: Some(worker),
        }
    }
    fn replace(&self, text: &str) {
        *self.feed.lock().unwrap() = (200, text.as_bytes().to_vec());
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        self.blocked.store(false, Ordering::Release);
        self.document_blocked.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.thread.take()
            && let Err(e) = h.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(e);
        }
    }
}
fn rss(items: &str) -> String {
    format!("<rss version=\"2.0\" xmlns:newznab=\"{NS}\"><channel>{items}</channel></rss>")
}
fn item(title: &str, size: &str, extra: &str) -> String {
    format!(
        "<item><title>{title}</title><enclosure url=\"/get?id=original&amp;apikey={PRIVATE_KEY}\" length=\"{size}\" type=\"application/x-nzb\"/><newznab:attr name=\"size\" value=\"{size}\"/>{extra}</item>"
    )
}
fn value(p: &Provider, h: &Http) -> Value {
    let mut v = config::default_json();
    v.insert("listen", "127.0.0.1:0");
    v.get_mut("downloads").unwrap().insert("enabled", false);
    let mut usenet = usenet_support::usenet(p);
    usenet
        .get_mut("downloads")
        .unwrap()
        .insert("enabled", false);
    v.insert("usenet", usenet);
    let mut s = Value::object();
    s.insert("id", "original-indexer");
    s.insert("name", "Original Newznab");
    s.insert("kind", "newznab");
    s.insert("url", format!("{}/api", h.url));
    s.insert("api_key_env", "MYNOU_ABSENT_NEWZNAB_FIXTURE_KEY_376185");
    s.insert(
        "usenet",
        json::parse(r#"{"server_id":"original","minimum_bytes":1,"maximum_bytes":4096}"#).unwrap(),
    );
    v.insert("indexers", Value::Array(vec![s]));
    v
}
fn source(v: &mut Value) -> &mut Value {
    let Value::Array(s) = v.get_mut("indexers").unwrap() else {
        panic!("source array")
    };
    &mut s[0]
}
fn cfg(d: &Directory, p: &Provider, h: &Http) -> Config {
    config::from_json(&value(p, h), &d.0).unwrap()
}
fn movie() -> Request {
    let mut r = library_support::movie("Fixture Movie");
    r.tmdb_id = Some(42);
    r
}
fn entries<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).unwrap().as_array().unwrap()
}
fn select(c: &Config, r: &Request) -> integrations::SelectedRelease {
    integrations::select_acquisition(c, r, Instant::now() + Duration::from_secs(3)).unwrap()
}
fn fetch(c: &Config, selected: &integrations::SelectedRelease) -> mynou::Result<Vec<u8>> {
    newznab::fetch_document(
        c,
        selected.usenet.as_ref().unwrap(),
        &selected.url,
        Instant::now() + Duration::from_secs(3),
    )
}
fn redacted(v: &Value) {
    let text = json::stringify(v);
    for private in [
        PRIVATE_KEY,
        "http://",
        "https://",
        "magnet:",
        "apikey=",
        "file0-part1@",
        "token_env",
        "password_env",
        "username_env",
    ] {
        assert!(
            !text.contains(private),
            "Public Newznab data exposed private metadata"
        );
    }
}
fn control(engine: &Engine, action: &str) {
    let mut q = ControlRequest {
        action: action.into(),
        apply: false,
        plan_id: None,
    };
    q.plan_id = Some(
        engine
            .indexer_control("original-indexer", &q)
            .unwrap()
            .get("plan_id")
            .unwrap()
            .as_str()
            .unwrap()
            .into(),
    );
    q.apply = true;
    engine.indexer_control("original-indexer", &q).unwrap();
}

#[test]
fn typed_discovery_uses_movie_identity_and_not_torrent_seed_thresholds() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let mut c = cfg(&d, &p, &h);
    c.minimum_seeders = 999_999;
    let before = files(&d.0);
    let report = integrations::search_report(&c, &movie()).unwrap();
    let accepted = entries(&report, "accepted");
    assert_eq!(accepted.len(), 1);
    assert!(entries(&report, "rejected").is_empty());
    let row = &accepted[0];
    assert_eq!(row.get("transport").and_then(Value::as_str), Some("usenet"));
    assert_eq!(row.get("seeders"), Some(&Value::Null));
    assert_eq!(
        row.get("advertised_bytes").and_then(Value::as_str),
        Some("1024")
    );
    assert_eq!(row.get("content_verified"), Some(&Value::Bool(false)));
    assert_eq!(
        row.get("library_admission_supported"),
        Some(&Value::Bool(false))
    );
    redacted(&report);
    let path = h.calls.lock().unwrap()[0].0.clone();
    for query in ["q=Fixture%20Movie", "t=movie", "year=2024", "tmdbid=42"] {
        assert!(path.contains(query));
    }
    let selected = select(&c, &movie());
    assert_eq!(selected.id, row.get("id").unwrap().as_str().unwrap());
    let target = selected.usenet.as_ref().unwrap();
    assert_eq!(target.indexer_id, "original-indexer");
    assert_eq!(target.server_id, "original");
    assert_eq!(Target::from_json(&target.to_json()).unwrap(), *target);
    redacted(&target.to_json());
    assert_eq!(files(&d.0), before);
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn ordinary_torrent_selection_cannot_misroute_an_nzb_to_the_torrent_client() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let c = cfg(&d, &p, &h);
    let before = files(&d.0);
    let error = integrations::select_release(&c, &movie()).err().unwrap();
    assert!(error.contains("Usenet library admission"));
    assert!(integrations::search(&c, &movie()).is_err());
    assert!(
        h.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(path, _)| path.starts_with("/api?"))
    );
    assert_eq!(files(&d.0), before);
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn quality_identity_size_and_password_policy_reject_independent_claims() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    h.replace(&rss(&[
        item(TITLE, "1024", ""),
        item(TITLE, "4097", ""),
        item(
            TITLE,
            "1024",
            "<newznab:attr name=\"password\" value=\"1\"/>",
        ),
        item("Fixture.Movie.2023.1080p.WEB-DL.x264", "1024", ""),
        item("Fixture.Movie.Sequel.2024.1080p", "1024", ""),
        item("Fixture.Movie.2024.720p.WEB-DL.x264", "1024", ""),
    ]
    .concat()));
    let mut v = value(&p, &h);
    v.insert("selection", json::parse(r#"{"movie_profile":"original","episode_profile":"original","profiles":{"original":{"resolutions":[1080]}}}"#).unwrap());
    let c = config::from_json(&v, &d.0).unwrap();
    let report = integrations::search_report(&c, &movie()).unwrap();
    assert_eq!(entries(&report, "accepted").len(), 1);
    assert_eq!(entries(&report, "rejected").len(), 5);
    let reasons = json::stringify(report.get("rejected").unwrap());
    assert!(reasons.contains("size interval"));
    assert!(reasons.contains("password protected"));
    assert!(reasons.contains("title, year or episode"));
    redacted(&report);
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn malformed_enclosures_and_conflicting_namespaces_cannot_impersonate_valid_items() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let good = item(TITLE, "1024", "");
    let bad = [
        good.replace("application/x-nzb", "application/x-bittorrent"),
        good.replace("/get?id=original", "https://other.invalid/get?id=original"),
        good.replace("/get?id=original", "magnet:?xt=urn:btih:fixture"),
        good.replace("length=\"1024\"", "length=\"1025\""),
        good.replace("newznab:attr", "attr"),
        good.replace(
            "<item>",
            "<item xmlns:newznab=\"https://untrusted.invalid/\">",
        ),
        item(
            TITLE,
            "1024",
            "<newznab:attr name=\"size\" value=\"1024\"/>",
        ),
        item(
            TITLE,
            "1024",
            "<newznab:attr name=\"password\" value=\"0\"/><newznab:attr name=\"passworded\" value=\"1\"/>",
        ),
        item(TITLE, "1024", "<title>Other Movie</title>"),
        good.replace("length=\"1024\"", "length=\"0\""),
        item(
            TITLE,
            "1024",
            "<newznab:attr name=\"password\" value=\"true\"/>",
        ),
    ];
    h.replace(&rss(&(good + &bad.concat())));
    let report = integrations::search_report(&cfg(&d, &p, &h), &movie()).unwrap();
    assert_eq!(entries(&report, "accepted").len(), 1);
    assert!(entries(&report, "rejected").is_empty());
    assert_eq!(
        report.get("candidate_count").and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(h.count(), 1);
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn namespace_aliases_size_attributes_and_alternate_nzb_mime_are_supported() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let text = item(
        TITLE,
        "1024",
        "<newznab:attr name=\"passworded\" value=\"0\"/>",
    )
    .replace(" length=\"1024\"", "")
    .replace("newznab:attr", "original:attr")
    .replace("<item>", &format!("<item xmlns:original=\"{NS}\">"))
    .replace("application/x-nzb", "application/x-nzb+xml");
    h.replace(&rss(&text));
    let selected = select(&cfg(&d, &p, &h), &movie());
    assert_eq!(selected.usenet.unwrap().advertised_bytes, 1024);
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn seasonal_and_absolute_episode_queries_keep_canonical_identity_rules() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let c = cfg(&d, &p, &h);
    let mut r = library_support::movie("Fixture Series");
    r.kind = "episode".into();
    r.season = 2;
    r.episode = 3;
    h.replace(&rss(&[
        item("Fixture.Series.2024.S02E03.1080p.WEB-DL", "1024", ""),
        item("Fixture.Series.2024.S01E03.1080p.WEB-DL", "1024", ""),
    ]
    .concat()));
    let report = integrations::search_report(&c, &r).unwrap();
    assert_eq!(entries(&report, "accepted").len(), 1);
    assert_eq!(entries(&report, "rejected").len(), 1);
    let query = h.calls.lock().unwrap()[0].0.clone();
    for field in ["t=tvsearch", "season=2", "ep=3"] {
        assert!(query.contains(field));
    }
    r.source_numbering = Some(SourceNumber::Absolute(13));
    h.replace(&rss(&[
        item("Fixture.Series.2024.013.1080p.WEB-DL", "1024", ""),
        item("Fixture.Series.2024.014.1080p.WEB-DL", "1024", ""),
        item("Fixture.Series.2024.S02E03.1080p.WEB-DL", "1024", ""),
    ]
    .concat()));
    let report = integrations::search_report(&c, &r).unwrap();
    assert_eq!(entries(&report, "accepted").len(), 1);
    assert_eq!(entries(&report, "rejected").len(), 2);
    let query = h.calls.lock().unwrap()[1].0.clone();
    assert!(query.contains("q=Fixture%20Series%20013"));
    assert!(!query.contains("season=") && !query.contains("ep="));
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn bound_document_fetch_reuses_native_basic_and_bearer_authentication_without_queueing() {
    for mode in ["basic", "bearer"] {
        let d = Directory::new();
        let p = Provider::open();
        let h = Http::open();
        let mut v = value(&p, &h);
        let mut a = Value::object();
        a.insert("method", mode);
        if mode == "basic" {
            a.insert("username_env", "PWD");
            a.insert("password_env", "PATH");
        } else {
            a.insert("token_env", "PWD");
        }
        source(&mut v).insert("authentication", a);
        let c = config::from_json(&v, &d.0).unwrap();
        let selected = select(&c, &movie());
        let before = files(&d.0);
        assert_eq!(fetch(&c, &selected).unwrap(), usenet_support::source(1, 1));
        let calls = h.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, calls[1].1);
        assert!(
            calls[1]
                .1
                .as_ref()
                .unwrap()
                .starts_with(if mode == "basic" { "Basic " } else { "Bearer " })
        );
        if mode == "bearer" {
            assert_eq!(
                calls[1].1.as_ref().unwrap(),
                &format!("Bearer {}", std::env::var("PWD").unwrap())
            );
        }
        assert!(calls[1].0.contains(PRIVATE_KEY));
        assert_eq!(files(&d.0), before);
        redacted(&mynou::indexers::report(&c.sources));
        assert!(p.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn captured_identity_and_deadline_fail_before_http_or_article_io() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let c = cfg(&d, &p, &h);
    let selected = select(&c, &movie());
    let original = selected.usenet.as_ref().unwrap();
    for target in [
        Target {
            indexer_binding: "0".repeat(64),
            ..original.clone()
        },
        Target {
            server_binding: "0".repeat(64),
            ..original.clone()
        },
        Target {
            server_id: "removed".into(),
            ..original.clone()
        },
        Target {
            advertised_bytes: 4097,
            ..original.clone()
        },
        Target {
            password_protected: true,
            ..original.clone()
        },
        Target {
            advertised_bytes: 0,
            ..original.clone()
        },
    ] {
        assert!(
            newznab::fetch_document(
                &c,
                &target,
                &selected.url,
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
        );
    }
    for url in [
        "https://other.invalid/get?id=original",
        "//other.invalid/get?id=original",
        "magnet:?xt=urn:btih:fixture",
    ] {
        assert!(
            newznab::fetch_document(&c, original, url, Instant::now() + Duration::from_secs(2))
                .is_err()
        );
    }
    assert!(newznab::fetch_document(&c, original, &selected.url, Instant::now()).is_err());
    let mut changed = value(&p, &h);
    source(&mut changed)
        .get_mut("usenet")
        .unwrap()
        .insert("maximum_bytes", 2048_u32);
    assert!(fetch(&config::from_json(&changed, &d.0).unwrap(), &selected).is_err());
    assert_eq!(h.count(), 1);
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn source_pause_and_in_flight_generation_fence_cover_document_fetches() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let c = cfg(&d, &p, &h);
    let engine = Engine::open_for_management(c.clone()).unwrap();
    let selected = select(&c, &movie());
    control(&engine, "pause");
    assert!(fetch(&c, &selected).is_err());
    assert_eq!(h.count(), 1);
    control(&engine, "enable");
    h.blocked.store(true, Ordering::Release);
    let target = selected.usenet.clone().unwrap();
    let url = selected.url.clone();
    let cloned = c.clone();
    let worker = thread::spawn(move || {
        newznab::fetch_document(
            &cloned,
            &target,
            &url,
            Instant::now() + Duration::from_secs(3),
        )
    });
    usenet_support::wait(|| h.count() == 2);
    control(&engine, "reset_session");
    h.blocked.store(false, Ordering::Release);
    assert!(worker.join().unwrap().is_err());
    assert_eq!(fetch(&c, &selected).unwrap(), usenet_support::source(1, 1));
    assert!(lock(&engine.store).unwrap().list().is_empty());
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn bounded_feed_errors_and_malformed_documents_are_fixed_and_nonacquiring() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let c = cfg(&d, &p, &h);
    for bad in [
        format!("<error code=\"100\" description=\"{PRIVATE_KEY}\"/>"),
        format!(
            "<!DOCTYPE rss [<!ENTITY leak SYSTEM 'https://other.invalid/'>]>{}",
            rss("")
        ),
        rss(&item(TITLE, "1024", "").repeat(4097)),
        "<rss><channel/><channel/></rss>".into(),
    ] {
        h.replace(&bad);
        let error = integrations::search_report(&c, &movie()).unwrap_err();
        assert_eq!(error, "Search: no indexer returned a usable response");
    }
    h.replace(&rss(&item(TITLE, "1024", "")));
    let selected = select(&c, &movie());
    *h.document.lock().unwrap() = (200, format!("<nzb>{PRIVATE_KEY}</nzb>").into_bytes());
    let error = fetch(&c, &selected).unwrap_err();
    assert!(!error.contains(PRIVATE_KEY));
    *h.document.lock().unwrap() = (503, PRIVATE_KEY.as_bytes().to_vec());
    assert!(!fetch(&c, &selected).unwrap_err().contains(PRIVATE_KEY));
    assert!(p.requests.lock().unwrap().is_empty());
    assert!(files(&d.0).is_empty());
}

#[test]
fn strict_configuration_requires_explicit_provider_and_bounded_size_policy() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let cases = [
        r#"{}"#,
        r#"{"server_id":"missing"}"#,
        r#"{"server_id":"original","minimum_bytes":0}"#,
        r#"{"server_id":"original","minimum_bytes":2048,"maximum_bytes":1024}"#,
        r#"{"server_id":"original","maximum_bytes":1099511627777}"#,
        r#"{"server_id":"original","maximum_bytes":"4096"}"#,
        r#"{"server_id":"original","unknown":true}"#,
    ];
    for settings in cases {
        let mut v = value(&p, &h);
        source(&mut v).insert("usenet", json::parse(settings).unwrap());
        assert!(config::from_json(&v, &d.0).is_err());
    }
    let mut v = value(&p, &h);
    source(&mut v).insert("kind", "torznab");
    assert!(config::from_json(&v, &d.0).is_err());
    let mut v = value(&p, &h);
    let Value::Object(s) = source(&mut v) else {
        panic!()
    };
    s.remove("usenet");
    assert!(config::from_json(&v, &d.0).is_err());
    assert_eq!(h.count(), 0);
    assert!(p.requests.lock().unwrap().is_empty());
}

#[test]
fn preceding_torrent_source_binding_contract_reopens_without_rewriting_policy() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let mut v = value(&p, &h);
    source(&mut v).insert("kind", "json");
    let Value::Object(s) = source(&mut v) else {
        panic!()
    };
    s.remove("usenet");
    let c = config::from_json(&v, &d.0).unwrap();
    drop(Engine::open_for_management(c.clone()).unwrap());
    let s = &c.sources[0];
    let fields = Value::Array(vec![
        s.name.clone().into(),
        s.kind.clone().into(),
        s.url.clone().into(),
        s.api_key_env.clone().into(),
        "0".into(),
        Value::Null,
    ]);
    let binding: String = sha256(json::stringify(&fields).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut row = Value::object();
    row.insert("id", "original-indexer");
    row.insert("binding", binding);
    row.insert("enabled", true);
    row.insert("revision", "1");
    row.insert("action", "initial");
    row.insert("at", "0");
    let mut root = Value::object();
    root.insert("records", Value::Array(vec![row]));
    let payload = json::stringify(&root).into_bytes();
    let mut bytes = b"MYNOUS01".to_vec();
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&sha256(&bytes));
    let path = c.store_dir.join("indexers.bin");
    fs::write(&path, &bytes).unwrap();
    let engine = Engine::open_for_management(c).unwrap();
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(
        entries(&engine.indexers().unwrap(), "sources")[0].get("enabled"),
        Some(&Value::Bool(true))
    );
    assert_eq!(h.count(), 0);
}

#[test]
fn authenticated_api_cli_and_browser_preview_typed_releases_without_jobs_or_downloads() {
    let d = Directory::new();
    let p = Provider::open();
    let h = Http::open();
    let mut v = value(&p, &h);
    let server = Server::open(config::from_json(&v, &d.0).unwrap());
    v.insert("listen", server.authority.clone());
    fs::write(d.0.join("mynou.json"), json::stringify(&v)).unwrap();
    let before = files(&d.0);
    let body = json::stringify(&movie().to_json());
    assert_eq!(
        server
            .call(
                "POST",
                "/api/search",
                &[("Content-Type", "application/json")],
                &body
            )
            .status,
        401
    );
    assert_eq!(h.count(), 0);
    let report = server.call(
        "POST",
        "/api/search",
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", "application/json"),
        ],
        &body,
    );
    assert_eq!(report.status, 200, "{}", report.body);
    let parsed = json::parse(&report.body).unwrap();
    redacted(&parsed);
    let mut child = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(&d.0)
        .env("MYNOU_API_TOKEN", TOKEN)
        .args([
            "search",
            "--kind",
            "movie",
            "--title",
            "Fixture Movie",
            "--year",
            "2024",
            "--tmdb-id",
            "42",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(8) {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("Newznab CLI fixture timed out");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        json::parse(std::str::from_utf8(&output.stdout).unwrap().trim()).unwrap(),
        parsed
    );
    let browser = Browser::login(&server);
    let preview = browser.post(
        &server,
        "/ui/search",
        &[
            ("kind", "movie"),
            ("title", "Fixture Movie"),
            ("year", "2024"),
            ("source_kind", "auto"),
        ],
    );
    assert_eq!(preview.status, 200, "{}", preview.body);
    assert!(preview.body.contains(TITLE));
    assert!(preview.body.contains("Usenet"));
    assert!(preview.body.contains("1024 bytes advertised"));
    assert!(!preview.body.contains(PRIVATE_KEY));
    preview.no_secrets();
    assert!(lock(&server.engine.store).unwrap().list().is_empty());
    assert!(
        h.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(path, _)| path.starts_with("/api?"))
    );
    assert!(p.requests.lock().unwrap().is_empty());
    assert_eq!(files(&d.0), before);
}
