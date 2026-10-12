use super::*;

const G: u64 = 1;
const ZERO: Duration = Duration::ZERO;

fn secs(value: u64) -> Duration {
    Duration::from_secs(value)
}

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

/// Replays scripted values as the first four bytes of every draw.
struct Script {
    values: Vec<u32>,
    calls: usize,
}

impl Entropy for Script {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let value = self.values[self.calls % self.values.len()];
        self.calls += 1;
        let mut out = [0u8; N];
        for (slot, byte) in out.iter_mut().zip(value.to_le_bytes()) {
            *slot = byte;
        }
        Ok(out)
    }
}

struct Failing;

impl Entropy for Failing {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        Err("entropy failed".into())
    }
}

fn timers(values: &[u32]) -> Timers<Script> {
    Timers::new(
        Script {
            values: values.to_vec(),
            calls: 0,
        },
        G,
        ZERO,
    )
}

/// A timer set with an established session; an initiator's confirming keepalive
/// is already reported as sent so only the scenario's deadlines remain.
fn established(role: Role, at: Duration) -> Timers<Script> {
    let mut timers = timers(&[0]);
    timers.session_established(role, G, at).unwrap();
    if role == Role::Initiator {
        timers.keepalive_sent(G, at).unwrap();
    }
    timers
}

/// Polls at every returned deadline until the attempt is abandoned. Returns the
/// instant it gave up and the number of initiations requested.
fn run_attempt(timers: &mut Timers<Script>, mut now: Duration) -> (Duration, u32) {
    let mut initiations = 0;
    loop {
        let due = timers.poll(G, now).unwrap();
        if due.initiate {
            initiations += 1;
        }
        if due.gave_up {
            return (now, initiations);
        }
        now = due.next.expect("an active attempt always has a deadline");
    }
}

/// `(initiator, responder)` sessions of one handshake with distinct local indexes.
fn sessions(initiator: u32, responder: u32, key: u8, born: Duration) -> (Session, Session) {
    (
        Session::from_handshake(
            [[key; 32], [key + 1; 32]],
            [initiator, responder],
            Role::Initiator,
            G,
            born,
        ),
        Session::from_handshake(
            [[key + 1; 32], [key; 32]],
            [responder, initiator],
            Role::Responder,
            G,
            born,
        ),
    )
}

#[test]
fn constants_match_the_whitepaper() {
    assert_eq!(REKEY_AFTER_TIME, secs(120));
    assert_eq!(REJECT_AFTER_TIME, secs(180));
    assert_eq!(REKEY_ATTEMPT_TIME, secs(90));
    assert_eq!(REKEY_TIMEOUT, secs(5));
    assert_eq!(KEEPALIVE_TIMEOUT, secs(10));
    assert_eq!(MAX_JITTER, ms(333));
    assert_eq!(
        REKEY_ON_RECEIVE,
        REJECT_AFTER_TIME - KEEPALIVE_TIMEOUT - REKEY_TIMEOUT
    );
    assert_eq!(DISCARD_AFTER, REJECT_AFTER_TIME * 3);
    assert_eq!(crate::wireguard::REKEY_AFTER_MESSAGES, 1 << 60);
    assert_eq!(
        crate::wireguard::REJECT_AFTER_MESSAGES,
        u64::MAX - (1 << 13)
    );
}

#[test]
fn retries_wait_the_timeout_plus_bounded_jitter_and_never_early() {
    let mut timers = timers(&[0, 333, 334, 667]);
    timers.queue_data(G, ZERO).unwrap();
    let due = timers.poll(G, ZERO).unwrap();
    assert!(due.initiate);
    assert_eq!(due.next, Some(secs(5)));

    let draws = timers.entropy.calls;
    let early = timers.poll(G, ms(4_999)).unwrap();
    assert_eq!(
        early,
        Due {
            next: Some(secs(5)),
            ..Due::default()
        }
    );
    assert_eq!(timers.entropy.calls, draws, "no entropy before a deadline");

    let due = timers.poll(G, secs(5)).unwrap();
    assert!(due.initiate);
    assert_eq!(due.next, Some(ms(10_333)));
    assert!(!timers.poll(G, ms(10_332)).unwrap().initiate);
    let due = timers.poll(G, ms(10_333)).unwrap();
    assert!(due.initiate);
    assert_eq!(due.next, Some(ms(15_333)), "334 wraps to no jitter");
    let due = timers.poll(G, ms(15_333)).unwrap();
    assert_eq!(due.next, Some(ms(20_666)), "667 wraps to 333 ms");
}

