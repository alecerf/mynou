## Fixed: Keep tunnel stream bytes intact under load

When many connections shared the library tunnel network, a read or write whose
deadline expired while the tunnel was running it could lose received bytes or
send them twice, and a full command queue made callers retry in a busy loop.
Started operations now always return their result, and stream operations wait
for queue space until their deadline. Torrent VPN protection remains
unavailable ([measurements and limits](/docs/tunnel-load.md)).

## Fixed: Bound tunnel network admission before dispatch

The library tunnel network now rejects expired literal-address resolutions and
malformed authority ports before DNS, and refuses oversized UDP payloads before
copying or enqueueing them. This preserves the adapter's deadline and command
memory bounds; production VPN activation remains pending ([scope](/docs/tunnel-network.md)).

## Added: Supply a torrent network backed by the native tunnel

The Rust library can give the torrent engine a network whose peer connections,
listeners, UDP sockets, name lookups and HTTP all run through one protected
tunnel generation, with IPv6 and unusable destinations refused and no direct
fallback. The engine does not use it yet, so torrent VPN protection is still
unavailable ([operations, generations and limits](/docs/tunnel-network.md)).

## Added: Supply an HTTP connector backed by the native tunnel

The Rust library can run HTTP streams and their DNS through one native tunnel
owner, with bounded commands, deadlines, cancellation and captured generations.
Redirects retain the same connector; there is no direct or proxy fallback in
this adapter. This prerequisite does not activate torrent VPN protection
([API, fixture boundary and limits](/docs/tunnel-http.md)).

## Changed: Accept only literal WireGuard endpoints in the tunnel runtime

