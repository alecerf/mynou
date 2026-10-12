use super::*;
use crate::crypto::{blake2s, hkdf_blake2s_expand, x25519_public_key};
use crate::json::{self, Value};
use std::io::Read;

const ZERO: Duration = Duration::ZERO;

fn secs(value: u64) -> Duration {
    Duration::from_secs(value)
}

/// Deterministic computed stream. Every value is derived by hashing a counter, so
/// no key or nonce is a source literal.
struct Stream {
    seed: [u8; 32],
    calls: u64,
}

impl Stream {
    fn new(label: &str) -> Self {
        Self {
            seed: blake2s(label.as_bytes()).unwrap(),
            calls: 0,
        }
    }
}

impl Entropy for Stream {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.calls += 1;
        let mut input = self.seed.to_vec();
        input.extend_from_slice(&self.calls.to_le_bytes());
        let block = hkdf_blake2s_expand(&blake2s(&input)?, &[], N)?;
        block.try_into().map_err(|_| "stream length".into())
    }
}

/// Replays caller-provided chunks, for independent reference vectors.
struct Script(Vec<Vec<u8>>);

impl Entropy for Script {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        if self.0.is_empty() {
            return Err("script exhausted".into());
        }
        self.0
            .remove(0)
            .try_into()
            .map_err(|_| "script length".into())
    }
}

struct Failing;

impl Entropy for Failing {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        Err("entropy unavailable".into())
    }
}

/// A stuck source: the same computed value on every request.
struct Stuck([u8; 32]);

impl Entropy for Stuck {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(std::array::from_fn(|index| self.0[index % 32]))
    }
}

fn public(label: &str) -> [u8; 32] {
    x25519_public_key(Stream::new(label).bytes::<32>().unwrap())
}

fn address(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

fn hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2));
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

/// An initiation (kind 1) or response (kind 2) addressed to `recipient`, with the
/// sender-side cookie state that produced it.
fn message(kind: u8, index: u32, recipient: &[u8; 32]) -> (Vec<u8>, Cookies) {
    let length = if kind == 1 { INITIATION } else { RESPONSE };
    let mut packet = vec![kind, 0, 0, 0];
    packet.extend_from_slice(&index.to_le_bytes());
    let mut filler = Stream::new("cookie test body");
    while packet.len() < length - 32 {
        packet.push(filler.bytes::<1>().unwrap()[0]);
    }
    let mut cookies = Cookies::new(recipient).unwrap();
    cookies.stamp(&mut packet, index, ZERO).unwrap();
    assert_eq!(packet.len(), length);
    (packet, cookies)
}

fn loaded(label: &str, generation: u64) -> (Admission<Stream>, [u8; 32]) {
    let local = public(label);
    (
        Admission::new(local, Stream::new(label), generation, ZERO).unwrap(),
        local,
    )
}

fn reply_of(outcome: Result<Admit>) -> Vec<u8> {
    match outcome.unwrap() {
        Admit::Reply(reply) => reply,
        other => panic!("expected a cookie reply, got {other:?}"),
    }
}

fn restamp(cookies: &mut Cookies, packet: &[u8], now: Duration) -> Vec<u8> {
    let mut retry = packet[..packet.len() - 32].to_vec();
    let index = u32::from_le_bytes(retry[4..8].try_into().unwrap());
    cookies.stamp(&mut retry, index, now).unwrap();
    retry
}

