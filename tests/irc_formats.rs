//! Original fixed-format text fixtures, protected previews and native import.
mod irc_routing_support;
mod irc_support;
mod library_support;
#[allow(dead_code)]
mod transfer_support;
mod web_support;
use library_support::Directory;
use mynou::{
    config,
    engine::Engine,
    irc::{
        self,
        protocol::{Decoder, Message},
    },
    json::{self, Value},
};
use std::{fs, io::Write, process::Command, thread};
use web_support::{Server, TOKEN};

fn format(episodes: bool) -> Value {
    let mut v = Value::object();
    v.insert("type", "delimited");
    v.insert("prefix", "NEW | ");
    v.insert("separator", " | ");
    v.insert("suffix", " END");
    let mut names = vec![
        "title",
        "kind",
        "media_title",
        "year",
        "tmdb_id",
        "info_hash",
    ];
    if episodes {
        names.extend(["season", "episode"]);
    }
    v.insert(
        "fields",
        Value::Array(names.into_iter().map(Value::from).collect()),
    );
    v
}
fn source(v: &mut Value) -> &mut Value {
    let Value::Array(s) = v.get_mut("irc").unwrap().get_mut("sources").unwrap() else {
        panic!()
    };
    &mut s[0]
}
fn settings(root: &Directory, url: &str) -> Value {
    let mut v = irc_support::value(&root.0, url);
    source(&mut v).insert("announcement_format", format(false));
    v
}
fn text(id: u64, hash: &str) -> String {
    format!(
        "NEW | Fixture.Movie.2024.1080p.BluRay.x264 | movie | Fixture Movie | 2024 | {id} | {hash} END"
    )
}
fn claim() -> String {
    text(7, "1234567890abcdef1234567890abcdef12345678")
}

#[test]
fn text_claims_map_exact_fields_and_reject_missing_ambiguous_or_secret_identity() {
    let dir = Directory::new();
    let cfg = config::from_json(&settings(&dir, "irc://127.0.0.1:1"), &dir.0).unwrap();
    let preview = irc::preview_text(&cfg, "local", &claim()).unwrap();
    assert_eq!(
        preview.get("announcement").unwrap(),
        &irc::Announcement::from_json(&irc_support::announcement(7))
            .unwrap()
            .to_json()
    );
    assert_eq!(preview.get("persisted").unwrap().as_bool(), Some(false));
    for bad in [
        claim().replace(" | 7 | ", " | 07 | "),
        claim().replace(" | 7 | ", " | 0 | "),
        claim().replace("2024 | 7", "02024 | 7"),
        claim().replace("movie | ", "unknown | "),
        claim().replace("Fixture Movie | ", "Fixture Movie | extra | "),
        claim().replace(" | Fixture Movie | ", " |  | "),
        claim().replace(" END", ""),
        claim().replace(" END", " END trailing"),
        claim().replace(
            "Fixture Movie",
            "Fixture Movie https://source.invalid/?token=private",
        ),
        claim().replace("Fixture Movie", "Fixture Movie passkey=private"),
        claim().replace("Fixture Movie", "Fixture Movie\0"),
        "x".repeat(8191),
    ] {
        let error = irc::preview_text(&cfg, "local", &bad).unwrap_err();
        assert!(!error.contains("private") && !error.contains("source.invalid"));
    }
    assert!(!dir.0.join("jobs").exists());
    let mut v = settings(&dir, "irc://127.0.0.1:1");
    source(&mut v).insert("announcement_format", format(true));
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let episode = claim()
        .replace("movie", "episode")
        .replace(" END", " | 0 | 3 END");
    let report = irc::preview_text(&cfg, "local", &episode).unwrap();
    assert_eq!(
        report
            .get("announcement")
            .unwrap()
            .get("episode")
            .unwrap()
            .as_u64(),
        Some(3)
    );
    assert!(irc::preview_text(&cfg, "local", &episode.replace(" | 0 | 3", " | 0 | 3-4")).is_err());
}

#[test]
fn colour_stripping_is_bounded_and_cannot_format_protocol_headers_or_ping_tokens() {
    let dir = Directory::new();
    let cfg = config::from_json(&settings(&dir, "irc://127.0.0.1:1"), &dir.0).unwrap();
    let expected = irc::preview_text(&cfg, "local", &claim()).unwrap();
    for style in [
        "\u{2}",
        "\u{3}04,12",
        "\u{4}FF00aa,0011FF",
        "\u{11}",
        "\u{16}",
        "\u{1d}",
        "\u{1e}",
        "\u{1f}",
    ] {
        let formatted = format!("{style}{}\u{f}", claim());
        assert_eq!(
            irc::preview_text(&cfg, "local", &formatted).unwrap(),
            expected
        );
        let wire = format!(
            ":{} PRIVMSG #announces :{formatted}\r\n",
            irc_support::SENDER
        );
        for width in [1, 3, 17, 4096] {
            let mut decoder = Decoder::default();
            let mut decoded = Vec::new();
            for chunk in wire.as_bytes().chunks(width) {
                decoded.extend(decoder.feed(chunk).unwrap());
            }
            assert_eq!(decoded.len(), 1);
        }
    }
    for bad in [
        "PING :\u{2}token",
        ":\u{2}sender!u@h PRIVMSG #announces :x",
        ":sender!u@h PRIVMSG \u{2}#announces :x",
        "@x=\u{3}04 PING :x",
        ":sender!u@h PRIVMSG #announces :\t",
    ] {
        assert!(Message::parse(bad).is_err());
    }
}