#[test]
fn jitter_stays_within_zero_to_333_ms_for_any_entropy() {
    for value in [0, 1, 332, 333, 334, 335, 1_000_000, u32::MAX] {
        let mut timers = timers(&[value]);
        timers.queue_data(G, ZERO).unwrap();
        let next = timers.poll(G, ZERO).unwrap().next.unwrap();
        assert!(next >= REKEY_TIMEOUT && next <= REKEY_TIMEOUT + MAX_JITTER);
    }
}

#[test]
fn an_attempt_gives_up_at_ninety_seconds_and_only_new_data_restarts_it() {
    let mut timers = timers(&[0]);
    timers.queue_data(G, ZERO).unwrap();
    let mut now = ZERO;
    let mut initiations = 0;
    let gave_up = loop {
        if now == secs(50) {
            // More queued data during the attempt does not extend its budget.
            timers.queue_data(G, now).unwrap();
        }
        let due = timers.poll(G, now).unwrap();
        initiations += u32::from(due.initiate);
        if due.gave_up {
            break now;
        }
        now = due.next.unwrap();
    };
    assert_eq!(gave_up, secs(90));
    assert_eq!(initiations, 18, "t = 0, 5, ..., 85 and none at 90");

    let idle = timers.poll(G, secs(200)).unwrap();
    assert_eq!(idle, Due::default());
    timers.queue_data(G, secs(200)).unwrap();
    let due = timers.poll(G, secs(200)).unwrap();
    assert!(due.initiate);
    assert_eq!(due.next, Some(secs(205)));
    let (gave_up, _) = run_attempt(&mut timers, secs(205));
    assert_eq!(gave_up, secs(290), "a fresh 90 second budget");
}

#[test]
fn initiator_rekeys_on_send_at_exactly_one_hundred_twenty_seconds() {
    let mut timers = established(Role::Initiator, ZERO);
    timers.data_sent(false, G, ms(119_999)).unwrap();
    assert!(!timers.poll(G, ms(119_999)).unwrap().initiate);
    timers.data_sent(false, G, secs(120)).unwrap();
    assert!(timers.poll(G, secs(120)).unwrap().initiate);
}

#[test]
fn responder_rekeys_only_on_the_message_count_signal() {
    let mut timers = established(Role::Responder, ZERO);
    timers.data_sent(false, G, secs(130)).unwrap();
    assert!(!timers.poll(G, secs(130)).unwrap().initiate);
    timers.data_sent(true, G, secs(131)).unwrap();
    assert!(timers.poll(G, secs(131)).unwrap().initiate);
}

#[test]
fn initiator_rekeys_on_receive_at_exactly_one_hundred_sixty_five_seconds() {
    let mut timers = established(Role::Initiator, ZERO);
    timers.data_received(false, false, G, ms(164_999)).unwrap();
    assert!(!timers.poll(G, ms(164_999)).unwrap().initiate);
    timers.data_received(false, false, G, secs(165)).unwrap();
    assert!(timers.poll(G, secs(165)).unwrap().initiate);

    let mut responder = established(Role::Responder, ZERO);
    responder.data_received(false, false, G, secs(170)).unwrap();
    assert!(!responder.poll(G, secs(170)).unwrap().initiate);
}

#[test]
fn received_traffic_never_restarts_an_abandoned_attempt_but_new_data_does() {
    let mut timers = established(Role::Responder, ZERO);
    timers.data_sent(true, G, secs(10)).unwrap();
    let (gave_up, _) = run_attempt(&mut timers, secs(10));
    assert_eq!(gave_up, secs(100));

    timers.data_received(false, true, G, secs(120)).unwrap();
    let due = timers.poll(G, secs(120)).unwrap();
    assert!(!due.initiate);
    assert!(matches!(timers.attempt, Attempt::Abandoned));

    timers.data_sent(true, G, secs(121)).unwrap();
    assert!(timers.poll(G, secs(121)).unwrap().initiate);
}

