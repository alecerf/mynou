//! Pure per-peer composition. Outputs are instructions, never network effects.
use super::{Admission, Admit, Entropy, Handshake, KeySets, Load, MAX_PAYLOAD, Role, Timers};
use crate::Result;
use crate::crypto::x25519_public_key;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

pub const MAX_QUEUED_PAYLOADS: usize = 32;
const INDEX_TRIES: usize = 16;

/// Captured clocks: no clock is read by Peer. Wall time is only for TAI64N.
#[derive(Clone, Copy)]
pub struct PeerClock {
    pub generation: u64,
    pub now: Duration,
    pub wall: SystemTime,
}

/// ProtectedReady means authenticated offline keys, never VPN protection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerStatus {
    NoSession,
    Handshaking,
    ProtectedReady,
    GaveUp,
    Expired,
    Revoked,
}

pub struct PeerOutput {
    /// All datagrams target Peer::endpoint(), including admission cookie replies.
    pub datagrams: Vec<Vec<u8>>,
    /// Authenticated padded bytes, still requiring an inner-IP parser.
    pub inner_payloads: Vec<Vec<u8>>,
    /// Only a successful Noise response/initiation or authenticated transport.
    pub authenticated_source: Option<SocketAddr>,
    pub status: PeerStatus,
    pub dropped_payloads: usize,
    pub next: Option<Duration>,
}

// Private shared entropy adapters; serial operations use fallible borrows, never
// RefCell panics. Neither the adapter nor its Rc is exposed to callers.
struct Random<E> {
    source: E,
    failed: bool,
}
struct Shared<E>(Rc<RefCell<Random<E>>>);
impl<E> Clone for Shared<E> {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}
impl<E: Entropy> Entropy for Shared<E> {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut random = self
            .0
            .try_borrow_mut()
            .map_err(|_| "WireGuard: entropy busy")?;
        match random.source.bytes() {
            Ok(bytes) => Ok(bytes),
            Err(error) => {
                random.failed = true;
                Err(error)
            }
        }
    }
}

/// Exclusively owns every key, pending handshake, timer, admission and queue.
/// No Clone/Debug, sockets, threads, clock reads, roaming or direct fallback.
pub struct Peer<E: Entropy> {
    handshake: Handshake,
    admission: Admission<Shared<E>>,
    timers: Timers<Shared<E>>,
    keys: KeySets,
    entropy: Shared<E>,
    endpoint: SocketAddr,
    generation: u64,
    observed: Duration,
    status: PeerStatus,
    queue: VecDeque<Vec<u8>>,
    queue_limit: usize,
    prefer_initiator: bool,
    last_ephemeral: Option<[u8; 32]>,
}

impl<E: Entropy> Peer<E> {
    pub fn new(
        secret: [u8; 32],
        peer: [u8; 32],
        psk: Option<[u8; 32]>,
        endpoint: SocketAddr,
        entropy: E,
        queue_limit: usize,
        clock: PeerClock,
    ) -> Result<Self> {
        if !(1..=MAX_QUEUED_PAYLOADS).contains(&queue_limit) || endpoint.port() == 0 {
            return Err("WireGuard: invalid peer bounds or endpoint".into());
        }
        let handshake = Handshake::configured(secret, peer, psk, clock.generation, clock.now, 0)?;
        let public = x25519_public_key(secret);
        let entropy = Shared(Rc::new(RefCell::new(Random {
            source: entropy,
            failed: false,
        })));
        Ok(Self {
            handshake,
            admission: Admission::new(public, entropy.clone(), clock.generation, clock.now)?,
            timers: Timers::new(entropy.clone(), clock.generation, clock.now),
            keys: KeySets::new(clock.generation, clock.now),
            entropy,
            endpoint,
            generation: clock.generation,
            observed: clock.now,
            status: PeerStatus::NoSession,
            queue: VecDeque::new(),
            queue_limit,
            prefer_initiator: public < peer,
            last_ephemeral: None,
        })
    }

    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    pub fn status(&self) -> PeerStatus {
        self.status
    }

    /// Whether a handshake initiation or response is framed and MAC1-valid
    /// for this peer's local identity; see [`Admission::screen`].
    pub fn screen(&self, packet: &[u8]) -> Result<()> {
        self.admission.screen(packet)
    }

    pub fn queued_payloads(&self) -> usize {
        self.queue.len()
    }

    pub fn revoke(&mut self) {
        self.handshake.revoke();
        self.admission.revoke();
        self.timers.revoke();
        self.keys.revoke();
        self.queue.clear();
        self.last_ephemeral = None;
        self.status = PeerStatus::Revoked;
    }

