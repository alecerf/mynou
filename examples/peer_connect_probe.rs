//! Opt-in measurement: how long real peers take to accept a TCP connection,
//! through the VPN and directly, to choose the peer connect timeout.
//!
//! ```sh
//! cargo run --release --example peer_connect_probe -- PRIVATE_PROFILE INFO_HASH_HEX [PEERS]
//! ```
//!
//! The profile is a WireGuard configuration as Settings, VPN takes it; it is
//! read, not stored. The info hash should be that of a busy public torrent, such
//! as a current Linux distribution image. The DHT is searched through the
//! tunnel (nothing is announced), then up to `PEERS` IPv4 peers (default 200)
//! are dialled through the tunnel and directly, 16 at a time, each with a
//! `LIMIT` second timeout (default 6). `DIRECT=0` skips the direct dials; a
//! profile of `-` measures direct connections only, without a VPN.
//! Mynou gives one attempt 1 second while downloading and 2 seconds while it
//! fetches metadata; the report says how many successful connections took
//! longer than each, which those timeouts would have lost. Each connected peer
//! is then sent a BitTorrent handshake: the time to its answer, or to the
//! connection closing, shows when a VPN accepts connections on the peers'
//! behalf (Proton's VPN Accelerator does), so that connecting proves nothing.
use mynou::{
    torrent::{probe_dht_peers, transport::Network},
    tunnel::TunnelState,
    tunnel_http::HttpTunnel,
    wireguard::profile,
};
use std::{
    io::{ErrorKind, Read, Write},
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const BOUNDS: [f64; 6] = [0.25, 0.5, 1.0, 2.0, 3.0, 4.0];

fn hash(text: &str) -> Result<[u8; 20], String> {
    if text.len() != 40 {
        return Err("the info hash is 40 hexadecimal digits".into());
    }
    let mut out = [0; 20];
    for (n, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * n..2 * n + 2], 16)
            .map_err(|_| "the info hash is 40 hexadecimal digits")?;
    }
    Ok(out)
}

/// What dialling the peers gave: connection times, then times to the peer's
/// BitTorrent handshake, counted from the start of the dial.
#[derive(Default)]
struct Dials {
    connected: Vec<f64>,
    refused: usize,
    answered: Vec<f64>,
    /// Connected, then closed or reset before answering: with a VPN that
    /// accepts connections itself, a dead peer looks like this.
    closed: Vec<f64>,
    silent: usize,
}

fn handshake(hash: &[u8; 20]) -> Vec<u8> {
    let mut out = vec![19];
    out.extend_from_slice(b"BitTorrent protocol");
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(hash);
    out.extend_from_slice(b"-MY0000-probeprobe00");
    out
}

fn dial(network: Arc<dyn Network>, peers: &[SocketAddr], hash: [u8; 20], limit: Duration) -> Dials {
    let queue = Arc::new(Mutex::new(peers.to_vec()));
    let results = Arc::new(Mutex::new(Dials::default()));
    let workers: Vec<_> = (0..16)
        .map(|_| {
            let (queue, results, network) = (queue.clone(), results.clone(), network.clone());
            thread::spawn(move || {
                loop {
                    let Some(peer) = queue.lock().unwrap().pop() else {
                        return;
                    };
                    let started = Instant::now();
                    let Ok(mut stream) = network.connect(peer, limit) else {
                        results.lock().unwrap().refused += 1;
                        continue;
                    };
                    let connected = started.elapsed().as_secs_f64();
                    let _ = stream.set_read_timeout(Some(limit));
                    let _ = stream.set_write_timeout(Some(limit));
                    let mut reply = [0; 68];
                    let outcome = stream
                        .write_all(&handshake(&hash))
                        .and_then(|()| stream.read_exact(&mut reply));
                    let elapsed = started.elapsed().as_secs_f64();
                    let mut results = results.lock().unwrap();
                    results.connected.push(connected);
                    match outcome {
                        Ok(()) => results.answered.push(elapsed),
                        Err(e)
                            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                        {
                            results.silent += 1
                        }
                        Err(_) => results.closed.push(elapsed),
                    }
                }
            })
        })
        .collect();
    for worker in workers {
        let _ = worker.join();
    }
    let mut dials = std::mem::take(&mut *results.lock().unwrap());
    for times in [&mut dials.connected, &mut dials.answered, &mut dials.closed] {
        times.sort_by(f64::total_cmp);
    }
    dials
}

