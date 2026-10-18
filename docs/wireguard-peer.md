# Offline WireGuard peer composition

Issue #191 assembles `Handshake`, `Admission`, `Timers` and `KeySets` into
`Peer<E: Entropy>`. This is an offline, pure datagram state machine: **no socket,
IP stack, routing, host DNS, VPN protection, torrent integration or independent
interoperability exists here**. Parent #111 remains open. `ProtectedReady` means
only that this object owns an authenticated current transport key.

The caller supplies a captured generation, monotonic elapsed time and wall time
in `PeerClock`; wall time encodes the handshake TAI64N timestamp and must advance
between initiations. The peer never reads either clock. Production callers must
supply `OsEntropy`, not scripted bytes. Test peers use deterministic independent
streams. The one private entropy owner feeds handshake ephemeral keys, random
local indexes, admission cookie secrets/nonces and retry jitter. A failed draw
irreversibly revokes the whole peer; recovery requires a fresh object and healthy
entropy. Immediate repeated ephemeral bytes also revoke it; this is a stuck-source
guard, not a proof that entropy is secure.

## Inputs and outputs

`send` admits a bounded inner payload, `receive` takes an outer datagram plus the
observed source and a caller-owned `Load` verdict, and `poll` advances deadlines.
Returned `PeerOutput` contains outbound datagrams, authenticated padded plaintext,
a status, a count of queued payloads dropped on attempt exhaustion, and the next
poll deadline. Output datagrams always target the configured `endpoint()`, including
cookie replies. Observing an authenticated source never changes that endpoint.
Cookie replies are only useful when the observed peer is reachable through that
fixed endpoint. An IP parser must validate and remove transport padding before
any plaintext is used. Empty transport packets are keepalives, not inner payloads.

All operations that accept a clock check the whole-peer fence first. A generation
change, backwards monotonic time or explicit `revoke` irrevocably discards all
keys, credentials, pending Noise/cookies, admission state, timers and queued data.
No error selects ordinary networking. A failed operation publishes no datagrams;
cryptographic nonces/entropy may have been consumed and are never rolled back.

One local index namespace covers pending initiation, current, previous and next
sessions. Allocation draws random 32-bit candidates, with at most 16 collision
retries before an explicit error. Pending state retires at the existing five-second handshake boundary, even when
no response is received; a stale pending index cannot lock out new contact.
Failed allocation cannot overwrite a live
session or pending initiation. Existing handshake APIs retain their original
allocation behavior; only `Peer` owns the composed namespace.

The outbound queue accepts 1 to 32 payloads (at most 1424 bytes each), configured
at construction. Full queues reject the newest submission before timer/crypto
mutation. Payloads wait for an authenticated current session. A failed 90-second
attempt clears queued payloads and reports their count; explicit new data starts
another attempt. Each event returns at most the queue bound plus two datagrams
and at most one inner payload. There is no network fallback. A session can remain
usable while a replacement handshake runs.

## Authenticated events and rotation

Types 1 and 2 pass framing, MAC1 and caller-supplied load admission before Noise.
An admission cookie reply or rate-limit drop performs no Noise work. Type 3
stores the bound cookie without retransmitting or resetting a retry. Type 4
routes by receiver index; AEAD and replay acceptance precede plaintext delivery,
timer updates and promotion of the responder's next session. Previous-session
traffic can deliver plaintext but never refreshes current-session timers.

Initiator completion feeds session establishment and immediately emits queued
data or an empty confirming packet. Responder promotion feeds establishment at
the actual session creation age, then authenticated receive; a delayed confirming
packet cannot make a nearly expired key appear fresh. Every clocked operation
expires all key slots, including unconfirmed next sessions. Timer requests cause
actual initiation, keepalive, expiration, discard and queue failure in this owner.
Discard clears ephemeral Noise/cookies but preserves static credentials and
monotonic timestamp replay watermarks. A still-active attempt retains its original
budget across discard.

Simultaneous initiations use the lexicographic order of the pinned public keys.
The lower key keeps its pending initiation and ignores competing admitted
initiations without changing Noise/cookie state or reporting authentication.
The higher key yields its pending initiation only after authenticating the peer
and installing the response key. Its existing retry budget is retained, allowing
recovery from a lost response or confirmation. This deterministic local policy
is covered by same-implementation simulations, not independent interoperability.

## Evidence and remaining work

Scripted-time tests use two peers and a bounded in-memory link, exercise first
contact, loss/retry, duplication/reordering, load cookies, renewal, passive
keepalive, simultaneous contact, restart and fresh keys, plus malformed/tampered
packets, index confusion, queue limits, entropy failures and irreversible fences.
CI also retains the independent Python-generated Noise, cookie and transport
vectors used by the underlying components. These fixtures and portable tests
can also run locally; follow the Docker-first
[validation runbook](validation.md#local-validation).

The outer-UDP driver is now [the tunnel runtime](/docs/tunnel.md). Global multi-peer handshake/admission bounds, profiles,
IP/TCP/UDP/DNS stack, HTTP/TLS and torrent integration, independent controlled
WireGuard interoperability and observed leak-attempt testing remain obligations
in [the design](/docs/wireguard-design.md). No deployment or user-facing VPN
capability is changed by this increment.