    fn admit(&mut self, clock: PeerClock) -> Result<()> {
        if self.status == PeerStatus::Revoked {
            return Err("WireGuard: peer is revoked".into());
        }
        if clock.generation != self.generation || clock.now < self.observed {
            self.revoke();
            return Err("WireGuard: peer generation or clock changed".into());
        }
        self.observed = clock.now;
        self.handshake.expire_pending(clock.now);
        let retired = self.timers.retire(clock.generation, clock.now)?;
        if retired.discard {
            self.keys.discard_all();
            self.handshake.discard_pending();
            self.status = PeerStatus::Expired;
        }
        let had_current = self.keys.current_role().is_some();
        let removed = self.keys.expire(clock.generation, clock.now)?;
        if self.keys.current_role().is_none()
            && (had_current || (removed > 0 && self.handshake.pending_index().is_none()))
        {
            self.status = PeerStatus::Expired;
        }
        Ok(())
    }

    fn next_deadline(&self) -> Option<Duration> {
        [self.timers.next_deadline(), self.keys.next_expiry()]
            .into_iter()
            .flatten()
            .min()
    }

    fn output(&self) -> PeerOutput {
        PeerOutput {
            datagrams: Vec::new(),
            inner_payloads: Vec::new(),
            authenticated_source: None,
            status: self.status,
            dropped_payloads: 0,
            next: self.next_deadline(),
        }
    }

    // Entropy failures irreversibly block the whole peer. The owner must create a
    // fresh Peer with fresh randomness, never silently reuse fixed keys/bytes.
    fn finish<T>(&mut self, result: Result<T>, clock: PeerClock) -> Result<T> {
        let failed = self
            .entropy
            .0
            .try_borrow()
            .map_or(true, |random| random.failed);
        if failed {
            self.revoke();
        } else if result.is_err() && self.status != PeerStatus::Revoked {
            let had_current = self.keys.current_role().is_some();
            self.keys.expire(clock.generation, clock.now)?;
            if had_current && self.keys.current_role().is_none() {
                self.status = PeerStatus::Expired;
            }
        }
        result
    }

    fn allocate(&mut self) -> Result<(u32, [u8; 32])> {
        for _ in 0..INDEX_TRIES {
            let index = u32::from_le_bytes(self.entropy.bytes::<4>()?);
            if self.keys.contains_index(index) || self.handshake.pending_index() == Some(index) {
                continue;
            }
            let ephemeral = self.entropy.bytes::<32>()?;
            if self.last_ephemeral == Some(ephemeral) {
                self.revoke();
                return Err("WireGuard: repeated handshake entropy".into());
            }
            self.last_ephemeral = Some(ephemeral);
            return Ok((index, ephemeral));
        }
        Err("WireGuard: local index collision budget exhausted".into())
    }

    pub fn set_persistent_keepalive(
        &mut self,
        interval: Option<Duration>,
        clock: PeerClock,
    ) -> Result<()> {
        self.admit(clock)?;
        self.timers
            .set_persistent_keepalive(interval, clock.generation, clock.now)
    }

    /// Reject newest on queue exhaustion; existing admitted payloads are kept.
    /// No-session payloads are queued, never sent through ordinary networking.
    pub fn send(&mut self, payload: &[u8], clock: PeerClock) -> Result<PeerOutput> {
        self.admit(clock)?;
        if payload.len() > MAX_PAYLOAD {
            return Err("WireGuard: payload exceeds datagram budget".into());
        }
        if self.queue.len() >= self.queue_limit {
            return Err("WireGuard: peer outbound queue is full".into());
        }
        self.timers.queue_data(clock.generation, clock.now)?;
        self.queue.push_back(payload.to_vec());
        let result = self.advance(clock);
        self.finish(result, clock)
    }

    /// Types 1/2 are admitted before Noise; type 3 never retransmits; only AEAD
    /// acceptance on type 4 publishes plaintext or changes authenticated timers.
    pub fn receive(
        &mut self,
        packet: &[u8],
        source: SocketAddr,
        load: Load,
        clock: PeerClock,
    ) -> Result<PeerOutput> {
        self.admit(clock)?;
        let result = self.receive_inner(packet, source, load, clock);
        self.finish(result, clock)
    }

