use super::*;
use crate::{config, engine::Engine, json, store::Request};
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    sync::mpsc,
    thread,
};

const PROFILE: &str = "[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nAddress = 10.23.0.2/24\nDNS = 10.23.0.1\nMTU = 1280\n[Peer]\nPublicKey = CQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\nPresharedKey = //////////////////////////////////////////8=\nAllowedIPs = 0.0.0.0/0\nEndpoint = 192.0.2.10:51820\nPersistentKeepalive = 25\n";
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mynou-vpn-{}",
            crate::crypto::random_bytes::<16>()
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn config(&self) -> crate::config::Config {
        let mut v = config::default_json();
        let downloads = v.get_mut("downloads").unwrap();
        downloads.insert("listen_port", 0_u32);
        downloads.insert("dht", false);
        config::from_json(&v, &self.0).unwrap()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn body(action: &str) -> Value {
    let mut v = Value::object();
    v.insert("action", action);
    v
}
fn import() -> Value {
    let mut v = body("import");
    v.insert("profile", PROFILE);
    v
}
fn state(value: &Value) -> &str {
    value.get("state").and_then(Value::as_str).unwrap()
}
fn assert_redacted(value: &Value) {
    let text = json::stringify(value);
    for secret in [
        "AQEBAQE",
        "////////",
        "PrivateKey",
        "PresharedKey",
        "192.0.2.10",
        "10.23.0.1",
        "vpn.conf",
    ] {
        assert!(!text.contains(secret), "report leaked a private value");
    }
}
fn submitted(engine: &Engine, request: Request) -> crate::store::Job {
    match engine.submit(request) {
        Ok(jobs) => match jobs.into_iter().next() {
            Some(job) => job,
            None => panic!("Synthetic VPN fixture request was not created"),
        },
        // Discard the error explicitly: no Result panic/debug wrapper may
        // format a credential-bearing storage or requester diagnostic.
        Err(_) => panic!("Synthetic VPN fixture request submission failed"),
    }
}
fn stored(config: &crate::config::Config) -> PathBuf {
    config.store_dir.join("vpn.conf")
}

#[test]
fn canonical_private_document_and_structured_settings_preserve_the_supported_profile() {
    let original = profile::parse(PROFILE).unwrap();
    let document = original.private_document();
    let roundtrip = profile::parse(&document).unwrap().into_tunnel_config();
    let original = original.into_tunnel_config();
    assert_eq!(roundtrip.secret, original.secret);
    assert_eq!(roundtrip.peer_public, original.peer_public);
    assert_eq!(roundtrip.psk, original.psk);
    assert_eq!(roundtrip.endpoint, original.endpoint);
    assert_eq!(roundtrip.ip, original.ip);
    assert_eq!(roundtrip.mtu, original.mtu);
    assert_eq!(roundtrip.keepalive, original.keepalive);
    assert_eq!(
        roundtrip.dns.unwrap().resolvers,
        original.dns.unwrap().resolvers
    );
    let mut settings = Value::object();
    for line in PROFILE.lines().filter_map(|s| s.split_once(" = ")) {
        settings.insert(line.0, line.1);
    }
    assert_eq!(
        profile::from_settings(&settings)
            .unwrap()
            .private_document(),
        document
    );
    for value in [
        "PostUp = secret-command",
        "secret\n[Peer]",
        "secret\r\n",
        "#secret",
        "é",
        "\t",
    ] {
        settings.insert("PrivateKey", value);
        assert!(
            !profile::from_settings(&settings)
                .err()
                .unwrap()
                .contains(value)
        );
    }
    settings.insert("hostile-secret-key", "value");
    assert!(
        !profile::from_settings(&settings)
            .err()
            .unwrap()
            .contains("hostile-secret-key")
    );
}

#[test]
fn intent_survives_import_revoke_enable_and_explicit_disable_without_network_io() {
    let directory = Directory::new();
    let config = directory.config();
    let engine = Engine::open_for_management(config.clone()).unwrap();
    assert_eq!(state(&engine.vpn_status()), "disabled");
    let result = engine.vpn_control(&import()).unwrap();
    assert_eq!(state(&result), "blocked");
    assert_redacted(&result);
    assert!(engine.torrent_publication().is_err());
    drop(engine);
    let path = stored(&config);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    for (action, mode) in [
        ("revoke", Mode::Revoked),
        ("enable", Mode::Enabled),
        ("disable", Mode::Disabled),
    ] {
        let engine = Engine::open_for_management(config.clone()).unwrap();
        assert_redacted(&engine.vpn_status());
        engine.vpn_control(&body(action)).unwrap();
        drop(engine);
        let intent = Intent::decode(&profile::read_private_text(&path).unwrap()).unwrap();
        assert!(intent.mode == mode);
        assert!(intent.profile.is_some());
    }
    let engine = Engine::open_for_management(config).unwrap();
    assert_eq!(state(&engine.vpn_status()), "disabled");
    assert!(engine.torrent_publication().is_ok());
}

#[test]
fn invalid_import_and_unknown_controls_leave_current_authority_intact_and_redact_errors() {
    let directory = Directory::new();
    let engine = Engine::open_for_management(directory.config()).unwrap();
    for mut value in [body("secret-unknown-action"), import(), body("disable")] {
        if value.get("profile").is_some() {
            value.insert("profile", "[Interface]\nPrivateKey = secret-invalid-key");
        } else if value.get("action").and_then(Value::as_str) == Some("disable") {
            value.insert("secret-key", "secret-value");
        }
        let error = engine.vpn_control(&value).unwrap_err();
        assert!(!error.contains("secret"));
        assert!(engine.torrent_publication().is_ok());
    }
    assert!(!stored(&engine.config).exists());
    let mut value = import();
    value.insert("settings", Value::object());
    assert!(engine.vpn_control(&value).is_err());
    assert!(engine.torrent_publication().is_ok());
}

#[test]
fn corrupt_or_shared_persistent_intent_blocks_every_network_entry_including_metadata() {
    for corrupt in [true, false] {
        let directory = Directory::new();
        let config = directory.config();
        let engine = Engine::open_for_management(config.clone()).unwrap();
        engine.vpn_control(&import()).unwrap();
        drop(engine);
        let path = stored(&config);
        if corrupt {
            fs::write(&path, "secret-corrupt-document").unwrap();
        } else {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let engine = Engine::open_for_management(config).unwrap();
        assert_eq!(state(&engine.vpn_status()), "error");
        assert_redacted(&engine.vpn_status());
        let network = &engine.torrent_network;
        assert!(network.generation().is_err());
        assert!(network.publication().is_err());
        assert!(
            network
                .resolve("secret.invalid:80", Instant::now() + Duration::from_secs(1))
                .is_err()
        );
        assert!(
            network
                .connect("127.0.0.1:9".parse().unwrap(), Duration::from_secs(1))
                .is_err()
        );
        assert!(network.listen("0.0.0.0:0".parse().unwrap()).is_err());
        assert!(network.datagram(false).is_err());
        assert!(network.datagram(true).is_err());
        assert!(network.http().get("http://127.0.0.1:9/").is_err());
    }
}

#[test]
fn failed_persistence_retires_authority_and_keeps_the_previous_durable_intent() {
    let directory = Directory::new();
    let config = directory.config();
    let engine = Engine::open_for_management(config.clone()).unwrap();
    engine.vpn_control(&import()).unwrap();
    drop(engine);
    let path = stored(&config);
    let before = fs::read(&path).unwrap();
    let engine = Engine::open_for_management(config.clone()).unwrap();
    let error =
        crate::store::with_full_disk_after(0, || engine.vpn_control(&body("disable"))).unwrap_err();
    assert_eq!(error, STORAGE);
    assert!(engine.torrent_publication().is_err());
    assert_eq!(state(&engine.vpn_status()), "error");
    assert!(!engine.vpn_needs_reload());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!fs::read_dir(&config.store_dir).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".vpn-")
    }));
    drop(engine);
    let engine = Engine::open_for_management(config).unwrap();
    assert_eq!(engine.vpn_status().get("enabled"), Some(&Value::Bool(true)));
    assert!(engine.torrent_publication().is_err());
}

