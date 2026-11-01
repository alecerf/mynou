//! The DHT node on the BitTorrent UDP port (BEP 5), which it shares with uTP
//! (see `utp`, whose hub thread feeds it). Lookups use their own
//! short-lived sockets and say they are read-only (BEP 43); this node is the
//! part of Mynou other nodes keep in their routing tables. It answers `ping`,
//! `find_node`, `get_peers` and `announce_peer`, keeps the peers announced to
//! it for 30 minutes, and refreshes its routing table from the nodes that
//! answer it. Everything it holds is bounded, and it answers at most
//! `REPLIES_PER_SECOND` queries a second, whatever their source.
use super::{
    discovery::{decode_peers, value_dict, value_field},
    transport::Network,
};
use crate::{Result, bencode::Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Nodes per routing-table bucket, and nodes in a `nodes` reply.
const K: usize = 8;
/// A node silent for this long may be replaced by a new one in its bucket.
const NODE_STALE: Duration = Duration::from_secs(15 * 60);
/// Announced peers expire after this (as in other clients).
const PEER_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_TORRENTS: usize = 512;
const MAX_PEERS_PER_TORRENT: usize = 100;
/// Peers in one `values` reply: 50 compact peers fit any path MTU.
const MAX_VALUES: usize = 50;
const REPLIES_PER_SECOND: u32 = 100;
/// Tokens are valid for one rotation and the next (BEP 5: about 10 minutes).
const SECRET_EVERY: Duration = Duration::from_secs(5 * 60);
/// The table is refreshed this often, and sooner while it holds few nodes.
const REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);
/// Bootstrap routers answer with a single node, often gone: while the table
/// holds fewer than K nodes, they are asked again this often.
const REFRESH_SPARSE: Duration = Duration::from_secs(3);
/// Queries one refresh may send, and how long their answers are awaited.
const REFRESH_QUERIES: usize = 32;
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// The node only answers at first: a client opened and closed at once, or
/// with nothing to download yet, sends nothing and resolves no router.
pub(crate) const FIRST_REFRESH: Duration = Duration::from_secs(15);
const ROUTERS: [&str; 3] = [
    "router.bittorrent.com:6881",
    "router.utorrent.com:6881",
    "dht.transmissionbt.com:6881",
];

fn distance(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    std::array::from_fn(|i| a[i] ^ b[i])
}

/// Bucket of `other` in the table of `own`: the length of their common prefix.
fn bucket(own: &[u8; 20], other: &[u8; 20]) -> usize {
    let d = distance(own, other);
    d.iter()
        .position(|byte| *byte != 0)
        .map_or(160, |i| i * 8 + d[i].leading_zeros() as usize)
}

/// Only nodes on a public IPv4 address and a real port enter the table: the
/// socket is IPv4 and a private address names no reachable node.
fn routable(address: SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(ip) => {
            address.port() != 0
                && !(ip.is_unspecified()
                    || ip.is_loopback()
                    || ip.is_private()
                    || ip.is_link_local()
                    || ip.is_broadcast()
                    || ip.is_multicast()
                    || ip.is_documentation())
        }
        IpAddr::V6(_) => false,
    }
}

fn compact(address: SocketAddr) -> Option<[u8; 6]> {
    let IpAddr::V4(ip) = address.ip() else {
        return None;
    };
    let mut out = [0; 6];
    out[..4].copy_from_slice(&ip.octets());
    out[4..].copy_from_slice(&address.port().to_be_bytes());
    Some(out)
}

fn id_of(value: Option<&Value>) -> Option<[u8; 20]> {
    value?.as_bytes()?.try_into().ok()
}

pub(crate) struct Node {
    id: [u8; 20],
    /// Nodes that answered or queried this one, by address.
    table: BTreeMap<SocketAddr, ([u8; 20], Instant)>,
    peers: BTreeMap<[u8; 20], BTreeMap<SocketAddr, Instant>>,
    secret: [u8; 16],
    previous: [u8; 16],
    rotated: Instant,
    /// Replies left in the current second.
    budget: (Instant, u32),
    /// This node's own queries awaiting an answer, by transaction.
    pending: BTreeMap<[u8; 2], (SocketAddr, Instant)>,
    /// Nodes named in answers, queried next by a refresh.
    heard: BTreeSet<SocketAddr>,
    transaction: u16,
    refreshed: Option<Instant>,
}

