use super::*;
use crate::Result;
use crate::netstack::ipv4::{MAX_MTU, parse_protocol};
use crate::netstack::support::Failing;

const G: u64 = 9;
const CLIENT: Ipv4Addr = Ipv4Addr::new(10, 64, 0, 2);
const SERVER: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 9);
const PORT: u16 = 80;
const T0: Duration = Duration::ZERO;
const CLIENT_ISS: u32 = 1000;
const SERVER_ISS: u32 = 5000;
/// The first ephemeral port: the scripted draw of zero.
const CLIENT_PORT: u16 = EPHEMERAL_START;

/// Replays scripted 32-bit values; a two-byte draw takes the low half.
struct Draws {
    values: Vec<u32>,
    calls: usize,
}

impl Draws {
    fn new(values: &[u32]) -> Self {
        Self {
            values: values.to_vec(),
            calls: 0,
        }
    }
}

impl Entropy for Draws {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let value = self.values[self.calls % self.values.len()];
        self.calls += 1;
        let all = value.to_be_bytes();
        let mut out = [0u8; N];
        for (slot, byte) in out.iter_mut().zip(all[4usize.saturating_sub(N)..].iter()) {
            *slot = *byte;
        }
        Ok(out)
    }
}

fn ip(local: Ipv4Addr) -> Ipv4Config {
    Ipv4Config::new(local, Ipv4Addr::UNSPECIFIED, 0).unwrap()
}

fn config(local: Ipv4Addr) -> TcpConfig {
    TcpConfig::new(ip(local), MAX_MTU)
}

type Stack = TcpStack<Draws>;

fn stack(config: TcpConfig, draws: &[u32]) -> Stack {
    TcpStack::new(config, Draws::new(draws), G, T0).unwrap()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dir {
    AToB,
    BToA,
}

/// A client stack `a` and a server stack `b` joined by a scriptable link.
struct Pair {
    a: Stack,
    b: Stack,
    now: Duration,
}

impl Pair {
    fn with(a: TcpConfig, b: TcpConfig, draws_a: &[u32], draws_b: &[u32]) -> Self {
        Self {
            a: stack(a, draws_a),
            b: stack(b, draws_b),
            now: T0,
        }
    }

    fn new(a: TcpConfig, b: TcpConfig) -> Self {
        Self::with(a, b, &[0, CLIENT_ISS], &[SERVER_ISS])
    }

    /// Polls both sides until no more packets are produced, passing each through
    /// `link`, and returns the earliest wake-up time.
    fn pump_with(
        &mut self,
        link: &mut dyn FnMut(Dir, Vec<u8>) -> Vec<Vec<u8>>,
    ) -> Option<Duration> {
        for _ in 0..2000 {
            let from_a = self.a.poll(G, self.now).unwrap();
            let from_b = self.b.poll(G, self.now).unwrap();
            let next = [from_a.next, from_b.next].into_iter().flatten().min();
            if from_a.send.is_empty() && from_b.send.is_empty() {
                return next;
            }
            for packet in from_a.send {
                for packet in link(Dir::AToB, packet) {
                    self.b.ingest(G, self.now, &packet);
                }
            }
            for packet in from_b.send {
                for packet in link(Dir::BToA, packet) {
                    self.a.ingest(G, self.now, &packet);
                }
            }
        }
        panic!("traffic never settled");
    }

    fn pump(&mut self) -> Option<Duration> {
        self.pump_with(&mut |_, packet| vec![packet])
    }
}

fn established_with(
    a: TcpConfig,
    b: TcpConfig,
    iss: (u32, u32),
) -> (Pair, FlowHandle, FlowHandle, ListenerHandle) {
    let mut pair = Pair::with(a, b, &[0, iss.0], &[iss.1]);
    let listener = pair.b.listen(G, PORT).unwrap();
    let client = pair.a.connect(G, T0, SERVER, PORT).unwrap();
    pair.pump();
    let server = pair.b.accept(G, listener).unwrap().unwrap();
    (pair, client, server, listener)
}

fn established() -> (Pair, FlowHandle, FlowHandle, ListenerHandle) {
    established_with(config(CLIENT), config(SERVER), (CLIENT_ISS, SERVER_ISS))
}

fn read(stack: &mut Stack, handle: FlowHandle) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buffer = [0u8; 1500];
    while let Read::Data(count) = stack.recv(G, handle, &mut buffer).unwrap() {
        out.extend_from_slice(&buffer[..count]);
    }
    out
}

