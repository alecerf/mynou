//! Feed watching uses synthetic loopback feeds and local jobs. No real source,
//! tracker or media is contacted.
mod api_support;
mod irc_routing_support;
mod irc_support;
mod library_support;
#[allow(dead_code)]
mod transfer_support;

use api_support::{Server, TOKEN};
use library_support::Directory;
use mynou::{
    config,
    engine::{Engine, lock},
    indexers::ControlRequest,
    integrations,
    json::{self, Value},
    store::{self, Job, RecordedRelease, Request},
};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: String,
    body: String,
}

struct Feed {
    url: String,
    reply: Arc<Mutex<Reply>>,
    requests: Arc<AtomicUsize>,
    lines: Arc<Mutex<Vec<String>>>,
    hold: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Feed {
    fn open(body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let reply = Arc::new(Mutex::new(Reply {
            status: 200,
            headers: String::new(),
            body,
        }));
        let requests = Arc::new(AtomicUsize::new(0));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let hold = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, q, l, h, s) = (
            reply.clone(),
            requests.clone(),
            lines.clone(),
            hold.clone(),
            stop.clone(),
        );
        let thread = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("Feed fixture accept failed: {error}"),
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
                    assert!(head.len() < 16_384);
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                l.lock()
                    .unwrap()
                    .push(head.lines().next().unwrap().to_owned());
                q.fetch_add(1, Ordering::AcqRel);
                let deadline = Instant::now() + Duration::from_secs(5);
                while h.load(Ordering::Acquire) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(2));
                }
                let reply = r.lock().unwrap().clone();
                // The client may stop waiting before a held reply is written.
                let _ = write!(
                    stream,
                    "HTTP/1.1 {} Fixture\r\n{}Content-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reply.headers,
                    reply.body.len(),
                    reply.body
                );
            }
        });
        Self {
            url,
            reply,
            requests,
            lines,
            hold,
            stop,
            thread: Some(thread),
        }
    }
    fn set(&self, body: String) {
        self.respond(200, "", body);
    }
    fn respond(&self, status: u16, headers: &str, body: String) {
        *self.reply.lock().unwrap() = Reply {
            status,
            headers: headers.into(),
            body,
        };
    }
    fn requests(&self) -> usize {
        self.requests.load(Ordering::Acquire)
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn hash(n: u64) -> String {
    format!("{n:040x}")
}
fn magnet(n: u64) -> String {
    format!("magnet:?xt=urn:btih:{}", hash(n))
}
fn item(title: &str, n: u64, seeders: u32) -> String {
    format!(
        "<item><title>{title}</title><guid>fixture-guid-{n}</guid><enclosure url=\"{}\" length=\"1\" type=\"application/x-bittorrent\"/><seeders>{seeders}</seeders></item>",
        magnet(n)
    )
}
fn rss(items: &[String]) -> String {
    format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>fixture</title>{}</channel></rss>",
        items.concat()
    )
}
fn movie(title: &str, id: u64) -> Request {
    Request {
        kind: "movie".into(),
        title: title.into(),
        year: 2024,
        season: 0,
        episode: 0,
        tmdb_id: Some(id),
        source_numbering: None,
        source_path: None,
        source_url: None,
    }
}
fn source(id: &str, feed: &Feed, kind: &str) -> Value {
    let mut s = Value::object();
    s.insert("id", id);
    s.insert("name", id);
    s.insert("kind", kind);
    s.insert("url", format!("{}/feed", feed.url));
    s.insert("api_key_env", "MYNOU_ABSENT_FEED_FIXTURE_KEY_110");
    let mut watch = Value::object();
    watch.insert("enabled", true);
    watch.insert("interval_secs", 60_u32);
    s.insert("watch", watch);
    s
}
fn configuration(dir: &Directory, sources: Vec<Value>) -> Value {
    let mut v = config::default_json();
    v.insert("listen", "127.0.0.1:0");
    v.insert("store_dir", dir.0.join("jobs").to_str().unwrap());
    v.insert("indexers", Value::Array(sources));
    v
}
fn open(dir: &Directory, sources: Vec<Value>) -> Arc<Engine> {
    Engine::open_for_management(config::from_json(&configuration(dir, sources), &dir.0).unwrap())
        .unwrap()
}
fn submit(engine: &Engine, request: Request) -> Job {
    // A rejected fixture request is reported without echoing the error.
    engine
        .submit(request)
        .map_err(|_| ())
        .expect("The fixture request was rejected")
        .remove(0)
}
fn state_dir(dir: &Directory) -> PathBuf {
    dir.0.join("jobs")
}
fn stored(engine: &Engine, id: &str) -> Job {
    lock(&engine.store).unwrap().get(id).unwrap()
}
fn first(report: &Value) -> &Value {
    &report.get("sources").unwrap().as_array().unwrap()[0]
}
fn outcome(row: &Value) -> &str {
    row.get("outcome").unwrap().as_str().unwrap()
}
fn num(row: &Value, key: &str) -> u64 {
    row.get(key).unwrap().as_u64().unwrap()
}
fn text(row: &Value, key: &str) -> String {
    row.get(key).unwrap().as_str().unwrap().to_owned()
}
fn status(engine: &Engine, index: usize) -> Value {
    engine
        .feeds()
        .unwrap()
        .get("sources")
        .unwrap()
        .as_array()
        .unwrap()[index]
        .clone()
}
fn no_private(v: &Value) {
    let s = json::stringify(v);
    let (first, second) = (hash(1), hash(2));
    for private in [
        "127.0.0.1",
        "http://",
        "magnet:",
        "Fixture",
        "fixture-guid",
        "MYNOU_ABSENT",
        first.as_str(),
        second.as_str(),
    ] {
        assert!(!s.contains(private), "Feed report exposed {private}");
    }
}
fn reviewed(engine: &Engine, id: &str, action: &str) -> ControlRequest {
    let mut q = ControlRequest {
        action: action.into(),
        apply: false,
        plan_id: None,
    };
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
fn first_poll_baselines_and_later_polls_route_only_new_entries_to_existing_demand() {
    let dir = Directory::new();
    let feed = Feed::open(rss(&[item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5)]));
    let engine = open(&dir, vec![source("feed-a", &feed, "rss")]);
    let job = submit(&engine, movie("Fixture Movie", 7));

    let report = engine.feeds_poll(None, true).unwrap();
    let row = first(&report);
    assert_eq!(outcome(row), "ok");
    assert_eq!(row.get("baseline"), Some(&Value::Bool(true)));
    assert_eq!((num(row, "observed"), num(row, "new")), (1, 0));
    assert!(stored(&engine, &job.id).acquisition_url.is_none());
    // The poll asks for the feed itself, never for a title.
    assert!(
        feed.lines
            .lock()
            .unwrap()
            .iter()
            .all(|line| line.starts_with("GET /feed ") && !line.contains("q="))
    );

    feed.set(rss(&[
        item("Other.Movie.2024.1080p.WEB-DL", 3, 9),
        item("Fixture.Movie.2024.1080p.BluRay", 2, 5),
        item("Fixture.Movie.2024.720p.WEB-DL", 4, 0),
        item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5),
    ]));
    let report = engine.feeds_poll(None, true).unwrap();
    let row = first(&report);
    assert_eq!(outcome(row), "ok");
    assert_eq!(row.get("baseline"), Some(&Value::Bool(false)));
    assert_eq!((num(row, "observed"), num(row, "new")), (4, 3));
    assert_eq!(num(row, "routed"), 1);
    let routed = stored(&engine, &job.id);
    assert_eq!(routed.state, "queued");
    assert_eq!(routed.acquisition_url, Some(magnet(2)));
    assert_eq!(
        routed.release,
        Some(RecordedRelease {
            title: "Fixture.Movie.2024.1080p.BluRay".into(),
            profile: "any".into(),
        })
    );
    // Entries without matching demand never create requests.
    assert_eq!(lock(&engine.store).unwrap().list().len(), 1);

    let report = engine.feeds_poll(None, true).unwrap();
    let row = first(&report);
    assert_eq!((num(row, "new"), num(row, "routed")), (0, 0));
    assert_eq!(stored(&engine, &job.id), routed);
    no_private(&engine.feeds().unwrap());
    no_private(&report);
}

