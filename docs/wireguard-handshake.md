# Offline pinned-peer Noise handshake

Issue #180 adds the WireGuard Noise IKpsk2 initiation/response building block to
the offline sessions from #178. There is no socket, profile, driver, tunnel,
protected mode or VPN leak protection. Parent #111 remains open.

## Authentication and ownership

The original implementation follows the equations, labels and packet layout in
the [WireGuard protocol](https://www.wireguard.com/protocol/) and
[whitepaper section 5.4](https://www.wireguard.com/papers/wireguard.pdf), read
2026-10-11. It uses the original std-only primitives from #169; no third-party
implementation is copied. It is not an independent cryptographic audit and makes
no compiler-level constant-time or guaranteed memory-zeroization claim.

One non-Clone/non-Debug owner holds a private static identity, one pinned peer,
optional PSK, one pending initiation, separate issued/accepted timestamps and
one generation. The constructor is crate-private with no engine/profile caller.
Ephemeral keys come exclusively from the OS random source in production; failures
block the operation. Deterministic synthetic inputs are used only by internal
tests. There are no exported private/session keys or public arbitrary-ephemeral
constructors. A new owner uses a random initial index and monotonically reserves
local u32 indexes without wrap, even across returned sessions. Exhaustion refuses
new handshakes. The future driver must maintain one index namespace/owner per
pinned peer and generation and retire all old sessions before replacing it;
separate owners are not a collision-safe global index registry.

Exact 148-byte type-1 and 92-byte type-2 messages enforce zero reserved bytes,
MAC1 filtering and receiver-index checks before response transcript processing.
MAC1 is a public-key-derived BLAKE2s-16 filter, **not peer authentication**.
The decrypted initiator static identity must equal the pinned peer. X25519
rejects low-order shared results. Noise mixes the correct BLAKE2s chaining keys,
transcript hashes, static/ephemeral DHs and PSK (32 zero bytes when absent).
Each new mixed handshake key owns a fresh Noise cipher counter beginning at
zero; the four-zero-prefix nonce and transcript AAD match WireGuard. No live
key counter is reset. Invalid encrypted identity, time or empty confirmation
never publishes an authenticated initiator Session.

Sender indexes are routing handles, not peer identities or cryptographically
authenticated transcript fields. A response receiver must match the outstanding
local index; the authenticated peer selects its sender index. A future driver
must still manage that routing namespace and endpoint/session admission.

## Time, replay and publication

TAI64N uses big-endian seconds `Unix seconds + 0x400000000000000a` and nanoseconds,
matching the WireGuard timestamp convention. Epoch/seconds-overflow errors and
nanoseconds outside 0..1,000,000,000 fail explicitly. Initiator-issued timestamps
must grow strictly, including across replaced attempts; clock repetition or
reversal fails without inventing a greater time. Responder-accepted timestamps
grow separately per pinned peer and are committed only after identity/timestamp
authentication and successful response/session construction. They are not
persisted here; restart lifecycle and fresh generation ownership are future
driver obligations.

The initiation does not prove the PSK: the responder Session remains unconfirmed
until its first valid, unseen authenticated transport packet. A mismatched PSK
therefore cannot authorize responder sending. Successful initiator processing
of the authenticated response consumes pending state once and creates fresh
directional keys. Malformed/forged responses preserve the outstanding attempt.

Pending state expires at age **at least 5 seconds**, measured with the caller's
monotonic clock, independently of the wall-clock timestamp. This conservative
offline budget is not a delivered handshake retry/jitter scheduler. New
initiations replace the previous pending attempt only after successful
construction. An expired attempt is discarded; a generation mismatch, monotonic
clock regression or explicit revoke discards credentials/pending irreversibly.
Invalid remote input cannot revive them. Old returned Sessions are separately
owned: the driver must revoke those too and serialize final network admission
with generation changes. Revoking Handshake alone does not revoke them.

## Evidence and remaining gates

Actions generates two independent deterministic transcripts, with zero/present
PSK, using the original Python-stdlib reference in crypto_fixtures.py. Python
hashlib/hmac supplies BLAKE2s/KDF; independently authored arbitrary-precision
Montgomery and Poly1305 equations plus scalar ChaCha supply DH/AEAD. Public
RFC7748 section 5.2 iteration-one, RFC8439 counter-1 block and section 2.8.2
nonempty-AAD ciphertext/tag, and Wycheproof tcId2 anchors are literal expected
data. No product code is imported or executed by the generator, and no helper
is invoked by the product. Synthetic secrets are fixture data, never credentials.

Rust checks complete independent initiation/response/transport bytes and the
pending transcript/chaining key, optional PSK, fresh OS ephemeral/index ownership,
confirmation, every packet-byte mutation, publicly recomputed MACs with invalid
tags/receiver, wrong peer/PSK, low-order ephemerals, replay/invalid timestamps,
pending expiry/replacement/consumption, generation/revocation and index exhaustion.
Validation runs only in Actions. Same-code round trips and computed transcripts
do not establish controlled independent WireGuard peer interoperability.

MAC2 must be zero: nonzero MAC2/cookie replies are explicitly unsupported in
this increment. Cookies, under-load admission, retries/jitter/keepalive scheduling,
key-set rotation, authenticated roaming, independent peer interoperability,
IP/TCP/UDP/DNS and HTTP/TLS/network integration, profile controls, and observed
ordinary-path attempts remain obligations in [the design](/docs/wireguard-design.md).
No unprotected socket fallback or protected-success state is introduced.
