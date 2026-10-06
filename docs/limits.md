# Supported formats and explicit limits

## Media analysis

Analysis reads container metadata. It does not decode pictures, audio samples,
or subtitles, and does not prove that a player can decode the whole file.
Unknown formats, inconsistent sizes, or excessive metadata produce an explicit
English error.

| Container | Analyzed information | Limits |
| --- | --- | --- |
| MP4/MOV | `moov`, tracks, codec descriptions, dimensions, timeline, languages, iTunes title/date, AAC configuration | Requires `moov`; no standalone `moof` fragment analysis without initialization; no DRM decryption |
| Matroska/WebM | EBML, Info, Tracks, global tags, dimensions, audio rate/channels, languages | Unknown sizes are accepted for Segment and allow stopping at an unknown-sized Cluster; later metadata remains reachable through SeekHead references; no duration reconstruction from every packet |
| AVI | RIFF `hdrl`, `avih`, `strh`, `strf`, INFO titles | Timeline from the main header; no complete OpenDML/AVIX indexing or frame decoding |
| WAV/RF64 | `fmt`, `ds64`, `data`, INFO title/date | Duration from audio byte count and declared rate; compressed variants can give an indicative duration rather than decoded sample count |
| FLAC | STREAMINFO, sample count, Vorbis TITLE/DATE comments | No audio block decoding or sample MD5 verification |
| MP3/MPEG audio | Two consistent frame headers, ID3 v2.2–v2.4, Xing/Info/VBRI | Duration only when described by an index; otherwise unknown; encrypted, compressed, or unsynchronized tags are not interpreted |

Embedded titles take precedence over filenames. Date stays absent unless usable
metadata describes it. Unknown codecs retain their container identifier. Missing
values remain `null` in JSON. Analysis is bounded to 8 MiB per metadata read,
64 MiB total, and 100,000 elements. Large media payloads are skipped with `seek`.

## BitTorrent

The engine implements v1, v2, and hybrid torrents, the TCP peer protocol,
metadata exchange, v2 hash proofs, HTTP/HTTPS/UDP trackers, a DHT client, and PEX
reception. Concurrency is bounded by configuration. It does not implement uTP,
WebRTC/WebTorrent, webseeds, UPnP/NAT-PMP, or a complete persistent DHT table.

Each transfer uses a bounded number of parallel TCP peer workers, controlled by
`downloads.max_peers`: default four, accepted range one through eight. Setting
one retains a single-peer baseline. Global worker and per-transfer resource
bounds may reduce concurrency. Each peer can pipeline up to 16 blocks when
unlimited; payload rate gating reduces the request pipeline when a cap applies.
The coordinator claims a piece exclusively and publishes verified data; failed
pieces are reclaimed without duplicate endgame requests. A peer count is a
ceiling, not a guarantee of connections or higher throughput.

Discovery caches at most 1,024 peers per torrent. Known usable peers can start
while background discovery proceeds. Seeding announces `started`, `completed`, and
`stopped` events and respects tracker intervals, bounded between 30 seconds and
24 hours. A forced stop does not guarantee a `stopped` announcement. Download and
upload counters describe content payload bytes, not all interface traffic.
Since 0.9.0, counters and seeding elapsed time persist across clean restart.
Updates are coalesced on a one-second interval and flushed on clean shutdown;
an abrupt crash can lose the latest unflushed increments. This accounting does
not claim exact crash durability or equivalence with a tracker's records.

Durable user pauses affect all requests sharing a transfer and are not undone
by automatic retries. Queue scheduling prefers higher priority, then FIFO, and
is nonpreemptive. File listings keep original metadata indices after filtering
padding, so indices can have gaps; file-priority controls reject padding indices.
File priorities change the order of required pieces, with no `skip` priority.
Mapped pack transfers can acquire an additive union of at most 1,024 exact paths
with a 1 MiB aggregate path limit. v1 selection also needs boundary bytes from
neighbor files; v2/hybrid selected roots remain mandatory. Partial availability
does not imply whole-torrent readiness, seeding or a tracker completion event.
Existing full acquisitions remain full; selections do not contract or remove
previously requested bytes. Global bandwidth limits
cover aggregate content payload; protocol/metadata overhead is outside those
caps. Each aggregate rate bucket allows a bounded 16 KiB burst. Per-transfer
rate caps cannot bypass the global cap. Local policy objects replace configured
seeding defaults in full; omitted seed fields mean no local cap, while clearing
the override restores configured defaults.

The seed-ratio denominator is verified non-padding torrent payload size, not the
download counter. Whole upload blocks cannot exceed the remaining ratio budget,
so seeding may stop below the nominal ratio. Elapsed seeding time counts ready,
unpaused online availability, including idle time; it excludes offline, paused
or seed-limited time. Older records without accounting start at zero rather than
inventing past activity. Policies retain downloads/imports. See
[transfers.md](transfers.md).

