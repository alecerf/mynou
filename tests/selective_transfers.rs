//! Selective acquisition uses synthetic local peers; execution belongs in CI.
#[allow(dead_code)]
mod transfer_support;
use mynou::{
    bencode::{self, Value as Bencode},
    crypto::{sha1, sha256},
    json::{self, Value},
    torrent::{Client, DownloadStatus, FileSelection, SelectionUpdate, TorrentControl},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};
use transfer_support::*;

fn selected(client: &Client, id: &str) -> DownloadStatus {
    wait(|| client.check(id).unwrap().selected_ready);
    client.check(id).unwrap()
}

fn bits(client: &Client, id: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", client.listen_port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut hello = [0_u8; 68];
    hello[0] = 19;
    hello[1..20].copy_from_slice(b"BitTorrent protocol");
    let hash: Vec<_> = id
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    hello[28..48].copy_from_slice(&hash[..20]);
    hello[48..].copy_from_slice(b"-SELECT-FIXTURE-0000");
    stream.write_all(&hello).unwrap();
    stream.read_exact(&mut hello).unwrap();
    loop {
        let mut size = [0; 4];
        stream.read_exact(&mut size).unwrap();
        let size = u32::from_be_bytes(size) as usize;
        assert!(size <= 1024);
        if size == 0 {
            continue;
        }
        let mut message = vec![0; size];
        stream.read_exact(&mut message).unwrap();
        if message[0] == 5 {
            return message[1..].to_vec();
        }
    }
}

#[test]
fn v1_boundary_pieces_are_verified_without_fetching_unneeded_pieces_or_advertising_completion() {
    for peers in [1, 4] {
        let scratch = Scratch::new();
        let torrent = Torrent::multiple(
            &scratch.0.join("meta"),
            "Boundary",
            vec![
                ("before.bin".into(), payload(BLOCK + 37, 11)),
                ("chosen.bin".into(), payload(BLOCK + 111, 23)),
                ("after.bin".into(), payload(BLOCK * 3, 31)),
                ("untouched.bin".into(), payload(BLOCK * 2, 41)),
            ],
        );
        let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
        let proxy = RecordingProxy::open(seed.client.listen_port());
        proxy.payloads_enabled.store(true, Ordering::Release);
        let mut cfg = client_config(&scratch.0.join("client"), true);
        cfg.max_peers = peers;
        let client = Client::open(cfg.clone()).unwrap();
        client
            .ensure_files(&torrent.magnet(proxy.port), &["Boundary/chosen.bin".into()])
            .unwrap();
        let status = selected(&client, &torrent.id);
        assert!(!status.ready);
        assert_eq!(status.progress, 1.0);
        assert_eq!(
            status.available_files,
            [cfg.data_dir.join(&torrent.id).join("Boundary/chosen.bin")]
        );
        assert_eq!(
            fs::read(&status.available_files[0]).unwrap(),
            torrent.files[1].1
        );
        assert!(
            !cfg.data_dir
                .join(&torrent.id)
                .join("Boundary/untouched.bin")
                .exists()
        );
        assert_eq!(
            proxy
                .requests
                .lock()
                .unwrap()
                .iter()
                .copied()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([1, 2])
        );
        let snapshot = client.transfer(&torrent.id).unwrap();
        assert_eq!(
            snapshot.get("status"),
            Some(&Value::String("selected_ready".into()))
        );
        assert_eq!(
            snapshot.get("seed_elapsed_secs"),
            Some(&Value::String("0".into()))
        );
        let files = snapshot.get("files").unwrap().as_array().unwrap();
        for (index, file) in files.iter().enumerate() {
            assert_eq!(file.get("selected"), Some(&Value::Bool(index == 1)));
            assert_eq!(file.get("verified"), Some(&Value::Bool(index == 1)));
        }
        assert!(bits(&client, &torrent.id).iter().all(|byte| *byte == 0));
        let downloaded = client.transfer_stats(&torrent.id).unwrap().0;
        assert_eq!(downloaded, (BLOCK * 2) as u64);
        drop(client);
        proxy.wait_idle();
        drop(proxy);
        drop(seed);
        let restored = Client::open(cfg).unwrap();
        let recovered = selected(&restored, &torrent.id);
        assert!(!recovered.ready);
        assert_eq!(recovered.available_files, status.available_files);
        assert_eq!(restored.transfer_stats(&torrent.id).unwrap().0, downloaded);
    }
}