#[test]
fn restart_keeps_the_cursor_and_never_replays_history() {
    let dir = Directory::new();
    let feed = Feed::open(rss(&[item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5)]));
    let sources = || vec![source("feed-a", &feed, "rss")];
    let engine = open(&dir, sources());
    assert_eq!(
        outcome(first(&engine.feeds_poll(None, true).unwrap())),
        "ok"
    );
    // Published while nobody wanted it: considered, never acquired later.
    feed.set(rss(&[
        item("Fixture.Movie.2024.1080p.BluRay", 2, 5),
        item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5),
    ]));
    let report = engine.feeds_poll(None, true).unwrap();
    assert_eq!(
        (num(first(&report), "new"), num(first(&report), "routed")),
        (1, 0)
    );
    let job = submit(&engine, movie("Fixture Movie", 7));
    let path = state_dir(&dir).join("feeds.bin");
    drop(engine);

    let private = fs::read(&path).unwrap();
    let private = String::from_utf8_lossy(&private);
    let second = hash(2);
    for forbidden in [
        "Fixture",
        "127.0.0.1",
        "magnet",
        "fixture-guid",
        second.as_str(),
    ] {
        assert!(
            !private.contains(forbidden),
            "Feed state stores {forbidden}"
        );
    }
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);

    let engine = open(&dir, sources());
    let row = status(&engine, 0);
    assert_eq!(row.get("baselined"), Some(&Value::Bool(true)));
    assert_eq!(text(&row, "successes"), "2");
    assert_eq!(num(&row, "retained_identities"), 2);
    let report = engine.feeds_poll(None, true).unwrap();
    let row = first(&report);
    assert_eq!(row.get("baseline"), Some(&Value::Bool(false)));
    assert_eq!((num(row, "new"), num(row, "routed")), (0, 0));
    assert!(stored(&engine, &job.id).acquisition_url.is_none());
    assert_eq!(text(&status(&engine, 0), "successes"), "3");
}

