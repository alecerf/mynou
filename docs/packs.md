# Explicit pack acquisition and file mappings

Acquire several catalog episodes from one torrent by choosing an exact video
path for each episode. Mynou records ordinary episode jobs with durable file
mappings. The native torrent client deduplicates their common content identity,
verifies the required pieces and selected file roots, then imports each mapped file under its catalog
season/episode name. New pack transfers retain the union of their mapped files, acquiring required
boundary bytes without scheduling unrelated pieces. Unmapped files are not
analyzed or imported by these mapped jobs.

This is an explicit operator choice. Review the torrent's files and their
contents before assigning them to episodes. Hash verification establishes the
downloaded bytes' torrent identity, not the correctness of your content mapping.
Absolute/anime-style filenames such as `Pack/001.mp4` can be mapped without
guessing their relationship to catalog numbering.

For release-title ranking, metadata-only discovery and guarded automatic
numbered-file mappings, see [automatic season packs](automatic-packs.md), added
in 0.15. This guide describes the explicit mapping operation.

## Choose the catalog and files

Enable TMDB and track a series. To retain its plan before choosing an acquisition,
create it without automatic monitoring:

```sh
mynou track-series --title "Example Series" --year 2026 --tmdb-id 123 \
  --unmonitored --config ./mynou.json
```

The browser's Track series form has **Save without automatic episode acquisition**.
`POST /api/series` accepts `enabled: false`; its default remains true. Reusing an
existing series scope preserves its prior monitoring policy. Ordinary Plex series
requests retain their existing automatic monitoring behavior.

Create a local mapping file containing an array:

```json
[
  { "season": 1, "episode": 1, "file_path": "Example.Pack/001.mp4" },
  { "season": 1, "episode": 2, "file_path": "Example.Pack/002.mp4" }
]
```

Paths are exact and case-sensitive relative to the native torrent content root,
including its top-level directory when present. They exclude the data directory
and torrent hash prefix. Use the paths shown in transfer file details or in the
torrent's metadata. Single-file torrents use their complete filename.

Submit the pack to the running service:

```sh
mynou series-pack ID --url 'magnet:?xt=urn:btih:...' \
  --mapping ./episodes.json --config ./mynou.json
```

The source can be a supported magnet, HTTP/HTTPS torrent URL or server-visible
`.torrent` path, as for existing explicit torrent requests. The mapping file is
read on the CLI host, with a 1 MiB read limit. Torrent paths refer to the service's
filesystem. Native torrent/source parsing happens during the ordinary acquisition
job, not while recording the mapping; an unreachable or unsupported source can
therefore create a job that subsequently fails with an explicit diagnostic.

In the browser, open a series and expand **Acquire a mapped pack**. Enter its
source and the same JSON array, then inspect the resulting jobs. Job details
display the retained mapped file and link to the shared native transfer once its
identity is known. The source is cleared after submission and omitted from public
reports. The existing session, origin and form-token checks apply.

The Bearer API operation is `POST /api/series/ID/packs`:

```json
{
  "source_url": "https://source.example/season.torrent",
  "episodes": [
    { "season": 1, "episode": 1, "file_path": "Example.Pack/001.mp4" },
    { "season": 1, "episode": 2, "file_path": "Example.Pack/002.mp4" }
  ]
}
```

The response contains `series_id`, `submitted`, `reused` and public `jobs`.
Acquisition URLs are replaced by the existing configured-source label.

## Validation and acquisition

A submission contains 1–64 unique episode numbers and distinct file paths.
Every episode must exist in the accepted series plan, have a nonzero catalog
identity and a valid air date on or before today's UTC date. Future, undated and
unidentified episodes cannot be submitted through this operation. Catalog
specials must first exist in the retained plan, with explicit catalog-specials
settings during planning.

Explicit pack acquisition can choose episodes excluded from automatic monitoring
or outside its earliest monitored date. It does not enable series monitoring.
Existing requests for the same resolved media identity are reused in every
state, including failed and cancelled jobs; the response says how many. It does
not replace an existing individual-episode acquisition, reopen a cancelled job
or create an upgrade of a ready import. Use existing retry controls deliberately.
Track an unmonitored new scope before choosing a pack when you want to avoid
automatic individual requests.

