use super::*;
use crate::json::Value;
use crate::netstack::support::{Failing, Script, binary, fixtures, icmp_unreachable, number, text};
use crate::netstack::{
    Delivery, EPHEMERAL_START, IcmpEffect, IcmpIngest, Ingest, Limits, MAX_MTU, build_udp,
};
use std::sync::OnceLock;

const G: u64 = 3;
const LOCAL: Ipv4Addr = Ipv4Addr::new(10, 64, 0, 2);
const R1: Ipv4Addr = Ipv4Addr::new(10, 2, 0, 1);
const R2: Ipv4Addr = Ipv4Addr::new(10, 2, 0, 2);
const NAME: &str = "tracker.example.net";
const TXID: u16 = 0x1A2B;

fn secs(value: u64) -> Duration {
    Duration::from_secs(value)
}

fn vectors() -> &'static Value {
    static VECTORS: OnceLock<Value> = OnceLock::new();
    VECTORS.get_or_init(fixtures)
}

/// The reference reply with this case name.
fn vector(case: &str) -> Vec<u8> {
    let rows = vectors().get("dns").unwrap().get("responses").unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| text(row, "case") == case)
        .unwrap();
    binary(row, "message")
}

fn ip() -> Ipv4Config {
    Ipv4Config::new(LOCAL, Ipv4Addr::UNSPECIFIED, 0).unwrap()
}

fn config(resolvers: &[Ipv4Addr]) -> DnsConfig {
    DnsConfig::new(ip(), resolvers.to_vec(), MAX_MTU)
}

struct Rig {
    client: DnsClient<Script>,
    sockets: UdpSockets,
    tcp: TcpStack<Script>,
}

impl Rig {
    fn new(values: &[u16], config: DnsConfig) -> Self {
        Self {
            client: DnsClient::new(config, Script::new(values), G, Duration::ZERO).unwrap(),
            sockets: UdpSockets::new(Limits::default(), G),
            tcp: TcpStack::new(
                crate::netstack::TcpConfig::new(ip(), MAX_MTU),
                Script::new(&[123]),
                G,
                Duration::ZERO,
            )
            .unwrap(),
        }
    }

    fn one(values: &[u16]) -> Self {
        Self::new(values, config(&[R1]))
    }

    fn start(&mut self, name: &str, now: Duration) -> (QueryId, Vec<u8>) {
        self.client.start(&mut self.sockets, G, name, now).unwrap()
    }

    fn poll(&mut self, now: Duration) -> Poll {
        self.client
            .poll(&mut self.sockets, &mut self.tcp, G, now)
            .unwrap()
    }

    /// Answers one sent query packet with ICMP port unreachable from its resolver.
    fn refuse(&mut self, packet: &[u8]) -> IcmpIngest {
        let advice = icmp_unreachable(destination(packet), packet, 3, 0);
        self.sockets.ingest_icmp(G, &ip(), &advice)
    }

    /// Delivers one reply datagram to the virtual UDP port.
    fn feed(&mut self, resolver: Ipv4Addr, from_port: u16, to_port: u16, message: &[u8]) -> Ingest {
        let packet = build_udp(resolver, LOCAL, from_port, to_port, message, MAX_MTU).unwrap();
        self.sockets.ingest(G, &ip(), &packet)
    }
}

fn destination(packet: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19])
}

fn source_port(packet: &[u8]) -> u16 {
    u16::from_be_bytes([packet[20], packet[21]])
}

fn dns_part(packet: &[u8]) -> &[u8] {
    &packet[28..]
}

fn queued() -> Ingest {
    Ingest::Delivered(Delivery::Queued)
}

fn filtered() -> Ingest {
    Ingest::Delivered(Delivery::Filtered)
}

fn answer(addresses: &[Ipv4Addr], ttl: Duration) -> Result<Answer, Failure> {
    Ok(Answer {
        addresses: addresses.to_vec(),
        ttl,
    })
}

fn wire(name: &str) -> Vec<u8> {
    let mut wire = Vec::new();
    for label in name.split('.') {
        wire.push(label.len() as u8);
        wire.extend_from_slice(label.as_bytes());
    }
    wire.push(0);
    wire
}

fn record(owner: &[u8], kind: u16, ttl: u32, data: &[u8]) -> Vec<u8> {
    let mut record = owner.to_vec();
    record.extend_from_slice(&kind.to_be_bytes());
    record.extend_from_slice(&[0, 1]);
    record.extend_from_slice(&ttl.to_be_bytes());
    record.extend_from_slice(&(data.len() as u16).to_be_bytes());
    record.extend_from_slice(data);
    record
}

