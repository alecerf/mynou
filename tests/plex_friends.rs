//! Friends' watchlists read with the owner's Plex connection alone, against a
//! local Plex built from tests/fixtures/plex: the community API, item
//! metadata and the connected account. Every request must carry the owner's
//! token; no friend hands over anything.
use crate::api_support::{self, TOKEN};
use crate::library_support::Directory;
use crate::series_support;
use mynou::{
    config::{self, Config},
    engine::{Engine, lock},
    json::{self, Value},
    requesters::Account,
    store::Job,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const ANA: &str = "plex-7001";
const BEN: &str = "plex-7002";
const CLEO: &str = "plex-7003";
const DAN: &str = "plex-7004";
const OWNER: &str = "plex-9001";
const ANA_UUID: &str = "0000000000000a01";
const BEN_UUID: &str = "0000000000000b02";
const CLEO_UUID: &str = "0000000000000c03";
const DAN_UUID: &str = "0000000000000d04";

fn token() -> String {
    std::env::var("MYNOU_TEST_SECRET_A").unwrap()
}
fn fixture(text: &str) -> Value {
    json::parse(text).unwrap().get("body").unwrap().clone()
}
/// The synthetic friend list: Ana, Ben, Cleo and Dan.
fn friend_list() -> Vec<Value> {
    fixture(include_str!("fixtures/plex/friends.json"))
        .get("data")
        .unwrap()
        .get("allFriendsV2")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec()
}
fn only_friends(names: &[&str]) -> Vec<Value> {
    friend_list()
        .into_iter()
        .filter(|f| {
            let name = f.get("user").unwrap().get("username").unwrap();
            names
                .iter()
                .any(|n| name.as_str() == Some(&format!("{n}.synthetic")))
        })
        .collect()
}
/// A watchlist entry as the community API lists it.
fn node(id: &str, kind: &str, title: &str) -> Value {
    let mut v = Value::object();
    v.insert("id", id);
    v.insert("key", format!("/library/metadata/{id}"));
    v.insert("type", kind);
    v.insert("title", title);
    v.insert("year", 2024_u32);
    v
}
/// An entry of the owner's own watchlist, as the discover provider lists it.
fn owner_item(tmdb: u32, title: &str) -> Value {
    let mut v = Value::object();
    v.insert("type", "movie");
    v.insert("title", title);
    v.insert("year", 2024_u32);
    v.insert("guid", format!("tmdb://{tmdb}"));
    v
}

#[derive(Clone)]
struct Item {
    kind: &'static str,
    title: String,
    year: u32,
    tmdb: u32,
}

/// What the fixture Plex answers, and what it was asked.
#[derive(Default)]
struct Plex {
    friends: Vec<Value>,
    /// A fixed answer to the friend list: status and body.
    friends_answer: Option<(u16, String)>,
    lists: BTreeMap<String, Vec<Value>>,
    /// A fixed answer to a friend's watchlist: status and body.
    answers: BTreeMap<String, (u16, String)>,
    items: BTreeMap<String, Item>,
    failing_items: BTreeSet<String>,
    failing_pages: BTreeSet<String>,
    owner: Vec<Value>,
    calls: Vec<String>,
}

struct Fixture {
    url: String,
    state: Arc<Mutex<Plex>>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    // A client past its size limit may have hung up already.
    let _ = write!(
        stream,
        "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}
fn graphql(stream: &mut TcpStream, data: Value) {
    let mut v = Value::object();
    v.insert("data", data);
    respond(stream, 200, "application/json", &json::stringify(&v));
}

impl Fixture {
    fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(Plex {
            friends: friend_list(),
            ..Plex::default()
        }));
        let stopped = Arc::new(AtomicBool::new(false));
        let (shared, stop) = (state.clone(), stopped.clone());
        let thread = thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("Plex fixture accept failed: {e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                Self::serve(&shared, &mut stream);
            }
        });
        Self {
            url,
            state,
            stopped,
            thread: Some(thread),
        }
    }

    fn serve(shared: &Mutex<Plex>, stream: &mut TcpStream) {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            assert!(head.len() < 16384);
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        let mut words = head.split(' ');
        let (method, target) = (words.next().unwrap(), words.next().unwrap());
        let path = target.split('?').next().unwrap();
        let header = |name: &str| {
            head.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        };
        // The owner's token, and only it, on every request.
        assert_eq!(header("X-Plex-Token"), Some(token()), "{method} {path}");
        assert!(header("Authorization").is_none());
        let length: usize = header("Content-Length").map_or(0, |n| n.parse().unwrap());
        assert!(length <= 4096);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        let mut plex = shared.lock().unwrap();
        match (method, path) {
            ("POST", "/community") => {
                assert_eq!(header("Content-Type").as_deref(), Some("application/json"));
                let request = json::parse(std::str::from_utf8(&body).unwrap()).unwrap();
                let query = request.get("query").unwrap().as_str().unwrap();
                let variables = request.get("variables").unwrap();
                if query.contains("allFriendsV2") {
                    plex.calls.push("friends".into());
                    if let Some((status, body)) = &plex.friends_answer {
                        return respond(stream, *status, "text/html", body);
                    }
                    let mut data = Value::object();
                    data.insert("allFriendsV2", Value::Array(plex.friends.clone()));
                    return graphql(stream, data);
                }
                assert!(
                    query.contains("user(id: $uuid) { watchlist(first: $first, after: $after)")
                );
                let uuid = variables.get("uuid").unwrap().as_str().unwrap().to_owned();
                assert_eq!(variables.get("first").unwrap().as_u64(), Some(100));
                let after = variables.get("after").unwrap().as_str().map(str::to_owned);
                plex.calls.push(format!(
                    "watchlist {uuid} {}",
                    after.as_deref().unwrap_or("-")
                ));
                if let Some((status, body)) = plex.answers.get(&uuid) {
                    return respond(stream, *status, "application/json", body);
                }
                if after
                    .as_ref()
                    .is_some_and(|a| plex.failing_pages.contains(a))
                {
                    return respond(stream, 503, "text/plain", "Service Unavailable");
                }
                let Some(list) = plex.lists.get(&uuid) else {
                    return graphql(stream, json::parse(r#"{"user":null}"#).unwrap());
                };
                let start =
                    after.map_or(0, |a| a.strip_prefix("cursor-").unwrap().parse().unwrap());
                let end = list.len().min(start + 100);
                let mut info = Value::object();
                info.insert("hasNextPage", end < list.len());
                info.insert("hasPreviousPage", start > 0);
                info.insert(
                    "endCursor",
                    if end > start {
                        Value::String(format!("cursor-{end}"))
                    } else {
                        Value::Null
                    },
                );
                let mut watchlist = Value::object();
                watchlist.insert("nodes", Value::Array(list[start..end].to_vec()));
                watchlist.insert("pageInfo", info);
                let mut user = Value::object();
                user.insert("watchlist", watchlist);
                let mut data = Value::object();
                data.insert("user", user);
                graphql(stream, data)
            }
            ("GET", path) if path.starts_with("/metadata/") => {
                let id = &path["/metadata/".len()..];
                assert!(target.ends_with("?includeGuids=1"));
                plex.calls.push(format!("metadata {id}"));
                if plex.failing_items.contains(id) {
                    return respond(
                        stream,
                        500,
                        "application/json",
                        r#"{"Error":{"message":"Internal error"}}"#,
                    );
                }
                let Some(item) = plex.items.get(id) else {
                    return respond(
                        stream,
                        404,
                        "application/json",
                        r#"{"Error":{"message":"Not found"}}"#,
                    );
                };
                // The real metadata answer, made to describe this item.
                let mut answer = fixture(include_str!("fixtures/plex/metadata-item.json"));
                let Some(Value::Object(container)) = answer.get_mut("MediaContainer") else {
                    unreachable!()
                };
                let Some(Value::Array(items)) = container.get_mut("Metadata") else {
                    unreachable!()
                };
                let entry = &mut items[0];
                entry.insert("type", item.kind);
                entry.insert("title", item.title.clone());
                entry.insert("year", item.year);
                entry.insert("guid", format!("plex://{}/{id}", item.kind));
                entry.insert("key", format!("/library/metadata/{id}"));
                entry.insert("ratingKey", id);
                entry.insert(
                    "Guid",
                    json::parse(&format!(
                        r#"[{{"id":"imdb://tt{0}"}},{{"id":"tmdb://{0}"}}]"#,
                        item.tmdb
                    ))
                    .unwrap(),
                );
                respond(stream, 200, "application/json", &json::stringify(&answer));
            }
            ("GET", "/identity") => {
                plex.calls.push("identity".into());
                respond(
                    stream,
                    200,
                    "application/json",
                    r#"{"id":9001,"uuid":"owner0000000001","username":"owner.synthetic","title":"Owner"}"#,
                );
            }
            ("GET", "/owner/watchlist") => {
                plex.calls.push("owner".into());
                let field = |name: &str| {
                    target
                        .split(['?', '&'])
                        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
                        .map(|n| n.parse::<usize>().unwrap())
                };
                let start = field("X-Plex-Container-Start").unwrap_or(0);
                let size = field("X-Plex-Container-Size").unwrap_or(100);
                let end = plex.owner.len().min(start + size);
                let mut container = Value::object();
                container.insert("size", (end - start.min(end)) as u32);
                container.insert("totalSize", plex.owner.len() as u32);
                container.insert(
                    "Metadata",
                    Value::Array(plex.owner[start.min(end)..end].to_vec()),
                );
                let mut v = Value::object();
                v.insert("MediaContainer", container);
                respond(stream, 200, "application/json", &json::stringify(&v));
            }
            ("GET", path) if path.starts_with("/token/") => {
                // A requester account of the settings: its own token, here the
                // same fixture value.
                let rest = &path["/token/".len()..];
                let (user, route) = rest.split_once('/').unwrap();
                plex.calls.push(format!("token {user} {route}"));
                let body = match route {
                    "identity" => format!(r#"{{"id":{user}}}"#),
                    "watchlist" => {
                        r#"{"MediaContainer":{"size":0,"totalSize":0,"Metadata":[]}}"#.into()
                    }
                    _ => panic!("Unexpected Plex fixture route: {path}"),
                };
                respond(stream, 200, "application/json", &body);
            }
            _ => panic!("Unexpected Plex fixture route: {method} {path}"),
        }
    }

    fn config(&self, root: &Path) -> Config {
        let mut v = config::default_json();
        v.insert(
            "selection",
            mynou::selection::SelectionConfig::default().to_json(),
        );
        v.insert("listen", "127.0.0.1:0");
        let d = v.get_mut("downloads").unwrap();
        d.insert("enabled", false);
        d.insert("listen_port", 0_u32);
        d.insert("dht", false);
        d.insert("pex", false);
        let mut c = config::from_json(&v, root).unwrap();
        c.plex.enabled = true;
        c.plex.url = self.url.clone();
        c.plex.token_override = Some(token());
        c.plex.watchlist_url = format!("{}/owner/watchlist", self.url);
        c.plex.identity_url = format!("{}/identity", self.url);
        c.plex.community_url = format!("{}/community", self.url);
        c.plex.metadata_url = format!("{}/metadata/", self.url);
        c
    }

    fn with<T>(&self, f: impl FnOnce(&mut Plex) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }
    fn item(&self, id: &str, kind: &'static str, title: &str, tmdb: u32) {
        self.with(|p| {
            p.items.insert(
                id.into(),
                Item {
                    kind,
                    title: title.into(),
                    year: 2024,
                    tmdb,
                },
            )
        });
    }
    /// A friend's watchlist of films, with their metadata.
    fn films(&self, uuid: &str, films: &[(&str, &str, u32)]) {
        let nodes = films
            .iter()
            .map(|(id, title, tmdb)| {
                self.item(id, "movie", title, *tmdb);
                node(id, "MOVIE", title)
            })
            .collect();
        self.with(|p| p.lists.insert(uuid.into(), nodes));
    }
    fn calls(&self) -> Vec<String> {
        self.with(|p| std::mem::take(&mut p.calls))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(h) = self.thread.take()
            && let Err(e) = h.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(e)
        }
    }
}

fn friends(engine: &Engine) -> BTreeMap<String, Value> {
    engine
        .requesters()
        .unwrap()
        .get("plex")
        .unwrap()
        .get("friends")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f.get("id").unwrap().as_str().unwrap().to_owned(), f.clone()))
        .collect()
}
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
fn state(engine: &Engine, id: &str) -> (String, Option<String>) {
    let friend = friends(engine).remove(id).unwrap();
    (
        field(&friend, "state").unwrap().to_owned(),
        field(&friend, "error").map(str::to_owned),
    )
}
fn turn(engine: &Engine, id: &str, on: bool) -> Value {
    let friend = engine.plex_friend(id, on).unwrap();
    assert_eq!(friend.get("enabled").unwrap().as_bool(), Some(on));
    friend
}
fn demands(engine: &Engine, id: &str) -> Vec<Value> {
    let mut after: Option<String> = None;
    let mut out = Vec::new();
    loop {
        let page = engine.requester(id, after.as_deref(), 200).unwrap();
        out.extend(
            page.get("demands")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .cloned(),
        );
        match page.get("next").unwrap().as_str() {
            Some(next) => after = Some(next.into()),
            None => return out,
        }
    }
}
fn titles(demands: &[Value]) -> BTreeSet<String> {
    demands
        .iter()
        .map(|d| {
            field(d.get("request").unwrap(), "title")
                .unwrap()
                .to_owned()
        })
        .collect()
}
fn demand_of<'a>(demands: &'a [Value], title: &str) -> &'a Value {
    demands
        .iter()
        .find(|d| field(d.get("request").unwrap(), "title") == Some(title))
        .unwrap()
}
fn jobs(engine: &Engine) -> Vec<Job> {
    lock(&engine.store).unwrap().list().unwrap()
}
fn job_state(engine: &Engine, id: &str) -> String {
    lock(&engine.store).unwrap().get(id).unwrap().unwrap().state
}
fn entry<'a>(report: &'a Value, id: &str) -> &'a Value {
    report
        .get("accounts")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|a| field(a, "account_id") == Some(id))
        .unwrap_or_else(|| panic!("{id} not polled: {}", json::stringify(report)))
}
fn polled(report: &Value) -> BTreeSet<String> {
    report
        .get("accounts")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|a| field(a, "account_id").unwrap().to_owned())
        .collect()
}
fn sync(engine: &Engine) -> Value {
    let report = engine.sync_requesters().unwrap();
    assert!(!json::stringify(&report).contains(&token()));
    report
}
/// Passes until the account's sweep is published.
fn until_published(engine: &Engine, id: &str) {
    for _ in 0..8 {
        let report = sync(engine);
        if entry(&report, id).get("complete").and_then(Value::as_bool) == Some(true) {
            return;
        }
    }
    panic!("{id} never published its sweep");
}
fn watchlist_calls(calls: &[String], uuid: &str) -> Vec<String> {
    calls
        .iter()
        .filter(|c| c.starts_with(&format!("watchlist {uuid}")))
        .cloned()
        .collect()
}
/// No file Mynou wrote holds the owner's token.
fn no_token_on_disk(root: &Path) {
    let token = token();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(bytes) = fs::read(&path) {
                assert!(
                    !bytes.windows(token.len()).any(|w| w == token.as_bytes()),
                    "{} holds the token",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn two_friends_make_requests_a_private_list_is_an_error_and_a_friend_left_off_is_never_read() {
    let dir = Directory::new();
    let plex = Fixture::open();
    let catalog = series_support::Catalog::open(vec![
        series_support::episode(1, 1, Some("2024-01-01"), "Aired"),
        series_support::episode(1, 2, Some("2200-01-01"), "Future"),
    ]);
    for (tmdb, title) in [(101, "Synthetic Harbour"), (201, "Synthetic Orchard")] {
        catalog.response(
            &format!("/3/movie/{tmdb}"),
            200,
            json::parse(&format!(
                r#"{{"id":{tmdb},"title":"{title}","runtime":100}}"#
            ))
            .unwrap(),
        );
    }
    // Ana's list is the synthetic page: a film, a series and a season.
    let page = fixture(include_str!("fixtures/plex/watchlist-page.json"));
    let nodes = page
        .get("data")
        .unwrap()
        .get("user")
        .unwrap()
        .get("watchlist")
        .unwrap()
        .get("nodes")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec();
    plex.with(|p| p.lists.insert(ANA_UUID.into(), nodes));
    plex.item(
        "000000000000000000000101",
        "movie",
        "Synthetic Harbour",
        101,
    );
    plex.item("000000000000000000000102", "show", "Fixture Series", 42);
    plex.films(
        BEN_UUID,
        &[("000000000000000000000201", "Synthetic Orchard", 201)],
    );
    plex.films(
        DAN_UUID,
        &[("000000000000000000000401", "Synthetic Unread", 401)],
    );
    let private = fixture(include_str!("fixtures/plex/watchlist-private.json"));
    plex.with(|p| {
        p.answers
            .insert(CLEO_UUID.into(), (200, json::stringify(&private)))
    });
    let mut cfg = plex.config(&dir.0);
    cfg.catalog = catalog.config(&dir.0).catalog;
    let engine = Engine::open_for_management(cfg).unwrap();

    // Nothing is asked of Plex until Settings lists the friends.
    sync(&engine);
    assert!(plex.calls().is_empty());
    let listed = engine.refresh_plex_friends().unwrap();
    assert_eq!(plex.calls(), ["friends"]);
    assert_eq!(listed.get("connected").unwrap().as_bool(), Some(true));
    let all = friends(&engine);
    assert_eq!(all.keys().collect::<Vec<_>>(), [ANA, BEN, CLEO, DAN]);
    for friend in all.values() {
        // New friends are off.
        assert_eq!(friend.get("enabled").unwrap().as_bool(), Some(false));
        assert_eq!(field(friend, "state"), Some("off"));
    }
    assert_eq!(field(&all[BEN], "name"), Some("ben.synthetic"));
    assert_eq!(field(&all[ANA], "name"), Some("Ana"));
    // Off friends are not read.
    sync(&engine);
    assert!(plex.calls().is_empty());

    for id in [ANA, BEN, CLEO] {
        assert_eq!(field(&turn(&engine, id, true), "state"), Some("reading"));
    }
    let report = sync(&engine);
    let calls = plex.calls();
    assert_eq!(
        polled(&report),
        [ANA, BEN, CLEO, OWNER].map(String::from).into()
    );
    for id in [ANA, BEN, OWNER] {
        assert_eq!(
            entry(&report, id).get("success").unwrap().as_bool(),
            Some(true)
        );
    }
    assert_eq!(
        field(entry(&report, CLEO), "error"),
        Some("Plex: This user's watchlist is private")
    );
    assert!(watchlist_calls(&calls, DAN_UUID).is_empty());
    // The season is skipped without asking for its metadata.
    assert!(!calls.contains(&"metadata 000000000000000000000103".to_owned()));
    assert!(!calls.contains(&"metadata 000000000000000000000401".to_owned()));
    assert_eq!(calls.iter().filter(|c| *c == "identity").count(), 1);

    let ana = demands(&engine, ANA);
    assert_eq!(
        titles(&ana),
        ["Fixture Series", "Synthetic Harbour"]
            .map(String::from)
            .into()
    );
    let episode = demand_of(&ana, "Fixture Series").get("request").unwrap();
    assert_eq!(
        (
            episode.get("season").unwrap().as_u64(),
            episode.get("episode").unwrap().as_u64()
        ),
        (Some(1), Some(1))
    );
    assert_eq!(
        titles(&demands(&engine, BEN)),
        ["Synthetic Orchard".to_owned()].into()
    );
    assert!(demands(&engine, CLEO).is_empty());
    let mut requested: Vec<_> = jobs(&engine).into_iter().map(|j| j.request.title).collect();
    requested.sort();
    assert_eq!(
        requested,
        ["Fixture Series", "Synthetic Harbour", "Synthetic Orchard"]
    );

    let all = friends(&engine);
    assert_eq!(field(&all[ANA], "state"), Some("on"));
    assert_eq!(all[ANA].get("titles").unwrap().as_u64(), Some(2));
    assert_eq!(field(&all[BEN], "state"), Some("on"));
    assert_eq!(
        state(&engine, CLEO),
        (
            "error".into(),
            Some("Plex: This user's watchlist is private".into())
        )
    );
    assert_eq!(field(&all[DAN], "state"), Some("off"));
    // The owner's own list became a requester, shown as such.
    let requesters = engine.requesters().unwrap();
    assert_eq!(field(requesters.get("plex").unwrap(), "owner"), Some(OWNER));
    no_token_on_disk(&dir.0);
}

#[test]
fn a_long_watchlist_is_read_whole_and_resumes_from_its_cursor_after_restarts() {
    let dir = Directory::new();
    let plex = Fixture::open();
    plex.with(|p| p.friends = only_friends(&["ana"]));
    let films: Vec<_> = (0..250)
        .map(|n| {
            (
                format!("item{n:04}"),
                format!("Synthetic Paged Film {n}"),
                1000 + n,
            )
        })
        .collect();
    let refs: Vec<_> = films
        .iter()
        .map(|(id, title, tmdb)| (id.as_str(), title.as_str(), *tmdb))
        .collect();
    plex.films(ANA_UUID, &refs);
    // The 151st film's description fails: the sweep stops inside page two.
    plex.with(|p| p.failing_items.insert("item0150".into()));
    let cfg = plex.config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    engine.refresh_plex_friends().unwrap();
    turn(&engine, ANA, true);
    plex.calls();
    let report = sync(&engine);
    assert_eq!(
        entry(&report, ANA).get("success").unwrap().as_bool(),
        Some(false)
    );
    let calls = plex.calls();
    assert_eq!(
        watchlist_calls(&calls, ANA_UUID),
        [
            format!("watchlist {ANA_UUID} -"),
            format!("watchlist {ANA_UUID} cursor-100"),
            format!("watchlist {ANA_UUID} cursor-100"),
        ]
    );
    assert_eq!(demands(&engine, ANA).len(), 150);
    drop(engine);

    // After a restart the sweep resumes inside page two, from Plex's cursor;
    // then page three fails.
    plex.with(|p| {
        p.failing_items.clear();
        p.failing_pages.insert("cursor-200".into());
    });
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let report = sync(&engine);
    assert_eq!(
        entry(&report, ANA).get("success").unwrap().as_bool(),
        Some(false)
    );
    let calls = plex.calls();
    assert_eq!(
        watchlist_calls(&calls, ANA_UUID),
        [
            format!("watchlist {ANA_UUID} cursor-100"),
            format!("watchlist {ANA_UUID} cursor-200"),
        ]
    );
    // Only the film that failed is described again.
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("metadata ")).count(),
        1 + 49
    );
    assert!(calls.contains(&"metadata item0150".to_owned()));
    assert_eq!(demands(&engine, ANA).len(), 200);
    drop(engine);

    plex.with(|p| p.failing_pages.clear());
    let engine = Engine::open_for_management(cfg).unwrap();
    sync(&engine);
    let calls = plex.calls();
    assert_eq!(
        watchlist_calls(&calls, ANA_UUID)[0],
        format!("watchlist {ANA_UUID} cursor-200")
    );
    // Every film once: no duplicate demand or job.
    let all = demands(&engine, ANA);
    assert_eq!(all.len(), 250);
    assert_eq!(titles(&all).len(), 250);
    let jobs = jobs(&engine);
    let unique: BTreeSet<_> = jobs.iter().map(|j| j.request.title.clone()).collect();
    assert_eq!(unique.len(), jobs.len());
    assert_eq!(jobs.len(), 250);

    // Confirming the sweep reads the list again from the top; it never
    // removes or repeats anything.
    until_published(&engine, ANA);
    let all = demands(&engine, ANA);
    assert_eq!(all.len(), 250);
    assert!(all.iter().all(|d| field(d, "state") != Some("removed")));
    assert_eq!(jobs_len(&engine), 250);
}

fn jobs_len(engine: &Engine) -> usize {
    jobs(engine).len()
}

#[test]
fn a_title_on_the_owner_and_a_friend_list_is_one_request_kept_while_either_wants_it() {
    let dir = Directory::new();
    let plex = Fixture::open();
    plex.with(|p| {
        p.friends = only_friends(&["ana"]);
        p.owner = vec![owner_item(500, "Synthetic Shared Film")];
    });
    plex.films(
        ANA_UUID,
        &[
            ("itemshared", "Synthetic Shared Film", 500),
            ("itemana", "Synthetic Ana Film", 501),
        ],
    );
    let engine = Engine::open_for_management(plex.config(&dir.0)).unwrap();
    engine.refresh_plex_friends().unwrap();
    turn(&engine, ANA, true);
    let report = sync(&engine);
    assert_eq!(polled(&report), [ANA, OWNER].map(String::from).into());
    assert_eq!(jobs_len(&engine), 2);
    let ana = demands(&engine, ANA);
    let owner = demands(&engine, OWNER);
    assert_eq!(owner.len(), 1);
    let shared = field(demand_of(&ana, "Synthetic Shared Film"), "job_id")
        .unwrap()
        .to_owned();
    assert_eq!(field(&owner[0], "job_id"), Some(shared.as_str()));
    let account = |id: &str| {
        engine
            .requesters()
            .unwrap()
            .get("accounts")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|a| field(a, "id") == Some(id))
            .unwrap()
            .clone()
    };
    let owner_account = account(OWNER);
    assert_eq!(
        owner_account.get("configured").unwrap().as_bool(),
        Some(true)
    );
    assert_eq!(
        owner_account
            .get("plex")
            .unwrap()
            .get("owner")
            .unwrap()
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        account(ANA)
            .get("plex")
            .unwrap()
            .get("friend")
            .unwrap()
            .as_bool(),
        Some(true)
    );

    // Ana removes it: the owner still wants it.
    plex.films(ANA_UUID, &[("itemana", "Synthetic Ana Film", 501)]);
    until_published(&engine, ANA);
    let ana = demands(&engine, ANA);
    assert_eq!(
        field(demand_of(&ana, "Synthetic Shared Film"), "state"),
        Some("removed")
    );
    assert_eq!(job_state(&engine, &shared), "queued");
    // The owner removes it too: nobody wants it any more.
    plex.with(|p| p.owner.clear());
    until_published(&engine, OWNER);
    assert_eq!(job_state(&engine, &shared), "cancelled");
}

