# Torrent network through the native tunnel

`HttpTunnel::network` returns a `torrent::transport::Network` for the
currently protected tunnel generation. It is the `Tunnel` counterpart of
`Direct` in [#221](https://github.com/alecerf/mynou/issues/221), delivered by
[#302](https://github.com/alecerf/mynou/issues/302). It runs on the same
[native owner](tunnel-http.md) as the HTTP connector. The library engine accepts it through `Engine::open_with_network`; see the
[construction and publication contract](torrent-transport.md). CLI/API VPN
activation and the complete isolation evidence remain pending.

```rust,no_run
# fn example(driver: mynou::tunnel_http::HttpTunnel) -> Result<(), Box<dyn std::error::Error>> {
use std::time::{Duration, Instant};
// Fails with PermissionDenied until the tunnel is Protected.
let network = driver.network(Instant::now() + Duration::from_secs(5))?;
let peers = network.resolve("tracker.example:6969", Instant::now() + Duration::from_secs(5))?;
let socket = network.datagram(false)?;
# let _ = (peers, socket);
# Ok(())
# }
```

## Operations

| `Network` call | Through the tunnel |
| --- | --- |
| `connect` | Virtual TCP to a usable IPv4 address, bounded by the timeout. |
| `resolve` | `host:port` through the tunnel DNS client only; a literal IPv4 needs no lookup. At most 16 answers; expired deadlines fail for literals too. |
| `datagram` | A virtual UDP socket on a random ephemeral port. IPv6 is refused. |
| `http` | The shared HTTP client over the generation-bound [tunnel connector](tunnel-http.md). |
| `listen` | A virtual TCP listener. Port 0 picks a random free port from 49152 upwards (at most 32 tries). `accept` does not block: `WouldBlock` when no peer is waiting. |

Destinations follow the [tunnel HTTP policy](tunnel-http.md#destination-and-http-policy):
IPv6 literals and answers, zero ports, and unspecified, loopback, link-local,
multicast, reserved and broadcast IPv4 are refused, both for connections and
for datagram targets. Private IPv4 is reached inside the tunnel only.

`listen` accepts `0.0.0.0` and `127.0.0.1`. Both name the virtual tunnel
interface: the listener is reachable only through the tunnel, and nothing is
bound on the Mac. The metadata inspection port reservation, which uses
`127.0.0.1`, therefore reserves a virtual port. Other addresses and IPv6 are
refused.

Datagrams behave like an OS socket: a long datagram is truncated to the
receive buffer, and a read without a datagram fails with `TimedOut` at its
read timeout (20 seconds when none is set). A send can fail when the tunnel's
outbound queue is full; it is not retried. Before copying a send payload or
queueing a command, the adapter rejects payloads larger than the stack's maximum
unfragmented IPv4 UDP payload (maximum MTU minus IPv4 and UDP headers). The
native socket also applies the configured MTU and any smaller path MTU.

Resolution requires an ASCII decimal nonzero port; signs and whitespace are
rejected before DNS. The supplied deadline is checked before resolution and
before returning answers, including literal IPv4 results.

## Generations

A network captures one protected generation, as a connector does. Every
stream, datagram socket and listener it creates keeps that generation. After
session loss, endpoint or configuration replacement, or `cancel`, every call
on them fails with `PermissionDenied`. A stale listener's `accept` therefore
stops the client accept loop instead of reporting `WouldBlock`. No call moves
to a newer generation or to the ordinary network: obtain a fresh network after
recovery. A literal-address resolution checks the generation before and after,
like a lookup.

Dropped streams, datagram sockets and listeners are reclaimed by the owner on
its next turn, as described in [Bounds and cleanup](tunnel-http.md#bounds-and-cleanup).
Resources of an old generation are released when the generation changes. Weak
leases require no Drop commands; a full owner queue does not prevent cleanup.

## Evidence and remaining work

The tunnel loopback fixtures run a responder tunnel behind an observable UDP
relay. They cover an outgoing peer exchange after a tunnel DNS lookup, an
inbound peer reaching a virtual listener, a UDP tracker-style exchange with
truncation and a bounded empty read, tracker HTTP with a redirect, refused
destinations, names and listener addresses, generation replacement and
cancellation of every handle type, and reclamation of dropped sockets and
listeners. The relay sees only WireGuard messages: peer and tracker payloads
and names are absent from it. This is a synthetic loopback fixture, not
production isolation evidence.

The [torrent acceptance record](torrent-transport.md#acceptance-evidence) covers
engine construction, generation publication fences, actual BitTorrent metadata
and payload, seeding, tracker protocols and DHT over two native owners. The
[inventory](network-inventory.md) now rejects direct sites outside Direct. Operator lifecycle remains
[#222](https://github.com/alecerf/mynou/issues/222); isolation, race and
resource evidence remain [#224](https://github.com/alecerf/mynou/issues/224).
