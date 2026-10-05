//! Original local SASL journeys; credentials never authorize unauthenticated work.
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

fn source(v: &mut Value) -> &mut Value {
    let Value::Array(s) = v.get_mut("irc").unwrap().get_mut("sources").unwrap() else {
        panic!()
    };
    &mut s[0]
}
fn sasl() -> Value {
    let mut v = Value::object();
    v.insert("mechanism", "PLAIN");
    v.insert("username_env", "PWD");
    v.insert("password_env", "PWD");
    v
}
fn configured(dir: &Directory, url: &str) -> Value {
    let mut v = value(&dir.0, url);
    source(&mut v).insert("sasl", sasl());
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
fn challenge(p: &mut Protocol) {
    assert_eq!(
        event(p, ":server CAP * LS :sasl=PLAIN"),
        Event::Reply("CAP REQ :sasl".into())
    );
    assert_eq!(
        event(p, ":server CAP mynou ACK :sasl"),
        Event::Reply("AUTHENTICATE PLAIN".into())
    );
}
fn health(engine: &Engine) -> Value {
    engine
        .irc_sources()
        .unwrap()
        .get("sources")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .get("health")
        .unwrap()
        .clone()
}
fn no_secrets(v: &Value) {
    no_credentials(v);
    let text = json::stringify(v);
    for private in [
        "username_env",
        "authorization_env",
        "AUTHENTICATE ",
        "CAP REQ",
    ] {
        assert!(!text.contains(private));
    }
}
fn start(stream: &mut TcpStream) {
    assert_eq!(line(stream), "CAP LS 302");
    assert_eq!(line(stream), "NICK mynou");
    assert_eq!(line(stream), "USER mynou 0 * :Mynou");
}
fn request_plain(stream: &mut TcpStream) {
    send(stream, ":server CAP * LS * :multi-prefix echo-message");
    send(stream, "PING :during-capabilities");
    assert_eq!(line(stream), "PONG :during-capabilities");
    let wire = b":server CAP mynou LS :sasl=EXTERNAL,PLAIN\r\n";
    for chunk in wire.chunks(3) {
        stream.write_all(chunk).unwrap();
    }
    assert_eq!(line(stream), "CAP REQ :sasl");
    send(stream, ":server CAP mynou ACK :sasl");
    assert_eq!(line(stream), "AUTHENTICATE PLAIN");
    send(stream, "AUTHENTICATE +");
}
fn response(stream: &mut TcpStream) -> (Vec<u8>, Vec<usize>, String) {
    let mut encoded = String::new();
    let mut lengths = Vec::new();
    loop {
        let command = line(stream);
        let chunk = command.strip_prefix("AUTHENTICATE ").unwrap();
        if chunk == "+" {
            break;
        }
        assert!(chunk.len() <= 400 && lengths.len() < 3);
        lengths.push(chunk.len());
        encoded.push_str(chunk);
        if chunk.len() < 400 {
            break;
        }
    }
    assert_eq!(encoded.len() % 4, 0);
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0_u32;
    let mut count = 0;
    let mut decoded = Vec::new();
    for b in encoded.bytes().take_while(|b| *b != b'=') {
        let index = alphabet.iter().position(|v| *v == b).unwrap() as u32;
        bits = (bits << 6 | index) & 0xffff;
        count += 6;
        if count >= 8 {
            count -= 8;
            decoded.push((bits >> count) as u8);
        }
    }
    (decoded, lengths, encoded)
}
fn welcome(stream: &mut TcpStream) {
    send(
        stream,
        ":server 900 mynou mynou!user@fixture account :Logged in",
    );
    send(stream, ":server 903 mynou :SASL authentication successful");
    assert_eq!(line(stream), "CAP END");
    send(stream, ":server 001 mynou :Welcome");
    assert_eq!(line(stream), "JOIN #announces");
    send(stream, ":mynou!user@fixture JOIN :#announces");
}

#[test]
fn settings_are_strict_and_authentication_policy_changes_require_a_new_source_identity() {
    let dir = Directory::new();
    let mut old = value(&dir.0, "irc://127.0.0.1:1");
    let old_cfg = config::from_json(&old, &dir.0).unwrap();
    let binding = old_cfg.irc.sources[0].binding();
    source(&mut old).insert("sasl", Value::Null);
    assert_eq!(
        config::from_json(&old, &dir.0).unwrap().irc.sources[0].binding(),
        binding
    );
    for auth in [
        "{}",
        "{\"mechanism\":\"SCRAM-SHA-256\",\"username_env\":\"PWD\",\"password_env\":\"PWD\"}",
        "{\"mechanism\":\"PLAIN\",\"username_env\":\"lowercase\",\"password_env\":\"PWD\"}",
        "{\"mechanism\":\"PLAIN\",\"username_env\":\"PWD\",\"password_env\":null}",
        "{\"mechanism\":\"PLAIN\",\"username_env\":\"PWD\",\"password_env\":\"PWD\",\"password\":\"secret\"}",
        "{\"mechanism\":\"PLAIN\",\"username_env\":\"PWD\",\"password_env\":\"PWD\",\"authorization_env\":\"bad name\"}",
    ] {
        let mut v = old.clone();
        source(&mut v).insert("sasl", json::parse(auth).unwrap());
        assert!(config::from_json(&v, &dir.0).is_err());
    }
    let engine = Engine::open(old_cfg.clone()).unwrap();
    receive(&engine, 7);
    drop(engine);
    let before = bytes(&old_cfg.store_dir);
    let mut v = configured(&dir, "irc://127.0.0.1:1");
    let changed = config::from_json(&v, &dir.0).unwrap();
    assert_ne!(changed.irc.sources[0].binding(), binding);
    assert!(Engine::open(changed).is_err());
    assert_eq!(bytes(&old_cfg.store_dir), before);
    source(&mut v).insert("id", "authenticated");
    let Value::Array(r) = v.get_mut("irc").unwrap().get_mut("rules").unwrap() else {
        panic!()
    };
    r[0].insert("source", "authenticated");
    let engine = Engine::open(config::from_json(&v, &dir.0).unwrap()).unwrap();
    assert_eq!(rows(&engine).len(), 1);
    no_secrets(&engine.irc_sources().unwrap());
    no_jobs(&engine);
    let mut remote = configured(&dir, "irc://example.test:6667");
    assert!(config::from_json(&remote, &dir.0).is_err());
    source(&mut remote).insert("url", "ircs://example.test:6697");
    assert!(config::from_json(&remote, &dir.0).is_ok());
}

#[test]
fn fragmented_capabilities_success_and_membership_all_precede_announcements() {
    for offer in ["sasl", "sasl=EXTERNAL,PLAIN"] {
        let mut p = protocol();
        let trusted = format!(":{SENDER} PRIVMSG #announces :MYNOU {{}}");
        assert_eq!(event(&mut p, &trusted), Event::Ignore);
        assert_eq!(event(&mut p, ":user!u@h CAP mynou LS :sasl"), Event::Ignore);
        assert_eq!(event(&mut p, ":server CAP other LS :sasl"), Event::Ignore);
        assert_eq!(
            event(&mut p, ":server CAP * LS * :multi-prefix"),
            Event::Ignore
        );
        let wire = format!(":server CAP mynou LS :{offer}\r\n");
        let mut decoder = Decoder::default();
        for byte in wire.bytes() {
            for m in decoder.feed(&[byte]).unwrap() {
                assert_eq!(p.receive(&m).unwrap(), Event::Reply("CAP REQ :sasl".into()));
            }
        }
        assert_eq!(
            event(&mut p, ":server CAP mynou ACK :sasl"),
            Event::Reply("AUTHENTICATE PLAIN".into())
        );
        assert_eq!(
            event(&mut p, "PING :nonce"),
            Event::Reply("PONG :nonce".into())
        );
        assert_eq!(event(&mut p, "AUTHENTICATE +"), Event::Authenticate);
        assert_eq!(
            event(&mut p, ":user!u@h 903 mynou :Forged success"),
            Event::Ignore
        );
        assert_eq!(
            event(&mut p, ":server 903 other :Wrong recipient"),
            Event::Ignore
        );
        assert_eq!(
            event(&mut p, ":server 903 * :Wrong recipient"),
            Event::Ignore
        );
        assert_eq!(
            event(&mut p, ":server 900 mynou n!u@h account :Logged in"),
            Event::Ignore
        );
        assert_eq!(
            event(&mut p, ":server 903 mynou :Successful"),
            Event::Reply("CAP END".into())
        );
        assert_eq!(event(&mut p, &trusted), Event::Ignore);
        assert_eq!(event(&mut p, ":server 001 mynou :Welcome"), Event::Join);
        assert_eq!(event(&mut p, &trusted), Event::Ignore);
        assert_eq!(
            event(&mut p, ":server 366 mynou #announces :End of names"),
            Event::Joined
        );
        assert!(matches!(event(&mut p, &trusted), Event::Announcement(_)));
        assert!(
            p.receive(&Message::parse(":server CAP mynou DEL :sasl").unwrap())
                .is_err()
        );
    }
}

#[test]
fn authentication_rejection_unexpected_success_and_repeated_challenges_never_fall_back() {
    for wire in [
        ":server 001 mynou :Premature welcome",
        ":server 903 mynou :Premature success",
        ":server 901 mynou :Logged out",
        ":server 902 mynou :Nickname locked",
        ":server 904 mynou :private-server-fixture",
        ":server 905 mynou :Too long",
        ":server 906 mynou :Aborted",
        ":server 907 mynou :Already authenticated",
        ":server 421 mynou CAP :Unsupported",
        ":server CAP mynou NAK :sasl",
        "AUTHENTICATE +",
    ] {
        let error = protocol()
            .receive(&Message::parse(wire).unwrap())
            .unwrap_err();
        assert!(!error.contains("private-server-fixture"));
    }
    for wire in [
        ":server CAP mynou ACK :sasl extra",
        ":server CAP mynou ACK :-sasl",
        ":server CAP mynou NAK :sasl",
    ] {
        let mut p = protocol();
        event(&mut p, ":server CAP * LS :sasl");
        assert!(p.receive(&Message::parse(wire).unwrap()).is_err());
    }
    for wire in [
        "AUTHENTICATE private-challenge",
        "AUTHENTICATE *",
        ":server 903 mynou :Not sent",
    ] {
        let mut p = protocol();
        challenge(&mut p);
        let error = p.receive(&Message::parse(wire).unwrap()).unwrap_err();
        assert!(!error.contains("private-challenge"));
    }
    let mut p = protocol();
    challenge(&mut p);
    event(&mut p, "AUTHENTICATE +");
    assert!(
        p.receive(&Message::parse("AUTHENTICATE +").unwrap())
            .is_err()
    );
    let mut p = protocol();
    challenge(&mut p);
    event(&mut p, "AUTHENTICATE +");
    event(&mut p, ":server 903 mynou :Successful");
    assert!(
        p.receive(&Message::parse(":server 901 mynou :Logged out").unwrap())
            .is_err()
    );
    assert_eq!(
        p.receive(&Message::parse(":server 001 mynou :Welcome").unwrap())
            .unwrap_err(),
        "IRC: SASL requires a new connection"
    );
}

#[test]
fn capability_lists_are_bounded_and_ambiguous_or_unsupported_offers_are_rejected() {
    for list in [
        "multi-prefix",
        "sasl=EXTERNAL",
        "sasl=",
        "sasl=PLAIN,PLAIN",
        "sasl=plain",
        "sasl sasl",
        "sasl=PLAIN sasl=EXTERNAL",
    ] {
        assert!(
            protocol()
                .receive(&Message::parse(&format!(":server CAP * LS :{list}")).unwrap())
                .is_err()
        );
    }
    let mut p = protocol();
    for _ in 0..16 {
        assert_eq!(event(&mut p, ":server CAP * LS * :"), Event::Ignore);
    }
    assert!(
        p.receive(&Message::parse(":server CAP * LS :sasl").unwrap())
            .is_err()
    );
    let list = (0..65)
        .map(|n| format!("cap-{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        protocol()
            .receive(&Message::parse(&format!(":server CAP * LS :sasl {list}")).unwrap())
            .is_err()
    );
    let mut p = protocol();
    for n in 0..8 {
        let list = format!(
            "cap-{n}-a={} cap-{n}-b={}",
            "v".repeat(246),
            "v".repeat(246)
        );
        assert_eq!(
            event(&mut p, &format!(":server CAP * LS * :{list}")),
            Event::Ignore
        );
    }
    let list = format!("cap-8-a={} cap-8-b={}", "v".repeat(246), "v".repeat(246));
    assert!(
        p.receive(&Message::parse(&format!(":server CAP * LS :{list}")).unwrap())
            .is_err()
    );
}

#[test]
fn live_authentication_reconnect_and_restart_repeat_the_handshake_before_duplicate_receipts() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&configured(&dir, &url), &dir.0).unwrap();
    let peer = thread::spawn(move || {
        for pass in 0..3 {
            let mut stream = accept(&listener);
            start(&mut stream);
            announce(&mut stream, SENDER, "#announces", &announcement(99));
            request_plain(&mut stream);
            let (bytes, _, _) = response(&mut stream);
            let pwd = std::env::var("PWD").unwrap();
            assert!(bytes == format!("\0{pwd}\0{pwd}").into_bytes());
            welcome(&mut stream);
            announce(&mut stream, SENDER, "#announces", &announcement(7));
            if pass != 0 {
                closed(&mut stream);
            }
        }
    });
    let engine = Engine::open(cfg.clone()).unwrap();
    let workers = engine.start();
    wait(|| health(&engine).get("received").unwrap().as_str() == Some("2"));
    assert_eq!(
        health(&engine).get("sasl_authenticated").unwrap().as_bool(),
        Some(true)
    );
    assert_eq!(rows(&engine).len(), 1);
    let before = bytes(&cfg.store_dir);
    no_secrets(&engine.irc_sources().unwrap());
    no_secrets(&rows(&engine)[0]);
    no_jobs(&engine);
    drop(workers);
    assert_eq!(
        health(&engine).get("sasl_authenticated").unwrap().as_bool(),
        Some(false)
    );
    drop(engine);
    let engine = Engine::open(cfg.clone()).unwrap();
    let workers = engine.start();
    wait(|| health(&engine).get("received").unwrap().as_str() == Some("1"));
    assert_eq!(bytes(&cfg.store_dir), before);
    assert_eq!(
        health(&engine).get("duplicates").unwrap().as_str(),
        Some("1")
    );
    no_jobs(&engine);
    drop(workers);
    peer.join().unwrap();
}

