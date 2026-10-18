# Independent WireGuard interoperability fixture

Issue [#220](https://github.com/alecerf/mynou/issues/220) authorizes a narrow
exception for an independent peer in tests and CI only. Mynou remains safe Rust
std with zero Cargo dependencies. No peer code or binary enters Cargo, product
runtime, packages or releases.

The peer is the WireGuard project's independently authored
[wireguard-go](https://git.zx2c4.com/wireguard-go), MIT licensed, tag
`0.0.20250522`, commit `f333402bd9cbe0f3eeb02507bd14e23d7d639280`.
It has separate Noise, cookie, timer and replay implementations. The
[peer Dockerfile](/.github/scripts/Dockerfile.wireguard-peer) pins:

- Go module `golang.zx2c4.com/wireguard@v0.0.0-20250521234502-f333402bd9cb`,
  `h1:whnFRlWMcXI9d+ZbWg+4sHnLp52d5yiIPUxMBSt4X9A=` (Go checksum
  transparency log record 37626092).
- Go 1.26.4 build image:
  `golang:1.26.4-alpine3.22@sha256:727cfc3c40be55cd1bc9a4a059406b28a059857e3be752aa9d09531e12c20c56`.
- Runtime image:
  `alpine:3.22.6@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8`.

The build verifies the module and its three transitive module hashes against
`go version -m`, and checks the compiler version. Only the build stage downloads
Go modules; the proxy has no direct VCS fallback, and checksum verification is
on. Go's toolchain cannot auto-upgrade. No upstream source is copied into this
repository. The image contains the peer binary and BusyBox, without wg-quick,
profile hooks or additional package installations. Moving any pin needs a
separate reviewed change.

## Isolation

The Python std driver runs the peer with `--network none` (only loopback), no
published ports, no credentials, checkout or host mounts. It drops all
capabilities except `NET_ADMIN`, grants only `/dev/net/tun`, keeps the filesystem
read-only, and uses anonymous volumes for the UAPI socket and log (the reader enforces size limits).
`no-new-privileges`, Docker's default seccomp/AppArmor, 64 processes, 128 MiB
memory and one CPU constrain the peer. TUN, addresses and routes exist only in
that disposable network namespace. Namespaced NET_ADMIN still exposes kernel
netlink/TUN code; this is a test runner boundary, not an untrusted-code sandbox.

The test builds offline as the Rust image's unprivileged `tester`. The test
container then joins only the peer namespace, with all capabilities dropped,
a read-only root filesystem and target volume, ephemeral `/tmp` and result
scratch, and read-only peer volumes. It uses uid 0 solely to connect to the
root-owned UAPI socket. Fresh synthetic keys are never logged; UAPI replies
filter private and preshared keys before returning to the harness. The peer
receives no production configuration or data. Containers and anonymous peer
volumes are removed even on test failure or timeout. The named Cargo cache
contains only build output.

## Scenarios and observations

The Rust fixture uses Mynou's public `Tunnel` API and exchanges nonce-bearing
UDP payloads through std sockets on the peer's kernel tunnel address. No public
media, external peer, notification or provider is contacted.

| Scenario | Required observation |
| --- | --- |
| Mynou initiator and peer initiator, without PSK | Authenticated handshake and UDP delivery in both directions; peer log confirms roles. |
| Both initiator roles with matching PSK | Same handshake and data assertions. |
| Mismatched PSK in each direction | Never `Protected` during a seven-second observation including retry; no inner delivery; rejection observed by the receiving implementation. |
| Wrong key pinned by either side; unknown static key to Mynou responder | No session; peer log or Mynou counters show refusals. |
| Mynou rekey | Move monotonic and wall clocks forward 121 seconds together; a new peer-logged handshake and changed UAPI handshake time, then data both ways in the same generation. |
| Peer rekey | Wait for its real 120-second timer with regular traffic; observe a fresh peer-initiated handshake and data both ways in the same generation. |
| Peer under load | Paced MAC1-valid junk from 127.0.0.2, cookie reply to Mynou, a junk cookie in the same logged second before the accepted retry, and a later junk cookie while load continues, then data both ways. |
| Mynou under load | Low handshake-load threshold, positive/increasing high-load counter, peer log shows cookie receipt, retry and first accepted response in that order, then data both ways. |

The log reader has byte, line-count and line-length bounds. The JSON-lines result
record has a size limit and requires every named scenario exactly once with a
passing outcome. Missing, duplicate, unknown, failed or malformed results fail
the driver. There are no scenario retries into success. An optional single
scenario run is clearly marked diagnostic and never gate evidence.

## Run and evidence

From the Issue worktree, with Docker available:

```sh
docker build -t mynou-wireguard-peer -f .github/scripts/Dockerfile.wireguard-peer .github/scripts
docker build -t mynou-local-rust -f .github/scripts/Dockerfile.rust .github/scripts
python3 .github/scripts/wireguard_interop.py --target-volume mynou-rust-220
```

The separate [WireGuard interoperability workflow](/.github/workflows/wireguard-interop.yml)
runs only on manual dispatch, on `ubuntu-24.04` with
read-only workflow permissions and checkout credentials disabled. It has a
20-minute job timeout and is deliberately **not a required branch check**.
Its step summary records module version/hashes, Go version, base-image digests,
actual peer image ID, kernel, architecture, scenario outcomes, durations and
observation sources. QA and Security must read a real green run before adding
this job to required checks in a separate reviewed change.

## Reviewed Actions evidence

[Actions run 39033029167](https://github.com/alecerf/mynou/actions/runs/39033029167),
job `independent-peer`, passed on 2026-10-17 at Mynou revision
`7c41ae1944757ca62502e86a00621f33ffc5a052`. Its recorded result is **all 13
required scenarios passed**, with no failed or ignored Rust tests (172.16 s).
The peer was wireguard-go `0.0.20250522` at the module/source pin above,
built with Go 1.26.4 from the digest-pinned images above. The runner recorded
Linux kernel `6.17.0-1022-azure`, architecture `x86_64`, and peer image ID
`sha256:57893542323c11c2ffd2a0d958b2d7874986de8cfa6cb6c32abef3231eb58584`;
the isolated traffic was IPv4
inside and outside the tunnel, over loopback.

The [Security review](https://github.com/alecerf/mynou/pull/257#issuecomment-6237176037)
and [QA review](https://github.com/alecerf/mynou/pull/257#issuecomment-6237176297)
read that real run and independently reproduced all 13 scenarios locally.
QA's rejection concerned the missing evidence handoff in these documents;
it found no product implementation defect. Those verdicts cover the recorded
revision only; revised PR heads still require fresh QA and Security verdicts.

This evidence satisfies the independent-interoperability gate in
[the design](/docs/wireguard-design.md) within the boundary below. The job
remains non-required; adding a required branch check is a separate reviewed
change. This evidence does not resolve any other protected-delivery gate.

The evidence covers one peer version on Linux/Docker, IPv4 inner and outer
traffic, and loopback without real loss or path MTU. It proves nothing about
native macOS behavior, torrent-path isolation, leak protection or universal
WireGuard compatibility. The other protected-delivery gates remain separate.
