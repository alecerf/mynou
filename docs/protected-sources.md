# Protected indexers and IRC

Turn on **Indexers** in **Settings → VPN** (`"vpn": {"indexers": true}`) to send
the discovery traffic of every indexer and IRC source through Mynou's native
WireGuard network while the VPN is on. This includes source DNS, form login,
bearer/basic authentication, searches, feeds, details, private metainfo
retrieval and IRC TLS registration, SASL/NickServ and reconnection. All sources
follow the one switch. With the VPN off, they are direct. Plex and management
remain on their ordinary or LAN connections. TMDB has its own switch.

The switch is off unless chosen. A document saved before it existed, without a
`vpn` section, turns it on when any source has the former per-source
`"protection": "protected"`. That field is still accepted, but the switch
decides. For migration from Gluetun-protected indexers/Autobrr, turn the switch
on before starting Mynou. Use `ircs://server.example:6697` for IRC sources.
Existing TLS verification and authentication requirements still apply. Source
HTTP redirects, including login and private metainfo redirects, remain refused;
no credentials or body are forwarded to a second origin.

## Readiness and recovery

Import and enable a supported native VPN profile using the
[operator controls](wireguard-operator.md). With the switch on and the VPN
on, a source refuses work when no protected generation is ready: while the VPN
connects, after revocation and on a retired engine. It never
consults the environment HTTP proxy or falls back to host DNS/TCP. The tunnel
reaches IPv6 destinations only when the profile routes IPv6; otherwise IPv6 is
rejected rather than retried on the host.

Every engine captures one immutable network generation. Ordinary rekey keeps it;
loss, profile changes and forwarding changes that retire it require the supervisor
to open a fresh engine. Source responses and final feed/IRC/acquisition mutations
check publication authority; a delayed response cannot publish through the old
engine after recovery. Publication leases cover local commits, never network waits.
IRC reads periodically check stop/revocation, so shutdown does not wait for the
source's full idle timeout.

Source cooldowns, bounded retries and IRC authentication run through the existing
schedulers. Feed observations/captured generations, IRC first receipts and admitted
demand remain in their existing durable stores. Restart retains duplicate
suppression and pending work. Ephemeral sessions are cleared when the captured
network generation changes; cookies are never persisted. Retrying failed discovery
does not create demand or bypass requester/profile/source policy.

Protection does not take part in the source binding: turning the switch on or
off keeps each source's receipts, cursors and policy.

`GET /api/indexers`, `GET /api/feeds` and `GET /api/irc` report `protection` and `protection_ready` per source without endpoints, credentials,
cookies or network generation tokens. Readiness describes transport authority,
not a successful source login or a provider connectivity guarantee.

## Validation boundary

The native fixtures under `source_network::tests` use two WireGuard owners and
virtual services, with no ordinary application listener. Positive exchanges cover
TLS form login, feed polling, detail search, private metainfo retrieval, redirect
refusal, IRC TLS/SASL, reconnect, restart and durable receipt deduplication. Other
cases cover in-flight cancellation, retained queued demand/feed cursors, generation
replacement, IPv6 rejection and configuration constraints.

The five protocol/lifecycle fixtures are included in the required
[attempted-host-I/O observer](host-io-observer.md). They permit only their declared
outer encrypted UDP endpoints, including child-process TLS/proxy fixtures. The
[source inventory](network-inventory.md) scans indexer and IRC modules too and
permits ordinary construction only in the explicit Direct adapters. This is
synthetic protocol and attempted-I/O evidence, not a kernel sandbox or live-source
qualification. Per-provider compatibility remains #336; deployment, NAT-PMP
renewal/inbound reachability and full-stack acceptance remain #344.
