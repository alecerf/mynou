//! Bracketed announcements resolved through loopback Torznab and catalog fixtures.
//! Sanitized synthetic lines only; no tracker, channel or credential is real.
mod irc_support;
mod library_support;
use library_support::Directory;
use mynou::{
    config::{self, Config},
    engine::Engine,
    json::Value,
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
};

const PREFIX: &str = "https://tracker.example/torrents/";
const HASH: &str = "a0b8304fc89c2a924f18d6fb9b1be593bec3df9e";

/// Hex of the hash's ASCII digits, as some Torznab aggregators return it.
fn doubled(hash: &str) -> String {
    hash.bytes().map(|b| format!("{b:02x}")).collect()
}
fn item(id: u64, tmdb: &str, hash: &str) -> String {
    format!(
        "<item><title>Fixture</title><guid>https://tracker.example/torrent/download/{id}.SECRETKEY</guid>\
         <comments>{PREFIX}{id}</comments><link>https://tracker.example/torrent/download/{id}.SECRETKEY</link>\
         <torznab:attr name=\"tmdbid\" value=\"{tmdb}\"/><torznab:attr name=\"infohash\" value=\"{hash}\"/></item>"
    )
}
fn feed(items: &[String]) -> String {
    format!(
        "<?xml version=\"1.0\"?><rss xmlns:torznab=\"http://torznab.com/schemas/2015/feed\"><channel>{}</channel></rss>",
        items.concat()
    )
}

/// Serves every request from `routes` (path prefix → status and body) and logs paths.
struct Fixture {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
    routes: Arc<Mutex<Vec<(String, u16, String)>>>,
}
impl Fixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let routes: Arc<Mutex<Vec<(String, u16, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let (log, table) = (seen.clone(), routes.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut head = Vec::new();
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).is_ok() {
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                log.lock().unwrap().push(path.clone());
                let (status, body) = table
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(p, ..)| path.starts_with(p.as_str()))
                    .map_or((404, String::new()), |(_, s, b)| (*s, b.clone()));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self { url, seen, routes }
    }
    fn route(&self, path: &str, status: u16, body: &str) {
        let mut routes = self.routes.lock().unwrap();
        routes.retain(|(p, ..)| p != path);
        routes.insert(0, (path.into(), status, body.into()));
    }
    fn searches(&self) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with("/api"))
            .count()
    }
}

fn format_json() -> Value {
    let mut categories = Value::object();
    categories.insert("Films", "movie");
    categories.insert("Series", "episode");
    let mut v = Value::object();
    v.insert("type", "bracketed");
    v.insert("lookup_indexer", "tracker");
    v.insert("details_prefix", PREFIX);
    v.insert("categories", categories);
    v
}
fn settings(dir: &Directory, http: &str) -> Value {
    let mut v = irc_support::value(&dir.0, "irc://127.0.0.1:1");
    let Value::Array(sources) = v.get_mut("irc").unwrap().get_mut("sources").unwrap() else {
        panic!()
    };
    sources[0].insert("announcement_format", format_json());
    let mut indexer = Value::object();
    indexer.insert("id", "tracker");
    indexer.insert("name", "tracker");
    indexer.insert("kind", "torznab");
    indexer.insert("url", format!("{http}/api"));
    indexer.insert("api_key_env", "MYNOU_ABSENT_IRC_LOOKUP_FIXTURE_KEY_109");
    v.insert("indexers", Value::Array(vec![indexer]));
    let catalog = v.get_mut("catalog").unwrap();
    catalog.insert("enabled", true);
    catalog.insert("url", format!("{http}/3"));
    catalog.insert("token_env", "PATH");
    v
}
fn setup() -> (Directory, Fixture, Config, std::sync::Arc<Engine>) {
    let dir = Directory::new();
    let fixture = Fixture::new();
    let cfg = config::from_json(&settings(&dir, &fixture.url), &dir.0).unwrap();
    let engine = Engine::open(cfg.clone()).unwrap();
    (dir, fixture, cfg, engine)
}
fn line(category: &str, name: &str, id: u64) -> String {
    format!(
        "\u{3}04[NEW]\u{3} [{category}] [{name}] [WEB-DL] [55.19 GiB] [0%] [pre 0h 1m 52s] par uploader -> {PREFIX}{id}"
    )
}
fn receive(engine: &Engine, text: &str) -> Option<Value> {
    engine
        .irc_receive_text("local", irc_support::SENDER, "#announces", text)
        .unwrap()
}
fn resolve(engine: &Engine) -> (String, Value) {
    let report = engine.irc_resolve_pending().unwrap();
    let outcome = report.get("outcome").unwrap().as_str().unwrap().to_owned();
    (outcome, report)
}
fn history(engine: &Engine) -> Vec<Value> {
    let page = engine.irc_announcements(0, 50).unwrap();
    page.get("announcements")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec()
}
fn movie_catalog(fixture: &Fixture) {
    fixture.route(
        "/3/movie/603",
        200,
        r#"{"id":603,"title":"Fixture Movie","release_date":"2024-03-01"}"#,
    );
}

