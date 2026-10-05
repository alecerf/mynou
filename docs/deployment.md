# Install Mynou with Docker

The final image is `scratch`: a static Rust executable and a TLS CA PEM bundle.
The container invokes no external programs. Rust 1.99.0 and its Alpine environment
are used only during builds. The default image target is
`x86_64-unknown-linux-musl`.

## Install without Rust on the host

Download the source ZIP and image archive from
[GitHub Releases](https://github.com/alecerf/mynou/releases), verify their
[checksums](validation.md#verify-release-assets), and extract the sources.
Load the validated release image:

```sh
docker load -i mynou-v0.20.1-linux-amd64-image.tar.gz
```

Alternatively, build the image from the extracted sources:

```sh
docker build -t mynou:0.20.1 .
```

Use the image binary to prepare an installation in a new directory:

```sh
docker run --rm --network none \
  --user "$(id -u):$(id -g)" \
  --mount "type=bind,src=$PWD,dst=/work" \
  --workdir /work \
  mynou:0.20.1 setup-docker --dir mynou-docker
```

If your account does not use UID/GID 1000, add its IDs to the generated `.env`:

```sh
printf '\nMYNOU_UID=%s\nMYNOU_GID=%s\n' "$(id -u)" "$(id -g)" >> mynou-docker/.env
```

Use a regular account for this procedure. If root created the files, assign the
installation directory to the chosen container user. That user must also be able
to read the private `mynou.json` file.

```sh
cd mynou-docker
docker compose config --quiet
docker compose up -d
docker compose logs -f mynou
docker compose exec mynou /mynou doctor --config /config/mynou.json
```

`setup-docker` refuses to overwrite an existing directory. It generates:

```text
mynou-docker/
├── compose.yaml
├── mynou.json
├── .env
├── data/
└── library/
    ├── movies/
    └── series/
```

`data` contains requests, torrents, and downloads. The library is mounted
separately. The image filesystem stays read-only, Linux capabilities are removed,
and the container uses an unprivileged user by default.

The source ZIP also includes `bin/mynou`; you may use its `setup-docker` command
instead of running that command inside Docker. The image archive is published as
a GitHub release asset, so no container registry login is needed beyond access to
the GitHub repository.

## Connect your existing Plex server

Edit `mynou.json` to enable `plex.enabled`, set `plex.url`, provide movie/series
section IDs, and, if needed, set `plex.watchlist_url`.
`http://host.docker.internal:32400` addresses the Docker host. For Plex in another
container, you can use a shared Docker network and its DNS name instead.

Set `MYNOU_PLEX_TOKEN` in `.env`. For TMDB, enable `catalog.enabled` and provide
`MYNOU_TMDB_TOKEN` or `MYNOU_TMDB_API_KEY`. Each `indexers` entry describes an
`rss`, `json`, or `torznab` source, its URL, and the variable name holding any API
key. Mynou makes these requests itself; the installation starts no additional
media manager.

Mount the same directories in Plex and Mynou. For example, mount the host
`./library` directory at `/library` in both containers: movies are then in
`/library/movies` and series in `/library/series`. Configuration paths are paths
as seen inside the Mynou container. They resolve to absolute roots relative to
the configuration location, even if `--config` uses a relative filename.

If Plex mounts the same host directory at `/media` instead, add this field inside
its existing `plex` configuration object:

```json
{
  "path_mappings": [
    {"mynou_prefix": "/library", "plex_prefix": "/media"}
  ]
}
```

Upgrade confirmation requires Plex to report the new imported file in
`Part.file`. Mappings match complete lexical path components and use the longest
matching prefix; without a mapping, paths must match exactly. They translate
confirmation paths, not mounts or files. See [library monitoring](library.md).

After changing secrets or configuration:

```sh
docker compose up -d --force-recreate
docker compose exec mynou /mynou sync --config /config/mynou.json
docker compose exec mynou /mynou jobs --config /config/mynou.json
```

## Configure selection before automatic acquisition

The optional `selection` object defines named movie and episode profiles. Follow
the [selection guide](selection.md) to restrict resolution, source, codec or
language markers and configure required/blocked terms or scores. Existing
configuration files without this object keep unrestricted selection.

Restart the service after changing its configuration, then preview a request:

```sh
docker compose up -d --force-recreate
docker compose exec mynou /mynou search --title "Example Movie" --year 2026 \
  --config /config/mynou.json
docker compose exec mynou /mynou search --title "Example Series" --kind episode \
  --season 1 --episode 2 --config /config/mynou.json
```

The report shows accepted/rejected candidates and the proposed winner. It omits
acquisition URLs and credentials, and creates no download or journal request.
The preview still contacts your configured sources. Release-title markers do
not verify actual tracks.

## Configure series monitoring

With TMDB and sources enabled, open **Series** in the browser or use the running
container to create a durable series plan:

```sh
docker compose exec mynou /mynou track-series --title "Example Series" \
  --year 2026 --tmdb-id 123 --future-only --config /config/mynou.json
docker compose exec mynou /mynou series --config /config/mynou.json
docker compose exec mynou /mynou calendar --config /config/mynou.json
```

A monitored record checks for newly aired missing episodes while the service
runs. This is independent from owned-library upgrade monitoring below. Plex show
watchlist entries also create series records. Their monitoring survives watchlist
removal; disable it explicitly in Series settings. Keep the complete data mount
in backups, including the request journal and private `series.json`. See the
[series guide](series.md) for unknown dates/identities, specials, refresh limits
and opt-in season-pack preference. Set `"series_packs": { "enabled": true }`
in `mynou.json` to try mapped packs before individual jobs during monitored
tracking and refresh; this defaults to false.

## Choose an explicit season pack

Use `track-series --unmonitored` to save a new catalog scope before automatic
individual acquisition, then use **Acquire a mapped pack** in browser series
details. The CLI also supports `series-pack ID --url … --mapping FILE`. Mapping
paths name files inside the torrent, including its top-level directory; they do
not name the download mount or torrent hash prefix. New mapped native transfers
acquire selected file interests and required boundary pieces, while jobs import
their exact verified video files. Earlier full transfers keep that policy.
See the [pack guide](packs.md) for the JSON format and CLI/API operations.

For automatic numbered-file mapping, use **Preview season packs** in series
details or the running container's CLI:

```sh
docker compose exec mynou /mynou series-pack-search ID --season 1 \
  --config /config/mynou.json
```

Preview contacts sources and authenticates metadata without payload downloads
or jobs. Review the resolved mapping before guarded apply. See
[automatic packs](automatic-packs.md) for discovery bounds and identity checks.

Pack jobs add a persistent file-mapping field. Retain all data mounts and avoid
downgrading installations containing mapped jobs to older binaries.

## Configure library monitoring

Background monitoring is disabled by default, including for existing
configurations. Add a top-level `monitoring` object to enable it deliberately:

```json
{
  "monitoring": {
    "enabled": true,
    "interval_secs": 3600,
    "max_checks": 32
  }
}
```

The interval must be 60–86,400 seconds and the check limit 1–256 entries per
pass. Configure profile cutoffs before enabling unattended upgrades. A cutoff
is optional and follows resolution preference order. Restart after configuration
changes, then inspect the current owned library and preview an upgrade pass:

```sh
docker compose up -d --force-recreate
docker compose exec mynou /mynou library --config /config/mynou.json
docker compose exec mynou /mynou upgrades --config /config/mynou.json
```

Use `upgrades --apply` for an explicit apply pass even when background monitoring
is disabled. Manual passes ignore the polling interval; background passes honor
it, with timestamps persisted even on failed searches. Use `monitor ID` or
`unmonitor ID` to control an individual current entry. Earlier or explicit
imports without a recorded release baseline remain
ineligible until `baseline ID --release-title TITLE` supplies a matching release
name for present, safe owned files with declared video streams. Plex files
that Mynou skipped rather than imported are not adopted automatically.

An upgrade keeps old imports and downloads and uses a unique filename for the
replacement. Failed or canceled replacements leave the earlier ready entry
current. A same-media manual request cannot become ready while an upgrade is
pending; cancel that pending upgrade if you choose the manual alternative.
Promotion preserves the parent's current monitoring choice. Plan disk space for
retained versions; no automatic cleanup is included.
The [library guide](library.md) covers baseline claims, preview/apply behavior,
cutoffs and the authenticated API.

## Configure peer concurrency and transfer limits

Inside the existing `downloads` object, `max_active` bounds active transfers and
`max_peers` bounds outgoing peer workers per transfer. `max_peers` defaults to
`4`, including when omitted by an older configuration, and accepts integers
`1` through `8`. Set it to `1` for a single-peer baseline. Global worker and
per-transfer resource bounds can lower the effective peer count. Restart the
service after changing it; see [parallel transfer bounds](transfers.md#parallel-peer-transfers).

Optional policy fields live directly inside the existing `downloads` object:

```json
{
  "download_limit_bps": 0,
  "upload_limit_bps": 0,
  "seed_ratio_milli": null,
  "seed_time_secs": null
}
```

The values above are the defaults, including for older configuration files.
Download/upload limits count global content payload bytes per second; `0` means
unlimited. A non-null ratio of `1000` represents 1.0, and the time limit is in
seconds. Global rate limits remain mandatory for every transfer; ratio/time
values are seeding defaults that a complete per-transfer policy can replace.
Rate buckets allow a bounded 16 KiB burst. These policies keep imported library
files and downloaded sources. Restart the service after changing configuration.
See [transfer controls](transfers.md) for bounds, durable counters, pause/resume, queue priorities and policy behavior.

## Ports and management

Browser management and the API share `127.0.0.1:8787`. Open
**http://127.0.0.1:8787/ui** and sign in with `MYNOU_API_TOKEN` from `.env`.
The browser uses separate expiring session cookies and protected forms. For
remote access, use a TLS proxy that preserves Host/Origin and keeps the plain
listener private; see the [browser deployment guide](web.md#sign-in-and-deployment).
`/healthz` and `/readyz` describe service health; `/api` operations require the
`MYNOU_API_TOKEN` Bearer token. The healthcheck uses the Mynou binary itself.

Port 6881/TCP accepts BitTorrent peers. DHT and UDP trackers make outgoing
requests using ephemeral sockets; no inbound UDP port is published. The DHT
client is not a complete DHT server, and the engine does not implement uTP.
Inbound TCP access depends on your firewall and router.

Manage requests with the container CLI:

```sh
docker compose exec mynou /mynou submit --title "Movie" --year 2026 \
  --url 'magnet:?xt=urn:btih:...' --config /config/mynou.json
docker compose exec mynou /mynou status --config /config/mynou.json
docker compose exec mynou /mynou events ID --config /config/mynou.json
docker compose exec mynou /mynou retry ID --config /config/mynou.json
docker compose exec mynou /mynou cancel ID --config /config/mynou.json
```

For a local source, mount its directory and pass its container path with `--path`.
Canceling a request does not delete files or cancel other requests that might
share the torrent.

## Back up and update

Keep `mynou.json`, `.env`, `data`, and the library directories. For a consistent
backup, stop the service before copying its data. The journal synchronizes
confirmed transactions. After an abrupt interruption, an incomplete final write
is recovered at the next writable service start. Offline library listing and
upgrade previews do not create files, change permissions or repair storage;
fresh storage returns no entries and an interrupted tail asks for explicit writable recovery.

Authenticated `POST /api/shutdown` lets the service finish its workers. The
standard library does not provide the portable Unix SIGTERM handler this project
would need. A forced container stop therefore relies on durable recovery rather
than application-level graceful shutdown.

For an update, download and load the new CI-published image or rebuild from its
sources, change the image version in your installation's `compose.yaml`, then
recreate the container. Preserve configuration and data. Keep old Go/SQLite data
in another directory: it is not the Rust persistence format and is not imported
automatically.

## TLS trust and proxies

HTTPS connections use the native TLS client. You can replace the image's trust
bundle with a read-only mounted PEM file and `MYNOU_CA_FILE`. Add a private CA to
that bundle for an internal service. There is no certificate-validation bypass.

Provide `HTTP_PROXY`, `HTTPS_PROXY`, and `NO_PROXY` to the service if your network
requires them. Local management commands bypass the proxy. See
[protocol limits](limits.md) for supported variants.
