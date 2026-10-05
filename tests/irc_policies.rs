//! Original configuration, admission-free review and persistence scenarios.
mod irc_support;
mod library_support;
use irc_support::*;
use library_support::Directory;
use mynou::{
    config,
    engine::Engine,
    irc::{
        self, Announcement,
        protocol::{Decoder, Event, Message, Protocol},
    },
    json::Value,
    selection::Profile,
};
use std::{fs, path::Path, thread};
#[test]
fn irc_is_opt_in_strict_bounded_and_requires_verified_remote_transport() {
    let dir = Directory::new();
    let legacy = config::from_json(&config::default_json(), &dir.0).unwrap();
    assert!(legacy.irc.sources.is_empty());
    let engine = Engine::open_for_management(legacy.clone()).unwrap();
    assert!(!legacy.store_dir.join("announcements.bin").exists());
    drop(engine);
    let mut v = value(&dir.0, "irc://127.0.0.1:1");
    let mut irc = settings("ircs://irc.example.test:6697");
    let Value::Array(s) = irc.get_mut("sources").unwrap() else {
        panic!()
    };
    let Value::Object(m) = &mut s[0] else {
        panic!()
    };
    m.remove("enabled");
    v.insert("irc", irc);
    assert!(!config::from_json(&v, &dir.0).unwrap().irc.sources[0].enabled);
    for url in [
        "irc://example.test:6667",
        "irc://localhost:6667",
        "ircs://user:secret@example.test:6697",
        "ircs://example.test",
        "ircs://example.test:6697/path",
        "ircs://example.test:0",
        "ircs://example.test:6697?password=secret",
    ] {
        let mut v = value(&dir.0, url);
        assert!(config::from_json(&v, &dir.0).is_err());
        v.insert("irc", Value::Null);
        assert!(config::from_json(&v, &dir.0).is_err());
    }
    for (key, val) in [
        ("password", Value::from("secret")),
        ("sender", Value::from("announce")),
        ("nickname", Value::from("bad nick")),
        ("channel", Value::from("#bad channel")),
        ("password_env", Value::from("lowercase")),
        ("idle_timeout_secs", Value::from(601_u32)),
    ] {
        let mut v = value(&dir.0, "irc://127.0.0.1:1");
        let Value::Array(s) = v.get_mut("irc").unwrap().get_mut("sources").unwrap() else {
            panic!()
        };
        s[0].insert(key, val);
        assert!(config::from_json(&v, &dir.0).is_err());
    }
    for key in ["source", "profile", "action"] {
        let mut v = value(&dir.0, "irc://127.0.0.1:1");
        let Value::Array(r) = v.get_mut("irc").unwrap().get_mut("rules").unwrap() else {
            panic!()
        };
        r[0].insert(key, "unknown");
        assert!(config::from_json(&v, &dir.0).is_err());
    }
    assert!(irc::page("limit=201").is_err());
    assert!(irc::page("offset=1001").is_err());
    assert!(irc::page("limit=1&limit=2").is_err());
}
#[test]
fn decoder_handles_every_fragment_boundary_tags_and_poisoned_input() {
    let wire =
        b"@time=fixture :announce!bot@fixture PRIVMSG #announces :MYNOU {}\r\nPING :nonce\r\n";
    for cut in 0..=wire.len() {
        let mut d = Decoder::default();
        let mut messages = d.feed(&wire[..cut]).unwrap();
        messages.extend(d.feed(&wire[cut..]).unwrap());
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].prefix.as_deref(), Some(SENDER));
        assert_eq!(messages[1].params, vec!["nonce"]);
        assert!(!d.has_partial());
    }
    for bad in [
        b"PING :x\n".as_slice(),
        b"PING :x\r\r\n",
        b"PING :\0\r\n",
        b"@a=1;a=2 PING :x\r\n",
        b"PING :\xff\r\n",
    ] {
        let mut d = Decoder::default();
        assert!(d.feed(bad).is_err());
        assert!(d.feed(b"PING :good\r\n").is_err());
    }
    let mut d = Decoder::default();
    d.feed(&[b'a'; 4096]).unwrap();
    d.feed(&[b'a'; 4096]).unwrap();
    assert!(d.feed(b"x").is_err());
    assert!(Message::parse("cmd param").is_err());
}
#[test]
fn protocol_requires_membership_exact_sender_and_channel_before_delivery() {
    let dir = Directory::new();
    let source = config(&dir.0).irc.sources[0].clone();
    let mut p = Protocol::new(source);
    let trusted = Message::parse(&format!(":{SENDER} PRIVMSG #announces :MYNOU {{}} ")).unwrap();
    assert_eq!(p.receive(&trusted).unwrap(), Event::Ignore);
    assert_eq!(
        p.receive(&Message::parse("PING :hello").unwrap()).unwrap(),
        Event::Reply("PONG :hello".into())
    );
    assert_eq!(
        p.receive(&Message::parse(":server 001 mynou :Hello").unwrap())
            .unwrap(),
        Event::Join
    );
    assert_eq!(p.receive(&trusted).unwrap(), Event::Ignore);
    assert_eq!(
        p.receive(&Message::parse(":mynou!user@host JOIN :#announces").unwrap())
            .unwrap(),
        Event::Joined
    );
    for wire in [
        ":other!bot@fixture PRIVMSG #announces :MYNOU {}",
        ":announce!bot@fixture PRIVMSG #other :MYNOU {}",
        ":announce!bot@fixture PRIVMSG mynou :MYNOU {}",
    ] {
        assert_eq!(
            p.receive(&Message::parse(wire).unwrap()).unwrap(),
            Event::Ignore
        );
    }
    assert!(matches!(
        p.receive(&trusted).unwrap(),
        Event::Announcement(_)
    ));
    assert!(
        p.receive(&Message::parse(":server KICK #announces mynou :Removed").unwrap())
            .is_err()
    );
}
#[test]
fn previews_apply_title_filters_and_selection_profiles_without_creating_storage() {
    let dir = Directory::new();
    let mut cfg = config(&dir.0);
    cfg.irc.rules[0].required_terms = vec!["bluray".into()];
    cfg.irc.rules[0].blocked_terms = vec!["blocked".into()];
    cfg.selection.profiles.insert(
        "any".into(),
        Profile {
            resolutions: vec![1080],
            ..Profile::default()
        },
    );
    for (title, outcome) in [
        ("Fixture.Movie.2024.1080p.BluRay", "matched"),
        ("Fixture.Movie.2024.1080p.WEB-DL", "required_missing"),
        ("Fixture.Movie.2024.1080p.BluRay.blocked", "blocked"),
        ("Fixture.Movie.2024.720p.BluRay", "profile_rejected"),
    ] {
        let mut a = announcement(7);
        a.insert("title", title);
        let report = irc::preview(&cfg, "local", &a).unwrap();
        assert_eq!(
            report.get("evaluations").unwrap().as_array().unwrap()[0]
                .get("outcome")
                .unwrap()
                .as_str(),
            Some(outcome)
        );
        assert_eq!(report.get("persisted").unwrap().as_bool(), Some(false));
        no_credentials(&report);
    }
    assert!(!cfg.store_dir.exists());
    for (key, val) in [
        ("source_url", Value::from("secret")),
        ("tmdb_id", Value::from("07")),
        ("info_hash", Value::from("bad")),
        ("kind", Value::from("series")),
        ("media_title", Value::from("control\n")),
    ] {
        let mut a = announcement(7);
        a.insert(key, val);
        assert!(irc::preview(&cfg, "local", &a).is_err());
    }
}
#[test]
fn first_claim_and_deduplication_survive_review_and_restart_without_job_writes() {
    let dir = Directory::new();
    let cfg = config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let journal = fs::read(cfg.store_dir.join("journal.bin")).unwrap();
    let first = receive(&engine, 7);
    let id = record_id(&first).to_owned();
    let snapshot_bytes = fs::read(cfg.store_dir.join("announcements.bin")).unwrap();
    let mut changed = announcement(7);
    changed.insert("title", "Changed title with the same hash");
    let duplicate = engine
        .irc_receive("local", SENDER, "#announces", &changed)
        .unwrap();
    assert_eq!(duplicate.get("duplicate").unwrap().as_bool(), Some(true));
    assert_eq!(
        duplicate.get("claim_changed").unwrap().as_bool(),
        Some(true)
    );
    assert_eq!(
        fs::read(cfg.store_dir.join("announcements.bin")).unwrap(),
        snapshot_bytes
    );
    let q = reviewed(&engine, &id, "acknowledge");
    engine.irc_control(&id, &q).unwrap();
    assert_eq!(
        engine
            .irc_announcement(&id)
            .unwrap()
            .get("decision")
            .unwrap()
            .as_str(),
        Some("acknowledged")
    );
    no_jobs(&engine);
    assert_eq!(
        fs::read(cfg.store_dir.join("journal.bin")).unwrap(),
        journal
    );
    drop(engine);
    let reopened = Engine::open_for_management(cfg).unwrap();
    let repeat = receive(&reopened, 7);
    assert_eq!(
        repeat.get("decision").unwrap().as_str(),
        Some("acknowledged")
    );
    assert_eq!(rows(&reopened).len(), 1);
    assert!(reopened.irc_control(&id, &q).is_err());
    no_jobs(&reopened);
}
#[test]
fn distinct_sources_hashes_and_canonical_episodes_keep_separate_identities() {
    let dir = Directory::new();
    let mut cfg = config(&dir.0);
    let mut source = cfg.irc.sources[0].clone();
    source.id = "other".into();
    cfg.irc.sources.push(source);
    let engine = Engine::open_for_management(cfg).unwrap();
    let a = announcement(7);
    let first = receive(&engine, 7);
    let other = engine
        .irc_receive("other", SENDER, "#announces", &a)
        .unwrap();
    assert_ne!(record_id(&first), record_id(&other));
    let mut b = a.clone();
    b.insert("info_hash", "1234567890abcdef1234567890abcdef12345679");
    engine
        .irc_receive("local", SENDER, "#announces", &b)
        .unwrap();
    for episode in [1_u32, 2] {
        let mut a = a.clone();
        a.insert("kind", "episode");
        a.insert("season", 1_u32);
        a.insert("episode", episode);
        engine
            .irc_receive("local", SENDER, "#announces", &a)
            .unwrap();
    }
    assert_eq!(rows(&engine).len(), 5);
    no_jobs(&engine);
}
#[test]
fn concurrent_review_guards_allow_one_decision_and_ignore_unrelated_receipts() {
    let dir = Directory::new();
    let engine = Engine::open_for_management(config(&dir.0)).unwrap();
    let r = receive(&engine, 7);
    let id = record_id(&r).to_owned();
    let choices = [
        reviewed(&engine, &id, "acknowledge"),
        reviewed(&engine, &id, "dismiss"),
    ];
    receive(&engine, 8);
    receive(&engine, 7);
    let threads = choices
        .into_iter()
        .map(|q| {
            let engine = engine.clone();
            let id = id.clone();
            thread::spawn(move || engine.irc_control(&id, &q))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        threads
            .into_iter()
            .map(|h| h.join().unwrap().is_ok())
            .filter(|ok| *ok)
            .count(),
        1
    );
    no_jobs(&engine);
}
#[test]
fn disabled_sources_and_untrusted_claims_cannot_change_durable_history() {
    let dir = Directory::new();
    let cfg = config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let before = bytes(&cfg.store_dir);
    for (sender, channel) in [("wrong!bot@fixture", "#announces"), (SENDER, "#other")] {
        assert!(
            engine
                .irc_receive("local", sender, channel, &announcement(7))
                .is_err()
        );
    }
    assert_eq!(bytes(&cfg.store_dir), before);
    drop(engine);
    let mut cfg = cfg;
    cfg.irc.sources[0].enabled = false;
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let before = bytes(&cfg.store_dir);
    assert!(
        engine
            .irc_receive("local", SENDER, "#announces", &announcement(7))
            .is_err()
    );
    assert_eq!(bytes(&cfg.store_dir), before);
    no_jobs(&engine);
}
#[test]
fn checked_history_rejects_corruption_and_signed_semantic_changes_before_startup() {
    let dir = Directory::new();
    let cfg = config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    receive(&engine, 7);
    drop(engine);
    let path = cfg.store_dir.join("announcements.bin");
    let original = fs::read(&path).unwrap();
    for index in [0, 8, 20, original.len() - 1] {
        let mut bad = original.clone();
        bad[index] ^= 1;
        fs::write(&path, bad).unwrap();
        assert!(Engine::open_for_management(cfg.clone()).is_err());
    }
    fs::write(&path, &original).unwrap();
    let original_json = snapshot(&cfg.store_dir);
    for (key, val) in [
        ("id", Value::from("f".repeat(64))),
        ("decision", Value::from("acknowledged")),
        ("binding", Value::from("f".repeat(64))),
    ] {
        let mut v = original_json.clone();
        let Value::Array(rows) = v.get_mut("records").unwrap() else {
            panic!()
        };
        rows[0].insert(key, val);
        write_snapshot(&cfg.store_dir, &v);
        let before = bytes(&cfg.store_dir);
        assert!(Engine::open_for_management(cfg.clone()).is_err());
        assert_eq!(bytes(&cfg.store_dir), before);
    }
    fs::write(&path, &original).unwrap();
    #[cfg(unix)]
    {
        let link = dir.0.join("history-link");
        fs::hard_link(&path, &link).unwrap();
        assert!(Engine::open_for_management(cfg.clone()).is_err());
        fs::remove_file(link).unwrap();
    }
    assert!(Engine::open_for_management(cfg).is_ok());
}
#[test]
fn full_history_keeps_duplicate_suppression_and_rejects_new_identities() {
    let dir = Directory::new();
    let cfg = config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    receive(&engine, 1);
    drop(engine);
    let mut v = snapshot(&cfg.store_dir);
    let prototype = v.get("records").unwrap().as_array().unwrap()[0].clone();
    let binding = prototype.get("binding").unwrap().as_str().unwrap();
    let mut records = Vec::new();
    for n in 1..=1000 {
        let mut record = prototype.clone();
        let a = announcement(n);
        let request = Announcement::from_json(&a).unwrap().request;
        record.insert("announcement", a);
        record.insert(
            "id",
            irc::digest(
                format!(
                    "{binding}\n{}\n{}",
                    request.media_key(),
                    "1234567890abcdef1234567890abcdef12345678"
                )
                .as_bytes(),
            ),
        );
        records.push(record);
    }
    v.insert("records", Value::Array(records));
    write_snapshot(&cfg.store_dir, &v);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let before = bytes(&cfg.store_dir);
    assert_eq!(
        receive(&engine, 1).get("duplicate").unwrap().as_bool(),
        Some(true)
    );
    assert!(
        engine
            .irc_receive("local", SENDER, "#announces", &announcement(1001))
            .is_err()
    );
    assert_eq!(bytes(&cfg.store_dir), before);
    assert_eq!(
        engine
            .irc_announcements(0, 1)
            .unwrap()
            .get("total")
            .unwrap()
            .as_u64(),
        Some(1000)
    );
    no_jobs(&engine);
}
#[test]
fn offline_reviews_do_not_initialize_repair_or_change_storage() {
    let dir = Directory::new();
    let cfg = config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let r = receive(&engine, 7);
    let id = record_id(&r).to_owned();
    drop(engine);
    let before = bytes(&cfg.store_dir);
    let preview = Engine::open_for_preview(cfg.clone()).unwrap();
    let q = reviewed(&preview, &id, "dismiss");
    assert!(preview.irc_control(&id, &q).is_err());
    assert!(
        preview
            .irc_receive("local", SENDER, "#announces", &announcement(8))
            .is_err()
    );
    assert_eq!(bytes(&cfg.store_dir), before);
    drop(preview);
    let mut legacy = config::default_json();
    legacy.insert("store_dir", cfg.store_dir.to_str().unwrap());
    let legacy = config::from_json(&legacy, Path::new(".")).unwrap();
    let existing = cfg.store_dir.join("announcements.bin");
    fs::remove_file(existing).unwrap();
    let before = bytes(&cfg.store_dir);
    let preview = Engine::open_for_preview(legacy).unwrap();
    assert!(rows(&preview).is_empty());
    assert_eq!(bytes(&cfg.store_dir), before);
}
#[test]
fn source_rebinding_and_profile_edits_fence_existing_reviews() {
    let dir = Directory::new();
    let cfg = config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let r = receive(&engine, 7);
    let id = record_id(&r).to_owned();
    let q = reviewed(&engine, &id, "acknowledge");
    drop(engine);
    let before = bytes(&cfg.store_dir);
    let mut rebound = cfg.clone();
    rebound.irc.sources[0].sender = "other!bot@fixture".into();
    assert!(Engine::open_for_management(rebound).is_err());
    assert_eq!(bytes(&cfg.store_dir), before);
    let mut cfg = cfg;
    cfg.selection.profiles.insert(
        "any".into(),
        Profile {
            blocked_terms: vec!["blocked".into()],
            ..Profile::default()
        },
    );
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    assert!(engine.irc_control(&id, &q).is_err());
    assert_eq!(bytes(&cfg.store_dir), before);
    let q = reviewed(&engine, &id, "acknowledge");
    engine.irc_control(&id, &q).unwrap();
    no_jobs(&engine);
}

#[test]
fn conflicting_matching_rules_and_disabled_filters_are_explicit_without_acquisition() {
    let dir = Directory::new();
    let mut cfg = config(&dir.0);
    let mut extra = cfg.irc.rules[0].clone();
    extra.id = "second".into();
    cfg.irc.rules.push(extra);
    let engine = Engine::open_for_management(cfg).unwrap();
    let row = receive(&engine, 7);
    assert_eq!(row.get("outcome").unwrap().as_str(), Some("conflict"));
    let query = reviewed(&engine, record_id(&row), "acknowledge");
    let report = engine.irc_control(record_id(&row), &query).unwrap();
    assert_eq!(
        report.get("acquisition_started").unwrap().as_bool(),
        Some(false)
    );
    no_jobs(&engine);
}
