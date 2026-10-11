//! Synthetic loopback proofs for manual seed-only reuse; no public media.
#[allow(dead_code)]
mod transfer_support;
use mynou::{
    bencode, config,
    engine::Engine,
    json::{self, Value},
    net::HttpClient,
    server::Api,
    torrent::{Client, CrossSeedRequest, SelectionUpdate},
};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use transfer_support::*;

struct ApiGuard {
    engine: Arc<Engine>,
    worker: Option<JoinHandle<mynou::Result<()>>>,
}
impl Drop for ApiGuard {
    fn drop(&mut self) {
        self.engine.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap().unwrap();
        }
    }
}

fn request(torrent: &Torrent, mapping: Option<Vec<usize>>) -> CrossSeedRequest {
    CrossSeedRequest {
        metadata: fs::read(&torrent.path).unwrap(),
        mapping,
    }
}
fn fixture_bytes(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}
fn scratch() -> Scratch {
    let mut scratch = Scratch::new();
    // macOS temp_dir can contain /var -> /private/var; fixtures need physical roots.
    scratch.0 = scratch.0.canonicalize().unwrap();
    scratch
}
fn failed(client: &Client, id: &str) {
    wait(|| client.transfer(id).unwrap().get("failed") == Some(&Value::Bool(true)));
    let snapshot = client.transfer(id).unwrap();
    assert_eq!(snapshot.get("ready"), Some(&Value::Bool(false)));
    assert_eq!(snapshot.get("selected_ready"), Some(&Value::Bool(false)));
    assert_eq!(client.transfer_stats(id).unwrap().0, 0);
    assert!(client.check(id).is_err());
    assert!(
        client
            .statuses()
            .unwrap()
            .iter()
            .find(|status| status.id == id)
            .unwrap()
            .available_files
            .is_empty()
    );
}

struct Tracker {
    url: String,
    events: Arc<Mutex<Vec<String>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Tracker {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/announce", listener.local_addr().unwrap());
        let events = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let record = events.clone();
        let stop = stopped.clone();
        let worker = thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(3));
                        continue;
                    }
                    Err(error) => panic!("loopback tracker: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                record.lock().unwrap().push(first);
                let mut size = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    size += line.len();
                    assert!(size < 16_384);
                    if line == "\r\n" {
                        break;
                    }
                    assert!(!line.is_empty());
                }
                let body = b"d8:intervali30e5:peers0:e";
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        Self {
            url,
            events,
            stopped,
            worker: Some(worker),
        }
    }
    fn attach(&self, torrent: &Torrent) -> Vec<u8> {
        let bencode::Value::Dict(mut fields) =
            bencode::parse(&fs::read(&torrent.path).unwrap()).unwrap()
        else {
            panic!("torrent");
        };
        fields.insert(
            b"announce".to_vec(),
            bencode::Value::Bytes(self.url.as_bytes().to_vec()),
        );
        bencode::encode(&bencode::Value::Dict(fields))
    }
}
impl Drop for Tracker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

