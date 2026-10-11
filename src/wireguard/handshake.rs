//! Noise IKpsk2 equations from the WireGuard protocol, independently authored.
use super::{Role, Session, nonce};
use crate::Result;
use crate::crypto::{
    ChaCha20Poly1305, blake2s, constant_time_eq, hkdf_blake2s_expand, hkdf_blake2s_extract,
    keyed_blake2s, random_bytes, x25519, x25519_public_key,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CONSTRUCTION: &[u8] = b"Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";
const IDENTIFIER: &[u8] = b"WireGuard v1 zx2c4 Jason@zx2c4.com";
const PENDING_AGE: Duration = Duration::from_secs(5);
const TAI_OFFSET: u64 = 0x400000000000000a;

struct Credentials {
    secret: [u8; 32],
    public: [u8; 32],
    peer: [u8; 32],
    psk: [u8; 32],
}

struct Noise {
    chain: [u8; 32],
    hash: [u8; 32],
}

impl Noise {
    fn new(responder: [u8; 32]) -> Result<Self> {
        let chain = blake2s(CONSTRUCTION)?;
        let hash = hash_pair(&hash_pair(&chain, IDENTIFIER)?, &responder)?;
        Ok(Self { chain, hash })
    }
    fn mix_hash(&mut self, input: &[u8]) -> Result<()> {
        self.hash = hash_pair(&self.hash, input)?;
        Ok(())
    }
    fn mix<const N: usize>(&mut self, input: &[u8]) -> Result<[[u8; 32]; N]> {
        let output = derive::<N>(&self.chain, input)?;
        self.chain = output[0];
        Ok(output)
    }
    fn encrypt(&mut self, key: [u8; 32], plaintext: &[u8]) -> Result<Vec<u8>> {
        let ciphertext = CipherState::new(key).seal(&self.hash, plaintext)?;
        self.mix_hash(&ciphertext)?;
        Ok(ciphertext)
    }
    fn decrypt(&mut self, key: [u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>> {
        let plaintext = CipherState::new(key).open(&self.hash, ciphertext)?;
        self.mix_hash(ciphertext)?;
        Ok(plaintext)
    }
}

// Each mixed handshake key owns a fresh Noise CipherState, starting at counter
// zero. No caller supplies/reuses a nonce or resets a live key's counter.
struct CipherState {
    cipher: ChaCha20Poly1305,
    counter: u64,
}
impl CipherState {
    fn new(key: [u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(key),
            counter: u64::default(),
        }
    }
    fn seal(&mut self, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        let next = self
            .counter
            .checked_add(1)
            .ok_or("Noise: cipher counter exhausted")?;
        let encoded = nonce(self.counter);
        self.counter = next;
        self.cipher.seal(encoded, aad, plaintext)
    }
    fn open(&mut self, aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
        let next = self
            .counter
            .checked_add(1)
            .ok_or("Noise: cipher counter exhausted")?;
        let plaintext = self.cipher.open(nonce(self.counter), aad, ciphertext)?;
        self.counter = next;
        Ok(plaintext)
    }
}

struct Pending {
    noise: Noise,
    ephemeral: [u8; 32],
    index: u32,
    created: Duration,
}

/// A single pinned-peer handshake owner. Not Clone/Debug; no key exports. Its
/// bounded pending state is independent of already returned transport Sessions.
pub struct Handshake {
    credentials: Option<Credentials>,
    pending: Option<Pending>,
    issued: Option<[u8; 12]>,
    accepted: Option<[u8; 12]>,
    next_index: u32,
    generation: u64,
    observed: Duration,
}

pub struct Response {
    pub packet: Vec<u8>,
    pub session: Session,
}

impl Handshake {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "future private profile/driver integration")
    )]
    pub(crate) fn new(
        secret: [u8; 32],
        peer: [u8; 32],
        psk: Option<[u8; 32]>,
        generation: u64,
        now: Duration,
    ) -> Result<Self> {
        let initial = u32::from_le_bytes(random_bytes::<4>()?);
        Self::configured(secret, peer, psk, generation, now, initial)
    }

    fn configured(
        secret: [u8; 32],
        peer: [u8; 32],
        psk: Option<[u8; 32]>,
        generation: u64,
        now: Duration,
        initial: u32,
    ) -> Result<Self> {
        x25519(secret, peer)?;
        let public = x25519_public_key(secret);
        if constant_time_eq(&public, &peer) {
            return Err("WireGuard: peer identity equals local identity".into());
        }
        Ok(Self {
            credentials: Some(Credentials {
                secret,
                public,
                peer,
                psk: psk.unwrap_or_default(),
            }),
            pending: None,
            issued: None,
            accepted: None,
            next_index: initial,
            generation,
            observed: now,
        })
    }

    /// The future driver must also revoke every returned Session and serialize
    /// final network admission. Dropping this owner cannot revoke those objects.
    pub fn revoke(&mut self) {
        self.credentials = None;
        self.pending = None;
    }

    fn admit(&mut self, generation: u64, now: Duration) -> Result<()> {
        if self.credentials.is_none() {
            return Err("WireGuard: handshake owner is revoked".into());
        }
        if generation != self.generation || now < self.observed {
            self.revoke();
            return Err("WireGuard: handshake generation or clock changed".into());
        }
        self.observed = now;
        Ok(())
    }

    fn index(&mut self) -> Result<u32> {
        let index = self.next_index;
        self.next_index = index
            .checked_add(1)
            .ok_or("WireGuard: local index space exhausted")?;
        Ok(index)
    }

    pub fn initiate(
        &mut self,
        wall: SystemTime,
        generation: u64,
        now: Duration,
    ) -> Result<Vec<u8>> {
        self.admit(generation, now)?;
        let timestamp = timestamp(wall)?;
        if self.issued.is_some_and(|previous| timestamp <= previous) {
            return Err("WireGuard: initiation clock did not advance".into());
        }
        let ephemeral = random_bytes::<32>()?;
        self.initiate_inner(timestamp, ephemeral, now)
    }

    fn initiate_inner(
        &mut self,
        timestamp: [u8; 12],
        ephemeral: [u8; 32],
        now: Duration,
    ) -> Result<Vec<u8>> {
        let index = self.index()?;
        let keys = self.credentials.as_ref().unwrap();
        let mut noise = Noise::new(keys.peer)?;
        let public = x25519_public_key(ephemeral);
        noise.mix_hash(&public)?;
        noise.mix::<1>(&public)?;
        let key = noise.mix::<2>(&x25519(ephemeral, keys.peer)?)?[1];
        let identity = noise.encrypt(key, &keys.public)?;
        let key = noise.mix::<2>(&x25519(keys.secret, keys.peer)?)?[1];
        let encrypted_time = noise.encrypt(key, &timestamp)?;
        let mut packet = header(1, index);
        packet.extend_from_slice(&public);
        packet.extend_from_slice(&identity);
        packet.extend_from_slice(&encrypted_time);
        append_macs(&mut packet, keys.peer)?;
        self.pending = Some(Pending {
            noise,
            ephemeral,
            index,
            created: now,
        });
        self.issued = Some(timestamp);
        Ok(packet)
    }

    pub fn respond(&mut self, packet: &[u8], generation: u64, now: Duration) -> Result<Response> {
        self.admit(generation, now)?;
        let keys = self.credentials.as_ref().unwrap();
        framing(packet, 1, 148, keys.public)?;
        let ephemeral = random_bytes::<32>()?;
        self.respond_inner(packet, ephemeral, now)
    }

    fn respond_inner(
        &mut self,
        packet: &[u8],
        ephemeral: [u8; 32],
        now: Duration,
    ) -> Result<Response> {
        let keys = self.credentials.as_ref().unwrap();
        let remote_index = u32::from_le_bytes(packet[4..8].try_into().unwrap());
        let remote_ephemeral: [u8; 32] = packet[8..40].try_into().unwrap();
        let mut noise = Noise::new(keys.public)?;
        noise.mix_hash(&remote_ephemeral)?;
        noise.mix::<1>(&remote_ephemeral)?;
        let key = noise.mix::<2>(&x25519(keys.secret, remote_ephemeral)?)?[1];
        let identity = noise.decrypt(key, &packet[40..88])?;
        if !constant_time_eq(&identity, &keys.peer) {
            return Err("WireGuard: unrecognized peer identity".into());
        }
        let key = noise.mix::<2>(&x25519(keys.secret, keys.peer)?)?[1];
        let decoded = noise.decrypt(key, &packet[88..116])?;
        let timestamp: [u8; 12] = decoded
            .try_into()
            .map_err(|_| "WireGuard: invalid timestamp size")?;
        validate_timestamp(timestamp)?;
        if self.accepted.is_some_and(|previous| timestamp <= previous) {
            return Err("WireGuard: replayed initiation timestamp".into());
        }
        let public = x25519_public_key(ephemeral);
        noise.mix_hash(&public)?;
        noise.mix::<1>(&public)?;
        noise.mix::<1>(&x25519(ephemeral, remote_ephemeral)?)?;
        noise.mix::<1>(&x25519(ephemeral, keys.peer)?)?;
        let output = noise.mix::<3>(&keys.psk)?;
        noise.mix_hash(&output[1])?;
        let empty = noise.encrypt(output[2], &[])?;
        let directional = derive::<2>(&noise.chain, &[])?;
        let peer = keys.peer;
        let index = self.index()?;
        let mut response = header(2, index);
        response.extend_from_slice(&remote_index.to_le_bytes());
        response.extend_from_slice(&public);
        response.extend_from_slice(&empty);
        append_macs(&mut response, peer)?;
        let session = Session::from_handshake(
            [directional[1], directional[0]],
            [index, remote_index],
            Role::Responder,
            self.generation,
            now,
        );
        self.accepted = Some(timestamp);
        Ok(Response {
            packet: response,
            session,
        })
    }

    pub fn finish(&mut self, packet: &[u8], generation: u64, now: Duration) -> Result<Session> {
        self.admit(generation, now)?;
        let pending = self
            .pending
            .as_ref()
            .ok_or("WireGuard: no pending initiation")?;
        if now - pending.created >= PENDING_AGE {
            self.pending = None;
            return Err("WireGuard: pending initiation expired".into());
        }
        let keys = self.credentials.as_ref().unwrap();
        framing(packet, 2, 92, keys.public)?;
        if u32::from_le_bytes(packet[8..12].try_into().unwrap()) != pending.index {
            return Err("WireGuard: response receiver index mismatch".into());
        }
        let remote_index = u32::from_le_bytes(packet[4..8].try_into().unwrap());
        let public: [u8; 32] = packet[12..44].try_into().unwrap();
        // Speculative transcript: unauthenticated input never consumes pending.
        let mut noise = Noise {
            chain: pending.noise.chain,
            hash: pending.noise.hash,
        };
        noise.mix_hash(&public)?;
        noise.mix::<1>(&public)?;
        noise.mix::<1>(&x25519(pending.ephemeral, public)?)?;
        noise.mix::<1>(&x25519(keys.secret, public)?)?;
        let output = noise.mix::<3>(&keys.psk)?;
        noise.mix_hash(&output[1])?;
        let empty = noise.decrypt(output[2], &packet[44..60])?;
        if !empty.is_empty() {
            return Err("WireGuard: response confirmation is not empty".into());
        }
        let directional = derive::<2>(&noise.chain, &[])?;
        let session = Session::from_handshake(
            directional,
            [pending.index, remote_index],
            Role::Initiator,
            self.generation,
            now,
        );
        self.pending = None;
        Ok(session)
    }
}