fn pattern(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

/// The fields of a packet a stack produced, as the receiving side sees them.
#[derive(Debug)]
struct Seen {
    sequence: u32,
    acknowledgment: u32,
    flags: u8,
    mss: Option<u16>,
    payload: Vec<u8>,
}

fn seen(packet: &[u8], to: Ipv4Addr) -> Seen {
    let parsed = parse_protocol(&ip(to), packet, PROTOCOL_TCP).unwrap();
    let segment = tcp::parse(parsed.source, parsed.destination, parsed.payload).unwrap();
    Seen {
        sequence: segment.sequence,
        acknowledgment: segment.acknowledgment,
        flags: segment.flags,
        mss: segment.mss,
        payload: segment.payload.to_vec(),
    }
}

/// A forged segment from `from` to `to`.
fn forge(
    from: (Ipv4Addr, u16),
    to: (Ipv4Addr, u16),
    fields: (u32, u32, u8),
    payload: &[u8],
) -> Vec<u8> {
    let header = Header {
        source_port: from.1,
        destination_port: to.1,
        sequence: fields.0,
        acknowledgment: fields.1,
        flags: fields.2,
        window: 1000,
        mss: None,
    };
    tcp::build(from.0, to.0, &header, payload, MAX_MTU).unwrap()
}

/// A segment the real server would send the client on the first connection.
fn from_server(sequence: u32, acknowledgment: u32, flags: u8, payload: &[u8]) -> Vec<u8> {
    forge(
        (SERVER, PORT),
        (CLIENT, CLIENT_PORT),
        (sequence, acknowledgment, flags),
        payload,
    )
}

#[test]
fn configuration_is_validated() {
    let good = config(CLIENT);
    assert!(TcpStack::new(good, Draws::new(&[1]), G, T0).is_ok());
    let second = Duration::from_secs(1);
    let hour = Duration::from_secs(3601);
    let bad = [
        TcpConfig { mtu: 100, ..good },
        TcpConfig { flows: 0, ..good },
        TcpConfig {
            flows: 1025,
            ..good
        },
        TcpConfig {
            listeners: 0,
            ..good
        },
        TcpConfig { backlog: 0, ..good },
        TcpConfig {
            send_buffer: 10,
            ..good
        },
        TcpConfig {
            receive_buffer: 70000,
            ..good
        },
        TcpConfig {
            receive_buffer: 100,
            ..good
        },
        TcpConfig {
            total_buffer: 1000,
            ..good
        },
        TcpConfig {
            reorder_segments: 65,
            ..good
        },
        TcpConfig { burst: 0, ..good },
        TcpConfig { outbox: 0, ..good },
        TcpConfig {
            initial_rto: Duration::ZERO,
            ..good
        },
        TcpConfig {
            initial_rto: hour,
            max_rto: hour,
            ..good
        },
        TcpConfig {
            initial_rto: second * 31,
            ..good
        },
        TcpConfig { retries: 0, ..good },
        TcpConfig {
            retries: 17,
            ..good
        },
        TcpConfig {
            syn_retries: 9,
            ..good
        },
        TcpConfig {
            time_wait: Duration::ZERO,
            ..good
        },
        TcpConfig {
            linger: hour,
            ..good
        },
        TcpConfig {
            persist_limit: Duration::ZERO,
            ..good
        },
    ];
    for config in bad {
        let built = TcpStack::new(config, Draws::new(&[1]), G, T0);
        assert_eq!(built.err(), Some(TcpError::Config), "{config:?}");
    }
}

#[test]
fn connect_validates_the_endpoint_before_drawing_randomness() {
    let ip = Ipv4Config::new(CLIENT, Ipv4Addr::new(203, 0, 113, 0), 24).unwrap();
    let mut client = stack(TcpConfig::new(ip, MAX_MTU), &[0, 1]);
    for (address, port) in [
        (SERVER, 0),
        (Ipv4Addr::LOCALHOST, PORT),
        (Ipv4Addr::UNSPECIFIED, PORT),
        (CLIENT, PORT),
        (Ipv4Addr::new(198, 51, 100, 1), PORT),
        (Ipv4Addr::new(224, 0, 0, 1), PORT),
        (Ipv4Addr::BROADCAST, PORT),
    ] {
        assert_eq!(
            client.connect(G, T0, address, port).err(),
            Some(TcpError::Endpoint),
            "{address}:{port}"
        );
    }
    assert_eq!(client.flows(), 0);
    assert_eq!(client.entropy.calls, 0);
    assert!(client.connect(G, T0, SERVER, PORT).is_ok());
}

#[test]
fn listener_limits() {
    let mut server = stack(
        TcpConfig {
            listeners: 1,
            ..config(SERVER)
        },
        &[1],
    );
    assert_eq!(server.listen(G, 0).err(), Some(TcpError::Endpoint));
    let handle = server.listen(G, PORT).unwrap();
    assert_eq!(handle.port(), PORT);
    assert_eq!(server.listen(G, PORT).err(), Some(TcpError::PortInUse));
    assert_eq!(server.listen(G, 81).err(), Some(TcpError::ListenersFull));
    assert_eq!(server.listeners(), 1);
    assert_eq!(server.accept(G, handle), Ok(None));
    server.unlisten(G, handle).unwrap();
    assert_eq!(server.unlisten(G, handle), Err(TcpError::Closed));
    assert_eq!(server.accept(G, handle), Err(TcpError::Closed));
    // A recycled port is a new listener: the old handle stays dead.
    let again = server.listen(G, PORT).unwrap();
    assert_eq!(server.accept(G, handle), Err(TcpError::Closed));
    assert_eq!(server.accept(G, again), Ok(None));
}

#[test]
fn handshake_and_data_both_ways() {
    let (mut pair, client, server, _) = established();
    assert_eq!(pair.a.state(G, client), Ok(State::Established));
    assert_eq!(pair.b.state(G, server), Ok(State::Established));
    assert_eq!(client.remote(), (SERVER, PORT));
    assert_eq!(client.local_port(), CLIENT_PORT);
    assert_eq!(server.remote(), (CLIENT, CLIENT_PORT));
    assert_eq!(server.local_port(), PORT);
    let mut buffer = [0u8; 8];
    assert_eq!(pair.a.recv(G, client, &mut buffer), Ok(Read::WouldBlock));
    assert_eq!(pair.a.send(G, client, b"hello"), Ok(5));
    pair.pump();
    assert_eq!(read(&mut pair.b, server), b"hello");
    assert_eq!(pair.b.send(G, server, b"world!"), Ok(6));
    pair.pump();
    assert_eq!(read(&mut pair.a, client), b"world!");
    assert_eq!(pair.a.send(G, client, b""), Ok(0));
    assert_eq!(pair.a.recv(G, client, &mut []), Ok(Read::WouldBlock));
    // Everything was acknowledged: no timer is left.
    assert_eq!(pair.pump(), None);
}

#[test]
fn a_refused_connection_fails_explicitly() {
    let mut pair = Pair::new(config(CLIENT), config(SERVER));
    let client = pair.a.connect(G, T0, SERVER, PORT).unwrap();
    pair.pump();
    assert_eq!(pair.a.state(G, client), Ok(State::Closed));
    assert_eq!(pair.a.failure(G, client), Ok(Some(Failure::Refused)));
    let refused = TcpError::Failed(Failure::Refused);
    assert_eq!(
        pair.a.send(G, client, b"x"),
        Err(TcpError::Failed(Failure::Refused))
    );
    assert_eq!(pair.a.recv(G, client, &mut [0u8; 4]), Err(refused));
    // The entry lingers for the configured time, then it is released.
    pair.now = Duration::from_secs(29);
    pair.pump();
    assert_eq!(pair.a.state(G, client), Ok(State::Closed));
    pair.now = Duration::from_secs(30);
    pair.pump();
    assert_eq!(pair.a.state(G, client), Err(TcpError::Closed));
    assert_eq!(pair.a.flows(), 0);
}

#[test]
fn closing_one_side_then_the_other() {
    let (mut pair, client, server, _) = established();
    pair.a.send(G, client, b"bye").unwrap();
    pair.a.close(G, pair.now, client).unwrap();
    pair.a.close(G, pair.now, client).unwrap();
    assert_eq!(pair.a.send(G, client, b"x"), Err(TcpError::Shutdown));
    pair.pump();
    assert_eq!(pair.b.state(G, server), Ok(State::CloseWait));
    assert_eq!(pair.a.state(G, client), Ok(State::FinWait2));
    assert_eq!(read(&mut pair.b, server), b"bye");
    assert_eq!(pair.b.recv(G, server, &mut [0u8; 4]), Ok(Read::Eof));
    // The server may keep sending after the client finished.
    pair.b.send(G, server, b"late").unwrap();
    pair.pump();
    assert_eq!(read(&mut pair.a, client), b"late");
    pair.b.close(G, pair.now, server).unwrap();
    pair.pump();
    assert_eq!(pair.b.state(G, server), Ok(State::Closed));
    assert_eq!(pair.b.failure(G, server), Ok(None));
    assert_eq!(pair.a.state(G, client), Ok(State::TimeWait));
    assert_eq!(pair.a.recv(G, client, &mut [0u8; 4]), Ok(Read::Eof));
    let wake = Some(Duration::from_secs(30));
    assert_eq!(pair.pump(), wake);
    pair.now = Duration::from_secs(30);
    pair.pump();
    assert_eq!((pair.a.flows(), pair.b.flows()), (0, 0));
}

#[test]
fn simultaneous_close() {
    let (mut pair, client, server, _) = established();
    pair.a.close(G, pair.now, client).unwrap();
    pair.b.close(G, pair.now, server).unwrap();
    pair.pump();
    assert_eq!(pair.a.state(G, client), Ok(State::TimeWait));
    assert_eq!(pair.b.state(G, server), Ok(State::TimeWait));
}

#[test]
fn a_repeated_fin_is_acknowledged_again_and_restarts_time_wait() {
    let (mut pair, client, server, _) = established();
    pair.b.close(G, pair.now, server).unwrap();
    let mut server_fin = Vec::new();
    pair.pump_with(&mut |dir, packet| {
        if dir == Dir::BToA && seen(&packet, CLIENT).flags & FIN != 0 {
            server_fin.push(packet.clone());
        }
        vec![packet]
    });
    assert_eq!(pair.a.state(G, client), Ok(State::CloseWait));
    pair.a.close(G, pair.now, client).unwrap();
    let mut client_fin = Vec::new();
    pair.pump_with(&mut |dir, packet| {
        if dir == Dir::AToB && seen(&packet, SERVER).flags & FIN != 0 {
            client_fin.push(packet.clone());
        }
        vec![packet]
    });
    assert_eq!(pair.b.state(G, server), Ok(State::TimeWait));
    assert_eq!(pair.a.state(G, client), Ok(State::Closed));
    // A repeated FIN reaches the TIME-WAIT side: it is acknowledged again and
    // the wait starts over.
    pair.now = Duration::from_secs(10);
    assert_eq!(
        pair.b.ingest(G, pair.now, &client_fin[0]),
        Ingest::Dropped(Discard::Window)
    );
    let poll = pair.b.poll(G, pair.now).unwrap();
    assert_eq!(poll.send.len(), 1);
    assert_eq!(seen(&poll.send[0], CLIENT).flags, ACK);
    assert_eq!(poll.next, Some(Duration::from_secs(40)));
    // A finished flow refuses whatever else arrives.
    assert_eq!(pair.a.ingest(G, pair.now, &server_fin[0]), Ingest::Answered);
}

fn transfer(iss: (u32, u32), total: usize) {
    let (mut pair, client, server, _) = established_with(config(CLIENT), config(SERVER), iss);
    let data = pattern(total);
    let mut sent = 0;
    let mut got = Vec::new();
    for _ in 0..1000 {
        if sent < data.len() {
            sent += pair.a.send(G, client, &data[sent..]).unwrap();
        }
        pair.pump();
        got.extend(read(&mut pair.b, server));
        assert!(pair.a.buffered() <= pair.a.config.total_buffer);
        assert!(pair.b.buffered() <= pair.b.config.total_buffer);
        if got.len() == data.len() {
            break;
        }
    }
    assert!(got == data, "received {} of {total} bytes", got.len());
    // The reverse direction works on the same flow.
    let reply = total.min(5000);
    pair.b.send(G, server, &data[..reply]).unwrap();
    pair.pump();
    assert_eq!(read(&mut pair.a, client), data[..reply]);
}

#[test]
fn bulk_transfer() {
    transfer((CLIENT_ISS, SERVER_ISS), 300_000);
}

#[test]
fn sequence_numbers_wrap() {
    transfer((0xffff_fe00, 0xffff_ff00), 60_000);
    transfer((u32::MAX, u32::MAX - 1), 3_000);
}

#[test]
fn lost_data_is_retransmitted_after_the_timeout() {
    let (mut pair, client, server, _) = established();
    pair.a.send(G, client, &[7u8; 100]).unwrap();
    let mut seen_by_link = 0;
    let next = pair.pump_with(&mut |dir, packet| {
        if dir == Dir::AToB {
            seen_by_link += 1;
            Vec::new()
        } else {
            vec![packet]
        }
    });
    assert_eq!(seen_by_link, 1);
    assert_eq!(read(&mut pair.b, server), b"");
    assert_eq!(next, Some(Duration::from_secs(1)));
    pair.now = Duration::from_secs(1);
    assert_eq!(pair.pump(), None);
    assert_eq!(read(&mut pair.b, server), vec![7u8; 100]);
}

#[test]
fn reordered_and_duplicated_segments_arrive_once_and_in_order() {
    let (mut pair, client, server, _) = established();
    let data = pattern(5000);
    pair.a.send(G, client, &data).unwrap();
    let mut held = Vec::new();
    pair.pump_with(&mut |dir, packet| {
        if dir == Dir::AToB {
            held.push(packet);
            Vec::new()
        } else {
            vec![packet]
        }
    });
    assert_eq!(held.len(), 4);
    for packet in held.iter().rev() {
        pair.b.ingest(G, pair.now, packet);
        pair.b.ingest(G, pair.now, packet);
    }
    pair.pump();
    assert!(read(&mut pair.b, server) == data);
    // A replay after delivery is acknowledged and dropped.
    assert_eq!(
        pair.b.ingest(G, pair.now, &held[0]),
        Ingest::Dropped(Discard::Window)
    );
    assert_eq!(read(&mut pair.b, server), b"");
}

#[test]
fn three_duplicate_acknowledgments_trigger_a_fast_retransmit() {
    let (mut pair, client, server, _) = established();
    let data = pattern(4 * 1384);
    pair.a.send(G, client, &data).unwrap();
    let packets = pair.a.poll(G, pair.now).unwrap().send;
    assert_eq!(packets.len(), 4);
    for packet in &packets[1..] {
        pair.b.ingest(G, pair.now, packet);
        for ack in pair.b.poll(G, pair.now).unwrap().send {
            assert_eq!(
                seen(&ack, CLIENT).acknowledgment,
                seen(&packets[0], SERVER).sequence
            );
            pair.a.ingest(G, pair.now, &ack);
        }
    }
    // No timer fired: the retransmission is the loss response.
    let resent = pair.a.poll(G, pair.now).unwrap().send;
    assert_eq!(resent.len(), 1);
    assert!(resent[0] == packets[0]);
    assert_eq!(read(&mut pair.b, server), b"");
    pair.b.ingest(G, pair.now, &resent[0]);
    pair.pump();
    assert!(read(&mut pair.b, server) == data);
}

#[test]
fn a_burst_limit_defers_the_rest_to_the_next_poll() {
    let (mut pair, client, _, _) = established();
    pair.a.config.burst = 2;
    pair.a.send(G, client, &pattern(5000)).unwrap();
    let first = pair.a.poll(G, pair.now).unwrap();
    assert_eq!(first.send.len(), 2);
    assert_eq!(first.next, Some(pair.now));
    let second = pair.a.poll(G, pair.now).unwrap();
    assert_eq!(second.send.len(), 2);
    assert_eq!(second.next, Some(Duration::from_secs(1)));
}

#[test]
fn unanswered_handshakes_time_out() {
    let mut client = stack(config(CLIENT), &[0, CLIENT_ISS]);
    let handle = client.connect(G, T0, SERVER, PORT).unwrap();
    let mut now = T0;
    let mut syns = 0;
    for _ in 0..50 {
        let poll = client.poll(G, now).unwrap();
        syns += poll.send.len();
        if client.state(G, handle) == Ok(State::Closed) {
            break;
        }
        now = poll.next.unwrap();
    }
    assert_eq!(syns, 6);
    assert_eq!(client.failure(G, handle), Ok(Some(Failure::Timeout)));
}

#[test]
fn unacknowledged_data_gives_up_with_a_reset() {
    let (mut pair, client, _, _) = established();
    pair.a.send(G, client, &[1u8; 100]).unwrap();
    let mut sent = Vec::new();
    for _ in 0..50 {
        let next = pair.pump_with(&mut |dir, packet| {
            if dir == Dir::AToB {
                sent.push(packet);
                Vec::new()
            } else {
                vec![packet]
            }
        });
        if pair.a.state(G, client) == Ok(State::Closed) {
            break;
        }
        pair.now = next.unwrap();
    }
    assert_eq!(pair.a.failure(G, client), Ok(Some(Failure::Timeout)));
    let data = sent.iter().filter(|packet| packet.len() > 60).count();
    assert_eq!(data, 9);
    assert_eq!(seen(sent.last().unwrap(), SERVER).flags & RST, RST);
    assert_eq!(
        pair.a.send(G, client, b"x"),
        Err(TcpError::Failed(Failure::Timeout))
    );
}

#[test]
fn a_zero_window_is_respected_and_reopened() {
    let server_config = TcpConfig {
        receive_buffer: 2048,
        ..config(SERVER)
    };
    let (mut pair, client, server, _) =
        established_with(config(CLIENT), server_config, (CLIENT_ISS, SERVER_ISS));
    let data = pattern(10_000);
    assert_eq!(pair.a.send(G, client, &data), Ok(10_000));
    pair.pump();
    assert_eq!(pair.b.buffered(), 2048);
    let mut got = read(&mut pair.b, server);
    assert_eq!(got.len(), 2048);
    for _ in 0..50 {
        pair.pump();
        got.extend(read(&mut pair.b, server));
        if got.len() == data.len() {
            break;
        }
    }
    assert!(got == data);
}

#[test]
fn a_window_that_never_reopens_fails_after_the_persist_limit() {
    let client_config = TcpConfig {
        persist_limit: Duration::from_secs(10),
        ..config(CLIENT)
    };
    let server_config = TcpConfig {
        receive_buffer: 2048,
        ..config(SERVER)
    };
    let (mut pair, client, _, _) =
        established_with(client_config, server_config, (CLIENT_ISS, SERVER_ISS));
    pair.a.send(G, client, &pattern(10_000)).unwrap();
    let mut probes = 0;
    for _ in 0..50 {
        let next = pair.pump_with(&mut |dir, packet| {
            // A one-byte segment is a window probe: 40 header bytes and one byte.
            if dir == Dir::AToB && packet.len() == 41 {
                probes += 1;
            }
            vec![packet]
        });
        if pair.a.state(G, client) == Ok(State::Closed) {
            break;
        }
        pair.now = next.unwrap();
    }
    assert_eq!(pair.a.failure(G, client), Ok(Some(Failure::Timeout)));
    assert_eq!(probes, 3);
}

#[test]
fn the_backlog_bounds_half_open_and_unaccepted_flows() {
    let server_config = TcpConfig {
        backlog: 2,
        ..config(SERVER)
    };
    let mut pair = Pair::with(
        config(CLIENT),
        server_config,
        &[0, 1000, 1, 2000, 2, 3000],
        &[5000, 6000, 7000],
    );
    let listener = pair.b.listen(G, PORT).unwrap();
    let first = pair.a.connect(G, T0, SERVER, PORT).unwrap();
    let second = pair.a.connect(G, T0, SERVER, PORT).unwrap();
    let third = pair.a.connect(G, T0, SERVER, PORT).unwrap();
    pair.pump();
    assert_eq!(pair.a.state(G, first), Ok(State::Established));
    assert_eq!(pair.a.state(G, second), Ok(State::Established));
    assert_eq!(pair.a.state(G, third), Ok(State::SynSent));
    assert_eq!(pair.b.flows(), 2);
    assert_eq!(pair.b.dropped(), 1);
    // Accepting frees a slot, and the retransmitted SYN gets in.
    let accepted = pair.b.accept(G, listener).unwrap().unwrap();
    assert_eq!(accepted.remote(), (CLIENT, EPHEMERAL_START));
    pair.now = Duration::from_secs(1);
    pair.pump();
    assert_eq!(pair.a.state(G, third), Ok(State::Established));
    assert_eq!(pair.b.flows(), 3);
}

#[test]
fn the_flow_table_is_bounded() {
    let one = TcpConfig {
        flows: 1,
        ..config(SERVER)
    };
    let mut pair = Pair::with(
        TcpConfig {
            flows: 1,
            ..config(CLIENT)
        },
        one,
        &[0, 1000, 1, 2000],
        &[5000, 6000],
    );
    pair.b.listen(G, PORT).unwrap();
    pair.a.connect(G, T0, SERVER, PORT).unwrap();
    assert_eq!(
        pair.a.connect(G, T0, SERVER, PORT).err(),
        Some(TcpError::FlowsFull)
    );
    pair.pump();
    assert_eq!(pair.b.flows(), 1);
    // A second client from the same stack is impossible; forge its SYN instead.
    let syn = forge((CLIENT, 40000), (SERVER, PORT), (1, 0, SYN), b"");
    assert_eq!(
        pair.b.ingest(G, pair.now, &syn),
        Ingest::Dropped(Discard::TableFull)
    );
}

#[test]
fn closing_a_listener_resets_flows_nobody_accepted() {
    let mut pair = Pair::new(config(CLIENT), config(SERVER));
    let listener = pair.b.listen(G, PORT).unwrap();
    let client = pair.a.connect(G, T0, SERVER, PORT).unwrap();
    pair.pump();
    assert_eq!(pair.a.state(G, client), Ok(State::Established));
    assert_eq!(pair.b.flows(), 1);
    pair.b.unlisten(G, listener).unwrap();
    assert_eq!(pair.b.flows(), 0);
    pair.pump();
    assert_eq!(pair.a.state(G, client), Ok(State::Closed));
    assert_eq!(pair.a.failure(G, client), Ok(Some(Failure::Reset)));
}

#[test]
fn a_flow_reset_before_it_is_accepted_is_never_handed_out() {
    let mut pair = Pair::new(config(CLIENT), config(SERVER));
    let listener = pair.b.listen(G, PORT).unwrap();
    let client = pair.a.connect(G, T0, SERVER, PORT).unwrap();
    pair.pump();
    pair.a.abort(G, client).unwrap();
    pair.pump();
    assert_eq!(pair.b.flows(), 0);
    assert_eq!(pair.b.accept(G, listener), Ok(None));
    assert_eq!(pair.a.abort(G, client), Err(TcpError::Closed));
}

#[test]
fn repeated_unaccepted_resets_do_not_accumulate_listener_entries() {
    let server_config = TcpConfig {
        backlog: 1,
        flows: 1,
        ..config(SERVER)
    };
    let mut pair = Pair::new(config(CLIENT), server_config);
    let listener = pair.b.listen(G, PORT).unwrap();
    for _ in 0..100 {
        let client = pair.a.connect(G, T0, SERVER, PORT).unwrap();
        pair.pump();
        assert_eq!(pair.a.state(G, client), Ok(State::Established));
        assert_eq!(pair.b.listeners[&PORT].ready.len(), 1);
        pair.a.abort(G, client).unwrap();
        pair.pump();
        assert_eq!(pair.b.flows(), 0);
        assert!(pair.b.listeners[&PORT].ready.is_empty());
    }
    let client = pair.a.connect(G, T0, SERVER, PORT).unwrap();
    pair.pump();
    let server = pair.b.accept(G, listener).unwrap().unwrap();
    pair.a.send(G, client, b"live").unwrap();
    pair.pump();
    assert_eq!(read(&mut pair.b, server), b"live");
    assert_eq!(pair.b.accept(G, listener), Ok(None));
}

#[test]
fn expired_unaccepted_flows_release_listener_entries() {
    let mut pair = Pair::new(config(CLIENT), config(SERVER));
    let listener = pair.b.listen(G, PORT).unwrap();
    let client = pair.a.connect(G, T0, SERVER, PORT).unwrap();
    pair.pump();
    let (key, serial) = pair.b.listeners[&PORT].ready[0];
    let server = FlowHandle { key, serial };
    pair.a.close(G, T0, client).unwrap();
    pair.pump();
    pair.b.close(G, T0, server).unwrap();
    pair.pump();
    assert_eq!(pair.b.listeners[&PORT].ready.len(), 1);
    pair.now = pair.b.config.linger;
    pair.pump();
    assert_eq!(pair.b.flows(), 0);
    assert!(pair.b.listeners[&PORT].ready.is_empty());
    assert_eq!(pair.b.accept(G, listener), Ok(None));
}

#[test]
fn aborting_resets_the_peer_and_old_handles_stay_dead() {
    let (mut pair, client, server, _) = established();
    pair.a.abort(G, client).unwrap();
    assert_eq!(pair.a.state(G, client), Err(TcpError::Closed));
    pair.pump();
    assert_eq!(pair.b.failure(G, server), Ok(Some(Failure::Reset)));
    assert_eq!(
        pair.b.send(G, server, b"x"),
        Err(TcpError::Failed(Failure::Reset))
    );
    // The same port and remote later give a new flow; the old handle stays dead.
    let mut other = stack(config(CLIENT), &[0, 1]);
    let old = other.connect(G, T0, SERVER, PORT).unwrap();
    other.abort(G, old).unwrap();
    let new = other.connect(G, T0, SERVER, PORT).unwrap();
    assert_eq!(old.local_port(), new.local_port());
    assert_eq!(other.state(G, old), Err(TcpError::Closed));
    assert_eq!(other.state(G, new), Ok(State::SynSent));
}

#[test]
fn calls_before_the_handshake_completes() {
    let mut client = stack(config(CLIENT), &[0, 1]);
    let handle = client.connect(G, T0, SERVER, PORT).unwrap();
    assert_eq!(client.send(G, handle, b"x"), Err(TcpError::NotConnected));
    assert_eq!(client.recv(G, handle, &mut [0u8; 4]), Ok(Read::WouldBlock));
    client.close(G, T0, handle).unwrap();
    assert_eq!(client.flows(), 0);
    assert_eq!(client.close(G, T0, handle), Err(TcpError::Closed));
}

#[test]
fn send_buffer_is_bounded() {
    let (mut pair, client, _, _) = established();
    let capacity = pair.a.config.send_buffer;
    assert_eq!(
        pair.a.send(G, client, &vec![0u8; capacity + 10]),
        Ok(capacity)
    );
    assert_eq!(pair.a.send(G, client, b"more"), Ok(0));
    assert_eq!(pair.a.buffered(), capacity);
}

#[test]
fn out_of_order_data_is_bounded_and_recovered() {
    let server_config = TcpConfig {
        reorder_segments: 2,
        ..config(SERVER)
    };
    let (mut pair, client, server, _) =
        established_with(config(CLIENT), server_config, (CLIENT_ISS, SERVER_ISS));
    let data = pattern(6000);
    pair.a.send(G, client, &data).unwrap();
    let packets = pair.a.poll(G, pair.now).unwrap().send;
    assert_eq!(packets.len(), 4);
    for packet in &packets[1..] {
        pair.b.ingest(G, pair.now, packet);
    }
    // Two segments were kept, the third was dropped.
    assert_eq!(pair.b.buffered(), 2 * 1384);
    let mut got = Vec::new();
    for _ in 0..30 {
        let next = pair.pump();
        got.extend(read(&mut pair.b, server));
        if got.len() == data.len() {
            break;
        }
        pair.now = next.unwrap_or(pair.now);
    }
    assert!(got == data);
}

#[test]
fn a_peer_mss_limits_the_segments_sent() {
    let mut server = stack(config(SERVER), &[SERVER_ISS]);
    let listener = server.listen(G, PORT).unwrap();
    let mut syn = Header {
        source_port: 40000,
        destination_port: PORT,
        sequence: 100,
        acknowledgment: 0,
        flags: SYN,
        window: 65535,
        mss: Some(100),
    };
    let packet = tcp::build(CLIENT, SERVER, &syn, &[], MAX_MTU).unwrap();
    assert_eq!(server.ingest(G, T0, &packet), Ingest::Handshake);
    let synack = server.poll(G, T0).unwrap().send;
    let reply = seen(&synack[0], CLIENT);
    assert_eq!(reply.flags, SYN | ACK);
    assert_eq!(reply.sequence, SERVER_ISS);
    assert_eq!(reply.acknowledgment, 101);
    assert_eq!(reply.mss, Some(1384));
    syn.flags = ACK;
    syn.mss = None;
    syn.sequence = 101;
    syn.acknowledgment = SERVER_ISS + 1;
    let ack = tcp::build(CLIENT, SERVER, &syn, &[], MAX_MTU).unwrap();
    assert_eq!(server.ingest(G, T0, &ack), Ingest::Delivered);
    let flow = server.accept(G, listener).unwrap().unwrap();
    server.send(G, flow, &pattern(300)).unwrap();
    let sent = server.poll(G, T0).unwrap().send;
    let sizes: Vec<usize> = sent
        .iter()
        .map(|packet| seen(packet, CLIENT).payload.len())
        .collect();
    assert_eq!(sizes, [100, 100, 100]);
    // An MSS below the floor is refused by the codec before any state changes.
    syn.flags = SYN;
    syn.sequence = 7;
    syn.acknowledgment = 0;
    syn.source_port = 40001;
    syn.mss = Some(63);
    let tiny = tcp::build(CLIENT, SERVER, &syn, &[], MAX_MTU).unwrap();
    assert_eq!(
        server.ingest(G, T0, &tiny),
        Ingest::Segment(SegmentError::Mss)
    );
    assert_eq!(server.rejected(), 1);
    assert_eq!(server.flows(), 1);
}

#[test]
fn a_handshake_ack_must_match_the_syn_ack() {
    let mut server = stack(config(SERVER), &[SERVER_ISS]);
    let listener = server.listen(G, PORT).unwrap();
    let syn = forge((CLIENT, 40000), (SERVER, PORT), (100, 0, SYN), b"");
    assert_eq!(server.ingest(G, T0, &syn), Ingest::Handshake);
    server.poll(G, T0).unwrap();
    for wrong in [0, SERVER_ISS, SERVER_ISS + 2, 1234] {
        let ack = forge((CLIENT, 40000), (SERVER, PORT), (101, wrong, ACK), b"");
        assert_eq!(
            server.ingest(G, T0, &ack),
            Ingest::Dropped(Discard::Ack),
            "ack {wrong}"
        );
        let reset = server.poll(G, T0).unwrap().send;
        let reset = seen(&reset[0], CLIENT);
        assert_eq!(reset.flags, RST);
        assert_eq!(reset.sequence, wrong);
    }
    assert_eq!(server.accept(G, listener), Ok(None));
    // A repeated SYN is answered with another SYN-ACK, not a second flow.
    assert_eq!(server.ingest(G, T0, &syn), Ingest::Dropped(Discard::State));
    let again = server.poll(G, T0).unwrap().send;
    assert_eq!(seen(&again[0], CLIENT).flags, SYN | ACK);
    assert_eq!(server.flows(), 1);
    let ack = forge(
        (CLIENT, 40000),
        (SERVER, PORT),
        (101, SERVER_ISS + 1, ACK),
        b"",
    );
    assert_eq!(server.ingest(G, T0, &ack), Ingest::Delivered);
    assert!(server.accept(G, listener).unwrap().is_some());
}

#[test]
fn segments_for_no_flow_are_answered_with_a_reset() {
    let mut server = stack(config(SERVER), &[SERVER_ISS]);
    let from = (CLIENT, 40000);
    let to = (SERVER, PORT);
    // An ACK draws an RST at its own acknowledgment number.
    let ack = forge(from, to, (10, 777, ACK), b"data");
    assert_eq!(server.ingest(G, T0, &ack), Ingest::Answered);
    // Without ACK the RST acknowledges everything the segment occupied.
    let fin = forge(from, to, (10, 0, FIN), b"ab");
    assert_eq!(server.ingest(G, T0, &fin), Ingest::Answered);
    // A SYN to a port nobody listens on is refused too.
    let syn = forge(from, to, (500, 0, SYN), b"");
    assert_eq!(server.ingest(G, T0, &syn), Ingest::Answered);
    // An RST is never answered.
    let reset = forge(from, to, (10, 0, RST), b"");
    assert_eq!(
        server.ingest(G, T0, &reset),
        Ingest::Dropped(Discard::Reset)
    );
    let sent = server.poll(G, T0).unwrap().send;
    assert_eq!(sent.len(), 3);
    let first = seen(&sent[0], CLIENT);
    assert_eq!((first.flags, first.sequence), (RST, 777));
    let second = seen(&sent[1], CLIENT);
    assert_eq!(second.flags, RST | ACK);
    assert_eq!((second.sequence, second.acknowledgment), (0, 13));
    let third = seen(&sent[2], CLIENT);
    assert_eq!((third.flags, third.acknowledgment), (RST | ACK, 501));
    assert_eq!(server.flows(), 0);
}

#[test]
fn control_packets_are_bounded() {
    let mut server = stack(
        TcpConfig {
            outbox: 2,
            ..config(SERVER)
        },
        &[1],
    );
    for port in 0..5 {
        let ack = forge((CLIENT, 40000 + port), (SERVER, PORT), (1, 1, ACK), b"");
        assert_eq!(server.ingest(G, T0, &ack), Ingest::Answered);
    }
    assert_eq!(server.dropped(), 3);
    assert_eq!(server.poll(G, T0).unwrap().send.len(), 2);
}

#[test]
fn spoofed_segments_cannot_disturb_an_established_flow() {
    let (mut pair, client, _, _) = established();
    let now = pair.now;
    let rcv = SERVER_ISS + 1;
    let snd = CLIENT_ISS + 1;
    // An RST that is in the window but not exact only draws a challenge ACK.
    let near = from_server(rcv + 1, snd, RST | ACK, b"");
    assert_eq!(
        pair.a.ingest(G, now, &near),
        Ingest::Dropped(Discard::Reset)
    );
    // Far outside the window it is ignored.
    let far = from_server(rcv.wrapping_add(200_000), snd, RST | ACK, b"");
    assert_eq!(
        pair.a.ingest(G, now, &far),
        Ingest::Dropped(Discard::Window)
    );
    let data = from_server(rcv.wrapping_add(70_000), snd, ACK, b"x");
    assert_eq!(
        pair.a.ingest(G, now, &data),
        Ingest::Dropped(Discard::Window)
    );
    // An acknowledgment of bytes never sent is refused and carries no data.
    let ahead = from_server(rcv, snd + 500, ACK, b"evil");
    assert_eq!(pair.a.ingest(G, now, &ahead), Ingest::Dropped(Discard::Ack));
    // A SYN inside an established flow is a challenge, not a restart.
    let syn = from_server(rcv, snd, SYN | ACK, b"");
    assert_eq!(pair.a.ingest(G, now, &syn), Ingest::Dropped(Discard::State));
    // Wrong remote address or port reaches no flow.
    let wrong_port = forge((SERVER, 81), (CLIENT, CLIENT_PORT), (rcv, snd, ACK), b"x");
    assert_eq!(pair.a.ingest(G, now, &wrong_port), Ingest::Answered);
    let other = Ipv4Addr::new(203, 0, 113, 10);
    let wrong_host = forge((other, PORT), (CLIENT, CLIENT_PORT), (rcv, snd, ACK), b"x");
    assert_eq!(pair.a.ingest(G, now, &wrong_host), Ingest::Answered);
    assert_eq!(pair.a.state(G, client), Ok(State::Established));
    assert_eq!(read(&mut pair.a, client), b"");
    // The challenge and the resets leave once, and the flow is still usable.
    let sent = pair.a.poll(G, now).unwrap().send;
    assert!(sent.len() >= 3);
    for packet in sent {
        pair.b.ingest(G, now, &packet);
    }
    // In-window data with the right numbers is delivered: that is what sequence
    // numbers are, and why they come from entropy.
    let good = from_server(rcv, snd, ACK, b"ok");
    assert_eq!(pair.a.ingest(G, now, &good), Ingest::Delivered);
    assert_eq!(read(&mut pair.a, client), b"ok");
    // Only the exact RST ends the flow.
    let exact = from_server(rcv + 2, snd, RST | ACK, b"");
    assert_eq!(pair.a.ingest(G, now, &exact), Ingest::Reset);
    assert_eq!(pair.a.failure(G, client), Ok(Some(Failure::Reset)));
    assert_eq!(
        pair.a.send(G, client, b"x"),
        Err(TcpError::Failed(Failure::Reset))
    );
    let after = from_server(rcv + 2, snd, ACK, b"x");
    assert_eq!(pair.a.ingest(G, now, &after), Ingest::Answered);
}

#[test]
fn invalid_datagrams_are_counted_and_rejected() {
    let (mut pair, client, _, _) = established();
    let now = pair.now;
    let good = from_server(SERVER_ISS + 1, CLIENT_ISS + 1, ACK, b"abc");
    let mut flipped = good.clone();
    flipped[35] ^= 1;
    assert_eq!(
        pair.a.ingest(G, now, &flipped),
        Ingest::Segment(SegmentError::Checksum)
    );
    assert!(matches!(
        pair.a.ingest(G, now, &good[..10]),
        Ingest::Ip(PacketError::Truncated)
    ));
    let udp = crate::netstack::build_udp(SERVER, CLIENT, 53, 40000, b"x", MAX_MTU).unwrap();
    assert_eq!(
        pair.a.ingest(G, now, &udp),
        Ingest::Ip(PacketError::Protocol(17))
    );
    let elsewhere = tcp::build(
        SERVER,
        Ipv4Addr::new(10, 64, 0, 3),
        &Header {
            source_port: PORT,
            destination_port: CLIENT_PORT,
            sequence: 1,
            acknowledgment: 1,
            flags: ACK,
            window: 1,
            mss: None,
        },
        b"",
        MAX_MTU,
    )
    .unwrap();
    assert_eq!(
        pair.a.ingest(G, now, &elsewhere),
        Ingest::Ip(PacketError::Destination)
    );
    assert_eq!(pair.a.rejected(), 4);
    assert_eq!(read(&mut pair.a, client), b"");
    assert_eq!(pair.a.state(G, client), Ok(State::Established));
}

#[test]
fn entropy_failure_creates_no_flow() {
    let mut client = TcpStack::new(config(CLIENT), Failing, G, T0).unwrap();
    assert_eq!(
        client.connect(G, T0, SERVER, PORT).err(),
        Some(TcpError::Entropy("entropy failed".into()))
    );
    assert_eq!(client.flows(), 0);
    let mut server = TcpStack::new(config(SERVER), Failing, G, T0).unwrap();
    server.listen(G, PORT).unwrap();
    let syn = forge((CLIENT, 40000), (SERVER, PORT), (1, 0, SYN), b"");
    assert_eq!(
        server.ingest(G, T0, &syn),
        Ingest::Dropped(Discard::Entropy)
    );
    assert_eq!(server.flows(), 0);
}

#[test]
fn ephemeral_ports_are_bounded() {
    let mut client = stack(config(CLIENT), &[0]);
    client.connect(G, T0, SERVER, PORT).unwrap();
    // The draw of zero is taken for the same remote: the retries are bounded.
    assert_eq!(
        client.connect(G, T0, SERVER, PORT).err(),
        Some(TcpError::PortsExhausted)
    );
    assert_eq!(client.entropy.calls, 1 + 1 + PORT_ATTEMPTS);
    // A different remote may share the local port.
    assert!(
        client
            .connect(G, T0, Ipv4Addr::new(203, 0, 113, 10), PORT)
            .is_ok()
    );
}

#[test]
fn a_new_generation_revokes_everything() {
    let (mut pair, client, _, listener) = established();
    let packet = from_server(SERVER_ISS + 1, CLIENT_ISS + 1, ACK, b"x");
    assert_eq!(pair.a.ingest(G + 1, pair.now, &packet), Ingest::Revoked);
    assert_eq!(pair.a.flows(), 0);
    assert_eq!(pair.a.send(G, client, b"x"), Err(TcpError::Revoked));
    assert_eq!(pair.a.poll(G, pair.now).err(), Some(TcpError::Revoked));
    assert_eq!(
        pair.a.connect(G, pair.now, SERVER, PORT).err(),
        Some(TcpError::Revoked)
    );
    // The other stack is unaffected until its own fence trips.
    assert_eq!(pair.b.accept(G, listener), Ok(None));
    assert_eq!(pair.b.accept(G + 1, listener), Err(TcpError::Revoked));
    assert_eq!(pair.b.listeners(), 0);
}

#[test]
fn a_clock_moving_backwards_revokes_and_revoke_is_final() {
    let (mut pair, client, server, _) = established();
    pair.now = Duration::from_secs(5);
    pair.pump();
    assert_eq!(
        pair.a.poll(G, Duration::from_secs(4)).err(),
        Some(TcpError::Revoked)
    );
    assert_eq!(pair.a.flows(), 0);
    assert_eq!(pair.a.state(G, client), Err(TcpError::Revoked));
    pair.b.revoke();
    assert_eq!(pair.b.flows(), 0);
    assert_eq!(pair.b.buffered(), 0);
    assert_eq!(
        pair.b.recv(G, server, &mut [0u8; 4]),
        Err(TcpError::Revoked)
    );
    assert_eq!(pair.b.listen(G, 81).err(), Some(TcpError::Revoked));
}

#[test]
fn a_standing_clock_is_allowed() {
    let (mut pair, client, _, _) = established();
    pair.a.send(G, client, b"x").unwrap();
    assert!(pair.a.poll(G, pair.now).is_ok());
    assert!(pair.a.poll(G, pair.now).is_ok());
    assert_eq!(pair.a.state(G, client), Ok(State::Established));
}
