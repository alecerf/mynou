use super::*;
use crate::json;
use std::io::Read;

fn pair() -> (Session, Session) {
    (
        Session::from_handshake(
            [[7; 32], [9; 32]],
            [17, 31],
            Role::Initiator,
            4,
            Duration::ZERO,
        ),
        Session::from_handshake(
            [[9; 32], [7; 32]],
            [31, 17],
            Role::Responder,
            4,
            Duration::ZERO,
        ),
    )
}

fn bytes(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2));
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn independent_reference_packets_include_nonce_endianness_and_padding() {
    let file = std::fs::File::open(std::env::var("MYNOU_CRYPTO_FIXTURES").unwrap()).unwrap();
    let mut data = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut data)
        .unwrap();
    assert!(data.len() <= 1024 * 1024);
    let fixtures = json::parse(&data).unwrap();
    assert_eq!(fixtures.get("schema").unwrap().as_u64(), Some(1));
    let rows = fixtures.get("wireguard").unwrap().as_array().unwrap();
    assert_eq!(rows.len(), 16);
    for row in rows {
        let key: [u8; 32] = bytes(row.get("key").unwrap().as_str().unwrap())
            .try_into()
            .unwrap();
        let payload = bytes(row.get("payload").unwrap().as_str().unwrap());
        let mut session =
            Session::from_handshake([key, [9; 32]], [17, 31], Role::Initiator, 4, Duration::ZERO);
        session.sent = row
            .get("counter")
            .unwrap()
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let actual = session.send(&payload, 4, Duration::ZERO).unwrap();
        assert_eq!(
            actual.bytes,
            bytes(row.get("packet").unwrap().as_str().unwrap())
        );
        let mut receiver =
            Session::from_handshake([[9; 32], key], [31, 17], Role::Responder, 4, Duration::ZERO);
        let expected = bytes(row.get("packet").unwrap().as_str().unwrap());
        let received = receiver.receive(&expected, 4, Duration::ZERO).unwrap();
        assert_eq!(&received.bytes[..payload.len()], &payload);
        assert!(
            received.bytes[payload.len()..]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert!(receiver.receive(&expected, 4, Duration::ZERO).is_err());
    }
}

#[test]
fn responder_confirmation_keepalive_directional_keys_and_padding() {
    let (mut initiator, mut responder) = pair();
    assert!(
        responder
            .send(b"before confirmation", 4, Duration::ZERO)
            .is_err()
    );
    let empty = initiator.send(b"", 4, Duration::ZERO).unwrap();
    assert_eq!(empty.bytes.len(), 32);
    assert!(
        responder
            .receive(&empty.bytes, 4, Duration::ZERO)
            .unwrap()
            .bytes
            .is_empty()
    );
    let packet = responder.send(b"reply", 4, Duration::ZERO).unwrap();
    let reply = initiator.receive(&packet.bytes, 4, Duration::ZERO).unwrap();
    assert_eq!(&reply.bytes[..5], b"reply");
    assert_eq!(&reply.bytes[5..], &[0; 11]);
    assert!(responder.receive(&empty.bytes, 4, Duration::ZERO).is_err());
}

#[test]
fn every_field_and_tag_tamper_is_rejected_without_confirmation_or_window_update() {
    let (mut initiator, _) = pair();
    let packet = initiator
        .send(b"synthetic", 4, Duration::ZERO)
        .unwrap()
        .bytes;
    for index in 0..packet.len() {
        let (_, mut responder) = pair();
        let mut altered = packet.clone();
        altered[index] ^= 1;
        assert!(
            responder.receive(&altered, 4, Duration::ZERO).is_err(),
            "byte {index}"
        );
        assert!(!responder.confirmed);
        assert!(responder.replay.highest.is_none());
        assert!(responder.send(b"", 4, Duration::ZERO).is_err());
        assert!(responder.receive(&packet, 4, Duration::ZERO).is_ok());
    }
    let (_, mut wrong_key) = pair();
    wrong_key.keys.as_mut().unwrap().1 = ChaCha20Poly1305::new([1; 32]);
    assert!(wrong_key.receive(&packet, 4, Duration::ZERO).is_err());
    assert!(wrong_key.replay.highest.is_none());
}

