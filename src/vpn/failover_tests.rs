//! Several WireGuard profiles (#407): the stored list and its controls, and a
//! dead server handing over to the next profile, with forwarding renewed
//! there and the switch reported.
use super::*;
use crate::tunnel_http::tests::{NatPmp, failover::Failover};

fn second() -> String {
    PROFILE.replace("192.0.2.10", "192.0.2.11")
}
fn named(profile: &str, name: &str) -> Value {
    let mut value = body("import");
    value.insert("profile", profile);
    value.insert("name", name);
    value
}
fn at(action: &str, index: u32, to: Option<u32>) -> Value {
    let mut value = body(action);
    value.insert("index", index);
    if let Some(to) = to {
        value.insert("to", to);
    }
    value
}
/// One control call on a fresh management engine, as the service does.
fn control(config: &crate::config::Config, value: &Value) -> Result<Value> {
    let engine = Engine::open_for_management(config.clone()).unwrap();
    let result = engine.vpn_control(value);
    if let Ok(report) = &result {
        assert_redacted(report);
        assert!(!json::stringify(report).contains("192.0.2.11"));
    }
    result
}
fn names(report: &Value) -> Vec<String> {
    report
        .get("profiles")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|p| p.get("name").and_then(Value::as_str).unwrap().to_owned())
        .collect()
}
fn stored_intent(config: &crate::config::Config) -> (String, Intent) {
    let text = profile::read_private_text(&stored(config)).unwrap();
    let intent = Intent::decode(&text).unwrap();
    (text, intent)
}

#[test]
fn todays_single_profile_opens_unchanged_as_a_one_profile_list() {
    let directory = Directory::new();
    let config = directory.config();
    // A store written by 0.29.0: one profile, VPN and forwarding on.
    drop(Engine::open_for_management(config.clone()).unwrap());
    let document = profile::parse(PROFILE).unwrap().private_document();
    let today = format!("{ENABLED}{FORWARDING}{document}");
    persist(&stored(&config), today.as_bytes()).unwrap();

    let intent = Intent::decode(&today).unwrap();
    assert_eq!(intent.profiles.len(), 1);
    assert!(intent.profiles[0].name.is_none());
    assert_eq!(intent.document(), today);
    let operator = Operator::load(&config.store_dir, false);
    {
        let state = operator.state.lock().unwrap();
        assert_eq!(state.names, ["Profile 1"]);
        assert_eq!(state.gateways, [Ipv4Addr::new(10, 23, 0, 1)]);
        assert_eq!(state.document, today);
    }
    let engine = Engine::open_for_management(config.clone()).unwrap();
    let report = engine.vpn_status();
    assert_eq!(report.get("enabled"), Some(&Value::Bool(true)));
    assert_eq!(names(&report), ["Profile 1"]);
    assert_eq!(
        forwarding(&report, "forwarding").as_deref(),
        Some("acquiring")
    );
    assert_redacted(&report);
    drop(engine);

    // Controls that leave the list alone keep today's layout on disk.
    control(&config, &body("disable-forwarding")).unwrap();
    control(&config, &body("enable-forwarding")).unwrap();
    let (text, intent) = stored_intent(&config);
    assert_eq!(text, today);
    assert!(intent.forwarding && intent.mode == Mode::Enabled);
}