#[test]
fn independent_reference_cookie_replies_and_mac2() {
    let file = std::fs::File::open(std::env::var("MYNOU_CRYPTO_FIXTURES").unwrap()).unwrap();
    let mut text = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .unwrap();
    assert!(text.len() <= 1024 * 1024);
    let value = json::parse(&text).unwrap();
    let rows = value.get("cookie").unwrap().as_array().unwrap();
    assert_eq!(rows.len(), 4);
    let field = |row: &Value, name: &str| hex(row.get(name).unwrap().as_str().unwrap());
    for row in rows {
        let recipient: [u8; 32] = field(row, "public").try_into().unwrap();
        let host = row.get("host").unwrap().as_str().unwrap();
        let port = row.get("port").unwrap().as_u64().unwrap();
        let source = address(&if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        });
        let body = field(row, "body");
        let index = u32::from_le_bytes(body[4..8].try_into().unwrap());
        let mut admission = Admission::new(
            recipient,
            Script(vec![field(row, "secret"), field(row, "nonce")]),
            4,
            ZERO,
        )
        .unwrap();
        let mut cookies = Cookies::new(&recipient).unwrap();
        let mut first = body.clone();
        cookies.stamp(&mut first, index, ZERO).unwrap();
        assert_eq!(&first[body.len()..body.len() + 16], field(row, "mac1"));
        assert_eq!(first[body.len() + 16..], [0; 16]);
        let reply = reply_of(admission.admit(&first, source, Load::High, 4, ZERO));
        assert_eq!(reply, field(row, "reply"));
        cookies.receive(&reply, secs(1)).unwrap();
        assert_eq!(
            cookies.stored.as_ref().unwrap().value,
            *field(row, "cookie")
        );
        let mut retry = body.clone();
        cookies.stamp(&mut retry, index, secs(2)).unwrap();
        assert_eq!(retry[retry.len() - 16..], field(row, "retry_mac2"));
        assert_eq!(
            admission
                .admit(&retry, source, Load::High, 4, secs(2))
                .unwrap(),
            Admit::Process
        );
    }
}

#[test]
fn normal_load_ignores_mac2_without_entropy_or_state_and_invalid_mac1_errors() {
    let (mut admission, local) = loaded("normal load", 4);
    for kind in [1, 2] {
        let (packet, _) = message(kind, 9, &local);
        let mut noisy = packet.clone();
        let length = noisy.len();
        for byte in &mut noisy[length - 16..] {
            *byte ^= 0x5a;
        }
        for candidate in [packet, noisy] {
            assert_eq!(
                admission
                    .admit(&candidate, address("192.0.2.1:1"), Load::Normal, 4, ZERO)
                    .unwrap(),
                Admit::Process
            );
        }
    }
    assert_eq!(admission.entropy.calls, 0);
    assert_eq!(admission.tracked_sources(), 0);
    let (other, _) = message(1, 9, &public("another identity"));
    assert!(
        admission
            .admit(&other, address("192.0.2.1:1"), Load::Normal, 4, ZERO)
            .is_err()
    );
}

#[test]
fn invalid_framing_and_mac1_stay_silent_and_stateless_under_load() {
    let (mut admission, local) = loaded("silence", 4);
    let source = address("203.0.113.9:7");
    for size in 0..=200 {
        assert!(
            admission
                .admit(&vec![1; size], source, Load::High, 4, ZERO)
                .is_err()
        );
    }
    for kind in [1, 2] {
        let (packet, _) = message(kind, 77, &local);
        for index in 0..packet.len() - 16 {
            let mut corrupted = packet.clone();
            corrupted[index] ^= 1;
            assert!(
                admission
                    .admit(&corrupted, source, Load::High, 4, ZERO)
                    .is_err(),
                "kind {kind} byte {index}"
            );
        }
        // Type confusion: the same bytes under the other type fail on length.
        let mut confused = packet.clone();
        confused[0] = 3 - kind;
        assert!(
            admission
                .admit(&confused, source, Load::High, 4, ZERO)
                .is_err()
        );
        let mut padded = packet.clone();
        padded.push(0);
        assert!(
            admission
                .admit(&padded, source, Load::High, 4, ZERO)
                .is_err()
        );
    }
    assert_eq!(admission.entropy.calls, 0, "no secret or nonce before MAC1");
    assert_eq!(admission.tracked_sources(), 0, "no state before MAC1");
    assert!(admission.secret.is_none());
}

