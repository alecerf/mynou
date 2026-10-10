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

Create a directory you own, outside Downloads:

```sh
mkdir -p "$HOME/Library/Application Support/Mynou" &&
  cd "$HOME/Library/Application Support/Mynou" &&
  "$HOME/.local/bin/mynou" init --config ./mynou.json &&
  "$HOME/.local/bin/mynou" doctor --config ./mynou.json
```

`init` writes `mynou.json` and a private `.env` with a random API token; Plex
and TMDB start disabled. Set your library folders, sources, Plex and catalog as
described in [Configure Mynou](#configure-mynou). Use ordinary folders you own
rather than paths through symbolic links: imports never overwrite files and
refuse symbolic links.

Mynou reads only `MYNOU_API_TOKEN` from the `.env` file beside the
configuration. Export the other configured secrets (Plex, TMDB, sources) in the
shell that starts the service, using your shell's quoting rules. Treat `.env`
as data: sourcing it interprets its contents as shell code. For example, replace
the placeholders below with shell-quoted values:

```sh
export MYNOU_PLEX_TOKEN='replace-with-your-Plex-token'
export MYNOU_TMDB_TOKEN='replace-with-your-TMDB-token'
"$HOME/.local/bin/mynou" serve --config ./mynou.json
```

The API listens on `http://127.0.0.1:8787` and takes the API token as a Bearer
token. Run the service as the user that owns its configuration, data and library. Mynou does
not install a launch agent or background service; supervising it is up to you.

## Configure Mynou

Settings live in `mynou.json`; secrets live in the environment of the process
that runs `mynou serve`. Relative paths resolve against the configuration file's
directory. Unknown fields are rejected, and `mynou doctor` reports whether the
file is valid. Restart the service after any change.

| Setting | Default | Purpose |
| --- | --- | --- |
| `library.movies_root`, `library.series_root` | `library/movies`, `library/series` | Where imports go |
| `downloads` | enabled, `downloads`, `state/torrents`, port 6881, seeding, DHT and PEX on | BitTorrent client; see [transfers](transfers.md) |
| `store_dir` | `state/jobs` | Request journal and snapshots |
| `listen` | `127.0.0.1:8787` | API listener |
| `api_token_env` | `MYNOU_API_TOKEN` | Variable holding the API token |
| `workers` | `2` (1 to 32) | Requests processed at the same time |
| `max_attempts` | `10` (up to 1,000) | Automatic retries of a failed request, with growing delays |
| `minimum_seeders` | `1` | Candidates with fewer seeders are rejected |
| `poll_interval_ms`, `lease_duration_secs` | `500`, `60` | Worker polling interval and lease length |
| `plex` | disabled | [Connect Plex](#connect-plex) |
| `catalog` | disabled | TMDB catalog, required for series |
| `indexers` | none | [Sources](sources.md) to search |
| `selection` | unrestricted | [Release selection](selection.md) |
| `monitoring` | disabled | [Library upgrades](library.md#background-monitoring) |
| `series_packs` | disabled | [Automatic packs](packs.md#prefer-packs-during-monitoring) |
| `irc`, `requesters` | absent | [IRC announcements](sources.md#irc-announcements), [requester accounts](requesters.md) |

Each integration names the environment variable that holds its secret:

| Variable | Used for |
| --- | --- |
| `MYNOU_API_TOKEN` | API Bearer token (generated in `.env`) |
| `MYNOU_PLEX_TOKEN` | Plex server and its watchlist |
| `MYNOU_TMDB_TOKEN` or `MYNOU_TMDB_API_KEY` | TMDB catalog |
| `MYNOU_INDEXER_API_KEY` | Default API key variable for sources |

Enable the catalog with `catalog.enabled`, and describe each source as shown in
[sources](sources.md). Mynou talks to these services itself; the installation
runs no other media manager. Then check a search:

```sh
mynou search --title "Example Movie" --year 2026 --config ./mynou.json
```

## Connect Plex

Set `plex.enabled`, `plex.url` and the movie and series section IDs
(`plex.movies_section`, `plex.series_section`), and export the token as
`MYNOU_PLEX_TOKEN`. The default URL `http://127.0.0.1:32400` reaches Plex on the
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

After changing the configuration, restart the service and synchronize:

```sh
mynou sync --config ./mynou.json
mynou jobs --config ./mynou.json
```

## Everyday use

The API listens on `127.0.0.1:8787`. `/healthz` and `/readyz` report
health, and `mynou healthcheck --config ./mynou.json` exits with an error unless
the running service answers `/readyz` (it bypasses any proxy). `/api` routes need
`Authorization: Bearer` with the API token. For remote access, put a TLS reverse
proxy in front and keep the plain listener private.

Port 6881/TCP accepts BitTorrent peers; whether peers can reach it depends on
your firewall and router. DHT and UDP trackers use outgoing connections only.

Run any CLI command with the same configuration file:

```sh
mynou submit --title "Movie" --year 2026 --url 'magnet:?xt=urn:btih:...' \
  --config ./mynou.json
mynou status --config ./mynou.json
mynou events ID --config ./mynou.json
mynou retry ID --config ./mynou.json
mynou cancel ID --config ./mynou.json
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

Keep `mynou.json`, `.env`, the `state/` and `downloads/` folders and the library
folders. For a consistent backup, stop the service before copying `state/`. The
journal confirms each transaction on disk, and an interrupted final write is
recovered at the next start. `POST /api/shutdown` lets running work finish;
stopping the process instead relies on that recovery.

To upgrade, read the new release notes, back up, stop the service, download the
next release's executable, verify it as above and install it at the same path.
Keep the configuration, `.env`, journal, downloads and library folders. Published
releases never change; each update is a new version. To roll back, reinstall the
previous executable with compatible data: some versions change storage formats
that older versions refuse (see [limits](limits.md#persistence-and-platform)).
Never delete older executables or user data automatically. Data from the former
Go release (SQLite) is not imported: keep it in a separate folder.