#[test]
fn profiles_are_added_moved_and_removed_and_keep_their_names() {
    let directory = Directory::new();
    let config = directory.config();
    control(&config, &import()).unwrap();
    let report = control(&config, &named(&second(), "  Amsterdam #5\u{7}  ")).unwrap();
    assert_eq!(names(&report), ["Profile 1", "Amsterdam #5"]);
    let (text, intent) = stored_intent(&config);
    assert!(text.contains("# mynou-profile Profile 1\n[Interface]"));
    assert!(text.contains("# mynou-profile Amsterdam #5\n[Interface]"));
    assert!(intent.mode == Mode::Enabled);
    assert_eq!(
        fs::metadata(stored(&config)).unwrap().permissions().mode() & 0o7777,
        0o600
    );

    // The same profile twice, or misplaced fields, are refused as a whole.
    for (value, refused) in [
        (import(), "This VPN profile is already imported"),
        (at("move", 2, Some(0)), "No such VPN profile"),
        (at("move", 0, None), "No such VPN profile"),
        (at("remove", 7, None), "No such VPN profile"),
        (
            {
                let mut value = body("enable");
                value.insert("name", "secret-name");
                value
            },
            "A profile name is only accepted by import",
        ),
        (
            {
                let mut value = import();
                value.insert("index", 0_u32);
                value
            },
            "A profile position is only accepted by remove and move",
        ),
    ] {
        assert_eq!(control(&config, &value).unwrap_err(), refused);
    }
    assert_eq!(stored_intent(&config).0, text);

    // Moving changes the order of failover, never a name.
    let report = control(&config, &at("move", 1, Some(0))).unwrap();
    assert_eq!(names(&report), ["Amsterdam #5", "Profile 1"]);
    let (_, intent) = stored_intent(&config);
    assert!(
        intent.profiles[0]
            .profile
            .private_document()
            .contains("192.0.2.11")
    );
    let operator = Operator::load(&config.store_dir, false);
    assert_eq!(
        operator.state.lock().unwrap().names,
        ["Amsterdam #5", "Profile 1"]
    );
    drop(operator);

    // The last profile goes only once the VPN is off: protected traffic
    // never falls back to a direct connection.
    let report = control(&config, &at("remove", 0, None)).unwrap();
    assert_eq!(names(&report), ["Profile 1"]);
    assert_eq!(
        control(&config, &at("remove", 0, None)).unwrap_err(),
        "Turn the VPN off before removing its last profile"
    );
    control(&config, &body("enable-forwarding")).unwrap();
    control(&config, &body("disable")).unwrap();
    let report = control(&config, &at("remove", 0, None)).unwrap();
    assert!(names(&report).is_empty());
    let (_, intent) = stored_intent(&config);
    assert!(intent.profiles.is_empty() && !intent.forwarding);
    assert!(intent.mode == Mode::Disabled);
    let engine = Engine::open_for_management(config.clone()).unwrap();
    assert_eq!(state(&engine.vpn_status()), "disabled");
    assert!(engine.torrent_publication().is_ok());
}

#[test]
fn the_list_is_bounded() {
    let directory = Directory::new();
    let config = directory.config();
    for port in 0..MAX_PROFILES {
        let profile = PROFILE.replace(":51820", &format!(":{}", 51820 + port));
        control(&config, &named(&profile, &"n".repeat(200))).unwrap();
    }
    let (text, intent) = stored_intent(&config);
    assert_eq!(intent.profiles.len(), MAX_PROFILES);
    assert!(text.len() <= profile::MAX_PROFILE_BYTES);
    assert!(
        intent
            .profiles
            .iter()
            .all(|e| e.name.as_deref() == Some(&"n".repeat(NAME_CHARS)[..]))
    );
    let one_more = PROFILE.replace(":51820", ":52820");
    assert_eq!(
        control(&config, &named(&one_more, "Extra")).unwrap_err(),
        format!("Mynou keeps at most {MAX_PROFILES} VPN profiles")
    );
    assert_eq!(stored_intent(&config).0, text);
}

