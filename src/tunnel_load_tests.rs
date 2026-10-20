//! Tunnel owner load (#313): an opt-in measurement harness for the macOS
//! evidence workflow and a required regression of the explicit flow limit.
//! Both owners run in this process over loopback WireGuard UDP; the responder
//! is the same implementation with generous limits so the client is measured.
use super::*;
use crate::{
    json::{self, Value},
    torrent::{
        transport::Network,
        wire::{read_exact_deadline, write_all_deadline},
    },
};
use std::{net::SocketAddr, sync::Barrier};

const PORT: u16 = 6881;
const DONE: &[u8] = b"done";

fn peer() -> SocketAddr {
    SocketAddr::from((SERVER, PORT))
}

fn generous(server: &mut TunnelConfig) {
    server.tcp.flows = 512;
    server.tcp.backlog = 512;
    server.tcp.total_buffer = 128 << 20;
}

/// Accepts flows until stopped; each reads `bytes`, answers `done` and keeps
/// its flow until the client closes, like a seeding peer.
fn responder(network: &dyn Network, bytes: usize, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    let listener = network.listen("0.0.0.0:6881".parse().unwrap()).unwrap();
    assert_eq!(listener.port(), PORT);
    thread::spawn(move || {
        let mut handlers = Vec::new();
        while !stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let stop = stop.clone();
                    handlers.push(thread::spawn(move || {
                        let deadline = Instant::now() + Duration::from_secs(120);
                        // Chunked, so responder memory does not scale with payloads.
                        let mut chunk = [0; 16 * 1024];
                        let mut offset = 0;
                        while offset < bytes {
                            let part = &mut chunk[..(bytes - offset).min(16 * 1024)];
                            read_exact_deadline(&mut *stream, part, Some(&stop), deadline)?;
                            if part
                                .iter()
                                .enumerate()
                                .any(|(i, b)| *b != pattern(offset + i))
                            {
                                return Err("corrupted payload".into());
                            }
                            offset += part.len();
                        }
                        write_all_deadline(&mut *stream, DONE, Some(&stop), deadline)?;
                        let mut end = [0; 1];
                        let _ = read_exact_deadline(&mut *stream, &mut end, Some(&stop), deadline);
                        crate::Result::Ok(())
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break,
            }
        }
        for handler in handlers {
            let _ = handler.join();
        }
    })
}

fn pattern(index: usize) -> u8 {
    (index * 7 % 251) as u8
}

/// Tunnel counters once the load has drained; a full queue is retried.
fn counters(driver: &HttpTunnel) -> crate::tunnel::Stats {
    let deadline = end();
    loop {
        match driver.telemetry(deadline) {
            Ok((_, stats, _)) => return stats,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => pause(deadline).unwrap(),
            Err(error) => panic!("{error}"),
        }
    }
}

fn transfer(stream: &mut dyn NetworkStream, payload: &[u8]) -> crate::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    write_all_deadline(stream, payload, None, deadline)?;
    let mut reply = [0; 4];
    read_exact_deadline(stream, &mut reply, None, deadline)?;
    if reply != DONE {
        return Err("unexpected reply".into());
    }
    Ok(())
}

#[test]
fn flows_above_the_limit_are_refused_at_once_and_released_flows_are_reusable() {
    let pair = Pair::with(|client, server| {
        client.tcp.flows = 4;
        generous(server);
    });
    let stop = Arc::new(AtomicBool::new(false));
    let worker = responder(&*pair.server_network, 4096, stop.clone());
    let payload: Vec<u8> = (0..4096).map(pattern).collect();
    let (_, before, _) = pair.client.telemetry(end()).unwrap();
    let mut admitted: Vec<_> = (0..4)
        .map(|_| {
            pair.client_network
                .connect(peer(), Duration::from_secs(5))
                .unwrap()
        })
        .collect();
    for _ in 0..4 {
        let started = Instant::now();
        assert!(
            pair.client_network
                .connect(peer(), Duration::from_secs(5))
                .is_err()
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no wait for a free flow"
        );
    }
    for stream in &mut admitted {
        transfer(&mut **stream, &payload).unwrap();
    }
    let (status, stats, _) = pair.client.telemetry(end()).unwrap();
    assert_eq!(status.state, TunnelState::Protected);
    assert_eq!(
        stats.generations, before.generations,
        "refusal never replaces the generation"
    );
    // Dropped streams are aborted by the owner, which frees their slots.
    drop(admitted);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut again = loop {
        match pair.client_network.connect(peer(), Duration::from_secs(5)) {
            Ok(stream) => break stream,
            Err(_) => pause(deadline).unwrap(),
        }
    };
    transfer(&mut *again, &payload).unwrap();
    drop(again);
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
}

