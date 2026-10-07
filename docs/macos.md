# Native macOS installation

Mynou provides separate executables for Apple Silicon (`macos-arm64`) and
Intel (`macos-x86_64`). Rust, Docker and external download/media programs are
not required at runtime. CI builds and runs the local acquisition/import demo
on standard macOS 26 runners for both architectures. Earlier macOS versions
have not been verified.

## Download and verify

Open [GitHub Releases](https://github.com/alecerf/mynou/releases) while signed
in with an account authorized for this private repository. Choose a published
release containing your architecture's executable and its matching `.sha256`
file. The proposed 0.22.16 version is available only after CI publishes it.
Use **About This Mac** to identify Apple Silicon or Intel; `uname -m` in a
Terminal running under Rosetta can report Intel on an Apple Silicon machine.

For Apple Silicon, after downloading both files into Downloads:

```sh
cd "$HOME/Downloads"
shasum -a 256 -c mynou-v0.22.16-macos-arm64.sha256
mkdir -p "$HOME/.local/bin"
install -m 755 mynou-v0.22.16-macos-arm64 "$HOME/.local/bin/mynou"
"$HOME/.local/bin/mynou" demo --dir "$HOME/mynou-demo"
```

On Intel, replace `macos-arm64` with `macos-x86_64` in both filenames.
The demonstration directory must not already exist. It uses synthetic media,
loopback peers and simulated Plex/indexer responses; it needs no personal secrets.
Add `$HOME/.local/bin` to your shell's PATH if you want to invoke `mynou` directly.

Release checksums detect changed downloads. They do not provide an Apple
Developer signature. These executables are not signed with an Apple Developer
identity or notarized. If macOS blocks a verified download, use the per-application
**Open Anyway** control in **System Settings → Privacy & Security**, then confirm
the application. Keep Gatekeeper enabled globally.

## Configure and start

Create an owned directory outside the download folder:

```sh
mkdir -p "$HOME/Library/Application Support/Mynou"
cd "$HOME/Library/Application Support/Mynou"
"$HOME/.local/bin/mynou" init --config ./mynou.json
"$HOME/.local/bin/mynou" doctor --config ./mynou.json
"$HOME/.local/bin/mynou" serve --config ./mynou.json
```

`init` creates configuration and a private `.env` containing a random API token.
Plex and TMDB start disabled. Configure the integrations, source credentials
and owned media paths before enabling acquisition. Secrets belong in the
environment or the private `.env`; do not put them in Issues or release notes.
Open **http://127.0.0.1:8787/ui** and sign in with the generated API token.
Run the service as the same ordinary user that owns its state and media paths.

Native TLS reads PEM trust bundles, including `/etc/ssl/cert.pem`; it does not
read macOS Keychain directly. Set `MYNOU_CA_FILE` to an approved PEM bundle when
your deployment needs private certificate authorities. Keep certificate and
hostname verification enabled. See [TLS and protocol limits](limits.md).

Native filesystem writes retain the existing owned-path, no-overwrite and
symlink protections. Use ordinary owned directories, rather than paths through
symlinked temporary folders. Service supervision is a separate deployment choice;
this release does not install a launch agent or background service.

## Upgrade

Stop the running service before replacing the installed executable. Download
the next published binary for the same architecture, verify its matching
checksum and install it at the same path. Retain the configuration, private
environment, journal, downloads and library directories. Published release
assets remain immutable; updates use a new release version.

[Browser management](web.md) · [Configuration](../README.md#configuration-and-commands)
· [Docker installation](deployment.md) · [CI guarantees](ci.md)