Paths contain at most 4,096 bytes, 32 normal slash-separated components and 255
bytes per component. Empty, absolute, dot, parent, backslash, colon and control
components are rejected. Supported video extensions are MP4/M4V/MOV, MKV/WebM and
AVI. The path must select exactly one file from authenticated native metadata,
then pass the required piece/file-root checks before import.
An absent mapped file produces an error; another video is never chosen as a
fallback. An audio-only or malformed mapped file also fails media analysis.
Two episodes cannot map to the same physical file in this release.

The entire syntax, catalog scope, duplicate constraints, source-key conflicts
and remaining request capacity are checked before recording the first new job.
No network I/O occurs while series/request locks protect that decision. New jobs
are separate synchronized request commits. An I/O failure or interruption can
leave a confirmed partial batch; a repeat submission reuses those jobs and can
record the rest. Workers cannot change mapping or source identities.
Individual episode cancellation and retries use the existing shared-transfer
rules; a durable native transfer pause affects every job using that torrent.

New native pack transfers acquire their retained mapped paths and overlapping
pieces. v1 boundary bytes can populate unselected neighbor files; v2/hybrid
selected file roots remain mandatory. Only a whole verified torrent can seed.
Existing full acquisitions retain their policy, including transfers created by
0.13 or an ordinary full-torrent request. Cancellation retains file interests
and never deletes source bytes. See [selective acquisition](transfers.md#selective-acquisition-in-0140). Only each job's selected verified path is
retained in its file list, avoiding repeated whole-pack file lists in the request
journal. Imports keep source bytes and never overwrite different existing media.
With Plex enabled, the normal episode scan/confirmation pipeline applies.
These explicit acquisitions have no inferred release baseline; existing library
upgrade eligibility still requires a deliberate matching baseline.

## Correct a failed mapping

If a selected path is wrong, first let its request fail or cancel it before an
import is recorded. Use `pack-remap JOB_ID --file-path Pack/corrected.mp4`,
`POST /api/jobs/JOB_ID/pack-mapping` with `{ "file_path": "Pack/corrected.mp4" }`,
or **Correct mapping and retry** in browser job details.

The action requires a mapped request in `failed` or `cancelled` state, no active
lease and no confirmed imports. It validates the new relative path and rejects
a file already mapped to another episode of the same known pack/series. It then
records the corrected mapping, clears the stale file list, resets attempts and
requeues that existing request. The catalog identity and private source remain
unchanged. Existing native pause/control policy still applies, and previously
verified torrent bytes can be reused. Workers cannot perform this correction.
Ready, importing, claimed and already imported requests cannot be remapped.

## Persistence and remaining scope

The optional job `pack_file` field persists through journal/snapshot recovery.
Mynou 0.13 loads earlier jobs with no mapping as ordinary requests. Keep complete
request, torrent and series data in backups. Earlier binaries do not understand
pack mappings: do not downgrade an installation containing these jobs to 0.12
or earlier. Their older readers may ignore the mapping and use older selection
behavior.

Browser source and mapping fields each accept at most 8,192 decoded bytes, so
browser payloads can reach a smaller practical episode/path limit than the API.
The API retains its 1 MiB body limit. JSON fields and types are strict, and
unknown fields are rejected. Public mapped-path labels are bounded, redacted and
escaped in HTML; actual acquisition still uses the private retained path.

Automatic season-pack search/ranking and strict numbered-file mapping are
available through the [automatic pack flow](automatic-packs.md). Selection
contraction, multi-episode videos, alternate catalog-number mappings and general
anime numbering remain a following stage. Explicit file mapping does not imply
a general numbering system or complete Sonarr parity. Mynou preserves
Rust std only, zero Cargo dependencies and CI-only validation.

[Series monitoring](series.md) · [Transfer controls](transfers.md) ·
[Library upgrades](library.md) · [Release roadmap](roadmap.md)