#[test]
fn mapped_cross_file_padding_reuse_uploads_and_survives_restart_without_media_writes() {
    let scratch = scratch();
    let first = fixture_bytes(11_000);
    let second = fixture_bytes(5_001);
    let source = Torrent::multiple_with_padding(
        &scratch.0,
        "Source",
        vec![
            (PathBuf::from("a.bin"), first.clone(), false),
            (PathBuf::from("pad"), vec![0; 1000], true),
            (PathBuf::from("b.bin"), second.clone(), false),
            (PathBuf::from("empty.bin"), Vec::new(), false),
        ],
    );
    let candidate = Torrent::multiple_with_padding(
        &scratch.0,
        "Renamed",
        vec![
            (PathBuf::from("new-b.bin"), second, false),
            (PathBuf::from("pad"), vec![0; 1024], true),
            (PathBuf::from("new-a.bin"), first, false),
            (PathBuf::from("zero.bin"), Vec::new(), false),
        ],
    );
    let tracker = Tracker::new();
    let seed = Seeder::open(&scratch.0.join("seed"), &[&source]);
    seed.client.pause(&source.id).unwrap();
    let cfg = client_config(&seed.root, true);
    let before: Vec<_> = seed
        .client
        .check(&source.id)
        .unwrap()
        .files
        .iter()
        .map(|path| {
            (
                fs::read(path).unwrap(),
                fs::metadata(path).unwrap().modified().unwrap(),
            )
        })
        .collect();
    let query = CrossSeedRequest {
        metadata: tracker.attach(&candidate),
        mapping: Some(vec![2, 0, 3]),
    };
    assert!(
        seed.client
            .cross_seed(&source.id, &request(&candidate, None))
            .is_err()
    );
    let status = seed.client.cross_seed(&source.id, &query).unwrap();
    assert_eq!(status.id, candidate.id);
    let complete = ready(&seed.client, &candidate.id);
    assert!(complete.available_files.is_empty());
    assert_eq!(
        seed.client
            .transfer(&candidate.id)
            .unwrap()
            .get("seed_only"),
        Some(&Value::Bool(true))
    );
    assert!(!cfg.data_dir.join(&candidate.id).exists());
    assert!(
        !cfg.state_dir
            .join(format!("{}.torrent", candidate.id))
            .exists()
    );
    assert!(
        !cfg.state_dir
            .join(format!("{}.source", candidate.id))
            .exists()
    );
    wait(|| !tracker.events.lock().unwrap().is_empty());
    assert!(
        tracker
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event.contains("left=0"))
    );
    let leecher = Client::open(client_config(&scratch.0.join("receiver"), false)).unwrap();
    leecher
        .ensure(&candidate.magnet(seed.client.listen_port()))
        .unwrap();
    candidate.assert_files(&ready(&leecher, &candidate.id));
    assert_eq!(seed.client.transfer_stats(&candidate.id).unwrap().0, 0);
    assert!(seed.client.transfer_stats(&candidate.id).unwrap().1 > 0);
    assert!(seed.client.controls(&source.id).unwrap().user_paused);
    for (path, (bytes, modified)) in seed
        .client
        .check(&source.id)
        .unwrap()
        .files
        .iter()
        .zip(&before)
    {
        assert_eq!(&fs::read(path).unwrap(), bytes);
        assert_eq!(&fs::metadata(path).unwrap().modified().unwrap(), modified);
    }
    seed.client.pause(&candidate.id).unwrap();
    drop(leecher);
    drop(seed);
    let seed = Client::open(cfg.clone()).unwrap();
    assert!(!seed.check(&candidate.id).unwrap().ready);
    assert!(seed.controls(&candidate.id).unwrap().user_paused);
    let resumed = seed.resume(&candidate.id).unwrap();
    assert!(!resumed.ready);
    ready(&seed, &candidate.id);
    assert!(!cfg.data_dir.join(&candidate.id).exists());
    assert!(seed.ensure(candidate.path.to_str().unwrap()).is_err());
    assert!(seed.require_all(&candidate.id).is_err());
    assert!(
        seed.require_files(&candidate.id, &["Renamed/new-b.bin".into()])
            .is_err()
    );
    assert!(
        seed.require_bound_files(&candidate.id, &["Renamed/new-b.bin".into()], &candidate.id)
            .is_err()
    );
    assert!(
        seed.select_files(&candidate.id, &SelectionUpdate::All)
            .is_err()
    );
    assert!(seed.cross_seed(&source.id, &query).is_err());
    assert_eq!(seed.transfer_stats(&candidate.id).unwrap().0, 0);
}

