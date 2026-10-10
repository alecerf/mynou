# Install Mynou on macOS

Mynou provides separate executables for Apple Silicon (`macos-arm64`) and Intel
(`macos-x86_64`). They need no Rust, Docker or other download or media program.
CI builds both and runs the local acquisition and import demo on macOS 26;
earlier macOS versions have not been verified.

## Download and verify

Open [GitHub Releases](https://github.com/alecerf/mynou/releases) and download
your architecture's executable and `SHA256SUMS` from the same release (0.22.19 or
later). Use **About This Mac** to tell Apple Silicon from Intel: `uname -m` in a
Terminal running under Rosetta can report Intel on Apple Silicon.

For Apple Silicon, with both files in Downloads (replace the version with the
one you downloaded):

```sh
cd "$HOME/Downloads" &&
  awk '$2 == "mynou-v0.22.19-macos-arm64"' SHA256SUMS | shasum -a 256 -c - &&
  mkdir -p "$HOME/.local/bin" &&
  install -m 755 mynou-v0.22.19-macos-arm64 "$HOME/.local/bin/mynou" &&
  "$HOME/.local/bin/mynou" demo --dir "$HOME/mynou-demo"
```

On Intel, use `macos-x86_64` in the manifest entry and the file name. Each step
runs only if the previous one succeeded, so a checksum failure stops before
installation: download the files again before retrying. The
[verification guide](validation.md#verify-release-assets) explains what the
checksums prove. The demo needs a directory that does not exist yet, uses
synthetic media, loopback peers and simulated Plex and indexer services, and
needs no secrets. Add `$HOME/.local/bin` to your `PATH` to run `mynou` directly.

The executables are not signed with an Apple Developer identity or notarized.
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
described in [configure Mynou](deployment.md#configure-mynou), using local paths
instead of container paths. Use ordinary folders you own rather than paths
through symbolic links: imports never overwrite files and refuse symbolic links.

Mynou reads only `MYNOU_API_TOKEN` from the `.env` file beside the
configuration. Export the other secrets (Plex, TMDB, sources) in the shell that
starts the service, for example by loading that file when its values contain no
spaces or quotes:

```sh
set -a && . ./.env && set +a &&
  "$HOME/.local/bin/mynou" serve --config ./mynou.json
```

Open **http://127.0.0.1:8787/ui** and sign in with the API token. Run the
service as the user that owns its configuration, data and library. Mynou does
not install a launch agent or background service; supervising it is up to you.

The TLS client reads PEM certificate bundles such as `/etc/ssl/cert.pem`, not
the macOS Keychain. To trust a private certificate authority, set
`MYNOU_CA_FILE` to an approved PEM bundle. Certificate and host name checks
cannot be disabled.

## Upgrade

Stop the service, download the next release's executable for the same
architecture, verify it as above and install it at the same path. Keep the
configuration, `.env`, journal, downloads and library folders. Published
releases never change; each update is a new version.