/// A NOERROR reply to the standard question with the given answer records.
fn reply(count: u16, answers: &[Vec<u8>]) -> Vec<u8> {
    let mut message = TXID.to_be_bytes().to_vec();
    message.extend_from_slice(&[0x81, 0x80, 0, 1]);
    message.extend_from_slice(&count.to_be_bytes());
    message.extend_from_slice(&[0; 4]);
    message.extend_from_slice(&wire(NAME));
    message.extend_from_slice(&[0, 1, 0, 1]);
    answers
        .iter()
        .for_each(|record| message.extend_from_slice(record));
    message
}

const POINTER: [u8; 2] = [0xc0, 12];

fn parse(message: &[u8]) -> Result<Reply, ReplyError> {
    let question = encode_name(NAME).unwrap().to_ascii_lowercase();
    parse_response(TXID, &question, message)
}

#[test]
fn independent_query_vectors() {
    let rows = vectors().get("dns").unwrap().get("queries").unwrap();
    for row in rows.as_array().unwrap() {
        let id = number(row, "id") as u16;
        let mut rig = Rig::one(&[id, 0]);
        let (_, packet) = rig.start(text(row, "name"), Duration::ZERO);
        assert_eq!(dns_part(&packet), binary(row, "message"));
        assert_eq!(destination(&packet), R1);
        assert_eq!(source_port(&packet), EPHEMERAL_START);
        assert_eq!(&packet[22..24], &[0, 53]);
    }
}

#[test]
fn independent_reply_vectors() {
    let rows = vectors().get("dns").unwrap().get("responses").unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 27);
    for row in rows {
        let case = text(row, "case");
        let question = encode_name(text(row, "name")).unwrap().to_ascii_lowercase();
        let result = parse_response(number(row, "id") as u16, &question, &binary(row, "message"));
        match text(row, "expect") {
            "answers" => {
                let addresses = row.get("addresses").unwrap().as_array().unwrap();
                let addresses: Vec<Ipv4Addr> = addresses
                    .iter()
                    .map(|item| item.as_str().unwrap().parse().unwrap())
                    .collect();
                let ttl = number(row, "ttl") as u32;
                assert_eq!(result, Ok(Reply::Addresses { addresses, ttl }), "{case}");
            }
            "no_address" => assert_eq!(result, Ok(Reply::NoAddress), "{case}"),
            "name_error" => assert_eq!(result, Ok(Reply::NameError), "{case}"),
            "truncated" => assert_eq!(result, Ok(Reply::Truncated), "{case}"),
            "failure" => {
                let code = number(row, "code") as u8;
                assert_eq!(result, Ok(Reply::Failure(code)), "{case}");
            }
            "reject" => assert!(result.is_err(), "{case}: {result:?}"),
            other => panic!("unknown expectation {other}"),
        }
    }
}

#[test]
fn specific_rejection_reasons() {
    let a = |ip: [u8; 4]| record(&POINTER, TYPE_A, 30, &ip);
    let cases = [
        ("aaaa", vector("aaaa answer"), ReplyError::Type),
        ("class", vector("wrong class"), ReplyError::Class),
        ("self pointer", vector("self pointer"), ReplyError::Pointer),
        (
            "forward pointer",
            vector("forward pointer"),
            ReplyError::Pointer,
        ),
        (
            "reserved label",
            vector("reserved label"),
            ReplyError::Label,
        ),
        ("rdlength", vector("rdlength five"), ReplyError::Rdata),
        ("overrun", vector("rdata overrun"), ReplyError::Truncated),
        ("id", vector("other id"), ReplyError::Id),
        ("question", vector("other question"), ReplyError::Question),
        ("qtype", vector("wrong qtype"), ReplyError::Question),
        ("no question", vector("no question"), ReplyError::Question),
        ("echoed query", vector("query echoed"), ReplyError::Header),
        ("short", vector("short header"), ReplyError::Truncated),
        ("count", reply(33, &[]), ReplyError::Records),
        (
            "opcode",
            {
                let mut message = reply(1, &[a([192, 0, 2, 1])]);
                message[2] = 0x89;
                message
            },
            ReplyError::Header,
        ),
    ];
    for (label, message, expected) in cases {
        assert_eq!(parse(&message), Err(expected), "{label}");
    }
}

