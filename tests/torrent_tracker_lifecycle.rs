use mynou::{
    bencode::{Value, encode},
    crypto::sha1,
    torrent::{Client, DownloadConfig},
};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
fn d(items: &[(&[u8], Value)]) -> Value {
    Value::Dict(items.iter().map(|(k, v)| (k.to_vec(), v.clone())).collect())
}
fn hex(v: &[u8]) -> String {
    v.iter().map(|b| format!("{b:02x}")).collect()
}
fn config(root: &std::path::Path, seed: bool) -> DownloadConfig {
    DownloadConfig {
        data_dir: root.join("downloads"),
        state_dir: root.join("state"),
        listen_port: 0,
        seed,
        dht: false,
        pex: false,
        max_active: 2,
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let started = Instant::now();
    while !predicate() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "Deadline exceeded"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn tracker_events_counters_and_interval_follow_real_transfer_and_pause() {
    let root = std::env::temp_dir().join(format!(
        "mynou-tracker-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("dir");
    let listener = TcpListener::bind("127.0.0.1:0").expect("tracker");
    listener.set_nonblocking(true).expect("nonblocking");
    let address: SocketAddr = listener.local_addr().expect("address");
    let events = Arc::new(Mutex::new(Vec::<BTreeMap<String, String>>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let events = events.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("accept: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("timeout");
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                reader.read_line(&mut line).expect("line");
                let path = line.split_whitespace().nth(1).expect("path");
                let query = path.split_once('?').expect("query").1;
                let params = query
                    .split('&')
                    .filter_map(|v| v.split_once('='))
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect();
                loop {
                    line.clear();
                    reader.read_line(&mut line).expect("header");
                    if line == "\r\n" {
                        break;
                    }
                }
                events.lock().expect("events").push(params);
                let body = encode(&d(&[
                    (b"interval", Value::Int(30)),
                    (b"peers", Value::Bytes(Vec::new())),
                ]));
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).expect("head");
                stream.write_all(&body).expect("body");
            }
        })
    };
    let data: Vec<u8> = (0..32768 + 17).map(|n| (n % 251) as u8).collect();
    let info = d(&[
        (b"name", Value::Bytes(b"data.bin".to_vec())),
        (b"piece length", Value::Int(16384)),
        (b"length", Value::Int(data.len() as i64)),
        (
            b"pieces",
            Value::Bytes(data.chunks(16384).flat_map(sha1).collect()),
        ),
        (b"private", Value::Int(1)),
    ]);
    let id = hex(&sha1(&encode(&info)));
    let source = root.join("source.torrent");
    fs::write(
        &source,
        encode(&d(&[
            (
                b"announce",
                Value::Bytes(format!("http://{address}/announce").into_bytes()),
            ),
            (b"info", info),
        ])),
    )
    .expect("torrent");
    let seed_root = root.join("seed");
    let seed_file = seed_root.join("downloads").join(&id).join("data.bin");
    fs::create_dir_all(seed_file.parent().expect("parent")).expect("dir");
    fs::write(&seed_file, &data).expect("data");
    let seed = Client::open(config(&seed_root, true)).expect("seed");
    seed.ensure(source.to_str().expect("path")).expect("ensure");
    wait(|| seed.check(&id).expect("status").ready);
    let seed_port = seed.listen_port().to_string();
    let client = Client::open(config(&root.join("client"), false)).expect("client");
    let client_port = client.listen_port().to_string();
    let magnet = format!(
        "magnet:?xt=urn:btih:{id}&x.pe=127.0.0.1:{}&tr=http%3A%2F%2F127.0.0.1%3A{}%2Fannounce",
        seed.listen_port(),
        address.port()
    );
    client.ensure(&magnet).expect("ensure");
    wait(|| client.check(&id).expect("status").ready);
    wait(|| {
        events.lock().expect("events").iter().any(|e| {
            e.get("port") == Some(&client_port)
                && e.get("event").map(String::as_str) == Some("stopped")
        })
    });
    let captured = events.lock().expect("events").clone();
    let client_events: Vec<_> = captured
        .iter()
        .filter(|e| e.get("port") == Some(&client_port))
        .collect();
    assert_eq!(client_events.len(), 3);
    assert_eq!(
        client_events
            .iter()
            .filter_map(|e| e.get("event").map(String::as_str))
            .collect::<Vec<_>>(),
        ["started", "completed", "stopped"]
    );
    let completed = client_events[1];
    assert_eq!(completed.get("left").map(String::as_str), Some("0"));
    assert_eq!(completed.get("downloaded"), Some(&data.len().to_string()));
    assert_eq!(completed.get("uploaded").map(String::as_str), Some("0"));
    assert_eq!(
        client.transfer_stats(&id).expect("stats"),
        (data.len() as u64, 0)
    );
    assert_eq!(
        seed.transfer_stats(&id).expect("seedstats"),
        (0, data.len() as u64)
    );
    assert!(
        client_events
            .iter()
            .all(|e| e.get("key") == client_events[0].get("key"))
    );
    thread::sleep(Duration::from_millis(250));
    let captured = events.lock().expect("events").clone();
    assert_eq!(
        captured
            .iter()
            .filter(|e| e.get("port") == Some(&seed_port)
                && e.get("event").map(String::as_str) == Some("started"))
            .count(),
        1
    );
    assert!(
        !captured
            .iter()
            .any(|e| e.get("port") == Some(&seed_port) && !e.contains_key("event")),
        "Tracker interval must prevent an early periodic announcement"
    );
    seed.cancel(&id).expect("pause");
    wait(|| {
        events.lock().expect("events").iter().any(|e| {
            e.get("port") == Some(&seed_port)
                && e.get("event").map(String::as_str) == Some("stopped")
        })
    });
    let stopped = events
        .lock()
        .expect("events")
        .iter()
        .find(|e| {
            e.get("port") == Some(&seed_port)
                && e.get("event").map(String::as_str) == Some("stopped")
        })
        .cloned()
        .expect("stopped");
    assert_eq!(stopped.get("uploaded"), Some(&data.len().to_string()));
    seed.resume(&id).expect("resume");
    wait(|| {
        events
            .lock()
            .expect("events")
            .iter()
            .filter(|e| {
                e.get("port") == Some(&seed_port)
                    && e.get("event").map(String::as_str) == Some("started")
            })
            .count()
            == 2
    });
    drop(client);
    drop(seed);
    stop.store(true, Ordering::Release);
    worker.join().expect("tracker");
    fs::remove_dir_all(root).expect("cleanup");
}
