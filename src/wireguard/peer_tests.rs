use super::*;
use crate::crypto::blake2s;
use std::time::UNIX_EPOCH;

struct Script {
    counter: u64,
    forced: VecDeque<Vec<u8>>,
    fail: bool,
}
#[derive(Clone)]
struct Stream(Rc<RefCell<Script>>);
impl Stream {
    fn new(seed: u64) -> Self {
        Self(Rc::new(RefCell::new(Script {
            counter: seed,
            forced: VecDeque::new(),
            fail: false,
        })))
    }
    fn force(&self, bytes: Vec<u8>) {
        self.0.borrow_mut().forced.push_back(bytes);
    }
}
impl Entropy for Stream {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut state = self.0.borrow_mut();
        if state.fail {
            return Err("scripted entropy failure".into());
        }
        if let Some(bytes) = state.forced.pop_front() {
            return bytes.try_into().map_err(|_| "script size mismatch".into());
        }
        let mut bytes = [0; N];
        for chunk in bytes.chunks_mut(32) {
            state.counter = state.counter.checked_add(1).unwrap();
            let block = blake2s(&state.counter.to_le_bytes())?;
            chunk.copy_from_slice(&block[..chunk.len()]);
        }
        Ok(bytes)
    }
}
fn clock(seconds: u64) -> PeerClock {
    PeerClock {
        generation: 4,
        now: Duration::from_secs(seconds),
        wall: UNIX_EPOCH + Duration::from_secs(1000 + seconds),
    }
}
fn address(port: u16) -> SocketAddr {
    SocketAddr::from(([192, 0, 2, 1], port))
}
fn pair_with_streams() -> (Peer<Stream>, Peer<Stream>, Stream, Stream) {
    let left = Stream::new(10);
    let right = Stream::new(1000);
    let a = Peer::new(
        [17; 32],
        x25519_public_key([29; 32]),
        Some([53; 32]),
        address(2),
        left.clone(),
        3,
        clock(0),
    )
    .unwrap();
    let b = Peer::new(
        [29; 32],
        x25519_public_key([17; 32]),
        Some([53; 32]),
        address(1),
        right.clone(),
        3,
        clock(0),
    )
    .unwrap();
    (a, b, left, right)
}
fn pair() -> (Peer<Stream>, Peer<Stream>) {
    let (a, b, _, _) = pair_with_streams();
    (a, b)
}
fn contact(a: &mut Peer<Stream>, b: &mut Peer<Stream>, time: u64) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let was_ready = b.keys.current_role().is_some();
    let initiation = a.send(b"first", clock(time)).unwrap().datagrams.remove(0);
    let response = b
        .receive(&initiation, address(1), Load::Normal, clock(time))
        .unwrap()
        .datagrams
        .remove(0);
    assert_eq!(
        b.status(),
        if was_ready {
            PeerStatus::ProtectedReady
        } else {
            PeerStatus::Handshaking
        }
    );
    let data = a
        .receive(&response, address(2), Load::Normal, clock(time))
        .unwrap()
        .datagrams
        .remove(0);
    let received = b
        .receive(&data, address(1), Load::Normal, clock(time))
        .unwrap();
    assert_eq!(&received.inner_payloads[0][..5], b"first");
    assert_eq!(a.status(), PeerStatus::ProtectedReady);
    assert_eq!(b.status(), PeerStatus::ProtectedReady);
    (initiation, response, data)
}

#[test]
fn first_contact_confirmation_padding_and_fixed_endpoint() {
    let (mut a, mut b) = pair();
    contact(&mut a, &mut b, 0);
    let roaming = address(9000);
    let sent = b.send(b"back", clock(1)).unwrap();
    let received = a
        .receive(&sent.datagrams[0], roaming, Load::Normal, clock(1))
        .unwrap();
    assert_eq!(received.authenticated_source, Some(roaming));
    assert_eq!(&received.inner_payloads[0][..4], b"back");
    assert_eq!(received.inner_payloads[0].len(), 16);
    assert_eq!(a.endpoint(), address(2));
    assert_eq!(a.queued_payloads(), 0);
}

