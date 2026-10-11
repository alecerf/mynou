# Inventory storage

The `mynou::inventory::Index` Rust API is the storage foundation for
[growing inventories](https://github.com/alecerf/mynou/issues/106). Series,
episode and numbering storage uses it (see [Series storage](#series-storage)).
The request journal, requester ledger and the other snapshots still use their
own files and fixed limits; see [limits](limits.md).

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

`identity.bin` holds a random 256-bit store identity and checksum. `tree.bin`
contains 4 KiB checksummed pages. Page zero holds the identity, sequence,
root, allocation counter and record count; remaining pages hold ordered keys
and either value references or child references. Nodes have at most 32 items;
child levels strictly decrease and child upper keys must match their parents.
Offsets/counts use checked arithmetic, supported offsets fit a signed 64-bit
file offset, and there is no configured total inventory cap.

Keys are 64-byte ordered binary values. Callers must define table/range
prefixes and identity encodings. Values have at most 1 MiB. An object is
addressed by its full key under `objects/XX/REMAINING_HEX`; reads do not list
the object directory. Its envelope binds the key, length and checksum, and
the tree records its content digest. Checksums detect corruption, not malicious
rewrites by somebody who can replace state and its checksums.

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
3. Write private temporary objects, synchronize them and rename them to their
   exact key paths; apply deletions. Synchronize every affected object directory
   and `objects`. Apply page images and the root, then synchronize the tree and
   store directory.
4. Remove the published WAL and synchronize the store directory before
   acknowledging or preparing another transaction.

Any error after staging begins blocks **all reads and writes** until reopen.
The caller must inspect recovered state rather than automatically retrying:
a failed commit response can mean a committed transaction. Validation failures
before staging leave the existing store usable and unchanged.

Writable open reads and validates the entire bounded published WAL before any
repair. Its store identity, old/new metadata, sequence, page images, actions
and resulting key references must agree. A valid redo may replace torn root,
page or object content only for the images/keys it covers. Links, wrong types
and public permissions remain errors. Complete corruption, truncated published
WALs, cross-store WALs and stale roots are errors, with no repair. Replaying a
valid transaction is idempotent. Only unpublished staging may be discarded.

Read-only open never repairs, creates, deletes or changes permissions. A pending
transaction or unpublished staging reports that writable recovery is required.
An interrupted initial creation is an explicit error, not a reason to initialize
over an existing directory. Legacy journal/snapshot directories are never
accepted as empty instances of this format.

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

Unreachable old page slots and interrupted temporary object files are not
reclaimed by this first increment. Repeated churn can therefore consume extra
disk space. Compaction/reclamation must be designed and verified before the
integration claims scalable routine operation. The index API itself has no
automatic cleanup or backup; migration and downgrade of series data are described
under [Series storage](#series-storage). A backup is a stopped-store copy,
including any recovery artifacts; a partial copy is not a backup.

## Inline values and the page cache

A key stores either a record file (up to 1 MiB, one file per key) or an inline
value of at most 32 bytes in its tree leaf (`Change::PutInline`). Inline values
need no object file and no per-record synchronization, so secondary index
entries that carry little or no payload cost one leaf slot. A key may change
between the two kinds; the record file is removed or created in the same
transaction, and recovery replays that idempotently. Inline lengths and zero
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
the same container; CI prints the time of that migration (debug build, macOS) in
its test log as `series-migration-at-old-limits`, but nothing asserts it, and the
start-up delay on a real macOS machine has not been measured.

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

## Remaining delivery

Issue #106 still needs the job/history/requester and ownership indexes, paginated
management views for every inventory, fair bounded scheduling in all admission
and monitoring paths, and resource/backlog diagnostics. Only the series store has
left the fixed total limits; do not read this page as evidence that the request
journal, requester ledger or library views are unbounded. Validation uses
original synthetic fixtures in GitHub Actions on the supported macOS target.
