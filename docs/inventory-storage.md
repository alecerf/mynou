# Inventory storage

The `mynou::inventory::Index` Rust API is the storage foundation for
[growing inventories](https://github.com/alecerf/mynou/issues/106). Jobs and
history, series and numbering, requester demand, retained feeds, source policies,
IRC announcements, cross-seed discovery and native transfers use private disk
indexes. Library tips and their monitoring entries share the job index.

Mynou reads only the current format of each store. There is no migration from
earlier storage formats and no downgrade path: a store written by an earlier
development build is refused at open and must be recreated (stop Mynou, move
the store directory aside, start again).

| Store | Storage and recovery |
| --- | --- |
| Jobs, history and library tips | [Job storage](#job-storage), [library monitoring](#indexed-library-monitoring) |
| Series, episodes and numbering | [Series storage](#series-storage) |
| Requester demand and operator interests | [Requester storage](#requester-storage) |
| Retained feeds and captured routing | [Feed storage](#feed-storage), [captured feed routing](#captured-feed-routing) |
| Source policies | [Source policy storage](#source-policy-storage) |
| IRC announcements | [IRC announcement storage](#irc-announcement-storage) |
| Cross-seed discovery | [Automatic cross-seed discovery](transfers.md#automatic-cross-seed-discovery) |
| Native transfers | [Transfer storage](transfers.md#storage) |

Per-record, page, transaction and configured-source bounds still apply; see
[limits](limits.md). Removing total inventory caps does not establish scalability:
[capacity acceptance](#capacity-acceptance) tracks diagnostics and measured evidence.

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
on x86-64 and arm64; other targets do not build). This covers index files, redo logs,
value-log segments and locks of the job, series, requester, feed, source-policy
and IRC stores, and the native transfer state (ownership lock and the persisted
`.torrent`, `.source`, `.control`, `.paused` and `.seed` files). If the final path component is swapped for a symbolic link
after inspection, the open itself refuses it, so the target is never read. A
FIFO or device returns at once instead of blocking, and the descriptor is
refused unless it is the inspected regular file (same device and inode). A
missing job store lock is created exclusively. Directory synchronization opens without blocking and refuses non-directories.
`O_NOFOLLOW` covers only the final component. A parent directory swapped for a
symbolic link between inspection and open is still followed, which stays within
the private-directory trust model above. The link-count and permission checks
remain as defense in depth.

`identity.bin` holds a random 256-bit store identity and checksum. `redo.bin`
holds the [redo log](#transaction-and-recovery). `tree.bin` contains 4 KiB
checksummed pages. Page zero holds the identity, sequence,
root, allocation counter, record count and the head of the free list;
remaining pages hold ordered keys and either value references or child
references. Nodes have at most 32 items;
child levels strictly decrease and child upper keys must match their parents.
Offsets/counts use checked arithmetic, supported offsets fit a signed 64-bit
file offset, and there is no configured total inventory cap.

Pages are rewritten in place. A page that deletions empty, or a root left
behind when the tree loses a level, becomes a free page (`MYNOUIF1`) that names
the next free page; the header names the first. Splits and new roots take a
free page before the file grows, in the same transaction and redo record as
the change that frees or reuses it. The file never shrinks, and pages that are
only partly full are not merged: a store that deletes most of its records
keeps those pages until new records fill them. A branch that references a free
page, or a free list that names a page in use, is corruption and an error.

Keys are 64-byte ordered binary values. Callers must define table/range
prefixes and identity encodings. Values have at most 1 MiB. Each value is
written in an envelope that binds the key, length and checksum, inside the
value log described below. Reads never list a directory. Checksums (XXH64 for
pages, envelopes and redo records, SHA-256 for identity and segment headers)
detect corruption, not
malicious rewrites by somebody who can replace state and its checksums.

## Exact range summaries

`inventory::Index::range_summary(start, end)` returns an exact key count and
first key for `[start, end)`; `None` means no upper bound. An equal start/end
is empty; a reversed range is refused. The scheduled job, library, series, requester, IRC and transfer diagnostics use
this primitive for #246; feeds use bounded configured-source scheduling metadata. A tree entry can be a secondary key, so its count is not
necessarily a count of jobs, episodes or transfers.

Branch pages (`MYNOUIN4`) carry the checked cardinality of each child beside
its reference; leaves use `MYNOUIN3`. Inserts, replacements, splits, deletions
and root collapse maintain cardinalities in the same transaction as the
records. The range read subtracts two ranks and reads the first key, at most
three root-to-leaf paths with at most 32 links per node. It does not read
values, enumerate files, or scan all the rows of the range. Counts describe
index keys; they do not verify the contents of record values.

Regression tests compare ranges to independent ordered collections through
growth, unordered churn, replacement, deletion, root collapse and restart;
check equal timestamps and cold-page read bounds; and cover inconsistent
cardinalities, read-only preservation, interruption and storage exhaustion at
redo, directory and tree stages. These
synthetic checks establish correctness and operation bounds, not the multi-store
macOS RSS/timing evidence recorded separately in [inventory-growth.md](inventory-growth.md).

## Value log

On Apple platforms every durability flush is an `F_FULLFSYNC`, which flushes
the whole drive cache and costs milliseconds; std offers no cheaper flush. A
commit therefore flushes one file, the [redo log](#transaction-and-recovery),
and everything else it writes waits for a checkpoint.

Values live in segments, `segments/<16 hex digits>`. A segment starts with a
magic, the store identity, its number and their SHA-256, followed by envelopes
in commit order. Commits append to segment `head` until it reaches 64 MiB (the
header records its length), then start the next one. An envelope binds the
key, the value length and an XXH64 checksum of both. The tree leaf keeps the
segment number, the envelope offset, the value length and that checksum, so a
read is one positioned read of the envelope and one checksum. Bytes are never rewritten once a record names them, so
a location names exactly one envelope.

Segment files are inspected (private directory, regular private file, header)
when they are first opened, and up to 64 descriptors stay open; reads then
skip those path checks. This narrows the private-directory trust model above:
a segment file replaced while its descriptor is open is not noticed by reads
until the next open, as for `tree.bin`. Commits still inspect the `segments`
directory, and recovery inspects every path it writes.

The header also records the oldest segment still in use (`tail`), how far the
cleaner has read into it, the bytes of all segments and the bytes of envelopes
still referenced. Once the segments exceed twice the referenced bytes plus
8 MiB, segment `head` stops taking appends and each commit targets 4 MiB (or
as much as it appends) from the oldest segments, rewriting every
still-referenced envelope at the end of the log as an ordinary replacement in
the same transaction. The cleaner stops before the commit would approach its
change, page or record bounds, or retire more than 256 segments. A later commit
resumes cleaning; corrupt or unsafe segment files are explicit errors. A
segment read to its end leaves the log; the checkpoint that makes the root
durable deletes its file.

The API validates the root at open and each referenced page/value on access;
opening does not audit the entire inventory. Corruption on an untouched path
is reported when that path is read, never returned as an empty/absent result.

## Transaction and recovery

One transaction contains at most 16,384 distinct key changes, 16,384 dirty pages
and 128 MiB of redo payload. These are operation bounds, not inventory bounds.
Primary and secondary keys can change together. Preparation verifies existing
records, stages tree changes in memory and validates the resulting key changes.

`redo.bin` starts with two checkpoint slots, each holding a generation, the
durable root and the oldest segment the previous checkpoint still kept, with
an XXH64 checksum. The slot with the highest valid generation wins, so a torn
slot write leaves the other one. Records follow from offset 16 KiB, each
aligned to 4 KiB and carrying the slot's generation, its length and an XXH64
checksum of the complete transaction: old and new roots, page images and every
value it writes.

1. Write the record after the previous one and flush `redo.bin`. Once that
   flush returns, the commit is durable and is acknowledged.
2. Without flushing: append the values to the segment, write the page images
   and the root to `tree.bin`.
3. A checkpoint flushes every segment written since the previous one, the
   directories whose entries changed, and `tree.bin`; then it writes the
   other slot with the new root and flushes `redo.bin`. Only then are the
   records discarded (the file is truncated to its slots) and retired segment
   files removed. A checkpoint runs before a record would grow the log past
   64 MiB, when an index is closed cleanly and after writable recovery.

A commit costs one flush whatever its record count; inline, deletion and
cleaning commits included. A checkpoint costs one flush per segment written
since the previous one (usually one or two), one per changed directory, and
two for the tree and the slot: about five, shared by every commit since the
previous checkpoint. `inventory::flush_tests` enforces both bounds.

Any error after the record is written blocks **all reads and writes** until
reopen. The caller must inspect recovered state rather than automatically
retrying: a failed commit response can mean a committed transaction. When the
redo flush itself fails, the record is cut off again before the error returns,
so the change is lost unless the machine stopped first. Validation failures
before the record leave the existing store usable and unchanged.

Writable open reads the newest valid slot and every record after it whose
generation matches and whose checksum is valid. Store identity, old and new
roots (each record continues the previous one), sequence, page images, actions,
segment locations and resulting key references must agree. The live root must
be the checkpoint or a state a record reaches; anything else is a stale log
and an error. Every path the replay will write or remove is inspected first;
then each record is reapplied, rebuilding its segment bytes and pages, so a
write the drive lost or tore before the checkpoint is restored. Replay is
idempotent. Recovery then checkpoints and drops segment bytes no record
names.

A final record that fails its checksum or ends early is an interrupted write
that was never acknowledged, and is ignored. A damaged record followed by an
intact one of the same generation is corruption and an error, with no repair.
Records of another store or an older generation end the log. Links, wrong
types and public permissions remain errors. Retirement is bounded to 256
segments for normal commits and replay.

Read-only open never repairs, creates, deletes or changes permissions. It
replays pending records in memory: their pages and segment bytes are served
from memory, and nothing on disk changes. An existing directory is never
initialized over. A store without `redo.bin` is refused ("Inventory redo log
is missing"), as is any page, envelope or root in another format.

Each store builds a new index under `<name>.idx.creating`, writes its format
and metadata, and only then renames it to `<name>.idx` and synchronizes the
parent directory. A leftover `.creating` directory is an interrupted creation
that held no record; the next writable start removes it and creates again.

## Benchmark

`examples/inventory_bench.rs` runs the workloads below through the public
`Index` API only, on a fresh store:

```sh
cargo run --release --example inventory_bench -- /private/tmp/inventory-bench
```

Run on 2026-10-28 on macOS 27.2 (Apple silicon, APFS on the internal SSD),
single thread, warm OS cache: 100,000 keys of 64 bytes inserted in random
order, 2 KiB values. The inline rows use 24-byte inline values. Every engine
flushes with `F_FULLFSYNC` before acknowledging a commit:

- SQLite 3.46.0 in WAL mode with `synchronous=FULL`, `fullfsync=ON` and
  `checkpoint_fullfsync=ON`.
- redb 2.6.4 with its default immediate durability.
- LMDB through heed 0.20.5 (lmdb-master-sys 0.2.6), default flags, 8 GiB map.

The other engines ran from a separate harness kept out of the repository,
since they would be dependencies. The reopen row includes closing, which for
Mynou is a checkpoint.

| Operation | Mynou 0.26 | Mynou | SQLite | redb | LMDB |
| --- | ---: | ---: | ---: | ---: | ---: |
| Load, 1,000 records per commit (rec/s) | 4,600 | **43,400** | 18,900 | 17,200 | 27,500 |
| One-record commit, mean (ms) | 31–35 | **4.2** | 5.0 | 5.2 | 5.3 |
| Updates, 100 per commit (rec/s) | 1,500 | **11,900** | 5,300 | 7,600 | 9,400 |
| Point read (µs) | 88 | 2.2 | 4.7 | 2.1 | **1.4** |
| Missing-key read (µs) | 17 | **0.8** | 2.8 | 2.1 | 1.2 |
| Close and reopen (ms) | **0.3** | 27 | 26 | 27 | 3.2 |
| Full scan, 200 per page (ms) | 6,600 | 130 | 194 | 184 | **47** |
| Exact count of a 10,000-key range (µs) | 44 | **1.8** | 231 | 2,546 | 822 |
| Deletes, 1,000 per commit (rec/s) | 3,900 | **55,600** | 20,700 | 30,800 | 27,500 |
| Disk after load (MiB) | **224** | 269 | 408 | 515 | 368 |
| Inline values: load (rec/s) | 6,300 | 62,800 | 51,400 | 62,300 | **80,400** |
| Inline values: point read (µs) | 21 | **0.56** | 2.26 | 0.98 | 0.59 |

Every engine ran in the same harness. Repeated runs varied by up to 20% for
loads and 10% elsewhere; the in-repository example measures Mynou within that
range. What the numbers do not show:

- A one-record commit is bound by its single `F_FULLFSYNC` (about 3–4 ms on
  this drive) in every engine; the differences there are small.
- Point reads and scans spend about half their time in `pread`. LMDB maps the
  file into memory, which needs `unsafe` and turns I/O errors into crashes,
  so the inventory keeps `pread`.
- Closing now costs a checkpoint, about five flushes; the next open does no
  work. The 0.26 store had nothing to flush at close.
- The disk total includes the redo log, which holds up to 64 MiB of records
  between checkpoints.
- Loading 1,000 records per commit peaked at about 420 MiB of process RSS
  against about 320 MiB in 0.26, from allocator retention of commit buffers;
  it fell back to under 60 MiB once loading stopped.
- One machine, one workload shape, one thread. These are not general rankings.

## Storage diagnostics and errors

`GET /api/status` includes `storage`, one entry per index:
`jobs`, `series`, `requesters`, `announcements`, `sources`, `feeds` and
`transfers`. Each entry has a `state` (`index`, `empty`, `not_open`, or `busy`
when a long operation holds the store, so the status request never waits for
it) and, for an open index:

- `entries`: index entries, including the store's own secondary keys, so it
  is larger than the number of jobs or series shown elsewhere in the status;
- `index_bytes` and `log_bytes`: bytes of the tree file and of the value-log
  segments, which include garbage the cleaner has not yet reclaimed, and
  `live_bytes`, the value-log bytes still in use;
- `sequence`, the commit counter, and `commit_uncertain`, true after a failed
  commit until Mynou restarts and recovery finishes.

These readings come from each index root, maintained by every commit; producing them reads no record.
They are sizes, not limits: Mynou reports no maximum inventory size, and
available disk space remains the physical bound.

The status never waits for a busy store and never fails because one store
cannot be read. Readings from such a store (`jobs`, `active`, `ready`, `library`,
`monitored_series`, `torrents` and the transfer byte totals) are `null`; its
`storage` entry says `busy` or shows `commit_uncertain`, and `storage_errors`
lists the store-named error of each reading that failed.

Every failed commit names its store (for example `Job storage`) and says what
happened and what to do. The error carries no path, credential or media title.

- **Full disk or quota.** Before the redo record is written the change was
  not saved and nothing else changed: free disk space, then restart Mynou.
  After it the outcome is uncertain: existing records and recovery files are
  kept, and the next start after freeing space finishes the change. A start
  that still cannot recover for lack of space says so and keeps the redo log.
- **Single record above 1 MiB**, and **a change above its operation bound**
  (16,384 entries, or 128 MiB of redo log including the index pages it
  rewrites): the change was not saved, existing records are
  unchanged, and the store remains usable. Report the item that produced it.
- **Other write failures.** Before publication the change was not saved; after
  it the outcome is uncertain. Either way reads and writes of that store pause
  until restart, which finishes recovery; keep the data directory intact.

No error evicts records, falls back to another store or deletes recovery
files. Tests inject a full disk while writing the redo record, the value-log
segment and the tree pages, and right after the redo flush; each store reopens with every committed record.

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
map. Action/value buffers, redo encoding/decoding buffers and response values
are also bounded. The height bound is 63; page allocation checks the file-offset
representation. These are source-level bounds, not measured RSS or throughput.

Unreachable old page slots and interrupted temporary object or segment files
are not reclaimed by this first increment; segment space is reclaimed by the
cleaner described under [Value log](#value-log). Repeated churn can therefore consume extra
disk space. Compaction/reclamation must be designed and verified before the
integration claims scalable routine operation. The index API itself has no
automatic cleanup or backup. A backup is a stopped-store copy,
including any recovery artifacts; a partial copy is not a backup.

## Listings and cursors

Every inventory listing returns a cursor page: pass the previous page's `next`
as `after` and stop when `next` is null. `limit` is 1 to 200
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

| Listing | API | Order | Count |
| --- | --- | --- | --- |
| Jobs | `GET /api/jobs` | newest first; `q`, `state` filters | `total` |
| Library | `GET /api/library` | newest first; `q`, `state` filters | `total` |
| Series | `GET /api/series` | identifier | `total` |
| Calendar | `GET /api/calendar` | air date, at most 367 days | `total`, null when not cheap |
| Requester demand | `GET /api/requesters/ID` | ledger order | `total` |
| Retained feeds | `GET /api/feeds/retained` | identifier | `total` (string) |
| IRC history | `GET /api/irc/announcements` | chronological | `total` |
| Transfers | `GET /api/transfers` | priority, then FIFO | `total` |
| Requests of a transfer | `GET /api/transfers/ID` | identifier (`request_ids_next`) | — |
| Search candidates | `POST /api/search` | live search ranking | — |

The requester and IRC history listings page only by `after`. Other lists are bounded by configuration
or retention and are returned whole: configured indexers (`GET /api/indexers`),
feeds (`GET /api/feeds`), IRC sources and rules (`GET /api/irc`) and requester
accounts (`GET /api/requesters`), and a job's retained history of at most 1,000
events (`GET /api/jobs/ID/events`). A library check (`POST /api/upgrades`)
examines a bounded oldest-first window and reports `limited` when more remain.

### Searching jobs and the library

`GET /api/jobs` and `GET /api/library` accept `q`, a percent-encoded text of at
most 128 characters matched case-insensitively against the title, release name,
state and identifier, and `state` (`queued`, `processing`, `downloading`,
`importing`, `ready`, `failed` or `cancelled`). A filtered request examines at most 1,000
rows after its cursor and returns the matches, `scanned` (rows examined) and
`next`, the cursor of the last examined row. A page can therefore hold fewer
matches than its limit, or none, while `next` continues the search: follow it
until it is null to search the whole inventory. The web interface searches this
way, reads a few windows at a time and offers "Search further". When jobs change
or the connection reopens, it reads the part already searched again, at most
once per second, so jobs entering or leaving the filter appear or disappear.

### Startup

`mynou serve` binds the API before opening storage. While storage opens,
`/healthz` answers `starting`, `/readyz` answers 503, `GET /api/status` returns
`{"status":"starting"}`, and every other route, including the web interface and
`/ws`, answers 503. A request from another site's page or naming another host
answers 403, as always.

## Inline values and the page cache

A key stores either a logged value (up to 1 MiB, see [Value log](#value-log)) or
an inline value of at most 32 bytes in its tree leaf (`Change::PutInline`).
Inline values take no segment space, so secondary index entries that carry
little or no payload cost one leaf slot. A key may change between the two
kinds in one transaction, and recovery replays that idempotently. Inline lengths and zero
padding are validated like any other tree field.

Decoded, checksum-verified pages are cached and shared between reads, at most
8,192 pages (32 MiB on disk, under 28 MiB decoded). That keeps every interior
page and most leaves of a 100,000-key store in memory. A commit replaces the
cached copies of the pages it wrote once its record is durable, and the cache
starts empty on every open. A page that is corrupted on disk after it was
cached is reported on the next read after a reopen, not before; the checksum
guards storage faults, not a hostile rewrite of an open directory.

## Series storage

`series.idx/` in the store directory is a private index, created on the first
save; a store that never held a series has none.

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

For native macOS arm64 measurements of every indexed store, see the
[capacity evidence record](inventory-growth.md). The figures below are the
earlier series-only Linux experiment.

One hand-run of the `series_scale_report` harness (release build, Linux
container on overlayfs, synthetic scopes of 3 seasons x 30 episodes, one reading
process per size). These are indicative container numbers: CI does not run or
enforce them, and file synchronization is cheaper there than on macOS.

| Scopes | Episodes | On disk | Open | 200-summary pages walked | Due pass | One scope save | Calendar page (200 rows) | Peak RSS reading |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 150 | 13,500 | 14 MiB | <1 ms | 5 ms | <1 ms | 3 ms | 22 ms | 6 MiB |
| 1,000 | 90,000 | 93 MiB | <1 ms | 33 ms | <1 ms | 4 ms | 27 ms | 6 MiB |
| 4,000 | 360,000 | 373 MiB | <1 ms | 135 ms | <1 ms | 9 ms | 38 ms | 8 MiB |
| 12,000 | 1,080,000 | 1.1 GiB | <1 ms | 733 ms | <1 ms | 34 ms | 42 ms | 8 MiB |

Building these inventories with the test-only bulk loader (16,384 changes per
transaction, keys in random order) peaked at 30 to 139 MiB resident; that is the
transaction bound, not steady-state use. Disk use is about 1 KiB per episode.

### Recovery

A failed write never discards stored records: pages are rewritten in place only
after the complete redo is durable, and an uncertain commit blocks the series
store until restart, when the redo is replayed. If the disk is still full,
opening reports the error and leaves the files as they are. Read-only opens
create and change nothing.

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
into the latest heartbeat.

`GET /api/jobs` and `GET /api/library` accept `limit` (1 to 200), `after`, and
`q` and `state` to [search](#searching-jobs-and-the-library), and return
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
[Indexed library monitoring](#indexed-library-monitoring)); an entry its
record contradicts is a storage error, never proof that nothing owns it.

### Job index format

The first writable start of a store that never held jobs creates `jobs.idx`
in format `mynou-jobs/5` with its counters (see [Ownership and
format](#ownership-and-format)). Any other format is refused. Disk errors leave
the published index in place; writes use the transaction/recovery protocol
described above and never evict jobs to make space.

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

The job index also lists the live owners of each transfer (split into direct
download or shared-file claims and pack or IRC references), cancelled
downloads, live acquisition addresses, the shared-file owners of each physical
video and the ready imported upgrade children of each parent. Transfer
ownership, co-ownership, claim and cancellation checks and the shared-file
binding lookup read at most two index entries and verify each against its job
record, instead of every attempt or pack mapping that ever named the transfer
or file; an entry its record contradicts is a storage error, never an absent
owner. IRC physical-video conflict checks likewise read at most two entries per
alias. Pack mappings are indexed by file and source URL or download (with the
shared flag and TMDB identity), with the ready, newest and non-upgrade versions
of each media identity and pending upgrade children, so pack conflict checks,
the media state shown for requests and the series calendar, requester media
reuse and shared replacement reservations each read at most two verified
entries. Feed routing uses a [durable captured generation](#captured-feed-routing),
committing candidates with seen state before routing and resuming checkpoints
after restart.

## Requester storage

`requesters.idx` stores requester demands and operator interests without a total
record-count or aggregate-byte limit. Only the bounded account configuration,
requested pages and explicit changed-record patches remain in memory. Demand
manifests bind checked chunks; account, media, admission, outcome and IRC-origin
references and counters change atomically with their primary records.

`GET /api/requesters/ACCOUNT?limit=200&after=CURSOR` (`limit` 1 to 200) returns `demands`, `next`,
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
at a time.

`requesters.idx` is created through `requesters.idx.creating` on the first
writable start. Read-only opens create and repair nothing; interrupted indexed
writes require a writable restart. Uncertain writes block further reads and
changes and retain recovery artifacts.

## Feed storage

`feeds.idx` retains baselines, checked counters and up to 1,024 opaque entry
identities per source, without the former 1,000-source / 4 MiB aggregate limit.
Historical sources stay on disk; startup checks configured bindings with point
reads and reconciles only active baselines. Configuration still allows at most
64 watched sources. Network and per-poll entry/routing bounds remain in force;
[source policies](#source-policy-storage) use their own index, with no fixed
retained-policy count or aggregate-byte cap.

`GET /api/feeds/retained?limit=200&after=CURSOR` (`limit` 1 to 200) lists retained summaries, including
unconfigured historical sources. Start without `after`, then follow `next` until
null. `total` and `sequence` are decimal strings. These are live key scans:
concurrent inserts before the cursor need a new listing. Pages hold at most
200 records and 4 MiB of hydrated bytes. Summaries never expose bindings,
retained identities, feed entry URLs or titles. Ordinary feed health also
reports `retained_sources`.

The first writable start creates `feeds.idx` (format `mynou-feeds/3`) through
`feeds.idx.creating`. Read-only opens create and change nothing; pending
indexed changes are replayed in memory only. Uncertain writes block subsequent
reads and changes until restart. Disk errors leave recovery artifacts in place
rather than evicting records.

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

<a id="remaining-delivery"></a>

## Capacity acceptance

Indexed retention is delivered for all seven stores, including IRC announcements
and source policies. Captured feed routing, resumable upstream paging, bounded
admission/monitoring and transfer scheduling, paginated views, startup validation
and per-store storage/backlog/freshness/error diagnostics are also delivered.
Their operation bounds remain intentional; a retained inventory is not truncated
to those windows.

The [native macOS arm64 evidence record](inventory-growth.md) describes the
three-size experiment for jobs/history, library tips, series, requesters, feeds,
source policies, IRC and transfers, including disk size, open time, a routine
storage pass, a point update, a page read and separate build/read peak RSS. It
also maps each parent acceptance criterion to its regression test and audits the
remaining intentional bounds. Numeric results and Actions links are retained on
[#217](https://github.com/alecerf/mynou/issues/217); the final acceptance table and
decision are on [#106](https://github.com/alecerf/mynou/issues/106).

These observations cover the stated synthetic workloads, not arbitrary long-term
churn, network throughput or infinite hardware capacity. Removing inventory caps
does not remove physical storage or compute limits.

## Captured feed routing

Feed format `mynou-feeds/2` adds one pending routing generation per configured
watched source, with at most 200 private captured entries and 4 MiB of encoded
payload. Each entry has its own checked object, binding source, generation and
ordinal. Candidates include the titles and acquisition URLs necessary for replay;
URLs may carry private access tokens, like saved job acquisition URLs. Headers,
cookies and standalone credential values are never captured. API diagnostics
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

The first writable start creates `indexers.idx` through
`indexers.idx.creating`. Read-only inspection never creates or repairs.

A storage failure is an error, not an absent or default-enabled policy. After
an uncertain control, runtime source access is blocked until a writable restart
recovers durable state; a failed response may mean the change committed.


## IRC announcement storage

`announcements.idx` stores immutable claims, decisions, admission intents and
routing origins on disk. The former aggregate 1,000-identity / 8 MiB limits no
longer apply. Bindings remain bounded metadata (32 retained source IDs); source,
rule, line and rate limits remain as documented in [limits](limits.md).
Duplicate checks and admission fences read the announcement by identity.
Chronological, pending and recovery tables change atomically with each record;
startup cross-checks reservations and requester/job provenance in bounded pages
before downloads start. An announcement alone never creates demand.

`GET /api/irc/announcements?limit=200` returns newest-first pages with `total`,
`announcements` and `next`. Pass that cursor as `after=CURSOR` until it
is null. Pages hold at most 200 rows and 4 MiB of stored
record data. These are live scans: new arrivals ahead of the cursor require a
new listing. A decision or receipt never
requires a whole-history clone. Transient retry state stays bounded and is not
part of retained duplicate suppression.

The first writable start with an IRC source creates `announcements.idx`
through `announcements.idx.creating`, with its bindings. Read-only inspection
never initializes, recovers redo, changes permissions or writes decisions; it
replays a pending index transaction in memory only. Corruption, public
permissions or links stop the operation. Failed writes report errors, and an
uncertain commit blocks decisions until storage is reopened.

### Series backlog and pass freshness

`GET /api/status` exposes `inventory.series` alongside the
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
network availability. A held store reports `state: busy`. An unprepared,
uncertain or unreadable store reports `state: unavailable` with a fixed recovery
hint rather than a guessed zero, scan, or raw error; other status fields remain
available. Only `state: ready` readings carry counts and timestamps.

A new store has no successful-pass timestamp until its first successful
background pass completes.

### Library backlog and pass freshness

`GET /api/status` also exposes `inventory.library` (#273),
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
The timestamp survives restart and stays null until the first successful
automatic pass. It does not claim that every library entry was checked.

Status uses at most three exact range summaries plus one point lookup for the
completion time. It never reads job payloads, walks the library or writes data.
Rescheduling, unmonitoring and replacing tips update the existing scheduling
indexes in the same transaction as the tip change. Busy storage reports
`state: busy`; an unprepared, uncertain or unreadable index reports
`state: unavailable` with a fixed recovery hint and no counts. Diagnostics contain
no titles, local paths, source payloads or credentials. The upgrade report's
`oldest_checked_at` remains a bounded-window reading; the service's
`inventory.library` reports the complete automatic due backlog.

The other #246 integrations are described below; #106 still links its
measured acceptance through #217 and [the evidence record](inventory-growth.md).

### Job backlog and claim-scan freshness

`GET /api/status` exposes `inventory.jobs` (#275) using the
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
transaction as described above. A new store reports null until the first
successful checkpoint. Idle polls within the interval do not add durable writes.

A dedicated wake-ordered secondary index changes atomically with job updates,
lease renewal/release, retries and cancellation. Status needs one exact range
summary plus two metadata point reads, never job/history payloads, queue
promotion, writes or a scan of due entries. Busy/unavailable states use the same
fixed recovery hint and omit counts; raw storage errors are not published here.

The remaining #246 integrations are described below. Increasing-size evidence
and the final acceptance table are linked from [the evidence record](inventory-growth.md).

### Requester, IRC, feed, source-policy and transfer diagnostics

Service status completes `inventory` with the following explicit scheduled units:

| Field | Unit and due membership | Successful bounded pass |
| --- | --- | --- |
| `requesters` | `requester_admission_scan`: retained pending, reserved and conflicting demand, before enabled-account, current-origin and compatibility checks | The admission window (at most 200 examined / 64 admitted), outcome window, bounded origin cleanup and cancellation reconciliation finish without error or shutdown |
| `irc` | `irc_routing_scan`: the durable pending routing table, before current rule/fingerprint, requester eligibility and transient retry checks | One scheduling page (up to 200 rows) is examined; idle/filtered passes succeed, a selected announcement must route or settle availability successfully |
| `feeds` | `feed_source_pass`: configured, enabled watched sources due under the watcher's actual monotonic schedule, including pending-generation continuation | An automatic unforced all-source window (up to four sources) completes with every attempted source returning `ok`; an empty window also completes |
| `source_policies` | `source_policy_control`: no periodic worker; `state: not_applicable`, null count, ages and freshness | None; operator controls are not background passes |
| `transfers` | `transfer_queue_scan`: unpaused, nonfailed, nonlimited transfers still needing selected payload or metadata, including running transfers retained in the durable queue | Both bounded head and rotating queue windows are read successfully, before hydration, admission and worker execution |

These queues describe scheduled work, not successful acquisition. Requester
counts may include demand awaiting bounded origin cleanup or account policy
checks. Active/ready demand follows job execution and is not new admission
backlog; upstream watchlist sweep progress remains in the requester report.
IRC counts include ineligible and temporarily backed-off candidates until their
durable decision/route removes them; no claim of immediately actionable routing
is made. Completed transfers retained for seeding are not unfinished download
backlog; tracker/DHT timers apply to the bounded live seed window, not every
retained transfer on every tick. A transfer client that has not opened reports
`state: not_started` without guessed counts. Disabled IRC routing and feeds
report zero due work and preserve existing pass freshness. Source-policy storage
failures still report `unavailable` even though scheduling is not applicable.

Requester, IRC and transfer membership and wait times change atomically with
their authoritative records. Status reads one format point, two exact tree
range summaries and one last-pass point per queue, without hydrating a record,
scanning the inventory or writing. A work item that remains in the queue keeps
its age; leaving and re-entering begins a new wait. Queue members are due
immediately: clock rollback keeps the count and clamps a future observation's
reported due time to now, with zero lateness. Every queue entry records the
wall-clock time it entered the queue; IRC uses the immutable `first_seen`
timestamp.

Feed status performs at most 64 point lookups into the bounded configured-source
runtime map plus one durable freshness lookup. It never reads retained feed
observations, generations or candidate payloads. A source's pending generation
is served instead of a new poll, using the same source scheduling slot; captured
entry/generation counts remain in the detailed feed report. Source intervals,
cooldowns and ten-second retry rests remain effective. The polling schedule is
intentionally volatile: restart makes configured sources due again, and wait
age begins at the new process's scheduling observation. A source re-enabled
after suspension has an unknown wait time until the next scheduling observation;
`unknown_due_times` makes that explicit. Durable successful-pass
freshness survives restart. Manual forced/targeted polls never advance the
automatic freshness field.

Successful requester, IRC, feed and transfer passes checkpoint freshness at most
once per 60 wall-clock seconds (also exposed as `freshness_checkpoint_secs`),
including successful idle passes. Clock rollback allows a fresh checkpoint.
The value is the last durable checkpoint, not the latest in-memory tick, a
whole-inventory traversal, or completed network work. Failed operations,
interrupted passes and shutdown preserve it. A failed commit may recover its
whole redo record; an uncertain store becomes unavailable until reopened.
Busy stores/runtime maps report `busy`; unprepared, uncertain or unreadable
indexes report `unavailable` with a fixed recovery hint and no counts. Status
publishes no account names, media titles, paths, source payloads or credentials.

Each store creates its backlog index together with its own index, so the
backlog always covers every record. A store whose records predate its backlog
index is refused rather than counted partially.

These readings complete #246; measured growth evidence and parent acceptance are
recorded in [the capacity evidence](inventory-growth.md), #217 and #106.