#[test]
fn answers_are_bounded_and_deduplicated() {
    let many: Vec<Vec<u8>> = (1..=12)
        .map(|index| record(&POINTER, TYPE_A, 30 + index, &[192, 0, 2, index as u8]))
        .collect();
    let Ok(Reply::Addresses { addresses, ttl }) = parse(&reply(12, &many)) else {
        panic!("expected addresses");
    };
    assert_eq!(addresses.len(), MAX_ADDRESSES);
    assert_eq!(addresses[0], Ipv4Addr::new(192, 0, 2, 1));
    assert_eq!(ttl, 31);
    let one = record(&POINTER, TYPE_A, 30, &[192, 0, 2, 1]);
    let twice = [one.clone(), one];
    let Ok(Reply::Addresses { addresses, .. }) = parse(&reply(2, &twice)) else {
        panic!("expected addresses");
    };
    assert_eq!(addresses.len(), 1);
    for special in [
        [0, 0, 0, 0],
        [127, 0, 0, 1],
        [224, 0, 0, 1],
        [255, 255, 255, 255],
    ] {
        let only = [record(&POINTER, TYPE_A, 30, &special)];
        assert_eq!(parse(&reply(1, &only)), Ok(Reply::NoAddress));
    }
}

#[test]
fn cname_chains_are_followed_but_bounded() {
    let first = wire("one.example.net");
    let second = wire("two.example.net");
    let cname = |owner: &[u8], target: &[u8]| record(owner, TYPE_CNAME, 20, target);
    let chain = [
        cname(&POINTER, &first),
        cname(&first, &second),
        record(&second, TYPE_A, 40, &[198, 51, 100, 1]),
    ];
    assert_eq!(
        parse(&reply(3, &chain)),
        Ok(Reply::Addresses {
            addresses: vec![Ipv4Addr::new(198, 51, 100, 1)],
            ttl: 20
        })
    );
    // A record for a name outside the chain, or before its CNAME, is not used.
    let outside = [
        cname(&POINTER, &first),
        record(&second, TYPE_A, 40, &[198, 51, 100, 1]),
    ];
    assert_eq!(parse(&reply(2, &outside)), Ok(Reply::NoAddress));
    let unrelated = [
        cname(&first, &second),
        record(&second, TYPE_A, 9, &[198, 51, 100, 2]),
    ];
    assert_eq!(parse(&reply(2, &unrelated)), Ok(Reply::NoAddress));
    let mut long = Vec::new();
    let mut owner = POINTER.to_vec();
    for index in 0..9 {
        let target = wire(&format!("hop{index}.example.net"));
        long.push(cname(&owner, &target));
        owner = target;
    }
    assert_eq!(parse(&reply(9, &long)), Err(ReplyError::Records));
    // The target must fill the record data exactly.
    let mut padded = first.clone();
    padded.push(0);
    assert_eq!(
        parse(&reply(1, &[cname(&POINTER, &padded)])),
        Err(ReplyError::Rdata)
    );
}

#[test]
fn name_compression_traps_are_refused() {
    // Twenty chained backward pointers exceed the pointer budget.
    let mut message = vec![0u8; 12];
    message.push(0);
    for index in 0..20usize {
        let target = if index == 0 { 12 } else { 13 + 2 * (index - 1) };
        message.push(0xc0 | (target >> 8) as u8);
        message.push(target as u8);
    }
    let start = message.len() - 2;
    assert_eq!(read_name(&message, start), Err(ReplyError::Pointer));
    // Four hops stay within it and resolve to the root.
    let short = 13 + 2 * 3;
    assert_eq!(read_name(&message, short), Ok((vec![0], short + 2)));
    // Pointers to themselves, forward and in a two-pointer cycle.
    let cycle = [0, 0, 0xc0, 4, 0xc0, 2];
    assert_eq!(read_name(&cycle, 2), Err(ReplyError::Pointer));
    assert_eq!(read_name(&cycle, 4), Err(ReplyError::Pointer));
    assert_eq!(read_name(&[0xc0, 0], 0), Err(ReplyError::Pointer));
    // Truncated pointer, label overruns, reserved label types and long names.
    assert_eq!(read_name(&[0xc0], 0), Err(ReplyError::Truncated));
    assert_eq!(read_name(&[], 0), Err(ReplyError::Truncated));
    assert_eq!(read_name(&[5, b'a', b'b'], 0), Err(ReplyError::Label));
    assert_eq!(
        read_name(&[3, b'a', b'b', b'c'], 0),
        Err(ReplyError::Truncated)
    );
    for reserved in [0x40u8, 0x80] {
        assert_eq!(read_name(&[reserved, 0], 0), Err(ReplyError::Label));
    }
    let mut long = Vec::new();
    for _ in 0..4 {
        long.push(63);
        long.extend_from_slice(&[b'a'; 63]);
    }
    long.push(0);
    assert_eq!(read_name(&long, 0), Err(ReplyError::Name));
    long.truncate(3 * 64);
    long.push(0);
    assert_eq!(read_name(&long, 0).unwrap().0.len(), 3 * 64 + 1);
    // Case folds, and the reported offset follows the first encoding only.
    let mixed = [3, b'x', b'Y', b'z', 0, 3, b'W', b'w', b'W', 0xc0, 0];
    let expected = vec![3, b'w', b'w', b'w', 3, b'x', b'y', b'z', 0];
    assert_eq!(read_name(&mixed, 5), Ok((expected, 11)));
}

