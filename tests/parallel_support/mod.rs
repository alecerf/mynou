//! Original bounded peers for parallel-download integration tests.
use mynou::bencode::{Value, encode};
use mynou::crypto::{sha1, sha256};
use mynou::torrent::{Client, DownloadConfig, DownloadStatus, TorrentControl};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const BLOCK: usize = 16 * 1024;
const PEER_ID: &[u8; 20] = b"-SYN010-abcdefghijkl";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mynou-parallel-{}-{timestamp}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn configuration(root: &Path, max_active: usize, max_peers: usize) -> DownloadConfig {
    DownloadConfig {
        data_dir: root.join("data"),
        state_dir: root.join("state"),
        listen_port: 0,
        seed: false,
        dht: false,
        pex: false,
        max_active,
        max_peers,
    }
}

#[derive(Clone, Copy)]
pub enum Kind {
    V1,
    V2,
    Hybrid,
}

#[derive(Clone)]
pub struct Torrent {
    pub id: String,
    pub data: Arc<Vec<u8>>,
    name: String,
    wire_hash: [u8; 20],
    encoded: Vec<u8>,
    v1: Option<[u8; 20]>,
    v2: Option<[u8; 32]>,
    root: Option<[u8; 32]>,
    leaves: Vec<[u8; 32]>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn pair(left: [u8; 32], right: [u8; 32]) -> [u8; 32] {
    let mut bytes = [0; 64];
    bytes[..32].copy_from_slice(&left);
    bytes[32..].copy_from_slice(&right);
    sha256(&bytes)
}

fn root(hashes: &[[u8; 32]]) -> [u8; 32] {
    let mut hashes = hashes.to_vec();
    hashes.resize(hashes.len().next_power_of_two(), [0; 32]);
    while hashes.len() > 1 {
        hashes = hashes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|children| pair(children[0], children[1]))
            .collect();
    }
    hashes[0]
}

fn dictionary(items: impl IntoIterator<Item = (Vec<u8>, Value)>) -> Value {
    Value::Dict(items.into_iter().collect())
}

impl Torrent {
    pub fn new(name: &str, pieces: usize, kind: Kind) -> Self {
        assert!(pieces > 1 && pieces <= 128);
        let data: Vec<u8> = (0..pieces * BLOCK)
            .map(|index| ((index * 137 + name.len() * 31) % 251) as u8)
            .collect();
        let leaves: Vec<_> = data.chunks(BLOCK).map(sha256).collect();
        let merkle_root = root(&leaves);
        let mut info = BTreeMap::from([
            (b"name".to_vec(), Value::Bytes(name.as_bytes().to_vec())),
            (b"piece length".to_vec(), Value::Int(BLOCK as i64)),
            (b"private".to_vec(), Value::Int(1)),
        ]);
        let has_v1 = matches!(kind, Kind::V1 | Kind::Hybrid);
        let has_v2 = matches!(kind, Kind::V2 | Kind::Hybrid);
        if has_v1 {
            info.insert(b"length".to_vec(), Value::Int(data.len() as i64));
            info.insert(
                b"pieces".to_vec(),
                Value::Bytes(data.chunks(BLOCK).flat_map(sha1).collect()),
            );
        }
        if has_v2 {
            info.insert(b"meta version".to_vec(), Value::Int(2));
            info.insert(
                b"file tree".to_vec(),
                dictionary([(
                    name.as_bytes().to_vec(),
                    dictionary([(
                        Vec::new(),
                        dictionary([
                            (b"length".to_vec(), Value::Int(data.len() as i64)),
                            (b"pieces root".to_vec(), Value::Bytes(merkle_root.to_vec())),
                        ]),
                    )]),
                )]),
            );
        }
        let info = Value::Dict(info);
        let bytes = encode(&info);
        let v1 = has_v1.then(|| sha1(&bytes));
        let v2 = has_v2.then(|| sha256(&bytes));
        let id = v2
            .map(|hash| hex(&hash))
            .unwrap_or_else(|| hex(&v1.unwrap()));
        let wire_hash = v1.unwrap_or_else(|| v2.unwrap()[..20].try_into().unwrap());
        // Omit v2 piece layers deliberately. Peers must authenticate sparse
        // hashes against the root before pure-v2 pieces can be committed.
        let encoded = encode(&dictionary([(b"info".to_vec(), info)]));
        Self {
            id,
            data: Arc::new(data),
            name: name.to_owned(),
            wire_hash,
            encoded,
            v1,
            v2,
            root: has_v2.then_some(merkle_root),
            leaves,
        }
    }

    pub fn pieces(&self) -> usize {
        self.data.len() / BLOCK
    }