#[test]
fn turning_a_friend_off_stops_reading_without_cancelling_admitted_work() {
    let dir = Directory::new();
    let plex = Fixture::open();
    plex.with(|p| p.friends = only_friends(&["ana"]));
    plex.films(ANA_UUID, &[("itemfirst", "Synthetic First Film", 601)]);
    let engine = Engine::open_for_management(plex.config(&dir.0)).unwrap();
    engine.refresh_plex_friends().unwrap();
    turn(&engine, ANA, true);
    sync(&engine);
    let job = field(&demands(&engine, ANA)[0], "job_id")
        .unwrap()
        .to_owned();
    assert_eq!(job_state(&engine, &job), "queued");

    assert_eq!(field(&turn(&engine, ANA, false), "state"), Some("off"));
    plex.films(
        ANA_UUID,
        &[
            ("itemfirst", "Synthetic First Film", 601),
            ("itemsecond", "Synthetic Second Film", 602),
        ],
    );
    plex.calls();
    for _ in 0..2 {
        let report = sync(&engine);
        assert!(!polled(&report).contains(ANA));
    }
    assert!(watchlist_calls(&plex.calls(), ANA_UUID).is_empty());
    assert_eq!(job_state(&engine, &job), "queued");
    assert_eq!(
        titles(&demands(&engine, ANA)),
        ["Synthetic First Film".to_owned()].into()
    );
    assert_eq!(jobs_len(&engine), 1);

    // On again: the film added meanwhile is requested.
    turn(&engine, ANA, true);
    sync(&engine);
    assert_eq!(
        titles(&demands(&engine, ANA)),
        ["Synthetic First Film", "Synthetic Second Film"]
            .map(String::from)
            .into()
    );
    assert_eq!(job_state(&engine, &job), "queued");
    assert_eq!(jobs_len(&engine), 2);
}