A magnet cannot reveal its private flag before metadata arrives. When a tracker
or `x.pe` is provided, public discovery waits for torrent classification. A magnet
without such a hint can discover peers through DHT, then stop public discovery if
the metadata marks it private. For a private acquisition whose confidentiality
must be known immediately, use its `.torrent` file or a magnet with its private
tracker.

Recovery rechecks local data. A modified file does not become ready merely by
existing. Unsafe paths and contradictory metadata are rejected. Canceling one
request does not necessarily stop a torrent shared by another request.

## HTTP, TLS, and integrations

The client implements HTTP/1.1 with size limits, deadlines, redirects, and
HTTP/CONNECT proxies. It does not negotiate HTTP/2 or HTTP/3. The local API accepts
HTTP/1.1 requests with a declared content length and rejects chunked request
bodies and ambiguous headers.

The TLS client uses TLS 1.3, X25519, and ChaCha20-Poly1305/SHA-256 with certificate
and hostname validation. Certificate signatures support RSA and ECDSA P-256/P-384
with SHA-256/SHA-384. Unsupported algorithms or critical extensions produce an
error; there is no unauthenticated fallback. Primitives are checked using vectors
and local protocol tests, without claiming an independent cryptographic audit.
X.509 NameConstraints is not implemented and causes explicit rejection. The
client does not consult CRLs or OCSP servers.

Plex, TMDB, and indexers need valid addresses and credentials. Local-response tests
do not mean the release has connected to your personal installation. Source
selection is bounded by implemented formats and matching criteria; it does not
perform a general Web search. Search, upgrade and series-refresh passes have a 90-second budget
for HTTP/socket operations and processing checks. Synchronous standard-library
DNS resolution can block beyond it, so this is not a strict wall-clock deadline;
late results are rejected.

Legacy TMDB enrichment responses are cached for one hour, with at most 256
entries and 32 MiB. Durable series planning bypasses that cache and retains future
and undated episodes. Newly aired acquisition requires a known episode identity
and monitoring policy; optional season-zero specials require explicit opt-in.
Plex availability confirmation always uses a fresh network response. Automatic
source selection requires a strict title match and an identified file for the
requested episode. [Automatic pack search](automatic-packs.md) uses a separate
season-title assessment and authenticated metadata mapping; unresolved numbering
or title variants are rejected. Explicit [pack mappings](packs.md) select exact verified torrent
paths for known aired catalog episodes, with 1–64 distinct files per submission
and no fallback to a different video. See
[series monitoring](series.md) for refresh, numbering and scheduling limits.

## Selection and library management

Named movie/episode profiles govern release selection. Resolution,
source, codec and language markers are inferred from the matched release-title
suffix. Required/blocked token phrases and additive scores filter candidates;
custom score, ordered preferences and seed count determine the ranking. See
[selection.md](selection.md) for the supported values and preview commands.

These markers are claims in a name. Selection does not verify actual audio
languages or decode video before acquisition. `VOSTFR` does not identify French
audio, and `MULTI` does not establish which audio languages are present. A
restricted attribute rejects unknown markers by default unless its explicit
`allow_unknown_*` flag is enabled. Empty attribute lists remain unrestricted.
Conflicting recognized markers and explicitly malformed/unsupported resolution
markers reject a candidate only when the corresponding attribute is restricted;
an `allow_unknown_*` flag does not override those contradictions. Unrestricted
attributes retain marker issues in the preview without changing acceptance.
Score totals below `minimum_score` are rejected; its default of zero means a
negative total needs an explicitly lower threshold to remain eligible.

Profiles apply to automatic source search. A source explicitly provided through
`--url` or `--path` is not a newly searched candidate. Search previews contact
configured sources but do not submit jobs or start torrents, and their public
reports omit acquisition URLs and credentials.
Preview reports display at most 1,000 candidate rows with explicit count and
truncation fields. Retained candidate data is limited to 16 MiB; exceeding this
budget reports an error instead of choosing from an incomplete set.

0.8.0 adds owned-library views, per-entry monitoring, resolution cutoffs and
controlled upgrades. Background monitoring defaults to disabled. Older or
explicitly submitted imports without a release baseline are ineligible until
you supply a release-title claim. An initial Plex skip does not adopt that file
as an owned import. The library view is not a scan of all existing Plex content.

Upgrade comparisons use the current movie/episode profile for both the baseline
and candidates. Acceptance is compared first: an accepted candidate may replace
a baseline rejected by the current policy even with a lower raw rank. If both
are accepted, a strict quality-rank improvement is required. A seed-count
increase alone is insufficient. A cutoff stops
further upgrades when an accepted baseline reaches that resolution or an earlier
position in the configured preference order; it is not a numeric resolution
threshold. These decisions still rely on release-name claims.

