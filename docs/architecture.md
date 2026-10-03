# Native Rust architecture

```text
CLI / API                 Plex watchlist
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
network responses into requests and sources. `engine` orchestrates transitions
without holding the journal lock during a transfer or import.

`selection` evaluates matched source candidates using the configured movie or
episode profile. It extracts bounded release-title attributes, filters candidates
and ranks accepted releases deterministically. Automatic acquisition and search
previews share this decision path. Preview serialization exposes opaque IDs and
assessments without acquisition URLs; a preview does not acquire the journal
owner lock or create a job.

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
Promotion inherits the parent's current monitoring choice. Check timestamps and per-entry monitoring policy are durable. Bounded
passes consider the oldest checks first. Background checks honor per-entry
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

GitHub Actions validates changes and publishes releases. Build and release tools
are separate from the runtime; the application never invokes them.

[Selection policies](selection.md) · [Library monitoring](library.md) ·
[Release stages](roadmap.md)
