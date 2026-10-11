## Added: Connect any JSON indexer with a mapping

A `json` source can now carry a `mapping` that tells Mynou where results, titles, download addresses and seeders are in the indexer's response, which parameter names to send (or leave out), the category to use for movies and episodes, and fixed parameters such as a page size. Indexers that offer Torznab, RSS or a JSON search API can be connected from `mynou.json` alone, without provider-specific code. Sources without a `mapping` behave as before, and unsupported cases (HTML pages, several result pages, header API keys) are listed in [the guide](/docs/sources.md#connecting-any-indexer).

## Changed: Track any number of series

Series, episodes and numbering are now stored in an indexed on-disk store instead of one in-memory snapshot, so the 128-series, 20,000-episode and 8 MiB limits are gone; each plan still holds at most 2,000 episodes. The first start with write access migrates `series.json` after validating it and keeps an exact copy as `series.legacy-*.json`; older versions refuse the migrated store, so downgrading means restoring that copy ([storage notes](/docs/inventory-storage.md#downgrade-and-recovery)). `mynou series`, `GET /api/series` and the web interface read the list in pages (`--limit`/`--after`, load more), and the calendar adds an `after` cursor ([guide](/docs/series.md)).

## Added: Manually cross-seed verified existing downloads

Use `cross-seed SOURCE_ID --torrent FILE` to seed a distinct v1 torrent from completed native files after full verification, without copying or downloading them. Enable seeding first; optional file mapping and the initial format limits are explained in [the transfer guide](/docs/transfers.md#manual-cross-seeding).
