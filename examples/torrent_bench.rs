//! Small public-API driver for .github/scripts/torrent_compare.py.
use mynou::torrent::{Client, DownloadConfig, TransferPolicy};
use std::{
    io,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 {
        return Err("usage: torrent_bench download|seed ROOT SOURCE PEERS RATE_BPS".into());
    }
    let start = Instant::now();
    let root = PathBuf::from(&args[2]);
    let config = DownloadConfig {
        data_dir: root.join("data"),
        state_dir: root.join("state"),
        listen_port: 0,
        seed: args[1] == "seed",
        dht: false,
        pex: false,
        max_active: 2,
        max_seeding: 0,
        max_peers: 4,
    };
    let policy = TransferPolicy {
        download_limit_bps: args[5].parse().map_err(|_| "invalid rate")?,
        ..Default::default()
    };
    let client = Client::open_with_policy(config, policy)?;
    let opened = start.elapsed();
    let mut source = args[3].clone();
    if source.starts_with("magnet:") {
        for peer in args[4].split(',').filter(|p| !p.is_empty()) {
            source.push_str("&x.pe=");
            source.push_str(peer);
        }
    }
    let id = client.ensure(&source)?.id;
    let admitted = start.elapsed();
    loop {
        let status = client.check(&id)?;
        if status.ready {
            break;
        }
        if start.elapsed() > Duration::from_secs(180) {
            return Err(format!("benchmark timed out: {}", status.message));
        }
        thread::sleep(Duration::from_millis(5));
    }
    let ready = start.elapsed();
    if args[1] == "seed" {
        println!(
            "{{\"ready_s\":{},\"port\":{},\"id\":\"{}\"}}",
            ready.as_secs_f64(),
            client.listen_port(),
            id
        );
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
    } else {
        let stopping = Instant::now();
        drop(client);
        println!(
            "{{\"ready_s\":{},\"open_s\":{},\"admission_s\":{},\"work_s\":{},\"shutdown_s\":{}}}",
            ready.as_secs_f64(),
            opened.as_secs_f64(),
            (admitted - opened).as_secs_f64(),
            (ready - admitted).as_secs_f64(),
            stopping.elapsed().as_secs_f64()
        );
    }
    Ok(())
}
