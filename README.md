# Mynou 0.22.11 — Rust, standard library only

Mynou automates a media library: a Plex request or local submission becomes a
search, verified torrent download, media import, and confirmed Plex update.
Its components use safe Rust with **zero Cargo dependencies**, including build
and development dependencies. There is no bundled third-party code, FFI,
`unsafe`, or external program invocation at runtime.

The BitTorrent client, media parsers, HTTP/TLS stack, JSON/bencode formats, and
durable journal belong to the project. SQLite, ffprobe, Go, qBittorrent, Radarr,
and Sonarr are not required. Plex, TMDB, and your chosen sources are configurable
network integrations.

Explicit [episode numbering](docs/numbering.md) separates source/catalog labels
from retained library identities. Preview and apply choices through CLI/API/browser;
existing jobs and imports keep their original numbers. Explicit
[shared video ownership](docs/shared-files.md) now binds consecutive episodes to
one authenticated torrent path and one Plex range file, with guarded preview/apply.
Reviewed [group upgrades](docs/group-upgrades.md) record complete baselines and
replacement lineage. Confirmed owners stage until one atomic promotion makes the
whole replacement current; prior library files remain in place.

[Plex requester policies](docs/requesters.md) add stable account bindings, explicit
opt-in profiles, durable approvals and quotas, captured destinations and recorded
notification outcomes. Compatible accounts share acquisition; removals retain
other demand and ready media. CLI, API and browser controls require reviewed
scope guards. Configurations without requester accounts retain single-account
behavior. [v0.19.1](https://github.com/alecerf/mynou/releases/tag/v0.19.1) verifies
destination and quality before new requester reuse of completed operator imports.
CI published it after all five jobs passed with **490 Rust tests** and four
scheduler checks. See the [release evidence](docs/validation.md#recorded-0191-ci-evidence).

[IRC announcement reviews](docs/irc.md) add opt-in verified TLS receivers,
sender/channel restrictions, deterministic filters, durable duplicate suppression,
source health and guarded CLI/API/browser reviews. Explicit grab rules now route
hash-pinned magnets to existing approved requests after metadata verification.
Immutable origins retain exact files, profiles and physical ownership through
retry/restart. CI published
[v0.20.1](https://github.com/alecerf/mynou/releases/tag/v0.20.1) after all five jobs
passed with **533 Rust tests** and four scheduler checks. See the
[recorded evidence](docs/validation.md#recorded-0201-ci-evidence).

Optional required SASL PLAIN authenticates IRC connections before channel
membership. Bounded capability negotiation and credential chunks reject failed
authentication without fallback. CI published
[v0.20.2](https://github.com/alecerf/mynou/releases/tag/v0.20.2) after all five jobs
passed with **543 Rust tests** and four scheduler checks. See
[IRC authentication](docs/irc.md#required-sasl-plain-authentication) and the
[release evidence](docs/validation.md#recorded-0202-ci-evidence).

Configurable fixed-delimiter text announcements now share the same claim,
filter and verified import path. IRC display formatting is bounded, links are
excluded from fields, and text previews are available through the CLI/API.
CI published [v0.20.3](https://github.com/alecerf/mynou/releases/tag/v0.20.3)
after all five jobs passed with **551 Rust tests** and four scheduler checks.
See the [recorded evidence](docs/validation.md#recorded-0203-ci-evidence).

Required [NickServ identification](docs/irc.md#required-nickserv-identification-in-0204)
now gates channel entry on exact trusted account confirmation. Failures close
the connection; retries identify again and public health redacts credentials.
CI published [v0.20.4](https://github.com/alecerf/mynou/releases/tag/v0.20.4)
from its exact validated source after all five jobs passed in run 37362054311,
attempt 4: 560 Rust tests across 50 harnesses and four scheduler checks.
See the [recorded evidence](docs/validation.md#recorded-0204-ci-evidence).

Optional [requester selectors](docs/irc.md#requester-selectors-in-0205) now bind
IRC grabs to one account's compatible approved demand, including shared work.
Waiting and metadata admission recheck the same captured interest.
CI published [v0.20.5](https://github.com/alecerf/mynou/releases/tag/v0.20.5)
after all five jobs passed with 568 Rust tests across 51 harnesses and four
scheduler checks. Exact source/tag/seven-asset evidence is recorded in
[validation](docs/validation.md#recorded-0205-ci-evidence).

[Reviewed IRC requests](docs/irc.md#reviewed-requester-demand-in-0206) now confirm
fresh catalog and requester identities, then retain canonical demand under the
existing approval, quota, sharing and captured-route controls. Explicit origins
survive empty watchlist polls; checked intent recovery prevents replay. CLI, API
and browser controls require the reviewed guard. The 0.20.6 implementation and
its original local-service fixtures passed all five jobs in run 37427693256:
585 Rust tests across 52 harnesses and four scheduler checks. CI published
[v0.20.6](https://github.com/alecerf/mynou/releases/tag/v0.20.6) from the exact
validated source with seven assets; the prior release remained unchanged.

[Durable notification delivery](docs/notifications.md) now records requester and
IRC outcomes in their owning snapshots before native HTTP delivery. Opt-in routes
retain immutable identities, stable event IDs, bounded attempts and redacted
CLI/API/browser controls. Delivery is at-least-once; receiver deduplication is
required. CI published v0.20.7 after all five jobs in run 37429460979 passed:
602 Rust tests across 53 harnesses and four scheduler checks. The exact tag/seven
assets were verified, preserving v0.20.6.

[Native indexer authentication](docs/indexers.md) adds Basic, Bearer and an
explicit single-cookie form adapter. Origin-bound in-memory sessions renew once
on a 401. Disabled/rate-limited sources reject new fetches, and protected source
health views expose fixed diagnostics. v0.21.0 passed all five jobs in run
37431238845 with 616 Rust tests across 54 harnesses and seven CI-published assets.
v0.21.1 also passed all five jobs with 628 Rust tests and seven CI-published assets.
Checked source policy and guarded controls retain pauses through restart.

[Native Usenet formats](docs/usenet.md) begin in 0.22.0 with read-only NZB
inspection, original yEnc decoding and mandatory part/whole-file CRC checks.
Multipart assembly rejects missing, overlapping or conflicting ranges. This
format scope passed all five jobs with 639 Rust tests and seven published assets.
The 0.22.1 scope adds original bounded NNTP/TLS authentication, exact article
body transport and reviewed non-acquiring probes through CLI/API/browser paths.
v0.22.1 passed all five jobs with 652 Rust tests and seven published assets.
v0.22.2 passed all five jobs with 663 Rust tests across 57 harnesses and seven
published assets. Checked private receipts support recoverable streamed output.
v0.22.3 passed all five jobs with 680 Rust tests across 58 harnesses and seven
published assets. Its durable native staging queue retains verified receipts and
article attempt budgets. v0.22.4 passed all five jobs with 693 Rust tests across
59 harnesses and seven published assets. v0.22.5 passed all five jobs with 712
Rust tests across 60 harnesses and seven published assets. Held owner bindings,
nonpersistent permission and revocation protect private verified output.
v0.22.6 connects Newznab and native Usenet to approved canonical
movie/episode jobs, captured quality/destinations, identity and size checks,
atomic imports and exact Plex confirmation. All five jobs passed with 729 Rust
tests and seven bot-published assets. Only
single-file direct-media NZBs are supported in this increment; archive/PAR2,
packs and Usenet upgrades remain later stages. See [Newznab configuration](docs/usenet.md#native-newznab-discovery-in-0224),
[held ownership](docs/usenet.md#held-library-ownership-in-0225) and
[library admission](docs/usenet.md#native-library-admission-in-0226).

v0.22.7 passed all five jobs with 750 Rust tests and seven bot-published assets,
providing [bounded ZIP and streaming DEFLATE](docs/archives.md) and read-only
`zip-inspect`. v0.22.8 passed all five jobs with 773 Rust tests and seven assets, adding opt-in
ownership-bound ZIP library admission for one media entry with the existing
requester, identity, import and Plex gates. v0.22.9 passed all five jobs with 791 Rust tests, adding original bounded RAR5
stored-file formats and read-only `rar-inspect`. v0.22.10 passed all five jobs with
804 Rust tests, adding opt-in stored RAR5 library admission with captured format/limits
and the existing private ownership/import/Plex gates.

0.22.11 adds [native PAR2 inspection](docs/par2.md) through `par2-inspect FILE`: bounded
core packets, verified metadata identity and read-only JSON reports. Content
verification, repair, volume merging and library admission remain future work.
Compressed RAR, multi-file packs and Usenet upgrades remain subsequent increments.

## Autonomous engineering

The [engineering organization](engineering/README.md) owns intent, native GitHub
backlog, sequential specialist roles, CI, logical review, delivery and recovery.
Only one engineering worker may execute at once. Bootstrap
[Issue #1](https://github.com/alecerf/mynou/issues/1) is complete; active product
work and recovery are tracked by native Issues and the GitHub control checkpoint.

## Try it

GitHub Actions publishes validated releases in
[alecerf/mynou](https://github.com/alecerf/mynou/releases). The source archive
includes a static **Linux x86_64** binary:

```sh
./bin/mynou analyze examples/demo.mp4 --json
./bin/mynou demo --dir /tmp/mynou-demo
```

To build the sources with Rust **1.99.0**:

```sh
cargo build --release --offline --locked
./target/release/mynou analyze examples/demo.mp4 --json
./target/release/mynou demo --dir /tmp/mynou-demo
```

The demo starts a local torrent peer and simulated Plex/indexer responses,
downloads the included synthetic media, analyzes it, imports it, and confirms
availability. It needs no Plex account, public source, or personal secret. The
demo directory must not already exist.

## Install with Docker

The final `scratch` image contains the static binary and TLS trust data. It runs
as user 1000 and contains no shell or shared library. Download the image archive
from [GitHub Releases](https://github.com/alecerf/mynou/releases) and load it, or
build the image from the source archive:

```sh
docker load -i mynou-v0.22.11-linux-amd64-image.tar.gz
# Alternative: docker build -t mynou:0.22.11 .
./bin/mynou setup-docker --dir ./mynou-docker
cd mynou-docker
docker compose up -d
docker compose exec mynou /mynou doctor --config /config/mynou.json
```

Configuration, an API token, the data directory, and the library directories are
generated. If your user ID is not 1000, set `MYNOU_UID` and `MYNOU_GID` in `.env`
before starting Compose. The [Docker guide](docs/deployment.md) explains file
ownership, shared Plex paths, and installation without Rust on the host.

## Implemented features

- Native MP4/MOV, Matroska/WebM, AVI, WAV/RF64, FLAC, and MP3 analysis: container,
  title, date, size, declared duration, and video/audio tracks.
- v1, v2, and hybrid torrents; `btih`/`btmh` magnets; peer metadata exchange;
  SHA-1/SHA-256 Merkle verification; restart recovery; and verified-file seeding.
- HTTP/HTTPS/UDP trackers, DHT, and PEX discovery, with private-torrent rules.
  Peer data uses TCP.
- Bounded parallel TCP peers share verified piece work within each transfer.
  Corrupt peer data is rejected; completed pieces still pass normal hash and
  final torrent verification before becoming ready.
- Persistent requests, deduplication, worker leases, retries, cancellation,
  imports without overwriting existing files, and preservation of source files.
- Plex watchlists, TMDB enrichment, RSS/JSON/Torznab sources, Plex refresh and
  availability confirmation, an authenticated local HTTP API, and management CLI.
- Named movie and episode selection profiles: resolution, source, codec and
  language preferences, required/blocked title terms, custom scores, and a
  minimum score. CLI/API previews explain accepted and rejected candidates
  without submitting a job or exposing acquisition URLs.
- Owned library records, per-entry monitoring, resolution cutoffs and controlled
  upgrades. Preview before applying; keep earlier imports current until the
  replacement is ready, with optional Plex path mappings for confirmation.
- Native transfer management: durable pause/resume, priority/FIFO scheduling,
  per-file piece priorities, global payload bandwidth limits, persistent
  counters and ratio/time seeding policies. Controls retain library files and
  downloads. Per-file priorities order required pieces; mapped packs can acquire a
  durable union of selected files without downloading unrelated pieces.
- Durable series monitoring with retained TMDB episode plans, newly aired
  episode acquisition, optional specials, earliest-air-date choices and
  per-episode exclusions; a paginated episode calendar shares CLI/API/browser
  controls. Unknown air dates or missing episode identities do not acquire
  automatically.
- Explicit season-pack acquisitions map exact video paths to catalog episodes.
  Mapped jobs share one native verified torrent and import only their selected
  files. New series can be saved without automatic acquisition while choosing a
  pack. New native pack transfers acquire only their retained file interests and
  necessary boundary pieces; partial availability remains distinct from complete
  torrent readiness and seeding.
- Automatic season-pack search ranks titles under the episode profile and
  inspects authenticated metadata without payload downloads. Unique numbered
  files must cover every eligible missing episode. CLI/API/browser previews and
  guarded apply bind catalog scope, source hash and exact mappings. Optional
  `series_packs.enabled` prefers packs during monitoring with individual fallback.
- Explicit shared videos bind 2–64 consecutive canonical episodes to one
  authenticated torrent path and one Plex range import. Guarded CLI/API/browser
  preview/apply records all owners atomically; cancellation, retry and subset
  requests preserve that ownership. Shared owners require group upgrades.
- Whole-group baselines and shared-video replacements through CLI/API/browser.
  Immutable lineage, staged exact Plex confirmations and atomic promotion retain
  old library versions. Replacement cancellation/retry covers every owner;
  monitoring changes fence claims and promotion.
- Browser management at `/ui`: search/request forms, jobs/history, owned library
  monitoring and upgrades, transfer controls, filters, pagination and bounded
  bulk actions with individual results. Rust renders all pages without scripts
  or frontend dependencies.

Formats and protocols have explicit limits. Analysis does not decode pictures
or sound and does not replace Plex transcoding. Older SQLite and Go state remain
separate; this version does not migrate them implicitly. See the
[supported formats and limits](docs/limits.md).

## Release selection and upgrades

Automatic acquisition can apply separate movie and episode profiles. An empty
profile is unrestricted; existing configurations without `selection` retain
that behavior. A configured profile filters candidates before ranking them by
custom score, ordered preferences, and seed count.

Preview a search before submitting it:

```sh
mynou search --title "Example Movie" --year 2026 --config ./mynou.json
mynou search --title "Example Series" --kind episode --season 1 --episode 2 \
  --config ./mynou.json
```

Searches contact configured sources. They do not start a download or change the
request journal. Selection reads release-title markers, so it cannot verify
actual audio tracks or image quality before downloading. `VOSTFR` does not prove
French audio, and `MULTI` does not identify individual languages. See the
[selection guide](docs/selection.md) for configuration and decision details.

Inspect ready imports and preview upgrades:

```sh
mynou library --config ./mynou.json
mynou upgrades --config ./mynou.json
mynou upgrades --apply --config ./mynou.json
mynou unmonitor ID --config ./mynou.json
```

Background monitoring defaults to disabled. An upgrade needs a recorded release
baseline. Under the current profile, an accepted baseline requires a strict
quality-rank improvement; a baseline rejected by an explicit profile change can
be replaced by an accepted candidate. More seeds alone are insufficient.
Earlier or explicitly submitted imports without a baseline need an explicit
`baseline` command before becoming eligible. Cutoffs
follow resolution preference order. Upgrades use distinct filenames and keep
old files and downloads; there is no automatic cleanup. With Plex enabled, the
new imported path must be confirmed before the replacement becomes current.
See the [library guide](docs/library.md) for configuration and API operations.

The [transfer guide](docs/transfers.md) explains native download controls,
parallel peer bounds, bandwidth limits and seeding policies. Use
`downloads.max_peers: 1` to retain a single-peer transfer baseline.

Mynou remains an early integrated implementation. This release adds automatic
pack search, guarded mappings and opt-in monitored pack preference.
Alternate/anime numbering rules, multi-episode videos, multi-user policies and full parity with
Radarr, Sonarr, Pulsarr, qBittorrent, qui, autobrr or Prowlarr remain future work. The
[release roadmap](docs/roadmap.md) separates the next stages.

## Follow a series

With TMDB and sources configured, track a series and inspect its episode calendar:

```sh
mynou track-series --title "Example Series" --year 2026 --tmdb-id 123 \
  --future-only --config ./mynou.json
mynou series --config ./mynou.json
mynou calendar --from 2026-10-01 --to 2026-10-31 --config ./mynou.json
```

The running service checks retained monitored plans for newly aired episodes.
Plex show requests also create these records. The browser provides Series
settings and Calendar pages. Existing requests prevent automatic duplicates,
including failed and cancelled jobs; use job controls for deliberate retries.
Removing a Plex watchlist entry does not disable its retained series monitoring.
See the [series guide](docs/series.md) for acquisition rules, persistence, bounds
and the independent owned-library upgrade policy.

Choose one torrent for several episodes with `series-pack ID --url … --mapping
FILE`, or use **Acquire a mapped pack** in browser series details.
`track-series --unmonitored` retains a catalog plan before choosing a source.
See the [pack guide](docs/packs.md) for mapping format, shared-transfer behavior
and persistence. The [transfer guide](docs/transfers.md#selective-acquisition-in-0140)
explains verified availability, boundary storage and selection expansion.

Preview automatic mapping before choosing a source:

```sh
mynou series-pack-search ID --season 1 --config ./mynou.json
mynou series-pack-search ID --season 1 --apply --scope-id SCOPE_ID \
  --candidate-id CANDIDATE_ID --config ./mynou.json
```

Copy both guard values from a resolved preview. The browser offers the same
preview/apply flow. Set `"series_packs": { "enabled": true }` to prefer mapped
packs during monitored tracking and refresh; this defaults to false. See the
[automatic pack guide](docs/automatic-packs.md) for eligibility, metadata-only
discovery, bounds and stale-result checks. General numbering variants remain
outside automatic mapping.

## Open the browser interface

After starting the service, open **http://127.0.0.1:8787/ui** and sign in with
`MYNOU_API_TOKEN` from your installation's `.env`. Sessions expire after eight
hours and end at sign-out or service restart. Browser cookies do not authenticate
the Bearer API. Pages use native forms; refresh to see new progress.

The [browser guide](docs/web.md) describes request/search/library/transfer
and series/calendar operations, safe bulk changes and TLS reverse-proxy
deployment for remote access.

## Configuration and commands

```sh
./target/release/mynou init --config ./mynou.json
./target/release/mynou serve --config ./mynou.json
```

`init` creates local configuration and a private `.env` with a random API token.
Plex and TMDB start disabled. Configure their addresses, enable the integrations
you need, and provide secrets in the service environment. The API token can also
be read from the `.env` beside the configuration file.

```sh
mynou submit --title "Local movie" --path ./movie.mp4 --config ./mynou.json
mynou submit --title "Movie" --year 2026 --url 'magnet:?xt=urn:btih:...' --config ./mynou.json
mynou sync --config ./mynou.json
mynou jobs --config ./mynou.json
mynou show ID --config ./mynou.json
mynou events ID --config ./mynou.json
mynou retry ID --config ./mynou.json
mynou cancel ID --config ./mynou.json
mynou status --config ./mynou.json
```

Management commands use the API when the service is running; otherwise they
open its local journal. Dedicated series/calendar commands and pack acquisition
require a running service; `series-pack-search` previews have a read-only offline
fallback. Commands do not start a second download service. For a local
Docker submission, use a path visible inside the container.

## Development and releases

Validation runs **only in GitHub Actions**. Do not run tests or lint locally.
Commit meaningful changes, push to the repository, and inspect the Actions run.
Fix a failing run with another commit and push; proceed once required checks
are green.

CI checks the offline Cargo graph, formatting, Clippy, tests, release builds,
and Docker behavior. The Cargo graph must contain exactly one package, `mynou`,
with no dependencies. Network tests use local services.

All Cargo test harnesses run with bounded process/thread parallelism and retained
per-target timings/logs. Native and static builds run concurrently; Docker and
packaging reuse the checked static artifacts. Every required job still gates
publication. See [CI execution and caches](docs/ci.md) for exact behavior.

A successful run on `trunk` automatically publishes the version from
`Cargo.toml` if it has not been released. CI creates the matching tag and release
from that validated commit, and publishes:

- `mynou-vVERSION-source.zip`, including the static binary and an internal
  `SHA256SUMS` manifest;
- `mynou-vVERSION-linux-x86_64`, the static binary;
- `mynou-vVERSION-linux-amd64-image.tar.gz`, a saved Docker image;
- `SHA256SUMS` and individual `.sha256` files for the release assets.

No Docker Hub account or manual artifact upload is needed. The
[validation guide](docs/validation.md) distinguishes historical 0.6.0 results
from current Actions runs. The [benchmark method](docs/performance.md) explains
the recorded measurements. Neither dependency absence nor passing tests prove
code perfection.

[Architecture](docs/architecture.md) · [Docker installation](docs/deployment.md) ·
[Dependencies](docs/dependencies.md) · [Formats and limits](docs/limits.md) ·
[Release selection](docs/selection.md) · [Library upgrades](docs/library.md) ·
[Transfer controls](docs/transfers.md) · [Series and calendar](docs/series.md) ·
[Pack acquisition](docs/packs.md) · [Automatic packs](docs/automatic-packs.md) ·
[Browser management](docs/web.md) · [Roadmap](docs/roadmap.md) ·
[Performance](docs/performance.md) · [Validation](docs/validation.md)
