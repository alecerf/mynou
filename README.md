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
- **Careful with your files.** Imports never overwrite, a video or a
  subtitle, and never write outside your library folders. A file far shorter
  or longer than the title is refused before anything is written. An upgrade
  deletes the file it replaces only once the new one is in place (and in
  Plex), and cancelling a request never deletes anything.

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
| 6. Import | Checks its length against the title's, then files it with its subtitles in the folder Plex already uses for the title, without overwriting anything. |
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
before anything is installed. To also check that the release key signed the
manifest, download `SHA256SUMS.sig` from the release and
[`release-key.pub.pem`](release-key.pub.pem) from this repository, then run
`openssl dgst -sha256 -verify release-key.pub.pem -signature SHA256SUMS.sig SHA256SUMS`.
Mynou's own updates make both checks. Prefer clicking? Download both files from the
[Releases page](https://github.com/alecerf/mynou/releases).

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

**2. Open <http://127.0.0.1:8787/>**. There is no account or sign-in on your
Mac. To use Mynou from other computers on your network, open Settings,
Network.

**3. Open the Settings view** and turn on what you use. Plex and TMDB start
disabled:

| To set up | In the Settings view | Secret |
| --- | --- | --- |
| Library folders | Library | |
| Downloads | Downloads (folders, port 6881, limits, seeding) | |
| Plex server and watchlist | Plex: **Connect to Plex**, then choose the server | |
| Your Plex friends' watchlists | Friends: a switch for each friend | |
| TMDB catalog, to find titles and follow series | TMDB catalog | TMDB token |
| Sources to search | Indexers | API key of each source |
| What goes through a VPN | VPN: a WireGuard profile, then torrents, indexers, TMDB | |

Without a VPN profile, everything uses your own connection. Once you import
your provider's WireGuard profile, torrents go through it; indexers and TMDB
can too.

Saving validates the settings and applies them at once; an invalid change is
refused and leaves the saved settings untouched. Tokens are kept in Mynou's
storage and never shown again. Every setting, including release profiles,
requester accounts and IRC, is also available as JSON in the same view, through
`GET` and `POST /api/settings`, and `POST /api/secrets`.

> [!NOTE]
> For unattended account-login startup, install the native launchd service
> with `mynou service --action install --dir /absolute/data-directory`.

## Web interface

The interface is served by Mynou itself at `http://127.0.0.1:8787/`: plain
HTML, CSS and JavaScript embedded in the executable, with no build step.

- **Five views**: overview, library (films and series), jobs, transfers and
  settings, updated live.
- **Like Radarr and Sonarr**: find a film or a series on TMDB and add it;
  Mynou then searches the indexers by itself. From a job, or from any episode
  of a followed series, **Choose a release** lists what the indexers offer,
  with why each release was kept or rejected, and downloads the one you pick.

By default it listens on a loopback address only and answers pages and
commands of this Mac, without sign-in, never a page of another site. Settings,
Network can open it to your local network: other computers then sign in with a
password.

Everything is also available over HTTP: `/api` routes, and `/healthz` and
`/readyz` for health.

## Features

| | |
| --- | --- |
| **Requests** | Plex watchlists, yours and your Plex friends' through your own Plex connection, or other accounts with their own token; the web interface and an HTTP API. |
| **Sources** | RSS, JSON and Torznab indexers with optional authentication, opt-in feed watching for new releases, and opt-in IRC announcement channels, invite-only ones included. |
| **Release selection** | Movie and episode profiles on resolution, source, codec, language (each title's original language included), terms and scores, with previews that explain every decision. |
| **BitTorrent** | v1, v2 and hybrid torrents, magnets, trackers, DHT and PEX, parallel peers, pause and resume, priorities, bandwidth limits and seeding policies. |
| **Series** | TMDB episode plans, automatic requests for newly aired episodes, an episode calendar, explicit numbering, season packs and multi-episode files. |
| **Library** | Imports that never overwrite, into the folders Plex already uses, with subtitles and a length check, Plex confirmation, and monitored quality upgrades. |
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
  when the profile routes it, IPv6, with NAT-PMP port forwarding.
- **The network and cryptographic code is original** and has not had an
  independent review.

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
also need the TMDB catalog (enabled in Settings, with its token).
</details>

<details>
<summary><strong>An IRC announce channel is invite-only</strong></summary>

Some private trackers invite you to their announce channel when you ask their
bot. Give the IRC source an invite command through `POST /api/settings`, such
as `"invite": {"nick": "Bot", "message": "invite {secret}", "secret_env":
"MYNOU_IRC_INVITE_KEY"}`, and save the key with `POST /api/secrets`. Once
registered and identified (SASL or NickServ), Mynou sends the message to the
bot with the key in place of `{secret}`, waits for its invitation up to
`timeout_secs` (10 seconds by default, at most 60), then joins. If the channel
still refuses the join, Mynou asks again three times, waiting longer each time,
then `GET /api/irc` shows the source's `last_error` (`invite_not_received`)
and the reason, and it tries again after the source's longest reconnect delay.
Without an invite command, the error is `invite_command_required`. The key is
never shown, logged or recorded.
</details>

<details>
<summary><strong>Which releases are cross-seeded?</strong></summary>

With cross-seed discovery on for a source, Mynou looks there for other
releases of what it completed and seeds them from the same files, read-only.
A candidate must be a v1 torrent whose videos match local files byte for byte:
the whole release, or a single episode of a season pack you have (Mynou also
searches the pack's season for them). Other files it lacks, such as an `.nfo`
or a sample, are downloaded into its own folder up to **Settings → Downloads →
Cross-seed extras** (50 MB by default); video is never downloaded, and every
piece is verified before it is seeded. While the tracker of a new transfer
answers with an error, as a private tracker does until it lists a new upload,
Mynou asks it again every 7 seconds for 10 minutes, then every 30 seconds.
</details>

<details>
<summary><strong>Plex does not show an imported title</strong></summary>

Plex and Mynou must see the library at the same paths. If Plex sees it under
another prefix, add a path mapping (`plex.path_mappings`) through
`POST /api/settings`.
</details>

<details>
<summary><strong>Where does an import go, and why was one refused?</strong></summary>

A title Plex already lists goes into its existing folder, and an episode into
the season folder that already holds its season (`Season 2`, `Specials`). A new
title gets `Title (Year)` and `Season 02`, with ` - ` in place of a colon.
When Plex's folder is outside your library folders, goes through a symbolic
link, or Plex lists the title in several folders, Mynou uses its own layout.
Subtitles of the release (`.srt`, `.ass`, `.ssa`, `.vtt`, `.idx`/`.sub`) are
named after the video with their language, such as `Film (2024).fr.srt` or
`Film (2024).en.forced.srt`; other files, like `.nfo`, are left out.

Before filing, Mynou compares the file's length with the runtime TMDB gives
and accepts 70% to 160% of it, so extended cuts pass. Outside that range the
import fails with `runtime_mismatch` and both lengths, nothing is written, and
the release is not picked again for that title. The job's **Files & quality**
tab, and `import_notes` in the API, say which folder was used and why, and
whether the length was checked.
</details>

<details>
<summary><strong>Why was a release refused for its language?</strong></summary>

A profile's languages are all required, and they decide before the quality.
**Original language** is each title's own audio, as TMDB gives it: VO, MULTi
and VOSTFR always hold it; a release without a language tag counts unless the
title is French; FRENCH, VFF, VFQ and TRUEFRENCH count only for a French title.
A French dub alone of an English film is refused, and a profile with VFF and
Original language needs MULTi with VFF for an English film. Mynou keeps the
language with each title once TMDB gave it; while TMDB is off or does not
answer, only VO, MULTi and VOSTFR count. **Choose a release** and the Quality
page's **Try it** say for each release whether it holds the original audio.
</details>

<details>
<summary><strong>A friend's watchlist is not read</strong></summary>

Open **Settings → Plex & TMDB → Friends**. Friends are off until you turn
them on, and the reason Plex gave shows under a friend whose watchlist cannot
be read, for example a private one. **Check now** reads your friend list
again; a friend who left it stops being read, and what was already requested
stays.
</details>

<details>
<summary><strong>How do I upgrade or back up?</strong></summary>

Mynou updates itself, like Plex. It checks for a new release once a day and
installs it once no request is in progress, after backing up its storage. If
the new version cannot start, Mynou goes back to the previous version and
data. `mynou update` installs one now. For your own backups, stop the service
and copy the data directory (its `state/` and `downloads/` folders, which also
hold your settings) and your library.
</details>

Still stuck? [Open an issue](https://github.com/alecerf/mynou/issues/new/choose),
without tokens, private paths or media names.

What changed in each version is in the
[release notes](https://github.com/alecerf/mynou/releases).

## Build from source

With Rust 1.99.1 and no network access needed:

```sh
cargo build --release --offline --locked
```

The executable is `target/release/mynou`.

## Contributing and security

Bug reports and feature ideas are welcome through the
[issue templates](https://github.com/alecerf/mynou/issues/new/choose). Before
changing code, read [CONTRIBUTING.md](CONTRIBUTING.md): product code stays on
the Rust standard library, with no dependency and no external helper.
Everyone follows the [code of conduct](CODE_OF_CONDUCT.md).

Found a vulnerability? Do not open a public issue; follow
[SECURITY.md](SECURITY.md).

## License

No formal license: anyone may use Mynou for anything, without warranty. See
[LICENSE.md](LICENSE.md).