#[test]
fn a_renamed_friend_keeps_its_requester_and_a_removed_friend_is_reported_and_no_longer_read() {
    let dir = Directory::new();
    let plex = Fixture::open();
    plex.with(|p| p.friends = only_friends(&["ana", "ben", "dan"]));
    plex.films(ANA_UUID, &[("itemana", "Synthetic Ana Film", 701)]);
    plex.films(BEN_UUID, &[("itemben", "Synthetic Ben Film", 702)]);
    let engine = Engine::open_for_management(plex.config(&dir.0)).unwrap();
    engine.refresh_plex_friends().unwrap();
    turn(&engine, ANA, true);
    turn(&engine, BEN, true);
    sync(&engine);
    let ben_job = field(&demands(&engine, BEN)[0], "job_id")
        .unwrap()
        .to_owned();

    // Ana renames herself; Ben and Dan stop being friends.
    plex.with(|p| {
        let mut ana = only_friends(&["ana"]).remove(0);
        let Some(Value::Object(fields)) = ana.get_mut("user") else {
            unreachable!()
        };
        fields.insert("username".into(), "ana.renamed".into());
        fields.insert("displayName".into(), "Ana Renamed".into());
        p.friends = vec![ana];
    });
    engine.refresh_plex_friends().unwrap();
    let all = friends(&engine);
    // Dan, never followed, leaves nothing behind.
    assert_eq!(all.keys().collect::<Vec<_>>(), [ANA, BEN]);
    assert_eq!(field(&all[ANA], "name"), Some("Ana Renamed"));
    assert_eq!(field(&all[ANA], "username"), Some("ana.renamed"));
    assert_eq!(
        titles(&demands(&engine, ANA)),
        ["Synthetic Ana Film".to_owned()].into()
    );
    assert_eq!(
        state(&engine, BEN),
        ("left".into(), Some("No longer your friend on Plex".into()))
    );

    plex.calls();
    let report = sync(&engine);
    let calls = plex.calls();
    assert!(polled(&report).contains(ANA));
    assert!(!polled(&report).contains(BEN));
    assert!(watchlist_calls(&calls, BEN_UUID).is_empty());
    let idle = report.get("idle").unwrap().as_array().unwrap();
    assert_eq!(idle.len(), 1);
    assert_eq!(field(&idle[0], "account_id"), Some(BEN));
    assert_eq!(
        field(&idle[0], "error"),
        Some("No longer your friend on Plex")
    );
    // Ben's admitted work stays; he cannot be turned on again.
    assert_eq!(job_state(&engine, &ben_job), "queued");
    assert_eq!(
        engine.plex_friend(BEN, true).unwrap_err(),
        "Requester: No longer your friend on Plex"
    );
    assert_eq!(
        engine.plex_friend(DAN, true).unwrap_err(),
        "Requester: unknown Plex friend"
    );
    turn(&engine, BEN, false);
    assert_eq!(job_state(&engine, &ben_job), "queued");
}