The Rust library's tunnel runtime no longer resolves endpoint hostnames through
the host resolver, which had no deadline or cancellation. It accepts only a
usable literal IPv4 endpoint, matching the supported profile subset. Torrent VPN
protection remains unavailable ([runtime boundary](/docs/tunnel.md#ownership)).

## Added: Supply an explicit HTTP transport connector

The Rust library's HTTP client can use a caller-supplied stream for every HTTP
or authenticated HTTPS hop, including redirects, without host resolution,
environment proxies or direct-connection fallback in that path. Connector
implementations own deadlines, destination policy and revocation. This is a
transport prerequisite; torrent VPN protection remains unavailable
([API contract and limits](/docs/http-connector.md)).

## Added: Read WireGuard profiles from private files

The Rust library can read a bounded WireGuard profile from a private regular
file, rejecting links, unsafe permissions and observed file changes while
keeping errors free of paths and credentials. This read-only prerequisite does
not activate torrent VPN protection or save configuration ([API and limits](/docs/wireguard-profiles.md#read-a-private-file)).

## Added: Validate WireGuard profiles offline

The Rust library can now parse a bounded subset of WireGuard `.conf` profiles
and construct tunnel runtime settings, rejecting hooks, unsupported routing,
hostname endpoints and malformed keys with redacted errors. This is preparation
for private profile import: there is no CLI/API activation or torrent VPN
protection yet ([scope and supported settings](/docs/wireguard-profiles.md)).

## Added: Connect any JSON indexer with a mapping

A `json` source can now carry a `mapping` that tells Mynou where results, titles, download addresses and seeders are in the indexer's response, which parameter names to send (or leave out), the category to use for movies and episodes, and fixed parameters such as a page size. Indexers that offer Torznab, RSS or a JSON search API can be connected from `mynou.json` alone, without provider-specific code. Sources without a `mapping` behave as before, and unsupported cases (HTML pages, several result pages, header API keys) are listed in [the guide](/docs/sources.md#connecting-any-indexer).

## Changed: Track any number of series

Series, episodes and numbering are now stored in an indexed on-disk store instead of one in-memory snapshot, so the 128-series, 20,000-episode and 8 MiB limits are gone; each plan still holds at most 2,000 episodes. The first start with write access migrates `series.json` after validating it and keeps an exact copy as `series.legacy-*.json`; older versions refuse the migrated store, so downgrading means restoring that copy ([storage notes](/docs/inventory-storage.md#downgrade-and-recovery)). `mynou series`, `GET /api/series` and the web interface read the list in pages (`--limit`/`--after`, load more), and the calendar adds an `after` cursor ([guide](/docs/series.md)).

## Added: Manually cross-seed verified existing downloads

Use `cross-seed SOURCE_ID --torrent FILE` to seed a distinct v1 torrent from completed native files after full verification, without copying or downloading them. Enable seeding first; optional file mapping and the initial format limits are explained in [the transfer guide](/docs/transfers.md#manual-cross-seeding).

## Changed: Store jobs and history on disk without a total job limit

Jobs and library listings now use cursor pages (`--limit`/`--after`, API `next`, and load more in the web interface). The first writable start migrates and preserves the old job files; history keeps up to 1,000 events per job. Earlier binaries refuse the new format, and restoring legacy backups loses changes made afterward; read [the storage guide](/docs/inventory-storage.md#job-storage) before downgrading.

## Changed: Store requester demand without a total inventory limit

Requester demands and operator interests now use an indexed on-disk ledger instead of the 10,000-record, 16 MiB aggregate snapshot. Use `mynou requester ACCOUNT --limit 200 --after CURSOR` or the account API’s `next` cursor to read further pages; polling batches remain bounded. The first writable start preserves legacy bytes before migration; older binaries refuse the new ledger and restoring the backup loses later changes ([storage guide](/docs/inventory-storage.md#requester-storage)).

## Changed: Schedule library checks without scanning the inventory

Library checks now read a bounded oldest-check window and report inventory counts and window truncation. The first writable start resumes an indexed scheduling upgrade after interruption; earlier binaries reject its new format, so keep a stopped-store backup before upgrading if downgrade may be needed ([storage guide](/docs/inventory-storage.md#indexed-library-monitoring)).

## Changed: Retain feed state without a total source limit

Feed history now uses an on-disk index instead of the 1,000-source, 4 MiB snapshot. Use `mynou feeds --retained --limit 200 --after CURSOR` to page through retained sources; configured watching remains limited to 64 sources. The first writable start preserves the old snapshot; downgrading requires restoring that copy and loses later observations ([storage guide](/docs/inventory-storage.md#feed-storage)).

## Changed: Faster library updates for repeatedly upgraded media

Job storage gains an index of ready upgrade children, so recording a library version no longer reads every earlier upgrade attempt of its parent. The first writable start upgrades the job index in resumable pages; earlier binaries refuse it, so keep a stopped-store backup before downgrading ([storage guide](/docs/inventory-storage.md#job-storage)).

## Changed: Resume feed routing across the complete inventory

Watched announcements are now captured privately before their identities are marked seen, and routing resumes in bounded pages after restart. Pending routing delays the next source poll; pausing a source cancels its captured work. Existing feed indexes upgrade on the first writable start; keep a complete stopped-store backup for downgrade ([storage guide](/docs/inventory-storage.md#captured-feed-routing)).

## Changed: Retained indexer source policies

Removed the fixed limits on retained source policies, including removed sources.
Existing policy migrates automatically on writable startup; see
[storage and downgrade](/docs/inventory-storage.md#source-policy-storage).


## Changed: Retain IRC announcements without a total history limit

IRC history now lives in an on-disk index, removing the 1,000-identity / 8 MiB limits while retaining duplicate suppression across restart. Use `mynou announcements --limit 200 --after CURSOR` or the API’s `next_cursor` to page through the history. The first writable start preserves and migrates the old snapshot; older binaries reject the new marker, so downgrade requires a consistent stopped-store backup ([storage guide](/docs/inventory-storage.md#irc-announcement-storage)).

## Changed: Read Plex watchlists and libraries of any size

Watchlists are now read in resumable sweeps of 200-entry pages, up to eight pages per account in each pass, so the 512-entry requester watchlist limit and the 100,000-item and 1,000-page Plex limits are gone. A sweep resumes after errors and restarts, and entries that left a requester watchlist are removed only after a sweep that saw the whole list unchanged; `GET /api/requesters` shows the progress of an unfinished sweep ([guide](/docs/requesters.md#how-demand-is-handled)).

## Changed: Page search previews without a candidate limit

Search previews no longer stop at 1,000 rows or fail beyond 16 MiB of candidates: candidates are assessed as each source answers, and the winner is still chosen from all of them. `mynou search` and `POST /api/search` return pages of 100 candidates (`--limit` up to 200) with a `next` cursor for `--after`, replacing `truncated`; the web interface loads more on demand ([guide](/docs/selection.md#preview-a-search)).

Exactly tied candidates now use their identity digest as the deterministic tie-breaker, which can select a different release than the former title/URL ordering.

## Fixed: Confirm large requester watchlists before removing demand

Watchlists spanning several polling passes now need two complete sweeps with matching identity digests before absent entries are removed, protecting demand from reorders that keep page anchors and totals unchanged. Confirmation and single-account progress before the first job survive restarts; large-list removals take longer ([guide](/docs/requesters.md#how-demand-is-handled)).

## Fixed: IRC routing no longer stalls behind failing announcements

Up to 200 announcements that kept failing to route (for example with no waiting job) could pause IRC routing for everyone until their 30-second backoff ended. Eligible announcements are now routed regardless, and `GET /api/irc` reports how many backoffs are held, the limit and how many were shortened under `routing` ([limits](/docs/limits.md)).

## Changed: Keep retained transfers on disk and read them in pages

Native transfer startup and scheduling use an on-disk index and bounded working
windows. `mynou torrents --limit 50 --after CURSOR` and `GET /api/transfers`
return `transfers`, `next` and `total` instead of a complete array; the web
interface loads more on demand. The first start preserves and migrates legacy
state; older binaries reject the indexed format. Read the [transfer guide](/docs/transfers.md#storage-migration-and-downgrade)
before downgrading.

## Changed: Much faster storage writes on macOS

Each save of jobs, requester demand, feeds, transfers, series or IRC state now writes its records into one file instead of one file per record. On macOS, where every durable write flushes the whole drive cache, a save of 100 records takes about 40 ms instead of 470 ms. Existing data is read in place without migration, but earlier versions refuse a store once Mynou has saved to it: keep a stopped backup before upgrading if you may need to downgrade ([storage guide](/docs/inventory-storage.md#value-log)).

## Added: Storage sizes and clearer storage errors

`mynou status` and `GET /api/status` now report each store's entries, index and value-log bytes and whether a failed write awaits recovery, without reading the inventory; the status still answers while a store is busy or failing, with that store's counts left empty. Storage errors name the store and say whether the change was saved and what to do, for example freeing disk space and restarting Mynou; existing records are never discarded ([storage guide](/docs/inventory-storage.md#storage-diagnostics-and-errors)).

## Fixed: Plex requests cut off by the time budget no longer report a failed poll

A Plex request cut off by an account's time budget now reports `Plex account poll deadline reached` and keeps the account due, as when the budget ran out between requests. Catalog (TMDB) lookups cut off by the budget still report `Plex account poll failed`.

## Fixed: Resume bounded scheduling after restart

Job claims and routine requester cancellation resume their scan position after restart, and series monitoring visits at most 400 due scopes per pass, including unreadable scopes. Storage status includes counters for the latest claim, requester sweep and series due pass.
Transfer ownership, pack mapping and media state checks no longer read every earlier attempt of a transfer, file or media.
The first writable start upgrades the job index in resumable pages and earlier binaries refuse it; keep a stopped-store backup before upgrading if you may need to downgrade ([storage guide](/docs/inventory-storage.md#job-storage)).

## Changed: Serve the API while startup validation runs

`mynou serve` now answers reads immediately and validates requester and IRC storage in bounded steps afterwards. Until it finishes, other requests get 503, workers and transfers wait, `/readyz` answers `starting` and `startup` in `/api/status` shows progress; an inconsistency is reported there as `failed` instead of stopping the process ([storage guide](/docs/inventory-storage.md#startup-validation)).

## Added: Search every job and library entry

The jobs search box and state buttons in the web interface now search the whole inventory on the server instead of only the loaded rows; "Search further" continues past the first matches. Use `mynou jobs --query TEXT --state STATE` (also `mynou library`) or `q` and `state` on `GET /api/jobs` and `GET /api/library`. Each request examines at most 1,000 entries and its `next` cursor continues the search; a cursor that names no job of this store is now refused ([guide](/docs/inventory-storage.md#searching-jobs-and-the-library)).

## Added: Follow storage migration progress

Storage migrations and index upgrades now report their stage and records done out of the known total, and whether they resumed. Commands print it on standard error; while `mynou serve` opens or migrates storage, `mynou status` and `GET /api/status` already answer with this progress and other routes report that the service is starting ([guide](/docs/inventory-storage.md#migration-progress)).

## Changed: Simpler native development

Routine validation uses installed Rust and Python directly. Removed container-based
repository checks, custom source-history/log audits and local agent delegation
hooks. CodeQL remains the required code security analysis in CI.
The isolated independent WireGuard peer remains an optional manually run fixture;
product dependencies, runtime and release version are unchanged.

## Fixed: Keep disk-full diagnostics during storage migration

When a storage migration runs out of disk space, the error now names the affected
store and asks you to free space and restart. Legacy data remains preserved so
the migration can resume after the storage problem is fixed.

## Changed: Prepare exact bounded inventory backlog counts

The storage index can now count a key range exactly and find its oldest key without scanning the range or reading record values. Series and library status use it for backlog diagnostics; other stores remain pending. New writes use counted branch pages that earlier binaries reject, so keep a complete stopped-store backup for downgrade ([storage guide](/docs/inventory-storage.md#exact-range-summaries)).

## Added: Series backlog and pass freshness

Service status now reports the exact number of due monitored series, the oldest
scheduled check's lateness and the persisted time of the last successful bounded
background pass under `inventory.series`. Busy or unavailable readings remain
explicit. Status does bounded index reads without loading series or episode
records. Writable startup prepares older series indexes in resumable batches;
read-only commands never migrate them. This delivers the series portion of #246;
library monitoring is delivered below and the remaining integrations are tracked in #246.

## Added: Library backlog and pass freshness

Service status now reports the exact automatic library-check backlog, the oldest
due check's lateness and the durable completion time of the last successful
bounded background pass under `inventory.library`. Disabled monitoring, busy
storage and unavailable readings are explicit. Manual previews and forced checks,
failed searches and interrupted passes do not advance freshness. Status reads
bounded index summaries without loading jobs. Writable startup prepares missing
legacy job-index cardinalities in resumable batches; keep a stopped-store backup
before upgrading if downgrade may be needed ([storage guide](/docs/inventory-storage.md#library-backlog-and-pass-freshness)).
This delivers #273, the library portion of #246; the remaining integrations are tracked in #246.


## Added: Job backlog and claim-scan freshness

Service status now reports exact scheduled job backlog, oldest wake lateness and
persisted bounded claim-scan freshness under `inventory.jobs`. It includes work
not yet promoted and excludes active leases and future retries. Freshness records
successful scheduler scans, before job execution; idle scans checkpoint at the
existing 60-second cadence without writing on every poll. Status reads bounded
index summaries without job/history payloads. Writable startup adds the wake
index in resumable pages; older binaries reject the new job format, so keep a
complete stopped-store backup for downgrade ([storage guide](/docs/inventory-storage.md#job-backlog-and-claim-scan-freshness)).
This delivers #275, the jobs/history portion of #246; five integrations remain.


## Added: Complete inventory backlog and pass diagnostics

Service status now includes requester admission, IRC routing, feed scheduling,
source-policy non-applicability and native transfer queue diagnostics alongside
jobs, series and library monitoring. Counts use bounded index reads or the
bounded configured-source schedule; successful-pass checkpoints survive restart.
Unknown wait times in migrated requester/transfer queues remain explicit, and
busy/unavailable stores do not publish guessed counts. Writable startup prepares
derived indexes in resumable pages; keep a complete stopped-store backup before
upgrading for downgrade ([storage guide](/docs/inventory-storage.md#requester-irc-feed-source-policy-and-transfer-diagnostics)).
This completes #246; the increasing-size evidence in #217 is still required for #106.

## Added: Reproducible inventory growth evidence

Add an opt-in macOS arm64 measurement workflow for jobs/history, library tips,
series, requester demands, retained feeds, source policies, IRC history and
native transfers. It records disk use, storage operation times and separate
construction/read peak RSS on synthetic inventories; operation bounds and
physical resource limits still apply. See [the evidence record](../inventory-growth.md).

## Changed: Use the torrent network for source metadata and magnet names

Remote torrent metadata now uses the same network context as peer discovery,
including HTTP redirects and HTTPS. Magnet peer names are kept without DNS at
admission and restart, then resolved by the active transfer or inspection
context; lookup failures are reported during that operation. Peer authorities
require an explicit nonzero port and an ASCII hostname, IPv4 or bracketed IPv6
address. Native VPN controls use this transport preparation; complete
isolation/race/resource evidence is still required before migration.

## Added: Explicit torrent engine transport and revocation fences

Library callers can construct an engine with one supplied torrent network for
transfers and metadata-only operations, including management without download
workers. Tunnel generation loss or cancellation rejects late metadata and peer
results before publication. CI refuses direct networking outside Direct, even
if added to the inventory. Native tunnel fixtures exercise BitTorrent metadata,
payload/seeding, HTTP/HTTPS/UDP trackers, DHT and remote metadata redirects.
This completes the transport boundary of #221 used by the operator VPN controls
below (#222). Full isolation/race/resource evidence (#224) is still required
before the migration guide is available. See [the transport contract](../torrent-transport.md).

## Added: Private WireGuard profiles and operator lifecycle

Import a private WireGuard profile or equivalent structured settings with
`mynou vpn` and the authenticated API. Intent survives restart; startup waits
for the protected tunnel, and profile changes, revocation and recovery retire
the old engine before replacement. Explicit disable is required for Direct;
failed protection never falls back automatically. Redacted status includes
handshake age and counters. Verified bytes and transfer controls are retained.
Complete isolation/race/resource evidence and the Gluetun migration guide remain
issue #224; existing Gluetun is untouched. See the [operator reference](../wireguard-operator.md).

## Added: Observe attempted host networking in native tunnel tests

Native macOS CI observes attempted ordinary socket and host-resolver calls in
selected WireGuard torrent fixtures, with negative controls for direct bypasses
and failed calls. Only declared fixture outer UDP endpoints are accepted.
This supplies a bounded evidence increment for #224; complete isolation, race
and resource gates and the migration guide remain pending. Product networking
and installed services are unchanged ([mechanism and limits](../host-io-observer.md)).

## Changed: Observe WireGuard races and recovery on native macOS

The required macOS arm64 host-I/O checks now observe nineteen synthetic scenarios,
including concurrent publication revocation, no-worker authentication failure,
restart, retained download state and rejection of delayed old-generation work.
This adds bounded evidence for #224; complete isolation, resource measurements
and the migration guide remain pending. Existing Gluetun deployments are untouched.

## Changed: Observe shared-pack metadata inspection through the native tunnel

Required macOS host-I/O checks now also cover explicit shared-file pack previews
and guarded application with download workers disabled, plus cancellation during
a pending metadata response. Only declared fixture outer UDP is accepted; late
responses and retries cannot publish owners after cancellation. This adds bounded
evidence for #224, which remains open for automatic pack search, group replacement,
IRC, remaining races, resources and the gated migration guide. See the
[observation scope](../host-io-observer.md).

## Changed: Observe library group replacement through the native tunnel

Required macOS host-I/O checks now also cover shared-group quality replacement:
preview and guarded application fetch replacement metadata through the tunnel with
download workers disabled, and cancellation during a pending metadata response
refuses the late result and retries without creating owners or touching the
original import. Only declared fixture outer UDP is accepted. This adds bounded
evidence for #224, which remains open for automatic pack search, IRC, remaining
races, engine-level load and the gated migration guide. See the
[observation scope](../host-io-observer.md).