Upgrades create child requests and unique imported filenames, preserving the
earlier ready entry until a child is ready. With Plex enabled, confirmation
requires the new file's `Part.file` path, optionally translated by configured
path mappings. Failed or canceled children leave the earlier entry current.
An unrelated same-media manual request cannot become ready while an upgrade is
pending; cancel the pending child before completing that alternative. Promotion
inherits the parent's current monitoring flag. Original imports and downloads
remain on disk; there is no automatic cleanup, rollback deletion or
library-directory adoption. See [library.md](library.md).

[Requester policies](requesters.md) support up to 32 retained account identities
and 32 configured destinations, 10,000 retained canonical demands, 512 watchlist
and expanded items per account, a 64-admission pass and a 16 MiB checked snapshot.
Each account shares a ten-second identity/watchlist/catalog budget within a
90-second poll pass. Unattempted accounts keep their older attempt timestamp for
subsequent priority. A poll advances only its own successful full snapshot; failed
accounts retain interests and cursors. New accounts require explicit opt-in.
Quotas count unique canonical requests per account and UTC-day admissions, with
ready/cancelled initial acquisitions releasing active capacity. Library maintenance
and explicit operator submissions remain separate from new requester admission.
Notification preferences select bounded recorded outcomes, without external
transport. Requester polling acquires aired episodes individually; optional
operator season-pack automation retains its earlier rules. Removed/rejected
requester demand remains a tombstone and is not silently revived. Ready imports
stay present. New reuse of uncaptured operator work requires ready regular-file
imports beneath the selected destination and a compatible recorded quality
baseline, or the unrestricted default profile when no baseline exists. Pending
operator work stays an uncharged conflict. Complete CI and publication passed
for [v0.19.1](validation.md#recorded-0191-ci-evidence); later changes need their
own completed run.

Series storage supports 128 tracked scopes, 2,000 episodes per plan, 20,000
episodes total and an 8 MiB verified snapshot. A due pass checks at most four
records and a submission batch at most 64 missing aired episodes. Calendar pages
accept at most 200 entries over an inclusive 367-day window. Settings revisions
prevent a late refresh from applying an earlier monitoring policy. Series
monitoring retains existing requests and files; it does not silently retry
terminal jobs or stop when a Plex watchlist entry disappears. Changed known catalog numbers require an explicit [numbering decision](numbering.md).
Retained IDs cannot be replaced or reassigned. The CLI/API/browser accept explicit
alternate/absolute source labels; automatic anime-order inference and
automatic range inference remain later work. Explicit packs can map exact source paths
to canonical episodes. Mapping fields cannot be changed by workers, and earlier unmapped jobs retain ordinary behavior. Do not downgrade
storage containing mapped jobs to an earlier binary that ignores those fields.

Automatic packs require one unique explicit numbered video for every eligible
missing episode in one season. At most eight ranked candidates undergo metadata
decisions; metadata listings are limited to 1,024 files and 1 MiB of path bytes.
The 90-second search budget and ten-second metadata attempt budget also apply
to opt-in monitored pack preference within its shared catalog deadline. Magnet
inspection uses up to eight outbound peers and four trackers, no payload and no
DHT/PEX fallback. Metadata-only tracker queries may be declined. Preview/apply
guards bind scope, candidate, hash and paths; queued sources must retain the
authenticated hash. Pack provenance does not establish episode upgrade quality.
See [automatic packs](automatic-packs.md) for exact filename and discovery bounds.

Shared files explicitly bind one authenticated video to 2–64 consecutive owners
in one canonical season. A later request can reuse any nonempty owner subset;
the full physical binding cannot be extended or reassigned. One synchronized
transaction records all new owners, and group claims serialize import work.
Preview uses the metadata-only discovery bounds above with a 60-second action
deadline. The CLI mapping is capped at 512 KiB; API bodies at 1 MiB; browser fields
at 8 KiB. Shared formats reject older readers. Individual remaps, baselines and
upgrades remain blocked for individual shared owners. Coordinated replacement
uses the complete captured owner range, one new shared video, group baselines and
staged confirmations before atomic promotion. It requires all parents to remain
current and monitored, an accepted quality improvement and a new URL/torrent hash.
Whole-group cancellation/retry retains bytes and rejects obsolete parents. Format
3 prevents silent downgrade. Automatic group search, splitting into individual
files and range inference remain later work. See
[shared-file ownership](shared-files.md) and [group upgrades](group-upgrades.md).

The browser interface uses a shared operator token, original server-rendered
pages and native forms, with page refreshes rather than live streaming. It has
bounded pagination and bulk job/library/transfer/series controls. It does not edit
configuration or adopt a complete existing Plex library. Plex requester policies
provide reviewed approvals, quotas and routing with recorded notification outcomes;
requester self-service remains later work. Explicit native HTTP notification routes
passed complete 0.20.7 CI/publication; see notifications.md.
Indexer integrations support RSS/JSON/Torznab endpoints. Native Basic, Bearer and
explicit single-cookie form authentication, bounded renewal, request intervals
and redacted health are implemented in 0.21.0 and await their own exact CI. Login
redirects, CSRF/CAPTCHA/interactive flows, arbitrary cookie jars and a general
tracker adapter catalog remain unsupported; Usenet follows later. Opt-in IRC reception
supports an explicit strict JSON envelope and review rules, with eight sources,
64 rules, 1,000 retained identities and an 8 MiB checked snapshot. Duplicates do
not rewrite history; full history rejects new identities without pruning.
Acknowledgement and dismissal create no acquisition work. The 0.20.1 increment adds explicit grab rules
and hash-pinned metadata verification for existing approved requests, immutable
origins and exact file imports. Catalog claims must agree with admitted labels;
this does not prove semantic media identity. Queued compatible jobs wait for
IRC, with no unsolicited demand or implicit search fallback while the rule is
enabled. The 0.20.3 increment adds configurable fixed-delimiter text formats
requiring complete explicit catalog/hash claims. It does not resolve title-only
provider messages. The 0.20.2 increment adds required SASL PLAIN with bounded capabilities
and credentials; remote use requires verified TLS, and failure has no
unauthenticated fallback. Only PLAIN is supported; credentials retain their
UTF-8 bytes without SASLprep or Unicode normalization. IRC pack/upgrade actions
and broader tracker text adapters,
broader notification transports, cross-seeding and broader bulk automation remain later work.
See [IRC behavior](irc.md) for protocol, deadlines and recovery. Complete
validation/publication passed for v0.20.3; later commits require their own complete CI.
The 0.20.4 NickServ increment requires exact configured sender/account notices
before membership. It supports the explicit IDENTIFY account/password command,
with bounded ASCII credentials and no interactive fallback; complete validation
and publication passed for that source with 560 Rust tests. The 0.20.5 selector
increment binds grabs to one compatible approved requester and passed complete
CI/publication with 568 Rust tests. The 0.20.6 implementation adds guarded new
demand with fresh catalog/account confirmation, explicit persistent origins,
approval/quotas and checked recovery. Request review uses a shared ten-second
network budget; synchronous DNS can exceed it and late results are rejected.
Title-only, missing/future catalog facts, ambiguous retained series and mismatched
source labels stay unresolved. Aborted admission intents are terminal and never
replayed. New origin semantics use requester snapshot format 2 and IRC format 3;
old binaries cannot safely read those files. Complete v0.20.6 CI/publication passed
with 585 Rust tests. Notification route/event semantics require requester format 3
and IRC format 4. Delivery is at-least-once, limited to 32 routes and 1,024 events
per store, eight attempts and eight events per dispatch. Only terminal events can
be pruned; full live capacity rejects a new owning outcome rather than dropping
work. No redirect or unbounded retry is permitted. HTTP calls have a five-second
budget; synchronous DNS can exceed it. Receiver deduplication is required.

The [roadmap](roadmap.md) separates these capabilities into future releases.

## Persistence and platform

The Rust journal replaces SQLite and requires a single owner of its directory.
Go-release files stay separate; no silent schema or torrent migration occurs.
Preserve the library and downloads when changing versions.

Offline library listing and upgrade previews use read-only storage. They do not
create directories/files, change permissions, compact or repair the journal.
Fresh storage returns an empty view; an interrupted tail reports explicit
writable recovery is needed rather than changing data during a preview.

Series metadata uses a separate private `series.json` snapshot under the same
directory owner, with file/rename/directory synchronization. Request and series
writes are separate commits; a partial confirmed acquisition batch is retained
and deduplicated after restart. An absent series snapshot means no tracked
series; earlier episode jobs are not adopted as monitoring records. Read-only
views do not create or mutate it. See [series persistence](series.md).

History retains the most recent 1,000 events across the journal, then filters by
request for `events`. Requests and their state remain in snapshots; event history
is not an unlimited archive. Automatic compaction is attempted at 4 MiB of journal
data. Records and snapshots are limited to 16 MiB, and the journal to 64 MiB;
exceeding a limit produces an error rather than unbounded growth.

The Docker delivery target is Linux x86_64 with musl. The project installs no Unix
signal handler through FFI: authenticated API shutdown is graceful, while journal
recovery handles forced interruptions. Security functions using `/dev/urandom`
require a system providing that source.

Explicit alternate/absolute [numbering](numbering.md) is implemented in 0.16.
Retained canonical identities cannot be reassigned; new jobs capture approved
source labels. Multi-episode physical ownership remains the next release stage.
