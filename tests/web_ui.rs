//! The embedded web interface and its WebSocket transport, over loopback only.
mod api_support;
mod series_support;

use api_support::{Server, TOKEN};
use mynou::{config, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    time::Duration,
};

fn server(name: &str) -> (Server, PathBuf) {
    let root = std::env::temp_dir().join(format!("mynou-web-ui-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for dir in ["movies", "series"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let mut value = config::default_json();
    value.insert("listen", "127.0.0.1:0");
    value.insert("store_dir", root.join("store").to_str().unwrap().to_owned());
    let library = value.get_mut("library").unwrap();
    library.insert(
        "movies_root",
        root.join("movies").to_str().unwrap().to_owned(),
    );
    library.insert(
        "series_root",
        root.join("series").to_str().unwrap().to_owned(),
    );
    value.get_mut("downloads").unwrap().insert("enabled", false);
    let cfg = config::from_json(&value, &root).unwrap();
    (Server::open(cfg), root)
}

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

/// Sends a handshake and returns the stream with the response head.
fn handshake(server: &Server, origin: Option<&str>, protocols: &str) -> (TcpStream, String) {
    let mut stream = TcpStream::connect(&server.authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    let mut request = format!(
        "GET /ws HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
        server.authority
    );
    if let Some(origin) = origin {
        request.push_str(&format!("Origin: {origin}\r\n"));
    }
    if !protocols.is_empty() {
        request.push_str(&format!("Sec-WebSocket-Protocol: {protocols}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    (stream, String::from_utf8(head).unwrap())
}

fn open(server: &Server) -> TcpStream {
    let origin = server.origin();
    let (stream, head) = handshake(
        server,
        Some(&origin),
        &format!("mynou.v1, auth.{}", hex(TOKEN)),
    );
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
    assert!(head.contains("Sec-WebSocket-Protocol: mynou.v1"));
    assert!(!head.contains(TOKEN));
    stream
}

fn open_when_free(server: &Server) -> TcpStream {
    let origin = server.origin();
    let protocols = format!("mynou.v1, auth.{}", hex(TOKEN));
    for _ in 0..200 {
        let (stream, head) = handshake(server, Some(&origin), &protocols);
        if head.starts_with("HTTP/1.1 101") {
            return stream;
        }
        assert!(head.starts_with("HTTP/1.1 503"), "{head}");
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("a socket slot never became free");
}

fn send(stream: &mut TcpStream, text: &str) {
    let mask = [9u8, 8, 7, 6];
    let mut frame = vec![0x81];
    match text.len() {
        n if n < 126 => frame.push(0x80 | n as u8),
        n => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    frame.extend(text.bytes().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    stream.write_all(&frame).unwrap();
}

/// Reads one unmasked server frame, returning its opcode and payload.
fn receive(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).unwrap();
    assert_eq!(head[1] & 0x80, 0, "server frames are never masked");
    let length = match head[1] & 0x7f {
        126 => {
            let mut b = [0u8; 2];
            stream.read_exact(&mut b).unwrap();
            usize::from(u16::from_be_bytes(b))
        }
        127 => {
            let mut b = [0u8; 8];
            stream.read_exact(&mut b).unwrap();
            u64::from_be_bytes(b) as usize
        }
        n => usize::from(n),
    };
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).unwrap();
    (head[0] & 0x0f, payload)
}

fn next_json(stream: &mut TcpStream) -> json::Value {
    loop {
        let (opcode, payload) = receive(stream);
        if opcode == 1 {
            return json::parse(&String::from_utf8(payload).unwrap()).unwrap();
        }
    }
}

#[test]
fn assets_are_public_and_locked_down_while_the_api_stays_private() {
    let (server, _root) = server("assets");
    for (path, kind) in [
        ("/", "text/html"),
        ("/app.js", "text/javascript"),
        ("/app.css", "text/css"),
        ("/favicon.svg", "image/svg+xml"),
    ] {
        let reply = server.call("GET", path, &[], "");
        assert_eq!(reply.status, 200, "{path}");
        assert!(reply.headers["content-type"].starts_with(kind));
        assert!(reply.headers["content-security-policy"].contains("default-src 'none'"));
        assert!(reply.headers["content-security-policy"].contains("script-src 'self'"));
        assert_eq!(reply.headers["x-content-type-options"], "nosniff");
        reply.no_secrets();
    }
    assert_eq!(server.call("GET", "/api/status", &[], "").status, 401);
    assert_eq!(server.call("GET", "/ui", &[], "").status, 401);
    assert_eq!(server.call("GET", "/healthz", &[], "").status, 200);
}

/// Sockets share one process-wide limit, so every socket scenario runs in
/// this one test instead of racing the bound check in parallel tests.
#[test]
fn websocket_scenarios() {
    let (server, _root) = server("sockets");
    handshake_requires_token_and_same_origin(&server);
    calls_and_subscriptions_use_the_existing_routes(&server);
    the_number_of_open_sockets_is_bounded(&server);
    series_beyond_two_hundred_reach_the_socket_in_pages();
}

fn handshake_requires_token_and_same_origin(server: &Server) {
    let origin = server.origin();
    let good = format!("mynou.v1, auth.{}", hex(TOKEN));
    for (origin, protocols, status) in [
        (Some(origin.as_str()), String::new(), "401"),
        (Some(origin.as_str()), "mynou.v1".to_owned(), "401"),
        (
            Some(origin.as_str()),
            format!(
                "mynou.v1, auth.{}",
                hex("wrong-token-wrong-token-wrong-token-0")
            ),
            "401",
        ),
        (Some(origin.as_str()), format!("auth.{}", hex(TOKEN)), "401"),
        (Some("http://evil.example"), good.clone(), "403"),
        (None, good.clone(), "403"),
    ] {
        let (_, head) = handshake(server, origin, &protocols);
        assert!(head.starts_with(&format!("HTTP/1.1 {status}")), "{head}");
        assert!(!head.contains("Upgrade: websocket"));
    }
}

fn calls_and_subscriptions_use_the_existing_routes(server: &Server) {
    let mut socket = open(server);

    send(&mut socket, r#"{"id":1,"method":"GET","path":"/api/jobs"}"#);
    let reply = next_json(&mut socket);
    assert_eq!(reply.get("id").and_then(json::Value::as_u64), Some(1));
    assert_eq!(reply.get("status").and_then(json::Value::as_u64), Some(200));
    assert_eq!(
        reply
            .get("body")
            .and_then(|body| body.get("jobs"))
            .and_then(json::Value::as_array)
            .map(<[_]>::len),
        Some(0)
    );

    for (id, call) in [
        (
            2,
            r#"{"id":2,"method":"POST","path":"/api/shutdown","body":{}}"#,
        ),
        (3, r#"{"id":3,"method":"GET","path":"/healthz"}"#),
        (
            6,
            r#"{"id":6,"method":"POST","path":"/api/shutdown?x=1","body":{}}"#,
        ),
        (4, r#"{"id":4,"method":"DELETE","path":"/api/jobs"}"#),
        (
            5,
            r#"{"id":5,"method":"GET","path":"/api/jobs","body":{"x":1}}"#,
        ),
    ] {
        send(&mut socket, call);
        let reply = next_json(&mut socket);
        assert_eq!(reply.get("id").and_then(json::Value::as_u64), Some(id));
        assert_eq!(
            reply.get("status").and_then(json::Value::as_u64),
            Some(400),
            "{call}"
        );
    }
    assert!(
        !server
            .engine
            .stopped
            .load(std::sync::atomic::Ordering::Acquire)
    );

    send(&mut socket, r#"{"subscribe":["status","nonsense"]}"#);
    assert_eq!(
        next_json(&mut socket)
            .get("status")
            .and_then(json::Value::as_u64),
        Some(400)
    );
    send(&mut socket, r#"{"subscribe":["status","jobs"]}"#);
    let mut topics = Vec::new();
    while topics.len() < 2 {
        let push = next_json(&mut socket);
        topics.push(
            push.get("topic")
                .and_then(json::Value::as_str)
                .unwrap()
                .to_owned(),
        );
        assert_eq!(push.get("status").and_then(json::Value::as_u64), Some(200));
    }
    topics.sort();
    assert_eq!(topics, ["jobs", "status"]);

    // An unmasked client frame is a protocol violation: the server closes.
    socket.write_all(&[0x81, 0x01, b'x']).unwrap();
    loop {
        let (opcode, _) = receive(&mut socket);
        if opcode == 8 {
            break;
        }
    }
}

fn the_number_of_open_sockets_is_bounded(server: &Server) {
    // Slots of earlier sockets are released asynchronously by the server.
    let held: Vec<_> = (0..8).map(|_| open_when_free(server)).collect();
    let origin = server.origin();
    let (_, head) = handshake(
        server,
        Some(&origin),
        &format!("mynou.v1, auth.{}", hex(TOKEN)),
    );
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    drop(held);
}

/// 201 migrated series (more than one page of any size, and more than the old
/// 128-scope snapshot allowed) reach the interface through bounded pages.
fn series_beyond_two_hundred_reach_the_socket_in_pages() {
    use series_support::{legacy_snapshot, record_id, synthetic_record};
    let root = std::env::temp_dir().join(format!("mynou-web-ui-pages-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let store = root.join("store");
    fs::create_dir_all(&root).unwrap();
    let records: Vec<_> = (1..=201u64)
        .map(|n| {
            synthetic_record(
                &record_id(n * 7919),
                5000 + n,
                &format!("Synthetic Series {n}"),
                2020,
                n * 1000 + 1,
                0,
                false,
            )
        })
        .collect();
    let mut value = config::default_json();
    value.insert("listen", "127.0.0.1:0");
    value.insert("store_dir", store.to_str().unwrap().to_owned());
    value.get_mut("downloads").unwrap().insert("enabled", false);
    let cfg = config::from_json(&value, &root).unwrap();
    // A store written by 0.23: create it, then leave only the old snapshot.
    drop(mynou::engine::Engine::open_for_management(cfg.clone()).unwrap());
    series_support::restore_legacy(&store, &legacy_snapshot(&records, 2));
    let server = Server::open(cfg);
    let mut socket = open_when_free(&server);

    // The live topic carries one bounded page and says how many exist.
    send(&mut socket, r#"{"subscribe":["series"]}"#);
    let push = next_json(&mut socket);
    assert_eq!(
        push.get("topic").and_then(json::Value::as_str),
        Some("series")
    );
    assert_eq!(push.get("status").and_then(json::Value::as_u64), Some(200));
    let first = push.get("data").unwrap();
    assert_eq!(first.get("total").and_then(json::Value::as_u64), Some(201));
    assert_eq!(
        first
            .get("series")
            .and_then(json::Value::as_array)
            .unwrap()
            .len(),
        50
    );
    assert!(first.get("next").and_then(json::Value::as_str).is_some());

    // The bare route has the same shape, and the cursor reaches every series.
    let mut seen = std::collections::BTreeSet::new();
    let mut after: Option<String> = None;
    let mut id = 10;
    loop {
        let path = match &after {
            Some(after) => format!("/api/series?limit=200&after={after}"),
            None => "/api/series".to_owned(),
        };
        id += 1;
        send(
            &mut socket,
            &format!(r#"{{"id":{id},"method":"GET","path":"{path}"}}"#),
        );
        let reply = next_json(&mut socket);
        if reply.get("topic").is_some() {
            id -= 1;
            continue;
        }
        assert_eq!(reply.get("status").and_then(json::Value::as_u64), Some(200));
        let body = reply.get("body").unwrap();
        assert_eq!(body.get("total").and_then(json::Value::as_u64), Some(201));
        for item in body.get("series").and_then(json::Value::as_array).unwrap() {
            assert!(
                seen.insert(
                    item.get("id")
                        .and_then(json::Value::as_str)
                        .unwrap()
                        .to_owned()
                )
            );
        }
        after = body
            .get("next")
            .and_then(json::Value::as_str)
            .map(str::to_owned);
        if after.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 201);
    drop(socket);
    drop(server);
    let _ = fs::remove_dir_all(root);
}
