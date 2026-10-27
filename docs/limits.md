# Supported formats and limits

This page lists what Mynou supports, its fixed bounds and what it does not do.
The guides explain how to use each feature; when a bound is exceeded, Mynou
reports an explicit English error instead of growing without limit.

Several network operations have time budgets. Standard-library DNS resolution
is synchronous and cannot be interrupted, so a slow resolver can exceed a
budget; results that arrive late are rejected. Budgets are therefore not strict
wall-clock deadlines. The same applies to blocking filesystem calls.

## Media analysis

Analysis reads container metadata. It does not decode pictures, audio samples
or subtitles, does not prove that a player can decode the whole file and does
not replace Plex transcoding. Unknown formats, inconsistent sizes or excessive
metadata produce an error.

| Container | Analyzed information | Limits |
| --- | --- | --- |
| MP4/MOV | `moov`, tracks, codec descriptions, dimensions, timeline, languages, iTunes title/date, AAC configuration | Requires `moov`; no standalone `moof` fragment analysis without initialization; no DRM decryption |
| Matroska/WebM | EBML, Info, Tracks, global tags, dimensions, audio rate/channels, languages | Unknown sizes are accepted for Segment and allow stopping at an unknown-sized Cluster; later metadata remains reachable through SeekHead references; no duration reconstruction from every packet |
| AVI | RIFF `hdrl`, `avih`, `strh`, `strf`, INFO titles | Timeline from the main header; no complete OpenDML/AVIX indexing or frame decoding |
| WAV/RF64 | `fmt`, `ds64`, `data`, INFO title/date | Duration from audio byte count and declared rate; compressed variants can give an indicative duration rather than decoded sample count |
| FLAC | STREAMINFO, sample count, Vorbis TITLE/DATE comments | No audio block decoding or sample MD5 verification |
| MP3/MPEG audio | Two consistent frame headers, ID3 v2.2–v2.4, Xing/Info/VBRI | Duration only when described by an index; otherwise unknown; encrypted, compressed, or unsynchronized tags are not interpreted |

Embedded titles take precedence over file names. A date stays absent unless
metadata describes it, unknown codecs keep their container identifier and
missing values are `null` in JSON. Each metadata read is limited to 8 MiB, all
metadata to 64 MiB and parsing to 100,000 elements. Payload is skipped with
seeks rather than read, including MP4 `mdat` boxes larger than 4 GiB.

## BitTorrent

Mynou implements v1, v2 and hybrid torrents, `btih` and `btmh` magnets, the TCP
peer protocol, metadata exchange, v2 hash proofs, HTTP, HTTPS and UDP trackers,
a DHT client and PEX reception. Peer data uses TCP only.

- **Peers**: `downloads.max_peers` (1 to 256, default 50) is a per-transfer
  ceiling. Outgoing workers per transfer are
  `min(max_peers, max(1, 256 / execution_slots))`, so at most 256 across active
  downloads. Execution slots are at most 256; `max_active: 0` removes the user
  download-queue limit while keeping this resource bound. The independent
  `max_seeding` queue limit also accepts zero for no user limit. Incoming seeding connections are limited to 32.
  An incoming peer of a torrent still downloading takes a free worker slot, ahead
  of any address to dial; up to 8 wait at most 5 seconds for one, then are closed.
  Concurrency is also reduced using a 128 MiB per-transfer memory estimate, with
  at least one worker. Once the pieces are known a peer message is at most the
  bitfield or 64 KiB, plus 1 KiB, so a worker's estimate is its piece, the
  blocks it requested of the next piece (at most 256 KiB) and two such messages. Workers share one copy of the metadata, counted once; when v2
  hashes must be fetched from peers each worker may keep its own, and the
  estimate counts one per worker. This is not a hard process memory limit. A peer pipelines
  up to 16 blocks, fewer when a rate limit applies; without a limit, the pipeline
  continues into the worker's next piece, so it does not drain at each piece
  boundary. A worker holds a second piece only while unclaimed pieces outnumber
  twice the workers, so the last pieces are not held behind a slow peer. A peer count is not a
  promise of connections or speed.
