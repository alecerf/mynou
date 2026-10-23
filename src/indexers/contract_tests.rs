//! Independently authored synthetic contracts for the ten direct sources of a
//! private replacement manifest, published only by alias. Each contract keeps
//! the source's protocol family, parameter names, authentication placement and
//! response fields; hosts, paths, accounts and credentials stay private. Every
//! request goes to a loopback fixture: no provider is contacted.
use crate::{
    config::{self, Config, Source},
    integrations,
    json::{self, Value},
    store::Request,
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

/// Item fields of the bearer-authenticated filter API shared by three sources.
const FILTER_API: &str = r#"{"results":"data","title":"attributes.name","url":"attributes.download_link","seeders":"attributes.seeders","size":"attributes.size","details":"attributes.details_link","tmdb_id":"attributes.tmdb_id","info_hash":"attributes.info_hash","params":{"query":"name","kind":"","year":"","season":"seasonNumber","episode":"episodeNumber","absolute":"","api_key":"","tmdb_id":"tmdbId"},"fixed_params":{"perPage":"100","sortField":"created_at","sortDirection":"desc"}}"#;
/// A flat JSON search API keyed by a query parameter.
const FLAT_API: &str = r#"{"results":"results","title":"title","url":"download","seeders":"seeders","size":"size","details":"details","tmdb_id":"tmdbid","info_hash":"infohash","params":{"kind":"","year":"","episode":"ep","absolute":"","tmdb_id":"tmdbid"},"fixed_params":{"limit":"100"}}"#;

#[derive(Clone, Copy, PartialEq)]
enum Family {
    Filter,
    Flat,
    Torznab,
}

const CONTRACTS: [(&str, Family); 10] = [
    ("source-01", Family::Filter),
    ("source-02", Family::Filter),
    ("source-03", Family::Filter),
    ("source-04", Family::Torznab),
    ("source-05", Family::Torznab),
    ("source-06", Family::Torznab),
    ("source-07", Family::Flat),
    ("source-08", Family::Torznab),
    ("source-09", Family::Torznab),
    ("source-10", Family::Torznab),
];

const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

struct Directory(std::path::PathBuf);
impl Directory {
    fn new(alias: &str) -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let p = std::env::temp_dir().join(format!(
            "mynou-contract-{alias}-{}",
            crate::crypto::random_bytes::<8>()
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
        Self(p)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Reply = (u16, String, Vec<u8>);
/// Answers each request with the next queued reply (404 when none is left) and
/// records its request line and headers.
struct Fixture {
    url: String,
    heads: Arc<Mutex<Vec<String>>>,
    replies: Arc<Mutex<Vec<Reply>>>,
}
impl Fixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let heads = Arc::new(Mutex::new(Vec::new()));
        let replies: Arc<Mutex<Vec<Reply>>> = Arc::new(Mutex::new(Vec::new()));
        let (log, queue) = (heads.clone(), replies.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                let mut head = Vec::new();
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).is_ok() {
                    head.push(byte[0]);
                }
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head).into_owned());
                let (status, headers, body) = {
                    let mut queue = queue.lock().unwrap();
                    if queue.is_empty() {
                        (404, String::new(), Vec::new())
                    } else {
                        queue.remove(0)
                    }
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .and_then(|()| stream.write_all(&body));
            }
        });
        Self {
            url,
            heads,
            replies,
        }
    }
    fn reply(&self, status: u16, headers: &str, body: impl Into<Vec<u8>>) {
        self.replies
            .lock()
            .unwrap()
            .push((status, headers.into(), body.into()));
    }
    fn requests(&self) -> usize {
        self.heads.lock().unwrap().len()
    }
    fn last(&self) -> String {
        self.heads
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

fn configuration(alias: &str, family: Family, url: &str, dir: &Directory) -> Config {
    let mut v = config::default_json();
    v.insert("listen", "127.0.0.1:0");
    v.insert("store_dir", dir.0.join("jobs").to_str().unwrap());
    v.get_mut("downloads").unwrap().insert("enabled", false);
    let mut source = Value::object();
    source.insert("id", alias);
    source.insert("name", alias);
    source.insert("url", format!("{url}/api"));
    match family {
        Family::Filter => {
            // The bearer token is the only credential; no API key is sent.
            source.insert("kind", "json");
            source.insert("api_key_env", "MYNOU_ABSENT_CONTRACT_FIXTURE_KEY_336");
            let mut auth = Value::object();
            auth.insert("method", "bearer");
            auth.insert("token_env", "PWD");
            source.insert("authentication", auth);
            source.insert("mapping", json::parse(FILTER_API).unwrap());
        }
        Family::Flat => {
            source.insert("kind", "json");
            source.insert("api_key_env", "PATH");
            source.insert("mapping", json::parse(FLAT_API).unwrap());
        }
        Family::Torznab => {
            source.insert("kind", "torznab");
            source.insert("api_key_env", "PATH");
        }
    }
    v.insert("indexers", Value::Array(vec![source]));
    config::from_json(&v, &dir.0).unwrap()
}

fn movie() -> Request {
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
fn episode() -> Request {
    Request {
        kind: "episode".into(),
        title: "Fixture Show".into(),
        season: 2,
        episode: 5,
        tmdb_id: Some(1399),
        ..movie()
    }
}

/// `(title, download path, details id)` rendered in the family's format.
fn listing(family: Family, url: &str, items: &[(&str, &str, u32)]) -> Vec<u8> {
    let field = |key: &str, value: Value| (key.to_owned(), value);
    match family {
        Family::Torznab => {
            let mut body = String::from(
                r#"<?xml version="1.0"?><rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel>"#,
            );
            for (title, path, id) in items {
                body.push_str(&format!(
                    "<item><title>{title}</title><guid>{url}/details/{id}</guid><comments>{url}/details/{id}</comments>\
                     <enclosure url=\"{url}{path}\" length=\"1048576\" type=\"application/x-bittorrent\"/>\
                     <torznab:attr name=\"seeders\" value=\"7\"/><torznab:attr name=\"tmdbid\" value=\"603\"/>\
                     <torznab:attr name=\"infohash\" value=\"{HASH}\"/></item>"
                ));
            }
            body.push_str("</channel></rss>");
            body.into_bytes()
        }
        Family::Filter | Family::Flat => {
            let rows: Vec<Value> = items
                .iter()
                .map(|(title, path, id)| {
                    let fields = if family == Family::Filter {
                        vec![
                            field("name", (*title).into()),
                            field("download_link", format!("{url}{path}").into()),
                            field("details_link", format!("{url}/details/{id}").into()),
                            field("seeders", 7_u32.into()),
                            field("size", 1_048_576_u32.into()),
                            field("tmdb_id", 603_u32.into()),
                            field("info_hash", HASH.into()),
                        ]
                    } else {
                        vec![
                            field("title", (*title).into()),
                            field("download", format!("{url}{path}").into()),
                            field("details", format!("{url}/details/{id}").into()),
                            field("seeders", "7".into()),
                            field("size", "1048576".into()),
                            field("tmdbid", "603".into()),
                            field("infohash", HASH.into()),
                        ]
                    };
                    let mut item = Value::object();
                    for (key, value) in fields {
                        item.insert(&key, value);
                    }
                    if family == Family::Filter {
                        let mut row = Value::object();
                        row.insert("attributes", item);
                        row
                    } else {
                        item
                    }
                })
                .collect();
            let mut v = Value::object();
            v.insert(
                if family == Family::Filter {
                    "data"
                } else {
                    "results"
                },
                Value::Array(rows),
            );
            json::stringify(&v).into_bytes()
        }
    }
}
fn empty(family: Family, url: &str) -> Vec<u8> {
    listing(family, url, &[])
}
fn malformed(family: Family) -> Vec<u8> {
    match family {
        Family::Torznab => b"<?xml version=\"1.0\"?><rss><channel><item>".to_vec(),
        _ => br#"{"data":[{"attributes":"#.to_vec(),
    }
}

fn health(c: &Config) -> Value {
    super::report(&c.sources)
        .get("sources")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .clone()
}
fn last_parse(c: &Config) -> Option<bool> {
    match health(c).get("last_parse") {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}
fn accepted(report: &Value) -> Vec<String> {
    report
        .get("accepted")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|c| c.get("title").and_then(Value::as_str).unwrap().to_owned())
        .collect()
}
fn source(c: &Config) -> &Source {
    &c.sources[0]
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

/// Request parameters and credential placement expected by each family.
fn expect_request(family: Family, head: &str, expected: &[&str], absent: &[&str]) {
    let line = head.lines().next().unwrap();
    for part in expected {
        assert!(line.contains(part), "{part} missing from {line}");
    }
    for part in absent {
        assert!(!line.contains(part), "{part} unexpected in {line}");
    }
    let bearer = format!("Bearer {}", std::env::var("PWD").unwrap());
    match family {
        Family::Filter => {
            assert!(head.contains(&bearer), "missing bearer token");
            assert!(!line.contains("apikey="));
        }
        Family::Flat | Family::Torznab => {
            assert!(line.contains("apikey="), "missing API key in {line}");
            assert!(!head.to_ascii_lowercase().contains("authorization:"));
        }
    }
}

fn no_secrets(v: &Value) {
    let text = json::stringify(v);
    for name in ["PATH", "PWD"] {
        let value = std::env::var(name).unwrap();
        assert!(!text.contains(&value), "{name} leaked");
    }
}

fn contract(alias: &str, family: Family) {
    let dir = Directory::new(alias);
    let f = Fixture::new();
    let c = configuration(alias, family, &f.url, &dir);
    let url = f.url.clone();

    // Movie search: identifiers and roles follow the contract; only the
    // release with the requested identity is a candidate.
    f.reply(
        200,
        "",
        listing(
            family,
            &url,
            &[
                ("Fixture Movie 2024 1080p WEB-DL x264-GRP", "/dl/1", 1),
                ("Other Film 2024 1080p WEB-DL x264-GRP", "/dl/2", 2),
            ],
        ),
    );
    let report = integrations::search_report(&c, &movie()).unwrap();
    assert_eq!(
        accepted(&report),
        ["Fixture Movie 2024 1080p WEB-DL x264-GRP"],
        "{alias}"
    );
    no_secrets(&report);
    let (movie_expected, movie_absent): (&[&str], &[&str]) = match family {
        Family::Filter => (
            &["tmdbId=603", "perPage=100", "sortField=created_at"],
            &["name=", "seasonNumber=", "episodeNumber="],
        ),
        Family::Flat => (&["tmdbid=603", "limit=100"], &["q=", "season=", "ep="]),
        Family::Torznab => (&["t=movie", "q=Fixture%20Movie"], &["season=", "ep="]),
    };
    expect_request(family, &f.last(), movie_expected, movie_absent);
    assert_eq!(last_parse(&c), Some(true), "{alias}");

    // Episode search sends the season and episode numbers.
    f.reply(
        200,
        "",
        listing(
            family,
            &url,
            &[("Fixture Show S02E05 1080p WEB-DL x264-GRP", "/dl/3", 3)],
        ),
    );
    let report = integrations::search_report(&c, &episode()).unwrap();
    assert_eq!(
        accepted(&report),
        ["Fixture Show S02E05 1080p WEB-DL x264-GRP"],
        "{alias}"
    );
    let (episode_expected, episode_absent): (&[&str], &[&str]) = match family {
        Family::Filter => (
            &["seasonNumber=2", "episodeNumber=5", "tmdbId=1399"],
            &["name="],
        ),
        Family::Flat => (&["season=2", "ep=5", "tmdbid=1399"], &["q="]),
        Family::Torznab => (&["t=tvsearch", "q=Fixture%20Show", "season=2", "ep=5"], &[]),
    };
    expect_request(family, &f.last(), episode_expected, episode_absent);

    // An empty answer parses and selects nothing; a malformed one is a parse
    // failure, never a release.
    f.reply(200, "", empty(family, &url));
    let report = integrations::search_report(&c, &movie()).unwrap();
    assert!(accepted(&report).is_empty(), "{alias}");
    assert_eq!(last_parse(&c), Some(true), "{alias}");
    f.reply(200, "", malformed(family));
    let _ = integrations::search_report(&c, &movie());
    assert_eq!(last_parse(&c), Some(false), "{alias}");

    // The newest-entries read used by feed watching and cross-seed discovery.
    f.reply(
        200,
        "",
        listing(
            family,
            &url,
            &[
                ("Fixture Movie 2024 2160p WEB-DL x265-GRP", "/dl/4", 4),
                ("Fixture Show S02E06 1080p WEB-DL x264-GRP", "/dl/5", 5),
            ],
        ),
    );
    let poll = integrations::poll_feed(source(&c), deadline()).unwrap();
    assert_eq!((poll.entries.len(), poll.skipped), (2, 0), "{alias}");
    let (feed_expected, feed_absent): (&[&str], &[&str]) = match family {
        Family::Filter => (&["sortDirection=desc", "perPage=100"], &["name="]),
        Family::Flat => (&["limit=100"], &["q="]),
        Family::Torznab => (&["t=search"], &["q="]),
    };
    expect_request(family, &f.last(), feed_expected, feed_absent);

    // Cross-seed completion search: the name alone, with size and hash.
    f.reply(
        200,
        "",
        listing(
            family,
            &url,
            &[("Fixture Show S02E06 1080p WEB-DL x264-GRP", "/dl/5", 5)],
        ),
    );
    let offers = integrations::offers_before(
        source(&c),
        Some("Fixture Show S02E06 1080p WEB-DL x264-GRP"),
        deadline(),
    )
    .unwrap();
    assert_eq!(offers.len(), 1, "{alias}");
    assert_eq!(
        (offers[0].size, offers[0].info_hash.as_deref()),
        (Some(1_048_576), Some(HASH))
    );
    let query = match family {
        Family::Filter => "name=Fixture%20Show%20S02E06",
        _ => "q=Fixture%20Show%20S02E06",
    };
    expect_request(family, &f.last(), &[query], &["tmdb", "season"]);

    // Details lookup by release name: one details page, TMDB ID and hash.
    f.reply(
        200,
        "",
        listing(
            family,
            &url,
            &[("Fixture.Movie.2024.1080p.WEB-DL.x264-GRP", "/dl/6", 6)],
        ),
    );
    let details = integrations::details_before(
        source(&c),
        "Fixture Movie 2024 1080p WEB-DL x264-GRP",
        deadline(),
    )
    .unwrap();
    assert_eq!(details.len(), 1, "{alias}");
    assert_eq!(details[0].details, format!("{url}/details/6"));
    assert_eq!(details[0].tmdb_ids, ["603"]);
    assert_eq!(details[0].info_hashes, [HASH]);
    assert_eq!(
        details[0].enclosure.as_deref(),
        Some(&*format!("{url}/dl/6"))
    );

    // Authenticated metainfo: same origin, no redirects, failures rejected.
    f.reply(200, "", b"d4:infod4:name1:xee".to_vec());
    let body =
        integrations::indexer_metainfo_before(source(&c), &format!("{url}/dl/6"), deadline())
            .unwrap();
    assert_eq!(body, b"d4:infod4:name1:xee");
    // The enclosure carries any query credential itself; only the header
    // authentication is added.
    let head = f.last();
    assert!(head.starts_with("GET /dl/6 "), "{alias}");
    assert_eq!(
        head.contains("Authorization: Bearer "),
        family == Family::Filter,
        "{alias}"
    );
    for (status, headers) in [(302, "Location: /elsewhere\r\n"), (404, ""), (500, "")] {
        f.reply(status, headers, Vec::new());
        assert!(
            integrations::indexer_metainfo_before(source(&c), "/dl/7", deadline()).is_err(),
            "{alias} {status}"
        );
    }
    let before = f.requests();
    assert!(
        integrations::indexer_metainfo_before(source(&c), "http://127.0.0.2:9/dl/8", deadline())
            .is_err()
    );
    assert_eq!(f.requests(), before, "{alias}: foreign origin contacted");

    // Denial never falls back to an anonymous request and leaks nothing.
    f.reply(401, "", Vec::new());
    let _ = integrations::search_report(&c, &movie());
    let row = health(&c);
    assert_eq!(
        row.get("last_error").and_then(Value::as_str),
        Some("authentication_rejected"),
        "{alias}"
    );
    no_secrets(&row);

    // A 429 rests the source for its Retry-After delay without another request.
    f.reply(429, "Retry-After: 120\r\n", Vec::new());
    let _ = integrations::search_report(&c, &movie());
    let before = f.requests();
    f.reply(200, "", empty(family, &url));
    let _ = integrations::search_report(&c, &movie());
    assert!(integrations::poll_feed(source(&c), deadline()).is_err());
    assert_eq!(f.requests(), before, "{alias}: rested source contacted");
    assert_eq!(
        health(&c).get("last_error").and_then(Value::as_str),
        Some("rate_limited"),
        "{alias}"
    );
}

#[test]
fn every_direct_source_contract_holds_against_its_synthetic_fixture() {
    for (alias, family) in CONTRACTS {
        contract(alias, family);
    }
}
