# Offline WireGuard transport sessions

[#178](https://github.com/alecerf/mynou/issues/178) adds an inert protocol
building block after the primitives in #169. [The offline handshake](/docs/wireguard-handshake.md)
now establishes its keys. Neither increment adds sockets, a profile, tunnel,
protected mode or VPN leak protection. #111 remains open.

## Ownership and protocol

The independently authored implementation follows the transport packet and
replay sections of the [WireGuard protocol](https://www.wireguard.com/protocol/)
and [whitepaper sections 5.4.6 and 6](https://www.wireguard.com/papers/wireguard.pdf)
(read 2026-10-11), using the existing RFC8439 ChaCha20-Poly1305 primitive.
No third-party implementation is copied. The constructor is crate-private,
and used only after successful offline handshake processing; the handshake owner
has no engine/profile/driver caller. It consumes local-send/local-receive keys, local/remote
receiver indexes, role, generation and a monotonic creation time. Keys must be
fresh and never reused in a different Session, including after restart.

Session has no Clone or Debug and never exposes keys. Directional send counters
are reserved before encryption and cannot be reset through the API. A responder
may send only after a valid, previously unseen authenticated inbound packet,
including an empty keepalive. Session alone cannot establish peer identity.
Dropping keys on revocation is best effort; safe Rust and optimization do not
guarantee memory zeroization or constant-time execution.

Type-4 packets contain the exact 16-byte little-endian header and zero reserved
bytes. The AEAD nonce is four zero bytes and the little-endian 64-bit counter;
associated data is empty. Payloads are zero-padded to a 16-byte boundary. The
maximum unpadded payload is 1424 bytes and datagram 1456 bytes, a conservative
operation budget, not an established path MTU. Oversized and malformed input
fails before decryption/allocation. Receive returns authenticated **padded**
bytes; the future IP parser must validate actual IP length, addresses, checksums
and padding before publishing a virtual packet. This module does not do that.

The receive replay window stores 2048 counters in 256 bytes and accepts
out-of-order counters within that window. Counter/tag validation precedes window
mutation and responder confirmation. A forged high counter cannot evict valid
packets. Clearing slots after advancement takes at most 2047 iterations.

## Admission and lifecycle

Every operation takes the caller's captured generation and monotonic elapsed
clock, never values from network input. A generation mismatch, clock regression,
explicit revoke, send-counter exhaustion or age of at least 180 seconds
irreversibly revokes the session. No retry reactivates its keys. Future drivers
must serialize final packet submission with generation revocation; these offline
checks are not a network admission or endpoint-roaming implementation.

Counters at or above `2^64 - 2^13 - 1` are rejected, so the last admitted counter
is one less. Sending requests rekey once the number of reserved messages reaches
`2^60`; an initiator also requests rekey on a successful send at age 120 seconds
or receive at age 165 seconds. Boundaries are conservative inclusive comparisons.
A signal requests a **fresh handshake**, never resets a counter or changes keys.
Responder temporal rekey is not initiated here. Invalid packets never emit an
accepted payload or rekey signal. Keepalive/retry scheduling and key-set rotation
live in [the timers guide](/docs/wireguard-timers.md), not in this module.

## Evidence and remaining gates

GitHub Actions generates 16 deterministic expected transport packets with the
original Python-stdlib-only reference in `.github/scripts/crypto_fixtures.py`.
It uses RFC8439 word equations for ChaCha and arbitrary-precision modular
arithmetic for Poly1305, independent of the Rust limb implementation; it neither
imports nor executes product code. The published
[C2SP/Wycheproof ChaCha20-Poly1305 vector](https://github.com/C2SP/wycheproof/blob/main/testvectors_v1/chacha20_poly1305_test.json)
`tcId 2` anchors the empty-AAD/tag reference; RFC8439 section 2.3.2 separately
anchors the counter-1 encryption stream. Existing RFC8439 vectors continue
to cover the product primitive. All keys/messages are synthetic, not credentials.

The matrix covers empty, partial, exact-block and maximum payloads plus counters
whose bytes distinguish nonce endianness. Rust fixtures additionally cover
directional keys, every packet-byte corruption, wrong key/index, confirmation,
duplicate/stale/out-of-order counters, forged high counters, replay-word wraps,
large jumps, counter/rekey/expiry boundaries, old generations and clock regression.
Validation runs only in Actions; no fixture is a product runtime helper.

Same-code round trips and independently computed AEAD bytes do **not** demonstrate
WireGuard peer interoperability. The offline Noise increment covers transcript
authentication and per-owner timestamp/index ownership. Rekey/retry
scheduling (cookies are in [their own guide](/docs/wireguard-cookies.md)), a controlled independent peer, the IP/TCP/UDP/DNS
stack, complete network integration and actual ordinary-path attempt observation
remain required by [the design](/docs/wireguard-design.md). Protected mode stays
unavailable until those gates and separate QA/Security are delivered.
