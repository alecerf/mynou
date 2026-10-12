//! WireGuard retry, rekey, keepalive and key-set rotation rules, following the
//! whitepaper sections 6.1 to 6.6. Pure and offline: no sockets, threads or clock
//! reads. The caller passes a captured generation and a monotonic elapsed time,
//! feeds events in and acts on the returned [`Due`] values.
//! See docs/wireguard-timers.md for the boundary and the remaining obligations.

use super::{Accepted, Entropy, Role, Session};
use crate::Result;
use std::time::Duration;

/// A session older than this makes its initiator start a fresh handshake.
pub const REKEY_AFTER_TIME: Duration = Duration::from_secs(120);
/// A session this old is never used again.
pub const REJECT_AFTER_TIME: Duration = Duration::from_secs(180);
/// Handshake retries stop this long after the attempt began.
pub const REKEY_ATTEMPT_TIME: Duration = Duration::from_secs(90);
/// Minimum spacing between two initiations of one attempt, before jitter.
pub const REKEY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a receiver waits for outbound data before sending a keepalive.
pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound (inclusive) of the random delay added to every retry.
pub const MAX_JITTER: Duration = Duration::from_millis(333);
/// An initiator that receives data from a session this old starts a rekey:
/// `REJECT_AFTER_TIME - KEEPALIVE_TIMEOUT - REKEY_TIMEOUT`.
pub const REKEY_ON_RECEIVE: Duration = Duration::from_secs(165);
/// All keys and handshake state are discarded this long after the last session
/// was established: three times [`REJECT_AFTER_TIME`].
pub const DISCARD_AFTER: Duration = Duration::from_secs(540);
const MAX_PERSISTENT_SECS: u64 = 65_535;
// Data sent with no packet received back within this long forces a handshake.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

/// Generation and monotonic-clock fence shared by the state machines here. Any
/// violation irreversibly revokes the state, like the other offline pieces.
struct Fence {
    generation: u64,
    observed: Duration,
    revoked: bool,
}

impl Fence {
    fn new(generation: u64, now: Duration) -> Self {
        Self {
            generation,
            observed: now,
            revoked: false,
        }
    }

    fn admit(&mut self, generation: u64, now: Duration) -> Result<()> {
        if self.revoked {
            return Err("WireGuard: state is revoked".into());
        }
        if generation != self.generation || now < self.observed {
            self.revoked = true;
            return Err("WireGuard: generation or monotonic clock changed".into());
        }
        self.observed = now;
        Ok(())
    }
}

/// What the caller must do now, and when to poll again. Every flag is a request
/// to the driver, never an action taken here.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Due {
    /// A handshake initiation must be sent. Already counted as sent: the next
    /// retry is scheduled, so an unsent message waits for that retry.
    pub initiate: bool,
    /// An authenticated empty packet must be sent, then reported with
    /// `Timers::keepalive_sent`. It stays due until reported.
    pub keepalive: bool,
    /// The 90 second attempt budget is spent: stop retrying and fail the queued
    /// data. Only new outbound data starts another attempt.
    pub gave_up: bool,
    /// The current session reached its reject age: call `KeySets::expire`.
    pub expired: bool,
    /// Three times the reject age passed without a new session: call
    /// `KeySets::discard_all`. Handshake and timer state are already cleared.
    pub discard: bool,
    /// The earliest instant at which polling again can produce a request. It may
    /// not be in the future while a keepalive awaits `keepalive_sent`.
    pub next: Option<Duration>,
}

#[derive(Clone, Copy)]
struct Clock {
    born: Duration,
    role: Role,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Attempt {
    Idle,
    Active { started: Duration, next: Duration },
    Abandoned,
}

/// Per-peer timer state machine. It owns only deadlines: keys live in
/// [`KeySets`] and the driver forwards events between them. Neither Clone nor
/// Debug, so a timer set cannot be duplicated into two diverging copies.
pub struct Timers<E: Entropy> {
    entropy: E,
    fence: Fence,
    persistent: Option<Duration>,
    session: Option<Clock>,
    last_established: Option<Duration>,
    attempt: Attempt,
    last_sent: Option<Duration>,
    keepalive_at: Option<Duration>,
    reply_by: Option<Duration>,
}

impl<E: Entropy> Timers<E> {
    pub fn new(entropy: E, generation: u64, now: Duration) -> Self {
        Self {
            entropy,
            fence: Fence::new(generation, now),
            persistent: None,
            session: None,
            last_established: None,
            attempt: Attempt::Idle,
            last_sent: None,
            keepalive_at: None,
            reply_by: None,
        }
    }

