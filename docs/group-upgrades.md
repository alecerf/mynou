# Coordinated shared-group upgrades

Mynou 0.18 replaces one shared video with another for the complete captured
canonical owner range. The original group stays current while replacements
download, import and confirm their exact destination in Plex. Confirmation
stages an owner; one synchronized transaction promotes every owner together.
Existing library files and native payloads remain in place.

Use an owner ID from `mynou library` or the browser library. The group must contain
all 2–64 recorded consecutive episodes in one canonical season, with every owner
ready and current. A single supplied owner ID identifies that complete immutable
group. There is no caller-selected owner subset for a baseline or replacement.

## Configure quality and record a missing group baseline

Quality decisions use the current episode [selection profile](selection.md).
Configure ordered resolutions, sources, codecs, languages or custom scores to
define improvement. Empty preference lists contribute zero rank: naming a
different resolution alone cannot establish improvement under that policy.
For example, `"resolutions": [2160, 1080, 720, 480]` prefers 1080p over 720p.
A configured cutoff stops replacements once the baseline reaches it.

Shared acquisitions from 0.17 lack release-name baselines. If you know the release
that describes the entire imported video, create `baseline.json`:

```json
{"action": "baseline", "release_title": "Example.Series.S01.720p.WEB-DL"}
```

Preview and apply the same file:

```sh
mynou library-group OWNER_ID --mapping baseline.json --config mynou.json
mynou library-group OWNER_ID --mapping baseline.json --apply \
  --plan-id REVIEWED_PLAN_ID --config mynou.json
```

All owners must retain their one exact regular-file import, with a declared video
stream. The baseline fills every missing release description in one transaction;
it cannot overwrite an existing baseline. A historical baseline can be outside
today's profile restrictions. A release title is an explicit operator claim,
not proof of decoded quality, audio language or external provenance.

## Review a whole-group replacement

Create `replacement.json`:

```json
{
  "action": "replace",
  "release_title": "Example.Series.S01E01-E02.1080p.WEB-DL",
  "source_url": "magnet:?xt=urn:btih:REPLACEMENT_TORRENT_HASH",
  "file_path": "Example/shared.mp4"
}
```

Use the same preview/apply commands with this mapping. Replacement requires every
current owner to remain monitored and a consistent whole-group baseline. The new
release must pass the current profile and improve its rank, or pass a policy
that now rejects the old baseline. A reached cutoff blocks replacement.

The release title must begin with the retained series title and an optional
matching year. It must describe the complete captured source range or its
season. Supported range labels include `S01E01-E02`, `S01E01E02` and
`S01E01-S01E02`. Consecutive explicit absolute sources use a full numeric range,
such as `13-14`. Mixed or nonconsecutive source labels are unresolved and reject
the operation. New jobs retain each parent's source numbering; later catalog
or numbering changes do not rewrite historical acquisitions.

Both the acquisition URL and authenticated torrent hash must differ from the
parent group. Metadata must contain one exact nonempty, non-padding video at the
reviewed path. Every child captures the full old owner set, individual parent ID,
new physical binding, release/profile and deterministic canonical range path.
The group suffix gives the new file its own destination. An ordinary individual
submission cannot replace a shared owner.

Preview authenticates metadata without requesting payload or recording jobs.
Apply authenticates it again. The guard binds the complete current parents,
monitoring choices, release/profile, private source, authenticated hash/path,
import file size/modification stamp and any existing replacement state. Changed
decisions require a new preview. A newly accepted replacement records all jobs
and full lineage in one frame before workers can acquire anything.

## API and browser

`POST /api/library/OWNER_ID/group` requires the usual Bearer token and JSON body.
Use the mapping fields above for a preview. For apply, additionally supply
`"apply": true` and `"plan_id": "REVIEWED_64_CHARACTER_HEXADECIMAL_GUARD"`.
Unknown fields, partial guards and owner-subset fields are rejected. A baseline
cannot contain acquisition fields. Public reports omit acquisition URLs.

Open a current shared owner's job page from the browser library. Use **Preview
whole-group baseline** or **Preview whole-group replacement**, review the full
owner count, canonical range, release, authenticated torrent/path and library
destination, then **Record reviewed whole-group decision**. Source credentials
remain server-side. Only one shared-file or group review is retained per browser
session, expires after ten minutes, and is tied to that session and supplied
owner ID. Logout/restart discards it. Apply accepts only its guard and normal
CSRF/session/origin fields; it cannot substitute a source or range.

The CLI mapping bound is 512 KiB, API JSON bodies are limited to 1 MiB, and browser
fields/forms retain their 8 KiB/64 KiB limits. Release titles are bounded to 2,048
bytes without controls. The action has a 60-second deadline; metadata inspection
retains the existing file/path/peer/tracker bounds and synchronous DNS caveat.
Offline preview opens existing state read-only. Apply requires a running service.

## Staging, cancellation, retry and monitoring

Native verification and safe import run through the existing shared-file pipeline.
One private copy becomes the group's new range file; each owner confirms that
exact path in Plex, including configured prefix mappings. A matching older path
for even one owner leaves the entire previous group current. Confirmed children
enter `staged`, release their leases and cannot be claimed as ordinary work.
The final confirmation promotes all owners atomically and inherits the current
parent monitoring choices. Without Plex, verified imports stage and promote
through the same transaction.

Disabling any current parent stops new group claims and prevents promotion.
Re-enable it to permit remaining work. An in-flight operation may retain imported
bytes or fail under its normal retry policy while a monitoring choice changes.
An exhausted failure continues to reserve the full parent group; cancel the
replacement before submitting a different candidate.

`cancel CHILD_ID` cancels every owner in that replacement in one transaction,
including staged owners, clears leases and pauses its native transfer when no
other interest requires it. It retains old and new library files and payloads.
A promoted group cannot be cancelled.

`retry CHILD_ID` retries the complete failed or cancelled group when there are no
active leases. Every parent must still be current and monitored, and no competing
replacement may reserve it. All children requeue together, retain their recorded
binding and imports, and repeat required Plex confirmation. Stale workers cannot
complete after cancellation or a new lease. Retry cannot revive an obsolete
parent group or reset a successfully promoted version.

Re-reviewing the same candidate can reuse its recorded jobs, including cancelled
ones. It never resets their state. A retry is a separate explicit operation.

## Persistence and scope

Whole-group baseline, creation, promotion, cancellation and retry frames use
`MYNOUJ03`, containing the complete ordered owner set and explicit action. Group
baselines/replacements require `MYNOUS03` snapshots. Readers still support formats
1 and 2 without migration writes. Older binaries reject format 3. Back up complete
request/series state, downloads and library; use Mynou 0.18 or later after these
group operations.

Torn final transactions recover all owners or none. Complete checksum failures,
incomplete lineage, conflicting ownership, partial promotion/cancellation,
inconsistent baselines and fabricated single-owner group controls fail closed.
Staged confirmations survive restart. Promotion is one journal event associated
with the first canonical owner, followed by an atomic in-memory update under the
journal lock. Iterative memoized lineage traversal keeps library projection
bounded without recursive histories.

This operation replaces one shared file with one shared file. Splitting a group
into individual videos, automatic range-file inference/search, video cutting,
ownership contraction/reassignment and deleting prior imports remain later work.
Ordinary pack mapping still requires unique single-episode files. See
[shared ownership](shared-files.md), [library upgrades](library.md) and the
[roadmap](roadmap.md) for adjacent scopes.
