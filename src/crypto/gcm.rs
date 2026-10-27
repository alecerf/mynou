//! AES-128 (FIPS 197) in GCM mode (NIST SP 800-38D), for TLS 1.2 servers that
//! offer no ChaCha20-Poly1305.
//!
//! The S-box is computed as an inversion in GF(2^8) followed by the affine map,
//! and GHASH multiplies bit by bit with masks: no table is indexed by secret
//! data. That costs speed; TLS 1.2 here carries tracker and indexer traffic.

use crate::Result;

/// Plaintext bound of SP 800-38D for a 96-bit nonce: 2^39 - 256 bits.
const MAX_PLAINTEXT: u64 = (1 << 36) - 32;

#[derive(Clone)]
pub struct Aes128Gcm {
    round_keys: [[u8; 16]; 11],
    hash_key: u128,
}

impl Aes128Gcm {
    pub fn new(key: [u8; 16]) -> Self {
        let round_keys = expand_key(key);
        let hash_key = u128::from_be_bytes(encrypt_block(&round_keys, [0; 16]));
        Self {
            round_keys,
            hash_key,
        }
    }

    /// Return ciphertext followed by its 16-byte tag. A nonce must never be
    /// reused with this key.
    pub fn seal(&self, nonce: [u8; 12], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        check_length(plaintext.len())?;
        let mut ciphertext = Vec::with_capacity(plaintext.len() + 16);
        ciphertext.extend_from_slice(plaintext);
        self.ctr(&nonce, &mut ciphertext);
        let tag = self.tag(&nonce, aad, &ciphertext);
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
        if !super::constant_time_eq(&self.tag(&nonce, aad, ciphertext), tag) {
            return Err("AEAD: authentication failed".into());
        }
        let mut plaintext = ciphertext.to_vec();
        self.ctr(&nonce, &mut plaintext);
        Ok(plaintext)
    }

    /// CTR encryption from inc32(J0), where J0 = nonce || 1.
    fn ctr(&self, nonce: &[u8; 12], data: &mut [u8]) {
        for (index, chunk) in data.chunks_mut(16).enumerate() {
            let mut counter = [0; 16];
            counter[..12].copy_from_slice(nonce);
            // At most 2^32 - 2 blocks: the length check keeps the counter below wrap.
            counter[12..].copy_from_slice(&(index as u32 + 2).to_be_bytes());
            let stream = encrypt_block(&self.round_keys, counter);
            for (byte, key) in chunk.iter_mut().zip(stream) {
                *byte ^= key;
            }
        }
    }

    fn tag(&self, nonce: &[u8; 12], aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
        let mut hash = 0u128;
        for data in [aad, ciphertext] {
            for chunk in data.chunks(16) {
                let mut block = [0; 16];
                block[..chunk.len()].copy_from_slice(chunk);
                hash = multiply(hash ^ u128::from_be_bytes(block), self.hash_key);
            }
        }
        let lengths = ((aad.len() as u128 * 8) << 64) | (ciphertext.len() as u128 * 8);
        hash = multiply(hash ^ lengths, self.hash_key);
        let mut first = [0; 16];
        first[..12].copy_from_slice(nonce);
        first[15] = 1;
        let mask = u128::from_be_bytes(encrypt_block(&self.round_keys, first));
        (hash ^ mask).to_be_bytes()
    }
}

fn check_length(length: usize) -> Result<()> {
    if length as u64 > MAX_PLAINTEXT {
        return Err("AEAD: input exceeds the GCM limit".into());
    }
    Ok(())
}

/// Multiplication in GF(2^128) with the GCM bit order (SP 800-38D, algorithm 1).
fn multiply(x: u128, y: u128) -> u128 {
    const R: u128 = 0xe1 << 120;
    let mut product = 0u128;
    let mut v = y;
    for bit in (0..128).rev() {
        product ^= v & 0u128.wrapping_sub((x >> bit) & 1);
        v = (v >> 1) ^ (R & 0u128.wrapping_sub(v & 1));
    }
    product
}

/// Doubling in GF(2^8) modulo x^8 + x^4 + x^3 + x + 1.
fn xtime(value: u8) -> u8 {
    (value << 1) ^ (0x1b & 0u8.wrapping_sub(value >> 7))
}

fn field_multiply(mut a: u8, b: u8) -> u8 {
    let mut product = 0;
    for bit in 0..8 {
        product ^= a & 0u8.wrapping_sub((b >> bit) & 1);
        a = xtime(a);
    }
    product
}