    fn receive_inner(
        &mut self,
        packet: &[u8],
        source: SocketAddr,
        load: Load,
        clock: PeerClock,
    ) -> Result<PeerOutput> {
        let mut out = self.output();
        let kind = *packet.first().ok_or("WireGuard: empty peer datagram")?;
        match kind {
            1 | 2 => {
                match self
                    .admission
                    .admit(packet, source, load, clock.generation, clock.now)?
                {
                    Admit::Reply(reply) => {
                        out.datagrams.push(reply);
                        return Ok(out);
                    }
                    Admit::Limited => return Ok(out),
                    Admit::Process => {}
                }
                if kind == 1 {
                    // Stable public-key ordering resolves simultaneous initiation.
                    // The winner ignores competing initiations without authenticating
                    // them or altering pending Noise/cookie state. The loser yields
                    // only after successfully authenticating the pinned identity.
                    let simultaneous = self.handshake.pending_index().is_some();
                    if simultaneous && self.prefer_initiator {
                        return Ok(out);
                    }
                    let (index, ephemeral) = self.allocate()?;
                    let response = self.handshake.respond_allocated(
                        packet,
                        ephemeral,
                        index,
                        clock.generation,
                        clock.now,
                    )?;
                    self.keys
                        .install_responder(response.session, clock.generation, clock.now)?;
                    if simultaneous {
                        // Keep the cookie binding for the response just emitted.
                        self.handshake.cancel_initiation();
                    }
                    self.status = if self.keys.current_role().is_some() {
                        PeerStatus::ProtectedReady
                    } else {
                        PeerStatus::Handshaking
                    };
                    out.datagrams.push(response.packet);
                } else {
                    let session = self.handshake.finish(packet, clock.generation, clock.now)?;
                    self.keys
                        .install_initiator(session, clock.generation, clock.now)?;
                    self.timers.session_established(
                        Role::Initiator,
                        clock.generation,
                        clock.now,
                    )?;
                    self.status = PeerStatus::ProtectedReady;
                }
                out.authenticated_source = Some(source);
            }
            3 => {
                self.handshake
                    .receive_cookie_reply(packet, clock.generation, clock.now)?;
                self.timers.cookie_stored(clock.generation, clock.now)?;
                return Ok(out);
            }
            4 => {
                let received = self.keys.receive(packet, clock.generation, clock.now)?;
                if received.promoted {
                    let born = self
                        .keys
                        .current_created()
                        .ok_or("WireGuard: missing promoted session")?;
                    self.timers
                        .session_promoted(born, clock.generation, clock.now)?;
                    self.status = PeerStatus::ProtectedReady;
                }
                if received.current {
                    self.timers.data_received(
                        received.accepted.bytes.is_empty(),
                        received.accepted.rekey,
                        clock.generation,
                        clock.now,
                    )?;
                }
                if !received.accepted.bytes.is_empty() {
                    out.inner_payloads.push(received.accepted.bytes);
                }
                out.authenticated_source = Some(source);
            }
            _ => return Err("WireGuard: unsupported peer message type".into()),
        }
        let advanced = self.advance(clock)?;
        out.datagrams.extend(advanced.datagrams);
        out.dropped_payloads = advanced.dropped_payloads;
        out.status = advanced.status;
        out.next = advanced.next;
        Ok(out)
    }

    pub fn poll(&mut self, clock: PeerClock) -> Result<PeerOutput> {
        self.admit(clock)?;
        let result = self.advance(clock);
        self.finish(result, clock)
    }

    fn advance(&mut self, clock: PeerClock) -> Result<PeerOutput> {
        let mut out = self.output();
        let due = self.timers.poll(clock.generation, clock.now)?;
        if due.discard {
            self.keys.discard_all();
            self.handshake.discard_pending();
            self.status = PeerStatus::Expired;
        }
        if due.expired && self.keys.current_role().is_none() {
            self.status = PeerStatus::Expired;
        }
        if due.gave_up {
            self.handshake.discard_pending();
            out.dropped_payloads = self.queue.len();
            self.queue.clear();
            self.status = if self.keys.current_role().is_some() {
                PeerStatus::ProtectedReady
            } else {
                PeerStatus::GaveUp
            };
        }
        if due.initiate {
            let (index, ephemeral) = self.allocate()?;
            out.datagrams.push(self.handshake.initiate_allocated(
                clock.wall,
                ephemeral,
                index,
                clock.generation,
                clock.now,
            )?);
            if self.keys.current_role().is_none() {
                self.status = PeerStatus::Handshaking;
            }
        }
        if self.keys.current_role().is_some() {
            for payload in &self.queue {
                let accepted = self.keys.send(payload, clock.generation, clock.now)?;
                if payload.is_empty() {
                    self.timers.keepalive_sent(clock.generation, clock.now)?;
                } else {
                    self.timers
                        .data_sent(accepted.rekey, clock.generation, clock.now)?;
                }
                out.datagrams.push(accepted.bytes);
            }
            self.queue.clear();
            if due.keepalive
                && out
                    .datagrams
                    .iter()
                    .all(|packet| packet.first() != Some(&4))
            {
                let accepted = self.keys.send(&[], clock.generation, clock.now)?;
                self.timers.keepalive_sent(clock.generation, clock.now)?;
                out.datagrams.push(accepted.bytes);
            }
            // Data can request a message-count/time rekey after this poll.
            // Leave it explicitly due at next_deadline(), without a second poll.
        }
        out.status = self.status;
        out.next = self.next_deadline();
        Ok(out)
    }
}

#[cfg(test)]
#[path = "peer_tests.rs"]
mod tests;
