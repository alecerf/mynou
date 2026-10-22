//! Automatic cross-seed discovery against a synthetic loopback Torznab source.
//! No real indexer, tracker or media is contacted.
#[allow(dead_code)]
mod transfer_support;
use mynou::{
    bencode::{self, Value as Bencode},
    config,
    crypto::sha1,
    engine::Engine,
    indexers::cross_seed::Run,
    json::{self, Value},
    net::HttpClient,
    server::Api,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use transfer_support::*;

const NAME: &str = "Show.S01E01.1080p.WEB-GRP";
const TITLE: &str = "Show S01E01 1080p WEB-GRP";

/// Serves `/api` (feed or search, chosen by `q`) and `/dl/NAME` metainfo.
struct Indexer {
    url: String,
    feed: Arc<Mutex<String>>,
    search: Arc<Mutex<String>>,
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    lines: Arc<Mutex<Vec<String>>>,
    failing: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Indexer {
    fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let feed = Arc::new(Mutex::new(rss(&[])));
        let search = Arc::new(Mutex::new(rss(&[])));
        let files = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let failing = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (f, s, d, l, e, x) = (
            feed.clone(),
            search.clone(),
            files.clone(),
            lines.clone(),
            failing.clone(),
            stop.clone(),
        );
        let thread = thread::spawn(move || {
            while !x.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("Indexer fixture accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    head.push(byte[0]);
                }
                let line = String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                l.lock().unwrap().push(line.clone());
                let target = line.split(' ').nth(1).unwrap_or_default();
                let (kind, body) = if let Some(name) = target.strip_prefix("/dl/") {
                    match d.lock().unwrap().get(name) {
                        Some(bytes) => ("application/x-bittorrent", Some(bytes.clone())),
                        None => ("text/plain", None),
                    }
                } else if target.contains("&q=") || target.contains("?q=") {
                    (
                        "application/rss+xml",
                        Some(s.lock().unwrap().clone().into_bytes()),
                    )
                } else {
                    (
                        "application/rss+xml",
                        Some(f.lock().unwrap().clone().into_bytes()),
                    )
                };
                let (status, body) = match body {
                    _ if e.load(Ordering::Acquire) => (503, Vec::new()),
                    Some(body) => (200, body),
                    None => (404, Vec::new()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .and_then(|()| stream.write_all(&body));
            }
        });
        Self {
            url,
            feed,
            search,
            files,
            lines,
            failing,
            stop,
            thread: Some(thread),
        }
    }
    fn downloads(&self) -> usize {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("/dl/"))
            .count()
    }
    fn requests(&self) -> usize {
        self.lines.lock().unwrap().len()
    }
    fn publish(&self, key: &str, metadata: &[u8]) {
        self.files
            .lock()
            .unwrap()
            .insert(key.into(), metadata.to_vec());
    }
}
impl Drop for Indexer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `(guid, title, download key, size, infohash)`.
type Item<'a> = (&'a str, &'a str, &'a str, Option<u64>, Option<&'a str>);
fn rss(items: &[Item]) -> String {
    let mut body = String::from(
        r#"<?xml version="1.0"?><rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel>"#,
    );
    for (guid, title, key, size, hash) in items {
        body.push_str(&format!(
            "<item><title>{title}</title><guid>{guid}</guid><enclosure url=\"/dl/{key}\" type=\"application/x-bittorrent\"/>"
        ));
        if let Some(size) = size {
            body.push_str(&format!("<size>{size}</size>"));
        }
        if let Some(hash) = hash {
            body.push_str(&format!(
                "<torznab:attr name=\"infohash\" value=\"{hash}\"/>"
            ));
        }
        body.push_str("</item>");
    }
    body.push_str("</channel></rss>");
    body
}

/// A private v1 multi-file torrent; `tag` changes only the identity.
fn metainfo(name: &str, files: &[(&str, &[u8])], tag: &str) -> (Vec<u8>, String) {
    let all: Vec<u8> = files.iter().flat_map(|(_, b)| b.iter().copied()).collect();
    let info = Bencode::Dict(BTreeMap::from([
        (b"name".to_vec(), Bencode::Bytes(name.as_bytes().to_vec())),
        (b"piece length".to_vec(), Bencode::Int(BLOCK as i64)),
        (
            b"pieces".to_vec(),
            Bencode::Bytes(all.chunks(BLOCK).flat_map(sha1).collect()),
        ),
        (b"private".to_vec(), Bencode::Int(1)),
        (b"source".to_vec(), Bencode::Bytes(tag.as_bytes().to_vec())),
        (
            b"files".to_vec(),
            Bencode::List(
                files
                    .iter()
                    .map(|(path, bytes)| {
                        Bencode::Dict(BTreeMap::from([
                            (b"length".to_vec(), Bencode::Int(bytes.len() as i64)),
                            (
                                b"path".to_vec(),
                                Bencode::List(vec![Bencode::Bytes(path.as_bytes().to_vec())]),
                            ),
                        ]))
                    })
                    .collect(),
            ),
        ),
    ]));
    let id = sha1(&bencode::encode(&info))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    (
        bencode::encode(&Bencode::Dict(BTreeMap::from([(b"info".to_vec(), info)]))),
        id,
    )
}

/// Deterministic synthetic bytes; each variant differs everywhere.
fn content(length: usize, variant: usize) -> Vec<u8> {
    (0..length)
        .map(|index| ((index * 131 + variant * 17) % 251) as u8)
        .collect()
}

fn transfer(engine: &Engine, id: &str) -> Option<Value> {
    engine.transfer(id).ok()
}
fn flag(value: &Value, key: &str) -> bool {
    value.get(key) == Some(&Value::Bool(true))
}
fn number(report: &Value, key: &str) -> u64 {
    report.get("indexers").and_then(Value::as_array).unwrap()[0]
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing {key} in {}", json::stringify(report)))
}

struct Fixture {
    scratch: Scratch,
    settings: Value,
    cfg: config::Config,
    source: String,
    video: Vec<u8>,
    nfo: Vec<u8>,
}

fn fixture(indexer: &Indexer) -> Fixture {
    let mut scratch = Scratch::new();
    scratch.0 = scratch.0.canonicalize().unwrap();
    let video = content(3 * BLOCK + 77, 1);
    let nfo = content(40, 2);
    let source = Torrent::multiple(
        &scratch.0,
        NAME,
        vec![
            (PathBuf::from("ep.mkv"), video.clone()),
            (PathBuf::from("info.nfo"), nfo.clone()),
        ],
    );
    let mut settings = configuration();
    let Value::Object(downloads) = settings.get_mut("downloads").unwrap() else {
        panic!("downloads must be an object");
    };
    downloads.insert("seed".into(), true.into());
    settings.insert(
        "indexers",
        json::parse(&format!(
            r#"[{{"name":"fixture","kind":"torznab","url":"{}/api","id":"tracker-b","api_key_env":"MYNOU_DISCOVERY_FIXTURE_KEY","cross_seed":{{"enabled":true}}}}]"#,
            indexer.url
        ))
        .unwrap(),
    );
    let cfg = config::from_json(&settings, &scratch.0).unwrap();
    fs::create_dir_all(&cfg.downloads.state_dir).unwrap();
    for (path, bytes) in &source.files {
        let destination = cfg.downloads.data_dir.join(&source.id).join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, bytes).unwrap();
    }
    fs::write(
        cfg.downloads
            .state_dir
            .join(format!("{}.torrent", source.id)),
        fs::read(&source.path).unwrap(),
    )
    .unwrap();
    Fixture {
        scratch,
        settings,
        cfg,
        source: source.id,
        video,
        nfo,
    }
}