#[test]
fn shared_interests_expand_after_partial_completion_and_user_pause_survives_expansion() {
    let scratch = Scratch::new();
    let torrent = Torrent::multiple(
        &scratch.0.join("meta"),
        "Shared",
        vec![
            ("first.bin".into(), payload(BLOCK * 2, 13)),
            ("second.bin".into(), payload(BLOCK * 2, 29)),
            ("third.bin".into(), payload(BLOCK * 2, 37)),
        ],
    );
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    proxy.payloads_enabled.store(true, Ordering::Release);
    let cfg = client_config(&scratch.0.join("client"), false);
    let client = Client::open(cfg.clone()).unwrap();
    let source = torrent.magnet(proxy.port);
    client
        .ensure_files(&source, &["Shared/first.bin".into()])
        .unwrap();
    let first = selected(&client, &torrent.id);
    assert!(!first.ready);
    client.pause(&torrent.id).unwrap();
    wait(|| client.transfer(&torrent.id).unwrap().get("running") == Some(&Value::Bool(false)));
    client
        .ensure_files(&source, &["Shared/second.bin".into()])
        .unwrap();
    let paused = client.transfer(&torrent.id).unwrap();
    assert_eq!(paused.get("user_paused"), Some(&Value::Bool(true)));
    assert_eq!(paused.get("running"), Some(&Value::Bool(false)));
    assert!(!client.check(&torrent.id).unwrap().selected_ready);
    assert_eq!(
        client.check(&torrent.id).unwrap().available_files,
        first.available_files
    );
    assert!(
        client
            .select_files(&torrent.id, &SelectionUpdate::Indices(vec![99]))
            .is_err()
    );
    assert_eq!(client.transfer(&torrent.id).unwrap(), paused);
    let control = client.controls(&torrent.id).unwrap();
    drop(client);
    proxy.wait_idle();
    let client = Client::open(cfg).unwrap();
    assert_eq!(client.controls(&torrent.id).unwrap(), control);
    assert!(!client.check(&torrent.id).unwrap().selected_ready);
    assert!(
        client
            .check(&torrent.id)
            .unwrap()
            .available_files
            .is_empty()
    );
    client.resume(&torrent.id).unwrap();
    let both = selected(&client, &torrent.id);
    assert_eq!(both.available_files.len(), 2);
    assert!(!both.ready);
    assert!(!both.files[2].exists());
    assert_eq!(client.transfers().unwrap().as_array().unwrap().len(), 1);
    client
        .select_files(&torrent.id, &SelectionUpdate::All)
        .unwrap();
    torrent.assert_files(&ready(&client, &torrent.id));
    assert_eq!(
        client.controls(&torrent.id).unwrap().file_selection,
        FileSelection::All
    );
}

#[test]
fn expansion_retires_an_inflight_generation_and_keeps_exclusive_verified_writes() {
    let scratch = Scratch::new();
    let torrent = Torrent::multiple(
        &scratch.0.join("meta"),
        "Changing",
        vec![
            ("first.bin".into(), payload(BLOCK * 3, 17)),
            ("second.bin".into(), payload(BLOCK * 3, 19)),
            ("unneeded.bin".into(), payload(BLOCK * 3, 23)),
        ],
    );
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let cfg = client_config(&scratch.0.join("client"), false);
    let client = Client::open(cfg.clone()).unwrap();
    client
        .ensure_files(&torrent.magnet(proxy.port), &["Changing/first.bin".into()])
        .unwrap();
    wait(|| !proxy.requests.lock().unwrap().is_empty());
    client
        .select_files(&torrent.id, &SelectionUpdate::Indices(vec![1]))
        .unwrap();
    client.pause(&torrent.id).unwrap();
    wait(|| client.transfer(&torrent.id).unwrap().get("running") == Some(&Value::Bool(false)));
    assert_eq!(client.transfer_stats(&torrent.id).unwrap().0, 0);
    assert!(
        client
            .check(&torrent.id)
            .unwrap()
            .available_files
            .is_empty()
    );
    assert!(
        !cfg.data_dir
            .join(&torrent.id)
            .join("Changing/unneeded.bin")
            .exists()
    );
    proxy.wait_idle();
    proxy.payloads_enabled.store(true, Ordering::Release);
    client.resume(&torrent.id).unwrap();
    let status = selected(&client, &torrent.id);
    assert!(!status.ready);
    for (file, (_, expected)) in status.available_files.iter().zip(&torrent.files) {
        assert_eq!(fs::read(file).unwrap(), *expected);
    }
    assert_eq!(
        client.transfer_stats(&torrent.id).unwrap().0,
        (BLOCK * 6) as u64
    );
}

