//! A resolved bracketed announcement acquired from a private tracker through the
//! authenticated indexer's `.torrent`. Loopback fixtures only; no real tracker.
mod irc_support;
mod library_support;
#[allow(dead_code)]
mod transfer_support;
use library_support::{Directory, run_until};
use mynou::{
    bencode::{self, Value as Bencode},
    config::{self, Config},
    engine::{Engine, lock, public_job},
    json::{self, Value},
    store::Request,
};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Mutex},
    thread,
};
use transfer_support::{Seeder, Torrent};

const PREFIX: &str = "https://tracker.example/torrents/";
const INDEXER_SECRET: &str = "INDEXERSECRET";
const PASSKEY: &str = "TRACKERPASSKEY";
const NAME: &str = "Fixture.Movie.2024.1080p.mp4";

type Routes = Arc<Mutex<Vec<(String, u16, Vec<u8>)>>>;
/// Serves every request from `routes` (first matching path prefix) and logs paths.
struct Fixture {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
    routes: Routes,
}
impl Fixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let routes: Routes = Arc::new(Mutex::new(Vec::new()));
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
                    .map_or((404, Vec::new()), |(_, s, b)| (*s, b.clone()));
                let mut response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                if status == 302 {
                    response.push_str("Location: /api/elsewhere\r\n");
                }
                response.push_str("\r\n");
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        Self { url, seen, routes }
    }
    fn route(&self, path: &str, status: u16, body: Vec<u8>) {
        let mut routes = self.routes.lock().unwrap();
        routes.retain(|(p, ..)| p != path);
        routes.insert(0, (path.into(), status, body));
    }
    fn hits(&self, prefix: &str) -> usize {
        let seen = self.seen.lock().unwrap();
        seen.iter().filter(|p| p.starts_with(prefix)).count()
    }
}

