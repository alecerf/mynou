use mynou::{
    bencode::{Value, encode},
    crypto::{sha1, sha256},
    torrent::{Client, DownloadConfig, DownloadStatus},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const BLOCK: usize = 16 * 1024;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mynou-torrent-{}-{n}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("directory");
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn d(items: &[(&[u8], Value)]) -> Value {
    Value::Dict(items.iter().map(|(k, v)| (k.to_vec(), v.clone())).collect())
}
fn b(v: &str) -> Value {
    Value::Bytes(v.as_bytes().to_vec())
}
fn hex(v: &[u8]) -> String {
    v.iter().map(|b| format!("{b:02x}")).collect()
}
fn pair(a: [u8; 32], b: [u8; 32]) -> [u8; 32] {
    let mut v = a.to_vec();
    v.extend_from_slice(&b);
    sha256(&v)
}
fn merkle(hashes: &[[u8; 32]]) -> [u8; 32] {
    let mut h = hashes.to_vec();
    h.resize(h.len().next_power_of_two(), [0; 32]);
    while h.len() > 1 {
        h = h
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| pair(p[0], p[1]))
            .collect();
    }
    h[0]
}
fn fixture(data: &[u8], v1: bool, v2: bool) -> (Vec<u8>, String) {
    fixture_with_piece(data, v1, v2, BLOCK)
}
fn fixture_with_piece(data: &[u8], v1: bool, v2: bool, piece: usize) -> (Vec<u8>, String) {
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), b("movie.bin"));
    info.insert(b"piece length".to_vec(), Value::Int(piece as i64));
    info.insert(b"private".to_vec(), Value::Int(1));
    if v1 {
        info.insert(b"length".to_vec(), Value::Int(data.len() as i64));
        info.insert(
            b"pieces".to_vec(),
            Value::Bytes(data.chunks(piece).flat_map(sha1).collect()),
        );
    }
    let hashes: Vec<_> = data
        .chunks(piece)
        .map(|chunk| {
            let mut hashes: Vec<_> = chunk.chunks(BLOCK).map(sha256).collect();
            let blocks = if data.len() <= piece {
                hashes.len().next_power_of_two()
            } else {
                piece / BLOCK
            };
            hashes.resize(blocks, [0; 32]);
            merkle(&hashes)
        })
        .collect();
    let mut padding = [0; 32];
    for _ in 0..(piece / BLOCK).ilog2() {
        padding = pair(padding, padding);
    }
    let mut tree = hashes.clone();
    tree.resize(tree.len().next_power_of_two(), padding);
    let root = merkle(&tree);
    if v2 {
        info.insert(b"meta version".to_vec(), Value::Int(2));
        info.insert(
            b"file tree".to_vec(),
            d(&[(
                b"movie.bin",
                d(&[(
                    b"",
                    d(&[
                        (b"length", Value::Int(data.len() as i64)),
                        (b"pieces root", Value::Bytes(root.to_vec())),
                    ]),
                )]),
            )]),
        );
    }
    let info = Value::Dict(info);
    let raw = encode(&info);
    let id = if v2 {
        hex(&sha256(&raw))
    } else {
        hex(&sha1(&raw))
    };
    let mut top = BTreeMap::new();
    top.insert(b"info".to_vec(), info);
    if v2 {
        top.insert(
            b"piece layers".to_vec(),
            d(&[(
                &root,
                Value::Bytes(hashes.iter().flat_map(|h| h.iter().copied()).collect()),
            )]),
        );
    }
    (encode(&Value::Dict(top)), id)
}
fn config(root: &Path, seed: bool) -> DownloadConfig {
    DownloadConfig {
        data_dir: root.join("downloads"),
        state_dir: root.join("state"),
        listen_port: 0,
        seed,
        dht: false,
        pex: true,
        max_active: 2,
    }
}
fn ready(client: &Client, id: &str) -> DownloadStatus {
    let start = Instant::now();
    loop {
        let status = client.check(id).expect("status");
        if status.ready {
            return status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "Download is not ready: {} / {}",
            status.progress,
            status.message
        );
        thread::sleep(Duration::from_millis(20));
    }
}
fn transfer(v1: bool, v2: bool) {
    let scratch = Scratch::new();
    let data: Vec<u8> = (0..BLOCK * 4 + 131)
        .map(|n| ((n * 137 + 19) % 251) as u8)
        .collect();
    let (torrent, id) = fixture(&data, v1, v2);
    let source = scratch.0.join("source.torrent");
    fs::write(&source, &torrent).expect("torrent");
    let seed_root = scratch.0.join("seeder");
    fs::create_dir_all(seed_root.join("downloads").join(&id)).expect("seed dir");
    fs::write(
        seed_root.join("downloads").join(&id).join("movie.bin"),
        &data,
    )
    .expect("seed");
    let seeder = Client::open(config(&seed_root, true)).expect("seeder");
    let seed_status = seeder
        .ensure(source.to_str().expect("path"))
        .expect("seed ensure");
    assert_eq!(seed_status.id, id);
    ready(&seeder, &id);
    let xt = if v2 {
        format!("urn:btmh:1220{id}")
    } else {
        format!("urn:btih:{id}")
    };
    let magnet = format!("magnet:?xt={xt}&x.pe=127.0.0.1:{}", seeder.listen_port());
    let download_root = scratch.0.join("client");
    let client = Client::open(config(&download_root, false)).expect("client");
    let status = client.ensure(&magnet).expect("magnet");
    assert_eq!(status.id, id);
    let status = ready(&client, &id);
    assert_eq!(status.files.len(), 1);
    assert_eq!(fs::read(&status.files[0]).expect("data"), data);
    drop(client);
    drop(seeder);
    let resumed = Client::open(config(&download_root, false)).expect("resume");
    let status = ready(&resumed, &id);
    assert_eq!(fs::read(&status.files[0]).expect("resumed data"), data);
}
#[test]
fn v1_magnet_real_transfer_and_offline_resume() {
    transfer(true, false)
}
#[test]
fn v2_magnet_merkle_proofs_and_offline_resume() {
    transfer(false, true)
}
#[test]
fn hybrid_v2_magnet_dual_hash_and_offline_resume() {
    transfer(true, true)
}

