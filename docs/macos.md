# Install and run Mynou on macOS

Mynou ships as one executable for Apple Silicon Macs (`macos-arm64`). It needs no
Rust or other download or media program. CI builds it and runs the local
acquisition and import demo on macOS 26; earlier macOS versions have not been
verified. There is no Intel, Linux or Docker build.

## Download and verify

Open [GitHub Releases](https://github.com/alecerf/mynou/releases) and download
the executable and `SHA256SUMS` from the same release (0.22.19 or later). With
both files in Downloads (replace the version with the one you downloaded):

```sh
cd "$HOME/Downloads" &&
  awk '$2 == "mynou-v0.22.19-macos-arm64"' SHA256SUMS | shasum -a 256 -c - &&
  mkdir -p "$HOME/.local/bin" &&
  install -m 755 mynou-v0.22.19-macos-arm64 "$HOME/.local/bin/mynou" &&
  "$HOME/.local/bin/mynou" demo --dir "$HOME/mynou-demo"
```

Each step runs only if the previous one succeeded, so a checksum failure stops
before installation: download the files again before retrying. The
[verification guide](validation.md#verify-release-assets) explains what the
checksums prove. The demo needs a directory that does not exist yet, uses
synthetic media, loopback peers and simulated Plex and indexer services, and
needs no secrets. Add `$HOME/.local/bin` to your `PATH` to run `mynou` directly.

The executable is not signed with an Apple Developer identity or notarized.
If macOS blocks a verified download, use **Open Anyway** in **System Settings →
Privacy & Security** for that application, then confirm. Keep Gatekeeper
enabled globally.

## Configure and start

Start the service:

```sh
"$HOME/.local/bin/mynou" serve
```

The first start creates `~/Library/Application Support/Mynou`, private to your
account. Open <http://127.0.0.1:8787/> and create your access token: Mynou
shows it once and keeps only its SHA-256 digest, so keep it safe. The first
visit can create it only from this Mac, while Mynou listens on a loopback
address; otherwise, and whenever the token is lost, stop Mynou and run
`mynou token --reset`, which prints a new token once. Commands that reach the
running service read the token from `MYNOU_API_TOKEN`, which you set the way
you prefer. Then set Mynou up in the **Settings** view: library folders, downloads, Plex, TMDB, sources and their
tokens. Plex and TMDB turn on once their token is saved. Use ordinary folders you own rather than
paths through symbolic links: imports never overwrite files and refuse
symbolic links.

Every command takes `--dir DIR` to use another data directory; `MYNOU_HOME`
sets the default. Run Mynou as the user that owns its data directory and
library. For unattended operation, the [native launchd service](macos-service.md)
starts at account login and restarts after crashes.

## Configure Mynou

Mynou keeps its settings and secrets in its own storage, in `state/jobs` of the
data directory. Change them from the
**Settings** view of the web interface, from the API (`GET` and `POST
/api/settings`, `GET` and `POST /api/secrets`) or from the command line:

```sh
mynou settings                            # every setting, as JSON
mynou settings --save < settings.json     # validate and save a whole document
printf '%s' "$TOKEN" | mynou secrets --set MYNOU_PLEX_TOKEN
mynou secrets                             # which secrets are set, never their values
```

Settings are validated before they are saved: unknown fields are rejected, so a
typo is reported instead of ignored, and an invalid document leaves the saved
one unchanged. The running service applies saved settings by restarting its
engine; a new `listen` address applies at the next start. Relative paths
resolve against the data directory.

| Setting | Default | Purpose |
| --- | --- | --- |
| `library.movies_root`, `library.series_root` | `library/movies`, `library/series` | Where imports go |
| `downloads` | enabled, `downloads`, `state/torrents`, port 6881, seeding, DHT and PEX on | BitTorrent client; see [transfers](transfers.md) |
| `listen` | `127.0.0.1:8787` | API and web interface listener |
| `workers` | `2` (1 to 32) | Requests processed at the same time |
| `max_attempts` | `10` (up to 1,000) | Automatic retries of a failed request, with growing delays |
| `minimum_seeders` | `1` | Candidates with fewer seeders are rejected |
| `poll_interval_ms`, `lease_duration_secs` | `500`, `60` | Worker polling interval and lease length |
| `plex` | disabled | [Connect Plex](#connect-plex) |
| `catalog` | disabled | TMDB catalog, required for series |
| `indexers` | none | [Sources](sources.md) to search, and [feeds to watch](sources.md#watching-a-feed) |
| `selection` | unrestricted | [Release selection](selection.md) |
| `monitoring` | disabled | [Library upgrades](library.md#background-monitoring) |
| `series_packs` | disabled | [Automatic packs](packs.md#prefer-packs-during-monitoring) |
| `updates.automatic` | `true` | [Install new versions automatically](#updates) |
| `irc`, `requesters` | absent | [IRC announcements](sources.md#irc-announcements), [requester accounts](requesters.md) |

Each integration names the secret that holds its credential (the `*_env`
settings). Save it in the **Settings** view or with `mynou secrets --set`; a
saved secret is kept in the store, is never returned by the API, and takes
precedence over an environment variable of the same name, which remains a
fallback:

| Secret | Used for |
| --- | --- |
| `MYNOU_PLEX_TOKEN` | Plex server and its watchlist |
| `MYNOU_TMDB_TOKEN` or `MYNOU_TMDB_API_KEY` | TMDB catalog |
| `MYNOU_INDEXER_API_KEY` | Default API key of sources |

Enable the catalog with `catalog.enabled`, and describe each source as shown in
[sources](sources.md). Mynou talks to these services itself; the installation
runs no other media manager. Then check a search:

```sh
mynou search --title "Example Movie" --year 2026
```

## Connect Plex

Set `plex.enabled`, `plex.url` and the movie and series section IDs
(`plex.movies_section`, `plex.series_section`), and save the token as the
`MYNOU_PLEX_TOKEN` secret. The default URL `http://127.0.0.1:32400` reaches Plex on the
same Mac; use the server's address for Plex elsewhere. `plex.watchlist_url` only
needs changing for a different watchlist service.

Plex and Mynou should see the library at the same paths. If Plex sees it under a
different prefix, for example a network share mounted elsewhere, add a mapping
inside the `plex` object:

```json
"path_mappings": [
  {"mynou_prefix": "/Volumes/Media", "plex_prefix": "/mnt/media"}
]
```

After an import, Mynou asks Plex to scan and waits until Plex lists the title.
Upgrades, shared videos and requester imports need more: Plex must report the
exact imported path in `Part.file`. Mappings translate those paths for
comparison only: they do not move files or change mounts. They match whole
path components (`/Volumes/Media` does not match `/Volumes/Media-extra`), the
longest prefix wins, and without a mapping the paths must be identical.

Once the settings are applied, synchronize:

```sh
mynou sync
mynou jobs
```

## Everyday use

The API listens on `127.0.0.1:8787`. `/healthz` reports liveness. While
Mynou validates its storage after a start, `/readyz` answers 503 `starting`
(`failed` if validation found an inconsistency), only reads are served and
`startup` in `/api/status` shows the stage and rows checked; requests, workers
and transfers begin once it is `ready`. `mynou healthcheck` exits with an error unless the running service answers `/readyz`
(it bypasses any proxy). `/api` routes need
`Authorization: Bearer` with the access token. For remote access, put a TLS reverse
proxy in front and keep the plain listener private.

Port 6881/TCP accepts BitTorrent peers; whether peers can reach it depends on
your firewall and router. DHT and UDP trackers use outgoing connections only.

CLI commands use the running service's API for the same data directory, or
the storage directly when it is stopped:

```sh
mynou submit --title "Movie" --year 2026 --url 'magnet:?xt=urn:btih:...'
mynou status
mynou events ID
mynou retry ID
mynou cancel ID
```

For a local file, pass its path with `--path`. Cancelling a request never
deletes files or cancels other requests sharing the torrent.

## TLS trust and proxies

The TLS client reads PEM certificate bundles such as `/etc/ssl/cert.pem`, not the
macOS Keychain. To trust a private certificate authority, set `MYNOU_CA_FILE` to
an approved PEM bundle. Certificate and host name checks cannot be disabled. If
your network needs a proxy, set `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` for the
service; local management commands bypass it.

## Back up and upgrade

For unattended account-login startup, crash recovery and explicit graceful
stop/start/uninstall commands, see the [native launchd service](macos-service.md).

Keep the data directory (its `state/` and `downloads/` folders, which hold
every setting, saved secret and the access token digest) and the library folders. For a consistent backup, stop the service before copying `state/`. The
journal confirms each transaction on disk, and an interrupted final write is
recovered at the next start. `POST /api/shutdown` lets running work finish;
stopping the process instead relies on that recovery.

Data from the former Go release (SQLite) is not imported: keep it in a separate
folder.

### Updates

Like Plex Media Server, Mynou updates itself. Once a day, starting a few
minutes after it starts, it checks the latest
[GitHub release](https://github.com/alecerf/mynou/releases). It downloads a
newer executable, checks it against the release's `SHA256SUMS`, makes sure it
runs and reports its version, and stages it next to the installed executable.
The dashboard then shows the new version.

Mynou installs it once no request is in progress, or at once with **Install
now**:

1. It stops cleanly, as `POST /api/shutdown` does.
2. It copies `state/` and the current executable to
   `updates/OLD-to-NEW/` in the data directory.
3. It replaces the executable atomically and starts it again in the same
   process, so the launchd service keeps running.

If the new version cannot start, Mynou restores that copy of `state/` and the
previous executable, then starts again on them. The state it could not open is
kept in `updates/OLD-to-NEW/state.failed`. Mynou keeps the two most recent
backups and never touches downloads or the library.

`mynou update` checks and installs now: through the running service, or
directly while Mynou is stopped. Turn off `updates.automatic` in the
**Settings** view to update only on demand. Only release executables update
themselves. Mynou must be able to write in the folder that holds its
executable (`~/.local/bin` above). Published releases never change; each update
is a new version.

To update by hand instead, back up, stop the service, download and verify the
next executable as above, and install it at the same path. To go back to an
earlier version, stop Mynou and reinstall the previous executable with
compatible data, such as a backup from `updates/`: some versions change storage
formats that older versions refuse (see
[limits](limits.md#persistence-and-platform)).

## Native WireGuard controls

Private profile import, redacted status and explicit lifecycle controls are
available through `mynou vpn` and the authenticated API. See the
[operator reference](wireguard-operator.md) for supported settings, startup and
restart behavior, file permissions and limitations. The
[WireGuard installation and migration guide](wireguard-macos-migration.md)
covers private import before startup, backups, verification, recovery and
rollback without changing the existing Gluetun deployment. Read the
[evidence and supported-subset limits](wireguard-acceptance.md) before migrating.
