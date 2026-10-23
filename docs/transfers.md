# Torrent transfers

Mynou downloads and seeds with its own BitTorrent engine. This guide covers
peer concurrency, transfer controls, selective downloads for packs, bandwidth
limits and seeding policies. None of these controls deletes downloads or
library imports. Protocol support and bounds are listed in
[limits](limits.md#bittorrent).

Several requests can share one transfer (the same torrent). Transfer controls
use the native transfer ID, 40 or 64 hexadecimal characters, which differs from
request (job) IDs, and they affect every request using that transfer. Transfer
commands need the running service.

## Peers and concurrency

```json
{
  "downloads": {
    "max_active": 2,
    "max_seeding": 0,
    "max_peers": 4
  }
}
```

`max_active` (default 2) controls the download queue and `max_seeding`
(default 0) controls the seed queue independently. For either setting, `0`
means no user-configured queue limit. A positive value admits that many
transfers in descending priority, then arrival order. Seeds waiting for a
slot keep their files and appear as `queued_seed` / **Waiting to seed**;
this is different from a user pause or reaching a ratio/time policy. Idle
eligible seeds count towards a positive seed limit. Pausing or reaching a
seeding policy releases a slot. A queued seed neither serves payload nor
consumes its seeding-time allowance.

For example, `max_active: 2, max_seeding: 8` queues downloads and seeds
separately, and `max_active: 0, max_seeding: 0` removes both user queue
limits. Restart the service after changing these settings. A zero setting
does not create one thread or connection per retained torrent: the existing
64 outbound execution-slot resource budget, 32 inbound peer connections,
memory checks and native tunnel flow bounds still apply. Additional work
waits in the download queue. API transfer snapshots expose the configured
queue limits and execution slots so these resource bounds remain visible.

`max_peers` (1 to 8, default 4) limits outgoing peer connections per transfer;
set it to 1 for a single-peer baseline. Reachable peers and the worker/memory
budgets determine the actual connections and throughput.

Retained transfers live in an on-disk index. Startup and routine scheduling read
bounded windows rather than enumerating the entire transfer directory; inactive
transfers remain available through point lookups and cursor pages. Retention has
no fixed transfer-count limit. Storage and the configured worker limits still
bound what can run at once.

Large seeding inventories rotate through bounded resident windows, with a
rotation requested every 30 seconds; existing operations must drain before
eviction. Retained seeds are rehashed before joining an active window. A
finished transfer can also be reverified on demand by a point status check,
without rechecking the entire retained inventory. If all resident slots are
busy, an operation reports that it should be retried.

Each transfer has one coordinator that gives every in-flight piece to a single
peer, verifies returned data before writing it, and hands failed or corrupt
pieces to another peer. Corrupt peers are set aside for the rest of that
download. Readiness still requires final v1, v2 or hybrid verification. Known
peers start while discovery continues. A magnet's metadata must be
authenticated before payload transfer starts and before its private flag is
known.

## Inspect, pause and prioritize

```sh
mynou torrents --limit 50 --config ./mynou.json
mynou torrents --limit 50 --after CURSOR --config ./mynou.json
mynou torrent ID --config ./mynou.json
mynou pause ID --config ./mynou.json
mynou resume ID --config ./mynou.json
mynou torrent-priority ID --priority 100 --config ./mynou.json
mynou file-priority ID --file 0 --priority high --config ./mynou.json
```

| CLI | API (Bearer token) |
| --- | --- |
| `torrents`, `torrent ID` | `GET /api/transfers`, `GET /api/transfers/ID` |
| `pause ID`, `resume ID` | `POST /api/transfers/ID/pause`, `POST /api/transfers/ID/resume` with `{}` |
| `torrent-priority ID --priority N` | `POST /api/transfers/ID/priority` with `{"priority": 100}` |
| `file-priority ID --file N --priority P` | `POST /api/transfers/ID/files` with `{"index": 0, "priority": "high"}` |
| `torrent-select ID --selection JSON` | `POST /api/transfers/ID/selection` |
| `torrent-policy ID --policy JSON` | `POST /api/transfers/ID/policy` |

`torrents` and `GET /api/transfers` return an object with `transfers`, `next`,
`total` and `sequence`, replacing the former complete array. A page has 50
transfers by default; `limit` accepts 1 to 200. Continue with `--after` or
`GET /api/transfers?limit=50&after=CURSOR` until `next` is null. Transfers are
ordered by priority, then FIFO. The web interface offers **Load more transfers**.
Point controls work for retained transfers outside the displayed page. Request
references have their own independent cursor, described below.
`sequence` changes when membership or priority/FIFO ordering changes; restart
from the first page if it changes during a scan. Progress and control updates
change `revision` without invalidating that ordering. `queue_position` is the
zero-based position within the displayed page, not a global queue rank; a point
response uses zero.

- **Pause** is durable: restarts, request retries and repeated checks never undo
  it; only `resume` does. A pause waits for the current disk write. Pausing a
  ready transfer stops torrent activity, not Plex access to imported files.
  Cancelling one request never deletes a shared download or lifts a pause.
- **Queue priority** is an integer from -1000 to 1000. Higher priorities start
  first and equal priorities keep first-in, first-out order. Raising a waiting
  transfer never stops an active one.
- **File priority** is `low`, `normal` or `high` and only orders the pieces that
  are needed; there is no `skip`. Use the `index` from the transfer listing: it
  is the original metadata index, so padding files leave gaps. A v1 piece shared
  by several files takes the highest of their priorities.

## Selective acquisition

Ordinary requests download every file. Mapped [season packs](packs.md) record
the exact files they need before payload starts, and requests sharing a
transfer keep the union of their files. Selections only grow: an ordinary
request for the same torrent, **Download all files** or `torrent-select`
expands them, and cancellation never removes interests or bytes.

```sh
mynou torrent-select ID --selection '{"indices":[0,2]}' --config ./mynou.json
mynou torrent-select ID --selection '{"all":true}' --config ./mynou.json
```

Choose exactly one of `indices` (original non-padding metadata indices) or
`all: true`. Expansion keeps the pause, priority and policy; resume explicitly
if the transfer is paused.

`torrent ID` distinguishes the states:

| Field | Meaning |
| --- | --- |
| `file_selection` | `null` means all files; otherwise the retained relative paths |
| `selected_ready` | Every selected file is verified and synchronized |
| `ready` | Every piece of the whole torrent is verified |
| `files[].selected` | The file is part of the selection |
| `files[].verified` | The file is verified and available |
| `progress` | Share of the required pieces that are verified |

A finished partial selection shows `selected_ready`, `progress: 1` and
`ready: false`; its episode imports can proceed. After a restart nothing is
available until the retained bytes are hashed again.

For v1 torrents, Mynou downloads every whole piece that overlaps a selected
file and keeps the bytes those pieces hold for neighboring files. Neighbors
are created at their full size (sparse where the filesystem allows), so a file's
size is not proof that it was downloaded. For v2 and hybrid torrents, selected
files also need their authenticated file roots. A partial transfer does not
seed: it advertises no pieces, uploads nothing, counts no seeding time and never
sends a tracker `completed` event. Once every piece is verified, normal seeding
rules apply.

## Manual cross-seeding

Enable `downloads.seed`, then explicitly supply a v1 torrent and the ID of a
fully verified ordinary native transfer:

```sh
mynou cross-seed SOURCE_ID --torrent ./candidate.torrent --config ./mynou.json
mynou torrent CANDIDATE_ID --config ./mynou.json
```

Every candidate piece must verify from the existing bytes before any announce
or peer advertisement. No media file, copy, link or library import is created.
Default mapping pairs non-padding files in metadata order; names may differ
but every pair must have equal size. For reordered files or a subset of the
completed source, supply original source file indices in candidate non-padding
order, for example `--mapping '[2,0]'`. Inspect source indices with `torrent`.
Missing mappings, duplicate indices, size mismatches and existing swarms fail.

The Bearer-authenticated API is `POST /api/transfers/SOURCE_ID/cross-seed` with
`{"metadata_hex":"...","mapping":[2,0]}`; `mapping` is optional. Send the
hexadecimal torrent bytes, never a server path. The first increment supports
distinct-infohash v1 candidates only, up to 256 KiB metadata and 1024 metadata
files. V2/hybrid candidates and chained reuse are excluded;
[automatic discovery](#automatic-cross-seed-discovery) uses the same path.

Diagnostics include `seed_only: true` and `cross_seed_source_id`. A mismatch
or missing/unsafe file shows `failed` with an error: there is no partial
advertisement or fallback acquisition. Repair the source and explicitly
`resume CANDIDATE_ID` to reverify read-only. Ordinary acquisition and file
selection refuse seed-only identities. Existing pause and policy controls
apply; resume and a seeding-policy change retire the old proof. Pausing the
source does not pause
its cross-seeds: their explicit seeding permission and controls are independent,
and verification never resumes or reacquires the original transfer.

Private candidates use their own trackers, with DHT and PEX excluded. Source
trackers and peers are not inherited. Uploads hash reused candidate pieces
again on every block request, including previously cached pieces; observed
missing, unsafe or mismatching files invalidate candidate readiness. Keep files
stable while seeding: checks occur during verification and upload requests,
not continuously, and already transmitted bytes cannot be recalled.
Source directories and their ancestors must be physical directories; mapped
files must be regular, have no extra hardlinks and exactly match their lengths.
The private cross-seed state directory and its ancestors must also be physical;
its canonical path must remain bound to the configured root.

A private atomic `.seed` record binds metadata/source/mapping independently
of ordinary queue files. Restart always rehashes before seeding. Damaged seed
state, missing source state or seed controls, or conflicting ordinary entries
are refused when migration, scheduling or point lookup reads that identity;
they never convert into a download. The indexed transfer format rejects
older binaries; follow the migration and downgrade procedure below. Keep the
source media and native state while seeding.

## Automatic cross-seed discovery

Discovery finds candidates for completed ordinary transfers and registers them
through manual cross-seeding, so the same verification applies: every piece is
hashed from the existing bytes before any announce, and a mismatch fails the
candidate without any download. It is opt-in per RSS, Torznab or JSON
[source](sources.md) and needs `downloads.seed`:

```json
{
  "id": "tracker-b",
  "kind": "torznab",
  "url": "https://indexer.example.test/api",
  "cross_seed": {
    "enabled": true,
    "feed": true,
    "interval_secs": 3600,
    "search": true,
    "search_interval_secs": null
  }
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `feed` | `true` | Read the newest entries every `interval_secs` (60 to 86,400, default hourly) |
| `search` | `true` for Torznab and JSON | Search by name once each completed transfer is verified (Torznab and JSON only) |
| `search_interval_secs` | `null` | Repeat that search after this delay (3,600 to 31,536,000); `null` searches once |

A JSON source reads its feed as a [watched feed](sources.md#watching-a-feed)
does and searches with only the name as its `query` role, its fixed parameters
and API key. Its `mapping` can name `size` (payload bytes, a number or numeric
text) and `info_hash` (40 hexadecimal digits) so that offers are filtered before
any download, as Torznab `size` and `infohash` attributes are.

A pass runs every minute and handles one source at a time. For each offer:

1. Its title must equal the name of a completed transfer once case,
   separators and a video extension are ignored. A stated size larger than the
   transfer's payload, or an advertised info hash that already exists, is
   skipped without downloading anything.
2. The `.torrent` is downloaded once through the source's authentication and
   [protection](protected-sources.md), at most eight per source and pass, after
   its minimum request spacing.
3. Only v1 metadata within the manual limits is accepted. Every candidate file
   needs a distinct source file of equal length; file names, then paths below
   the root, break ties, and anything still ambiguous is refused. Candidates
   may be a subset of the source and may rename or reorder files.
4. The candidate is registered with that explicit mapping and verified before
   it can be advertised.

A source must be verified, not failed and not paused. Retained seeds that are
waiting for their rehash are retried later. Season packs are compared like any
other transfer; discovery has no pack-specific matching. Private candidates
keep their own trackers, without DHT or PEX, and the protection of the transfer
network applies as for any seed.

Feed entries and completed searches are remembered in a private
`cross-seed.idx` store, together with the provenance of each registered
cross-seed (source transfer, indexer, `feed` or `completion` trigger and time),
counters per result and reason, and the last 100 outcomes. A restart therefore
neither repeats a registration nor downloads a remembered entry again. An entry
without a matching completed transfer is remembered too: a transfer completed
later is found by its completion search. Up to 2,048 entry identities are kept
per source. Deleting the store makes discovery start over; registered
cross-seeds are kept.

```sh
mynou cross-seed-discovery --config ./mynou.json
mynou cross-seed-discovery --run --dry-run --config ./mynou.json
mynou cross-seed-discovery --run --indexer tracker-b --transfer ID --config ./mynou.json
```

| CLI | API (Bearer token) |
| --- | --- |
| `cross-seed-discovery` | `GET /api/cross-seed/discovery` |
| `cross-seed-discovery --run [...]` | `POST /api/cross-seed/discovery/run` with `{"dry_run": true, "force": false, "indexer": "ID", "transfer": "ID"}` (all optional) |

`--dry-run` downloads and maps candidates but registers and remembers
nothing. `--force` ignores the feed schedule and search delays, not the source's
rate limits. The status lists the enabled sources and their schedule, counters,
recent outcomes and registered cross-seeds. Reports name only transfer
identities, source IDs and fixed reasons, never titles, links or credentials:

| Result | Reasons |
| --- | --- |
| `added` | `verification_queued` |
| `would_add` | `verified_layout` (dry run) |
| `skipped` | `already_present`, `size_mismatch`, `source_paused` |
| `rejected` | `layout_mismatch`, `ambiguous_layout`, `unsupported_metadata`, `metainfo_unavailable`, `registration_refused` |

Network failures, rate limits and unverified sources leave the entry pending
for a later pass and count as `deferred`. A pass report also counts unmatched
offers, searches and downloaded metadata.

## Bandwidth and seeding

Global limits and default seeding policy live in the `downloads` object:

```json
{
  "downloads": {
    "download_limit_bps": 0,
    "upload_limit_bps": 0,
    "seed_ratio_milli": null,
    "seed_time_secs": null
  }
}
```

| Field | Default | Accepted values |
| --- | --- | --- |
| `download_limit_bps` | `0` (unlimited) | Integer 0 to 1,073,741,824 payload bytes per second |
| `upload_limit_bps` | `0` (unlimited) | Integer 0 to 1,073,741,824 payload bytes per second |
| `seed_ratio_milli` | `null` (no limit) | `null` or 1 to 1,000,000; `1000` means ratio 1.0 |
| `seed_time_secs` | `null` (no limit) | `null` or 1 to 315,360,000 seconds |

Numeric strings, booleans, negative numbers and fractions are rejected. Restart
after changing them. Rate limits apply to all transfers together and count
torrent payload only: protocol, tracker and other traffic is not included. They
always apply, even under a per-transfer policy.

To override the seeding defaults or add tighter rates for one transfer:

```sh
mynou torrent-policy ID --policy '{"upload_limit_bps": 1048576}' --config ./mynou.json
mynou torrent-policy ID --policy 'null' --config ./mynou.json
```

The API body is `{"policy": {...}}` or `{"policy": null}`. A policy is a
**complete replacement**, not a patch: omitted rates become `0` and omitted
seeding fields become `null`. The example above therefore removes the default
ratio and time limits for that transfer; include them to keep them. `null`
removes the override and restores the configured defaults.

The ratio is measured against the torrent's verified payload size (padding
excluded), not against downloaded bytes, so a torrent found complete on disk
still gets its full upload budget. Mynou sends whole blocks and stops before a
block that would exceed the budget, so seeding can end slightly below the
ratio. Seeding time counts time online while ready, enabled, unpaused and not
yet limited, including idle time. Counters and seeding time survive a clean
restart; transfers created before accounting existed start at zero. Reaching a
limit stops seeding only: files, imports and Plex availability stay.

## Storage migration and downgrade

The first writable startup indexes the existing native state. This one-time
migration reads the legacy directory, preserves its transfer files privately in
`.transfers-legacy`, and records each migrated identity durably. An interrupted
migration resumes on the next start; errors leave the backup and recovery state
in place. Later starts use the index rather than enumerating legacy files.
Indexed identities and their canonical lowercase filenames are authoritative;
manually adding files after migration does not register new transfers. Use the
service's submission or cross-seed controls instead.
Corrupt inactive metadata is therefore reported when that transfer is read or
scheduled, rather than by scanning every retained file at startup. Such errors
never authorize a transfer. Every restart still requires rehashing before
verified availability or seeding is restored.

Successful snapshot updates reclaim the superseded snapshot. A crash before
the index references a newly written snapshot can leave an unreferenced private
file; it does not become an accepted transfer or restore availability.

Keep the complete native state directory together: `transfers.idx`,
`transfer-snapshots`, the individual metadata/source/control/seed files and the
format marker are one store. The marker occupies a reserved all-zero torrent
identity and deliberately makes older binaries refuse the directory. Do not
delete it to force an old binary to open indexed state.

Before upgrading, stop Mynou and back up its configuration, application state,
native transfer state and payload directories together. To downgrade, stop
Mynou and restore that complete matching backup. The migration's
`.transfers-legacy` copies preserve only the native state at migration time;
they do not include later controls, new transfers or the application request
journal. Never combine a journal from one date with transfer state from another.
Restoring an earlier backup loses later changes. Payloads are not deleted by
migration, pause, cancellation or downgrade preparation.

### Paged request references

Transfer responses contain one bounded page of `request_ids` and
`request_ids_next`. Continue with
`GET /api/transfers/TRANSFER_ID?limit=200&after=CURSOR` until the latter is null.
The transfer list starts each reference list at its first page; individual
transfer requests expose the following pages. Transfer controls and ownership
checks still apply to all owners, including those outside the displayed page.
