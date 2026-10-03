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
       Native BitTorrent and verification
                   |
       Media metadata analysis
                   |
          Import without overwriting
                   |
       Plex scan and confirmation
```

The components are independent of frameworks. `config` validates fields and
resolves paths relative to the configuration file. `integrations` converts
network responses into requests and sources. `engine` orchestrates transitions
without holding the journal lock during a transfer or import.

`store` synchronizes each transaction before confirming it. Records form a
SHA-256-verified chain: an incomplete tail after interruption is recoverable,
while corruption of a complete record is reported. A file lock prevents two
owners of the same journal. Workers use leases and renew ownership during long
operations.

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
intact, and symbolic links on controlled import paths are rejected.

`net`, `tls`, `pki`, and `crypto` implement HTTP/1.1, the TLS client, X.509
validation, and the required primitives. Protocol errors are explicit; a failed
negotiation never disables certificate validation. The API server uses HTTP on
the local interface with a Bearer token. See [protocol limits](limits.md).

GitHub Actions validates changes and publishes releases. Build and release tools
are separate from the runtime; the application never invokes them.
