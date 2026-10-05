# Native Rust architecture

```text
CLI / API / Browser       Plex watchlist
    |                          |
    +--------- Requests -------+
                   |
           Journal and snapshots
                   |
           Workers with leases
                   |
       TMDB + RSS / JSON / Torznab
                   |
       Release selection profiles
                   |
       Durable transfer queue and policy
                   |
       Native BitTorrent and verification
                   |
       Media metadata analysis
                   |
          Import without overwriting
                   |
       Plex scan and confirmation
                   |
       Current owned library entry
                   |
       Monitor + baseline + cutoff
                   |
         Controlled upgrade child
```

The components are independent of frameworks. `config` validates fields and
resolves paths relative to the configuration file into absolute roots, including
when the configuration filename is relative. `integrations` converts
network responses into requests, catalog plans and sources. `engine` orchestrates transitions
without holding the journal lock during a transfer or import.

`requesters` stores versioned account policies, per-account cursors, approvals,
quota reservations and compatible canonical demand in a private checked atomic
snapshot. Network identity/watchlist/catalog I/O stays outside requester and job
locks. Locks proceed requester-then-job. Admission synchronizes requester
provenance and quota before recording a job with immutable captured behavior in
format 4. A reserved admission without a linked job recovers by media identity;
ready files and earlier numbering/shared-group lineage retain their ownership.
Startup validates both stores before native transfers begin. Notifications record
bounded outcomes locally. See [requester policies](requesters.md).

`series` retains bounded catalog episode plans and monitoring revisions in a
private verified snapshot under the request-store directory owner. A catalog
refresh captures policy, fetches outside storage locks, verifies numbering and
known episode identities, then rejects a result if its revision has changed.
A bounded submission batch takes locks in series-then-request order and uses
existing media identities to prevent duplicate episode jobs. Series and request
persistence are separate commits, so confirmed partial batches remain recoverable
through deduplication. Unknown dates and missing episode identities do not
automatically acquire. A background worker refreshes due records; calendar
reads sort bounded borrowed rows and serialize only the requested page. See
[series monitoring](series.md) for exact budgets and supported mappings.

`pack` prevalidates explicit catalog/file mappings and source-key conflicts
before recording separate episode jobs. Each job retains its mapped
path while the native client deduplicates their common torrent identity. Once
the required pieces and selected file roots verify and synchronize, import
retains and analyzes only the exact selected file. Whole-torrent readiness and
seeding remain gated by complete payload verification. An absent selection cannot fall back to another video. Existing
media identities are reused rather than automatically reopened. Source paths
and mappings remain independent of series monitoring policy. See [packs](packs.md).

`pack::automatic` captures an eligible missing-episode scope, separately assesses
season titles under the episode profile and resolves bounded authenticated
metadata outside storage locks. Preview uses no native transfer queue or payload
requests. Catalog content, policy, UTC date and missing identities form the scope
fingerprint; a resolved decision also binds torrent hash and exact mapped paths.
Apply rechecks the scope under series-then-request locks and persists immutable
origin provenance with each mapped job before workers can proceed. Native queue
publication requires the source's authenticated hash to match that provenance.
Pack title provenance does not establish individual upgrade baselines. Optional
monitored pack preference shares catalog/search deadlines and the 64-job batch
with ordinary fallback. See [automatic packs](automatic-packs.md).

`selection` evaluates matched source candidates using the configured movie or
episode profile. It extracts bounded release-title attributes, filters candidates
and ranks accepted releases deterministically. Automatic acquisition and search
previews share this decision path. Preview serialization exposes opaque IDs and
assessments without acquisition URLs; a preview does not acquire the journal
owner lock or create a job.

`pack::shared` authenticates one explicitly selected video and binds its full
consecutive canonical owner range. Guarded apply commits every new owner in one
journal frame, with one immutable library destination. Group claims are
serialized; native hash/path verification and atomic import reuse preserve that
destination across restart and cancellation. Each owner confirms the exact Plex
path separately. Media/physical ownership indexes are rebuilt from verified jobs.
Shared owners remain outside individual baseline/upgrade logic. See
[shared files](shared-files.md).

`library::groups` reviews complete baselines and authenticated one-file
replacements under the current episode profile. `store::groups` persists their
immutable full parent set and per-owner lineage in format 3 transactions. Each
replacement confirms its exact path and becomes staged; the final confirmation
promotes the complete group in one synchronized frame. Staged jobs cannot hide
old library tips or be claimed as ordinary work. Group cancellation/retry covers
every child and monitoring choices fence claims/promotion. Memoized iterative
root traversal keeps projection bounded across histories. See
[group upgrades](group-upgrades.md).

Ready imports form the owned-library view. Monitoring checks the current ready
entry's recorded release title against source candidates under the current
movie/episode profile. Profile acceptance is compared before rank: a baseline
rejected by today's policy can be replaced by an accepted candidate, while an
accepted baseline requires a strict improvement in custom score or ordered
attribute preferences. Seeds alone cannot trigger an upgrade. Optional
cutoffs follow resolution preference order. Global background checks are disabled
by default, and entries without a baseline remain ineligible.

An applied upgrade is a deduplicated child request. Its parent stays current
until the child becomes ready; failed or canceled children cannot hide the
parent. An unrelated same-media request cannot become ready while an upgrade is
pending; cancel the pending upgrade before promoting the manual alternative.
Promotion inherits the parent's current monitoring choice. Check timestamps and
per-entry monitoring policy are durable. Bounded passes consider the oldest checks first. Background checks honor per-entry
polling intervals; manual checks ignore them. Applied passes persist timestamps
even after search failure for backoff. Preview passes contact indexers without
journal writes or request submission. The 90-second search/pass budget covers
HTTP/socket operations and processing checks. Standard-library synchronous DNS
can stall beyond the deadline; late results are rejected.

