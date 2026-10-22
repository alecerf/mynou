# Native WireGuard on macOS: installation and migration

This procedure covers the supported native IPv4 subset and its
[recorded CI evidence and limits](wireguard-acceptance.md). On 2026-10-21 the
owner explicitly waived the final independent QA/Security acceptance for #224.
That decision is not an independent review or crypto audit. The API's conservative
`complete_isolation_verified` flag remains false: these bounded fixtures do not
establish a universal isolation guarantee. Preserve the existing Gluetun service,
configuration and data for rollback; this work does not alter that deployment.

## Supported installation

The tested target is macOS 26 on Apple Silicon. Use the single executable from
a release containing the accepted changes, following the checksum and local
demo steps in the [macOS installation guide](macos.md#download-and-verify).
An older release with the same command names does not establish acceptance.
No kernel tunnel, root privileges, route changes, extra daemon or runtime helper
is needed by Mynou. The privileged route changes in CI are test instrumentation
on a disposable runner; they are never installation steps.

Use a provider-authorized WireGuard profile for one peer, with a literal IPv4
endpoint, one inner IPv4 address, explicit IPv4 DNS and `AllowedIPs = 0.0.0.0/0`.
The exact field limits and unsupported directives are in the
[profile reference](wireguard-profiles.md#accepted-subset). The parser rejects
IPv6 addresses/endpoints, endpoint hostnames, split routes, multiple peers,
`ListenPort`, hooks and scripts. Unsupported torrent IPv6 is blocked, rather
than sent through the host. OpenVPN was set aside on 2026-10-10 and requires a
separate issue.

Provider forwarding supports opt-in NAT-PMP, delivered by #223 / PR #331,
following ProtonVPN's documented P2P method. Generate a WireGuard profile with
NAT-PMP enabled at the provider and use its first inner DNS server as the
gateway; no provider is inferred from a name. After importing the profile,
enable forwarding if that supported method applies:

```sh
mynou vpn --action enable-forwarding --config ./mynou.json
```

The protected generation requests and renews its lease through virtual UDP.
The listener and announces use the granted TCP port; an unavailable/expired
lease is reported separately from VPN health and uses `downloads.listen_port`
inside the tunnel. That setting is a virtual TCP listener, not the outer
WireGuard UDP port. Do not forward the host's torrent port as a substitute.
Check `forwarding` and `forwarded_port` in VPN status. A successful handshake
does not imply an active lease or inbound reachability. Synthetic gateway and
inbound fixtures do not establish real provider connectivity. PCP and provider
HTTP APIs are unsupported; see the [operator reference](wireguard-operator.md#port-forwarding).

## Privacy and traffic boundary

Peer payload, magnet metadata and peer DNS, torrent-file HTTP/HTTPS and its
redirects, tracker TCP/UDP/DNS, enabled DHT/PEX and inbound torrent TCP use the
captured userspace VPN network. Pack inspection, library replacement previews,
automatic pack lookup and IRC-triggered torrent metadata use it even with no
download workers. A lost or replaced generation blocks old handles and delayed
results; recovery never switches those handles to Direct.

Indexer search/feed/authentication/detail traffic and IRC sessions can use the
same native network with per-source `"protection": "protected"`. Migrating a
Gluetun-protected source requires this explicit setting on every indexer and IRC
source; an omitted field preserves ordinary discovery behavior. See
[protected sources](protected-sources.md) for configuration, readiness and recovery.
Plex, catalog and the management API retain ordinary/LAN networking. The local
API is plain HTTP: retain authenticated loopback access or a trusted TLS reverse
proxy for remote management. Protected connectors ignore environment HTTP proxies.

The host sends outer encrypted UDP to the configured literal VPN endpoint.
The VPN server and the ISP can see the outer connection's source IP. The VPN
server also handles the traffic exiting the tunnel. Neither a handshake nor
these controlled fixtures supports an unconditional zero-leak guarantee,
audited-crypto or constant-time claim. This is an application transport boundary,
not an OS sandbox for other applications or a defense against a process able
to replace Mynou's executable or trusted configuration.

## Preserve data and stage the change

Read the evidence boundary above before migrating. Schedule a maintenance stop;
do not run two downloaders against the same transfer state or output files.
Keep the old deployment available for rollback and leave its configuration,
volumes and VPN credentials unchanged.

Stop the old downloader before taking a consistent private backup. Preserve
`mynou.json`, its `.env`, the complete `store_dir`, download transfer state,
download output, library folders and the old executable. The default stores are
`state/jobs` and `state/torrents`; use your configured paths if different. Retain
file selection, user pauses and priorities. Never delete or rebuild transfer
state to enable the VPN. Do not import SQLite data from the former Go release;
there is no supported importer.

For an existing Rust installation, use its original configuration and data;
do not run `init` over them. For a fresh installation, follow
[configure and start](macos.md#configure-and-start) through configuration creation,
then import the VPN profile **before the first `serve`**. A store with no VPN
intent uses Direct for backward compatibility.

Keep the profile in a trusted private directory, at a physical absolute path
with no symlink components or `..`. Its regular file must have mode 0400 or
0600 and one hard link. Do not paste profile contents into arguments, shell
history, issues or logs. Import while the new service is stopped:

```sh
mynou doctor --config ./mynou.json
mynou vpn --action import --profile /private/path/tunnel.conf --config ./mynou.json
mynou vpn --config ./mynou.json
```

Replace the placeholder with your trusted physical profile path. Import
validates the supported subset and saves private enabled intent in
`store_dir/vpn.conf`. Offline import makes no VPN connection. Confirm enabled
intent and a stored profile before starting; `blocked` before startup is
expected. An import error must be resolved before continuing. Never treat a
missing profile as permission to start unprotected.

## Start and verify

```sh
mynou serve --config ./mynou.json
```

In another terminal using the same configuration:

```sh
mynou vpn --config ./mynou.json
mynou healthcheck --config ./mynou.json
mynou status --config ./mynou.json
```

The service exposes local management while torrent work waits for a protected
generation. Expect `connecting`, then `protected` with enabled intent, a stored
profile and a recent handshake. Liveness or readiness alone does not prove VPN
protection. Status is redacted; do not publish private profiles to diagnose a
failure. See the [operator reference](wireguard-operator.md#state-and-recovery)
for `blocked`, `error` and `reconnecting` recovery.

Before admitting ordinary workloads, use a small torrent you own or are
authorized to transfer and check verified completion and expected retained
selection/pause state. With a controlled peer/tracker, check that the observed
source is the provider's egress. This operator check complements CI; it does
not replace attempted-I/O observation or establish universal provider behavior.
Test an explicit revoke during that controlled workload:

```sh
mynou vpn --action revoke --config ./mynou.json
mynou vpn --config ./mynou.json
mynou vpn --action reconnect --config ./mynou.json
```

Revocation persists blocked intent and never requests Direct. Confirm protected
recovery before proceeding. Restart once with the same configuration and confirm
that enabled intent, verified bytes, pauses and selection remain. Keep the
private whole-store backup; do not delete `vpn.conf` to resolve a blocked state.

## Rollback and operating limits

On failed acceptance, stop the new service and resume the preserved old
downloader behind its existing Gluetun protection. Do not share live state
between versions, overwrite newly verified data with an older backup, or open
a migrated store with an incompatible older executable. Restore only from a
consistent compatible backup when required by the release's storage notes.
`mynou vpn --action disable` explicitly requests Direct and is not a protected
rollback procedure.

CI exercises synthetic peers and services, including real loss/restoration of
one loopback host route. It does not establish behavior for every default-route
change, Wi-Fi sleep/wake, physical interface replacement, provider deployment or
active swarm over hours. Retained engine controls across rekey/expiry/roaming
are checked with paused transfers; route recovery separately checks live stream
bytes and new DNS/HTTP/TCP operations. Load measurements apply only to the
recorded synthetic [tunnel](tunnel-load.md) and [torrent](torrent-load.md) levels.
Keep those limits when assessing whether this supported subset fits your use.