#[test]
fn passive_keepalive_follows_received_data_by_ten_seconds() {
    let mut timers = established(Role::Responder, ZERO);
    timers.data_received(true, false, G, secs(1)).unwrap();
    assert_eq!(timers.keepalive_at, None, "a keepalive arms nothing");
    timers.data_received(false, false, G, secs(2)).unwrap();
    timers.data_received(false, false, G, secs(5)).unwrap();
    assert_eq!(timers.keepalive_at, Some(secs(12)), "not extended");

    let early = timers.poll(G, ms(11_999)).unwrap();
    assert_eq!(
        early,
        Due {
            next: Some(secs(12)),
            ..Due::default()
        }
    );
    let due = timers.poll(G, secs(12)).unwrap();
    assert!(due.keepalive);
    assert!(
        timers.poll(G, secs(12)).unwrap().keepalive,
        "due until sent"
    );
    timers.keepalive_sent(G, secs(12)).unwrap();
    assert!(!timers.poll(G, secs(12)).unwrap().keepalive);

    let mut busy = established(Role::Responder, ZERO);
    busy.data_received(false, false, G, secs(2)).unwrap();
    busy.data_sent(false, G, secs(8)).unwrap();
    busy.data_received(true, false, G, secs(9)).unwrap();
    assert!(!busy.poll(G, secs(12)).unwrap().keepalive);
}

#[test]
fn initiator_owes_a_confirming_keepalive_and_the_responder_does_not() {
    let mut initiator = timers(&[0]);
    initiator
        .session_established(Role::Initiator, G, secs(3))
        .unwrap();
    assert!(initiator.poll(G, secs(3)).unwrap().keepalive);
    initiator.data_sent(false, G, secs(3)).unwrap();
    assert!(!initiator.poll(G, secs(3)).unwrap().keepalive);

    let mut responder = timers(&[0]);
    responder
        .session_established(Role::Responder, G, secs(3))
        .unwrap();
    assert!(!responder.poll(G, secs(3)).unwrap().keepalive);
}

#[test]
fn persistent_keepalive_counts_from_the_last_sent_packet() {
    let mut timers = established(Role::Initiator, ZERO);
    timers
        .set_persistent_keepalive(Some(secs(25)), G, ZERO)
        .unwrap();
    let early = timers.poll(G, ms(24_999)).unwrap();
    assert!(!early.keepalive);
    assert_eq!(early.next, Some(secs(25)));
    assert!(timers.poll(G, secs(25)).unwrap().keepalive);
    timers.keepalive_sent(G, secs(25)).unwrap();

    timers.data_sent(false, G, secs(40)).unwrap();
    timers.data_received(true, false, G, secs(41)).unwrap();
    let delayed = timers.poll(G, ms(64_999)).unwrap();
    assert!(!delayed.keepalive);
    assert_eq!(delayed.next, Some(secs(65)));
    assert!(timers.poll(G, secs(65)).unwrap().keepalive);
}

#[test]
fn persistent_keepalive_interval_is_validated() {
    let mut timers = timers(&[0]);
    for bad in [ZERO, ms(1_500), secs(65_536), ms(999)] {
        assert!(timers.set_persistent_keepalive(Some(bad), G, ZERO).is_err());
    }
    timers
        .set_persistent_keepalive(Some(secs(1)), G, ZERO)
        .unwrap();
    timers
        .set_persistent_keepalive(Some(secs(65_535)), G, ZERO)
        .unwrap();
    timers.set_persistent_keepalive(None, G, ZERO).unwrap();
}

#[test]
fn unanswered_data_forces_a_handshake_after_fifteen_seconds() {
    let mut timers = established(Role::Initiator, ZERO);
    timers.data_sent(false, G, secs(1)).unwrap();
    let early = timers.poll(G, ms(15_999)).unwrap();
    assert!(!early.initiate);
    assert_eq!(early.next, Some(secs(16)));
    let due = timers.poll(G, secs(16)).unwrap();
    assert!(due.initiate);
    assert_eq!(due.next, Some(secs(21)));

    let mut answered = established(Role::Initiator, ZERO);
    answered.data_sent(false, G, secs(1)).unwrap();
    answered.data_received(true, false, G, secs(10)).unwrap();
    let due = answered.poll(G, secs(16)).unwrap();
    assert!(!due.initiate);
    assert_eq!(due.next, Some(secs(180)));
}