`store` synchronizes each transaction before confirming it. Records form a
SHA-256-verified chain: an incomplete tail after interruption is recoverable,
while corruption of a complete record is reported. A file lock prevents two
owners of the same journal. Workers use leases and renew ownership during long
operations. Offline library listing and upgrade previews use a read-only store:
no directory/file creation, permission changes, compaction or tail repair. A
fresh store returns no entries; an interrupted tail requires explicit writable
recovery. Reader access excludes a concurrent writer.

`torrent` retains verified identities and metadata, rechecks pieces after
restart, validates file names and size limits, and then exposes a ready state.
Bytes merely existing on disk do not make a torrent ready. A hybrid torrent must
satisfy both v1 hashes and v2 roots before final publication.

Parallel payload work uses one coordinator per active transfer and a bounded
number of TCP peer workers. The coordinator owns piece claims, verified writes
and completion. One peer owns an in-flight piece at a time; failed ownership
returns the piece for another attempt. Only verified data can update readiness,
and final torrent verification remains required. A file-priority change affects
the next claim rather than canceling an already claimed piece.

`downloads.max_peers` is a per-transfer ceiling with a default of four and a
range of one through eight. Global worker and per-transfer resource bounds may
reduce the effective count. Known usable peers can start while bounded discovery
work proceeds. Private torrents retain their discovery restrictions; magnet
metadata is authenticated before parallel payload acquisition. A retired
coordinator joins all workers before exiting, and completion joins them before
publishing readiness. Verified writes and metadata publication check generation
ownership while holding the native control mutex; pause waits for an ongoing
disk write. Result sends poll cancellation even when their bounded queue is full.
Corrupt-peer classification remains available after a worker exits.

Transfer controls belong to the native engine and use native transfer IDs, which
are distinct from request IDs. Durable user pause state is separate from
retryable internal interruptions; request retries cannot clear a user pause.
Queue selection uses priority then FIFO without preempting active transfers.
Per-file priority orders the required pieces. Durable native file interests
form an additive union across mapped requests; ordinary acquisitions require all
files. Selection expansion retires the old generation under the same verified
write mutex. Missing paths never select another file. The coordinator authenticates
v1 boundary pieces and selected v2 roots before publishing synchronized file
availability, independently of whole-torrent readiness. Boundary neighbors retain
required bytes in normal confined files, and restart rehashes them. Partial
transfers advertise an empty bitfield and do not seed or announce completion.

Global payload bandwidth limits cover shared download/upload activity.
Per-transfer rate caps add restrictions without bypassing the global cap. Rate
buckets allow a bounded 16 KiB burst. Local policy objects replace configured
seeding defaults in full rather than patching omitted fields.
Persisted payload counters and seeding elapsed time support ratio/time policies.
The ratio budget uses verified non-padding payload size; whole-block reservations
cannot exceed it. Elapsed time counts online seeding availability, including idle
time, while ready, enabled, unpaused and not seed-limited. Accounting is coalesced
on a one-second interval and flushed on clean shutdown;
abrupt failure may lose unflushed increments. Policies retain all downloaded
sources and imports. See [transfer controls](transfers.md).

`media` finds metadata using buffered reads and file seeking. MP4 `mdat` blocks,
Matroska clusters of known size, and WAV payloads are not loaded into memory.
Individual metadata reads are limited to 8 MiB, total metadata to 64 MiB, and
element count to 100,000. Counters, sizes, and parent boundaries are checked.

`organizer` publishes media without overwriting an existing file. It prefers a
hard link and uses a synchronized copy when filesystems differ. Sources remain
intact, and symbolic links on controlled import paths are rejected. Upgrade
imports add a unique job-ID suffix rather than overwriting the earlier version.
Old imports and downloads remain; there is no automatic cleanup.

With Plex enabled, an upgrade becomes ready only after a fresh response reports
the new imported path in `Part.file`. Optional mappings translate Mynou's path
prefix to Plex's, using whole lexical components and the longest matching
prefix. A pre-existing matching title is insufficient for upgrade confirmation.
Initial Plex skips do not create an owned import or invent a release baseline.

`net`, `tls`, `pki`, and `crypto` implement HTTP/1.1, the TLS client, X.509
validation, and the required primitives. Protocol errors are explicit; a failed
negotiation never disables certificate validation. The API server uses HTTP on
the local interface with a Bearer token. See [protocol limits](limits.md).

`web` shares that listener and directly calls existing engine operations.
Original Rust-rendered HTML and an embedded stylesheet provide native browser
forms without JavaScript or third-party assets. Bounded in-memory sessions use
random opaque cookies and independent form tokens, rotate after login and
expire at an absolute deadline. Session locks never span engine/network work.
Host/origin checks and strict form decoding precede mutations. Bulk identifiers
are all prevalidated, then each engine operation reports its own outcome.
Public labels are bounded/redacted and escaped before insertion into HTML;
acquisition URLs and API tokens are not rendered. See [browser management](web.md).

GitHub Actions validates changes and publishes releases. Build and release tools
are separate from the runtime; the application never invokes them.

[Selection policies](selection.md) · [Library monitoring](library.md) ·
[Transfer controls](transfers.md) · [Series monitoring](series.md) ·
[Release stages](roadmap.md)