    /// Irreversible: clears every deadline and refuses all later events.
    pub fn revoke(&mut self) {
        self.fence.revoked = true;
        self.clear();
    }

    fn clear(&mut self) {
        self.session = None;
        self.last_established = None;
        self.attempt = Attempt::Idle;
        self.last_sent = None;
        self.keepalive_at = None;
        self.reply_by = None;
    }

    /// Optional persistent keepalive: a whole number of seconds from 1 to 65535.
    pub fn set_persistent_keepalive(
        &mut self,
        interval: Option<Duration>,
        generation: u64,
        now: Duration,
    ) -> Result<()> {
        self.fence.admit(generation, now)?;
        if let Some(value) = interval
            && (value.subsec_nanos() != 0
                || !(1..=MAX_PERSISTENT_SECS).contains(&value.as_secs()))
        {
            return Err("WireGuard: persistent keepalive must be 1 to 65535 seconds".into());
        }
        self.persistent = interval;
        Ok(())
    }

    fn live(&self, now: Duration) -> Option<Clock> {
        self.session
            .filter(|clock| now.saturating_sub(clock.born) < REJECT_AFTER_TIME)
    }

    fn rekey_due(&self, now: Duration, hint: bool) -> bool {
        self.live(now).is_some_and(|clock| {
            hint || (clock.role == Role::Initiator
                && now.saturating_sub(clock.born) >= REKEY_AFTER_TIME)
        })
    }

    /// Only explicit new outbound data may leave the abandoned state, so the
    /// 90 second budget is never reset by a timer or by received traffic.
    fn start(&mut self, now: Duration, explicit: bool) {
        match self.attempt {
            Attempt::Active { .. } => {}
            Attempt::Abandoned if !explicit => {}
            _ => {
                self.attempt = Attempt::Active {
                    started: now,
                    next: now,
                };
                self.reply_by = None;
            }
        }
    }

    /// Outbound data is waiting. With no usable session, or one that must be
    /// replaced, this begins an attempt (or restarts an abandoned one).
    pub fn queue_data(&mut self, generation: u64, now: Duration) -> Result<()> {
        self.fence.admit(generation, now)?;
        if self.live(now).is_none() || self.rekey_due(now, false) {
            self.start(now, true);
        }
        Ok(())
    }

    /// A handshake completed: the initiator on receiving the response, the
    /// responder on the first authenticated packet of its next session. The
    /// initiator owes the responder a confirming packet, so a keepalive is due.
    pub fn session_established(
        &mut self,
        role: Role,
        generation: u64,
        now: Duration,
    ) -> Result<()> {
        self.fence.admit(generation, now)?;
        self.session = Some(Clock { born: now, role });
        self.last_established = Some(now);
        self.reply_by = None;
        if role == Role::Initiator {
            self.attempt = Attempt::Idle;
            self.keepalive_at = Some(now);
        }
        Ok(())
    }

    /// A data packet was sent. `rekey` is the flag of the send's [`Accepted`].
    pub fn data_sent(&mut self, rekey: bool, generation: u64, now: Duration) -> Result<()> {
        self.fence.admit(generation, now)?;
        self.last_sent = Some(now);
        self.keepalive_at = None;
        if self.live(now).is_some() {
            if self.reply_by.is_none() && !matches!(self.attempt, Attempt::Active { .. }) {
                self.reply_by = Some(now.saturating_add(REPLY_TIMEOUT));
            }
            if self.rekey_due(now, rekey) {
                self.start(now, true);
            }
        }
        Ok(())
    }

    /// A keepalive was sent, whether requested by [`Due`] or not.
    pub fn keepalive_sent(&mut self, generation: u64, now: Duration) -> Result<()> {
        self.fence.admit(generation, now)?;
        self.last_sent = Some(now);
        self.keepalive_at = None;
        Ok(())
    }

    /// An authenticated packet arrived on a session. `keepalive` is true for an
    /// empty payload; `rekey` is the flag of the receive's [`Accepted`].
    pub fn data_received(
        &mut self,
        keepalive: bool,
        rekey: bool,
        generation: u64,
        now: Duration,
    ) -> Result<()> {
        self.fence.admit(generation, now)?;
        self.reply_by = None;
        if let Some(clock) = self.live(now) {
            if !keepalive && self.keepalive_at.is_none() {
                self.keepalive_at = Some(now.saturating_add(KEEPALIVE_TIMEOUT));
            }
            let late =
                clock.role == Role::Initiator && now.saturating_sub(clock.born) >= REKEY_ON_RECEIVE;
            if rekey || late {
                self.start(now, false);
            }
        }
        Ok(())
    }

