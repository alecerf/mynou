//! Original service-authentication fixtures run only against local synthetic peers.
mod irc_support;
mod library_support;
use irc_support::*;
use library_support::Directory;
use mynou::{
    config,
    engine::Engine,
    irc::protocol::{Decoder, Event, Message, Protocol},
    json::{self, Value},
};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const SERVICE: &str = "NickServ!service@fixture";
fn source(v: &mut Value) -> &mut Value {
    let Value::Array(s) = v.get_mut("irc").unwrap().get_mut("sources").unwrap() else {
        panic!()
    };
    &mut s[0]
}
fn nickserv() -> Value {
    json::parse(r#"{"account":"fixture","password_env":"PWD","service":"NickServ","sender":"NickServ!service@fixture","success_notice":"Identified as {account}","failure_notices":["Invalid password","Account unavailable"]}"#).unwrap()
}
fn configured(dir: &Directory, url: &str) -> Value {
    let mut v = value(&dir.0, url);
    source(&mut v).insert("nickserv", nickserv());
    v
}
fn protocol() -> Protocol {
    let dir = Directory::new();
    let cfg = config::from_json(&configured(&dir, "irc://127.0.0.1:1"), &dir.0).unwrap();
    Protocol::new(cfg.irc.sources[0].clone())
}
fn event(p: &mut Protocol, line: &str) -> Event {
    p.receive(&Message::parse(line).unwrap()).unwrap()
}
fn health(e: &Engine) -> Value {
    e.irc_sources()
        .unwrap()
        .get("sources")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .get("health")
        .unwrap()
        .clone()
}
fn private(v: &Value) {
    no_credentials(v);
    let text = json::stringify(v);
    for forbidden in [
        "IDENTIFY ",
        "NickServ!service@fixture",
        "success_notice",
        "failure_notices",
        "password_env",
    ] {
        assert!(!text.contains(forbidden));
    }
}
fn identify(stream: &mut TcpStream) {
    assert_eq!(line(stream), "NICK mynou");
    assert_eq!(line(stream), "USER mynou 0 * :Mynou");
    send(stream, ":server 001 mynou :Welcome");
    assert!(
        line(stream)
            == format!(
                "PRIVMSG NickServ :IDENTIFY fixture {}",
                std::env::var("PWD").unwrap()
            )
    );
}
fn confirm(stream: &mut TcpStream) {
    send(
        stream,
        &format!(":{SERVICE} NOTICE mynou :Identified as fixture"),
    );
    assert_eq!(line(stream), "JOIN #announces");
    send(stream, ":mynou!user@fixture JOIN :#announces");
}

#[test]
fn service_configuration_is_strict_and_legacy_source_bindings_are_unchanged() {
    let dir = Directory::new();
    let mut old = value(&dir.0, "irc://127.0.0.1:1");
    let cfg = config::from_json(&old, &dir.0).unwrap();
    source(&mut old).insert("nickserv", Value::Null);
    assert_eq!(
        config::from_json(&old, &dir.0).unwrap().irc.sources[0].binding(),
        cfg.irc.sources[0].binding()
    );
    for (key, bad) in [
        ("account", Value::from("")),
        ("account", Value::from("space account")),
        ("account", Value::from("a".repeat(65))),
        ("password_env", Value::Null),
        ("password_env", Value::from("lowercase")),
        ("service", Value::from("Nick Serv")),
        ("sender", Value::from("NickServ")),
        ("sender", Value::from("Other!service@fixture")),
        ("sender", Value::from("NickServ!service@bad\r\n")),
        ("success_notice", Value::from("Identified")),
        ("success_notice", Value::from("{account}{account}")),
        ("success_notice", Value::from("{account}{unknown}")),
        (
            "success_notice",
            Value::from(format!("{}{{account}}", "a".repeat(256))),
        ),
        ("failure_notices", Value::Array(vec![])),
        (
            "failure_notices",
            Value::Array(vec![Value::from("Invalid password"); 9]),
        ),
        (
            "failure_notices",
            Value::Array(vec![Value::from("{account}")]),
        ),
        (
            "failure_notices",
            Value::Array(vec![Value::from("Identified as fixture")]),
        ),
        ("password", Value::from("private-value")),
    ] {
        let mut v = configured(&dir, "irc://127.0.0.1:1");
        source(&mut v).get_mut("nickserv").unwrap().insert(key, bad);
        let error = config::from_json(&v, &dir.0).unwrap_err();
        assert!(!error.contains("private-value"));
    }
    let mut both = configured(&dir, "irc://127.0.0.1:1");
    source(&mut both).insert(
        "sasl",
        json::parse(r#"{"mechanism":"PLAIN","username_env":"PWD","password_env":"PWD"}"#).unwrap(),
    );
    assert!(config::from_json(&both, &dir.0).is_err());
    let e = Engine::open(cfg.clone()).unwrap();
    receive(&e, 7);
    drop(e);
    let before = bytes(&cfg.store_dir);
    assert!(
        Engine::open(config::from_json(&configured(&dir, "irc://127.0.0.1:1"), &dir.0).unwrap())
            .is_err()
    );
    assert_eq!(bytes(&cfg.store_dir), before);
}

#[test]
fn exact_service_account_and_membership_are_required_before_announcements() {
    let mut p = protocol();
    let trusted = format!(":{SENDER} PRIVMSG #announces :MYNOU {{}}");
    assert_eq!(
        event(&mut p, ":user!u@h 001 mynou :Forged welcome"),
        Event::Ignore
    );
    assert_eq!(event(&mut p, &trusted), Event::Ignore);
    assert_eq!(event(&mut p, ":server 001 mynou :Welcome"), Event::Identify);
    for wire in [
        ":server 001 mynou :Repeated welcome",
        ":server 366 mynou #announces :Premature membership",
        ":mynou!user@fixture JOIN :#announces",
        ":NickServ!impostor@fixture NOTICE mynou :Identified as fixture",
        ":NickServ!service@fixture NOTICE other :Identified as fixture",
        ":NickServ!service@fixture PRIVMSG mynou :Identified as fixture",
        ":NickServ!service@fixture NOTICE mynou :Identified as other",
        ":NickServ!service@fixture NOTICE mynou :Identified as fixture extra",
    ] {
        assert_eq!(event(&mut p, wire), Event::Ignore);
    }
    assert_eq!(event(&mut p, &trusted), Event::Ignore);
    assert_eq!(
        event(&mut p, "PING :identifying"),
        Event::Reply("PONG :identifying".into())
    );
    let wire = format!(":{SERVICE} NOTICE mynou :\u{2}Identified as fixture\u{f}\r\n");
    let mut decoder = Decoder::default();
    for chunk in wire.as_bytes().chunks(3) {
        for m in decoder.feed(chunk).unwrap() {
            assert_eq!(p.receive(&m).unwrap(), Event::Join);
        }
    }
    assert_eq!(event(&mut p, &trusted), Event::Ignore);
    assert_eq!(
        event(
            &mut p,
            ":NickServ!service@fixture NOTICE mynou :Identified as fixture"
        ),
        Event::Ignore
    );
    assert_eq!(
        event(&mut p, ":user!u@h 366 mynou #announces :Forged membership"),
        Event::Ignore
    );
    assert_eq!(
        event(&mut p, ":server 366 mynou #announces :End of names"),
        Event::Joined
    );
    assert!(matches!(event(&mut p, &trusted), Event::Announcement(_)));
}

#[test]
fn premature_confirmation_rejection_and_lost_identification_invalidate_the_connection() {
    let mut p = protocol();
    assert!(
        p.receive(
            &Message::parse(&format!(":{SERVICE} NOTICE mynou :Identified as fixture")).unwrap()
        )
        .is_err()
    );
    assert_eq!(
        p.receive(&Message::parse(":server 001 mynou :Welcome").unwrap())
            .unwrap_err(),
        "IRC: NickServ requires a new connection"
    );
    for joined in [false, true] {
        let mut p = protocol();
        event(&mut p, ":server 001 mynou :Welcome");
        if joined {
            event(
                &mut p,
                &format!(":{SERVICE} NOTICE mynou :Identified as fixture"),
            );
            event(&mut p, ":mynou!u@h JOIN :#announces");
        }
        assert_eq!(
            p.receive(
                &Message::parse(&format!(":{SERVICE} NOTICE mynou :Invalid password")).unwrap()
            )
            .unwrap_err(),
            "IRC: required NickServ identification failed"
        );
        assert!(
            p.receive(
                &Message::parse(&format!(":{SERVICE} NOTICE mynou :Identified as fixture"))
                    .unwrap()
            )
            .is_err()
        );
    }
}

#[test]
fn live_reconnect_and_restart_repeat_identification_and_retain_first_receipts() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&configured(&dir, &url), &dir.0).unwrap();
    let peer = thread::spawn(move || {
        for pass in 0..3 {
            let mut stream = accept(&listener);
            identify(&mut stream);
            send(&mut stream, ":mynou!user@fixture JOIN :#announces");
            announce(&mut stream, SENDER, "#announces", &announcement(99));
            confirm(&mut stream);
            announce(&mut stream, SENDER, "#announces", &announcement(7));
            if pass != 0 {
                closed(&mut stream);
            }
        }
    });
    let e = Engine::open(cfg.clone()).unwrap();
    let workers = e.start();
    wait(|| health(&e).get("received").unwrap().as_str() == Some("2"));
    assert_eq!(
        health(&e).get("nickserv_authenticated").unwrap().as_bool(),
        Some(true)
    );
    assert_eq!(rows(&e).len(), 1);
    let before = bytes(&cfg.store_dir);
    private(&e.irc_sources().unwrap());
    no_jobs(&e);
    drop(workers);
    assert_eq!(
        health(&e).get("nickserv_authenticated").unwrap().as_bool(),
        Some(false)
    );
    drop(e);
    let e = Engine::open(cfg.clone()).unwrap();
    let workers = e.start();
    wait(|| health(&e).get("received").unwrap().as_str() == Some("1"));
    assert_eq!(health(&e).get("duplicates").unwrap().as_str(), Some("1"));
    assert_eq!(bytes(&cfg.store_dir), before);
    no_jobs(&e);
    drop(workers);
    peer.join().unwrap();
}

