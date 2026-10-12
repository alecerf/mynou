//! Offline WireGuard handshake/transport; no sockets or VPN protection.
//! Session keys must come from a fresh authenticated handshake. See
//! docs/wireguard-transport.md for provenance, ownership and integration gates,
//! docs/wireguard-cookies.md for cookies and under-load admission, and
//! docs/wireguard-timers.md for retry, rekey and keepalive timers and key sets.

use crate::Result;
use crate::crypto::ChaCha20Poly1305;
use std::time::Duration;

#[path = "wireguard/cookie.rs"]
mod cookie;
#[path = "wireguard/handshake.rs"]
mod handshake;
#[path = "wireguard/timers.rs"]
mod timers;
pub use cookie::{
    Admission, Admit, COOKIE_LIFETIME, Entropy, Load, MAX_SOURCES, OsEntropy, SECRET_LIFETIME,
};
pub use handshake::{Handshake, Response};
pub use timers::{
    DISCARD_AFTER, Due, KEEPALIVE_TIMEOUT, KeySets, MAX_JITTER, REJECT_AFTER_TIME,
    REKEY_AFTER_TIME, REKEY_ATTEMPT_TIME, REKEY_ON_RECEIVE, REKEY_TIMEOUT, Received, Timers,
};

pub const MAX_PAYLOAD: usize = 1424;
pub const MAX_DATAGRAM: usize = MAX_PAYLOAD + 32;
pub const REKEY_AFTER_MESSAGES: u64 = 1 << 60;
pub const REJECT_AFTER_MESSAGES: u64 = u64::MAX - (1 << 13);
const WINDOW: u64 = 2048;
const REJECT_AGE: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Responder,
}

/// Accepted bytes plus a request to start a fresh handshake, not permission to
/// reset the current key's counter. Received bytes retain authenticated padding.
pub struct Accepted {
    pub bytes: Vec<u8>,
    pub rekey: bool,
}

/// Exclusively owned directional keys/counters. Neither Clone nor Debug exposes
/// or duplicates this state. Only a future crate-owned handshake can create it.
pub struct Session {
    keys: Option<(ChaCha20Poly1305, ChaCha20Poly1305)>,
    local_index: u32,
    remote_index: u32,
    role: Role,
    generation: u64,
    created: Duration,
    observed: Duration,
    sent: u64,
    confirmed: bool,
    replay: Replay,
}

impl Session {
    pub(crate) fn from_handshake(
        keys: [[u8; 32]; 2],
        indexes: [u32; 2],
        role: Role,
        generation: u64,
        now: Duration,
    ) -> Self {
        Self {
            keys: Some((
                ChaCha20Poly1305::new(keys[0]),
                ChaCha20Poly1305::new(keys[1]),
            )),
            local_index: indexes[0],
            remote_index: indexes[1],
            role,
            generation,
            created: now,
            observed: now,
            sent: 0,
            confirmed: role == Role::Initiator,
            replay: Replay::default(),
        }
    }

    /// Irreversible. Safe Rust does not guarantee memory zeroization of keys.
    pub fn revoke(&mut self) {
        self.keys = None;
        self.confirmed = false;
    }

    fn admit(&mut self, generation: u64, now: Duration) -> Result<Duration> {
        if self.keys.is_none() {
            return Err("WireGuard: session is revoked".into());
        }
        if generation != self.generation || now < self.observed {
            self.revoke();
            return Err("WireGuard: generation or monotonic clock changed".into());
        }
        self.observed = now;
        let age = now - self.created;
        if age >= REJECT_AGE {
            self.revoke();
            return Err("WireGuard: session expired".into());
        }
        Ok(age)
    }

