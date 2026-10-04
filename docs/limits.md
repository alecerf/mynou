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

Series storage supports 128 tracked scopes, 2,000 episodes per plan, 20,000
episodes total and an 8 MiB verified snapshot. A due pass checks at most four
records and a submission batch at most 64 missing aired episodes. Calendar pages
accept at most 200 entries over an inclusive 367-day window. Settings revisions
prevent a late refresh from applying an earlier monitoring policy. Series
monitoring retains existing requests and files; it does not silently retry
terminal jobs or stop when a Plex watchlist entry disappears. Changed known
episode identities require a mapping decision; an alternate-number mapping
editor is not implemented. Explicit packs
can map absolute/anime-style filenames to canonical episodes; general numbering
rules and multi-episode videos remain future work. Mapping fields cannot be changed by workers, and earlier unmapped jobs retain ordinary behavior. Do not downgrade
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

The browser interface uses a shared operator token, original server-rendered
pages and native forms, with page refreshes rather than live streaming. It has
bounded pagination and bulk job/library/transfer/series controls. It does not edit
configuration or adopt a complete existing Plex library. Plex integration
does not provide multi-user approvals, quotas, permission policies, notifications
or per-user routing. Indexer integrations support RSS/JSON/Torznab endpoints,
not a general tracker adapter catalog, interactive logins, Usenet, or IRC
announcement rules. Cross-seeding and broader bulk automation remain unimplemented.
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
