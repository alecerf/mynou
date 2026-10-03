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
docker load -i mynou-v0.7.0-linux-amd64-image.tar.gz
```

Alternatively, build the image from the extracted sources:

```sh
docker build -t mynou:0.7.0 .
```

Use the image binary to prepare an installation in a new directory:

```sh
docker run --rm --network none \
  --user "$(id -u):$(id -g)" \
  --mount "type=bind,src=$PWD,dst=/work" \
  --workdir /work \
  mynou:0.7.0 setup-docker --dir mynou-docker
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
as seen inside the Mynou container.

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
not verify actual tracks, and 0.7.0 does not upgrade previously imported files.

## Ports and management

The API is published only on `127.0.0.1:8787`. `/healthz` and `/readyz` describe
service health; management operations require the `MYNOU_API_TOKEN` Bearer token.
The healthcheck uses the Mynou binary itself.

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
is recovered at the next start.

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