- **Connections**: new connections have a one-second attempt timeout,
  established sockets check for cancellation every 100 ms, idle peers rotate
  after two seconds and corrupt peers are excluded for the rest of the download.
  No duplicate end-game requests are sent.
- **Discovery** keeps at most 1,024 peers per torrent. Tracker intervals are
  bounded between 30 seconds and 24 hours. Seeding announces `started`,
  `completed` and `stopped`, but a forced stop may not send `stopped`. Torrent
  metadata is limited to 8 MiB.
- **Private torrents**: a magnet cannot reveal its private flag before its
  metadata arrives. With a tracker or `x.pe` hint, public discovery waits for the
  metadata; without one, Mynou may use DHT first and stops public discovery if
  the torrent turns out to be private. Use the `.torrent` file or a magnet with
  its private tracker when confidentiality must hold from the start.
- **Integrity**: data is rechecked after a restart, and a modified file never
  becomes ready just because it exists. Unsafe paths and contradictory metadata
  are rejected. Cancelling one request does not stop a torrent another request
  shares.
- **Network**: port 6881/TCP accepts peers; inbound access depends on your
  firewall and router. DHT and UDP trackers use outgoing ephemeral sockets, no
  inbound UDP port is published and the DHT client is not a complete DHT
  server.

### Transfer controls

- Pauses are durable, affect every request sharing the transfer and are never
  undone by automatic retries. Queue priority is -1000 to 1000, higher first,
  then first-in first-out, without preempting active transfers.
- File indices are the original metadata indices (below 100,000), with gaps
  where padding files were filtered; padding indices are rejected. Priorities
  order the required pieces; there is no `skip` priority.
- A selection update holds 1 to 1,024 distinct indices. Retained selections
  hold at most 1,024 paths and 1 MiB of path bytes, each path at most 4,096
  bytes, 65 components and 255 bytes per component. Selections never shrink, a
  v1 selection also stores boundary bytes of neighbor files, v2 and hybrid
  selections require their file roots, and partial availability is not
  whole-torrent readiness: partial transfers never seed or announce completion.
- Rate limits accept 0 to 1,073,741,824 payload bytes per second (0 means
  unlimited) and cover payload only, not protocol, metadata or tracker
  overhead. Each aggregate rate bucket allows a 16 KiB burst. Per-transfer rates
  cannot exceed the global ones.
- Seeding ratios accept 1 to 1,000,000 thousandths and seeding times 1 to
  315,360,000 seconds. A per-transfer policy replaces the configured seeding
  defaults entirely. The ratio budget is the verified non-padding payload size
  times the ratio, rounded up; whole blocks never exceed it, so seeding can stop
  slightly below the ratio. Seeding time counts online availability, including
  idle time, and excludes offline, paused or limited time.
- Payload counters and seeding time survive a clean restart. Updates are saved
  every 30 seconds and on clean shutdown, so a crash can lose up to 30 seconds
  of increments; counters are not equivalent to a tracker's records. Older
  transfers start their counters at zero.