#[test]
fn bounded_loss_reorder_duplicate_link_recovers_retry_and_old_response() {
    let (mut a, mut b) = pair();
    let lost = a.send(b"one", clock(0)).unwrap().datagrams.remove(0);
    assert_eq!(a.poll(clock(4)).unwrap().datagrams.len(), 0);
    let retry = a.poll(clock(6)).unwrap().datagrams.remove(0);
    assert_ne!(&lost[4..8], &retry[4..8]);
    let old_response = b
        .receive(&lost, address(1), Load::Normal, clock(6))
        .unwrap()
        .datagrams
        .remove(0);
    let response = b
        .receive(&retry, address(1), Load::Normal, clock(6))
        .unwrap()
        .datagrams
        .remove(0);
    assert!(
        a.receive(&old_response, address(2), Load::Normal, clock(6))
            .is_err()
    );
    let data = a
        .receive(&response, address(2), Load::Normal, clock(6))
        .unwrap()
        .datagrams
        .remove(0);
    assert!(
        a.receive(&response, address(2), Load::Normal, clock(6))
            .is_err()
    );
    let accepted = b
        .receive(&data, address(1), Load::Normal, clock(6))
        .unwrap();
    assert_eq!(accepted.inner_payloads.len(), 1);
    assert!(
        b.receive(&data, address(1), Load::Normal, clock(6))
            .is_err()
    );
    let x = a.send(b"two", clock(7)).unwrap().datagrams.remove(0);
    let y = a.send(b"three", clock(7)).unwrap().datagrams.remove(0);
    assert_eq!(
        &b.receive(&y, address(1), Load::Normal, clock(7))
            .unwrap()
            .inner_payloads[0][..5],
        b"three"
    );
    assert_eq!(
        &b.receive(&x, address(1), Load::Normal, clock(7))
            .unwrap()
            .inner_payloads[0][..3],
        b"two"
    );
}

#[test]
fn cookie_under_load_preserves_retry_deadline_and_never_authenticates_source() {
    let (mut a, mut b) = pair();
    let initiation = a.send(b"waiting", clock(0)).unwrap().datagrams.remove(0);
    let challenged = b
        .receive(&initiation, address(1), Load::High, clock(0))
        .unwrap();
    assert_eq!(challenged.authenticated_source, None);
    assert_eq!(b.status(), PeerStatus::NoSession);
    let before = a.timers.next_deadline();
    let stored = a
        .receive(&challenged.datagrams[0], address(2), Load::Normal, clock(1))
        .unwrap();
    assert!(stored.datagrams.is_empty());
    assert_eq!(stored.authenticated_source, None);
    assert_eq!(a.timers.next_deadline(), before);
    assert!(a.poll(clock(4)).unwrap().datagrams.is_empty());
    let retry = a.poll(clock(6)).unwrap().datagrams.remove(0);
    assert_ne!(&retry[retry.len() - 16..], &[0; 16]);
    let response = b.receive(&retry, address(1), Load::High, clock(6)).unwrap();
    assert_eq!(response.datagrams[0][0], 2);
    let confirm = a
        .receive(&response.datagrams[0], address(2), Load::Normal, clock(6))
        .unwrap();
    assert_eq!(
        b.receive(&confirm.datagrams[0], address(1), Load::Normal, clock(6))
            .unwrap()
            .status,
        PeerStatus::ProtectedReady
    );
}

#[test]
fn queue_full_rejects_newest_and_attempt_timeout_drops_with_count() {
    let (mut a, _) = pair();
    a.send(b"one", clock(0)).unwrap();
    a.send(b"two", clock(0)).unwrap();
    a.send(b"three", clock(0)).unwrap();
    let before = a.timers.next_deadline();
    assert!(a.send(b"four", clock(0)).is_err());
    assert_eq!(a.queued_payloads(), 3);
    assert_eq!(a.timers.next_deadline(), before);
    assert!(a.send(&vec![0; MAX_PAYLOAD + 1], clock(0)).is_err());
    let output = a.poll(clock(90)).unwrap();
    assert_eq!(output.status, PeerStatus::GaveUp);
    assert_eq!(output.dropped_payloads, 3);
    assert_eq!(a.queued_payloads(), 0);
    assert!(a.poll(clock(91)).unwrap().datagrams.is_empty());
    assert_eq!(a.send(b"new", clock(91)).unwrap().datagrams[0][0], 1);
}

