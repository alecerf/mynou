use crate::{config, engine::Engine, json, server::route};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
/// Loading settings replaces the process-wide secrets; these tests take turns.
static SECRETS: Mutex<()> = Mutex::new(());

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "mynou-settings-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Opens the saved settings without binding the default BitTorrent port.
fn open(dir: &Directory) -> std::sync::Arc<Engine> {
    let mut config = config::load(&dir.0).unwrap();
    config.downloads_enabled = false;
    Engine::open(config).unwrap()
}

fn call(engine: &std::sync::Arc<Engine>, method: &str, path: &str, body: &str) -> json::Value {
    // Reports may carry secret names: keep them out of panic messages.
    let Ok((200..=299, value)) = route(engine, method, path, body.as_bytes()) else {
        panic!("{method} {path} failed");
    };
    value
}

#[test]
fn saved_settings_are_validated_and_survive_reopening() {
    let _turn = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Directory::new();
    let engine = open(&dir);
    // Folders show as full paths, under the data directory by default.
    let defaults = super::full_folders(config::default_json(), &engine.config.data_dir).unwrap();
    let movies = engine.config.data_dir.join("library/movies");
    assert_eq!(
        defaults.get("library").and_then(|l| l.get("movies_root")),
        Some(&json::Value::String(movies.to_string_lossy().into_owned()))
    );
    let report = call(&engine, "GET", "/api/settings", "");
    assert_eq!(report.get("settings"), Some(&defaults));
    assert_eq!(report.get("applying"), Some(&json::Value::Bool(false)));
    // Invalid documents are refused and change nothing.
    for body in [
        r#"{"settings":{"schema_version":1,"bogus":true}}"#,
        r#"{"settings":[]}"#,
    ] {
        assert!(route(&engine, "POST", "/api/settings", body.as_bytes()).is_err());
    }
    assert_eq!(
        call(&engine, "GET", "/api/settings", "").get("settings"),
        Some(&defaults)
    );
    let mut document = config::default_json();
    document.insert("workers", 3_u32);
    // A folder must not be a file.
    let mut library = json::Value::object();
    library.insert("movies_root", "~");
    library.insert("series_root", "/etc/hosts");
    let mut file = document.clone();
    file.insert("library", library);
    let mut body = json::Value::object();
    body.insert("settings", file);
    assert!(
        route(
            &engine,
            "POST",
            "/api/settings",
            json::stringify(&body).as_bytes()
        )
        .is_err()
    );
    let mut body = json::Value::object();
    body.insert("settings", document.clone());
    let reply = call(&engine, "POST", "/api/settings", &json::stringify(&body));
    assert_eq!(
        reply.get("state").and_then(json::Value::as_str),
        Some("saved")
    );
    drop(engine);
    let config = config::load(&dir.0).unwrap();
    assert_eq!(config.workers, 3);
    // Saved with full folder paths.
    assert_eq!(
        config.document,
        super::full_folders(document, &config.data_dir).unwrap()
    );
    assert_eq!(config.store_dir, dir.0.join("state/jobs"));
}

#[test]
fn secrets_are_write_only_and_apply_at_once() {
    let _turn = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Directory::new();
    let engine = open(&dir);
    let name = "MYNOU_SETTINGS_TEST_TOKEN";
    let saved = call(
        &engine,
        "POST",
        "/api/secrets",
        &format!(r#"{{"name":"{name}","value":"value-never-returned"}}"#),
    );
    let text = json::stringify(&saved);
    assert!(text.contains(name) && text.contains("stored"));
    assert!(!text.contains("value-never-returned"));
    assert!(!json::stringify(&call(&engine, "GET", "/api/settings", "")).contains("value-never"));
    assert_eq!(config::secret(name).unwrap(), "value-never-returned");
    for body in [
        r#"{"name":"lower","value":"x"}"#,
        r#"{"name":"MYNOU_X","value":""}"#,
        r#"{"name":"MYNOU_X","value":"a\nb"}"#,
        r#"{"name":"MYNOU_X","value":"x","extra":1}"#,
    ] {
        assert!(route(&engine, "POST", "/api/secrets", body.as_bytes()).is_err());
    }
    drop(engine);
    config::install_secrets(Default::default());
    // Saved secrets load with the settings.
    config::load(&dir.0).unwrap();
    assert_eq!(config::secret(name).unwrap(), "value-never-returned");
    let engine = open(&dir);
    call(
        &engine,
        "POST",
        "/api/secrets",
        &format!(r#"{{"name":"{name}","value":null}}"#),
    );
    assert!(config::secret(name).is_err());
    assert!(json::stringify(&call(&engine, "GET", "/api/secrets", "")).contains("missing"));
}

#[test]
fn updates_are_automatic_by_default_and_only_a_staged_one_installs() {
    let _turn = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Directory::new();
    let engine = open(&dir);
    assert!(engine.config.updates_automatic);
    let mut document = config::default_json();
    document.insert("updates", json::parse(r#"{"automatic":false}"#).unwrap());
    assert!(
        !config::from_json(&document, &dir.0)
            .unwrap()
            .updates_automatic
    );
    let report = call(&engine, "GET", "/api/update", "");
    assert_eq!(
        report.get("version").and_then(json::Value::as_str),
        Some(env!("CARGO_PKG_VERSION"))
    );
    // A development build never replaces itself.
    assert!(
        report
            .get("eligible")
            .and_then(json::Value::as_str)
            .is_some_and(|reason| reason.contains("published release"))
    );
    let (status, _) = route(&engine, "POST", "/api/update/install", b"").unwrap();
    assert_eq!(status, 409);
    assert!(!engine.stopped.load(Ordering::Acquire));
}