#[test]
fn reply_is_bound_to_message_peer_and_every_byte() {
    let (mut admission, local) = loaded("binding", 4);
    let source = address("198.51.100.20:9000");
    let (packet, mut cookies) = message(1, 0x01020304, &local);
    let reply = reply_of(admission.admit(&packet, source, Load::High, 4, ZERO));
    assert_eq!(reply.len(), 64);
    assert_eq!(reply[..4], [3, 0, 0, 0]);
    assert_eq!(reply[4..8], packet[4..8]);
    assert!(reply.len() < packet.len(), "a reply never amplifies");
    for index in 0..reply.len() {
        let mut corrupted = reply.clone();
        corrupted[index] ^= 1;
        assert!(
            cookies.receive(&corrupted, secs(1)).is_err(),
            "byte {index}"
        );
        assert!(cookies.stored.is_none());
    }
    for size in [0, 1, 63, 65, 148] {
        assert!(cookies.receive(&vec![3; size], secs(1)).is_err());
    }
    // A reply sealed for another responder or to other associated data fails.
    let (mut stranger, other) = loaded("stranger", 4);
    let (their_packet, _) = message(1, 0x01020304, &other);
    let foreign = reply_of(stranger.admit(&their_packet, source, Load::High, 4, ZERO));
    assert!(cookies.receive(&foreign, secs(1)).is_err());
    assert!(
        Cookies::new(&public("never contacted"))
            .unwrap()
            .receive(&reply, secs(1))
            .is_err()
    );
    cookies.receive(&reply, secs(1)).unwrap();
    // After a new message, the old genuine reply no longer authenticates.
    let mut changed = packet[..116].to_vec();
    changed[40] ^= 1;
    cookies.stamp(&mut changed, 0x01020304, secs(6)).unwrap();
    assert!(cookies.receive(&reply, secs(6)).is_err());
    // The wrong receiver index fails even with a valid AEAD for the right MAC1.
    let mut other_index = Cookies::new(&local).unwrap();
    let mut body = packet[..116].to_vec();
    other_index.stamp(&mut body, 0x01020305, ZERO).unwrap();
    assert!(other_index.receive(&reply, secs(1)).is_err());
}

#[test]
fn stored_cookie_and_message_age_boundaries() {
    let (mut admission, local) = loaded("ages", 4);
    let source = address("192.0.2.77:51820");
    let (packet, mut cookies) = message(1, 5, &local);
    let reply = reply_of(admission.admit(&packet, source, Load::High, 4, ZERO));
    // A reply to a message older than the cookie lifetime is stale.
    assert!(cookies.receive(&reply, COOKIE_LIFETIME).is_err());
    cookies
        .receive(&reply, COOKIE_LIFETIME - Duration::from_nanos(1))
        .unwrap();
    let received = COOKIE_LIFETIME - Duration::from_nanos(1);
    let used = restamp(
        &mut cookies,
        &packet,
        received + COOKIE_LIFETIME - Duration::from_nanos(1),
    );
    assert_ne!(used[used.len() - 16..], [0; 16]);
    let expired = restamp(&mut cookies, &packet, received + COOKIE_LIFETIME);
    assert_eq!(expired[expired.len() - 16..], [0; 16]);
    assert!(cookies.stored.is_none(), "an expired cookie is discarded");
    // A newer reply replaces the stored cookie.
    let (second, mut again) = message(1, 6, &local);
    let one = reply_of(admission.admit(&second, source, Load::High, 4, secs(1)));
    again.receive(&one, secs(2)).unwrap();
    let first_value = again.stored.as_ref().unwrap().value;
    let two = reply_of(admission.admit(&second, source, Load::High, 4, secs(3)));
    again.receive(&two, secs(4)).unwrap();
    assert_eq!(again.stored.as_ref().unwrap().value, first_value);
    assert_eq!(again.stored.as_ref().unwrap().at, secs(4));
    // Revocation-style clearing drops everything.
    again.clear();
    assert!(again.receive(&two, secs(5)).is_err());
}