    pub fn install(&self, config: &DownloadConfig, ports: &[u16], queue_order: u64) {
        fs::create_dir_all(&config.state_dir).unwrap();
        fs::write(
            config.state_dir.join(format!("{}.torrent", self.id)),
            &self.encoded,
        )
        .unwrap();
        let mut source = String::from("magnet:?");
        if let Some(hash) = self.v1 {
            source.push_str(&format!("xt=urn:btih:{}&", hex(&hash)));
        }
        if let Some(hash) = self.v2 {
            source.push_str(&format!("xt=urn:btmh:1220{}&", hex(&hash)));
        }
        for port in ports {
            source.push_str(&format!("x.pe=127.0.0.1:{port}&"));
        }
        fs::write(
            config.state_dir.join(format!("{}.source", self.id)),
            source.trim_end_matches('&'),
        )
        .unwrap();
        fs::write(
            config.state_dir.join(format!("{}.control", self.id)),
            TorrentControl::new(self.id.clone(), queue_order)
                .unwrap()
                .encode()
                .unwrap(),
        )
        .unwrap();
    }

    pub fn assert_files(&self, status: &DownloadStatus) {
        assert!(
            status.ready,
            "Torrent remains unverified: {}",
            status.message
        );
        assert_eq!(status.files.len(), 1);
        assert!(status.files[0].ends_with(&self.name));
        assert_eq!(fs::read(&status.files[0]).unwrap(), *self.data);
    }

    fn proof(&self, request: &[u8]) -> Option<Vec<u8>> {
        if request.len() != 48 || request[..32] != self.root? {
            return None;
        }
        let number =
            |offset| u32::from_be_bytes(request[offset..offset + 4].try_into().unwrap()) as usize;
        let first = number(36);
        let height = self.pieces().next_power_of_two().ilog2() as usize;
        if number(32) != 0
            || number(40) != 2
            || number(44) != height - 1
            || first % 2 != 0
            || first >= self.pieces()
        {
            return None;
        }
        let mut hashes = self.leaves.clone();
        hashes.resize(hashes.len().next_power_of_two(), [0; 32]);
        let mut response = request.to_vec();
        response.extend_from_slice(&hashes[first]);
        response.extend_from_slice(&hashes[first + 1]);
        let mut position = first / 2;
        hashes = hashes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|children| pair(children[0], children[1]))
            .collect();
        while hashes.len() > 1 {
            response.extend_from_slice(&hashes[position ^ 1]);
            position /= 2;
            hashes = hashes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|children| pair(children[0], children[1]))
                .collect();
        }
        Some(response)
    }
}

pub fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "Synthetic peer deadline expired");
        thread::sleep(Duration::from_millis(5));
    }
}

pub fn ready(client: &Client, torrent: &Torrent) -> DownloadStatus {
    wait_for(Duration::from_secs(20), || {
        let status = client.check(&torrent.id).unwrap();
        assert!(
            !client
                .transfer(&torrent.id)
                .unwrap()
                .get("failed")
                .unwrap()
                .as_bool()
                .unwrap(),
            "Parallel transfer failed: {}",
            status.message
        );
        status.ready
    });
    client.check(&torrent.id).unwrap()
}

#[derive(Default)]
pub struct Fleet {
    pub active: AtomicUsize,
    pub peak: AtomicUsize,
}

struct Active(Arc<Fleet>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy)]
pub enum Mode {
    Good,
    CorruptOnce,
    StallHandshake,
}

pub struct Peer {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<usize>>>,
    pub payloads_enabled: Arc<AtomicBool>,
    pub payload_bytes: Arc<AtomicU64>,
    pub proof_requests: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct Connection {
    torrent: Torrent,
    pieces: Vec<usize>,
    delay: Duration,
    mode: Mode,
    requests: Arc<Mutex<Vec<usize>>>,
    enabled: Arc<AtomicBool>,
    bytes: Arc<AtomicU64>,
    proofs: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
}

impl Peer {
    pub fn open(
        torrent: &Torrent,
        pieces: Vec<usize>,
        delay: Duration,
        mode: Mode,
        enabled: bool,
        fleet: &Arc<Fleet>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        Self::with_listener(listener, torrent, pieces, delay, mode, enabled, fleet)
    }