#[test]
fn forged_high_counter_cannot_evict_valid_out_of_order_packets() {
    let (mut sender, mut receiver) = pair();
    let first = sender.send(b"first", 4, Duration::ZERO).unwrap().bytes;
    sender.sent = WINDOW;
    let later = sender.send(b"later", 4, Duration::ZERO).unwrap().bytes;
    let mut forged = later.clone();
    forged[8..16].copy_from_slice(&(REJECT_AFTER_MESSAGES - 1).to_le_bytes());
    assert!(receiver.receive(&forged, 4, Duration::ZERO).is_err());
    assert!(receiver.replay.highest.is_none());
    assert!(receiver.receive(&first, 4, Duration::ZERO).is_ok());
    sender.sent = WINDOW - 1;
    let edge = sender.send(b"edge", 4, Duration::ZERO).unwrap().bytes;
    assert!(receiver.receive(&edge, 4, Duration::ZERO).is_ok());
    assert!(receiver.receive(&later, 4, Duration::ZERO).is_ok());
    assert!(receiver.receive(&first, 4, Duration::ZERO).is_err());
    assert!(receiver.receive(&edge, 4, Duration::ZERO).is_err());
    sender.sent = 1;
    let within = sender.send(b"within", 4, Duration::ZERO).unwrap().bytes;
    assert!(receiver.receive(&within, 4, Duration::ZERO).is_ok());
    assert!(receiver.receive(&within, 4, Duration::ZERO).is_err());
}

#[test]
fn replay_window_wraps_words_and_handles_large_jumps_in_bounded_memory() {
    let mut replay = Replay::default();
    for counter in 0..(WINDOW * 3) {
        assert!(replay.available(counter));
        replay.record(counter);
        assert!(!replay.available(counter));
        if counter >= WINDOW {
            assert!(!replay.available(counter - WINDOW));
        }
    }
    replay.record(REJECT_AFTER_MESSAGES - 1);
    assert!(!replay.available(REJECT_AFTER_MESSAGES - 1));
    assert!(replay.available(REJECT_AFTER_MESSAGES - 2));
    assert!(!replay.available(0));
}

#[test]
fn send_and_receive_reject_budgets_framing_and_counter_exhaustion() {
    let (mut sender, mut receiver) = pair();
    assert!(
        sender
            .send(&vec![1; MAX_PAYLOAD + 1], 4, Duration::ZERO)
            .is_err()
    );
    assert_eq!(sender.sent, 0);
    let packet = sender
        .send(&vec![1; MAX_PAYLOAD], 4, Duration::ZERO)
        .unwrap()
        .bytes;
    assert_eq!(packet.len(), MAX_DATAGRAM);
    for length in [0, 1, 15, 16, 31, 33, MAX_DATAGRAM + 16] {
        assert!(
            receiver
                .receive(&vec![0; length], 4, Duration::ZERO)
                .is_err()
        );
    }
    assert!(receiver.receive(&packet, 4, Duration::ZERO).is_ok());
    sender.sent = REJECT_AFTER_MESSAGES - 1;
    let last = sender.send(b"", 4, Duration::ZERO).unwrap().bytes;
    assert!(receiver.receive(&last, 4, Duration::ZERO).is_ok());
    assert!(sender.send(b"", 4, Duration::ZERO).is_err());
    assert!(sender.keys.is_none());
    let mut rejected = last;
    rejected[8..16].copy_from_slice(&REJECT_AFTER_MESSAGES.to_le_bytes());
    assert!(receiver.receive(&rejected, 4, Duration::ZERO).is_err());
}

