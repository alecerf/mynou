# Outer-UDP tunnel runtime

[#218](https://github.com/alecerf/mynou/issues/218) composes the offline layers
of [the design](/docs/wireguard-design.md): the [WireGuard peer](/docs/wireguard-peer.md),
the [IPv4/UDP/DNS stack](/docs/netstack.md) and the [virtual TCP](/docs/netstack-tcp.md).
`Tunnel` in `src/tunnel.rs` owns one physical UDP socket and drives all of them
from it. **Nothing calls it outside its tests:** there is no torrent, HTTP/TLS,
engine, profile or user-visible protection, and Mynou still has no VPN mode.
Parent [#111](https://github.com/alecerf/mynou/issues/111) stays open.

## Ownership

One `Tunnel` is the single owner of:

- the outer UDP socket, bound once to an IPv4 address (IPv6 is refused);
- the pinned endpoint, a literal IPv4 address checked by `validate_endpoint`
  when the tunnel is created or `set_endpoint` replaces it. Hostnames are not
  accepted and the host resolver is never consulted, so the encrypted outer
  datagrams are the runtime's only ordinary-path traffic. Accepting names later
  needs a design change with a bounded, cancellable lookup;
- the clock (`Clock`: monotonic elapsed time and the wall time for handshake
  timestamps; `SystemClock` in production);
- the handshake load verdict: more than `handshake_load` handshake messages
  in one second is `Load::High`, so new initiations get a cookie reply until
  they prove their address with MAC2. Only an initiation or response (types 1
  and 2) with its exact length (148 or 92 bytes), zero reserved bytes and a
  valid MAC1 for the local public key counts: that is the work a cookie
  defends. Any other type 1 or 2 datagram is refused before any Noise work,
  counted in `handshake_invalid` (and `rejected`) and never metered, so a
  flood of junk cannot force genuine peers into cookie round trips. The check
  is one fixed-size MAC per datagram and keeps no per-source state;
- the live generation: one `Peer`, one virtual UDP table, one TCP stack and one
  DNS client, all created with the same generation number.

`Tunnel` holds `Rc` state through `Peer` and is not `Send`. It starts no thread:
a caller drives it with `pump(max_wait)`, which waits at most `max_wait` (never
past the next timer) for datagrams, attempts at most `datagrams_per_pump` reads
(including socket errors), and runs every due timer. Operations and revocation
are therefore serialized by construction; a later integration owns it from one
thread.

## Data path

Inbound, a datagram longer than a WireGuard data message or empty is dropped
before any work. The rest goes to `Peer::receive` with the observed source and the
load verdict. Authenticated plaintext is dispatched by its IPv4 protocol to the
UDP table (17) or the TCP stack (6); each validates the whole packet and drops the
transport padding. ICMP (1) is offered to the TCP stack first, then to the UDP
table if no flow matched; only validated destination-unreachable advice that
quotes a packet we sent applies (see [ICMP advice](/docs/netstack.md#icmp-advice-and-fragment-policy)).
Invalid ICMP is counted in `inner_rejected`, unmatched or revoked advice in
`inner_dropped`. Every other protocol is counted in `inner_rejected` and refused.
The DNS client falls back to the shared TCP stack when a resolver sets TC, using
one of its flows.

Outbound, every inner packet that a virtual socket, the DNS client or the TCP
stack emits goes through `Peer::send`. Before a session exists the peer queues it
(at most `peer_queue`, 32 by default); a full queue returns `QueueFull` for user
sends and counts a drop for stack traffic, which retransmits. There is no other
way out: no inner byte reaches the socket unencrypted.

Outer datagrams go to the pinned endpoint until an authenticated handshake
response or transport data packet arrives from another source. Only then does
the runtime roam to that source. Both are bound to a fresh session and cannot be
replayed. A handshake initiation never moves the remote, even when it
authenticates: a fresh generation's peer has forgotten the previous timestamp
fence, so an old initiation could be replayed. A cookie reply, a replayed data
packet or anything unauthenticated never moves it either. A new generation
starts again from the pinned endpoint. Under `Load::High` the cookie reply goes
to the current remote, so a peer that moved cannot complete a handshake until
the load drops. The responder role exists for tests; Mynou is the initiator.

## Generations, loss and reconnection

Every handle (`VirtualUdp`, `VirtualFlow`, `VirtualListener`, `Lookup`) carries
its generation. When the peer reports `GaveUp`, `Expired` or `Revoked`, a
protected generation's peer is no longer `ProtectedReady` (it can expire its keys
and start a new handshake within one call), the clock
moves backwards, entropy fails, `set_endpoint` replaces the endpoint, or `cancel`
is called, the runtime revokes the peer, every virtual socket and queued
datagram, every flow and listener, and every pending or finished DNS lookup in one step.
Nothing the peer emitted in the call that lost the session is sent or delivered,
and a send in that call returns `Revoked`.
Afterwards each old handle returns `Revoked`. Delayed datagrams of the old session
fail authentication against the fresh peer, so no old-generation result can be
delivered.

States are `Connecting`, `Protected` (an authenticated current session; not a
reachability or isolation claim), `Reconnecting`, `Blocked` and `Closed`, with
a reason (`GaveUp`, `Expired`, `Failed`, `EndpointChanged`, `Cancelled`,
`Exhausted`). A lost generation is replaced after `reconnect_initial`, doubling up
to `reconnect_max`. After `reconnect_attempts` consecutive generations without a
session the runtime is `Blocked`. `cancel` stops reconnection, `reconnect` starts
a fresh budget and `shutdown` revokes everything and closes the socket.

While protected, either role with `keepalive` set sends an empty keepalive at
that interval (25 seconds by default; the test responder disables it). On the
initiator this also lets the peer start the time-based rekey on an idle tunnel. Without it an idle session expires after 180 seconds, and
the generation with it. A silent endpoint is detected the same way: the session
expires, the generation is revoked and reconnection starts.

## Bounds

| Bound | Default | Exhaustion |
| --- | --- | --- |
| Outer reads per `pump` (including socket errors) | 64 | the rest wait for the next pump |
| Outer datagram size | 1456 bytes | dropped and counted (`oversized`) |
| Framed, MAC1-valid handshake messages per second before `Load::High` | 32 | cookie replies, no Noise work |
| Peer queue without a session | 32 | `QueueFull`, or a counted drop |
| Unclaimed plus pending DNS lookups | DNS `max_pending` (16) | `Busy` |
| Reconnection delay | 1 s doubling to 60 s | capped |
| Consecutive failed generations | 8 | `Blocked` |

The virtual sockets, flows and DNS client keep their own bounds from their
guides. `Stats` only counts; no packet content is retained.

## Evidence and limits

Tests in `src/tunnel_tests.rs` run two tunnels over real loopback UDP with
scripted time, through a relay that can drop, duplicate, reorder, hold and
re-source datagrams. They cover:

- UDP, DNS and a 200 KB TCP transfer end to end, with no inner byte visible on the wire;
- loss, reordering and duplication, with replays refused;
- rekey under 300 seconds of traffic without a new generation;
- cookie replies under a flood of MAC1-valid initiations, then MAC2 admission;
- a junk handshake flood (bad MAC1, length or reserved bytes) that is refused
  and counted while a genuine initiator connects at once, with no cookie reply;
- roaming on authenticated responses and data only, including an initiation
  replayed after a new generation;
- an expiry seen only by a late pump or a send, which still revokes;
- endpoint silence, revocation and reconnection;
- bounded backoff, exhaustion and cancellation;
- delayed old-generation answers racing a new generation;
- unauthenticated, oversized and inner floods;
- the pre-session queue, clock and entropy failures, shutdown and configuration checks.

Both ends are this same code, so **this is not interoperability evidence** and
must not be reported as such; no independent WireGuard peer is involved.
Throughput, CPU and memory are not measured. Docker on Linux runs these tests;
macOS behavior of the socket is left to the required macOS CI.

## Remaining gates

An independent interoperability peer in CI ([#220](https://github.com/alecerf/mynou/issues/220));
routing every torrent path through a transport abstraction with a network-inventory
check ([#221](https://github.com/alecerf/mynou/issues/221)); the private profile,
lifecycle states and controls ([#222](https://github.com/alecerf/mynou/issues/222));
forwarded ports ([#223](https://github.com/alecerf/mynou/issues/223)); and the
isolation, race and resource evidence with the macOS guide
([#224](https://github.com/alecerf/mynou/issues/224)). Until then nothing here
changes user-visible behavior.
