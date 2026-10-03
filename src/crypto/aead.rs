//! ChaCha20-Poly1305 AEAD from RFC 8439 (96-bit nonce, 128-bit tag).

use crate::Result;

#[derive(Clone)]
pub struct ChaCha20Poly1305 {
    key: [u8; 32],
}

impl ChaCha20Poly1305 {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// Return ciphertext followed by its 16-byte authentication tag.
    /// A nonce must never be reused with this key.
    pub fn seal(&self, nonce: [u8; 12], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        check_length(plaintext.len())?;
        let total = plaintext
            .len()
            .checked_add(16)
            .ok_or("AEAD: output size exceeds the limit")?;
        let mut ciphertext = Vec::with_capacity(total);
        ciphertext.extend_from_slice(plaintext);
        chacha_xor(&self.key, &nonce, &mut ciphertext);
        let tag = authenticate(&self.key, &nonce, aad, &ciphertext);
        ciphertext.extend_from_slice(&tag);
        Ok(ciphertext)
    }

    /// Authenticate before decrypting; errors never release unauthenticated data.
    pub fn open(&self, nonce: [u8; 12], aad: &[u8], ciphertext_and_tag: &[u8]) -> Result<Vec<u8>> {
        if ciphertext_and_tag.len() < 16 {
            return Err("AEAD: missing authentication tag".into());
        }
        let (ciphertext, tag) = ciphertext_and_tag.split_at(ciphertext_and_tag.len() - 16);
        check_length(ciphertext.len())?;
        let expected = authenticate(&self.key, &nonce, aad, ciphertext);
        if !super::constant_time_eq(&expected, tag) {
            return Err("AEAD: authentication failed".into());
        }
        let mut plaintext = ciphertext.to_vec();
        chacha_xor(&self.key, &nonce, &mut plaintext);
        Ok(plaintext)
    }
}

fn check_length(length: usize) -> Result<()> {
    // Counter 0 is reserved for the Poly1305 key; data uses 1 through 2^32 - 1.
    if (length as u128) > u128::from(u32::MAX) * 64 {
        Err("ChaCha20: counter space exhausted".into())
    } else {
        Ok(())
    }
}

fn quarter(words: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    words[a] = words[a].wrapping_add(words[b]);
    words[d] ^= words[a];
    words[d] = words[d].rotate_left(16);
    words[c] = words[c].wrapping_add(words[d]);
    words[b] ^= words[c];
    words[b] = words[b].rotate_left(12);
    words[a] = words[a].wrapping_add(words[b]);
    words[d] ^= words[a];
    words[d] = words[d].rotate_left(8);
    words[c] = words[c].wrapping_add(words[d]);
    words[b] ^= words[c];
    words[b] = words[b].rotate_left(7);
}

fn chacha_block(key: &[u8; 32], nonce: &[u8; 12], counter: u32) -> [u8; 64] {
    let mut words = [0u32; 16];
    words[..4].copy_from_slice(&[0x61707865, 0x3320646e, 0x79622d32, 0x6b206574]);
    for (word, bytes) in words[4..12].iter_mut().zip(key.as_chunks::<4>().0.iter()) {
        *word = u32::from_le_bytes(*bytes);
    }
    words[12] = counter;
    for (word, bytes) in words[13..].iter_mut().zip(nonce.as_chunks::<4>().0.iter()) {
        *word = u32::from_le_bytes(*bytes);
    }
    let initial = words;
    for _ in 0..10 {
        quarter(&mut words, 0, 4, 8, 12);
        quarter(&mut words, 1, 5, 9, 13);
        quarter(&mut words, 2, 6, 10, 14);
        quarter(&mut words, 3, 7, 11, 15);
        quarter(&mut words, 0, 5, 10, 15);
        quarter(&mut words, 1, 6, 11, 12);
        quarter(&mut words, 2, 7, 8, 13);
        quarter(&mut words, 3, 4, 9, 14);
    }
    let mut output = [0u8; 64];
    for (i, chunk) in output.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        chunk.copy_from_slice(&words[i].wrapping_add(initial[i]).to_le_bytes());
    }
    output
}

