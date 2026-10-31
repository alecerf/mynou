# WireGuard operator controls

Mynou can persist a private WireGuard profile and run torrent operations through
its native userspace tunnel. This delivers the profile/lifecycle controls of
[#222](https://github.com/alecerf/mynou/issues/222), following the transport of
[#221](https://github.com/alecerf/mynou/issues/221). The bounded attempted-I/O,
isolation/race and increasing-load evidence for #224 is
[recorded separately](wireguard-acceptance.md). For installation, preserved
state and rollback, use the [macOS migration guide](wireguard-macos-migration.md).

## Import and inspect

Use the [supported profile subset](wireguard-profiles.md#accepted-subset).
Import the profile from the **VPN** page of **Settings**, which reads the chosen
`.conf` file in the browser and sends its text, or with `POST /api/vpn/control`
and `{"action":"import","profile":"..."}`. Never put the profile in a URL, a
command line or a log.

Import atomically stores a canonical private copy in `store_dir/vpn.conf` and
requests protection; the running service then starts the tunnel. The API
answers only this machine (see [limits](limits.md)) and is plain HTTP.

A structured import sends `{"action":"import","settings":{...}}` instead. The object
contains the exact supported WireGuard directive names (`PrivateKey`, `Address`,
`DNS`, optional `MTU`, `PublicKey`, optional `PresharedKey`, `AllowedIPs`,
`Endpoint`, optional `PersistentKeepalive`). Every value is a string, for example
`"AllowedIPs":"0.0.0.0/0"`. JSON files have the same private-file checks.
Unknown fields, hooks, injected newlines and unsupported profiles are rejected
before changing the current authorization. Supply exactly one profile or
settings object. Never paste actual profile bodies into public issues.

`GET /api/vpn` reports state, enabled intent, presence of a
profile, a redacted block reason, current installed-session handshake age and
outer-datagram counters. Counts/ages are decimal strings. No profile, key, PSK,
endpoint, DNS server, input path or provider payload is returned. A successful
handshake means the runtime is `protected`; it does not certify the complete
isolation. `complete_isolation_verified` conservatively remains false: the
controlled evidence is bounded, not a universal isolation guarantee. Port
forwarding is reported separately ([below](#port-forwarding)).

## State and recovery

States are `disabled`, `connecting`, `protected`, `reconnecting`, `blocked` and
`error`. A store without `vpn.conf` is `disabled`: everything is direct until a
profile is imported.

While the VPN is on, the `vpn` settings choose what goes through it:
`torrents` (on unless chosen otherwise), `indexers` (every indexer and IRC
source, see [protected sources](protected-sources.md)) and `tmdb` (off unless
chosen). What is chosen never falls back to Direct: new torrent work, including
metadata inspections without download workers, waits for a protected
generation, and protected sources and TMDB fail until there is one. What is not
chosen is direct, and still stops with each engine generation. Startup does not
wait for the tunnel: workers start, and only the native transfers and the
torrent listener wait for a protected generation. Plex and management always
keep their ordinary or LAN connections. Only the outer VPN UDP uses the ordinary torrent
path. Endpoint hostnames and IPv6 outer endpoints are unsupported; inner IPv6
is carried when the profile routes it (see [profiles](wireguard-profiles.md)),
and the status reports `ipv6: true` while such a tunnel is protected.
Tracker/peer DNS goes through the configured inner resolvers.

The service owns one immutable engine network. On session loss it closes
admission, finishes old API requests, stops/joins workers and drops the old
client/listener before opening a new engine. The existing tunnel runtime supplies
bounded reconnection/backoff and its terminal blocked state. A recovered engine
captures a new protected generation; old sockets and delayed results cannot
switch to it. Pending requests keep their retry budget, verified bytes and
transfer controls. User pauses and selection/priorities survive replacement.
Retirement can wait for bounded network operations or an already admitted local
publication; filesystem stalls and the Direct system resolver can delay it.
The successor is not activated while old work remains.

Controls are `POST /api/vpn/control` with `{"action":"revoke"}` (or `enable`,
`reconnect`, `disable`); the **VPN** page offers turning protection on and off.
`revoke` persists a blocked intent while retaining the private imported profile;
it never selects Direct. `enable` enables the retained profile. `reconnect`
explicitly requests a fresh owner/handshake, including after a terminal error.
`disable` is the only action that requests Direct. Imports and every mode change
retire the old generation first. Accepted controls return 202. A retiring generation refuses further controls until replacement.

The intent file is written as 0600 with a unique create-new temporary, file sync,
atomic rename and parent sync. A failed save leaves the running engine blocked;
it never resumes its old transport. An unreadable, unsafe or malformed existing
intent blocks startup rather than treating it as disabled. The trusted operator
may explicitly re-import or disable to recover. Back up the complete stopped
store privately. Do not delete `vpn.conf`: absence is the compatibility default
for an installation that has never configured VPN protection. As with main
configuration, deletion or replacement by a process with directory write access
is outside the trust boundary. Safe std checks are not an OS sandbox, atomic
snapshot against hostile writers, proof of file ownership or memory zeroization.

## Port forwarding

Inbound seeding through the tunnel can use a provider-forwarded port obtained
with NAT-PMP ([RFC 6886](https://www.rfc-editor.org/rfc/rfc6886)), the method
[ProtonVPN documents](https://protonvpn.com/support/port-forwarding-manual-setup)
for its P2P servers ([#223](https://github.com/alecerf/mynou/issues/223)). Generate
the WireGuard profile for a P2P server with "NAT-PMP (port forwarding)" enabled,
import it, then opt in with **Open a port for seeding** on the **VPN** page, or
`{"action":"enable-forwarding"}` (or `disable-forwarding`). The choice is stored in the private intent and survives
`revoke`, `enable` and restarts; a new import keeps it. The NAT-PMP gateway is
the profile's first `DNS` server (`10.2.0.1` in Proton profiles), by owner
decision; no provider name is interpreted. A profile without `DNS` cannot enable
forwarding.

Each protected engine generation requests the UDP then the TCP mapping exactly
as documented (public port 1, private port 0, 60 seconds), as virtual UDP inside
that generation only. Replies from other sources or not answering the request are
ignored. Retransmission doubles from 250 ms within a 10-second budget. Startup
waits for this first outcome. With a grant, the torrent listener opens on the
public TCP port before any tracker, DHT or peer is told about it; otherwise
it uses `downloads.listen_port` inside the tunnel. The lease is renewed at three
quarters of its lifetime (45 of 60 seconds). A held port stays advertised until
its lease expires; failed attempts then back off from 2 to 60 seconds.

The status reports `forwarding` as `disabled`, `acquiring`, `active` with
`forwarded_port`, or `unavailable` with `forwarding_reason`, apart from the
tunnel `state`: a refused or expired lease never marks the VPN failed.
When the leased port changes, expires or is first granted after startup, the
service replaces the engine generation (as on session loss), so the listener
and announces always name the current port. Transfers continue on outgoing
connections without a forwarded port. Inbound reachability still depends on the
provider and is not promised. NAT-PMP is the only supported method; PCP and
provider HTTP APIs are not implemented.

## Privacy boundary

The VPN server and underlying ISP can observe the outer connection's source IP.
These controls do not claim a universal zero-leak guarantee, audited crypto,
constant-time behavior. Resource bounds and measured
load levels are documented in the [delivery evidence](wireguard-acceptance.md).
OpenVPN is outside scope under the owner's 2026-10-10 decision.

## Validation boundary

`src/vpn/tests.rs` covers private round trips and structured imports, hostile
inputs/redaction, durable intent transitions, unsafe files, injected sync
failure, concurrent publication/revocation, startup gating, native tunnel DNS
and cancellation, suspended request retries and supervised engine replacement.
The native fixture injects already-constructed controlled loopback owners into
the operator; it does not connect a provider profile or replace #224's complete
ordinary-path attempted-I/O observation. Parser/private-file and existing torrent
protocol fixtures supply their own documented coverage.