#[test]
fn a_bracketed_candidate_resolves_through_the_exact_details_page_and_catalog() {
    let (dir, fixture, cfg, engine) = setup();
    let name = "Fixture.Movie.2024.MULTi.1080p.WEB.x264-GROUP";
    let preview = mynou::irc::preview_text(&cfg, "local", &line("Films", name, 468190)).unwrap();
    assert_eq!(
        preview.get("lookup_started").unwrap().as_bool(),
        Some(false)
    );
    assert_eq!(fixture.searches(), 0);
    let before = irc_support::bytes(&cfg.store_dir);
    let queued = receive(&engine, &line("Films", name, 468190)).unwrap();
    assert_eq!(queued.get("queued").unwrap().as_bool(), Some(true));
    assert_eq!(
        queued
            .get("candidate")
            .unwrap()
            .get("torrent_id")
            .unwrap()
            .as_str(),
        Some("468190")
    );
    // The same ID announced again while waiting is not queued twice.
    let again = receive(&engine, &line("Films", name, 468190)).unwrap();
    assert_eq!(again.get("queued").unwrap().as_bool(), Some(false));
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);

    // A neighbouring ID with another identity must not be picked.
    fixture.route(
        "/api",
        200,
        &feed(&[
            item(468189, "999", &doubled(&"b".repeat(40))),
            item(468190, "603", &doubled(HASH)),
        ]),
    );
    movie_catalog(&fixture);
    let (outcome, report) = resolve(&engine);
    assert_eq!(outcome, "recorded", "{report:?}");
    let rows = history(&engine);
    assert_eq!(rows.len(), 1);
    let a = rows[0].get("announcement").unwrap();
    assert_eq!(a.get("info_hash").unwrap().as_str(), Some(HASH));
    assert_eq!(a.get("tmdb_id").unwrap().as_str(), Some("603"));
    assert_eq!(
        a.get("media_title").unwrap().as_str(),
        Some("Fixture Movie")
    );
    assert_eq!(a.get("title").unwrap().as_str(), Some(name));
    assert_eq!(
        rows[0].get("evaluations").unwrap().as_array().unwrap()[0]
            .get("outcome")
            .unwrap()
            .as_str(),
        Some("matched")
    );
    let shown = mynou::json::stringify(&engine.irc_announcements(0, 50).unwrap());
    let reported = mynou::json::stringify(&report);
    for text in [
        shown,
        reported,
        mynou::json::stringify(&engine.irc_sources().unwrap()),
    ] {
        assert!(!text.contains("SECRETKEY") && !text.contains("download"));
    }
    // A later announcement of the same release is a restart-safe duplicate.
    drop(engine);
    let engine = Engine::open(cfg.clone()).unwrap();
    receive(&engine, &line("Films", name, 468190)).unwrap();
    assert_eq!(resolve(&engine).0, "duplicate");
    assert_eq!(history(&engine).len(), 1);
    assert!(!dir.0.join("jobs").join("downloads").exists());
}

#[test]
fn missing_ambiguous_or_unverifiable_identities_never_enter_history() {
    let (_dir, fixture, _cfg, engine) = setup();
    movie_catalog(&fixture);
    let name = "Fixture.Movie.2024.1080p.WEB.x264-GROUP";
    for (items, expected) in [
        (
            vec![item(7, "603", HASH), item(7, "603", HASH)],
            "ambiguous",
        ),
        (vec![item(7, "0", HASH)], "identity_rejected"),
        (vec![item(7, "0603", HASH)], "identity_rejected"),
        (vec![item(7, "603", "abc")], "identity_rejected"),
        (vec![item(7, "603", &doubled("zz"))], "identity_rejected"),
        (vec![item(7, "603", &"7a".repeat(40))], "identity_rejected"),
    ] {
        fixture.route("/api", 200, &feed(&items));
        receive(&engine, &line("Films", name, 7)).unwrap();
        let (outcome, report) = resolve(&engine);
        assert_eq!(outcome, expected);
        assert_eq!(report.get("retrying").unwrap().as_bool(), Some(false));
    }
}