/// `MYNOU_TUNNEL_LOAD_FLOWS` peer connections, each sending
/// `MYNOU_TUNNEL_LOAD_BYTES`, reported on one `MYNOU_TUNNEL_LOAD` line.
/// `MYNOU_TUNNEL_LOAD_CONNECT` is `concurrent` (one thread per connection,
/// started together) or `serial` (one after another, then transfers in
/// parallel). Without the variables the test returns at once.
#[test]
fn tunnel_load_report() {
    let Some(flows) = std::env::var("MYNOU_TUNNEL_LOAD_FLOWS").ok() else {
        return;
    };
    let flows: usize = flows.parse().unwrap();
    let bytes: usize = std::env::var("MYNOU_TUNNEL_LOAD_BYTES")
        .unwrap()
        .parse()
        .unwrap();
    let mode = std::env::var("MYNOU_TUNNEL_LOAD_CONNECT").unwrap();
    let serial = match mode.as_str() {
        "serial" => true,
        "concurrent" => false,
        _ => panic!("unknown connect mode"),
    };
    let started = Instant::now();
    let pair = Pair::with(|_, server| generous(server));
    let protected_us = started.elapsed().as_micros();
    let stop = Arc::new(AtomicBool::new(false));
    let worker = responder(&*pair.server_network, bytes, stop.clone());
    let payload: Arc<Vec<u8>> = Arc::new((0..bytes).map(pattern).collect());
    // Every flow connects before any transfers, so admission sees them all.
    let connected = Arc::new(Barrier::new(flows + 1));
    let go = Arc::new(Barrier::new(flows + 1));
    let mut opened: Vec<_> = (0..flows)
        .map(|_| {
            serial.then(|| {
                let begun = Instant::now();
                let stream = pair.client_network.connect(peer(), Duration::from_secs(30));
                (begun.elapsed(), stream)
            })
        })
        .collect();
    let workers: Vec<_> = opened
        .drain(..)
        .map(|opened| {
            let (network, payload) = (pair.client_network.clone(), payload.clone());
            let (connected, go) = (connected.clone(), go.clone());
            thread::spawn(move || {
                let (connect, stream) = opened.unwrap_or_else(|| {
                    let begun = Instant::now();
                    let stream = network.connect(peer(), Duration::from_secs(30));
                    (begun.elapsed(), stream)
                });
                connected.wait();
                go.wait();
                let mut stream = stream.map_err(|e| (connect, e.to_string()))?;
                let begun = Instant::now();
                transfer(&mut *stream, &payload).map_err(|e| (connect, e.to_string()))?;
                let elapsed = begun.elapsed();
                Ok::<_, (Duration, String)>((connect, elapsed, stream))
            })
        })
        .collect();
    connected.wait();
    let transfers = Instant::now();
    go.wait();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    let transfer_us = transfers.elapsed().as_micros();
    let mut completed = Vec::new();
    let mut connects = Vec::new();
    let mut refusals = BTreeMap::<String, u32>::new();
    let mut streams = Vec::new();
    for result in results {
        match result {
            Ok((connect, elapsed, stream)) => {
                connects.push(connect.as_micros());
                completed.push(elapsed.as_micros());
                streams.push(stream);
            }
            Err((connect, message)) => {
                connects.push(connect.as_micros());
                *refusals.entry(message).or_default() += 1;
            }
        }
    }
    drop(streams);
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    let (client, server) = (counters(&pair.client), counters(&pair.server));
    completed.sort_unstable();
    connects.sort_unstable();
    let quantile = |values: &[u128], q: usize| {
        values
            .get((values.len().saturating_sub(1)) * q / 100)
            .map_or(String::new(), u128::to_string)
    };
    let mut report = Value::object();
    report.insert("connect", mode);
    report.insert("flows", flows.to_string());
    report.insert("bytes_per_flow", bytes.to_string());
    report.insert("protected_us", protected_us.to_string());
    report.insert("transfer_us", transfer_us.to_string());
    report.insert("completed", completed.len().to_string());
    for (name, q) in [("p50", 50), ("p90", 90), ("max", 100)] {
        report.insert(format!("connect_{name}_us"), quantile(&connects, q));
        report.insert(format!("flow_{name}_us"), quantile(&completed, q));
    }
    let mut refused = Value::object();
    for (message, count) in refusals {
        refused.insert(message, count);
    }
    report.insert("refused", refused);
    for (side, stats) in [("client", client), ("server", server)] {
        let mut counters = Value::object();
        for (name, value) in [
            ("generations", stats.generations),
            ("received", stats.received),
            ("sent", stats.sent),
            ("rejected", stats.rejected),
            ("send_errors", stats.send_errors),
            ("inner_rejected", stats.inner_rejected),
            ("inner_dropped", stats.inner_dropped),
        ] {
            counters.insert(name, value.to_string());
        }
        report.insert(side, counters);
    }
    eprintln!("MYNOU_TUNNEL_LOAD {}", json::stringify(&report));
}