fn open(f: &Fixture) -> Arc<Engine> {
    let engine = Engine::open(f.cfg.clone()).unwrap();
    wait(|| transfer(&engine, &f.source).is_some_and(|t| flag(&t, "ready")));
    engine
}

fn data_dirs(root: &Path) -> usize {
    fs::read_dir(root).map_or(0, |entries| entries.count())
}

#[test]
fn feeds_and_completion_searches_register_only_verified_layouts_once() {
    let indexer = Indexer::open();
    let f = fixture(&indexer);
    // Reordered files under another source tag: a genuine cross-seed.
    let (good, good_id) = metainfo(NAME, &[("info.nfo", &f.nfo), ("ep.mkv", &f.video)], "B");
    // Same name and sizes, different bytes: only verification can tell.
    let forged = content(f.video.len(), 9);
    let (bad, bad_id) = metainfo(NAME, &[("ep.mkv", &forged), ("info.nfo", &f.nfo)], "C");
    // Same name, different layout: refused before registration.
    let (other, other_id) = metainfo(NAME, &[("ep.mkv", &f.video[..BLOCK])], "D");
    // Found by the completion search: a subset with only the video.
    let (subset, subset_id) = metainfo(NAME, &[("ep.mkv", &f.video)], "E");
    for (key, bytes) in [
        ("good", &good),
        ("bad", &bad),
        ("other", &other),
        ("subset", &subset),
    ] {
        indexer.publish(key, bytes);
    }
    *indexer.feed.lock().unwrap() = rss(&[
        ("g1", TITLE, "good", None, None),
        ("g2", &format!("{NAME}.mkv"), "bad", None, None),
        ("g3", TITLE, "other", None, None),
        ("g4", "Unrelated Movie 2020 1080p", "missing", None, None),
        ("g5", TITLE, "missing", Some(1 << 40), None),
    ]);
    *indexer.search.lock().unwrap() = rss(&[
        ("s1", TITLE, "subset", None, None),
        ("s2", TITLE, "good", None, Some(&good_id)),
    ]);
    let engine = open(&f);

    // A dry run fetches and maps but registers and remembers nothing.
    let dry = engine
        .cross_seed_discover(&Run {
            dry_run: true,
            ..Run::default()
        })
        .unwrap();
    assert_eq!(number(&dry, "would_add"), 3, "{}", json::stringify(&dry));
    assert_eq!(number(&dry, "unmatched"), 1);
    assert_eq!(number(&dry, "metainfo_fetched"), 4);
    assert_eq!(indexer.downloads(), 4);
    for id in [&good_id, &bad_id, &subset_id] {
        assert!(transfer(&engine, id).is_none());
    }

    let report = engine.cross_seed_discover(&Run::default()).unwrap();
    let text = json::stringify(&report);
    assert_eq!(number(&report, "added"), 3, "{text}");
    assert_eq!(number(&report, "rejected"), 1, "{text}");
    assert_eq!(number(&report, "skipped"), 2, "{text}");
    assert_eq!(number(&report, "searches"), 1, "{text}");
    // The second search result was known by its infohash: never downloaded.
    assert_eq!(indexer.downloads(), 8);
    assert!(!text.contains("/dl/") && !text.contains(TITLE));
    assert!(transfer(&engine, &other_id).is_none());
    for id in [&good_id, &subset_id] {
        wait(|| transfer(&engine, id).is_some_and(|t| flag(&t, "ready")));
        let t = transfer(&engine, id).unwrap();
        assert!(flag(&t, "seed_only"));
        assert_eq!(
            t.get("cross_seed_source_id").and_then(Value::as_str),
            Some(f.source.as_str())
        );
    }
    wait(|| transfer(&engine, &bad_id).is_some_and(|t| flag(&t, "failed")));
    assert!(!flag(&transfer(&engine, &bad_id).unwrap(), "ready"));
    // No payload, copy or link was created for any candidate.
    assert_eq!(data_dirs(&f.cfg.downloads.data_dir), 1);

    // Remembered entries and searches need no further request or download.
    let before = indexer.requests();
    engine.cross_seed_discover(&Run::default()).unwrap();
    assert_eq!(indexer.requests(), before);
    drop(engine);

    let engine = open(&f);
    let repeat = engine
        .cross_seed_discover(&Run {
            force: true,
            ..Run::default()
        })
        .unwrap();
    assert_eq!(number(&repeat, "added"), 0, "{}", json::stringify(&repeat));
    // Forced: the feed and the search are read again, the search's own
    // candidates already exist, and nothing is downloaded twice except the
    // subset result without an advertised hash.
    assert_eq!(indexer.downloads(), 9);
    let status = engine.cross_seed_status().unwrap();
    assert_eq!(status.get("added_total").and_then(Value::as_str), Some("3"));
    let counts = status.get("counts").unwrap();
    assert_eq!(
        counts
            .get("rejected")
            .and_then(|r| r.get("layout_mismatch"))
            .and_then(Value::as_str),
        Some("1")
    );
    let triggers: Vec<_> = status
        .get("added")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|p| p.get("trigger").and_then(Value::as_str))
        .collect();
    assert_eq!(
        triggers.iter().filter(|t| **t == "completion").count(),
        1,
        "{triggers:?}"
    );

    // A paused source nominates nothing new.
    let (late, late_id) = metainfo(NAME, &[("ep.mkv", &f.video), ("info.nfo", &f.nfo)], "F");
    indexer.publish("late", &late);
    *indexer.feed.lock().unwrap() = rss(&[("g6", TITLE, "late", None, None)]);
    engine.pause_transfer(&f.source).unwrap();
    let paused = engine
        .cross_seed_discover(&Run {
            force: true,
            ..Run::default()
        })
        .unwrap();
    assert_eq!(
        number(&paused, "skipped"),
        1,
        "{}",
        json::stringify(&paused)
    );
    assert!(transfer(&engine, &late_id).is_none());
    drop(f.scratch);
}

