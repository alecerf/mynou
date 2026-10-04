//! Synthetic local swarms exercise exclusive work, cancellation and throughput.
mod parallel_support;

use mynou::json::Value;
use mynou::torrent::{Client, TransferPolicy};
use parallel_support::{
    BLOCK, Fleet, Kind, Mode, Peer, Scratch, Torrent, configuration, ready, wait_for,
};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

fn counter(snapshot: &Value, field: &str) -> u64 {
    snapshot
        .get(field)
        .unwrap()
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

fn fleet(
    torrent: &Torrent,
    count: usize,
    delay: Duration,
    enabled: bool,
) -> (Arc<Fleet>, Vec<Peer>) {
    let live = Arc::new(Fleet::default());
    let peers = (0..count)
        .map(|_| {
            Peer::open(
                torrent,
                (0..torrent.pieces()).collect(),
                delay,
                Mode::Good,
                enabled,
                &live,
            )
        })
        .collect();
    (live, peers)
}

fn ports(peers: &[Peer]) -> Vec<u16> {
    peers.iter().map(|peer| peer.port).collect()
}

#[test]
fn disjoint_peers_contribute_exclusive_pieces_with_exact_verified_accounting() {
    let scratch = Scratch::new();
    let torrent = Torrent::new("disjoint.bin", 24, Kind::V1);
    let live = Arc::new(Fleet::default());
    let peers: Vec<_> = (0..4)
        .map(|partition| {
            Peer::open(
                &torrent,
                (0..torrent.pieces())
                    .filter(|piece| piece % 4 == partition)
                    .collect(),
                Duration::from_millis(15),
                Mode::Good,
                true,
                &live,
            )
        })
        .collect();
    let config = configuration(&scratch.0, 1, 4);
    torrent.install(&config, &ports(&peers), 1);
    let client = Client::open(config).unwrap();
    torrent.assert_files(&ready(&client, &torrent));
    let mut requested = Vec::new();
    for (partition, peer) in peers.iter().enumerate() {
        let pieces = peer.requests.lock().unwrap();
        assert!(!pieces.is_empty(), "A disjoint peer contributed nothing");
        assert!(pieces.iter().all(|piece| piece % 4 == partition));
        requested.extend_from_slice(&pieces);
    }
    requested.sort_unstable();
    assert_eq!(requested, (0..torrent.pieces()).collect::<Vec<_>>());
    let snapshot = client.transfer(&torrent.id).unwrap();
    assert_eq!(
        counter(&snapshot, "verified_bytes"),
        torrent.data.len() as u64
    );
    assert_eq!(
        counter(&snapshot, "downloaded_bytes"),
        torrent.data.len() as u64
    );
}

#[test]
fn authenticated_v2_proofs_from_different_peers_survive_offline_restart() {
    for kind in [Kind::V2, Kind::Hybrid] {
        let scratch = Scratch::new();
        let torrent = Torrent::new("merkle.bin", 16, kind);
        let live = Arc::new(Fleet::default());
        let peers: Vec<_> = (0..4)
            .map(|partition| {
                Peer::open(
                    &torrent,
                    (0..torrent.pieces())
                        .filter(|piece| piece % 4 == partition)
                        .collect(),
                    Duration::from_millis(15),
                    Mode::Good,
                    true,
                    &live,
                )
            })
            .collect();
        let config = configuration(&scratch.0, 1, 4);
        torrent.install(&config, &ports(&peers), 1);
        let client = Client::open(config.clone()).unwrap();
        torrent.assert_files(&ready(&client, &torrent));
        assert!(
            peers
                .iter()
                .all(|peer| !peer.requests.lock().unwrap().is_empty())
        );
        if matches!(kind, Kind::V2) {
            assert!(
                peers
                    .iter()
                    .all(|peer| peer.proof_requests.load(Ordering::Acquire) > 0)
            );
        }
        drop(client);
        drop(peers);
        let reopened = Client::open(config).unwrap();
        torrent.assert_files(&ready(&reopened, &torrent));
        assert_eq!(
            counter(&reopened.transfer(&torrent.id).unwrap(), "verified_bytes"),
            torrent.data.len() as u64
        );
    }
}

#[test]
fn a_corrupt_peer_is_quarantined_without_discarding_honest_work() {
    let scratch = Scratch::new();
    let torrent = Torrent::new("corrupt.bin", 24, Kind::V1);
    let live = Arc::new(Fleet::default());
    let bad = Peer::open(
        &torrent,
        (0..torrent.pieces()).collect(),
        Duration::ZERO,
        Mode::CorruptOnce,
        true,
        &live,
    );
    let good = Peer::open(
        &torrent,
        (0..torrent.pieces()).collect(),
        Duration::from_millis(25),
        Mode::Good,
        true,
        &live,
    );
    let config = configuration(&scratch.0, 1, 2);
    torrent.install(&config, &[bad.port, good.port], 1);
    let client = Client::open(config).unwrap();
    torrent.assert_files(&ready(&client, &torrent));
    assert_eq!(
        bad.requests.lock().unwrap().len(),
        1,
        "Corrupt peer was retried"
    );
    let mut honest = good.requests.lock().unwrap().clone();
    honest.sort_unstable();
    assert_eq!(honest, (0..torrent.pieces()).collect::<Vec<_>>());
    let snapshot = client.transfer(&torrent.id).unwrap();
    assert_eq!(
        counter(&snapshot, "verified_bytes"),
        torrent.data.len() as u64
    );
    assert_eq!(
        counter(&snapshot, "downloaded_bytes"),
        torrent.data.len() as u64 + BLOCK as u64
    );
}

#[test]
fn an_earlier_silent_handshake_does_not_hold_back_a_usable_peer() {
    let scratch = Scratch::new();
    let torrent = Torrent::new("stalled.bin", 8, Kind::V1);
    let live = Arc::new(Fleet::default());
    let first = TcpListener::bind("127.0.0.1:0").unwrap();
    let second = TcpListener::bind("127.0.0.1:0").unwrap();
    let (earlier, later) =
        if first.local_addr().unwrap().port() < second.local_addr().unwrap().port() {
            (first, second)
        } else {
            (second, first)
        };
    let bad = Peer::with_listener(
        earlier,
        &torrent,
        (0..torrent.pieces()).collect(),
        Duration::ZERO,
        Mode::StallHandshake,
        true,
        &live,
    );
    let good = Peer::with_listener(
        later,
        &torrent,
        (0..torrent.pieces()).collect(),
        Duration::ZERO,
        Mode::Good,
        true,
        &live,
    );
    assert!(bad.port < good.port);
    let config = configuration(&scratch.0, 1, 2);
    torrent.install(&config, &[bad.port, good.port], 1);
    let started = Instant::now();
    let client = Client::open(config).unwrap();
    torrent.assert_files(&ready(&client, &torrent));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "Waited for the silent peer's five-second handshake deadline"
    );
}