#[test]
fn cookie_is_bound_to_source_address_port_and_canonical_family() {
    let (mut admission, local) = loaded("source binding", 4);
    let home = address("192.0.2.33:5000");
    let (packet, mut cookies) = message(1, 11, &local);
    let reply = reply_of(admission.admit(&packet, home, Load::High, 4, ZERO));
    cookies.receive(&reply, secs(1)).unwrap();
    let retry = restamp(&mut cookies, &packet, secs(2));
    for (offset, other) in [
        "192.0.2.34:5000",
        "192.0.2.33:5001",
        "[2001:db8::21]:5000",
        "203.0.113.33:5000",
    ]
    .into_iter()
    .enumerate()
    {
        let now = secs(3 + offset as u64);
        assert!(
            matches!(
                admission.admit(&retry, address(other), Load::High, 4, now),
                Ok(Admit::Reply(_))
            ),
            "{other}"
        );
    }
    for accepted in ["192.0.2.33:5000", "[::ffff:192.0.2.33]:5000"] {
        assert_eq!(
            admission
                .admit(&retry, address(accepted), Load::High, 4, secs(10))
                .unwrap(),
            Admit::Process,
            "{accepted}"
        );
    }
    // IPv6 scope and flow information are not part of the observed address.
    let mut scoped = std::net::SocketAddrV6::new("2001:db8::1".parse().unwrap(), 4000, 0, 0);
    let (packet, mut cookies) = message(2, 12, &local);
    let reply = reply_of(admission.admit(&packet, scoped.into(), Load::High, 4, secs(20)));
    cookies.receive(&reply, secs(21)).unwrap();
    let retry = restamp(&mut cookies, &packet, secs(22));
    scoped.set_scope_id(9);
    scoped.set_flowinfo(77);
    assert_eq!(
        admission
            .admit(&retry, scoped.into(), Load::High, 4, secs(23))
            .unwrap(),
        Admit::Process
    );
}

#[test]
fn secret_rotates_exactly_at_two_minutes_and_old_cookies_fail() {
    let (mut admission, local) = loaded("rotation", 4);
    let source = address("192.0.2.40:4000");
    let (packet, mut cookies) = message(1, 3, &local);
    let reply = reply_of(admission.admit(&packet, source, Load::High, 4, ZERO));
    let first_secret = admission.secret.as_ref().unwrap().value;
    cookies.receive(&reply, secs(1)).unwrap();
    let retry = restamp(&mut cookies, &packet, secs(2));
    let last = SECRET_LIFETIME - Duration::from_nanos(1);
    assert_eq!(
        admission
            .admit(&retry, source, Load::High, 4, last)
            .unwrap(),
        Admit::Process
    );
    assert_eq!(admission.secret.as_ref().unwrap().value, first_secret);
    let rotated = reply_of(admission.admit(&retry, source, Load::High, 4, SECRET_LIFETIME));
    let second = admission.secret.as_ref().unwrap();
    assert_ne!(second.value, first_secret);
    assert_eq!(second.born, SECRET_LIFETIME);
    // The initiator's old cookie is still fresh for it but no longer valid there.
    assert!(cookies.fresh(SECRET_LIFETIME).is_some());
    cookies
        .receive(&rotated, SECRET_LIFETIME + secs(1))
        .unwrap();
    let again = restamp(&mut cookies, &packet, SECRET_LIFETIME + secs(2));
    assert_eq!(
        admission
            .admit(&again, source, Load::High, 4, SECRET_LIFETIME + secs(2))
            .unwrap(),
        Admit::Process
    );
}

