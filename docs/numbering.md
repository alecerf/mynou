# Explicit episode numbering

An episode has three separate labels: its retained library number, its current
catalog number, and the number used by a source. Mynou binds these labels to a
known catalog episode ID. Library numbers, request keys, exclusions and existing
import paths stay fixed when an approved catalog or source label changes.

For example, catalog ID `11001` originally belongs to library `S01E01`. If the
catalog moves it to `S01E10` and your source calls it `013`, an explicit choice can
retain `S01E01`, approve catalog `S01E10`, and search source absolute `13`. A new
request imports as `S01E01`. An existing request keeps the source choice captured
when it was submitted, including through retries and upgrades.

Save a new series with `--unmonitored` when you need to choose numbering before
its first acquisition. Numbering apply records choices and an accepted catalog
plan; it does not itself create requests. Enabling monitoring or refreshing the
series can then queue aired missing episodes under the normal 64-job bound.

## Preview and apply

Inspect the current catalog comparison without writes:

```sh
mynou series-numbering SERIES_ID --config ./mynou.json
```

The report includes each known ID's retained `canonical`, observed `catalog`,
and proposed `source` labels, `issues`, `resolved`, and a `plan_id`. It contains
no acquisition URLs. Unknown IDs cannot be used for a numbering decision.

Write `numbering.json` with only a `changes` array:

```json
{
  "changes": [
    {
      "catalog_id": 11001,
      "catalog": { "season": 1, "episode": 10 },
      "source": { "absolute": 13 }
    },
    {
      "catalog_id": 11002,
      "catalog": { "season": 1, "episode": 11 },
      "source": { "season": 2, "episode": 3 }
    }
  ]
}
```

The `catalog` numbers must match that episode's current catalog labels. Source
labels are explicit choices: either `season` plus `episode`, or `absolute` alone.
Choices must not duplicate another retained episode's catalog/source label.
Unspecified choices retain their accepted value, defaulting to the original
library number. To restore a source label, explicitly choose its original
season/episode; there is no implicit reset or identity reassignment.

```sh
mynou series-numbering SERIES_ID --mapping numbering.json --config ./mynou.json
mynou series-numbering SERIES_ID --mapping numbering.json --apply \
  --plan-id PLAN_ID_FROM_PREVIEW --config ./mynou.json
```

Apply fetches the catalog again and compares the exact prior record, observed
catalog and proposed choices. Changed metadata, settings or choices require a
new preview. An unresolved proposal never persists. All changes in one decision
are saved together in the verified snapshot before they affect future requests.
Apply requires a running service. Offline preview opens existing storage without
creating files, changing permissions, repairing journals or migrating snapshots.

The Bearer-authenticated API uses `POST /api/series/SERIES_ID/numbering`. A preview
body contains `changes` and optional `"apply": false`. Apply adds `"apply": true`
and `"plan_id": "..."`. Unknown fields, duplicate IDs and invalid numbers fail
before a decision can be recorded.

Browser series details provide **Episode numbering**. Preview the JSON choices,
review the comparison, then use **Save reviewed numbering**. The normal session,
same-origin and form-token protections apply. The browser shows the first 100
comparison rows; CLI/API reports contain all bounded rows. Browser fields retain
their 8 KiB limit; use CLI/API for larger decisions.

## Acquisition and refresh

New series jobs capture the approved source number separately from their
canonical season/episode. Numbered source titles must contain the exact single
`SxxExx` or `NxNN` label. Explicit absolute requests accept a single decimal label
immediately after the series title and optional first-air year, such as
`Example Series 013 1080p`. Absolute titles without an explicit choice remain
unresolved. Chained labels, ranges and conflicting labels fail identity matching.
Profiles, scores, seed bounds and acquisition-URL redaction still apply.

Numbered JSON/Torznab queries send the approved source season/episode. Absolute
queries add the padded number to the search term; JSON also receives `absolute`,
while Torznab omits season/episode parameters. When a transfer contains multiple
videos, exactly one filename must match the saved source label. A multi-episode
video does not gain logical ownership through this feature.

Standalone `search` and `submit` accept `--source-numbering JSON` for an explicit
episode request. The API request object accepts the same `source_numbering` value.
This does not change its canonical identity or update a tracked series policy.
Existing deduplicated requests retain their captured choice. Series choices do
not silently replace a failed/cancelled request or its acquisition source.

Accepted catalog changes normalize known IDs back to retained library numbers
before monitoring, calendar lookup, pack submission or deduplication. Cross-season
moves retain the original scope; numbering preview and changed-number refreshes
fetch the bounded full catalog before filtering by canonical scope. A replacement
ID cannot take a retained number, including after the earlier ID disappears.
Conflicting identities across overlapping scopes fail rather than creating jobs.

Automatic season-pack mapping retains its original strict numbering rules. A
season with alternate source labels requires explicit [pack mappings](packs.md);
background pack preference falls back to individually numbered jobs. Plex
confirmation and import naming use retained canonical numbers. Configure Plex's
episode order consistently with those library names.

## Persistence and bounds

The series snapshot reads schema 1 and 2. Schema 1 infers anchors from existing
known IDs in memory. Read-only access never rewrites it. A successful explicit
save writes schema 2 with retained ID/number anchors and approved choices, using
the existing checksum, private file, atomic replacement and directory sync.
Keep a backup of earlier state when changing versions: 0.15 cannot open schema 2.
Jobs without `source_numbering` retain their historical behavior and keys.

There are at most 2,000 retained IDs and choices per scope, 20,000 retained/active
entries across scopes, 128 scopes and an 8 MiB snapshot. A decision accepts at
most 2,000 changes; CLI mapping files are bounded to 512 KiB and API bodies to
1 MiB. Seasons range from 0–9,999, episodes/absolute numbers from 1–99,999 and
catalog IDs from 1 to 2^53−1. Full catalog fetches retain the 100-season,
2,000-episode and shared 90-second bounds. Numbering history is not silently
discarded to make room or to reuse an earlier identity.

Automatic anime-order inference, reassignment to a replacement catalog ID,
multi-episode physical ownership, video splitting and library renaming/deletion
remain later stages. [CI scenarios](../tests/episode_numbering.rs) and
[protected management scenarios](../tests/numbering_management.rs) use synthetic
catalogs, indexers, files and local services; validation runs only in Actions.