#[test]
fn unlisted_releases_and_failing_services_retry_a_bounded_number_of_times() {
    let name = "Fixture.Movie.2024.1080p.WEB.x264-GROUP";
    for (status, body, catalog, expected) in [
        (200, feed(&[item(8, "603", HASH)]), true, "not_found"),
        (503, String::new(), true, "lookup_failed"),
        (
            200,
            feed(&[item(7, "603", HASH)]),
            false,
            "catalog_unavailable",
        ),
    ] {
        let (_dir, fixture, _cfg, engine) = setup();
        if catalog {
            movie_catalog(&fixture);
        }
        fixture.route("/api", status, &body);
        receive(&engine, &line("Films", name, 7)).unwrap();
        let (outcome, report) = resolve(&engine);
        assert_eq!(outcome, expected);
        assert_eq!(report.get("retrying").unwrap().as_bool(), Some(true));
        assert_eq!(resolve(&engine).0, "idle", "retries wait for their delay");
        let lookup = engine.irc_sources().unwrap().get("lookup").unwrap().clone();
        assert_eq!(lookup.get("pending").unwrap().as_u64(), Some(1));
        assert!(history(&engine).is_empty());
        assert!(fixture.searches() <= 1);
    }
}

#[test]
fn episodes_need_one_explicit_number_and_other_categories_are_rejected() {
    let (_dir, fixture, cfg, engine) = setup();
    for bad in [
        line("Series", "Fixture.Show.E12.MULTi.1080p.WEB-GROUP", 1),
        line("Series", "Fixture.Show.S06.MULTi.1080p.WEB-GROUP", 1),
        line("Series", "Fixture.Show.S01E01E02.1080p.WEB-GROUP", 1),
        line("Series", "Fixture.Show.S01E01.S01E02.1080p.WEB-GROUP", 1),
        line("Jeux", "Fixture.Game-GROUP", 1),
        line("Films", "Fixture.Movie.2024.1080p", 1)
            .replace(PREFIX, "https://other.example/torrents/"),
        line("Films", "Fixture.Movie.2024.1080p", 1) + "?passkey=private",
        line("Films", "Fixture.Movie.2024.1080p", 1)
            .replace(&format!("{PREFIX}1"), &format!("{PREFIX}01")),
        line("Films", "Fixture.Movie.2024.1080p", 1).replace(" par ", " by "),
        line("Films", "Fixture.Movie.2024.1080p", 1).replace("[0%]", "[0]"),
        line("Films", "https://x.example/?token=private", 1),
    ] {
        let error = engine
            .irc_receive_text("local", irc_support::SENDER, "#announces", &bad)
            .unwrap_err();
        assert!(!error.contains("private") && !error.contains("example"));
        assert!(mynou::irc::preview_text(&cfg, "local", &bad).is_err());
    }
    // Wrong sender and unrelated chatter stay outside the queue.
    let good = line("Series", "Fixture.Show.S01E05.MULTi.1080p.WEB-GROUP", 2);
    assert!(
        engine
            .irc_receive_text("local", "wrong!u@h", "#announces", &good)
            .is_err()
    );
    assert!(receive(&engine, "hello").is_none());
    // An exclusive tag without a pre time is part of the same grammar.
    let exclusive = good
        .replace("\u{3}04[NEW]\u{3}", " [EXCLUSiVE]")
        .replace(" [pre 0h 1m 52s]", "");
    let queued = receive(&engine, &exclusive).unwrap();
    let candidate = queued.get("candidate").unwrap();
    assert_eq!(candidate.get("kind").unwrap().as_str(), Some("episode"));
    assert_eq!(candidate.get("season").unwrap().as_u64(), Some(1));
    assert_eq!(candidate.get("episode").unwrap().as_u64(), Some(5));
    fixture.route("/api", 200, &feed(&[item(2, "95480", &doubled(HASH))]));
    fixture.route(
        "/3/tv/95480",
        200,
        r#"{"id":95480,"name":"Fixture Show","first_air_date":"2022-04-01"}"#,
    );
    assert_eq!(resolve(&engine).0, "recorded");
    let a = history(&engine)[0].get("announcement").unwrap().clone();
    assert_eq!(a.get("kind").unwrap().as_str(), Some("episode"));
    assert_eq!(a.get("year").unwrap().as_u64(), Some(2022));
    assert_eq!(a.get("episode").unwrap().as_u64(), Some(5));
}

#[test]
fn the_lookup_must_name_a_configured_torznab_indexer() {
    let dir = Directory::new();
    let mut v = settings(&dir, "http://127.0.0.1:1");
    let Value::Array(indexers) = v.get_mut("indexers").unwrap() else {
        panic!()
    };
    indexers[0].insert("kind", "rss");
    assert!(config::from_json(&v, &dir.0).is_err());
    for (key, value) in [
        ("details_prefix", "http://tracker.example/torrents/"),
        ("details_prefix", "https://tracker.example/torrents"),
        ("details_prefix", "https://tracker.example/t/?a=1/"),
        ("lookup_indexer", "Unknown Indexer"),
    ] {
        let mut v = settings(&dir, "http://127.0.0.1:1");
        let Value::Array(sources) = v.get_mut("irc").unwrap().get_mut("sources").unwrap() else {
            panic!()
        };
        let mut f = format_json();
        f.insert(key, value);
        sources[0].insert("announcement_format", f);
        assert!(config::from_json(&v, &dir.0).is_err(), "{key}={value}");
    }
}
