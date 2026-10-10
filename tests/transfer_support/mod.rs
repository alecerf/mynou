//! Bounded synthetic peers and payloads for transfer-management integration tests.
use mynou::bencode::{self, Value as Bencode};
use mynou::config::{self, Config};
use mynou::crypto::sha1;
use mynou::json::Value;
use mynou::torrent::{Client, DownloadConfig, DownloadStatus, TransferPolicy};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const BLOCK: usize = 16 * 1024;
pub const TOKEN: &str = "transfer-management-fixture-token-0123456789";
static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mynou-transfer-management-{}-{timestamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
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

pub fn configuration() -> Value {
    let mut value = config::default_json();
    value.insert("listen", "127.0.0.1:0");
    value.insert("workers", 1_u32);
    value.insert("max_attempts", 1_u32);
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads section")
    };
    downloads.insert("enabled".into(), true.into());
    downloads.insert("listen_port".into(), 0_u32.into());
    downloads.insert("seed".into(), false.into());
    downloads.insert("dht".into(), false.into());
    downloads.insert("pex".into(), false.into());
    downloads.insert("max_active".into(), 2_u32.into());
    value
}

pub fn engine_config(root: &Path) -> Config {
    config::from_json(&configuration(), root).unwrap()
}

pub fn client_config(root: &Path, seed: bool) -> DownloadConfig {
    let mut value = configuration();
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads section")
    };
    downloads.insert("seed".into(), seed.into());
    config::from_json(&value, root).unwrap().downloads
}

pub fn payload(length: usize, salt: u8) -> Vec<u8> {
    (0..length)
        .map(|index| ((index * 137 + usize::from(salt)) % 251) as u8)
        .collect()
}

pub struct Torrent {
    pub id: String,
    pub path: PathBuf,
    pub files: Vec<(PathBuf, Vec<u8>)>,
    pub total: usize,
}

impl Torrent {
    pub fn single(root: &Path, name: &str, bytes: Vec<u8>) -> Self {
        Self::create(root, name, vec![(PathBuf::from(name), bytes, false)], false)
    }

    pub fn multiple(root: &Path, name: &str, files: Vec<(PathBuf, Vec<u8>)>) -> Self {
        Self::create(
            root,
            name,
            files
                .into_iter()
                .map(|(path, bytes)| (path, bytes, false))
                .collect(),
            true,
        )
    }

    pub fn multiple_with_padding(
        root: &Path,
        name: &str,
        files: Vec<(PathBuf, Vec<u8>, bool)>,
    ) -> Self {
        Self::create(root, name, files, true)
    }

    fn create(
        root: &Path,
        name: &str,
        files: Vec<(PathBuf, Vec<u8>, bool)>,
        multiple: bool,
    ) -> Self {
        fs::create_dir_all(root).unwrap();
        let all: Vec<u8> = files
            .iter()
            .flat_map(|(_, bytes, _)| bytes.iter().copied())
            .collect();
        let total = files
            .iter()
            .filter(|(_, _, padding)| !padding)
            .map(|(_, bytes, _)| bytes.len())
            .sum();
        let mut info = BTreeMap::from([
            (b"name".to_vec(), Bencode::Bytes(name.as_bytes().to_vec())),
            (b"piece length".to_vec(), Bencode::Int(BLOCK as i64)),
            (
                b"pieces".to_vec(),
                Bencode::Bytes(all.chunks(BLOCK).flat_map(sha1).collect()),
            ),
            (b"private".to_vec(), Bencode::Int(1)),
        ]);
        let files = if multiple {
            let encoded_files = files
                .iter()
                .map(|(path, bytes, padding)| {
                    let mut fields = BTreeMap::from([
                        (b"length".to_vec(), Bencode::Int(bytes.len() as i64)),
                        (
                            b"path".to_vec(),
                            Bencode::List(
                                path.components()
                                    .map(|component| {
                                        Bencode::Bytes(
                                            component
                                                .as_os_str()
                                                .to_str()
                                                .unwrap()
                                                .as_bytes()
                                                .to_vec(),
                                        )
                                    })
                                    .collect(),
                            ),
                        ),
                    ]);
                    if *padding {
                        fields.insert(b"attr".to_vec(), Bencode::Bytes(b"p".to_vec()));
                    }
                    Bencode::Dict(fields)
                })
                .collect();
            info.insert(b"files".to_vec(), Bencode::List(encoded_files));
            files
                .into_iter()
                .filter(|(_, _, padding)| !padding)
                .map(|(path, bytes, _)| (Path::new(name).join(path), bytes))
                .collect()
        } else {
            info.insert(b"length".to_vec(), Bencode::Int(all.len() as i64));
            files
                .into_iter()
                .map(|(path, bytes, _)| (path, bytes))
                .collect()
        };
        let info = Bencode::Dict(info);
        let id = sha1(&bencode::encode(&info))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let torrent = bencode::encode(&Bencode::Dict(BTreeMap::from([(b"info".to_vec(), info)])));
        let path = root.join(format!("{name}.torrent"));
        fs::write(&path, torrent).unwrap();
        Self {
            id,
            path,
            files,
            total,
        }
    }

