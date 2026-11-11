<h1 align="center">Mynou</h1>

<p align="center">
  <strong>Your media library on autopilot, in one self-contained program.</strong>
</p>

<p align="center">
  Add a title to your Plex watchlist. Mynou finds a release, downloads it,<br>
  files it in your library and checks that Plex shows it.
</p>

<p align="center">
  <a href="https://github.com/alecerf/mynou/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/alecerf/mynou?label=release&color=blue"></a>
  <img alt="Platform: macOS on Apple Silicon" src="https://img.shields.io/badge/platform-macOS%20on%20Apple%20Silicon-lightgrey">
  <img alt="Zero dependencies" src="https://img.shields.io/badge/dependencies-0-brightgreen">
  <img alt="Unsafe code only for the SHA instructions" src="https://img.shields.io/badge/unsafe-SHA%20instructions%20only-brightgreen">
  <a href="LICENSE.md"><img alt="License: do anything" src="https://img.shields.io/badge/license-do%20anything-blue"></a>
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#what-it-does">What it does</a> ·
  <a href="#set-up-your-library">Set up</a> ·
  <a href="#features">Features</a> ·
  <a href="#good-to-know">Good to know</a> ·
  <a href="#questions-and-troubleshooting">Help</a>
</p>

## Quick start

On a Mac with Apple Silicon, paste this into Terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/alecerf/mynou/trunk/install.sh | sh
```

No account, token or Rust needed. The [script](install.sh) downloads the
latest release, checks that the release key signed its `SHA256SUMS` and that
the executable matches it, and only then installs `mynou` into `~/.local/bin`,
without sudo. It ends with the line to add to `~/.zshrc` if that folder is not
on your `PATH`, and the next steps: start Mynou and
[set up your library](#set-up-your-library).

Run it again to reinstall. For a given version, end the line with
`| sh -s -- 0.29.0`; for another folder, set `MYNOU_INSTALL_DIR`.

<details>
<summary><strong>Verify and install by hand</strong></summary>

Download `mynou-v<version>-macos-arm64`, `SHA256SUMS` and `SHA256SUMS.sig`
from the [latest release](https://github.com/alecerf/mynou/releases/latest),
and [`release-key.pub.pem`](release-key.pub.pem) from this repository, into
one folder. Then, in that folder:

```sh
VERSION=0.29.0
openssl dgst -sha256 -verify release-key.pub.pem -signature SHA256SUMS.sig SHA256SUMS &&
  shasum -a 256 -c SHA256SUMS &&
  mkdir -p "$HOME/.local/bin" &&
  install -m 755 "mynou-v$VERSION-macos-arm64" "$HOME/.local/bin/mynou"
