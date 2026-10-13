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
continue through persisted cursors. A successful complete upstream poll publishes
an origin generation, and a later poll waits until its retirement pass finishes.
Failed or partial reads never authorize removing an unseen origin. The upstream
512-item/expanded-request batch limit remains.

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
resource/backlog diagnostics. Jobs, series and requester demand have left their
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