#[test]
fn a_failed_commit_reports_failure_and_the_entries_are_observed_again() {
    let dir = Directory::new();
    let feed = Feed::open(rss(&[item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5)]));
    let engine = open(&dir, vec![source("feed-a", &feed, "rss")]);
    let job = submit(&engine, movie("Fixture Movie", 7));
    assert_eq!(
        outcome(first(&engine.feeds_poll(None, true).unwrap())),
        "ok"
    );
    feed.set(rss(&[
        item("Fixture.Movie.2024.1080p.BluRay", 2, 5),
        item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5),
    ]));
    let directory = state_dir(&dir);
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).unwrap();
    let report = engine.feeds_poll(None, true);
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let report = report.unwrap();
    let row = first(&report);
    assert_eq!(outcome(row), "storage_failed");
    assert_eq!(num(row, "routed"), 0);
    assert!(stored(&engine, &job.id).acquisition_url.is_none());
    let health = status(&engine, 0);
    assert_eq!(text(&health, "successes"), "1");
    assert_eq!(text(&health, "last_outcome"), "storage_failed");
    assert_eq!(num(&health, "consecutive_failures"), 1);

    let report = engine.feeds_poll(None, true).unwrap();
    let row = first(&report);
    assert_eq!(outcome(row), "ok");
    assert_eq!((num(row, "new"), num(row, "routed")), (1, 1));
    assert_eq!(stored(&engine, &job.id).acquisition_url, Some(magnet(2)));
    assert_eq!(num(&status(&engine, 0), "consecutive_failures"), 0);
}

