use crate::{Result, bencode::Value};
use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub fn decode_peers(data: &[u8], ipv6: bool) -> Result<Vec<SocketAddr>> {
    let width = if ipv6 { 18 } else { 6 };
    if !data.len().is_multiple_of(width) || data.len() / width > 10_000 {
        return Err("Liste de pairs compacte invalide".into());
    }
    let mut out = Vec::new();
    for p in data.chunks_exact(width) {
        let ip = if ipv6 {
            let mut a = [0; 16];
            a.copy_from_slice(&p[..16]);
            IpAddr::V6(Ipv6Addr::from(a))
        } else {
            IpAddr::V4(Ipv4Addr::new(p[0], p[1], p[2], p[3]))
        };
        let port = u16::from_be_bytes([p[width - 2], p[width - 1]]);
        if port != 0 && !ip.is_unspecified() && !ip.is_multicast() {
            out.push(SocketAddr::new(ip, port));
        }
    }
    Ok(out)
}
fn dict(items: &[(&[u8], Value)]) -> Value {
    Value::Dict(items.iter().map(|(k, v)| (k.to_vec(), v.clone())).collect())
}
fn value_bytes(v: Option<&Value>) -> Option<&[u8]> {
    match v {
        Some(Value::Bytes(b)) => Some(b),
        _ => None,
    }
}
fn field<'a>(v: &'a Value, k: &[u8]) -> Option<&'a Value> {
    match v {
        Value::Dict(d) => d.get(k),
        _ => None,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackerEvent {
    None,
    Started,
    Completed,
    Stopped,
}

impl TrackerEvent {
    fn query_name(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Started => Some("started"),
            Self::Completed => Some("completed"),
            Self::Stopped => Some("stopped"),
        }
    }
    fn udp_code(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Completed => 1,
            Self::Started => 2,
            Self::Stopped => 3,
        }
    }
}

pub struct TrackerRequest<'a> {
    pub hash: &'a [u8; 20],
    pub peer_id: &'a [u8; 20],
    pub port: u16,
    pub left: u64,
    pub uploaded: u64,
    pub downloaded: u64,
    pub event: TrackerEvent,
}

#[derive(Clone, Debug)]
pub struct TrackerReply {
    pub peers: Vec<SocketAddr>,
    pub interval: Duration,
}

pub const DEFAULT_TRACKER_INTERVAL: Duration = Duration::from_secs(1800);

fn bounded_interval(seconds: u64) -> Duration {
    Duration::from_secs(seconds.clamp(30, 86_400))
}

fn session_key(request: &TrackerRequest<'_>) -> u32 {
    // The random peer ID is stable during one Client lifetime; combine it with
    // the swarm identity so HTTP and UDP announcements retain the same key.
    let mut material = [0u8; 40];
    material[..20].copy_from_slice(request.peer_id);
    material[20..].copy_from_slice(request.hash);
    let digest = crate::crypto::sha256(&material);
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
}

pub fn query_url(url: &str, request: &TrackerRequest<'_>) -> String {
    fn escape(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 3);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
        out
    }
    let url = url.split('#').next().unwrap_or(url);
    let separator = if url.ends_with(['?', '&']) {
        ""
    } else if url.contains('?') {
        "&"
    } else {
        "?"
    };
    let mut query = format!(
        "{url}{separator}info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant=100",
        escape(request.hash),
        escape(request.peer_id),
        request.port,
        request.uploaded,
        request.downloaded,
        request.left,
    );
    use std::fmt::Write;
    let _ = write!(query, "&key={:08X}", session_key(request));
    if let Some(event) = request.event.query_name() {
        query.push_str("&event=");
        query.push_str(event);
    }
    query
}

pub fn tracker(url: &str, request: &TrackerRequest<'_>) -> Result<TrackerReply> {
    if url.len() > 8192 || url.chars().any(char::is_control) {
        return Err("URL de tracker excessive ou caractères de contrôle".into());
    }
    if request.port == 0 {
        return Err("Annonce tracker : port nul interdit".into());
    }
    if url.starts_with("udp://") {
        return udp_tracker(url, request);
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("Protocole de tracker non pris en charge".into());
    }
    let response = crate::net::HttpClient::new()
        .with_timeout(Duration::from_secs(5))
        .with_max_body(1024 * 1024)
        .get(&query_url(url, request))?;
    if response.status != 200 {
        return Err("Le tracker a refusé la requête".into());
    }
    let value = crate::bencode::parse(&response.body)?;
    parse_tracker_reply(&value)
}

