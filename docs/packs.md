# Season packs and shared videos

A season pack is one torrent containing several episodes. Mynou can acquire a
pack in three ways, all built on a tracked [series](series.md) plan:

- **Explicit mapping**: you name the exact file for each episode.
- **Automatic search**: Mynou finds a pack and maps its numbered files itself.
- **Shared video**: one video file contains several consecutive episodes.

In every case Mynou records one ordinary episode job per episode. The jobs share
one native transfer that downloads only the selected files (plus the boundary
pieces they need, see [transfers](transfers.md#selective-acquisition)), and each
job imports exactly its own file. Hash verification proves the bytes belong to
the torrent; it cannot prove that a file shows the episode you assigned. Review
the files before mapping them.

Packs work while series monitoring is off. To choose a pack before Mynou
requests individual episodes, track the series with `--unmonitored` (or
**Save without automatic episode acquisition** in the browser). Existing
requests for an episode, in any state, are always reused rather than replaced.
Pack jobs have no release baseline, so [library upgrades](library.md) need an
explicit baseline first.

## Map files explicitly

Create a mapping file with the exact path of each episode's file:

```json
[
  {"season": 1, "episode": 1, "file_path": "Example.Pack/001.mp4"},
  {"season": 1, "episode": 2, "file_path": "Example.Pack/002.mp4"}
]
```

Paths are case-sensitive and relative to the torrent's content root, including
its top-level folder; they never include the download directory or a hash
prefix. Single-file torrents use the file name. Use the paths shown in transfer
details or in the torrent metadata. Absolute or anime-style names such as
`Pack/001.mp4` work because you choose the mapping.

```sh
mynou series-pack ID --url 'magnet:?xt=urn:btih:...' \
  --mapping ./episodes.json --config ./mynou.json
```

The source can be a magnet, an HTTP or HTTPS torrent URL or a `.torrent` path
visible to the service. The torrent is only read when the job runs, so an
unreachable source creates jobs that then fail with a clear error. In the
browser, open the series and use **Acquire a mapped pack**. The API equivalent
is `POST /api/series/ID/packs` with `{"source_url": "...", "episodes": [...]}`;
the response contains `series_id`, `submitted`, `reused` and the public `jobs`.

Each mapped episode must exist in the accepted plan with a catalog ID and an
air date on or before today (UTC); future, undated and unidentified episodes
are refused. Specials must already be in the plan. A mapping can include
episodes excluded from monitoring or before the earliest monitored date, and it
does not enable monitoring. Two episodes cannot map to the same file (use a
[shared video](#one-video-for-several-episodes) for that). A missing,
audio-only or malformed file makes its job fail; Mynou never picks another
video instead. The whole submission is checked before the first job is
recorded, but jobs are saved one by one: after an interruption, submitting the
same mapping again reuses the recorded jobs and adds the rest.

### Correct a wrong mapping

Let the job fail or cancel it before anything is imported, then remap it:

```sh
mynou pack-remap JOB_ID --file-path Pack/corrected.mp4 --config ./mynou.json
```

The API equivalent is `POST /api/jobs/JOB_ID/pack-mapping` with
`{"file_path": "Pack/corrected.mp4"}`; the browser offers **Correct mapping and
retry** in job details. The job must be `failed` or `cancelled`, without an
active lease or a confirmed import, and the new file must not belong to another
episode of the same pack. Mynou records the correction, resets attempts and
requeues the job, reusing bytes that were already verified.

## Search for a pack automatically

```sh
mynou series-pack-search ID --season 1 --config ./mynou.json
mynou series-pack-search ID --season 1 --apply \
  --scope-id SCOPE_ID --candidate-id CANDIDATE_ID --config ./mynou.json
```

The preview searches your sources, ranks pack titles under the episode profile
and inspects the best candidates' metadata, without downloading payload,
recording jobs or creating a transfer. It stops at the first candidate whose
files cover every requested episode. The report shows title assessments,
`requested_episodes`, `metadata_decisions`, `scope_id` and, when a candidate
resolves, `selected_candidate_id`, `torrent_id` and the file `mapping`. It never
includes acquisition URLs. When the service is stopped, the preview opens
existing storage read-only.

To acquire, copy both guards from the preview. Apply searches and inspects again
and refuses a changed catalog, monitoring choice, missing-episode set, ranking,
torrent hash or mapping; guards are checks, not reservations. `--apply`
without guards acquires whatever currently resolves. An empty scope returns
`scope_empty: true` without contacting sources. Applying needs the running
service. The API equivalent is `POST /api/series/ID/pack-search` with
`{"season": 1}`, plus `"apply": true`, `"scope_id"` and `"candidate_id"` to
acquire; the response then adds `submission`. In the browser, use **Preview
season packs** and **Acquire resolved pack** in series details.

What qualifies:

- **Episodes**: at most 64 missing episodes of one season, each with a catalog
  ID and an air date on or before today and after any earliest monitored date.
  Excluded episodes and disabled specials are skipped; season zero needs
  `include_specials`. Any existing job for an episode, even failed or cancelled,
  excludes it.
- **Titles**: the pack title starts with the series title, optionally its
  year, then `S01` or `Season 1`. Other titles, years, seasons and individual
  episode markers are rejected. The minimum seed count and the episode profile
  apply before any metadata is inspected.
- **Files**: each requested episode needs one non-empty video whose name has
  exactly one `S01E02` or `1x02` marker (any series name in it must match). MP4,
  M4V, MOV, MKV, WebM and AVI files count; padding, non-video files, `sample` or
  `trailer` files and `samples`, `trailers` or `extras` folders are ignored.
  Duplicates, other seasons, ambiguous markers and absolute numbers are
  rejected; use an explicit mapping in those cases.

### Prefer packs during monitoring

```json
"series_packs": {"enabled": true}
```

With this option (off by default), monitored tracking and refreshes try mapped
packs before individual episode jobs. A pass looks at up to four missing
seasons in order and never applies a pack larger than its remaining 64-job
allowance; unresolved seasons fall back to individual episodes. Unmonitored
records do no background pack search. Packs never retry existing jobs, unpause
transfers or change specials, dates or exclusions. Use a manual preview to see
why a season fell back.

## One video for several episodes

Some releases put consecutive episodes in one file. A shared video binds one
file to 2 to 64 consecutive episodes of one season. Each episode keeps its own
job and Plex check, and all of them share one library file. Every episode needs
a catalog ID and an air date in the past. Create `shared.json` with library
(canonical) numbers, as shown in series details:

```json
{"file_path": "Pack/combined.mp4", "season": 1, "episodes": [1, 2]}
```

Preview, then apply with a running service:

```sh
mynou series-shared-file SERIES_ID \
  --url 'magnet:?xt=urn:btih:TORRENT_HASH' --mapping shared.json \
  --config ./mynou.json
mynou series-shared-file SERIES_ID \
  --url 'magnet:?xt=urn:btih:TORRENT_HASH' --mapping shared.json \
  --apply --plan-id REVIEWED_PLAN_ID --config ./mynou.json
```

The preview authenticates the metadata and requires exactly one non-empty video
at that path; it downloads no payload and records no jobs. Review `binding`,
`group_id`, `new_owners` and `plan_id` before applying. The API equivalent is
`POST /api/series/SERIES_ID/shared-file` with `source_url`, `file_path`,
`season`, `episodes` and, to apply, `"apply": true` and `"plan_id"`. The
browser offers **One video for multiple episodes**, **Preview shared file** and
**Record reviewed shared ownership** in series details.

The file is imported once, under a name that covers the whole range, for
example `Example Series/Season 01/Example Series - S01E01-E02 [mynou-GROUP_ID].mp4`,
and keeps that destination even if the library root changes later. With Plex,
each episode is ready when Plex reports that exact path for it. Cancelling one
episode keeps the binding and the file; retry reuses them. A later review of the
same file can request any subset of the recorded episodes, but a group cannot be
extended, reassigned or partly replaced, and it cannot take an episode that
already has another request. Shared episodes cannot be remapped, given a
baseline or upgraded individually: see
[shared-group upgrades](library.md#upgrade-a-shared-group).