#[test]
fn session_expiry_and_the_triple_age_discard_have_exact_boundaries() {
    let mut timers = established(Role::Initiator, ZERO);
    let before = timers.poll(G, ms(179_999)).unwrap();
    assert_eq!(
        before,
        Due {
            next: Some(secs(180)),
            ..Due::default()
        }
    );
    let due = timers.poll(G, secs(180)).unwrap();
    assert!(due.expired && !due.discard);
    assert_eq!(due.next, Some(secs(540)));
    let due = timers.poll(G, ms(539_999)).unwrap();
    assert_eq!(due.next, Some(secs(540)));
    let due = timers.poll(G, secs(540)).unwrap();
    assert!(due.discard);
    assert_eq!(due.next, None);
    assert!(timers.last_established.is_none() && timers.session.is_none());

    // With no usable session, new data starts a fresh attempt.
    timers.queue_data(G, secs(541)).unwrap();
    assert!(timers.poll(G, secs(541)).unwrap().initiate);
}

#[test]
fn a_new_session_postpones_the_discard() {
    let mut timers = established(Role::Initiator, ZERO);
    timers
        .session_established(Role::Initiator, G, secs(100))
        .unwrap();
    timers.keepalive_sent(G, secs(100)).unwrap();
    let due = timers.poll(G, secs(280)).unwrap();
    assert!(due.expired);
    assert_eq!(due.next, Some(secs(640)));
    assert!(!timers.poll(G, ms(639_999)).unwrap().discard);
    assert!(timers.poll(G, secs(640)).unwrap().discard);
}

#[test]
fn a_stored_cookie_neither_retransmits_nor_moves_the_retry_deadline() {
    let mut timers = timers(&[0]);
    timers.queue_data(G, ZERO).unwrap();
    assert!(timers.poll(G, ZERO).unwrap().initiate);
    let draws = timers.entropy.calls;
    timers.cookie_stored(G, secs(1)).unwrap();
    let due = timers.poll(G, secs(1)).unwrap();
    assert!(!due.initiate);
    assert_eq!(due.next, Some(secs(5)));
    assert_eq!(timers.entropy.calls, draws);
    assert!(timers.poll(G, secs(5)).unwrap().initiate);
}

#[test]
fn generation_and_clock_changes_revoke_irreversibly() {
    let mut timers = timers(&[0]);
    assert!(timers.queue_data(2, ZERO).is_err());
    assert!(timers.poll(G, ZERO).is_err());
    assert!(timers.session.is_none());

    let mut timers = self::timers(&[0]);
    timers.poll(G, secs(5)).unwrap();
    assert!(timers.poll(G, secs(4)).is_err());
    assert!(timers.queue_data(G, secs(6)).is_err());

    let mut timers = self::timers(&[0]);
    timers.queue_data(G, ZERO).unwrap();
    timers.revoke();
    assert!(timers.poll(G, ZERO).is_err());
    assert!(matches!(timers.attempt, Attempt::Idle));
}

#[test]
fn the_largest_clock_value_fails_closed_without_panicking() {
    let mut timers = Timers::new(
        Script {
            values: vec![0],
            calls: 0,
        },
        G,
        Duration::MAX,
    );
    timers.queue_data(G, Duration::MAX).unwrap();
    let due = timers.poll(G, Duration::MAX).unwrap();
    assert!(due.gave_up && !due.initiate, "a retry cannot be scheduled");
    timers
        .session_established(Role::Initiator, G, Duration::MAX)
        .unwrap();
    timers.data_sent(true, G, Duration::MAX).unwrap();
    timers.poll(G, Duration::MAX).unwrap();
}

