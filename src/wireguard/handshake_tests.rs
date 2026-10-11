use super::*;
use crate::json::{self, Value};
use std::io::Read;

fn wall() -> SystemTime {
    UNIX_EPOCH + Duration::new(123, 456)
}
fn pair(psk: Option<[u8; 32]>) -> (Handshake, Handshake) {
    let left = [17; 32];
    let right = [29; 32];
    (
        Handshake::configured(left, x25519_public_key(right), psk, 4, Duration::ZERO, 17).unwrap(),
        Handshake::configured(right, x25519_public_key(left), psk, 4, Duration::ZERO, 31).unwrap(),
    )
}
fn bytes(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2));
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}
fn binary(row: &Value, field: &str) -> Vec<u8> {
    bytes(row.get(field).unwrap().as_str().unwrap())
}
fn array(row: &Value, field: &str) -> [u8; 32] {
    binary(row, field).try_into().unwrap()
}
fn remac(packet: &mut Vec<u8>, recipient: [u8; 32]) {
    packet.truncate(packet.len() - 32);
    append_macs(packet, recipient).unwrap();
}

#[test]
fn independent_noise_transcript_packets_and_session_keys() {
    let file = std::fs::File::open(std::env::var("MYNOU_CRYPTO_FIXTURES").unwrap()).unwrap();
    let mut text = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .unwrap();
    assert!(text.len() <= 1024 * 1024);
    let value = json::parse(&text).unwrap();
    assert_eq!(value.get("schema").unwrap().as_u64(), Some(1));
    let rows = value.get("handshake").unwrap().as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        let left = array(row, "initiator");
        let right = array(row, "responder");
        let psk = array(row, "psk");
        let mut initiator = Handshake::configured(
            left,
            x25519_public_key(right),
            Some(psk),
            4,
            Duration::ZERO,
            17,
        )
        .unwrap();
        let mut responder = Handshake::configured(
            right,
            x25519_public_key(left),
            Some(psk),
            4,
            Duration::ZERO,
            31,
        )
        .unwrap();
        let initiation = initiator
            .initiate_inner(
                timestamp(wall()).unwrap(),
                array(row, "ephemeral_i"),
                Duration::ZERO,
            )
            .unwrap();
        assert_eq!(initiation, binary(row, "initiation"));
        let pending = initiator.pending.as_ref().unwrap();
        assert_eq!(pending.noise.chain.as_slice(), binary(row, "initial_chain"));
        assert_eq!(pending.noise.hash.as_slice(), binary(row, "initial_hash"));
        framing(&initiation, 1, 148, x25519_public_key(right)).unwrap();
        let mut response = responder
            .respond_inner(&initiation, array(row, "ephemeral_r"), Duration::ZERO)
            .unwrap();
        assert_eq!(response.packet, binary(row, "response"));
        assert!(response.session.send(b"", 4, Duration::ZERO).is_err());
        let mut session = initiator
            .finish(&binary(row, "response"), 4, Duration::ZERO)
            .unwrap();
        let packet = session
            .send(b"reference proof!", 4, Duration::ZERO)
            .unwrap()
            .bytes;
        assert_eq!(packet, binary(row, "transport"));
        assert_eq!(
            response
                .session
                .receive(&binary(row, "transport"), 4, Duration::ZERO)
                .unwrap()
                .bytes,
            b"reference proof!"
        );
        let reply = response.session.send(b"back", 4, Duration::ZERO).unwrap();
        assert_eq!(
            &session
                .receive(&reply.bytes, 4, Duration::ZERO)
                .unwrap()
                .bytes[..4],
            b"back"
        );
    }
}