#[test]
fn partial_match_never_announces_or_acquires_and_resume_reverifies() {
    let scratch = scratch();
    let bytes = fixture_bytes(BLOCK * 16 + 37);
    let source = Torrent::single(&scratch.0, "original.bin", bytes.clone());
    let good = Torrent::single(&scratch.0, "candidate.bin", bytes.clone());
    let mut wrong = bytes.clone();
    wrong[BLOCK * 15] ^= 1;
    let bad = Torrent::single(&scratch.0, "mismatch.bin", wrong);
    let seed = Seeder::open(&scratch.0.join("seed"), &[&source]);
    let cfg = client_config(&seed.root, true);
    let tracker = Tracker::new();
    let bad_request = CrossSeedRequest {
        metadata: tracker.attach(&bad),
        mapping: None,
    };
    seed.client.cross_seed(&source.id, &bad_request).unwrap();
    failed(&seed.client, &bad.id);
    assert!(tracker.events.lock().unwrap().is_empty());
    assert!(!cfg.data_dir.join(&bad.id).exists());
    assert!(seed.client.check(&bad.id).unwrap_err().contains("mismatch"));
    seed.client.resume(&bad.id).unwrap();
    failed(&seed.client, &bad.id);
    assert!(tracker.events.lock().unwrap().is_empty());
    let path = &seed.client.check(&source.id).unwrap().files[0];
    let mut altered = bytes.clone();
    altered[BLOCK] ^= 1;
    fs::write(path, &altered).unwrap();
    seed.client
        .cross_seed(&source.id, &request(&good, None))
        .unwrap();
    failed(&seed.client, &good.id);
    assert_eq!(fs::read(path).unwrap(), altered);
    fs::write(path, &bytes).unwrap();
    seed.client.resume(&good.id).unwrap();
    ready(&seed.client, &good.id);
    assert_eq!(seed.client.transfer_stats(&good.id).unwrap().0, 0);
    assert!(!cfg.data_dir.join(&good.id).exists());
}

#[test]
#[cfg(unix)]
fn unsafe_or_truncated_sources_fail_without_repairing_or_linking() {
    use std::os::unix::fs::symlink;
    for attack in [
        "file-link",
        "directory-link",
        "hardlink",
        "truncated",
        "missing",
    ] {
        let scratch = scratch();
        let data = fixture_bytes(BLOCK + 17);
        let source = Torrent::single(&scratch.0, "original.bin", data.clone());
        let target = Torrent::single(&scratch.0, "target.bin", data);
        let seed = Seeder::open(&scratch.0.join("seed"), &[&source]);
        let cfg = client_config(&seed.root, true);
        let path = seed.client.check(&source.id).unwrap().files[0].clone();
        let elsewhere = scratch.0.join("external");
        match attack {
            "file-link" => {
                fs::rename(&path, &elsewhere).unwrap();
                symlink(&elsewhere, &path).unwrap();
            }
            "directory-link" => {
                let base = cfg.data_dir.join(&source.id);
                fs::rename(&base, &elsewhere).unwrap();
                symlink(&elsewhere, &base).unwrap();
            }
            "hardlink" => fs::hard_link(&path, &elsewhere).unwrap(),
            "truncated" => fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(1)
                .unwrap(),
            "missing" => fs::remove_file(&path).unwrap(),
            _ => unreachable!(),
        }
        seed.client
            .cross_seed(&source.id, &request(&target, None))
            .unwrap();
        failed(&seed.client, &target.id);
        assert!(!cfg.data_dir.join(&target.id).exists());
        if attack == "truncated" {
            assert_eq!(fs::metadata(&path).unwrap().len(), 1);
        }
        if attack == "missing" {
            assert!(!path.exists());
        }
    }
}