#[test]
fn resolves_through_virtual_udp() {
    let mut rig = Rig::one(&[TXID, 0]);
    let (id, packet) = rig.start(NAME, secs(1));
    assert_eq!(rig.client.pending(), 1);
    let port = source_port(&packet);
    assert_eq!(rig.feed(R1, 53, port, &vector("single")), queued());
    let poll = rig.poll(secs(1));
    let expected = Finished {
        id,
        result: answer(&[Ipv4Addr::new(192, 0, 2, 10)], secs(300)),
    };
    assert_eq!(poll.finished, vec![expected]);
    assert!(poll.send.is_empty());
    assert_eq!(poll.next, None);
    assert_eq!(rig.client.pending(), 0);
    assert!(rig.sockets.is_empty());
}

#[test]
fn answer_ttl_is_capped() {
    let mut capped = config(&[R1]);
    capped.max_ttl = secs(100);
    let mut rig = Rig::new(&[TXID, 0], capped);
    let (_, packet) = rig.start(NAME, Duration::ZERO);
    rig.feed(R1, 53, source_port(&packet), &vector("single"));
    let poll = rig.poll(Duration::ZERO);
    assert_eq!(poll.finished[0].result.as_ref().unwrap().ttl, secs(100));
}

#[test]
fn spoofed_replies_never_complete_a_query() {
    let mut rig = Rig::one(&[TXID, 0]);
    let (id, packet) = rig.start(NAME, Duration::ZERO);
    let port = source_port(&packet);
    let stranger = Ipv4Addr::new(203, 0, 113, 9);
    assert_eq!(rig.feed(stranger, 53, port, &vector("single")), filtered());
    assert_eq!(rig.feed(R1, 5353, port, &vector("single")), filtered());
    assert_eq!(
        rig.feed(R1, 53, port.wrapping_add(1), &vector("single")),
        Ingest::Delivered(Delivery::NoSocket)
    );
    for case in ["other id", "other question", "aaaa answer", "query echoed"] {
        assert_eq!(rig.feed(R1, 53, port, &vector(case)), queued(), "{case}");
    }
    let poll = rig.poll(Duration::ZERO);
    assert!(poll.finished.is_empty());
    assert_eq!(rig.client.rejected(), 4);
    assert_eq!(rig.client.pending(), 1);
    rig.feed(R1, 53, port, &vector("single"));
    let poll = rig.poll(Duration::ZERO);
    assert_eq!(poll.finished.len(), 1);
    assert_eq!(poll.finished[0].id, id);
    assert!(poll.finished[0].result.is_ok());
}

#[test]
fn retries_rotate_resolvers_until_the_scripted_timeout() {
    let mut rig = Rig::new(&[TXID, 0], config(&[R1, R2]));
    let (id, first) = rig.start(NAME, Duration::ZERO);
    assert_eq!(destination(&first), R1);
    let port = source_port(&first);
    let poll = rig.poll(secs(1));
    assert!(poll.send.is_empty() && poll.finished.is_empty());
    assert_eq!(poll.next, Some(secs(2)));
    let poll = rig.poll(secs(2));
    assert_eq!(poll.send.len(), 1);
    assert_eq!(destination(&poll.send[0]), R2);
    assert_eq!(dns_part(&poll.send[0]), dns_part(&first));
    assert_eq!(source_port(&poll.send[0]), port);
    assert_eq!(poll.next, Some(secs(4)));
    // The first resolver is no longer accepted once the query moved on.
    assert_eq!(rig.feed(R1, 53, port, &vector("single")), filtered());
    assert!(rig.poll(secs(3)).send.is_empty());
    let poll = rig.poll(secs(4));
    assert_eq!(destination(&poll.send[0]), R1);
    assert_eq!(poll.next, Some(secs(6)));
    assert!(rig.poll(secs(5)).finished.is_empty());
    let poll = rig.poll(secs(6));
    assert!(poll.send.is_empty());
    assert_eq!(
        poll.finished,
        vec![Finished {
            id,
            result: Err(Failure::Timeout)
        }]
    );
    assert_eq!(poll.next, None);
    assert!(rig.sockets.is_empty());
    assert_eq!(rig.client.pending(), 0);
}

