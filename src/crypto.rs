//! Cryptographic primitives implemented with the Rust standard library.
//!
//! Algorithms follow FIPS 180-4 and 197, NIST SP 800-38D, RFCs 2104/5869/7693/7748/8017/8439 and
//! draft-irtf-cfrg-xchacha-03. Reference provenance: docs/crypto-vectors.md.
//! This implementation is exercised with public test vectors. It has not had an
//! independent cryptographic audit; no claim of formal constant-time execution
//! is made for code compiled by a general-purpose optimizer.

#[path = "crypto/aead.rs"]
mod aead;
#[path = "crypto/blake2s.rs"]
mod blake2s;
#[path = "crypto/curve.rs"]
mod curve;
#[path = "crypto/gcm.rs"]
mod gcm;
#[path = "crypto/hashes.rs"]
mod hashes;
// The one place allowed to use `unsafe`: see the module documentation.
#[cfg(all(target_arch = "aarch64", target_feature = "sha2"))]
#[allow(unsafe_code)]
#[path = "crypto/sha_arm.rs"]
mod sha_arm;

pub use aead::{ChaCha20Poly1305, XChaCha20Poly1305, hchacha20};
pub use blake2s::{
    Blake2s, blake2s, hkdf_blake2s_expand, hkdf_blake2s_extract, hmac_blake2s, keyed_blake2s,
};
pub use curve::{x25519, x25519_public_key};
pub use gcm::Aes128Gcm;
pub use hashes::{Sha1, Sha256, Sha384, Sha512, sha1, sha256, sha384, sha512};

use crate::Result;

/// Compare equal-length byte strings without an early return on their contents.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (&x, &y) in a.iter().zip(b) {
        difference |= x ^ y;
    }
    difference == 0
}

/// Obtain cryptographic random bytes from the operating system, never from a PRNG.
pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    use std::io::Read;
    let mut out = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut out))
        .map_err(|error| format!("System random source is unavailable: {error}"))?;
    Ok(out)
}

pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    hmac_parts(key, &[message])
}

fn hmac_parts(key: &[u8], messages: &[&[u8]]) -> [u8; 32] {
    let mut normalized = [0u8; 64];
    if key.len() > normalized.len() {
        normalized[..32].copy_from_slice(&sha256(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for i in 0..64 {
        inner_pad[i] ^= normalized[i];
        outer_pad[i] ^= normalized[i];
    }
    let mut inner = Sha256::new();
    inner.update(&inner_pad);
    for message in messages {
        inner.update(message);
    }
    let mut outer = Sha256::new();
    outer.update(&outer_pad);
    outer.update(&inner.finalize());
    outer.finalize()
}

/// The first 32-byte block of PBKDF2-HMAC-SHA256 (RFC 8018). The padded key
/// states are hashed once, so each iteration costs two compressions.
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut normalized = [0u8; 64];
    if password.len() > normalized.len() {
        normalized[..32].copy_from_slice(&sha256(password));
    } else {
        normalized[..password.len()].copy_from_slice(password);
    }
    let mut inner = Sha256::new();
    let mut outer = Sha256::new();
    inner.update(&normalized.map(|b| b ^ 0x36));
    outer.update(&normalized.map(|b| b ^ 0x5c));
    let mac = |message: &[&[u8]]| {
        let mut hash = inner.clone();
        message.iter().for_each(|part| hash.update(part));
        let mut result = outer.clone();
        result.update(&hash.finalize());
        result.finalize()
    };
    let mut block = mac(&[salt, &1u32.to_be_bytes()]);
    let mut output = block;
    for _ in 1..iterations {
        block = mac(&[&block]);
        output.iter_mut().zip(block).for_each(|(o, b)| *o ^= b);
    }
    output
}

pub fn hkdf_extract(salt: &[u8], input_key_material: &[u8]) -> [u8; 32] {
    hmac_sha256(salt, input_key_material)
}

pub fn hkdf_expand(prk: &[u8], info: &[u8], length: usize) -> Result<Vec<u8>> {
    if prk.len() < 32 {
        return Err("HKDF: extracted key is too short".into());
    }
    if length > 255 * 32 {
        return Err("HKDF: output exceeds 8160 bytes".into());
    }
    let mut output = Vec::with_capacity(length);
    let mut block = [0u8; 32];
    for counter in 1..=length.div_ceil(32) {
        let previous: &[u8] = if counter == 1 { &[] } else { &block };
        block = hmac_parts(prk, &[previous, info, &[counter as u8]]);
        let take = (length - output.len()).min(32);
        output.extend_from_slice(&block[..take]);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(crate) fn bytes(hex: &str) -> Vec<u8> {
        assert_eq!(hex.len() % 2, 0);
        hex.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let s = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(s, 16).unwrap()
            })
            .collect()
    }
    #[test]
    fn hmac_rfc4231() {
        assert_eq!(
            hmac_sha256(&[0x0b; 20], b"Hi There").as_slice(),
            bytes("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
        );
        assert_eq!(
            hmac_sha256(b"Jefe", b"what do ya want for nothing?").as_slice(),
            bytes("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
        assert_eq!(
            hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )
            .as_slice(),
            bytes("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")
        );
    }
    #[test]
    fn pbkdf2_rfc7914() {
        assert_eq!(
            pbkdf2_sha256(b"passwd", b"salt", 1).as_slice(),
            bytes("55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc")
        );
        assert_eq!(
            pbkdf2_sha256(b"Password", b"NaCl", 80000).as_slice(),
            bytes("4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56")
        );
        // A password longer than a block is hashed first, as in HMAC.
        assert_eq!(
            pbkdf2_sha256(&[b'p'; 100], b"salt", 2).as_slice(),
            bytes("7fb39a0c2291de62231e50ab5f6805b83bab97446d73dccf38114fb21c055427")
        );
    }
    #[test]
    fn hkdf_rfc5869() {
        let ikm = [0x0b; 22];
        let salt = bytes("000102030405060708090a0b0c");
        let info = bytes("f0f1f2f3f4f5f6f7f8f9");
        let prk = hkdf_extract(&salt, &ikm);
        assert_eq!(
            prk.as_slice(),
            bytes("077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5")
        );
        assert_eq!(
            hkdf_expand(&prk, &info, 42).unwrap(),
            bytes(
                "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
            )
        );
        let prk = hkdf_extract(&[], &ikm);
        assert_eq!(
            prk.as_slice(),
            bytes("19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04")
        );
        assert_eq!(
            hkdf_expand(&prk, &[], 42).unwrap(),
            bytes(
                "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"
            )
        );
        assert!(hkdf_expand(&prk, &[], 8161).is_err());
    }
    #[test]
    fn secure_random_and_comparison() {
        let a = random_bytes::<32>().unwrap();
        let b = random_bytes::<32>().unwrap();
        assert_ne!(a, b);
        assert!(constant_time_eq(&a, &a));
        assert!(!constant_time_eq(&a, &b));
        assert!(!constant_time_eq(&a, &a[..31]));
    }
}
