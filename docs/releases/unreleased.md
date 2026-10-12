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