#[test]
fn entropy_failure_changes_no_state_and_other_polls_need_none() {
    let mut timers = Timers::new(Failing, G, ZERO);
    timers.queue_data(G, ZERO).unwrap();
    assert!(timers.poll(G, ZERO).is_err());
    assert!(timers.poll(G, ZERO).is_err());
    assert!(
        timers.attempt
            == Attempt::Active {
                started: ZERO,
                next: ZERO
            }
    );

    let mut quiet = Timers::new(Failing, G, ZERO);
    quiet.session_established(Role::Initiator, G, ZERO).unwrap();
    assert!(quiet.poll(G, ZERO).unwrap().keepalive);
}

#[test]
fn initiator_installs_rotate_current_previous_and_revoke_the_rest() {
    let mut sets = KeySets::new(G, ZERO);
    let (local, mut peer) = sessions(1, 101, 10, ZERO);
    sets.install_initiator(local, G, ZERO).unwrap();
    let hello = sets.send(b"hello", G, secs(1)).unwrap().bytes;
    peer.receive(&hello, G, secs(1)).unwrap();

    let (second, _) = sessions(2, 102, 20, secs(10));
    sets.install_initiator(second, G, secs(10)).unwrap();
    let late = peer.send(b"late", G, secs(11)).unwrap().bytes;
    let received = sets.receive(&late, G, secs(11)).unwrap();
    assert!(!received.promoted);
    assert_eq!(&received.accepted.bytes[..4], b"late");

    let (third, _) = sessions(3, 103, 30, secs(20));
    sets.install_initiator(third, G, secs(20)).unwrap();
    let gone = peer.send(b"gone", G, secs(21)).unwrap().bytes;
    assert!(sets.receive(&gone, G, secs(21)).is_err());
    assert_eq!(sets.current.as_ref().unwrap().local_index, 3);
    assert_eq!(sets.previous.as_ref().unwrap().local_index, 2);
    assert!(sets.next.is_none());
}

#[test]
fn the_first_authenticated_packet_promotes_the_next_session() {
    let mut sets = KeySets::new(G, ZERO);
    let (local, _peer) = sessions(1, 101, 10, ZERO);
    sets.install_initiator(local, G, ZERO).unwrap();
    let (mut peer, next) = sessions(102, 2, 20, secs(10));
    sets.install_responder(next, G, secs(10)).unwrap();
    assert_eq!(sets.current.as_ref().unwrap().local_index, 1);

    // Forged traffic for the next index does not rotate anything.
    let mut forged = peer.send(b"first", G, secs(11)).unwrap().bytes;
    let last = forged.len() - 1;
    forged[last] ^= 1;
    assert!(sets.receive(&forged, G, secs(11)).is_err());
    assert!(sets.next.is_some());
    assert_eq!(sets.current.as_ref().unwrap().local_index, 1);

    let first = peer.send(b"first", G, secs(12)).unwrap().bytes;
    let received = sets.receive(&first, G, secs(12)).unwrap();
    assert!(received.promoted);
    assert!(sets.next.is_none());
    assert_eq!(sets.current.as_ref().unwrap().local_index, 2);
    assert_eq!(sets.previous.as_ref().unwrap().local_index, 1);
    sets.send(b"reply", G, secs(13)).unwrap();
}

#[test]
fn a_replaced_next_session_is_revoked() {
    let mut sets = KeySets::new(G, ZERO);
    let (mut peer_two, two) = sessions(102, 2, 20, ZERO);
    let (mut peer_three, three) = sessions(103, 3, 30, ZERO);
    sets.install_responder(two, G, ZERO).unwrap();
    sets.install_responder(three, G, secs(1)).unwrap();
    let stale = peer_two.send(b"old", G, secs(2)).unwrap().bytes;
    assert!(sets.receive(&stale, G, secs(2)).is_err());
    let fresh = peer_three.send(b"new", G, secs(3)).unwrap().bytes;
    let received = sets.receive(&fresh, G, secs(3)).unwrap();
    assert!(received.promoted);
    assert!(sets.previous.is_none());
}

