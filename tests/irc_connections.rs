//! Native loopback IRC journeys, bounded reconnect and interruptible reads.
mod irc_support;
mod library_support;
use irc_support::*;
use library_support::Directory;
use mynou::{config, engine::Engine, json::Value};
use std::{
    io::{Read, Write},
    thread,
    time::{Duration, Instant},
};
fn received(engine: &Engine, n: u64) -> bool {
    engine
        .irc_sources()
        .unwrap()
        .get("sources")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .get("health")
        .unwrap()
        .get("received")
        .unwrap()
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap()
        >= n
}
#[test]
fn live_registration_fragments_ping_and_allowlists_precede_durable_receipts() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let mut v = value(&dir.0, &url);
    let Value::Array(s) = v.get_mut("irc").unwrap().get_mut("sources").unwrap() else {
        panic!()
    };
    s[0].insert("password_env", "PWD");
    s[0].insert("join_key_env", "PWD");
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        assert!(line(&mut stream) == format!("PASS {}", std::env::var("PWD").unwrap()));
        assert_eq!(line(&mut stream), "NICK mynou");
        assert_eq!(line(&mut stream), "USER mynou 0 * :Mynou");
        announce(&mut stream, SENDER, "#announces", &announcement(99));
        send(&mut stream, ":server 001 mynou :Welcome");
        assert!(line(&mut stream) == format!("JOIN #announces {}", std::env::var("PWD").unwrap()));
        announce(&mut stream, SENDER, "#announces", &announcement(99));
        send(&mut stream, ":mynou!user@fixture JOIN :#announces");
        send(&mut stream, "PING :local-nonce");
        assert_eq!(line(&mut stream), "PONG :local-nonce");
        announce(
            &mut stream,
            "wrong!bot@fixture",
            "#announces",
            &announcement(100),
        );
        announce(&mut stream, SENDER, "#other", &announcement(101));
        let wire = format!(
            ":{SENDER} PRIVMSG #announces :MYNOU {}\r\n",
            mynou::json::stringify(&announcement(7))
        );
        for chunk in wire.as_bytes().chunks(7) {
            stream.write_all(chunk).unwrap();
        }
        announce(&mut stream, SENDER, "#announces", &announcement(7));
        let mut changed = announcement(7);
        changed.insert("title", "Same identity, changed claim");
        announce(&mut stream, SENDER, "#announces", &changed);
        closed(&mut stream);
    });
    let engine = Engine::open(cfg).unwrap();
    let workers = engine.start();
    wait(|| received(&engine, 3));
    assert_eq!(rows(&engine).len(), 1);
    assert_eq!(
        rows(&engine)[0]
            .get("announcement")
            .unwrap()
            .get("tmdb_id")
            .unwrap()
            .as_str(),
        Some("7")
    );
    let report = engine.irc_sources().unwrap();
    no_credentials(&report);
    assert_eq!(
        report.get("sources").unwrap().as_array().unwrap()[0]
            .get("health")
            .unwrap()
            .get("duplicates")
            .unwrap()
            .as_str(),
        Some("2")
    );
    no_jobs(&engine);
    let start = Instant::now();
    drop(workers);
    assert!(start.elapsed() < Duration::from_secs(2));
    peer.join().unwrap();
}
#[test]
fn reconnect_and_process_restart_retain_the_original_duplicate_identity() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&value(&dir.0, &url), &dir.0).unwrap();
    let peer = thread::spawn(move || {
        for pass in 0..3 {
            let mut stream = accept(&listener);
            registration(&mut stream, false);
            announce(&mut stream, SENDER, "#announces", &announcement(7));
            if pass != 0 {
                closed(&mut stream);
            }
        }
    });
    let engine = Engine::open(cfg.clone()).unwrap();
    let workers = engine.start();
    wait(|| received(&engine, 2));
    let row = rows(&engine).remove(0);
    let before = std::fs::read(cfg.store_dir.join("announcements.bin")).unwrap();
    drop(workers);
    drop(engine);
    let engine = Engine::open(cfg.clone()).unwrap();
    let workers = engine.start();
    wait(|| received(&engine, 1));
    assert_eq!(rows(&engine).remove(0), row);
    assert_eq!(
        std::fs::read(cfg.store_dir.join("announcements.bin")).unwrap(),
        before
    );
    assert_eq!(
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
            .unwrap()
            .as_str(),
        Some("1")
    );
    no_jobs(&engine);
    drop(workers);
    peer.join().unwrap();
}
#[test]
fn failures_redact_server_text_and_shutdown_interrupts_an_unfinished_line() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&value(&dir.0, &url), &dir.0).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        assert_eq!(line(&mut stream), "NICK mynou");
        assert_eq!(line(&mut stream), "USER mynou 0 * :Mynou");
        send(
            &mut stream,
            &format!(":server 464 mynou :{}", std::env::var("PWD").unwrap()),
        );
        drop(stream);
        let mut stream = accept(&listener);
        registration(&mut stream, false);
        stream.write_all(b":partial").unwrap();
        ready_tx.send(()).unwrap();
        closed(&mut stream);
    });
    let engine = Engine::open(cfg).unwrap();
    let workers = engine.start();
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
            .get("phase")
            .unwrap()
            .as_str()
            == Some("backoff")
    });
    no_credentials(&engine.irc_sources().unwrap());
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    no_credentials(&engine.irc_sources().unwrap());
    assert!(rows(&engine).is_empty());
    no_jobs(&engine);
    let start = Instant::now();
    drop(workers);
    assert!(start.elapsed() < Duration::from_secs(2));
    peer.join().unwrap();
}
#[test]
fn an_invalid_remote_tls_record_never_reaches_irc_registration() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&value(&dir.0, &url.replace("irc://", "ircs://")), &dir.0).unwrap();
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        let mut first = [0; 5];
        stream.read_exact(&mut first).unwrap();
        assert_eq!(first[0], 22);
        stream.write_all(&[23, 3, 3, 0, 0]).unwrap();
    });
    let engine = Engine::open(cfg).unwrap();
    let workers = engine.start();
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
            .get("phase")
            .unwrap()
            .as_str()
            == Some("backoff")
    });
    no_credentials(&engine.irc_sources().unwrap());
    assert!(rows(&engine).is_empty());
    no_jobs(&engine);
    drop(workers);
    peer.join().unwrap();
}
fn rejected(engine: &Engine) -> String {
    engine
        .irc_sources()
        .unwrap()
        .get("sources")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .get("health")
        .unwrap()
        .get("rejected")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}