#[test]
fn rejected_authentication_redacts_server_text_and_shutdown_interrupts_a_new_challenge() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&configured(&dir, &url), &dir.0).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        start(&mut stream);
        request_plain(&mut stream);
        response(&mut stream);
        send(
            &mut stream,
            &format!(":server 904 mynou :{}", std::env::var("PWD").unwrap()),
        );
        closed(&mut stream);
        let mut stream = accept(&listener);
        start(&mut stream);
        send(&mut stream, ":server CAP * LS :sasl");
        assert_eq!(line(&mut stream), "CAP REQ :sasl");
        send(&mut stream, ":server CAP mynou ACK :sasl");
        assert_eq!(line(&mut stream), "AUTHENTICATE PLAIN");
        stream.write_all(b"AUTHENTICATE ").unwrap();
        ready_tx.send(()).unwrap();
        closed(&mut stream);
    });
    let engine = Engine::open(cfg).unwrap();
    let workers = engine.start();
    wait(|| health(&engine).get("phase").unwrap().as_str() == Some("backoff"));
    assert_eq!(
        health(&engine).get("sasl_authenticated").unwrap().as_bool(),
        Some(false)
    );
    no_secrets(&engine.irc_sources().unwrap());
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(rows(&engine).is_empty());
    no_jobs(&engine);
    let started = Instant::now();
    drop(workers);
    assert!(started.elapsed() < Duration::from_secs(2));
    peer.join().unwrap();
}