impl Node {
    pub(crate) fn new() -> Result<Self> {
        let secret = crate::crypto::random_bytes::<16>()?;
        Ok(Self {
            id: crate::crypto::random_bytes::<20>()?,
            table: BTreeMap::new(),
            peers: BTreeMap::new(),
            secret,
            previous: secret,
            rotated: Instant::now(),
            budget: (Instant::now(), REPLIES_PER_SECOND),
            pending: BTreeMap::new(),
            heard: BTreeSet::new(),
            transaction: u16::from_be_bytes(crate::crypto::random_bytes::<2>()?),
            refreshed: None,
        })
    }

    fn token(secret: &[u8; 16], ip: IpAddr) -> [u8; 8] {
        let mut hash = crate::crypto::Sha256::new();
        hash.update(secret);
        match ip {
            IpAddr::V4(ip) => hash.update(&ip.octets()),
            IpAddr::V6(ip) => hash.update(&ip.octets()),
        }
        let digest = hash.finalize();
        std::array::from_fn(|i| digest[i])
    }

    fn rotate(&mut self, now: Instant) -> Result<()> {
        if now.duration_since(self.rotated) >= SECRET_EVERY {
            self.previous = self.secret;
            self.secret = crate::crypto::random_bytes::<16>()?;
            self.rotated = now;
        }
        Ok(())
    }

    fn valid_token(&self, token: &[u8], ip: IpAddr) -> bool {
        token == Self::token(&self.secret, ip) || token == Self::token(&self.previous, ip)
    }

    /// Adds or refreshes a node. A full bucket takes a new node only in place
    /// of one silent for `NODE_STALE`.
    fn learn(&mut self, id: [u8; 20], address: SocketAddr, now: Instant) {
        if !routable(address) || id == self.id {
            return;
        }
        if let Some(entry) = self.table.get_mut(&address) {
            *entry = (id, now);
            return;
        }
        let index = bucket(&self.id, &id);
        let members: Vec<_> = self
            .table
            .iter()
            .filter(|(_, (other, _))| bucket(&self.id, other) == index)
            .map(|(address, (_, seen))| (*seen, *address))
            .collect();
        if members.len() >= K {
            match members.iter().min() {
                Some((seen, stale)) if now.duration_since(*seen) >= NODE_STALE => {
                    self.table.remove(stale);
                }
                _ => return,
            }
        }
        self.table.insert(address, (id, now));
    }

    fn closest(&self, target: &[u8; 20]) -> Vec<u8> {
        let mut nodes: Vec<_> = self
            .table
            .iter()
            .map(|(address, (id, _))| (distance(id, target), *id, *address))
            .collect();
        nodes.sort_unstable();
        let mut out = Vec::with_capacity(K * 26);
        for (_, id, address) in nodes.into_iter().take(K) {
            if let Some(compact) = compact(address) {
                out.extend_from_slice(&id);
                out.extend_from_slice(&compact);
            }
        }
        out
    }

    fn store(&mut self, hash: [u8; 20], peer: SocketAddr, now: Instant) {
        if !self.peers.contains_key(&hash) && self.peers.len() >= MAX_TORRENTS {
            self.expire(now);
            if self.peers.len() >= MAX_TORRENTS {
                return;
            }
        }
        let peers = self.peers.entry(hash).or_default();
        if !peers.contains_key(&peer) && peers.len() >= MAX_PEERS_PER_TORRENT {
            let Some(oldest) = peers.iter().min_by_key(|(_, at)| **at).map(|(p, _)| *p) else {
                return;
            };
            peers.remove(&oldest);
        }
        peers.insert(peer, now);
    }

    fn expire(&mut self, now: Instant) {
        self.peers.retain(|_, peers| {
            peers.retain(|_, at| now.duration_since(*at) < PEER_TTL);
            !peers.is_empty()
        });
    }

    fn take_budget(&mut self, now: Instant) -> bool {
        if now.duration_since(self.budget.0) >= Duration::from_secs(1) {
            self.budget = (now, REPLIES_PER_SECOND);
        }
        let left = self.budget.1 > 0;
        self.budget.1 = self.budget.1.saturating_sub(1);
        left
    }