    pub fn magnet(&self, peer_port: u16) -> String {
        format!("magnet:?xt=urn:btih:{}&x.pe=127.0.0.1:{peer_port}", self.id)
    }

    pub fn assert_files(&self, status: &DownloadStatus) {
        assert!(status.ready, "Torrent is not verified: {}", status.message);
        assert_eq!(status.files.len(), self.files.len());
        for ((path, expected), imported) in self.files.iter().zip(&status.files) {
            assert!(imported.ends_with(path));
            assert_eq!(fs::read(imported).unwrap(), *expected);
        }
    }
}

pub struct Seeder {
    pub client: Client,
    pub root: PathBuf,
}

impl Seeder {
    pub fn open(root: &Path, torrents: &[&Torrent]) -> Self {
        Self::open_with_policy(root, torrents, TransferPolicy::default())
    }

    pub fn open_with_policy(root: &Path, torrents: &[&Torrent], policy: TransferPolicy) -> Self {
        let cfg = client_config(root, true);
        for torrent in torrents {
            for (path, bytes) in &torrent.files {
                let destination = cfg.data_dir.join(&torrent.id).join(path);
                fs::create_dir_all(destination.parent().unwrap()).unwrap();
                fs::write(destination, bytes).unwrap();
            }
        }
        let client = Client::open_with_policy(cfg, policy).unwrap();
        for torrent in torrents {
            client.ensure(torrent.path.to_str().unwrap()).unwrap();
            ready(&client, &torrent.id);
        }
        Self {
            client,
            root: root.to_owned(),
        }
    }
}

pub fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "Synthetic transfer exceeded its deadline"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

pub fn ready(client: &Client, id: &str) -> DownloadStatus {
    wait(|| client.check(id).unwrap().ready);
    client.check(id).unwrap()
}

