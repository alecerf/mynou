# HTTP through the native tunnel

`tunnel_http::HttpTunnel` owns one `Tunnel<OsEntropy, SystemClock>` on one native
thread. It constructs the non-`Send` tunnel inside that thread and exposes a
`Send + Sync` HTTP connector without sharing the tunnel's `Rc` state. This is a
Rust library prerequisite for [#221](https://github.com/alecerf/mynou/issues/221),
delivered by [#286](https://github.com/alecerf/mynou/issues/286). Production torrent
traffic, listeners and metadata inspection still use their existing paths.
The separate [operator layer](wireguard-operator.md) supplies CLI/API controls;
this transport alone does not establish a complete isolation claim.

## Use

Starting with an already validated `TunnelConfig`, for example one obtained
from the [offline profile parser](wireguard-profiles.md):

```rust,no_run
# fn example(config: mynou::tunnel::TunnelConfig) -> Result<(), Box<dyn std::error::Error>> {
use std::time::{Duration, Instant};
let driver = mynou::tunnel_http::HttpTunnel::start(config)?;
let deadline = Instant::now() + Duration::from_secs(5);
// Inspect driver.status(deadline) while the handshake is in progress.
// connector fails immediately unless the tunnel is already Protected.
let connector = driver.connector(deadline)?;
let http = mynou::net::HttpClient::new().with_connector(connector);
let response = http.get("https://tracker.example/announce")?;
driver.cancel();
# let _ = response;
# Ok(())
# }
```

`start` requires a literal IPv4 outer endpoint; it rejects a hostname endpoint
before constructing the runtime and makes no host DNS request. It starts the
handshake but does not wait for protected readiness. Initialization errors and
stream errors have fixed messages without keys, names, paths or OS/provider
payloads. `Debug` prints only the type name. `status` exposes the runtime's
state, generation, reason, retry data and configured/roamed outer address;
callers must apply their operator-facing redaction policy. No keys are exposed.

A connector captures the currently protected generation. Every DNS, TCP and
stream request is checked by the owner after pumping timers; the caller checks
cancellation and the published generation again before returning a result.
Session loss/replacement makes old connectors and streams unusable. Runtime
reconnection may establish a fresh generation, but callers must explicitly
obtain a fresh connector; old connectors never migrate automatically.

`cancel` permanently fences the driver, all cloned drivers, connectors and
streams. The owner revokes the runtime and closes its outer UDP socket on its
next turn. A command already admitted can finish concurrently with cancellation;
its caller rejects a result observed after cancellation. This is not an atomic
transaction with application-level publication: integration must capture and
recheck its operation context before publishing selections or other state.

Cloned drivers, connectors and streams keep the owner alive. Dropping the final
owner requests shutdown and joins the thread. Dropping the driver alone does
not cancel a connector still in use. There is no detached runtime thread.

## Destination and HTTP policy

HTTP hostname queries use only the virtual tunnel DNS client. Literal IPv4
needs no lookup. IPv6 literals (including brackets and scoped forms), zero
ports, unspecified, loopback, link-local, multicast, reserved and broadcast IPv4
are rejected. The same IPv4 checks apply to DNS answers. The virtual resolver
requests A records; an IPv6-only destination fails without host resolver or
Happy Eyeballs fallback. The inner stack also enforces its configured allowed
network and rejects its own local address.

Private/LAN IPv4 may be reached **inside the tunnel only**, subject to that
network policy. A LAN URL unreachable through the tunnel fails. No destination,
redirect, proxy setting or error selects a direct socket or resolver. The
configured DNS server may itself be private IPv4 inside the tunnel.

The [HTTP connector contract](http-connector.md) still applies. Redirects share
the same captured connector and absolute request deadline. HTTP ignores
environment proxies on this path; authenticated HTTPS still verifies the URL
hostname and certificate chain. The driver supplies a virtual byte stream, not
a TLS bypass or new trust policy. Trust anchors retain their existing file
source. HTTP framing, body limits, downgrade refusal and cross-origin credential
stripping remain in the shared client.

## Bounds and cleanup

| Bound | Behavior |
| --- | --- |
| Owner command queue | 32 commands. Status, telemetry, listener accepts and datagram sends fail immediately (`WouldBlock`) when it is full; connects, lookups, stream reads/writes/flushes and datagram reads retry every 2 ms until their deadline |
| Commands between timed pumps | At most 8; a zero-wait timer pump also precedes each command |
| Outer pump wait / caller polling interval | At most 2 ms per turn, subject to OS scheduling |
| Stream command payload/result | At most 16 KiB; standard partial read/write semantics |
| DNS/flows/packet buffers | Existing `TunnelConfig` and virtual stack bounds |
| Connect / RPC wait | Supplied absolute deadline; expired or abandoned queued commands do no work. A command the owner has already started always returns its result, even after the deadline |
| Stream read/write/flush | Configured nonzero timeout; `None` uses a 20 s safety bound |
| Startup acknowledgement | 5 s wait, followed by owner cleanup on failure |

A started command can consume received bytes or accept bytes for sending, so
its result is never dropped for lateness: the caller would otherwise lose data
or resend it ([#313](https://github.com/alecerf/mynou/issues/313)). The extra wait is one owner
action, which never blocks on the network.

These are work/queue limits, not hard real-time scheduling or CPU, RSS or
throughput guarantees; [tunnel load evidence](tunnel-load.md) records measured values. Safe std Rust cannot preempt OS calls, allocation,
entropy reads or thread scheduling. Final-owner cleanup joins the thread; its
duration is not guaranteed by the startup acknowledgement timeout. No runtime
helper, FFI, `unsafe` code or external dependency is used.

Weak leases let the owner reclaim a lookup/flow whose caller times out, drops
its response or abandons a connection, even if the command queue is full. Taking
a completed DNS result removes its lease immediately. Generation changes remove
old lease records even when an application retains a stale stream; native flow
expiry also removes its record. Each owner turn collects abandoned leases;
stack table limits bound live resources, with at most one command batch of
collection delay. Dropped streams reset/release their virtual TCP flow; they do
not wait for a graceful FIN exchange. Writes accepted before a timeout or reset
may have reached the peer, as with ordinary TCP; a timeout is not a rollback.
`flush` checks that the generation/flow remains valid; it adds no acknowledgement
or durability guarantee beyond the virtual TCP send queue.

## Evidence and remaining work

Native fixtures run a client driver and responder through an observable
loopback UDP relay. Only virtual services listen for HTTP/DNS. They cover HTTP,
A lookups, cross-name redirects, large partial reads, a tunneled TLS ClientHello
with the URL name, rejected destinations and DNS answers, an ignored proxy
configuration, pre-readiness refusal, cancellation during I/O, generation
replacement, deadlines, queue saturation and abandoned resource collection.
The shared HTTP/TLS regressions additionally exercise authenticated custom-stream
TLS. The relay asserts that HTTP/DNS plaintext is absent from its observed
WireGuard messages; this is a synthetic loopback fixture, not a universal
production isolation proof, independent interoperability or complete end-to-end
HTTPS server fixture through the tunnel.

The same owner also supplies a [torrent network](tunnel-network.md) with
virtual peers, listeners and UDP. Engine integration and inspection/orchestration publication fences are delivered
by #221. The [source inventory check](network-inventory.md) guards Direct sites.
[Operator lifecycle and private profiles](wireguard-operator.md) are provided by #222.
Production isolation/races/resource measurements and the macOS migration guide
remain [#224](https://github.com/alecerf/mynou/issues/224); parent
[#111](https://github.com/alecerf/mynou/issues/111) stays open. The running Gluetun
deployment is untouched.