fn chacha_xor(key: &[u8; 32], nonce: &[u8; 12], bytes: &mut [u8]) {
    for (i, chunk) in bytes.chunks_mut(64).enumerate() {
        let block = chacha_block(key, nonce, (i as u32) + 1);
        for (byte, stream) in chunk.iter_mut().zip(block) {
            *byte ^= stream;
        }
    }
}

struct Poly1305 {
    r: [u64; 5],
    h: [u64; 5],
    pad: [u32; 4],
    buffer: [u8; 16],
    buffered: usize,
}

fn load32(bytes: &[u8]) -> u64 {
    u64::from(u32::from_le_bytes(
        bytes[..4].try_into().expect("Poly1305 word"),
    ))
}

impl Poly1305 {
    fn new(key: &[u8; 32]) -> Self {
        let mut r_bytes: [u8; 16] = key[..16].try_into().expect("Poly1305 key");
        for i in [3, 7, 11, 15] {
            r_bytes[i] &= 15;
        }
        for i in [4, 8, 12] {
            r_bytes[i] &= 252;
        }
        let r = [
            load32(&r_bytes) & 0x3ffffff,
            (load32(&r_bytes[3..]) >> 2) & 0x3ffffff,
            (load32(&r_bytes[6..]) >> 4) & 0x3ffffff,
            (load32(&r_bytes[9..]) >> 6) & 0x3ffffff,
            (load32(&r_bytes[12..]) >> 8) & 0x3ffffff,
        ];
        let pad = std::array::from_fn(|i| load32(&key[16 + i * 4..]) as u32);
        Self {
            r,
            h: [0; 5],
            pad,
            buffer: [0; 16],
            buffered: 0,
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        if self.buffered != 0 {
            let take = (16 - self.buffered).min(bytes.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&bytes[..take]);
            self.buffered += take;
            bytes = &bytes[take..];
            if self.buffered != 16 {
                return;
            }
            self.block(self.buffer, 1 << 24);
            self.buffered = 0;
        }
        let (chunks, tail) = bytes.as_chunks::<16>();
        for block in chunks {
            self.block(*block, 1 << 24);
        }
        self.buffer[..tail.len()].copy_from_slice(tail);
        self.buffered = tail.len();
    }

    fn block(&mut self, bytes: [u8; 16], high: u64) {
        self.h[0] += load32(&bytes) & 0x3ffffff;
        self.h[1] += (load32(&bytes[3..]) >> 2) & 0x3ffffff;
        self.h[2] += (load32(&bytes[6..]) >> 4) & 0x3ffffff;
        self.h[3] += (load32(&bytes[9..]) >> 6) & 0x3ffffff;
        self.h[4] += (load32(&bytes[12..]) >> 8) | high;
        let mut product = [0u64; 5];
        for i in 0..5 {
            for j in 0..5 {
                let degree = i + j;
                product[degree % 5] += self.h[i] * self.r[j] * if degree >= 5 { 5 } else { 1 };
            }
        }
        for i in 0..4 {
            let carry = product[i] >> 26;
            self.h[i] = product[i] & 0x3ffffff;
            product[i + 1] += carry;
        }
        self.h[4] = product[4] & 0x3ffffff;
        self.h[0] += (product[4] >> 26) * 5;
        let carry = self.h[0] >> 26;
        self.h[0] &= 0x3ffffff;
        self.h[1] += carry;
    }

    fn finalize(mut self) -> [u8; 16] {
        if self.buffered != 0 {
            self.buffer[self.buffered] = 1;
            self.buffer[self.buffered + 1..].fill(0);
            self.block(self.buffer, 0);
        }
        // Normalize carries before the conditional subtraction of 2^130 - 5.
        for i in 1..4 {
            let carry = self.h[i] >> 26;
            self.h[i] &= 0x3ffffff;
            self.h[i + 1] += carry;
        }
        let carry = self.h[4] >> 26;
        self.h[4] &= 0x3ffffff;
        self.h[0] += carry * 5;
        let carry = self.h[0] >> 26;
        self.h[0] &= 0x3ffffff;
        self.h[1] += carry;
        let mut reduced = self.h;
        reduced[0] += 5;
        for i in 0..4 {
            let carry = reduced[i] >> 26;
            reduced[i] &= 0x3ffffff;
            reduced[i + 1] += carry;
        }
        reduced[4] = reduced[4].wrapping_sub(1 << 26);
        let mask = (reduced[4] >> 63).wrapping_sub(1);
        for (h, g) in self.h.iter_mut().zip(reduced) {
            *h = (*h & !mask) | (g & mask);
        }
        let words = [
            self.h[0] | (self.h[1] << 26),
            (self.h[1] >> 6) | (self.h[2] << 20),
            (self.h[2] >> 12) | (self.h[3] << 14),
            (self.h[3] >> 18) | (self.h[4] << 8),
        ];
        let mut output = [0u8; 16];
        let mut carry = 0u64;
        for i in 0..4 {
            let sum = (words[i] & 0xffffffff) + u64::from(self.pad[i]) + carry;
            output[i * 4..i * 4 + 4].copy_from_slice(&(sum as u32).to_le_bytes());
            carry = sum >> 32;
        }
        output
    }
}

fn authenticate(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
    let first_block = chacha_block(key, nonce, 0);
    let poly_key = first_block[..32].try_into().expect("Poly1305 one-time key");
    let mut poly = Poly1305::new(&poly_key);
    poly.update(aad);
    let padding = [0u8; 16];
    poly.update(&padding[..(16 - aad.len() % 16) % 16]);
    poly.update(ciphertext);
    poly.update(&padding[..(16 - ciphertext.len() % 16) % 16]);
    poly.update(&(aad.len() as u64).to_le_bytes());
    poly.update(&(ciphertext.len() as u64).to_le_bytes());
    poly.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::tests::bytes;
    #[test]
    fn rfc8439_chacha_block() {
        let key: [u8; 32] = std::array::from_fn(|i| i as u8);
        let nonce = bytes("000000090000004a00000000").try_into().unwrap();
        assert_eq!(
            chacha_block(&key, &nonce, 1).as_slice(),
            bytes(
                "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4ed2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
            )
        );
    }
    #[test]
    fn rfc8439_poly1305() {
        let key = bytes("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b")
            .try_into()
            .unwrap();
        let message = b"Cryptographic Forum Research Group";
        for stride in [1, 3, 16, 17, 34] {
            let mut poly = Poly1305::new(&key);
            for chunk in message.chunks(stride) {
                poly.update(chunk);
            }
            assert_eq!(
                poly.finalize().as_slice(),
                bytes("a8061dc1305136c6c22b8baf0c0127a9")
            );
        }
    }
    #[test]
    fn rfc8439_poly1305_reduction_edges() {
        // RFC 8439 Appendix A.3, vectors 5 through 11 exercise carry and
        // canonical reduction independently of encryption/decryption roundtrips.
        let vectors = [
            (
                "02000000000000000000000000000000",
                "00000000000000000000000000000000",
                "ffffffffffffffffffffffffffffffff",
                "03000000000000000000000000000000",
            ),
            (
                "02000000000000000000000000000000",
                "ffffffffffffffffffffffffffffffff",
                "02000000000000000000000000000000",
                "03000000000000000000000000000000",
            ),
            (
                "01000000000000000000000000000000",
                "00000000000000000000000000000000",
                "fffffffffffffffffffffffffffffffff0ffffffffffffffffffffffffffffff11000000000000000000000000000000",
                "05000000000000000000000000000000",
            ),
            (
                "01000000000000000000000000000000",
                "00000000000000000000000000000000",
                "fffffffffffffffffffffffffffffffffbfefefefefefefefefefefefefefefe01010101010101010101010101010101",
                "00000000000000000000000000000000",
            ),
            (
                "02000000000000000000000000000000",
                "00000000000000000000000000000000",
                "fdffffffffffffffffffffffffffffff",
                "faffffffffffffffffffffffffffffff",
            ),
            (
                "01000000000000000400000000000000",
                "00000000000000000000000000000000",
                "e33594d7505e43b900000000000000003394d7505e4379cd01000000000000000000000000000000000000000000000001000000000000000000000000000000",
                "14000000000000005500000000000000",
            ),
            (
                "01000000000000000400000000000000",
                "00000000000000000000000000000000",
                "e33594d7505e43b900000000000000003394d7505e4379cd010000000000000000000000000000000000000000000000",
                "13000000000000000000000000000000",
            ),
        ];
        for (r, s, message, expected) in vectors {
            let key: [u8; 32] = bytes(&format!("{r}{s}")).try_into().unwrap();
            let message = bytes(message);
            for stride in [1, 7, 15, 16, 17, 64] {
                let mut poly = Poly1305::new(&key);
                for chunk in message.chunks(stride) {
                    poly.update(chunk);
                }
                assert_eq!(
                    poly.finalize().as_slice(),
                    bytes(expected),
                    "r={r}, message={message:?}, stride={stride}"
                );
            }
        }
        let mut zero_key = Poly1305::new(&[0; 32]);
        zero_key.update(&[0; 64]);
        assert_eq!(zero_key.finalize(), [0; 16]);
        let mut constant_key = [0u8; 32];
        constant_key[16..].fill(0x42);
        let mut poly = Poly1305::new(&constant_key);
        poly.update(b"Arbitrary nonzero input with r equal to zero");
        assert_eq!(poly.finalize(), [0x42; 16]);
    }

    #[test]
    fn rfc8439_aead() {
        let key = bytes("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f")
            .try_into()
            .unwrap();
        let nonce = bytes("070000004041424344454647").try_into().unwrap();
        let aad = bytes("50515253c0c1c2c3c4c5c6c7");
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let expected = bytes(
            "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d63dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b3692ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc3ff4def08e4b7a9de576d26586cec64b61161ae10b594f09e26a7e902ecbd0600691",
        );
        let aead = ChaCha20Poly1305::new(key);
        assert_eq!(aead.seal(nonce, &aad, plaintext).unwrap(), expected);
        assert_eq!(aead.open(nonce, &aad, &expected).unwrap(), plaintext);
        for index in 0..expected.len() {
            let mut corrupt = expected.clone();
            corrupt[index] ^= 1;
            assert!(aead.open(nonce, &aad, &corrupt).is_err());
        }
        assert!(
            aead.open(nonce, b"wrong associated data", &expected)
                .is_err()
        );
        assert!(aead.open(nonce, &aad, &[0u8; 15]).is_err());
    }
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn rejects_counter_exhaustion_without_allocating() {
        let maximum = u32::MAX as usize * 64;
        assert!(check_length(maximum).is_ok());
        assert!(check_length(maximum + 1).is_err());
        assert!(check_length(usize::MAX).is_err());
    }

    #[test]
    fn aead_empty_and_partial_blocks() {
        let cipher = ChaCha20Poly1305::new([42; 32]);
        for length in [0, 1, 15, 16, 17, 63, 64, 65, 1023] {
            for aad_length in [0, 1, 15, 16, 17] {
                let plaintext = vec![7; length];
                let aad = vec![9; aad_length];
                let mut nonce = [0u8; 12];
                nonce[..4].copy_from_slice(&(length as u32).to_le_bytes());
                nonce[4..8].copy_from_slice(&(aad_length as u32).to_le_bytes());
                let encrypted = cipher.seal(nonce, &aad, &plaintext).unwrap();
                assert_eq!(encrypted.len(), length + 16);
                assert_eq!(cipher.open(nonce, &aad, &encrypted).unwrap(), plaintext);
                let mut bad_nonce = nonce;
                bad_nonce[11] = 1;
                assert!(cipher.open(bad_nonce, &aad, &encrypted).is_err());
            }
        }
    }
}
