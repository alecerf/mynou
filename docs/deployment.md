# Install and run Mynou with Docker

The Docker image is Linux amd64 and contains only the static Mynou executable
and TLS certificate data: no shell, library or external program. The host needs
Docker and its Compose plugin, nothing else. Apple Silicon hosts need amd64
emulation; for a native install without Docker, follow the
[macOS guide](macos.md).

Images are published to the private `ghcr.io/alecerf/mynou` package from
release 0.22.19 on. Use a published release whose notes give a verified
`ghcr.io/alecerf/mynou@sha256:...` reference; a version in the sources or a
pull request check does not mean an image exists. Earlier releases keep their
own installation instructions.

## Authenticate and pull

The repository is public; the container package is private. Use an account
allowed to read the package and a GitHub personal access token (classic) with
`read:packages`. Log
in interactively so the token stays out of your shell history, and let Docker's
credential helper keep it:

```sh
docker login ghcr.io
```

Never put this token in Mynou's configuration, an Issue or release notes. Copy
the digest from the release notes: unlike a tag such as
`ghcr.io/alecerf/mynou:0.22.19`, a digest always names the same image.

```sh
MYNOU_IMAGE='ghcr.io/alecerf/mynou@sha256:REPLACE_WITH_RELEASE_DIGEST'
docker pull "$MYNOU_IMAGE"
```

## Create the installation

Run `setup-docker` from the image in a directory you own:

```sh
docker run --rm --network none --read-only --cap-drop ALL \
  --security-opt no-new-privileges:true \
  --user "$(id -u):$(id -g)" \
  --mount "type=bind,src=$PWD,dst=/work" --workdir /work \
  "$MYNOU_IMAGE" setup-docker --dir mynou-docker &&
  printf '\nMYNOU_UID=%s\nMYNOU_GID=%s\nMYNOU_IMAGE=%s\n' \
    "$(id -u)" "$(id -g)" "$MYNOU_IMAGE" >> mynou-docker/.env
```

`setup-docker` refuses an existing directory. It creates `compose.yaml`,
`mynou.json`, a private `.env` holding a random API token, `data/`,
`library/movies/` and `library/series/`. `data/` holds the request journal,
torrent state and downloads; the library is mounted separately. The Compose
file uses the image's own version unless `MYNOU_IMAGE` in `.env` overrides it,
runs the container read-only, without capabilities, as your user. Run Mynou as
an ordinary account that owns the configuration, data and library.

```sh
cd mynou-docker &&
  docker compose config --quiet &&
  docker compose pull &&
  docker compose up -d &&
  docker compose exec mynou /mynou doctor --config /config/mynou.json
```

