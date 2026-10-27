# WireGuard delivery evidence for #224

The [macOS migration procedure](wireguard-macos-migration.md) is delivered for
the bounded native IPv4 subset below. On 2026-10-21 the owner explicitly waived
the final independent QA/Security acceptance for #224. This records that scope
decision; it does not claim an independent review. The API conservatively keeps
`complete_isolation_verified` false because controlled fixtures do not establish
universal isolation, even when all required checks pass.

The final implementation is [PR #332](https://github.com/alecerf/mynou/pull/332).
Its [current-head checks](https://github.com/alecerf/mynou/pull/332/checks) record
native tests, real routes, build/package, CodeQL, repository checks, release
policy and tunnel load. The exact successful run and validated commit are
recorded on #224 and #111 at closure; earlier failed or superseded attempts
do not count as delivery evidence.

## Evidence to inspect

The required `validate` job compiles the zero-dependency graph, runs formatting,
the torrent network inventory and Clippy, then all Rust harnesses. It runs the
43 attempted-host-I/O scenarios with negative controls and repeats the two
route-loss scenarios against real macOS routes. Its uploaded `mynou-tests-*`
artifact contains `host-io/summary.json`, full per-scenario traces/logs and
`host-routes/summary.json`, with the ordinary harness report/logs under `rust/`.
Under each route case, `before.txt`, `lost.txt` and
`restored.txt` record the kernel's route flags, `trace.jsonl` records attempted
ordinary I/O, `fixture.log` records the assertions and `cleanup.txt` confirms
removal of the owned host route and alias. Missing observations or route transitions fail
the required job. A green fixture that was filtered out is not evidence.

The new route runner adds only a `127.0.0.2` alias on `lo0` and its host route, changes the route to
`REJECT`, restores it and deletes its own route and alias. It refuses execution outside
a disposable GitHub-hosted macOS arm64 runner and refuses to replace an existing
host route. The fixture stays unprivileged and requests route transitions using
files; product code launches no helper. Cleanup runs on errors, child failure,
timeout and catchable termination. An uncatchable runner kill can prevent
cleanup, which is why this must never run on a persistent operator machine.

The short loss retains a live stream, verifies every byte and runs new TCP,
DNS and HTTP work started during loss through the same generation after
restoration. The long loss first forces an encrypted UDP send and verifies the
outer route error, then advances the owner's test clock past session expiry,
refuses new work, restores the OS route, obtains a fresh generation and keeps
old handles revoked; the virtual listener is acquired once. These repetitions
never activate the injected-error seam. Ordinary local runs of the same tests
still inject route-class errors, including transient read errors that changing
a real UDP route is not guaranteed to produce.

Earlier delivered evidence:

| Boundary | Recorded evidence |
| --- | --- |
| Independent protocol peer | [13 wireguard-go scenarios, with independent review and limits](wireguard-interop.md#reviewed-actions-evidence) |
| Inventory and complete torrent transport | [Source inventory](network-inventory.md), [transport acceptance](torrent-transport.md) |
| Attempted ordinary I/O; pack, group, automatic and IRC metadata paths | [Observer mechanism, scenarios and limits](host-io-observer.md) |
| Retained paused engine state through rekey, expiry/replacement and authenticated roaming | [Required run 39654435141](https://github.com/alecerf/mynou/actions/runs/39654435141), #327 / PR #329 |
| Route-class read/write error recovery | [Required run 39658312256](https://github.com/alecerf/mynou/actions/runs/39658312256), #328 / PR #330 |
| Increasing-load CPU/RSS/timing, queues and flow limits | [Tunnel-owner measurements](tunnel-load.md), [real engine measurements](torrent-load.md) |
| Supported profile, file privacy and lifecycle controls | [Profile subset](wireguard-profiles.md), [operator controls and limits](wireguard-operator.md) |
| Opt-in NAT-PMP forwarding, renewal, port change and loss | #223 / [PR #331](https://github.com/alecerf/mynou/pull/331), included in the required observed scenarios; [method and limits](wireguard-operator.md#port-forwarding) |

## Accepted delivery boundary

Required Actions links and the validated commit are recorded on #224 and #111.
The evidence boundary is safe std-only
Rust, libc interposition plus source inventory rather than a kernel sandbox;
IPv4, and IPv6 inside the tunnel when the profile routes it (#372), with an
IPv4 outer endpoint; opt-in NAT-PMP forwarding (#223), IPv4 only, without
real-provider evidence;
synthetic loopback swarms;
one real host-route loss rather than every interface/default-route change;
paused engine-state fixtures and bounded load levels. No universal zero-leak,
constant-time, audited-crypto or provider-connectivity claim is supported.

Technical checks, automatic CodeQL and the prior protocol review are described
with their actual coverage, without relabeling them as the waived final review.
Existing Gluetun is untouched. NAT-PMP forwarding is delivered separately by
#223; PCP and provider HTTP APIs are unsupported. Missing forwarding is not a tunnel failure.
OpenVPN remains a separate issue under the owner's 2026-10-10 decision.
