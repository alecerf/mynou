<h1 align="center">Mynou</h1>

<p align="center">
  <strong>Your media library on autopilot, in one self-contained program.</strong><br>
  Add a title to your Plex watchlist. Mynou finds a release, downloads it,
  files it in your library and checks that Plex shows it.
</p>

<p align="center">
  <a href="https://github.com/alecerf/mynou/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/alecerf/mynou?label=release"></a>
  <img alt="Platform: macOS on Apple Silicon" src="https://img.shields.io/badge/platform-macOS%20Apple%20Silicon-lightgrey">
  <img alt="Zero dependencies" src="https://img.shields.io/badge/dependencies-0-brightgreen">
  <img alt="No unsafe code" src="https://img.shields.io/badge/unsafe-forbidden-brightgreen">
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#set-up-your-library">Set up</a> ·
  <a href="#everyday-commands">Commands</a> ·
  <a href="#features">Features</a> ·
  <a href="#documentation">Documentation</a> ·
  <a href="#questions-and-troubleshooting">Help</a>
</p>

---

## Why Mynou

- **One program instead of a stack.** The BitTorrent client, the release
  search, the media parsers, the web interface and the storage are all inside
  one executable. There is no qBittorrent, Radarr, Sonarr, ffprobe, SQLite or
  Docker to install and keep in sync.
- **Nothing hidden underneath.** Mynou is safe Rust built on the standard
  library only: zero Cargo dependencies, no `unsafe` code and no external
  program at runtime.
- **It explains itself.** A search preview shows which release Mynou would pick
  and why every other one was rejected, before anything is downloaded.
- **Careful with your files.** Imports never overwrite, upgrades keep the
  earlier file, and cancelling a request never deletes anything.

You bring the network services: Plex, a TMDB account for series, and the
indexers you want Mynou to search.

## How it works

<p align="center">
  <strong>Request</strong> → <strong>Search</strong> → <strong>Select</strong> →
  <strong>Download</strong> → <strong>Analyze</strong> → <strong>Import</strong> →
  <strong>Confirm</strong>
</p>

| Step | What Mynou does |
| --- | --- |
| 1. Request | Takes a title from a Plex watchlist, the command line, the API or the web interface. |
| 2. Search | Asks the indexers you configured for matching releases. |
| 3. Select | Picks the best release for your profile and records why the others lost. |
| 4. Download | Fetches it with the built-in BitTorrent client and verifies every piece. |
| 5. Analyze | Reads the media file's metadata to check what was downloaded. |
| 6. Import | Files it in your library without overwriting anything. |
| 7. Confirm | Asks Plex to scan and waits until Plex lists the title. |

After that, Mynou can keep watching: new episodes of the series you follow are
requested when they air, and better releases of titles you already own can be
imported as upgrades.

## Quick start

**You need:** a Mac with Apple Silicon. Nothing else — no account, no token, no
Rust.

