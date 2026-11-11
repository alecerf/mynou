//! Invite-only announce channels against local synthetic peers: the bot's
//! invitation after identification, refusals retried then reported, and the
//! invite secret kept out of every answer, log and record.
use crate::api_support;
use crate::home_support;
use crate::irc_support;
use crate::library_support;
use irc_support::*;
use library_support::Directory;
use mynou::{
    config,
    engine::Engine,
    irc::protocol::{Event, Message, Protocol},
    json::{self, Value},
};
use std::{
    collections::BTreeMap,
    fs,
    net::TcpStream,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const SERVICE: &str = "NickServ!service@fixture";
const INVITE: &str = ":Bot!bot@fixture INVITE mynou :#announces";
const REFUSED: &str = ":server 473 mynou #announces :Cannot join channel (+i)";
fn secret() -> String {
    std::env::var("MYNOU_TEST_SECRET_A").unwrap()
}
fn source(v: &mut Value) -> &mut Value {
    let Value::Array(s) = v.get_mut("irc").unwrap().get_mut("sources").unwrap() else {
        panic!()
    };
    &mut s[0]
}
fn invite(timeout_secs: u32) -> Value {
    let mut v = Value::object();
    v.insert("nick", "Bot");
    v.insert("message", "invite {secret}");
    v.insert("secret_env", "MYNOU_TEST_SECRET_A");
    v.insert("timeout_secs", timeout_secs);
    v
}
fn nickserv() -> Value {
    json::parse(r#"{"account":"fixture","password_env":"MYNOU_TEST_SECRET_B","service":"NickServ","sender":"NickServ!service@fixture","success_notice":"Identified as {account}","failure_notices":["Invalid password"]}"#).unwrap()
}
fn configured(url: &str, timeout_secs: u32) -> Value {
    let mut v = value(url);
    source(&mut v).insert("invite", invite(timeout_secs));
    v
}
fn protocol(v: &Value) -> Protocol {
    let dir = Directory::new();
    Protocol::new(config::from_json(v, &dir.0).unwrap().irc.sources[0].clone())
}
fn event(p: &mut Protocol, line: &str) -> Event {
    p.receive(&Message::parse(line).unwrap()).unwrap()
}
fn error(p: &mut Protocol, line: &str) -> String {
    p.receive(&Message::parse(line).unwrap()).unwrap_err()
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
fn field(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => json::stringify(other),
    }
}
/// Fails without printing the secret.
fn secretless(what: &str, text: &str, secret: &str) {
    assert!(!text.contains(secret), "{what} exposed the invite secret");
    assert!(
        !text.contains("invite mynou-"),
        "{what} exposed the command"
    );
}
/// The invite command, compared without printing the secret on failure.
fn invited(stream: &mut TcpStream, secret: &str) {
    assert!(line(stream) == format!("PRIVMSG Bot :invite {secret}"));
}
fn registered(stream: &mut TcpStream) {
    assert_eq!(line(stream), "NICK mynou");
    assert_eq!(line(stream), "USER mynou 0 * :Mynou");
    send(stream, ":server 001 mynou :Welcome");
}

#[test]
fn the_invite_command_follows_identification_and_the_bots_invite_opens_the_join() {
    // NickServ: nothing before the confirmation, then the invite command.
    let mut v = configured("irc://127.0.0.1:1", 10);
    source(&mut v).insert("nickserv", nickserv());
    let mut p = protocol(&v);
    assert_eq!(event(&mut p, INVITE), Event::Ignore);
    assert_eq!(event(&mut p, ":server 001 mynou :Welcome"), Event::Identify);
    assert_eq!(event(&mut p, INVITE), Event::Ignore);
    assert_eq!(p.elapsed(), Event::Ignore);
    assert_eq!(
        event(
            &mut p,
            &format!(":{SERVICE} NOTICE mynou :Identified as fixture")
        ),
        Event::Invite
    );
    for other in [
        ":Bot!bot@fixture INVITE other :#announces",
        ":Bot!bot@fixture INVITE mynou :#other",
        ":Bot!bot@fixture INVITE mynou #announces extra",
    ] {
        assert_eq!(event(&mut p, other), Event::Ignore);
    }
    assert_eq!(event(&mut p, INVITE), Event::Join);
    assert_eq!(event(&mut p, INVITE), Event::Ignore);
    assert_eq!(p.elapsed(), Event::Ignore);
    assert_eq!(
        event(&mut p, ":server 366 mynou #announces :End of names"),
        Event::Joined
    );
    assert_eq!(event(&mut p, INVITE), Event::Ignore);
    assert_eq!(p.elapsed(), Event::Ignore);

    // SASL: the invite command follows the welcome that ends registration.
    let mut v = configured("irc://127.0.0.1:1", 10);
    source(&mut v).insert(
        "sasl",
        json::parse(r#"{"mechanism":"PLAIN","username_env":"MYNOU_TEST_SECRET_B","password_env":"MYNOU_TEST_SECRET_B"}"#).unwrap(),
    );
    let mut p = protocol(&v);
    assert_eq!(
        event(&mut p, ":server CAP * LS :sasl=PLAIN"),
        Event::Reply("CAP REQ :sasl".into())
    );
    assert_eq!(
        event(&mut p, ":server CAP mynou ACK :sasl"),
        Event::Reply("AUTHENTICATE PLAIN".into())
    );
    assert_eq!(event(&mut p, "AUTHENTICATE +"), Event::Authenticate);
    assert_eq!(
        event(&mut p, ":server 903 mynou :SASL authentication successful"),
        Event::Reply("CAP END".into())
    );
    assert_eq!(event(&mut p, ":server 001 mynou :Welcome"), Event::Invite);
    // No invitation in time: the join is tried anyway.
    assert_eq!(p.elapsed(), Event::Join);
    assert_eq!(p.elapsed(), Event::Ignore);

    // Without an invite command nothing changes, and a refusal says why.
    let mut p = protocol(&value("irc://127.0.0.1:1"));
    assert_eq!(event(&mut p, ":server 001 mynou :Welcome"), Event::Join);
    assert_eq!(event(&mut p, INVITE), Event::Ignore);
    assert_eq!(error(&mut p, REFUSED), "IRC: the channel is invite-only");
}

#[test]
fn refusals_ask_again_with_a_growing_delay_then_fail() {
    let mut v = configured("irc://127.0.0.1:1", 10);
    source(&mut v).insert("reconnect_max_secs", 60_u32);
    let mut p = protocol(&v);
    assert_eq!(event(&mut p, ":server 001 mynou :Welcome"), Event::Invite);
    // A refusal before any join is not ours to retry.
    assert_eq!(
        protocol(&v)
            .receive(&Message::parse(REFUSED).unwrap())
            .unwrap_err(),
        "IRC: registration or channel rejected"
    );
    assert_eq!(p.elapsed(), Event::Join);
    // Only the server's refusal for this channel counts.
    for forged in [
        ":user!u@h 473 mynou #announces :Forged",
        ":server 473 mynou #other :Other channel",
    ] {
        let mut copy = protocol(&v);
        event(&mut copy, ":server 001 mynou :Welcome");
        copy.elapsed();
        assert_eq!(
            error(&mut copy, forged),
            "IRC: registration or channel rejected"
        );
    }
    for delay in [2, 4, 8] {
        assert_eq!(event(&mut p, REFUSED), Event::Retry(delay));
        if delay == 4 {
            // A late invitation during the delay joins at once.
            assert_eq!(event(&mut p, INVITE), Event::Join);
            continue;
        }
        assert_eq!(p.elapsed(), Event::Invite);
        assert_eq!(p.elapsed(), Event::Join);
    }
    assert_eq!(
        error(&mut p, REFUSED),
        "IRC: the invite-only channel refused every join"
    );
    // The delay never exceeds the source's reconnect bound.
    let mut p = protocol(&configured("irc://127.0.0.1:1", 10));
    event(&mut p, ":server 001 mynou :Welcome");
    p.elapsed();
    assert_eq!(event(&mut p, REFUSED), Event::Retry(1));
}

#[test]
fn an_invite_only_channel_is_joined_after_identification_and_after_a_reconnect() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let mut v = configured(&url, 10);
    source(&mut v).insert("nickserv", nickserv());
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let peer = thread::spawn(move || {
        let secret = secret();
        for pass in 0..2 {
            let mut stream = accept(&listener);
            registered(&mut stream);
            assert!(
                line(&mut stream)
                    == format!(
                        "PRIVMSG NickServ :IDENTIFY fixture {}",
                        std::env::var("MYNOU_TEST_SECRET_B").unwrap()
                    )
            );
            // An invitation before identification does not open the join.
            send(&mut stream, INVITE);
            send(
                &mut stream,
                &format!(":{SERVICE} NOTICE mynou :Identified as fixture"),
            );
            invited(&mut stream, &secret);
            send(&mut stream, "PING :inviting");
            assert_eq!(line(&mut stream), "PONG :inviting");
            send(&mut stream, INVITE);
            assert_eq!(line(&mut stream), "JOIN #announces");
            send(&mut stream, ":mynou!user@fixture JOIN :#announces");
            announce(&mut stream, SENDER, "#announces", &announcement(7 + pass));
            if pass == 1 {
                closed(&mut stream);
            }
        }
    });
    let e = Engine::open(cfg).unwrap();
    let workers = e.start();
    wait_for(Duration::from_secs(10), || {
        field(&health(&e), "received") == "2"
    });
    let h = health(&e);
    assert_eq!(field(&h, "phase"), "connected");
    assert_eq!(
        h.get("nickserv_authenticated").unwrap().as_bool(),
        Some(true)
    );
    assert_eq!(field(&h, "reason"), "");
    let sources = e.irc_sources().unwrap();
    assert_eq!(
        field(
            &sources.get("sources").unwrap().as_array().unwrap()[0],
            "invite_nick"
        ),
        "Bot"
    );
    no_credentials(&sources);
    secretless("IRC sources", &json::stringify(&sources), &secret());
    assert_eq!(rows(&e).len(), 2);
    drop(workers);
    peer.join().unwrap();
}

#[test]
fn without_an_invitation_in_time_the_join_is_tried_anyway() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&configured(&url, 1), &dir.0).unwrap();
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        registered(&mut stream);
        invited(&mut stream, &secret());
        let asked = Instant::now();
        assert_eq!(line(&mut stream), "JOIN #announces");
        assert!(asked.elapsed() >= Duration::from_millis(900));
        send(&mut stream, ":mynou!user@fixture JOIN :#announces");
        announce(&mut stream, SENDER, "#announces", &announcement(7));
        closed(&mut stream);
    });
    let e = Engine::open(cfg).unwrap();
    let workers = e.start();
    wait(|| field(&health(&e), "phase") == "inviting");
    wait(|| field(&health(&e), "received") == "1");
    assert_eq!(field(&health(&e), "phase"), "connected");
    drop(workers);
    peer.join().unwrap();
}