#[test]
fn selection_handles_padding_and_empty_files_and_rejects_absent_paths_before_payload() {
    let scratch = Scratch::new();
    let torrent = Torrent::multiple_with_padding(
        &scratch.0.join("meta"),
        "Padding",
        vec![
            ("first.bin".into(), payload(9000, 11), false),
            (".pad/7384".into(), vec![0; BLOCK - 9000], true),
            ("empty.bin".into(), Vec::new(), false),
            ("last.bin".into(), payload(BLOCK + 49, 23), false),
        ],
    );
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    proxy.payloads_enabled.store(true, Ordering::Release);
    let cfg = client_config(&scratch.0.join("client"), false);
    let client = Client::open(cfg.clone()).unwrap();
    let source = torrent.magnet(proxy.port);
    client
        .ensure_files(&source, &["Padding/empty.bin".into()])
        .unwrap();
    let empty = selected(&client, &torrent.id);
    assert!(!empty.ready);
    assert_eq!(empty.available_files.len(), 1);
    assert_eq!(fs::metadata(&empty.available_files[0]).unwrap().len(), 0);
    assert!(proxy.requests.lock().unwrap().is_empty());
    let saved = client.controls(&torrent.id).unwrap();
    for path in ["Padding/absent.bin", "Padding/.pad/7384"] {
        assert!(client.require_files(&torrent.id, &[path.into()]).is_err());
        assert_eq!(client.controls(&torrent.id).unwrap(), saved);
    }
    assert!(
        client
            .select_files(&torrent.id, &SelectionUpdate::Indices(vec![1]))
            .is_err()
    );
    client
        .select_files(&torrent.id, &SelectionUpdate::Indices(vec![3]))
        .unwrap();
    let result = selected(&client, &torrent.id);
    assert!(!result.ready);
    assert_eq!(
        proxy
            .requests
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([1, 2])
    );
    assert!(
        !cfg.data_dir
            .join(&torrent.id)
            .join("Padding/first.bin")
            .exists()
    );
    assert!(!cfg.data_dir.join(&torrent.id).join("Padding/.pad").exists());
}

#[test]
fn an_unresolved_missing_magnet_path_fails_without_payload_and_explicit_correction_can_recover() {
    let scratch = Scratch::new();
    let torrent = Torrent::multiple(
        &scratch.0.join("meta"),
        "Corrected",
        vec![
            ("right.bin".into(), payload(BLOCK, 53)),
            ("unneeded.bin".into(), payload(BLOCK * 2, 59)),
        ],
    );
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    proxy.payloads_enabled.store(true, Ordering::Release);
    let client = Client::open(client_config(&scratch.0.join("client"), false)).unwrap();
    client
        .ensure_files(
            &torrent.magnet(proxy.port),
            &["Corrected/absent.bin".into()],
        )
        .unwrap();
    wait(|| client.transfer(&torrent.id).unwrap().get("failed") == Some(&Value::Bool(true)));
    assert!(
        client
            .check(&torrent.id)
            .unwrap_err()
            .contains("absent or ambiguous")
    );
    assert!(proxy.requests.lock().unwrap().is_empty());
    assert_eq!(client.transfer_stats(&torrent.id).unwrap().0, 0);
    client
        .require_files(&torrent.id, &["Corrected/right.bin".into()])
        .unwrap();
    let recovered = selected(&client, &torrent.id);
    assert!(!recovered.ready);
    assert_eq!(
        fs::read(&recovered.available_files[0]).unwrap(),
        torrent.files[0].1
    );
    assert_eq!(
        client.controls(&torrent.id).unwrap().file_selection,
        FileSelection::paths(&["Corrected/right.bin".into()]).unwrap()
    );
    assert!(
        client
            .require_files(&torrent.id, &["Corrected/absent.bin".into()])
            .is_err()
    );
    assert_eq!(
        proxy
            .requests
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([0])
    );
}

#[test]
fn corrupted_selected_bytes_are_reverified_on_restart_before_their_path_is_available() {
    let scratch = Scratch::new();
    let torrent = Torrent::multiple(
        &scratch.0.join("meta"),
        "Recheck",
        vec![
            ("wanted.bin".into(), payload(BLOCK, 61)),
            ("skipped.bin".into(), payload(BLOCK * 3, 67)),
        ],
    );
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    proxy.payloads_enabled.store(true, Ordering::Release);
    let cfg = client_config(&scratch.0.join("client"), false);
    let client = Client::open(cfg.clone()).unwrap();
    client
        .ensure_files(&torrent.magnet(proxy.port), &["Recheck/wanted.bin".into()])
        .unwrap();
    let verified = selected(&client, &torrent.id);
    drop(client);
    proxy.wait_idle();
    fs::write(&verified.available_files[0], payload(BLOCK, 71)).unwrap();
    proxy.payloads_enabled.store(false, Ordering::Release);
    proxy.requests.lock().unwrap().clear();
    let restored = Client::open(cfg).unwrap();
    wait(|| !proxy.requests.lock().unwrap().is_empty());
    let pending = restored.check(&torrent.id).unwrap();
    assert!(!pending.ready && !pending.selected_ready);
    assert!(pending.available_files.is_empty());
    proxy.payloads_enabled.store(true, Ordering::Release);
    let repaired = selected(&restored, &torrent.id);
    assert!(!repaired.ready);
    assert_eq!(
        fs::read(&repaired.available_files[0]).unwrap(),
        torrent.files[0].1
    );
    assert_eq!(
        proxy
            .requests
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([0])
    );
}

