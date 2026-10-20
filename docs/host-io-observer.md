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
records libc `socket`, `bind`, `connect`, `sendto`, `sendmsg` and `getaddrinfo`
calls. Connect, bind, datagram-send and lookup records are appended before the
real call, so refusal and failed resolution remain visible. Socket records
include the returned descriptor, including failure; successful binds also record
the assigned local port. Names, credentials and payload bytes are never logged.
The observer preserves errno and exits with failure if its log cannot be written.

Fixture code explicitly declares its owned loopback outer-UDP ports. The runner
checks the complete trace, including startup, shutdown and fixture subprocesses:

- Only IPv4 datagram sockets bound to `127.0.0.1:0` and assigned a declared
  outer port are allowed. Every observed socket must reach an authorized bind.
- Sends must originate from those observed sockets and target a declared
  loopback outer port in the same fixture process. Other UDP, all TCP and host
  name lookups fail validation, whether or not the underlying call succeeded.
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

```sh
TMPDIR=/private/tmp cargo test --lib --no-run --offline --locked \
  --message-format=json > /private/tmp/mynou-observer-build.jsonl
TMPDIR=/private/tmp python3 -B .github/scripts/host_io_observer.py \
  --build-json /private/tmp/mynou-observer-build.jsonl \
  --output /private/tmp/mynou-observer-results
```

Use a fresh output directory for every run. `summary.json` records scenario
counts; per-process event traces and fixture output remain beside it. Required
native CI runs the observer after the ordinary Rust suite and uploads its output
with the existing test-results artifact, including logs on failure. Changes to
the observer select native validation rather than only the Python tooling checks.
No system setting, route, privilege, global tool or installed service changes.
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

The observer does not inspect encrypted datagram content, prove server privacy,
exercise real provider connectivity, alter host routes or supply increasing-load
CPU/RSS/timing evidence. Additional orchestration paths (automatic pack search
and IRC),
the remaining concurrent expiry/roaming/restart matrix,
resource measurements and the gated macOS guide remain #224. Existing protocol,
generation and operator fixtures retain their own documented scope.
