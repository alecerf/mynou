//! Transfer policy settings are strict, bounded and absent in older configurations.
use mynou::config;
use mynou::json::{self, Value};
use mynou::torrent::TransferPolicy;
use std::path::Path;

const FIELDS: &[&str] = &[
    "download_limit_bps",
    "upload_limit_bps",
    "seed_ratio_milli",
    "seed_time_secs",
];

fn document(text: &str) -> Value {
    json::parse(text).unwrap()
}

fn configured(field: &str, setting: Value) -> Value {
    let mut value = config::default_json();
    value.get_mut("downloads").unwrap().insert(field, setting);
    value
}

fn policy(value: &Value) -> TransferPolicy {
    config::from_json(value, Path::new("."))
        .unwrap()
        .download_policy
}

#[test]
fn default_configuration_exposes_unlimited_transfer_policy() {
    let value = config::default_json();
    let configuration = config::from_json(&value, Path::new(".")).unwrap();
    assert_eq!(configuration.download_policy, TransferPolicy::default());
    let downloads = value.get("downloads").unwrap();
    for name in ["download_limit_bps", "upload_limit_bps"] {
        assert_eq!(downloads.get(name).unwrap().as_u64(), Some(0));
    }
    for name in ["seed_ratio_milli", "seed_time_secs"] {
        assert_eq!(downloads.get(name), Some(&Value::Null));
    }
    assert!(configuration.downloads.seed);
    assert_eq!(configuration.downloads.max_active, 2);
    assert_eq!(configuration.downloads.listen_port, 6881);
}

#[test]
fn legacy_download_configuration_keeps_original_defaults() {
    let mut value = config::default_json();
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads object");
    };
    for name in FIELDS {
        downloads.remove(*name);
    }
    let configuration = config::from_json(&value, Path::new(".")).unwrap();
    assert_eq!(configuration.download_policy, TransferPolicy::default());
    assert!(configuration.downloads_enabled);
    assert!(configuration.downloads.seed);
    assert!(configuration.downloads.dht);
    assert!(configuration.downloads.pex);
    assert_eq!(configuration.downloads.max_active, 2);
}

#[test]
fn bandwidth_limits_accept_zero_and_both_positive_endpoints() {
    for name in ["download_limit_bps", "upload_limit_bps"] {
        for limit in [0_u32, 1, 1_073_741_824] {
            let policy = policy(&configured(name, limit.into()));
            let actual = if name == "download_limit_bps" {
                policy.download_limit_bps
            } else {
                policy.upload_limit_bps
            };
            assert_eq!(actual, u64::from(limit));
        }
    }
}

#[test]
fn bandwidth_limits_reject_non_integer_types_and_values_over_bound() {
    for name in ["download_limit_bps", "upload_limit_bps"] {
        for setting in [
            "null",
            "false",
            "[]",
            "{}",
            r#""1""#,
            "-1",
            "0.5",
            "1073741825",
            "9007199254740991",
        ] {
            assert!(
                config::from_json(&configured(name, document(setting)), Path::new(".")).is_err(),
                "invalid {name} accepted: {setting}"
            );
        }
    }
}

#[test]
fn nullable_seeding_goals_accept_supported_bounds() {
    for (name, maximum) in [
        ("seed_ratio_milli", 1_000_000_u32),
        ("seed_time_secs", 315_360_000),
    ] {
        for limit in [1_u32, maximum] {
            let policy = policy(&configured(name, limit.into()));
            let actual = if name == "seed_ratio_milli" {
                policy.seed_ratio_milli.map(u64::from)
            } else {
                policy.seed_time_secs
            };
            assert_eq!(actual, Some(u64::from(limit)));
        }
        let policy = policy(&configured(name, Value::Null));
        let actual = if name == "seed_ratio_milli" {
            policy.seed_ratio_milli.map(u64::from)
        } else {
            policy.seed_time_secs
        };
        assert_eq!(actual, None);
    }
}

#[test]
fn seeding_goals_reject_zero_non_integer_types_and_values_over_bound() {
    for (name, over_bound) in [
        ("seed_ratio_milli", "1000001"),
        ("seed_time_secs", "315360001"),
    ] {
        for setting in [
            "0",
            "-1",
            "1.5",
            "true",
            "[]",
            "{}",
            r#""1000""#,
            over_bound,
        ] {
            assert!(
                config::from_json(&configured(name, document(setting)), Path::new(".")).is_err(),
                "invalid {name} accepted: {setting}"
            );
        }
    }
}

#[test]
fn configured_policy_survives_json_roundtrip_without_changing_native_settings() {
    let mut value = config::default_json();
    let downloads = value.get_mut("downloads").unwrap();
    downloads.insert("download_limit_bps", 2_000_000_u32);
    downloads.insert("upload_limit_bps", 250_000_u32);
    downloads.insert("seed_ratio_milli", 1500_u32);
    downloads.insert("seed_time_secs", 7200_u32);
    downloads.insert("seed", false);
    downloads.insert("max_active", 5_u32);
    let parsed = json::parse(&json::stringify(&value)).unwrap();
    let configuration = config::from_json(&parsed, Path::new(".")).unwrap();
    assert_eq!(
        configuration.download_policy,
        TransferPolicy {
            download_limit_bps: 2_000_000,
            upload_limit_bps: 250_000,
            seed_ratio_milli: Some(1500),
            seed_time_secs: Some(7200),
        }
    );
    assert!(!configuration.downloads.seed);
    assert_eq!(configuration.downloads.max_active, 5);
}

#[test]
fn policy_settings_remain_strict_even_when_downloads_are_disabled() {
    let mut value = configured("seed_ratio_milli", 0_u32.into());
    value.get_mut("downloads").unwrap().insert("enabled", false);
    assert!(config::from_json(&value, Path::new(".")).is_err());
    let mut value = configured("upload_limit_bps", 1_073_741_825_u32.into());
    value.get_mut("downloads").unwrap().insert("seed", false);
    assert!(config::from_json(&value, Path::new(".")).is_err());
}

#[test]
fn unknown_transfer_fields_and_top_level_policies_are_rejected() {
    for name in [
        "download_limit",
        "upload_limits_bps",
        "seed_ratio",
        "transfer_policy",
    ] {
        assert!(config::from_json(&configured(name, 1_u32.into()), Path::new(".")).is_err());
    }
    let mut value = config::default_json();
    value.insert("download_policy", TransferPolicy::default().to_json());
    assert!(config::from_json(&value, Path::new(".")).is_err());
}