fn attempts(engine: &Engine) -> String {
    engine
        .irc_sources()
        .unwrap()
        .get("sources")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .get("health")
        .unwrap()
        .get("attempts")
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}
fn wire(body: &str) -> String {
    format!(":{SENDER} PRIVMSG #announces :{body}\r\n")
}
fn delimited() -> Value {
    let mut format = Value::object();
    format.insert("type", "delimited");
    format.insert("prefix", "NEW | ");
    format.insert("separator", " | ");
    format.insert("suffix", " END");
    format.insert(
        "fields",
        Value::Array(
            [
                "title",
                "kind",
                "media_title",
                "year",
                "tmdb_id",
                "info_hash",
            ]
            .into_iter()
            .map(Value::from)
            .collect(),
        ),
    );
    format
}
const MARKER: &str = "private-marker";
/// One connection receives `malformed`, a valid announcement and a PING, in a single
/// write or in small fragments, then a second PING; it must never reconnect.
fn malformed_then_valid(format: Option<Value>, malformed: &str, valid: &str, fragmented: bool) {
    let dir = Directory::new();
    let (listener, url) = listener();
    let mut v = value(&dir.0, &url);
    if let Some(format) = format {
        let Value::Array(s) = v.get_mut("irc").unwrap().get_mut("sources").unwrap() else {
            panic!()
        };
        s[0].insert("announcement_format", format);
    }
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let batch = format!("{}{}PING :after-reject\r\n", wire(malformed), wire(valid));
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        registration(&mut stream, false);
        if fragmented {
            for chunk in batch.as_bytes().chunks(5) {
                stream.write_all(chunk).unwrap();
                stream.flush().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        } else {
            stream.write_all(batch.as_bytes()).unwrap();
        }
        assert_eq!(line(&mut stream), "PONG :after-reject");
        send(&mut stream, "PING :still-open");
        assert_eq!(line(&mut stream), "PONG :still-open");
        closed(&mut stream);
    });
    let engine = Engine::open(cfg).unwrap();
    let workers = engine.start();
    wait(|| received(&engine, 1));
    assert_eq!(rejected(&engine), "1");
    assert_eq!(attempts(&engine), "1");
    let report = engine.irc_sources().unwrap();
    no_credentials(&report);
    assert!(!mynou::json::stringify(&report).contains(MARKER));
    assert_eq!(rows(&engine).len(), 1);
    assert_eq!(
        rows(&engine)[0]
            .get("announcement")
            .unwrap()
            .get("tmdb_id")
            .unwrap()
            .as_str(),
        Some("7")
    );
    no_jobs(&engine);
    drop(workers);
    peer.join().unwrap();
    assert_eq!(attempts(&engine), "1");
}
#[test]
fn a_malformed_json_announcement_is_rejected_without_losing_the_connection() {
    let valid = format!("MYNOU {}", mynou::json::stringify(&announcement(7)));
    let mut invalid = announcement(8);
    invalid.insert("tmdb_id", format!("not-a-number-{MARKER}"));
    for malformed in [
        format!("MYNOU {{\"title\":\"{MARKER}\","),
        format!("MYNOU {}", mynou::json::stringify(&invalid)),
    ] {
        for fragmented in [false, true] {
            malformed_then_valid(None, &malformed, &valid, fragmented);
        }
    }
}
#[test]
fn a_malformed_delimited_announcement_is_rejected_without_losing_the_connection() {
    let valid = "NEW | Fixture.Movie.2024.1080p.BluRay.x264 | movie | Fixture Movie | 2024 | 7 | 1234567890abcdef1234567890abcdef12345678 END";
    for malformed in [
        format!("NEW | Fixture.Movie | movie | {MARKER} END"),
        format!("{} | {MARKER} END", &valid[..valid.len() - 4]),
        format!("{} {MARKER}", &valid[..valid.len() - 4]),
    ] {
        for fragmented in [false, true] {
            malformed_then_valid(Some(delimited()), &malformed, valid, fragmented);
        }
    }
}
