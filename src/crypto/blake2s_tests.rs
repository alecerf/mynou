use super::*;
use crate::json::{self, Value};

fn bytes(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2));
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}
fn binary(record: &Value, field: &str) -> Vec<u8> {
    bytes(record.get(field).unwrap().as_str().unwrap())
}
fn size(record: &Value) -> usize {
    record.get("size").unwrap().as_u64().unwrap() as usize
}
fn fixtures() -> Value {
    use std::io::Read;
    let path = std::env::var("MYNOU_CRYPTO_FIXTURES")
        .expect("CI must generate independent Python std reference vectors before running tests");
    let file = std::fs::File::open(path).unwrap();
    let mut data = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut data)
        .unwrap();
    assert!(data.len() <= 1024 * 1024);
    let value = json::parse(&data).unwrap();
    assert_eq!(value.get("schema").unwrap().as_u64(), Some(1));
    value
}

#[test]
fn rfc7693_public_digest_and_parameter_semantics() {
    assert_eq!(
        blake2s(b"abc").unwrap().as_slice(),
        bytes("508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982")
    );
    assert_eq!(
        blake2s(b"").unwrap().as_slice(),
        bytes("69217a3079908094e11121d042354a7c1f55b6482ca1a51e1b250dfd1ed0eef9")
    );
    assert_ne!(
        keyed_blake2s(b"synthetic key", b"abc", 16).unwrap(),
        keyed_blake2s(b"synthetic key", b"abc", 32).unwrap()[..16]
    );
    for output in [0, 33, usize::MAX] {
        assert!(Blake2s::new(output, &[]).is_err());
    }
    assert!(Blake2s::new(32, &[1; 33]).is_err());
    assert!(hkdf_blake2s_expand(&[0; 32], b"", 8161).is_err());
    assert!(hkdf_blake2s_expand(&[0; 32], b"", usize::MAX).is_err());
}

#[test]
fn independent_blake2s_streaming_and_keyed_reference_matrix() {
    let vectors = fixtures();
    let rows = vectors.get("blake2s").unwrap().as_array().unwrap();
    assert_eq!(rows.len(), 144);
    for record in rows {
        let key = binary(record, "key");
        let data = binary(record, "data");
        let expected = binary(record, "digest");
        assert_eq!(keyed_blake2s(&key, &data, size(record)).unwrap(), expected);
        for stride in [1, 7, 63, 64, 65, 129, 1024] {
            let mut hash = Blake2s::new(size(record), &key).unwrap();
            hash.update(&[]).unwrap();
            for chunk in data.chunks(stride) {
                hash.update(chunk).unwrap();
                hash.update(&[]).unwrap();
            }
            assert_eq!(
                hash.finalize().unwrap(),
                expected,
                "key bytes={}, data bytes={}, output={}, stride={stride}",
                key.len(),
                data.len(),
                size(record)
            );
        }
    }
}

#[test]
fn independent_hmac_and_hkdf_reference_matrices() {
    let vectors = fixtures();
    let hmacs = vectors.get("hmac").unwrap().as_array().unwrap();
    assert_eq!(hmacs.len(), 42);
    for record in hmacs {
        assert_eq!(
            hmac_blake2s(&binary(record, "key"), &binary(record, "data"))
                .unwrap()
                .as_slice(),
            binary(record, "digest")
        );
    }
    let derivations = vectors.get("hkdf").unwrap().as_array().unwrap();
    assert_eq!(derivations.len(), 96);
    for record in derivations {
        let prk = hkdf_blake2s_extract(&binary(record, "salt"), &binary(record, "data")).unwrap();
        assert_eq!(prk.as_slice(), binary(record, "prk"));
        assert_eq!(
            hkdf_blake2s_expand(&prk, &binary(record, "info"), size(record)).unwrap(),
            binary(record, "output")
        );
    }
}

#[test]
fn counter_carry_and_overflow_are_explicit() {
    let mut hash = Blake2s::new(32, &[]).unwrap();
    hash.count = u32::MAX as u64 - 63;
    hash.update(&[7; 65]).unwrap();
    assert_eq!(hash.count, u32::MAX as u64 + 1);
    assert_eq!(hash.buffered, 1);
    let mut full = Blake2s::new(16, b"key").unwrap();
    full.count = u64::MAX - 64;
    let state = full.state;
    let buffer = full.buffer;
    assert!(full.update(&[1]).is_err());
    assert_eq!(full.count, u64::MAX - 64);
    assert_eq!(full.state, state);
    assert_eq!(full.buffer, buffer);
    assert_eq!(full.buffered, 64);
    assert!(full.finalize().is_ok());
}