#[test]
fn v2_large_pieces_and_leaf_hash_proofs_use_the_file_merkle_root() {
    use std::{
        io::{Read, Write},
        net::TcpStream,
    };
    let scratch = Scratch::new();
    let piece = 128 * 1024;
    let data: Vec<u8> = (0..piece * 2 + 9000).map(|n| (n % 251) as u8).collect();
    let (torrent, id) = fixture_with_piece(&data, false, true, piece);
    let source = scratch.0.join("v2.torrent");
    fs::write(&source, &torrent).expect("torrent");
    let root = scratch.0.join("seed");
    let file = root.join("downloads").join(&id).join("movie.bin");
    fs::create_dir_all(file.parent().expect("parent")).expect("dir");
    fs::write(file, &data).expect("data");
    let seed = Client::open(config(&root, true)).expect("seed");
    seed.ensure(source.to_str().expect("path")).expect("ensure");
    ready(&seed, &id);
    let target = scratch.0.join("target");
    let client = Client::open(config(&target, false)).expect("client");
    client
        .ensure(&format!(
            "magnet:?xt=urn:btmh:1220{id}&x.pe=127.0.0.1:{}",
            seed.listen_port()
        ))
        .expect("ensure");
    let status = ready(&client, &id);
    assert_eq!(fs::read(&status.files[0]).expect("received"), data);
    let meta = mynou::bencode::parse(&torrent).expect("meta");
    let file_root = meta
        .get(b"info")
        .and_then(|v| v.get(b"file tree"))
        .and_then(|v| v.get(b"movie.bin"))
        .and_then(|v| v.get(b""))
        .and_then(|v| v.get(b"pieces root"))
        .and_then(Value::as_bytes)
        .expect("root");
    let info = mynou::bencode::parse_info_raw(&torrent).expect("info");
    let hash = sha256(&info);
    let mut stream = TcpStream::connect(("127.0.0.1", seed.listen_port())).expect("peer");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout");
    let mut hs = [0; 68];
    hs[0] = 19;
    hs[1..20].copy_from_slice(b"BitTorrent protocol");
    hs[28..48].copy_from_slice(&hash[..20]);
    hs[48..].copy_from_slice(&[11; 20]);
    stream.write_all(&hs).expect("handshake");
    stream.read_exact(&mut hs).expect("response");
    let height = data.len().div_ceil(BLOCK).next_power_of_two().ilog2();
    let mut req = file_root.to_vec();
    req.extend_from_slice(&0u32.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes());
    req.extend_from_slice(&2u32.to_be_bytes());
    req.extend_from_slice(&(height - 1).to_be_bytes());
    stream.write_all(&49u32.to_be_bytes()).expect("length");
    stream.write_all(&[21]).expect("id");
    stream.write_all(&req).expect("request");
    loop {
        let mut n = [0; 4];
        stream.read_exact(&mut n).expect("length");
        let n = u32::from_be_bytes(n) as usize;
        assert!(n < 64 * 1024);
        let mut response = vec![0; n];
        stream.read_exact(&mut response).expect("message");
        if response.first() != Some(&22) {
            continue;
        }
        assert_eq!(&response[1..49], &req);
        let mut left = [0; 32];
        left.copy_from_slice(&response[49..81]);
        let mut right = [0; 32];
        right.copy_from_slice(&response[81..113]);
        let mut current = pair(left, right);
        for sibling in response[113..].as_chunks::<32>().0 {
            let mut s = [0; 32];
            s.copy_from_slice(sibling);
            current = pair(current, s);
        }
        assert_eq!(current.as_slice(), file_root);
        break;
    }
}
#[test]
fn malicious_paths_and_hashes_are_rejected() {
    let scratch = Scratch::new();
    let client = Client::open(config(&scratch.0, false)).expect("client");
    assert!(
        client
            .ensure("magnet:?xt=urn:btih:0000000000000000000000000000000000000000")
            .is_err()
    );
    let info = d(&[
        (b"name", b("../escape")),
        (b"piece length", Value::Int(BLOCK as i64)),
        (b"length", Value::Int(0)),
        (b"pieces", Value::Bytes(Vec::new())),
    ]);
    let source = scratch.0.join("invalid.torrent");
    fs::write(&source, encode(&d(&[(b"info", info)]))).expect("torrent");
    assert!(client.ensure(source.to_str().expect("path")).is_err());
}