#[test]
fn paused_parallel_workers_retire_and_restart_requires_explicit_resume() {
    let scratch = Scratch::new();
    let torrent = Torrent::new("paused.bin", 24, Kind::V1);
    let (live, peers) = fleet(&torrent, 4, Duration::ZERO, false);
    let config = configuration(&scratch.0, 1, 4);
    torrent.install(&config, &ports(&peers), 1);
    let client = Client::open(config.clone()).unwrap();
    wait_for(Duration::from_secs(5), || {
        peers
            .iter()
            .all(|peer| !peer.requests.lock().unwrap().is_empty())
    });
    client.pause(&torrent.id).unwrap();
    wait_for(Duration::from_secs(3), || {
        live.active.load(Ordering::Acquire) == 0
    });
    assert!(!client.check(&torrent.id).unwrap().ready);
    assert!(client.controls(&torrent.id).unwrap().user_paused);
    drop(client);
    let reopened = Client::open(config).unwrap();
    reopened.resume_if_allowed(&torrent.id).unwrap();
    thread::sleep(Duration::from_millis(150));
    assert!(reopened.controls(&torrent.id).unwrap().user_paused);
    assert_eq!(live.active.load(Ordering::Acquire), 0);
    for peer in &peers {
        peer.release();
    }
    reopened.resume(&torrent.id).unwrap();
    torrent.assert_files(&ready(&reopened, &torrent));
}

