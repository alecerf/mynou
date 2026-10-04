//! Durable transfer controls use native local peers and synthetic payloads only.
mod transfer_support;

use mynou::config;
use mynou::engine::{Engine, lock};
use mynou::json::{self, Value};
use mynou::net::HttpClient;
use mynou::server::Api;
use mynou::store::Request;
use mynou::torrent::{Client, FilePriority, TorrentControl, TransferPolicy};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use transfer_support::*;

fn policy(text: &str) -> TransferPolicy {
    TransferPolicy::from_json(&json::parse(text).unwrap()).unwrap()
}

fn counter(snapshot: &Value, key: &str) -> u64 {
    let value = snapshot.get(key).unwrap().as_str().unwrap();
    assert!(value.bytes().all(|byte| byte.is_ascii_digit()));
    value.parse().unwrap()
}

fn ids(client: &Client) -> Vec<String> {
    client
        .transfers()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.get("id").unwrap().as_str().unwrap().to_owned())
        .collect()
}

fn redacted(snapshot: &Value) {
    let text = json::stringify(snapshot);
    for forbidden in [
        TOKEN,
        "synthetic-download-secret",
        "magnet:",
        "http://",
        "https://",
        "source_url",
        "acquisition_url",
    ] {
        assert!(
            !text.contains(forbidden),
            "Transfer snapshot contains {forbidden}"
        );
    }
}

#[test]
fn durable_user_pause_survives_ensure_automatic_resume_and_restart() {
    let scratch = Scratch::new();
    let cfg = client_config(&scratch.0.join("client"), false);
    let magnet =
        "magnet:?xt=urn:btih:ffffffffffffffffffffffffffffffffffffffff&dn=synthetic-download-secret";
    let client = Client::open(cfg.clone()).unwrap();
    let id = client.ensure(magnet).unwrap().id;
    client.pause(&id).unwrap();
    wait(|| {
        !client
            .transfer(&id)
            .unwrap()
            .get("running")
            .unwrap()
            .as_bool()
            .unwrap()
    });
    for _ in 0..3 {
        client.ensure(magnet).unwrap();
        client.resume_if_allowed(&id).unwrap();
        assert_eq!(
            client.transfer(&id).unwrap().get("user_paused"),
            Some(&Value::Bool(true))
        );
    }
    let before = client.transfer(&id).unwrap();
    assert_eq!(counter(&before, "downloaded_bytes"), 0);
    redacted(&before);
    drop(client);
    let resumed = Client::open(cfg).unwrap();
    let snapshot = resumed.transfer(&id).unwrap();
    assert_eq!(snapshot.get("user_paused"), Some(&Value::Bool(true)));
    assert_eq!(snapshot.get("running"), Some(&Value::Bool(false)));
    resumed.resume_if_allowed(&id).unwrap();
    assert_eq!(
        resumed.transfer(&id).unwrap().get("user_paused"),
        Some(&Value::Bool(true))
    );
    resumed.resume(&id).unwrap();
    assert_eq!(
        resumed.transfer(&id).unwrap().get("user_paused"),
        Some(&Value::Bool(false))
    );
}

#[test]
fn queue_snapshots_use_priority_then_fifo_and_keep_controls_after_restart() {
    let scratch = Scratch::new();
    let cfg = client_config(&scratch.0.join("client"), false);
    let client = Client::open(cfg.clone()).unwrap();
    // Hash order intentionally opposes submission order.
    let mut submitted = Vec::new();
    for digit in ['f', '8', '1'] {
        let hash = digit.to_string().repeat(40);
        let id = client
            .ensure(&format!("magnet:?xt=urn:btih:{hash}"))
            .unwrap()
            .id;
        client.pause(&id).unwrap();
        submitted.push(id);
    }
    wait(|| {
        client
            .transfers()
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .all(|snapshot| snapshot.get("running") == Some(&Value::Bool(false)))
    });
    assert_eq!(ids(&client), submitted);
    client.set_priority(&submitted[2], 10).unwrap();
    assert_eq!(
        ids(&client),
        [
            submitted[2].clone(),
            submitted[0].clone(),
            submitted[1].clone()
        ]
    );
    client.set_priority(&submitted[1], 10).unwrap();
    assert_eq!(
        ids(&client),
        [
            submitted[1].clone(),
            submitted[2].clone(),
            submitted[0].clone()
        ]
    );
    client.set_priority(&submitted[0], 10).unwrap();
    assert_eq!(ids(&client), submitted);
    client
        .set_policy(
            &submitted[1],
            Some(policy(
                r#"{"download_limit_bps":65536,"seed_ratio_milli":1000}"#,
            )),
        )
        .unwrap();
    let control = client.controls(&submitted[1]).unwrap();
    drop(client);
    let resumed = Client::open(cfg).unwrap();
    assert_eq!(ids(&resumed), submitted);
    assert_eq!(resumed.controls(&submitted[1]).unwrap(), control);
    for id in &submitted {
        assert_eq!(
            resumed.transfer(id).unwrap().get("user_paused"),
            Some(&Value::Bool(true))
        );
    }
}

