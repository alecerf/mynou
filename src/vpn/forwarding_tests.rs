//! Synthetic NAT-PMP gateway: grant, renewal, port change, expiry, refusal,
//! malformed and foreign answers, retransmission and cancellation.
use super::*;
use crate::{
    Result,
    net::{HttpClient, NetworkStream},
    torrent::transport::NetworkListener,
};
use std::{collections::VecDeque, io};

const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 2, 0, 1);
type Replies = Vec<(Vec<u8>, SocketAddr)>;
type Script = Box<dyn FnMut(&[u8]) -> Replies + Send>;

fn gateway() -> SocketAddr {
    SocketAddr::from((GATEWAY, GATEWAY_PORT))
}
fn reply(op: u8, code: u16, port: u16, lifetime: u32) -> Vec<u8> {
    let mut packet = vec![0, 128 + op];
    packet.extend_from_slice(&code.to_be_bytes());
    packet.extend_from_slice(&77_u32.to_be_bytes());
    packet.extend_from_slice(&0_u16.to_be_bytes());
    packet.extend_from_slice(&port.to_be_bytes());
    packet.extend_from_slice(&lifetime.to_be_bytes());
    packet
}
fn grant(port: u16, lifetime: u32) -> Script {
    Box::new(move |packet| vec![(reply(packet[1], 0, port, lifetime), gateway())])
}
fn fast() -> Timing {
    Timing {
        first_wait: Duration::from_millis(20),
        attempts: 3,
        budget: Duration::from_millis(300),
        retry: Duration::from_millis(50),
        retry_max: Duration::from_millis(100),
    }
}