#[test]
fn a_refusal_asks_again_and_a_late_invitation_joins() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&configured(&url, 1), &dir.0).unwrap();
    let peer = thread::spawn(move || {
        let secret = secret();
        let mut stream = accept(&listener);
        registered(&mut stream);
        invited(&mut stream, &secret);
        assert_eq!(line(&mut stream), "JOIN #announces");
        send(&mut stream, REFUSED);
        let refused = Instant::now();
        invited(&mut stream, &secret);
        assert!(refused.elapsed() >= Duration::from_millis(900));
        send(&mut stream, INVITE);
        assert_eq!(line(&mut stream), "JOIN #announces");
        send(&mut stream, ":mynou!user@fixture JOIN :#announces");
        announce(&mut stream, SENDER, "#announces", &announcement(7));
        closed(&mut stream);
    });
    let e = Engine::open(cfg).unwrap();
    let workers = e.start();
    wait_for(Duration::from_secs(10), || {
        field(&health(&e), "received") == "1"
    });
    assert_eq!(field(&health(&e), "phase"), "connected");
    assert_eq!(field(&health(&e), "attempts"), "1");
    drop(workers);
    peer.join().unwrap();
}

#[test]
fn refusals_are_retried_then_reported_with_their_reason() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&configured(&url, 1), &dir.0).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let peer = thread::spawn(move || {
        let secret = secret();
        let mut stream = accept(&listener);
        registered(&mut stream);
        let mut invites = 0;
        let mut refused: Option<Instant> = None;
        for _ in 0..4 {
            invited(&mut stream, &secret);
            if let Some(at) = refused {
                assert!(at.elapsed() >= Duration::from_millis(900));
            }
            invites += 1;
            assert_eq!(line(&mut stream), "JOIN #announces");
            send(&mut stream, REFUSED);
            refused = Some(Instant::now());
        }
        closed(&mut stream);
        tx.send(invites).unwrap();
    });
    let e = Engine::open(cfg).unwrap();
    let workers = e.start();
    wait_for(Duration::from_secs(20), || {
        field(&health(&e), "last_error") == "invite_not_received"
    });
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 4);
    let h = health(&e);
    assert_eq!(field(&h, "phase"), "backoff");
    assert_eq!(
        field(&h, "reason"),
        "#announces is invite-only and Bot did not invite mynou after 4 invite commands"
    );
    // The next attempt waits for the source's longest reconnect delay.
    assert_eq!(field(&h, "retry_in_secs"), "1");
    let sources = e.irc_sources().unwrap();
    no_credentials(&sources);
    secretless("IRC sources", &json::stringify(&sources), &secret());
    assert!(rows(&e).is_empty());
    no_jobs(&e);
    drop(workers);
    peer.join().unwrap();
}

