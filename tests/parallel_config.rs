//! Parallel peer configuration rejects invalid bounds before native side effects.
use mynou::config;
use mynou::json::{self, Value};
use mynou::torrent::Client;
use std::path::Path;

#[test]
fn old_configuration_uses_fifty_peer_slots_without_changing_transfer_policy() {
    let mut value = config::default_json();
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads object");
    };
    downloads.remove("max_peers");
    let configuration = config::from_json(&value, Path::new(".")).unwrap();
    assert_eq!(configuration.downloads.max_peers, 50);
    assert_eq!(configuration.downloads.max_active, 2);
    assert_eq!(configuration.download_policy.download_limit_bps, 0);
}

#[test]
fn peer_slots_accept_both_endpoints_and_remain_strict_when_disabled() {
    for limit in [1_u32, 50, 256] {
        let mut value = config::default_json();
        value
            .get_mut("downloads")
            .unwrap()
            .insert("max_peers", limit);
        let configuration = config::from_json(&value, Path::new(".")).unwrap();
        assert_eq!(configuration.downloads.max_peers, limit as usize);
    }
    for text in ["0", "257", "-1", "0.5", "null", "true", "[]", "{}", "\"4\""] {
        let mut value = config::default_json();
        let downloads = value.get_mut("downloads").unwrap();
        downloads.insert("enabled", false);
        downloads.insert("max_peers", json::parse(text).unwrap());
        assert!(
            config::from_json(&value, Path::new(".")).is_err(),
            "accepted {text}"
        );
    }
}

#[test]
fn native_peer_bounds_are_validated_before_creating_state_or_opening_ports() {
    let configuration = config::from_json(&config::default_json(), Path::new(".")).unwrap();
    for limit in [0, 257, usize::MAX] {
        let mut downloads = configuration.downloads.clone();
        // Invalid bounds must fail before inspecting an unavailable state root.
        downloads.state_dir = Path::new("/dev/null/mynou-invalid-peers").to_path_buf();
        downloads.data_dir = downloads.state_dir.join("downloads");
        downloads.max_peers = limit;
        let error = match Client::open(downloads) {
            Ok(_) => panic!("accepted invalid peer limit {limit}"),
            Err(error) => error,
        };
        assert!(error.to_ascii_lowercase().contains("peer"), "{error}");
    }
}

#[test]
fn independent_download_and_seed_queue_limits_accept_zero_and_keep_legacy_defaults() {
    let mut legacy = config::default_json();
    if let Value::Object(downloads) = legacy.get_mut("downloads").unwrap() {
        downloads.remove("max_seeding");
    }
    let cfg = config::from_json(&legacy, Path::new(".")).unwrap();
    assert_eq!(
        (cfg.downloads.max_active, cfg.downloads.max_seeding),
        (2, 0)
    );
    for (downloads, seeds) in [(0, 0), (0, 3), (2, 0), (3, 1), (1000, 500)] {
        let mut value = config::default_json();
        value
            .get_mut("downloads")
            .unwrap()
            .insert("max_active", downloads as u32);
        value
            .get_mut("downloads")
            .unwrap()
            .insert("max_seeding", seeds as u32);
        let cfg = config::from_json(&value, Path::new(".")).unwrap();
        assert_eq!(
            (cfg.downloads.max_active, cfg.downloads.max_seeding),
            (downloads, seeds)
        );
    }
    for name in ["max_active", "max_seeding"] {
        for text in [
            "-1",
            "0.5",
            "null",
            "true",
            "[]",
            "{}",
            "\"0\"",
            "4294967296",
        ] {
            let mut value = config::default_json();
            value
                .get_mut("downloads")
                .unwrap()
                .insert(name, json::parse(text).unwrap());
            assert!(
                config::from_json(&value, Path::new(".")).is_err(),
                "{name} accepted {text}"
            );
        }
    }
}