#[test]
fn the_active_slot_selects_priority_then_submission_order() {
    let scratch = Scratch::new();
    let mut torrents = [
        Torrent::single(&scratch.0, "queue-a.bin", payload(BLOCK, 41)),
        Torrent::single(&scratch.0, "queue-b.bin", payload(BLOCK, 42)),
        Torrent::single(&scratch.0, "queue-c.bin", payload(BLOCK, 43)),
    ];
    torrents.sort_by(|left, right| right.id.cmp(&left.id));
    let seed = Seeder::open(
        &scratch.0.join("seed"),
        &[&torrents[0], &torrents[1], &torrents[2]],
    );
    let proxies = [
        RecordingProxy::open(seed.client.listen_port()),
        RecordingProxy::open(seed.client.listen_port()),
        RecordingProxy::open(seed.client.listen_port()),
    ];
    let mut cfg = client_config(&scratch.0.join("client"), false);
    cfg.max_active = 1;
    fs::create_dir_all(&cfg.state_dir).unwrap();
    // Open one complete queue so a worker cannot race individual resume calls.
    for (index, torrent) in torrents.iter().enumerate() {
        fs::write(
            cfg.state_dir.join(format!("{}.torrent", torrent.id)),
            fs::read(&torrent.path).unwrap(),
        )
        .unwrap();
        fs::write(
            cfg.state_dir.join(format!("{}.source", torrent.id)),
            torrent.magnet(proxies[index].port),
        )
        .unwrap();
        let mut control = TorrentControl::new(torrent.id.clone(), index as u64 + 1).unwrap();
        control.priority = if index == 0 { -10 } else { 10 };
        fs::write(
            cfg.state_dir.join(format!("{}.control", torrent.id)),
            control.encode().unwrap(),
        )
        .unwrap();
    }
    let client = Client::open(cfg).unwrap();
    for index in [1, 2, 0] {
        wait(|| !proxies[index].requests.lock().unwrap().is_empty());
        for pending in [1, 2, 0]
            .into_iter()
            .skip_while(|value| *value != index)
            .skip(1)
        {
            assert!(
                proxies[pending].requests.lock().unwrap().is_empty(),
                "A later queue entry took the only active slot"
            );
        }
        proxies[index]
            .payloads_enabled
            .store(true, Ordering::Release);
        torrents[index].assert_files(&ready(&client, &torrents[index].id));
    }
}

#[test]
fn legacy_native_state_gets_a_stable_queue_without_inventing_traffic_counters() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "legacy.bin", payload(BLOCK + 9, 47));
    let cfg = client_config(&scratch.0.join("client"), false);
    fs::create_dir_all(&cfg.state_dir).unwrap();
    fs::write(
        cfg.state_dir.join(format!("{}.torrent", torrent.id)),
        fs::read(&torrent.path).unwrap(),
    )
    .unwrap();
    fs::write(
        cfg.state_dir.join(format!("{}.source", torrent.id)),
        torrent.path.to_str().unwrap(),
    )
    .unwrap();
    for (path, bytes) in &torrent.files {
        let destination = cfg.data_dir.join(&torrent.id).join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, bytes).unwrap();
    }
    let mut expected = vec![torrent.id.clone()];
    for digit in ['f', '1'] {
        let id = digit.to_string().repeat(40);
        fs::write(
            cfg.state_dir.join(format!("{id}.source")),
            format!("magnet:?xt=urn:btih:{id}"),
        )
        .unwrap();
        fs::write(cfg.state_dir.join(format!("{id}.paused")), b"paused\n").unwrap();
        expected.push(id);
    }
    expected.sort();
    let client = Client::open(cfg.clone()).unwrap();
    torrent.assert_files(&ready(&client, &torrent.id));
    assert_eq!(ids(&client), expected);
    let controls: Vec<_> = expected
        .iter()
        .map(|id| client.controls(id).unwrap())
        .collect();
    for (index, id) in expected.iter().enumerate() {
        let snapshot = client.transfer(id).unwrap();
        assert_eq!(counter(&snapshot, "downloaded_bytes"), 0);
        assert_eq!(counter(&snapshot, "uploaded_bytes"), 0);
        assert_eq!(controls[index].queue_order, index as u64 + 1);
        assert!(
            !controls[index].user_paused,
            "Legacy cancellation must not become an explicit user pause"
        );
        assert!(cfg.state_dir.join(format!("{id}.control")).is_file());
        if *id != torrent.id {
            assert_eq!(snapshot.get("paused"), Some(&Value::Bool(true)));
        }
    }
    assert_eq!(
        counter(&client.transfer(&torrent.id).unwrap(), "verified_bytes"),
        torrent.total as u64
    );
    drop(client);
    let resumed = Client::open(cfg).unwrap();
    assert_eq!(ids(&resumed), expected);
    for (id, control) in expected.iter().zip(&controls) {
        assert_eq!(resumed.controls(id).unwrap(), *control);
    }
    let legacy_paused = "1".repeat(40);
    resumed.resume_if_allowed(&legacy_paused).unwrap();
    let snapshot = resumed.transfer(&legacy_paused).unwrap();
    assert_eq!(snapshot.get("paused"), Some(&Value::Bool(false)));
    assert_eq!(snapshot.get("user_paused"), Some(&Value::Bool(false)));
}

#[test]
fn corrupted_and_inconsistent_control_files_are_rejected_without_rewriting_them() {
    let scratch = Scratch::new();
    let id = "a".repeat(40);
    let original = TorrentControl::new(id.clone(), 1).unwrap();
    let mut corrupt = original.encode().unwrap();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    let mut priorities_without_metadata = original.clone();
    priorities_without_metadata
        .file_priorities
        .insert(0, FilePriority::High);
    for (index, encoded) in [
        corrupt,
        original.encode().unwrap()[..43].to_vec(),
        TorrentControl::new("b".repeat(40), 1)
            .unwrap()
            .encode()
            .unwrap(),
        priorities_without_metadata.encode().unwrap(),
    ]
    .into_iter()
    .enumerate()
    {
        let cfg = client_config(&scratch.0.join(format!("invalid-{index}")), false);
        fs::create_dir_all(&cfg.state_dir).unwrap();
        fs::write(
            cfg.state_dir.join(format!("{id}.source")),
            format!("magnet:?xt=urn:btih:{id}"),
        )
        .unwrap();
        let control_path = cfg.state_dir.join(format!("{id}.control"));
        fs::write(&control_path, &encoded).unwrap();
        assert!(
            Client::open(cfg).is_err(),
            "Accepted invalid persisted controls {index}"
        );
        assert_eq!(fs::read(control_path).unwrap(), encoded);
    }
    let cfg = client_config(&scratch.0.join("duplicate-order"), false);
    fs::create_dir_all(&cfg.state_dir).unwrap();
    for id in ["a".repeat(40), "b".repeat(40)] {
        fs::write(
            cfg.state_dir.join(format!("{id}.source")),
            format!("magnet:?xt=urn:btih:{id}"),
        )
        .unwrap();
        fs::write(
            cfg.state_dir.join(format!("{id}.control")),
            TorrentControl::new(id.clone(), 1)
                .unwrap()
                .encode()
                .unwrap(),
        )
        .unwrap();
    }
    assert!(
        Client::open(cfg).is_err(),
        "Accepted duplicate persisted FIFO positions"
    );
}