#[test]
fn persistence_refuses_links_without_touching_the_target_and_read_only_controls_refuse_writes() {
    let directory = Directory::new();
    let config = directory.config();
    let engine = Engine::open_for_management(config.clone()).unwrap();
    let target = directory.0.join("unrelated");
    fs::write(&target, "preserved").unwrap();
    symlink(&target, stored(&config)).unwrap();
    assert!(engine.vpn_control(&import()).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "preserved");
    drop(engine);
    let engine = Engine::open_for_preview(config).unwrap();
    assert!(engine.vpn_control(&body("disable")).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "preserved");
}

#[test]
fn revoke_closes_admission_before_waiting_for_an_existing_publication() {
    let directory = Directory::new();
    let engine = Engine::open_for_management(directory.config()).unwrap();
    let publication = engine.torrent_publication().unwrap();
    let gate = engine.vpn_gate.as_ref().unwrap().clone();
    let other = engine.clone();
    let (send, recv) = mpsc::channel();
    let worker = thread::spawn(move || {
        send.send(other.vpn_control(&import())).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    while !gate.retired.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    assert!(gate.generation().is_err());
    assert!(matches!(recv.try_recv(), Err(mpsc::TryRecvError::Empty)));
    drop(publication);
    recv.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    worker.join().unwrap();
    assert!(engine.torrent_publication().is_err());
}

#[test]
fn revoked_startup_keeps_workers_and_listeners_closed_but_allows_recovery_controls() {
    let directory = Directory::new();
    let config = directory.config();
    let engine = Engine::open_for_management(config.clone()).unwrap();
    engine.vpn_control(&import()).unwrap();
    drop(engine);
    let engine = Engine::open_for_management(config.clone()).unwrap();
    engine.vpn_control(&body("revoke")).unwrap();
    drop(engine);
    let engine = Engine::open_serving(config).unwrap();
    for _ in 0..20 {
        assert!(!engine.startup_step().unwrap());
    }
    assert!(engine.starting.load(Ordering::Acquire));
    assert_eq!(
        crate::server::route(&engine, "POST", "/api/jobs", b"{}")
            .unwrap()
            .0,
        503
    );
    let report = crate::server::route(&engine, "GET", "/api/vpn", &[])
        .unwrap()
        .1;
    assert_eq!(state(&report), "blocked");
    assert_redacted(&report);
    assert!(engine.transfer(&"ab".repeat(20)).is_err());
    assert_eq!(
        crate::server::route(
            &engine,
            "POST",
            "/api/vpn/control",
            b"{\"action\":\"disable\"}"
        )
        .unwrap()
        .0,
        202
    );
    assert!(engine.vpn_needs_reload());
}

fn native_operator(directory: &Directory, driver: HttpTunnel) -> Arc<Operator> {
    let config = directory.config();
    let engine = Engine::open_for_management(config.clone()).unwrap();
    engine.vpn_control(&import()).unwrap();
    drop(engine);
    let operator = Operator::load(&config.store_dir, false);
    operator.state.lock().unwrap().driver = Some(driver);
    operator
}

#[test]
fn real_native_owner_is_used_for_protected_startup_dns_and_revocation() {
    let directory = Directory::new();
    let pair = crate::tunnel_http::tests::Pair::new();
    let operator = native_operator(&directory, pair.client.clone());
    let engine =
        Engine::open_operator(directory.config(), true, false, true, Some(operator)).unwrap();
    while !engine.startup_step().unwrap() {}
    assert_eq!(state(&engine.vpn_status()), "protected");
    assert_redacted(&engine.vpn_status());
    assert!(
        engine
            .vpn_status()
            .get("handshake_age_secs")
            .and_then(Value::as_str)
            .is_some()
    );
    let generation = engine.torrent_network.generation().unwrap().unwrap();
    assert_eq!(
        engine
            .torrent_network
            .resolve("fixture.test:80", Instant::now() + Duration::from_secs(3))
            .unwrap(),
        vec!["10.7.0.1:80".parse::<SocketAddr>().unwrap()]
    );
    assert!(pair.dns.load(Ordering::Relaxed) > 0);
    let job = submitted(
        &engine,
        Request {
            kind: "movie".into(),
            title: "Synthetic preserved request".into(),
            year: 2026,
            season: 0,
            episode: 0,
            tmdb_id: Some(7),
            source_path: None,
            source_url: None,
            source_numbering: None,
        },
    );
    let report = engine.vpn_control(&body("revoke")).unwrap();
    assert_redacted(&report);
    assert!(engine.torrent_network.publication().is_err());
    assert!(pair.client_network.publication().is_err());
    drop(engine);
    let engine = Engine::open_for_management(directory.config()).unwrap();
    assert_eq!(state(&engine.vpn_status()), "blocked");
    assert!(
        engine
            .store
            .lock()
            .unwrap()
            .get(&job.id)
            .unwrap()
            .unwrap()
            .request
            .title
            == "Synthetic preserved request",
        "Synthetic request was not retained"
    );
    assert!(engine.torrent_network.generation().is_err());
    assert!(!generation.is_empty());
}

#[test]
fn owner_loss_never_replaces_a_captured_engine_transport_with_direct() {
    let directory = Directory::new();
    let pair = crate::tunnel_http::tests::Pair::new();
    let operator = native_operator(&directory, pair.client.clone());
    let engine = Engine::open_operator(
        directory.config(),
        true,
        false,
        true,
        Some(operator.clone()),
    )
    .unwrap();
    while !engine.startup_step().unwrap() {}
    let queued = submitted(
        &engine,
        Request {
            kind: "movie".into(),
            title: "Synthetic suspended demand".into(),
            year: 2026,
            season: 0,
            episode: 0,
            tmdb_id: Some(9),
            source_path: None,
            source_url: None,
            source_numbering: None,
        },
    );
    pair.client.cancel();
    assert!(!engine.tick().unwrap());
    let retained = engine
        .store
        .lock()
        .unwrap()
        .get(&queued.id)
        .unwrap()
        .unwrap();
    assert!(
        retained.attempts == 0,
        "Suspension consumed a fixture retry"
    );
    assert!(
        retained.state == "queued",
        "Suspension changed the fixture state"
    );
    assert!(engine.vpn_needs_reload());
    assert!(engine.torrent_network.publication().is_err());
    assert!(engine.torrent_network.datagram(false).is_err());
    drop(engine);
    let replacement =
        Engine::open_operator(directory.config(), true, false, true, Some(operator)).unwrap();
    for _ in 0..14 {
        assert!(!replacement.startup_step().unwrap());
    }
    assert!(replacement.torrent_publication().is_err());
    assert_eq!(
        replacement.vpn_status().get("enabled"),
        Some(&Value::Bool(true))
    );
}

#[test]
fn supervisor_replaces_retired_generations_and_keeps_management_api_reachable() {
    let directory = Directory::new();
    let pair = crate::tunnel_http::tests::Pair::new();
    let operator = native_operator(&directory, pair.client.clone());
    let mut config = directory.config();
    config.listen = "127.0.0.1:0".into();
    let engine = Engine::open_operator(config.clone(), true, false, true, Some(operator)).unwrap();
    let api = crate::server::Api::bind(engine).unwrap();
    let url = format!("http://{}", api.address().unwrap());
    let worker = thread::spawn(move || api.run_supervised());
    let client = HttpClient::new()
        .without_proxy()
        .with_timeout(Duration::from_secs(10));
    let call = |method: &str, path: &str, data: &str| {
        client
            .request(
                method,
                &format!("{url}{path}"),
                &[
                    (
                        "Authorization".into(),
                        "Bearer 0123456789abcdef0123456789abcdef".into(),
                    ),
                    ("Content-Type".into(), "application/json".into()),
                ],
                data.as_bytes(),
            )
            .unwrap()
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = call("GET", "/api/status", "");
        let value = json::parse(std::str::from_utf8(&response.body).unwrap()).unwrap();
        if value
            .get("startup")
            .unwrap()
            .get("state")
            .and_then(Value::as_str)
            == Some("ready")
        {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        call("POST", "/api/vpn/control", "{\"action\":\"revoke\"}").status,
        202
    );
    loop {
        let response = call("GET", "/api/vpn", "");
        let value = json::parse(std::str::from_utf8(&response.body).unwrap()).unwrap();
        if value.get("reason").and_then(Value::as_str) == Some("Profile revoked by operator") {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(call("GET", "/readyz", "").status, 503);
    assert_eq!(
        call("POST", "/api/vpn/control", "{\"action\":\"disable\"}").status,
        202
    );
    loop {
        if call("GET", "/readyz", "").status == 200 {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    let response = call("GET", "/api/vpn", "");
    let value = json::parse(std::str::from_utf8(&response.body).unwrap()).unwrap();
    assert_eq!(state(&value), "disabled");
    assert_eq!(call("POST", "/api/shutdown", "{}").status, 200);
    worker.join().unwrap().unwrap();
    assert!(
        Intent::decode(&profile::read_private_text(&stored(&config)).unwrap())
            .unwrap()
            .mode
            == Mode::Disabled
    );
}

#[test]
fn unauthenticated_startup_stays_closed_even_without_download_workers_and_restart_keeps_intent() {
    use std::net::UdpSocket;
    let directory = Directory::new();
    let endpoint = UdpSocket::bind("127.0.0.1:0").unwrap();
    endpoint
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut config = profile::parse(PROFILE).unwrap().into_tunnel_config();
    crate::tunnel_http::tests::allow_outer(endpoint.local_addr().unwrap());
    config.bind = "127.0.0.1:0".parse().unwrap();
    config.endpoint = crate::tunnel::Endpoint::Address(endpoint.local_addr().unwrap());
    let driver = HttpTunnel::start(config).unwrap();
    crate::tunnel_http::tests::allow_driver_outer(&driver);
    let mut bytes = [0; 2048];
    let (_, remote) = endpoint.recv_from(&mut bytes).unwrap();
    endpoint.send_to(&[1; 148], remote).unwrap();
    let operator = native_operator(&directory, driver);
    let mut config = directory.config();
    config.downloads_enabled = false;
    let engine = Engine::open_operator(config.clone(), true, false, true, Some(operator)).unwrap();
    for _ in 0..15 {
        assert!(!engine.startup_step().unwrap());
    }
    assert_eq!(state(&engine.vpn_status()), "connecting");
    assert_redacted(&engine.vpn_status());
    assert!(engine.torrent_publication().is_err());
    assert!(engine.torrent_network.datagram(false).is_err());
    assert!(!engine.tick().unwrap());
    drop(engine);
    let engine = Engine::open_for_management(config).unwrap();
    assert_eq!(engine.vpn_status().get("enabled"), Some(&Value::Bool(true)));
    assert!(engine.torrent_publication().is_err());
}

#[test]
fn verified_bytes_user_pause_and_priorities_survive_owner_loss_and_a_new_protected_engine() {
    use crate::bencode::{self, Value as B};
    use std::collections::BTreeMap;
    let directory = Directory::new();
    let pair = crate::tunnel_http::tests::Pair::new();
    let payload = vec![7_u8; 32_000];
    let pieces = payload
        .chunks(16_384)
        .flat_map(crate::crypto::sha1)
        .collect();
    let info = B::Dict(BTreeMap::from([
        (b"name".to_vec(), B::Bytes(b"fixture.bin".to_vec())),
        (b"length".to_vec(), B::Int(payload.len() as i64)),
        (b"piece length".to_vec(), B::Int(16_384)),
        (b"pieces".to_vec(), B::Bytes(pieces)),
    ]));
    let hash: String = crate::crypto::sha1(&bencode::encode(&info))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let source = directory.0.join("fixture.torrent");
    fs::write(
        &source,
        bencode::encode(&B::Dict(BTreeMap::from([(b"info".to_vec(), info)]))),
    )
    .unwrap();
    let config = directory.config();
    let data = config.downloads.data_dir.join(&hash).join("fixture.bin");
    fs::create_dir_all(data.parent().unwrap()).unwrap();
    fs::write(&data, &payload).unwrap();
    let operator = native_operator(&directory, pair.client.clone());
    let engine = Engine::open_operator(config.clone(), true, false, true, Some(operator)).unwrap();
    while !engine.startup_step().unwrap() {}
    submitted(
        &engine,
        Request {
            kind: "movie".into(),
            title: "Synthetic retained bytes".into(),
            year: 2026,
            season: 0,
            episode: 0,
            tmdb_id: Some(13),
            source_path: None,
            source_url: Some(source.to_str().unwrap().into()),
            source_numbering: None,
        },
    );
    engine.tick().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while engine.transfer(&hash).unwrap().get("ready") != Some(&Value::Bool(true)) {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    engine.pause_transfer(&hash).unwrap();
    engine.set_transfer_priority(&hash, 7).unwrap();
    engine
        .set_file_priority(&hash, 0, crate::torrent::FilePriority::Low)
        .unwrap();
    let before = engine.transfer(&hash).unwrap();
    pair.client.cancel();
    assert!(engine.vpn_needs_reload());
    drop(engine);
    assert_eq!(fs::read(&data).unwrap(), payload);
    let replacement_pair = crate::tunnel_http::tests::Pair::new();
    let operator = Operator::load(&config.store_dir, false);
    operator.state.lock().unwrap().driver = Some(replacement_pair.client.clone());
    let engine = Engine::open_operator(config, true, false, true, Some(operator)).unwrap();
    while !engine.startup_step().unwrap() {}
    assert_eq!(state(&engine.vpn_status()), "protected");
    let after = engine.transfer(&hash).unwrap();
    assert_eq!(after.get("user_paused"), Some(&Value::Bool(true)));
    assert_eq!(after.get("priority"), Some(&Value::Number(7.0)));
    assert_eq!(after.get("file_priorities"), before.get("file_priorities"));
    assert_eq!(after.get("failed"), Some(&Value::Bool(false)));
    assert_eq!(fs::read(&data).unwrap(), payload);
}

#[test]
fn recovered_owner_requires_a_new_engine_and_never_reauthorizes_delayed_old_work() {
    let directory = Directory::new();
    let pair = crate::tunnel_http::tests::Pair::new();
    let operator = native_operator(&directory, pair.client.clone());
    let engine = Engine::open_operator(
        directory.config(),
        true,
        false,
        true,
        Some(operator.clone()),
    )
    .unwrap();
    while !engine.startup_step().unwrap() {}
    let old = engine.torrent_network.generation().unwrap().unwrap();
    let captured = engine.torrent_network.clone();
    pair.replace_client_generation();
    assert!(engine.vpn_needs_reload());
    let deadline = Instant::now() + Duration::from_secs(5);
    while pair.client.network(deadline).is_err() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(captured.publication().is_err());
    assert_eq!(state(&engine.vpn_status()), "reconnecting");
    engine.retire_vpn_generation();
    drop(engine);
    let engine =
        Engine::open_operator(directory.config(), true, false, true, Some(operator)).unwrap();
    while !engine.startup_step().unwrap() {
        assert!(Instant::now() < deadline);
    }
    let new = engine.torrent_network.generation().unwrap().unwrap();
    assert_ne!(old, new);
    assert!(engine.torrent_publication().is_ok());
    assert!(captured.publication().is_err());
    assert_eq!(
        engine
            .torrent_network
            .resolve("fixture.test:80", deadline)
            .unwrap(),
        vec!["10.7.0.1:80".parse::<SocketAddr>().unwrap()]
    );
}

/// Retained, partially selected torrent shared by the native session fixtures.
/// Only selected.bin exists: expanding the selection or acquiring the other
/// file is observable independently of the serialized control comparison.
pub(crate) fn retained_engine_session_change(
    driver: HttpTunnel,
    replace: bool,
    change: impl FnOnce(&Arc<Engine>),
) {
    use crate::{
        bencode::{self, Value as B},
        torrent::{Client, FilePriority, TransferPolicy},
    };
    use std::collections::BTreeMap;
    let directory = Directory::new();
    let mut config = directory.config();
    config.downloads.listen_port = 7001;
    let payload: Vec<u8> = (0..32_768).map(|n| (n % 251) as u8).collect();
    let missing = vec![9_u8; 16_384];
    let pieces = payload
        .chunks(16_384)
        .chain(missing.chunks(16_384))
        .flat_map(crate::crypto::sha1)
        .collect();
    let file = |name: &[u8], size: usize| {
        B::Dict(BTreeMap::from([
            (b"length".to_vec(), B::Int(size as i64)),
            (b"path".to_vec(), B::List(vec![B::Bytes(name.to_vec())])),
        ]))
    };
    let info = B::Dict(BTreeMap::from([
        (b"name".to_vec(), B::Bytes(b"retained".to_vec())),
        (
            b"files".to_vec(),
            B::List(vec![
                file(b"selected.bin", payload.len()),
                file(b"unselected.bin", missing.len()),
            ]),
        ),
        (b"piece length".to_vec(), B::Int(16_384)),
        (b"pieces".to_vec(), B::Bytes(pieces)),
        (b"private".to_vec(), B::Int(1)),
    ]));
    let hash: String = crate::crypto::sha1(&bencode::encode(&info))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let source = directory.0.join("retained.torrent");
    fs::write(
        &source,
        bencode::encode(&B::Dict(BTreeMap::from([(b"info".to_vec(), info)]))),
    )
    .unwrap();
    let base = config.downloads.data_dir.join(&hash).join("retained");
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("selected.bin"), &payload).unwrap();
    let network = driver
        .network(Instant::now() + Duration::from_secs(5))
        .unwrap();
    let client = Client::open_with_policy_and_admission(
        config.downloads.clone(),
        TransferPolicy::default(),
        Arc::new(|_| Ok(true)),
        network,
    )
    .unwrap();
    client
        .ensure_files(source.to_str().unwrap(), &["retained/selected.bin".into()])
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !client.check(&hash).unwrap().selected_ready {
        assert!(
            Instant::now() < deadline,
            "selected retained bytes were not verified"
        );
        thread::sleep(Duration::from_millis(5));
    }
    client.pause(&hash).unwrap();
    client.set_priority(&hash, 7).unwrap();
    client
        .set_file_priority(&hash, 0, FilePriority::Low)
        .unwrap();
    let before = client.transfer(&hash).unwrap();
    assert_eq!(
        before.get("verified_bytes").and_then(Value::as_str),
        Some("32768")
    );
    drop(client);

    let operator = native_operator(&directory, driver);
    let mut engine =
        Engine::open_operator(config.clone(), true, false, true, Some(operator.clone())).unwrap();
    let startup = |engine: &Engine| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !engine.startup_step().unwrap() {
            assert!(
                Instant::now() < deadline,
                "retained engine startup timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
    };
    let check = |engine: &Engine| {
        let after = engine.transfer(&hash).unwrap();
        for key in [
            "user_paused",
            "priority",
            "file_priorities",
            "file_selection",
        ] {
            assert_eq!(
                after.get(key),
                before.get(key),
                "changed retained control: {key}"
            );
        }
        assert_eq!(after.get("user_paused"), Some(&Value::Bool(true)));
        assert_eq!(after.get("failed"), Some(&Value::Bool(false)));
        assert_eq!(after.get("running"), Some(&Value::Bool(false)));
        let files = after.get("files").unwrap().as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(after.get("priority"), Some(&Value::Number(7.0)));
        assert!(
            engine
                .torrent_network
                .listen("0.0.0.0:7001".parse().unwrap())
                .is_err(),
            "the engine did not exclusively own its retained listener port"
        );
        assert_eq!(files[0].get("selected"), Some(&Value::Bool(true)));
        assert_eq!(files[1].get("selected"), Some(&Value::Bool(false)));
        assert_eq!(fs::read(base.join("selected.bin")).unwrap(), payload);
        assert!(!base.join("unselected.bin").exists());
        assert_eq!(state(&engine.vpn_status()), "protected");
        assert_eq!(
            engine.vpn_status().get("complete_isolation_verified"),
            Some(&Value::Bool(false))
        );
    };
    startup(&engine);
    check(&engine);
    let old = engine.torrent_network.clone();
    let generation = old.generation().unwrap();
    change(&engine);
    assert_eq!(engine.vpn_needs_reload(), replace);
    if replace {
        assert!(old.publication().is_err());
        engine.retire_vpn_generation();
        drop(engine);
        engine = Engine::open_operator(config.clone(), true, false, true, Some(operator.clone()))
            .unwrap();
        startup(&engine);
        assert_ne!(engine.torrent_network.generation().unwrap(), generation);
        assert!(old.publication().is_err());
        assert!(
            old.resolve("stale.example:80", Instant::now() + Duration::from_secs(1))
                .is_err()
        );
    } else {
        assert_eq!(engine.torrent_network.generation().unwrap(), generation);
        assert!(old.publication().is_ok());
    }
    check(&engine);
    // Drop the native client and reopen its durable index/control files too.
    engine.retire_vpn_generation();
    drop(engine);
    let engine = Engine::open_operator(config, true, false, true, Some(operator)).unwrap();
    startup(&engine);
    check(&engine);
}

fn wait(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "condition not reached");
        thread::sleep(Duration::from_millis(10));
    }
}
/// The port peers and trackers are told, from the public engine status.
fn peer_port(engine: &Engine) -> u16 {
    match engine.status().unwrap().get("peer_port") {
        Some(Value::Number(port)) => *port as u16,
        other => panic!("no peer port: {other:?}"),
    }
}
fn forwarding(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}

#[test]
fn forwarding_intent_takes_the_profile_dns_as_gateway_and_survives_other_controls() {
    let directory = Directory::new();
    let config = directory.config();
    let engine = Engine::open_for_management(config.clone()).unwrap();
    assert!(engine.vpn_control(&body("enable-forwarding")).is_err());
    engine.vpn_control(&import()).unwrap();
    drop(engine);
    for (action, enabled) in [
        ("enable-forwarding", true),
        ("revoke", true),
        ("enable", true),
        ("disable-forwarding", false),
        ("enable-forwarding", true),
    ] {
        let engine = Engine::open_for_management(config.clone()).unwrap();
        let report = engine.vpn_control(&body(action)).unwrap();
        assert_redacted(&report);
        drop(engine);
        let text = profile::read_private_text(&stored(&config)).unwrap();
        let intent = Intent::decode(&text).unwrap();
        assert_eq!(intent.forwarding, enabled, "{action}");
        let operator = Operator::load(&config.store_dir, false);
        let gateway = operator.state.lock().unwrap().gateway;
        assert_eq!(gateway, enabled.then(|| Ipv4Addr::new(10, 23, 0, 1)));
    }
    // Without the running service the lease is not attempted; forwarding is
    // reported apart from the tunnel state and never names the gateway.
    let engine = Engine::open_for_management(config.clone()).unwrap();
    let status = engine.vpn_status();
    assert_eq!(state(&status), "blocked");
    assert_eq!(
        forwarding(&status, "forwarding").as_deref(),
        Some("acquiring")
    );
    assert_redacted(&status);
    // A forwarding line without a profile is not a valid intent.
    assert!(Intent::decode("# mynou-vpn-v1 disabled\n# mynou-forwarding natpmp\n").is_err());
}

#[test]
fn forwarded_port_is_listened_on_in_the_tunnel_then_renewed_changed_and_lost() {
    let directory = Directory::new();
    let pair = crate::tunnel_http::tests::Pair::new();
    let gateway = crate::tunnel_http::tests::NatPmp::start(&pair.server, 53186);
    let config = directory.config();
    let engine = Engine::open_for_management(config.clone()).unwrap();
    engine.vpn_control(&import()).unwrap();
    drop(engine);
    let engine = Engine::open_for_management(config.clone()).unwrap();
    engine.vpn_control(&body("enable-forwarding")).unwrap();
    drop(engine);
    let operator = Operator::load(&config.store_dir, false);
    {
        // The fixture's tunnel gateway and DNS server is 10.7.0.1.
        let mut state = operator.state.lock().unwrap();
        state.gateway = Some(Ipv4Addr::new(10, 7, 0, 1));
        state.driver = Some(pair.client.clone());
    }
    let open = || {
        let engine =
            Engine::open_operator(config.clone(), true, false, true, Some(operator.clone()))
                .unwrap();
        while !engine.startup_step().unwrap() {}
        engine
    };
    let inbound = |port: u16| {
        pair.server_network
            .connect(
                SocketAddr::from(([10, 7, 0, 2], port)),
                Duration::from_secs(2),
            )
            .is_ok()
    };

    // The listener opens on the granted port before anything announces it.
    let engine = open();
    let status = engine.vpn_status();
    assert_eq!(state(&status), "protected");
    assert_eq!(forwarding(&status, "forwarding").as_deref(), Some("active"));
    assert_eq!(
        forwarding(&status, "forwarded_port").as_deref(),
        Some("53186")
    );
    assert_redacted(&status);
    assert_eq!(peer_port(&engine), 53186);
    assert!(inbound(53186));

    // Renewals keep the port and the engine generation.
    let requests = gateway.requests.load(Ordering::Relaxed);
    wait(|| gateway.requests.load(Ordering::Relaxed) >= requests + 4);
    assert!(!engine.vpn_needs_reload());

    // A changed port retires this generation; its successor listens there.
    gateway.port.store(53190, Ordering::Release);
    wait(|| engine.vpn_needs_reload());
    assert_eq!(
        forwarding(&engine.vpn_status(), "forwarded_port").as_deref(),
        Some("53190")
    );
    engine.retire_vpn_generation();
    drop(engine);
    let engine = open();
    assert_eq!(peer_port(&engine), 53190);
    assert!(inbound(53190));
    assert!(!inbound(53186), "a stale forwarded listener remained");

    // A refused renewal loses the port at expiry; the tunnel stays protected.
    gateway.port.store(0, Ordering::Release);
    wait(|| engine.vpn_needs_reload());
    let refused = Some("Gateway refused forwarding; use a P2P server and profile".to_owned());
    wait(|| forwarding(&engine.vpn_status(), "forwarding_reason") == refused);
    let status = engine.vpn_status();
    assert_eq!(state(&status), "protected");
    assert_eq!(
        forwarding(&status, "forwarding").as_deref(),
        Some("unavailable")
    );
    assert_eq!(forwarding(&status, "forwarded_port"), None);
    engine.retire_vpn_generation();
    drop(engine);

    // Its successor keeps outgoing operation on an ordinary virtual port.
    let engine = open();
    let port = peer_port(&engine);
    assert_ne!(port, 53190);
    assert!(!inbound(53190));
    assert_eq!(state(&engine.vpn_status()), "protected");
    assert!(!engine.vpn_needs_reload());
    drop(engine);
    drop(gateway);
}

/// Native load fixtures use a synthetic profile and the existing loopback owner.
pub(crate) fn load_operator(config: &crate::config::Config, driver: HttpTunnel) -> Arc<Operator> {
    let engine = Engine::open_for_management(config.clone()).unwrap();
    engine.vpn_control(&import()).unwrap();
    drop(engine);
    let engine = Engine::open_for_management(config.clone()).unwrap();
    engine.vpn_control(&body("enable-forwarding")).unwrap();
    drop(engine);
    let operator = Operator::load(&config.store_dir, false);
    {
        let mut state = operator.state.lock().unwrap();
        state.gateway = Some(Ipv4Addr::new(10, 7, 0, 1));
        state.driver = Some(driver);
    }
    operator
}