    /// The caller supplies its own monotonic clock and captured generation, never
    /// values from a packet. Exclusive access must cover final driver admission.
    pub fn send(&mut self, payload: &[u8], generation: u64, now: Duration) -> Result<Accepted> {
        let age = self.admit(generation, now)?;
        if !self.confirmed {
            return Err("WireGuard: responder key is not confirmed".into());
        }
        if payload.len() > MAX_PAYLOAD {
            return Err("WireGuard: payload exceeds datagram budget".into());
        }
        if self.sent >= REJECT_AFTER_MESSAGES {
            self.revoke();
            return Err("WireGuard: send counter exhausted".into());
        }
        let counter = self.sent;
        // Reserve before encryption: an error can burn a nonce, never reuse it.
        self.sent += 1;
        let mut padded = vec![0; payload.len().div_ceil(16) * 16];
        padded[..payload.len()].copy_from_slice(payload);
        let ciphertext = self
            .keys
            .as_ref()
            .unwrap()
            .0
            .seal(nonce(counter), &[], &padded)?;
        let mut bytes = Vec::with_capacity(16 + ciphertext.len());
        bytes.extend_from_slice(&[4, 0, 0, 0]);
        bytes.extend_from_slice(&self.remote_index.to_le_bytes());
        bytes.extend_from_slice(&counter.to_le_bytes());
        bytes.extend_from_slice(&ciphertext);
        Ok(Accepted {
            bytes,
            rekey: self.sent >= REKEY_AFTER_MESSAGES
                || (self.role == Role::Initiator && age >= Duration::from_secs(120)),
        })
    }

    pub fn receive(&mut self, packet: &[u8], generation: u64, now: Duration) -> Result<Accepted> {
        let age = self.admit(generation, now)?;
        if !(32..=MAX_DATAGRAM).contains(&packet.len())
            || !packet.len().is_multiple_of(16)
            || packet[..4] != [4, 0, 0, 0]
            || u32::from_le_bytes(packet[4..8].try_into().unwrap()) != self.local_index
        {
            return Err("WireGuard: invalid transport framing".into());
        }
        let counter = u64::from_le_bytes(packet[8..16].try_into().unwrap());
        if counter >= REJECT_AFTER_MESSAGES || !self.replay.available(counter) {
            return Err("WireGuard: rejected transport counter".into());
        }
        let bytes = self
            .keys
            .as_ref()
            .unwrap()
            .1
            .open(nonce(counter), &[], &packet[16..])?;
        // No authentication failure can move the window or confirm a responder.
        self.replay.record(counter);
        self.confirmed = true;
        Ok(Accepted {
            bytes,
            rekey: self.role == Role::Initiator && age >= Duration::from_secs(165),
        })
    }
}

fn nonce(counter: u64) -> [u8; 12] {
    let encoded = counter.to_le_bytes();
    // WireGuard mandates a zero prefix; uniqueness comes from the owned counter.
    std::array::from_fn(|index| if index < 4 { 0 } else { encoded[index - 4] })
}

#[derive(Default)]
struct Replay {
    highest: Option<u64>,
    bits: [u64; 32],
}

impl Replay {
    fn available(&self, counter: u64) -> bool {
        if let Some(highest) = self.highest
            && counter <= highest
            && highest - counter >= WINDOW
        {
            return false;
        }
        let bit = (counter % WINDOW) as usize;
        self.highest.is_none_or(|highest| counter > highest)
            || self.bits[bit / 64] & (1 << (bit % 64)) == 0
    }

    fn record(&mut self, counter: u64) {
        if let Some(highest) = self.highest
            && counter > highest
        {
            if counter - highest >= WINDOW {
                self.bits.fill(0);
            } else {
                for expired in highest + 1..=counter {
                    let bit = (expired % WINDOW) as usize;
                    self.bits[bit / 64] &= !(1 << (bit % 64));
                }
            }
        }
        self.highest = Some(self.highest.map_or(counter, |highest| highest.max(counter)));
        let bit = (counter % WINDOW) as usize;
        self.bits[bit / 64] |= 1 << (bit % 64);
    }
}

#[cfg(test)]
#[path = "wireguard_tests.rs"]
mod tests;