/// A protected network that only offers datagrams to the scripted gateway.
#[derive(Clone)]
struct Gateway {
    script: Arc<Mutex<Script>>,
    sent: Arc<Mutex<Replies>>,
}
impl Gateway {
    fn new(script: Script) -> Self {
        Self {
            script: Arc::new(Mutex::new(script)),
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn script(&self, script: Script) {
        *self.script.lock().unwrap() = script;
    }
    fn sent(&self) -> Replies {
        self.sent.lock().unwrap().clone()
    }
}
struct Socket {
    gateway: Gateway,
    inbox: Mutex<VecDeque<(Vec<u8>, SocketAddr)>>,
    timeout: Mutex<Option<Duration>>,
}
impl NetworkDatagram for Socket {
    fn send_to(&self, packet: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.gateway
            .sent
            .lock()
            .unwrap()
            .push((packet.to_vec(), target));
        let replies = (self.gateway.script.lock().unwrap())(packet);
        self.inbox.lock().unwrap().extend(replies);
        Ok(packet.len())
    }
    fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let until = Instant::now() + self.timeout.lock().unwrap().unwrap_or(POLL);
        loop {
            if let Some((packet, source)) = self.inbox.lock().unwrap().pop_front() {
                let length = packet.len().min(buffer.len());
                buffer[..length].copy_from_slice(&packet[..length]);
                return Ok((length, source));
            }
            if Instant::now() >= until {
                return Err(ErrorKind::WouldBlock.into());
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self.timeout.lock().unwrap() = timeout;
        Ok(())
    }
    fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
}
impl Network for Gateway {
    fn connect(&self, _: SocketAddr, _: Duration) -> Result<Box<dyn NetworkStream>> {
        Err("No peer connection in this fixture".into())
    }
    fn resolve(&self, _: &str, _: Instant) -> Result<Vec<SocketAddr>> {
        Err("Forwarding never resolves names".into())
    }
    fn datagram(&self, ipv6: bool) -> Result<Box<dyn NetworkDatagram>> {
        assert!(!ipv6);
        Ok(Box::new(Socket {
            gateway: self.clone(),
            inbox: Mutex::new(VecDeque::new()),
            timeout: Mutex::new(None),
        }))
    }
    fn http(&self) -> HttpClient {
        unreachable!("forwarding never uses HTTP")
    }
    fn listen(&self, _: SocketAddr) -> Result<Box<dyn NetworkListener>> {
        Err("Forwarding never listens".into())
    }
}
fn wait(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "condition not reached");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn requests_follow_the_documented_mapping_and_grant_the_tcp_port() {
    let network = Gateway::new(Box::new(|packet| {
        let port = if packet[1] == UDP { 40001 } else { 53186 };
        vec![(reply(packet[1], 0, port, 60), gateway())]
    }));
    let stop = AtomicBool::new(false);
    let mapping = acquire(&network, GATEWAY, &fast(), &stop).unwrap();
    assert_eq!(
        mapping,
        Mapping {
            port: 53186,
            lifetime: Duration::from_secs(60)
        }
    );
    // UDP then TCP, public port 1, private port 0, 60 seconds, version 0.
    assert_eq!(
        network.sent(),
        [
            (vec![0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 60], gateway()),
            (vec![0, 2, 0, 0, 0, 0, 0, 1, 0, 0, 0, 60], gateway()),
        ]
    );
}

#[test]
fn every_result_code_refuses_at_once_and_a_failed_udp_mapping_stops() {
    for code in 1..=7 {
        let network = Gateway::new(Box::new(move |packet| {
            vec![(reply(packet[1], code, 0, 0), gateway())]
        }));
        let stop = AtomicBool::new(false);
        let failure = acquire(&network, GATEWAY, &fast(), &stop).unwrap_err();
        assert_eq!(failure, Failure::Refused(code));
        assert_eq!(network.sent().len(), 1, "a refusal is not retransmitted");
    }
    assert_eq!(
        Failure::Refused(2).reason(),
        "Gateway refused forwarding; use a P2P server and profile"
    );
    assert_eq!(
        Failure::Refused(9).reason(),
        "Gateway returned an undefined NAT-PMP result"
    );
}

#[test]
fn foreign_and_unrelated_answers_are_ignored_and_bad_grants_are_malformed() {
    let network = Gateway::new(Box::new(|packet| {
        let op = packet[1];
        let valid = reply(op, 0, 53186, 7200);
        let mut short = valid.clone();
        short.pop();
        let mut long = valid.clone();
        long.push(0);
        let mut version = valid.clone();
        version[0] = 1;
        vec![
            (valid.clone(), "10.2.0.2:5351".parse().unwrap()),
            (valid.clone(), "10.2.0.1:5352".parse().unwrap()),
            (short, gateway()),
            (long, gateway()),
            (version, gateway()),
            (reply(3 - op, 0, 1, 60), gateway()),
            (vec![0, op], gateway()),
            (valid, gateway()),
        ]
    }));
    let stop = AtomicBool::new(false);
    let mapping = acquire(&network, GATEWAY, &fast(), &stop).unwrap();
    // Never trust a lifetime longer than requested.
    assert_eq!(mapping.lifetime, Duration::from_secs(60));
    assert_eq!(network.sent().len(), 2);

    for (port, lifetime) in [(0, 60), (53186, 0)] {
        let network = Gateway::new(grant(port, lifetime));
        let failure = acquire(&network, GATEWAY, &fast(), &stop).unwrap_err();
        assert_eq!(failure, Failure::Malformed);
    }
}

#[test]
fn silence_retransmits_on_a_doubling_schedule_within_the_budget() {
    let network = Gateway::new(Box::new(|_| Vec::new()));
    let stop = AtomicBool::new(false);
    let mut timing = fast();
    timing.attempts = 4;
    timing.budget = Duration::from_secs(5);
    let started = Instant::now();
    assert_eq!(
        acquire(&network, GATEWAY, &timing, &stop),
        Err(Failure::Timeout)
    );
    // 20 + 40 + 80 + 160 ms.
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert_eq!(network.sent().len(), 4);

    // The total budget ends the schedule early.
    let network = Gateway::new(Box::new(|_| Vec::new()));
    timing.attempts = 9;
    timing.budget = Duration::from_millis(100);
    let started = Instant::now();
    assert_eq!(
        acquire(&network, GATEWAY, &timing, &stop),
        Err(Failure::Timeout)
    );
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(network.sent().len() <= 3);
}

#[test]
fn the_lease_renews_changes_port_expires_and_returns() {
    let network = Gateway::new(grant(53186, 1));
    let forwarder = Forwarder::start(Arc::new(network.clone()), GATEWAY, fast());
    wait(|| forwarder.status() == Status::Active(53186));
    assert_eq!(forwarder.advertise(), Some(53186));
    assert!(!forwarder.stale());

    // Renewal at three quarters of the one-second lease keeps the port.
    let requests = network.sent().len();
    wait(|| network.sent().len() >= requests + 4);
    assert_eq!(forwarder.status(), Status::Active(53186));
    assert!(!forwarder.stale());

    // A renewal returning another port makes the advertised one stale.
    network.script(grant(53187, 1));
    wait(|| forwarder.status() == Status::Active(53187));
    assert!(forwarder.stale());
    assert_eq!(forwarder.advertise(), Some(53187));
    assert!(!forwarder.stale());

    // Refused renewals keep the held port only until its lease expires.
    network.script(Box::new(|packet| {
        vec![(reply(packet[1], 2, 0, 0), gateway())]
    }));
    let refused = Instant::now();
    wait(|| matches!(forwarder.status(), Status::Unavailable(_)));
    assert!(refused.elapsed() <= Duration::from_millis(1200));
    assert!(forwarder.stale());
    assert_eq!(forwarder.advertise(), None);
    assert!(!forwarder.stale());
    wait(|| {
        forwarder.status()
            == Status::Unavailable("Gateway refused forwarding; use a P2P server and profile")
    });

    // A later grant is stale for a listener that started without a port.
    network.script(grant(53188, 1));
    wait(|| forwarder.status() == Status::Active(53188));
    assert!(forwarder.stale());
}

#[test]
fn an_unavailable_network_and_stopping_end_promptly() {
    struct Revoked;
    impl Network for Revoked {
        fn connect(&self, _: SocketAddr, _: Duration) -> Result<Box<dyn NetworkStream>> {
            Err("revoked".into())
        }
        fn resolve(&self, _: &str, _: Instant) -> Result<Vec<SocketAddr>> {
            Err("revoked".into())
        }
        fn datagram(&self, _: bool) -> Result<Box<dyn NetworkDatagram>> {
            Err("revoked".into())
        }
        fn http(&self) -> HttpClient {
            unreachable!()
        }
        fn listen(&self, _: SocketAddr) -> Result<Box<dyn NetworkListener>> {
            Err("revoked".into())
        }
    }
    let forwarder = Forwarder::start(Arc::new(Revoked), GATEWAY, fast());
    wait(|| forwarder.status() == Status::Unavailable("Protected network unavailable"));
    drop(forwarder);

    // A silent gateway on the default RFC schedule: dropping does not wait
    // for the pending retransmission.
    let network = Gateway::new(Box::new(|_| Vec::new()));
    let forwarder = Forwarder::start(Arc::new(network.clone()), GATEWAY, Timing::default());
    wait(|| !network.sent().is_empty());
    assert_eq!(forwarder.status(), Status::Acquiring);
    let started = Instant::now();
    drop(forwarder);
    assert!(started.elapsed() < Duration::from_millis(500));
}