/// The AES S-box: the inverse a^254 (zero maps to zero), then the affine map.
fn substitute(value: u8) -> u8 {
    let square = field_multiply(value, value);
    let cube = field_multiply(square, value);
    let sixth = field_multiply(cube, cube);
    let twelfth = field_multiply(sixth, sixth);
    let fifteenth = field_multiply(twelfth, cube);
    let thirtieth = field_multiply(fifteenth, fifteenth);
    let sixtieth = field_multiply(thirtieth, thirtieth);
    let hundred_twentieth = field_multiply(sixtieth, sixtieth);
    let hundred_twenty_sixth = field_multiply(hundred_twentieth, sixth);
    let two_hundred_fifty_second = field_multiply(hundred_twenty_sixth, hundred_twenty_sixth);
    let inverse = field_multiply(two_hundred_fifty_second, square);
    inverse
        ^ inverse.rotate_left(1)
        ^ inverse.rotate_left(2)
        ^ inverse.rotate_left(3)
        ^ inverse.rotate_left(4)
        ^ 0x63
}

fn expand_key(key: [u8; 16]) -> [[u8; 16]; 11] {
    let mut words = [[0u8; 4]; 44];
    for (word, chunk) in words.iter_mut().zip(key.chunks(4)) {
        word.copy_from_slice(chunk);
    }
    let mut constant = 1u8;
    for index in 4..44 {
        let mut word = words[index - 1];
        if index % 4 == 0 {
            word.rotate_left(1);
            for byte in &mut word {
                *byte = substitute(*byte);
            }
            word[0] ^= constant;
            constant = xtime(constant);
        }
        for (byte, earlier) in word.iter_mut().zip(words[index - 4]) {
            *byte ^= earlier;
        }
        words[index] = word;
    }
    let mut round_keys = [[0u8; 16]; 11];
    for (round, key) in round_keys.iter_mut().enumerate() {
        for column in 0..4 {
            key[column * 4..column * 4 + 4].copy_from_slice(&words[round * 4 + column]);
        }
    }
    round_keys
}

/// One block; the state is column-major, as the input bytes fill it.
fn encrypt_block(round_keys: &[[u8; 16]; 11], block: [u8; 16]) -> [u8; 16] {
    let mut state = block;
    add_round_key(&mut state, &round_keys[0]);
    for (round, key) in round_keys.iter().enumerate().skip(1) {
        for byte in &mut state {
            *byte = substitute(*byte);
        }
        let shifted = state;
        for row in 1..4 {
            for column in 0..4 {
                state[row + 4 * column] = shifted[row + 4 * ((column + row) % 4)];
            }
        }
        if round != 10 {
            for column in state.chunks_mut(4) {
                let [a, b, c, d] = [column[0], column[1], column[2], column[3]];
                let all = a ^ b ^ c ^ d;
                column[0] ^= all ^ xtime(a ^ b);
                column[1] ^= all ^ xtime(b ^ c);
                column[2] ^= all ^ xtime(c ^ d);
                column[3] ^= all ^ xtime(d ^ a);
            }
        }
        add_round_key(&mut state, key);
    }
    state
}

fn add_round_key(state: &mut [u8; 16], key: &[u8; 16]) {
    for (byte, key) in state.iter_mut().zip(key) {
        *byte ^= key;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn the_s_box_matches_fips_197() {
        // FIPS 197 figure 7: a few entries, including both ends of the table.
        for (input, output) in [(0x00, 0x63), (0x01, 0x7c), (0x53, 0xed), (0xff, 0x16)] {
            assert_eq!(substitute(input), output);
        }
    }

    #[test]
    fn aes_128_matches_fips_197_appendix_c() {
        let key: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let block: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
        assert_eq!(
            encrypt_block(&expand_key(key), block).to_vec(),
            hex("69c4e0d86a7b0430d8cdb78070b4c55a")
        );
    }

    #[test]
    fn gcm_matches_the_published_test_cases() {
        // The GCM specification's test cases 1, 2 and 4 (AES-128).
        let zero = Aes128Gcm::new([0; 16]);
        assert_eq!(
            zero.seal([0; 12], &[], &[]).unwrap(),
            hex("58e2fccefa7e3061367f1d57a4e7455a")
        );
        assert_eq!(
            zero.seal([0; 12], &[], &[0; 16]).unwrap(),
            hex("0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf")
        );
        let cipher = Aes128Gcm::new(hex("feffe9928665731c6d6a8f9467308308").try_into().unwrap());
        let nonce: [u8; 12] = hex("cafebabefacedbaddecaf888").try_into().unwrap();
        let aad = hex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let plaintext = hex(concat!(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72",
            "1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39"
        ));
        let sealed = cipher.seal(nonce, &aad, &plaintext).unwrap();
        assert_eq!(
            sealed,
            hex(concat!(
                "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e",
                "21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091",
                "5bc94fbc3221a5db94fae95ae7121a47"
            ))
        );
        assert_eq!(cipher.open(nonce, &aad, &sealed).unwrap(), plaintext);
        let mut forged = sealed.clone();
        forged[0] ^= 1;
        assert!(cipher.open(nonce, &aad, &forged).is_err());
        assert!(cipher.open(nonce, &aad[1..], &sealed).is_err());
    }
}
