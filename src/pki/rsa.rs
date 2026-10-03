use crate::Result;
use crate::crypto::{constant_time_eq, sha256, sha384};

/// Fixed-width Montgomery arithmetic for public-key verification. Inputs and
/// exponents are public; this is deliberately not a private-key API.
pub(super) struct Modulus {
    words: Vec<u32>,
    inverse: u32,
    r2: Vec<u32>,
}

impl Modulus {
    pub(super) fn new(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes[0] == 0 || bytes.last().unwrap() & 1 == 0 {
            return Err("Public modulus is noncanonical or even".into());
        }
        let words = from_bytes(bytes, bytes.len().div_ceil(4));
        let mut inverse = 1u32;
        for _ in 0..5 {
            inverse = inverse.wrapping_mul(2u32.wrapping_sub(words[0].wrapping_mul(inverse)));
        }
        let mut r2 = vec![0; words.len()];
        r2[0] = 1;
        for _ in 0..64 * words.len() {
            r2 = add_mod(&r2, &r2, &words);
        }
        Ok(Self {
            words,
            inverse: inverse.wrapping_neg(),
            r2,
        })
    }

    pub(super) fn width(&self) -> usize {
        self.words.len()
    }
    pub(super) fn encode(&self, bytes: &[u8]) -> Result<Vec<u32>> {
        if bytes.len() > self.words.len() * 4 {
            return Err("Public integer is too large".into());
        }
        let words = from_bytes(bytes, self.words.len());
        if compare(&words, &self.words) != std::cmp::Ordering::Less {
            return Err("Public integer is outside the modulus".into());
        }
        Ok(self.multiply(&words, &self.r2))
    }
    pub(super) fn one(&self) -> Vec<u32> {
        let mut one = vec![0; self.width()];
        one[0] = 1;
        self.multiply(&one, &self.r2)
    }
    pub(super) fn decode(&self, value: &[u32]) -> Vec<u32> {
        let mut one = vec![0; self.width()];
        one[0] = 1;
        self.multiply(value, &one)
    }
    pub(super) fn add(&self, a: &[u32], b: &[u32]) -> Vec<u32> {
        add_mod(a, b, &self.words)
    }
    pub(super) fn subtract(&self, a: &[u32], b: &[u32]) -> Vec<u32> {
        let (mut out, borrow) = subtract_words(a, b);
        if borrow != 0 {
            let mut carry = 0u64;
            for (word, modulus) in out.iter_mut().zip(&self.words) {
                let sum = u64::from(*word) + u64::from(*modulus) + carry;
                *word = sum as u32;
                carry = sum >> 32;
            }
        }
        out
    }
    pub(super) fn multiply(&self, a: &[u32], b: &[u32]) -> Vec<u32> {
        let n = self.words.len();
        let mut product = vec![0u32; 2 * n + 2];
        for i in 0..n {
            let mut carry = 0u64;
            for j in 0..n {
                let sum = u64::from(a[i]) * u64::from(b[j]) + u64::from(product[i + j]) + carry;
                product[i + j] = sum as u32;
                carry = sum >> 32;
            }
            add_carry(&mut product, i + n, carry);
        }
        for i in 0..n {
            let factor = product[i].wrapping_mul(self.inverse);
            let mut carry = 0u64;
            for j in 0..n {
                let sum = u64::from(factor) * u64::from(self.words[j])
                    + u64::from(product[i + j])
                    + carry;
                product[i + j] = sum as u32;
                carry = sum >> 32;
            }
            add_carry(&mut product, i + n, carry);
        }
        let mut out = product[n..2 * n].to_vec();
        if product[2 * n] != 0 || compare(&out, &self.words) != std::cmp::Ordering::Less {
            out = subtract_words(&out, &self.words).0;
        }
        out
    }
    pub(super) fn power(&self, base: &[u32], exponent: &[u8]) -> Vec<u32> {
        let mut result = self.one();
        for byte in exponent {
            for bit in (0..8).rev() {
                result = self.multiply(&result, &result);
                if byte & (1 << bit) != 0 {
                    result = self.multiply(&result, base);
                }
            }
        }
        result
    }
}

fn add_carry(words: &mut [u32], mut position: usize, mut carry: u64) {
    while carry != 0 {
        let sum = u64::from(words[position]) + carry;
        words[position] = sum as u32;
        carry = sum >> 32;
        position += 1;
    }
}

