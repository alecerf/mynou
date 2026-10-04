# Native transfers and controls

Mynou 0.9.0 adds controls for its native BitTorrent engine: durable user
pause/resume, queue priority, per-file piece priority, payload bandwidth limits,
and persistent accounting with ratio/time seeding policies. These operations
retain downloads and library imports. They do not invoke another torrent
client or add dependencies.

Mynou 0.10.0 adds bounded parallel TCP peer transfers. The same controls,
verification and shared-transfer behavior apply across the cooperating peers.

## Parallel peer transfers

Configure peer concurrency in the existing `downloads` object:

```json
{
  "downloads": {
    "max_active": 2,
    "max_peers": 4
  }
}
```

Merge these fields with the rest of your configuration. `max_active` limits
active transfers. `max_peers` defaults to `4`, including when absent from an
older configuration, and accepts integers `1` through `8`. Values of zero,
fractions, booleans and numeric strings are rejected. Restart after changes.
Set `max_peers` to `1` for a single-peer baseline.

The peer setting is a ceiling, not a promised connection count. A transfer
needs known, reachable peers that advertise needed pieces. Global worker and
per-transfer resource bounds can further reduce concurrency. It does not add
new peer transports, guarantee faster public downloads or bypass bandwidth caps.

The connection allocation is `min(max_peers, max(1, 64 / max_active))`, using
integer division. With configuration's maximum of 64 active transfers, this
bounds outgoing payload workers to 64 across all active transfers. Direct Rust
library callers can select up to 128 active transfers; their minimum one-peer
allocation permits up to 128 workers. Incoming seeding handlers have a separate
32-connection bound.

Concurrency is also reduced using a 128 MiB per-transfer estimate for worker
metadata clones, one piece buffer per worker and a protocol buffer allowance.
At least one worker remains possible for a large torrent. This estimate is not
a hard process RAM limit: canonical metadata, parsers, allocator overhead,
journal data and operating-system socket buffers also consume memory. A worker
receives no new piece until the coordinator accepts its previous result.

Known peers connect alongside discovery after metadata authentication. New
parallel TCP connections have a one-second attempt timeout; standard-library
connects can observe cancellation only after that attempt returns. Established
socket operations poll cancellation every 100 ms. Idle peers rotate after two
seconds, and corrupt peers are quarantined for the current download generation.
Tracker jobs rotate fairly, and shared tracker/seeding-DHT capacity alternates
between task kinds when both are eligible.

The coordinator gives each in-flight piece one owner. It verifies returned data
before writing and publishing the piece; disconnects and invalid pieces return
work for another peer. No duplicate endgame requests are issued. Final v1/v2 or
hybrid verification still decides readiness. A file-priority change affects
subsequent piece selection without taking a piece away from its active owner.
All files remain required.