**1. Download, verify and install.** Paste this into Terminal, with `VERSION`
set to the [latest release](https://github.com/alecerf/mynou/releases/latest):

```sh
VERSION=0.23.0
cd "$HOME/Downloads" &&
  curl -fLO "https://github.com/alecerf/mynou/releases/download/v$VERSION/mynou-v$VERSION-macos-arm64" &&
  curl -fLO "https://github.com/alecerf/mynou/releases/download/v$VERSION/SHA256SUMS" &&
  awk -v file="mynou-v$VERSION-macos-arm64" '$2 == file' SHA256SUMS | shasum -a 256 -c - &&
  mkdir -p "$HOME/.local/bin" &&
  install -m 755 "mynou-v$VERSION-macos-arm64" "$HOME/.local/bin/mynou"
```

Each step runs only if the previous one succeeded, so a failed checksum stops
before anything is installed. Prefer clicking? Download both files from the
[Releases page](https://github.com/alecerf/mynou/releases) and follow the
[macOS guide](docs/macos.md).

**2. Put `mynou` on your `PATH`** (add this line to `~/.zshrc` to keep it):

```sh
export PATH="$HOME/.local/bin:$PATH"
```

**3. Watch it work.** The demo runs the whole chain on your Mac with a local
torrent peer, a simulated indexer and a simulated Plex. It downloads synthetic
media, analyzes it, imports it and confirms it:

```sh
mynou demo --dir "$HOME/mynou-demo"
```

The directory must not exist yet. Delete it whenever you like.

> [!TIP]
> Curious about a file you already have? `mynou analyze FILE --json` prints its
> container, tracks, codecs, duration and languages.

## Set up your library

**1. Create the configuration** in a folder you own, outside Downloads:

```sh
mkdir -p "$HOME/Library/Application Support/Mynou" &&
  cd "$HOME/Library/Application Support/Mynou" &&
  mynou init &&
  mynou doctor
```

`init` writes `mynou.json` and a private `.env` holding a random API token.
`doctor` tells you whether the configuration is valid and what is reachable.

**2. Edit `mynou.json`.** Plex and TMDB start disabled; turn on what you use:

| To set up | Settings in `mynou.json` | Secret (environment variable) |
| --- | --- | --- |
| Library folders | `library.movies_root`, `library.series_root` | |
| Downloads | `downloads` (folders, port 6881, peers, limits) | |
| Plex server and watchlist | `plex.enabled`, `plex.url`, `plex.movies_section`, `plex.series_section` | `MYNOU_PLEX_TOKEN` |
| TMDB catalog, needed for series | `catalog.enabled` | `MYNOU_TMDB_TOKEN` or `MYNOU_TMDB_API_KEY` |
| Sources to search | `indexers` | `MYNOU_INDEXER_API_KEY`, or one per source |

Relative paths resolve against the folder of `mynou.json`. Unknown settings are
rejected, so a typo is reported instead of ignored. The
[configuration reference](docs/macos.md#configure-mynou) lists every setting
and the [sources guide](docs/sources.md) shows how to describe an indexer.

**3. Start the service** with your secrets exported in the same shell:

```sh
export MYNOU_PLEX_TOKEN='replace-with-your-Plex-token'
export MYNOU_TMDB_TOKEN='replace-with-your-TMDB-token'
mynou serve
```

**4. Open <http://127.0.0.1:8787/>** and paste the API token: it is the
`MYNOU_API_TOKEN` value in the `.env` file next to `mynou.json`.

> [!NOTE]
> Foreground `serve` reads only `MYNOU_API_TOKEN` from `.env`; every other secret
> comes from its environment. For unattended account-login startup, use the
> [native launchd service](docs/macos-service.md), which injects a private snapshot
> of the adjacent `.env`. Restart after configuration changes; reinstall the
> registration after changing that secret snapshot.

## Web interface

The interface is served by Mynou itself at `http://127.0.0.1:8787/`: plain
HTML, CSS and JavaScript embedded in the executable, with no build step.

- **Seven views**: dashboard, requests, jobs, transfers, library, series and
  sources, updated live.
- **Preview before you commit**: search a title, read why each release was
  kept or rejected, then submit it explicitly.
- **Keyboard first**: `/` or `Ctrl+K` opens the command palette, `1`–`7`
  switch views.

The token stays in the browser tab and the interface accepts same-origin pages
only. It listens on your Mac alone by default; to reach it from elsewhere, put
a reverse proxy with TLS that you trust in front of it.

## Everyday commands

Run these from the folder that holds `mynou.json`, or add `--config PATH`.

| I want to… | Command |
| --- | --- |
| See which release would be chosen, without recording anything | `mynou search --title "Example Movie" --year 2026` |
| Request a movie and let Mynou search for it | `mynou submit --title "Example Movie" --year 2026` |
| Request it from a magnet or torrent I already have | `mynou submit --title "Example Movie" --year 2026 --url 'magnet:?xt=urn:btih:...'` |
| Import a file that is already on my Mac | `mynou submit --title "Local movie" --path ./movie.mp4` |
| Read my Plex watchlists now | `mynou sync` |
| Follow a series | `mynou track-series --title "Example Series" --year 2026` |
| See upcoming episodes | `mynou calendar` |
| List requests and their state | `mynou jobs` |
| Understand what happened to one request | `mynou show ID`, `mynou events ID` |
| Try a request again, or stop it | `mynou retry ID`, `mynou cancel ID` |
| Watch downloads | `mynou torrents` |
| See my library and available upgrades | `mynou library`, `mynou upgrades` |
| Check that everything is healthy | `mynou status`, `mynou doctor` |

`mynou help` lists every command. Commands talk to the running service when
there is one and otherwise read the local journal; series, calendar and
transfer commands need the service.

Everything is also available over HTTP: `/api` routes take the API token as an
`Authorization: Bearer` header, and `/healthz` and `/readyz` report health.

## Features

| | |
| --- | --- |
| **Requests** | Plex watchlists from one or several accounts, the command line, the web interface and an HTTP API. |
| **Sources** | RSS, JSON and Torznab indexers with optional authentication, opt-in feed watching for new releases, and opt-in IRC announcement channels. |
| **Release selection** | Movie and episode profiles on resolution, source, codec, language, terms and scores, with previews that explain every decision. |
| **BitTorrent** | v1, v2 and hybrid torrents, magnets, trackers, DHT and PEX, parallel peers, pause and resume, priorities, bandwidth limits and seeding policies. |
| **Series** | TMDB episode plans, automatic requests for newly aired episodes, an episode calendar, explicit numbering, season packs and multi-episode files. |
| **Library** | Imports that never overwrite, Plex confirmation, and monitored quality upgrades that keep earlier files. |
| **Media analysis** | Metadata of MP4/MOV, Matroska/WebM, AVI, WAV/RF64, FLAC and MP3 files. |

## Good to know

Mynou is young and deliberately explicit about its edges:

- **macOS on Apple Silicon only.** There is no Intel, Linux, Windows or Docker
  build.
- **Not a drop-in replacement** for Radarr, Sonarr, Prowlarr or qBittorrent: it
  covers the same journey with fewer options.
- **Every feature has a stated bound**, and exceeding one produces a clear
  error instead of unbounded growth.
- **Native WireGuard protects the supported IPv4 torrent paths.** The
  [macOS installation and migration guide](docs/wireguard-macos-migration.md)
  explains the traffic boundary, supported NAT-PMP forwarding and preserved
  state; [controlled evidence and limits](docs/wireguard-acceptance.md) describe
  what was verified.
- **The network and cryptographic code is original** and has not had an
  independent review.

The full list lives in [supported formats and limits](docs/limits.md),
including [what is not supported](docs/limits.md#not-supported).

## Questions and troubleshooting

<details>
<summary><strong>macOS refuses to open the executable</strong></summary>

The executable is not signed with an Apple Developer identity or notarized.
Once you have verified the checksum, open **System Settings → Privacy &
Security**, choose **Open Anyway** for Mynou and confirm. Keep Gatekeeper
enabled.
</details>

<details>
<summary><strong><code>command not found: mynou</code></strong></summary>

`$HOME/.local/bin` is not on your `PATH`. Run
`export PATH="$HOME/.local/bin:$PATH"`, add that line to `~/.zshrc`, or call
`"$HOME/.local/bin/mynou"` directly.
</details>

<details>
<summary><strong>The demo says the directory already exists</strong></summary>

`mynou demo` only writes into a directory it creates. Pick a new path or remove
the previous demo directory.
</details>

<details>
<summary><strong>Where is my API token?</strong></summary>

In the `.env` file that `mynou init` created next to `mynou.json`, as
`MYNOU_API_TOKEN`. Treat that file as data: do not `source` it.
</details>

<details>
<summary><strong>A search finds nothing</strong></summary>

Run `mynou doctor`, then `mynou indexers` to see the state of each source. Series
also need the TMDB catalog (`catalog.enabled` and its token). The
[sources guide](docs/sources.md#health-pause-and-probe) explains source health.
</details>

<details>
<summary><strong>Plex does not show an imported title</strong></summary>

Plex and Mynou must see the library at the same paths. If Plex sees it under
another prefix, add a path mapping as described in
[Connect Plex](docs/macos.md#connect-plex).
</details>

<details>
<summary><strong>How do I upgrade or back up?</strong></summary>

Stop the service, back up `mynou.json`, `.env`, the `state/` and `downloads/`
folders and your library, then verify and install the new executable at the
same path. The details, including rolling back, are in
[Back up and upgrade](docs/macos.md#back-up-and-upgrade).
</details>

Still stuck? [Open an issue](https://github.com/alecerf/mynou/issues/new/choose),
without tokens, private paths or media names.

## Documentation

**Using Mynou**

| Guide | What you will find |
| --- | --- |
| [Install and configure on macOS](docs/macos.md) | Installation, every setting, Plex, TLS, backups and upgrades |
| [Sources](docs/sources.md) | Indexers, feed watching and IRC announcements |
| [Release selection](docs/selection.md) | Profiles that decide which release wins |
| [Series](docs/series.md) | Following series, the calendar and episode numbering |
| [Season packs](docs/packs.md) | Packs and videos shared by several episodes |
| [Library](docs/library.md) | Monitoring and quality upgrades |
| [Torrent transfers](docs/transfers.md) | Priorities, limits, seeding and cross-seeding |
| [Plex requester accounts](docs/requesters.md) | Watchlists from several Plex accounts |
| [Formats and limits](docs/limits.md) | What is supported, bounded and not supported |
| [Release notes](https://github.com/alecerf/mynou/releases) | What changed in each version |

**Working on Mynou**

- [Architecture](docs/architecture.md): how a request flows through the modules.
- [CI, releases and download verification](docs/validation.md).

## Build from source

With Rust 1.99.0 and no network access needed:

```sh
cargo build --release --offline --locked
```

The executable is `target/release/mynou`.

## Contributing and security

Bug reports and feature ideas are welcome through the
[issue templates](https://github.com/alecerf/mynou/issues/new/choose). Product
code stays on the Rust standard library: no dependency, no `unsafe`, no
external helper.

Found a vulnerability? Do not open a public issue; follow
[SECURITY.md](SECURITY.md).
