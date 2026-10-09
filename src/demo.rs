//! Fully local demonstration using synthetic media and native Mynou peers.
use crate::{
    Result,
    bencode::{self, Value as B},
    config, crypto,
    engine::{Engine, lock, public_job},
    json::{self, Value},
    store,
    torrent::{Client, DownloadConfig},
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

// Never answer an incomplete request with an empty success.
const BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Reads one request head from an accepted loopback connection and returns its path.
///
/// macOS and BSD give an accepted socket the listener's `O_NONBLOCK` flag, while
/// Linux does not, so a read could fail with `WouldBlock` before the client's
/// bytes arrived. Reads here are blocking and bounded by a per-read timeout, a
/// total deadline and 16 KiB; `None` means the head never completed.
fn read_request(stream: &mut TcpStream, deadline: Instant) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    let mut request = Vec::new();
    let mut byte = [0];
    while request.len() < 16_384 && !request.ends_with(b"\r\n\r\n") {
        if Instant::now() > deadline || stream.read_exact(&mut byte).is_err() {
            return None;
        }
        request.push(byte[0]);
    }
    if !request.ends_with(b"\r\n\r\n") {
        return None;
    }
    let text = String::from_utf8_lossy(&request);
    text.split_whitespace().nth(1).map(str::to_owned)
}