#[test]
fn limiter_budget_recovery_and_reply_accounting() {
    let (mut admission, local) = loaded("limits", 4);
    let source = address("192.0.2.50:1000");
    let (packet, _) = message(1, 1, &local);
    // Five replies spend the burst, then the source is silent.
    for _ in 0..5 {
        assert!(matches!(
            admission.admit(&packet, source, Load::High, 4, ZERO),
            Ok(Admit::Reply(_))
        ));
    }
    let used = admission.entropy.calls;
    assert_eq!(
        admission
            .admit(&packet, source, Load::High, 4, ZERO)
            .unwrap(),
        Admit::Limited
    );
    assert_eq!(
        admission.entropy.calls, used,
        "limited work draws no entropy"
    );
    // One interval later exactly one more message is admitted.
    let later = SOURCE_INTERVAL;
    assert!(matches!(
        admission.admit(&packet, source, Load::High, 4, later),
        Ok(Admit::Reply(_))
    ));
    assert_eq!(
        admission
            .admit(&packet, source, Load::High, 4, later)
            .unwrap(),
        Admit::Limited
    );
    // Another source and, after recovery, the same source are unaffected.
    assert!(matches!(
        admission.admit(&packet, address("192.0.2.51:1000"), Load::High, 4, later),
        Ok(Admit::Reply(_))
    ));
    assert!(matches!(
        admission.admit(&packet, source, Load::High, 4, secs(1)),
        Ok(Admit::Reply(_))
    ));
    // Normal load never consults the limiter.
    for _ in 0..50 {
        assert_eq!(
            admission
                .admit(&packet, source, Load::Normal, 4, secs(1))
                .unwrap(),
            Admit::Process
        );
    }
    // Hosts in one IPv6 /64 share a budget; another /64 does not.
    let base = secs(100);
    for tail in 0..5 {
        let host = address(&format!("[2001:db8:0:1::{tail:x}]:1000"));
        assert!(matches!(
            admission.admit(&packet, host, Load::High, 4, base),
            Ok(Admit::Reply(_))
        ));
    }
    assert_eq!(
        admission
            .admit(
                &packet,
                address("[2001:db8:0:1::ffff]:1"),
                Load::High,
                4,
                base
            )
            .unwrap(),
        Admit::Limited
    );
    assert!(matches!(
        admission.admit(&packet, address("[2001:db8:0:2::1]:1"), Load::High, 4, base),
        Ok(Admit::Reply(_))
    ));
}

#[test]
fn source_table_is_bounded_keeps_indebted_sources_and_prunes_idle_ones() {
    let (mut admission, local) = loaded("table", 4);
    let (packet, _) = message(1, 1, &local);
    let heavy = address("198.51.100.1:1");
    for _ in 0..5 {
        admission
            .admit(&packet, heavy, Load::High, 4, ZERO)
            .unwrap();
    }
    let limited = |admission: &mut Admission<Stream>, now| {
        admission.admit(&packet, heavy, Load::High, 4, now).unwrap() == Admit::Limited
    };
    assert!(limited(&mut admission, ZERO));
    for number in 0..(3 * MAX_SOURCES) as u32 {
        let octets = (0x0a00_0000u32 + number).to_be_bytes();
        let source = SocketAddr::from((octets, 2000));
        admission
            .admit(&packet, source, Load::High, 4, ZERO)
            .unwrap();
        assert!(admission.tracked_sources() <= MAX_SOURCES);
    }
    assert_eq!(admission.tracked_sources(), MAX_SOURCES);
    assert!(
        limited(&mut admission, ZERO),
        "the indebted source was not evicted"
    );
    // Once every debt has been paid the table forgets idle sources when it must grow.
    let idle = secs(10);
    admission
        .admit(&packet, address("203.0.113.5:5"), Load::High, 4, idle)
        .unwrap();
    assert_eq!(admission.tracked_sources(), 1);
    admission.revoke();
    assert_eq!(admission.tracked_sources(), 0);
}