#[test]
fn actual_payload_counters_persist_after_download_and_seed_restart() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "counters.bin", payload(BLOCK * 5 + 31, 19));
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let client_cfg = client_config(&scratch.0.join("client"), false);
    let client = Client::open(client_cfg.clone()).unwrap();
    client
        .ensure(&torrent.magnet(seed.client.listen_port()))
        .unwrap();
    torrent.assert_files(&ready(&client, &torrent.id));
    wait(|| {
        counter(
            &seed.client.transfer(&torrent.id).unwrap(),
            "uploaded_bytes",
        ) == torrent.total as u64
    });
    let downloaded = client.transfer(&torrent.id).unwrap();
    let uploaded = seed.client.transfer(&torrent.id).unwrap();
    assert_eq!(
        counter(&downloaded, "downloaded_bytes"),
        torrent.total as u64
    );
    assert_eq!(counter(&downloaded, "uploaded_bytes"), 0);
    assert_eq!(counter(&downloaded, "verified_bytes"), torrent.total as u64);
    assert_eq!(counter(&uploaded, "downloaded_bytes"), 0);
    assert_eq!(counter(&uploaded, "uploaded_bytes"), torrent.total as u64);
    let seed_cfg = client_config(&seed.root, true);
    drop(client);
    drop(seed);
    let resumed_client = Client::open(client_cfg).unwrap();
    let resumed_seed = Client::open(seed_cfg).unwrap();
    torrent.assert_files(&ready(&resumed_client, &torrent.id));
    torrent.assert_files(&ready(&resumed_seed, &torrent.id));
    assert_eq!(
        counter(
            &resumed_client.transfer(&torrent.id).unwrap(),
            "downloaded_bytes"
        ),
        torrent.total as u64
    );
    assert_eq!(
        counter(
            &resumed_seed.transfer(&torrent.id).unwrap(),
            "uploaded_bytes"
        ),
        torrent.total as u64
    );
}

#[test]
fn file_priorities_change_real_piece_order_without_skipping_verification() {
    let scratch = Scratch::new();
    let torrent = Torrent::multiple(
        &scratch.0,
        "priority-pack",
        vec![
            (PathBuf::from("low.bin"), payload(BLOCK, 1)),
            (PathBuf::from("normal.bin"), payload(BLOCK, 2)),
            (PathBuf::from("high.bin"), payload(BLOCK, 3)),
        ],
    );
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let cfg = client_config(&scratch.0.join("client"), false);
    let client = Client::open(cfg.clone()).unwrap();
    client.ensure(&torrent.magnet(proxy.port)).unwrap();
    wait(|| !proxy.requests.lock().unwrap().is_empty());
    client.pause(&torrent.id).unwrap();
    wait(|| client.transfer(&torrent.id).unwrap().get("running") == Some(&Value::Bool(false)));
    assert_eq!(
        counter(&client.transfer(&torrent.id).unwrap(), "verified_bytes"),
        0
    );
    client
        .set_file_priority(&torrent.id, 0, FilePriority::Low)
        .unwrap();
    client
        .set_file_priority(&torrent.id, 1, FilePriority::Normal)
        .unwrap();
    client
        .set_file_priority(&torrent.id, 2, FilePriority::High)
        .unwrap();
    let snapshot = client.transfer(&torrent.id).unwrap();
    let files = snapshot.get("files").unwrap().as_array().unwrap();
    for (file, expected) in files.iter().zip(["low", "normal", "high"]) {
        assert_eq!(file.get("priority").and_then(Value::as_str), Some(expected));
    }
    // Dropping also verifies priority persistence before the actual transfer.
    drop(client);
    proxy.wait_idle();
    let client = Client::open(cfg).unwrap();
    proxy.requests.lock().unwrap().clear();
    proxy.payloads_enabled.store(true, Ordering::Release);
    client.resume(&torrent.id).unwrap();
    torrent.assert_files(&ready(&client, &torrent.id));
    let requests = proxy.requests.lock().unwrap().clone();
    assert_eq!(requests, [2, 1, 0]);
    assert_eq!(
        counter(&client.transfer(&torrent.id).unwrap(), "verified_bytes"),
        torrent.total as u64
    );
}

#[test]
fn visible_file_indices_keep_metadata_padding_gaps_and_reject_padding_controls() {
    let scratch = Scratch::new();
    let torrent = Torrent::multiple_with_padding(
        &scratch.0,
        "padding-pack",
        vec![
            (PathBuf::from("first.bin"), payload(9000, 51), false),
            (PathBuf::from(".pad/7384"), vec![0; BLOCK - 9000], true),
            (PathBuf::from("empty.bin"), Vec::new(), false),
            (PathBuf::from("last.bin"), payload(BLOCK + 49, 52), false),
        ],
    );
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    seed.client.pause(&torrent.id).unwrap();
    let files = seed
        .client
        .transfer(&torrent.id)
        .unwrap()
        .get("files")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec();
    assert_eq!(files.len(), 3);
    for ((file, index), (path, bytes)) in files.iter().zip([0, 2, 3]).zip(&torrent.files) {
        assert_eq!(file.get("index").and_then(Value::as_u64), Some(index));
        assert_eq!(file.get("path").and_then(Value::as_str), path.to_str());
        assert_eq!(counter(file, "size_bytes"), bytes.len() as u64);
    }
    seed.client
        .set_file_priority(&torrent.id, 3, FilePriority::High)
        .unwrap();
    let saved = seed.client.controls(&torrent.id).unwrap();
    for index in [1, 4] {
        assert!(
            seed.client
                .set_file_priority(&torrent.id, index, FilePriority::Low)
                .is_err()
        );
        assert_eq!(seed.client.controls(&torrent.id).unwrap(), saved);
    }
    assert_eq!(
        counter(
            &seed.client.transfer(&torrent.id).unwrap(),
            "verified_bytes"
        ),
        torrent.total as u64
    );
    let cfg = client_config(&seed.root, true);
    drop(seed);
    let resumed = Client::open(cfg).unwrap();
    assert_eq!(resumed.controls(&torrent.id).unwrap(), saved);
    let files = resumed
        .transfer(&torrent.id)
        .unwrap()
        .get("files")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec();
    assert_eq!(files[2].get("index").and_then(Value::as_u64), Some(3));
    assert_eq!(
        files[2].get("priority").and_then(Value::as_str),
        Some("high")
    );
}