#[test]
fn corrupt_or_conflicting_seed_state_refuses_restart_before_any_download() {
    for corruption in [
        "damaged",
        "ordinary-metadata",
        "ordinary-source",
        "missing-source",
        "wrong-id",
        "missing-control",
    ] {
        let scratch = scratch();
        let bytes = fixture_bytes(BLOCK + 1);
        let source = Torrent::single(&scratch.0, "source.bin", bytes.clone());
        let target = Torrent::single(&scratch.0, "target.bin", bytes);
        let seed = Seeder::open(&scratch.0.join("seed"), &[&source]);
        let cfg = client_config(&seed.root, true);
        seed.client
            .cross_seed(&source.id, &request(&target, None))
            .unwrap();
        ready(&seed.client, &target.id);
        seed.client.pause(&target.id).unwrap();
        drop(seed);
        let primary = cfg.state_dir.join(format!("{}.seed", target.id));
        match corruption {
            "damaged" => fs::write(&primary, b"not a seed record").unwrap(),
            "ordinary-metadata" => fs::write(
                cfg.state_dir.join(format!("{}.torrent", target.id)),
                fs::read(&target.path).unwrap(),
            )
            .unwrap(),
            "ordinary-source" => fs::write(
                cfg.state_dir.join(format!("{}.source", target.id)),
                format!("magnet:?xt=urn:btih:{}", target.id),
            )
            .unwrap(),
            "missing-source" => {
                fs::remove_file(cfg.state_dir.join(format!("{}.torrent", source.id))).unwrap();
                fs::remove_file(cfg.state_dir.join(format!("{}.source", source.id))).unwrap();
            }
            "wrong-id" => {
                fs::rename(
                    &primary,
                    cfg.state_dir.join(format!("{}.seed", "f".repeat(40))),
                )
                .unwrap();
            }
            "missing-control" => {
                fs::remove_file(cfg.state_dir.join(format!("{}.control", target.id))).unwrap()
            }
            _ => unreachable!(),
        }
        let reopened = Client::open(cfg.clone());
        assert!(reopened.is_err(), "{corruption}");
        assert!(!cfg.data_dir.join(&target.id).exists());
    }
}

fn message(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    loop {
        let mut size = [0; 4];
        stream.read_exact(&mut size)?;
        let size = u32::from_be_bytes(size) as usize;
        if size == 0 {
            continue;
        }
        assert!(size < 65_536);
        let mut data = vec![0; size];
        stream.read_exact(&mut data)?;
        return Ok(data);
    }
}
fn peer(client: &Client, id: &str, extensions: bool) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", client.listen_port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut handshake = [0; 68];
    handshake[0] = 19;
    handshake[1..20].copy_from_slice(b"BitTorrent protocol");
    if extensions {
        handshake[25] = 0x10;
    }
    for index in 0..20 {
        handshake[28 + index] = u8::from_str_radix(&id[2 * index..2 * index + 2], 16).unwrap();
    }
    handshake[48..].fill(b'X');
    stream.write_all(&handshake).unwrap();
    stream.read_exact(&mut handshake).unwrap();
    if extensions {
        let extended = message(&mut stream).unwrap();
        assert_eq!(&extended[..2], &[20, 0]);
        let bencode::Value::Dict(fields) = bencode::parse(&extended[2..]).unwrap() else {
            panic!("handshake");
        };
        let bencode::Value::Dict(methods) = fields.get(b"m".as_slice()).unwrap() else {
            panic!("methods");
        };
        assert!(!methods.contains_key(b"ut_pex".as_slice()));
    }
    assert_eq!(message(&mut stream).unwrap()[0], 5);
    stream.write_all(&[0, 0, 0, 1, 2]).unwrap();
    assert_eq!(message(&mut stream).unwrap(), [1]);
    stream
}
fn block(stream: &mut TcpStream, index: u32) -> std::io::Result<Vec<u8>> {
    let mut request = vec![0, 0, 0, 13, 6];
    request.extend_from_slice(&index.to_be_bytes());
    request.extend_from_slice(&0_u32.to_be_bytes());
    request.extend_from_slice(&(BLOCK as u32).to_be_bytes());
    stream.write_all(&request)?;
    message(stream)
}