    /// A valid cookie reply was stored by the handshake. Per whitepaper 6.6
    /// nothing is retransmitted now: the retry deadline is unchanged.
    pub fn cookie_stored(&mut self, generation: u64, now: Duration) -> Result<()> {
        self.fence.admit(generation, now)
    }

    fn jitter(&mut self) -> Result<Duration> {
        let value = u32::from_le_bytes(self.entropy.bytes::<4>()?);
        // The bias of the modulo over 2^32 values is below 0.00001 percent.
        let bound = MAX_JITTER.as_millis() as u32 + 1;
        Ok(Duration::from_millis(u64::from(value % bound)))
    }

    /// Whether this poll can send an initiation, so entropy is drawn (and may
    /// fail) before any state changes.
    fn may_initiate(&self, now: Duration) -> bool {
        match self.attempt {
            Attempt::Active { started, next } => {
                now >= next && now.saturating_sub(started) < REKEY_ATTEMPT_TIME
            }
            Attempt::Idle => self.reply_by.is_some_and(|deadline| now >= deadline),
            Attempt::Abandoned => false,
        }
    }

    /// Advance every deadline that has passed. An entropy failure returns an
    /// error with no state changed, so the caller may poll again.
    pub fn poll(&mut self, generation: u64, now: Duration) -> Result<Due> {
        self.fence.admit(generation, now)?;
        let jitter = if self.may_initiate(now) {
            Some(self.jitter()?)
        } else {
            None
        };
        let mut due = Due::default();
        if self
            .last_established
            .is_some_and(|born| now.saturating_sub(born) >= DISCARD_AFTER)
        {
            self.clear();
            due.discard = true;
        }
        if self
            .session
            .is_some_and(|clock| now.saturating_sub(clock.born) >= REJECT_AFTER_TIME)
        {
            self.session = None;
            self.keepalive_at = None;
            self.reply_by = None;
            due.expired = true;
        }
        if self.reply_by.is_some_and(|deadline| now >= deadline) {
            self.reply_by = None;
            self.start(now, false);
        }
        if let Attempt::Active { started, next } = self.attempt {
            if now.saturating_sub(started) >= REKEY_ATTEMPT_TIME {
                self.attempt = Attempt::Abandoned;
                due.gave_up = true;
            } else if now >= next {
                let delay = REKEY_TIMEOUT.saturating_add(jitter.ok_or("WireGuard: missing jitter")?);
                match now.checked_add(delay) {
                    Some(next) => {
                        self.attempt = Attempt::Active { started, next };
                        due.initiate = true;
                    }
                    // The clock cannot represent the retry: fail closed.
                    None => {
                        self.attempt = Attempt::Abandoned;
                        due.gave_up = true;
                    }
                }
            }
        }
        if let Some(clock) = self.session {
            let passive = self.keepalive_at.is_some_and(|deadline| now >= deadline);
            due.keepalive = passive || self.persistent_deadline(clock).is_some_and(|d| now >= d);
        }
        due.next = self.next_deadline();
        Ok(due)
    }

    fn persistent_deadline(&self, clock: Clock) -> Option<Duration> {
        let base = self
            .last_sent
            .map_or(clock.born, |sent| sent.max(clock.born));
        self.persistent.map(|interval| base.saturating_add(interval))
    }

