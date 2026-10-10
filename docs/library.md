# Library monitoring and upgrades

Mynou keeps a view of the imports it owns and can replace one with a better
release. A replacement becomes current only after its download, import and
required Plex confirmation complete; until then the earlier import stays
current. Earlier files and downloads always stay on disk.

Single movies and episodes use the commands below. Episodes that share one
video file are upgraded as a whole group; see
[shared groups](#upgrade-a-shared-group).

## Inspect the owned library

```sh
mynou library --config ./mynou.json
```

The API equivalent is `GET /api/library`. The view lists current ready imports
owned by Mynou: job ID, media identity, recorded release, monitoring state,
whether a baseline is required, whether the imported files are present and any
pending upgrade. It is not an inventory of Plex or a scan of existing library
folders. A request skipped because Plex already had the title does not create an
owned import.

When the service is stopped, `library` and upgrade previews open the journal
read-only: they create nothing and repair nothing. After an interrupted write,
they report that writable recovery is required; start the service once, then
preview again.

## Record a missing baseline

An upgrade compares a candidate with the release title recorded when the import
was acquired. Automatic acquisitions record it; explicit `--url` or `--path`
submissions and older jobs may not have one. Mynou never guesses it from a file
name. If you know the release, record it once:

```sh
mynou baseline ID --release-title "Example.Movie.2026.720p.WEB-DL.H264.English" \
  --config ./mynou.json
```

The API equivalent is `POST /api/library/ID/baseline` with
`{"release_title": "..."}`. Use the current entry's job ID. The title must
identify the same movie or episode, and the import must be present, safe and
contain a declared video stream. A baseline only fills a missing value; it never
replaces a recorded one. It is your claim, not proof of the file's tracks or
quality, and it may fall outside today's profile.

## Preview and apply upgrades

```sh
mynou upgrades --config ./mynou.json
mynou upgrades --apply --config ./mynou.json
mynou monitor ID --config ./mynou.json
mynou unmonitor ID --config ./mynou.json
```

`upgrades` previews decisions for monitored entries. It contacts your sources
but writes nothing, queues nothing and does not change check times. `--apply`
records the decisions and queues the replacements without waiting for them.
Manual passes ignore the polling interval, so you can check again right after
changing a profile. The API equivalents are `POST /api/upgrades` with
`{"apply": false}` (the default) or `{"apply": true}`, and
`POST /api/library/ID/monitor` with `{"enabled": true}`.

Both the baseline and the candidates are judged with the **current** profile
for that movie or episode:

- If the current profile rejects the baseline, any accepted candidate can
  replace it, even with a lower raw rank. Changing a required language is one
  example of such a deliberate policy change.
- If the baseline is still accepted, the candidate needs a strictly better
  custom score or ordered resolution, source, codec or language preference.
  More seeds or a different name alone are not an improvement.
- A reached [cutoff](#quality-cutoffs) stops further upgrades.

Reports include `apply`, `checked`, `queued`, `entries`, `limited` and `skipped`
(with `unmonitored`, `baseline_required` and `unsupported_kind` counts). Only
eligible entries count toward the check limit; `limited: true` means the pass
ran out of budget before covering every entry. Each entry explains its outcome,
such as a reached cutoff, missing files, an unavailable search, a pending
upgrade, no improvement or a queued replacement. Reports never include
acquisition URLs or credentials.

An applied upgrade is a child request, deduplicated by replacement. A failed or
cancelled child leaves the earlier entry current. Once the child is ready it
becomes the current entry: use its ID from then on. It inherits the parent's
monitoring choice at that moment. While an upgrade is pending, another manual
request for the same movie or episode cannot become ready; cancel the pending
upgrade first if you prefer the manual request.

## Background monitoring

Monitoring is off by default. To let the service check monitored imports and
queue upgrades on its own, add a top-level `monitoring` object:

```json
{
  "monitoring": {
    "enabled": true,
    "interval_secs": 3600,
    "max_checks": 32
  }
}
```

`interval_secs` accepts 60 to 86,400 and `max_checks` (entries per pass) 1 to
256; the values shown are the defaults. Configure profile cutoffs before
enabling unattended upgrades, and restart the service after the change. Entries
due the longest are checked first. Applied checks record their time even when
the search fails, which spaces out retries; previews record nothing. Entries
without a baseline stay ineligible, and enabling monitoring never adopts
unrelated Plex files.

This setting only concerns upgrades of existing imports. Acquiring newly aired
episodes is controlled by [series monitoring](series.md).

## Quality cutoffs

A profile can stop upgrades once an accepted baseline is good enough:

```json
"hd": {
  "resolutions": [1080, 720],
  "cutoff_resolution": 1080,
  "sources": ["web-dl", "bluray", "webrip"]
}
```

`cutoff_resolution` defaults to `null` (no cutoff). It must appear in the
profile's `resolutions` list, and the list order defines it: a baseline at the
cutoff's position or earlier has reached it. It is not a pixel threshold, so
`[720, 1080]` with cutoff `1080` also stops at 720p. A reached cutoff stops
source, codec and score upgrades too. Initial selection is unaffected.

## Import and Plex confirmation

An upgrade imports beside the earlier file under a distinct name ending in
`[mynou-<job id>]`; it never overwrites. Old imports and downloads are kept and
there is no automatic cleanup, so Plex may show several versions until you
remove old files yourself. Plan disk space accordingly.

With Plex enabled, the upgrade is ready only when a fresh Plex response reports
the **new** path in `Part.file`; an existing matching title is not enough. If
Plex sees the library under another path, configure
[path mappings](macos.md#connect-plex).

## Upgrade a shared group

Episodes that share one video
([shared videos](packs.md#one-video-for-several-episodes)) cannot be upgraded
or given a baseline individually; upgrade reports show
`shared_group_upgrade_required` and scans never search these groups. Instead,
review the whole group with `library-group`, using any owner's job ID. Every
owner must be ready and current, and the operation always covers the complete
recorded range.

To record a missing baseline for the whole group, create `baseline.json`:

```json
{"action": "baseline", "release_title": "Example.Series.S01.720p.WEB-DL"}
```

To replace the video, create `replacement.json`:

```json
{
  "action": "replace",
  "release_title": "Example.Series.S01E01-E02.1080p.WEB-DL",
  "source_url": "magnet:?xt=urn:btih:REPLACEMENT_TORRENT_HASH",
  "file_path": "Example/shared.mp4"
}
```

Preview, then apply the reviewed plan with a running service:

```sh
mynou library-group OWNER_ID --mapping replacement.json --config ./mynou.json
mynou library-group OWNER_ID --mapping replacement.json --apply \
  --plan-id REVIEWED_PLAN_ID --config ./mynou.json
```

A replacement requires every owner to be monitored and a consistent group
baseline, and follows the same quality rules as single upgrades. Empty
preference lists give no rank, so name ordered resolutions, sources, codecs,
languages or scores to define an improvement. The release title must start
with the series title (and optional matching year) and describe the whole
range or its season, for example `S01E01-E02`, `S01E01E02`, `S01E01-S01E02` or,
for absolute numbering, `13-14`. Mixed or non-consecutive labels are rejected.
The URL and the authenticated torrent hash must differ from the current group,
and the metadata must contain exactly one non-empty video at `file_path`.

Preview authenticates the torrent metadata without downloading payload or
recording jobs. Apply authenticates it again and checks that nothing changed:
owners, monitoring, release, source, hash, path and import files. Otherwise
run a new preview. The API equivalent is `POST /api/library/OWNER_ID/group` with
the same fields, plus `"apply": true` and `"plan_id"` to apply. 

The new video downloads, imports once and must be confirmed in Plex for every
owner. Confirmed owners wait in the `staged` state; the last confirmation
promotes the whole group at once, so the old group stays current until then.
Without Plex, verified imports promote the group directly. Disabling monitoring
on any owner stops new work and blocks promotion until re-enabled.
`cancel CHILD_ID` cancels the whole replacement (including staged owners) and
keeps all files. `retry CHILD_ID` retries the whole group when every parent is
still current and monitored and no other replacement holds it. A promoted group
cannot be cancelled. To try a different candidate after an exhausted failure,
cancel the replacement first. Reviewing the same candidate again reuses its
recorded jobs, including cancelled ones, without resetting them; retry is
always a separate, explicit action.
