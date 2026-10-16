# Inventory storage

The `mynou::inventory::Index` Rust API is the storage foundation for
[growing inventories](https://github.com/alecerf/mynou/issues/106). Series,
episode and numbering storage uses it (see [Series storage](#series-storage)).
Jobs and requester demand also use private disk indexes. Retained feed state and source policy use their own indexes. IRC
snapshots retain their own files and fixed limits; see [limits](limits.md).

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
headers; one due-work pass reads one tree path plus its (at most four) rows;
scope and TMDB lookups read their own entries; calendar windows are cursor
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

`mynou jobs` and `mynou library` accept `--limit 1..200` and `--after CURSOR`.
`GET /api/jobs` and `GET /api/library` accept `limit` and `after` and return
`jobs`/`library`, `next` (null at the end), and `total`, plus a string `sequence` identifying changes. WebSocket requests use the
same routes; live topics send the first page, and the embedded interface loads
older pages on demand. Loaded tails refresh in place when records change; page membership changes and
reconnects reset the loaded window.
Pages have a 4 MiB hydrated-job byte budget, except one larger record may appear
alone. Existing network and JSON limits still apply; large individual records
can require direct store inspection instead of an API response.

Claims examine bounded queue windows and resume past ineligible jobs. Feed
routing and requester cancellation sweeps also retain an in-memory cursor.
Transfer retirement settles at most 64 jobs (up to eight transfer debts each)
per pass. Indexed ownership
predicates page through the matching physical identity before concluding it is
unowned; a response page is never used as an exhaustive ownership proof.

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

Before upgrading, make a complete stopped-store backup if downgrade may be
needed. Earlier binaries reject formats 2 and 3 and the pending marker.
Downgrade requires restoring that complete pre-upgrade backup, or the matching
original legacy snapshots as described above; subsequent changes are only in the newer
index. Never edit the index marker to bypass this rejection. Feed routing still
needs a durable captured routing generation: merely moving its library cursor
would lose transient releases after the feed marks them seen.

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
64 watched sources. Network, per-poll entry/routing and separate `indexers.bin`
policy snapshot bounds remain in force.

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

Durable captured routing generations are still required to route transient
announcements across every inventory page.

## Remaining delivery

Issue #106 still needs fair bounded scheduling across remaining
admission/monitoring paths, upstream paging and IRC inventory migration, and
backlog and freshness diagnostics (due backlog, oldest unserved entry, last
completed pass) beyond the [storage diagnostics](#storage-diagnostics-and-errors). Jobs, series and requester demand have left their
fixed total inventory limits; IRC and source snapshots retain their documented
bounds. Validation uses original synthetic fixtures in GitHub Actions on the
supported macOS target.

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