pub(super) fn compare(a: &[u32], b: &[u32]) -> std::cmp::Ordering {
    a.iter().rev().cmp(b.iter().rev())
}
fn subtract_words(a: &[u32], b: &[u32]) -> (Vec<u32>, u64) {
    let mut borrow = 0u64;
    let mut out = Vec::with_capacity(a.len());
    for (&a, &b) in a.iter().zip(b) {
        let subtrahend = u64::from(b) + borrow;
        out.push(u64::from(a).wrapping_sub(subtrahend) as u32);
        borrow = u64::from(u64::from(a) < subtrahend);
    }
    (out, borrow)
}
fn add_mod(a: &[u32], b: &[u32], modulus: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut carry = 0u64;
    for (&a, &b) in a.iter().zip(b) {
        let sum = u64::from(a) + u64::from(b) + carry;
        out.push(sum as u32);
        carry = sum >> 32;
    }
    if carry != 0 || compare(&out, modulus) != std::cmp::Ordering::Less {
        out = subtract_words(&out, modulus).0;
    }
    out
}
fn from_bytes(bytes: &[u8], width: usize) -> Vec<u32> {
    let mut out = vec![0; width];
    for (i, byte) in bytes.iter().rev().enumerate() {
        out[i / 4] |= u32::from(*byte) << (8 * (i % 4));
    }
    out
}
pub(super) fn to_bytes(words: &[u32], width: usize) -> Vec<u8> {
    (0..width)
        .rev()
        .map(|i| (words[i / 4] >> (8 * (i % 4))) as u8)
        .collect()
}

fn rsa_message(modulus: &[u8], exponent: &[u8], signature: &[u8]) -> Result<(Vec<u8>, usize)> {
    if !(256..=1024).contains(&modulus.len()) || modulus[0] < 0x80 {
        return Err("RSA: only 2048- to 8192-bit keys are accepted".into());
    }
    if exponent.is_empty() || exponent.len() > 4 || exponent[0] == 0 {
        return Err("RSA: invalid public exponent".into());
    }
    let value = exponent
        .iter()
        .fold(0u32, |n, byte| (n << 8) | u32::from(*byte));
    if value < 3 || value & 1 == 0 {
        return Err("RSA: invalid public exponent".into());
    }
    if signature.len() != modulus.len() {
        return Err("RSA: invalid signature length".into());
    }
    let arithmetic = Modulus::new(modulus)?;
    let signature = arithmetic.encode(signature)?;
    let decoded = arithmetic.decode(&arithmetic.power(&signature, exponent));
    Ok((to_bytes(&decoded, modulus.len()), modulus.len() * 8))
}

enum Digest {
    Sha256,
    Sha384,
}
impl Digest {
    fn hash(&self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha256 => sha256(bytes).to_vec(),
            Self::Sha384 => sha384(bytes).to_vec(),
        }
    }
    fn info(&self) -> &'static [u8] {
        match self {
            Self::Sha256 => &[
                0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x01, 0x05, 0x00, 0x04, 0x20,
            ],
            Self::Sha384 => &[
                0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x02, 0x05, 0x00, 0x04, 0x30,
            ],
        }
    }
}

/// Verify RSASSA-PKCS1-v1_5 using SHA-256 (RFC 8017). No signing is exposed.
pub fn verify_pkcs1_sha256(
    modulus: &[u8],
    exponent: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    verify_pkcs1(modulus, exponent, message, signature, Digest::Sha256)
}
/// Verify RSASSA-PKCS1-v1_5 using SHA-384 (RFC 8017).
pub fn verify_pkcs1_sha384(
    modulus: &[u8],
    exponent: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    verify_pkcs1(modulus, exponent, message, signature, Digest::Sha384)
}
fn verify_pkcs1(
    modulus: &[u8],
    exponent: &[u8],
    message: &[u8],
    signature: &[u8],
    digest: Digest,
) -> Result<()> {
    let (encoded, _) = rsa_message(modulus, exponent, signature)?;
    let hash = digest.hash(message);
    let info = digest.info();
    let padding_len = encoded.len() - info.len() - hash.len() - 3;
    let mut expected = vec![0xff; encoded.len()];
    expected[0] = 0;
    expected[1] = 1;
    expected[2 + padding_len] = 0;
    expected[3 + padding_len..3 + padding_len + info.len()].copy_from_slice(info);
    expected[encoded.len() - hash.len()..].copy_from_slice(&hash);
    if constant_time_eq(&encoded, &expected) {
        Ok(())
    } else {
        Err("RSA: invalid signature".into())
    }
}

