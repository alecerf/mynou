//! Startup failures after native transfers open; validation runs in CI.
use super::{Engine, lock};
use crate::{config, crypto::random_bytes, store::Request};
use std::{fs, path::PathBuf};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let suffix: String = random_bytes::<8>()
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self(std::env::temp_dir().join(format!("mynou-engine-startup-{suffix}")))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Native transfers on a peerless client (no DHT or PEX) with a loopback API.
fn config(directory: &Directory) -> crate::config::Config {
    let mut value = config::default_json();
    value.insert("listen", "127.0.0.1:0");
    let downloads = value.get_mut("downloads").unwrap();
    downloads.insert("enabled", true);
    downloads.insert("listen_port", 0_u32);
    downloads.insert("dht", false);
    downloads.insert("pex", false);
    config::from_json(&value, &directory.0).unwrap()
}

#[test]
fn a_failed_transfers_stage_leaves_no_native_client_running() {
    let directory = Directory::new();
    let config = config(&directory);
    let engine = Engine::open_for_management(config.clone()).unwrap();
    // A retried request records its previous transfer for retirement, which
    // only the last startup stage settles.
    let id = {
        let mut store = lock(&engine.store).unwrap();
        let mut job = store
            .submit(Request {
                kind: "movie".into(),
                title: "Synthetic retired movie".into(),
                year: 2026,
                season: 0,
                episode: 0,
                tmdb_id: Some(7),
                source_path: None,
                source_url: None,
                source_numbering: None,
            })
            .unwrap();
        job.state = "failed".into();
        job.last_error = Some("Media validation failed".into());
        job.download_id = Some("ab".repeat(20));
        store.update(job.clone()).unwrap();
        assert_eq!(store.retry(&job.id).unwrap().retiring_transfers.len(), 1);
        job.id
    };
    drop(engine);
    let key = crate::store::Store::record_key(&id).unwrap();
    assert!(crate::inventory::damage_record(&config.store_dir.join("jobs.idx"), &key) > 0);
    let engine = Engine::open_serving(config).unwrap();
    let error = loop {
        match engine.startup_step() {
            Ok(done) => assert!(!done, "startup ignored the damaged record"),
            Err(error) => break error,
        }
    };
    let status = engine.startup_status().unwrap();
    assert_eq!(
        status.get("stage").and_then(crate::json::Value::as_str),
        Some("transfers"),
        "{error}"
    );
    assert_eq!(engine.startup_state().unwrap(), "failed");
    // The client opened for that stage was dropped with the failure, so no
    // listener or transfer outlives a startup that reports `failed`.
    assert!(engine.downloads.get().is_none());
    assert!(engine.transfer(&"ab".repeat(20)).is_err());
}

#[test]
fn job_backlog_status_does_not_wait_for_held_storage() {
    use crate::json::Value;
    let directory = Directory::new();
    let engine = Engine::open_for_management(config(&directory)).unwrap();
    let inventory = |status: Value| {
        status
            .get("inventory")
            .unwrap()
            .get("jobs")
            .unwrap()
            .clone()
    };
    let value = inventory(engine.status().unwrap());
    assert_eq!(value.get("state"), Some(&Value::from("ready")));
    assert_eq!(value.get("due"), Some(&Value::from("0")));
    let guard = lock(&engine.store).unwrap();
    let value = inventory(engine.status().unwrap());
    assert_eq!(value.get("state"), Some(&Value::from("busy")));
    assert!(value.get("due").is_none());
    drop(guard);
    assert_eq!(
        inventory(engine.status().unwrap()).get("state"),
        Some(&Value::from("ready"))
    );
}