#[test]
fn monotonic_age_generation_and_revocation_never_reactivate_keys() {
    for mode in 0..3 {
        let (mut sender, _) = pair();
        sender.send(b"", 4, Duration::from_secs(10)).unwrap();
        match mode {
            0 => assert!(sender.send(b"", 4, Duration::from_secs(9)).is_err()),
            1 => assert!(sender.send(b"", 5, Duration::from_secs(10)).is_err()),
            _ => sender.revoke(),
        }
        assert!(sender.send(b"", 4, Duration::from_secs(11)).is_err());
        assert!(sender.keys.is_none());
    }
    let (mut sender, mut receiver) = pair();
    let packet = sender.send(b"", 4, Duration::from_secs(179)).unwrap().bytes;
    assert!(
        receiver
            .receive(&packet, 4, Duration::from_secs(180))
            .is_err()
    );
    assert!(receiver.keys.is_none());
    assert!(sender.send(b"", 4, Duration::from_secs(180)).is_err());
    for mode in 0..3 {
        let (mut sender, mut receiver) = pair();
        let first = sender.send(b"", 4, Duration::ZERO).unwrap().bytes;
        receiver
            .receive(&first, 4, Duration::from_secs(10))
            .unwrap();
        let next = sender.send(b"", 4, Duration::ZERO).unwrap().bytes;
        match mode {
            0 => assert!(receiver.receive(&next, 4, Duration::from_secs(9)).is_err()),
            1 => assert!(receiver.receive(&next, 5, Duration::from_secs(10)).is_err()),
            _ => receiver.revoke(),
        }
        assert!(receiver.receive(&next, 4, Duration::from_secs(11)).is_err());
        assert!(receiver.keys.is_none());
    }
    let created = Duration::from_secs(1000);
    let mut session =
        Session::from_handshake([[7; 32], [9; 32]], [17, 31], Role::Initiator, 4, created);
    assert!(
        !session
            .send(b"", 4, created + Duration::from_secs(119))
            .unwrap()
            .rekey
    );
    assert!(
        session
            .send(b"", 4, created + Duration::from_secs(120))
            .unwrap()
            .rekey
    );
    assert!(
        session
            .send(b"", 4, created + Duration::from_secs(179))
            .is_ok()
    );
    assert!(
        session
            .send(b"", 4, created + Duration::from_secs(180))
            .is_err()
    );
}

#[test]
fn rekey_signals_observe_role_time_and_message_boundaries() {
    let (mut sender, mut receiver) = pair();
    sender.sent = REKEY_AFTER_MESSAGES - 2;
    assert!(!sender.send(b"", 4, Duration::from_secs(119)).unwrap().rekey);
    assert!(sender.send(b"", 4, Duration::from_secs(119)).unwrap().rekey);
    let (mut initiator, mut responder) = pair();
    assert!(
        !initiator
            .send(b"", 4, Duration::from_secs(119))
            .unwrap()
            .rekey
    );
    let packet = initiator.send(b"", 4, Duration::from_secs(120)).unwrap();
    assert!(packet.rekey);
    assert!(
        !responder
            .receive(&packet.bytes, 4, Duration::from_secs(120))
            .unwrap()
            .rekey
    );
    let response = responder.send(b"", 4, Duration::from_secs(164)).unwrap();
    assert!(!response.rekey);
    assert!(
        !initiator
            .receive(&response.bytes, 4, Duration::from_secs(164))
            .unwrap()
            .rekey
    );
    let response = responder.send(b"", 4, Duration::from_secs(165)).unwrap();
    assert!(
        initiator
            .receive(&response.bytes, 4, Duration::from_secs(165))
            .unwrap()
            .rekey
    );
    let forged = vec![0; 32];
    assert!(
        receiver
            .receive(&forged, 4, Duration::from_secs(165))
            .is_err()
    );
    assert!(!receiver.confirmed);
}