#[test]
fn private_seed_upload_change_revokes_all_peers_and_requires_new_proof() {
    let scratch = scratch();
    let bytes = fixture_bytes(BLOCK * 2 + 17);
    let source = Torrent::single(&scratch.0, "source.bin", bytes.clone());
    let target = Torrent::single(&scratch.0, "private-target.bin", bytes.clone());
    let mut cfg = client_config(&scratch.0.join("seed"), true);
    cfg.pex = true;
    for (path, bytes) in &source.files {
        let destination = cfg.data_dir.join(&source.id).join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, bytes).unwrap();
    }
    let seed = Seeder {
        client: Client::open(cfg).unwrap(),
        root: scratch.0.join("seed"),
    };
    seed.client.ensure(source.path.to_str().unwrap()).unwrap();
    ready(&seed.client, &source.id);
    seed.client
        .cross_seed(&source.id, &request(&target, None))
        .unwrap();
    ready(&seed.client, &target.id);
    let mut first = peer(&seed.client, &target.id, true);
    let valid = block(&mut first, 0).unwrap();
    assert_eq!(valid[0], 7);
    assert_eq!(&valid[9..], &bytes[..BLOCK]);
    let mut second = peer(&seed.client, &target.id, false);
    let path = &seed.client.check(&source.id).unwrap().files[0];
    let mut corrupt = bytes.clone();
    corrupt[BLOCK + 1] ^= 1;
    fs::write(path, &corrupt).unwrap();
    assert!(block(&mut second, 1).is_err());
    failed(&seed.client, &target.id);
    assert!(block(&mut first, 0).is_err());
    let resumed = seed.client.resume(&target.id).unwrap();
    assert!(!resumed.ready);
    failed(&seed.client, &target.id);
    fs::write(path, &bytes).unwrap();
    seed.client.resume(&target.id).unwrap();
    ready(&seed.client, &target.id);
    assert_eq!(seed.client.transfer_stats(&target.id).unwrap().0, 0);
}

#[test]
fn raising_a_seed_limit_requires_a_new_proof_before_advertising_changed_files() {
    let scratch = scratch();
    let bytes = fixture_bytes(BLOCK + 17);
    let source = Torrent::single(&scratch.0, "source.bin", bytes.clone());
    let target = Torrent::single(&scratch.0, "limited-target.bin", bytes.clone());
    let tracker = Tracker::new();
    let seed = Seeder::open(&scratch.0.join("seed"), &[&source]);
    seed.client
        .cross_seed(
            &source.id,
            &CrossSeedRequest {
                metadata: tracker.attach(&target),
                mapping: None,
            },
        )
        .unwrap();
    ready(&seed.client, &target.id);
    seed.client
        .set_policy(
            &target.id,
            Some(mynou::torrent::TransferPolicy {
                seed_time_secs: Some(1),
                ..Default::default()
            }),
        )
        .unwrap();
    wait(|| {
        seed.client
            .transfer(&target.id)
            .unwrap()
            .get("seed_limited")
            == Some(&Value::Bool(true))
    });
    let path = seed.client.check(&source.id).unwrap().files[0].clone();
    let mut corrupt = bytes.clone();
    corrupt[0] ^= 1;
    fs::write(&path, &corrupt).unwrap();
    let before = tracker
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| !event.contains("event=stopped"))
        .count();
    seed.client.set_policy(&target.id, None).unwrap();
    failed(&seed.client, &target.id);
    let after = tracker
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| !event.contains("event=stopped"))
        .count();
    assert_eq!(after, before);
    assert_eq!(fs::read(&path).unwrap(), corrupt);
    fs::write(&path, &bytes).unwrap();
    seed.client.resume(&target.id).unwrap();
    ready(&seed.client, &target.id);
}