pub fn run(directory: &Path) -> Result<Value> {
    if directory.exists() {
        return Err("The demonstration directory must not already exist".into());
    }
    fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let directory = fs::canonicalize(directory).map_err(|e| e.to_string())?;
    let bytes = include_bytes!("../examples/demo.mp4");
    let pieces: Vec<u8> = bytes.chunks(16_384).flat_map(crypto::sha1).collect();
    let info = B::Dict(BTreeMap::from([
        (b"length".to_vec(), B::Int(bytes.len() as i64)),
        (b"name".to_vec(), B::Bytes(b"Mynou.Demo.2026.mp4".to_vec())),
        (b"piece length".to_vec(), B::Int(16_384)),
        (b"pieces".to_vec(), B::Bytes(pieces)),
        (b"private".to_vec(), B::Int(1)),
    ]));
    let hash = crypto::sha1(&bencode::encode(&info));
    let id: String = hash.iter().map(|v| format!("{v:02x}")).collect();
    let torrent = bencode::encode(&B::Dict(BTreeMap::from([(b"info".to_vec(), info)])));
    let torrent_path = directory.join("demo.torrent");
    fs::write(&torrent_path, torrent).map_err(|e| e.to_string())?;
    let seed_dir = directory.join("seeder/data").join(&id);
    fs::create_dir_all(&seed_dir).map_err(|e| e.to_string())?;
    fs::write(seed_dir.join("Mynou.Demo.2026.mp4"), bytes).map_err(|e| e.to_string())?;
    let seeder = Client::open(DownloadConfig {
        data_dir: directory.join("seeder/data"),
        state_dir: directory.join("seeder/state"),
        listen_port: 0,
        seed: true,
        dht: false,
        pex: false,
        max_active: 1,
        max_peers: 4,
    })?;
    seeder.ensure(
        torrent_path
            .to_str()
            .ok_or("Demo path is not valid UTF-8")?,
    )?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !seeder.check(&id)?.ready {
        if Instant::now() > deadline {
            return Err("The demo source peer did not become available".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
    let magnet = format!(
        "magnet:?xt=urn:btih:{id}&x.pe=127.0.0.1:{}",
        seeder.listen_port()
    );
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let origin = format!(
        "http://{}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    let scanned = Arc::new(AtomicBool::new(false));
    let scan = scanned.clone();
    let stub = thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let Some(path) = read_request(&mut stream, deadline) else {
                        let _ = stream.write_all(BAD_REQUEST);
                        continue;
                    };
                    let body = if path.starts_with("/watchlist") {
                        json::parse(r#"{"MediaContainer":{"Metadata":[{"type":"movie","title":"Mynou Demo","year":2026}]}}"#).unwrap_or(Value::Null)
                    } else if path.starts_with("/indexer") {
                        let mut release = Value::object();
                        release.insert("title", "Mynou.Demo.2026.1080p");
                        release.insert("magnet", magnet.clone());
                        release.insert("seeders", 1_u32);
                        Value::Array(vec![release])
                    } else if path.contains("/refresh") {
                        scan.store(true, Ordering::Release);
                        Value::object()
                    } else if path.contains("/all") || path.contains("/search") {
                        if scan.load(Ordering::Acquire) {
                            json::parse(r#"{"MediaContainer":{"Metadata":[{"type":"movie","title":"Mynou Demo","year":2026,"ratingKey":"1","Media":[{"Part":[{"file":"/library/movies/Mynou Demo (2026)/Mynou Demo (2026).mp4"}]}]}]}}"#).unwrap_or(Value::Null)
                        } else {
                            json::parse(r#"{"MediaContainer":{"Metadata":[]}}"#)
                                .unwrap_or(Value::Null)
                        }
                    } else {
                        Value::object()
                    };
                    let body = json::stringify(&body);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(_) => break,
            }
        }
    });
    let result = (|| {
        let mut value = config::default_json();
        value.insert("store_dir", "jobs");
        value.insert("listen", "127.0.0.1:0");
        value.insert("plex",json::parse(&format!(r#"{{"enabled":true,"url":"{origin}","watchlist_url":"{origin}/watchlist","token_env":"MYNOU_DEMO_TOKEN","movies_section":"1","series_section":"2"}}"#))?);
        if let Some(Value::Object(d)) = value.get_mut("downloads") {
            d.insert("listen_port".into(), Value::Number(0.0));
            d.insert("dht".into(), false.into());
            d.insert("pex".into(), false.into());
        }
        let mut indexer = Value::object();
        indexer.insert("name", "local demo");
        indexer.insert("kind", "json");
        indexer.insert("url", format!("{origin}/indexer"));
        value.insert("indexers", Value::Array(vec![indexer]));
        // Supply the fixture token through a dedicated field without changing the environment.
        let mut cfg = config::from_json(&value, &directory)?;
        cfg.plex.token_override = Some("fixture-token-mynou-local".into());
        let engine = Engine::open(cfg)?;
        engine.sync()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            engine.tick()?;
            let jobs = lock(&engine.store)?.list();
            if let Some(job) = jobs.first() {
                if job.state == "ready" {
                    let mut output = Value::object();
                    output.insert("directory", directory.to_string_lossy().into_owned());
                    output.insert("job", public_job(job));
                    output.insert(
                        "events",
                        Value::Array(
                            lock(&engine.store)?
                                .events(&job.id)
                                .iter()
                                .map(|e| e.to_json())
                                .collect(),
                        ),
                    );
                    output.insert("plex_scan_confirmed", scanned.load(Ordering::Acquire));
                    output.insert("completed_at", store::now().to_string());
                    return Ok(output);
                }
                if job.state == "failed" {
                    return Err(job.last_error.clone().unwrap_or("Demo failed".into()));
                }
            }
            if Instant::now() > deadline {
                return Err("Demonstration timed out".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    })();
    stopped.store(true, Ordering::Release);
    let _ = stub.join();
    result
}

#[cfg(test)]
mod tests {
    use super::read_request;
    use std::{
        io::Write,
        net::{TcpListener, TcpStream},
        thread,
        time::{Duration, Instant},
    };

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn fragmented_request_on_an_inherited_nonblocking_stream_is_read_completely() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            for part in ["GET /all", " HTTP/1.1\r\nHost: demo\r\n", "\r\n"] {
                thread::sleep(Duration::from_millis(50));
                stream.write_all(part.as_bytes()).unwrap();
            }
        });
        let (mut stream, _) = listener.accept().unwrap();
        // Reproduce macOS: the accepted socket keeps the listener's O_NONBLOCK.
        stream.set_nonblocking(true).unwrap();
        let path = read_request(&mut stream, deadline());
        client.join().unwrap();
        assert_eq!(path.as_deref(), Some("/all"));
    }

    #[test]
    fn incomplete_requests_are_never_parsed() {
        for request in ["", "GET /watchlist HTTP/1.1\r\n"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let client = thread::spawn(move || {
                let mut stream = TcpStream::connect(address).unwrap();
                stream.write_all(request.as_bytes()).unwrap();
            });
            let (mut stream, _) = listener.accept().unwrap();
            client.join().unwrap();
            assert_eq!(read_request(&mut stream, deadline()), None);
        }
    }
}
