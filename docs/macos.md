# Install and run Mynou on macOS

Mynou ships as one executable for Apple Silicon Macs (`macos-arm64`). It needs no
Rust or other download or media program. CI builds it and starts it on
macOS 26; earlier macOS versions have not been
verified. There is no Intel, Linux or Docker build.

## Download and verify

Open [GitHub Releases](https://github.com/alecerf/mynou/releases) and download
the executable and `SHA256SUMS` from the same release (0.22.19 or later). With
both files in Downloads (replace the version with the one you downloaded):

```sh
cd "$HOME/Downloads" &&
  awk '$2 == "mynou-v0.22.19-macos-arm64"' SHA256SUMS | shasum -a 256 -c - &&
  mkdir -p "$HOME/.local/bin" &&
  install -m 755 mynou-v0.22.19-macos-arm64 "$HOME/.local/bin/mynou"
```

Each step runs only if the previous one succeeded, so a checksum failure stops
before installation: download the files again before retrying. The
[verification guide](validation.md#verify-release-assets) explains what the
checksums prove. Add `$HOME/.local/bin` to your `PATH` to run `mynou` directly.

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
account. Open <http://127.0.0.1:8787/>; there is no sign-in. Then set Mynou up in the **Settings** view: library folders, downloads, Plex, TMDB, sources and their
tokens. Plex and TMDB turn on once their token is saved. Torrents wait until
**Settings → VPN** has a choice: a WireGuard profile, or your own connection.
Use ordinary folders you own rather than
paths through symbolic links: imports never overwrite files and refuse
symbolic links.

`mynou serve --dir DIR` uses another data directory; `MYNOU_HOME` sets the
default. Run Mynou as the user that owns its data directory and
library. For unattended operation, the [native launchd service](macos-service.md)
starts at account login and restarts after crashes.

## Configure Mynou

Mynou keeps its settings and secrets in its own storage, in `state/jobs` of the
data directory. Change them from the
**Settings** view of the web interface or from the API (`GET` and `POST
/api/settings`, `GET` and `POST /api/secrets`). Settings the view does not
show, such as `selection`, `irc` and `requesters`, are saved through the API:

```sh
curl -s http://127.0.0.1:8787/api/settings            # every setting, as JSON
curl -s -X POST -H 'Content-Type: application/json' \
  --data @settings.json http://127.0.0.1:8787/api/settings
```

`settings.json` holds `{"settings": {...}}`, the whole document.

Settings are validated before they are saved: fields this version does not know
are ignored, so check the spelling of a setting you add by hand, and an invalid
document leaves the saved one unchanged. The running service applies saved settings by restarting its
engine; a new `listen` address applies at the next start. The movies, series
and download folders are full paths on this Mac (`~/` is the home folder);
the Settings view shows them so and refuses a relative one. Through the API, a
relative path still resolves against the data directory, and is saved as the
full path.

| Setting | Default | Purpose |
| --- | --- | --- |
| `library.movies_root`, `library.series_root` | `library/movies`, `library/series` | Where imports go |
| `downloads` | enabled, `downloads`, `state/torrents`, port 6881, seeding, DHT and PEX on | BitTorrent client; see [transfers](transfers.md) |
| `listen` | `127.0.0.1:8787` | API and web interface listener; another address needs a password or `access.local_network_open` ([network access](#network-access)) |
| `access.allowed_hosts` | none | More host names other computers may use ([network access](#network-access)) |
| `access.local_network_open` | `false` | Computers on the local network need no sign-in ([network access](#network-access)) |
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
settings). Save it in the **Settings** view or with `POST /api/secrets`; a
saved secret is kept in the store, is never returned by the API, and takes
precedence over an environment variable of the same name, which remains a
fallback:

| Secret | Used for |
| --- | --- |
| `MYNOU_PLEX_TOKEN` | Plex server and its watchlist |
| `MYNOU_TMDB_TOKEN` | TMDB catalog (read access token) |
| `MYNOU_INDEXER_API_KEY` | Default API key of sources |

Enable the catalog with `catalog.enabled`, and describe each source as shown in
[sources](sources.md). Mynou talks to these services itself; the installation
runs no other media manager. Then check a search from **New request**.

## Connect Plex

In **Settings, Plex & TMDB**, click **Connect to Plex** and sign in on the Plex
page that opens (allow the pop-up if the browser asks). Mynou saves the account
token as the `MYNOU_PLEX_TOKEN` secret, without showing it, and lists the
servers the account owns; with several, choose one. Mynou then:

- finds an address of that server that answers from this Mac: Plex on this Mac
  (`http://127.0.0.1:32400`) first, then the server's addresses on the local
  network, then the others Plex publishes, never a relay;
- picks its film and series libraries: the one whose folder holds Mynou's
  movies or series folder (through the path mappings below), else the one
  already chosen, else the first. **Films go to** and **Series go to** change
  them from a list of the server's libraries;
- saves `plex.url`, `plex.movies_section`, `plex.series_section` and
  `plex.enabled`, and applies them.

**Change server** chooses again, and **Disconnect** removes the token and turns
Plex off. The same settings can still be written by hand, with the token saved
as the secret. `plex.watchlist_url` only needs changing for a different
watchlist service.

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

Once the settings are applied, Mynou reads the watchlists on its own; the jobs
appear in the web interface.

## Everyday use

The API listens on `127.0.0.1:8787`. `/healthz` reports liveness. While
Mynou validates its storage after a start, `/readyz` answers 503 `starting`
(`failed` if validation found an inconsistency), only reads are served and
`startup` in `/api/status` shows the stage and rows checked; requests, workers
and transfers begin once it is `ready`. On loopback the API has no sign-in;
requests must name it (`127.0.0.1`, `localhost` or `[::1]`) and come from no
other site's page.

## Network access

By default only this Mac can open Mynou. To use it from other computers on
your local network:

1. On the Mac running Mynou (in person, with Screen Sharing, or through
   `ssh -L 8787:127.0.0.1:8787`), open **Settings, Network** and save a
   password of at least 10 characters, or turn on **No password on the local
   network**, or both.
2. Turn on **Allow other computers**, choose the port (80 lets addresses leave
   it out) and save. `listen` becomes `0.0.0.0:PORT`; restart Mynou to apply it
   (`mynou service --action stop`, then `--action start`).
3. From another computer, open `http://NAME.local` (or `http://NAME.local:PORT`),
   the address the Network page shows, and sign in unless the local network
   needs no password.

Mynou refuses a `listen` address other than loopback, at startup and when
settings are saved, until a password is set or the local network is open. `0.0.0.0` (or `[::]`) listens on
every address. macOS lets an ordinary account use a port below 1024, such as 80,
only on these addresses, not on one specific address such as `192.168.1.20:80`.
With the macOS firewall on, the first start on the network asks whether
`mynou` may accept incoming connections; allow it, or other computers cannot
connect.

How requests are checked:

- Every request except `/healthz`, `/readyz` and the interface's files must
  name an allowed host in `Host`: `127.0.0.1`, `localhost`, `[::1]`, an address
  of this Mac, this Mac's `NAME.local` (from its Sharing settings), or a name
  in `access.allowed_hosts`. A domain rebound to this Mac is refused (DNS
  rebinding). An `Origin`, which browsers send with cross-site writes and
  sockets, must be the same as `Host`, for HTTP requests and `/ws`.
- Connections from this Mac, from loopback or from the Mac's own address, need
  no sign-in, so `mynou update`, `mynou service` and local scripts work as
  before. A reverse proxy on the same Mac would therefore pass every request as
  local: do not put one in front of Mynou.
- With **No password on the local network** (`access.local_network_open`),
  computers at a private or link-local address (`10.0.0.0/8`, `172.16.0.0/12`,
  `192.168.0.0/16`, `169.254.0.0/16`, `fc00::/7`, `fe80::/10`) need no sign-in
  either. Anyone on that network can then use Mynou: turn it on only on a
  network you trust. A router forwarding a port keeps the public address of
  whoever connects, so they still need the password.
- Other computers sign in with the password (`POST /api/session` with
  `{"password": "..."}`) and get a session cookie (`HttpOnly`,
  `SameSite=Strict`) valid for 30 days. Without it, `/api` routes and `/ws`
  answer 401. Scripts on another computer sign in the same way and send the
  cookie back. `DELETE /api/session` signs out.
- After 5 failed sign-ins from one address, or 30 in all, within 15 minutes,
  sign-in waits until the window ends. Failures are logged without the password:
  the service journal (`.mynou-service/events.log`) records `sign_in_refused`
  or `sign_in_throttled`, and `mynou serve` also prints the address.
- Mynou keeps a PBKDF2-HMAC-SHA256 hash of the password (600,000 iterations,
  random salt) and the random key that signs sessions; the API never returns
  them. The password is set and removed on this Mac only (`POST /api/access`),
  a new one signs every other computer out, and it can be removed only once
  network access is off or the local network needs no password.

Mynou serves plain HTTP. On your own network that is usually acceptable, but
anyone who can watch the traffic, for example on a shared or public Wi-Fi, can
read the password and the session cookie. Do not open Mynou on a network you do
not trust, and do not forward its port from your router to the internet.

Port 6881/TCP accepts BitTorrent peers; whether peers can reach it depends on
your firewall and router. DHT and UDP trackers use outgoing connections only.

Requests, retries and cancellations happen in the web interface. Cancelling a
request never deletes files or cancels other requests sharing the torrent.

## TLS trust and proxies

The TLS client reads PEM certificate bundles such as `/etc/ssl/cert.pem`, not the
macOS Keychain. To trust a private certificate authority, set `MYNOU_CA_FILE` to
an approved PEM bundle. Certificate and host name checks cannot be disabled. If
your network needs a proxy, set `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` for the
service; `mynou update` and `mynou service` bypass it to reach the local API.

## Back up and upgrade

For unattended account-login startup, crash recovery and explicit graceful
stop/start/uninstall commands, see the [native launchd service](macos-service.md).

Keep the data directory (its `state/` and `downloads/` folders, which hold
every setting and saved secret) and the library folders. For a consistent backup, stop the service before copying `state/`. The
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
available in **Settings → VPN** and through the API. See the
[operator reference](wireguard-operator.md) for supported settings, startup and
restart behavior, file permissions and limitations. The
[WireGuard installation and migration guide](wireguard-macos-migration.md)
covers the first import, backups, verification, recovery and
rollback without changing the existing Gluetun deployment. Read the
[evidence and supported-subset limits](wireguard-acceptance.md) before migrating.