#[test]
fn an_unreachable_resolver_fails_over_to_the_next_at_once() {
    let applied = IcmpIngest::Applied(IcmpEffect::Unreachable(3));
    let mut rig = Rig::new(&[TXID, 0], config(&[R1, R2]));
    let (id, first) = rig.start(NAME, Duration::ZERO);
    assert_eq!(rig.refuse(&first), applied);
    let poll = rig.poll(secs(1));
    assert!(poll.finished.is_empty());
    assert_eq!(poll.send.len(), 1);
    assert_eq!(destination(&poll.send[0]), R2);
    assert_eq!(dns_part(&poll.send[0]), dns_part(&first));
    assert_eq!(poll.next, Some(secs(3)));
    rig.feed(R2, 53, source_port(&first), &vector("single"));
    let poll = rig.poll(secs(1));
    assert_eq!(poll.finished.len(), 1);
    assert_eq!(poll.finished[0].id, id);
    assert!(poll.finished[0].result.is_ok());
    assert!(rig.sockets.is_empty());
}

#[test]
fn only_an_unreachable_last_attempt_ends_the_query_as_unreachable() {
    let applied = IcmpIngest::Applied(IcmpEffect::Unreachable(3));
    // The last attempt is refused: the query ends at once with the ICMP code.
    let mut rig = Rig::new(&[TXID, 0], config(&[R1, R2]));
    let (id, first) = rig.start(NAME, Duration::ZERO);
    rig.refuse(&first);
    let second = rig.poll(Duration::ZERO).send.remove(0);
    assert_eq!(destination(&second), R2);
    let third = rig.poll(secs(2)).send.remove(0);
    assert_eq!(destination(&third), R1);
    assert_eq!(rig.refuse(&third), applied);
    let poll = rig.poll(secs(2));
    assert!(poll.send.is_empty());
    assert_eq!(
        poll.finished,
        vec![Finished {
            id,
            result: Err(Failure::Unreachable(3))
        }]
    );
    assert!(rig.sockets.is_empty());

    // An earlier refusal only spends its attempt; a silent last one times out.
    let mut rig = Rig::one(&[TXID, 0]);
    let (id, first) = rig.start(NAME, Duration::ZERO);
    assert_eq!(rig.refuse(&first), applied);
    let poll = rig.poll(secs(1));
    assert_eq!(destination(&poll.send[0]), R1);
    assert_eq!(poll.next, Some(secs(3)));
    assert_eq!(rig.poll(secs(3)).send.len(), 1);
    let poll = rig.poll(secs(5));
    assert!(poll.send.is_empty());
    assert_eq!(
        poll.finished,
        vec![Finished {
            id,
            result: Err(Failure::Timeout)
        }]
    );
    assert!(rig.sockets.is_empty());
}

#[test]
fn a_reply_after_rotation_from_the_new_resolver_is_accepted() {
    let mut rig = Rig::new(&[TXID, 0], config(&[R1, R2]));
    let (_, first) = rig.start(NAME, Duration::ZERO);
    rig.poll(secs(2));
    assert_eq!(
        rig.feed(R2, 53, source_port(&first), &vector("single")),
        queued()
    );
    let poll = rig.poll(secs(2));
    assert!(poll.finished[0].result.is_ok());
}

#[test]
fn server_failures_are_reported_explicitly() {
    let cases = [
        ("nxdomain", Failure::NameError),
        ("servfail", Failure::ServerFailure(2)),
        ("no address", Failure::NoAddress),
    ];
    for (case, failure) in cases {
        let mut rig = Rig::one(&[TXID, 0]);
        let (_, packet) = rig.start(NAME, Duration::ZERO);
        rig.feed(R1, 53, source_port(&packet), &vector(case));
        let poll = rig.poll(Duration::ZERO);
        assert_eq!(poll.finished[0].result, Err(failure), "{case}");
        assert!(rig.sockets.is_empty());
    }
}