    fn error(transaction: &[u8], code: i64, text: &str) -> Vec<u8> {
        crate::bencode::encode(&value_dict(&[
            (b"t", Value::Bytes(transaction.to_vec())),
            (b"y", Value::Bytes(b"e".to_vec())),
            (
                b"e",
                Value::List(vec![
                    Value::Int(code),
                    Value::Bytes(text.as_bytes().to_vec()),
                ]),
            ),
        ]))
    }

    /// The reply to one datagram, if any; answers to this node's own queries
    /// feed its routing table instead.
    pub(crate) fn handle(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        now: Instant,
    ) -> Option<Vec<u8>> {
        let message = crate::bencode::parse(packet).ok()?;
        let transaction = value_field(&message, b"t")?.as_bytes()?;
        if transaction.len() > 32 {
            return None;
        }
        match value_field(&message, b"y")?.as_bytes()? {
            b"q" => {}
            b"r" => {
                self.answered(&message, transaction, from, now);
                return None;
            }
            _ => return None,
        }
        if !self.take_budget(now) {
            return None;
        }
        let _ = self.rotate(now);
        let Some(arguments) = value_field(&message, b"a") else {
            return Some(Self::error(transaction, 203, "Protocol Error"));
        };
        let Some(sender) = id_of(value_field(arguments, b"id")) else {
            return Some(Self::error(transaction, 203, "Protocol Error"));
        };
        let read_only = matches!(value_field(arguments, b"ro"), Some(Value::Int(1)));
        let method = value_field(&message, b"q")?.as_bytes()?;
        let mut reply = vec![(&b"id"[..], Value::Bytes(self.id.to_vec()))];
        match method {
            b"ping" => {}
            b"find_node" => {
                let Some(target) = id_of(value_field(arguments, b"target")) else {
                    return Some(Self::error(transaction, 203, "Protocol Error"));
                };
                reply.push((b"nodes", Value::Bytes(self.closest(&target))));
            }
            b"get_peers" => {
                let Some(hash) = id_of(value_field(arguments, b"info_hash")) else {
                    return Some(Self::error(transaction, 203, "Protocol Error"));
                };
                reply.push((
                    b"token",
                    Value::Bytes(Self::token(&self.secret, from.ip()).to_vec()),
                ));
                let values: Vec<Value> = self
                    .peers
                    .get(&hash)
                    .into_iter()
                    .flat_map(|peers| peers.iter())
                    .filter(|(_, at)| now.duration_since(**at) < PEER_TTL)
                    .filter_map(|(peer, _)| compact(*peer))
                    .take(MAX_VALUES)
                    .map(|peer| Value::Bytes(peer.to_vec()))
                    .collect();
                if !values.is_empty() {
                    reply.push((b"values", Value::List(values)));
                }
                reply.push((b"nodes", Value::Bytes(self.closest(&hash))));
            }
            b"announce_peer" => {
                let (Some(hash), Some(token)) = (
                    id_of(value_field(arguments, b"info_hash")),
                    value_field(arguments, b"token").and_then(Value::as_bytes),
                ) else {
                    return Some(Self::error(transaction, 203, "Protocol Error"));
                };
                if !self.valid_token(token, from.ip()) {
                    return Some(Self::error(transaction, 203, "Bad token"));
                }
                let implied =
                    matches!(value_field(arguments, b"implied_port"), Some(Value::Int(1)));
                let port = match value_field(arguments, b"port") {
                    _ if implied => from.port(),
                    Some(Value::Int(port)) => match u16::try_from(*port) {
                        Ok(port) if port != 0 => port,
                        _ => return Some(Self::error(transaction, 203, "Protocol Error")),
                    },
                    _ => return Some(Self::error(transaction, 203, "Protocol Error")),
                };
                let peer = SocketAddr::new(from.ip(), port);
                if routable(peer) {
                    self.store(hash, peer, now);
                }
            }
            _ => return Some(Self::error(transaction, 204, "Method Unknown")),
        }
        if !read_only {
            self.learn(sender, from, now);
        }
        Some(crate::bencode::encode(&value_dict(&[
            (b"t", Value::Bytes(transaction.to_vec())),
            (b"y", Value::Bytes(b"r".to_vec())),
            (b"r", value_dict(&reply)),
        ])))
    }

