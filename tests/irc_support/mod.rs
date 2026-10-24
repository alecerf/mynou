//! Original loopback announcement fixtures; no public services or torrents.
#![allow(dead_code)]
use mynou::{
    config::{self, Config},
    crypto::sha256,
    engine::{Engine, lock},
    irc::ControlRequest,
    json::{self, Value},
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
pub const SENDER: &str = "announce!bot@fixture";
pub fn settings(url: &str) -> Value {
    let mut source = Value::object();
    source.insert("id", "local");
    source.insert("url", url);
    source.insert("enabled", true);
    source.insert("nickname", "mynou");
    source.insert("channel", "#announces");
    source.insert("sender", SENDER);
    source.insert("reconnect_min_secs", 1_u32);
    source.insert("reconnect_max_secs", 1_u32);
    let mut rule = Value::object();
    rule.insert("id", "movies");
    rule.insert("source", "local");
    rule.insert("enabled", true);
    rule.insert("kind", "movie");
    rule.insert("profile", "any");
    rule.insert("action", "review");
    let mut v = Value::object();
    v.insert("sources", Value::Array(vec![source]));
    v.insert("rules", Value::Array(vec![rule]));
    v
}
pub fn value(url: &str) -> Value {
    let mut v = config::default_json();
    v.insert("listen", "127.0.0.1:0");
    let d = v.get_mut("downloads").unwrap();
    d.insert("enabled", false);
    d.insert("dht", false);
    d.insert("pex", false);
    d.insert("listen_port", 0_u32);
    v.insert("irc", settings(url));
    v
}
pub fn config(root: &Path) -> Config {
    config::from_json(&value("irc://127.0.0.1:1"), root).unwrap()
}
pub fn announcement(id: u64) -> Value {
    let mut v = Value::object();
    v.insert("title", "Fixture.Movie.2024.1080p.BluRay.x264");
    v.insert("media_title", "Fixture Movie");
    v.insert("kind", "movie");
    v.insert("year", 2024_u32);
    v.insert("tmdb_id", id.to_string());
    v.insert("info_hash", "1234567890abcdef1234567890abcdef12345678");
    v
}
pub fn receive(engine: &Engine, id: u64) -> Value {
    engine
        .irc_receive("local", SENDER, "#announces", &announcement(id))
        .unwrap()
}
pub fn query(action: &str) -> ControlRequest {
    ControlRequest {
        action: action.into(),
        apply: false,
        plan_id: None,
    }
}
pub fn reviewed(engine: &Engine, id: &str, action: &str) -> ControlRequest {
    let mut q = query(action);
    let v = engine.irc_control(id, &q).unwrap();
    q.apply = true;
    q.plan_id = Some(v.get("plan_id").unwrap().as_str().unwrap().into());
    q
}
pub fn record_id(v: &Value) -> &str {
    v.get("id").unwrap().as_str().unwrap()
}
pub fn rows(engine: &Engine) -> Vec<Value> {
    engine
        .irc_announcements(0, 200)
        .unwrap()
        .get("announcements")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec()
}
pub fn no_jobs(engine: &Engine) {
    assert!(lock(&engine.store).unwrap().list().unwrap().is_empty());
}
fn fixture_path(path: &Path) -> mynou::Result<PathBuf> {
    let declared_temporary = std::env::temp_dir();
    let temporary = declared_temporary
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let relative = path
        .strip_prefix(&declared_temporary)
        .or_else(|_| path.strip_prefix(&temporary))
        .map_err(|_| "Fixture path escaped temporary storage")?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err("Fixture path must be strictly inside temporary storage".into());
    }
    // Permit only the system temporary-root alias. Any other symbolic link
    // changes this expected physical identity and is refused without statting
    // the original caller-supplied path.
    let expected = temporary.join(relative);
    let normalized = path.canonicalize().map_err(|error| error.to_string())?;
    if !normalized.as_path().starts_with(&temporary) {
        return Err("Fixture path escaped temporary storage".into());
    }
    if normalized == temporary {
        return Err("Fixture path must be strictly inside temporary storage".into());
    }
    if normalized != expected {
        return Err("Fixture path contains a symbolic link".into());
    }
    Ok(normalized)
}