    /// The earliest instant at which a poll can produce a request, if any.
    pub fn next_deadline(&self) -> Option<Duration> {
        let attempt = match self.attempt {
            Attempt::Active { started, next } => {
                Some(next.min(started.saturating_add(REKEY_ATTEMPT_TIME)))
            }
            _ => None,
        };
        [
            self.session
                .map(|clock| clock.born.saturating_add(REJECT_AFTER_TIME)),
            self.last_established
                .map(|born| born.saturating_add(DISCARD_AFTER)),
            attempt,
            self.keepalive_at.filter(|_| self.session.is_some()),
            self.reply_by,
            self.session
                .and_then(|clock| self.persistent_deadline(clock)),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

/// Result of a packet accepted by [`KeySets::receive`].
pub struct Received {
    pub accepted: Accepted,
    /// The packet was the first authenticated one of the unconfirmed next
    /// session, which is now current. Report it with
    /// `Timers::session_established(Role::Responder, ..)`.
    pub promoted: bool,
}

fn discard(slot: Option<Session>) {
    if let Some(mut session) = slot {
        session.revoke();
    }
}

/// Current, previous and unconfirmed next sessions of one peer: at most three,
/// sharing one local index namespace. A session that leaves a slot is revoked
/// before it is dropped. Neither Clone nor Debug.
pub struct KeySets {
    current: Option<Session>,
    previous: Option<Session>,
    next: Option<Session>,
    fence: Fence,
}

impl KeySets {
    pub fn new(generation: u64, now: Duration) -> Self {
        Self {
            current: None,
            previous: None,
            next: None,
            fence: Fence::new(generation, now),
        }
    }

    /// Irreversible: revokes every session and refuses all later calls.
    pub fn revoke(&mut self) {
        self.fence.revoked = true;
        self.discard_all();
    }

    /// Revokes and drops all three sessions; later installs are still accepted.
    pub fn discard_all(&mut self) {
        discard(self.current.take());
        discard(self.previous.take());
        discard(self.next.take());
    }

    fn check(&mut self, session: &Session, role: Role, generation: u64, now: Duration) -> Result<()> {
        self.fence.admit(generation, now)?;
        if session.role != role
            || session.generation != generation
            || session.keys.is_none()
            || session.created > now
        {
            return Err("WireGuard: session does not fit this key set".into());
        }
        let index = session.local_index;
        let taken = [&self.current, &self.previous, &self.next]
            .into_iter()
            .flatten()
            .any(|held| held.local_index == index);
        if taken {
            return Err("WireGuard: local index is already in use".into());
        }
        Ok(())
    }

    /// The initiator completed a handshake: its session becomes current, the old
    /// current becomes previous, and the old previous and any next are revoked.
    pub fn install_initiator(
        &mut self,
        mut session: Session,
        generation: u64,
        now: Duration,
    ) -> Result<()> {
        if let Err(error) = self.check(&session, Role::Initiator, generation, now) {
            session.revoke();
            return Err(error);
        }
        discard(self.next.take());
        discard(self.previous.take());
        self.previous = self.current.take();
        self.current = Some(session);
        Ok(())
    }

    /// The responder answered an initiation: its session waits as next, unable
    /// to send, until the peer's first authenticated packet. A replaced next is
    /// revoked.
    pub fn install_responder(
        &mut self,
        mut session: Session,
        generation: u64,
        now: Duration,
    ) -> Result<()> {
        if let Err(error) = self.check(&session, Role::Responder, generation, now) {
            session.revoke();
            return Err(error);
        }
        discard(self.next.take());
        self.next = Some(session);
        Ok(())
    }

    /// Sends on the current session only; previous and next never send.
    pub fn send(&mut self, payload: &[u8], generation: u64, now: Duration) -> Result<Accepted> {
        self.fence.admit(generation, now)?;
        self.current
            .as_mut()
            .ok_or("WireGuard: no current session")?
            .send(payload, generation, now)
    }

    /// Routes by the receiver index in the packet header. Authentication happens
    /// before any rotation, so a forged packet cannot promote the next session.
    pub fn receive(&mut self, packet: &[u8], generation: u64, now: Duration) -> Result<Received> {
        self.fence.admit(generation, now)?;
        let index = packet
            .get(4..8)
            .ok_or("WireGuard: invalid transport framing")?;
        let index = u32::from_le_bytes(index.try_into().unwrap());
        if let Some(session) = self.current.as_mut()
            && session.local_index == index
        {
            let accepted = session.receive(packet, generation, now)?;
            return Ok(Received {
                accepted,
                promoted: false,
            });
        }
        if let Some(session) = self.previous.as_mut()
            && session.local_index == index
        {
            let accepted = session.receive(packet, generation, now)?;
            return Ok(Received {
                accepted,
                promoted: false,
            });
        }
        if let Some(session) = self.next.as_mut()
            && session.local_index == index
        {
            let accepted = session.receive(packet, generation, now)?;
            discard(self.previous.take());
            self.previous = self.current.take();
            self.current = self.next.take();
            return Ok(Received {
                accepted,
                promoted: true,
            });
        }
        Err("WireGuard: unknown receiver index".into())
    }

    /// Drops every session that reached the reject age or was revoked, returning
    /// how many were removed.
    pub fn expire(&mut self, generation: u64, now: Duration) -> Result<usize> {
        self.fence.admit(generation, now)?;
        let mut removed = 0;
        for slot in [&mut self.current, &mut self.previous, &mut self.next] {
            let stale = slot.as_ref().is_some_and(|session| {
                session.keys.is_none()
                    || now.saturating_sub(session.created) >= REJECT_AFTER_TIME
            });
            if stale {
                discard(slot.take());
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
#[path = "timers_tests.rs"]
mod tests;