#[test]
fn the_owner_token_never_leaves_the_requests_to_plex() {
    let dir = Directory::new();
    let plex = Fixture::open();
    let token = token();
    plex.with(|p| {
        p.friends = only_friends(&["ana", "ben", "cleo"]);
        // Plex echoing the token back, in an error and in a refusal.
        p.answers.insert(
            ANA_UUID.into(),
            (
                200,
                format!(r#"{{"data":null,"errors":[{{"message":"Token {token} cannot read this list"}}]}}"#),
            ),
        );
        p.answers
            .insert(BEN_UUID.into(), (401, format!("Invalid token {token}")));
        // An answer past the size limit.
        p.answers
            .insert(CLEO_UUID.into(), (200, format!(r#"{{"data":"{}"}}"#, "x".repeat(2 << 20))));
    });
    let engine = Engine::open_for_management(plex.config(&dir.0)).unwrap();
    engine.refresh_plex_friends().unwrap();
    for id in [ANA, BEN, CLEO] {
        turn(&engine, id, true);
    }
    let report = sync(&engine);
    assert_eq!(
        field(entry(&report, ANA), "error"),
        Some("Plex: Token [token] cannot read this list")
    );
    assert_eq!(
        field(entry(&report, BEN), "error"),
        Some("Plex: HTTP response 401: Invalid token [token]")
    );
    assert_eq!(
        field(entry(&report, CLEO), "error"),
        Some("Plex: network request failed")
    );
    assert_eq!(
        state(&engine, ANA),
        (
            "error".into(),
            Some("Plex: Token [token] cannot read this list".into())
        )
    );
    // The friend list refused with the token echoed back.
    plex.with(|p| p.friends_answer = Some((401, format!("Invalid token {token}"))));
    let error = engine.refresh_plex_friends().unwrap_err();
    assert_eq!(error, "Plex: HTTP response 401: Invalid token [token]");
    let requesters = json::stringify(&engine.requesters().unwrap());
    assert!(!requesters.contains(&token));
    assert!(requesters.contains("Invalid token [token]"));
    for id in [ANA, BEN, CLEO] {
        assert!(!json::stringify(&engine.requester(id, None, 10).unwrap()).contains(&token));
    }
    drop(engine);
    no_token_on_disk(&dir.0);

    // Plex's own origins, which no setting changes.
    let defaults = config::from_json(&config::default_json(), &dir.0).unwrap();
    assert_eq!(defaults.plex.community_url, "https://community.plex.tv/api");
    assert_eq!(defaults.plex.identity_url, "https://plex.tv/api/v2/user");
    assert_eq!(
        defaults.plex.metadata_url,
        "https://discover.provider.plex.tv/library/metadata/"
    );
    assert!(!json::stringify(&config::default_json()).contains("community"));
}

#[test]
fn a_friend_also_set_up_as_a_requester_account_cannot_be_turned_on() {
    let dir = Directory::new();
    let plex = Fixture::open();
    plex.with(|p| p.friends = only_friends(&["ana", "ben"]));
    plex.films(ANA_UUID, &[("itemana", "Synthetic Ana Film", 801)]);
    let mut cfg = plex.config(&dir.0);
    // Ben and the owner already have requester accounts of their own.
    cfg.requesters.accounts = [("ben", "7002"), ("me", "9001")]
        .into_iter()
        .map(|(id, user)| Account {
            id: id.into(),
            expected_user_id: user.into(),
            token_env: "MYNOU_TEST_SECRET_A".into(),
            identity_url: format!("{}/token/{user}/identity", plex.url),
            watchlist_url: format!("{}/token/{user}/watchlist", plex.url),
        })
        .collect();
    let engine = Engine::open_for_management(cfg).unwrap();
    engine.refresh_plex_friends().unwrap();
    assert_eq!(
        state(&engine, BEN),
        (
            "duplicate".into(),
            Some("Also set up as the requester account ben".into())
        )
    );
    assert_eq!(
        engine.plex_friend(BEN, true).unwrap_err(),
        "Requester: Also set up as the requester account ben"
    );
    turn(&engine, ANA, true);
    let report = sync(&engine);
    // The owner's list is read once, by its own account.
    assert_eq!(polled(&report), ["ben", "me", ANA].map(String::from).into());
    let accounts = engine.requesters().unwrap();
    assert_eq!(
        accounts.get("plex").unwrap().get("owner"),
        Some(&Value::Null)
    );
    assert!(
        !accounts
            .get("accounts")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|a| field(a, "id") == Some(OWNER) || field(a, "id") == Some(BEN))
    );
}

#[test]
fn settings_list_and_switch_friends_through_the_api() {
    let dir = Directory::new();
    let plex = Fixture::open();
    plex.with(|p| p.friends = only_friends(&["ana"]));
    plex.films(ANA_UUID, &[("itemana", "Synthetic Ana Film", 901)]);
    let server = api_support::Server::open(plex.config(&dir.0));
    let call = |route: &str, body: &str| {
        let reply = server.call(
            "POST",
            route,
            &[
                ("Authorization", &format!("Bearer {TOKEN}")),
                ("Content-Type", "application/json"),
            ],
            body,
        );
        assert!(!reply.body.contains(&token()));
        reply
    };
    let listed = call("/api/plex/friends", "");
    assert_eq!(listed.status, 200, "{}", listed.body);
    let listed = json::parse(&listed.body).unwrap();
    assert_eq!(
        field(&listed.get("friends").unwrap().as_array().unwrap()[0], "id"),
        Some(ANA)
    );

    let on = call(&format!("/api/plex/friends/{ANA}"), r#"{"enabled":true}"#);
    assert_eq!(on.status, 200, "{}", on.body);
    assert_eq!(
        field(&json::parse(&on.body).unwrap(), "state"),
        Some("reading")
    );
    for (route, body) in [
        (format!("/api/plex/friends/{ANA}"), r#"{"enabled":"yes"}"#),
        (
            format!("/api/plex/friends/{ANA}"),
            r#"{"enabled":true,"x":1}"#,
        ),
        (format!("/api/plex/friends/{ANA}"), ""),
    ] {
        assert_eq!(call(&route, body).status, 400, "{body}");
    }
    assert_eq!(
        call("/api/plex/friends/ana", r#"{"enabled":true}"#).status,
        404
    );
    assert_ne!(
        call("/api/plex/friends/plex-1", r#"{"enabled":true}"#).status,
        200
    );

    let reply = server.call(
        "GET",
        "/api/requesters",
        &[("Authorization", &format!("Bearer {TOKEN}"))],
        "",
    );
    assert_eq!(reply.status, 200);
    assert!(!reply.body.contains(&token()));
    let value = json::parse(&reply.body).unwrap();
    let plex_value = value.get("plex").unwrap();
    assert_eq!(plex_value.get("connected").unwrap().as_bool(), Some(true));
    let friend = &plex_value.get("friends").unwrap().as_array().unwrap()[0];
    assert_eq!(friend.get("enabled").unwrap().as_bool(), Some(true));
    assert_eq!(field(friend, "name"), Some("Ana"));
    assert!(
        value
            .get("accounts")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|a| field(a, "id") == Some(ANA))
    );
}

#[test]
fn without_a_plex_connection_friends_are_not_read() {
    let dir = Directory::new();
    let plex = Fixture::open();
    let mut cfg = plex.config(&dir.0);
    cfg.plex.enabled = false;
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    assert_eq!(
        engine.refresh_plex_friends().unwrap_err(),
        "Plex is not connected"
    );
    let requesters = engine.requesters().unwrap();
    assert_eq!(
        requesters
            .get("plex")
            .unwrap()
            .get("connected")
            .unwrap()
            .as_bool(),
        Some(false)
    );
    drop(engine);

    // Connected, a friend turned on, then disconnected: it says why it waits.
    cfg.plex.enabled = true;
    plex.with(|p| p.friends = only_friends(&["ana"]));
    plex.films(ANA_UUID, &[("itemana", "Synthetic Ana Film", 911)]);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    engine.refresh_plex_friends().unwrap();
    turn(&engine, ANA, true);
    drop(engine);
    cfg.plex.enabled = false;
    let engine = Engine::open_for_management(cfg).unwrap();
    plex.calls();
    let report = sync(&engine);
    assert!(polled(&report).is_empty());
    assert!(plex.calls().is_empty());
    let idle = report.get("idle").unwrap().as_array().unwrap();
    assert_eq!(field(&idle[0], "error"), Some("Plex is not connected"));
    assert_eq!(
        state(&engine, ANA),
        ("error".into(), Some("Plex is not connected".into()))
    );
}