- Native transfer state written by 0.14 or later is refused by 0.13 and earlier.
- Retained transfers use an on-disk index with no fixed total-count limit.
  Routine startup and scheduling read bounded windows, not the complete transfer
  directory. Transfer lists return cursor pages of 50 rows by default, at most
  200; every retained identity remains reachable. Paused and finished entries do
  not each require a resident worker. The first indexed startup migrates legacy
  state and keeps a private backup; old binaries reject the new format. See
  [transfer storage and downgrade](transfers.md#storage-migration-and-downgrade).

## Network, HTTP and TLS

- The HTTP client implements HTTP/1.1 with size limits, deadlines, redirects
  and HTTP CONNECT proxies (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`); local
  management commands bypass the proxy. HTTP/2 and HTTP/3 are not supported.
- The API accepts HTTP/1.1 requests with a declared content length and bodies
  up to 1 MiB; it rejects chunked request bodies and ambiguous headers. It has
  no sign-in: `listen` must be a loopback address, and a request must name
  loopback in its `Host` and carry no other site's `Origin`.
- The TLS client supports TLS 1.3 with X25519 and ChaCha20-Poly1305/SHA-256,
  and TLS 1.2 for servers without it: ECDHE with X25519 or P-256,
  AES-128-GCM or ChaCha20-Poly1305 with the SHA-256 PRF, secure renegotiation
  required, the extended master secret whenever the server offers it, and
  neither session resumption nor renegotiation. A TLS 1.3 server's downgrade
  sentinel is refused. AES and the P-256 exchange are written for these
  servers: AES computes its S-box without tables, and P-256 uses a Montgomery
  ladder over variable-time field arithmetic with a fresh key per handshake.
  Both versions validate the certificate and host name, with RSA (PSS, and
  PKCS#1 v1.5 for TLS 1.2 key exchanges) and ECDSA P-256/P-384
  signatures with SHA-256/SHA-384. Unsupported algorithms or critical extensions
  are errors; there is no unauthenticated fallback. X.509 NameConstraints is not
  implemented and causes rejection, and CRLs and OCSP are not consulted. Trust
  comes from PEM bundles (`MYNOU_CA_FILE`); the macOS Keychain is not read.
  The primitives are checked with test vectors and local protocol tests, not an
  independent audit.
- Plex, TMDB and sources need valid addresses and credentials. Plex
  availability confirmation always uses a fresh response. Plex watchlists and
  library sections are read 200 items per request with no total item or page
  count; availability checks stop at the first match within their 90-second
  budget, and the single-account watchlist sync reads up to eight pages per
  pass, resuming from a saved position. Catalog responses used
  for ordinary requests are cached for one hour (at most 256 entries and 32
  MiB); series planning bypasses that cache.

## Sources

Indexers:

- Supported formats are RSS, JSON and Torznab, from an endpoint that serves
  them directly; Mynou does not perform a general web search. A search uses at
  most 1,000 configured sources. Responses are limited to 8 MiB.
- Authentication supports `none`, `bearer`, `basic` and a single-cookie `form`
  login. Credential values keep their exact UTF-8 bytes; empty, oversized or
  control-character values are rejected, and Bearer tokens must use the token
  alphabet. Remote authentication requires verified HTTPS.
- Form login uses a direct same-origin POST, a 64 KiB response limit and at most
  ten seconds within the search budget. The response must set exactly one
  matching cookie; path, domain, expiry, `Secure`, `HttpOnly` and `SameSite` are
  checked, and multiple or ambiguous cookies, duplicate or unknown attributes
  and unrelated scopes are rejected. `Max-Age` wins over `Expires`, and
  `max_age_secs` (1 to 3,600) is always an upper bound.
- A JSON `mapping` has paths of up to eight parts, up to four alternative
  `url` paths and up to eight fixed parameters of at most 256 characters; names
  are at most 64 characters. A search reads a single result page.
- Request intervals accept 0 to 60,000 ms. `Retry-After` on HTTP 429 defaults
  to 60 seconds and is clamped to 1 to 3,600. A probe has a five-second budget;
  if the service crashes during a probe its outcome stays unknown, and probes
  are never replayed.
- Retained source policy uses a private `indexers.idx/` with no fixed total
  source count or aggregate byte cap, including removed sources. Reads and
  controls address individual records; configuration and watched-source bounds
  still apply. See [storage and downgrade](inventory-storage.md#source-policy-storage). Corruption, unsupported
  formats, links and public permissions stop the service before workers start.

Watched feeds:

- At most 64 watched RSS, Torznab or JSON sources, polled every 60 to 86,400
  seconds (default 900), at most four per pass, with a 30-second budget per
  poll.
- A poll reads at most 500 entries, considers 200 unseen ones, and routes at
  most 32 jobs and 32 upgrades. The window keeps 1,024 identities per source.
- `feeds.idx` retains source state without a total count or aggregate byte cap;
  retained listings and active reconciliation use 200-row, 4 MiB windows.
  `feeds.bin` is an identity-bound marker; legacy reads remain bounded to 4 MiB.
  A failed save reports `storage_failed`; uncertain writes require restart.
  Corruption, unsupported formats, links and public permissions stop startup.
  A source not polled successfully for seven days is baselined again.

IRC announcements:

- At most 8 sources, 32 retained source IDs and 64 rules, each rule with at most
  16 required and 16 blocked terms of 128 bytes.
- Nicknames have 1 to 24 ASCII letters, digits, hyphens or underscores,
  starting with a letter. Channels are `#` followed by 1 to 63 letters, digits,
  hyphens or underscores. Senders are an exact `nick!user@host` of at most 128
  bytes. Plain-text IRC is accepted only on loopback IP addresses.
- Retained announcement identities live in `announcements.idx` without a total
  record or byte limit. Duplicate suppression survives restart without pruning.
  Point reads and listing pages (200 rows, 4 MiB) bound each operation; follow
  `next_cursor` as `after` beyond the compatibility offset
  window of 1,000 rows. Legacy snapshots remain bounded to 1,000 rows / 8 MiB
  during migration. Counters and source health reset when the service restarts.
  Routing scans one 200-row pending page per turn (plus an empty wrap probe)
  and makes at most one attempt per turn. A transient failure backs off for 30
  seconds; at most 200 backoffs are remembered, and a full table drops the one
  closest to expiry instead of pausing routing, so failing announcements never
  block others. A page whose rows all back off costs one idle turn and the
  cursor moves on. Historical decisions and reservations are skipped by the
  pending index; transient outcomes retain at most 200 rows. Storage failures
  never discard identities.
- Lines are limited to 8,192 bytes, reads to 4,096 bytes, batches to 128
  parsed messages and each source to 256 messages per second. Connection and
  registration (including authentication) must finish within ten seconds,
  which incoming traffic cannot extend; a partial line must complete within ten
  seconds. Idle timeout is 30 to 600 seconds. Server error text is not echoed.
- SASL supports only `PLAIN`, without SASLprep or Unicode normalization.
  Capability negotiation accepts 16 lines, 64 capabilities and 4,096 bytes;
  credential values have 1 to 256 bytes; the encoded response has at most
  1,028 bytes, sent in at most three chunks of 400 bytes.
- NickServ accounts have 1 to 64 ASCII letters, digits, hyphens or
  underscores; success and the 1 to 8 failure notices have at most 256 bytes;
  passwords have 1 to 256 printable non-space ASCII characters. Only the
  `IDENTIFY` command is supported.
- Text formats use a non-empty prefix and a suffix of at most 128 ASCII bytes
  each, separators of 1 to 16 ASCII bytes containing punctuation, and unique
  fields among the eight documented names. Pattern languages and URL fields are
  not supported. Announcements without a catalog ID and a hash, such as
  title-only messages, cannot be used, except through a `bracketed` lookup.
- Bracketed lookups hold at most 64 unresolved candidates in memory, try each
  at most three times (30 seconds apart, ten-second budget each) and require
  one Torznab item for the exact details page, plus the TMDB catalog. Episodes
  need one `SxxEyy` marker; season packs are not supported.
- A routed bracketed acquisition downloads one `.torrent` of at most 8 MiB
  from the indexer's origin, within the ten-second routing budget and the
  indexer's minimum request spacing. The verified files under `irc-metainfo/` are kept
  with their transfers and are not pruned automatically.
- Preview files and bodies are limited to 8 KiB; announcement lists return at
  most 200 entries per page.
- Automatic cross-seed discovery compares at most 50,000 completed transfers
  per pass, reads at most 500 entries per feed or search, downloads at most
  eight `.torrent` files and runs one completion search per source and pass,
  with a 30-second budget per request. It remembers 2,048 entry identities per
  source and the last 100 outcomes; counters keep 64 reasons per result.
- Magnet templates contain `{xt}` once, at most four trackers and eight
  literal-IP `x.pe` peers. HTTPS and UDP trackers are supported; plain HTTP
  trackers only on loopback. Embedded credentials, host names in `x.pe`, zero
  ports, unspecified or multicast addresses and malformed escapes are rejected.
- Routing attempts one announcement per pass, sharing a ten-second budget for
  availability and metadata; the worker checks once per second, a failed
  announcement waits 30 seconds, attempts reset at restart, and the worker may
  take up to that budget to notice shutdown. Held jobs defer negative Plex
  checks for 60 seconds. A request review has a ten-second budget; title-only,
  incomplete, future, ambiguous or mismatched facts stay unresolved, and an
  aborted review is never replayed.
- Announced catalog IDs remain claims: matching a hash and labels does not prove
  what the video depicts.
- IRC routing and request reviews use newer storage formats (IRC history 2 and
  3, requester snapshot 2, job journal 5) that earlier versions refuse.

## Search and selection

- Up to 64 profiles, named with 1 to 64 ASCII letters, digits, hyphens or
  underscores. Lists hold at most 32 entries, terms at most 128 bytes, and
  scores and minimums are bounded to ±100,000.
- Markers are claims in a release name. Selection does not verify audio
  languages or decode video: `VOSTFR` does not mean French audio and `MULTI`
  does not say which languages are present. Automatic selection requires a
  strict title match and, for episodes, an identified file.
- Search, upgrade and series refresh passes have a 90-second budget. There is
  no total candidate count or candidate memory limit: each indexer response
  (at most 8 MiB, 100,000 results) is assessed as it arrives and only the
  winner, the requested report page and eight season-pack candidates are kept.
  Reports are pages of 1 to 200 candidates (100 by default) continued with an
  opaque `next` cursor; each page repeats the live search, so a page can differ
  if an indexer changed in between. Identical results from one source share a
  candidate ID and are listed once, while `candidate_count` counts every result.
- Explicit `source_url` and `source_path` sources are not searched or ranked.
- At most 100,000 per-title profile bindings, in `profiles.idx`, keyed by TMDB
  ID; requests without a TMDB ID cannot be bound. A preview covers one title.

## Library and upgrades

- The library view covers imports Mynou owns. It is not a scan of Plex or of
  existing folders, and a Plex skip does not create an owned import.
- Monitoring is off by default; `interval_secs` accepts 60 to 86,400 and
  `max_checks` 1 to 256.
- Baselines are release titles of 1 to 2,048 bytes without control characters.
  Decisions rely on release-name claims.
- Upgrades keep earlier imports and downloads: there is no automatic cleanup,
  rollback deletion or adoption of existing library folders.
- `plex.path_mappings` holds at most 32 entries of normalized absolute paths of
  at most 4,096 bytes, with distinct Mynou prefixes.
- Shared-group upgrades replace one video for 2 to 64 owners with one new video.
  They need every owner current and monitored, an accepted improvement and a
  new URL and torrent hash. Mapping files are limited to 512 KiB, API bodies to
  1 MiB, titles to 2,048 bytes and the action to 60 seconds. Group data uses
  storage format 3, which versions before 0.18 refuse.

## Series, numbering and packs

- Series storage keeps as many tracked scopes and episodes as the disk allows,
  with no total count, aggregate size or automatic eviction: it is an indexed
  [disk store](inventory-storage.md#series-storage), not a snapshot held in memory.
  Each plan still holds at most 2,000 episodes, 100 seasons and 1,000 episodes
  per catalog season. Earlier versions stopped at 128 scopes, 20,000 episodes and
  8 MiB; their data migrates on the first start with write access.
- The service checks due records every minute: at most four per pass (240 an
  hour, so with more monitored records each waits longer than an hour), 64
  episode requests per batch, a 90-second pass budget and 20 seconds per catalog
  request.
- Calendar pages return 1 to 200 rows over an inclusive window of at most 367
  days, for years 1800 to 9999. An `offset` reaches at most row 20,000; the
  `after` cursor returned as `next` reaches every later row. `total` is exact for
  windows of at most 20,000 rows and `null` beyond that. Series summaries are always
  a page: `limit` (1 to 200, default 50) and the `after` series ID returned as
  `next` reach every series, however many are tracked.
  Air dates are catalog days in UTC, not premiere times.
- Refresh checks compare the catalog with the retained plan, not with a complete
  numbering history. A known episode ID can never be replaced or reassigned.
- Numbering keeps at most 2,000 IDs and choices per scope, with no total across
  scopes. A decision holds at most 2,000 changes. Seasons range from 0 to 9,999,
  episodes and absolute numbers from 1 to 99,999 and catalog IDs from 1 to
  2^53−1. Mapping files are limited to 512 KiB. Numbering history is never discarded to make room. Version
  0.15 cannot read numbering data saved by later versions.
- Explicit packs map 1 to 64 episodes to distinct files. Paths have at most
  4,096 bytes, 32 components and 255 bytes per component, without empty,
  absolute, dot, parent, backslash, colon or control components. Supported
  videos are MP4, M4V, MOV, MKV, WebM and AVI. Mapping files are read up to
  1 MiB. Versions 0.12 and earlier ignore pack mappings: do not downgrade.
- Automatic pack searches cover at most 64 episodes of one season and inspect
  the metadata of at most eight candidates, each within ten seconds of the
  90-second budget. Inspection accepts 1,024 files and 1 MiB of paths. Magnet
  inspection uses at most eight peers and four trackers (five seconds per
  attempt), never payload, DHT or PEX; some trackers decline metadata-only
  queries, so a DHT-only magnet can fail automatic inspection yet work as an
  explicit source. Monitored pack preference looks at four seasons per pass.
  Failed pack searches are not kept in a history. Versions before 0.15 ignore
  the identity checks of automatic pack jobs: do not downgrade.
- Shared videos bind 2 to 64 consecutive episodes of one season. Inspection
  uses the same metadata bounds with a 60-second deadline; mapping files are
  limited to 512 KiB. File names fit 255 bytes. Versions before 0.17 refuse
  shared data.
- Pack and shared jobs have no release baseline, and pack provenance does not
  establish upgrade quality.

## Plex requester accounts

- At most 32 account aliases and 32 destinations, and 64 new requests per
  admission step (one step follows each committed watchlist batch). Watchlists have no total entry, page or expanded-request count: each
  pass reads up to eight pages of 200 entries per account and commits at most
  512 demands at a time, and the sweep resumes there in the next pass.
  Retained demands and operator
  interests have no total count or aggregate-byte cap in `requesters.idx`.
  Demand records keep a 16 MiB bound; pages hold up to 200 rows and 4 MiB,
  except one larger demand may appear alone. Legacy offsets stop at 10,000;
  `after` cursors reach further rows ([storage guide](inventory-storage.md#requester-storage)).
- A poll pass has 90 seconds, with ten seconds per account for identity,
  watchlist pages, catalog and retiring entries a finished sweep did not see.
  Removals need a complete sweep during which the watchlist did not change, so
  a list that changes during every sweep keeps its removed entries until it
  stays still for one. Control files are limited to 64 KiB.
- Requester demand is acquired episode by episode; automatic packs keep their
  separate operator rules.
- Requester data uses job storage format 4, which earlier versions refuse.

## Persistence and platform

- Job journal formats 6 to 8, which stored the removed Usenet provenance, are
  rejected. Requester snapshots containing removed approval, quota or
  notification fields and IRC history format 4 are also rejected during startup.
  These removals provide no migration and do not delete stored data automatically. Read the
  [release notes](releases/) and back up private storage before upgrading.
- Jobs and their secondary ownership indexes live in `jobs.idx`, a single-owner
  disk index with no fixed total job, snapshot or journal-size cap. Job JSON
  keeps its 16 MiB parser/record bound (large jobs use checked 1 MiB chunks).
  History keeps at most 1,000 events **per job**, with 4,096-byte messages;
  consecutive processing heartbeats are folded. History is not an unlimited
  archive. Job/library listings return cursor pages of up to 200 rows and a
  4 MiB job-byte budget; one larger job is returned alone within its record
  bound. A filtered job or library page examines at most 1,000 rows and may
  hold fewer matches while its cursor continues
  ([listings](inventory-storage.md#listings-and-cursors)). Network/JSON response limits still apply to unusually large records.
  The settings document remains limited to 1 MiB. See
  [job migration and recovery](inventory-storage.md#job-storage).
- Jobs/history, library tips, series, requester demand, feeds, source policies,
  IRC announcements and native transfers use private checksummed disk indexes
  with write-ahead recovery. Retained inventories have no fixed total count or
  aggregate-byte cap; per-record, page, transaction, configured-source and
  concurrency bounds remain. Legacy snapshot bounds apply to migration inputs,
  and physical disk and compute resources still constrain growth. Recovery and
  downgrade guidance for every store is linked in the
  [storage overview](inventory-storage.md). Checksums detect corruption; they do
  not authenticate against someone who can rewrite the files. After an uncertain
  write, reads and writes of that index pause until writable recovery on restart.
- `GET /api/status` reports per-store sizes and recovery state,
  with unavailable counts for busy or failed stores. Common backlog/freshness
  diagnostics are delivered; increasing-size native macOS arm64 disk/time/RSS
  evidence, remaining intentional bounds and the acceptance mapping are linked
  from [the capacity evidence record](inventory-growth.md).
  These readings cover their stated synthetic workloads, not arbitrary network
  throughput, long-term churn or infinite hardware capacity.
- Series data moves from `series.json` to `series.idx` on the first start with
  write access; the previous snapshot is kept as `series.legacy-*.json` and
  `series.json` becomes a marker that older versions refuse. Going back means
  restoring that copy, which lacks later changes and keeps the old limits. A
  read-only open of a store that has not been migrated reports that it must be
  migrated and writes nothing.
- Request and series writes are separate commits: after a crash, a partly
  recorded episode batch is kept and completed without duplicates. Without
  series storage no series is tracked, and earlier episode jobs are never adopted
  as series records.
- Data from the earlier Go release (SQLite) is not migrated: keep it in a
  separate directory. Preserve configuration, data, downloads and library when
  changing versions, and back up before a downgrade.
- Mynou ships for macOS on Apple Silicon only: there is no Intel, Linux or
  Docker build. The executable is verified on macOS 26 only and is not signed or
  notarized.
- Mynou installs no Unix signal handler. `POST /api/shutdown` stops gracefully;
  a forced stop relies on journal recovery. Security randomness needs
  `/dev/urandom`.
- The optional [launchd service](macos-service.md) starts at account login, not
  pre-login boot. It restarts unsuccessful exits with a 60-second throttle,
  uses a private credential snapshot in the plist, and bounds its fixed-code
  logs to two 64 KiB files. Native service registration does not change the VPN
  protection boundary or authorize stopping the legacy stack.

## Not supported

- Advanced torrent transports and networking: uTP, WebRTC/WebTorrent, webseeds,
  automatic NAT traversal (UPnP/NAT-PMP) and a complete persistent DHT table.
- Partial seeding, shrinking a selection and automatic cleanup of downloads or
  old imports.
- Broad tracker and provider coverage, changing authentication schemes and a
  comprehensive adapter catalog; login redirects, CSRF, CAPTCHA or interactive
  logins and arbitrary cookie jars.
- IRC pack or upgrade actions, broader announcement formats and interactive IRC
  authentication.
- V2/hybrid cross-seeding, chained reuse, pack-specific cross-seed matching
  and further bulk automation. See [the transfer guide](transfers.md#manual-cross-seeding)
  for the limits of supported manual and automatic v1 cross-seeding.
- Automatic anime-order or episode-range inference, splitting or cutting
  videos, automatic search for shared groups, and renaming or deleting library
  files.
- Scheduling episodes by premiere time or time zone.
- Requester self-service sign-in.
- Full parity with Radarr, Sonarr, Pulsarr, qBittorrent, qui, autobrr or
  Prowlarr.
- Mature administration across multiple installations and operating systems.
- An independent review of the original cryptographic and protocol code.

"Zero dependencies" describes how Mynou is built. It does not guarantee
completeness, security, optimal performance or compatibility. CI uses synthetic
media, loopback peers and simulated services: passing CI does not establish
compatibility with your Plex installation or public-swarm throughput.
