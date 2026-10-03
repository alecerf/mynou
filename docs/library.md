# Library monitoring and controlled upgrades

Mynou 0.8.0 adds a view of its own ready imports, per-entry monitoring, quality
cutoffs, and upgrade requests. A replacement becomes current only after its
download, import and required Plex confirmation complete. The earlier ready
request and its files remain available while the replacement is pending.

## Inspect the owned library

```sh
mynou library --config ./mynou.json
```

The authenticated equivalent is `GET /api/library`. This view covers current
ready imports owned by Mynou. It is not an inventory of every file in Plex or a
scan of arbitrary existing library directories. Each entry includes its job ID,
media identity, recorded release, monitoring state, whether a baseline is
required, whether imported files are present, and any pending upgrade job ID.
A request skipped because Plex already has the title does not establish an owned
import or a release baseline.

When the service is stopped, offline `library` and upgrade previews open the
journal read-only. They do not create its directory/files, change permissions,
compact data or repair an interrupted write. A fresh store produces an empty
view. An incomplete journal tail reports that explicit writable recovery is
required; starting the service permits its normal recovery before previewing
again. Read-only access still excludes a concurrent journal writer.

New automatic acquisitions record the selected release title and profile.
Earlier jobs and explicit local/URL submissions can lack this baseline. They
remain usable, but upgrade checks cannot safely infer a previous release from a
filename or the presence of a Plex title.

If you know the release title for such an owned import, set it explicitly:

```sh
mynou baseline ID --release-title "Example.Movie.2026.720p.WEB-DL.H264.English" \
  --config ./mynou.json
```

The authenticated equivalent is `POST /api/library/ID/baseline`:

```json
{"release_title": "Example.Movie.2026.720p.WEB-DL.H264.English"}
```

Use the current entry's job ID from the library view. A baseline is an explicit
release-name claim supplied by you. This operation fills a missing baseline; it
does not overwrite an existing recorded baseline. The title must identify the
same movie or episode, contain 1–2,048 bytes without control characters, and
refer to a current owned import whose files are present, safe and have declared
video streams. The older release may fall outside today's profile restrictions.
These checks do not prove actual audio tracks, decoded video quality, or the
file's provenance.

## Preview and apply upgrades

```sh
mynou upgrades --config ./mynou.json
mynou upgrades --apply --config ./mynou.json
```

`upgrades` previews decisions for monitored entries. It contacts configured
indexers but does not write the journal, update check timestamps, queue a
replacement, or start a download. `--apply` explicitly permits those journal
changes and queues eligible replacements; it does not wait for every transfer
to complete. Manual preview and apply passes ignore the background polling
interval, so you can check again immediately after changing a profile.

The authenticated equivalent is `POST /api/upgrades`:

```json
{"apply": false}
```

Omitting `apply` defaults to `false`. Set it to `true` to apply decisions. Search
and upgrade reports omit acquisition URLs and credentials.

Both the recorded baseline and new candidates are assessed with the **current
configured profile** for that movie or episode. Eligibility comes first. If the
baseline no longer passes the current profile, an accepted candidate can replace
it even when its raw rank is lower. For example, changing the required language
can permit an accepted English release to replace a previously accepted French
baseline. This is an explicit policy change.

If the baseline still passes the current profile, the candidate needs a strict
improvement in custom score or ordered resolution/source/codec/language
preferences. More seeds alone, or a different tie-breaking name, do not make an
equal-quality release an upgrade. The profile name recorded at acquisition is
historical context; changing the configuration changes subsequent comparisons.

Reports include `apply`, `checked`, `queued`, `entries`, `limited` and `skipped`.
The skipped counts identify `unmonitored`, `baseline_required` and
`unsupported_kind` entries. Only eligible movie/episode entries consume the
configured check limit. `limited: true` means the pass did not cover every
eligible entry within its bounded work budget. Individual entries explain
outcomes such as a reached cutoff, missing imports, unavailable search, an
existing pending upgrade, no improvement, or a queued replacement.

A search/upgrade pass has a 90-second work budget covering indexer HTTP/socket
operations and processing checks. Synchronous standard-library DNS resolution
can block beyond that deadline; a result arriving after it is rejected. The
budget is therefore not a strict wall-clock guarantee when DNS stalls.

