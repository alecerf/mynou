# Inventory storage

The `mynou::inventory::Index` Rust API is the storage foundation for
[growing inventories](https://github.com/alecerf/mynou/issues/106). Jobs and
history, series and numbering, requester demand, retained feeds, source policies,
IRC announcements and native transfers use private disk indexes. Library tips
and their monitoring entries share the job index. Legacy snapshots remain only
as migration inputs or preserved recovery copies, not as current inventory caps.

| Store | Recovery and downgrade guidance |
| --- | --- |
| Jobs, history and library tips | [Job migration and downgrade](#job-migration-and-downgrade), [index upgrades](#indexed-library-monitoring) |
| Series, episodes and numbering | [Downgrade and recovery](#downgrade-and-recovery) |
| Requester demand and operator interests | [Requester migration and downgrade](#requester-migration-and-downgrade) |
| Retained feeds and captured routing | [Feed storage](#feed-storage), [captured feed routing](#captured-feed-routing) |
| Source policies | [Source policy storage](#source-policy-storage) |
| IRC announcements | [IRC announcement storage](#irc-announcement-storage) |
| Native transfers | [Transfer storage migration and downgrade](transfers.md#storage-migration-and-downgrade) |

Per-record, page, transaction and configured-source bounds still apply; see
[limits](limits.md). Removing total inventory caps does not establish scalability:
[remaining delivery](#remaining-delivery) tracks diagnostics and measured evidence.

## Ownership and format

`Index::create(path)` requires a new directory whose parent already exists.
It never overwrites an existing directory. `Index::open(path, read_only)`
accepts this format only, creates nothing and changes no permissions. The API
holds an exclusive lock on `.lock` for its lifetime, including read-only use.
Creation sets directories to 0700 and files to 0600. Opening requires private
directories and regular private files, rejects symbolic links and hard-linked
state files, and checks an opened file against the inspected inode. This is
the existing private-directory trust model, not protection against an actor
who can rewrite the owner's directory while it is in use.

Open policy: private store files and locks are opened with
`O_NOFOLLOW | O_NONBLOCK`, named per target in `src/store.rs` (macOS, and Linux
on x86-64 and arm64; other targets do not build). This covers index files, WALs,
record files, value-log segments, staging locks, legacy snapshots and their
preserved copies in the job, series, requester, feed, source-policy and IRC
stores and their migrations, and the native transfer state (ownership and
creation locks and the persisted `.torrent`, `.source`, `.control`, `.paused`
and `.seed` files). If the final path component is swapped for a symbolic link
after inspection, the open itself refuses it, so the target is never read. A
FIFO or device returns at once instead of blocking, and the descriptor is
refused unless it is the inspected regular file (same device and inode). A
missing job store lock is created exclusively, and a leftover legacy journal is
checked against its preserved copy again through the descriptor that empties it.
Directory synchronization opens without blocking and refuses non-directories.
`O_NOFOLLOW` covers only the final component. A parent directory swapped for a
symbolic link between inspection and open is still followed, which stays within
the private-directory trust model above. The link-count and permission checks
remain as defense in depth.

`identity.bin` holds a random 256-bit store identity and checksum. `tree.bin`
contains 4 KiB checksummed pages. Page zero holds the identity, sequence,
root, allocation counter and record count; remaining pages hold ordered keys
and either value references or child references. Nodes have at most 32 items;
child levels strictly decrease and child upper keys must match their parents.
Offsets/counts use checked arithmetic, supported offsets fit a signed 64-bit
file offset, and there is no configured total inventory cap.

Keys are 64-byte ordered binary values. Callers must define table/range
prefixes and identity encodings. Values have at most 1 MiB. Each value is
written in an envelope that binds the key, length and checksum, inside the
value log described below. Stores written before the value log keep their
values as one file per key under `objects/XX/REMAINING_HEX`; those files stay
readable, and replacing or deleting such a key removes its file. Reads never
list a directory. Checksums detect corruption, not malicious rewrites by
somebody who can replace state and its checksums.

## Exact range summaries

`inventory::Index::range_summary(start, end)` returns an exact key count and
first key for `[start, end)`; `None` means no upper bound. An equal start/end
is empty; a reversed range is refused. Series and library monitoring use this
primitive for #246; other stores' due-work definitions, last-pass timestamps
and status exposure remain pending. A tree entry can be a secondary key, so its count is not
necessarily a count of jobs, episodes or transfers.

New branch pages (`MYNOUIN2`) carry an optional checked cardinality beside
each child reference; leaves retain `MYNOUIN1`. Inserts, replacements,
splits, deletions and root collapse maintain known cardinalities in the same
WAL transaction as the records. The range read subtracts two ranks and reads
the first key, at most three root-to-leaf paths with at most 32 links per node.
It does not read values, enumerate files, or scan all the rows of the range.
Counts describe index keys; they do not verify the contents of record values.

Old branches remain readable by point/page APIs, with unknown counts kept
explicit. Reading a range that needs unknown counts returns an upgrade error
instead of a partial count or a scan. Opening an index does not migrate it.
An ordinary write can fill counts along the path it already changes; completing
legacy counts is an explicit `prepare_range_counts(budget)` operation used by
writable series and job-store startup. There is no separate CLI migration command.

The caller repeats this operation until it returns `true`, with a branch-page
budget of 1–256. Each batch stages at most budget plus tree height branch pages;
child visits and redo validation cost `O(32 * (budget + height))`. Completed
subtree totals and partial parents persist, so a restart resumes rather than
rescanning completed subtrees. The operation writes metadata-only WAL batches,
keeps the root allocation, records and value-log bytes unchanged, and advances
the sequence. Redo validates that existing keys/pointers are unchanged and that
new counts match their child nodes. An uncertain write pauses reads and writes
until writable recovery, as for ordinary transactions. Read-only callers never
upgrade. A completed upgrade is a no-op when repeated.

**Downgrade:** earlier binaries reject the new branch-page magic (including
inside a pending WAL); they must not write a partially or fully upgraded store.
Keep a complete stopped-store backup before a new binary writes to an index or
before explicitly upgrading counts. Downgrading requires stopping Mynou and
restoring that consistent backup, which loses later changes. This increment
does not touch live stores or existing migration backups.

Regression tests compare ranges to independent ordered collections through
growth, unordered churn, replacement, deletion, root collapse and restart;
check equal timestamps and cold-page read bounds; and cover explicit legacy
upgrade, restart between batches, read-only preservation, forged metadata redo,
interruption and storage exhaustion at WAL, directory and tree stages. These
synthetic checks establish correctness and operation bounds, not the multi-store
macOS RSS/timing evidence still required by #217.

## Value log

On Apple platforms every durability flush is an `F_FULLFSYNC`, which flushes
the whole drive cache and costs milliseconds; std offers no cheaper flush. One
file per value therefore made a commit's cost grow with its record count. Each
commit now writes all of its values into a single segment,
`segments/<16 hex digits>`, numbered by the header's `head` counter. A segment
starts with a magic, the store identity, its number and their SHA-256, followed
by the envelopes in transaction order. The tree leaf keeps the segment number,
the envelope offset and the first 20 bytes of the value's SHA-256 in the slot a
record file used for its full digest, flagged in the length field, so page
layout is unchanged. A read checks the segment header, the envelope checksum,
its key and length, and the digest prefix. Segments are never rewritten after
publication, so a location names exactly one envelope.

The header also records the oldest segment still in use (`tail`), how far the
cleaner has read into it, the bytes of all segments and the bytes of envelopes
still referenced. Once the segments exceed twice the referenced bytes plus
8 MiB, each commit targets 4 MiB (or as much as it appends) from the
oldest segments and rewrites every still-referenced envelope into its own
segment, as an ordinary replacement in the same transaction. The cleaner stops
before the commit would approach its change, page or WAL bounds, or retire
more than 256 segments. A later commit resumes cleaning; corrupt or unsafe
segment files are explicit errors. A segment read to its end leaves the log; its
file is deleted after the root that no longer references it is durable.

An ordinary non-inline commit flushes the WAL, the store directory, its segment
and the `segments` directory, the tree, and the store directory again when it
retires the WAL: six flushes after the segment directory exists. The first
non-inline commit also flushes the parent when it creates that directory, for
seven flushes. Inline and deletion-only commits need fewer flushes. These costs
do not grow with the record count. Deleting legacy record files adds a flush
per affected object directory, and retiring cleaned segments adds one.
`inventory::flush_tests` enforces each commit's bound and reports the initial,
steady and mean costs separately.

Existing stores need no migration: their record files are read in place and
leave as their keys change. Releases before the value log refuse a store once
any commit has written a segment, reporting invalid header padding. To keep a
downgrade path, make a complete stopped-store backup before upgrading;
restoring it loses changes made afterwards.

The API validates the root at open and each referenced page/value on access;
opening does not audit the entire inventory. Corruption on an untouched path
is reported when that path is read, never returned as an empty/absent result.

## Transaction and recovery

One transaction contains at most 16,384 distinct key changes, 16,384 dirty pages
and 128 MiB of WAL payload. These are operation bounds, not inventory bounds.
Primary and secondary keys can change together. Preparation verifies existing
records, stages tree changes in memory and validates the resulting key changes.

1. Create `transaction.tmp`, write the complete checksummed redo and synchronize
   the file. This unpublished staging artifact has never applied any data.
2. Rename it to `transaction.bin` and synchronize the store directory. Only
   after this publication is durable may any record or tree page change.
3. Write the transaction's segment under a private temporary name, synchronize
   it, rename it to its number and synchronize `segments`. Remove legacy record
   files of replaced or deleted keys and synchronize their directories (a WAL
   from before the value log instead writes and synchronizes one record file
   per put, and synchronizes `objects` when it created a directory). Apply page
   images and the root, then synchronize the tree. Delete segments the cleaner
   retired and synchronize `segments`.
4. Remove the published WAL and synchronize the store directory before
   acknowledging or preparing another transaction.

Any error after staging begins blocks **all reads and writes** until reopen.
The caller must inspect recovered state rather than automatically retrying:
a failed commit response can mean a committed transaction. Validation failures
before staging leave the existing store usable and unchanged.

Writable open reads and validates the entire bounded published WAL before any
repair. Its store identity, old/new metadata, sequence, page images, actions,
segment locations and resulting key references must agree. Replay rebuilds
the same segment bytes from the WAL, so a torn or missing segment of the
interrupted transaction is rewritten. A valid redo may replace torn root,
page or object content only for the images/keys it covers. Links, wrong types
and public permissions remain errors. Existing segment directories, publication
destinations and retirement candidates are checked before any replay mutation;
inline-only commits also check an existing segment directory. Retirement is
bounded to 256 segments both for normal commits and published redo.
Complete corruption, truncated published
WALs, cross-store WALs and stale roots are errors, with no repair. Replaying a
valid transaction is idempotent. Only unpublished staging may be discarded.

Read-only open never repairs, creates, deletes or changes permissions. A pending
transaction or unpublished staging reports that writable recovery is required.
An interrupted initial creation is an explicit error, not a reason to initialize
over an existing directory. Legacy journal/snapshot directories are never
accepted as empty instances of this format.

## Storage diagnostics and errors

`mynou status` and `GET /api/status` include `storage`, one entry per index:
`jobs`, `series`, `requesters`, `announcements`, `sources`, `feeds` and
`transfers`. Each entry has a `state` (`index`, `empty`, `needs_migration`,
`legacy`, `migrating`, `not_open`, or `busy` when a long operation holds the
store, so the status request never waits for it) and, for an open index:

- `entries`: index entries, including the store's own secondary keys, so it
  is larger than the number of jobs or series shown elsewhere in the status;
- `index_bytes` and `log_bytes`: bytes of the tree file and of the value-log
  segments, which include garbage the cleaner has not yet reclaimed, and
  `live_bytes`, the value-log bytes still in use;
- `sequence`, the commit counter, and `commit_uncertain`, true after a failed
  commit until Mynou restarts and recovery finishes.

The job and series entries add `migrated_at_start`. These readings come from
each index root, maintained by every commit; producing them reads no record.
They are sizes, not limits: Mynou reports no maximum inventory size, and
available disk space remains the physical bound.

The status never waits for a busy store and never fails because one store
cannot be read. Readings from such a store (`jobs`, `active`, `ready`,
`monitored_series`, `torrents` and the transfer byte totals) are `null`; its
`storage` entry says `busy` or shows `commit_uncertain`, and `storage_errors`
lists the store-named error of each reading that failed.

Every failed commit names its store (for example `Job storage`) and says what
happened and what to do. The error carries no path, credential or media title.

- **Full disk or quota.** Before publication the change was not saved and
  nothing else changed: free disk space, then restart Mynou. After publication
  the outcome is uncertain: existing records and recovery files are kept, and
  the next start after freeing space finishes the change. A start that still
  cannot recover for lack of space says so and keeps the WAL.
- **Single record above 1 MiB**, and **a change above its operation bound**
  (16,384 entries, or 128 MiB of write-ahead log including the index pages it
  rewrites): the change was not saved, existing records are
  unchanged, and the store remains usable. Report the item that produced it.
- **Other write failures.** Before publication the change was not saved; after
  it the outcome is uncertain. Either way reads and writes of that store pause
  until restart, which finishes recovery; keep the data directory intact.

No error evicts records, falls back to another store or deletes recovery
files. Tests inject a full disk while writing the WAL, the value-log segment
or a legacy record file, the tree pages, and while synchronizing the store
directory; each store reopens with every committed record.

## Pagination and bounds

Queries use an inclusive start, exclusive end and exclusive last-key cursor.
The tree prunes preceding subtrees; deletions prune empty branches and collapse
unary roots. There are no sibling links or empty-leaf scans. A page emits at
most 200 rows and 4 MiB of value bytes; a byte-limited page resumes after its
last **emitted** key. `total` counts the whole index, not just the requested
range. `next` is absent when no further row is found.

Each page reports the current sequence. Passing that sequence on later pages
rejects intervening mutations and asks the caller to restart. Omitting it gives
a live key scan: insertions before the cursor need not appear. This is not MVCC
or a retained snapshot.

Memory and work depend on tree height, the bounded transaction and the requested
page, rather than retaining all record identities in RAM. The implementation
keeps at most 16,384 staged nodes plus at most one extra split path during
preparation. Overlay validation may retain two copies of the completed staged
map. Action/value buffers, WAL encoding/decoding buffers and response values
are also bounded. The height bound is 63; page allocation checks the file-offset
representation. These are source-level bounds, not measured RSS or throughput.

Unreachable old page slots and interrupted temporary object or segment files
are not reclaimed by this first increment; segment space is reclaimed by the
cleaner described under [Value log](#value-log). Repeated churn can therefore consume extra
disk space. Compaction/reclamation must be designed and verified before the
integration claims scalable routine operation. The index API itself has no
automatic cleanup or backup; migration and downgrade of series data are described
under [Series storage](#series-storage). A backup is a stopped-store copy,
including any recovery artifacts; a partial copy is not a backup.

## Listings and cursors

Every inventory listing returns a cursor page: pass the previous page's `next`
as `after` (CLI `--after`) and stop when `next` is null. `limit` is 1 to 200
(50 by default, 100 for search candidates and IRC history), and each page also
stops at its byte budget. Order is stable: a later write never moves rows of an
older page, so following `next` under concurrent writes visits each remaining
row once (rows written meanwhile before the cursor are not revisited). A
malformed cursor is rejected with an error. Jobs, library entries and series
are never deleted, so their cursors are also checked against the store: a
well-formed cursor naming no row (from another store, or altered) is rejected as
stale rather than silently restarting. Other cursors are positions: removing the
row they name does not invalidate them. `total` counts the whole listing when its
index keeps a counter.

| Listing | API / CLI | Order | Count |
| --- | --- | --- | --- |
| Jobs | `GET /api/jobs`, `mynou jobs` | newest first; `q`, `state` filters | `total` |
| Library | `GET /api/library`, `mynou library` | newest first; `q`, `state` filters | `total` |
| Series | `GET /api/series`, `mynou series` | identifier | `total` |
| Calendar | `GET /api/calendar`, `mynou calendar` | air date, at most 367 days | `total`, null when not cheap |
| Requester demand | `GET /api/requesters/ID`, `mynou requester` | ledger order | `total` |
| Retained feeds | `GET /api/feeds/retained`, `mynou feeds --retained` | identifier | `total` (string) |
| IRC history | `GET /api/irc/announcements`, `mynou announcements` | chronological | `total` |
| Transfers | `GET /api/transfers`, `mynou torrents` | priority, then FIFO | `total` |
| Requests of a transfer | `GET /api/transfers/ID` | identifier (`request_ids_next`) | — |
| Search candidates | `POST /api/search`, `mynou search` | live search ranking | — |

The requester and IRC history listings still accept a legacy `offset` (at most
10,000 and 1,000) for old callers; only `after` reaches every row. IRC history
also repeats `next` as `next_cursor`. Other lists are bounded by configuration
or retention and are returned whole: configured indexers (`GET /api/indexers`),
feeds (`GET /api/feeds`), IRC sources and rules (`GET /api/irc`) and requester
accounts (`GET /api/requesters`), and a job's retained history of at most 1,000
events (`GET /api/jobs/ID/events`). A library check (`POST /api/upgrades`)
examines a bounded oldest-first window and reports `limited` when more remain.

### Searching jobs and the library

`GET /api/jobs` and `GET /api/library` accept `q`, a percent-encoded text of at
most 128 characters matched case-insensitively against the title, release name,
state and identifier, and `state` (`queued`, `processing`, `downloading`,
`importing`, `ready`, `failed` or `cancelled`). On the command line they are
`--query TEXT` and `--state STATE`. A filtered request examines at most 1,000
rows after its cursor and returns the matches, `scanned` (rows examined) and
`next`, the cursor of the last examined row. A page can therefore hold fewer
matches than its limit, or none, while `next` continues the search: follow it
until it is null to search the whole inventory. The web interface searches this
way, reads a few windows at a time and offers "Search further". When jobs change
or the connection reopens, it reads the part already searched again, at most
once per second, so jobs entering or leaving the filter appear or disappear.

### Migration progress

Each migration of an earlier storage format, and each resumable job index
upgrade, reports its progress: `store` (`jobs`, `series`, `requesters`, `irc`,
`feeds`, `indexers`, `transfers`, or the job index upgrades `library_schedule`
and `upgrade_children`), `stage` (`validating`, `preserving`, `copying`,
`indexing`, `verifying`, `promoting`, `complete`, or `failed` when it stopped
with an error), `done` and `total` records (null when unknown in advance) and
`resumed` when it continued an interrupted attempt. Reports never contain paths,
titles or identifiers.

Commands that open storage with write access print progress on standard error
(each stage change, then about once a second). `mynou serve` binds the API
before opening storage: while it opens and migrates, `/healthz` answers
`starting`, `/readyz` answers 503, an authenticated `GET /api/status` (or
`mynou status`) returns `{"status":"starting","migrations":[...]}`, and every
other route, including the web interface and `/ws`, answers 401 without the API
token and 503 with it. Once storage is open, `GET /api/status` keeps the
`migrations` of this start. An interrupted migration resumes from its last
durable batch on the next writable start, which then reports `resumed`.

## Inline values and the page cache

A key stores either a logged value (up to 1 MiB, see [Value log](#value-log)) or
an inline value of at most 32 bytes in its tree leaf (`Change::PutInline`).
Inline values take no segment space, so secondary index entries that carry
little or no payload cost one leaf slot. A key may change between the two
kinds in one transaction, and recovery replays that idempotently. Inline lengths and zero
padding are validated like any other tree field.

Decoded, checksum-verified pages are cached (at most 2,048, about 8 MiB) and the
cache is cleared before every commit applies and on every open, so a cached page
never outlives the pages it was read from. A page that is corrupted on disk
after it was cached is reported on the next read after a reopen or commit, not
before; the checksum guards storage faults, not a hostile rewrite of an open
directory.

## Series storage

`series.idx/` in the store directory is a private index. `series.json` is
reduced to a schema 3 marker bound to that index; `series.legacy-<digest>.json`
is the exact snapshot a migration started from (see below).

A series scope is one header record (settings, schedule, exclusions and counters)
plus block records, one per season and 64 consecutive episode numbers, holding
the episodes, retained numbering identities and numbering choices of that range.
A record is bounded by the per-plan limits (2,000 episodes, identities, choices
and exclusions), never by how many scopes exist. Inline secondary entries cover:

- calendar: air date, scope, season, episode;
- due work: next check time, scope (monitored scopes only);
- exact scope identity (a digest of the canonical key), TMDB series/season/
  episode, and title-token lookups for series requested without a TMDB ID;
- catalog identity to canonical number, and canonical number to catalog identity,
  per TMDB series, shared by every overlapping scope;
- the scope and monitored counters and the format marker.

A save is one index transaction: header, changed blocks, calendar/due/lookup
entries, new identity entries and counters become durable together or not at all.
The identity entries enforce the invariants the whole snapshot used to check:
a catalog episode identity has one canonical number and a canonical number one
identity across every scope of a series, and a scope exists once. The widest
valid record (2,000 unmapped dated episodes and 2,000 retained identities with
choices spread over 4,000 blocks, then every date changed) needs about 10,000
changes and fits one transaction; a CI test saves a record of exactly that shape
and then changes every date (about 6,000 changes), each in one commit.

Reads touch only what they need: summaries are cursor pages of at most 200
headers. A scheduled due-work pass reads 50-row index pages, visits at most
400 due scopes (including unreadable ones) and attempts at most four refreshes
within its time budget. Its durable cursor resumes after restart. Scope and
TMDB lookups read their own entries; calendar windows are cursor
pages over the date entries (an `offset` of at most 20,000 skips rows, an
`after` cursor reaches any later row, and the exact `total` is reported only when
the window holds at most 20,000 rows). Loading one scope reads its header and
blocks (at most the per-plan bounds).

### Measured behavior

One hand-run of the `series_scale_report` harness (release build, Linux
container on overlayfs, synthetic scopes of 3 seasons x 30 episodes, one reading
process per size). These are indicative container numbers: CI does not run or
enforce them, and file synchronization is cheaper there than on macOS. Migrating
a snapshot at the old limits (128 scopes, 19,968 episodes, 8.3 MB) took 3.1 s in
the same container. One CI run on a macOS runner (unoptimized test build, so a
pessimistic figure) took 38 s for it; CI logs it as `series-migration-at-old-limits`
but nothing asserts it, and the start-up delay of the release build on a real
machine has not been measured. Expect a first start after upgrading to wait up to
tens of seconds for a snapshot near the old limits.

| Scopes | Episodes | On disk | Open | 200-summary pages walked | Due pass | One scope save | Calendar page (200 rows) | Peak RSS reading |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 150 | 13,500 | 14 MiB | <1 ms | 5 ms | <1 ms | 3 ms | 22 ms | 6 MiB |
| 1,000 | 90,000 | 93 MiB | <1 ms | 33 ms | <1 ms | 4 ms | 27 ms | 6 MiB |
| 4,000 | 360,000 | 373 MiB | <1 ms | 135 ms | <1 ms | 9 ms | 38 ms | 8 MiB |
| 12,000 | 1,080,000 | 1.1 GiB | <1 ms | 733 ms | <1 ms | 34 ms | 42 ms | 8 MiB |

Building these inventories with the test-only bulk loader (16,384 changes per
transaction, keys in random order) peaked at 30 to 139 MiB resident; that is the
transaction bound, not steady-state use. Disk use is about 1 KiB per episode.

### Migration from `series.json`

Opening storage with write access migrates a legacy snapshot (schema 1 or 2):

1. The whole snapshot is validated first (checksum, every record and field bound,
   duplicate scopes, identity invariants). An invalid snapshot is refused and
   nothing is written.
2. Its exact bytes are copied to `series.legacy-<digest>.json` (never replacing a
   file with other contents).
3. A staging index `series.idx.migrating` is filled one record per transaction;
   the legacy digest and a cursor commit with the records, so an interrupted
   migration resumes after the last record. A staging directory built from other
   bytes is derived data and is rebuilt.
4. Every record, index entry and counter is verified against the legacy parse.
5. The staging directory is renamed to `series.idx`.
6. `series.json` is atomically replaced by the schema 3 marker. Before this step
   an older binary still finds the unchanged legacy file; after it, older binaries
   reject the unsupported schema instead of reading stale or empty data.

Migration runs on the first start with write access, before the service listens,
and prints no progress; a failure stops the start. There is no validate-only or
dry-run mode beyond the read-only `needs_migration` status. If verification fails
(`Series migration verification failed`), `series.json` has not been touched:
stop Mynou, remove the derived `series.idx.migrating` directory, and start again;
if it fails again, keep `series.json` and the preserved copy and report it.

Every step is safe to repeat. A legacy snapshot that differs from the one the
index came from, a marker without its index and a marker bound to another index
are refused, never overwritten. Read-only opens never migrate: they create and
change nothing, and series calls report that the store has not been migrated.
A store that never held series stays empty until the first series is saved,
which creates the index and marker atomically.

### Downgrade and recovery

A downgrade is restoring the preserved snapshot: stop Mynou, move `series.idx`
aside, copy `series.legacy-<digest>.json` to `series.json`. Everything saved after
the migration (new series, refreshes, settings) is only in the index and is not in
that copy; older binaries also refuse more than 128 scopes. Opening the restored
snapshot with a newer binary while `series.idx` is still present is refused unless
it is the unchanged snapshot that produced the index.

A failed write never discards stored records: record files are replaced by
rename, pages are rewritten in place only after the complete redo is durable, and
an uncertain commit blocks the series store until restart, when the redo is
replayed. If the disk is still full, opening reports the error and leaves the
files as they are.

## Job storage

Jobs, history and job ownership now use `jobs.idx`, beside the existing series
index. Opening reads tree paths and requested records rather than one complete
job inventory. Canonical deduplication, media identity, library tips, lineage,
shared-file and pack ownership, IRC origins, requester job bindings, acquisition
and transfer ownership are secondary entries committed with their jobs. The
requester demand ledger uses its own `requesters.idx` index (see below).

There is no total job-count or job-store byte cap. Each job retains the old
16 MiB JSON/record limit; records larger than the index's 1 MiB object limit use
up to sixteen checked chunks, with their manifest and SHA-256 binding in the
same transaction. History uses one record per event and a sequence index. The
latest 1,000 events are retained per job; consecutive lease-renewal events fold
into the latest heartbeat. Migration preserves every event retained by the
previous globally bounded history, including maximum-size messages.

`mynou jobs` and `mynou library` accept `--limit 1..200` and `--after CURSOR`,
and `--query`/`--state` to [search](#searching-jobs-and-the-library).
`GET /api/jobs` and `GET /api/library` accept `limit`, `after`, `q` and `state` and return
`jobs`/`library`, `next` (null at the end), and `total`, plus a string `sequence` identifying changes. WebSocket requests use the
same routes; live topics send the first page, and the embedded interface loads
older pages on demand. Loaded tails refresh in place when records change; page membership changes and
reconnects reset the loaded window.
Pages have a 4 MiB hydrated-job byte budget, except one larger record may appear
alone. Existing network and JSON limits still apply; large individual records
can require direct store inspection instead of an API response.

Claims examine bounded queue windows and resume past ineligible jobs. Their
position is written at most once a minute, so a backlog of ineligible jobs does
not cost a durable commit per attempt; a restart resumes from the last
checkpoint, which a new process writes on its first attempt. Requester
cancellation sweeps keep a durable cursor that advances only after a page
settles, so a restart resumes where the last settled page ended. Feed routing
commits its cursor with the [captured generation](#captured-feed-routing).
Transfer retirement settles at most 64 jobs (up to
eight transfer debts each) per pass. Ownership lookups read at most two index
entries and verify each against its job record (see
[Job migration and downgrade](#job-migration-and-downgrade)); an entry its
record contradicts is a storage error, never proof that nothing owns it.

### Job migration and downgrade

The first **writable** start validates the legacy `snapshot.bin`/`journal.bin`
formats 1 through 5, checksums, hash chain, snapshot invariants and every complete
journal transaction before creating migration state. Service and management
starts also preflight requester demand and IRC reservation relationships against
the validated legacy jobs before publishing any migration files. Removed formats remain
explicit errors. An incomplete final journal frame is ignored as an
unacknowledged write, while the preserved journal retains its exact bytes.
Read-only opens create or repair nothing; an unmigrated store reports that a
writable start is needed. Legacy files must be private regular single-link files.

Validated originals are preserved as `snapshot.legacy-DIGEST.bin` and
`journal.legacy-DIGEST.bin` (only files with content need a backup), never
replacing a copy with different bytes. `jobs.idx.migrating` records its source
binding and migration cursor with each bounded chunk. A clean matching stage
resumes; a stage that cannot be safely read, or belongs to different originals,
is kept under a unique `jobs.idx.abandoned-*` name before rebuilding from the
validated originals. Its contents, including unexpected files and interrupted
transaction artifacts, are never recursively deleted.

Migration verifies jobs, secondary entries, history and counters before atomic
promotion to `jobs.idx`. It then replaces `snapshot.bin` with a marker bound to
that index and empties `journal.bin` only when it matches its preserved copy.
Crashes at promotion or marker publication settle on the next writable open.
Different restored originals beside an existing index are refused. Disk errors
leave preserved originals or the published index in place; normal index writes
use the transaction/recovery protocol described above and never evict jobs to
make space.

Earlier binaries reject the new `MYNOUS09` marker. To downgrade, stop Mynou,
back up the complete current store, move `jobs.idx` aside, and restore the matching
preserved files to `snapshot.bin` and `journal.bin`; remove a legacy file if that
original was absent. This restores only the pre-migration state: subsequent jobs
and changes exist in the index and are **not** in the legacy copies. Keep the
index separately if those changes must be recovered. Do not downgrade over live
storage or mix backup pairs.

### Indexed library monitoring

Scheduled and manual checks read the oldest check-time entries directly from
the disk index, rather than first walking every library page. One pass hydrates
at most `monitoring.max_checks + 1` eligible jobs (up to 257) and a shared-group
summary window of at most 200 jobs. Each window uses the usual 4 MiB hydrated
byte budget, allowing one larger job alone. Skip classifications and total
eligible/shared counts are maintained with library-tip transactions. Unchecked
imports have a separate creation-ordered queue, merged with elapsed checks by
their due time, so new imports cannot continually displace older due work. A check
rotates a durable sequence tie-breaker even within the same second; unrelated
job updates do not move its priority. Future check times and future-created
unchecked imports receive a bounded correction window after wall-clock rollback;
their next applied check returns them to normal scheduling.

Upgrade reports include exact decimal `inventory_counts`, `eligible_total`,
`oldest_checked_at` and `window_limited`. The eligible total includes entries
whose interval has not elapsed; it is not an exact due-backlog count. A truncated
shared summary also sets `limited`. Shared groups retain their explicit group
upgrade workflow. Storage failures stop the check and report the error instead
of presenting it as a concurrent entry change. No measured throughput claim is
made by this increment.

The first writable open upgrades existing job indexes from `mynou-jobs/1` to
`mynou-jobs/2`. A pending format marker excludes old readers before any derived
entries change. Each bounded library page commits scheduling entries, counters
and its cursor together. Reopening resumes that cursor; existing jobs, history,
ownership entries and preserved legacy files are never discarded or rewritten
by this upgrade. Disk exhaustion leaves the pending marker and recovery data
for a writable restart after storage is available. Read-only opening refuses
an unfinished or required upgrade and never repairs it.

After that, the same writable open upgrades `mynou-jobs/2` to `mynou-jobs/3`,
which adds an index of ready imported upgrade children so a library-tip write
asks one bounded question about a parent instead of reading every child it ever
had. The pending marker `mynou-jobs/3-building` excludes older readers; each
bounded page of derived entries commits with its cursor, reopening resumes it,
and read-only opening refuses an unfinished upgrade without repairing it.

Then `mynou-jobs/3` becomes `mynou-jobs/4`, which indexes the live owners of
each transfer (split into direct download or shared-file claims and pack or
IRC references), cancelled downloads, live acquisition addresses and the
shared-file owners of each physical video. Transfer ownership, co-ownership,
claim and cancellation checks and the shared-file binding lookup read at most
two index entries and verify each against its job record, instead of every
attempt or pack mapping that ever named the transfer or file; an entry its
record contradicts is a storage error, never an absent owner. IRC physical-video
conflict checks likewise read at most two entries per alias. The same format
indexes pack mappings by file and source URL or download (with the shared flag
and TMDB identity), the ready, newest and non-upgrade versions of each media
identity, and pending upgrade children, so pack conflict checks, the media
state shown for requests and the series calendar, requester media reuse and
shared replacement reservations each read at most two verified entries. The
upgrade walks the creation-ordered job table in
pages of 200 under `mynou-jobs/4-building`, committing each page with its
cursor; reopening resumes it and read-only opening refuses it.

Before upgrading, make a complete stopped-store backup if downgrade may be
needed. Earlier binaries reject formats 2 to 4 and the pending markers.
Downgrade requires restoring that complete pre-upgrade backup, or the matching
original legacy snapshots as described above; subsequent changes are only in the newer
index. Never edit the index marker to bypass this rejection. Feed routing uses
a [durable captured generation](#captured-feed-routing), committing candidates
with seen state before routing and resuming checkpoints after restart.

## Requester storage

`requesters.idx` stores requester demands and operator interests without a total
record-count or aggregate-byte limit. Only the bounded account configuration,
requested pages and explicit changed-record patches remain in memory. Demand
manifests bind checked chunks; account, media, admission, outcome and IRC-origin
references and counters change atomically with their primary records.

`mynou requester ACCOUNT --limit 1..200 --after CURSOR` and
`GET /api/requesters/ACCOUNT?limit=200&after=CURSOR` return `demands`, `next`,
`total` and a string `sequence`. Start without `after`; use `next` until null.
Offsets remain available up to 10,000. Cursors are live key scans: concurrent
insertions before the cursor may require restarting the listing. Pages hold at
most 200 demands and 4 MiB of hydrated records, except a larger individual
record may appear alone within its historical 16 MiB record bound. Network and
JSON response bounds still apply.

Policy previews bind ledger and job sequences, the affected account/demand and
captured configuration without loading the inventory. Policy revisions fence
new admissions; unadmitted demand adopts current policy during bounded admission
passes. Durable reservation precedes job creation. Admission examines up to 200
rows and admits at most 64 per pass; outcome reconciliation and origin retirement
continue through persisted cursors. Each account keeps one watchlist sweep
record: its position, the anchor item that detects a shifted listing, the
upstream total, a staged origin generation and an optional confirmation digest.
Every page commits its demands, origins and the new position in transactions
of at most 512 demands each. Only a
sweep that reached the end of an unchanged listing publishes its generation;
a sweep spanning multiple polling passes also needs a complete confirming
traversal with the same ordered origin digest. Existing unfinished sweep records
are read conservatively as requiring confirmation. The confirmation digest
commits with progress and survives restarts. The next sweep waits until the
retirement pass finishes. Failed, partial or
shifted reads never authorize removing an unseen origin. Origin rows of
generations that are neither published nor staged are deleted a bounded window
at a time. Binaries from before sweep support ignore the sweep record and poll
as before; after upgrading again, a sweep older than their last poll is
discarded. Binaries with the original fixed-size sweep format refuse an
unfinished extended sweep record; keep a complete stopped-store backup before
upgrading if downgrade may be needed.

### Requester migration and downgrade

Before publication, startup validates legacy `MYNOUR01`/`MYNOUR02` bytes and both
directions of requester/job and requester/IRC provenance. Validated originals
are preserved exactly as private `requesters.legacy-DIGEST.bin` copies.
`requesters.idx.staging` binds its source and resumes bounded demand batches;
a staging directory that cannot be safely reused is preserved under a unique
`requesters.idx.abandoned-*` name. Every primary, secondary key and count is
verified before promotion. An already promoted index must match the exact
legacy image before marker publication; a restored older snapshot cannot rewrite
acknowledged index changes. `requesters.bin` then becomes the `MYNOUR03` marker,
bound to the verified index identity, which earlier binaries refuse.

Read-only opens never migrate or repair. Legacy listings remain readable before
migration; interrupted indexed writes require a writable restart. Uncertain
writes block further reads and changes and retain recovery artifacts.

To downgrade, stop Mynou, back up the complete current store, move
`requesters.idx` and any staging directory aside, and restore the matching
preserved original to `requesters.bin`. For an originally absent snapshot,
remove the marker only after moving the index aside. Restore matching job/IRC
state as well: incompatible provenance is refused. This restores pre-migration
state only; later demands, policy changes and operator interests remain solely
in the preserved index. Never mix backup generations or downgrade live storage.

## Feed storage

`feeds.idx` retains baselines, checked counters and up to 1,024 opaque entry
identities per source, without the former 1,000-source / 4 MiB aggregate limit.
Historical sources stay on disk; startup checks configured bindings with point
reads and reconciles only active baselines. Configuration still allows at most
64 watched sources. Network and per-poll entry/routing bounds remain in force;
[source policies](#source-policy-storage) use their own index, with no fixed
retained-policy count or aggregate-byte cap.

`mynou feeds --retained --limit 1..200 --after CURSOR` and
`GET /api/feeds/retained?limit=200&after=CURSOR` list retained summaries, including
unconfigured historical sources. Start without `after`, then follow `next` until
null. `total` and `sequence` are decimal strings. These are live key scans:
concurrent inserts before the cursor need a new listing. Pages hold at most
200 records and 4 MiB of hydrated bytes. Summaries never expose bindings,
retained identities, feed entry URLs or titles. Ordinary feed health also
reports `retained_sources` and `needs_migration`.

The first writable start validates the bounded legacy `MYNOUF01` snapshot and
configured source bindings before publication. Exact private bytes are preserved
as `feeds.legacy-DIGEST.bin`. `feeds.idx.migrating` binds its source digest and
commits bounded batches with a migration cursor. A clean matching stage resumes;
a foreign or unreadable stage is preserved intact under `feeds.idx.abandoned-*`
before rebuilding. Unexpected contents and interrupted WAL are never recursively
deleted. Every record, active reference and count is verified before promotion.
`feeds.bin` then becomes an identity-bound `MYNOUF02` marker. Promotion interrupted
before marker publication is verified against the unchanged original on reopen.
An unmarked index with an unfinished transaction is refused without repair.

Read-only opens create and change nothing. Legacy summaries remain available
with `needs_migration`; pending indexed transactions require writable recovery.
Uncertain writes block subsequent reads and changes until restart. Disk errors
leave originals and recovery artifacts in place rather than evicting records.

Earlier binaries reject `MYNOUF02`. To downgrade, stop Mynou, keep a complete
stopped-store backup, move the index and staging aside, and restore the matching
preserved original as `feeds.bin`. If no original existed, move the index aside
before removing the marker. Later observations exist only in the index and are
lost by restoring the old snapshot; keep it separately for recovery. Never mix
backup generations or edit markers to bypass rejection.

[Captured routing generations](#captured-feed-routing) persist transient
announcements and routing progress across every inventory page and restart.

## Startup validation

`mynou serve` answers the API at once and checks requester and IRC provenance
afterwards in bounded steps of at most one 200-row page: retained demands and
requester requests, IRC admissions, demands, routes and IRC-origin requests,
then store initialization, settlement of interrupted IRC admissions and
routes, the requester cancellation sweep and native transfers. Nothing is
written before every read-only stage passes. Until the last stage, only reads
and shutdown are served (other requests get 503), workers and native
transfers wait, `/readyz` answers `starting` and `startup` in `/api/status`
reports the stage, pages and rows. An inconsistency leaves the service in
`failed` with the error and no effects: a native client opened for the last
stage is published only after its retirement checks pass, and closed
otherwise. A restart validates from the first
stage again, since the stores may have changed while Mynou was stopped.
Command-line management and previews still complete the same stages before
they act.

## Remaining delivery

[Issue #106](https://github.com/alecerf/mynou/issues/106) remains open. Indexed
retention is delivered for all seven stores, including IRC announcements and
source policies. Captured feed routing, resumable upstream paging, bounded
admission/monitoring and transfer scheduling, paginated views, startup validation
and storage-size/error diagnostics are also delivered. Their operation bounds
remain intentional; a retained inventory is not truncated to those windows.

Two acceptance steps remain:

- [#246](https://github.com/alecerf/mynou/issues/246): complete per-store due
  backlog, oldest unserved age and last completed pass readings alongside
  [storage diagnostics](#storage-diagnostics-and-errors). Series (#271) and
  library monitoring (#273) are delivered under `inventory.series` and
  `inventory.library`; jobs/history is delivered by #275 under `inventory.jobs`.
  Requester demand, feeds, source policies, IRC announcements and transfers remain. Feed routing counts alone are not
  the complete common diagnostics. Migration-exhaustion fixtures and their
  store-named full-disk error correction are already delivered.
- [#217](https://github.com/alecerf/mynou/issues/217), after #246: record disk
  size, open/startup time, a routine pass, a point update, a page read and peak
  resident memory at three or more increasing sizes for every indexed store on
  native macOS arm64; publish the acceptance table and audit remaining bounds.
  Existing series measurements and bounded-window tests do not establish this
  evidence for all stores, so no overall scalability claim is made here.

Use synthetic fixtures and relevant native local validation, followed by the
required GitHub Actions checks. Removing inventory caps does not remove physical
storage or compute limits.

## Captured feed routing

Feed format `mynou-feeds/2` adds one pending routing generation per configured
watched source, with at most 200 private captured entries and 4 MiB of encoded
payload. Each entry has its own checked object, binding source, generation and
ordinal. Candidates include the titles and acquisition URLs necessary for replay;
URLs may carry private access tokens, like saved job acquisition URLs. Headers,
cookies and standalone credential values are never captured. CLI/API diagnostics
show only counts, age and phase. An announcement outside the capture bounds (a
control character in its title, or a title and address too large once encoded)
is counted as skipped and never blocks the entries polled with it.

Observation state and candidates commit together. A pending generation delays
another source poll until its job/library scan finishes; it resumes on the next
watcher tick while it advances and rests ten seconds after a pass that settled
nothing (an unverifiable availability check or a busy upgrade). Each source pass reads
at most 200 jobs in 4 MiB hydrated windows (one larger existing job may appear
alone), settles at most 32 acquisitions or
upgrades and checks a ten-second routing deadline between decisions. One Plex
availability request can run until its own ten-second deadline. Up to four sources
run per watcher tick; a persisted round-robin source cursor prevents a busy source
from continually displacing the others. Poll I/O retains its existing deadlines.

Checkpoints record the last settled row, including nonmatches; unfinished page
rows are never skipped. Creation-ordered scans are live: new rows before the
cursor wait for a later generation. Older surviving eligible rows remain reachable.
A crash after a job effect but before checkpoint replays through selection and
upgrade ownership guards without resetting or duplicating the effect. Storage
failures retain progress and block uncertain reads/writes until restart; a busy
upgrade lock retains the library phase.

Captured candidates grant no authority. Source policy revision, current requester
interest, job state, library tip and release/profile baseline fence final mutations.
Every applied source control action (pause, enable, probe or session reset) changes
the source revision and so cancels work captured under the previous one; the source
baselines again. Pause and removal cancel pending work while clearing the baseline. A failed cancellation after a
persisted pause reports an error; startup settles it under the disabled policy.

The format-1 upgrade atomically changes the header and adds empty routing/schedule
metadata without enumerating retained records. Earlier binaries reject format 2.
Until that first writable start, read-only commands refuse the format-1 index.
Before upgrading, keep a complete stopped-store backup if format-1 downgrade is
needed; restoring it loses subsequent observations and captured work. Restoring
legacy snapshots has the older limitations described above.

## Source policy storage

Retained indexer policies live in private `indexers.idx/`, keyed by source ID,
with immutable configuration binding, enabled state, revision, last action and
time. Retired sources remain on disk without a fixed retained count or byte
cap. Startup checks only configured IDs; authorization, reports and controls use
point reads. Reports still cover configured sources, not retired history.
A guarded preview binds the store identity and transaction sequence along with
the target policy and action. Any durable policy mutation invalidates earlier
previews, including controls on other sources, without scanning retained rows.
Cookies, credentials and their environment values are never persisted.

On the first writable startup, the old bounded `indexers.bin` is validated and
preserved exactly as private `indexers.legacy-<digest>.bin`. Migration commits
at most 100 rows and their continuation cursor together into
`indexers.idx.migrating/`. Restart resumes valid staging; unexpected or
interrupted derived staging is preserved as `indexers.idx.abandoned-<digest>/`
before rebuilding. Every legacy row and the complete count are checked before
promotion. The new `indexers.bin` is a checksummed marker bound to the index
identity; old readers reject it. A crash after promotion but before marker
publication verifies the promoted index against the unchanged legacy bytes
before completing publication. Read-only inspection never migrates or repairs.

A storage failure is an error, not an absent or default-enabled policy. After
an uncertain control, runtime source access is blocked until a writable restart
recovers durable state; a failed response may mean the change committed.
To downgrade, stop Mynou, preserve a complete copy of the store including the
index and recovery artifacts, then restore the exact legacy backup as
`indexers.bin` and move the new index aside. This restores pre-migration policy;
later controls cannot be represented by that backup. Do not downgrade using
only the new marker or remove rollback artifacts while the service runs.


## IRC announcement storage

`announcements.idx` stores immutable claims, decisions, admission intents and
routing origins on disk. The former aggregate 1,000-identity / 8 MiB limits no
longer apply. Bindings remain bounded metadata (32 retained source IDs); source,
rule, line and rate limits remain as documented in [limits](limits.md).
Duplicate checks and admission fences read the announcement by identity.
Chronological, pending and recovery tables change atomically with each record;
startup cross-checks reservations and requester/job provenance in bounded pages
before downloads start. An announcement alone never creates demand.

`mynou announcements --limit 200` and
`GET /api/irc/announcements?limit=200` return newest-first pages with `total`,
`announcements` and `next_cursor`. Pass that cursor with `--after CURSOR` or
`after=CURSOR` until it is null. Pages hold at most 200 rows and 4 MiB of stored
record data. These are live scans: new arrivals ahead of the cursor require a
new listing. The older `--offset`/`offset` interface remains available up to
1,000 rows; use a cursor for the complete history. A decision or receipt never
requires a whole-history clone. Transient retry state stays bounded and is not
part of retained duplicate suppression.

The first writable start validates the old `MYNOUI01`/`02`/`03` snapshot and
configured bindings, keeps an exact private `announcements.legacy-DIGEST.bin`,
and commits migration batches with their cursor atomically. Staging is derived
from the validated source digest (`announcements.idx.staging-DIGEST`); a private
`announcements.migration-DIGEST.bin` binds the digest to the index identity
before the staging directory is created or any transaction can be recovered.
An interruption during creation preserves known private empty artifacts and
recreates them with the bound identity; foreign or populated artifacts are
refused. Restart validates the committed prefix
before resuming; every original primary row, secondary entry, metadata value and
count is verified before promotion. Extra, changed or foreign entries are
refused. An interrupted promotion is reverified before publishing the marker.

`announcements.bin` becomes a checked `MYNOUI04` marker bound to the index
identity. Older binaries reject it. Read-only inspection never initializes,
migrates, recovers redo, changes permissions or writes decisions. A pending
index transaction requires writable recovery; corruption, public permissions,
links or mismatched proofs stop the operation. Incomplete creation artifacts are preserved rather than silently deleted or
adopted. Failed writes report errors, and an uncertain
commit blocks decisions until storage is reopened.

For downgrade, stop Mynou and preserve the complete current storage directory.
Restore `announcements.bin` from its exact legacy backup in a separate copy,
move the new index, staging and migration proofs aside, and use a binary that
supports the restored snapshot format. Restoring the old copy loses later IRC
receipts, decisions and origins; requester and job stores must come from the
same stopped-store backup so their provenance still agrees. Merely restoring
legacy bytes alongside an acknowledged index is refused. A full backup made
before upgrade is the safest recovery point.

### Series backlog and pass freshness

`GET /api/status` and `mynou status` expose `inventory.series` alongside the
existing `storage` sizes. This is the first store integration of #246 (#271);
library monitoring is described below. A ready reading contains:

- `due`: exact number of monitored scopes whose `next_check_at <= now`;
- `oldest_due_at` and `oldest_due_age_secs`: the earliest due timestamp and
  its elapsed wall-clock seconds, or `null` when no scope is due;
- `last_completed_pass_at`: the persisted completion time of the last
  successful bounded background series pass, or `null` before the first one.

Counts and timestamps are decimal strings to preserve all 64 bits. Zero is a
valid due timestamp. Age measures lateness from the scheduled check, not the
age of the media or of its last catalog response. Clock rollback excludes
future due keys; no negative age is returned. A pass is the scheduler's
bounded unit (at most four refresh attempts / 400 examined due keys), not a
promise that the entire inventory was checked. An empty successful pass also
advances freshness. Errors, unreadable scopes (including cached skips), shutdown
and deadline expiry preserve the previous successful-pass time. The scheduling
checkpoint and completion timestamp share one durable transaction. The reading
contains no titles, paths, provider responses or credentials.

Status reads use the timestamp-ordered due index: at most three tree paths for
range cardinality and the first key, plus one point lookup for pass freshness.
They never load series headers or episode records, scan due rows or write data.
Unmonitoring/rescheduling removes the old due entry atomically with the scope's
update, so counts and oldest time follow ordinary writes and survive restart.
These are index scheduling diagnostics; they do not certify record health or
network availability. A held store reports `state: busy`. A legacy, unprepared,
uncertain or unreadable store reports `state: unavailable` with a fixed recovery
hint rather than a guessed zero, scan, or raw error; other status fields remain
available. Only `state: ready` readings carry counts and timestamps.

Writable series-store startup now explicitly prepares missing legacy branch
cardinalities through `prepare_range_counts(64)` batches after the existing
snapshot migration/identity checks. Each batch is resumable through the ordinary
WAL; failed preparation prevents writable startup until recovery succeeds.
Read-only startup and status do not perform this upgrade. See the range-count
upgrade and **Downgrade** rules above: keep a stopped-store backup before the
first writable upgrade; binaries predating `MYNOUIN2` must not open it for writes.
Existing series snapshot migration backups are preserved. Old stores have no
successful-pass timestamp until a new successful background pass completes.

### Library backlog and pass freshness

`GET /api/status` and `mynou status` also expose `inventory.library` (#273),
using the same decimal-string fields as series: `due`, `oldest_due_at`,
`oldest_due_age_secs` and `last_completed_pass_at`. The unit is an eligible
current library tip, as defined by [indexed library monitoring](#indexed-library-monitoring).
Unmonitored tips, missing release baselines, unsupported media kinds and shared
files requiring the explicit group workflow are excluded from this automatic
queue. History and replaced versions are excluded as well.

A never-checked tip is due from its creation time. A previously checked tip is
due when `monitor_checked_at + monitoring.interval_secs <= now`. Future check
or creation times are due immediately for clock-rollback correction; their due
time is reported as `now`, so their lateness is zero. The oldest time is the
minimum across these disjoint queues, with no negative age. This is scheduled
work, before per-entry checks such as a missing local import or an existing
upgrade. When monitoring is disabled, `state: disabled` reports zero due work,
null oldest fields and the preserved last successful-pass time.

An automatic pass completes after successfully handling its bounded window,
even when `limited` says there are more entries. Settled skips, such as missing
imports, are handled work. An empty pass also completes. Shutdown, a 90-second
deadline, search failure or a storage error preserves the last successful-pass
time; manual forced checks and previews never advance it. A successful pass
writes its completion time through the ordinary durable job-index transaction
protocol after the window has completed. A write error can have an uncertain
outcome, as described under [transaction and recovery](#transaction-and-recovery).
The timestamp survives restart and stays null in older stores until the first
successful automatic pass. It does not claim that every library entry was checked.

Status uses at most three exact range summaries plus one point lookup for the
completion time. It never reads job payloads, walks the library or writes data.
Rescheduling, unmonitoring and replacing tips update the existing scheduling
indexes in the same transaction as the tip change. Busy storage reports
`state: busy`; a legacy, unprepared, uncertain or unreadable index reports
`state: unavailable` with a fixed recovery hint and no counts. Diagnostics contain
no titles, local paths, source payloads or credentials. The upgrade report's
`oldest_checked_at` remains a bounded-window reading; the service's
`inventory.library` reports the complete automatic due backlog.

Writable job-store startup prepares missing branch cardinalities in resumable
`prepare_range_counts(64)` metadata batches after the existing format/provenance
checks and scheduling upgrades. Read-only opening and status never prepare them
or substitute an inventory scan. Failure preserves recovery data and requires a
writable restart. The [range-count downgrade rules](#exact-range-summaries) apply:
keep a complete stopped-store backup before upgrading; earlier binaries that
predate counted branch pages refuse the new format. Existing migration backups
are preserved. This delivery leaves #246 and #106 open for the remaining stores
and measured acceptance.


### Job backlog and claim-scan freshness

`GET /api/status` and `mynou status` expose `inventory.jobs` (#275) using the
same decimal-string fields as series/library. The scheduled unit, explicitly
reported as `unit: job_claim_scan`, is one bounded claim attempt: at most one
promotion page and 256 examined queue entries. It measures the scheduler's
progress, before execution and current requester, IRC, shared-group or upgrade
eligibility checks. It does not assert that a download or import succeeded.
Retained ready, cancelled, staged and exhausted failed jobs are not periodic work.

`due` counts open jobs with `wake <= now`, including jobs still awaiting bounded
promotion. `wake` is `next_attempt_at`, or its maximum with `lease_until` while
a lease exists. Active leases and future retries are excluded until their wake;
expired leases rejoin automatically. `oldest_due_at` is the earliest scheduled
wake, and `oldest_due_age_secs` is its lateness; both are null when no job is due.
Zero is a valid immediate wake in the job format and is reported literally,
including its elapsed age; it is not replaced by a guessed creation time.
Clock rollback excludes future wake times even after a job has been promoted.

`last_completed_pass_at` is the durable completion time of a successful bounded
claim scan, not a completed job. Acquiring a lease records it in the same
transaction. Successful idle or eligibility-filtered scans record it with the
existing durable claim cursor, at most once per `freshness_checkpoint_secs`
(currently 60 seconds) per worker process. Between checkpoints a newer scan may
have finished; the field reports the last recorded successful scan. Restart
records the first successful scan immediately. Failed eligibility/storage reads
and invalid claims do not advance it; a failed commit may recover its entire
transaction as described above. Older stores report null until the first
successful checkpoint. Idle polls within the interval do not add durable writes.

A dedicated wake-ordered secondary index changes atomically with job updates,
lease renewal/release, retries and cancellation. Status needs one exact range
summary plus two metadata point reads, never job/history payloads, queue
promotion, writes or a scan of due entries. Busy/unavailable states use the same
fixed recovery hint and omit counts; raw storage errors are not published here.

The first writable open upgrades `mynou-jobs/4` to `mynou-jobs/5`, adding the
secondary entries in resumable pages of at most 200 jobs. Each page and its
continuation commit together. Primary records, history and existing legacy
backups are preserved. Read-only commands refuse an unprepared format; they
never migrate. Interrupted or full-disk upgrades resume after storage recovery.
Earlier binaries reject format 5: keep a complete stopped-store backup before
upgrading, and restore that consistent backup to downgrade, losing subsequent
changes. Missing legacy branch counts use the existing explicit resumable
writable preparation; status never substitutes an inventory scan.

This closes the jobs/history portion of #246 only. Requester demand, feeds,
source policies, IRC announcements and transfers remain, followed by #217's
increasing-size measurements before #106 can close. Source policies have no
periodic worker and must receive explicit non-applicability semantics.