#[test]
fn cached_seed_piece_cannot_hide_missing_or_modified_source_files() {
    for change in ["missing", "modified", "hardlink"] {
        let scratch = scratch();
        let bytes = fixture_bytes(BLOCK);
        let source = Torrent::single(&scratch.0, "source.bin", bytes.clone());
        let target = Torrent::single(&scratch.0, "cached-target.bin", bytes.clone());
        let seed = Seeder::open(&scratch.0.join("seed"), &[&source]);
        seed.client
            .cross_seed(&source.id, &request(&target, None))
            .unwrap();
        ready(&seed.client, &target.id);
        let mut connection = peer(&seed.client, &target.id, false);
        let piece = block(&mut connection, 0).unwrap();
        assert_eq!(&piece[9..], &bytes);
        let path = seed.client.check(&source.id).unwrap().files[0].clone();
        match change {
            "missing" => fs::remove_file(&path).unwrap(),
            "modified" => {
                let mut changed = bytes.clone();
                changed[0] ^= 1;
                fs::write(&path, changed).unwrap();
            }
            "hardlink" => fs::hard_link(&path, scratch.0.join("extra-link.bin")).unwrap(),
            _ => unreachable!(),
        }
        assert!(block(&mut connection, 0).is_err(), "{change}");
        failed(&seed.client, &target.id);
    }
}

#[test]
fn api_authorization_and_cli_register_only_the_selected_readonly_seed() {
    let scratch = scratch();
    let bytes = fixture_bytes(BLOCK + 17);
    let source = Torrent::single(&scratch.0, "source.bin", bytes.clone());
    let target = Torrent::single(&scratch.0, "target.bin", bytes);
    let mut settings = configuration();
    let Value::Object(downloads) = settings.get_mut("downloads").unwrap() else {
        panic!("downloads must be an object");
    };
    downloads.insert("seed".into(), true.into());
    let cfg = config::from_json(&settings, &scratch.0).unwrap();
    fs::create_dir_all(&cfg.downloads.state_dir).unwrap();
    for (path, bytes) in &source.files {
        let destination = cfg.downloads.data_dir.join(&source.id).join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, bytes).unwrap();
    }
    fs::write(
        cfg.downloads
            .state_dir
            .join(format!("{}.torrent", source.id)),
        fs::read(&source.path).unwrap(),
    )
    .unwrap();
    let engine = Engine::open(cfg.clone()).unwrap();
    wait(|| engine.transfer(&source.id).unwrap().get("ready") == Some(&Value::Bool(true)));
    let api = Api::bind(engine.clone(), TOKEN.into()).unwrap();
    let origin = format!("http://{}", api.address().unwrap());
    settings.insert("listen", api.address().unwrap().to_string());
    let config_path = scratch.0.join("mynou.json");
    fs::write(&config_path, json::stringify(&settings)).unwrap();
    let _server = ApiGuard {
        engine: engine.clone(),
        worker: Some(thread::spawn(move || api.run())),
    };
    let mut body = Value::object();
    body.insert(
        "metadata_hex",
        fs::read(&target.path)
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    );
    let route = format!("{origin}/api/transfers/{}/cross-seed", source.id);
    let http = HttpClient::new()
        .without_proxy()
        .with_timeout(Duration::from_secs(5));
    assert_eq!(
        http.request("POST", &route, &[], json::stringify(&body).as_bytes())
            .unwrap()
            .status,
        401
    );
    body.insert("unknown", true);
    let headers = vec![("Authorization".into(), format!("Bearer {TOKEN}"))];
    assert_eq!(
        http.request("POST", &route, &headers, json::stringify(&body).as_bytes())
            .unwrap()
            .status,
        400
    );
    assert_eq!(engine.transfers().unwrap().as_array().unwrap().len(), 1);
    let output = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args([
            "cross-seed",
            &source.id,
            "--torrent",
            target.path.to_str().unwrap(),
            "--config",
            config_path.to_str().unwrap(),
        ])
        .env(&cfg.api_token_env, TOKEN)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot = json::parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    assert_eq!(
        snapshot.get("id").unwrap().as_str(),
        Some(target.id.as_str())
    );
    assert_eq!(snapshot.get("seed_only"), Some(&Value::Bool(true)));
    wait(|| engine.transfer(&target.id).unwrap().get("ready") == Some(&Value::Bool(true)));
    assert!(!cfg.downloads.data_dir.join(&target.id).exists());
}