#[test]
fn os_random_handshakes_optional_psk_and_single_response_consumption() {
    for psk in [None, Some([53; 32])] {
        let (mut left, mut right) = pair(psk);
        let first = left.initiate(wall(), 4, Duration::ZERO).unwrap();
        let mut response = right.respond(&first, 4, Duration::ZERO).unwrap();
        let mut session = left.finish(&response.packet, 4, Duration::ZERO).unwrap();
        assert!(left.finish(&response.packet, 4, Duration::ZERO).is_err());
        let keepalive = session.send(b"", 4, Duration::ZERO).unwrap();
        response
            .session
            .receive(&keepalive.bytes, 4, Duration::ZERO)
            .unwrap();
        assert!(response.session.send(b"", 4, Duration::ZERO).is_ok());
        assert!(right.respond(&first, 4, Duration::ZERO).is_err());
        let next = left
            .initiate(wall() + Duration::from_nanos(1), 4, Duration::ZERO)
            .unwrap();
        assert_ne!(&first[8..40], &next[8..40]);
        assert_ne!(&first[4..8], &next[4..8]);
        assert!(right.respond(&next, 4, Duration::ZERO).is_ok());
    }
}

#[test]
fn every_packet_byte_tamper_preserves_pending_or_accepted_timestamp() {
    let (mut left, mut right) = pair(None);
    let initiation = left.initiate(wall(), 4, Duration::ZERO).unwrap();
    for index in 0..initiation.len() {
        let mut corrupted = initiation.clone();
        corrupted[index] ^= 1;
        assert!(
            right.respond(&corrupted, 4, Duration::ZERO).is_err(),
            "initiation byte {index}"
        );
        assert!(right.accepted.is_none());
    }
    let response = right.respond(&initiation, 4, Duration::ZERO).unwrap();
    for index in 0..response.packet.len() {
        let mut corrupted = response.packet.clone();
        corrupted[index] ^= 1;
        assert!(
            left.finish(&corrupted, 4, Duration::ZERO).is_err(),
            "response byte {index}"
        );
        assert!(left.pending.is_some());
    }
    assert!(left.finish(&response.packet, 4, Duration::ZERO).is_ok());
}

#[test]
fn publicly_recomputed_macs_do_not_authenticate_ciphertext_or_wrong_receiver() {
    let (mut left, mut right) = pair(None);
    let initiation = left.initiate(wall(), 4, Duration::ZERO).unwrap();
    let response = right.respond(&initiation, 4, Duration::ZERO).unwrap();
    for index in [8, 12, 25, 44, 59] {
        let mut corrupted = response.packet.clone();
        corrupted[index] ^= 1;
        remac(&mut corrupted, left.credentials.as_ref().unwrap().public);
        assert!(left.finish(&corrupted, 4, Duration::ZERO).is_err());
        assert!(left.pending.is_some());
    }
    let mut unknown = Handshake::configured(
        [61; 32],
        right.credentials.as_ref().unwrap().public,
        None,
        4,
        Duration::ZERO,
        70,
    )
    .unwrap();
    let unknown_packet = unknown
        .initiate(wall() + Duration::from_secs(1), 4, Duration::ZERO)
        .unwrap();
    assert!(right.respond(&unknown_packet, 4, Duration::ZERO).is_err());
    assert_eq!(right.accepted, Some(timestamp(wall()).unwrap()));
    assert!(left.finish(&response.packet, 4, Duration::ZERO).is_ok());
}

#[test]
fn wrong_psk_cannot_confirm_session_and_low_order_ephemerals_fail_without_commit() {
    let (mut left, mut right) = pair(Some([53; 32]));
    right.credentials.as_mut().unwrap().psk = [54; 32];
    let initiation = left.initiate(wall(), 4, Duration::ZERO).unwrap();
    let mut response = right.respond(&initiation, 4, Duration::ZERO).unwrap();
    assert!(left.finish(&response.packet, 4, Duration::ZERO).is_err());
    assert!(left.pending.is_some());
    assert!(response.session.send(b"", 4, Duration::ZERO).is_err());
    let (mut left, mut right) = pair(None);
    let original = left.initiate(wall(), 4, Duration::ZERO).unwrap();
    let mut corrupt = original.clone();
    corrupt[8..40].fill(0);
    remac(&mut corrupt, right.credentials.as_ref().unwrap().public);
    assert!(right.respond(&corrupt, 4, Duration::ZERO).is_err());
    assert!(right.accepted.is_none());
    let response = right.respond(&original, 4, Duration::ZERO).unwrap();
    let mut corrupt = response.packet.clone();
    corrupt[12..44].fill(0);
    remac(&mut corrupt, left.credentials.as_ref().unwrap().public);
    assert!(left.finish(&corrupt, 4, Duration::ZERO).is_err());
    assert!(left.pending.is_some());
    assert!(left.finish(&response.packet, 4, Duration::ZERO).is_ok());
    assert!(Handshake::new([17; 32], [0; 32], None, 4, Duration::ZERO).is_err());
}