#[test]
fn legacy_json_bindings_and_pending_fingerprints_survive_an_explicit_default_format() {
    let dir = Directory::new();
    let mut v = irc_support::value(&dir.0, "irc://127.0.0.1:1");
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let binding = cfg.irc.sources[0].binding();
    let engine = Engine::open(cfg.clone()).unwrap();
    irc_support::receive(&engine, 7);
    let before = irc_support::bytes(&cfg.store_dir);
    drop(engine);
    for declared in [Value::Null, json::parse("{\"type\":\"json\"}").unwrap()] {
        source(&mut v).insert("announcement_format", declared);
        let cfg = config::from_json(&v, &dir.0).unwrap();
        assert_eq!(cfg.irc.sources[0].binding(), binding);
        let engine = Engine::open(cfg.clone()).unwrap();
        assert!(
            engine
                .irc_control(
                    irc_support::record_id(&irc_support::rows(&engine)[0]),
                    &irc_support::query("dismiss")
                )
                .is_ok()
        );
        assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    }
    source(&mut v).insert("announcement_format", format(false));
    assert!(Engine::open(config::from_json(&v, &dir.0).unwrap()).is_err());
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
}

#[test]
fn library_delivery_preserves_first_claim_and_never_creates_unsolicited_jobs() {
    let dir = Directory::new();
    let cfg = config::from_json(&settings(&dir, "irc://127.0.0.1:1"), &dir.0).unwrap();
    let engine = Engine::open(cfg.clone()).unwrap();
    let before = irc_support::bytes(&cfg.store_dir);
    assert!(
        engine
            .irc_receive_text("local", "wrong!u@h", "#announces", &claim())
            .is_err()
    );
    assert!(
        engine
            .irc_receive_text(
                "local",
                irc_support::SENDER,
                "#announces",
                "unrelated message"
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    let row = engine
        .irc_receive_text("local", irc_support::SENDER, "#announces", &claim())
        .unwrap()
        .unwrap();
    let before = irc_support::bytes(&cfg.store_dir);
    let duplicate = engine
        .irc_receive_text(
            "local",
            irc_support::SENDER,
            "#announces",
            &claim().replace("1080p", "720p"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        duplicate.get("claim_changed").unwrap().as_bool(),
        Some(true)
    );
    assert_eq!(irc_support::bytes(&cfg.store_dir), before);
    irc_support::no_jobs(&engine);
    irc_support::no_credentials(&row);
    drop(engine);
    let engine = Engine::open(cfg).unwrap();
    assert_eq!(irc_support::rows(&engine).len(), 1);
    assert!(json::stringify(&irc_support::rows(&engine)[0]).contains("1080p"));
    irc_support::no_jobs(&engine);
}

#[test]
fn live_fragmented_text_requires_membership_and_exact_sender_before_receipts() {
    let dir = Directory::new();
    let (listener, url) = irc_support::listener();
    let cfg = config::from_json(&settings(&dir, &url), &dir.0).unwrap();
    let peer = thread::spawn(move || {
        let mut s = irc_support::accept(&listener);
        assert_eq!(irc_support::line(&mut s), "NICK mynou");
        assert_eq!(irc_support::line(&mut s), "USER mynou 0 * :Mynou");
        irc_support::send(
            &mut s,
            &format!(
                ":{} PRIVMSG #announces :{}",
                irc_support::SENDER,
                text(99, &"1".repeat(40))
            ),
        );
        irc_support::send(&mut s, ":server 001 mynou :Welcome");
        assert_eq!(irc_support::line(&mut s), "JOIN #announces");
        irc_support::send(&mut s, ":mynou!u@h JOIN :#announces");
        irc_support::send(
            &mut s,
            &format!(":wrong!u@h PRIVMSG #announces :{}", claim()),
        );
        let wire = format!(
            ":{} PRIVMSG #announces :\u{3}04,12{}\u{f}\r\n",
            irc_support::SENDER,
            claim()
        );
        for chunk in wire.as_bytes().chunks(11) {
            s.write_all(chunk).unwrap();
        }
        irc_support::send(
            &mut s,
            &format!(":{} NOTICE #announces :{}", irc_support::SENDER, claim()),
        );
        irc_support::closed(&mut s);
    });
    let engine = Engine::open(cfg).unwrap();
    let workers = engine.start();
    irc_support::wait(|| {
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
            == Some("2")
    });
    assert_eq!(irc_support::rows(&engine).len(), 1);
    assert_eq!(
        irc_support::rows(&engine)[0]
            .get("announcement")
            .unwrap()
            .get("tmdb_id")
            .unwrap()
            .as_str(),
        Some("7")
    );
    irc_support::no_jobs(&engine);
    drop(workers);
    peer.join().unwrap();
}

#[test]
fn a_text_candidate_enters_the_existing_verified_native_import_path() {
    let dir = Directory::new();
    let torrent = transfer_support::Torrent::single(
        &dir.0.join("source"),
        "Fixture.Movie.2024.1080p.mp4",
        include_bytes!("../examples/demo.mp4").to_vec(),
    );
    let seed = transfer_support::Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let mut cfg =
        irc_routing_support::configuration(&dir.0.join("engine"), seed.client.listen_port());
    cfg.irc.sources[0].announcement_format =
        config::from_json(&settings(&dir, "irc://127.0.0.1:1"), &dir.0)
            .unwrap()
            .irc
            .sources[0]
            .announcement_format
            .clone();
    let engine = Engine::open(cfg.clone()).unwrap();
    let admitted = engine
        .submit(irc_routing_support::request(7))
        .unwrap()
        .remove(0);
    assert!(!engine.tick().unwrap());
    irc_routing_support::no_candidate_work(&engine, &admitted.id);
    engine
        .irc_receive_text(
            "local",
            irc_support::SENDER,
            "#announces",
            &text(7, &torrent.id),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        engine
            .irc_route_pending()
            .unwrap()
            .get("routed")
            .unwrap()
            .as_bool(),
        Some(true)
    );
    assert!(engine.transfers().unwrap().as_array().unwrap().is_empty());
    let ready = library_support::run_until(&engine, &admitted.id, "ready");
    assert_eq!(ready.imports.len(), 1);
    assert_eq!(
        fs::read(&ready.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
    drop(engine);
    let engine = Engine::open(cfg).unwrap();
    assert_eq!(irc_routing_support::job(&engine, &admitted.id), ready);
}

#[test]
fn format_configuration_is_bounded_complete_and_has_no_url_or_pattern_fields() {
    let dir = Directory::new();
    for (key, value) in [
        ("type", Value::from("regex")),
        ("prefix", Value::from("")),
        ("separator", Value::from("")),
        ("separator", Value::from(" ")),
        ("separator", Value::from("alphabet")),
        ("prefix", Value::from("x".repeat(129))),
        ("suffix", Value::from("x".repeat(129))),
        ("prefix", Value::from("\n")),
        ("url", Value::from("https://source.invalid/?token=private")),
        ("fields", Value::Array(vec![Value::from("title"); 6])),
    ] {
        let mut v = settings(&dir, "irc://127.0.0.1:1");
        source(&mut v)
            .get_mut("announcement_format")
            .unwrap()
            .insert(key, value);
        assert!(config::from_json(&v, &dir.0).is_err());
    }
    for names in [
        vec!["title", "kind", "media_title", "year", "tmdb_id", "url"],
        vec![
            "title",
            "kind",
            "media_title",
            "year",
            "tmdb_id",
            "info_hash",
            "season",
        ],
        vec![
            "title",
            "kind",
            "media_title",
            "year",
            "info_hash",
            "episode",
        ],
    ] {
        let mut v = settings(&dir, "irc://127.0.0.1:1");
        source(&mut v)
            .get_mut("announcement_format")
            .unwrap()
            .insert(
                "fields",
                Value::Array(names.into_iter().map(Value::from).collect()),
            );
        assert!(config::from_json(&v, &dir.0).is_err());
    }
    assert!(!dir.0.join("jobs").exists());
}

#[test]
fn protected_api_and_offline_cli_share_pure_text_previews_without_storage_writes() {
    let dir = Directory::new();
    let v = settings(&dir, "irc://127.0.0.1:1");
    let cfg = config::from_json(&v, &dir.0).unwrap();
    let path = dir.0.join("mynou.json");
    let file = dir.0.join("announce.txt");
    fs::write(&path, json::stringify(&v)).unwrap();
    fs::write(&file, claim()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args(["irc-preview", "local", "--text"])
        .arg(&file)
        .arg("--config")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!cfg.store_dir.exists());
    let report = json::parse(std::str::from_utf8(&out.stdout).unwrap().trim()).unwrap();
    let server = Server::open(cfg);
    let before = irc_support::bytes(&server.engine.config.store_dir);
    let mut body = Value::object();
    body.insert("source_id", "local");
    body.insert("text", claim());
    assert_eq!(
        server
            .call("POST", "/api/irc/preview", &[], &json::stringify(&body))
            .status,
        401
    );
    let auth = format!("Bearer {TOKEN}");
    let headers = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
    ];
    let reply = server.call(
        "POST",
        "/api/irc/preview",
        &headers,
        &json::stringify(&body),
    );
    assert_eq!(reply.status, 200);
    assert_eq!(json::parse(&reply.body).unwrap(), report);
    body.insert("announcement", irc_support::announcement(7));
    assert_eq!(
        server
            .call(
                "POST",
                "/api/irc/preview",
                &headers,
                &json::stringify(&body)
            )
            .status,
        400
    );
    assert_eq!(irc_support::bytes(&server.engine.config.store_dir), before);
    irc_support::no_jobs(&server.engine);
}