```

Each step runs only if the previous one succeeded: a bad signature or checksum
stops before anything is installed. Mynou's own updates make both checks.
</details>

## What it does

| Step | What Mynou does |
| --- | --- |
| **Request** | Takes a title from a Plex watchlist, the web interface or the API. |
| **Search** | Asks the indexers you configured for matching releases. |
| **Select** | Picks the best release for your profile and records why the others lost. |
| **Download** | Fetches it with the built-in BitTorrent client and verifies every piece. |
| **Analyze** | Reads the media file's metadata to check what was downloaded. |
| **Import** | Checks its length against the title's, then files it with its subtitles in the folder Plex already uses for the title. |
| **Confirm** | Asks Plex to scan and waits until Plex lists the title. |

Then it keeps watching: new episodes of the series you follow are requested
when they air and, if you turn it on, better releases replace the ones you
have.

### Why Mynou

- **One program, not a stack.** The BitTorrent client, the release search, the
  media parsers, the web interface and the storage live in one executable: no
  qBittorrent, Radarr, Sonarr, ffprobe, SQLite or Docker to install and keep in
  sync.
- **Nothing hidden underneath.** Safe Rust on the standard library only: zero
  Cargo dependencies, no external program at runtime, and `unsafe` code only
  to call the processor's SHA instructions.
- **It explains itself.** A title's release list shows which release Mynou
  would pick and why every other one lost; you can also pick one yourself.
- **Careful with your files.** Imports never overwrite a video or a subtitle,
  and never write outside your library folders. A file far shorter or longer
  than the title is refused before anything is written. An upgrade deletes the
  file it replaces only once the new one is in place (and in Plex), and
  cancelling a request never deletes anything.

You bring the services: Plex, a TMDB account for series, and the indexers you
want Mynou to search.

## Set up your library

**1. Start Mynou.**

```sh
mynou serve
```

Its data goes to `~/Library/Application Support/Mynou`, created on first
start; use `--dir DIR` or `MYNOU_HOME` for another place.

**2. Open <http://127.0.0.1:8787/>.** There is no account or sign-in on your
Mac.

**3. Open Settings** and turn on what you use. Plex and TMDB start off.

| To set up | Settings page | You need |
| --- | --- | --- |
| Movies and series folders | Library | |
| Downloads | Downloads: folder, downloads at once, peers, speed limits (BitTorrent port 6881 by default) | |
| What a good release is | Quality: profiles with qualities, sources, languages and sizes | |
| Plex server and watchlist | Plex & TMDB: **Connect to Plex**, then choose the server | A Plex account |
| Your Plex friends' watchlists | Plex & TMDB: Friends, a switch for each friend | |
| TMDB catalog, to find titles and follow series | Plex & TMDB: TMDB | A TMDB read access token |
| Sources to search | Indexers, each with its own rules (priority, seeders, seeding goal) | The API key of each source |
| What goes through a VPN | VPN: WireGuard profiles, then Torrents, Indexers, TMDB | Your provider's WireGuard profiles |
| Use from other computers | Network: Allow other computers | A password, or none on your local network |

- **Without a VPN profile**, everything uses your own connection. Once you
  import one, torrents go through it; indexers and TMDB can too. Import
  several, such as several servers of your provider, and Mynou moves to the
  next one by itself when a server stops answering.
- **Saving** validates the settings and applies them at once; an invalid change
  is refused and leaves the saved settings untouched.
- **Tokens** are kept in Mynou's storage and never shown again.
- **Every setting**, including those the view does not show, such as IRC
  sources, is also available through `GET` and `POST /api/settings`, and
  secrets through `POST /api/secrets`.

> [!TIP]
> To start Mynou at login without a terminal, stop `mynou serve` (Ctrl-C) and
> install the native launchd service with `mynou service --action install`.
> Add `--dir` with the absolute path of your data directory if it is not the
> default one. `--action status`, `stop`, `start` and `uninstall` manage it.

## Features

| | |
| --- | --- |
| **Requests** | Plex watchlists, yours and your Plex friends' through your own Plex connection, or other accounts with their own token; the web interface and an HTTP API. |
| **Sources** | RSS, JSON and Torznab indexers with optional authentication, opt-in feed watching for new releases, and opt-in IRC announcement channels, invite-only ones included. |
| **Release selection** | Quality profiles for films and series on quality, source, language (each title's original language included), size, codec and preferred words, with French versions told apart (VFF, VFQ, VFI, VOSTFR) and previews that explain every decision. |
| **BitTorrent** | v1, v2 and hybrid torrents, magnets, trackers, DHT and PEX, TCP and uTP peers, pause and resume, priorities, bandwidth limits and seeding policies. |
| **Series** | TMDB episode plans, automatic requests for newly aired episodes, an episode calendar, explicit numbering, season packs and multi-episode files. |
| **Library** | Imports that never overwrite, into the folders Plex already uses, with subtitles and a length check, Plex confirmation, monitored quality upgrades, and a Storage view to free up space. |
| **Media analysis** | Metadata of MP4/MOV, Matroska/WebM, AVI, WAV/RF64, FLAC and MP3 files. |
| **Web interface** | Overview, Library, Transfers, Activity and Settings, updated live. Find a film or a series on TMDB and add it, as in Radarr and Sonarr. **Choose a release**, from a job or any episode of a followed series, lists what the indexers offer and why each release was kept or rejected. Plain HTML, CSS and JavaScript built into the executable, with no build step. |
| **API** | Everything over HTTP under `/api`, with `/healthz` and `/readyz` for health checks. |

## Good to know

Mynou is young and deliberately explicit about its edges:

- **macOS on Apple Silicon only.** There is no Intel, Linux, Windows or Docker
  build.
- **Not a drop-in replacement** for Radarr, Sonarr, Prowlarr or qBittorrent: it
  covers the same journey with fewer options.
- **Every feature has a stated bound**, and exceeding one produces a clear
  error instead of unbounded growth.
- **Local by default.** Mynou listens on this Mac only. It answers this Mac's
  pages and commands without sign-in, and never a page from another site.
- **Native WireGuard protects the supported torrent paths**, over IPv4 and,
  when the profile routes it, IPv6, with NAT-PMP port forwarding. Each profile
  has one server, reached at an IPv4 address; up to 16 profiles take over
  from one another.
- **The network and cryptographic code is original** and has not had an
  independent review.

## Questions and troubleshooting

<details>
<summary><strong>macOS refuses to open the executable</strong></summary>

The executable is not signed with an Apple Developer identity or notarized.
The install script downloads it with `curl`, which macOS does not quarantine;
a browser download is. Once you have verified it, open **System Settings →
Privacy & Security**, choose **Open Anyway** for Mynou and confirm. Keep
Gatekeeper enabled.
</details>

<details>
<summary><strong><code>command not found: mynou</code></strong></summary>

`~/.local/bin` is not on your `PATH`. Add this line to `~/.zshrc`, then open a
new terminal:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Or call `~/.local/bin/mynou` directly.
</details>

<details>
<summary><strong>A search finds nothing</strong></summary>

Open **Settings → Indexers** to see the state of each source. Series also need
the TMDB catalog (turned on in **Settings → Plex & TMDB**, with its token).
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
<summary><strong>The VPN server stopped answering</strong></summary>

Open **Settings → VPN**. With several profiles, when a server stops answering
for a few minutes, Mynou moves to the next profile in the list, wrapping
around, and asks the new server for a forwarded port; peers and trackers are
then told the new port. Meanwhile torrents, and whatever else you send through
the VPN, wait: nothing uses your own connection instead. A lone profile gets
about fifteen minutes before Mynou gives up on it. Once every profile has
failed in a row, Mynou tries the list again after five minutes, then waits
twice as long each time, up to an hour; **Try again** starts at once. **Recent
switches** says when and why Mynou changed profile, and the arrows set the
order in which profiles are tried.
</details>

<details>
<summary><strong>A friend's watchlist is not read</strong></summary>

Open **Settings → Plex & TMDB → Friends**. Friends are off until you turn them
on, and the reason Plex gave shows under a friend whose watchlist cannot be
read, for example a private one. **Check now** reads your friend list again; a
friend who left it stops being read, and what was already requested stays.
</details>

<details>
<summary><strong>Where are the logs?</strong></summary>

In the interface: **Overview → Tracking & statistics → Read the logs**, or
search for "Logs". The newest records come first; choose **Errors**,
**Warnings**, **Activity** or **Everything**, and **Download** saves every log
file as one text file. Over HTTP, `GET /api/logs?level=warn&limit=200` reads
them (levels `error`, `warn`, `info`, `debug`; at most 1,000 records) and
`GET /api/logs/download` returns the files.

On disk they are in the data directory, under `logs/`: `mynou.log`, then
`mynou.1.log` to `mynou.4.log`, older. Each file holds at most 1 MiB, so the
log never takes more than 5 MiB: the oldest records make room for new ones.
Each line reads `<UTC time> <LEVEL> <source>: <message>`. Tokens, passkeys, API
keys, passwords and cookies are replaced by `[redacted]` before anything is
written: every secret Mynou keeps, and what looks like one in URLs, headers and
IRC commands.

Under the launchd service, what the process prints itself, such as a crash
report, goes to `logs/output.log`; Mynou moves it into the log at start and
every minute, then empties it. A service installed by an earlier version is
switched over at its next start. The fixed-code journal in
`.mynou-service/events.log` stays as it was.
</details>

<details>
<summary><strong>How do I upgrade or back up?</strong></summary>

Mynou updates itself, like Plex, unless you turn off **Update Mynou
automatically** in **Settings → Library**. It checks for a new release once a
day and installs it once no request is in progress, after backing up its
storage. If the new version cannot start, Mynou goes back to the previous
version and data. `mynou update` installs one now, the same way. For your own
backups, stop the service and copy the data directory (its `state/` and
`downloads/` folders, which also hold your settings) and your library.
</details>

<details>
<summary><strong>How do I uninstall Mynou?</strong></summary>

If you installed the service, remove it with `mynou service --action uninstall`
(with the same `--dir`, if any). Then delete `~/.local/bin/mynou`. Your
settings and data stay in the data directory until you delete it, and your
library stays where it is.
</details>

Still stuck? [Open an issue](https://github.com/alecerf/mynou/issues/new/choose),
without tokens, private paths or media names. What changed in each version is
in the [release notes](https://github.com/alecerf/mynou/releases).

## Build from source

With Rust 1.99.1, which `rust-toolchain.toml` pins, and no network access:

```sh
cargo build --release --offline --locked
```

The executable is `target/release/mynou`.

## Contributing and security

Bug reports and feature ideas are welcome through the
[issue forms](https://github.com/alecerf/mynou/issues/new/choose). Before
changing code, read [CONTRIBUTING.md](CONTRIBUTING.md): product code stays on
the Rust standard library, with no dependency and no external helper.
Everyone follows the [code of conduct](CODE_OF_CONDUCT.md).

Found a vulnerability? Do not open a public issue; follow
[SECURITY.md](SECURITY.md).

## License

No formal license: anyone may use Mynou for anything, without warranty. See
[LICENSE.md](LICENSE.md).