/// Up to `wanted` IPv4 peers from three DHT searches.
fn discover(
    network: &dyn Network,
    hash: &[u8; 20],
    wanted: usize,
) -> Result<Vec<SocketAddr>, String> {
    let mut peers = Vec::new();
    for _ in 0..3 {
        for peer in probe_dht_peers(network, hash)? {
            if peer.is_ipv4() && !peers.contains(&peer) {
                peers.push(peer);
            }
        }
        if peers.len() >= wanted {
            break;
        }
    }
    peers.truncate(wanted);
    println!("{} IPv4 peers from the DHT", peers.len());
    Ok(peers)
}

fn summary(times: &[f64]) -> String {
    let percentile = |p: f64| {
        times
            .get(((times.len() as f64 * p) as usize).min(times.len().saturating_sub(1)))
            .copied()
            .unwrap_or(f64::NAN)
    };
    format!(
        "median {:.2} s, p90 {:.2} s, p99 {:.2} s",
        percentile(0.5),
        percentile(0.9),
        percentile(0.99)
    )
}

fn report(name: &str, dials: &Dials) {
    let times = &dials.connected;
    println!(
        "{name}: {} connected, {} failed to connect; {}",
        times.len(),
        dials.refused,
        summary(times)
    );
    println!(
        "  BitTorrent answer from {}: {}",
        dials.answered.len(),
        summary(&dials.answered)
    );
    println!(
        "  closed before answering {}: {}; silent {}",
        dials.closed.len(),
        summary(&dials.closed),
        dials.silent
    );
    for bound in BOUNDS {
        let slower = times.iter().filter(|&&t| t > bound).count();
        println!(
            "  slower than {bound:>4} s: {slower:>4} ({:.0}% of connections)",
            100.0 * slower as f64 / times.len().max(1) as f64
        );
    }
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if !(3..=4).contains(&args.len()) {
        return Err("usage: peer_connect_probe PRIVATE_PROFILE INFO_HASH_HEX [PEERS]".into());
    }
    let hash = hash(&args[2])?;
    let wanted: usize = args.get(3).and_then(|n| n.parse().ok()).unwrap_or(200);
    let setting = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let limit = Duration::from_secs(setting("LIMIT", 6));
    if args[1] == "-" {
        let direct = mynou::torrent::transport::direct();
        let peers = discover(&*direct, &hash, wanted)?;
        report("direct", &dial(direct, &peers, hash, limit));
        return Ok(());
    }
    let config = profile::read_private_file(Path::new(&args[1]))
        .map_err(|e| e.to_string())?
        .into_tunnel_config();
    let started = Instant::now();
    let driver = HttpTunnel::start(config).map_err(|e| e.to_string())?;
    while driver
        .status(Instant::now() + Duration::from_secs(2))
        .map_err(|e| e.to_string())?
        .state
        != TunnelState::Protected
    {
        if started.elapsed() > Duration::from_secs(30) {
            return Err("provider handshake timed out".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    let tunnel: Arc<dyn Network> = driver
        .network(Instant::now() + Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    let peers = discover(&*tunnel, &hash, wanted)?;
    report("through the VPN", &dial(tunnel, &peers, hash, limit));
    if setting("DIRECT", 1) != 0 {
        report(
            "direct",
            &dial(mynou::torrent::transport::direct(), &peers, hash, limit),
        );
    }
    driver.cancel();
    Ok(())
}
