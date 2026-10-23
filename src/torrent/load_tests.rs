//! Engine-level torrent load (#320): the real native client, with magnet
//! metadata exchange, piece verification, disk writes and inbound seeding,
//! over two native tunnel owners in this process. An opt-in measurement
//! harness for the macOS evidence workflow and a required regression of the
//! explicit engine and flow limits. Peers are seeding native `Client`s on the
//! responder tunnel's virtual listeners; nothing is bound on the host except
//! the two WireGuard UDP endpoints.
use super::*;
use crate::{
    json::{self, Value as Json},
    net::{NetworkDatagram, NetworkStream},
    tunnel_http::tests::Pair,
};
use std::{
    io::{self, Read, Write},
    sync::atomic::AtomicUsize,
};
use transport::{Network, NetworkListener, Publication};

/// The responder side of `tunnel_http::tests::Pair`.
const SERVER: [u8; 4] = [10, 7, 0, 1];
const PIECE: usize = 256 * 1024;

#[derive(Default)]
struct Flows {
    open: usize,
    max_open: usize,
    /// Current and maximum open peer flows per announced info hash.
    torrents: BTreeMap<[u8; 20], (usize, usize)>,
    attempts: u64,
    refused: BTreeMap<String, u64>,
}

/// Delegates to the tunnel `Network` and observes every peer flow the engine
/// opens through it; the info hash is read from the outgoing handshake.
struct Observed {
    inner: Arc<dyn Network>,
    flows: Arc<Mutex<Flows>>,
}
impl Network for Observed {
    fn generation(&self) -> Result<Option<String>> {
        self.inner.generation()
    }
    fn publication(&self) -> Result<Box<dyn Publication + '_>> {
        self.inner.publication()
    }
    fn connect(&self, address: SocketAddr, timeout: Duration) -> Result<Box<dyn NetworkStream>> {
        self.flows.lock().unwrap().attempts += 1;
        match self.inner.connect(address, timeout) {
            Ok(inner) => {
                let mut flows = self.flows.lock().unwrap();
                flows.open += 1;
                flows.max_open = flows.max_open.max(flows.open);
                Ok(Box::new(Flow {
                    inner,
                    flows: self.flows.clone(),
                    handshake: Vec::new(),
                    hash: None,
                }))
            }
            Err(error) => {
                *self
                    .flows
                    .lock()
                    .unwrap()
                    .refused
                    .entry(error.clone())
                    .or_default() += 1;
                Err(error)
            }
        }
    }
    fn resolve(&self, authority: &str, deadline: Instant) -> Result<Vec<SocketAddr>> {
        self.inner.resolve(authority, deadline)
    }
    fn datagram(&self, ipv6: bool) -> Result<Box<dyn NetworkDatagram>> {
        self.inner.datagram(ipv6)
    }
    fn http(&self) -> crate::net::HttpClient {
        self.inner.http()
    }
    fn listen(&self, address: SocketAddr) -> Result<Box<dyn NetworkListener>> {
        self.inner.listen(address)
    }
}

struct Flow {
    inner: Box<dyn NetworkStream>,
    flows: Arc<Mutex<Flows>>,
    handshake: Vec<u8>,
    hash: Option<[u8; 20]>,
}
impl Read for Flow {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buffer)
    }
}
impl Write for Flow {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        if self.hash.is_none() && self.handshake.len() < 48 {
            let take = written.min(48 - self.handshake.len());
            self.handshake.extend_from_slice(&buffer[..take]);
            if self.handshake.len() == 48 {
                let hash: [u8; 20] = self.handshake[28..48].try_into().unwrap();
                let mut flows = self.flows.lock().unwrap();
                let entry = flows.torrents.entry(hash).or_default();
                entry.0 += 1;
                entry.1 = entry.1.max(entry.0);
                self.hash = Some(hash);
            }
        }
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl NetworkStream for Flow {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }
}
impl Drop for Flow {
    fn drop(&mut self) {
        let mut flows = self.flows.lock().unwrap();
        flows.open -= 1;
        if let Some(hash) = self.hash
            && let Some(entry) = flows.torrents.get_mut(&hash)
        {
            entry.0 -= 1;
        }
    }
}

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn download_config(root: &Path, seed: bool, max_active: usize, max_peers: usize) -> DownloadConfig {
    DownloadConfig {
        data_dir: root.join("data"),
        state_dir: root.join("state"),
        listen_port: 0,
        seed,
        dht: false,
        pex: false,
        max_active,
        max_seeding: 0,
        max_peers,
    }
}

