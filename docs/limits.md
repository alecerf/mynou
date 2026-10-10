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

- **Peers**: `downloads.max_peers` (1 to 8, default 4) is a per-transfer
  ceiling. Outgoing workers per transfer are
  `min(max_peers, max(1, 64 / max_active))`, so at most 64 across all active
  transfers (direct Rust library callers can run up to 128 active transfers
  with one worker each). Incoming seeding connections are limited to 32.
  Concurrency is also reduced using a 128 MiB per-transfer memory estimate, with
  at least one worker; this is not a hard process memory limit. A peer pipelines
  up to 16 blocks, fewer when a rate limit applies. A peer count is not a
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
  every second and on clean shutdown, so a crash can lose the latest
  increments; counters are not equivalent to a tracker's records. Older
  transfers start their counters at zero.
- Native transfer state written by 0.14 or later is refused by 0.13 and earlier.

## Network, HTTP and TLS

- The HTTP client implements HTTP/1.1 with size limits, deadlines, redirects
  and HTTP CONNECT proxies (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`); local
  management commands bypass the proxy. HTTP/2 and HTTP/3 are not supported.
- The API accepts HTTP/1.1 requests with a declared content length and bodies
  up to 1 MiB; it rejects chunked request bodies and ambiguous headers.
- The TLS client supports TLS 1.3 with X25519 and ChaCha20-Poly1305/SHA-256,
  certificate and host name validation, and RSA and ECDSA P-256/P-384
  signatures with SHA-256/SHA-384. Unsupported algorithms or critical extensions
  are errors; there is no unauthenticated fallback. X.509 NameConstraints is not
  implemented and causes rejection, and CRLs and OCSP are not consulted. Trust
  comes from PEM bundles (`MYNOU_CA_FILE`); the macOS Keychain is not read.
  The primitives are checked with test vectors and local protocol tests, not an
  independent audit.
- Plex, TMDB and sources need valid addresses and credentials. Plex
  availability confirmation always uses a fresh response. Catalog responses used
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
- Request intervals accept 0 to 60,000 ms. `Retry-After` on HTTP 429 defaults
  to 60 seconds and is clamped to 1 to 3,600. A probe has a five-second budget;
  if the service crashes during a probe its outcome stays unknown, and probes
  are never replayed.
- Source policy is kept in a private checksummed `indexers.bin` of up to 1,000
  sources and 2 MiB, including removed sources. Corruption, unsupported
  formats, links and public permissions stop the service before workers start.

IRC announcements:

- At most 8 sources, 32 retained source IDs and 64 rules, each rule with at most
  16 required and 16 blocked terms of 128 bytes.
- Nicknames have 1 to 24 ASCII letters, digits, hyphens or underscores,
  starting with a letter. Channels are `#` followed by 1 to 63 letters, digits,
  hyphens or underscores. Senders are an exact `nick!user@host` of at most 128
  bytes. Plain-text IRC is accepted only on loopback IP addresses.
- The history (`announcements.bin`, checked, at most 8 MiB) keeps 1,000
  announcement identities. When full, new identities are rejected rather than
  pruned. Counters and source health reset when the service restarts.
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
  title-only messages, cannot be used.
- Preview files and bodies are limited to 8 KiB; announcement lists return at
  most 200 entries per page.
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
- Search, upgrade and series refresh passes have a 90-second budget. Reports
  show at most 1,000 candidate rows; retained candidate data is limited to
  16 MiB, beyond which the search fails rather than choosing from an incomplete
  set.
- Explicit `--url` and `--path` sources are not searched or ranked.

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

- Series storage holds 128 tracked scopes, 2,000 episodes per plan, 20,000
  episodes in total and an 8 MiB snapshot, with no automatic eviction. A plan
  covers at most 100 seasons and 1,000 episodes per catalog season.
- The service checks due records every minute: at most four per pass, 64
  episode requests per batch, a 90-second pass budget and 20 seconds per catalog
  request.
- Calendar pages return 1 to 200 rows (offset up to 20,000) over an inclusive
  window of at most 367 days, for years 1800 to 9999. Air dates are catalog days
  in UTC, not premiere times.
- Refresh checks compare the catalog with the retained plan, not with a complete
  numbering history. A known episode ID can never be replaced or reassigned.
- Numbering keeps at most 2,000 IDs and choices per scope and 20,000 in total.
  A decision holds at most 2,000 changes. Seasons range from 0 to 9,999,
  episodes and absolute numbers from 1 to 99,999 and catalog IDs from 1 to
  2^53−1. Mapping files are limited to 512 KiB; the browser shows the first 100
  comparison rows. Numbering history is never discarded to make room. Version
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

- At most 32 account aliases and 32 destinations, 10,000 retained demands, 512
  watchlist and expanded items per account and 64 new requests per pass. The
  checked `requesters.bin` snapshot is limited to 16 MiB.
- A poll pass has 90 seconds, with ten seconds per account for identity,
  watchlist and catalog. Control files are limited to 64 KiB.
- Requester demand is acquired episode by episode; automatic packs keep their
  separate operator rules.
- Requester data uses job storage format 4, which earlier versions refuse.

## Browser interface

- One shared operator token, no individual users. The interface does not edit
  configuration, adopt an existing Plex library or stream over WebSocket.
- Sessions last eight hours from sign-in and are not extended by activity.
  Sign-in challenges expire after ten minutes and after five wrong tokens. At
  most 128 sessions and challenges exist; new challenges can evict older
  challenges but never signed-in sessions.
- Forms accept at most 64 fields, 65,536 encoded bytes and 8,192 decoded bytes
  per field, so the browser accepts smaller inputs than the API. Malformed
  escapes, invalid UTF-8, unexpected or duplicate fields, control characters and
  invalid numbers are rejected.
- Bulk actions select at most 32 entries; lists show 50 rows per page.
- Live progress polls visible pages every ten seconds, one request at a time,
  with an eight-second deadline, at most 50 rows and 48 KiB per reply. It does
  not remember preferences, insert rows, stream, or estimate time or speed. Its
  read routes require the browser session and exact origin, and they never
  renew the session or consume messages.
- Browser and assistive-technology compatibility has not been independently
  verified.

## Persistence and platform

- Job journal formats 6 to 8, which stored the removed Usenet provenance, are
  rejected. Requester snapshots containing removed notification records and
  IRC history format 4 are also rejected during startup. These removals provide
  no migration and do not delete stored data automatically. Read the
  [release notes](releases/) and back up private storage before upgrading.
- The journal replaces SQLite and needs a single owner of its directory. It
  holds at most 10,000 requests and keeps the latest 1,000 events in total, so
  history is not an unlimited archive. It compacts at 4 MiB; records and
  snapshots are limited to 16 MiB and the journal to 64 MiB. The configuration
  file is limited to 1 MiB.
- Offline listings and previews open storage read-only: they create nothing,
  change no permissions, repair nothing and still exclude a concurrent writer.
  After an interrupted write they ask for writable recovery, which the next
  service start performs.
- Series, requester, IRC and source data live in separate private snapshots,
  written through a temporary file, synchronization and an atomic rename. Their
  checksums detect corruption; they do not authenticate against someone who can
  rewrite the files. When a write's durability is uncertain, that store refuses
  further changes until the service restarts.
- Request and series writes are separate commits: after a crash, a partly
  recorded episode batch is kept and completed without duplicates. Without a
  series snapshot no series is tracked, and earlier episode jobs are never
  adopted as series records.
- Data from the earlier Go release (SQLite) is not migrated: keep it in a
  separate directory. Preserve configuration, data, downloads and library when
  changing versions, and back up before a downgrade.
- The container targets Linux x86_64 (musl). Apple Silicon Docker hosts need
  amd64 emulation; there is no native Linux arm64 image. The macOS executables
  are verified on macOS 26 only and are not signed or notarized.
- Mynou installs no Unix signal handler. `POST /api/shutdown` stops gracefully;
  a forced stop relies on journal recovery. Security randomness needs
  `/dev/urandom`.

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
- Cross-seeding and further bulk automation.
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
compatibility with your Plex installation, public-swarm throughput or browser
and assistive-technology support.