/// Verify RSASSA-PSS SHA-256/MGF1-SHA-256, with the 32-byte TLS salt.
pub fn verify_pss_sha256(
    modulus: &[u8],
    exponent: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    verify_pss(modulus, exponent, message, signature, Digest::Sha256)
}
/// Verify RSASSA-PSS SHA-384/MGF1-SHA-384 with a 48-byte salt.
pub fn verify_pss_sha384(
    modulus: &[u8],
    exponent: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    verify_pss(modulus, exponent, message, signature, Digest::Sha384)
}
fn verify_pss(
    modulus: &[u8],
    exponent: &[u8],
    message: &[u8],
    signature: &[u8],
    digest: Digest,
) -> Result<()> {
    let (encoded, bits) = rsa_message(modulus, exponent, signature)?;
    let message_hash = digest.hash(message);
    let hash_len = message_hash.len();
    let em_bits = bits - 1;
    let em_len = em_bits.div_ceil(8);
    let db_len = em_len - hash_len - 1;
    if encoded.len() != em_len || encoded[em_len - 1] != 0xbc || encoded[0] & 0x80 != 0 {
        return Err("RSA-PSS: invalid encoding".into());
    }
    let hash = &encoded[db_len..db_len + hash_len];
    let mut db = encoded[..db_len].to_vec();
    for (counter, chunk) in db.chunks_mut(hash_len).enumerate() {
        let mut input = hash.to_vec();
        input.extend_from_slice(&(counter as u32).to_be_bytes());
        let mask = digest.hash(&input);
        for (out, mask) in chunk.iter_mut().zip(mask) {
            *out ^= mask;
        }
    }
    db[0] &= 0x7f;
    let padding_len = db_len - hash_len - 1;
    if db[..padding_len].iter().any(|byte| *byte != 0) || db[padding_len] != 1 {
        return Err("RSA-PSS: invalid padding".into());
    }
    let mut input = vec![0u8; 8];
    input.extend_from_slice(&message_hash);
    input.extend_from_slice(&db[padding_len + 1..]);
    if constant_time_eq(hash, &digest.hash(&input)) {
        Ok(())
    } else {
        Err("RSA-PSS: invalid signature".into())
    }
}

/// Test fixture signer only. Private-key operations do not exist in release code.
#[cfg(test)]
pub(crate) fn sign_pss_sha256_for_test(
    modulus: &[u8],
    private_exponent: &[u8],
    message: &[u8],
) -> Result<Vec<u8>> {
    let mut encoded = vec![0u8; modulus.len()];
    let db_len = encoded.len() - 33;
    let padding_len = db_len - 33;
    let salt = [0x42u8; 32];
    let mut input = [0u8; 72];
    input[8..40].copy_from_slice(&sha256(message));
    input[40..].copy_from_slice(&salt);
    let hash = sha256(&input);
    encoded[padding_len] = 1;
    encoded[padding_len + 1..db_len].copy_from_slice(&salt);
    for (counter, chunk) in encoded[..db_len].chunks_mut(32).enumerate() {
        let mut seed = [0u8; 36];
        seed[..32].copy_from_slice(&hash);
        seed[32..].copy_from_slice(&(counter as u32).to_be_bytes());
        for (out, mask) in chunk.iter_mut().zip(sha256(&seed)) {
            *out ^= mask;
        }
    }
    encoded[0] &= 0x7f;
    encoded[db_len..db_len + 32].copy_from_slice(&hash);
    encoded[db_len + 32] = 0xbc;
    let field = Modulus::new(modulus)?;
    let message = field.encode(&encoded)?;
    Ok(to_bytes(
        &field.decode(&field.power(&message, private_exponent)),
        modulus.len(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn montgomery_matches_small_integer_arithmetic() {
        for modulus in [
            3u64,
            17,
            65537,
            0xffff_ffff,
            0x0001_ffff_ffff,
            0xffff_ffff_ffff_ffc5,
        ] {
            let encoded = modulus.to_be_bytes();
            let first = encoded.iter().position(|byte| *byte != 0).unwrap();
            let field = Modulus::new(&encoded[first..]).unwrap();
            for a in [0, 1, 2, modulus / 2, modulus - 1] {
                for b in [0, 1, 2, modulus / 2, modulus - 1] {
                    let a_bytes = a.to_be_bytes();
                    let b_bytes = b.to_be_bytes();
                    let a_start = a_bytes.iter().position(|x| *x != 0).unwrap_or(8);
                    let b_start = b_bytes.iter().position(|x| *x != 0).unwrap_or(8);
                    let result = field.decode(&field.multiply(
                        &field.encode(&a_bytes[a_start..]).unwrap(),
                        &field.encode(&b_bytes[b_start..]).unwrap(),
                    ));
                    let output = to_bytes(&result, field.width() * 4);
                    let value = output
                        .iter()
                        .fold(0u128, |n, byte| (n << 8) | u128::from(*byte));
                    assert_eq!(value, (u128::from(a) * u128::from(b)) % u128::from(modulus));
                }
            }
        }
    }
}
