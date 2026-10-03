# Mynou 0.6.1 — Rust, standard library only

Mynou automates a media library: a Plex request or local submission becomes a
search, verified torrent download, media import, and confirmed Plex update.
Its components use safe Rust with **zero Cargo dependencies**, including build
and development dependencies. There is no bundled third-party code, FFI,
`unsafe`, or external program invocation at runtime.

The BitTorrent client, media parsers, HTTP/TLS stack, JSON/bencode formats, and
durable journal belong to the project. SQLite, ffprobe, Go, qBittorrent, Radarr,
and Sonarr are not required. Plex, TMDB, and your chosen sources are configurable
network integrations.

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
docker load -i mynou-v0.6.1-linux-amd64-image.tar.gz
# Alternative: docker build -t mynou:0.6.1 .
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
- Persistent requests, deduplication, worker leases, retries, cancellation,
  imports without overwriting existing files, and preservation of source files.
- Plex watchlists, TMDB enrichment, RSS/JSON/Torznab sources, Plex refresh and
  availability confirmation, an authenticated local HTTP API, and management CLI.

Formats and protocols have explicit limits. Analysis does not decode pictures
or sound and does not replace Plex transcoding. Older SQLite and Go state remain
separate; this version does not migrate them implicitly. See the
[supported formats and limits](docs/limits.md).

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
open its local journal. They do not start a second download service. For a local
Docker submission, use a path visible inside the container.

## Development and releases

Validation runs **only in GitHub Actions**. Do not run tests or lint locally.
Commit meaningful changes, push to the repository, and inspect the Actions run.
Fix a failing run with another commit and push; proceed once required checks
are green.

CI checks the offline Cargo graph, formatting, Clippy, tests, release builds,
and Docker behavior. The Cargo graph must contain exactly one package, `mynou`,
with no dependencies. Network tests use local services.

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
[Performance](docs/performance.md) · [Validation](docs/validation.md)