#[test]
fn missing_credentials_stop_before_opening_an_irc_socket() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let mut v = configured(&dir, &url);
    let absent = format!("MYNOU_IRC_SASL_UNAVAILABLE_{}", std::process::id());
    assert!(std::env::var_os(&absent).is_none());
    source(&mut v)
        .get_mut("sasl")
        .unwrap()
        .insert("password_env", absent);
    let engine = Engine::open(config::from_json(&v, &dir.0).unwrap()).unwrap();
    let workers = engine.start();
    wait(|| health(&engine).get("phase").unwrap().as_str() == Some("backoff"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(rows(&engine).is_empty());
    no_jobs(&engine);
    no_secrets(&engine.irc_sources().unwrap());
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
fn the_native_cli_sends_an_exact_400_byte_response_and_its_required_terminator() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let mut v = configured(&dir, &url);
    let auth = source(&mut v).get_mut("sasl").unwrap();
    auth.insert("username_env", "MYNOU_IRC_TEST_USERNAME");
    auth.insert("password_env", "MYNOU_IRC_TEST_PASSWORD");
    let path = dir.0.join("mynou.json");
    fs::write(&path, json::stringify(&v)).unwrap();
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let log_path = dir.0.join("process.log");
    let log = fs::File::create(&log_path).unwrap();
    let username = "u".repeat(100);
    let password = "p".repeat(198);
    let process = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_mynou"))
            .args(["serve", "--config", path.to_str().unwrap()])
            .env("MYNOU_API_TOKEN", "0123456789abcdef0123456789abcdef")
            .env("MYNOU_IRC_TEST_USERNAME", &username)
            .env("MYNOU_IRC_TEST_PASSWORD", &password)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let mut stream = accept(&listener);
    start(&mut stream);
    request_plain(&mut stream);
    let (payload, lengths, encoded) = response(&mut stream);
    assert_eq!(lengths, [400]);
    assert!(payload == format!("\0{username}\0{password}").into_bytes());
    welcome(&mut stream);
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
    drop(process);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    assert_eq!(rows(&engine).len(), 1);
    no_jobs(&engine);
    no_secrets(&engine.irc_sources().unwrap());
    let mut content = bytes(&cfg.store_dir).into_values().collect::<Vec<_>>();
    content.push(fs::read(log_path).unwrap());
    for content in content {
        for secret in [&username, &password, &encoded] {
            assert!(
                !content
                    .windows(secret.len())
                    .any(|s| s == secret.as_bytes())
            );
        }
    }
}

#[test]
fn ping_traffic_cannot_extend_the_overall_registration_deadline() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&configured(&dir, &url), &dir.0).unwrap();
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        start(&mut stream);
        let started = Instant::now();
        loop {
            if stream.write_all(b"PING :registration\r\n").is_err() {
                break;
            }
            let mut reply = Vec::new();
            let mut b = [0];
            while !reply.ends_with(b"\r\n") && reply.len() < 512 {
                match stream.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => reply.push(b[0]),
                }
            }
            if reply.is_empty() {
                break;
            }
            assert_eq!(reply, b"PONG :registration\r\n");
            assert!(started.elapsed() < Duration::from_secs(12));
            thread::sleep(Duration::from_millis(100));
        }
        assert!(started.elapsed() >= Duration::from_secs(9));
        assert!(started.elapsed() < Duration::from_secs(12));
    });
    let engine = Engine::open(cfg).unwrap();
    let workers = engine.start();
    let deadline = Instant::now() + Duration::from_secs(14);
    while health(&engine).get("phase").unwrap().as_str() != Some("backoff") {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(rows(&engine).is_empty());
    no_jobs(&engine);
    no_secrets(&engine.irc_sources().unwrap());
    drop(workers);
    peer.join().unwrap();
}
