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
pub fn value(root: &Path, url: &str) -> Value {
    let mut v = config::default_json();
    v.insert("listen", "127.0.0.1:0");
    v.insert("store_dir", root.join("jobs").to_str().unwrap());
    let d = v.get_mut("downloads").unwrap();
    d.insert("enabled", false);
    d.insert("dht", false);
    d.insert("pex", false);
    d.insert("listen_port", 0_u32);
    v.insert("irc", settings(url));
    v
}
pub fn config(root: &Path) -> Config {
    config::from_json(&value(root, "irc://127.0.0.1:1"), root).unwrap()
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
    assert!(lock(&engine.store).unwrap().list().is_empty());
}
pub fn bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fs::read_dir(root)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            p.is_file().then(|| (p.clone(), fs::read(p).unwrap()))
        })
        .collect()
}
pub fn snapshot(root: &Path) -> Value {
    let b = fs::read(root.join("announcements.bin")).unwrap();
    json::parse(std::str::from_utf8(&b[16..b.len() - 32]).unwrap()).unwrap()
}
pub fn write_snapshot(root: &Path, v: &Value) {
    let p = json::stringify(v).into_bytes();
    let mut b = b"MYNOUI01".to_vec();
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
pub fn wait(mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "IRC fixture deadline exceeded");
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
