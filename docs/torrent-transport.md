# Torrent engine transport

`Engine::open_with_network(config, network, start_downloads)` constructs a
library engine with exactly the supplied `Arc<dyn torrent::transport::Network>`.
Passing `HttpTunnel::network(deadline)?` supplies the native protected transport.
This constructor does not construct Direct, and it fails if that captured tunnel
generation is unavailable. `start_downloads=false` supports metadata management
without a torrent client or listener worker. The standard `open`, `open_serving`,
`open_for_management` and `open_for_preview` constructors honor the
[persisted operator VPN intent](wireguard-operator.md). Without that intent,
they retain unprotected Direct behavior.

The engine owns an immutable network for its lifetime. Its transfer client and
all metadata callers share that network. There is no runtime transport setter
or automatic fallback. To change transport or generation, stop/join workers and
drop the old engine/client before constructing its replacement. Existing Direct
connections cannot be converted in place. Durable operator state and supervised engine replacement are described in the
[operator reference](wireguard-operator.md); the generic constructor trusts the caller's
Network implementation and does not certify a custom network as protected.

## Converted call sites

| Path | Transport |
| --- | --- |
| Sequential and parallel outgoing peers; PEX-discovered peers | `Network::connect`; PEX addresses enter the same bounded peer set. |
| Incoming peers and seeding | `Network::listen`, accepted `NetworkStream`. |
| HTTP/HTTPS trackers and redirected requests | `Network::http`, preserving its connector and TLS host authentication. |
| UDP trackers and DHT queries/announcements | `Network::datagram`; DHT bootstrap uses `Network::resolve`. |
| Magnet `x.pe` names | Deferred `Network::resolve`, with no admission/restart DNS. |
| Remote torrent files | Source parsing through `Network::http`, including redirects and TLS. |
| Standalone magnet/HTTP/local metadata inspection | Explicit Network, including the temporary virtual listener reservation. |
| Automatic packs and shared-file acquisition | Engine Network for inspection, publication lease before report/commit. |
| Library group replacements | Same engine Network, including management without workers. |
| IRC-triggered torrent metadata | Same engine Network, followed by publication and existing demand/policy fences. |

Plex, catalog, indexer searches, the IRC session itself and management traffic
keep their existing ordinary-network behavior. The native tunnel uses only its
literal IPv4 outer endpoint on the host; tunnel policy carries private IPv4
inside, refuses unusable IPv4 and IPv6, ignores proxies and never falls back.
See [the tunnel adapter](tunnel-network.md) for supported destinations and limits.

## Publication and revocation

`Network::generation` reports an opaque captured protected generation (None for
Direct) and rejects revocation. Native tokens include a random 128-bit owner
identity as well as the runtime counter, so recreating an owner cannot reuse
a previous process/session preview token. Shared-file/group plan guards and automatic
pack candidate guards include it only when protected, so a preview cannot
authorize an apply in a replacement generation. Direct guard hashes are unchanged.

`Network::publication` returns a local lease. Direct has no revocation; a
protected implementation must check its captured generation and serialize
revocation with admitted leases. Native Tunnel does so with a read/write fence:

1. Metadata/network I/O runs without a publication lease.
2. Before publishing its result or committing inspected metadata, the caller
   obtains a lease and rechecks the engine stop flag and existing policy/store
   guards. Inspection also checks before I/O and before returning metadata.
3. Tunnel loss/replacement or cancellation first closes admission, then waits
   for admitted local publications. Results arriving after that closure fail.
   Already admitted commits finish before revocation returns; the owner does
   not dispatch more network commands while waiting for them.

Leases cover local report construction and bounded store work only. Never hold
a lease during network I/O or call cancel on the same thread while holding a
lease. A slow filesystem can delay completion of an already admitted commit
and revocation; no hard wall-time or resource claim is established here.
Streams and sockets keep their own generation checks on every owner operation.
Transfer admission, authenticated metadata, verified peer-piece results and
completion publication are checked; retained verified bytes are not deleted.
A stopped engine rejects new metadata publications. Operator-driven stopping and durable recovery use the separate operator layer
(#222); full isolation/race/resource evidence remains #224.

## Acceptance evidence

`src/torrent/tunnel_tests.rs` runs two native WireGuard owners on loopback with
only outer UDP sockets on the host. Services and DNS exist in virtual stacks:

- Real BitTorrent metadata inspection, magnet peer-name lookup, payload download
  and incoming seeding, in sequential and parallel modes, with PEX enabled for
  a public torrent. Exact payload bytes are checked. Revocation refuses new
  admission/inspection and retains downloaded bytes.
- Real HTTP and authenticated HTTPS tracker replies, UDP tracker connect/announce
  transactions and stopped counters, and DHT get_peers plus announce_peer.
- Remote `.torrent` metadata across a cross-name HTTP redirect and virtual DNS.
  The HTTPS test uses the existing synthetic certificate fixture in an isolated
  test subprocess; it does not change global trust or disable authentication.

`src/tunnel_network_tests.rs` and `src/tunnel_http_tests.rs` additionally cover
all handle types, readiness, generation replacement, encrypted relay observations,
DNS and destination refusal, HTTP redirects/proxies, queue/deadline limits and
publication/cancellation ordering. Existing source and TLS fixtures cover HTTPS
metadata and rejected authentication/downgrades over the shared connector.
The four orchestration harnesses (`automatic_packs`, `shared_episode_files`,
`shared_group_replacements`, `irc_routing`) return delayed successful reads after
synthetic revocation and verify no report/admission/commit, without download
workers; the existing Direct journeys remain regression coverage.

The [inventory gate](network-inventory.md) rejects direct sites outside Direct,
including an otherwise matching inventory; negative alias/helper fixtures run
in CI. This lexical check and these bounded synthetic scenarios establish their
stated transport contracts. They do not establish observation of every attempted
host I/O, all concurrent failure/restart cases or measured increasing-load
resources. Those remain #224; #111 stays open until lifecycle and final evidence
are delivered. The running Gluetun deployment is untouched.