#[test]
fn status_and_run_are_served_to_the_operator_cli() {
    let indexer = Indexer::open();
    let f = fixture(&indexer);
    let (good, good_id) = metainfo(NAME, &[("ep.mkv", &f.video), ("info.nfo", &f.nfo)], "B");
    indexer.publish("good", &good);
    *indexer.feed.lock().unwrap() = rss(&[("g1", TITLE, "good", None, None)]);
    let engine = open(&f);
    let api = Api::bind(engine.clone(), TOKEN.into()).unwrap();
    let origin = format!("http://{}", api.address().unwrap());
    let mut settings = f.settings.clone();
    settings.insert("listen", api.address().unwrap().to_string());
    let config_path = f.scratch.0.join("mynou.json");
    fs::write(&config_path, json::stringify(&settings)).unwrap();
    let worker = {
        let api = api;
        thread::spawn(move || api.run())
    };
    let http = HttpClient::new()
        .without_proxy()
        .with_timeout(Duration::from_secs(5));
    let route = format!("{origin}/api/cross-seed/discovery");
    assert_eq!(http.request("GET", &route, &[], &[]).unwrap().status, 401);
    let auth = vec![
        ("Authorization".into(), format!("Bearer {TOKEN}")),
        ("Content-Type".into(), "application/json".into()),
    ];
    assert_eq!(
        http.request(
            "POST",
            &format!("{route}/run"),
            &auth,
            br#"{"unknown":true}"#
        )
        .unwrap()
        .status,
        400
    );
    let cli = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_mynou"))
            .args(args)
            .args(["--config", config_path.to_str().unwrap()])
            .env(&f.cfg.api_token_env, TOKEN)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        json::parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap()
    };
    let preview = cli(&["cross-seed-discovery", "--run", "--dry-run"]);
    assert_eq!(number(&preview, "would_add"), 1);
    assert!(transfer(&engine, &good_id).is_none());
    let run = cli(&["cross-seed-discovery", "--run", "--indexer", "tracker-b"]);
    assert_eq!(number(&run, "added"), 1);
    let status = cli(&["cross-seed-discovery"]);
    assert_eq!(status.get("enabled"), Some(&Value::Bool(true)));
    let recent = status.get("recent").and_then(Value::as_array).unwrap();
    assert_eq!(
        recent[0].get("result").and_then(Value::as_str),
        Some("added")
    );
    assert_eq!(
        recent[0].get("candidate_id").and_then(Value::as_str),
        Some(good_id.as_str())
    );
    engine.stopped.store(true, Ordering::Release);
    worker.join().unwrap().unwrap();
}