#[test]
fn pending_magnet_and_cancel_are_durable_and_resume_on_ensure() {
    let scratch = Scratch::new();
    let source = "magnet:?xt=urn:btih:1111111111111111111111111111111111111111&x.pe=127.0.0.1:1";
    let client = Client::open(config(&scratch.0, false)).expect("client");
    let id = client.ensure(source).expect("queue").id;
    client.cancel(&id).expect("pause");
    assert_eq!(
        client.check(&id).expect("paused").message,
        "Download paused"
    );
    drop(client);
    let client = Client::open(config(&scratch.0, false)).expect("restart");
    assert_eq!(
        client.check(&id).expect("durable queue").message,
        "Download paused"
    );
    assert_eq!(client.resume(&id).expect("cached resume").id, id);
    assert!(
        !scratch
            .0
            .join("state")
            .join(format!("{id}.paused"))
            .exists()
    );
    assert!(!client.check(&id).expect("status").ready);
}

#[test]
fn native_client_drop_cancels_a_slow_incoming_handshake() {
    use std::{io::Write, net::TcpStream};
    let scratch = Scratch::new();
    let client = Client::open(config(&scratch.0, false)).expect("client");
    let mut peer = TcpStream::connect(("127.0.0.1", client.listen_port())).expect("peer");
    peer.write_all(&[19]).expect("start handshake");
    thread::sleep(Duration::from_millis(40));
    let started = Instant::now();
    drop(client);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "Stopping must interrupt the incomplete handshake"
    );
}

