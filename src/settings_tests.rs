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
    let (status, value) = route(engine, method, path, body.as_bytes()).unwrap();
    assert!((200..300).contains(&status), "{status}");
    value
}

#[test]
fn saved_settings_are_validated_and_survive_reopening() {
    let _turn = SECRETS.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Directory::new();
    let engine = open(&dir);
    let report = call(&engine, "GET", "/api/settings", "");
    assert_eq!(report.get("settings"), Some(&config::default_json()));
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
        Some(&config::default_json())
    );
    let mut document = config::default_json();
    document.insert("workers", 3_u32);
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
    assert_eq!(config.document, document);
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
        r#"{"name":"MYNOU_API_TOKEN","value":"x"}"#,
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