fn parse_tracker_reply(value: &Value) -> Result<TrackerReply> {
    if !matches!(value, Value::Dict(_)) {
        return Err("Réponse de tracker : dictionnaire attendu".into());
    }
    if field(value, b"failure reason").is_some() {
        return Err("Le tracker a refusé l'annonce".into());
    }
    let seconds = |key: &[u8]| -> Result<Option<u64>> {
        match field(value, key) {
            Some(Value::Int(n)) => u64::try_from(*n)
                .map(Some)
                .map_err(|_| "Intervalle de tracker négatif".into()),
            Some(_) => Err("Intervalle de tracker invalide".into()),
            None => Ok(None),
        }
    };
    let interval = seconds(b"interval")?.unwrap_or(DEFAULT_TRACKER_INTERVAL.as_secs());
    let interval = bounded_interval(interval.max(seconds(b"min interval")?.unwrap_or(0)));
    let mut peers = Vec::new();
    if let Some(p) = value_bytes(field(value, b"peers")) {
        peers.extend(decode_peers(p, false)?);
    } else if let Some(Value::List(list)) = field(value, b"peers") {
        if list.len() > 10_000 {
            return Err("Trop de pairs du tracker".into());
        }
        for p in list {
            let ip = value_bytes(field(p, b"ip"))
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|v| v.parse::<IpAddr>().ok());
            let port = match field(p, b"port") {
                Some(Value::Int(n)) => u16::try_from(*n).ok(),
                _ => None,
            };
            if let (Some(ip), Some(port)) = (ip, port)
                && port != 0
                && !ip.is_unspecified()
                && !ip.is_multicast()
            {
                peers.push(SocketAddr::new(ip, port));
            }
        }
    } else if field(value, b"peers").is_some() {
        return Err("Liste de pairs du tracker invalide".into());
    }
    if let Some(p) = value_bytes(field(value, b"peers6")) {
        peers.extend(decode_peers(p, true)?);
    } else if field(value, b"peers6").is_some() {
        return Err("Liste de pairs IPv6 du tracker invalide".into());
    }
    peers.sort_unstable();
    peers.dedup();
    Ok(TrackerReply { peers, interval })
}