#[test]
fn an_unavailable_indexer_leaves_entries_pending_until_it_recovers() {
    let indexer = Indexer::open();
    let f = fixture(&indexer);
    let (good, good_id) = metainfo(NAME, &[("ep.mkv", &f.video), ("info.nfo", &f.nfo)], "B");
    indexer.publish("good", &good);
    *indexer.feed.lock().unwrap() = rss(&[("g1", TITLE, "good", None, None)]);
    let engine = open(&f);
    indexer.failing.store(true, Ordering::Release);
    let failed = engine.cross_seed_discover(&Run::default()).unwrap();
    let source = &failed.get("indexers").and_then(Value::as_array).unwrap()[0];
    assert_ne!(
        source.get("feed").and_then(Value::as_str),
        Some("ok"),
        "{}",
        json::stringify(&failed)
    );
    assert_eq!(number(&failed, "added"), 0);
    let status = engine.cross_seed_status().unwrap();
    let indexers = status.get("indexers").and_then(Value::as_array).unwrap();
    assert_eq!(
        indexers[0]
            .get("remembered_entries")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert!(transfer(&engine, &good_id).is_none());

    indexer.failing.store(false, Ordering::Release);
    let recovered = engine
        .cross_seed_discover(&Run {
            force: true,
            ..Run::default()
        })
        .unwrap();
    assert_eq!(
        number(&recovered, "added"),
        1,
        "{}",
        json::stringify(&recovered)
    );
    wait(|| transfer(&engine, &good_id).is_some_and(|t| flag(&t, "ready")));
}