#[test]
fn timestamp_encoding_strict_growth_and_invalid_timestamp_do_not_create_demand() {
    assert_eq!(
        timestamp(wall()).unwrap(),
        [0x40, 0, 0, 0, 0, 0, 0, 0x85, 0, 0, 1, 0xc8]
    );
    assert!(timestamp(UNIX_EPOCH - Duration::from_secs(1)).is_err());
    let (mut left, mut right) = pair(None);
    let first = left.initiate(wall(), 4, Duration::ZERO).unwrap();
    assert!(left.initiate(wall(), 4, Duration::ZERO).is_err());
    assert!(
        left.initiate(wall() - Duration::from_secs(1), 4, Duration::ZERO)
            .is_err()
    );
    assert!(left.pending.is_some());
    right.respond(&first, 4, Duration::ZERO).unwrap();
    let replay = left
        .initiate_inner(timestamp(wall()).unwrap(), [37; 32], Duration::ZERO)
        .unwrap();
    assert!(right.respond(&replay, 4, Duration::ZERO).is_err());
    let mut invalid = timestamp(wall() + Duration::from_secs(1)).unwrap();
    invalid[8..].copy_from_slice(&1_000_000_000u32.to_be_bytes());
    let packet = left
        .initiate_inner(invalid, [38; 32], Duration::ZERO)
        .unwrap();
    assert!(right.respond(&packet, 4, Duration::ZERO).is_err());
    assert_eq!(right.accepted, Some(timestamp(wall()).unwrap()));
}

#[test]
fn monotonic_expiry_generation_revoke_and_pending_replacement_are_terminal() {
    for mode in 0..4 {
        let (mut left, mut right) = pair(None);
        let start = Duration::from_secs(1000);
        let initiation = left.initiate(wall(), 4, start).unwrap();
        let response = right.respond(&initiation, 4, start).unwrap();
        match mode {
            0 => assert!(
                left.finish(&response.packet, 4, start + PENDING_AGE)
                    .is_err()
            ),
            1 => assert!(left.finish(&response.packet, 5, start).is_err()),
            2 => assert!(
                left.finish(&response.packet, 4, start - Duration::from_secs(1))
                    .is_err()
            ),
            _ => left.revoke(),
        }
        assert!(left.pending.is_none());
        assert!(
            left.finish(&response.packet, 4, start + PENDING_AGE)
                .is_err()
        );
    }
    let (mut left, mut right) = pair(None);
    let first = left.initiate(wall(), 4, Duration::ZERO).unwrap();
    let first_response = right.respond(&first, 4, Duration::ZERO).unwrap();
    let second = left
        .initiate(wall() + Duration::from_nanos(1), 4, Duration::ZERO)
        .unwrap();
    assert!(
        left.finish(&first_response.packet, 4, Duration::ZERO)
            .is_err()
    );
    let response = right.respond(&second, 4, Duration::ZERO).unwrap();
    assert!(
        left.finish(&response.packet, 4, PENDING_AGE - Duration::from_nanos(1))
            .is_ok()
    );
}

#[test]
fn framing_cookie_and_index_exhaustion_fail_before_state_publication() {
    let (mut left, mut right) = pair(None);
    for size in [0, 1, 4, 147, 149, 4096] {
        assert!(right.respond(&vec![0; size], 4, Duration::ZERO).is_err());
    }
    let first = left.initiate(wall(), 4, Duration::ZERO).unwrap();
    let mut cookie = first.clone();
    cookie[147] = 1;
    assert!(right.respond(&cookie, 4, Duration::ZERO).is_err());
    right.next_index = u32::MAX;
    assert!(right.respond(&first, 4, Duration::ZERO).is_err());
    assert!(right.accepted.is_none());
    left.next_index = u32::MAX;
    assert!(
        left.initiate(wall() + Duration::from_secs(1), 4, Duration::ZERO)
            .is_err()
    );
    assert_eq!(left.issued, Some(timestamp(wall()).unwrap()));
}