fn merkle(hashes: &[[u8; 32]]) -> [u8; 32] {
    let mut level = hashes.to_vec();
    level.resize(level.len().next_power_of_two(), [0; 32]);
    while level.len() > 1 {
        level = level
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| sha256(&pair.concat()))
            .collect();
    }
    level[0]
}

fn v2_fixture(root: &Path, hybrid: bool) -> Torrent {
    fs::create_dir_all(root).unwrap();
    let files = vec![
        (PathBuf::from("a.bin"), payload(9000, 11)),
        (PathBuf::from("empty.bin"), Vec::new()),
        (PathBuf::from("z.bin"), payload(BLOCK * 3 + 49, 29)),
        (PathBuf::from("zz.bin"), payload(BLOCK * 2, 31)),
    ];
    let mut tree = BTreeMap::new();
    let mut encoded_layers = BTreeMap::new();
    let mut joined = Vec::new();
    let mut v1_files = Vec::new();
    for (index, (path, data)) in files.iter().enumerate() {
        let mut leaf = BTreeMap::from([(b"length".to_vec(), Bencode::Int(data.len() as i64))]);
        if !data.is_empty() {
            let hashes: Vec<_> = data.chunks(BLOCK).map(sha256).collect();
            let root = merkle(&hashes);
            leaf.insert(b"pieces root".to_vec(), Bencode::Bytes(root.to_vec()));
            if hashes.len() > 1 {
                encoded_layers.insert(
                    root.to_vec(),
                    Bencode::Bytes(hashes.iter().flatten().copied().collect()),
                );
            }
        }
        tree.insert(
            path.to_str().unwrap().as_bytes().to_vec(),
            Bencode::Dict(BTreeMap::from([(Vec::new(), Bencode::Dict(leaf))])),
        );
        v1_files.push(Bencode::Dict(BTreeMap::from([
            (b"length".to_vec(), Bencode::Int(data.len() as i64)),
            (
                b"path".to_vec(),
                Bencode::List(vec![Bencode::Bytes(
                    path.to_str().unwrap().as_bytes().to_vec(),
                )]),
            ),
        ])));
        joined.extend_from_slice(data);
        if index + 1 != files.len() && !data.len().is_multiple_of(BLOCK) {
            let padding = BLOCK - data.len() % BLOCK;
            joined.resize(joined.len() + padding, 0);
            v1_files.push(Bencode::Dict(BTreeMap::from([
                (b"attr".to_vec(), Bencode::Bytes(b"p".to_vec())),
                (b"length".to_vec(), Bencode::Int(padding as i64)),
                (
                    b"path".to_vec(),
                    Bencode::List(vec![Bencode::Bytes(
                        format!("padding-{index}").into_bytes(),
                    )]),
                ),
            ])));
        }
    }
    let mut info = BTreeMap::from([
        (b"name".to_vec(), Bencode::Bytes(b"V2".to_vec())),
        (b"piece length".to_vec(), Bencode::Int(BLOCK as i64)),
        (b"private".to_vec(), Bencode::Int(1)),
        (b"meta version".to_vec(), Bencode::Int(2)),
        (b"file tree".to_vec(), Bencode::Dict(tree)),
    ]);
    if hybrid {
        info.insert(b"files".to_vec(), Bencode::List(v1_files));
        info.insert(
            b"pieces".to_vec(),
            Bencode::Bytes(joined.chunks(BLOCK).flat_map(sha1).collect()),
        );
    }
    let info = Bencode::Dict(info);
    let id = sha256(&bencode::encode(&info))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let encoded = bencode::encode(&Bencode::Dict(BTreeMap::from([
        (b"info".to_vec(), info),
        (b"piece layers".to_vec(), Bencode::Dict(encoded_layers)),
    ])));
    let path = root.join("v2.torrent");
    fs::write(&path, encoded).unwrap();
    Torrent {
        id,
        path,
        total: files.iter().map(|(_, bytes)| bytes.len()).sum(),
        files: files
            .into_iter()
            .map(|(path, data)| (Path::new("V2").join(path), data))
            .collect(),
    }
}

