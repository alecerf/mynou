//! Selection profiles are strict configuration data, including legacy defaults.
use mynou::config;
use mynou::json::{self, Value};
use mynou::selection::SelectionConfig;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mynou-selection-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn configuration(selection: Value) -> Value {
    let mut value = config::default_json();
    value.insert("selection", selection);
    value
}

fn selection() -> Value {
    SelectionConfig::default().to_json()
}

#[test]
fn default_configuration_includes_resolvable_profiles() {
    let value = config::default_json();
    let configuration = config::from_json(&value, Path::new(".")).unwrap();
    assert_eq!(
        value.get("selection").unwrap(),
        &configuration.selection.to_json()
    );
    for kind in ["movie", "episode"] {
        let (name, _) = configuration.selection.profile(kind).unwrap();
        assert!(!name.is_empty());
        assert!(
            value
                .get("selection")
                .unwrap()
                .get("profiles")
                .unwrap()
                .get(name)
                .is_some()
        );
    }
}

#[test]
fn legacy_configuration_without_selection_loads_default_profiles() {
    let directory = Directory::new();
    let mut value = config::default_json();
    let Value::Object(fields) = &mut value else {
        panic!("configuration object");
    };
    fields.remove("selection");
    let file = directory.0.join("mynou.json");
    fs::write(&file, json::stringify(&value)).unwrap();
    let configuration = config::load(&file).unwrap();
    assert_eq!(configuration.selection.to_json(), selection());
    assert_eq!(configuration.store_dir, directory.0.join("state/jobs"));
}

#[test]
fn named_profile_references_survive_json_serialization() {
    let mut selection = selection();
    let profile = selection
        .get("profiles")
        .unwrap()
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    let mut profiles = Value::object();
    profiles.insert("cinema", profile.clone());
    profiles.insert("television", profile);
    selection.insert("profiles", profiles);
    selection.insert("movie_profile", "cinema");
    selection.insert("episode_profile", "television");
    let document = json::stringify(&configuration(selection.clone()));
    let parsed = json::parse(&document).unwrap();
    let configuration = config::from_json(&parsed, Path::new(".")).unwrap();
    assert_eq!(configuration.selection.to_json(), selection);
    assert_eq!(
        configuration.selection.profile("movie").unwrap().0,
        "cinema"
    );
    assert_eq!(
        configuration.selection.profile("episode").unwrap().0,
        "television"
    );
}

#[test]
fn explicit_quality_profile_is_loaded_for_both_media_kinds() {
    let selection = json::parse(
        r#"{"profiles":{"hd":{"resolutions":[1080,720]}},"movie_profile":"hd","episode_profile":"hd"}"#,
    )
    .unwrap();
    let configuration = config::from_json(&configuration(selection), Path::new(".")).unwrap();
    let serialized = configuration.selection.to_json();
    let resolutions = serialized
        .get("profiles")
        .unwrap()
        .get("hd")
        .unwrap()
        .get("resolutions")
        .unwrap();
    assert_eq!(resolutions, &json::parse("[1080,720]").unwrap());
    for kind in ["movie", "episode"] {
        assert_eq!(configuration.selection.profile(kind).unwrap().0, "hd");
    }
}

#[test]
fn unknown_profile_references_are_rejected() {
    for field in ["movie_profile", "episode_profile"] {
        let mut selection = selection();
        selection.insert(field, "missing-profile");
        assert!(
            config::from_json(&configuration(selection), Path::new(".")).is_err(),
            "unknown reference in {field} must not use a fallback profile"
        );
    }
}

#[test]
fn unknown_selection_and_profile_fields_are_rejected() {
    let mut unknown_selection = selection();
    unknown_selection.insert("movie_proflie", "typo");
    assert!(config::from_json(&configuration(unknown_selection), Path::new(".")).is_err());

    let mut unknown_profile = selection();
    let Value::Object(profiles) = unknown_profile.get_mut("profiles").unwrap() else {
        panic!("profiles object");
    };
    profiles
        .values_mut()
        .next()
        .unwrap()
        .insert("unknown_setting", true);
    assert!(config::from_json(&configuration(unknown_profile), Path::new(".")).is_err());
}

#[test]
fn malformed_selection_sections_are_rejected_instead_of_defaulting() {
    for malformed in [
        Value::Null,
        Value::Bool(false),
        Value::String("default".into()),
        Value::Array(Vec::new()),
    ] {
        assert!(config::from_json(&configuration(malformed), Path::new(".")).is_err());
    }
    for malformed in [Value::Null, Value::Array(Vec::new()), Value::Bool(true)] {
        let mut selection = selection();
        selection.insert("profiles", malformed);
        assert!(config::from_json(&configuration(selection), Path::new(".")).is_err());
    }
    for field in ["movie_profile", "episode_profile"] {
        let mut selection = selection();
        selection.insert(field, Value::Bool(true));
        assert!(config::from_json(&configuration(selection), Path::new(".")).is_err());
    }
    for malformed in [Value::Null, Value::Array(Vec::new()), Value::Bool(true)] {
        let mut selection = selection();
        let Value::Object(profiles) = selection.get_mut("profiles").unwrap() else {
            panic!("profiles object");
        };
        *profiles.values_mut().next().unwrap() = malformed;
        assert!(config::from_json(&configuration(selection), Path::new(".")).is_err());
    }
}