#[test]
fn partial_source_and_item_failures_are_isolated() {
    let dir = Directory::new();
    let down = Feed::open(rss(&[item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5)]));
    down.respond(503, "", "unavailable".into());
    let healthy = Feed::open(rss(&[
        item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5),
        "<item><link>magnet:?xt=urn:btih:ffffffffffffffffffffffffffffffffffffffff</link></item>"
            .into(),
    ]));
    let engine = open(
        &dir,
        vec![
            source("feed-a", &down, "rss"),
            source("feed-b", &healthy, "rss"),
        ],
    );
    let job = submit(&engine, movie("Fixture Movie", 7));
    let report = engine.feeds_poll(None, true).unwrap();
    let rows = report.get("sources").unwrap().as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(outcome(&rows[0]), "response_rejected");
    assert_eq!(outcome(&rows[1]), "ok");
    assert_eq!(num(&rows[1], "skipped"), 1);
    assert_eq!(
        status(&engine, 0).get("baselined"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        status(&engine, 1).get("baselined"),
        Some(&Value::Bool(true))
    );

    // A source that recovers is baselined first, even with matching demand.
    down.set(rss(&[
        item("Fixture.Movie.2024.1080p.BluRay", 2, 5),
        item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5),
    ]));
    let report = engine.feeds_poll(Some("feed-a"), true).unwrap();
    assert_eq!(report.get("polled").and_then(Value::as_u64), Some(1));
    let row = first(&report);
    assert_eq!(outcome(row), "ok");
    assert_eq!(row.get("baseline"), Some(&Value::Bool(true)));
    assert!(stored(&engine, &job.id).acquisition_url.is_none());

    // A malformed document changes nothing.
    healthy.set("<rss><channel><item><title>truncated".into());
    let before = text(&status(&engine, 1), "successes");
    let report = engine.feeds_poll(Some("feed-b"), true).unwrap();
    assert_eq!(outcome(first(&report)), "parse_rejected");
    assert_eq!(text(&status(&engine, 1), "successes"), before);
    assert!(engine.feeds_poll(Some("missing"), true).is_err());
}

#[test]
fn a_rate_limited_source_rests_even_when_polling_is_forced() {
    let dir = Directory::new();
    let feed = Feed::open(rss(&[]));
    feed.respond(429, "Retry-After: 120\r\n", String::new());
    let engine = open(&dir, vec![source("feed-a", &feed, "rss")]);
    let report = engine.feeds_poll(None, true).unwrap();
    assert_eq!(outcome(first(&report)), "rate_limited");
    assert_eq!(feed.requests(), 1);
    let row = status(&engine, 0);
    assert!(num(&row, "cooldown_secs") > 60);
    assert!(num(&row, "next_poll_in_secs") > 100);
    assert_eq!(row.get("baselined"), Some(&Value::Bool(false)));

    feed.set(rss(&[item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5)]));
    let report = engine.feeds_poll(None, true).unwrap();
    assert_eq!(outcome(first(&report)), "rate_limited");
    assert_eq!(feed.requests(), 1);
    // The scheduled pass honors the delay without forcing.
    let report = engine.feeds_poll(None, false).unwrap();
    assert_eq!(report.get("polled").and_then(Value::as_u64), Some(0));
    assert_eq!(feed.requests(), 1);
}

#[test]
fn intervals_pausing_and_configuration_disable_polling_and_rebaseline() {
    let dir = Directory::new();
    let feed = Feed::open(rss(&[item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5)]));
    let engine = open(&dir, vec![source("feed-a", &feed, "rss")]);
    let job = submit(&engine, movie("Fixture Movie", 7));
    assert_eq!(
        outcome(first(&engine.feeds_poll(None, false).unwrap())),
        "ok"
    );
    let report = engine.feeds_poll(None, false).unwrap();
    assert_eq!(report.get("polled").and_then(Value::as_u64), Some(0));
    assert_eq!(report.get("not_due").and_then(Value::as_u64), Some(1));
    assert_eq!(feed.requests(), 1);
    let next = num(&status(&engine, 0), "next_poll_in_secs");
    assert!((50..=60).contains(&next), "next poll in {next}");

    let pause = reviewed(&engine, "feed-a", "pause");
    engine.indexer_control("feed-a", &pause).unwrap();
    let report = engine.feeds_poll(None, true).unwrap();
    assert_eq!(report.get("polled").and_then(Value::as_u64), Some(0));
    assert_eq!(feed.requests(), 1);
    let row = status(&engine, 0);
    assert_eq!(row.get("active"), Some(&Value::Bool(false)));
    assert_eq!(row.get("baselined"), Some(&Value::Bool(false)));
    assert_eq!(text(&row, "last_outcome"), "disabled");

    // What the source published while paused is not new on resumption.
    feed.set(rss(&[
        item("Fixture.Movie.2024.1080p.BluRay", 2, 5),
        item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5),
    ]));
    let enable = reviewed(&engine, "feed-a", "enable");
    engine.indexer_control("feed-a", &enable).unwrap();
    let report = engine.feeds_poll(None, true).unwrap();
    assert_eq!(first(&report).get("baseline"), Some(&Value::Bool(true)));
    assert!(stored(&engine, &job.id).acquisition_url.is_none());
    drop(engine);

    let mut disabled = source("feed-a", &feed, "rss");
    disabled.get_mut("watch").unwrap().insert("enabled", false);
    let engine = open(&dir, vec![disabled]);
    let requests = feed.requests();
    let row = status(&engine, 0);
    assert_eq!(row.get("watch_enabled"), Some(&Value::Bool(false)));
    assert_eq!(row.get("baselined"), Some(&Value::Bool(false)));
    let report = engine.feeds_poll(None, true).unwrap();
    assert_eq!(report.get("polled").and_then(Value::as_u64), Some(0));
    assert!(engine.feeds_poll(Some("feed-a"), true).is_err());
    assert_eq!(feed.requests(), requests);
}

