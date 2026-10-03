# Standard library only

The manifest declares no dependency, development-dependency, or build-dependency
sections. `Cargo.lock` contains only `mynou`. There is no vendored library,
third-party library submodule, FFI linkage, or generation step invoking the
former Go implementation.

| Former component | Rust replacement |
| --- | --- |
| Go BitTorrent engine | `src/torrent.rs` and `src/torrent/` |
| SQLite | Transaction journal and snapshots in `src/store.rs` |
| ffprobe | Container parsers in `src/media.rs` and `src/media/` |
| HTTP/TLS libraries | `src/net.rs`, `src/tls.rs`, and `src/pki/` |
| JSON and bencode | Bounded parsers in `src/json.rs` and `src/bencode.rs` |
| Hashing libraries | Native algorithms in `src/crypto/` |

Rust and Cargo are build tools. Docker and Compose support deployment; the binary
can also run directly. File, network, time, and randomness operations use the
standard library and operating system. Linux provides security randomness through
`/dev/urandom`; no improvised pseudorandom generator replaces it.

The TLS CA PEM bundle is trust data. The final image contains that bundle and the
static binary. It does not install OpenSSL, ffprobe, curl, a shell, a SQL engine,
or an external torrent client.

CI checks the graph offline using Cargo metadata and the dependency tree. The
metadata must contain one package with `dependencies: []`, and the tree must show
only `mynou` at its current version. Tests and lint must not run locally.

Plex, TMDB, and indexers are user-configured network services, rather than build
dependencies or commands executed by Mynou. GitHub Actions and its build tools
are development infrastructure, not runtime dependencies.