Discovery proceeds alongside usable known peers rather than postponing them
until every lookup finishes. Discovery retains bounded peer results and
private-torrent restrictions. A magnet still needs authenticated metadata before
parallel payload work and before its private flag can be known. The privacy
limits in [protocol support](limits.md#bittorrent) continue to apply.

## Inspect transfers

Transfer commands require the running `serve` service:

```sh
mynou torrents --config ./mynou.json
mynou torrent ID --config ./mynou.json
```

Use the native transfer ID from this list, not a request/job ID. Native IDs are
40 or 64 hexadecimal characters; request IDs are a different identity. Requests
can share one transfer, so a transfer-level control affects every request that
references it. These commands do not start a second torrent service.

The authenticated API equivalents are `GET /api/transfers` and
`GET /api/transfers/ID`.

## Pause and resume

```sh
mynou pause ID --config ./mynou.json
mynou resume ID --config ./mynou.json
```

The authenticated API accepts `POST /api/transfers/ID/pause` and
`POST /api/transfers/ID/resume`, each with an empty JSON object `{}`.

A user pause is durable and distinct from an internal retryable interruption.
Restart, request retries and repeated acquisition checks do not undo it. Resume
is an explicit user operation. Pausing the shared transfer affects all requests
using it; canceling one request does not imply permission to delete its shared
download or undo a user pause.

Parallel verified writes share the control mutex with pause. A pause waits for
the current disk write, then prevents the retired generation from committing
more pieces. The coordinator cancels and joins its workers before exiting or
publishing readiness. Blocking filesystem operations depend on the operating
system and have no strict wall-clock interruption guarantee.

Pause/resume retains downloaded pieces, source files and imports. Resuming still
requires normal verification and does not make unverified bytes ready. Pausing
a ready transfer controls its torrent activity, not availability of an already
imported file in Plex.

## Queue priority

```sh
mynou torrent-priority ID --priority 100 --config ./mynou.json
```

The authenticated equivalent is `POST /api/transfers/ID/priority`:

```json
{"priority": 100}
```

Priority is an integer from `-1000` through `1000`. Higher-priority waiting
transfers start first. Equal priorities use first-in, first-out order. Queue
selection is nonpreemptive: raising a waiting transfer's priority does not evict
another already active transfer. Concurrency remains bounded by the configured
download capacity. Priority and user pause state survive restart.

## Per-file priority

```sh
mynou file-priority ID --file 0 --priority high --config ./mynou.json
```

The authenticated equivalent is `POST /api/transfers/ID/files`:

```json
{"index": 0, "priority": "high"}
```

Use the `index` returned for that file by the transfer's listing. This is the
original zero-based index in the torrent metadata, not the row number in the
visible list. Padding entries are filtered out, so listed indices can have gaps.
Indices must be below 100,000 and identify a real, non-padding file; padding
indices are rejected. The allowed priorities are `low`, `normal` and `high`.
They influence piece ordering within a transfer. A v1 piece overlapping several
files uses the highest priority of its intersecting non-padding files.

**Every file is still downloaded and required for completion.** There is no
`skip` priority, selective season-pack acquisition or extra library file
selection in this release. Piece verification remains mandatory, including
pieces crossing file boundaries.

## Configure global rates and default seeding policy

Policy fields live directly in the existing `downloads` object in
`mynou.json`; there is no additional policy nesting:

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

Merge these fields with your existing download configuration. These are the
defaults, including when an older configuration omits them:

| Field | Default | Accepted values |
| --- | --- | --- |
| `download_limit_bps` | `0` | Integer 0–1,073,741,824 content bytes per second |
| `upload_limit_bps` | `0` | Integer 0–1,073,741,824 content bytes per second |
| `seed_ratio_milli` | `null` | `null` or integer 1–1,000,000; `1000` represents ratio 1.0 |
| `seed_time_secs` | `null` | `null` or integer 1–315,360,000 seconds |

Rate `0` means unlimited. Seeding `null` means no limit of that kind. Numeric
strings, booleans, negative numbers and fractions are rejected. Restart the
service after changing configuration.

Download and upload caps apply across transfers to content payload bytes.
Protocol messages, tracker traffic and other network overhead are outside these
counters and caps, so they are not a cap on all interface traffic. Additional
per-transfer caps cannot bypass the global bandwidth cap. Each aggregate rate
bucket permits a bounded 16 KiB burst; the configured rate is not an
instantaneous, burst-free limit. Download rate gating occurs before issuing
payload block requests.

Configured ratio/time fields provide the default seeding policy for transfers
without a local override. Unlike global bandwidth caps, these seeding defaults
can be replaced by a complete per-transfer policy.

## Override a transfer policy

```sh
mynou torrent-policy ID --policy '{"upload_limit_bps": 1048576}' \
  --config ./mynou.json
mynou torrent-policy ID --policy 'null' --config ./mynou.json
```

The authenticated equivalent is `POST /api/transfers/ID/policy`:

```json
{"policy": {"upload_limit_bps": 1048576}}
```

The policy object is a **complete override**, not a field-by-field patch.
Omitted rate fields become `0` and omitted/null seeding fields become `null`.
An override containing only `upload_limit_bps` therefore removes configured
default ratio/time caps for that transfer. Include the desired seeding fields
explicitly when you want to retain them.

Global download/upload rates remain enforced, plus any local rate limits. Local
ratio/time fields replace the configured default seeding caps. `{"policy": null}`
clears the whole override and restores those configured defaults. The override
uses the same bounded field values. Policy changes retain imported files and
downloaded sources.

## Accounting and retention

Payload counters and seeding elapsed time are persisted across clean restart.
Updates are coalesced on a one-second interval and flushed on clean shutdown.
An abrupt crash can lose the latest unflushed increments; the counters do not
claim exact crash durability. They also do not measure protocol overhead or
establish public-tracker accounting equivalence.

The ratio denominator is the completed verified torrent's non-padding payload
size, not the downloaded-byte counter. For example, ratio `1000` permits an
upload budget equal to that payload size even when restart recovery found all
its pieces already on disk. The integer budget is rounded upward from
`payload_size * seed_ratio_milli / 1000`. Padding is excluded.

Upload reservations bound concurrently served blocks. The engine sends whole
protocol blocks and stops before accepting a block that would exceed the
remaining ratio budget. It can therefore stop below the nominal ratio when the
remaining allowance is smaller than a peer's requested block.

Seeding elapsed time counts online availability while a transfer is verified,
ready, seeding-enabled, unpaused and not limited by its seeding policy. Idle
availability counts; it is not summed per-peer transmission time. Offline time,
user/internal pauses and time after a seeding limit is reached do not count.

Earlier transfers without accounting records start their persisted download,
upload and elapsed totals at zero; past activity is not reconstructed or
invented. Their migrated queue ordering is deterministic and then persisted.

Ratio/time policies control seeding activity while retaining data. Reaching a
limit does not delete a downloaded file, remove an import or revoke Plex
availability. This release adds no automatic cleanup.

uTP/WebTorrent, webseeds, automatic NAT traversal and a complete persistent DHT
table remain outside this release. There is no selective file skipping or
automatic cleanup. Browser controls are described in [web management](web.md).
See [limits](limits.md),
[library monitoring](library.md) and the [roadmap](roadmap.md).