    /// An answer to one of this node's queries: its sender joins the table and
    /// the nodes it names are queried by the next refresh.
    fn answered(&mut self, message: &Value, transaction: &[u8], from: SocketAddr, now: Instant) {
        let Ok(transaction) = <[u8; 2]>::try_from(transaction) else {
            return;
        };
        if self
            .pending
            .get(&transaction)
            .is_none_or(|(to, _)| *to != from)
        {
            return;
        }
        self.pending.remove(&transaction);
        let Some(reply) = value_field(message, b"r") else {
            return;
        };
        if let Some(id) = id_of(value_field(reply, b"id")) {
            self.learn(id, from, now);
        }
        if let Some(raw) = value_field(reply, b"nodes").and_then(Value::as_bytes)
            && raw.len() % 26 == 0
        {
            for node in raw.as_chunks::<26>().0.iter().take(K * 2) {
                if let Ok(found) = decode_peers(&node[20..], false) {
                    for address in found {
                        if routable(address)
                            && !self.table.contains_key(&address)
                            && self.heard.len() < 256
                        {
                            self.heard.insert(address);
                        }
                    }
                }
            }
        }
    }

    fn periodic(&self, now: Instant) -> bool {
        let every = if self.table.len() < K {
            REFRESH_SPARSE
        } else {
            REFRESH_EVERY
        };
        self.refreshed
            .is_none_or(|at| now.duration_since(at) >= every)
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        self.periodic(now) || !self.heard.is_empty()
    }

    /// `find_node` queries for this node's own id: to the nodes heard of, then
    /// the known nodes, then `routers` when the table is empty. Answers make
    /// other nodes learn this one and fill its table.
    pub(crate) fn refresh(
        &mut self,
        routers: &[SocketAddr],
        now: Instant,
    ) -> Vec<(Vec<u8>, SocketAddr)> {
        self.pending
            .retain(|_, (_, at)| now.duration_since(*at) < QUERY_TIMEOUT);
        let periodic = self.periodic(now);
        let mut targets: Vec<SocketAddr> = std::mem::take(&mut self.heard).into_iter().collect();
        if periodic {
            self.refreshed = Some(now);
            self.table
                .retain(|_, (_, seen)| now.duration_since(*seen) < NODE_STALE * 2);
            self.expire(now);
            let mut known: Vec<_> = self
                .table
                .iter()
                .map(|(address, (id, _))| (distance(id, &self.id), *address))
                .collect();
            known.sort_unstable();
            targets.extend(known.into_iter().take(K).map(|(_, address)| address));
            if self.table.len() < K {
                targets.extend(routers.iter().filter(|r| r.is_ipv4()));
            }
        }
        let mut queries = Vec::new();
        let mut seen = BTreeSet::new();
        for target in targets {
            if queries.len() >= REFRESH_QUERIES || self.pending.len() >= REFRESH_QUERIES * 2 {
                break;
            }
            if !seen.insert(target) {
                continue;
            }
            self.transaction = self.transaction.wrapping_add(1);
            let transaction = self.transaction.to_be_bytes();
            self.pending.insert(transaction, (target, now));
            queries.push((
                crate::bencode::encode(&value_dict(&[
                    (b"t", Value::Bytes(transaction.to_vec())),
                    (b"y", Value::Bytes(b"q".to_vec())),
                    (b"q", Value::Bytes(b"find_node".to_vec())),
                    (
                        b"a",
                        value_dict(&[
                            (b"id", Value::Bytes(self.id.to_vec())),
                            (b"target", Value::Bytes(self.id.to_vec())),
                        ]),
                    ),
                ])),
                target,
            ));
        }
        queries
    }
}

/// Resolves the bootstrap routers into `routers` once, after
/// `FIRST_REFRESH`, then again every minute while none resolved.
pub(crate) fn resolve_routers(
    network: &dyn Network,
    stop: &AtomicBool,
    routers: &std::sync::Mutex<Vec<SocketAddr>>,
) {
    let mut wait = FIRST_REFRESH;
    loop {
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if stop.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let mut found = Vec::new();
        for router in ROUTERS {
            if stop.load(Ordering::Acquire) {
                return;
            }
            if let Ok(addresses) = network.resolve(router, Instant::now() + Duration::from_secs(5))
            {
                found.extend(addresses.into_iter().filter(SocketAddr::is_ipv4).take(2));
            }
        }
        if !found.is_empty() {
            *routers.lock().unwrap_or_else(|e| e.into_inner()) = found;
            return;
        }
        wait = Duration::from_secs(60);
    }
}

