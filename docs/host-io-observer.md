# Native macOS host-I/O observation

The test-only observer in `.github/scripts/host_io_observer.py` supplies the
attempted ordinary-I/O increment [#309](https://github.com/alecerf/mynou/issues/309)
under [#224](https://github.com/alecerf/mynou/issues/224). It does not complete
#224 or the parent #111, certify universal isolation or authorize migration
away from the existing Gluetun deployment.

## Mechanism and checks

On native macOS arm64, the runner compiles `host_io_observer.c` with the installed
Clang into a temporary dylib. `DYLD_INSERT_LIBRARIES` loads it only into selected
compiled test harness processes and their fixture children. Mach-O interposition
records libc `socket`, `bind`, `connect`, `sendto`, `sendmsg`, `getaddrinfo` and network `close`
calls. Connect, bind, datagram-send and lookup records are appended before the
real call, so refusal and failed resolution remain visible. Socket records
include the returned descriptor, including failure; successful binds also record
the assigned local port. Names, credentials and payload bytes are never logged.
The observer preserves errno and exits with failure if its log cannot be written.
Socket closes retire descriptor authorization before reuse, including reuse by
an accepted TCP connection on a declared service. Closing an unauthorized or
incompletely admitted socket does not turn it into accepted evidence.
An untracked duplicated descriptor may close without granting any authorization
to send or connect; `TcpStream::try_clone` does not create another socket.

Fixture code explicitly declares its owned loopback outer-UDP ports. The runner
checks the complete trace, including startup, shutdown and fixture subprocesses:

- Only IPv4 datagram sockets bound to `127.0.0.1:0` (or the dedicated
  `127.0.0.2:0` route fixture relay) and assigned a declared outer address/port
  are allowed. Every observed socket must reach an authorized bind.
- Sends must originate from those observed sockets and target a declared
  loopback outer port in the same fixture process. Other UDP, undeclared TCP and host
  name lookups fail validation, whether or not the underlying call succeeded.
- A fixture may also declare one ordinary-path loopback TCP service (a synthetic
  indexer or IRC server) that by design never enters the tunnel. Then only that
  process may create IPv4 stream sockets, bind one to `127.0.0.1:0` assigned the
  declared port, or connect to that port on `127.0.0.1`; their address-less
  stream sends must come from a connected client socket or from the declared
  local port. Fixtures without such a declaration keep the UDP-only rule, and
  host name lookups stay forbidden everywhere. The summary counts service
  listeners, connections and sends separately from outer datagrams.
- Missing loader records, missing declarations, empty/incomplete observation,
  unexpected events and truncated JSON fail validation. An exact test name must
  appear in the compiled harness list before the runner invokes it.

Negative controls run first. Actual Rust std/Direct calls attempt refused TCP,
host `localhost` resolution, a direct loopback UDP send and default HTTP to a
closed loopback port. A separate tiny C fixture requires `getaddrinfo` failure
with `AI_NUMERICHOST`, which cannot issue an external DNS query. The runner
requires the corresponding observations; Python negative tests separately prove
that each forbidden event and undeclared endpoint is rejected. These controls
also detect stripped or ineffective dylib injection on the exercised symbols.

## Scenarios and reproduction

Observed fixtures cover sequential/parallel BitTorrent metadata and payload,
incoming seeding with PEX enabled, HTTP/authenticated HTTPS/UDP trackers, DHT
get/announce, magnet peer names, remote torrent cross-name redirects, tunnel DNS,
hostile destinations/redirects, proxy environment variables, unsupported IPv6,
and handle revocation after loss/cancellation. The protocol fixtures assert
their successful protected exchanges as well as the host-I/O trace checks.

The race/recovery increment [#311](https://github.com/alecerf/mynou/issues/311)
adds ten observed scenarios, bringing the required runner to nineteen:

- Cancellation with an in-flight stream read and retained connectors; generation
  replacement rejecting stale streams while a fresh HTTP connector succeeds;
  loss detected inside an owner command rejecting its delayed result.
- Cancellation and generation replacement concurrent with an admitted publication:
  new publication admission closes before the old commit is released.
- Native operator startup with tunnel DNS followed by durable revocation; owner
  loss retaining queued demand without consuming attempts or enabling Direct.
- Unauthenticated startup with download workers disabled, then restart preserving
  protected intent and refusing publication. Both outer sockets are declared
  from their owned local addresses, never inferred from intercepted traffic.
- Verified bytes, explicit pause and priorities retained across owner loss and a
  new protected engine; a recovered owner admits a new engine while captured old
  engine authority remains revoked.

These are deterministic synthetic scenarios with their existing behavioral
assertions plus complete attempted host-I/O observation. They do not cover every
possible scheduling interleaving or complete the remaining matrix in #224.

The explicit shared-file pack increment
[#314](https://github.com/alecerf/mynou/issues/314) adds two observed scenarios,
bringing the required runner to twenty-one. An engine with download workers
disabled inspects synthetic metadata through tunnel DNS and a cross-name HTTP
redirect. Preview leaves the accepted catalog and job store unchanged; explicit
guarded application creates only the two requested queued owners, with no
transfer state or payload. The second scenario holds preview and application
metadata responses in turn, cancels the native owner, then releases each response.
Both operations and subsequent preview/application retries fail without committing
owners or enabling Direct. The accepted catalog remains unchanged. These tests exercise the explicit
shared-file path, not automatic pack search or other orchestration paths.

The library group replacement increment
[#317](https://github.com/alecerf/mynou/issues/317) adds two observed scenarios,
bringing the required runner to twenty-three. An engine with download workers
disabled first builds an imported, monitored two-owner shared group with a
recorded baseline, its original metadata fetched through the tunnel. A quality
replacement then inspects new synthetic metadata through tunnel DNS and a
cross-name HTTP redirect. Preview leaves the job store unchanged; guarded
application creates only the two queued replacement owners bound to the new
torrent, with no transfer state, payload or change to the original import. The
second scenario holds the replacement metadata response for preview and then
application, cancels the native owner and releases it. Both operations and later
retries fail without creating owners or enabling Direct; the original group and
its import are unchanged.

The automatic pack search increment
[#319](https://github.com/alecerf/mynou/issues/319) adds two observed scenarios,
bringing the required runner to twenty-five. An engine with download workers
disabled searches a declared synthetic JSON indexer on a literal loopback port
(the only ordinary TCP); the selected candidate's two-episode metadata arrives
through tunnel DNS and a cross-name HTTP redirect. Preview leaves the job and
series stores unchanged; guarded application with the returned scope and
candidate guards creates only the two queued owners bound to that torrent, with
no transfer state or payload. The second scenario holds the candidate metadata
response for preview and then application, cancels the native owner and releases
it. Both operations and later retries fail without creating owners; the observed
ordinary connections equal the indexer requests, so no metadata fetch fell back
to the host network.

The session timer and roaming increment
[#321](https://github.com/alecerf/mynou/issues/321) adds three observed
scenarios through the real `HttpTunnel` owner and its torrent network, bringing
the required runner to twenty-eight. The client owner keeps OS entropy and the
system clock, but a test-only forward skew of its monotonic time (`cfg(test)`,
absent from the product) reaches WireGuard session ages without waiting:

- Rekey: past the rekey age, new tunnel DNS, peer and HTTP operations run
  concurrently with the time-based handshake. The new session completes in the
  same generation; an existing stream and its handles keep working with verified
  echoed bytes.
- Expiry and reconnect: with the outer path silenced, a pending read and new
  operations fail after the reject age and no network is issued while
  unprotected. The returned path yields a fresh generation through a scheduled
  retry. Old handles and the old listener stay refused, the same virtual listener
  port is acquired once and a duplicate listen is refused, and the listener
  accepts an inbound peer.
- Roaming: authenticated server datagrams from a second declared relay source
  move the client's remote without a new generation while a stream keeps intact
  bytes. Later client datagrams go only to that source; a replayed authentic
  datagram and a forgery from a third declared source are refused without
  redirecting it.

The expiry scenario found a product defect, fixed with it: a scheduled
reconnection built its generation from a fresh clock read and was then polled
at the timer pass's earlier instant. The monotonic fences revoked every retried
generation, so automatic recovery after session loss always ended blocked. It
failed closed. A deterministic core regression with a clock that moves on every
read covers it. These scenarios exercise the owner and network, not engine-level
durable pause or selection across these events, OS route changes or real
provider roaming.

The IRC orchestration increment
[#323](https://github.com/alecerf/mynou/issues/323) adds two observed scenarios,
bringing the required runner to thirty. An engine with download workers
disabled runs only its live IRC client against a declared synthetic IRC server
on a literal loopback port (the only ordinary TCP). The server registers the
client, joins the channel and announces a release matching an admitted movie
job; a `grab` rule's pinned magnet names only a literal peer inside the tunnel.
Routing fetches BEP 9 metadata from that peer and attaches the verified IRC
origin (torrent, file and length) to the existing queued job, with no transfer
state, payload or download directories. The second scenario holds the metadata
piece, cancels the native owner and releases it: routing reports unavailable
metadata and reserves nothing, and a later release announced over the same IRC
session is refused before any metadata attempt. Each run observes exactly one
ordinary connection, the IRC session, so no metadata lookup fell back to the
host network.

The retained-engine session increment
[#327](https://github.com/alecerf/mynou/issues/327) adds three required observed
scenarios, bringing the runner to thirty-three. A native client first verifies
two pieces (32 KiB) of a two-file private torrent from synthetic local bytes;
only that file is selected and the other file is absent. It persists an explicit
user pause, transfer priority 7 and low priority for the selected file. A real
operator-managed engine reopens this state on the controlled native owner.

- Time-based rekey completes while the engine's captured network performs DNS
  and verified echo traffic; the engine keeps its generation.
- Authenticated endpoint roaming moves the outer destination while existing
  stream bytes remain intact and new engine-network operations succeed, with
  the same engine generation.
- Session expiry with the relay silenced refuses engine publication and new
  work. A scheduled reconnect yields a fresh generation, but the old engine
  stays refused. It is retired and dropped before a successor opens its durable
  torrent state; captured old authority remains refused after recovery.

Each scenario checks the exact persisted pause, selection and transfer/file
priorities, both selected-file flags, intact selected bytes and absence of the
unselected file. It also checks exclusive ownership of the fixed virtual
listener port, then retires/drops the engine and reopens the durable state again
on that same port. These are retained, paused transfers, not active swarm
downloads during the event; successful DNS and echo probes exercise the engine's network. Only
the test owner's monotonic time is skewed. No host routes or provider services
are changed; `complete_isolation_verified` remains false.

The host route increment
[#328](https://github.com/alecerf/mynou/issues/328) adds two observed scenarios
through the real `HttpTunnel` owner, bringing the required runner to
thirty-five. A test-only seam (`cfg(test)`, absent from the product) emulates a
host route or interface change at the owner's outer socket calls: every outer
send fails and every read reports `NetworkDown` or `NetworkUnreachable`. The
ordinary local tests never change the host routing table. A separate required
CI step now repeats these scenarios against real OS routes as described below.

- Short loss: an existing stream's bytes and new DNS, peer and HTTP operations
  started during the loss wait, then complete in the same generation once the
  route returns, with verified echoed bytes. No client datagram leaves during
  the loss and the owner keeps serving commands.
- Loss past expiry: with the route still lost, the session expires; new
  operations, the old listener and new networks are refused, and no client
  datagram leaves. The returned route yields a fresh generation through a
  scheduled retry; old handles stay refused and the listener port is acquired
  exactly once and accepts an inbound peer.

These scenarios found a product defect, fixed with them: any outer read error
other than an ICMP refusal or reset stopped the native owner for good, so a
route or interface error reported by the outer socket permanently blocked
protection (fail closed) instead of letting the session timers recover it.
Route-class read errors (`NetworkDown`, `NetworkUnreachable`, `HostUnreachable`,
`AddrNotAvailable`) are now counted as outer path errors and keep the pump's
wait instead of spinning; other read errors still stop the owner. A core
regression covers each kind. Which error a given macOS route change reports, if
any, is not established here.

The port forwarding increment
[#223](https://github.com/alecerf/mynou/issues/223) adds one required observed
scenario, bringing the runner to thirty-six. A synthetic NAT-PMP gateway runs on
virtual UDP 5351 of the controlled native server owner. An operator-managed
engine with forwarding enabled obtains the UDP and TCP mappings inside the
tunnel, opens its torrent listener on the granted port and accepts an inbound
virtual connection there. Renewals keep the generation. A changed port makes
the engine request replacement, and its successor listens on the new port while
the old one is refused. A refused renewal ends the lease at expiry with the
tunnel still protected, and the next engine uses an ordinary virtual port. No
forwarding request, lookup or listener appears on the ordinary path.

```sh
TMPDIR=/private/tmp cargo test --lib --no-run --offline --locked \
  --message-format=json > /private/tmp/mynou-observer-build.jsonl
TMPDIR=/private/tmp python3 -B .github/scripts/host_io_observer.py \
  --build-json /private/tmp/mynou-observer-build.jsonl \
  --output /private/tmp/mynou-observer-results
```

Use a fresh output directory for every run. `summary.json` records scenario
counts; per-process event traces and fixture output remain beside it. Required
native CI runs the observer alongside the ordinary Rust suite and uploads its output
with the existing test-results artifact, including logs on failure. Changes to
the observer select native validation rather than only the Python tooling checks.
The ordinary local observer changes no system setting, route, privilege, global
tool or installed service. The separate required
[`host_route_observer.py`](../.github/scripts/host_route_observer.py) supervisor
repeats the two route scenarios on a disposable GitHub-hosted macOS arm64 runner:
it adds only a dedicated `127.0.0.2` loopback alias and its host route, changes the route to `REJECT`,
restores it, records the OS route flags and removes its own route and alias on exit.
The test stays unprivileged and uses the same libc observation and protocol
assertions, with no injected socket errors. The supervisor refuses to operate
on a persistent local machine or to replace an existing host route. See the
[delivery evidence](wireguard-acceptance.md) for artifacts and cleanup limits.
The dylib and C control are test instrumentation, never linked into or started by
the product. Product Rust remains safe, std-only and dependency-free.

## Evidence boundary

This is dynamic observation of the listed libc entry points in the selected
synthetic processes, combined with the existing source inventory gate. It is
not a kernel syscall monitor or an OS network sandbox. It cannot see arbitrary
raw syscalls, uninstrumented external programs, a different resolver ABI, writes
to inherited sockets or code deliberately evading the harness. The scope assumes
the reviewed std-only implementation; future networking APIs require extending
both the observer and its negative controls. Dylib injection depends on macOS
loader policy; loader failure is a failed check, never skipped evidence.

The observer does not inspect encrypted datagram content or prove server privacy
or real provider connectivity. Real route checks cover one loopback host route,
not every default route, interface replacement or provider deployment. Separate
[tunnel](tunnel-load.md) and [engine](torrent-load.md) runs supply increasing-load
CPU/RSS/timing evidence. The [delivery record](wireguard-acceptance.md) links
required CI, the macOS migration guide and the owner's explicit waiver of final
independent acceptance. Existing protocol, generation and operator fixtures
retain their scope.

## Protected discovery (#337)

Five `source_network::tests` fixtures extend the required observer with virtual
indexer authentication/feed/detail/private metainfo, redirect refusal, live IRC
TLS/SASL with reconnect/restart deduplication, feed cursor retention and in-flight
loss, generation replacement/IPv6 refusal and refusal of a delayed multi-source search result. They declare no ordinary service
port: only the owned outer UDP endpoints are permitted. Each test asserts a
positive protocol result before its failure cases. The TLS fixtures run in child
processes with a fixture CA and unusable environment proxies, preserving observer
instrumentation across that boundary. See [source policy](protected-sources.md).
