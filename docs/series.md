# Series monitoring and episode calendar

Track a series once to retain its catalog plan and monitoring choices. Mynou
checks for newly aired episodes while the service runs, then records ordinary
episode jobs. Those jobs use the existing selection, torrent, import and Plex
confirmation pipeline. Future and unresolved episodes stay in the plan.

Enable `catalog.enabled` and configure TMDB credentials before tracking a new
series. Configure your sources and episode selection profile before enabling
unattended acquisition. Plex watchlist synchronization can create series
records, but Plex is optional for manual tracking.

## Track and manage

The following commands use the running service and its API token:

```sh
mynou track-series --title "Example Series" --year 2026 --tmdb-id 123 \
  --config ./mynou.json
mynou series --config ./mynou.json
mynou series ID --config ./mynou.json
mynou series-unmonitor ID --config ./mynou.json
mynou series-monitor ID --config ./mynou.json
mynou episode-unmonitor ID --season 1 --episode 2 --config ./mynou.json
mynou episode-monitor ID --season 1 --episode 2 --config ./mynou.json
mynou series-refresh ID --config ./mynou.json
mynou calendar --from 2026-10-01 --to 2026-10-31 --series-id ID \
  --config ./mynou.json
```

Replace `ID` with the series record's 32-character identifier. It differs from
job and native transfer identifiers. `--tmdb-id` is optional when title/year
resolve unambiguously; supply it when the catalog has several matches.
`--season N` restricts a record to that regular season. An omitted or zero season
means all seasons. `--episode N` can narrow the selected scope further.

New records monitor all known dated regular episodes by default.
`--unmonitored` (API `enabled: false`) retains a new plan without queuing aired
episodes; the browser offers the same choice before explicit pack acquisition.
`--future-only` sets the
earliest monitored air date to today's UTC date, including episodes dated today.
`--include-specials` opts into catalog season zero. Tracking the same resolved
series and scope reuses the existing record and preserves its settings rather
than reenabling an explicitly disabled record. Settings changes mark a record
due for the next background pass; `series-refresh` checks it immediately.

`submit --kind series` and Plex show watchlist entries also create durable
monitoring records. The existing submit API still returns an episode-job array,
which can be empty when nothing has aired. `track-series` returns the series
record and a `submitted` count instead. Only `submit` retains its existing offline
management fallback; the dedicated series/calendar mutation commands require a
running service. `series-numbering` also supports read-only offline preview; its
apply action requires the service. See [numbering](numbering.md).

The browser has **Series** and **Calendar** pages. Track a series, inspect its
catalog plan, change monitoring/specials/earliest-date settings, exclude or
include individual episodes, and refresh the catalog. Bulk monitor/unmonitor
uses the existing 32-entry limit and reports an independent outcome per entry.
Episode rows show unknown dates and missing catalog identities explicitly.
See [browser management](web.md) for sign-in and form protections.

## Acquisition rules

Automatic submission requires all of the following:

- The series record is monitored and the episode is not explicitly excluded.
- A special has its series-level `include_specials` option enabled.
- The catalog provides a valid air date on or after the configured earliest
  monitored date, and on or before today's UTC date.
- The catalog episode has a stable nonzero TMDB episode identifier, and its
  season/episode numbering is unambiguous in the accepted plan.
- No request already represents the same resolved series/season/episode.

The date is a catalog day interpreted against UTC. It is not an exact premiere
time or a guarantee that a release is already available. A future episode never
queues early. Missing or invalid air dates become `null` and remain unresolved.
An episode without a catalog identifier stays visible but requires mapping
before automatic acquisition; its dated calendar row uses `mapping_required`.

Deduplication checks every existing request state, including ready, failed and
cancelled jobs. An excluded or disabled record does not cancel existing work.
Enabling monitoring does not retry a failed job or reopen a cancelled one; use
the existing job controls deliberately. Overlapping all-series/season records
can show the same episode in both calendar scopes, while acquisition deduplicates
their shared media identity. Their monitoring settings are independent.

Removing a show from Plex's watchlist does not unmonitor the retained series
record. Use Series settings or `series-unmonitor` to stop future automatic
requests. These controls retain requests, downloads and library imports. Series
monitoring controls missing episodes; the separate `monitoring.enabled` setting
controls upgrades of existing owned imports. Neither replaces the other.

## Refresh, failure and persistence

Optional `"series_packs": { "enabled": true }` tries automatically mapped
season packs before queuing individual episodes during monitored tracking and
refresh. It defaults to false, including for older configurations. The combined
batch remains bounded to 64 submissions; at most four seasons are considered
within the existing shared catalog/search deadline. Packs exceeding the remaining
allowance are skipped, and unresolved candidates fall back to individual jobs.
Unmonitored scopes perform no background pack lookup. An on-demand
`series-pack-search` preview/apply can use those scopes without enabling monitoring.
See [automatic packs](automatic-packs.md) for strict identity rules and guards.

With the catalog enabled, the service checks due monitored records every minute.
A pass refreshes at most four records, oldest due time first, within a shared
90-second HTTP/processing budget. A successful record is due again in one hour.
Each acquisition batch records at most 64 missing aired episodes; remaining
backlog schedules the record for another pass after one minute. A failed catalog
refresh retains the previous plan and schedules retry after five minutes.
Manual refresh uses its own 90-second budget and may inspect a disabled record;
it still respects the disabled acquisition policy.

