//! Sequential BLAKE2s from RFC 7693, with RFC 2104 HMAC and RFC 5869 HKDF.
//! Normative constants/equations, independently authored implementation.

use crate::Result;

const IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];
const ORDER: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];
const LANES: [[usize; 4]; 8] = [
    [0, 4, 8, 12],
    [1, 5, 9, 13],
    [2, 6, 10, 14],
    [3, 7, 11, 15],
    [0, 5, 10, 15],
    [1, 6, 11, 12],
    [2, 7, 8, 13],
    [3, 4, 9, 14],
];

/// Fixed-size streaming state. No Debug/Clone or guaranteed secret zeroization.
/// Only sequential hashing is supported: no salt, personalization or tree mode.
pub struct Blake2s {
    state: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    count: u64,
    output: usize,
}

impl Blake2s {
    /// Output length is part of the hash parameters, never digest truncation.
    pub fn new(output: usize, key: &[u8]) -> Result<Self> {
        if !(1..=32).contains(&output) || key.len() > 32 {
            return Err("BLAKE2s: invalid output or key length".into());
        }
        let mut state = IV;
        state[0] ^= 0x01010000 | ((key.len() as u32) << 8) | output as u32;
        let mut buffer = [0; 64];
        buffer[..key.len()].copy_from_slice(key);
        Ok(Self {
            state,
            buffer,
            buffered: if key.is_empty() { 0 } else { 64 },
            count: 0,
            output,
        })
    }

    /// Overflow is rejected before changing state; empty updates preserve the
    /// final full block, including a padded key with no message bytes.
    pub fn update(&mut self, mut data: &[u8]) -> Result<()> {
        self.count
            .checked_add(self.buffered as u64)
            .and_then(|length| length.checked_add(data.len() as u64))
            .ok_or("BLAKE2s: byte counter exhausted")?;
        while !data.is_empty() {
            if self.buffered == 64 {
                self.count += 64;
                self.compress(false);
                self.buffered = 0;
            }
            let take = (64 - self.buffered).min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
        }
        Ok(())
    }

    pub fn finalize(mut self) -> Result<Vec<u8>> {
        self.count = self
            .count
            .checked_add(self.buffered as u64)
            .ok_or("BLAKE2s: byte counter exhausted")?;
        self.buffer[self.buffered..].fill(0);
        self.compress(true);
        Ok(self
            .state
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .take(self.output)
            .collect())
    }

    fn compress(&mut self, last: bool) {
        let mut message = [0u32; 16];
        for (word, bytes) in message.iter_mut().zip(self.buffer.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*bytes);
        }
        let mut working = [0; 16];
        working[..8].copy_from_slice(&self.state);
        working[8..].copy_from_slice(&IV);
        working[12] ^= self.count as u32;
        working[13] ^= (self.count >> 32) as u32;
        if last {
            working[14] = !working[14];
        }
        for order in ORDER {
            for (index, [a, b, c, d]) in LANES.into_iter().enumerate() {
                for (word, rotation) in
                    [(order[2 * index], [16, 12]), (order[2 * index + 1], [8, 7])]
                {
                    working[a] = working[a]
                        .wrapping_add(working[b])
                        .wrapping_add(message[word]);
                    working[d] = (working[d] ^ working[a]).rotate_right(rotation[0]);
                    working[c] = working[c].wrapping_add(working[d]);
                    working[b] = (working[b] ^ working[c]).rotate_right(rotation[1]);
                }
            }
        }
        for (index, state) in self.state.iter_mut().enumerate() {
            *state ^= working[index] ^ working[index + 8];
        }
    }
}

pub fn keyed_blake2s(key: &[u8], data: &[u8], output: usize) -> Result<Vec<u8>> {
    let mut hash = Blake2s::new(output, key)?;
    hash.update(data)?;
    hash.finalize()
}

fn hash_parts(parts: &[&[u8]]) -> Result<[u8; 32]> {
    let mut hash = Blake2s::new(32, &[])?;
    for part in parts {
        hash.update(part)?;
    }
    hash.finalize()?
        .try_into()
        .map_err(|_| "BLAKE2s: invalid digest length".into())
}

pub fn blake2s(data: &[u8]) -> Result<[u8; 32]> {
    hash_parts(&[data])
}

fn hmac_parts(key: &[u8], parts: &[&[u8]]) -> Result<[u8; 32]> {
    let mut normalized = [0; 64];
    if key.len() > 64 {
        normalized[..32].copy_from_slice(&blake2s(key)?);
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36; 64];
    let mut outer_pad = [0x5c; 64];
    for index in 0..64 {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Blake2s::new(32, &[])?;
    inner.update(&inner_pad)?;
    for part in parts {
        inner.update(part)?;
    }
    hash_parts(&[&outer_pad, &inner.finalize()?])
}

/// RFC 2104 HMAC, distinct from BLAKE2s keyed hashing.
pub fn hmac_blake2s(key: &[u8], data: &[u8]) -> Result<[u8; 32]> {
    hmac_parts(key, &[data])
}

pub fn hkdf_blake2s_extract(salt: &[u8], input: &[u8]) -> Result<[u8; 32]> {
    hmac_blake2s(salt, input)
}

/// A 32-byte PRK and at most 255 digest blocks (8160 bytes).
pub fn hkdf_blake2s_expand(prk: &[u8; 32], info: &[u8], length: usize) -> Result<Vec<u8>> {
    if length > 255 * 32 {
        return Err("HKDF-BLAKE2s: output exceeds 8160 bytes".into());
    }
    let mut output = Vec::with_capacity(length);
    let mut previous = [0; 32];
    let mut used = 0;
    for counter in 1..=length.div_ceil(32) {
        previous = hmac_parts(prk, &[&previous[..used], info, &[counter as u8]])?;
        used = 32;
        let take = (length - output.len()).min(32);
        output.extend_from_slice(&previous[..take]);
    }
    Ok(output)
}

#[cfg(test)]
#[path = "blake2s_tests.rs"]
mod tests;
