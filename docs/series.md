# Series, calendar and episode numbering

Follow a series once and Mynou keeps its TMDB episode plan. While the service
runs, it requests newly aired episodes as ordinary episode jobs, which use the
usual selection, download, import and Plex confirmation. Future and unresolved
episodes stay in the plan and in the calendar.

Before tracking a series, enable the catalog (`catalog.enabled` with a TMDB
token or API key) and configure your sources and episode profile. Plex
watchlists can create series records, but Plex is optional for manual tracking.

## Track and manage

These commands need the running service:

```sh
mynou track-series --title "Example Series" --year 2026 --tmdb-id 123
mynou series
mynou series --limit 50 --after ID
mynou series ID
mynou series-unmonitor ID
mynou series-monitor ID
mynou episode-unmonitor ID --season 1 --episode 2
mynou episode-monitor ID --season 1 --episode 2
mynou series-refresh ID
mynou calendar --from 2026-10-01 --to 2026-10-31 --series-id ID
```

`mynou series` prints one page object, `{"series": [...], "next": ID or null,
"total": N}`: 50 records by default, or `--limit` (1 to 200) after the record `--after
ID`. Pass `next` as `--after` for the following page. There is no limit on how many
series you track: records are stored on disk, not in one in-memory snapshot (see
[storage](inventory-storage.md#series-storage)). The web interface shows the first
50 and loads more on request.

`ID` is the series record's 32-character identifier, distinct from job and
transfer IDs. `--tmdb-id` is optional when the title and year identify one
series. `--season N` limits the record to one regular season (omitted or `0`
means all seasons) and `--episode N` narrows it further.

New records monitor all known dated regular episodes. Options:

- `--unmonitored` keeps the plan without requesting anything, for example
  before choosing [numbering](#episode-numbering) or a
  [season pack](packs.md).
- `--future-only` starts monitoring at today's UTC date, including today.
- `--include-specials` adds catalog season zero.

Tracking the same series and scope again reuses the existing record and keeps
its settings; it does not re-enable a record you disabled. A settings change
makes the record due for the next background pass; `series-refresh` checks it
immediately. `submit --kind series` and Plex show watchlist entries also create
series records. Removing a show from the Plex watchlist does not stop its
monitoring: unmonitor it explicitly. Monitoring controls never cancel requests
or delete files.

## Which episodes are requested

An episode is requested automatically only when:

- the record is monitored and the episode is not excluded;
- a special (season zero) has `include_specials` enabled;
- its catalog air date is on or after the earliest monitored date and on or
  before today's UTC date;
- it has a stable TMDB episode ID and unambiguous numbering in the plan;
- no request for the same series, season and episode exists in any state,
  including failed and cancelled.

Air dates are catalog days compared with UTC, not premiere times, and they do
not mean a release exists yet. Episodes without a date stay unresolved; dated
episodes without a catalog ID show `mapping_required`. Enabling monitoring
never retries a failed job or reopens a cancelled one: use the job controls.
Overlapping records (a whole series and one of its seasons) can both show an
episode, but only one request is created. This is separate from library
[upgrade monitoring](library.md#background-monitoring).

Set `"series_packs": {"enabled": true}` to try a season pack before individual
episodes during monitored tracking and refresh; see
[automatic packs](packs.md#prefer-packs-during-monitoring).

## Refresh and failures

With the catalog enabled, the service checks due records every minute and
refreshes at most four per pass, oldest first. Due records come from an index
ordered by due time, so a pass costs the same with ten series or ten thousand, and
a record that was just checked goes behind every older one: no record starves.
At four per minute, at most 240 records are checked per hour: with more than 240
monitored records a record waits longer than its hour, oldest first, and new
episodes are noticed later; the pass limit is not configurable. A record whose
stored data cannot be read is skipped for an hour, counted in
`status.series_unreadable` and named in `status.last_series_error`; it never holds
back the records behind it. A successful record is due again
after one hour; a remaining backlog brings it back after one minute; a failed
catalog refresh keeps the previous plan and retries after five minutes. A
manual refresh can inspect a disabled record but still respects its policy.
Changing settings during a refresh discards that refresh's result, so a late
result never requests episodes under an earlier policy.

A refresh is rejected, and the last accepted plan kept with an error, when the
catalog shows duplicate numbering, reuses an episode ID, puts a different ID at
a known number, or moves a known ID to another number without an explicit
[numbering decision](#episode-numbering).
Background failures appear in `status.last_series_error` and on the series
record. Diagnostics never include URLs or credentials.

## Calendar and API

| Route | Purpose |
| --- | --- |
| `GET /api/series` | Summaries with episode, undated and unmapped counts, always as `{"series", "next", "total"}`: 50 by default, `?limit=N` (1 to 200) and `after=ID` choose the page |
| `POST /api/series` | Track: `{"request": {"kind": "series", "title": "Example Series", "year": 2026, "tmdb_id": 123}, "include_specials": false, "future_only": false}`; add `"enabled": false` to keep it unmonitored |
| `GET /api/series/ID` | Record, accepted plan and monitoring choices |
| `POST /api/series/ID/monitor` | Any of `enabled`, `include_specials` and `start_date` (`YYYY-MM-DD` or `null`) |
| `POST /api/series/ID/episodes` | `{"season": 1, "episode": 2, "enabled": false}` for an episode in the plan |
| `POST /api/series/ID/refresh` | Empty body or `{}`: fresh catalog check and bounded submission |
| `POST /api/series/ID/numbering` | Numbering preview or apply (below) |
| `POST /api/series/ID/search`, `/grab` | Search an episode's releases and choose one by hand; see [selection](selection.md#choose-a-release-by-hand) |
| `POST /api/series/ID/packs`, `/pack-search`, `/shared-file` | [Season packs and shared videos](packs.md) |
| `GET /api/calendar` | Dated episodes with monitoring choices and matching jobs |

Calendar queries accept `from`, `to`, `series_id`, `offset`, `after` and `limit`,
each at most once, as literal ASCII values. The default window is today through 30 days
later, with offset 0 and limit 50. Rows are sorted by date, record, season and
episode. A row's state comes from an existing job (ready first when several
versions exist), otherwise `mapping_required`, `unmonitored`, `missing` (aired
and monitored) or `scheduled` (future and monitored). Undated episodes appear in
series details, never on an invented day. Reading the calendar changes nothing.

A calendar page reports `next`, a cursor to pass as `after` (not together with
`offset`) for the following rows; unlike `offset`, which reaches at most row
20,000, it reaches every row of a window however many series are tracked. `total`
is the exact row count for windows of at most 20,000 rows and `null` for larger
ones.

## Storage and upgrading

Series, episodes and numbering are stored in `series.idx` inside the store
directory. The first start with write access after upgrading from 0.23 or
earlier migrates `series.json`: it is validated, copied unchanged to
`series.legacy-<digest>.json`, rebuilt into the new store, verified and then
replaced by a marker that older versions refuse. An interrupted migration resumes
on the next start. Offline commands that only read storage do not migrate; they
report that the store must be migrated first. To go back to an older version,
restore the preserved copy as described in
[the storage notes](inventory-storage.md#downgrade-and-recovery); changes made
after the migration are not in it.

## Episode numbering

Each episode has three labels: the library number Mynou keeps, the current
catalog number and the number your source uses. Mynou ties them to the
episode's catalog ID, so library numbers, request keys, exclusions and import
paths stay fixed when the catalog or source numbering changes.

For example, catalog ID `11001` first belongs to library `S01E01`. If the
catalog moves it to `S01E10` and your source calls it `013`, a numbering
decision can keep `S01E01`, accept catalog `S01E10` and search source absolute
`13`. New requests import as `S01E01`; existing requests keep the source label
captured when they were created, through retries and upgrades.

Preview the current comparison (this works offline, read-only):

```sh
mynou series-numbering SERIES_ID
```

The report shows each known ID's `canonical`, `catalog` and `source` labels,
`issues`, `resolved` and a `plan_id`. To change labels, write `numbering.json`
with only a `changes` array:

```json
{
  "changes": [
    {
      "catalog_id": 11001,
      "catalog": {"season": 1, "episode": 10},
      "source": {"absolute": 13}
    },
    {
      "catalog_id": 11002,
      "catalog": {"season": 1, "episode": 11},
      "source": {"season": 2, "episode": 3}
    }
  ]
}
```

`catalog` must match the episode's current catalog labels. `source` is either
`season` plus `episode` or `absolute` alone, and must not duplicate another
episode's label. Unchanged episodes keep their accepted values, by default the
original library number. To restore a source label, choose the original
season and episode explicitly. Then preview and apply with a running service:

```sh
mynou series-numbering SERIES_ID --mapping numbering.json
mynou series-numbering SERIES_ID --mapping numbering.json --apply \
  --plan-id PLAN_ID_FROM_PREVIEW
```

Apply fetches the catalog again and refuses changed metadata, settings or
choices; unresolved proposals are never saved. It records labels and the
accepted plan but creates no requests: monitoring or a refresh then requests
aired episodes as usual. The API equivalent is
`POST /api/series/SERIES_ID/numbering` with `changes`, plus `"apply": true` and
`"plan_id"` to apply.

With a saved source label, searches send that season and episode (JSON and
Torznab), or add the padded absolute number to the search term (JSON also
receives `absolute`; Torznab then omits season and episode). Release titles must
contain exactly that single `SxxExx` or `NxNN` label, or for absolute labels a
single number right after the series title and optional year, such as
`Example Series 013 1080p`. Absolute titles without a saved choice, chained
labels, ranges and conflicting labels do not match. In a multi-file torrent,
exactly one file name must match the saved label. `search` and `submit` also
accept `--source-numbering JSON` (API `source_numbering`) for one explicit
episode request without changing a series.

Imports and Plex confirmation use the library numbers: configure Plex's episode
order to match them. A replacement catalog ID can never take a retained number,
even after the earlier ID disappears.
