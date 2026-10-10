# Architecture

This page is for contributors. It explains how Mynou is built and how a request
moves through the code. User-facing behavior and bounds are in the guides and in
[limits](limits.md).

## Standard library only

Mynou is safe Rust that uses only the standard library. `Cargo.toml` declares no
dependencies, development dependencies or build dependencies, and `Cargo.lock`
contains only `mynou`. There is no vendored or copied third-party code, no
submodule, no FFI, no `unsafe` code and no generation step. CI checks the graph
offline on every run (see [CI](validation.md#mynou-ci)).

| Former component | Rust replacement |
| --- | --- |
| Go BitTorrent engine | `src/torrent.rs` and `src/torrent/` |
| SQLite | Transaction journal and snapshots in `src/store.rs` |
| ffprobe | Container parsers in `src/media.rs` and `src/media/` |
| HTTP/TLS libraries | `src/net.rs`, `src/tls.rs` and `src/pki/` |
| JSON, bencode and XML | Bounded parsers in `src/json.rs`, `src/bencode.rs` and `src/xml.rs` |
| Hashing and cryptography | Native algorithms in `src/crypto/` |

Rust and Cargo are build tools. At runtime Mynou uses only the standard library
and the operating system for files, network, time and randomness. Security
randomness comes from `/dev/urandom`. The executable needs no OpenSSL, ffprobe,
curl, shell, SQL engine or torrent client. Plex, TMDB, indexers and IRC servers
are network services that the user configures, not dependencies. GitHub Actions
and its tools are development infrastructure; the application never invokes
them.

## Request flow

```text
CLI / API / browser   Plex watchlists   IRC announcements
        |                    |                  |
        +------------- Requests ----------------+
                           |
                  Journal and snapshots
                           |
                  Workers with leases
                           |
          TMDB catalog + RSS / JSON / Torznab sources
                           |
                Release selection profiles
                           |
           Durable transfer queue and policies
                           |
           Native BitTorrent and verification
                           |
                 Media metadata analysis
                           |
               Import without overwriting
                           |
                Plex scan and confirmation
                           |
       Owned library: monitoring, baselines, upgrades
```

## Modules

- `config` validates every field and resolves paths against the configuration
  file into absolute roots, even when the configuration path is relative.
- `engine` orchestrates request transitions. It never holds the journal lock
  during a transfer or an import. Workers take leases and renew them during
  long operations.
- `store` synchronizes each transaction before confirming it. Records form a
  SHA-256 chain: an incomplete tail after an interruption is recoverable, while
  a corrupted complete record is reported. A file lock gives the journal a single
  owner. Read-only opening (offline listings and previews) creates nothing,
  changes no permissions and repairs nothing.
- `integrations` turns Plex, TMDB and source responses into requests, catalog
  plans and candidates. `indexers` keeps source policy, authentication sessions
  and health. `selection` scores candidates against the movie or episode
  profile; automatic acquisition and search previews share that code, and
  previews never take the journal owner lock or create a job.
- `irc` runs one receiver per source with strict framing and verified TLS, keeps
  a checked announcement history, and routes hash-pinned candidates to existing
  jobs. Source I/O holds no storage lock. Admission takes locks in IRC, requester,
  job order and rechecks demand after network I/O; recovery completes committed
  origins and aborts uncommitted reservations without replaying them.
- `requesters` keeps Plex account bindings, policies, poll cursors and shared
  canonical demand in a private checked snapshot. Network I/O stays outside
  requester and job locks, which are taken in requester, job order. Startup
  validates the requester snapshot against the journal before transfers start.
- `series` keeps catalog plans and monitoring revisions in a private verified
  snapshot under the journal directory owner. A refresh captures the policy,
  fetches outside storage locks, verifies numbering and identities, and drops its
  result if the revision changed meanwhile. Submission batches lock series, then
  requests, and deduplicate by media identity. `numbering` holds explicit source
  labels, kept separate from the fixed library numbers, and `series::numbering`
  applies reviewed numbering decisions.
- `pack` validates explicit file mappings before recording episode jobs that
  share one torrent. `pack::automatic` searches season packs and maps files from
  authenticated metadata without payload. `pack::shared` binds one video to a
  range of episodes. `library` builds the owned-library view and upgrade
  decisions; `library::groups` and `store::groups` handle whole-group baselines,
  replacements and atomic promotion.
- `torrent` verifies metadata and pieces, rechecks data after restart and only
  then reports a transfer ready; bytes on disk are never enough. One coordinator
  per transfer owns piece claims, verified writes and completion, and hands
  pieces to a bounded set of peer workers. `torrent::control` holds durable
  pause, priority, selection and policy state; `torrent::selection` holds file
  interests for mapped packs.
- `media` reads container metadata with buffered reads and seeks, skipping
  payload such as MP4 `mdat`, Matroska clusters and WAV data. `organizer`
  publishes imports without overwriting: a hard link when possible, otherwise a
  synchronized copy; it rejects symbolic links on controlled paths.
- `net`, `tls`, `pki` and `crypto` implement HTTP/1.1, the TLS 1.3 client, X.509
  validation and the primitives they need. A failed negotiation never disables
  certificate validation.
- `server` serves the Bearer-token API and the health routes. `web` shares its
  listener and calls engine operations directly. Pages are rendered in Rust with
  an embedded stylesheet and work without JavaScript. The optional live-progress
  script (`/ui/live.js`) is embedded and pinned by its SHA-256 digest in the
  content security policy and in script integrity; it polls the session-only
  `/ui/live/jobs` and `/ui/live/transfers` read routes. Sessions use random
  opaque cookies and separate form tokens, and session locks never span engine or
  network work.
- `demo` runs the end-to-end demonstration with synthetic media, a loopback peer
  and simulated Plex and indexer services.
