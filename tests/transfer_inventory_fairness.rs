//! Retained queue windows must make progress past ineligible or damaged heads.
use mynou::torrent::{Client, DownloadConfig, TorrentControl, TransferPolicy};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

struct Fixture {
    root: PathBuf,
    config: DownloadConfig,
}
impl Fixture {
    fn new() -> Self {
        let nonce: [u8; 16] = mynou::crypto::random_bytes().unwrap();
        let root = std::env::temp_dir().join(format!("mynou-transfer-fairness-{nonce:02x?}"));
        let config = DownloadConfig {
            data_dir: root.join("data"),
            state_dir: root.join("state"),
            listen_port: 0,
            seed: false,
            dht: false,
            pex: false,
            max_active: 1,
            max_peers: 1,
        };
        fs::create_dir_all(&config.state_dir).unwrap();
        Self { root, config }
    }
    fn retain(&self, count: usize) -> String {
        for n in 1..=count {
            let id = format!("{n:040x}");
            for (suffix, bytes) in [
                ("source", format!("magnet:?xt=urn:btih:{id}").into_bytes()),
                (
                    "control",
                    TorrentControl::new(id.clone(), n as u64)
                        .unwrap()
                        .encode()
                        .unwrap(),
                ),
            ] {
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                options
                    .open(self.config.state_dir.join(format!("{id}.{suffix}")))
                    .unwrap()
                    .write_all(&bytes)
                    .unwrap();
            }
        }
        format!("{count:040x}")
    }
    fn wait_for_worker(&self, id: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !self.config.data_dir.join(id).is_dir() {
            assert!(
                Instant::now() < deadline,
                "eligible tail never received a worker beyond the blocked queue window"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn admission_denied_head_windows_do_not_starve_an_eligible_tail() {
    let fixture = Fixture::new();
    let tail = fixture.retain(100);
    let admitted = tail.clone();
    let client = Client::open_with_policy_and_admission(
        fixture.config.clone(),
        TransferPolicy::default(),
        Arc::new(move |id| Ok(id == admitted)),
        mynou::torrent::transport::direct(),
    )
    .unwrap();
    // The worker creates this directory. Reading a point status here would
    // hydrate the tail itself and hide a queue traversal regression.
    fixture.wait_for_worker(&tail);
    assert!(!fixture.config.data_dir.join(format!("{:040x}", 1)).exists());
    drop(client);
}

#[test]
fn damaged_cold_heads_are_quarantined_without_starving_a_healthy_tail() {
    let fixture = Fixture::new();
    let tail = fixture.retain(100);
    let client = Client::open_with_policy_and_admission(
        fixture.config.clone(),
        TransferPolicy::default(),
        Arc::new(|_| Ok(false)),
        mynou::torrent::transport::direct(),
    )
    .unwrap();
    drop(client);
    // Damage only the mutable legacy point files after catalog publication.
    // The authoritative descriptors remain valid and therefore can record
    // failed hydration while keeping the following queue entries reachable.
    for n in 1..=80 {
        fs::write(
            fixture.config.state_dir.join(format!("{n:040x}.source")),
            b"invalid synthetic source",
        )
        .unwrap();
    }
    let admitted = tail.clone();
    let client = Client::open_with_policy_and_admission(
        fixture.config.clone(),
        TransferPolicy::default(),
        Arc::new(move |id| Ok(id == admitted)),
        mynou::torrent::transport::direct(),
    )
    .unwrap();
    fixture.wait_for_worker(&tail);
    let (statuses, _) = client.statuses_page(None, 1).unwrap();
    assert!(
        statuses[0].message == "Missing persisted metadata for this source",
        "damaged queue head must retain an explicit failure: {}",
        statuses[0].message
    );
    let mut after = None;
    let mut listed = std::collections::BTreeSet::new();
    loop {
        let page = client.transfers_page(7, after.as_deref()).unwrap();
        for row in page.get("transfers").unwrap().as_array().unwrap() {
            let id = row.get("id").unwrap().as_str().unwrap();
            assert!(listed.insert(id.to_owned()));
            if id == format!("{:040x}", 1) {
                assert_eq!(
                    row.get("failed").and_then(mynou::json::Value::as_bool),
                    Some(true)
                );
                assert_eq!(
                    row.get("ready").and_then(mynou::json::Value::as_bool),
                    Some(false)
                );
            }
        }
        after = page
            .get("next")
            .and_then(mynou::json::Value::as_str)
            .map(str::to_owned);
        if after.is_none() {
            break;
        }
        assert!(listed.len() < 100);
    }
    assert_eq!(
        listed.len(),
        100,
        "damaged rows must not hide later cursor pages"
    );
    assert!(client.check(&format!("{:040x}", 1)).is_err());
    drop(client);
}