#[test]
fn pending_queries_and_transactions_are_bounded() {
    let mut limited = config(&[R1]);
    limited.max_pending = 2;
    let mut rig = Rig::new(&[1, 100, 2, 200, 3, 300], limited);
    rig.start("a.example", Duration::ZERO);
    rig.start("b.example", Duration::ZERO);
    let third = rig
        .client
        .start(&mut rig.sockets, G, "c.example", Duration::ZERO);
    assert_eq!(third, Err(DnsError::Busy));
    assert_eq!((rig.client.pending(), rig.sockets.len()), (2, 2));
    // A source that only ever repeats one transaction id cannot start a second.
    let mut rig = Rig::one(&[5]);
    rig.start("a.example", Duration::ZERO);
    let second = rig
        .client
        .start(&mut rig.sockets, G, "b.example", Duration::ZERO);
    assert_eq!(second, Err(DnsError::Busy));
    assert_eq!((rig.client.pending(), rig.sockets.len()), (1, 1));
}

#[test]
fn names_are_validated_before_any_work() {
    let label = |length: usize| "a".repeat(length);
    let longest = [label(63), label(63), label(63), label(61)].join(".");
    let too_long = [label(63), label(63), label(63), label(62)].join(".");
    assert_eq!(longest.len(), MAX_NAME);
    let invalid = [
        String::new(),
        ".".into(),
        "a..b".into(),
        ".a".into(),
        "a.b..".into(),
        label(64),
        too_long,
        "exa mple.com".into(),
        "caf\u{e9}.example".into(),
        "a\0b".into(),
        "a/b".into(),
        "a@b".into(),
    ];
    for name in invalid {
        let mut rig = Rig::one(&[TXID, 0]);
        let started = rig.client.start(&mut rig.sockets, G, &name, Duration::ZERO);
        assert_eq!(started, Err(DnsError::BadName), "{name:?}");
        assert!(rig.sockets.is_empty());
    }
    for name in [
        &longest[..],
        "x.example.",
        "a_b.Example-1.NET",
        "1.2.3.4",
        "localhost",
    ] {
        let mut rig = Rig::one(&[TXID, 0]);
        let (_, packet) = rig.start(name, Duration::ZERO);
        assert!(packet.len() <= 28 + 12 + 255 + 4);
    }
}

#[test]
fn failures_do_not_fall_back_or_leak_sockets() {
    let mut client = DnsClient::new(config(&[R1]), Failing, G, Duration::ZERO).unwrap();
    let mut sockets = UdpSockets::new(Limits::default(), G);
    let started = client.start(&mut sockets, G, NAME, Duration::ZERO);
    assert_eq!(started, Err(DnsError::Entropy("entropy failed".into())));
    assert_eq!((client.pending(), sockets.len()), (0, 0));
    let tiny = Limits {
        sockets: 1,
        ..Limits::default()
    };
    let mut rig = Rig::one(&[1, 100, 2, 200]);
    rig.sockets = UdpSockets::new(tiny, G);
    rig.start("a.example", Duration::ZERO);
    let full = rig
        .client
        .start(&mut rig.sockets, G, "b.example", Duration::ZERO);
    assert_eq!(full, Err(DnsError::Socket(SocketError::TableFull)));
    assert_eq!(rig.client.pending(), 1);
}

#[test]
fn generation_clock_and_table_revocation_are_irreversible() {
    let mut rig = Rig::one(&[TXID, 0]);
    rig.start(NAME, secs(5));
    let stale = rig
        .client
        .poll(&mut rig.sockets, &mut rig.tcp, G + 1, secs(5));
    assert_eq!(stale.unwrap_err(), DnsError::Revoked);
    assert_eq!(rig.client.pending(), 0);
    let again = rig.client.poll(&mut rig.sockets, &mut rig.tcp, G, secs(5));
    assert_eq!(again.unwrap_err(), DnsError::Revoked);
    let started = rig.client.start(&mut rig.sockets, G, NAME, secs(5));
    assert_eq!(started, Err(DnsError::Revoked));

    let mut rig = Rig::one(&[TXID, 0]);
    rig.start(NAME, secs(5));
    let backwards = rig.client.poll(&mut rig.sockets, &mut rig.tcp, G, secs(4));
    assert_eq!(backwards.unwrap_err(), DnsError::Revoked);
    assert_eq!(rig.client.pending(), 0);

    let mut rig = Rig::one(&[TXID, 0]);
    let (id, _) = rig.start(NAME, secs(5));
    rig.sockets.revoke();
    let revoked = rig.client.poll(&mut rig.sockets, &mut rig.tcp, G, secs(5));
    assert_eq!(revoked.unwrap_err(), DnsError::Revoked);
    let cancel = rig.client.cancel(&mut rig.sockets, &mut rig.tcp, G, id);
    assert_eq!(cancel, Err(DnsError::Revoked));
    let started = rig.client.start(&mut rig.sockets, G, NAME, secs(5));
    assert_eq!(started, Err(DnsError::Revoked));
}