Then open **http://127.0.0.1:8787/ui**, sign in with `MYNOU_API_TOKEN` from
`.env` and open **Setup** for a checklist of what is still missing (see the
[browser guide](web.md#guided-setup)).

To build the image yourself instead, clone the repository and use its own
`compose.yaml`, or build a local tag and set `MYNOU_IMAGE` to it. GitHub's
source downloads contain sources only.

## Configure Mynou

Settings live in `mynou.json`; secrets live in `.env`, which Compose passes to
the container as environment variables. Configuration paths are paths inside
the container. Relative paths resolve against the configuration file's
directory. Unknown fields are rejected, and `mynou doctor` reports whether the
file is valid. After any change, recreate the service:

```sh
docker compose up -d --force-recreate
```

| Setting | Generated value | Purpose |
| --- | --- | --- |
| `library.movies_root`, `library.series_root` | `/library/movies`, `/library/series` | Where imports go |
| `downloads` | enabled, `/data/downloads`, `/data/torrents`, port 6881, seeding, DHT and PEX on | BitTorrent client; see [transfers](transfers.md) |
| `store_dir` | `/data/jobs` | Request journal and snapshots |
| `listen` | `0.0.0.0:8787` (`127.0.0.1:8787` outside Docker) | API and browser listener |
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

Each integration names the environment variable that holds its secret; put the
values in `.env`:

| Variable | Used for |
| --- | --- |
| `MYNOU_API_TOKEN` | API and browser sign-in (generated) |
| `MYNOU_PLEX_TOKEN` | Plex server and its watchlist |
| `MYNOU_TMDB_TOKEN` or `MYNOU_TMDB_API_KEY` | TMDB catalog |
| `MYNOU_INDEXER_API_KEY` | Default API key variable for sources |

Enable the catalog with `catalog.enabled`, and describe each source as shown in
[sources](sources.md). Mynou talks to these services itself; the installation
runs no other media manager. Then check a search:

```sh
docker compose exec mynou /mynou search --title "Example Movie" --year 2026 \
  --config /config/mynou.json
```

## Connect Plex

Set `plex.enabled`, `plex.url` and the movie and series section IDs
(`plex.movies_section`, `plex.series_section`), and put the token in
`MYNOU_PLEX_TOKEN`. `http://host.docker.internal:32400` reaches Plex on the
Docker host; for Plex in another container, use a shared Docker network and its
name. `plex.watchlist_url` only needs changing for a different watchlist
service.

Mount the same host folders in Plex and Mynou. If both see the host's
`./library` as `/library`, movies are in `/library/movies` and series in
`/library/series`. If Plex mounts it as `/media` instead, add a mapping inside
the `plex` object:

```json
"path_mappings": [
  {"mynou_prefix": "/library", "plex_prefix": "/media"}
]
```

After an import, Mynou asks Plex to scan and waits until Plex lists the title.
Upgrades, shared videos and requester imports need more: Plex must report the
exact imported path in `Part.file`. Mappings translate those paths for
comparison only: they do not move files or change mounts. They match whole
path components (`/library` does not match `/library-extra`), the longest
prefix wins, and without a mapping the paths must be identical.

After changing the configuration, recreate the service and synchronize:

```sh
docker compose up -d --force-recreate
docker compose exec mynou /mynou sync --config /config/mynou.json
docker compose exec mynou /mynou jobs --config /config/mynou.json
```

## Everyday use

The browser and the API share `127.0.0.1:8787`. `/healthz` and `/readyz` report
health; `/api` routes need `Authorization: Bearer` with the API token. The
container health check runs the Mynou binary itself. For remote access, put a
TLS reverse proxy in front and keep the plain listener private; see
[remote access](web.md#sign-in-sessions-and-remote-access).

Port 6881/TCP accepts BitTorrent peers; whether peers can reach it depends on
your firewall and router. DHT and UDP trackers use outgoing connections only.

Run any CLI command inside the container:

```sh
docker compose exec mynou /mynou submit --title "Movie" --year 2026 \
  --url 'magnet:?xt=urn:btih:...' --config /config/mynou.json
docker compose exec mynou /mynou status --config /config/mynou.json
docker compose exec mynou /mynou events ID --config /config/mynou.json
docker compose exec mynou /mynou retry ID --config /config/mynou.json
docker compose exec mynou /mynou cancel ID --config /config/mynou.json
```

For a local file, mount its folder and pass the container path with `--path`.
Cancelling a request never deletes files or cancels other requests sharing the
torrent.

## TLS trust and proxies

HTTPS uses Mynou's own TLS client and the image's certificate bundle. To trust a
private certificate authority, mount a PEM bundle read-only and point
`MYNOU_CA_FILE` at it. Certificate validation cannot be disabled. If your
network needs a proxy, set `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` for the
service; local management commands bypass it.

## Back up and upgrade

Keep `mynou.json`, `.env`, `data/` and the library folders. For a consistent
backup, stop the service before copying `data/`. The journal confirms each
transaction on disk, and an interrupted final write is recovered at the next
start. `POST /api/shutdown` lets running work finish; `docker compose stop`
instead relies on that recovery.

To upgrade, read the new release notes, back up, then pull the new digest, set
it as `MYNOU_IMAGE` in `.env` and recreate the service. Keep the configuration,
journal, downloads and library. To roll back, restore the previous digest with
compatible data: some versions change storage formats that older versions
refuse (see [limits](limits.md#persistence-and-platform)). Never delete older
images or user data automatically. Data from the former Go release (SQLite) is
not imported: keep it in a separate folder.