fn nonce() -> Result<u32> {
    Ok(u32::from_be_bytes(crate::crypto::random_bytes::<4>()?))
}
fn udp_exchange(socket: &UdpSocket, packet: &[u8], action: u32, tx: u32) -> Result<Vec<u8>> {
    let mut buf = [0; 65_507];
    for _ in 0..2 {
        socket
            .send(packet)
            .map_err(|_| "Envoi UDP au tracker impossible")?;
        match socket.recv(&mut buf) {
            Ok(n) if n >= 8 && buf[4..8] == tx.to_be_bytes() => {
                if u32::from_be_bytes(buf[..4].try_into().map_err(|_| "Réponse UDP invalide")?) == 3
                {
                    return Err("Le tracker UDP a refusé l'annonce".into());
                }
                if buf[..4] != action.to_be_bytes() {
                    return Err("Réponse UDP de tracker inattendue".into());
                }
                return Ok(buf[..n].to_vec());
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    Err("Le tracker UDP ne répond pas".into())
}
fn udp_tracker(url: &str, request: &TrackerRequest<'_>) -> Result<TrackerReply> {
    let rest = url.strip_prefix("udp://").ok_or("Tracker UDP invalide")?;
    let authority = rest.split('/').next().ok_or("Tracker UDP invalide")?;
    if authority.contains('@') {
        return Err("Adresse UDP de tracker invalide".into());
    }
    let address = authority
        .to_socket_addrs()
        .map_err(|_| "Résolution du tracker impossible")?
        .next()
        .ok_or("Tracker sans adresse")?;
    let socket = UdpSocket::bind(if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .map_err(|_| "Création UDP impossible")?;
    socket
        .connect(address)
        .map_err(|_| "Connexion UDP au tracker impossible")?;
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|_| "Délai UDP impossible")?;
    let tx = nonce()?;
    let mut connect = Vec::with_capacity(16);
    connect.extend_from_slice(&0x41727101980u64.to_be_bytes());
    connect.extend_from_slice(&0u32.to_be_bytes());
    connect.extend_from_slice(&tx.to_be_bytes());
    let response = udp_exchange(&socket, &connect, 0, tx)?;
    if response.len() != 16 {
        return Err("Réponse UDP de connexion invalide".into());
    }
    let tx = nonce()?;
    let mut announce = Vec::with_capacity(98);
    announce.extend_from_slice(&response[8..16]);
    announce.extend_from_slice(&1u32.to_be_bytes());
    announce.extend_from_slice(&tx.to_be_bytes());
    announce.extend_from_slice(request.hash);
    announce.extend_from_slice(request.peer_id);
    announce.extend_from_slice(&request.downloaded.to_be_bytes());
    announce.extend_from_slice(&request.left.to_be_bytes());
    announce.extend_from_slice(&request.uploaded.to_be_bytes());
    announce.extend_from_slice(&request.event.udp_code().to_be_bytes());
    announce.extend_from_slice(&0u32.to_be_bytes());
    announce.extend_from_slice(&session_key(request).to_be_bytes());
    announce.extend_from_slice(&100i32.to_be_bytes());
    announce.extend_from_slice(&request.port.to_be_bytes());
    let response = udp_exchange(&socket, &announce, 1, tx)?;
    if response.len() < 20 {
        return Err("Réponse UDP d'annonce tronquée".into());
    }
    let interval = u32::from_be_bytes(
        response[8..12]
            .try_into()
            .map_err(|_| "Intervalle UDP tronqué")?,
    );
    let peers = decode_peers(&response[20..], address.is_ipv6())?;
    Ok(TrackerReply {
        peers,
        interval: bounded_interval(u64::from(interval)),
    })
}

// A bounded iterative Kademlia lookup. Unknown metadata only performs get_peers;
// announcing is allowed after the caller established that the torrent is public.
pub fn dht_lookup(
    hash: &[u8; 20],
    port: u16,
    announce: bool,
    stop: &AtomicBool,
    seeds: &[SocketAddr],
) -> Result<Vec<SocketAddr>> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|_| "Création DHT impossible")?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|_| "Délai DHT impossible")?;
    let id = crate::crypto::random_bytes::<20>()?;
    let mut nodes: Vec<SocketAddr> = seeds.iter().copied().filter(SocketAddr::is_ipv4).collect();
    if seeds.is_empty() {
        for host in [
            "router.bittorrent.com:6881",
            "router.utorrent.com:6881",
            "dht.transmissionbt.com:6881",
        ] {
            if let Ok(addresses) = host.to_socket_addrs() {
                nodes.extend(addresses.filter(SocketAddr::is_ipv4).take(2));
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut peers = BTreeSet::new();
    let mut tx = nonce()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    for index in 0..64 {
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline || index >= nodes.len() {
            break;
        }
        let target = nodes[index];
        if !seen.insert(target) {
            continue;
        }
        tx = tx.wrapping_add(1);
        let transaction = tx.to_be_bytes();
        let query = dict(&[
            (b"t", Value::Bytes(transaction.to_vec())),
            (b"y", Value::Bytes(b"q".to_vec())),
            (b"q", Value::Bytes(b"get_peers".to_vec())),
            (
                b"a",
                dict(&[
                    (b"id", Value::Bytes(id.to_vec())),
                    (b"info_hash", Value::Bytes(hash.to_vec())),
                ]),
            ),
        ]);
        socket
            .send_to(&crate::bencode::encode(&query), target)
            .map_err(|_| "Envoi de requête DHT impossible")?;
        let mut buf = [0; 4096];
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if from != target {
            continue;
        }
        let response = match crate::bencode::parse(&buf[..n]) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value_bytes(field(&response, b"t")) != Some(transaction.as_slice())
            || value_bytes(field(&response, b"y")) != Some(b"r".as_slice())
        {
            continue;
        }
        let Some(r) = field(&response, b"r") else {
            continue;
        };
        if let Some(Value::List(values)) = field(r, b"values") {
            for value in values.iter().take(200) {
                if let Ok(raw) = super::metainfo::bytes(value)
                    && let Ok(found) = decode_peers(raw, false)
                {
                    peers.extend(found);
                }
            }
        }
        if let Some(raw) = value_bytes(field(r, b"nodes"))
            && raw.len() % 26 == 0
        {
            for node in raw.as_chunks::<26>().0.iter().take(32) {
                if let Ok(found) = decode_peers(&node[20..], false)
                    && nodes.len() < 128
                {
                    nodes.extend(found);
                }
            }
        }
        if announce
            && let Some(token) = value_bytes(field(r, b"token"))
            && token.len() <= 256
        {
            tx = tx.wrapping_add(1);
            let query = dict(&[
                (b"t", Value::Bytes(tx.to_be_bytes().to_vec())),
                (b"y", Value::Bytes(b"q".to_vec())),
                (b"q", Value::Bytes(b"announce_peer".to_vec())),
                (
                    b"a",
                    dict(&[
                        (b"id", Value::Bytes(id.to_vec())),
                        (b"info_hash", Value::Bytes(hash.to_vec())),
                        (b"port", Value::Int(i64::from(port))),
                        (b"token", Value::Bytes(token.to_vec())),
                    ]),
                ),
            ]);
            let _ = socket.send_to(&crate::bencode::encode(&query), target);
        }
        if peers.len() >= 100 {
            break;
        }
    }
    Ok(peers.into_iter().collect())
}

pub fn pex_peers(payload: &[u8]) -> Result<Vec<SocketAddr>> {
    let value = crate::bencode::parse(payload)?;
    let mut peers = Vec::new();
    if let Some(raw) = value_bytes(field(&value, b"added")) {
        peers.extend(decode_peers(raw, false)?);
    }
    if let Some(raw) = value_bytes(field(&value, b"added6")) {
        peers.extend(decode_peers(raw, true)?);
    }
    Ok(peers)
}

pub fn value_dict(items: &[(&[u8], Value)]) -> Value {
    dict(items)
}
pub fn value_field<'a>(v: &'a Value, key: &[u8]) -> Option<&'a Value> {
    field(v, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_peers_are_bounded() {
        assert_eq!(
            decode_peers(&[127, 0, 0, 1, 0x1a, 0xe1], false).expect("pair")[0].port(),
            6881
        );
        assert!(decode_peers(&[1], false).is_err());
        assert!(
            decode_peers(&[224, 0, 0, 1, 0, 1], false)
                .expect("multicast")
                .is_empty()
        );
    }

    #[test]
    fn real_udp_dht_lookup_and_authenticated_announce() {
        for announce in [false, true] {
            let server = UdpSocket::bind("127.0.0.1:0").expect("DHT locale");
            server
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("délai");
            let address = server.local_addr().expect("adresse");
            let hash = [19; 20];
            let worker = std::thread::spawn(move || {
                let mut packet = [0; 4096];
                let (n, peer) = server.recv_from(&mut packet).expect("get_peers");
                let query = crate::bencode::parse(&packet[..n]).expect("requête");
                assert_eq!(
                    value_bytes(field(&query, b"q")),
                    Some(b"get_peers".as_slice())
                );
                assert_eq!(
                    value_bytes(field(field(&query, b"a").expect("args"), b"info_hash")),
                    Some(hash.as_slice())
                );
                let tx = field(&query, b"t").expect("transaction").clone();
                let response = dict(&[
                    (b"t", tx),
                    (b"y", Value::Bytes(b"r".to_vec())),
                    (
                        b"r",
                        dict(&[
                            (b"id", Value::Bytes([3; 20].to_vec())),
                            (b"token", Value::Bytes(b"proof".to_vec())),
                            (
                                b"values",
                                Value::List(vec![Value::Bytes(vec![127, 0, 0, 1, 0x1a, 0xe1])]),
                            ),
                        ]),
                    ),
                ]);
                server
                    .send_to(&crate::bencode::encode(&response), peer)
                    .expect("réponse");
                if announce {
                    let (n, _) = server.recv_from(&mut packet).expect("announce_peer");
                    let query = crate::bencode::parse(&packet[..n]).expect("annonce");
                    assert_eq!(
                        value_bytes(field(&query, b"q")),
                        Some(b"announce_peer".as_slice())
                    );
                    assert_eq!(
                        value_bytes(field(field(&query, b"a").expect("args"), b"token")),
                        Some(b"proof".as_slice())
                    );
                }
            });
            let peers = dht_lookup(&hash, 6881, announce, &AtomicBool::new(false), &[address])
                .expect("découverte");
            assert_eq!(peers, vec!["127.0.0.1:6881".parse().expect("pair")]);
            worker.join().expect("serveur");
        }
    }

    #[test]
    fn tracker_intervals_are_bounded_and_minimum_is_respected() {
        let reply = |fields: &[(&[u8], Value)]| parse_tracker_reply(&dict(fields));
        assert_eq!(reply(&[]).unwrap().interval, DEFAULT_TRACKER_INTERVAL);
        assert_eq!(
            reply(&[(b"interval", Value::Int(0))]).unwrap().interval,
            Duration::from_secs(30)
        );
        assert_eq!(
            reply(&[(b"interval", Value::Int(i64::MAX))])
                .unwrap()
                .interval,
            Duration::from_secs(86_400)
        );
        assert_eq!(
            reply(&[
                (b"interval", Value::Int(30)),
                (b"min interval", Value::Int(90))
            ])
            .unwrap()
            .interval,
            Duration::from_secs(90)
        );
        assert!(reply(&[(b"interval", Value::Int(-1))]).is_err());
        assert!(reply(&[(b"interval", Value::Bytes(b"30".to_vec()))]).is_err());
        assert!(reply(&[(b"min interval", Value::Int(-1))]).is_err());
        assert!(reply(&[(b"failure reason", Value::Bytes(b"private refusal".to_vec()))]).is_err());
        assert!(parse_tracker_reply(&Value::Int(1)).is_err());
        assert!(reply(&[(b"peers", Value::Bytes(vec![0; 6 * 10_001]))]).is_err());
    }

    #[test]
    fn real_http_tracker_events_counters_and_interval() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        server.set_nonblocking(true).unwrap();
        let address = server.local_addr().unwrap();
        let events = [
            (TrackerEvent::None, None),
            (TrackerEvent::Started, Some("started")),
            (TrackerEvent::Completed, Some("completed")),
            (TrackerEvent::Stopped, Some("stopped")),
        ];
        let worker = std::thread::spawn(move || {
            let mut first_key = None;
            for (_, expected) in events {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match server.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(1))
                        }
                        Err(error) => panic!("Tracker local absent : {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let query = line.split_whitespace().nth(1).unwrap();
                assert!(query.starts_with("/announce?passkey=fixture&info_hash="));
                assert!(query.contains("&downloaded=12345&left=54321"));
                assert!(query.contains("&uploaded=9876&"));
                assert!(query.contains("&port=6882&"));
                match expected {
                    Some(event) => assert!(query.ends_with(&format!("&event={event}"))),
                    None => assert!(!query.contains("event=")),
                }
                assert!(!query.contains("#ignored"));
                let key = query
                    .split('&')
                    .find_map(|part| part.strip_prefix("key="))
                    .unwrap()
                    .to_owned();
                assert_eq!(key.len(), 8);
                if let Some(previous) = &first_key {
                    assert_eq!(previous, &key);
                }
                first_key = Some(key);
                for _ in 0..32 {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                let body = crate::bencode::encode(&dict(&[
                    (b"interval", Value::Int(5)),
                    (b"min interval", Value::Int(90)),
                    (b"peers", Value::Bytes(vec![127, 0, 0, 1, 0x1a, 0xe1])),
                ]));
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                reader.get_mut().write_all(&body).unwrap();
            }
        });
        for (event, _) in events {
            let reply = tracker(
                &format!("http://{address}/announce?passkey=fixture#ignored"),
                &TrackerRequest {
                    hash: &[7; 20],
                    peer_id: &[9; 20],
                    port: 6882,
                    left: 54321,
                    uploaded: 9876,
                    downloaded: 12345,
                    event,
                },
            )
            .unwrap();
            assert_eq!(reply.interval, Duration::from_secs(90));
            assert_eq!(reply.peers, vec!["127.0.0.1:6881".parse().unwrap()]);
        }
        worker.join().unwrap();
    }

    #[test]
    fn real_udp_tracker_all_event_codes_and_counters() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let address = server.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let mut first_key = None;
            for code in [0u32, 1, 2, 3] {
                let mut buf = [0u8; 1024];
                let (n, peer) = server.recv_from(&mut buf).unwrap();
                assert_eq!(n, 16);
                let mut reply = vec![0; 4];
                reply.extend_from_slice(&buf[12..16]);
                reply.extend_from_slice(&99u64.to_be_bytes());
                server.send_to(&reply, peer).unwrap();
                let (n, peer) = server.recv_from(&mut buf).unwrap();
                assert_eq!(n, 98);
                assert_eq!(&buf[56..64], &12345u64.to_be_bytes());
                assert_eq!(&buf[64..72], &54321u64.to_be_bytes());
                assert_eq!(&buf[72..80], &9876u64.to_be_bytes());
                assert_eq!(&buf[80..84], &code.to_be_bytes());
                let key: [u8; 4] = buf[88..92].try_into().unwrap();
                if let Some(previous) = first_key {
                    assert_eq!(previous, key);
                }
                first_key = Some(key);
                let mut reply = 1u32.to_be_bytes().to_vec();
                reply.extend_from_slice(&buf[12..16]);
                reply.extend_from_slice(&5u32.to_be_bytes());
                reply.extend_from_slice(&[0; 8]);
                server.send_to(&reply, peer).unwrap();
            }
        });
        for event in [
            TrackerEvent::None,
            TrackerEvent::Completed,
            TrackerEvent::Started,
            TrackerEvent::Stopped,
        ] {
            let reply = tracker(
                &format!("udp://{address}/announce"),
                &TrackerRequest {
                    hash: &[7; 20],
                    peer_id: &[9; 20],
                    port: 6882,
                    left: 54321,
                    uploaded: 9876,
                    downloaded: 12345,
                    event,
                },
            )
            .unwrap();
            assert_eq!(reply.interval, Duration::from_secs(30));
            assert!(reply.peers.is_empty());
        }
        worker.join().unwrap();
    }

    #[test]
    fn real_udp_tracker_connect_and_announce() {
        let server = UdpSocket::bind("127.0.0.1:0").expect("tracker");
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("délai");
        let address = server.local_addr().expect("adresse");
        let hash = [7; 20];
        let id = [9; 20];
        let worker = std::thread::spawn(move || {
            let mut buf = [0; 1024];
            let (n, peer) = server.recv_from(&mut buf).expect("connect");
            assert_eq!(n, 16);
            let mut reply = 0u32.to_be_bytes().to_vec();
            reply.extend_from_slice(&buf[12..16]);
            reply.extend_from_slice(&17u64.to_be_bytes());
            server.send_to(&reply, peer).expect("reply");
            let (n, peer) = server.recv_from(&mut buf).expect("announce");
            assert_eq!(n, 98);
            assert_eq!(&buf[16..36], &hash);
            assert_eq!(&buf[36..56], &id);
            assert_eq!(&buf[56..64], &17u64.to_be_bytes());
            assert_eq!(&buf[64..72], &23u64.to_be_bytes());
            assert_eq!(&buf[72..80], &19u64.to_be_bytes());
            assert_eq!(&buf[80..84], &2u32.to_be_bytes());
            let mut reply = 1u32.to_be_bytes().to_vec();
            reply.extend_from_slice(&buf[12..16]);
            reply.extend_from_slice(&30u32.to_be_bytes());
            reply.extend_from_slice(&1u32.to_be_bytes());
            reply.extend_from_slice(&1u32.to_be_bytes());
            reply.extend_from_slice(&[127, 0, 0, 1, 0x1a, 0xe1]);
            server.send_to(&reply, peer).expect("reply");
        });
        assert_eq!(
            tracker(
                &format!("udp://{address}/announce"),
                &TrackerRequest {
                    hash: &hash,
                    peer_id: &id,
                    port: 6882,
                    left: 23,
                    downloaded: 17,
                    uploaded: 19,
                    event: TrackerEvent::Started
                }
            )
            .expect("pairs")
            .peers,
            vec!["127.0.0.1:6881".parse().expect("pair")]
        );
        worker.join().expect("worker");
    }
}
