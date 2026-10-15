//! WireGuard cookie MACs, cookie replies and under-load admission, following the
//! whitepaper sections 5.3, 5.4.4, 5.4.7 and 6.6. Pure and offline: no sockets,
//! threads or clock reads. The caller passes a monotonic elapsed time and, for the
//! responder, the observed source address and its own load verdict.
//! See docs/wireguard-cookies.md for the boundary and the remaining obligations.

use super::handshake::hash_pair;
use crate::Result;
use crate::crypto::{XChaCha20Poly1305, constant_time_eq, keyed_blake2s, random_bytes};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// A received cookie is used for MAC2 only while it is younger than this.
pub const COOKIE_LIFETIME: Duration = Duration::from_secs(120);
/// The responder replaces its cookie secret once it is this old.
pub const SECRET_LIFETIME: Duration = Duration::from_secs(120);
/// Upper bound on distinct source prefixes tracked by the under-load limiter.
pub const MAX_SOURCES: usize = 1024;
// Repository policy, not a whitepaper constant: 20 admitted messages per second
// per source with a burst of five (the limiter tolerates four intervals of debt).
const SOURCE_INTERVAL: Duration = Duration::from_millis(50);
const SOURCE_TOLERANCE: Duration = Duration::from_millis(200);
const INITIATION: usize = 148;
const RESPONSE: usize = 92;
const REPLY: usize = 64;
const MAC1_LABEL: &[u8] = b"mac1----";
const COOKIE_LABEL: &[u8] = b"cookie--";

/// Source of unpredictable bytes for the cookie secret and reply nonces. The
/// production implementation is [`OsEntropy`]; tests inject computed streams.
/// A failure must be returned, never papered over with fixed bytes.
pub trait Entropy {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]>;
}

/// Operating-system randomness, the only production entropy source.
#[derive(Clone, Copy)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        random_bytes::<N>()
    }
}

/// The caller's judgement of whether handshake processing is overloaded. The
/// whitepaper defines no threshold; the future driver owns the measurement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Load {
    Normal,
    High,
}

/// Decision for one received handshake initiation or response message.
#[derive(PartialEq, Eq, Debug)]
pub enum Admit {
    /// MAC1 is valid and, under load, MAC2 proved the source address: process it.
    Process,
    /// Under load and MAC2 absent/invalid/expired: send this 64-byte reply instead.
    Reply(Vec<u8>),
    /// The per-source budget is spent: drop silently, spending no Noise work.
    Limited,
}

pub(super) fn label_key(label: &[u8], public: &[u8; 32]) -> Result<[u8; 32]> {
    hash_pair(label, public)
}

pub(super) fn mac(key: &[u8], data: &[u8]) -> Result<[u8; 16]> {
    keyed_blake2s(key, data, 16)?
        .try_into()
        .map_err(|_| "WireGuard: unexpected MAC length".into())
}

pub(super) fn mac1_key(public: &[u8; 32]) -> Result<[u8; 32]> {
    label_key(MAC1_LABEL, public)
}

/// MAC1 is a public-key filter, not authentication of the sender.
pub(super) fn verify_mac1(key: &[u8; 32], packet: &[u8]) -> Result<()> {
    let offset = packet
        .len()
        .checked_sub(32)
        .ok_or("WireGuard: handshake message too short")?;
    let expected = mac(key, &packet[..offset])?;
    if constant_time_eq(&expected, &packet[offset..offset + 16]) {
        Ok(())
    } else {
        Err("WireGuard: invalid handshake MAC1".into())
    }
}

/// Exact length, type and zero reserved bytes of an initiation (1) or response (2).
fn frame(packet: &[u8]) -> Result<()> {
    let valid = match packet.first() {
        Some(1) => packet.len() == INITIATION,
        Some(2) => packet.len() == RESPONSE,
        _ => false,
    };
    if !valid || packet[1..4] != [0, 0, 0] {
        return Err("WireGuard: invalid handshake framing".into());
    }
    Ok(())
}

/// Responder-side under-load admission for one local identity. It holds the
/// rotating cookie secret, so it has no Clone or Debug and errors never print it.
pub struct Admission<E: Entropy> {
    mac1_key: [u8; 32],
    cookie: XChaCha20Poly1305,
    entropy: E,
    secret: Option<Secret>,
    last_nonce: Option<[u8; 24]>,
    limiter: Limiter,
    generation: u64,
    observed: Duration,
    revoked: bool,
}

struct Secret {
    value: [u8; 32],
    born: Duration,
}

impl<E: Entropy> Admission<E> {
    pub fn new(local_public: [u8; 32], entropy: E, generation: u64, now: Duration) -> Result<Self> {
        Ok(Self {
            mac1_key: mac1_key(&local_public)?,
            cookie: XChaCha20Poly1305::new(label_key(COOKIE_LABEL, &local_public)?),
            entropy,
            secret: None,
            last_nonce: None,
            limiter: Limiter::default(),
            generation,
            observed: now,
            revoked: false,
        })
    }