    pub fn with_listener(
        listener: TcpListener,
        torrent: &Torrent,
        pieces: Vec<usize>,
        delay: Duration,
        mode: Mode,
        enabled: bool,
        fleet: &Arc<Fleet>,
    ) -> Self {
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let payloads_enabled = Arc::new(AtomicBool::new(enabled));
        let payload_bytes = Arc::new(AtomicU64::new(0));
        let proof_requests = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let context = Arc::new(Connection {
            torrent: torrent.clone(),
            pieces,
            delay,
            mode,
            requests: requests.clone(),
            enabled: payloads_enabled.clone(),
            bytes: payload_bytes.clone(),
            proofs: proof_requests.clone(),
            stopped: stopped.clone(),
        });
        let fleet = fleet.clone();
        let thread = thread::spawn(move || {
            let mut workers = Vec::new();
            while !context.stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        assert!(
                            workers.len() < 128,
                            "Synthetic peer connection bound exceeded"
                        );
                        let active = fleet.active.fetch_add(1, Ordering::AcqRel) + 1;
                        fleet.peak.fetch_max(active, Ordering::AcqRel);
                        let guard = Active(fleet.clone());
                        let context = context.clone();
                        workers.push(thread::spawn(move || {
                            let _guard = guard;
                            let _ = serve(stream, &context);
                        }));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("Cannot accept synthetic connection: {error}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            port,
            requests,
            payloads_enabled,
            payload_bytes,
            proof_requests,
            stopped,
            thread: Some(thread),
        }
    }

    pub fn release(&self) {
        self.payloads_enabled.store(true, Ordering::Release);
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}

fn exact(stream: &mut TcpStream, mut bytes: &mut [u8], stopped: &AtomicBool) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !bytes.is_empty() {
        if stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match stream.read(bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(length) => bytes = &mut bytes[length..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn send(stream: &mut TcpStream, id: u8, payload: &[u8]) -> io::Result<()> {
    stream.write_all(&((payload.len() + 1) as u32).to_be_bytes())?;
    stream.write_all(&[id])?;
    stream.write_all(payload)
}

fn connected(stream: &TcpStream, stopped: &AtomicBool) -> io::Result<()> {
    if stopped.load(Ordering::Acquire) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let mut byte = [0];
    match stream.peek(&mut byte) {
        Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn serve(mut stream: TcpStream, context: &Connection) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(20)))?;
    stream.set_write_timeout(Some(Duration::from_millis(250)))?;
    stream.set_nodelay(true)?;
    let mut handshake = [0; 68];
    exact(&mut stream, &mut handshake, &context.stopped)?;
    if handshake[0] != 19
        || &handshake[1..20] != b"BitTorrent protocol"
        || handshake[28..48] != context.torrent.wire_hash
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    if matches!(context.mode, Mode::StallHandshake) {
        loop {
            connected(&stream, &context.stopped)?;
            thread::sleep(Duration::from_millis(5));
        }
    }
    handshake[20..28].fill(0);
    handshake[48..].copy_from_slice(PEER_ID);
    stream.write_all(&handshake)?;
    let mut bitfield = vec![0; context.torrent.pieces().div_ceil(8)];
    for &piece in &context.pieces {
        assert!(piece < context.torrent.pieces());
        bitfield[piece / 8] |= 0x80 >> (piece % 8);
    }
    send(&mut stream, 5, &bitfield)?;
    send(&mut stream, 1, &[])?;
    loop {
        let mut header = [0; 4];
        exact(&mut stream, &mut header, &context.stopped)?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 {
            continue;
        }
        if length > BLOCK + 1024 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut message = vec![0; length];
        exact(&mut stream, &mut message, &context.stopped)?;
        if message[0] == 21 {
            context.proofs.fetch_add(1, Ordering::Relaxed);
            let Some(proof) = context.torrent.proof(&message[1..]) else {
                return Err(io::ErrorKind::InvalidData.into());
            };
            send(&mut stream, 22, &proof)?;
            continue;
        }
        if message[0] != 6 {
            continue;
        }
        if message.len() != 13 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let piece = u32::from_be_bytes(message[1..5].try_into().unwrap()) as usize;
        let begin = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
        let length = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
        if !context.pieces.contains(&piece) || begin != 0 || length != BLOCK {
            return Err(io::ErrorKind::InvalidData.into());
        }
        context.requests.lock().unwrap().push(piece);
        let deadline = Instant::now() + context.delay;
        while !context.enabled.load(Ordering::Acquire) || Instant::now() < deadline {
            connected(&stream, &context.stopped)?;
            thread::sleep(Duration::from_millis(2));
        }
        let mut payload = message[1..9].to_vec();
        payload.extend_from_slice(&context.torrent.data[piece * BLOCK..(piece + 1) * BLOCK]);
        if matches!(context.mode, Mode::CorruptOnce) {
            payload[8] ^= 0x80;
        }
        send(&mut stream, 7, &payload)?;
        context.bytes.fetch_add(BLOCK as u64, Ordering::Relaxed);
        if matches!(context.mode, Mode::CorruptOnce) {
            stream.shutdown(Shutdown::Both)?;
            return Ok(());
        }
    }
}