/// A protocol-preserving TCP relay records requests and can delay payloads until
/// management controls have been applied. Handshakes and metadata still flow.
pub struct RecordingProxy {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<u32>>>,
    pub keep_alives: Arc<AtomicU64>,
    pub payloads_enabled: Arc<AtomicBool>,
    active: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct ActiveRelay(Arc<AtomicU64>);

impl Drop for ActiveRelay {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl RecordingProxy {
    pub fn open(seed_port: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread_requests = requests.clone();
        let keep_alives = Arc::new(AtomicU64::new(0));
        let thread_keep_alives = keep_alives.clone();
        let active = Arc::new(AtomicU64::new(0));
        let thread_active = active.clone();
        let payloads_enabled = Arc::new(AtomicBool::new(false));
        let thread_enabled = payloads_enabled.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = stopped.clone();
        let thread = thread::spawn(move || {
            let mut workers = Vec::new();
            while !thread_stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((client, _)) => {
                        client.set_nonblocking(false).unwrap();
                        let requests = thread_requests.clone();
                        let keep_alives = thread_keep_alives.clone();
                        let enabled = thread_enabled.clone();
                        let stopped = thread_stopped.clone();
                        let active = thread_active.clone();
                        active.fetch_add(1, Ordering::AcqRel);
                        workers.push(thread::spawn(move || {
                            let _active = ActiveRelay(active);
                            let seed = TcpStream::connect_timeout(
                                &SocketAddr::from(([127, 0, 0, 1], seed_port)),
                                Duration::from_secs(2),
                            )
                            .unwrap();
                            relay(client, seed, requests, keep_alives, enabled, stopped);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("Cannot accept recording proxy connection: {error}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            port,
            requests,
            keep_alives,
            payloads_enabled,
            active,
            stopped,
            thread: Some(thread),
        }
    }

    pub fn wait_idle(&self) {
        wait(|| self.active.load(Ordering::Acquire) == 0);
    }
}

impl Drop for RecordingProxy {
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

fn read_exact(
    stream: &mut TcpStream,
    mut bytes: &mut [u8],
    stopped: &AtomicBool,
) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !bytes.is_empty() && !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
        match stream.read(bytes) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(length) => bytes = &mut bytes[length..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
    if bytes.is_empty() {
        Ok(())
    } else {
        Err(std::io::ErrorKind::TimedOut.into())
    }
}

fn relay(
    mut client: TcpStream,
    mut seed: TcpStream,
    requests: Arc<Mutex<Vec<u32>>>,
    keep_alives: Arc<AtomicU64>,
    enabled: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
) {
    for stream in [&client, &seed] {
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.set_nodelay(true).unwrap();
    }
    let mut incoming = client.try_clone().unwrap();
    let mut outgoing = seed.try_clone().unwrap();
    let back_stop = stopped.clone();
    let disconnected = Arc::new(AtomicBool::new(false));
    let back_disconnected = disconnected.clone();
    let back = thread::spawn(move || {
        let result = forward_messages(&mut outgoing, &mut incoming, &back_stop, |message| {
            if message.first() == Some(&7) {
                let deadline = Instant::now() + Duration::from_secs(15);
                while !enabled.load(Ordering::Acquire)
                    && !back_stop.load(Ordering::Acquire)
                    && !back_disconnected.load(Ordering::Acquire)
                {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
            !back_stop.load(Ordering::Acquire) && !back_disconnected.load(Ordering::Acquire)
        });
        let _ = outgoing.shutdown(Shutdown::Both);
        let _ = incoming.shutdown(Shutdown::Both);
        result
    });
    let _ = forward_messages(&mut client, &mut seed, &stopped, |message| {
        if message.is_empty() {
            keep_alives.fetch_add(1, Ordering::Relaxed);
        }
        if message.first() == Some(&6) && message.len() == 13 {
            requests
                .lock()
                .unwrap()
                .push(u32::from_be_bytes(message[1..5].try_into().unwrap()));
        }
        true
    });
    disconnected.store(true, Ordering::Release);
    let _ = client.shutdown(Shutdown::Both);
    let _ = seed.shutdown(Shutdown::Both);
    let _ = back.join().unwrap();
}

fn forward_messages(
    from: &mut TcpStream,
    to: &mut TcpStream,
    stopped: &AtomicBool,
    mut inspect: impl FnMut(&[u8]) -> bool,
) -> std::io::Result<()> {
    let mut handshake = [0; 68];
    read_exact(from, &mut handshake, stopped)?;
    to.write_all(&handshake)?;
    loop {
        let mut header = [0; 4];
        read_exact(from, &mut header, stopped)?;
        let length = u32::from_be_bytes(header) as usize;
        if length > 4 * 1024 * 1024 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut message = vec![0; length];
        read_exact(from, &mut message, stopped)?;
        if !inspect(&message) {
            return Ok(());
        }
        to.write_all(&header)?;
        to.write_all(&message)?;
    }
}
