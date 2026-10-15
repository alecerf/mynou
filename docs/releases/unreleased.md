## Added: Connect any JSON indexer with a mapping

A `json` source can now carry a `mapping` that tells Mynou where results, titles, download addresses and seeders are in the indexer's response, which parameter names to send (or leave out), the category to use for movies and episodes, and fixed parameters such as a page size. Indexers that offer Torznab, RSS or a JSON search API can be connected from `mynou.json` alone, without provider-specific code. Sources without a `mapping` behave as before, and unsupported cases (HTML pages, several result pages, header API keys) are listed in [the guide](/docs/sources.md#connecting-any-indexer).

## Changed: Track any number of series

Series, episodes and numbering are now stored in an indexed on-disk store instead of one in-memory snapshot, so the 128-series, 20,000-episode and 8 MiB limits are gone; each plan still holds at most 2,000 episodes. The first start with write access migrates `series.json` after validating it and keeps an exact copy as `series.legacy-*.json`; older versions refuse the migrated store, so downgrading means restoring that copy ([storage notes](/docs/inventory-storage.md#downgrade-and-recovery)). `mynou series`, `GET /api/series` and the web interface read the list in pages (`--limit`/`--after`, load more), and the calendar adds an `after` cursor ([guide](/docs/series.md)).

## Added: Manually cross-seed verified existing downloads

Use `cross-seed SOURCE_ID --torrent FILE` to seed a distinct v1 torrent from completed native files after full verification, without copying or downloading them. Enable seeding first; optional file mapping and the initial format limits are explained in [the transfer guide](/docs/transfers.md#manual-cross-seeding).

## Changed: Store jobs and history on disk without a total job limit

Jobs and library listings now use cursor pages (`--limit`/`--after`, API `next`, and load more in the web interface). The first writable start migrates and preserves the old job files; history keeps up to 1,000 events per job. Earlier binaries refuse the new format, and restoring legacy backups loses changes made afterward; read [the storage guide](/docs/inventory-storage.md#job-storage) before downgrading.

## Changed: Store requester demand without a total inventory limit

Requester demands and operator interests now use an indexed on-disk ledger instead of the 10,000-record, 16 MiB aggregate snapshot. Use `mynou requester ACCOUNT --limit 200 --after CURSOR` or the account API’s `next` cursor to read further pages; polling batches remain bounded. The first writable start preserves legacy bytes before migration; older binaries refuse the new ledger and restoring the backup loses later changes ([storage guide](/docs/inventory-storage.md#requester-storage)).

## Changed: Schedule library checks without scanning the inventory

Library checks now read a bounded oldest-check window and report inventory counts and window truncation. The first writable start resumes an indexed scheduling upgrade after interruption; earlier binaries reject its new format, so keep a stopped-store backup before upgrading if downgrade may be needed ([storage guide](/docs/inventory-storage.md#indexed-library-monitoring)).

## Changed: Retain feed state without a total source limit

Feed history now uses an on-disk index instead of the 1,000-source, 4 MiB snapshot. Use `mynou feeds --retained --limit 200 --after CURSOR` to page through retained sources; configured watching remains limited to 64 sources. The first writable start preserves the old snapshot; downgrading requires restoring that copy and loses later observations ([storage guide](/docs/inventory-storage.md#feed-storage)).

## Changed: Faster library updates for repeatedly upgraded media

Job storage gains an index of ready upgrade children, so recording a library version no longer reads every earlier upgrade attempt of its parent. The first writable start upgrades the job index in resumable pages; earlier binaries refuse it, so keep a stopped-store backup before downgrading ([storage guide](/docs/inventory-storage.md#job-storage)).

## Changed: Resume feed routing across the complete inventory

Watched announcements are now captured privately before their identities are marked seen, and routing resumes in bounded pages after restart. Pending routing delays the next source poll; pausing a source cancels its captured work. Existing feed indexes upgrade on the first writable start; keep a complete stopped-store backup for downgrade ([storage guide](/docs/inventory-storage.md#captured-feed-routing)).

## Changed: Retained indexer source policies

Removed the fixed limits on retained source policies, including removed sources.
Existing policy migrates automatically on writable startup; see
[storage and downgrade](/docs/inventory-storage.md#source-policy-storage).


## Changed: Retain IRC announcements without a total history limit

IRC history now lives in an on-disk index, removing the 1,000-identity / 8 MiB limits while retaining duplicate suppression across restart. Use `mynou announcements --limit 200 --after CURSOR` or the API’s `next_cursor` to page through the history. The first writable start preserves and migrates the old snapshot; older binaries reject the new marker, so downgrade requires a consistent stopped-store backup ([storage guide](/docs/inventory-storage.md#irc-announcement-storage)).

## Changed: Read Plex watchlists and libraries of any size

Watchlists are now read in resumable sweeps of 200-entry pages, up to eight pages per account in each pass, so the 512-entry requester watchlist limit and the 100,000-item and 1,000-page Plex limits are gone. A sweep resumes after errors and restarts, and entries that left a requester watchlist are removed only after a sweep that saw the whole list unchanged; `GET /api/requesters` shows the progress of an unfinished sweep ([guide](/docs/requesters.md#how-demand-is-handled)).

## Changed: Page search previews without a candidate limit

Search previews no longer stop at 1,000 rows or fail beyond 16 MiB of candidates: candidates are assessed as each source answers, and the winner is still chosen from all of them. `mynou search` and `POST /api/search` return pages of 100 candidates (`--limit` up to 200) with a `next` cursor for `--after`, replacing `truncated`; the web interface loads more on demand ([guide](/docs/selection.md#preview-a-search)).

## Fixed: IRC routing no longer stalls behind failing announcements

Up to 200 announcements that kept failing to route (for example with no waiting job) could pause IRC routing for everyone until their 30-second backoff ended. Eligible announcements are now routed regardless, and `GET /api/irc` reports how many backoffs are held, the limit and how many were shortened under `routing` ([limits](/docs/limits.md)).