Requests for the same replacement are deduplicated. A pending child does not
take over the library view. A failed or canceled child leaves the earlier ready
entry current. After a child is ready, it becomes the current entry and its ID
is the one to use for later monitoring and management. An unrelated manual
request for the same movie or episode cannot become ready while an upgrade is
pending; cancel that pending upgrade first if you want the manual request to
complete instead.

## Monitoring policy

Add the optional top-level `monitoring` object to `mynou.json`:

```json
{
  "monitoring": {
    "enabled": false,
    "interval_secs": 3600,
    "max_checks": 32
  }
}
```

These are the defaults, including for older configurations with no `monitoring`
object. `enabled: true` permits the running service to check monitored imports
and queue upgrades in the background. Manual `upgrades --apply` remains an
explicit action when background monitoring is disabled.

`interval_secs` must be between 60 and 86,400. `max_checks` must be between 1
and 256 and bounds eligible entries checked in a pass. Background checks honor
the per-entry polling interval. Due entries are considered by oldest check time
so a repeatedly failing or low-ID entry does not monopolize the schedule.
Applied checks persist timestamps even when source search fails, providing
background backoff. Previews do not persist timestamps or change scheduling.

Change monitoring for an individual current entry:

```sh
mynou monitor ID --config ./mynou.json
mynou unmonitor ID --config ./mynou.json
```

The authenticated equivalent is `POST /api/library/ID/monitor`:

```json
{"enabled": true}
```

An entry without a recorded baseline is ineligible for automatic upgrades even
when monitoring is enabled. Enabling global monitoring does not invent that
baseline for older jobs or adopt unrelated Plex files. If an upgrade is already
in progress, promotion inherits the parent's current monitoring choice rather
than restoring the choice recorded when the child was queued.

## Quality cutoffs

A profile can stop further upgrades when an accepted baseline reaches its
configured cutoff:

```json
{
  "selection": {
    "movie_profile": "hd",
    "episode_profile": "hd",
    "profiles": {
      "hd": {
        "resolutions": [1080, 720],
        "cutoff_resolution": 1080,
        "sources": ["web-dl", "bluray", "webrip"],
        "codecs": ["h265", "h264"],
        "languages": ["en"]
      }
    }
  }
}
```

`cutoff_resolution` defaults to `null`, meaning no cutoff. A non-null value must
be one of the supported resolutions and appear in that profile's `resolutions`
list. The list's preference order defines the cutoff: a baseline at the cutoff
or an earlier position has reached it. This is not a numeric "at least this
many pixels" comparison. For example, `[720, 1080]` with cutoff `1080` treats
`720` as already preferred enough to stop. Reaching a cutoff stops later source,
codec and custom-score upgrades too.

See [selection profiles](selection.md) for marker interpretation, required and
blocked terms, custom scores, and unknown attributes. Release-title markers
remain claims rather than verified decoded-media characteristics.

## Import and Plex confirmation

Upgrade imports use a distinct filename with a `[mynou-32hexjobid]` suffix. They
do not overwrite the earlier import. Original library files and downloaded
sources remain on disk; there is no automatic cleanup. Plex can therefore
temporarily or permanently expose multiple versions until you remove old files
deliberately.

When Plex is enabled, confirming that the title already exists is insufficient
for an upgrade. Plex must report the **new imported path** in a `Part.file`
field before the child becomes ready. If Plex sees the shared directory under a
different path, configure `plex.path_mappings`:

```json
{
  "plex": {
    "path_mappings": [
      {"mynou_prefix": "/library", "plex_prefix": "/media"}
    ]
  }
}
```

This maps `/library/movies/Example.mp4` to `/media/movies/Example.mp4` for
confirmation. Prefixes match complete lexical path components; `/library` does
not match `/library-extra`. The longest matching prefix wins. With no mapping,
Plex must report the same path seen by Mynou. Mapping translates paths for
comparison; it does not move files or configure container mounts. Even when
`--config` uses a relative filename, configured library roots resolve to absolute
paths before importing and comparing Plex paths.

Back up configuration, the journal, downloads and library before changing
versions. Existing Go/SQLite data is still a separate format. See
[Docker deployment](deployment.md) and [limits](limits.md).