/// The fixture torrent plus a private announce URL; the info hash is unchanged.
fn private_metainfo(torrent: &Torrent, tracker: &str) -> Vec<u8> {
    let Bencode::Dict(mut outer) = bencode::parse(&fs::read(&torrent.path).unwrap()).unwrap()
    else {
        panic!()
    };
    outer.insert(
        b"announce".to_vec(),
        Bencode::Bytes(format!("{tracker}/announce/{PASSKEY}").into_bytes()),
    );
    bencode::encode(&Bencode::Dict(outer))
}
fn feed(hash: &str, enclosure: &str) -> Vec<u8> {
    let doubled: String = hash.bytes().map(|b| format!("{b:02x}")).collect();
    format!(
        "<?xml version=\"1.0\"?><rss xmlns:torznab=\"http://torznab.com/schemas/2015/feed\"><channel>\
         <item><title>{NAME}</title><comments>{PREFIX}41</comments>\
         <enclosure url=\"{}\" type=\"application/x-bittorrent\"/>\
         <torznab:attr name=\"tmdbid\" value=\"603\"/><torznab:attr name=\"infohash\" value=\"{doubled}\"/>\
         </item></channel></rss>",
        enclosure.replace('&', "&amp;")
    )
    .into_bytes()
}
fn feed_empty() -> Vec<u8> {
    b"<?xml version=\"1.0\"?><rss><channel></channel></rss>".to_vec()
}
fn configuration(root: &Path, http: &str) -> Config {
    let mut v = transfer_support::configuration();
    v.insert("store_dir", root.join("jobs").to_str().unwrap());
    let mut settings = irc_support::settings("irc://127.0.0.1:1");
    let Value::Array(sources) = settings.get_mut("sources").unwrap() else {
        panic!()
    };
    let mut categories = Value::object();
    categories.insert("Films", "movie");
    let mut format = Value::object();
    format.insert("type", "bracketed");
    format.insert("lookup_indexer", "tracker");
    format.insert("details_prefix", PREFIX);
    format.insert("categories", categories);
    sources[0].insert("announcement_format", format);
    let Value::Array(rules) = settings.get_mut("rules").unwrap() else {
        panic!()
    };
    rules[0].insert("action", "grab");
    v.insert("irc", settings);
    let mut indexer = Value::object();
    indexer.insert("id", "tracker");
    indexer.insert("name", "tracker");
    indexer.insert("kind", "torznab");
    indexer.insert("url", format!("{http}/api"));
    indexer.insert("api_key_env", "MYNOU_ABSENT_IRC_LOOKUP_FIXTURE_KEY_334");
    v.insert("indexers", Value::Array(vec![indexer]));
    let catalog = v.get_mut("catalog").unwrap();
    catalog.insert("enabled", true);
    catalog.insert("url", format!("{http}/3"));
    catalog.insert("token_env", "PATH");
    config::from_json(&v, root).unwrap()
}
fn request() -> Request {
    Request {
        kind: "movie".into(),
        title: "Fixture Movie".into(),
        year: 2024,
        season: 0,
        episode: 0,
        tmdb_id: Some(603),
        source_numbering: None,
        source_path: None,
        source_url: None,
    }
}
struct Setup {
    dir: Directory,
    fixture: Fixture,
    cfg: Config,
    engine: Arc<Engine>,
    torrent: Torrent,
    job: String,
}
/// One admitted request and one recorded, resolved announcement for it.
fn setup() -> Setup {
    let dir = Directory::new();
    let torrent = Torrent::single(
        &dir.0.join("source"),
        NAME,
        include_bytes!("../examples/demo.mp4").to_vec(),
    );
    let fixture = Fixture::new();
    let cfg = configuration(&dir.0.join("engine"), &fixture.url);
    fixture.route(
        "/3/movie/603",
        200,
        br#"{"id":603,"title":"Fixture Movie","release_date":"2024-03-01"}"#.to_vec(),
    );
    let download = format!(
        "{}/api/download?apikey={INDEXER_SECRET}&link=x",
        fixture.url
    );
    // The ordinary search finds nothing, so the admitted job waits for IRC.
    fixture.route("/api", 200, feed_empty());
    fixture.route("/api?t=search", 200, feed(&torrent.id, &download));
    let engine = Engine::open(cfg.clone()).unwrap();
    let job = engine.submit(request()).unwrap().remove(0).id;
    assert!(!engine.tick().unwrap());
    assert!(stored(&engine, &job).acquisition_url.is_none());
    let line = format!("[NEW] [Films] [{NAME}] [WEB] [1.2 MiB] [0%] par uploader -> {PREFIX}41");
    engine
        .irc_receive_text("local", irc_support::SENDER, "#announces", &line)
        .unwrap()
        .unwrap();
    let resolved = engine.irc_resolve_pending().unwrap();
    assert_eq!(
        resolved.get("outcome").unwrap().as_str(),
        Some("recorded"),
        "{resolved:?}"
    );
    Setup {
        dir,
        fixture,
        cfg,
        engine,
        torrent,
        job,
    }
}
fn stored(job: &Arc<Engine>, id: &str) -> mynou::store::Job {
    lock(&job.store).unwrap().get(id).unwrap().unwrap()
}
fn files_containing(root: &Path, needle: &[u8], found: &mut Vec<String>) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            files_containing(&path, needle, found);
        } else if meta.is_file()
            && fs::read(&path)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
        {
            found.push(path.display().to_string());
        }
    }
}

