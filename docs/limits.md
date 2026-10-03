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

Each transfer uses one active peer and up to 16 blocks in flight. Discovery caches
at most 1,024 peers per torrent. Seeding announces `started`, `completed`, and
`stopped` events and respects tracker intervals, bounded between 30 seconds and
24 hours. A forced stop does not guarantee a `stopped` announcement. Download and
upload counters describe content bytes transferred in the current session;
they reset on restart and are not a lifetime ratio.

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
perform a general Web search.

TMDB responses are cached for one hour, with at most 256 entries and 32 MiB.
Episodes already in a response are filtered by their air date during
synchronization. New metadata is discovered after cache expiry and on the next
sync. Plex availability confirmation always uses a fresh network response.
Automatic source selection requires a strict title match and an identified file
for the requested episode. Packs and title variants are not resolved implicitly.
Season-zero specials are excluded from automatic series expansion.

## Persistence and platform

The Rust journal replaces SQLite and requires a single owner of its directory.
Go-release files stay separate; no silent schema or torrent migration occurs.
Preserve the library and downloads when changing versions.

History retains the most recent 1,000 events across the journal, then filters by
request for `events`. Requests and their state remain in snapshots; event history
is not an unlimited archive. Automatic compaction is attempted at 4 MiB of journal
data. Records and snapshots are limited to 16 MiB, and the journal to 64 MiB;
exceeding a limit produces an error rather than unbounded growth.

The Docker delivery target is Linux x86_64 with musl. The project installs no Unix
signal handler through FFI: authenticated API shutdown is graceful, while journal
recovery handles forced interruptions. Security functions using `/dev/urandom`
require a system providing that source.
