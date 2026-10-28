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
  <img alt="Unsafe code only for the SHA instructions" src="https://img.shields.io/badge/unsafe-SHA%20instructions%20only-brightgreen">
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#set-up-your-library">Set up</a> ·
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
  library only: zero Cargo dependencies and no external program at runtime.
  The only `unsafe` code calls the processor's SHA instructions.
- **It explains itself.** The release list of a title shows which one Mynou
  would pick and why every other one was rejected; you can also pick one
  yourself.
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
| 1. Request | Takes a title from a Plex watchlist, the API or the web interface. |
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

## Set up your library

Mynou keeps its settings in its own storage and you set it up from the web
interface.

**1. Start Mynou:**

```sh
mynou serve
```

It keeps its data in `~/Library/Application Support/Mynou` (use `--dir DIR` or
`MYNOU_HOME` for another place) and creates that folder on first start.

**2. Open <http://127.0.0.1:8787/>**. There is no account or sign-in: Mynou
listens on your Mac alone.

**3. Open the Settings view** and turn on what you use. Plex and TMDB start
disabled:

| To set up | In the Settings view | Secret |
| --- | --- | --- |
| Library folders | Library | |
| Downloads | Downloads (folders, port 6881, limits, seeding) | |
| Plex server and watchlist | Plex | Plex token |
| TMDB catalog, to find titles and follow series | TMDB catalog | TMDB token |
| Sources to search | Indexers | API key of each source |
| How torrents connect | VPN: a WireGuard profile, or your own connection | |

Torrents wait until the VPN page has a choice: import your provider's WireGuard
profile, or choose **Use my own connection**. The overview shows this until it
is done.

Saving validates the settings and applies them at once; an invalid change is
refused and leaves the saved settings untouched. Tokens are kept in Mynou's
storage and never shown again. Every setting, including release profiles,
requester accounts and IRC, is also available as JSON in the same view, through
`GET` and `POST /api/settings`, and `POST /api/secrets`. The [configuration reference](docs/macos.md#configure-mynou)
lists every setting and the [sources guide](docs/sources.md) shows how to
describe an indexer.

> [!NOTE]
> For unattended account-login startup, use the
> [native launchd service](docs/macos-service.md).

## Web interface

The interface is served by Mynou itself at `http://127.0.0.1:8787/`: plain
HTML, CSS and JavaScript embedded in the executable, with no build step.

- **Five views**: overview, library (films and series), jobs, transfers and
  settings, updated live.
- **Like Radarr and Sonarr**: find a film or a series on TMDB and add it;
  Mynou then searches the indexers by itself. From a job, or from any episode
  of a followed series, **Choose a release** lists what the indexers offer,
  with why each release was kept or rejected, and downloads the one you pick.

It has no sign-in, so it listens on a loopback address only and answers pages
and commands of this Mac, never a page of another site.

Everything is also available over HTTP from this Mac: `/api` routes, and
`/healthz` and `/readyz` for health.

## Features

| | |
| --- | --- |
| **Requests** | Plex watchlists from one or several accounts, the web interface and an HTTP API. |
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
- **Native WireGuard protects the supported torrent paths**, over IPv4 and,
  when the profile routes it, IPv6. The
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
<summary><strong>A search finds nothing</strong></summary>

Open **Settings → Indexers** to see the state of each source. Series
also need the TMDB catalog (enabled in Settings, with its token). The
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

Mynou updates itself, like Plex. It checks for a new release once a day and
installs it once no request is in progress, after backing up its storage. If
the new version cannot start, Mynou goes back to the previous version and
data. `mynou update` installs one now. For your own backups, stop the service
and copy the data directory (its `state/` and `downloads/` folders, which also
hold your settings) and your library. The details, including updating by hand,
are in [Back up and upgrade](docs/macos.md#back-up-and-upgrade).
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
- [Plex-plus-Mynou cutover qualification](docs/stack-cutover.md).

## Build from source

With Rust 1.99.1 and no network access needed:

```sh
cargo build --release --offline --locked
```

The executable is `target/release/mynou`.

## Contributing and security

Bug reports and feature ideas are welcome through the
[issue templates](https://github.com/alecerf/mynou/issues/new/choose). Product
code stays on the Rust standard library: no dependency, no external helper,
and no `unsafe` outside `src/crypto/sha_arm.rs`.

Found a vulnerability? Do not open a public issue; follow
[SECURITY.md](SECURITY.md).