#[test]
fn rejected_service_notices_are_redacted_and_shutdown_interrupts_identification() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&configured(&dir, &url), &dir.0).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        identify(&mut stream);
        send(
            &mut stream,
            &format!(":{SERVICE} NOTICE mynou :Invalid password"),
        );
        closed(&mut stream);
        let mut stream = accept(&listener);
        identify(&mut stream);
        stream
            .write_all(b":NickServ!service@fixture NOTICE mynou :Identified as ")
            .unwrap();
        tx.send(()).unwrap();
        closed(&mut stream);
    });
    let e = Engine::open(cfg).unwrap();
    let workers = e.start();
    wait(|| health(&e).get("phase").unwrap().as_str() == Some("backoff"));
    assert_eq!(
        health(&e).get("nickserv_authenticated").unwrap().as_bool(),
        Some(false)
    );
    private(&e.irc_sources().unwrap());
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(rows(&e).is_empty());
    no_jobs(&e);
    let started = Instant::now();
    drop(workers);
    assert!(started.elapsed() < Duration::from_secs(2));
    peer.join().unwrap();
}

#[test]
fn unavailable_service_credentials_fail_before_opening_a_socket() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let mut v = configured(&dir, &url);
    let absent = format!("MYNOU_NICKSERV_ABSENT_{}", std::process::id());
    assert!(std::env::var_os(&absent).is_none());
    source(&mut v)
        .get_mut("nickserv")
        .unwrap()
        .insert("password_env", absent);
    let e = Engine::open(config::from_json(&v, &dir.0).unwrap()).unwrap();
    let workers = e.start();
    wait(|| health(&e).get("phase").unwrap().as_str() == Some("backoff"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(rows(&e).is_empty());
    private(&e.irc_sources().unwrap());
    no_jobs(&e);
    drop(workers);
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn native_cli_sends_a_maximum_password_once_without_recording_or_logging_it() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let mut v = configured(&dir, &url);
    source(&mut v)
        .get_mut("nickserv")
        .unwrap()
        .insert("password_env", "MYNOU_NICKSERV_TEST_PASSWORD");
    let path = dir.0.join("mynou.json");
    fs::write(&path, json::stringify(&v)).unwrap();
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let log_path = dir.0.join("process.log");
    let log = fs::File::create(&log_path).unwrap();
    let password = "p".repeat(256);
    let child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_mynou"))
            .args(["serve", "--config", path.to_str().unwrap()])
            .env("MYNOU_API_TOKEN", "0123456789abcdef0123456789abcdef")
            .env("MYNOU_NICKSERV_TEST_PASSWORD", &password)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let mut stream = accept(&listener);
    assert_eq!(line(&mut stream), "NICK mynou");
    assert_eq!(line(&mut stream), "USER mynou 0 * :Mynou");
    send(&mut stream, ":server 001 mynou :Welcome");
    let command = line(&mut stream);
    assert!(command == format!("PRIVMSG NickServ :IDENTIFY fixture {password}"));
    confirm(&mut stream);
    send(
        &mut stream,
        &format!(":{SERVICE} NOTICE mynou :Identified as fixture"),
    );
    announce(&mut stream, SENDER, "#announces", &announcement(7));
    wait(|| {
        snapshot(&cfg.store_dir)
            .get("records")
            .unwrap()
            .as_array()
            .unwrap()
            .len()
            == 1
    });
    drop(child);
    let e = Engine::open_for_management(cfg.clone()).unwrap();
    assert_eq!(rows(&e).len(), 1);
    no_jobs(&e);
    private(&e.irc_sources().unwrap());
    let mut contents = bytes(&cfg.store_dir).into_values().collect::<Vec<_>>();
    contents.push(fs::read(log_path).unwrap());
    for bytes in contents {
        assert!(
            !bytes
                .windows(password.len())
                .any(|b| b == password.as_bytes())
        );
    }
}

#[test]
fn service_identification_deadline_cannot_be_renewed_by_ping_traffic() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&configured(&dir, &url), &dir.0).unwrap();
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        identify(&mut stream);
        let started = Instant::now();
        loop {
            if stream.write_all(b"PING :identifying\r\n").is_err() {
                break;
            }
            let mut reply = Vec::new();
            let mut byte = [0];
            while !reply.ends_with(b"\r\n") && reply.len() < 512 {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => reply.push(byte[0]),
                }
            }
            if reply.is_empty() {
                break;
            }
            assert_eq!(reply, b"PONG :identifying\r\n");
            assert!(started.elapsed() < Duration::from_secs(12));
            thread::sleep(Duration::from_millis(100));
        }
        assert!(started.elapsed() >= Duration::from_secs(9));
        assert!(started.elapsed() < Duration::from_secs(12));
    });
    let e = Engine::open(cfg).unwrap();
    let workers = e.start();
    let deadline = Instant::now() + Duration::from_secs(14);
    while health(&e).get("phase").unwrap().as_str() != Some("backoff") {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(rows(&e).is_empty());
    no_jobs(&e);
    drop(workers);
    peer.join().unwrap();
}