#[test]
fn queued_zero_length_confirms_and_passive_keepalive_follows_data_only() {
    let (mut a, mut b) = pair();
    let first = a.send(b"", clock(0)).unwrap().datagrams.remove(0);
    let response = b
        .receive(&first, address(1), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    let confirm = a
        .receive(&response, address(2), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    let output = b
        .receive(&confirm, address(1), Load::Normal, clock(0))
        .unwrap();
    assert_eq!(output.status, PeerStatus::ProtectedReady);
    assert!(output.inner_payloads.is_empty());
    assert!(b.poll(clock(10)).unwrap().datagrams.is_empty());
    let data = a.send(b"data", clock(11)).unwrap().datagrams.remove(0);
    b.receive(&data, address(1), Load::Normal, clock(11))
        .unwrap();
    assert!(b.poll(clock(20)).unwrap().datagrams.is_empty());
    let passive = b.poll(clock(21)).unwrap().datagrams.remove(0);
    let output = a
        .receive(&passive, address(2), Load::Normal, clock(21))
        .unwrap();
    assert!(output.inner_payloads.is_empty());
    assert!(a.poll(clock(21)).unwrap().datagrams.is_empty());
}

#[test]
fn time_rekey_loss_recovery_and_previous_reordering() {
    let (mut a, mut b) = pair();
    contact(&mut a, &mut b, 0);
    let old = b.send(b"old", clock(1)).unwrap().datagrams.remove(0);
    let sent = a.send(b"at120", clock(120)).unwrap();
    let rekey = sent.datagrams.iter().find(|p| p[0] == 1).unwrap().clone();
    let data = sent.datagrams.iter().find(|p| p[0] == 4).unwrap().clone();
    assert_eq!(rekey[0], 1);
    let response = b
        .receive(&rekey, address(1), Load::Normal, clock(120))
        .unwrap()
        .datagrams
        .remove(0);
    let confirm = a
        .receive(&response, address(2), Load::Normal, clock(120))
        .unwrap()
        .datagrams
        .remove(0);
    b.receive(&confirm, address(1), Load::Normal, clock(120))
        .unwrap();
    let before = a.timers.next_deadline();
    let previous = a
        .receive(&old, address(2), Load::Normal, clock(121))
        .unwrap();
    assert_eq!(&previous.inner_payloads[0][..3], b"old");
    assert_eq!(a.timers.next_deadline(), before);
    // Old data can be reordered across the handshake and is authenticated once.
    assert_eq!(
        &b.receive(&data, address(1), Load::Normal, clock(121))
            .unwrap()
            .inner_payloads[0][..5],
        b"at120"
    );
    assert!(
        a.receive(&old, address(2), Load::Normal, clock(121))
            .is_err()
    );
    let lost = a.send(b"no peer", clock(240)).unwrap();
    assert!(lost.datagrams.iter().any(|p| p[0] == 1));
    assert!(lost.datagrams.iter().any(|p| p[0] == 4));
    let retry = a.poll(clock(246)).unwrap().datagrams.remove(0);
    let response = b
        .receive(&retry, address(1), Load::Normal, clock(246))
        .unwrap()
        .datagrams
        .remove(0);
    let confirm = a
        .receive(&response, address(2), Load::Normal, clock(246))
        .unwrap()
        .datagrams
        .remove(0);
    assert_eq!(
        b.receive(&confirm, address(1), Load::Normal, clock(246))
            .unwrap()
            .status,
        PeerStatus::ProtectedReady
    );
}

#[test]
fn simultaneous_initiations_converge_for_both_delivery_orders() {
    for reverse in [false, true] {
        let (mut a, mut b) = pair();
        let left = a.send(b"left", clock(0)).unwrap().datagrams.remove(0);
        let right = b.send(b"right", clock(0)).unwrap().datagrams.remove(0);
        let (ao, bo) = if reverse {
            let bo = b
                .receive(&left, address(1), Load::Normal, clock(0))
                .unwrap();
            let ao = a
                .receive(&right, address(2), Load::Normal, clock(0))
                .unwrap();
            (ao, bo)
        } else {
            let ao = a
                .receive(&right, address(2), Load::Normal, clock(0))
                .unwrap();
            let bo = b
                .receive(&left, address(1), Load::Normal, clock(0))
                .unwrap();
            (ao, bo)
        };
        let mut link: VecDeque<(bool, Vec<u8>)> = ao
            .datagrams
            .into_iter()
            .map(|p| (false, p))
            .chain(bo.datagrams.into_iter().map(|p| (true, p)))
            .collect();
        for _ in 0..8 {
            let Some((to_a, packet)) = link.pop_front() else {
                break;
            };
            let output = if to_a {
                a.receive(&packet, address(2), Load::Normal, clock(0))
                    .unwrap()
            } else {
                b.receive(&packet, address(1), Load::Normal, clock(0))
                    .unwrap()
            };
            link.extend(output.datagrams.into_iter().map(|p| (!to_a, p)));
        }
        assert!(link.is_empty());
        assert_eq!(a.status(), PeerStatus::ProtectedReady);
        assert_eq!(b.status(), PeerStatus::ProtectedReady);
        assert_eq!(a.queued_payloads() + b.queued_payloads(), 0);
        let data = b.send(b"aligned", clock(1)).unwrap().datagrams.remove(0);
        assert_eq!(
            &a.receive(&data, address(2), Load::Normal, clock(1))
                .unwrap()
                .inner_payloads[0][..7],
            b"aligned"
        );
    }
}

#[test]
fn fresh_restart_keys_reject_old_transport_and_restore_contact() {
    let (mut a, mut b, _, _) = pair_with_streams();
    let (_, _, old) = contact(&mut a, &mut b, 0);
    a.revoke();
    let mut fresh = Peer::new(
        [17; 32],
        x25519_public_key([29; 32]),
        Some([53; 32]),
        address(2),
        Stream::new(5000),
        3,
        clock(1),
    )
    .unwrap();
    let (_, _, new) = contact(&mut fresh, &mut b, 1);
    assert_ne!(old, new);
    assert!(b.receive(&old, address(1), Load::Normal, clock(1)).is_err());
    assert!(a.send(b"revoked", clock(1)).is_err());
}

#[test]
fn forged_replayed_misrouted_and_every_byte_tamper_never_promote_or_consume() {
    let (mut a, mut b) = pair();
    let init = a.send(b"proof", clock(0)).unwrap().datagrams.remove(0);
    for i in 0..init.len() - 16 {
        let mut packet = init.clone();
        packet[i] ^= 1;
        assert!(
            b.receive(&packet, address(1), Load::Normal, clock(0))
                .is_err()
        );
        assert_eq!(b.status(), PeerStatus::NoSession);
    }
    let response = b
        .receive(&init, address(1), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    for i in 0..response.len() - 16 {
        let mut packet = response.clone();
        packet[i] ^= 1;
        assert!(
            a.receive(&packet, address(2), Load::Normal, clock(0))
                .is_err()
        );
        assert_eq!(a.status(), PeerStatus::Handshaking);
    }
    let data = a
        .receive(&response, address(2), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    for i in 0..data.len() {
        let mut packet = data.clone();
        packet[i] ^= 1;
        assert!(
            b.receive(&packet, address(1), Load::Normal, clock(0))
                .is_err()
        );
        assert_eq!(b.status(), PeerStatus::Handshaking);
    }
    b.receive(&data, address(1), Load::Normal, clock(0))
        .unwrap();
    assert!(
        b.receive(&data, address(1), Load::Normal, clock(0))
            .is_err()
    );
    assert!(
        a.receive(&data, address(2), Load::Normal, clock(0))
            .is_err()
    );
    for packet in [
        vec![],
        vec![0],
        vec![5; 4],
        vec![4; 4096],
        vec![3; 63],
        vec![2; 93],
        vec![1; 147],
    ] {
        assert!(
            b.receive(&packet, address(1), Load::Normal, clock(0))
                .is_err()
        );
    }
}

#[test]
fn generation_and_clock_fences_irrevocably_revoke_all_components() {
    for regression in [false, true] {
        let (mut a, mut b) = pair();
        contact(&mut a, &mut b, 10);
        let mut bad = clock(10);
        if regression {
            bad.now = Duration::from_secs(9);
        } else {
            bad.generation = 5;
        }
        // Invalid framing cannot bypass the fence.
        assert!(a.receive(&[], address(2), Load::Normal, bad).is_err());
        assert_eq!(a.status(), PeerStatus::Revoked);
        assert_eq!(a.queued_payloads(), 0);
        assert_eq!(a.handshake.pending_index(), None);
        assert_eq!(a.admission.tracked_sources(), 0);
        assert_eq!(a.timers.next_deadline(), None);
        assert!(a.keys.current_role().is_none());
        assert!(a.poll(clock(11)).is_err());
        assert!(a.send(b"", clock(11)).is_err());
        assert!(
            a.set_persistent_keepalive(Some(Duration::from_secs(1)), clock(11))
                .is_err()
        );
    }
}

#[test]
fn entropy_failure_and_repeated_ephemeral_never_fall_back() {
    let (mut a, _, control, _) = pair_with_streams();
    control.0.borrow_mut().fail = true;
    assert!(a.send(b"waiting", clock(0)).is_err());
    assert_eq!(a.status(), PeerStatus::Revoked);
    control.0.borrow_mut().fail = false;
    assert!(a.poll(clock(1)).is_err());
    let (mut a, _, control, _) = pair_with_streams();
    a.send(b"waiting", clock(0)).unwrap();
    let previous = a.last_ephemeral.unwrap();
    control.force(vec![0; 4]); // jitter
    control.force(vec![7; 4]); // new index
    control.force(previous.to_vec());
    assert!(a.poll(clock(6)).is_err());
    assert_eq!(a.status(), PeerStatus::Revoked);
}

#[test]
fn index_collision_budget_preserves_pending_and_current_keys() {
    let (mut a, mut b, control, _) = pair_with_streams();
    let initiation = a.send(b"queued", clock(0)).unwrap().datagrams.remove(0);
    let index = initiation[4..8].to_vec();
    let response = b
        .receive(&initiation, address(1), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    let pending = a.handshake.pending_index();
    for _ in 0..INDEX_TRIES {
        control.force(index.clone());
    }
    assert!(a.allocate().is_err());
    assert_eq!(a.handshake.pending_index(), pending);
    assert_eq!(a.queued_payloads(), 1);
    // Pending old response is expired, but a later fresh retry recovers.
    assert!(
        a.receive(&response, address(2), Load::Normal, clock(6))
            .is_err()
    );
    assert_eq!(a.handshake.pending_index(), None);
    let retry = a.poll(clock(12)).unwrap().datagrams.remove(0);
    let response = b
        .receive(&retry, address(1), Load::Normal, clock(12))
        .unwrap()
        .datagrams
        .remove(0);
    let confirm = a
        .receive(&response, address(2), Load::Normal, clock(12))
        .unwrap()
        .datagrams
        .remove(0);
    b.receive(&confirm, address(1), Load::Normal, clock(12))
        .unwrap();
    control.force(vec![0; 4]);
    for _ in 0..INDEX_TRIES {
        control.force(retry[4..8].to_vec());
    }
    assert!(a.send(b"rekey", clock(132)).is_err());
    assert_eq!(a.status(), PeerStatus::ProtectedReady);
    assert!(
        a.keys
            .contains_index(u32::from_le_bytes(retry[4..8].try_into().unwrap()))
    );
}

#[test]
fn delayed_confirmation_uses_real_key_age_and_unconfirmed_keys_expire() {
    let (mut a, mut b) = pair();
    let init = a.send(b"data", clock(0)).unwrap().datagrams.remove(0);
    let response = b
        .receive(&init, address(1), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    let confirm = a
        .receive(&response, address(2), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    b.receive(&confirm, address(1), Load::Normal, clock(179))
        .unwrap();
    assert_eq!(b.keys.current_created(), Some(Duration::ZERO));
    let output = b.poll(clock(180)).unwrap();
    assert_eq!(output.status, PeerStatus::Expired);
    assert!(
        b.receive(&confirm, address(1), Load::Normal, clock(180))
            .is_err()
    );
    let (mut a, mut b) = pair();
    let init = a.send(b"data", clock(0)).unwrap().datagrams.remove(0);
    let response = b
        .receive(&init, address(1), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    let confirm = a
        .receive(&response, address(2), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    assert!(
        b.receive(&confirm, address(1), Load::Normal, clock(180))
            .is_err()
    );
    assert!(b.keys.current_role().is_none());
}

#[test]
fn persistent_keepalive_discard_recovery_and_clock_overflow_are_bounded() {
    let (mut a, mut b) = pair();
    contact(&mut a, &mut b, 0);
    a.set_persistent_keepalive(Some(Duration::from_secs(1)), clock(0))
        .unwrap();
    assert_eq!(a.poll(clock(1)).unwrap().datagrams[0][0], 4);
    assert!(
        a.set_persistent_keepalive(Some(Duration::ZERO), clock(1))
            .is_err()
    );
    assert!(
        a.set_persistent_keepalive(Some(Duration::from_millis(1500)), clock(1))
            .is_err()
    );
    a.poll(clock(540)).unwrap();
    assert_eq!(a.handshake.pending_index(), None);
    assert!(a.keys.current_role().is_none());
    let init = a
        .send(b"after discard", clock(541))
        .unwrap()
        .datagrams
        .remove(0);
    let response = b
        .receive(&init, address(1), Load::Normal, clock(541))
        .unwrap()
        .datagrams
        .remove(0);
    let confirm = a
        .receive(&response, address(2), Load::Normal, clock(541))
        .unwrap()
        .datagrams
        .remove(0);
    b.receive(&confirm, address(1), Load::Normal, clock(541))
        .unwrap();
    assert_eq!(a.status(), PeerStatus::ProtectedReady);
    let c = PeerClock {
        now: Duration::MAX,
        ..clock(0)
    };
    let mut huge = Peer::new(
        [17; 32],
        x25519_public_key([29; 32]),
        None,
        address(2),
        Stream::new(77),
        1,
        c,
    )
    .unwrap();
    assert_eq!(
        huge.send(b"overflow", c).unwrap().status,
        PeerStatus::GaveUp
    );
    assert!(huge.poll(c).unwrap().datagrams.is_empty());
}

#[test]
fn message_count_rekey_and_reject_keep_queue_truthful() {
    let (mut a, mut b) = pair();
    contact(&mut a, &mut b, 0);
    a.keys.current_mut().sent = super::super::REKEY_AFTER_MESSAGES - 1;
    let data = a.send(b"threshold", clock(1)).unwrap().datagrams.remove(0);
    b.receive(&data, address(1), Load::Normal, clock(1))
        .unwrap();
    assert_eq!(a.poll(clock(6)).unwrap().datagrams[0][0], 1);
    a.keys.current_mut().sent = super::super::REJECT_AFTER_MESSAGES;
    assert!(a.send(b"cannot send", clock(7)).is_err());
    assert_eq!(a.queued_payloads(), 1);
    // The exhausted session is removed before returning the failed operation.
    assert_eq!(a.status(), PeerStatus::Expired);
    a.poll(clock(7)).unwrap();
    assert_eq!(a.status(), PeerStatus::Expired);
    assert!(a.keys.current_role().is_none());
}

#[test]
fn queue_flush_preserves_fifo_and_output_bound() {
    let (mut a, mut b) = pair();
    let init = a.send(b"one", clock(0)).unwrap().datagrams.remove(0);
    a.send(b"two", clock(0)).unwrap();
    a.send(b"three", clock(0)).unwrap();
    let response = b
        .receive(&init, address(1), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    let flushed = a
        .receive(&response, address(2), Load::Normal, clock(0))
        .unwrap();
    assert_eq!(flushed.datagrams.len(), 3);
    for (packet, expected) in flushed
        .datagrams
        .iter()
        .zip([b"one".as_slice(), b"two", b"three"])
    {
        let received = b
            .receive(packet, address(1), Load::Normal, clock(0))
            .unwrap();
        assert_eq!(&received.inner_payloads[0][..expected.len()], expected);
    }
    assert_eq!(a.queued_payloads(), 0);
}

#[test]
fn allocator_rejects_previous_next_and_pending_namespace_collisions() {
    let (mut a, mut b, control, _) = pair_with_streams();
    let (first, _, _) = contact(&mut a, &mut b, 0);
    // Keep the first index resident in previous after a second handshake.
    let rekey = a
        .send(b"rotate", clock(120))
        .unwrap()
        .datagrams
        .into_iter()
        .find(|p| p[0] == 1)
        .unwrap();
    let response = b
        .receive(&rekey, address(1), Load::Normal, clock(120))
        .unwrap()
        .datagrams
        .remove(0);
    let confirm = a
        .receive(&response, address(2), Load::Normal, clock(120))
        .unwrap()
        .datagrams
        .remove(0);
    b.receive(&confirm, address(1), Load::Normal, clock(120))
        .unwrap();
    control.force(first[4..8].to_vec());
    control.force(rekey[4..8].to_vec());
    control.force(123456u32.to_le_bytes().to_vec());
    let (index, _) = a.allocate().unwrap();
    assert_eq!(index, 123456);
    // A responder's next index is also reserved even before confirmation.
    let (mut a, mut b, _, control) = pair_with_streams();
    let init = a.send(b"waiting", clock(0)).unwrap().datagrams.remove(0);
    let response = b
        .receive(&init, address(1), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    control.force(response[4..8].to_vec());
    control.force(234567u32.to_le_bytes().to_vec());
    assert_eq!(b.allocate().unwrap().0, 234567);
    assert!(
        b.keys
            .contains_index(u32::from_le_bytes(response[4..8].try_into().unwrap()))
    );
}

#[test]
fn responder_discards_old_state_before_installing_new_handshake() {
    let (mut a, mut b) = pair();
    contact(&mut a, &mut b, 0);
    a.poll(clock(540)).unwrap();
    let init = a
        .send(b"new contact", clock(541))
        .unwrap()
        .datagrams
        .remove(0);
    // No intervening poll on the responder: stale discard must not erase next.
    let response = b
        .receive(&init, address(1), Load::Normal, clock(541))
        .unwrap()
        .datagrams
        .remove(0);
    let confirm = a
        .receive(&response, address(2), Load::Normal, clock(541))
        .unwrap()
        .datagrams
        .remove(0);
    assert_eq!(
        b.receive(&confirm, address(1), Load::Normal, clock(541))
            .unwrap()
            .status,
        PeerStatus::ProtectedReady
    );
}

#[test]
fn unconfirmed_responder_expiry_is_scheduled_and_reported() {
    let (mut a, mut b) = pair();
    let init = a.send(b"waiting", clock(0)).unwrap().datagrams.remove(0);
    let response = b
        .receive(&init, address(1), Load::Normal, clock(0))
        .unwrap();
    assert_eq!(response.next, Some(Duration::from_secs(180)));
    assert_eq!(b.poll(clock(180)).unwrap().status, PeerStatus::Expired);
    assert_eq!(b.poll(clock(181)).unwrap().next, None);
}

#[test]
fn every_cookie_byte_tamper_and_mac2_challenge_preserve_attempt() {
    let (mut a, mut b) = pair();
    let init = a.send(b"waiting", clock(0)).unwrap().datagrams.remove(0);
    let reply = b
        .receive(&init, address(1), Load::High, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    let deadline = a.timers.next_deadline();
    for i in 0..reply.len() {
        let mut packet = reply.clone();
        packet[i] ^= 1;
        assert!(
            a.receive(&packet, address(2), Load::Normal, clock(0))
                .is_err()
        );
        assert_eq!(a.timers.next_deadline(), deadline);
        assert_eq!(a.status(), PeerStatus::Handshaking);
    }
    a.receive(&reply, address(2), Load::Normal, clock(0))
        .unwrap();
    let retry = a.poll(clock(6)).unwrap().datagrams.remove(0);
    for i in retry.len() - 16..retry.len() {
        let mut packet = retry.clone();
        packet[i] ^= 1;
        let out = b
            .receive(&packet, address(1), Load::High, clock(6))
            .unwrap();
        assert!(out.authenticated_source.is_none());
        assert!(out.datagrams.is_empty() || out.datagrams[0][0] == 3);
        assert_eq!(b.status(), PeerStatus::NoSession);
    }
}

#[test]
fn response_cookie_admission_recovers_on_next_initiation() {
    let (mut a, mut b) = pair();
    let init = a.send(b"waiting", clock(0)).unwrap().datagrams.remove(0);
    let response = b
        .receive(&init, address(1), Load::Normal, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    let cookie = a
        .receive(&response, address(2), Load::High, clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    assert_eq!(cookie[0], 3);
    assert!(
        b.receive(&cookie, address(1), Load::Normal, clock(0))
            .unwrap()
            .datagrams
            .is_empty()
    );
    let retry = a.poll(clock(6)).unwrap().datagrams.remove(0);
    let response = b
        .receive(&retry, address(1), Load::Normal, clock(6))
        .unwrap()
        .datagrams
        .remove(0);
    let confirm = a
        .receive(&response, address(2), Load::High, clock(6))
        .unwrap()
        .datagrams
        .remove(0);
    assert_eq!(confirm[0], 4);
    b.receive(&confirm, address(1), Load::Normal, clock(6))
        .unwrap();
}

#[test]
fn spoofed_pinned_identity_and_admission_entropy_failure_are_closed() {
    let (mut a, mut b, _, control) = pair_with_streams();
    let mut stranger = Peer::new(
        [61; 32],
        x25519_public_key([29; 32]),
        Some([53; 32]),
        address(2),
        Stream::new(700),
        1,
        clock(0),
    )
    .unwrap();
    let forged_identity = stranger
        .send(b"spoof", clock(0))
        .unwrap()
        .datagrams
        .remove(0);
    assert!(
        b.receive(&forged_identity, address(3), Load::Normal, clock(0))
            .is_err()
    );
    assert_eq!(b.status(), PeerStatus::NoSession);
    let init = a.send(b"real", clock(0)).unwrap().datagrams.remove(0);
    control.0.borrow_mut().fail = true;
    assert!(b.receive(&init, address(1), Load::High, clock(0)).is_err());
    assert_eq!(b.status(), PeerStatus::Revoked);
    assert_eq!(b.admission.tracked_sources(), 0);
}

#[test]
fn expired_simultaneous_pending_does_not_lock_out_fresh_contact() {
    let (mut a, mut b) = pair();
    a.send(b"left", clock(0)).unwrap();
    b.send(b"right", clock(0)).unwrap();
    // Drop both initiations. Only the higher public key retries; the lower key
    // receives it before its own timer poll, with its five-second pending stale.
    let (mut lower, mut higher) = if a.prefer_initiator { (a, b) } else { (b, a) };
    let retry = higher.poll(clock(6)).unwrap().datagrams.remove(0);
    let accepted = lower
        .receive(&retry, address(2), Load::Normal, clock(6))
        .unwrap();
    assert_eq!(accepted.authenticated_source, Some(address(2)));
    assert!(accepted.datagrams.iter().any(|packet| packet[0] == 2));
}