#[test]
fn installs_enforce_role_generation_age_and_one_index_namespace() {
    let mut sets = KeySets::new(G, ZERO);
    let (first, _) = sessions(1, 101, 10, ZERO);
    sets.install_initiator(first, G, ZERO).unwrap();

    let (same, _) = sessions(1, 201, 12, ZERO);
    assert!(sets.install_initiator(same, G, ZERO).is_err());
    let (_, clash) = sessions(301, 1, 14, ZERO);
    assert!(sets.install_responder(clash, G, ZERO).is_err());
    let (_, responder) = sessions(401, 4, 16, ZERO);
    assert!(sets.install_initiator(responder, G, ZERO).is_err());
    let (initiator, _) = sessions(5, 501, 18, ZERO);
    assert!(sets.install_responder(initiator, G, ZERO).is_err());
    let other = Session::from_handshake([[1; 32], [2; 32]], [6, 601], Role::Initiator, 2, ZERO);
    assert!(sets.install_initiator(other, G, ZERO).is_err());
    let (future, _) = sessions(7, 701, 20, secs(9));
    assert!(sets.install_initiator(future, G, secs(8)).is_err());

    assert_eq!(sets.current.as_ref().unwrap().local_index, 1);
    assert!(sets.previous.is_none() && sets.next.is_none());
    let (valid, _) = sessions(8, 801, 22, secs(9));
    sets.install_initiator(valid, G, secs(9)).unwrap();
}

#[test]
fn unknown_indexes_and_short_packets_change_nothing() {
    let mut sets = KeySets::new(G, ZERO);
    let (local, mut peer) = sessions(1, 101, 10, ZERO);
    sets.install_initiator(local, G, ZERO).unwrap();
    assert!(sets.receive(&[0; 7], G, ZERO).is_err());
    let mut unknown = vec![4, 0, 0, 0, 9, 0, 0, 0];
    unknown.extend_from_slice(&[0; 24]);
    assert!(sets.receive(&unknown, G, ZERO).is_err());

    let hello = sets.send(b"hi", G, secs(1)).unwrap().bytes;
    peer.receive(&hello, G, secs(1)).unwrap();
    let back = peer.send(b"ok", G, secs(2)).unwrap().bytes;
    assert!(sets.receive(&back, G, secs(2)).is_ok());
}

#[test]
fn expire_removes_exactly_the_sessions_at_the_reject_age() {
    let mut sets = KeySets::new(G, ZERO);
    let (first, _) = sessions(1, 101, 10, ZERO);
    let (second, _) = sessions(2, 102, 20, secs(100));
    sets.install_initiator(first, G, ZERO).unwrap();
    sets.install_initiator(second, G, secs(100)).unwrap();
    assert_eq!(sets.expire(G, ms(179_999)).unwrap(), 0);
    assert_eq!(sets.expire(G, secs(180)).unwrap(), 1);
    assert!(sets.previous.is_none() && sets.current.is_some());
    assert_eq!(sets.expire(G, ms(279_999)).unwrap(), 0);
    assert_eq!(sets.expire(G, secs(280)).unwrap(), 1);
    assert!(sets.send(b"x", G, secs(281)).is_err());
}

#[test]
fn discard_all_and_fences_are_strict() {
    let mut sets = KeySets::new(G, ZERO);
    let (first, _) = sessions(1, 101, 10, ZERO);
    let (_, next) = sessions(102, 2, 20, ZERO);
    sets.install_initiator(first, G, ZERO).unwrap();
    sets.install_responder(next, G, ZERO).unwrap();
    sets.discard_all();
    assert!(sets.current.is_none() && sets.previous.is_none() && sets.next.is_none());
    let (again, _) = sessions(1, 101, 30, ZERO);
    sets.install_initiator(again, G, ZERO).unwrap();

    assert!(sets.send(b"x", 2, ZERO).is_err());
    assert!(
        sets.send(b"x", G, ZERO).is_err(),
        "a generation change revokes"
    );

    let mut regress = KeySets::new(G, ZERO);
    regress.expire(G, secs(5)).unwrap();
    assert!(regress.expire(G, secs(4)).is_err());
    assert!(regress.expire(G, secs(6)).is_err());

    let mut revoked = KeySets::new(G, ZERO);
    let (session, _) = sessions(1, 101, 40, ZERO);
    revoked.install_initiator(session, G, ZERO).unwrap();
    revoked.revoke();
    assert!(revoked.current.is_none());
    let (session, _) = sessions(2, 102, 42, ZERO);
    assert!(revoked.install_initiator(session, G, ZERO).is_err());
}