#[cfg(unix)]
#[test]
fn symlink_download_and_symlink_metadata_are_rejected() {
    use std::os::unix::fs::symlink;
    let scratch = Scratch::new();
    let data = b"safe";
    let (torrent, id) = fixture(data, true, false);
    let source = scratch.0.join("torrent");
    fs::write(&source, &torrent).expect("torrent");
    let root = scratch.0.join("client");
    let dir = root.join("downloads").join(&id);
    fs::create_dir_all(&dir).expect("dir");
    let outside = scratch.0.join("outside");
    fs::write(&outside, b"protected").expect("outside");
    symlink(&outside, dir.join("movie.bin")).expect("symlink");
    let client = Client::open(config(&root, false)).expect("client");
    client
        .ensure(source.to_str().expect("path"))
        .expect("queue");
    thread::sleep(Duration::from_millis(100));
    assert!(client.check(&id).is_err());
    assert_eq!(fs::read(&outside).expect("outside"), b"protected");
    drop(client);
    let state = root.join("state").join(format!("{id}.torrent"));
    fs::remove_file(&state).expect("remove");
    symlink(&source, &state).expect("meta symlink");
    assert!(Client::open(config(&root, false)).is_err());
}
#[test]
fn v1_multifile_utf8_and_empty_file_transfer() {
    let scratch = Scratch::new();
    let a: Vec<u8> = (0..BLOCK + 23).map(|n| (n % 251) as u8).collect();
    let bdata: Vec<u8> = (0..BLOCK + 19).map(|n| (n % 193) as u8).collect();
    let mut joined = a.clone();
    joined.extend_from_slice(&bdata);
    let files = Value::List(vec![
        d(&[
            (b"length", Value::Int(a.len() as i64)),
            (b"path", Value::List(vec![b("café.mkv")])),
        ]),
        d(&[
            (b"length", Value::Int(0)),
            (b"path", Value::List(vec![b("empty.bin")])),
        ]),
        d(&[
            (b"length", Value::Int(bdata.len() as i64)),
            (b"path", Value::List(vec![b("subfolder"), b("audio.bin")])),
        ]),
    ]);
    let info = d(&[
        (b"name", b("media-🎬")),
        (b"piece length", Value::Int(BLOCK as i64)),
        (b"private", Value::Int(1)),
        (b"files", files),
        (
            b"pieces",
            Value::Bytes(joined.chunks(BLOCK).flat_map(sha1).collect()),
        ),
    ]);
    let id = hex(&sha1(&encode(&info)));
    let source = scratch.0.join("multi.torrent");
    fs::write(&source, encode(&d(&[(b"info", info)]))).expect("torrent");
    let seed_root = scratch.0.join("seed");
    let base = seed_root.join("downloads").join(&id).join("media-🎬");
    fs::create_dir_all(base.join("subfolder")).expect("dirs");
    fs::write(base.join("café.mkv"), &a).expect("a");
    fs::write(base.join("empty.bin"), []).expect("empty");
    fs::write(base.join("subfolder/audio.bin"), &bdata).expect("b");
    let seed = Client::open(config(&seed_root, true)).expect("seed");
    seed.ensure(source.to_str().expect("path")).expect("ensure");
    ready(&seed, &id);
    let client = Client::open(config(&scratch.0.join("target"), false)).expect("client");
    client
        .ensure(&format!(
            "magnet:?xt=urn:btih:{id}&x.pe=127.0.0.1:{}",
            seed.listen_port()
        ))
        .expect("magnet");
    let result = ready(&client, &id);
    assert_eq!(result.files.len(), 3);
    let target = scratch.0.join("target/downloads").join(id).join("media-🎬");
    assert_eq!(fs::read(target.join("café.mkv")).expect("a"), a);
    assert_eq!(
        fs::metadata(target.join("empty.bin")).expect("empty").len(),
        0
    );
    assert_eq!(
        fs::read(target.join("subfolder/audio.bin")).expect("b"),
        bdata
    );
}