fn timestamp(wall: SystemTime) -> Result<[u8; 12]> {
    let elapsed = wall
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "WireGuard: wall clock precedes Unix epoch")?;
    let seconds = elapsed
        .as_secs()
        .checked_add(TAI_OFFSET)
        .ok_or("WireGuard: timestamp seconds overflow")?;
    let mut encoded = [0; 12];
    encoded[..8].copy_from_slice(&seconds.to_be_bytes());
    encoded[8..].copy_from_slice(&elapsed.subsec_nanos().to_be_bytes());
    Ok(encoded)
}

fn validate_timestamp(timestamp: [u8; 12]) -> Result<()> {
    if u64::from_be_bytes(timestamp[..8].try_into().unwrap()) < TAI_OFFSET
        || u32::from_be_bytes(timestamp[8..].try_into().unwrap()) >= 1_000_000_000
    {
        return Err("WireGuard: invalid TAI64N timestamp".into());
    }
    Ok(())
}

fn header(kind: u8, index: u32) -> Vec<u8> {
    let mut header = vec![kind, 0, 0, 0];
    header.extend_from_slice(&index.to_le_bytes());
    header
}
fn hash_pair(left: &[u8], right: &[u8]) -> Result<[u8; 32]> {
    let mut input = Vec::with_capacity(left.len() + right.len());
    input.extend_from_slice(left);
    input.extend_from_slice(right);
    blake2s(&input)
}
fn derive<const N: usize>(chain: &[u8; 32], input: &[u8]) -> Result<[[u8; 32]; N]> {
    let prk = hkdf_blake2s_extract(chain, input)?;
    let bytes = hkdf_blake2s_expand(&prk, &[], N * 32)?;
    Ok(std::array::from_fn(|index| {
        bytes[index * 32..(index + 1) * 32].try_into().unwrap()
    }))
}
fn append_macs(packet: &mut Vec<u8>, recipient: [u8; 32]) -> Result<()> {
    let mac = keyed_blake2s(&hash_pair(b"mac1----", &recipient)?, packet, 16)?;
    packet.extend_from_slice(&mac);
    packet.extend_from_slice(&[0; 16]);
    Ok(())
}
fn framing(packet: &[u8], kind: u8, length: usize, recipient: [u8; 32]) -> Result<()> {
    if packet.len() != length || packet[..4] != [kind, 0, 0, 0] {
        return Err("WireGuard: invalid handshake framing".into());
    }
    if packet[length - 16..].iter().any(|byte| *byte != 0) {
        return Err("WireGuard: cookies are not supported in this increment".into());
    }
    let offset = length - 32;
    let expected = keyed_blake2s(&hash_pair(b"mac1----", &recipient)?, &packet[..offset], 16)?;
    if !constant_time_eq(&expected, &packet[offset..offset + 16]) {
        return Err("WireGuard: invalid handshake MAC1".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "handshake_tests.rs"]
mod tests;