#[test]
fn entropy_failures_and_stuck_sources_fail_closed() {
    let local = public("entropy");
    let source = address("192.0.2.60:1");
    let (packet, _) = message(1, 1, &local);
    let mut failing = Admission::new(local, Failing, 4, ZERO).unwrap();
    assert_eq!(
        failing
            .admit(&packet, source, Load::Normal, 4, ZERO)
            .unwrap(),
        Admit::Process
    );
    assert!(failing.admit(&packet, source, Load::High, 4, ZERO).is_err());
    assert!(failing.secret.is_none(), "no secret was invented");
    // A stuck source gives the same nonce twice; the second reply is refused.
    let stuck = Stuck(Stream::new("stuck").bytes::<32>().unwrap());
    let mut admission = Admission::new(local, stuck, 4, ZERO).unwrap();
    assert!(matches!(
        admission.admit(&packet, source, Load::High, 4, ZERO),
        Ok(Admit::Reply(_))
    ));
    assert!(
        admission
            .admit(&packet, source, Load::High, 4, secs(1))
            .is_err()
    );
    // A failing replacement after expiry leaves no usable stale secret.
    let mut script = Admission::new(
        local,
        Script(vec![
            Stream::new("secret").bytes::<32>().unwrap().to_vec(),
            Stream::new("nonce").bytes::<24>().unwrap().to_vec(),
        ]),
        4,
        ZERO,
    )
    .unwrap();
    assert!(matches!(
        script.admit(&packet, source, Load::High, 4, ZERO),
        Ok(Admit::Reply(_))
    ));
    assert!(
        script
            .admit(&packet, source, Load::High, 4, SECRET_LIFETIME)
            .is_err()
    );
    assert!(script.secret.is_none());
}

#[test]
fn generation_clock_and_revocation_fences_are_irreversible() {
    let source = address("192.0.2.70:1");
    let (mut admission, local) = loaded("fences", 4);
    let (packet, _) = message(1, 1, &local);
    admission
        .admit(&packet, source, Load::High, 4, secs(5))
        .unwrap();
    assert!(
        admission
            .admit(&packet, source, Load::High, 4, secs(4))
            .is_err()
    );
    assert!(
        admission
            .admit(&packet, source, Load::Normal, 4, secs(6))
            .is_err()
    );
    assert!(admission.secret.is_none() && admission.tracked_sources() == 0);
    let (mut other, local) = loaded("fences two", 4);
    let (packet, _) = message(1, 1, &local);
    assert!(other.admit(&packet, source, Load::Normal, 5, ZERO).is_err());
    assert!(other.admit(&packet, source, Load::Normal, 4, ZERO).is_err());
    let (mut explicit, local) = loaded("fences three", 4);
    let (packet, _) = message(1, 1, &local);
    explicit.revoke();
    assert!(
        explicit
            .admit(&packet, source, Load::Normal, 4, ZERO)
            .is_err()
    );
}

#[test]
fn extreme_clock_values_do_not_overflow() {
    let (mut admission, local) = loaded("extreme", 4);
    let (packet, _) = message(1, 1, &local);
    let source = address("192.0.2.80:1");
    assert_eq!(
        admission
            .admit(&packet, source, Load::High, 4, Duration::MAX)
            .unwrap(),
        Admit::Limited
    );
    let (second, mut cookies) = message(1, 2, &local);
    assert_eq!(
        admission
            .admit(&second, source, Load::Normal, 4, Duration::MAX)
            .unwrap(),
        Admit::Process
    );
    let mut body = second[..116].to_vec();
    cookies.stamp(&mut body, 2, Duration::MAX).unwrap();
    assert!(cookies.fresh(Duration::MAX).is_none());
}

#[test]
fn production_entropy_yields_distinct_values() {
    let mut os = OsEntropy;
    assert_ne!(os.bytes::<32>().unwrap(), os.bytes::<32>().unwrap());
    assert_ne!(os.bytes::<24>().unwrap(), os.bytes::<24>().unwrap());
}