#[test]
fn cancel_frees_the_socket_and_is_idempotent() {
    let mut rig = Rig::one(&[TXID, 0]);
    let (id, _) = rig.start(NAME, Duration::ZERO);
    assert_eq!(
        rig.client.cancel(&mut rig.sockets, &mut rig.tcp, G, id),
        Ok(true)
    );
    assert_eq!(
        rig.client.cancel(&mut rig.sockets, &mut rig.tcp, G, id),
        Ok(false)
    );
    assert!(rig.sockets.is_empty());
    let poll = rig.poll(secs(100));
    assert_eq!(poll, Poll::default());
    let stale = rig.client.cancel(&mut rig.sockets, &mut rig.tcp, G + 1, id);
    assert_eq!(stale, Err(DnsError::Revoked));
}

#[test]
fn configuration_is_validated() {
    let too_many = vec![
        R1,
        R2,
        Ipv4Addr::new(10, 2, 0, 3),
        Ipv4Addr::new(10, 2, 0, 4),
    ];
    for edit in 0..14 {
        let mut config = config(&[R1, R2]);
        match edit {
            0 => {}
            1 => config.resolvers.clear(),
            2 => config.resolvers = vec![R1; 2],
            3 => config.resolvers = too_many.clone(),
            4 => config.resolvers = vec![Ipv4Addr::UNSPECIFIED],
            5 => config.resolvers = vec![Ipv4Addr::LOCALHOST],
            6 => config.mtu = 100,
            7 => config.mtu = MAX_MTU + 1,
            8 => config.attempts = 0,
            9 => config.attempts = 9,
            10 => config.max_pending = 0,
            11 => config.max_pending = 65,
            12 => config.retry_interval = Duration::ZERO,
            _ => config.retry_interval = secs(61),
        }
        let client = DnsClient::new(config, Script::new(&[0]), G, Duration::ZERO);
        let expected = (edit != 0).then_some(DnsError::BadConfig);
        assert_eq!(client.err(), expected, "edit {edit}");
    }
}

fn pump_tcp_dns(rig: &mut Rig, server: &mut TcpStack<Script>, now: Duration) -> Poll {
    let mut result = Poll::default();
    for _ in 0..16 {
        let dns = rig.poll(now);
        result.finished.extend(dns.finished);
        result.next = dns.next;
        let outbound = rig.tcp.poll(G, now).unwrap();
        let inbound = server.poll(G, now).unwrap();
        let quiet = outbound.send.is_empty() && inbound.send.is_empty();
        for packet in outbound.send {
            server.ingest(G, now, &packet);
        }
        for packet in inbound.send {
            rig.tcp.ingest(G, now, &packet);
        }
        if quiet {
            break;
        }
    }
    result
}

fn fallback() -> (Rig, TcpStack<Script>, FlowHandle, QueryId) {
    let mut rig = Rig::one(&[TXID, 0]);
    let (id, packet) = rig.start(NAME, Duration::ZERO);
    rig.feed(R1, 53, source_port(&packet), &vector("truncated"));
    let poll = rig.poll(Duration::ZERO);
    assert!(poll.finished.is_empty());
    assert_eq!(rig.client.pending(), 1);
    let server_ip = Ipv4Config::new(R1, Ipv4Addr::UNSPECIFIED, 0).unwrap();
    let mut server = TcpStack::new(
        crate::netstack::TcpConfig::new(server_ip, MAX_MTU),
        Script::new(&[456]),
        G,
        Duration::ZERO,
    )
    .unwrap();
    let listener = server.listen(G, 53).unwrap();
    assert!(
        pump_tcp_dns(&mut rig, &mut server, Duration::ZERO)
            .finished
            .is_empty()
    );
    let flow = server.accept(G, listener).unwrap().unwrap();
    let mut request = [0; 512];
    let Read::Data(length) = server.recv(G, flow, &mut request).unwrap() else {
        panic!("missing TCP query")
    };
    assert_eq!(be16(&request) as usize, length - 2);
    assert_eq!(&request[2..length], dns_part(&packet));
    (rig, server, flow, id)
}

fn frame(message: &[u8]) -> Vec<u8> {
    let mut frame = (message.len() as u16).to_be_bytes().to_vec();
    frame.extend_from_slice(message);
    frame
}