#[test]
fn watch_configuration_is_strict_and_routing_needs_downloads() {
    let dir = Directory::new();
    let feed = Feed::open(rss(&[]));
    let build = |edit: &dyn Fn(&mut Value)| {
        let mut s = source("feed-a", &feed, "rss");
        edit(&mut s);
        config::from_json(&configuration(&dir, vec![s]), &dir.0)
    };
    assert!(build(&|_| {}).is_ok());
    assert!(build(&|s| s.insert("kind", "json")).is_err());
    assert!(build(&|s| s.get_mut("watch").unwrap().insert("interval_secs", 59_u32)).is_err());
    assert!(
        build(&|s| s
            .get_mut("watch")
            .unwrap()
            .insert("interval_secs", 86_401_u32))
        .is_err()
    );
    assert!(build(&|s| s.get_mut("watch").unwrap().insert("extra", true)).is_err());
    assert!(build(&|s| s.get_mut("watch").unwrap().insert("enabled", "yes")).is_err());
    let mut v = configuration(&dir, vec![source("feed-a", &feed, "rss")]);
    v.get_mut("downloads").unwrap().insert("enabled", false);
    let engine = Engine::open_for_management(config::from_json(&v, &dir.0).unwrap()).unwrap();
    assert!(engine.feeds_poll(None, true).is_err());
    assert_eq!(feed.requests(), 0);
    let config = engine.config.clone();
    drop(engine);
    let preview = Engine::open_for_preview(config).unwrap();
    assert!(preview.feeds_poll(None, true).is_err());
}

#[test]
fn torznab_polls_the_latest_feed_with_its_seeders() {
    let dir = Directory::new();
    let entry = |title: &str, n: u64| {
        format!(
            "<item><title>{title}</title><link>{}</link><torznab:attr name=\"seeders\" value=\"4\"/></item>",
            magnet(n)
        )
    };
    let document = |items: String| {
        format!(
            "<?xml version=\"1.0\"?><rss version=\"2.0\" xmlns:torznab=\"http://torznab.com/schemas/2015/feed\"><channel>{items}</channel></rss>"
        )
    };
    let feed = Feed::open(document(entry("Fixture.Movie.2024.1080p.WEB-DL", 1)));
    let engine = open(&dir, vec![source("feed-a", &feed, "torznab")]);
    let job = submit(&engine, movie("Fixture Movie", 7));
    assert_eq!(
        outcome(first(&engine.feeds_poll(None, true).unwrap())),
        "ok"
    );
    assert!(
        feed.lines.lock().unwrap()[0].starts_with("GET /feed?t=search "),
        "{:?}",
        feed.lines.lock().unwrap()
    );
    feed.set(document(
        entry("Fixture.Movie.2024.2160p.WEB-DL", 2) + &entry("Fixture.Movie.2024.1080p.WEB-DL", 1),
    ));
    let report = engine.feeds_poll(None, true).unwrap();
    assert_eq!(num(first(&report), "routed"), 1);
    assert_eq!(stored(&engine, &job.id).acquisition_url, Some(magnet(2)));
}