struct Torrent {
    hash: String,
    file: PathBuf,
    name: String,
}

/// `count` single-file torrents of `bytes` distinct synthetic bytes, written
/// once under `root`; seeds copy them (a clone on APFS).
fn torrents(root: &Path, count: usize, bytes: usize, piece: usize) -> Vec<Torrent> {
    torrents_from(root, count, bytes, piece, 0)
}

fn torrents_from(
    root: &Path,
    count: usize,
    bytes: usize,
    piece: usize,
    offset: usize,
) -> Vec<Torrent> {
    fs::create_dir_all(root).unwrap();
    (offset..offset + count)
        .map(|n| {
            let payload: Vec<u8> = (0..bytes).map(|i| ((i * 7 + n * 13) % 251) as u8).collect();
            let pieces = payload
                .chunks(piece)
                .flat_map(crate::crypto::sha1)
                .collect();
            let name = format!("load-{n}.bin");
            let info = discovery::value_dict(&[
                (b"name", Value::Bytes(name.clone().into_bytes())),
                (b"length", Value::Int(bytes as i64)),
                (b"piece length", Value::Int(piece as i64)),
                (b"pieces", Value::Bytes(pieces)),
                (b"private", Value::Int(0)),
            ]);
            let hash = hex(&crate::crypto::sha1(&crate::bencode::encode(&info)));
            let encoded = crate::bencode::encode(&discovery::value_dict(&[(b"info", info)]));
            let file = root.join(&name);
            fs::write(&file, &payload).unwrap();
            fs::write(root.join(format!("{name}.torrent")), encoded).unwrap();
            Torrent { hash, file, name }
        })
        .collect()
}

