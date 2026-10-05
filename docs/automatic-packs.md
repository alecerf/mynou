# Automatic season-pack search and acquisition

Mynou 0.15 can search for a season pack, rank its release title under the episode
profile, inspect authenticated torrent metadata and map its files to a retained
catalog plan. A preview records no jobs, starts no payload download and creates
no native transfer. Acquisition records ordinary mapped episode jobs sharing
the native torrent identity and selective file interests from 0.14.

Enable TMDB and configure sources and the episode profile before using this
feature. Track the series first. To choose a pack before individual acquisition,
save a new record with `track-series --unmonitored`; browser tracking offers the
same choice. On-demand pack acquisition works with background monitoring off
and does not enable it.

## Preview and acquire

Use the series record's 32-character identifier:

```sh
mynou series-pack-search ID --season 1 --config ./mynou.json
```

The report contains title assessments, `requested_episodes`, `metadata_decisions`,
`scope_id` and, when a candidate resolves, `selected_candidate_id`, `torrent_id`
and a catalog/file `mapping`. Accepted title assessments alone do not prove that
a pack can be acquired. Mynou inspects ranked candidates in order, stopping at
the first candidate whose files uniquely cover every requested episode.
Unresolved metadata decisions explain why earlier candidates could not map.
Public reports omit acquisition URLs and redact credentials from labels.

To acquire a particular previewed decision, copy both guard values:

```sh
mynou series-pack-search ID --season 1 --apply \
  --scope-id SCOPE_ID --candidate-id CANDIDATE_ID --config ./mynou.json
```

Acquisition searches and inspects again. The scope fingerprint includes today's
UTC date, the accepted catalog content, monitoring choices and missing episode
identities. The selected candidate fingerprint also binds its release decision,
authenticated torrent hash and exact paths. A changed scope, ranking decision,
source hash or mapping rejects that guarded apply. Run a new preview to review
the current result; guard values are not reservations.

`--apply` without either guard intentionally acquires the current resolved
decision. Partial guard pairs and guards on a preview are invalid. An empty
eligible scope returns `scope_empty: true` without contacting sources or
resurrecting terminal jobs. Acquisition requires a running service. CLI previews
can open existing storage read-only when the service is unavailable; they do not
create, repair or change the journal or series snapshot.

In browser series details, use **Preview season packs**, review the file mapping
and choose **Acquire resolved pack**. The acquisition form always carries both
preview guards and uses the existing session, origin and form-token checks.

The Bearer API route is `POST /api/series/ID/pack-search`. Preview body:

```json
{ "season": 1 }
```

Guarded acquisition body:

```json
{
  "season": 1,
  "apply": true,
  "scope_id": "COPY_THE_64_CHARACTER_SCOPE_ID",
  "candidate_id": "COPY_THE_64_CHARACTER_SELECTED_CANDIDATE_ID"
}
```

Replace the example guard strings with the returned lowercase hexadecimal
values. Only these four fields are accepted; season is an integer from 0 to
9999 and `apply` is a boolean defaulting to false. Season must exist in the
accepted plan. The acquisition response adds `submission` with its public jobs
and submitted/reused counts.

## Which episodes and files qualify

The scope includes at most 64 missing episodes from one season. Each needs a
known catalog identity and an air date on or before today, at or after any
configured earliest date. Excluded episodes and disabled specials do not
qualify. Season zero requires `include_specials`. All existing same-media jobs
suppress acquisition, including failed and cancelled jobs. Use deliberate job
retry controls rather than automatic pack search to reopen work.

Pack titles must begin with the normalized series title, optionally its matching
known year, then `S01` or `Season 1`. Foreign titles, another explicit year,
another season marker and individual episode markers are rejected. This is a
separate matching path: ordinary movie and episode searches retain their rules.
Minimum seed counts and the configured episode profile apply before metadata
inspection. Torznab season search omits the individual `ep` parameter.

Each selected video basename needs exactly one explicit `S01E02` or `1x02`
marker. A preceding series name, when present, must match the accepted title and
optional year. Conflicting parent season markers, duplicate assignments, unknown
episodes, another season, ambiguous/multiple episode markers and absolute/anime
numbers reject the mapping. Every requested episode must have one nonempty
video. A known but unrequested episode can remain unselected.