    /// Irreversible: drops the secret and per-source state. Safe Rust does not
    /// guarantee memory zeroization.
    pub fn revoke(&mut self) {
        self.revoked = true;
        self.secret = None;
        self.last_nonce = None;
        self.limiter.clear();
    }

    /// Number of source prefixes currently tracked (never above [`MAX_SOURCES`]).
    pub fn tracked_sources(&self) -> usize {
        self.limiter.len()
    }

    /// Exact framing and a valid MAC1 for this identity, nothing else: no
    /// state, entropy or clock. The owner uses it to weigh only messages that
    /// could reach Noise when it judges the load; any sender who knows the
    /// public key passes it, so it is evidence of load, not of identity.
    pub fn screen(&self, packet: &[u8]) -> Result<()> {
        frame(packet)?;
        verify_mac1(&self.mac1_key, packet)
    }

    fn fence(&mut self, generation: u64, now: Duration) -> Result<()> {
        if self.revoked {
            return Err("WireGuard: admission is revoked".into());
        }
        if generation != self.generation || now < self.observed {
            self.revoke();
            return Err("WireGuard: admission generation or clock changed".into());
        }
        self.observed = now;
        Ok(())
    }

    /// Classify one datagram that claims to be a handshake initiation or response.
    /// `source` is the observed outer address, never a value from the packet.
    /// Invalid framing or MAC1 yields an error and, as the whitepaper requires,
    /// no reply of any kind. Under `Load::Normal` MAC2 is ignored and nothing is
    /// retained or random. Under `Load::High` every outcome first spends the
    /// source's budget, so a source cannot elicit unbounded replies or Noise work.
    pub fn admit(
        &mut self,
        packet: &[u8],
        source: SocketAddr,
        load: Load,
        generation: u64,
        now: Duration,
    ) -> Result<Admit> {
        self.fence(generation, now)?;
        self.screen(packet)?;
        if load == Load::Normal {
            return Ok(Admit::Process);
        }
        if !self.limiter.allow(source.ip(), now) {
            return Ok(Admit::Limited);
        }
        let cookie = self.cookie_for(source, now)?;
        let split = packet.len() - 16;
        let expected = mac(&cookie, &packet[..split])?;
        if constant_time_eq(&expected, &packet[split..]) {
            return Ok(Admit::Process);
        }
        Ok(Admit::Reply(self.reply(packet, &cookie)?))
    }

