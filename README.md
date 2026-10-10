# Mynou

Mynou automates a media library. A Plex watchlist entry, a followed series or a
request you submit becomes a release search, a verified BitTorrent download, an
import into your library and a confirmed Plex update.

Mynou is a single program written in safe Rust using only the standard library:
no Cargo dependencies, no `unsafe` code and no external programs at runtime. Its
BitTorrent client, media parsers, HTTP and TLS stack and storage journal are
part of the project, so it needs no SQLite, ffprobe, qBittorrent, Radarr or
Sonarr. Plex, TMDB and your indexers are network services you configure.

## What it does

- **Requests** from Plex watchlists (one or several accounts), the command
  line and an HTTP API.
- **Sources**: RSS, JSON and Torznab indexers, with optional authentication,
  and opt-in IRC announcement channels.
- **Release selection** with movie and episode profiles (resolution, source,
  codec, language, terms and scores) and previews that explain every decision.
- **BitTorrent**: v1, v2 and hybrid torrents, magnets, trackers, DHT and PEX,
  parallel peers, pause and resume, priorities, bandwidth limits and seeding
  policies.
- **Series**: TMDB episode plans, automatic requests for newly aired episodes,
  an episode calendar, explicit numbering, season packs and multi-episode
  files.
- **Library**: imports that never overwrite, Plex confirmation, and monitored
  quality upgrades that keep earlier files.
- **Media analysis** of MP4/MOV, Matroska/WebM, AVI, WAV/RF64, FLAC and MP3
  metadata.

Every feature has explicit bounds, listed in [limits](docs/limits.md).

## Try it

Each [release](https://github.com/alecerf/mynou/releases) provides
`mynou-vVERSION-macos-arm64`, the executable for Apple Silicon Macs, and a
`SHA256SUMS` manifest. [Verify](docs/validation.md#verify-release-assets) the
file you downloaded, install it as `mynou` and run the demo:

```sh
install -m 755 mynou-vVERSION-macos-arm64 "$HOME/.local/bin/mynou"
mynou demo --dir "$HOME/mynou-demo"
```

The demo starts a local torrent peer and simulated Plex and indexer services,
downloads synthetic media, analyzes and imports it, and confirms it in the
simulated Plex. It needs no account or secret; the directory must not exist yet.
`mynou analyze FILE --json` prints the metadata of any supported media file.

To build from source, use Rust 1.99.0: `cargo build --release --offline --locked`.

## Install

Mynou runs on macOS with Apple Silicon: follow the [macOS guide](docs/macos.md).
There is no Intel, Linux or Docker build.

## First start

```sh
mynou init --config ./mynou.json
mynou doctor --config ./mynou.json
mynou serve --config ./mynou.json
```

`init` creates `mynou.json` and a private `.env` holding a random API token.
Run `mynou doctor --config ./mynou.json` to list what is still missing; the HTTP
API at `http://127.0.0.1:8787` takes `MYNOU_API_TOKEN` from `.env` as a Bearer token.

## Essential configuration

Plex and TMDB start disabled. Edit the generated `mynou.json`, export secrets in
the environment of the service (only `MYNOU_API_TOKEN` is read from `.env`) and
restart the service after each change:

| What | Settings in `mynou.json` | Secret |
| --- | --- | --- |
| Library folders | `library.movies_root`, `library.series_root` | |
| Downloads | `downloads` (folders, port 6881, peers, limits) | |
| Plex server and watchlist | `plex.enabled`, `plex.url`, `plex.movies_section`, `plex.series_section` | `MYNOU_PLEX_TOKEN` |
| TMDB catalog, needed for series | `catalog.enabled` | `MYNOU_TMDB_TOKEN` or `MYNOU_TMDB_API_KEY` |
| Sources to search | `indexers` | `MYNOU_INDEXER_API_KEY` or per source |

Relative paths resolve against the configuration file. The
[configuration reference](docs/macos.md#configure-mynou) lists every
setting.

## Everyday commands

```sh
mynou search --title "Example Movie" --year 2026 --config ./mynou.json
mynou submit --title "Example Movie" --year 2026 --config ./mynou.json
mynou submit --title "Local movie" --path ./movie.mp4 --config ./mynou.json
mynou sync --config ./mynou.json
mynou jobs --config ./mynou.json
mynou show ID --config ./mynou.json
mynou events ID --config ./mynou.json
mynou retry ID --config ./mynou.json
mynou cancel ID --config ./mynou.json
mynou track-series --title "Example Series" --year 2026 --config ./mynou.json
mynou calendar --config ./mynou.json
mynou library --config ./mynou.json
mynou upgrades --config ./mynou.json
mynou torrents --config ./mynou.json
mynou status --config ./mynou.json
```

`search` previews the release Mynou would choose without recording anything.
`submit` records a request: without `--url` or `--path`, Mynou searches your
sources. `sync` reads Plex watchlists now. Commands use the API while the
service runs and otherwise open the local journal; series, calendar and transfer
commands need the running service. `mynou help` lists every command.

## Documentation

For users and operators:

- [Install and configure on macOS](docs/macos.md)
- [Sources: indexers and IRC announcements](docs/sources.md)
- [Release selection profiles](docs/selection.md)
- [Series, calendar and episode numbering](docs/series.md)
- [Season packs and shared videos](docs/packs.md)
- [Library monitoring and upgrades](docs/library.md)
- [Torrent transfers](docs/transfers.md)
- [Plex requester accounts](docs/requesters.md)
- [Supported formats and limits](docs/limits.md)
- [Release notes](docs/releases/)

For contributors:

- [Architecture](docs/architecture.md)
- [CI, releases and download verification](docs/validation.md)
- Agents build Mynou through GitHub Issues and pull requests; see
  [AGENTS.md](AGENTS.md) and the [engineering runbook](engineering/README.md).