#[test]
fn v2_and_hybrid_selective_roots_proofs_padding_and_offline_recovery() {
    for (hybrid, peers) in [(false, 1), (false, 4), (true, 4), (true, 1)] {
        let scratch = Scratch::new();
        let torrent = v2_fixture(&scratch.0.join("meta"), hybrid);
        let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
        let proxy = RecordingProxy::open(seed.client.listen_port());
        proxy.payloads_enabled.store(true, Ordering::Release);
        let mut cfg = client_config(&scratch.0.join("client"), true);
        cfg.max_peers = peers;
        let client = Client::open(cfg.clone()).unwrap();
        let source = format!(
            "magnet:?xt=urn:btmh:1220{}&x.pe=127.0.0.1:{}",
            torrent.id, proxy.port
        );
        client
            .ensure_files(&source, &["V2/z.bin".into(), "V2/empty.bin".into()])
            .unwrap();
        let result = selected(&client, &torrent.id);
        assert!(!result.ready);
        assert_eq!(result.available_files.len(), 2);
        assert_eq!(
            fs::read(cfg.data_dir.join(&torrent.id).join("V2/z.bin")).unwrap(),
            torrent.files[2].1
        );
        assert!(!cfg.data_dir.join(&torrent.id).join("V2/a.bin").exists());
        assert!(!cfg.data_dir.join(&torrent.id).join("V2/zz.bin").exists());
        assert_eq!(
            proxy
                .requests
                .lock()
                .unwrap()
                .iter()
                .copied()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([1, 2, 3, 4])
        );
        assert!(bits(&client, &torrent.id).iter().all(|byte| *byte == 0));
        let counter = client.transfer_stats(&torrent.id).unwrap();
        drop(client);
        proxy.wait_idle();
        drop(proxy);
        drop(seed);
        let restored = Client::open(cfg).unwrap();
        let recovered = selected(&restored, &torrent.id);
        assert!(!recovered.ready);
        assert_eq!(recovered.available_files, result.available_files);
        assert_eq!(restored.transfer_stats(&torrent.id).unwrap(), counter);
    }
}

#[test]
fn selection_parser_bounds_checksum_and_legacy_default_are_explicit() {
    let mut control = TorrentControl::new("a".repeat(40), 1).unwrap();
    let mut legacy = control.to_json();
    if let Value::Object(fields) = &mut legacy {
        fields.remove("file_selection");
    }
    assert_eq!(
        TorrentControl::from_json(&legacy).unwrap().file_selection,
        FileSelection::All
    );
    control.file_selection =
        FileSelection::paths(&["Pack/one.bin".into(), "Pack/two.bin".into()]).unwrap();
    assert_eq!(
        TorrentControl::decode(&control.encode().unwrap()).unwrap(),
        control
    );
    let mut corrupted = control.encode().unwrap();
    corrupted[40] ^= 1;
    assert!(TorrentControl::decode(&corrupted).is_err());
    for text in [
        r#"{}"#,
        r#"{"all":false}"#,
        r#"{"all":"true"}"#,
        r#"{"all":true,"indices":[0]}"#,
        r#"{"indices":[]}"#,
        r#"{"indices":[0,0]}"#,
        r#"{"indices":[1.5]}"#,
        r#"{"indices":[-1]}"#,
        r#"{"indices":["1"]}"#,
        r#"{"indices":[100000]}"#,
        r#"{"unknown":true}"#,
    ] {
        assert!(
            SelectionUpdate::from_json(&json::parse(text).unwrap()).is_err(),
            "Accepted {text}"
        );
    }
    for path in [
        "",
        "/absolute",
        "../outside",
        "Pack/./file",
        "Pack//file",
        "Pack/..",
        "Pack\\file",
        "Pack/name:token",
        "Pack/a\n",
    ] {
        assert!(
            FileSelection::paths(&[path.into()]).is_err(),
            "Accepted {path}"
        );
    }
    assert!(FileSelection::paths(&["same".into(), "same".into()]).is_err());
    assert!(
        FileSelection::paths(
            &(0..1025)
                .map(|n| format!("Pack/{n}.bin"))
                .collect::<Vec<_>>()
        )
        .is_err()
    );
    assert!(FileSelection::from_json(&json::parse(r#"["one",false]"#).unwrap()).is_err());
}