    fn cookie_for(&mut self, source: SocketAddr, now: Duration) -> Result<[u8; 16]> {
        let fresh = self
            .secret
            .as_ref()
            .filter(|secret| {
                now.checked_sub(secret.born)
                    .is_some_and(|age| age < SECRET_LIFETIME)
            })
            .map(|secret| secret.value);
        let secret = match fresh {
            Some(value) => value,
            None => {
                // An expired secret is gone before the replacement is requested.
                self.secret = None;
                let value = self.entropy.bytes::<32>()?;
                self.secret = Some(Secret { value, born: now });
                value
            }
        };
        let mut address = match source.ip().to_canonical() {
            IpAddr::V4(ip) => ip.octets().to_vec(),
            IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        address.extend_from_slice(&source.port().to_be_bytes());
        mac(&secret, &address)
    }

    fn reply(&mut self, packet: &[u8], cookie: &[u8; 16]) -> Result<Vec<u8>> {
        let nonce = self.entropy.bytes::<24>()?;
        // Guard against a stuck entropy source; it cannot prove randomness.
        if self.last_nonce == Some(nonce) {
            return Err("WireGuard: entropy source repeated a cookie nonce".into());
        }
        self.last_nonce = Some(nonce);
        let split = packet.len() - 32;
        let sealed = self
            .cookie
            .seal(nonce, &packet[split..split + 16], cookie)?;
        if sealed.len() != 32 {
            return Err("WireGuard: unexpected sealed cookie length".into());
        }
        let mut reply = Vec::with_capacity(REPLY);
        reply.extend_from_slice(&[3, 0, 0, 0]);
        reply.extend_from_slice(&packet[4..8]);
        reply.extend_from_slice(&nonce);
        reply.extend_from_slice(&sealed);
        Ok(reply)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Source {
    V4([u8; 4]),
    V6([u8; 8]),
}

/// Generic cell-rate limiter: each source stores the time at which its debt
/// reaches zero. The table is bounded; a full table first forgets idle sources,
/// then the source closest to idle, which loses only its small remaining debt.
#[derive(Default)]
struct Limiter {
    debts: HashMap<Source, Duration>,
}

impl Limiter {
    fn clear(&mut self) {
        self.debts.clear();
    }

    fn len(&self) -> usize {
        self.debts.len()
    }

    fn allow(&mut self, address: IpAddr, now: Duration) -> bool {
        let key = match address.to_canonical() {
            IpAddr::V4(ip) => Source::V4(ip.octets()),
            // A /64 is the usual allocation to one subscriber.
            IpAddr::V6(ip) => Source::V6(std::array::from_fn(|index| ip.octets()[index])),
        };
        let known = self.debts.get(&key).copied();
        let start = known.map_or(now, |debt| debt.max(now));
        if start - now > SOURCE_TOLERANCE {
            return false;
        }
        let Some(next) = start.checked_add(SOURCE_INTERVAL) else {
            return false;
        };
        if known.is_none() && self.debts.len() >= MAX_SOURCES {
            self.debts.retain(|_, debt| *debt > now);
            if self.debts.len() >= MAX_SOURCES
                && let Some(victim) = self
                    .debts
                    .iter()
                    .min_by_key(|(source, debt)| (**debt, **source))
                    .map(|(source, _)| *source)
            {
                self.debts.remove(&victim);
            }
        }
        self.debts.insert(key, next);
        true
    }
}

/// Initiator-side (or responder-side, for response messages) cookie state for the
/// one pinned peer: the last message sent, for binding replies, and the latest
/// cookie. No Clone or Debug: the cookie is a short-lived bearer value.
pub(super) struct Cookies {
    mac1_key: [u8; 32],
    cookie: XChaCha20Poly1305,
    sent: Option<Sent>,
    stored: Option<Stored>,
}

struct Sent {
    index: u32,
    mac1: [u8; 16],
    at: Duration,
}

struct Stored {
    value: [u8; 16],
    at: Duration,
}

impl Cookies {
    pub(super) fn new(peer: &[u8; 32]) -> Result<Self> {
        Ok(Self {
            mac1_key: mac1_key(peer)?,
            cookie: XChaCha20Poly1305::new(label_key(COOKIE_LABEL, peer)?),
            sent: None,
            stored: None,
        })
    }

    pub(super) fn clear(&mut self) {
        self.sent = None;
        self.stored = None;
    }

    /// Append MAC1 and MAC2 to a message body and remember MAC1 and the sender
    /// index so that only a reply to this message is accepted. MAC2 is zero
    /// unless a stored cookie is younger than [`COOKIE_LIFETIME`].
    pub(super) fn stamp(&mut self, packet: &mut Vec<u8>, index: u32, now: Duration) -> Result<()> {
        let first = mac(&self.mac1_key, packet)?;
        packet.extend_from_slice(&first);
        let second = match self.fresh(now) {
            Some(cookie) => mac(&cookie, packet)?,
            None => [0u8; 16],
        };
        packet.extend_from_slice(&second);
        self.sent = Some(Sent {
            index,
            mac1: first,
            at: now,
        });
        Ok(())
    }

    fn fresh(&mut self, now: Duration) -> Option<[u8; 16]> {
        let stored = self.stored.as_ref()?;
        if now
            .checked_sub(stored.at)
            .is_some_and(|age| age < COOKIE_LIFETIME)
        {
            return Some(stored.value);
        }
        self.stored = None;
        None
    }

    /// Store the cookie of a reply bound to the last message sent. Per the
    /// whitepaper this never causes a retransmission: the retry timer sends the
    /// next message, which then carries MAC2. A reply cannot be forged without
    /// the last MAC1, but an on-path observer can; that only costs a retry.
    pub(super) fn receive(&mut self, packet: &[u8], now: Duration) -> Result<()> {
        if packet.len() != REPLY || packet[..4] != [3, 0, 0, 0] {
            return Err("WireGuard: invalid cookie reply framing".into());
        }
        let sent = self
            .sent
            .as_ref()
            .ok_or("WireGuard: no handshake message awaits a cookie reply")?;
        if now
            .checked_sub(sent.at)
            .is_none_or(|age| age >= COOKIE_LIFETIME)
        {
            return Err("WireGuard: cookie reply is stale".into());
        }
        if packet[4..8] != sent.index.to_le_bytes() {
            return Err("WireGuard: cookie reply receiver mismatch".into());
        }
        let nonce: [u8; 24] = packet[8..32]
            .try_into()
            .map_err(|_| "WireGuard: invalid cookie reply nonce")?;
        let value: [u8; 16] = self
            .cookie
            .open(nonce, &sent.mac1, &packet[32..])?
            .try_into()
            .map_err(|_| "WireGuard: invalid cookie length")?;
        // A replayed or repeated reply carries the cookie we already hold; keeping
        // the first receipt time never lets a replay extend the cookie's life.
        let repeated = self.stored.as_ref().is_some_and(|stored| {
            constant_time_eq(&stored.value, &value)
                && now
                    .checked_sub(stored.at)
                    .is_some_and(|age| age < COOKIE_LIFETIME)
        });
        if !repeated {
            self.stored = Some(Stored { value, at: now });
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "cookie_tests.rs"]
mod tests;