#[test]
fn without_an_invite_command_a_refusal_says_one_is_needed() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&value(&url), &dir.0).unwrap();
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        registered(&mut stream);
        assert_eq!(line(&mut stream), "JOIN #announces");
        send(&mut stream, REFUSED);
        closed(&mut stream);
    });
    let e = Engine::open(cfg).unwrap();
    let workers = e.start();
    wait(|| field(&health(&e), "last_error") == "invite_command_required");
    assert_eq!(
        field(&health(&e), "reason"),
        "#announces is invite-only: this source needs an invite command"
    );
    drop(workers);
    peer.join().unwrap();
}

#[test]
fn adding_an_invite_command_keeps_the_source_history_and_its_decisions() {
    let dir = Directory::new();
    let plain = config::from_json(&value("irc://127.0.0.1:1"), &dir.0).unwrap();
    let e = Engine::open(plain.clone()).unwrap();
    receive(&e, 7);
    drop(e);
    let with = config::from_json(&configured("irc://127.0.0.1:1", 10), &dir.0).unwrap();
    assert_eq!(
        with.irc.sources[0].binding(),
        plain.irc.sources[0].binding()
    );
    let e = Engine::open(with).unwrap();
    assert_eq!(rows(&e).len(), 1);
    // The configuration is validated with the source.
    let mut bad = configured("irc://127.0.0.1:1", 10);
    source(&mut bad)
        .get_mut("invite")
        .unwrap()
        .insert("message", "invite");
    assert!(config::from_json(&bad, &dir.0).is_err());
    let mut bad = configured("irc://127.0.0.1:1", 10);
    source(&mut bad)
        .get_mut("invite")
        .unwrap()
        .insert("secret_env", "OTHER_VARIABLE");
    assert!(config::from_json(&bad, &dir.0).is_err());
}

