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
| SQLite | Disk-resident index API in `src/inventory/`, used by the job, series, requester, feed, source-policy, IRC, transfer and title-profile stores |
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
CLI / API            Plex watchlists   IRC announcements
        |                    |                  |
        +------------- Requests ----------------+
                           |
              Disk indexes and recovery log
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
- `engine` orchestrates request transitions. It never holds the job-store lock
  during a transfer or an import. Workers take leases and renew them during
  long operations.
- `store` commits jobs, history and secondary ownership entries atomically in
  the disk-resident `inventory::Index`. Point lookups and cursor pages keep
  routine reads independent of the total inventory. A directory lock excludes
  concurrent owners; checksummed redo recovers interrupted published commits.
  Legacy journal/snapshot migration validates and preserves the old files before
  promotion. Read-only opening creates nothing and repairs nothing; see
  [storage and recovery](inventory-storage.md#job-storage).
- `integrations` turns Plex, TMDB and source responses into requests, catalog
  plans and candidates. `indexers` keeps source policy, authentication sessions
  and health. `selection` scores candidates against the movie or episode
  profile; automatic acquisition and search previews share that code, and
  previews never take the journal owner lock or create a job. `profiles`
  keeps per-title profile bindings in `profiles.idx` behind a leaf lock that
  callers may take while holding the requester or job locks;
  `configuration_for` applies a binding over the requester capture.
- `indexers::feed` and `indexers::watch` poll opted-in RSS and Torznab sources
  independently of IRC and of per-title search. Observation state and bounded
  captured candidates commit together in `feeds.idx` before routing; durable
  generation checkpoints resume job/library pages after restart. Routing only
  selects a release for demand that already exists: it rechecks the job and its
  admitted requester demand under the same locks that
  workers, upgrade checks and IRC admission use, so the first selection wins.
- `irc` runs one receiver per source with strict framing and verified TLS, keeps
  indexed announcement history in `announcements.idx`, and routes hash-pinned
  candidates to existing jobs. Source I/O holds no storage lock. Admission takes
  locks in IRC, requester, job order and rechecks demand after network I/O; recovery completes committed
  origins and aborts uncommitted reservations without replaying them.
- `requesters` keeps Plex account bindings, policies, poll cursors and shared
  canonical demand in `requesters.idx`. Watchlist sweeps stage bounded pages and
  resume from durable progress. Network I/O stays outside requester and job
  locks, which are taken in requester, job order. Startup validates requester
  and IRC provenance against jobs in bounded pages before workers or transfers
  start; the API reports that progress while serving reads.
- `series` keeps catalog plans, episode blocks, numbering and monitoring revisions
  in `series.idx` under the job-store directory owner. Due-work and calendar
  indexes provide bounded scheduled windows and cursor pages. A refresh captures
  the policy, fetches outside storage locks, verifies numbering and identities, and drops its
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
- `torrent` retains its catalog in `transfers.idx`; startup and scheduling read
  bounded windows instead of enumerating all retained transfers. It verifies
  metadata and pieces, rechecks data after restart and only then reports a
  transfer ready; bytes on disk are never enough. One coordinator
  per transfer owns piece claims, verified writes and completion, and hands
  pieces to a bounded set of peer workers. `torrent::control` holds durable
  pause, priority, selection and policy state; `torrent::selection` holds file
  interests for mapped packs.
- `media` reads container metadata with buffered reads and seeks, skipping
  payload such as MP4 `mdat`, Matroska clusters and WAV data. `organizer`
  publishes imports without overwriting: a hard link when possible, otherwise a
  synchronized copy; it rejects symbolic links on controlled paths.
- `net`, `tls`, `pki` and `crypto` implement HTTP/1.1, the TLS 1.3 and 1.2 client, X.509
  validation and the primitives they need. A failed negotiation never disables
  certificate validation.
- `server` serves the API and the health routes on a loopback listener, with
  no sign-in: a request must name loopback in its `Host` and carry no other
  site's `Origin`. `ui` embeds the web interface (`src/ui/`: plain HTML, CSS
  and JavaScript, no build step). `ws` implements a bounded RFC 6455 WebSocket
  on the same listener: the client offers the `mynou.v1` subprotocol, the
  origin must match the host, and at most eight sockets may be open. Calls reach the same `route` function as HTTP, and subscribed
  topics are pushed only when their serialized value changes.
- `demo` runs the end-to-end demonstration with synthetic media, a loopback peer
  and simulated Plex and indexer services.
