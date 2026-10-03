//! Monitoring and Plex mappings remain opt-in, bounded and backward compatible.
use mynou::config::{self, Monitoring};
use mynou::json::{self, Value};
use mynou::selection::Profile;
use std::path::Path;

fn document(text: &str) -> Value {
    json::parse(text).unwrap()
}

fn configured_monitoring(monitoring: Value) -> Value {
    let mut value = config::default_json();
    value.insert("monitoring", monitoring);
    value
}

fn configured_mappings(mappings: Value) -> Value {
    let mut value = config::default_json();
    value
        .get_mut("plex")
        .unwrap()
        .insert("path_mappings", mappings);
    value
}

fn mapping(mynou_prefix: &str, plex_prefix: &str) -> Value {
    let mut value = Value::object();
    value.insert("mynou_prefix", mynou_prefix);
    value.insert("plex_prefix", plex_prefix);
    value
}

#[test]
fn monitoring_defaults_are_explicit_and_legacy_configuration_is_disabled() {
    let mut value = config::default_json();
    let configuration = config::from_json(&value, Path::new(".")).unwrap();
    assert_eq!(configuration.monitoring, Monitoring::default());
    assert_eq!(
        value.get("monitoring").unwrap(),
        &document(r#"{"enabled":false,"interval_secs":3600,"max_checks":32}"#)
    );
    assert!(configuration.plex.path_mappings.is_empty());

    let Value::Object(fields) = &mut value else {
        panic!("configuration object");
    };
    fields.remove("monitoring");
    let Value::Object(plex) = fields.get_mut("plex").unwrap() else {
        panic!("Plex object");
    };
    plex.remove("path_mappings");
    let configuration = config::from_json(&value, Path::new(".")).unwrap();
    assert_eq!(configuration.monitoring, Monitoring::default());
    assert!(configuration.plex.path_mappings.is_empty());

    let empty = configured_monitoring(Value::object());
    assert_eq!(
        config::from_json(&empty, Path::new(".")).unwrap().monitoring,
        Monitoring::default()
    );
}

#[test]
fn monitoring_bounds_include_both_endpoints() {
    for (interval_secs, max_checks) in [(60_u32, 1_u32), (86400, 256)] {
        let mut monitoring = Value::object();
        monitoring.insert("enabled", true);
        monitoring.insert("interval_secs", interval_secs);
        monitoring.insert("max_checks", max_checks);
        let configuration =
            config::from_json(&configured_monitoring(monitoring), Path::new(".")).unwrap();
        assert!(configuration.monitoring.enabled);
        assert_eq!(configuration.monitoring.interval_secs, u64::from(interval_secs));
        assert_eq!(configuration.monitoring.max_checks, max_checks as usize);
    }
}

#[test]
fn malformed_monitoring_never_silently_enables_or_defaults() {
    for monitoring in [
        "null",
        "true",
        "[]",
        r#"{"enabled":"true"}"#,
        r#"{"enabled":null}"#,
        r#"{"interval_secs":0}"#,
        r#"{"interval_secs":59}"#,
        r#"{"interval_secs":86401}"#,
        r#"{"interval_secs":60.5}"#,
        r#"{"max_checks":0}"#,
        r#"{"max_checks":257}"#,
        r#"{"max_checks":-1}"#,
        r#"{"max_checks":null}"#,
        r#"{"interval_seconds":3600}"#,
    ] {
        assert!(
            config::from_json(&configured_monitoring(document(monitoring)), Path::new("."))
                .is_err(),
            "invalid monitoring accepted: {monitoring}"
        );
    }
}

#[test]
fn resolution_cutoff_requires_an_explicit_ordered_profile_member() {
    for profile in [
        r#"{"cutoff_resolution":1080}"#,
        r#"{"resolutions":[],"cutoff_resolution":1080}"#,
        r#"{"resolutions":[720],"cutoff_resolution":1080}"#,
        r#"{"resolutions":[1080],"cutoff_resolution":1440}"#,
        r#"{"resolutions":[1080],"cutoff_resolution":"1080"}"#,
        r#"{"resolutions":[1080],"cutoff_resolution":true}"#,
        r#"{"resolutions":[1080],"cutoff_resolution":1080.5}"#,
        r#"{"resolutions":[1080],"cutoff_resolution":-1080}"#,
    ] {
        assert!(
            Profile::from_json(&document(profile)).is_err(),
            "invalid cutoff accepted: {profile}"
        );
    }
    for profile in [r#"{}"#, r#"{"cutoff_resolution":null}"#] {
        let profile = Profile::from_json(&document(profile)).unwrap();
        assert_eq!(profile.cutoff_resolution, None);
        assert_eq!(profile.to_json().get("cutoff_resolution"), Some(&Value::Null));
        assert!(!profile.cutoff_reached(&profile.assess("Example 1080p", "Example")));
    }
}

#[test]
fn cutoff_uses_preference_order_instead_of_numeric_resolution() {
    for (resolutions, cutoff, reached, unfinished) in [
        ("[1080,720]", 1080, 1080, 720),
        ("[720,1080]", 720, 720, 1080),
        ("[720,1080,2160]", 1080, 720, 2160),
    ] {
        let profile = Profile::from_json(&document(&format!(
            "{{\"resolutions\":{resolutions},\"cutoff_resolution\":{cutoff}}}"
        )))
        .unwrap();
        let satisfied = profile.assess(&format!("Example 2026 {reached}p"), "Example");
        let unfinished = profile.assess(&format!("Example 2026 {unfinished}p"), "Example");
        assert!(satisfied.accepted && unfinished.accepted);
        assert!(profile.cutoff_reached(&satisfied));
        assert!(!profile.cutoff_reached(&unfinished));
        assert_eq!(
            Profile::from_json(&profile.to_json()).unwrap(),
            profile,
            "cutoff must survive configuration serialization"
        );
    }
}

#[test]
fn rejected_or_unknown_resolution_never_reaches_cutoff() {
    let profile = Profile::from_json(&document(
        r#"{"resolutions":[1080,720],"cutoff_resolution":720,"allow_unknown_resolution":true,"blocked_terms":["cam"]}"#,
    ))
    .unwrap();
    let rejected = profile.assess("Example 1080p CAM", "Example");
    assert!(!rejected.accepted);
    assert!(!profile.cutoff_reached(&rejected));
    let unknown = profile.assess("Example 2026", "Example");
    assert!(unknown.accepted);
    assert!(!profile.cutoff_reached(&unknown));
}

#[test]
fn unknown_profile_references_stay_invalid_with_monitoring_enabled() {
    for kind in ["movie_profile", "episode_profile"] {
        let mut value = configured_monitoring(document(r#"{"enabled":true}"#));
        value
            .get_mut("selection")
            .unwrap()
            .insert(kind, "not-configured");
        assert!(config::from_json(&value, Path::new(".")).is_err());
    }
}

#[test]
fn mappings_accept_root_and_overlapping_normalized_prefixes() {
    let entries = Value::Array(vec![
        mapping("/", "/root"),
        mapping("/library", "/media"),
        mapping("/library/movies", "/media/movies"),
        mapping("/library/series", "/media"),
        mapping("/space in filename", "/"),
    ]);
    let value = configured_mappings(entries);
    let configuration = config::from_json(&value, Path::new(".")).unwrap();
    assert_eq!(configuration.plex.path_mappings.len(), 5);
    assert_eq!(configuration.plex.path_mappings[0].mynou_prefix, "/");
    assert_eq!(configuration.plex.path_mappings[4].plex_prefix, "/");
    let value = json::parse(&json::stringify(&value)).unwrap();
    assert_eq!(
        config::from_json(&value, Path::new("."))
            .unwrap()
            .plex
            .path_mappings,
        configuration.plex.path_mappings
    );
}

#[test]
fn mappings_reject_ambiguous_or_non_absolute_paths_without_trimming() {
    for path in [
        "",
        "library/movies",
        " /library",
        "//library",
        "/library/",
        "/library//movies",
        "/./library",
        "/library/.",
        "/library/../movies",
        "/library/..",
        "/library\\movies",
        "/library\nmovies",
        "/library\0movies",
        "/library\u{85}movies",
    ] {
        for entry in [mapping(path, "/media"), mapping("/library", path)] {
            let value = configured_mappings(Value::Array(vec![entry]));
            assert!(
                config::from_json(&value, Path::new(".")).is_err(),
                "invalid mapping path accepted: {path:?}"
            );
        }
    }
}

#[test]
fn mapping_sizes_and_duplicate_prefixes_are_bounded() {
    let longest = format!("/{}", "a".repeat(4095));
    let value = configured_mappings(Value::Array(vec![mapping(&longest, "/media")]));
    assert!(config::from_json(&value, Path::new(".")).is_ok());
    let too_long = format!("{longest}a");
    for entry in [mapping(&too_long, "/media"), mapping("/library", &too_long)] {
        assert!(
            config::from_json(
                &configured_mappings(Value::Array(vec![entry])),
                Path::new(".")
            )
            .is_err()
        );
    }
    let entries: Vec<_> = (0..32)
        .map(|index| mapping(&format!("/library/{index}"), "/media"))
        .collect();
    let value = configured_mappings(Value::Array(entries.clone()));
    assert!(config::from_json(&value, Path::new(".")).is_ok());
    let mut too_many = entries;
    too_many.push(mapping("/library/32", "/media"));
    assert!(
        config::from_json(&configured_mappings(Value::Array(too_many)), Path::new("."))
            .is_err()
    );
    let duplicate = Value::Array(vec![
        mapping("/library", "/media"),
        mapping("/library", "/another-media-root"),
    ]);
    assert!(config::from_json(&configured_mappings(duplicate), Path::new(".")).is_err());
}

#[test]
fn malformed_mapping_fields_are_rejected() {
    for entries in [
        "null",
        "{}",
        "[null]",
        "[true]",
        r#"[{"mynou_prefix":"/library"}]"#,
        r#"[{"mynou_prefix":false,"plex_prefix":"/media"}]"#,
        r#"[{"mynou_prefix":"/library","plex_prefix":null}]"#,
        r#"[{"mynou_prefix":"/library","plex_prefix":"/media","extra":true}]"#,
    ] {
        assert!(
            config::from_json(&configured_mappings(document(entries)), Path::new("."))
                .is_err(),
            "invalid mapping accepted: {entries}"
        );
    }
}