#[test]
fn leased_selected_and_unrelated_work_is_never_overridden() {
    let dir = Directory::new();
    let feed = Feed::open(rss(&[]));
    let engine = open(&dir, vec![source("feed-a", &feed, "rss")]);
    let jobs: Vec<Job> = [
        ("Fixture Movie", 7),
        ("Second Movie", 8),
        ("Third Movie", 9),
    ]
    .into_iter()
    .map(|(title, id)| submit(&engine, movie(title, id)))
    .collect();
    assert_eq!(
        outcome(first(&engine.feeds_poll(None, true).unwrap())),
        "ok"
    );
    // A worker already searches for one job and another path selected a second.
    let (leased, selected) = {
        let mut store = lock(&engine.store).unwrap();
        let leased = store.claim(store::now(), 60).unwrap().unwrap();
        let mut selected = jobs.iter().find(|job| job.id != leased.id).unwrap().clone();
        selected.acquisition_url = Some(magnet(77));
        selected.release = Some(RecordedRelease {
            title: "Selected.Release.2024.1080p.BluRay".into(),
            profile: "any".into(),
        });
        store.update(selected.clone()).unwrap();
        (leased, selected)
    };
    let free = jobs
        .iter()
        .find(|job| job.id != leased.id && job.id != selected.id)
        .unwrap();
    let entries: Vec<String> = jobs
        .iter()
        .enumerate()
        .map(|(n, job)| {
            item(
                &format!("{}.2024.1080p.BluRay", job.request.title.replace(' ', ".")),
                n as u64 + 2,
                5,
            )
        })
        .collect();
    feed.set(rss(&entries));
    let report = engine.feeds_poll(None, true).unwrap();
    assert_eq!(num(first(&report), "new"), 3);
    assert_eq!(num(first(&report), "routed"), 1);
    assert!(stored(&engine, &leased.id).acquisition_url.is_none());
    assert_eq!(
        stored(&engine, &selected.id).acquisition_url,
        Some(magnet(77))
    );
    assert!(stored(&engine, &free.id).acquisition_url.is_some());
}

#[test]
fn a_search_never_waits_for_a_polling_source_and_both_finish() {
    let dir = Directory::new();
    let feed = Feed::open(rss(&[item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5)]));
    let engine = open(&dir, vec![source("feed-a", &feed, "rss")]);
    feed.hold.store(true, Ordering::Release);
    let poller = {
        let engine = engine.clone();
        thread::spawn(move || engine.feeds_poll(None, true).unwrap())
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while feed.requests() == 0 {
        assert!(Instant::now() < deadline, "The poll never reached the feed");
        thread::sleep(Duration::from_millis(2));
    }
    // The source is busy: the search fails at once instead of queueing.
    assert!(integrations::search(&engine.config, &movie("Fixture Movie", 7)).is_err());
    assert_eq!(feed.requests(), 1);
    feed.hold.store(false, Ordering::Release);
    let report = poller.join().unwrap();
    assert_eq!(outcome(first(&report)), "ok");
    assert_eq!(feed.requests(), 1);
}

#[test]
fn irc_and_feed_share_one_selection_and_the_later_path_cannot_replace_it() {
    let dir = Directory::new();
    let feed = Feed::open(rss(&[item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5)]));
    let mut v = transfer_support::configuration();
    v.insert("store_dir", dir.0.join("jobs").to_str().unwrap());
    let mut settings = irc_support::settings("irc://127.0.0.1:1");
    let Value::Array(sources) = settings.get_mut("sources").unwrap() else {
        panic!()
    };
    sources[0].insert("magnet_template", "magnet:?xt={xt}&x.pe=127.0.0.1:1");
    let Value::Array(rules) = settings.get_mut("rules").unwrap() else {
        panic!()
    };
    rules[0].insert("action", "grab");
    v.insert("irc", settings);
    v.insert(
        "indexers",
        Value::Array(vec![source("feed-a", &feed, "rss")]),
    );
    let engine = Engine::open(config::from_json(&v, &dir.0).unwrap()).unwrap();
    let admitted = submit(&engine, irc_routing_support::request(7));
    irc_routing_support::receive(&engine, &"1".repeat(40), 7);
    assert_eq!(
        outcome(first(&engine.feeds_poll(None, true).unwrap())),
        "ok"
    );
    feed.set(rss(&[
        item("Fixture.Movie.2024.1080p.BluRay", 2, 5),
        item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5),
    ]));
    let report = engine.feeds_poll(None, true).unwrap();
    assert_eq!(num(first(&report), "routed"), 1);
    let routed = stored(&engine, &admitted.id);
    assert_eq!(routed.acquisition_url, Some(magnet(2)));
    assert!(routed.irc_origin.is_none());
    let irc = engine.irc_route_pending().unwrap();
    assert_eq!(irc.get("routed"), Some(&Value::Bool(false)));
    assert_eq!(
        irc.get("outcome").and_then(Value::as_str),
        Some("waiting_for_admitted_job")
    );
    assert_eq!(stored(&engine, &admitted.id), routed);
}

