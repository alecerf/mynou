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
    "max_peers": 4
  }
}
```

`max_active` (1 to 64, default 2) limits active transfers. `max_peers` (1 to 8,
default 4) limits outgoing peer connections per transfer; set it to 1 for a
single-peer baseline. Restart the service after a change. The peer count is a
ceiling, not a promise: a transfer needs reachable peers that have the pieces it
needs, and global worker and memory budgets can lower it further.

Each transfer has one coordinator that gives every in-flight piece to a single
peer, verifies returned data before writing it, and hands failed or corrupt
pieces to another peer. Corrupt peers are set aside for the rest of that
download. Readiness still requires final v1, v2 or hybrid verification. Known
peers start while discovery continues. A magnet's metadata must be
authenticated before payload transfer starts and before its private flag is
known.

## Inspect, pause and prioritize

```sh
mynou torrents --config ./mynou.json
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

The browser's **Transfers** page offers the same controls.

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
if the transfer is paused. In the browser, transfer details provide **Include
file** and **Download all files**.

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