#[test]
fn the_api_never_answers_with_the_invite_secret() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let cfg = config::from_json(&configured(&url, 10), &dir.0).unwrap();
    let peer = thread::spawn(move || {
        let mut stream = accept(&listener);
        registered(&mut stream);
        invited(&mut stream, &secret());
        send(&mut stream, INVITE);
        assert_eq!(line(&mut stream), "JOIN #announces");
        send(&mut stream, ":mynou!user@fixture JOIN :#announces");
        closed(&mut stream);
    });
    let server = api_support::Server::open(cfg);
    let workers = server.engine.start();
    wait(|| field(&health(&server.engine), "phase") == "connected");
    let secret = secret();
    for route in [
        "/api/settings",
        "/api/irc",
        "/api/secrets",
        "/api/stats",
        "/api/status",
    ] {
        let reply = server.call("GET", route, &[], "");
        assert_eq!(reply.status, 200, "{route}");
        secretless(route, &reply.body, &secret);
        for value in reply.headers.values() {
            secretless(route, value, &secret);
        }
    }
    let secrets = server.call("GET", "/api/secrets", &[], "").body;
    assert!(secrets.contains("MYNOU_TEST_SECRET_A"));
    drop(workers);
    drop(server);
    peer.join().unwrap();
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn a_stored_invite_secret_is_sent_once_and_never_logged_or_recorded() {
    let dir = Directory::new();
    let (listener, url) = listener();
    let mut v = configured(&url, 10);
    source(&mut v)
        .get_mut("invite")
        .unwrap()
        .insert("secret_env", "MYNOU_IRC_INVITE_FIXTURE_KEY");
    home_support::save_settings(&dir.0, &v);
    let key = format!("k{}", "e".repeat(255));
    mynou::store::Store::open(&config::store_dir(&dir.0))
        .unwrap()
        .save_secrets(&BTreeMap::from([(
            "MYNOU_IRC_INVITE_FIXTURE_KEY".to_owned(),
            key.clone(),
        )]))
        .unwrap();
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let log_path = dir.0.join("process.log");
    let log = fs::File::create(&log_path).unwrap();
    let child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_mynou"))
            .args(["serve", "--dir", dir.0.to_str().unwrap()])
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let mut stream = accept(&listener);
    registered(&mut stream);
    invited(&mut stream, &key);
    send(&mut stream, INVITE);
    assert_eq!(line(&mut stream), "JOIN #announces");
    send(&mut stream, ":mynou!user@fixture JOIN :#announces");
    announce(&mut stream, SENDER, "#announces", &announcement(7));
    // The ordered PONG follows the durable receipt.
    send(&mut stream, "PING :durable-receipt");
    assert_eq!(line(&mut stream), "PONG :durable-receipt");
    drop(child);
    let e = Engine::open_for_management(cfg.clone()).unwrap();
    assert_eq!(rows(&e).len(), 1);
    secretless(
        "IRC sources",
        &json::stringify(&e.irc_sources().unwrap()),
        &key,
    );
    secretless("settings", &json::stringify(&e.settings().unwrap()), &key);
    drop(e);
    // Only the secrets record of the job store holds it, as for every secret.
    let mut checked = 0;
    for (path, bytes) in bytes(&dir.0) {
        if path.components().any(|c| c.as_os_str() == "jobs.idx") {
            continue;
        }
        checked += 1;
        assert!(
            !bytes.windows(key.len()).any(|b| b == key.as_bytes()),
            "{} holds the invite secret",
            path.display()
        );
    }
    assert!(checked >= 2);
}