struct Peer {
    sets: KeySets,
    timers: Timers<Script>,
}

impl Peer {
    fn new() -> Self {
        Self {
            sets: KeySets::new(G, ZERO),
            timers: timers(&[0]),
        }
    }

    fn send(&mut self, payload: &[u8], now: Duration) -> Vec<u8> {
        let sent = self.sets.send(payload, G, now).unwrap();
        if payload.is_empty() {
            self.timers.keepalive_sent(G, now).unwrap();
        } else {
            self.timers.data_sent(sent.rekey, G, now).unwrap();
        }
        sent.bytes
    }

    fn receive(&mut self, packet: &[u8], now: Duration) {
        let received = self.sets.receive(packet, G, now).unwrap();
        if received.promoted {
            self.timers
                .session_established(Role::Responder, G, now)
                .unwrap();
        }
        let keepalive = received.accepted.bytes.is_empty();
        self.timers
            .data_received(keepalive, received.accepted.rekey, G, now)
            .unwrap();
    }
}

#[test]
fn two_peers_survive_loss_retry_passive_keepalive_and_rekey() {
    let mut a = Peer::new();
    let mut b = Peer::new();

    // The first initiation is lost; the retry five seconds later gets through.
    a.timers.queue_data(G, ZERO).unwrap();
    assert!(a.timers.poll(G, ZERO).unwrap().initiate);
    assert!(!a.timers.poll(G, ms(4_999)).unwrap().initiate);
    assert!(a.timers.poll(G, secs(5)).unwrap().initiate);
    let (a_one, b_one) = sessions(1, 2, 10, secs(5));
    a.sets.install_initiator(a_one, G, secs(5)).unwrap();
    a.timers
        .session_established(Role::Initiator, G, secs(5))
        .unwrap();
    b.sets.install_responder(b_one, G, secs(5)).unwrap();

    // The initiator confirms the key with a keepalive; the responder promotes.
    let due = a.timers.poll(G, secs(5)).unwrap();
    assert!(due.keepalive && !due.initiate);
    let confirm = a.send(b"", secs(5));
    assert_eq!(confirm.len(), 32);
    b.receive(&confirm, secs(5));
    assert_eq!(b.sets.current.as_ref().unwrap().local_index, 2);

    // Data from A makes B owe a passive keepalive ten seconds later.
    let data = a.send(b"data", secs(6));
    b.receive(&data, secs(6));
    assert!(!b.timers.poll(G, ms(15_999)).unwrap().keepalive);
    assert!(b.timers.poll(G, secs(16)).unwrap().keepalive);
    let keepalive = b.send(b"", secs(16));
    a.receive(&keepalive, secs(16));
    assert!(!a.timers.poll(G, secs(19)).unwrap().initiate, "reply seen");

    // Data to a vanished peer: no reply for 15 seconds forces a new handshake.
    a.send(b"lost", secs(20));
    assert!(!a.timers.poll(G, ms(34_999)).unwrap().initiate);
    assert!(a.timers.poll(G, secs(35)).unwrap().initiate);

    let (a_two, b_two) = sessions(3, 4, 30, secs(35));
    a.sets.install_initiator(a_two, G, secs(35)).unwrap();
    a.timers
        .session_established(Role::Initiator, G, secs(35))
        .unwrap();
    b.sets.install_responder(b_two, G, secs(35)).unwrap();
    assert!(a.timers.poll(G, secs(35)).unwrap().keepalive);
    let confirm = a.send(b"", secs(35));
    b.receive(&confirm, secs(35));

    assert_eq!(a.sets.current.as_ref().unwrap().local_index, 3);
    assert_eq!(a.sets.previous.as_ref().unwrap().local_index, 1);
    assert_eq!(b.sets.current.as_ref().unwrap().local_index, 4);
    assert_eq!(b.sets.previous.as_ref().unwrap().local_index, 2);
    assert_eq!(a.timers.next_deadline(), Some(secs(215)));
    assert_eq!(b.timers.next_deadline(), Some(secs(215)));
}