pub fn bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(
        root: &Path,
        directory: &Path,
        files: &mut BTreeMap<PathBuf, Vec<u8>>,
    ) -> mynou::Result<()> {
        let directory = fixture_path(directory)?;
        if !directory.as_path().starts_with(root) {
            return Err("Fixture directory escaped its root".into());
        }
        for entry in fs::read_dir(&directory).map_err(|error| error.to_string())? {
            let path = entry.map_err(|error| error.to_string())?.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("Fixture entry must not be a symbolic link".into());
            }
            if metadata.is_dir() {
                walk(root, &path, files)?;
            } else if metadata.is_file() {
                files.insert(
                    path.clone(),
                    fs::read(path).map_err(|error| error.to_string())?,
                );
            }
        }
        Ok(())
    }
    let root = fixture_path(root).unwrap();
    let mut files = BTreeMap::new();
    walk(&root, &root, &mut files).unwrap();
    files
}
pub fn snapshot(root: &Path) -> Value {
    let b = fs::read(root.join("announcements.bin")).unwrap();
    if b.starts_with(b"MYNOUI05") {
        let mut index =
            mynou::inventory::Index::open(&root.join("announcements.idx"), true).unwrap();
        let mut v =
            json::parse(std::str::from_utf8(&index.get(&[0; 64]).unwrap().unwrap()).unwrap())
                .unwrap();
        let mut rows = Vec::new();
        let mut after = None;
        let mut start = [0; 64];
        start[0] = 1;
        let mut end = [0; 64];
        end[0] = 2;
        loop {
            let page = index
                .page(&mynou::inventory::Query {
                    start,
                    end: Some(end),
                    after,
                    limit: 200,
                    sequence: None,
                })
                .unwrap();
            rows.extend(
                page.entries
                    .iter()
                    .map(|(_, b)| json::parse(std::str::from_utf8(b).unwrap()).unwrap()),
            );
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        v.insert("records", Value::Array(rows));
        v
    } else {
        json::parse(std::str::from_utf8(&b[16..b.len() - 32]).unwrap()).unwrap()
    }
}
/// Deliberately replace the synthetic fixture with an earlier format for migration tests.
pub fn remove_index(root: &Path) {
    for e in fs::read_dir(root).unwrap() {
        let path = e.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        if name == "announcements.idx" || name.starts_with("announcements.idx.staging") {
            fs::remove_dir_all(path).unwrap();
        } else if name.starts_with("announcements.migration-")
            || name.starts_with("announcements.legacy-")
        {
            fs::remove_file(path).unwrap();
        }
    }
}
pub fn write_snapshot(root: &Path, v: &Value) {
    write_snapshot_format(root, b"MYNOUI01", v);
}
pub fn write_snapshot_format(root: &Path, magic: &[u8; 8], v: &Value) {
    remove_index(root);
    let p = json::stringify(v).into_bytes();
    let mut b = magic.to_vec();
    b.extend_from_slice(&(p.len() as u64).to_le_bytes());
    b.extend_from_slice(&p);
    b.extend_from_slice(&sha256(&b));
    fs::write(root.join("announcements.bin"), b).unwrap();
}
pub fn no_credentials(v: &Value) {
    let text = json::stringify(v);
    assert!(!text.contains(&std::env::var("PWD").unwrap()));
    for s in [
        "password_env",
        "join_key_env",
        "ircs://",
        "irc://",
        "PASS ",
        "source_url",
        "acquisition_url",
    ] {
        assert!(!text.contains(s));
    }
}
pub fn wait(f: impl FnMut() -> bool) {
    wait_for(Duration::from_secs(5), f);
}
/// Allow a whole live journey more headroom than a single protocol exchange.
pub fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(
            Instant::now() < deadline,
            "IRC fixture deadline exceeded ({timeout:?})"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
pub fn listener() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let url = format!("irc://{}", l.local_addr().unwrap());
    (l, url)
}
pub fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        match listener.accept() {
            Ok((s, _)) => {
                // macOS keeps the listener's non-blocking mode on accepted sockets.
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                s.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
                s.set_nodelay(true).unwrap();
                return s;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("Local IRC listener failed: {e}"),
        }
    }
}
pub fn line(stream: &mut TcpStream) -> String {
    let mut v = Vec::new();
    while !v.ends_with(b"\r\n") {
        assert!(v.len() < 512);
        let mut b = [0];
        stream.read_exact(&mut b).unwrap();
        v.push(b[0]);
    }
    String::from_utf8(v[..v.len() - 2].to_vec()).unwrap()
}
pub fn send(stream: &mut TcpStream, s: &str) {
    stream.write_all(s.as_bytes()).unwrap();
    stream.write_all(b"\r\n").unwrap();
}
pub fn registration(stream: &mut TcpStream, secret: bool) {
    if secret {
        assert!(line(stream) == format!("PASS {}", std::env::var("PWD").unwrap()));
    }
    assert_eq!(line(stream), "NICK mynou");
    assert_eq!(line(stream), "USER mynou 0 * :Mynou");
    send(stream, ":server 001 mynou :Welcome");
    if secret {
        assert!(line(stream) == format!("JOIN #announces {}", std::env::var("PWD").unwrap()));
    } else {
        assert_eq!(line(stream), "JOIN #announces");
    }
    send(stream, ":mynou!user@fixture JOIN :#announces");
}
pub fn announce(stream: &mut TcpStream, sender: &str, channel: &str, v: &Value) {
    send(
        stream,
        &format!(":{sender} PRIVMSG {channel} :MYNOU {}", json::stringify(v)),
    );
}
pub fn closed(stream: &mut TcpStream) {
    let mut byte = [0];
    assert!(matches!(stream.read(&mut byte), Ok(0) | Err(_)));
}