/// One untrusted datagram, as a query and as an answer to a pending query.
#[doc(hidden)]
pub(crate) fn fuzz(data: &[u8]) {
    let Ok(mut node) = Node::new() else {
        return;
    };
    let from = SocketAddr::from(([198, 51, 100, 7], 6881));
    let now = Instant::now();
    let _ = node.handle(data, from, now);
    node.pending.insert(*b"aa", (from, now));
    let _ = node.handle(data, from, now);
    let _ = node.refresh(&[from], now);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(method: &[u8], arguments: &[(&[u8], Value)]) -> Vec<u8> {
        crate::bencode::encode(&value_dict(&[
            (b"t", Value::Bytes(b"aa".to_vec())),
            (b"y", Value::Bytes(b"q".to_vec())),
            (b"q", Value::Bytes(method.to_vec())),
            (b"a", value_dict(arguments)),
        ]))
    }

    fn reply(node: &mut Node, packet: &[u8], from: SocketAddr) -> Value {
        crate::bencode::parse(&node.handle(packet, from, Instant::now()).unwrap()).unwrap()
    }

    fn field<'a>(value: &'a Value, path: &[&[u8]]) -> Option<&'a Value> {
        path.iter()
            .try_fold(value, |value, key| value_field(value, key))
    }

    #[test]
    fn answers_queries_stores_announces_and_checks_tokens() {
        let mut node = Node::new().unwrap();
        let client: SocketAddr = "8.8.4.4:7000".parse().unwrap();
        let other: SocketAddr = "1.1.1.1:6881".parse().unwrap();
        let hash = [3; 20];
        let ping = reply(
            &mut node,
            &query(b"ping", &[(b"id", Value::Bytes(vec![1; 20]))]),
            client,
        );
        assert_eq!(
            field(&ping, &[b"r", b"id"]).unwrap().as_bytes(),
            Some(&node.id[..])
        );
        assert_eq!(node.table.get(&client).map(|e| e.0), Some([1; 20]));
        // A read-only lookup is answered but never enters the table.
        let ro = query(
            b"ping",
            &[(b"id", Value::Bytes(vec![2; 20])), (b"ro", Value::Int(1))],
        );
        reply(&mut node, &ro, other);
        assert!(!node.table.contains_key(&other));

        let get = query(
            b"get_peers",
            &[
                (b"id", Value::Bytes(vec![1; 20])),
                (b"info_hash", Value::Bytes(hash.to_vec())),
            ],
        );
        let answer = reply(&mut node, &get, client);
        let token = field(&answer, &[b"r", b"token"])
            .unwrap()
            .as_bytes()
            .unwrap()
            .to_vec();
        assert!(field(&answer, &[b"r", b"values"]).is_none());
        assert_eq!(
            field(&answer, &[b"r", b"nodes"])
                .unwrap()
                .as_bytes()
                .unwrap()
                .len(),
            26
        );

        let announce = |token: &[u8], implied: i64| {
            query(
                b"announce_peer",
                &[
                    (b"id", Value::Bytes(vec![1; 20])),
                    (b"info_hash", Value::Bytes(hash.to_vec())),
                    (b"port", Value::Int(51413)),
                    (b"implied_port", Value::Int(implied)),
                    (b"token", Value::Bytes(token.to_vec())),
                ],
            )
        };
        // Another address cannot use the token.
        let refused = reply(&mut node, &announce(&token, 0), other);
        assert_eq!(
            field(&refused, &[b"y"]).unwrap().as_bytes(),
            Some(&b"e"[..])
        );
        reply(&mut node, &announce(&token, 0), client);
        reply(&mut node, &announce(&token, 1), client);
        let answer = reply(&mut node, &get, other);
        let Some(Value::List(values)) = field(&answer, &[b"r", b"values"]) else {
            panic!("no values");
        };
        let mut peers: Vec<_> = values
            .iter()
            .flat_map(|v| decode_peers(v.as_bytes().unwrap(), false).unwrap())
            .collect();
        peers.sort();
        assert_eq!(
            peers,
            [
                "8.8.4.4:7000".parse().unwrap(),
                "8.8.4.4:51413".parse().unwrap()
            ]
        );

        // A token from the previous secret is still valid, not an older one.
        node.rotated -= SECRET_EVERY;
        let _ = node.rotate(Instant::now());
        assert!(node.valid_token(&token, client.ip()));
        node.rotated -= SECRET_EVERY;
        let _ = node.rotate(Instant::now());
        assert!(!node.valid_token(&token, client.ip()));

        let unknown = reply(
            &mut node,
            &query(b"vote", &[(b"id", Value::Bytes(vec![1; 20]))]),
            client,
        );
        assert_eq!(
            field(&unknown, &[b"e"]).and_then(|e| match e {
                Value::List(items) => items.first(),
                _ => None,
            }),
            Some(&Value::Int(204))
        );
        assert!(
            node.handle(b"not bencode", client, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn replies_are_rate_limited_and_buckets_bounded() {
        let mut node = Node::new().unwrap();
        let ping = query(b"ping", &[(b"id", Value::Bytes(vec![1; 20]))]);
        let from: SocketAddr = "8.8.8.8:1".parse().unwrap();
        let now = Instant::now();
        let answered = (0..REPLIES_PER_SECOND + 20)
            .filter(|_| node.handle(&ping, from, now).is_some())
            .count();
        assert_eq!(answered, REPLIES_PER_SECOND as usize);
        assert!(
            node.handle(&ping, from, now + Duration::from_secs(1))
                .is_some()
        );

        // Ids sharing no prefix with the node's own id all land in bucket 0.
        let mut node = Node::new().unwrap();
        let own = node.id;
        let far = |n: u8| {
            let mut id = own;
            id[0] ^= 0x80;
            id[19] = n;
            id
        };
        for n in 0..20 {
            node.learn(far(n), SocketAddr::from(([9, 9, 9, n], 6881)), now);
        }
        assert_eq!(node.table.len(), K);
        // Private and loopback addresses never enter the table.
        node.learn([7; 20], "192.168.1.2:6881".parse().unwrap(), now);
        node.learn([7; 20], "127.0.0.1:6881".parse().unwrap(), now);
        assert_eq!(node.table.len(), K);
        // A stale member gives way.
        node.learn(far(30), "9.9.9.30:6881".parse().unwrap(), now + NODE_STALE);
        assert_eq!(node.table.len(), K);
        assert!(node.table.contains_key(&"9.9.9.30:6881".parse().unwrap()));
    }

    #[test]
    fn refresh_queries_routers_then_the_nodes_they_name() {
        let mut node = Node::new().unwrap();
        let router: SocketAddr = "67.215.246.10:6881".parse().unwrap();
        let now = Instant::now();
        let queries = node.refresh(&[router], now);
        assert_eq!(queries.len(), 1);
        let sent = crate::bencode::parse(&queries[0].0).unwrap();
        let transaction = value_field(&sent, b"t")
            .unwrap()
            .as_bytes()
            .unwrap()
            .to_vec();
        let mut nodes = vec![5; 20];
        nodes.extend_from_slice(&[9, 9, 9, 9, 0x1a, 0xe1]);
        let answer = crate::bencode::encode(&value_dict(&[
            (b"t", Value::Bytes(transaction.clone())),
            (b"y", Value::Bytes(b"r".to_vec())),
            (
                b"r",
                value_dict(&[
                    (b"id", Value::Bytes(vec![4; 20])),
                    (b"nodes", Value::Bytes(nodes)),
                ]),
            ),
        ]));
        // An answer from another address is ignored.
        assert!(
            node.handle(&answer, "9.9.9.10:6881".parse().unwrap(), now)
                .is_none()
        );
        assert!(node.table.is_empty());
        assert!(node.handle(&answer, router, now).is_none());
        assert!(node.table.contains_key(&router));
        assert!(node.due(now));
        let next = node.refresh(&[router], now);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].1, "9.9.9.9:6881".parse().unwrap());
    }
}