#[test]
fn monitored_owned_media_takes_a_strictly_better_new_entry_once() {
    let dir = Directory::new();
    let feed = Feed::open(rss(&[]));
    let mut v =
        library_support::configuration(None, r#"{"resolutions":[1080,720],"languages":["en"]}"#);
    v.insert(
        "monitoring",
        json::parse(r#"{"enabled":true,"interval_secs":3600,"max_checks":8}"#).unwrap(),
    );
    v.get_mut("downloads").unwrap().insert("enabled", true);
    v.insert(
        "indexers",
        Value::Array(vec![source("feed-a", &feed, "rss")]),
    );
    let engine = Engine::open_for_management(config::from_json(&v, &dir.0).unwrap()).unwrap();
    let original = library_support::local_import(&engine, &dir.0, "Fixture Movie");
    let original = engine
        .set_baseline(&original.id, "Fixture.Movie.2024.720p.WEB-DL.EN")
        .unwrap();
    assert_eq!(
        outcome(first(&engine.feeds_poll(None, true).unwrap())),
        "ok"
    );
    feed.set(rss(&[
        item("Fixture.Movie.2024.1080p.WEB-DL.EN", 2, 5),
        item("Fixture.Movie.2024.720p.WEB-DL.EN", 3, 5),
        item("Fixture.Movie.2024.480p.WEB-DL.EN", 4, 5),
    ]));
    let report = engine.feeds_poll(None, true).unwrap();
    let row = first(&report);
    assert_eq!((num(row, "new"), num(row, "routed")), (3, 0));
    assert_eq!(num(row, "upgrades_queued"), 1);
    let children: Vec<_> = lock(&engine.store)
        .unwrap()
        .list()
        .into_iter()
        .filter(|job| job.upgrade_parent.as_deref() == Some(original.id.as_str()))
        .collect();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].request.source_url, Some(magnet(2)));
    let report = engine.feeds_poll(None, true).unwrap();
    assert_eq!(num(first(&report), "upgrades_queued"), 0);
}

#[test]
fn protected_status_and_offline_cli_expose_only_redacted_freshness() {
    let dir = Directory::new();
    let feed = Feed::open(rss(&[item("Fixture.Movie.2024.1080p.WEB-DL", 1, 5)]));
    let mut v = configuration(&dir, vec![source("feed-a", &feed, "rss")]);
    v.get_mut("downloads").unwrap().insert("enabled", false);
    let server = Server::open(config::from_json(&v, &dir.0).unwrap());
    assert_eq!(server.call("GET", "/api/feeds", &[], "").status, 401);
    let reply = server.call(
        "GET",
        "/api/feeds",
        &[("Authorization", &format!("Bearer {TOKEN}"))],
        "",
    );
    assert_eq!(reply.status, 200);
    let report = json::parse(&reply.body).unwrap();
    no_private(&report);
    assert_eq!(report.get("routing_enabled"), Some(&Value::Bool(false)));
    let row = &report.get("sources").unwrap().as_array().unwrap()[0];
    assert_eq!(row.get("watch_enabled"), Some(&Value::Bool(true)));
    assert_eq!(text(row, "successes"), "0");
    v.insert("listen", server.authority.clone());
    drop(server);
    let path = dir.0.join("mynou.json");
    fs::write(&path, json::stringify(&v)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .env_remove("MYNOU_API_TOKEN")
        .args(["feeds", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    no_private(&json::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap());
    assert_eq!(feed.requests(), 0);
}