Series refresh bypasses the legacy one-hour catalog response cache. Individual
HTTP requests have at most 20 seconds, reduced to the remaining pass budget.
Standard-library synchronous DNS can exceed that budget, so it is not a strict
wall-clock guarantee; late results are rejected. Only one refresh pass runs at
a time. Fresh network responses do not hold the request or series storage lock.
Changing settings while a refresh is running invalidates its captured revision;
the late result cannot enqueue episodes under the earlier policy.

Duplicate numbering, reused catalog identifiers, a changed known identifier at
the same number, or a known identifier moving to a different number reject a
refresh. The last accepted plan remains available and shows an error. These
checks compare against the retained plan, not a complete historical numbering
archive. This release has no manual alternate-number mapping editor. Correcting
the catalog can unblock the normal mapping; explicit alternate/anime mappings
are a following stage. URLs and credentials are redacted from public diagnostics.
Background failures appear in `status.last_series_error`; each catalog failure
also persists on its series record.

`series.json` is a private SHA-256-verified snapshot in `store_dir`, under the
request store's single-owner directory lock. Writes use a private temporary
file, file synchronization, atomic rename and directory synchronization before
confirming the new in-memory state. Corruption, unsupported schemas and linked
snapshots are rejected. A directory-sync failure blocks further series changes
until reopening storage. A checksum detects corruption; it is not authentication
against someone who can rewrite the file and checksum.

Series and request persistence are separate commits, not one transaction across
both files. A crash or capacity failure can leave a confirmed partial episode
batch. Already recorded jobs remain; subsequent refresh deduplicates them before
continuing. Retain both stores in backups. Temporary files left by interruption
are ignored as snapshots. Existing installations with no series snapshot open
with no tracked records; earlier jobs are not silently adopted as series records.
Read-only engine views load existing data without creating a snapshot or changing
its permissions, and reject series mutations.

## API and calendar

All API operations require the existing Bearer token:

| Method and route | Body / result |
| --- | --- |
| `GET /api/series` | Array of bounded summaries, including episode/undated/unmapped counts |
| `POST /api/series` | `{ "request": { "kind": "series", "title": "Example Series", "year": 2026, "tmdb_id": 123 }, "include_specials": false, "future_only": false }`; returns record and submission count |
| `GET /api/series/ID` | Record, accepted episode plan and effective monitoring choices |
| `POST /api/series/ID/monitor` | Any of `enabled`, `include_specials` and `start_date`; date is `YYYY-MM-DD`, or `null` to clear it |
| `POST /api/series/ID/episodes` | `{ "season": 1, "episode": 2, "enabled": false }`; episode must exist in the plan |
| `POST /api/series/ID/packs` | Explicit torrent source and 1–64 file-to-episode mappings; see [pack acquisition](packs.md) |
| `POST /api/series/ID/pack-search` | Season-pack preview or guarded apply; see [automatic packs](automatic-packs.md) |
| `POST /api/series/ID/refresh` | Empty body or `{}`; fresh catalog check and bounded submission |
| `POST /api/series/ID/numbering` | Read-only comparison or guarded explicit numbering apply; see [numbering](numbering.md) |
| `GET /api/calendar` | Dated episodes with monitoring choices and matching job identifiers/states |

Unknown fields, invalid types and invalid scopes are rejected before mutation.
Calendar query fields are `from`, `to`, `series_id`, `offset` and `limit`, each at
most once. They use literal ASCII dates/identifiers/numbers, without percent
encoding. Defaults are today through 30 days later, offset zero and limit 50.
The date window is inclusive, ordered and at most 367 days; dates support years
1800–9999. Limit is 1–200 and offset is 0–20,000. Unknown valid series IDs return
an empty calendar. A read changes no monitoring or request state.

Rows sort by date, series-record ID, season and episode. An existing job supplies
its state, preferring ready when several versions share the media identity.
Otherwise the state is `mapping_required`, `unmonitored`, `missing` for an aired
monitored episode, or `scheduled` for a future monitored episode. Undated episodes
remain in series details rather than appearing under an invented calendar day.

Mynou does not export calendar files or feeds. The unreleased iCalendar download
was withdrawn by [Issue116](https://github.com/alecerf/mynou/issues/116): after
sign-in, `GET /ui/calendar.ics` answers `410 Gone` with a short explanation
instead of a file. It ignores the former `from`, `to` and `series_id` fields and
reads or changes no series, request or library data. Use the Calendar page,
`mynou calendar` or `GET /api/calendar` for known episode dates.

## Bounds and next stage

Storage accepts at most 128 tracked scopes, 2,000 episodes in one plan, 20,000
episodes across records and an 8 MiB serialized snapshot. Catalog planning accepts
at most 100 selected seasons and 1,000 episodes per season. Input/catalog limits
can fail before storage limits. There is no automatic record eviction or deletion.

Episode requests search individual numbered episodes by default. Explicit
[pack acquisition](packs.md) and [automatic packs](automatic-packs.md) import
selected verified files from a shared native transfer; new mapped transfers
acquire only selected interests and required boundary pieces. Explicit
[numbering choices](numbering.md) retain catalog identities and capture source
labels for future jobs. Multi-episode videos, automatic alternate/anime-order
inference, time-zone premiere scheduling, adoption of an existing
Plex library and multi-user request policies remain future work. See the
[roadmap](roadmap.md) for the following releases and [limits](limits.md) for the
rest of the supported surface.