#[test]
fn a_download_rate_limit_is_shared_by_all_parallel_peers() {
    let scratch = Scratch::new();
    let torrent = Torrent::new("limited.bin", 48, Kind::V1);
    let (_, peers) = fleet(&torrent, 4, Duration::ZERO, true);
    let config = configuration(&scratch.0, 1, 4);
    torrent.install(&config, &ports(&peers), 1);
    let limit = 128 * 1024_u64;
    let client = Client::open_with_policy(
        config,
        TransferPolicy {
            download_limit_bps: limit,
            ..TransferPolicy::default()
        },
    )
    .unwrap();
    wait_for(Duration::from_secs(5), || {
        peers
            .iter()
            .all(|peer| !peer.requests.lock().unwrap().is_empty())
    });
    let before = counter(&client.transfer(&torrent.id).unwrap(), "downloaded_bytes");
    let start = Instant::now();
    thread::sleep(Duration::from_millis(600));
    let delta = counter(&client.transfer(&torrent.id).unwrap(), "downloaded_bytes") - before;
    // Reservations pace requests. Allow the burst and four previously issued
    // responses; this is not an instantaneous limit on delayed network traffic.
    let maximum =
        (limit as u128 * start.elapsed().as_nanos() / 1_000_000_000) as u64 + 5 * BLOCK as u64;
    assert!(
        delta > 0 && delta <= maximum,
        "Aggregate payload exceeded shared rate: {delta} > {maximum}"
    );
    torrent.assert_files(&ready(&client, &torrent));
    assert_eq!(
        counter(&client.transfer(&torrent.id).unwrap(), "downloaded_bytes"),
        torrent.data.len() as u64
    );
}

#[test]
fn peer_slots_remain_bounded_and_queue_priority_does_not_preempt_active_work() {
    let scratch = Scratch::new();
    let first = Torrent::new("first.bin", 16, Kind::V1);
    let second = Torrent::new("second.bin", 16, Kind::V1);
    let (live, first_peers) = fleet(&first, 4, Duration::ZERO, false);
    let (_, second_peers) = fleet(&second, 4, Duration::ZERO, true);
    let config = configuration(&scratch.0, 1, 2);
    first.install(&config, &ports(&first_peers), 1);
    second.install(&config, &ports(&second_peers), 2);
    let client = Client::open(config).unwrap();
    wait_for(Duration::from_secs(5), || {
        first_peers
            .iter()
            .map(|peer| peer.requests.lock().unwrap().len())
            .sum::<usize>()
            == 2
    });
    assert!(live.peak.load(Ordering::Acquire) <= 2);
    assert!(
        second_peers
            .iter()
            .all(|peer| peer.requests.lock().unwrap().is_empty())
    );
    client.set_priority(&second.id, 1000).unwrap();
    thread::sleep(Duration::from_millis(150));
    assert!(
        second_peers
            .iter()
            .all(|peer| peer.requests.lock().unwrap().is_empty())
    );
    for peer in &first_peers {
        peer.release();
    }
    first.assert_files(&ready(&client, &first));
    second.assert_files(&ready(&client, &second));
}

#[test]
fn measured_parallel_transfers_improve_the_delayed_single_peer_baseline() {
    let scratch = Scratch::new();
    let torrent = Torrent::new("throughput.bin", 48, Kind::V1);
    let (live, peers) = fleet(&torrent, 4, Duration::from_millis(100), true);
    let transfer = |name: &str, max_peers| {
        let config = configuration(&scratch.0.join(name), 1, max_peers);
        torrent.install(&config, &ports(&peers), 1);
        let start = Instant::now();
        let client = Client::open(config).unwrap();
        torrent.assert_files(&ready(&client, &torrent));
        let elapsed = start.elapsed();
        drop(client);
        wait_for(Duration::from_secs(3), || {
            live.active.load(Ordering::Acquire) == 0
        });
        elapsed
    };
    let baseline = transfer("single", 1);
    let parallel = transfer("parallel", 4);
    let speedup = baseline.as_secs_f64() / parallel.as_secs_f64();
    println!(
        "native_parallel_throughput baseline_max_peers=1 baseline_ms={} parallel_max_peers=4 parallel_ms={} payload_bytes={} block_delay_ms=100 speedup={speedup:.3}",
        baseline.as_millis(),
        parallel.as_millis(),
        torrent.data.len()
    );
    assert!(
        speedup > 1.67,
        "Parallel delayed fixture did not improve throughput: {speedup:.3}"
    );
    assert!(live.peak.load(Ordering::Acquire) <= 4);
    assert_eq!(
        peers
            .iter()
            .map(|peer| peer.payload_bytes.load(Ordering::Acquire))
            .sum::<u64>(),
        2 * torrent.data.len() as u64
    );
}