fn wait_for(deadline: Instant, what: &str, mut ready: impl FnMut() -> bool) {
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Seeding native clients on the responder network, each holding every torrent.
fn seeds(
    pair: &Pair,
    root: &Path,
    library: &Path,
    torrents: &[Torrent],
    count: usize,
) -> Vec<Client> {
    let seeds: Vec<_> = (0..count)
        .map(|n| {
            let root = root.join(format!("seed-{n}"));
            let config = download_config(&root, true, 64, 8);
            for torrent in torrents {
                let base = config.data_dir.join(&torrent.hash);
                fs::create_dir_all(&base).unwrap();
                fs::copy(&torrent.file, base.join(&torrent.name)).unwrap();
            }
            let seed = Client::open_with_policy_and_admission(
                config,
                TransferPolicy::default(),
                Arc::new(|_| Ok(true)),
                pair.server_network.clone(),
            )
            .unwrap();
            for torrent in torrents {
                let path = library.join(format!("{}.torrent", torrent.name));
                seed.ensure(path.to_str().unwrap()).unwrap();
            }
            seed
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(300);
    for seed in &seeds {
        for torrent in torrents {
            wait_for(deadline, "seed verification", || {
                seed.check(&torrent.hash).unwrap().ready
            });
        }
    }
    seeds
}

fn magnet(hash: &str, seeds: &[Client]) -> String {
    let [a, b, c, d] = SERVER;
    let mut magnet = format!("magnet:?xt=urn:btih:{hash}");
    for seed in seeds {
        magnet.push_str(&format!("&x.pe={a}.{b}.{c}.{d}:{}", seed.listen_port()));
    }
    magnet
}

/// Running transfers and first-ready instants, sampled until `stop`.
struct Sampler {
    max_running: Arc<AtomicUsize>,
    ready: Arc<Mutex<BTreeMap<String, Instant>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Sampler {
    fn start(client: &Client) -> Self {
        let (jobs, max_running) = (client.jobs.clone(), Arc::new(AtomicUsize::new(0)));
        let ready = Arc::new(Mutex::new(BTreeMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (max_running, ready, stop) = (max_running.clone(), ready.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    if let Ok(jobs) = jobs.lock() {
                        let running = jobs.values().filter(|job| job.running).count();
                        max_running.fetch_max(running, Ordering::Relaxed);
                        let mut ready = ready.lock().unwrap();
                        for (id, job) in jobs.iter() {
                            if job.status.ready {
                                ready.entry(id.clone()).or_insert_with(Instant::now);
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };
        Self {
            max_running,
            ready,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    let (mut left, mut right) = (
        fs::File::open(left).unwrap(),
        fs::File::open(right).unwrap(),
    );
    let (mut a, mut b) = (vec![0; 1 << 16], vec![0; 1 << 16]);
    loop {
        let n = left.read(&mut a).unwrap();
        if n == 0 {
            return right.read(&mut b).unwrap() == 0;
        }
        if right.read_exact(&mut b[..n]).is_err() || a[..n] != b[..n] {
            return false;
        }
    }
}

fn scratch() -> Directory {
    let path = std::env::temp_dir().join(format!(
        "mynou-torrent-load-{}",
        hex(&crate::crypto::random_bytes::<8>().unwrap())
    ));
    fs::create_dir(&path).unwrap();
    Directory(path)
}

#[test]
fn queued_torrents_peer_flows_and_an_exhausted_flow_table_hold_their_limits() {
    const FLOWS: usize = 2;
    let pair = Pair::with(|client, server| {
        client.tcp.flows = FLOWS;
        server.tcp.flows = 64;
    });
    let directory = scratch();
    let library = directory.0.join("library");
    let torrents = torrents(&library, 3, 64 * 1024, BLOCK);
    let seeds = seeds(&pair, &directory.0, &library, &torrents, 3);
    // Occupy the whole client flow table before the engine starts.
    let holder = pair
        .server_network
        .listen(SocketAddr::from(([0, 0, 0, 0], 0)))
        .unwrap();
    let held_port = holder.port();
    let held: Vec<_> = (0..FLOWS)
        .map(|_| {
            pair.client_network
                .connect(
                    SocketAddr::from((SERVER, held_port)),
                    Duration::from_secs(5),
                )
                .unwrap()
        })
        .collect();
    let (_, before, _) = pair
        .client
        .telemetry(Instant::now() + Duration::from_secs(5))
        .unwrap();
    let flows = Arc::new(Mutex::new(Flows::default()));
    let network = Arc::new(Observed {
        inner: pair.client_network.clone(),
        flows: flows.clone(),
    });
    let config = download_config(&directory.0.join("download"), false, 1, 2);
    let client = Client::open_with_policy_and_admission(
        config.clone(),
        TransferPolicy::default(),
        Arc::new(|_| Ok(true)),
        network,
    )
    .unwrap();
    let sampler = Sampler::start(&client);
    for torrent in &torrents {
        client.ensure(&magnet(&torrent.hash, &seeds)).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    wait_for(deadline, "refused engine connects", || {
        flows.lock().unwrap().refused.values().sum::<u64>() >= 2
    });
    assert_eq!(
        flows.lock().unwrap().max_open,
        0,
        "no flow beyond the table"
    );
    for torrent in &torrents {
        let status = client.check(&torrent.hash).unwrap();
        assert!(!status.ready, "{}", status.message);
        assert!(!client.jobs.lock().unwrap()[&torrent.hash].failed);
    }
    drop(held);
    drop(holder);
    wait_for(deadline, "verified downloads", || {
        torrents
            .iter()
            .all(|torrent| client.check(&torrent.hash).unwrap().ready)
    });
    assert_eq!(sampler.max_running.load(Ordering::Relaxed), 1);
    let flows = flows.lock().unwrap();
    assert!(flows.max_open <= FLOWS);
    assert_eq!(flows.torrents.len(), torrents.len());
    assert!(
        flows
            .torrents
            .values()
            .all(|(open, max)| *open <= 2 && *max <= 2)
    );
    assert!(
        flows
            .refused
            .keys()
            .all(|message| message == "Tunnel operation failed"),
        "{:?}",
        flows.refused
    );
    drop(flows);
    for torrent in &torrents {
        let downloaded = config.data_dir.join(&torrent.hash).join(&torrent.name);
        assert!(same_file(&downloaded, &torrent.file));
    }
    let (status, after, _) = pair
        .client
        .telemetry(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(status.state, crate::tunnel::TunnelState::Protected);
    assert_eq!(after.generations, before.generations);
    drop(sampler);
    drop(client);
    drop(seeds);
}

/// Process CPU seconds and resident bytes from `ps`, for phase deltas.
fn usage() -> (f64, u64) {
    let _spawns = crate::store::SPAWNING.write().unwrap();
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "time=,rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let mut fields = text.split_whitespace();
    let time = fields.next().unwrap();
    let seconds = time
        .split(['-', ':'])
        .rev()
        .zip([1.0, 60.0, 3600.0, 86400.0])
        .map(|(part, unit)| part.parse::<f64>().unwrap() * unit)
        .sum();
    let rss: u64 = fields.next().unwrap().parse().unwrap();
    (seconds, rss * 1024)
}

fn variable(name: &str) -> usize {
    std::env::var(name).unwrap().parse().unwrap()
}

/// `MYNOU_TORRENT_LOAD_TORRENTS` magnets of `MYNOU_TORRENT_LOAD_BYTES` each,
/// from `MYNOU_TORRENT_LOAD_SEEDS` seeding clients, with the downloader's
/// `MYNOU_TORRENT_LOAD_ACTIVE` and `MYNOU_TORRENT_LOAD_PEERS` and the client
/// tunnel's `MYNOU_TORRENT_LOAD_FLOWS`, reported on one `MYNOU_TORRENT_LOAD`
/// line. Without the variables the test returns at once.
#[test]
fn torrent_load_report() {
    if std::env::var_os("MYNOU_TORRENT_LOAD_TORRENTS").is_none() {
        return;
    }
    let count = variable("MYNOU_TORRENT_LOAD_TORRENTS");
    let bytes = variable("MYNOU_TORRENT_LOAD_BYTES");
    let seed_count = variable("MYNOU_TORRENT_LOAD_SEEDS");
    let max_active = variable("MYNOU_TORRENT_LOAD_ACTIVE");
    let max_peers = variable("MYNOU_TORRENT_LOAD_PEERS");
    let flow_limit = variable("MYNOU_TORRENT_LOAD_FLOWS");
    let directory = scratch();
    let started = Instant::now();
    let pair = Pair::with(|client, server| {
        client.tcp.flows = flow_limit;
        server.tcp.flows = 512;
        server.tcp.backlog = 512;
        server.tcp.total_buffer = 128 << 20;
    });
    let protected_us = started.elapsed().as_micros();
    let library = directory.0.join("library");
    let torrents = torrents(&library, count, bytes, PIECE);
    let seeds = seeds(&pair, &directory.0, &library, &torrents, seed_count);
    let setup_us = started.elapsed().as_micros();
    let flows = Arc::new(Mutex::new(Flows::default()));
    let network = Arc::new(Observed {
        inner: pair.client_network.clone(),
        flows: flows.clone(),
    });
    let config = download_config(&directory.0.join("download"), false, max_active, max_peers);
    let (cpu_before, rss_before) = usage();
    let transfers = Instant::now();
    let client = Client::open_with_policy_and_admission(
        config.clone(),
        TransferPolicy::default(),
        Arc::new(|_| Ok(true)),
        network,
    )
    .unwrap();
    let sampler = Sampler::start(&client);
    for torrent in &torrents {
        client.ensure(&magnet(&torrent.hash, &seeds)).unwrap();
    }
    wait_for(
        transfers + Duration::from_secs(900),
        "verified downloads",
        || {
            torrents
                .iter()
                .all(|torrent| client.check(&torrent.hash).unwrap().ready)
        },
    );
    let transfer_us = transfers.elapsed().as_micros();
    let (cpu_after, rss_after) = usage();
    let max_running = sampler.max_running.load(Ordering::Relaxed);
    let ready = sampler.ready.lock().unwrap().clone();
    drop(sampler);
    let verified = torrents
        .iter()
        .filter(|torrent| {
            same_file(
                &config.data_dir.join(&torrent.hash).join(&torrent.name),
                &torrent.file,
            )
        })
        .count();
    let uploaded: u64 = seeds
        .iter()
        .map(|seed| {
            seed.jobs
                .lock()
                .unwrap()
                .values()
                .map(|job| job.counters.uploaded.load(Ordering::Relaxed))
                .sum::<u64>()
        })
        .sum();
    let limit = (parallel::CONNECTION_BUDGET / config.download_slots())
        .max(1)
        .min(max_peers);
    let mut completions: Vec<u128> = ready
        .values()
        .map(|at| at.saturating_duration_since(transfers).as_micros())
        .collect();
    completions.sort_unstable();
    let (_, client_stats, _) = pair
        .client
        .telemetry(Instant::now() + Duration::from_secs(5))
        .unwrap();
    drop(client);
    drop(seeds);
    let flows = flows.lock().unwrap();
    let quantile = |values: &[u128], q: usize| {
        values
            .get((values.len().saturating_sub(1)) * q / 100)
            .map_or(String::new(), u128::to_string)
    };
    let mut report = Json::object();
    for (name, value) in [
        ("torrents", count),
        ("bytes_per_torrent", bytes),
        ("seeds", seed_count),
        ("max_active", max_active),
        ("max_peers", max_peers),
        ("peer_limit", limit),
        ("flow_limit", flow_limit),
        ("verified", verified),
        ("max_running", max_running),
        ("max_open_flows", flows.max_open),
        (
            "max_torrent_flows",
            flows
                .torrents
                .values()
                .map(|(_, max)| *max)
                .max()
                .unwrap_or(0),
        ),
        ("flow_torrents", flows.torrents.len()),
    ] {
        report.insert(name, value.to_string());
    }
    report.insert("protected_us", protected_us.to_string());
    report.insert("setup_us", setup_us.to_string());
    report.insert("transfer_us", transfer_us.to_string());
    report.insert("transfer_cpu_s", format!("{:.2}", cpu_after - cpu_before));
    report.insert("rss_before_bytes", rss_before.to_string());
    report.insert("rss_after_bytes", rss_after.to_string());
    report.insert("seed_uploaded_bytes", uploaded.to_string());
    report.insert("connect_attempts", flows.attempts.to_string());
    for (name, q) in [("p50", 50), ("p90", 90), ("max", 100)] {
        report.insert(format!("complete_{name}_us"), quantile(&completions, q));
    }
    let mut refused = Json::object();
    for (message, count) in &flows.refused {
        refused.insert(message.clone(), count.to_string());
    }
    report.insert("refused", refused);
    let mut counters = Json::object();
    for (name, value) in [
        ("generations", client_stats.generations),
        ("received", client_stats.received),
        ("sent", client_stats.sent),
        ("rejected", client_stats.rejected),
        ("inner_rejected", client_stats.inner_rejected),
        ("inner_dropped", client_stats.inner_dropped),
    ] {
        counters.insert(name, value.to_string());
    }
    report.insert("client", counters);
    eprintln!("MYNOU_TORRENT_LOAD {}", json::stringify(&report));
    assert_eq!(verified, count);
    assert!(max_running <= config.download_slots());
    assert!(flows.max_open <= flow_limit.min(config.download_slots() * limit));
}

#[path = "representative_tests.rs"]
mod representative;

#[test]
fn zero_download_queue_limit_runs_multiple_transfers_with_bounded_workers() {
    let directory = scratch();
    let pair = Pair::new();
    let library = directory.0.join("library");
    let torrents = torrents(&library, 3, 256 << 10, BLOCK);
    let sources = seeds(&pair, &directory.0, &library, &torrents, 1);
    for torrent in &torrents {
        sources[0]
            .set_policy(
                &torrent.hash,
                Some(TransferPolicy {
                    upload_limit_bps: 64 << 10,
                    ..TransferPolicy::default()
                }),
            )
            .unwrap();
    }
    let config = download_config(&directory.0.join("download"), false, 0, 4);
    let subject = Client::open_with_policy_and_admission(
        config.clone(),
        TransferPolicy::default(),
        Arc::new(|_| Ok(true)),
        pair.client_network.clone(),
    )
    .unwrap();
    let sampler = Sampler::start(&subject);
    for torrent in &torrents {
        subject.ensure(&magnet(&torrent.hash, &sources)).unwrap();
    }
    wait_for(
        Instant::now() + Duration::from_secs(45),
        "unlimited queue downloads",
        || {
            torrents
                .iter()
                .all(|t| subject.check(&t.hash).unwrap().ready)
        },
    );
    assert!(sampler.max_running.load(Ordering::Relaxed) >= 2);
    assert!(sampler.max_running.load(Ordering::Relaxed) <= config.download_slots());
    for torrent in &torrents {
        assert!(same_file(
            &config.data_dir.join(&torrent.hash).join(&torrent.name),
            &torrent.file
        ));
        let value = subject.transfer(&torrent.hash).unwrap();
        assert_eq!(
            value.get("download_queue_limit").and_then(Json::as_str),
            Some("0")
        );
    }
}

#[test]
fn independent_seed_queue_enforces_fifo_priority_pause_and_restart_without_losing_payload() {
    let directory = scratch();
    let pair = Pair::new();
    let library = directory.0.join("library");
    let torrents = torrents(&library, 3, 128 << 10, BLOCK);
    let mut config = download_config(&directory.0.join("subject"), true, 1, 2);
    config.max_seeding = 1;
    for torrent in &torrents {
        let base = config.data_dir.join(&torrent.hash);
        fs::create_dir_all(&base).unwrap();
        fs::copy(&torrent.file, base.join(&torrent.name)).unwrap();
    }
    let open = || {
        Client::open_with_policy_and_admission(
            config.clone(),
            TransferPolicy::default(),
            Arc::new(|_| Ok(true)),
            pair.client_network.clone(),
        )
        .unwrap()
    };
    let subject = open();
    for torrent in &torrents {
        subject
            .ensure(
                library
                    .join(format!("{}.torrent", torrent.name))
                    .to_str()
                    .unwrap(),
            )
            .unwrap();
    }
    let queued = |client: &Client, n: usize| {
        client
            .transfer(&torrents[n].hash)
            .unwrap()
            .get("status")
            .and_then(Json::as_str)
            == Some("queued_seed")
    };
    wait_for(
        Instant::now() + Duration::from_secs(30),
        "seed queue publication",
        || {
            subject.check(&torrents[0].hash).unwrap().ready
                && subject.policy.seed_queue.allowed(&torrents[0].hash)
                && queued(&subject, 1)
                && queued(&subject, 2)
        },
    );
    assert!(subject.policy.seed_queue.allowed(&torrents[0].hash));
    // A queued seed refuses an actual peer before advertising or sending data.
    let mut stream = pair
        .server_network
        .connect(
            SocketAddr::from(([10, 7, 0, 2], subject.listen_port())),
            Duration::from_secs(5),
        )
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut handshake = [0; 68];
    handshake[0] = 19;
    handshake[1..20].copy_from_slice(b"BitTorrent protocol");
    handshake[28..48].copy_from_slice(&decode_hex::<20>(&torrents[1].hash).unwrap());
    stream.write_all(&handshake).unwrap();
    assert!(stream.read_exact(&mut [0; 68]).is_err());
    assert!(
        !subject
            .transfer(&torrents[1].hash)
            .unwrap()
            .get("failed")
            .unwrap()
            .as_bool()
            .unwrap()
    );
    subject.pause(&torrents[0].hash).unwrap();
    wait_for(
        Instant::now() + Duration::from_secs(15),
        "next FIFO seed",
        || subject.policy.seed_queue.allowed(&torrents[1].hash),
    );
    subject.set_priority(&torrents[2].hash, 10).unwrap();
    wait_for(
        Instant::now() + Duration::from_secs(15),
        "seed priority",
        || subject.policy.seed_queue.allowed(&torrents[2].hash) && queued(&subject, 1),
    );
    drop(subject);
    let subject = open();
    wait_for(
        Instant::now() + Duration::from_secs(30),
        "restarted seed admission",
        || subject.policy.seed_queue.allowed(&torrents[2].hash),
    );
    assert!(subject.controls(&torrents[0].hash).unwrap().user_paused);
    assert_eq!(subject.controls(&torrents[2].hash).unwrap().priority, 10);
    let before = subject
        .controls(&torrents[1].hash)
        .unwrap()
        .seed_elapsed_secs;
    let consumer = Client::open_with_policy_and_admission(
        download_config(&directory.0.join("consumer"), false, 0, 1),
        TransferPolicy {
            download_limit_bps: 64 << 10,
            ..TransferPolicy::default()
        },
        Arc::new(|_| Ok(true)),
        pair.server_network.clone(),
    )
    .unwrap();
    wait_for(
        Instant::now() + Duration::from_secs(15),
        "seed rehash",
        || {
            subject.check(&torrents[2].hash).unwrap().ready
                && subject
                    .jobs
                    .lock()
                    .unwrap()
                    .get(&torrents[2].hash)
                    .is_some_and(|j| !j.needs_rehash && !j.running)
        },
    );
    consumer
        .ensure(&format!(
            "magnet:?xt=urn:btih:{}&x.pe=10.7.0.2:{}",
            torrents[2].hash,
            subject.listen_port()
        ))
        .unwrap();
    wait_for(
        Instant::now() + Duration::from_secs(30),
        "admitted seed upload",
        || consumer.check(&torrents[2].hash).unwrap().ready,
    );
    assert!(same_file(
        &consumer
            .config
            .data_dir
            .join(&torrents[2].hash)
            .join(&torrents[2].name),
        &torrents[2].file
    ));
    assert!(queued(&subject, 1));
    assert_eq!(
        subject
            .controls(&torrents[1].hash)
            .unwrap()
            .seed_elapsed_secs,
        before
    );
    for torrent in &torrents {
        assert!(same_file(
            &config.data_dir.join(&torrent.hash).join(&torrent.name),
            &torrent.file
        ));
    }
}
