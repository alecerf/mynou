# Offline WireGuard timers and key sets

[#184](https://github.com/alecerf/mynou/issues/184) adds the retry, rekey,
keepalive and key-set rotation rules to the offline pieces from
[the transport](/docs/wireguard-transport.md) (#178),
[the Noise handshake](/docs/wireguard-handshake.md) (#180) and
[the cookies](/docs/wireguard-cookies.md) (#182). There is no socket, driver,
profile, engine or provider caller, so **this provides no VPN protection, leak
prevention or independent peer interoperability**. Parent #111 stays open.

## Protocol followed

The independently authored code follows the public
[WireGuard protocol](https://www.wireguard.com/protocol/) and the
[whitepaper](https://www.wireguard.com/papers/wireguard.pdf) sections 6.1 to 6.6;
no third-party implementation is copied.

| Constant | Value |
| --- | --- |
| Rekey-After-Messages / Reject-After-Messages | 2^60 / 2^64 - 2^13 - 1 |
| Rekey-After-Time / Reject-After-Time | 120 s / 180 s |
| Rekey-Attempt-Time / Rekey-Timeout | 90 s / 5 s |
| Keepalive-Timeout | 10 s |
| Retry jitter | 0 to 333 ms, inclusive |

## `Timers`: deadlines only

`Timers<E: Entropy>` is one peer's deadline state. It reads no clock and sends
nothing. The caller passes its captured generation and a monotonic elapsed time to
every call, reports events (`queue_data`, `data_sent`, `keepalive_sent`,
`data_received`, `session_established`, `cookie_stored`) and calls `poll`, which
returns a `Due` of explicit requests plus the next deadline:

- `initiate`: send an initiation. The first goes out immediately; every retry
  waits 5 s plus jitter drawn from the injectable `Entropy`, so there is never
  more than one initiation per Rekey-Timeout, also across a give-up, a restart
  and a discard. `poll` itself records the send, so a driver that polls
  repeatedly cannot exceed that rate.
- `gave_up`: the attempt is 90 s old (inclusive). Only explicit new outbound data
  (`queue_data`, or `data_sent` needing a rekey) starts another attempt with a
  fresh budget; received traffic, timers and queued data during an attempt never
  reset it.
- `keepalive`: an empty authenticated packet is due, either passive (10 s after a
  non-empty packet arrived with nothing sent since), persistent (the configured
  1 to 65535 whole seconds since the last packet sent) or the initiator's
  confirming packet after a handshake. It stays due until `keepalive_sent`.
- `expired` and `discard`: the current session reached 180 s, or 3 x 180 s passed
  since the last session was established. Discard also clears every handshake and
  timer state.

Rekey rules: the initiator rekeys when it sends at a session age of at least
120 s or receives at an age of at least 165 s; either side rekeys on the message
count signal of `Session`; data sent with no packet received within 15 s forces a
handshake. A stored cookie reply changes nothing (whitepaper 6.6): the retry
deadline is unchanged. Entropy is drawn before any state changes, so a failure
returns an error and a later poll retries. If the clock cannot represent the next
retry, the attempt fails closed as `gave_up`.

## `KeySets`: bounded rotation

`KeySets` holds at most three sessions: current, previous and an unconfirmed next.
All share one local index namespace; an install that reuses an index, has the wrong
role or generation, is already revoked or comes from the future is refused and
revoked. An initiator install makes the new session current and the old current
previous; the old previous and any next are revoked. A responder install replaces
(and revokes) next. Receive routes by receiver index and only a successfully
authenticated first packet promotes next to current (`Received::promoted`), so a
forged packet rotates nothing. Only current sends. `expire` removes sessions at
the reject age and `discard_all` removes everything.

Generation or monotonic-clock changes and `revoke` irreversibly refuse all later
calls. `Timers` and `KeySets` have separate fences, so a driver must revoke both,
and a fence violation in `KeySets` alone leaves keys resident until `discard_all`
or drop. A responder's established session does not cancel a running attempt of
our own, so redundant retries may continue until the 90 s budget ends. Safe Rust does not guarantee memory zeroization of keys, and nothing here is
a cryptographic audit.

## Evidence

Tests cover inclusive and exclusive boundaries at 5 s, 90 s, 120 s, 165 s, 15 s,
10 s, 180 s and 540 s, jitter bounds for extreme entropy, entropy failure,
generation and clock fences, the largest representable clock, every install
refusal, forged-packet promotion, revocation of replaced sessions, and a scripted
two-peer simulation through lost initiation, retry, confirmation, passive
keepalive, a vanished peer and rekey. Both peers share one reading of the
specification: this is **not** interoperability evidence with an independent
WireGuard peer.

## Remaining obligations

A driver that owns the clock, generations, indexes, sockets and the glue between
`Handshake`, `KeySets` and `Timers`, measures load and bounds handshakes globally;
authenticated endpoint roaming; the IP/TCP/UDP/DNS stack; HTTP/TLS and torrent
integration; profile controls; an independently implemented controlled peer; and
observed ordinary-path attempts, as listed in [the design](/docs/wireguard-design.md).
No unprotected fallback or protected-success state is introduced.
