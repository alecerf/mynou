# One physical video for multiple episodes

Mynou 0.17 explicitly binds one authenticated torrent video to 2–64 consecutive
canonical episodes in one season. Every episode has its own request and Plex
availability check, while all owners retain one physical library destination.
The accepted series catalog must identify every owner and give an already aired
date. This explicit action works while series monitoring is off and can include
episodes excluded from automatic acquisition.

This feature assumes that the video contains all the selected episodes. Torrent
hashes authenticate bytes and paths; they cannot establish the editorial contents
of a video. Review those contents before applying the mapping.

## Preview and acquire

Track the series without automatic acquisition first. Create `shared.json`:

```json
{
  "file_path": "Pack/combined.mp4",
  "season": 1,
  "episodes": [1, 2]
}
```

Include the torrent's top-level directory in the exact case-sensitive path.
Use canonical library numbers from series details, including when a source uses
approved absolute or alternate numbering. Then preview:

```sh
./bin/mynou series-shared-file SERIES_ID \
  --url 'magnet:?xt=urn:btih:TORRENT_HASH' --mapping shared.json
```

Preview authenticates metadata and requires exactly one nonempty, non-padding
video at that path. It requests no payload and records no jobs. Metadata inspection
uses the existing bounded local/HTTP torrent and magnet discovery path: at most
1,024 files, 1 MiB of path data, eight peer attempts and four trackers. The action
has a 60-second deadline; synchronous standard-library DNS may exceed it.

Review the returned `binding`, `group_id`, `new_owners` and `plan_id`. Apply through
a running service using the same source, mapping and reviewed guard:

```sh
./bin/mynou series-shared-file SERIES_ID \
  --url 'magnet:?xt=urn:btih:TORRENT_HASH' --mapping shared.json \
  --apply --plan-id REVIEWED_PLAN_ID
```

An offline preview opens existing storage read-only. Offline apply is rejected.
The CLI mapping file is limited to 512 KiB and contains only `file_path`, `season`
and `episodes`. Unknown fields, duplicate owners and missing/partial apply guards
are rejected before metadata I/O. Sources can include private credentials; public
JSON reports never return the acquisition URL.

## API and browser

`POST /api/series/SERIES_ID/shared-file` requires the usual Bearer token and a
JSON object containing `source_url`, `file_path`, `season`, `episodes`, and optional
`apply`/`plan_id`. Omit `apply` or set it to `false` for a preview. Apply requires
`apply: true` and the exact 64-character lowercase hexadecimal preview ID. The
normal 1 MiB API body bound applies.

Open **One video for multiple episodes** in browser series details, enter the
source/path/owners, then select **Preview shared file**. The confirmation page
displays the authenticated hash, complete owner range and single library path.
**Record reviewed shared ownership** repeats metadata authentication and guards
the decision. It keeps source credentials server-side: one pending review per
signed-in browser session, expiring after ten minutes. A new review replaces the
previous one. The form cannot be transferred to another session or series;
normal CSRF, origin and session checks still apply. Browser fields retain their
8,192-byte decoded bounds and 65,536-byte form limit.

## Physical identity and import

All owners capture the same `shared_file` descriptor: authenticated torrent
identity, exact file path, TMDB series identity, canonical season/range, retained
title/year and destination. The group ID is a 128-bit prefix of the SHA-256
identity over torrent/path/series/range. It is an import namespace, not an
authentication credential; the complete torrent hashes remain mandatory.

The deterministic Plex filename is:

```text
Fixture Series/Season 01/Fixture Series - S01E01-E02 [mynou-GROUP_ID].mp4
```

The name uses canonical numbers and the full owner range, even when one owner
is cancelled. The suffix separates independent physical acquisitions. Titles
are sanitized and truncated at UTF-8 character boundaries; filenames fit the
255-byte component bound. Source extensions are retained.

A single synchronized journal frame records every new owner before a worker can
acquire any bytes. Its creation event belongs to the first canonical owner and
describes the group transaction; subsequent per-owner transitions have ordinary
events. Recovery accepts the whole transaction or none of it. Complete checksum
or semantic corruption fails closed. Claims for one group are serialized while
unrelated groups can progress independently. Ownership lookups use bounded
in-memory media/physical indexes rebuilt from verified persistent jobs.

Workers bind native acquisition to the accepted torrent identity and select only
the exact video, retaining the client's additive interests and boundary-piece
verification. A mutable URL cannot silently replace that identity. The native
transfer must retain the same identity used by the review; an already queued
hybrid under another alias is rejected rather than silently rebinding ownership.

Each import uses one recorded destination. Existing bytes must compare equal;
different files and symlinks are rejected. The organizer copies a shared source
once into a private temporary file, then publishes the library name atomically
without overwriting. The native payload retains an independent inode and its
existing hard-link protection. Later owners compare and reuse that same copy;
no media is deleted. Recovery after publication but before the import journal
write reuses the verified file.
Changing the configured library root does not reassign an unimported group's
destination; plan a separate explicit migration. Confirmed paths remain fixed.

Plex must report that exact destination, after configured path-prefix mappings,
for each canonical owner separately. An older playable episode at another path
does not complete the owner. One owner can be ready while another waits for Plex.
With Plex disabled, verified import completes the request. Missing or unsafe
recorded imports are rejected when a pending owner resumes.

## Cancellation, retry and later subsets

Cancelling an owner clears its processing lease and retains the full binding.
It does not change another owner's transfer interest or delete the imported file.
A queued owner without a native download ID still retains its known physical
interest across cancellation and restart. If every interest is cancelled, normal
native cancellation pauses the transfer while retaining bytes.

Retry uses the same recorded source, binding and import path. Individual
`pack-remap` actions are blocked for shared owners, including failed or cancelled
owners with no import. A later review for the same authenticated path may request
any nonempty subset of the recorded owners and reuses their existing jobs.
It does not reset cancellation, retry exhaustion or earlier source labels.
Creating a new group requires the complete consecutive range; extending,
reassigning or partially replacing an existing group is rejected. A new shared
group cannot adopt an episode that already has an unrelated request or ordinary
mapped ownership.

## Persistence and upgrades

The reader supports earlier journal/snapshot format 1 without migration writes.
Shared transactions use `MYNOUJ02`; a snapshot containing shared owners uses
`MYNOUS02`. Earlier binaries reject these formats instead of ignoring owners.
Keep complete request/series state, downloads and library in backups, and use
Mynou 0.17 or later after creating shared ownership. Existing ordinary jobs retain
their single-file import behavior and keys.

Individual upgrade and baseline actions are blocked for shared owners. Upgrade
reports show `shared_group_upgrade_required`; a separately submitted acquisition
cannot replace a shared owner. Earlier imports remain current. Coordinated group
replacement, including readiness of every required replacement, is the next
release prerequisite before automatic multi-episode mapping can be enabled.

Automatic packs and the ordinary explicit pack validator retain one file per
episode. This release adds the dedicated shared action; it does not infer range
contents, cut videos, rename an existing library, remove old imports, or contract
native selection. Full stack parity remains on the [roadmap](roadmap.md).