#[test]
fn v2_and_hybrid_multifile_short_piece_padding_and_empty_file_transfer() {
    for hybrid in [false, true] {
        let scratch = Scratch::new();
        let a = vec![13; 9000];
        let z = vec![29; BLOCK + 49];
        let leaf = |data: &[u8]| {
            let mut attrs = BTreeMap::new();
            attrs.insert(b"length".to_vec(), Value::Int(data.len() as i64));
            if !data.is_empty() {
                attrs.insert(
                    b"pieces root".to_vec(),
                    Value::Bytes(
                        merkle(&data.chunks(BLOCK).map(sha256).collect::<Vec<_>>()).to_vec(),
                    ),
                );
            }
            d(&[(b"", Value::Dict(attrs))])
        };
        let tree = d(&[
            (b"a.bin", leaf(&a)),
            (b"empty.bin", leaf(&[])),
            (b"z.bin", leaf(&z)),
        ]);
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), b("library"));
        info.insert(b"piece length".to_vec(), Value::Int(BLOCK as i64));
        info.insert(b"private".to_vec(), Value::Int(1));
        info.insert(b"meta version".to_vec(), Value::Int(2));
        info.insert(b"file tree".to_vec(), tree);
        if hybrid {
            let files = Value::List(vec![
                d(&[
                    (b"length", Value::Int(a.len() as i64)),
                    (b"path", Value::List(vec![b("a.bin")])),
                ]),
                d(&[
                    (b"attr", b("p")),
                    (b"length", Value::Int((BLOCK - a.len()) as i64)),
                    (b"path", Value::List(vec![b(".pad"), b("padding")])),
                ]),
                d(&[
                    (b"length", Value::Int(0)),
                    (b"path", Value::List(vec![b("empty.bin")])),
                ]),
                d(&[
                    (b"length", Value::Int(z.len() as i64)),
                    (b"path", Value::List(vec![b("z.bin")])),
                ]),
            ]);
            let mut joined = a.clone();
            joined.resize(BLOCK, 0);
            joined.extend_from_slice(&z);
            info.insert(b"files".to_vec(), files);
            info.insert(
                b"pieces".to_vec(),
                Value::Bytes(joined.chunks(BLOCK).flat_map(sha1).collect()),
            );
        }
        let info = Value::Dict(info);
        let id = hex(&sha256(&encode(&info)));
        let zhashes: Vec<_> = z.chunks(BLOCK).map(sha256).collect();
        let zroot = merkle(&zhashes);
        let torrent = encode(&d(&[
            (b"info", info),
            (
                b"piece layers",
                d(&[(
                    &zroot,
                    Value::Bytes(zhashes.iter().flat_map(|h| h.iter().copied()).collect()),
                )]),
            ),
        ]));
        let source = scratch.0.join("multi.torrent");
        fs::write(&source, &torrent).expect("torrent");
        let seed_root = scratch.0.join("seed");
        let base = seed_root.join("downloads").join(&id).join("library");
        fs::create_dir_all(&base).expect("dir");
        fs::write(base.join("a.bin"), &a).expect("a");
        fs::write(base.join("empty.bin"), []).expect("empty");
        fs::write(base.join("z.bin"), &z).expect("z");
        let seed = Client::open(config(&seed_root, true)).expect("seed");
        seed.ensure(source.to_str().expect("path")).expect("ensure");
        ready(&seed, &id);
        let target = scratch.0.join("target");
        let client = Client::open(config(&target, false)).expect("client");
        client
            .ensure(&format!(
                "magnet:?xt=urn:btmh:1220{id}&x.pe=127.0.0.1:{}",
                seed.listen_port()
            ))
            .expect("magnet");
        let result = ready(&client, &id);
        assert_eq!(result.files.len(), 3);
        let base = target.join("downloads").join(&id).join("library");
        assert_eq!(fs::read(base.join("a.bin")).expect("a"), a);
        assert_eq!(fs::read(base.join("z.bin")).expect("z"), z);
        assert_eq!(
            fs::metadata(base.join("empty.bin")).expect("empty").len(),
            0
        );
        assert!(!base.join(".pad").exists());
    }
}