Supported video extensions are MP4/M4V/MOV, MKV/WebM and AVI. Padding and nonvideo
files are ignored, as are exact `sample`/`trailer` basenames and `samples`,
`trailers` or `extras` directories below the torrent root. Paths use the exact
native metadata spelling, including the torrent root. Existing confined-path
limits apply: 4,096 bytes, 32 components and 255 bytes per component. A hash
authenticates torrent bytes; filename markers still cannot prove the depicted
episode or its audio/video quality.

Use [explicit pack mappings](packs.md) when numbered files cannot resolve under
these rules. Explicit [numbering choices](numbering.md) retain source labels and
canonical identities. Use the dedicated [shared-file action](shared-files.md)
for one video containing consecutive episodes; automatic range inference remains
blocked until coordinated group replacement is available.

## Prefer packs during monitoring

This optional configuration keeps earlier installations on individual-episode
acquisition by default:

```json
"series_packs": { "enabled": true }
```

With it enabled, new monitored tracking and catalog refresh try mapped packs
before individual episode jobs. Unmonitored records do not perform this
background lookup. A pass considers at most four missing seasons in numeric
order and never applies a pack exceeding its remaining 64-job allowance. An
unresolved pack falls back to the existing individual-episode flow. The combined
pack/individual batch remains bounded to 64 submissions; backlog schedules
another pass. Catalog fetching, pack searches and submission checks share the
existing 90-second pass deadline. Expired or changed catalog/policy results are
discarded before the remaining fallback batch.

Automatic pack preference does not retry existing jobs, infer upgrade quality,
disable a durable native pause or change specials/date/exclusion policy. A
manual preview provides detailed pack decisions when diagnosing an individual
fallback. There is no new persistent pack-search error history in this release.

## Discovery bounds and persistence

At most eight ranked candidates undergo metadata decisions. A per-call cache
reuses metadata for an identical source URL within that bound. The overall
search budget is 90 seconds; each metadata attempt has at most ten seconds,
reduced to the remaining search/pass budget. Inspection accepts at most 1,024
files and 1 MiB of aggregate path bytes. The existing torrent parser separately
bounds encoded input to 8 MiB and enforces its native piece limits; the path
bound is not a bound on all process memory.

HTTP/HTTPS and service-visible local `.torrent` sources expose their metadata
directly. Magnets use at most eight distinct outbound peers and four trackers,
with a shared deadline and at most five seconds per peer/tracker attempt.
Metadata is authenticated against the requested v1/v2 hash, retaining the
source alias for hybrid magnets. Preview peers request metadata, never payload.
HTTP/HTTPS/UDP tracker lookups use a stopped event, zero payload counters,
nonzero bytes left and an ephemeral port; they do not announce completion or
serve payload. Some trackers decline such metadata-only queries. This inspection
path has no DHT or PEX fallback; queued native acquisition retains its existing
discovery support. A DHT-only magnet can therefore fail automatic inspection
while remaining usable through explicit acquisition.

Synchronous standard-library DNS and filesystem calls cannot be interrupted by
the deadline. Late results are rejected; the budget is not a strict wall-clock
guarantee. Source/metadata I/O never holds series or request storage locks.

Applied jobs persist `pack_file` and `pack_origin` before workers can begin.
The origin records series revision/scope, selected candidate, authenticated
torrent hash and release provenance. It is immutable during worker updates.
Before native queue publication, workers require the source to match that
authenticated identity, including cached transfer paths. A changed HTTP torrent
fails instead of silently acquiring another torrent. Operator mapping correction
retains the original decision provenance and hash.

Pack provenance is not a per-episode upgrade baseline: `Job.release` remains
absent. Explicit baseline controls are still required before upgrade eligibility.
Confirmed jobs are separate synchronized commits, so storage failure can leave
a partial batch; a later submission deduplicates it. Earlier jobs without origin
fields keep their behavior. Preserve all storage in backups and do not downgrade
automatic pack jobs to binaries that ignore their identity guards.

[Series monitoring](series.md) · [Explicit mappings](packs.md) ·
[Selective transfers](transfers.md#selective-acquisition-in-0140) ·
[Validation](validation.md) · [Roadmap](roadmap.md)