#[test]
fn a_private_release_downloads_through_the_verified_indexer_metainfo() {
    let s = setup();
    let seed = Seeder::open(&s.dir.0.join("seed"), &[&s.torrent]);
    let port = seed.client.listen_port();
    let mut peers = vec![127, 0, 0, 1];
    peers.extend(port.to_be_bytes());
    let mut tracker = b"d8:intervali60e5:peers6:".to_vec();
    tracker.extend(&peers);
    tracker.push(b'e');
    s.fixture.route("/tracker/announce", 200, tracker);
    s.fixture.route(
        "/api/download",
        200,
        private_metainfo(&s.torrent, &format!("{}/tracker", s.fixture.url)),
    );
    let report = s.engine.irc_route_pending().unwrap();
    assert_eq!(
        report.get("routed").unwrap().as_bool(),
        Some(true),
        "{report:?}"
    );
    assert_eq!(s.fixture.hits("/api/download"), 1);
    let routed = stored(&s.engine, &s.job);
    let origin = routed.irc_origin.clone().unwrap();
    let metainfo = s
        .cfg
        .store_dir
        .join("irc-metainfo")
        .join(format!("{}.torrent", s.torrent.id));
    assert_eq!(origin.magnet, metainfo.to_str().unwrap());
    assert_eq!(
        routed.acquisition_url.as_deref(),
        Some(origin.magnet.as_str())
    );
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&metainfo), 0o600);
    assert_eq!(mode(metainfo.parent().unwrap()), 0o700);
    let ready = run_until(&s.engine, &s.job, "ready");
    assert_eq!(
        fs::read(&ready.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
    assert!(s.fixture.hits("/tracker/announce") >= 1);
    // The indexer link and its key are never kept; the passkey stays in the
    // private metainfo and the native transfer state only.
    let mut found = Vec::new();
    files_containing(
        &s.dir.0.join("engine"),
        INDEXER_SECRET.as_bytes(),
        &mut found,
    );
    assert!(found.is_empty(), "{found:?}");
    files_containing(&s.cfg.store_dir, PASSKEY.as_bytes(), &mut found);
    let state = s.cfg.downloads.state_dir.to_str().unwrap().to_owned();
    assert!(
        found
            .iter()
            .all(|p| p == metainfo.to_str().unwrap() || p.starts_with(&state)),
        "{found:?}"
    );
    let row = s.engine.irc_announcements(0, 10).unwrap();
    for v in [
        public_job(&ready),
        s.engine.irc_sources().unwrap(),
        row,
        report,
    ] {
        let text = json::stringify(&v);
        for private in [INDEXER_SECRET, PASSKEY, "irc-metainfo", "/api/download"] {
            assert!(!text.contains(private), "{private} in {text}");
        }
    }
    drop(s.engine);
    let engine = Engine::open(s.cfg.clone()).unwrap();
    assert_eq!(stored(&engine, &s.job), ready);
}

/// Status and body of the download, the advertised enclosure, the outcome.
type Case = (
    u16,
    fn(&Setup) -> Vec<u8>,
    Option<&'static str>,
    &'static str,
);

#[test]
fn wrong_redirected_or_foreign_metainfo_never_routes() {
    // A different hash fails like a magnet's wrong metadata; transport failures are unavailable.
    let cases: [Case; 6] = [
        (
            200,
            |s| {
                let other = Torrent::single(&s.dir.0.join("other"), NAME, vec![7; 70_000]);
                private_metainfo(&other, "http://127.0.0.1:9")
            },
            None,
            "admission_or_storage_rejected",
        ),
        (302, |_| Vec::new(), None, "metadata_unavailable"),
        (401, |_| Vec::new(), None, "metadata_unavailable"),
        (
            200,
            |_| b"not bencode".to_vec(),
            None,
            "metadata_unavailable",
        ),
        (
            200,
            |_| Vec::new(),
            Some("https://tracker.example/download/41.KEY"),
            "metadata_unavailable",
        ),
        (
            200,
            |_| Vec::new(),
            Some("magnet:?xt=urn:btih:0000"),
            "metadata_unavailable",
        ),
    ];
    for (status, body, enclosure, expected) in cases {
        let s = setup();
        if let Some(enclosure) = enclosure {
            s.fixture
                .route("/api?t=search", 200, feed(&s.torrent.id, enclosure));
        }
        s.fixture.route("/api/download", status, body(&s));
        let report = s.engine.irc_route_pending().unwrap();
        assert_eq!(
            report.get("routed").unwrap().as_bool(),
            Some(false),
            "{report:?}"
        );
        assert_eq!(
            report.get("outcome").unwrap().as_str(),
            Some(expected),
            "{report:?}"
        );
        let job = stored(&s.engine, &s.job);
        assert!(job.irc_origin.is_none() && job.acquisition_url.is_none());
        let dir = s.cfg.store_dir.join("irc-metainfo");
        assert_eq!(fs::read_dir(&dir).map_or(0, |d| d.count()), 0);
        assert_eq!(s.fixture.hits("/api/elsewhere"), 0);
        let text = json::stringify(&report);
        assert!(!text.contains(INDEXER_SECRET) && !text.contains("KEY"));
    }
}