#[test]
fn malformed_mapping_metadata_and_disabled_optin_have_no_queue_side_effects() {
    for value in [
        r#"{}"#,
        r#"{"metadata_hex":"x0"}"#,
        r#"{"metadata_hex":"00","mapping":[1.5]}"#,
        r#"{"metadata_hex":"00","mapping":[-1]}"#,
        r#"{"metadata_hex":"00","mapping":null}"#,
    ] {
        assert!(CrossSeedRequest::from_json(&json::parse(value).unwrap()).is_err());
    }
    let scratch = scratch();
    let source = Torrent::single(&scratch.0, "source.bin", fixture_bytes(BLOCK));
    let target = Torrent::single(&scratch.0, "target.bin", fixture_bytes(BLOCK));
    let seed = Seeder::open(&scratch.0.join("seed"), &[&source]);
    for hybrid in [false, true] {
        use bencode::Value as B;
        use std::collections::BTreeMap;
        let bytes = fixture_bytes(BLOCK);
        let leaf = B::Dict(BTreeMap::from([
            (b"length".to_vec(), B::Int(BLOCK as i64)),
            (
                b"pieces root".to_vec(),
                B::Bytes(mynou::crypto::sha256(&bytes).to_vec()),
            ),
        ]));
        let tree = B::Dict(BTreeMap::from([(
            b"file.bin".to_vec(),
            B::Dict(BTreeMap::from([(Vec::new(), leaf)])),
        )]));
        let mut info = BTreeMap::from([
            (b"name".to_vec(), B::Bytes(b"Pack".to_vec())),
            (b"piece length".to_vec(), B::Int(BLOCK as i64)),
            (b"meta version".to_vec(), B::Int(2)),
            (b"file tree".to_vec(), tree),
        ]);
        if hybrid {
            info.insert(
                b"files".to_vec(),
                B::List(vec![B::Dict(BTreeMap::from([
                    (b"length".to_vec(), B::Int(BLOCK as i64)),
                    (
                        b"path".to_vec(),
                        B::List(vec![B::Bytes(b"file.bin".to_vec())]),
                    ),
                ]))]),
            );
            info.insert(
                b"pieces".to_vec(),
                B::Bytes(mynou::crypto::sha1(&bytes).to_vec()),
            );
        }
        let metadata = bencode::encode(&B::Dict(BTreeMap::from([(
            b"info".to_vec(),
            B::Dict(info),
        )])));
        let error = seed
            .client
            .cross_seed(
                &source.id,
                &CrossSeedRequest {
                    metadata,
                    mapping: None,
                },
            )
            .unwrap_err();
        assert!(error.contains("v1 torrents only"), "{error}");
    }
    for mapping in [vec![], vec![1], vec![0, 0]] {
        assert!(
            seed.client
                .cross_seed(&source.id, &request(&target, Some(mapping)))
                .is_err()
        );
    }
    assert!(
        seed.client
            .cross_seed(
                &source.id,
                &CrossSeedRequest {
                    metadata: vec![0; 262145],
                    mapping: None
                }
            )
            .is_err()
    );
    let disabled = Client::open(client_config(&scratch.0.join("disabled"), false)).unwrap();
    assert!(
        disabled
            .cross_seed(&source.id, &request(&target, None))
            .is_err()
    );
    assert!(disabled.transfers().unwrap().as_array().unwrap().is_empty());
    assert_eq!(
        seed.client.transfers().unwrap().as_array().unwrap().len(),
        1
    );
}