#[test]
fn truncated_udp_uses_virtual_tcp_with_split_prefix_and_body() {
    let (mut rig, mut server, flow, id) = fallback();
    let response = frame(&vector("single"));
    for (index, byte) in response.iter().enumerate() {
        assert_eq!(server.send(G, flow, &[*byte]), Ok(1));
        let poll = pump_tcp_dns(&mut rig, &mut server, Duration::ZERO);
        if index + 1 == response.len() {
            assert_eq!(poll.finished.len(), 1);
            assert_eq!(poll.finished[0].id, id);
            assert!(poll.finished[0].result.is_ok());
        } else {
            assert!(poll.finished.is_empty());
        }
    }
    assert_eq!(
        (rig.client.pending(), rig.sockets.len(), rig.tcp.flows()),
        (0, 0, 0)
    );
}

#[test]
fn tcp_spoofed_and_malformed_frames_never_finish_the_query() {
    let (mut rig, mut server, flow, _) = fallback();
    let mut wrong_id = vector("single");
    wrong_id[0] ^= 1;
    let mut wrong_question = vector("single");
    wrong_question[13] ^= 1;
    for invalid in [wrong_id, wrong_question, vector("pointer loop"), vec![]] {
        server.send(G, flow, &frame(&invalid)).unwrap();
        assert!(
            pump_tcp_dns(&mut rig, &mut server, Duration::ZERO)
                .finished
                .is_empty()
        );
        assert_eq!(rig.client.pending(), 1);
    }
    assert_eq!(rig.client.rejected(), 4);
    server.send(G, flow, &frame(&vector("single"))).unwrap();
    let poll = pump_tcp_dns(&mut rig, &mut server, Duration::ZERO);
    assert!(poll.finished[0].result.is_ok());
    assert_eq!(rig.tcp.flows(), 0);
}

#[test]
fn tcp_lengths_timeout_eof_truncation_and_cancel_release_the_flow() {
    let (mut rig, mut server, flow, _) = fallback();
    server
        .send(G, flow, &((MAX_DNS_MESSAGE + 1) as u16).to_be_bytes())
        .unwrap();
    let poll = pump_tcp_dns(&mut rig, &mut server, Duration::ZERO);
    assert_eq!(poll.finished[0].result, Err(Failure::TooLarge));
    assert_eq!((rig.tcp.flows(), rig.sockets.len()), (0, 0));

    let (mut rig, mut server, flow, _) = fallback();
    server.send(G, flow, &[0]).unwrap(); // Partial length cannot extend deadline.
    assert!(
        pump_tcp_dns(&mut rig, &mut server, secs(5))
            .finished
            .is_empty()
    );
    let poll = rig.poll(secs(6));
    assert_eq!(poll.finished[0].result, Err(Failure::Timeout));
    assert_eq!((rig.tcp.flows(), rig.sockets.len()), (0, 0));

    let (mut rig, mut server, flow, _) = fallback();
    server.close(G, Duration::ZERO, flow).unwrap();
    let poll = pump_tcp_dns(&mut rig, &mut server, Duration::ZERO);
    assert_eq!(poll.finished[0].result, Err(Failure::Aborted));
    assert_eq!(rig.tcp.flows(), 0);

    let (mut rig, mut server, flow, _) = fallback();
    server.send(G, flow, &frame(&vector("truncated"))).unwrap();
    let poll = pump_tcp_dns(&mut rig, &mut server, Duration::ZERO);
    assert_eq!(poll.finished[0].result, Err(Failure::Truncated));
    assert_eq!(rig.tcp.flows(), 0);

    let (mut rig, _, _, id) = fallback();
    assert_eq!(
        rig.client.cancel(&mut rig.sockets, &mut rig.tcp, G, id),
        Ok(true)
    );
    assert_eq!(
        (rig.client.pending(), rig.tcp.flows(), rig.sockets.len()),
        (0, 0, 0)
    );
}

#[test]
fn tcp_fallback_respects_the_shared_flow_limit_and_the_message_bound() {
    let mut rig = Rig::one(&[TXID, 0]);
    let mut tcp_config = crate::netstack::TcpConfig::new(ip(), MAX_MTU);
    tcp_config.flows = 1;
    rig.tcp = TcpStack::new(tcp_config, Script::new(&[123]), G, Duration::ZERO).unwrap();
    rig.tcp.connect(G, Duration::ZERO, R2, 80).unwrap();
    let (_, packet) = rig.start(NAME, Duration::ZERO);
    rig.feed(R1, 53, source_port(&packet), &vector("truncated"));
    let poll = rig.poll(Duration::ZERO);
    assert_eq!(poll.finished[0].result, Err(Failure::Aborted));
    assert_eq!(rig.tcp.flows(), 1);
    assert!(rig.sockets.is_empty());
    assert_eq!(
        parse_response(TXID, &wire(NAME), &vec![0; MAX_DNS_MESSAGE + 1]),
        Err(ReplyError::TooLarge)
    );
}
