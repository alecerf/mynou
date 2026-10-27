//! Opt-in provider check. Reads a private profile without persisting credentials.
use mynou::{
    torrent::inspect_metadata, tunnel::TunnelState, tunnel_http::HttpTunnel, wireguard::profile,
};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: torrent_vpn_probe PRIVATE_PROFILE TORRENT_HTTPS_URL".into());
    }
    let config = profile::read_private_file(Path::new(&args[1]))
        .map_err(|e| e.to_string())?
        .into_tunnel_config();
    let started = Instant::now();
    let driver = HttpTunnel::start(config).map_err(|e| e.to_string())?;
    loop {
        let status = driver
            .status(Instant::now() + Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        if status.state == TunnelState::Protected {
            break;
        }
        if started.elapsed() > Duration::from_secs(30) {
            return Err("provider handshake timed out".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    let handshake_s = started.elapsed().as_secs_f64();
    let network = driver
        .network(Instant::now() + Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    let generation = network.generation()?;
    let started = Instant::now();
    let metadata = inspect_metadata(&*network, &args[2], started + Duration::from_secs(45))?;
    let metadata_s = started.elapsed().as_secs_f64();
    let stable = generation.is_some() && network.generation()? == generation;
    driver.cancel();
    let revoked = network.publication().is_err();
    println!(
        "{{\"handshake_s\":{handshake_s},\"metadata_s\":{metadata_s},\"id\":\"{}\",\"files\":{},\"same_protected_generation\":{stable},\"revocation_rejected\":{revoked}}}",
        metadata.id,
        metadata.files.len()
    );
    if !stable || !revoked {
        return Err("protected generation check failed".into());
    }
    Ok(())
}
