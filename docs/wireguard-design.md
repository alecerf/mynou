# WireGuard transport design

This is the pre-implementation Security design for [#111](https://github.com/alecerf/mynou/issues/111),
delivered by [#167](https://github.com/alecerf/mynou/issues/167). **Mynou currently
has no VPN transport or VPN leak protection.** This document adds neither. The
parent stays open until implementation and its verification gates are complete.

The recorded owner decisions are an integrated userspace WireGuard tunnel,
WireGuard only, and native macOS on Apple Silicon. OpenVPN, Docker, kernel
interfaces/routes, root privileges, FFI, unsafe Rust, Cargo dependencies and
runtime helper programs are outside this design. No existing deployment changes.

## Boundary and threat model

In protected mode every torrent-related connection, datagram, name lookup and
listener is a virtual flow in a userspace IP stack. Its only physical-network
outlet is authenticated WireGuard encryption to the configured peer. Loss of
the tunnel blocks virtual flows; it cannot make them ordinary sockets. A
periodic IP check, local source-address bind or reactive kill switch is not this
boundary.

The property applies to the reviewed Mynou torrent implementation, not arbitrary
modified code, other applications or an actor controlling the owner's process
and private configuration. It is not an OS network sandbox. The VPN server and
ISP necessarily see the outer connection's source IP; torrent counterparties
should see the VPN egress. This is a design obligation, not a measured guarantee.

Treat peers, trackers, DNS answers, redirects and unauthenticated outer packets
as hostile. They cannot select a Direct transport, proxy, host resolver, new
VPN public key or local listener. Resource exhaustion, malformed packets,
authentication failures, unsupported destinations and uncertain configuration
durability fail closed. Peer authentication does not grant acquisition authority:
existing demand, ownership, numbering, hashes and captured-policy checks remain.

Plex, catalog requests, indexer searches/feeds, IRC and management remain outside
the tunnel. A torrent metadata download or peer inspection is inside it even
when initiated by one of those subsystems. Classification is by the operation,
not a hostname guess. A LAN-only torrent URL that cannot be reached through the
tunnel fails explicitly; there is no local-network bypass. A local `.torrent`
file needs no network. Protection is not enabled silently for existing users.

## Current paths that must change

Source audit: `1e9d6b5fb8e6d48d495387a4018663f90db169ba`, read only.

| Current path | Required protected behavior |
| --- | --- |
| `src/torrent.rs`, `Source::parse_with_deadline`: magnet `x.pe` uses `ToSocketAddrs`; HTTP `.torrent` fetch uses `HttpClient` | Separate syntax parsing from network work. Resolve peer names and fetch remote metadata through the captured torrent context. Preserve input/body bounds and deadlines. |
| `src/torrent/wire.rs`, `Peer::connect_with_stop`, message I/O | Virtual TCP connect and bounded cancellable stream I/O, with no ordinary socket retry. |
| `src/torrent.rs`, `Client::open_with_policy` and `listen_loop` | Virtual listener on the tunnel address, not `0.0.0.0` on the Mac; no listener before protected readiness. |
| `src/torrent/discovery.rs`, HTTP/UDP tracker and DHT lookup | Tunnel HTTP/TLS, virtual UDP, tunnel DNS for tracker and bootstrap names. Disabled/private-torrent discovery rules still apply. |
| `src/torrent/parallel.rs` and sequential download/discovery/seeding paths in `src/torrent.rs` | The same captured context for every worker, metadata exchange, reconnect, announce and seed discovery. |
| `src/torrent/inspection.rs`, `inspect_metadata` | A required torrent context, including the ephemeral inspection listener, HTTP source fetch, tracker request and peer metadata exchange. No network-taking standalone default. |
| `src/pack/shared.rs` and `src/pack/automatic.rs`, metadata inspection callers | Pass the engine's captured context to manual/automatic pack inspection. Preview or no-payload status is not permission for direct traffic. |
| `src/library/groups.rs`, `Engine::library_group_before`, replacement-preview inspection | Pass the captured context even without transfer workers. Fence lookup and preview results by network generation alongside the current group ownership/policy checks; stale results cannot authorize a replacement. |
| `src/irc/routing.rs`, `Engine::route_announcement`, candidate metadata inspection | The IRC connection stays outside the tunnel, but its torrent lookup uses the captured protected context. Recheck admitted demand, captured policy and network generation before lookup and selection publication; a delayed result cannot create demand or bypass revocation. |
| `src/net.rs`, `HttpClient::single_request`, `DeadlineStream`; `src/tls.rs`, `TlsStream` | Support an explicitly supplied connector/stream, with deadline/cancellation semantics. Protected HTTP cannot invoke the host resolver or environment proxy; every redirect preserves the same context. |
| `src/engine.rs`, construction and worker startup | Validate private VPN configuration and acquire one context before any torrent-specific networking. Pass it to inspection paths even when download workers are not started. |

These are migration obligations; the current APIs still use ordinary sockets.
Review the complete reachable call graph again when integration changes it.

## Transport and generation ownership

One engine-owned network context exposes resolve, TCP connect/listen and UDP
operations with a shared deadline, cancellation and configuration generation.
Wire message I/O and TLS use its stream abstraction rather than concrete
`std::net::TcpStream`. Protected construction yields only virtual handles.
Non-torrent clients keep their existing connector and verified TLS behavior.

Keep physical socket creation in two narrow implementations: Direct for an
explicitly unprotected engine, and the WireGuard outer-UDP driver. Protocol,
IP-stack, DNS and torrent helpers receive handles, never a default constructor
that can create a Direct client. Private context construction owns mode choice;
protected errors have no branch to Direct. Merely calling `without_proxy()` on
today's HTTP client would still leak and is insufficient.

Protected HTTP ignores `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY`; it neither
connects to a host proxy nor resolves it. Redirects retain the captured context,
existing origin/credential rules, TLS certificate/hostname validation and one
operation deadline. HTTP bodies, response bounds and authenticated torrent
hashes keep their current checks. TLS must not contain an emergency TCP connector.

Every virtual handle and queued operation captures a generation. Revocation is
checked at the final inner-packet admission point, serialized with generation
replacement, not only when a worker starts. Old results cannot publish readiness,
ports or provider state into a newer generation. Pack plans and network-taking
controls include this fence alongside their current policy/identity checks.

Initially mode/profile changes require a service restart. Shutdown revokes
admission, cancels/joins torrent workers and closes listeners/flows and the outer
driver. Startup never opens Direct while constructing a protected engine.
Unprotected-to-protected migration therefore requires stopping the old service,
not starting two engines or retaining direct connections. Later live changes
must prove the same quiescence before publication. Disabling protection is an
explicit durable operator choice; authentication/timeouts never disable it.

An encrypted outer datagram already submitted to the OS may leave after shutdown;
it is still tunnel traffic. No newly admitted inner payload may use a revoked
generation. A dropped/expired tunnel revokes key readiness immediately; flow
queues stay bounded, fail or resume under the same protected mode, and never
reach Direct. Preserve verified files and durable pause/selection/seed policy;
restart rechecks bytes as it does today. Retries cannot create new demand.

## First supported profile subset

The initial implementation targets one peer, one assigned inner IPv4 address,
an IPv4 default route in `AllowedIPs`, explicit inner DNS server IPv4 addresses,
and a literal outer IPv4 `Endpoint` with a nonzero UDP port. Optional PSK and
persistent keepalive are supported. An endpoint hostname is rejected in this
first subset, avoiding host DNS bootstrap. Inner or outer IPv6 is explicitly
rejected, including IPv6 answers, literal peers, trackers and redirects; there
is no Happy Eyeballs fallback on the Mac.

Import the understood `[Interface]`/`[Peer]` fields only: `PrivateKey`, `Address`,
`DNS`, `MTU`, `PublicKey`, `PresharedKey`, `AllowedIPs`, `Endpoint` and
`PersistentKeepalive`. Unknown/duplicate fields, additional peers/addresses,
partial routes, `ListenPort`, scripts/hooks and unsupported directives fail
explicitly. Configuration import performs no DNS, subprocess or network work.
Final parser byte/line/list and MTU/keepalive bounds must be specified and tested
in its implementation Issue; a design does not claim importer compatibility.
The [offline parser](wireguard-profiles.md) now implements this `.conf` subset
and documents its bounds. A separate bounded private-file reader is available
through the same API module; it neither writes settings nor activates protection.
Durable import, persistence and operator controls remain #222, after transport
integration #221.

Credentials use private configuration/environment references and never enter
public diagnostics, URLs, raw profile dumps or `Debug` output. Use the existing
private-file/link checks for any durable imported profile. No session keys or
ephemeral private keys are persisted. OS randomness is mandatory; failure blocks
handshake. Key disposal is best effort under safe std Rust and an optimizing
compiler, with no guaranteed memory zeroization or constant-time claim.

## WireGuard and userspace network obligations

Follow the [WireGuard protocol specification](https://www.wireguard.com/protocol/)
(read 2026-10-11), its whitepaper and the referenced primitive standards. Author
new code independently; do not copy a third-party implementation. Existing
X25519/ChaCha20-Poly1305 are building blocks, not a WireGuard audit. Add BLAKE2s
with proper keyed/output-length forms, HMAC/HKDF over BLAKE2s, HChaCha20 and
XChaCha20-Poly1305. Do not substitute SHA-256 HKDF or truncate a 32-byte keyed
BLAKE2s result to stand in for the protocol's 16-byte MAC.

Implement Noise IKpsk2, pinned server identity/optional PSK, authenticated
initiation/response/cookies, timestamp handling, key confirmation, data counters,
authentication-before-replay-window updates, protocol rekey/reject timers and
bounded handshake retries. Counters never roll back under a key; wrap/expiry
blocks send. Restarts generate fresh ephemeral/session keys. The initiation
timestamp must satisfy the peer's strict freshness rule; clock failure/backward
movement cannot trigger unauthenticated fallback or a fabricated success state.
Cookie replies are not proof of peer identity and cannot cause endpoint roaming.

Only a packet authenticated under the pinned peer's valid session/handshake can
update a roaming endpoint; unauthenticated packets cannot redirect outer sends.
Keep replay state per receive key, bound current/previous/in-progress sessions,
check receiver indices and framing before allocations, and reject invalid
lengths/reserved bytes/types and over-budget datagrams. Session expiration and
generation revocation are synchronized with packet admission.

The stack implements IPv4 validation/checksums, UDP and TCP client/listener
semantics, including loss/reordering, sequence arithmetic, ACK/window validation,
flow/congestion control, retransmission, FIN/RST and deadlines. Validate ingress
source/destination against the configured peer/inner address before delivery.
No host-route discovery, kernel TUN, raw sockets or direct ICMP is involved.
Choose a conservative tunnel MTU from the outer encapsulation/padding overhead;
reject oversized/fragmented traffic explicitly until bounded fragmentation and
reassembly are independently verified. TCP MSS must fit that MTU. ICMP/path-MTU
handling cannot turn an injected error into a direct retry or unchecked MTU.

DNS packets go only through virtual UDP/TCP to configured tunnel resolvers.
Bound names, records, compression traversal, pending transactions, cache entries
and TTL; validate reply source/port, transaction and question. Truncation may
retry via virtual TCP, never the host resolver. Reject unsupported address
families. All flow tables, receive/send queues, retransmit state, replay windows,
timers and outstanding DNS/handshake work have explicit global and per-flow
budgets. Exhaustion reports a bounded error/backlog and preserves retained data.
Those are concurrency/message limits, not new retained-inventory caps.

## Lifecycle, listener and forwarding

Expose disabled, connecting, protected, reconnecting, blocked and error states
with redacted reason codes and generation/freshness. Protected readiness means
authenticated usable session/stack state; a bound UDP socket or sent handshake
is insufficient. It does not mean every tracker, port or DNS query is reachable.
Bound automatic retries, idle keepalive and cancellation; protocol timers take
precedence over arbitrary reconnect scheduling. Failed attempts report blocked
state and never publish a protected success or spin without a budget.

Inbound seeding accepts only decapsulated virtual TCP for the inner listener.
There is no public host TCP listener in protected mode. Use the effective tunnel
listener port consistently in tracker/DHT announcements and inspection; preserve
private-torrent and seeding policy. A provider-assigned port is activated with a
generation-bound virtual listener before advertising it. Expiry/failure stops
advertising the stale port and reports forwarding unavailable while the tunnel
can remain protected; no host listener or direct forwarding request replaces it.

No provider forwarding protocol is established by the repository today. NAT-PMP,
PCP or an HTTP API must not be assumed from provider names. Initial forwarding
can be reported unsupported; a provider-specific increment requires authorized
documentation and synthetic request/renewal/error fixtures. Its requests/DNS
use the protected context too. Seeding through outgoing peer connections remains
subject to the VPN/provider's reachability; do not promise inbound connectivity.

## Verification gates before protected delivery

Validate locally first with the installed tools, using native macOS for
platform-specific checks. Docker is limited to the optional independent-peer fixture. Required GitHub Actions checks
remain the final gate; record any coverage that cannot be exercised locally.
Product remains safe Rust std only, with no runtime helper fallback. Fixtures use
controlled endpoints and synthetic payloads, never public media, private tracker
accounts or real notifications.

| Gate | Required evidence |
| --- | --- |
| Primitives/protocol | Public vectors with provenance, keyed and boundary cases, negative AEAD/low-order DH cases, fresh-session/counter/replay/timer/cookie/clock fixtures. |
| Independent interoperability | Satisfied within the [recorded fixture boundary](/docs/wireguard-interop.md#reviewed-actions-evidence): [Actions run 39033029167](https://github.com/alecerf/mynou/actions/runs/39033029167) passed all 13 scenarios at `7c41ae1944757ca62502e86a00621f33ffc5a052` against independent wireguard-go `0.0.20250522`, including both handshake/data roles, rekey, PSK, cookie under load and wrong-key refusal. Linux/Docker, IPv4 inner/outer, loopback only; no macOS, torrent isolation, leak or universal compatibility claim. |
| Stack and DNS | Deterministic loss/reordering/corruption/flow/window/MTU/fragment cases, TCP listener/close, UDP checksums, DNS spoof/compression/truncation and bounded exhaustion. |
| Complete network inventory | CI source-policy checks cover physical sockets, host DNS and default HTTP/TLS construction, including aliases/helpers and inspection/orchestration callers. Negative fixtures demonstrate checker failures. These checks aid call-graph review and are not a formal proof of arbitrary Rust programs. The [inventory check](/docs/network-inventory.md) is in place; it passes only by listing today's direct sites, which #221 must convert and remove. |
| Isolation | Controlled ordinary-path trap endpoints and observable tunnel egress for TCP peers, inbound seeds, UDP/HTTP/HTTPS trackers, DHT/PEX, magnet names, metadata inspections and remote torrent files. Test redirects, environment proxies, literal/answered IPv6, loopback/LAN references and malformed endpoints. Observe attempted lookups as well as successful connections. |
| Races and restart | Concurrent new operations with session loss/expiry, route changes, roaming, reconnect, configuration revocation/restart and delayed old-generation results. Ordinary path sees only authorized outer VPN UDP; unsupported flows stay blocked. Verify retained bytes, durable pause/selection, no fallback acquisition and no duplicate listeners. |
| Resources and claims | Measured increasing-load CPU/memory/timing and explicit queue/flow limits on macOS arm64. Independent QA/Security read the real current-head CI and supported-subset limits before a protected mode or installation guide ships. |

The [independent peer fixture](/docs/wireguard-interop.md) records the approved
wireguard-go pin, confinement, scenarios and evidence limits. Its separate CI job
is not a required branch check. The green run and its QA/Security review are
[recorded with their limits](/docs/wireguard-interop.md#reviewed-actions-evidence);
only the independent-interoperability gate is satisfied within that boundary.
All other protected-delivery gates remain separate.

Include separate isolation/revocation fixtures for pack inspection, library
group replacement previews and IRC-triggered metadata lookup, including their
no-transfer-worker and delayed-result paths. Check the authorization fences on
each trigger as well as protected networking; an IRC session itself remains
outside the tunnel.

The ordinary-path test harness must observe actual socket/name-resolution
attempts, including failed ones; asserting zero accepted trap connections alone
cannot prove no attempted leak. Specify the CI observation mechanism and its
coverage in the integration Issue. Do not weaken std-only or repository policy
to construct it. A controlled reference peer/observation facility is a delivery
prerequisite, not a reason to use a real provider or claim unobserved behavior.

Passing controlled scenarios supports only their stated boundary and coverage.
It is not a universal zero-leak, constant-time or independently audited-crypto
claim. A blocked gate keeps protection unavailable rather than silently using
Direct. Existing unprotected behavior is documented explicitly until delivery.

## Remaining delivery on #111

After separate QA/Security approval of this design, deliver bounded increments
for primitives; WireGuard session/datagram handling; IPv4/UDP/DNS; TCP/listener;
the transport/HTTP/TLS/inspection integration; private profile and lifecycle
controls; verified provider forwarding if supported; and full isolation/resource
evidence with the macOS installation/migration guide. Create each implementation
Issue when its prerequisites are executable. Do not remove the owner's existing
protection or close #111 on this design or an isolated crypto increment.
