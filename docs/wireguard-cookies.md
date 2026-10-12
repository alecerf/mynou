# Offline WireGuard cookies and under-load admission

[#182](https://github.com/alecerf/mynou/issues/182) adds the cookie reply and
under-load admission state machines to the offline pieces from
[the transport](/docs/wireguard-transport.md) (#178) and
[the Noise handshake](/docs/wireguard-handshake.md) (#180). There is no socket,
driver, profile, engine or provider caller, so **this provides no VPN protection,
leak prevention or independent peer interoperability**. Parent #111 stays open.

## Protocol followed

The independently authored code follows the public
[WireGuard protocol](https://www.wireguard.com/protocol/) and the
[whitepaper](https://www.wireguard.com/papers/wireguard.pdf) sections 5.3, 5.4.4,
5.4.7 and 6.6 (read 2026-10-12); no third-party implementation is copied.

- `mac1 = BLAKE2s-16(key = Hash("mac1----" || receiver public), message up to mac1)`;
  `mac2 = BLAKE2s-16(cookie, message up to mac2)`, or 16 zero bytes without a
  cookie younger than 120 s.
- Cookie `tau = BLAKE2s-16(Rm, source IP || source UDP port)`, where `Rm` is a
  random 32-byte secret replaced at an age of 120 s. The whitepaper binds the
  address and port; the protocol page mentions only the address. `tau` is opaque
  to the peer, so either is wire-compatible; this code follows the whitepaper.
  IPv4-mapped IPv6 sources are canonicalized to IPv4; IPv6 scope and flow
  information are not part of the observed address.
- Cookie reply (type 3, 64 bytes): `3, 0, 0, 0`, the receiver index (the sender
  index of the provoking message), a 24-byte random nonce and
  `XChaCha20-Poly1305(Hash("cookie--" || responder public), nonce, tau, aad = the
  provoking message's mac1)`. It is smaller than any handshake message, so it does
  not amplify.

## What this increment owns

`Admission` (responder side, one per local identity) validates exact framing
(type 1 with 148 bytes, type 2 with 92 bytes, zero reserved bytes) and then MAC1
in constant time. Invalid framing or MAC1 returns an error and **never a reply**,
and holds no secret or per-source state. The caller supplies an observed source
address, a `Load` verdict, its captured generation and a monotonic elapsed time.
Without load, MAC2 is ignored, no entropy is used and no state is kept. Under
load every message first spends the source's budget, then MAC2 is compared in
constant time: valid means `Admit::Process`, otherwise `Admit::Reply`, and an
exhausted budget is `Admit::Limited` (silent drop after the MAC1 check, no entropy and no cookie MAC work).
The load verdict is a driver obligation: the whitepaper defines no threshold.

The rate limit is repository policy, not a whitepaper constant: a generic
cell-rate limiter of 20 messages per second per source with a burst of five,
keyed by IPv4 address or IPv6 /64. At most 1024 sources are tracked. A full table
drops sources whose debt is paid, then the source with the least remaining debt,
so a flood of fresh sources cannot erase a heavily indebted one. All arithmetic
is checked; overflow denies the message.

`Cookies` (inside `Handshake`, one pinned peer) stamps every outgoing initiation
or response with MAC1 and MAC2, remembers the last sent message's index and MAC1,
and accepts a cookie reply only if framing, receiver index, age under 120 s and
the AEAD (keyed by the peer's public key, bound to that MAC1) all match. A
verified cookie is only **stored** with its receipt time; per whitepaper 6.6
nothing is retransmitted, so the future retry timer sends the next message. An
on-path observer who sees the MAC1 can forge a reply; that only costs a retry.
Replaying an authentic reply is idempotent: the cookie already held keeps its
first receipt time, so a replay never extends its 120 s life.
`Handshake` no longer rejects nonzero MAC2, because the protocol ignores it
without load. A caller that is loaded must run `Admission` first.

The cookie secret and reply nonces come from the `Entropy` trait, whose only
production implementation is `OsEntropy` (the OS random source). A failure is
returned, never replaced by fixed bytes; a repeated consecutive reply nonce is
refused as a stuck-source guard (it cannot prove randomness). `Admission` and
`Cookies` have no `Clone` or `Debug`, errors are static strings, and generation
or monotonic-clock changes and explicit `revoke` irreversibly discard the secret
and tables. Safe Rust does not guarantee memory zeroization, and no
compiler-level constant-time claim is made. This is not a cryptographic audit.

## Evidence

Actions generates, with the Python standard library only, an independent
HChaCha20/XChaCha20-Poly1305 reference anchored by the draft-irtf-cfrg-xchacha-03
section 2.2.1 and A.3.1 public vectors, plus four exchanges (IPv4 and IPv6,
initiation and response). Rust test `independent_reference_cookie_replies_and_mac2`
compares MAC1, the exact 64-byte replies, the cookie and the retry MAC2 byte for
byte, then checks the retry passes admission. Other tests cover every-byte tamper
and silence before MAC1, no entropy or state without load or before MAC1, source
address/port binding, exact reply replay, the 120 s secret and cookie boundaries, the budget, the
table bound and eviction, entropy failure, clock/generation/revocation fences,
extreme clock values, and end-to-end handshakes through a loaded responder or
initiator. Same-code round trips and the reference share one reading of the
specification: they are **not** interoperability evidence with an independent
WireGuard peer.

## Remaining obligations

Bounded retry, rekey and keepalive timers and key-set rotation now exist in
[the timers guide](/docs/wireguard-timers.md); a driver that measures load, owns generations, indexes and retry scheduling, and
adds a global (not only per-source) handshake bound; authenticated endpoint
roaming; the IP/TCP/UDP/DNS stack; HTTP/TLS and torrent integration; profile
controls; an independently implemented controlled peer exercising cookies; and
observed ordinary-path attempts, as listed in [the design](/docs/wireguard-design.md).
No unprotected fallback or protected-success state is introduced.