fn downloaded(client: &Client, torrents: &[&Torrent]) -> u64 {
    torrents
        .iter()
        .map(|torrent| counter(&client.transfer(&torrent.id).unwrap(), "downloaded_bytes"))
        .sum()
}

#[test]
fn a_global_download_cap_is_shared_by_transfers_and_cannot_be_bypassed_by_an_override() {
    const RATE: u64 = 128 * 1024;
    let scratch = Scratch::new();
    let first = Torrent::single(&scratch.0, "rate-first.bin", payload(BLOCK * 16, 5));
    let second = Torrent::single(&scratch.0, "rate-second.bin", payload(BLOCK * 16, 6));
    let torrents = [&first, &second];
    let seed = Seeder::open(&scratch.0.join("seed"), &torrents);
    let cfg = client_config(&scratch.0.join("client"), false);
    let client = Client::open_with_policy(cfg, policy(r#"{"download_limit_bps":131072}"#)).unwrap();
    client
        .ensure(&first.magnet(seed.client.listen_port()))
        .unwrap();
    client
        .set_policy(&first.id, Some(policy(r#"{"download_limit_bps":1048576}"#)))
        .unwrap();
    client
        .ensure(&second.magnet(seed.client.listen_port()))
        .unwrap();
    wait(|| {
        torrents.iter().all(|torrent| {
            counter(&client.transfer(&torrent.id).unwrap(), "downloaded_bytes") >= BLOCK as u64
        })
    });
    let before = downloaded(&client, &torrents);
    let start = Instant::now();
    thread::sleep(Duration::from_millis(600));
    let elapsed = start.elapsed().as_secs_f64();
    let delta = downloaded(&client, &torrents) - before;
    // A bounded allowance covers reservations at the sampling boundary.
    let maximum = (RATE as f64 * elapsed).ceil() as u64 + 2 * BLOCK as u64;
    assert!(delta > 0, "Capped transfers must still make progress");
    assert!(
        delta <= maximum,
        "Aggregate download cap exceeded: {delta} bytes in {elapsed:.3}s, allowed {maximum}"
    );
    torrent_pair_ready(&client, &torrents);
}

fn torrent_pair_ready(client: &Client, torrents: &[&Torrent]) {
    for torrent in torrents {
        torrent.assert_files(&ready(client, &torrent.id));
    }
}

#[test]
fn a_global_upload_cap_is_shared_across_incoming_peers() {
    const RATE: u64 = 128 * 1024;
    let scratch = Scratch::new();
    let first = Torrent::single(&scratch.0, "upload-first.bin", payload(BLOCK * 16, 7));
    let second = Torrent::single(&scratch.0, "upload-second.bin", payload(BLOCK * 16, 8));
    let torrents = [&first, &second];
    let cfg = client_config(&scratch.0.join("seed"), true);
    for torrent in torrents {
        for (path, bytes) in &torrent.files {
            let destination = cfg.data_dir.join(&torrent.id).join(path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::write(destination, bytes).unwrap();
        }
    }
    let seed = Client::open_with_policy(cfg, policy(r#"{"upload_limit_bps":131072}"#)).unwrap();
    for torrent in torrents {
        seed.ensure(torrent.path.to_str().unwrap()).unwrap();
        ready(&seed, &torrent.id);
    }
    let first_client = Client::open(client_config(&scratch.0.join("first-client"), false)).unwrap();
    let second_client =
        Client::open(client_config(&scratch.0.join("second-client"), false)).unwrap();
    first_client
        .ensure(&first.magnet(seed.listen_port()))
        .unwrap();
    second_client
        .ensure(&second.magnet(seed.listen_port()))
        .unwrap();
    let uploaded = || {
        torrents
            .iter()
            .map(|torrent| counter(&seed.transfer(&torrent.id).unwrap(), "uploaded_bytes"))
            .sum::<u64>()
    };
    wait(|| {
        torrents.iter().all(|torrent| {
            counter(&seed.transfer(&torrent.id).unwrap(), "uploaded_bytes") >= BLOCK as u64
        })
    });
    let before = uploaded();
    let start = Instant::now();
    thread::sleep(Duration::from_millis(600));
    let elapsed = start.elapsed().as_secs_f64();
    let delta = uploaded() - before;
    let maximum = (RATE as f64 * elapsed).ceil() as u64 + 2 * BLOCK as u64;
    assert!(delta > 0, "Capped uploads must still make progress");
    assert!(
        delta <= maximum,
        "Aggregate upload cap exceeded: {delta} bytes in {elapsed:.3}s, allowed {maximum}"
    );
    first.assert_files(&ready(&first_client, &first.id));
    second.assert_files(&ready(&second_client, &second.id));
}

#[test]
fn a_transfer_upload_cap_is_shared_by_two_incoming_peers() {
    const RATE: u64 = 64 * 1024;
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "local-upload.bin", payload(BLOCK * 16, 61));
    let seed = Seeder::open_with_policy(
        &scratch.0.join("seed"),
        &[&torrent],
        policy(r#"{"upload_limit_bps":131072}"#),
    );
    seed.client
        .set_policy(&torrent.id, Some(policy(r#"{"upload_limit_bps":65536}"#)))
        .unwrap();
    let first = Client::open(client_config(&scratch.0.join("first"), false)).unwrap();
    let second = Client::open(client_config(&scratch.0.join("second"), false)).unwrap();
    for client in [&first, &second] {
        client
            .ensure(&torrent.magnet(seed.client.listen_port()))
            .unwrap();
    }
    wait(|| {
        [&first, &second].iter().all(|client| {
            counter(&client.transfer(&torrent.id).unwrap(), "downloaded_bytes") >= BLOCK as u64
        })
    });
    let before = counter(
        &seed.client.transfer(&torrent.id).unwrap(),
        "uploaded_bytes",
    );
    let start = Instant::now();
    thread::sleep(Duration::from_millis(900));
    let elapsed = start.elapsed().as_secs_f64();
    let delta = counter(
        &seed.client.transfer(&torrent.id).unwrap(),
        "uploaded_bytes",
    ) - before;
    let maximum = (RATE as f64 * elapsed).ceil() as u64 + 2 * BLOCK as u64;
    assert!(delta > 0, "Capped incoming peers must keep making progress");
    assert!(
        delta <= maximum,
        "The local upload cap was duplicated per peer: {delta} bytes in {elapsed:.3}s, allowed {maximum}"
    );
    for client in [&first, &second] {
        torrent.assert_files(&ready(client, &torrent.id));
    }
}

#[test]
fn a_one_byte_rate_keeps_the_peer_alive_and_remains_responsive_to_pause() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "one-byte-rate.bin", payload(BLOCK * 2, 67));
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    proxy.payloads_enabled.store(true, Ordering::Release);
    let client = Client::open_with_policy(
        client_config(&scratch.0.join("client"), false),
        policy(r#"{"download_limit_bps":1}"#),
    )
    .unwrap();
    client.ensure(&torrent.magnet(proxy.port)).unwrap();
    wait(|| counter(&client.transfer(&torrent.id).unwrap(), "downloaded_bytes") == BLOCK as u64);
    let deadline = Instant::now() + Duration::from_secs(7);
    while proxy.keep_alives.load(Ordering::Relaxed) == 0 {
        assert!(
            Instant::now() < deadline,
            "A long rate wait did not send a peer keep-alive"
        );
        let snapshot = client.transfer(&torrent.id).unwrap();
        assert_eq!(snapshot.get("failed"), Some(&Value::Bool(false)));
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        counter(&client.transfer(&torrent.id).unwrap(), "downloaded_bytes"),
        BLOCK as u64
    );
    let paused_at = Instant::now();
    client.pause(&torrent.id).unwrap();
    while client.transfer(&torrent.id).unwrap().get("running") == Some(&Value::Bool(true)) {
        assert!(
            paused_at.elapsed() < Duration::from_secs(2),
            "Pausing waited for the long rate reservation"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let snapshot = client.transfer(&torrent.id).unwrap();
    assert_eq!(snapshot.get("user_paused"), Some(&Value::Bool(true)));
    assert_eq!(snapshot.get("ready"), Some(&Value::Bool(false)));
    assert_eq!(counter(&snapshot, "downloaded_bytes"), BLOCK as u64);
}

#[test]
fn a_ratio_limit_stops_seeding_but_retains_verified_files_and_can_be_cleared() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "ratio.bin", payload(BLOCK * 3 + 5, 11));
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    seed.client
        .set_policy(&torrent.id, Some(policy(r#"{"seed_ratio_milli":1000}"#)))
        .unwrap();
    let first = Client::open(client_config(&scratch.0.join("first"), false)).unwrap();
    first
        .ensure(&torrent.magnet(seed.client.listen_port()))
        .unwrap();
    torrent.assert_files(&ready(&first, &torrent.id));
    wait(|| {
        seed.client
            .transfer(&torrent.id)
            .unwrap()
            .get("seed_limited")
            == Some(&Value::Bool(true))
    });
    let snapshot = seed.client.transfer(&torrent.id).unwrap();
    assert_eq!(counter(&snapshot, "uploaded_bytes"), torrent.total as u64);
    torrent.assert_files(&seed.client.check(&torrent.id).unwrap());
    seed.client.set_policy(&torrent.id, None).unwrap();
    wait(|| {
        seed.client
            .transfer(&torrent.id)
            .unwrap()
            .get("seed_limited")
            == Some(&Value::Bool(false))
    });
    let second = Client::open(client_config(&scratch.0.join("second"), false)).unwrap();
    second
        .ensure(&torrent.magnet(seed.client.listen_port()))
        .unwrap();
    torrent.assert_files(&ready(&second, &torrent.id));
    wait(|| {
        counter(
            &seed.client.transfer(&torrent.id).unwrap(),
            "uploaded_bytes",
        ) == 2 * torrent.total as u64
    });
    torrent.assert_files(&seed.client.check(&torrent.id).unwrap());
}

#[test]
fn a_seed_time_limit_survives_restart_and_preserves_the_verified_payload() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "seed-time.bin", payload(BLOCK + 7, 13));
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    seed.client
        .set_policy(&torrent.id, Some(policy(r#"{"seed_time_secs":1}"#)))
        .unwrap();
    wait(|| {
        seed.client
            .transfer(&torrent.id)
            .unwrap()
            .get("seed_limited")
            == Some(&Value::Bool(true))
    });
    torrent.assert_files(&seed.client.check(&torrent.id).unwrap());
    let cfg = client_config(&seed.root, true);
    drop(seed);
    let resumed = Client::open(cfg).unwrap();
    wait(|| resumed.transfer(&torrent.id).unwrap().get("seed_limited") == Some(&Value::Bool(true)));
    torrent.assert_files(&ready(&resumed, &torrent.id));
    assert_eq!(
        counter(&resumed.transfer(&torrent.id).unwrap(), "uploaded_bytes"),
        0
    );
}

#[test]
fn paused_and_offline_time_do_not_consume_the_remaining_seed_budget() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "seed-clock.bin", payload(BLOCK + 11, 57));
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    seed.client
        .set_policy(&torrent.id, Some(policy(r#"{"seed_time_secs":3}"#)))
        .unwrap();
    seed.client.pause(&torrent.id).unwrap();
    let before = seed.client.controls(&torrent.id).unwrap().seed_elapsed_secs;
    assert!(before < 3);
    thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        seed.client.controls(&torrent.id).unwrap().seed_elapsed_secs,
        before
    );
    assert_eq!(
        seed.client
            .transfer(&torrent.id)
            .unwrap()
            .get("seed_limited"),
        Some(&Value::Bool(false))
    );
    seed.client.resume(&torrent.id).unwrap();
    wait(|| seed.client.controls(&torrent.id).unwrap().seed_elapsed_secs > before);
    seed.client.pause(&torrent.id).unwrap();
    let elapsed = seed.client.controls(&torrent.id).unwrap().seed_elapsed_secs;
    assert!(
        elapsed < 3,
        "The fixture must restart before the cap expires"
    );
    let cfg = client_config(&seed.root, true);
    drop(seed);
    thread::sleep(Duration::from_millis(1200));
    let resumed = Client::open(cfg).unwrap();
    assert_eq!(
        resumed.controls(&torrent.id).unwrap().seed_elapsed_secs,
        elapsed
    );
    assert_eq!(
        resumed.transfer(&torrent.id).unwrap().get("seed_limited"),
        Some(&Value::Bool(false))
    );
    resumed.resume(&torrent.id).unwrap();
    wait(|| resumed.transfer(&torrent.id).unwrap().get("seed_limited") == Some(&Value::Bool(true)));
    assert!(resumed.controls(&torrent.id).unwrap().seed_elapsed_secs >= 3);
    torrent.assert_files(&resumed.check(&torrent.id).unwrap());
}

#[test]
fn a_whole_transfer_policy_override_can_remove_and_restore_default_seed_limits() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "seed-default.bin", payload(BLOCK, 59));
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    seed.client.pause(&torrent.id).unwrap();
    let cfg = client_config(&seed.root, true);
    drop(seed);
    let seed = Client::open_with_policy(cfg, policy(r#"{"seed_time_secs":1}"#)).unwrap();
    seed.set_policy(&torrent.id, Some(TransferPolicy::default()))
        .unwrap();
    seed.resume(&torrent.id).unwrap();
    torrent.assert_files(&ready(&seed, &torrent.id));
    wait(|| seed.controls(&torrent.id).unwrap().seed_elapsed_secs >= 1);
    let snapshot = seed.transfer(&torrent.id).unwrap();
    assert_eq!(snapshot.get("seed_limited"), Some(&Value::Bool(false)));
    assert_eq!(
        snapshot
            .get("effective_policy")
            .unwrap()
            .get("seed_time_secs"),
        Some(&Value::Null)
    );
    seed.set_policy(&torrent.id, None).unwrap();
    wait(|| seed.transfer(&torrent.id).unwrap().get("seed_limited") == Some(&Value::Bool(true)));
    torrent.assert_files(&seed.check(&torrent.id).unwrap());
}

struct Server {
    engine: Arc<Engine>,
    origin: String,
    thread: Option<JoinHandle<mynou::Result<()>>>,
}

impl Server {
    fn open(cfg: mynou::config::Config) -> Self {
        let engine = Engine::open(cfg).unwrap();
        let api = Api::bind(engine.clone(), TOKEN.into()).unwrap();
        let origin = format!("http://{}", api.address().unwrap());
        Self {
            engine,
            origin,
            thread: Some(thread::spawn(move || api.run())),
        }
    }

    fn call(
        &self,
        method: &str,
        route: &str,
        token: Option<&str>,
        value: Option<&Value>,
    ) -> (u16, Value) {
        let body = value.map_or_else(Vec::new, |value| json::stringify(value).into_bytes());
        self.call_raw(method, route, token, &body)
    }

    fn call_raw(
        &self,
        method: &str,
        route: &str,
        token: Option<&str>,
        body: &[u8],
    ) -> (u16, Value) {
        let mut headers = vec![("Content-Type".into(), "application/json".into())];
        if let Some(token) = token {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        let response = HttpClient::new()
            .without_proxy()
            .with_timeout(Duration::from_secs(5))
            .request(method, &format!("{}{route}", self.origin), &headers, body)
            .unwrap();
        (
            response.status,
            json::parse(std::str::from_utf8(&response.body).unwrap()).unwrap(),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.engine.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if !thread::panicking() => {
                    panic!("Transfer API fixture failed: {error}")
                }
                Err(error) if !thread::panicking() => std::panic::resume_unwind(error),
                _ => {}
            }
        }
    }
}

fn request(source: &str, title: &str) -> Request {
    Request {
        kind: "movie".into(),
        title: title.into(),
        year: 2026,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: Some(source.into()),
        tmdb_id: None,
    }
}

fn poll_paused(engine: &Arc<Engine>, id: &str) {
    let deadline = Instant::now() + Duration::from_millis(1200);
    let mut ticks = 0;
    while Instant::now() < deadline {
        ticks += usize::from(engine.tick().unwrap());
        let snapshot = engine.transfer(id).unwrap();
        assert_eq!(snapshot.get("user_paused"), Some(&Value::Bool(true)));
        assert_eq!(counter(&snapshot, "verified_bytes"), 0);
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ticks > 0, "The engine must poll the paused transfer");
}

#[test]
fn authenticated_pause_is_durable_during_engine_polling_and_explicit_resume_completes_import() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(
        &scratch.0,
        "pause.mp4",
        include_bytes!("../examples/demo.mp4").to_vec(),
    );
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let cfg = engine_config(&scratch.0.join("engine"));
    let server = Server::open(cfg.clone());
    let job = server
        .engine
        .submit(request(&torrent.magnet(proxy.port), "Paused Movie"))
        .unwrap()
        .remove(0);
    assert!(server.engine.tick().unwrap());
    wait(|| !proxy.requests.lock().unwrap().is_empty());
    let route = format!("/api/transfers/{}/pause", torrent.id);
    let (status, paused) = server.call("POST", &route, Some(TOKEN), Some(&Value::object()));
    assert_eq!(status, 200, "{}", json::stringify(&paused));
    assert_eq!(paused.get("user_paused"), Some(&Value::Bool(true)));
    poll_paused(&server.engine, &torrent.id);
    let transfer = server.engine.transfer(&torrent.id).unwrap();
    assert_eq!(transfer.get("user_paused"), Some(&Value::Bool(true)));
    assert_eq!(counter(&transfer, "verified_bytes"), 0);
    assert_eq!(
        transfer.get("request_ids").unwrap().as_array().unwrap(),
        [Value::String(job.id.clone())]
    );
    redacted(&transfer);
    drop(server);
    proxy.wait_idle();
    let server = Server::open(cfg);
    poll_paused(&server.engine, &torrent.id);
    assert_eq!(
        server
            .engine
            .transfer(&torrent.id)
            .unwrap()
            .get("user_paused"),
        Some(&Value::Bool(true))
    );
    proxy.payloads_enabled.store(true, Ordering::Release);
    let (status, resumed) = server.call(
        "POST",
        &format!("/api/transfers/{}/resume", torrent.id),
        Some(TOKEN),
        Some(&Value::object()),
    );
    assert_eq!(status, 200, "{}", json::stringify(&resumed));
    assert_eq!(resumed.get("user_paused"), Some(&Value::Bool(false)));
    wait(|| {
        server.engine.tick().unwrap();
        let job = lock(&server.engine.store).unwrap().get(&job.id).unwrap();
        assert_ne!(job.state, "failed", "{:?}", job.last_error);
        job.state == "ready"
    });
    let imported = lock(&server.engine.store).unwrap().get(&job.id).unwrap();
    assert_eq!(
        fs::read(&imported.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
}

#[test]
fn transfer_api_authentication_and_strict_controls_preserve_existing_state_on_errors() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "controls.bin", payload(BLOCK, 17));
    let server = Server::open(engine_config(&scratch.0.join("engine")));
    let job = server
        .engine
        .submit(request(torrent.path.to_str().unwrap(), "Control Fixture"))
        .unwrap()
        .remove(0);
    server.engine.tick().unwrap();
    server.engine.pause_transfer(&torrent.id).unwrap();
    server
        .engine
        .set_transfer_policy(&torrent.id, Some(policy(r#"{"upload_limit_bps":65536}"#)))
        .unwrap();
    server
        .engine
        .set_file_priority(&torrent.id, 0, FilePriority::High)
        .unwrap();
    let before = server.engine.transfer(&torrent.id).unwrap();
    let base = format!("/api/transfers/{}", torrent.id);
    for token in [None, Some("incorrect-token")] {
        assert_eq!(server.call("GET", "/api/transfers", token, None).0, 401);
        assert_eq!(server.call("GET", &base, token, None).0, 401);
        assert_eq!(
            server
                .call(
                    "POST",
                    &format!("{base}/resume"),
                    token,
                    Some(&Value::object())
                )
                .0,
            401
        );
    }
    let malformed = [
        ("pause", r#"{"extra":true}"#),
        ("resume", "[]"),
        ("priority", r#"{"priority":"10"}"#),
        ("priority", r#"{"priority":1001}"#),
        ("priority", r#"{"priority":1.5}"#),
        ("priority", r#"{"priority":10,"extra":true}"#),
        ("files", r#"{"index":0,"priority":"skip"}"#),
        ("files", r#"{"index":999,"priority":"high"}"#),
        ("files", r#"{"index":-1,"priority":"high"}"#),
        ("files", r#"{"index":"0","priority":"high"}"#),
        ("files", r#"{"index":0.5,"priority":"high"}"#),
        ("policy", r#"{}"#),
        ("policy", r#"{"policy":{"download_limit_bps":-1}}"#),
        ("policy", r#"{"policy":{"download_limit_bps":1073741825}}"#),
        ("policy", r#"{"policy":{"upload_limit_bps":"65536"}}"#),
        ("policy", r#"{"policy":{"upload_limit_bps":null}}"#),
        ("policy", r#"{"policy":{"seed_ratio_milli":0}}"#),
        ("policy", r#"{"policy":{"seed_ratio_milli":1000001}}"#),
        ("policy", r#"{"policy":{"seed_time_secs":0}}"#),
        ("policy", r#"{"policy":{"seed_time_secs":315360001}}"#),
        ("policy", r#"{"policy":{"seed_time_secs":true}}"#),
        ("policy", r#"{"policy":{"unknown":true}}"#),
        ("policy", r#"{"policy":null,"extra":true}"#),
    ];
    for (action, body) in malformed {
        let (status, error) = server.call(
            "POST",
            &format!("{base}/{action}"),
            Some(TOKEN),
            Some(&json::parse(body).unwrap()),
        );
        assert_eq!(
            status,
            400,
            "Accepted invalid {action}: {body} => {}",
            json::stringify(&error)
        );
        redacted(&error);
        let after = server.engine.transfer(&torrent.id).unwrap();
        for key in [
            "user_paused",
            "priority",
            "files",
            "policy",
            "effective_policy",
        ] {
            assert_eq!(
                after.get(key),
                before.get(key),
                "Invalid {action} changed {key}: {body}"
            );
        }
    }
    assert_eq!(
        server
            .call_raw("POST", &format!("{base}/policy"), Some(TOKEN), b"{")
            .0,
        400
    );
    assert_eq!(
        server
            .call("GET", "/api/transfers/invalid", Some(TOKEN), None)
            .0,
        404
    );
    assert_eq!(
        server
            .call(
                "POST",
                &format!("{base}/unknown"),
                Some(TOKEN),
                Some(&Value::object())
            )
            .0,
        404
    );
    let (status, snapshot) = server.call("GET", &base, Some(TOKEN), None);
    assert_eq!(status, 200);
    assert_eq!(snapshot.get("user_paused"), Some(&Value::Bool(true)));
    assert_eq!(snapshot.get("priority").and_then(Value::as_i64), Some(0));
    assert_eq!(
        snapshot.get("files").unwrap().as_array().unwrap()[0]
            .get("priority")
            .and_then(Value::as_str),
        Some("high")
    );
    assert_eq!(
        snapshot.get("request_ids").unwrap().as_array().unwrap(),
        [Value::String(job.id)]
    );
    redacted(&snapshot);
}

#[test]
fn shared_requests_expose_one_transfer_and_apply_controls_to_both_requests() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(
        &scratch.0,
        "shared.mp4",
        include_bytes!("../examples/demo.mp4").to_vec(),
    );
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let server = Server::open(engine_config(&scratch.0.join("engine")));
    let mut expected = Vec::new();
    for (index, title) in ["Shared First", "Shared Second"].into_iter().enumerate() {
        // Different acquisition URLs keep the application requests distinct,
        // while their identical info hash shares one native transfer.
        let source = format!("{}&dn=request-{index}", torrent.magnet(proxy.port));
        expected.push(
            server
                .engine
                .submit(request(&source, title))
                .unwrap()
                .remove(0)
                .id,
        );
    }
    assert_ne!(expected[0], expected[1]);
    wait(|| {
        server.engine.tick().unwrap();
        let store = lock(&server.engine.store).unwrap();
        expected
            .iter()
            .all(|id| store.get(id).unwrap().download_id.as_deref() == Some(torrent.id.as_str()))
    });
    wait(|| !proxy.requests.lock().unwrap().is_empty());
    let (status, list) = server.call("GET", "/api/transfers", Some(TOKEN), None);
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);
    let snapshot = &list.as_array().unwrap()[0];
    let mut observed: Vec<_> = snapshot
        .get("request_ids")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect();
    observed.sort();
    expected.sort();
    assert_eq!(observed, expected);
    let base = format!("/api/transfers/{}", torrent.id);
    assert_eq!(
        server
            .call("POST", &format!("{base}/pause"), Some(TOKEN), None)
            .0,
        200
    );
    poll_paused(&server.engine, &torrent.id);
    server.engine.cancel(&expected[0]).unwrap();
    assert_eq!(
        server
            .engine
            .transfer(&torrent.id)
            .unwrap()
            .get("user_paused"),
        Some(&Value::Bool(true))
    );
    proxy.payloads_enabled.store(true, Ordering::Release);
    assert_eq!(
        server
            .call("POST", &format!("{base}/resume"), Some(TOKEN), None)
            .0,
        200
    );
    wait(|| {
        server.engine.tick().unwrap();
        let store = lock(&server.engine.store).unwrap();
        assert_eq!(store.get(&expected[0]).unwrap().state, "cancelled");
        let active = store.get(&expected[1]).unwrap();
        assert_ne!(active.state, "failed", "{:?}", active.last_error);
        active.state == "ready"
    });
    let active = lock(&server.engine.store)
        .unwrap()
        .get(&expected[1])
        .unwrap();
    assert_eq!(
        fs::read(&active.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
    let (status, snapshot) = server.call("GET", &base, Some(TOKEN), None);
    assert_eq!(status, 200);
    assert_eq!(
        snapshot
            .get("request_ids")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    redacted(&snapshot);
}

fn command(directory: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(directory)
        .env_remove("MYNOU_API_TOKEN")
        .env_remove("MYNOU_PLEX_TOKEN")
        .env_remove("MYNOU_DEMO_TOKEN")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "Transfer CLI timed out: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn successful(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    json::parse(std::str::from_utf8(&output.stdout).unwrap().trim()).unwrap()
}

#[test]
fn online_cli_controls_the_existing_native_service_and_validates_file_priorities() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "cli.bin", payload(BLOCK, 23));
    let engine_root = scratch.0.join("engine");
    let server = Server::open(engine_config(&engine_root));
    let job = server
        .engine
        .submit(request(torrent.path.to_str().unwrap(), "CLI Transfer"))
        .unwrap()
        .remove(0);
    server.engine.tick().unwrap();
    let mut value = configuration();
    value.insert("listen", server.origin.strip_prefix("http://").unwrap());
    fs::write(engine_root.join("mynou.json"), json::stringify(&value)).unwrap();
    fs::write(
        engine_root.join(".env"),
        format!("MYNOU_API_TOKEN={TOKEN}\n"),
    )
    .unwrap();
    let transfers = successful(command(&engine_root, &["torrents"]));
    assert_eq!(transfers.as_array().unwrap().len(), 1);
    let transfer = successful(command(&engine_root, &["torrent", &torrent.id]));
    assert_eq!(
        transfer.get("request_ids").unwrap().as_array().unwrap(),
        [Value::String(job.id)]
    );
    let paused = successful(command(&engine_root, &["pause", &torrent.id]));
    assert_eq!(paused.get("user_paused"), Some(&Value::Bool(true)));
    let priority = successful(command(
        &engine_root,
        &["torrent-priority", &torrent.id, "--priority", "-10"],
    ));
    assert_eq!(priority.get("priority").and_then(Value::as_i64), Some(-10));
    let files = successful(command(
        &engine_root,
        &[
            "file-priority",
            &torrent.id,
            "--file",
            "0",
            "--priority",
            "high",
        ],
    ));
    assert_eq!(
        files.get("files").unwrap().as_array().unwrap()[0]
            .get("priority")
            .and_then(Value::as_str),
        Some("high")
    );
    let policy = successful(command(
        &engine_root,
        &[
            "torrent-policy",
            &torrent.id,
            "--policy",
            r#"{"upload_limit_bps":65536}"#,
        ],
    ));
    let cleared = successful(command(
        &engine_root,
        &["torrent-policy", &torrent.id, "--policy", "null"],
    ));
    for args in [
        vec![
            "torrent-priority",
            torrent.id.as_str(),
            "--priority",
            "1001",
        ],
        vec!["torrent-priority", torrent.id.as_str(), "--priority", "1.5"],
        vec![
            "file-priority",
            torrent.id.as_str(),
            "--file",
            "0",
            "--priority",
            "skip",
        ],
        vec![
            "file-priority",
            torrent.id.as_str(),
            "--file",
            "999",
            "--priority",
            "high",
        ],
        vec![
            "torrent-policy",
            torrent.id.as_str(),
            "--policy",
            r#"{"unknown":true}"#,
        ],
        vec!["torrent", "0123456789abcdef0123456789abcdef"],
    ] {
        let output = command(&engine_root, &args);
        assert!(
            !output.status.success(),
            "Accepted invalid transfer arguments: {args:?}"
        );
        assert!(!output.stderr.is_empty());
    }
    let resumed = successful(command(&engine_root, &["resume", &torrent.id]));
    assert_eq!(resumed.get("user_paused"), Some(&Value::Bool(false)));
    for snapshot in [
        &transfers, &transfer, &paused, &priority, &files, &policy, &cleared, &resumed,
    ] {
        redacted(snapshot);
    }
}

#[test]
fn transfer_cli_requires_a_running_service_without_creating_local_state() {
    let scratch = Scratch::new();
    let cfg = scratch.0.join("mynou.json");
    fs::write(&cfg, json::stringify(&configuration())).unwrap();
    let before = fs::read(&cfg).unwrap();
    for args in [
        vec!["torrents"],
        vec!["torrent", "ffffffffffffffffffffffffffffffffffffffff"],
        vec!["pause", "ffffffffffffffffffffffffffffffffffffffff"],
    ] {
        let output = command(&scratch.0, &args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("running Mynou service"));
        assert_eq!(fs::read(&cfg).unwrap(), before);
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1);
    }
    // Parsing a valid policy does not change the read-only configuration either.
    assert!(config::load(&cfg).unwrap().downloads_enabled);
}