#[test]
fn a_dead_server_hands_over_to_the_next_profile_and_forwarding_follows() {
    let directory = Directory::new();
    let f = Failover::new();
    let gateways = [
        NatPmp::start(&f.servers[0], 53186),
        NatPmp::start(&f.servers[1], 53190),
    ];
    let config = directory.config();
    control(&config, &named(PROFILE, "Zurich")).unwrap();
    control(&config, &named(&second(), "Amsterdam")).unwrap();
    control(&config, &body("enable-forwarding")).unwrap();
    let operator = Operator::load(&config.store_dir, false);
    {
        // The fixture's owner stands in for the two profiles' tunnel; each
        // fixture server is gateway and DNS server 10.7.0.1 in its tunnel.
        let mut state = operator.state.lock().unwrap();
        state.gateways = vec![Ipv4Addr::new(10, 7, 0, 1); 2];
        state.driver = Some(f.client.clone());
    }
    let open = || {
        let engine =
            Engine::open_operator(config.clone(), true, false, true, Some(operator.clone()))
                .unwrap();
        while !engine.startup_step().unwrap() {}
        engine.start_protected_transfers();
        engine
    };
    let text = |value: &Value, field: &str| forwarding(value, field);

    let engine = open();
    let status = engine.vpn_status();
    assert_eq!(state(&status), "protected");
    assert_eq!(text(&status, "active_profile").as_deref(), Some("Zurich"));
    assert_eq!(text(&status, "forwarded_port").as_deref(), Some("53186"));
    assert_eq!(status.get("switches"), Some(&Value::Array(Vec::new())));
    assert_eq!(peer_port(&engine), 53186);
    let probe = engine.torrent_network.datagram(false).unwrap();
    let tracker: SocketAddr = "10.7.0.1:6969".parse().unwrap();
    let captured = engine.torrent_network.clone();

    // Zurich stops answering. Its session expires and its retry gives up
    // (the client's clock is skipped); Amsterdam then takes over.
    f.relays[0].silent.store(true, Ordering::Release);
    probe.send_to(b"failover-marker", tracker).unwrap();
    wait(|| {
        let status = f.report().status;
        if status.profile == 1 && status.state == TunnelState::Protected {
            return true;
        }
        if status.state != TunnelState::Protected {
            // Torrents wait: nothing they open works, and nothing goes direct.
            assert!(engine.vpn_needs_reload());
            assert!(captured.publication().is_err());
            assert!(probe.send_to(b"failover-marker", tracker).is_err());
            assert!(captured.datagram(false).is_err());
            assert!(captured.connect(tracker, Duration::from_secs(1)).is_err());
        }
        if status.profile == 0 {
            f.skip(Duration::from_secs(10));
        }
        false
    });
    let status = engine.vpn_status();
    assert_redacted(&status);
    assert_eq!(
        text(&status, "active_profile").as_deref(),
        Some("Amsterdam")
    );
    let profiles = status.get("profiles").and_then(Value::as_array).unwrap();
    assert_eq!(profiles[0].get("active"), Some(&Value::Bool(false)));
    assert_eq!(profiles[1].get("active"), Some(&Value::Bool(true)));
    let switches = status.get("switches").and_then(Value::as_array).unwrap();
    assert_eq!(switches.len(), 1);
    assert_eq!(text(&switches[0], "from").as_deref(), Some("Zurich"));
    assert_eq!(text(&switches[0], "to").as_deref(), Some("Amsterdam"));
    assert_eq!(text(&switches[0], "reason").as_deref(), Some("GaveUp"));
    let at = switches[0].get("at").and_then(Value::as_u64).unwrap();
    assert!(at.abs_diff(crate::store::now()) < 60);
    // The statistics carry the same, for the overview.
    let stats = engine.vpn_failover();
    assert_eq!(stats.get("active_profile"), status.get("active_profile"));
    assert_eq!(stats.get("switches"), status.get("switches"));

    // The service then replaces the engine: its listener takes the port
    // forwarded on the new profile, which peers and trackers are told.
    engine.retire_vpn_generation();
    drop(engine);
    let engine = open();
    let status = engine.vpn_status();
    assert_eq!(state(&status), "protected");
    assert_eq!(text(&status, "forwarded_port").as_deref(), Some("53190"));
    assert_eq!(peer_port(&engine), 53190);
    let server = f.servers[1]
        .network(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert!(
        server
            .connect(
                SocketAddr::from(([10, 7, 0, 2], 53190)),
                Duration::from_secs(2)
            )
            .is_ok()
    );
    assert!(gateways[1].requests.load(Ordering::Relaxed) > 0);

    // Every datagram the client sent reached one of its two servers, and
    // each is a WireGuard message: nothing left in clear or elsewhere.
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.wire().len() as u64 != f.report().stats.sent {
        assert!(Instant::now() < deadline, "a datagram went elsewhere");
        thread::sleep(Duration::from_millis(5));
    }
    let wire = f.wire();
    assert!(wire.iter().all(|d| matches!(d.first(), Some(1..=4))));
    assert!(
        !wire
            .iter()
            .any(|d| d.windows(15).any(|w| w == b"failover-marker"))
    );
    drop(engine);
    drop(gateways);
}
